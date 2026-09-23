// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! F-05 — G1's card table: the per-address half of a remembered set whose
//! other half is per-REGION.
//!
//! # The finding
//!
//! G1's [`RememberedSet`](crate::region::RememberedSet) records SOURCE REGION
//! INDICES. To act on one entry, Phase 2
//! ([`G1Collector::scan_source_region_for_cset_refs`](crate::g1)) linearly
//! walks the whole source region `[0, cursor)` — validating every object
//! header, dispatching on layout, and visiting every reference slot — to find
//! the handful of slots that point into the collection set. The cost is
//! proportional to BYTES IN THE SOURCE REGION, not to the number of edges, so
//! one remembered edge into a 1 MiB Old region costs a megabyte walk. HotSpot's
//! G1 pays per dirty 512-byte card instead.
//!
//! This table is that card granularity. It does **not** replace the
//! region-index set: that set is what tells a pause WHICH regions to look at,
//! and this one tells it WHERE INSIDE one. Two questions, two structures.
//!
//! # Why not [`crate::card_table::CardTable`]
//!
//! REASSESSED 2026-09-05, because the first of the two reasons below stopped
//! being true. `CardTable::mark_dirty_lockfree` is now a bounds check, a shift,
//! a relaxed load and a conditional release byte-store — byte for byte the rule
//! the JIT's inline barrier emits — so "its dirty path is a buffered push plus
//! an eventual mutex-protected fold" no longer describes it.
//!
//! The merge is still not worth making, and the honest reason is not either of
//! the ones below. It is that the SHARED core is about thirty lines — a byte
//! map, a base-relative index, and one conditional store — while everything
//! around it is genuinely different work. `CardTable` is a QUEUE: per-thread
//! buffers, a cross-thread registry, `drain_pending`, `flush_all`, and table-id
//! scoping so several live instances cannot steal each other's offsets.
//! `G1CardTable` is a MAP with span queries: `snapshot`, `clean_and_redirty`,
//! `any_dirty_in`, and a `CardSet`. Neither wants the other's half, and merging
//! them would mean one type carrying both, on two collectors' hottest barrier
//! paths, for thirty lines.
//!
//! What DID need fixing is the part a comment was holding together: the card
//! SIZE. There were THREE copies, not two — `CARD_SIZE = 512` here's sibling,
//! `G1_CARD_SHIFT = 9` in this file, and `emit_shr_r64_imm8(RCX, 9);
//! // CARD_SIZE = 512` in the x64 emitter — bound by two comments. The third is
//! the one that matters: it is a shift BAKED INTO MACHINE CODE, so a divergence
//! would not read as a mismatch between two Rust constants, it would be a
//! barrier that dirties the wrong card.
//!
//! All three now derive from [`cratonvm_types::CARD_SIZE_BYTES`], which is the
//! only crate the emitter and both card tables can name.
//!
//! BOTH JIT PATHS ARE CURRENTLY LATENT, which is why this was a trap rather
//! than a bug: `inline_card_mark_available()` is a deliberate constant `false`,
//! so the emitter's sequence is unreachable; and the G1 inline barrier CALLs the
//! lean helper rather than emitting a card store, so words `[3]` and `[4]` of
//! `JIT_G1_BARRIER` (`card_table_base`, `card_shift`) are published with no
//! reader. Whoever wires either path is the one who would have re-derived the
//! constant by hand, at the moment they were thinking about something else.
//!
//! The two original reasons, kept because the first is dated history and the
//! second is still true:
//!
//! * its dirty path is a per-thread `Vec` buffer behind a `Mutex`, drained into
//!   the authoritative `Mutex<CardCells>` at a safepoint (see its module docs:
//!   `THREAD_BUFFER_FLUSH_THRESHOLD`, `BUFFER_REGISTRY`, `flush_all`). That
//!   design exists so the generational collector can drain a *queue* of dirty
//!   offsets; G1's post-write barrier is already on the critical path of every
//!   reference store and cannot afford a buffered push plus an eventual
//!   mutex-protected fold. It also cannot be expressed as inline machine code,
//!   which is precisely what F-08 needs;
//! * it is indexed relative to a `base_addr` that a `GenerationalHeap` owns,
//!   and carries table-id scoping to keep several live instances from stealing
//!   each other's buffered offsets. G1 has exactly one arena for the collector's
//!   whole life, so none of that machinery buys anything here.
//!
//! What is shared is the CARD SIZE and the vocabulary, so that a reader who
//! knows one knows the other — and since 2026-09-05 the size is shared by
//! DERIVATION rather than by two constants that happened to agree.
//!
//! # A byte per card, not a bit
//!
//! One bit per card would be 8x smaller (256 bytes of metadata per 1 MiB
//! region rather than 2 KiB). It is the wrong trade here, and the reason is
//! correctness rather than speed: setting one bit of a shared byte is a
//! read-modify-write, so two mutators dirtying two different cards in the same
//! 4 KiB of heap can lose one of the two updates unless the RMW is atomic. A
//! lost dirty card is a live cross-region edge that Phase 2 never scans, whose
//! referent is therefore not evacuated, and whose region Phase 5 then frees —
//! a use-after-free. Making the RMW atomic (`lock or byte`) costs roughly
//! twenty cycles on the hottest path in the write barrier.
//!
//! A byte per card is a plain unsynchronised store of a constant. That is what
//! HotSpot does, for the same reason, and it is what makes F-08's inline
//! barrier a single `mov byte [table + idx], 1` instead of a locked
//! instruction. The cost is 1/512 of the heap: 512 KiB for the 256 MiB default,
//! 64 MiB for a 32 GiB heap.
//!
//! # Lifetime: dirty until the region is RESET, and no sooner
//!
//! HotSpot cleans a card once it has been scanned, and re-dirties it during
//! evacuation for any slot whose referent moved. This table deliberately does
//! not: a card goes clean only in [`G1CardTable::clear_range`], which
//! `G1Region::reset` calls when the region is recycled and zero-filled.
//!
//! The reason is an invariant Phase 2 leans on. `scan_source_region_for_cset_refs`
//! skips a source region outright when none of its cards is dirty, which is
//! sound exactly because "the remembered set names region S" implies "S has a
//! dirty card" — the card and the rset entry are written by the same call, and
//! the rset entry is itself additive (its only pruning is `cleanup`'s
//! recycled-source pass). Cleaning a card at the end of a pause while leaving
//! the rset entry in place breaks that implication in the dangerous direction:
//! a slot rewritten by Phase 2 to a survivor's new address still points into a
//! region that may be in the NEXT pause's collection set, and its card would be
//! clean. HotSpot avoids this by re-dirtying during evacuation; matching the
//! rset's own additive lifetime is the cheaper way to be sure.
//!
//! What is lost is that a long-lived Old region accumulates dirty cards and
//! eventually screens nothing. That is the residual, and it is stated in the
//! commit message. What would falsify the "never clean" choice is a measurement
//! showing an old generation whose cards saturate; the fix would then be a
//! HotSpot-style re-dirty during Phase 2's own slot rewrite, not a bare clean.
//!
//! That measurement is now computable: [`G1CardTable::dirty_card_total`] over
//! [`G1CardTable::card_count`]. It is not yet published as a metric — see
//! `docs/internal/g1-2026-09-20/lane-a-proposals.md` §5.
//!
//! # How a region walk reads this table (2026-09-20)
//!
//! Two structures sit between the raw byte map and Phase 2's per-object
//! question, and each removes a different part of the cost:
//!
//! * the **summary** (`G1CardTable::summary`, private) is one byte per
//!   [`G1_CARDS_PER_SUMMARY`] cards, and it answers the WHOLE-REGION screen. A 1 MiB source region that turns out
//!   to hold nothing used to cost 2048 byte loads before it could say so, and a
//!   heap whose remembered set has coarsened asks that of every live region,
//!   every pause. It now costs 32. A clean summary byte is a guarantee about
//!   its 64 cards; a dirty one is a "maybe" that falls through to them, so the
//!   summary can only ever cause more scanning;
//! * the **cursor** ([`CardSet::next_dirty_addr`]) answers the PER-OBJECT
//!   screen. The walk holds the address of the next dirty card and advances it
//!   only when it has stepped past that card, so a skipped object costs two
//!   address compares instead of a card-range derivation and a bitset probe.
//!   It decides identically to [`CardSet::any_in_span`] — that is a theorem
//!   about a reindexing of the same bitset, and it is cross-checked by
//!   `the_cursor_agrees_with_any_in_span_everywhere` here and by
//!   `gc/tests/g1_lane_a_cards.rs`.
//!
//! What neither removes is the LINEAR STEP: the walk still reads a header and
//! sizes every object in the region, because a linear walk is the only thing
//! that can tell where an object begins. That is the remaining O(region bytes)
//! term, and the third structure here is what addresses it:
//!
//! * the **block-offset table** (`starts`, W4-A) answers WHERE A WALK MAY
//!   RESUME. One byte per card, holding the lowest card-relative object start
//!   any card producer has named there, maintained by a load-guarded atomic
//!   minimum on the barrier and reset only by [`G1CardTable::clear_range`] at
//!   region recycle. With it a walk can enter a dirty card directly instead of
//!   sizing its way to it from the region's base.
//!
//!   It is the one structure here whose failure mode is NOT over-scanning. A
//!   stale card or a stale rset entry makes a pause do more work; a stale block
//!   offset makes it decode an object header at an address that does not hold
//!   one. Read [`G1CardTable`]'s `starts` field docs before touching it, and in
//!   particular note that the reader treats every entry as a hint to be
//!   validated rather than a fact — see `walk_source_region_for_cset_refs`.
//!
//! The design is `docs/internal/g1-2026-09-20/lane-a-block-offset-table.md`;
//! what was actually built, where it deviates and what it measured is
//! `docs/internal/g1-2026-09-20/w4a-block-offset-table.md`.

use cratonvm_types::{ObjectHeader, ObjectRef, GC_FLAG_HEADER, HEADER_SIZE};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

/// log2 of the bytes one card covers.
///
/// DERIVED from [`crate::card_table::CARD_SIZE`] rather than written down
/// beside it. This was `= 9` with a doc comment saying "512 bytes, matching
/// `crate::card_table::CARD_SIZE`" — two independent constants that must agree,
/// bound by nothing but that sentence.
///
/// The agreement is load-bearing in a way a comment cannot hold. The generational
/// collector and G1 both publish a card-table base to the JIT, and the emitter
/// BAKES the shift into the inline post-write barrier
/// (`jit/src/x64/objects.rs`: `[arena_base, arena_len, region_mask,
/// card_table_base, card_shift]`). A divergence would not be a mismatch between
/// two Rust constants; it would be compiled into machine code that dirties the
/// wrong card, and a missed dirty card is a live cross-region edge Phase 2 never
/// scans, whose referent is not evacuated and whose region is then freed.
pub const G1_CARD_SHIFT: u32 = cratonvm_types::CARD_SHIFT;

/// Bytes of heap one card covers.
pub const G1_CARD_BYTES: usize = 1 << G1_CARD_SHIFT;

// The derivation above is only sound for a power of two: `trailing_zeros` of a
// non-power-of-two silently rounds DOWN, so a `CARD_SIZE` of 768 would yield a
// 256-byte card here and the two tables would disagree with no diagnostic
// anywhere. Fail the build instead.
const _: () = assert!(
    G1_CARD_BYTES == crate::card_table::CARD_SIZE,
    "G1_CARD_SHIFT is derived from card_table::CARD_SIZE and must reproduce it      exactly; a CARD_SIZE that is not a power of two breaks the derivation and      the JIT bakes the shift into its inline barrier",
);

/// No cross-region reference has been stored into this card since the region
/// was last recycled.
pub const G1_CARD_CLEAN: u8 = 0;

/// A cross-region reference store landed in this card.
pub const G1_CARD_DIRTY: u8 = 1;

/// log2 of how many cards one SUMMARY byte covers.
///
/// 64 cards = 32 KiB of heap per summary byte, so the summary is 1/64 of the
/// card table and 1/32768 of the heap: 8 KiB for a 256 MiB heap, 1 MiB for a
/// 32 GiB one. Chosen so that the whole-region screen for the 1 MiB default
/// region size reads 32 bytes instead of 2048 — two cache lines rather than
/// thirty-two.
pub const G1_CARD_SUMMARY_SHIFT: u32 = 6;

/// Cards covered by one summary byte.
pub const G1_CARDS_PER_SUMMARY: usize = 1 << G1_CARD_SUMMARY_SHIFT;

/// log2 of the granularity a block-start offset is recorded at.
///
/// Object starts in a G1 region are 8-aligned — every allocator path bumps by
/// a multiple of 8 and `plausible_heap_pointer` refuses anything else — so a
/// card-relative object start is exactly representable in 8-byte units, and
/// the whole 512-byte card fits in 64 of them.
pub const G1_BLOCK_GRAIN_SHIFT: u32 = 3;

/// "No object start has been recorded in this card." The block-offset table's
/// initial and reset value, and the one the source walk refuses to jump on.
///
/// `0xFF` rather than `0` for two reasons that both matter. `0` is a LEGAL
/// offset (an object starting exactly at the card base), so it cannot double
/// as "unknown". And the table is maintained by a monotone MINIMUM
/// ([`G1CardTable::dirty_addr_at_object_start`]), for which the identity
/// element must be the largest representable value — `0xFF` is what makes the
/// very first `fetch_min` install the producer's real offset with no separate
/// "is this the first one?" branch.
pub const G1_BLOCK_UNKNOWN: u8 = 0xFF;

/// A card's block offsets must fit in a `u8` with [`G1_BLOCK_UNKNOWN`] left
/// over as a sentinel, or the whole representation collapses silently: an
/// offset that wraps past 255 does not fail, it names a DIFFERENT address
/// inside the card, and the source walk then enters a walk at something that
/// is not an object header. At the 512-byte card this is 64 against 255, so
/// there is a factor of four of headroom; a card of 4 KiB or more would need a
/// wider element and this is where that is discovered.
const _: () = assert!(
    (G1_CARD_BYTES >> G1_BLOCK_GRAIN_SHIFT) < G1_BLOCK_UNKNOWN as usize,
    "a card-relative object start must fit in a u8 below G1_BLOCK_UNKNOWN; widen      the block-offset table's element type before growing G1_CARD_BYTES",
);

/// W5-B — **a proof, carried in the type system, that an address is an object
/// START.** The only thing [`G1CardTable::dirty_addr_at_object_start`] accepts.
///
/// # The gap this closes
///
/// W4-A shipped the block-offset table default-OFF and said why (see
/// `docs/internal/g1-2026-09-20/w4a-block-offset-table.md` §8.1): the jump it
/// licenses is sound **iff an object holding a cross-region reference always
/// has a dirty START card**, and that rule was *audited* across four producers
/// and *tripwired* on one of them — but it was not ENFORCED. A fifth producer
/// that dirtied a SLOT's card would pass every test in the tree. It would not
/// even reliably surface as an entry refusal, because a slot address that
/// happens to sit at a real object boundary further along the card decodes as
/// a perfectly plausible header — the walk jumps to it, steps over the object
/// that needed scanning, and reports `entries_refused=0`.
///
/// The fix is to make the rule impossible to state wrongly, rather than to
/// keep restating it in comments. The producer no longer hands this module an
/// address; it hands it a **witness**, and the witness can only be minted from
/// something that an object start is the only way to have:
///
/// * [`ObjectStart::of_header`] — a `&ObjectHeader`. The address is DERIVED
///   from the reference (`h as *const _ as usize`), never passed beside it, so
///   there is no second value to get wrong.
/// * [`ObjectStart::of_object`] — an [`ObjectRef`], whose whole type contract
///   is "points at a published object header".
/// * [`ObjectStart::from_asserted_start`] — `unsafe`, for the synthetic
///   addresses the unit tests dirty in a table over memory that does not
///   exist.
///
/// So a new producer that wants to name a slot has to write `unsafe` code to
/// fabricate a header reference at it. That is the same move wave 3 made when
/// it put `G1Region::region_type` behind accessors rather than merely making
/// it atomic: the protocol becomes something the compiler checks instead of
/// something a reviewer remembers.
///
/// # And a runtime vouch behind it, because a witness is not a proof
///
/// A `&ObjectHeader` can be fabricated, and — more to the point — a producer
/// can hand over a perfectly real header that belongs to the WRONG object.
/// So the witness is backed by a second, independent check at the moment an
/// entry is actually installed: the sixteen bytes must carry
/// [`GC_FLAG_HEADER`], the bit every allocator sets and nothing ever clears.
/// See [`G1CardTable::dirty_addr_at_object_start`] for where that runs, why it
/// is free in the steady state, and what happens when it fails.
#[derive(Clone, Copy, Debug)]
pub struct ObjectStart<'a> {
    /// The object's first byte.
    addr: usize,
    /// May `addr` be read as an [`ObjectHeader`] when the vouch needs to?
    ///
    /// `false` only for [`ObjectStart::from_asserted_start`], whose caller has
    /// taken on the whole obligation by writing `unsafe`. The distinction is
    /// what lets the unit tests dirty addresses in a table built over a
    /// synthetic base with no memory behind it: a vouch that dereferenced
    /// there would not fail, it would fault.
    readable: bool,
    /// The borrow `of_header` was minted from, so that `addr` is still
    /// dereferenceable when the vouch runs.
    ///
    /// A `PhantomData` and not the reference itself, on purpose. The vouch
    /// runs on the COLD path — inside the load-before-RMW guard — and forming
    /// a `&ObjectHeader` there and only there keeps the mutator write barrier,
    /// which constructs one of these on every cross-region store, from
    /// carrying a live shared borrow of a header that other threads are
    /// concurrently mutating through its `AtomicU64`. The borrow is sound
    /// either way; this way the hot path does not have to argue it.
    _borrow: std::marker::PhantomData<&'a ObjectHeader>,
}

impl<'a> ObjectStart<'a> {
    /// From the object's own header. **The address is derived from the
    /// reference**, which is the whole point: a caller cannot pass a header
    /// and an unrelated address.
    #[inline]
    pub fn of_header(header: &'a ObjectHeader) -> Self {
        Self {
            addr: header as *const ObjectHeader as usize,
            readable: true,
            _borrow: std::marker::PhantomData,
        }
    }

    /// From an [`ObjectRef`].
    ///
    /// `ObjectRef`'s contract is that it points at a published object header —
    /// it is the type the whole VM dereferences as one, `as_ptr` re-checks its
    /// invariants in debug builds, and `post_write_barrier_rset` is handed the
    /// object a store TARGETED, never an interior address (`set_field`,
    /// `set_array_element` and `write_barrier` are its three production
    /// callers).
    #[inline]
    pub fn of_object(obj: ObjectRef) -> ObjectStart<'static> {
        ObjectStart {
            addr: obj.as_ptr() as usize,
            readable: true,
            _borrow: std::marker::PhantomData,
        }
    }

    /// An address the caller SWEARS is an object start, with no header to
    /// read.
    ///
    /// The one production-shaped use this has is none: it exists for the unit
    /// tests in this file, which build a [`G1CardTable`] over a synthetic base
    /// with no memory behind it and must be able to dirty addresses in it.
    ///
    /// # Safety
    ///
    /// `addr` must be the first byte of an object. Passing an interior address
    /// — a slot, a field, an array element — installs a block-offset entry
    /// that makes the source walk enter a region's object grid in the middle
    /// of an object, which is a WILD READ inside a GC pause and not a slow
    /// path. It also silently skips whatever object began below it, which is a
    /// dropped remembered-set edge, i.e. a use-after-free. There is no runtime
    /// check behind this constructor; that is what `unsafe` is saying.
    #[inline]
    pub unsafe fn from_asserted_start(addr: usize) -> ObjectStart<'static> {
        ObjectStart {
            addr,
            readable: false,
            _borrow: std::marker::PhantomData,
        }
    }

    /// The address.
    #[inline]
    pub fn addr(&self) -> usize {
        self.addr
    }

    /// The runtime half of the enforcement: do these sixteen bytes carry the
    /// allocator's [`GC_FLAG_HEADER`]?
    ///
    /// `true` for [`Self::from_asserted_start`], which has nothing readable at
    /// `addr` and whose caller signed for it in `unsafe`.
    ///
    /// Called ONLY from inside [`G1CardTable::dirty_addr_at_object_start`]'s
    /// load-before-RMW guard, which is what keeps the header read off the
    /// barrier's steady state.
    #[inline]
    fn vouched(&self) -> bool {
        if !self.readable {
            return true;
        }
        // SAFETY: `readable` is set only by `of_header`, whose lifetime keeps
        // the header borrowed for `'a`, and by `of_object`, whose `ObjectRef`
        // points at a published header by construction. The read is of an
        // `AtomicU64` field, so it is defined even while another thread is
        // locking, hashing or forwarding this object.
        let header = unsafe { &*(self.addr as *const ObjectHeader) };
        header.gc_flags() & GC_FLAG_HEADER != 0
    }
}

/// W5-B — producers that named an address carrying no [`GC_FLAG_HEADER`].
///
/// **On a sound VM this is exactly zero**, and it is the number the
/// block-offset table's default rests on. `GC_FLAG_HEADER` is set by
/// `ObjectHeader::new` and by both JIT inline-allocation emitters (which have
/// their own contract tests, `jit/src/x64/flag_and_header_contracts.rs`), and
/// is never cleared by any mark-word transition — so an address that lacks it
/// is not an object start, whatever the caller believed.
///
/// A non-zero value here means the producer rule has been broken, and the
/// response is [`block_offsets_disarmed`]: the table stops being read for the
/// rest of the process. It is a counter and a global fallback rather than a
/// `debug_assert!`, for the reason this whole subsystem repeats — an assertion
/// that fires inside a GC pause manufactures a worse bug than it reports.
static BLOCK_PRODUCER_UNVOUCHED: AtomicU64 = AtomicU64::new(0);

/// W5-B — has an unvouched producer address disarmed the block-offset jump for
/// the rest of this process?
///
/// # Why a global latch and not a per-card poison
///
/// The obvious local response to "this producer's address does not vouch" is
/// to skip the offset store and keep the card dirty. **That is unsound**, and
/// the hole is worth writing down because it is not obvious: two objects can
/// start in one card, and if the LOWER one's store is dropped while the higher
/// one's is kept, the card's recorded entry names the higher — so the walk
/// jumps past the lower object, which is precisely the dropped edge the
/// minimum exists to prevent. A minimum has no absorbing element, so there is
/// no value that could be stored to mean "never enter this card"; it would
/// take a second table.
///
/// Disarming globally needs no second table, costs one relaxed load per source
/// region walked, and fails in the only direction a remembered set may fail —
/// the walk goes back to stepping over every object linearly, which is what it
/// did before W4-A and what it still does with the lever off.
static BLOCK_OFFSETS_DISARMED: AtomicBool = AtomicBool::new(false);

/// W5-B — how many times the vouch actually ran.
///
/// This is the enforcement's ENGAGEMENT COUNT, and it is what answers "what
/// did the enforcement cost" without a clock. It is *not* the number of card
/// producer calls: the vouch sits inside the load-before-RMW guard, so it runs
/// only when a card's recorded minimum is about to FALL — at most 64 times per
/// card between resets, and in practice once. The steady state of the mutator
/// barrier — the same holder taking a store every round — never reaches it.
///
/// Read `unvouched / vouch_checks` as the failure rate of the invariant, and
/// `vouch_checks` beside the run's total barrier traffic as the cost.
static BLOCK_VOUCH_CHECKS: AtomicU64 = AtomicU64::new(0);

/// W5-B — `(vouch_checks, unvouched_producer_addresses, disarmed)`.
///
/// Read by the `[GC] g1 block-offsets:` census line and by the `g1_w5b_*`
/// tests. Process-wide rather than per-collector for the same reason
/// `BLOCK_JUMPS_TAKEN` in `gc/src/g1.rs` is: the producers include the mutator
/// write barrier, which is not holding a collector when it runs.
pub fn block_offset_enforcement_census() -> (u64, u64, bool) {
    (
        BLOCK_VOUCH_CHECKS.load(Ordering::Relaxed),
        BLOCK_PRODUCER_UNVOUCHED.load(Ordering::Relaxed),
        BLOCK_OFFSETS_DISARMED.load(Ordering::Acquire),
    )
}

/// W5-B — disarm the block-offset jump for the rest of this process, from a
/// reader that has caught the producer rule breaking.
///
/// The vouch above catches a producer naming bytes that are not a published
/// header. It cannot catch a producer naming a REAL object start that belongs
/// to the wrong object — the address vouches, the entry decodes, and the walk
/// quietly steps over something. The only thing that sees that is the source
/// walk's own audit (`audit_block_jump_span` in `gc/src/g1.rs`), which is a
/// reader and lives in the other file, so it needs a door into this latch.
///
/// The response is identical because the fault is identical: the table is no
/// longer trustworthy, and a remembered set may only fail by over-scanning.
pub fn disarm_block_offsets() {
    BLOCK_OFFSETS_DISARMED.store(true, Ordering::Release);
}

/// W5-B — one producer address refused, counted, reported and disarmed.
///
/// Shared by the two refusal sites in
/// [`G1CardTable::dirty_addr_at_object_start`] — the alignment test and the
/// `GC_FLAG_HEADER` vouch — because they are the same finding with different
/// evidence, and because the RESPONSE is the part that must not diverge
/// between them. Dropping an offset without disarming is unsound whichever
/// test rejected it: see [`BLOCK_OFFSETS_DISARMED`].
#[cold]
#[inline(never)]
fn report_unvouched_producer(addr: usize, card: usize, why: &str) {
    let n = BLOCK_PRODUCER_UNVOUCHED.fetch_add(1, Ordering::Relaxed) + 1;
    // Disarm BEFORE the report: the report is rate-limited and the disarm is
    // the part that has to happen.
    BLOCK_OFFSETS_DISARMED.store(true, Ordering::Release);
    if n <= 8 || n.is_power_of_two() {
        tracing::warn!(
            "[g1] block-offset PRODUCER UNVOUCHED (#{n}): addr=0x{addr:x} card={card} — a card              producer named an address that is not an object start ({why}). No block-offset              entry was recorded, the card IS dirty, and the block-offset jump is now DISARMED              for the rest of this process: every source-region walk falls back to the linear              walk. This is the producer rule in              docs/internal/g1-2026-09-20/w5b-block-offset-enforcement.md breaking; find the              producer."
        );
    }
}

/// W5-B — must the source walk refuse to use the block-offset table?
///
/// `Acquire` against the `Release` in the disarming path, so a walk that
/// observes `false` is guaranteed not to be looking at a table some producer
/// has already reported as untrustworthy. Read once per source region, not per
/// object.
#[inline]
pub fn block_offsets_disarmed() -> bool {
    BLOCK_OFFSETS_DISARMED.load(Ordering::Acquire)
}

/// W5-B — for the tests: forget that anything was ever disarmed.
///
/// The latch is process-wide and sticky by design, so a test that deliberately
/// trips it would poison every test after it in the same binary. Not `cfg(test)`
/// because the integration binaries in `gc/tests/` are separate crates.
pub fn reset_block_offset_enforcement_for_tests() {
    BLOCK_VOUCH_CHECKS.store(0, Ordering::Relaxed);
    BLOCK_PRODUCER_UNVOUCHED.store(0, Ordering::Relaxed);
    BLOCK_OFFSETS_DISARMED.store(false, Ordering::Release);
}

/// One byte per [`G1_CARD_BYTES`] of the G1 arena.
///
/// Indexed by `(addr - base) >> G1_CARD_SHIFT`. The arena is a single
/// allocation whose regions are `base + i * region_size` slices of it
/// (`G1Collector::new`), and `region_size` is a power of two of at least 1 MiB,
/// so every region boundary is also a card boundary *relative to `base`* — no
/// card is ever shared between two regions, whatever `base`'s own alignment is.
/// That is what makes [`Self::clear_range`] at region reset exact rather than
/// approximate.
#[derive(Debug)]
pub struct G1CardTable {
    /// Arena base address; the origin of the card index.
    base: usize,
    /// Arena length in bytes.
    len: usize,
    /// `cards[i]` covers `[base + i*G1_CARD_BYTES, base + (i+1)*G1_CARD_BYTES)`.
    ///
    /// `AtomicU8` with `Relaxed` accesses throughout: this is a plain
    /// `mov byte` on every target this runs on, but going through the atomic
    /// type is what makes concurrent mutator dirtying defined behaviour in Rust
    /// rather than a data race. Nothing here ever does a read-modify-write, so
    /// no ordering is needed between two dirtiers — they store the same value.
    cards: Box<[AtomicU8]>,
    /// One byte per [`G1_CARDS_PER_SUMMARY`] cards: the whole-region screen's
    /// index.
    ///
    /// # The invariant, which is the whole of its correctness
    ///
    /// **A CLEAN summary byte implies every card it covers is clean.** The
    /// converse is NOT claimed: a dirty summary byte means "maybe", and every
    /// reader that sees one falls through to the cards themselves. So the
    /// summary can only ever cause MORE scanning, never less, which is the
    /// direction a remembered set must fail in.
    ///
    /// Three writers maintain it, and each maintains it in the only way that is
    /// cheap for what it is doing:
    ///
    /// * [`Self::dirty_addr`] — the write barrier — stores `G1_CARD_DIRTY` into
    ///   the summary byte unconditionally, beside the card store. That is one
    ///   extra `mov` of a constant on the hot path, and it cannot violate the
    ///   invariant because it only ever makes the summary dirtier. The line is
    ///   effectively always hot: one summary cache line indexes 2 MiB of heap.
    /// * [`Self::clear_range`] and [`Self::clean_and_redirty`] RECOMPUTE the
    ///   summary for every chunk they touched, by reading the 64 cards under
    ///   it. Both are already O(cards touched), so recomputing costs at most
    ///   two extra chunks' worth of loads at the ends of the range.
    ///
    /// # Why not a count
    ///
    /// A per-chunk count of dirty cards would let `clear_range` decrement
    /// instead of recompute, but a count is a read-modify-write, and the
    /// barrier is the one path that must not have one — the same argument the
    /// type docs make for a byte per card rather than a bit.
    summary: Box<[AtomicU8]>,
    /// W4-A — THE BLOCK-OFFSET TABLE. One byte per card: the LOWEST
    /// card-relative object start any producer has named in that card since the
    /// card's region was last recycled, in [`G1_BLOCK_GRAIN_SHIFT`] units, or
    /// [`G1_BLOCK_UNKNOWN`].
    ///
    /// # What it is for
    ///
    /// Without it, a source-region walk that wants to look at one dirty card in
    /// two thousand still has to reach that card by SIZING every object before
    /// it, because a linear walk is the only thing that can tell where an
    /// object begins. That is the last O(region bytes) term in Phase 2. With
    /// it, the walk can enter at `card_base + starts[i] * 8` and skip the
    /// clean run wholesale.
    ///
    /// # Why the producers can fill it and the allocator cannot
    ///
    /// The obvious maintainer is `G1Region::bump_alloc`, and it does not work:
    /// `bump_alloc` carves a whole TLAB in ONE call and the hundreds of objects
    /// inside it are written later by the mutator, so a table maintained there
    /// records one entry for a 32 KiB carve and nothing for its contents. That
    /// is not incomplete, it is WRONG, and it is why this table was designed
    /// twice and deferred twice.
    ///
    /// The card PRODUCERS have the information for free, because every one of
    /// them already holds an object START address and passes it to
    /// [`Self::dirty_addr`]:
    ///
    /// * `post_write_barrier_rset` — `src_obj.as_ptr()`, the holder;
    /// * `record_outgoing_rset_edges` — `obj_addr`, the seed object;
    /// * `collect_outgoing_cross_region_edges`, both arms — `obj_ptr`;
    /// * the source walk's own re-dirty — `obj_ptr`.
    ///
    /// The rule has a mutation-checked tripwire in `gc/src/g1.rs`
    /// (`a_store_into_a_straddling_objects_last_slot_dirties_its_start_card`),
    /// which is what keeps it true rather than merely currently true. It is a
    /// test rather than a `debug_assert!` in [`Self::dirty_addr_at_object_start`]
    /// because this type has no way to ask whether an address is an object
    /// start, and an assertion that fires inside a GC pause manufactures a
    /// worse bug than it reports.
    ///
    /// # Why a MINIMUM, and why the minimum is not optional
    ///
    /// The walk's entry must be at or before the first object in the card that
    /// could hold a cross-region reference. Two objects can start in the same
    /// card and both take stores; a plain last-writer-wins byte would leave
    /// whichever was stored into LAST, and entering there skips the other one.
    /// That is not an over-scan, it is a remembered-set source walk missing a
    /// live edge — a use-after-free. So the merge has to be a minimum, and the
    /// minimum has to be atomic, because two mutators can be storing into two
    /// objects of the same card at the same instant.
    ///
    /// # Why the atomic minimum is nevertheless not a cost on the barrier
    ///
    /// [`Self::dirty_addr_at_object_start`] LOADS FIRST and only performs the
    /// read-modify-write when the value would actually fall. The offsets in one
    /// card are drawn from at most 64 values and only ever descend, so the RMW
    /// can fire at most 64 times per card between resets and in practice fires
    /// once: the steady state — the same object taking a store every round — is
    /// a load, a compare and a not-taken branch. This is the same
    /// load-before-store argument [`Self::clean_and_redirty`] makes, and it is
    /// what separates this from the unconditional `fetch_min` the design page
    /// costed at "roughly twenty cycles on the barrier's hot path" and
    /// rejected.
    ///
    /// # What a wrong entry does, and why it cannot be a wild read
    ///
    /// A stale or wrong byte here does not make the walk over-scan; it makes it
    /// MIS-ENTER, decoding at an address that is not an object header. The
    /// walk therefore never takes an entry on trust: it runs the full
    /// `candidate_header_is_plausible` screen on the candidate address BEFORE
    /// assigning it to the walk cursor, so a refusal costs a validated address
    /// and nothing else — there is no rewind, because there was no jump. See
    /// `walk_source_region_for_cset_refs`.
    ///
    /// # Cost
    ///
    /// One byte per card, i.e. the same size as `cards` and 1/512 of the heap:
    /// 512 KiB for a 256 MiB heap. Allocated unconditionally rather than behind
    /// the lever, because the lever's whole purpose is to A/B the READ side
    /// inside one binary and a table that only exists on one arm cannot do
    /// that. `calloc` of `0` and a memset to `G1_BLOCK_UNKNOWN`, once, at
    /// startup.
    starts: Box<[AtomicU8]>,
}

impl G1CardTable {
    /// A table covering `[base, base + len)`, all cards clean.
    pub fn new(base: usize, len: usize) -> Self {
        let count = len.div_ceil(G1_CARD_BYTES);
        // `vec![0u8; n]` reaches `calloc`, so a 64 MiB table for a 32 GiB heap
        // costs a zero page mapping rather than 64 MiB of stores. Building it
        // as `Vec<AtomicU8>` through `collect` would not.
        let zeros: Box<[u8]> = vec![0u8; count].into_boxed_slice();
        // SAFETY: `AtomicU8` is `#[repr(transparent)]` over `UnsafeCell<u8>`,
        // which has the same size and alignment as `u8`. The allocation is
        // owned here and never aliased as `[u8]` afterwards.
        let cards: Box<[AtomicU8]> =
            unsafe { Box::from_raw(Box::into_raw(zeros) as *mut [AtomicU8]) };
        let summary_zeros: Box<[u8]> =
            vec![0u8; count.div_ceil(G1_CARDS_PER_SUMMARY)].into_boxed_slice();
        // SAFETY: as above.
        let summary: Box<[AtomicU8]> =
            unsafe { Box::from_raw(Box::into_raw(summary_zeros) as *mut [AtomicU8]) };
        // W4-A — the block-offset table. `G1_BLOCK_UNKNOWN` is not zero, so
        // this one cannot ride on `calloc`; `vec![v; n]` is still a single
        // `memset` over a fresh mapping and costs a page fault per 4 KiB of a
        // table that is 1/512 of the heap.
        let unknown_starts: Box<[u8]> = vec![G1_BLOCK_UNKNOWN; count].into_boxed_slice();
        // SAFETY: as above.
        let starts: Box<[AtomicU8]> =
            unsafe { Box::from_raw(Box::into_raw(unknown_starts) as *mut [AtomicU8]) };
        Self {
            base,
            len,
            cards,
            summary,
            starts,
        }
    }

    /// The summary chunks covering `cards`, as a half-open range.
    #[inline]
    fn summary_range(&self, cards: &std::ops::Range<usize>) -> std::ops::Range<usize> {
        if cards.is_empty() {
            return 0..0;
        }
        (cards.start >> G1_CARD_SUMMARY_SHIFT)..cards.end.div_ceil(G1_CARDS_PER_SUMMARY)
    }

    /// Re-derive the summary bytes covering `cards` from the cards themselves.
    ///
    /// Called by the two cleaners after they have rewritten the card bytes.
    /// It reads the WHOLE chunk, including cards outside `cards` — it has to:
    /// the invariant is about the chunk, and a chunk that is half inside a
    /// cleaned range and half outside is clean only if both halves are.
    fn refresh_summary(&self, cards: std::ops::Range<usize>) {
        for chunk in self.summary_range(&cards) {
            let lo = chunk << G1_CARD_SUMMARY_SHIFT;
            let hi = (lo + G1_CARDS_PER_SUMMARY).min(self.cards.len());
            let any = (lo..hi).any(|i| self.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN);
            let want = if any { G1_CARD_DIRTY } else { G1_CARD_CLEAN };
            if self.summary[chunk].load(Ordering::Relaxed) != want {
                self.summary[chunk].store(want, Ordering::Relaxed);
            }
        }
    }

    /// Run `f` over every card index in `range` whose summary chunk is not
    /// known-clean, in ascending order, stopping early when `f` returns
    /// `false`. Returns `false` iff `f` stopped it.
    ///
    /// This is the one place the summary is READ, so it is the one place the
    /// "clean chunk implies clean cards" invariant has to hold. Skipping a
    /// chunk here is the only way the summary can make a caller do less work,
    /// and it is exactly what the invariant licenses.
    #[inline]
    fn for_each_maybe_dirty_card(
        &self,
        range: std::ops::Range<usize>,
        mut f: impl FnMut(usize) -> bool,
    ) -> bool {
        for chunk in self.summary_range(&range) {
            if self.summary[chunk].load(Ordering::Relaxed) == G1_CARD_CLEAN {
                continue;
            }
            let lo = (chunk << G1_CARD_SUMMARY_SHIFT).max(range.start);
            let hi = ((chunk + 1) << G1_CARD_SUMMARY_SHIFT).min(range.end);
            for i in lo..hi {
                if !f(i) {
                    return false;
                }
            }
        }
        true
    }

    /// The card index owning `addr`, or `None` when `addr` is outside the
    /// arena this table describes.
    #[inline]
    fn index_of(&self, addr: usize) -> Option<usize> {
        if addr < self.base || addr >= self.base + self.len {
            return None;
        }
        Some((addr - self.base) >> G1_CARD_SHIFT)
    }

    /// Mark the card owning `addr` dirty. A no-op for an address outside the
    /// arena (the caller's own region lookup has already returned `None` for
    /// such an address, so there is nothing to remember).
    #[inline]
    pub fn dirty_addr(&self, addr: usize) {
        if let Some(i) = self.index_of(addr) {
            // Relaxed: an unconditional store of a constant. Ordering against
            // the rset insert that accompanies it is not needed because both
            // are read only at a stop-the-world safepoint, after every mutator
            // has passed through a full barrier to park.
            //
            // The SUMMARY is stored FIRST, and the order is deliberate even
            // though nothing in this collector can currently observe it. The
            // one state that would break the summary's invariant is
            // "card dirty, summary clean"; writing the summary first means no
            // reordering — by the compiler, the CPU, or a future reader that
            // is not at a safepoint — can produce it from this path. The
            // reverse order (summary dirty, card clean) is the harmless one:
            // the reader falls through to the cards and finds nothing.
            self.summary[i >> G1_CARD_SUMMARY_SHIFT].store(G1_CARD_DIRTY, Ordering::Relaxed);
            self.cards[i].store(G1_CARD_DIRTY, Ordering::Relaxed);
        }
    }

    /// W4-A — [`Self::dirty_addr`], for a producer that knows `addr` is an
    /// OBJECT START: also lower that card's block offset to it.
    ///
    /// Every production card producer knows this and every one of them is
    /// wired to this form; [`Self::dirty_addr`] stays as the conservative
    /// entry point for a caller that does not know, and for the tests that
    /// dirty synthetic addresses. A caller using the conservative form cannot
    /// make the table WRONG — it only leaves the card's offset higher than it
    /// could have been, which the reader below turns into "no jump", which is
    /// the linear walk this exists to avoid and never an unsound one.
    ///
    /// # W5-B — the two things that now enforce the rule rather than audit it
    ///
    /// **It takes an [`ObjectStart`], not a `usize`.** A producer cannot name
    /// a slot without fabricating a header reference at it, which takes new
    /// `unsafe` code. Read that type's docs for why that is the shape this
    /// codebase reaches for.
    ///
    /// **And the entry is vouched before it is installed.** The sixteen bytes
    /// must carry [`GC_FLAG_HEADER`] — the bit every allocator sets, that no
    /// mark-word transition clears, and whose absence is therefore a statement
    /// that the address is not an object start. The check sits INSIDE the
    /// load-before-RMW guard, so it runs exactly when a value would actually
    /// be installed: at most 64 times per card between resets, in practice
    /// once, and NEVER in the steady state of a holder that takes a store
    /// every round. That placement is not an optimisation with a correctness
    /// cost — it is exactly correct, because the guard is the only door
    /// through which a value reaches `starts`.
    ///
    /// When the vouch fails the card is still dirtied (a remembered set may
    /// only ever over-remember), the offset is NOT lowered, the address is
    /// counted in [`BLOCK_PRODUCER_UNVOUCHED`], and the block-offset jump is
    /// disarmed for the rest of the process — see [`BLOCK_OFFSETS_DISARMED`]
    /// for why the response has to be global rather than per-card.
    ///
    /// # Why it does not cost the barrier a read-modify-write in the steady state
    ///
    /// See the `starts` field docs. The load-and-compare in front of the
    /// `fetch_min` means the RMW fires only when a card's minimum actually
    /// FALLS, which can happen at most 64 times per card between resets and in
    /// practice happens once — the mutator that stores into the same holder
    /// every round finds the value already equal and branches away.
    ///
    /// # The alignment case
    ///
    /// An `addr` that is not 8-aligned is not an object start under any path in
    /// this VM (`plausible_heap_pointer` refuses it, and every allocator bumps
    /// by a multiple of 8). Truncating it to the grain would name an address
    /// BELOW the real object, which is the one direction that makes the reader
    /// mis-enter, so it is refused instead: the card is dirtied and the offset
    /// is left alone. The result is an offset that may be higher than a real
    /// object start in the card, and the reader's answer to that is a linear
    /// walk, never a jump to a wrong place.
    #[inline]
    pub fn dirty_addr_at_object_start(&self, at: ObjectStart<'_>) {
        let addr = at.addr();
        let Some(i) = self.index_of(addr) else {
            return;
        };
        // Exactly `dirty_addr`'s two stores, summary first, for exactly the
        // reason given there.
        self.summary[i >> G1_CARD_SUMMARY_SHIFT].store(G1_CARD_DIRTY, Ordering::Relaxed);
        self.cards[i].store(G1_CARD_DIRTY, Ordering::Relaxed);
        // RELATIVE TO `base`, NOT TO THE ABSOLUTE ADDRESS, and the difference is
        // not cosmetic.
        //
        // The card index is `(addr - base) >> G1_CARD_SHIFT`, so card `i`
        // covers `[base + i*512, base + (i+1)*512)` — a run that is
        // 512-aligned relative to `base` and, unless `base` happens to be
        // 512-aligned itself, to nothing else. `addr & (G1_CARD_BYTES - 1)`
        // measures the offset into a 512-aligned block of the ADDRESS SPACE,
        // which is a different block. Mixing the two names an address inside
        // the right card at the wrong offset, which is exactly the mis-entry
        // shape.
        //
        // The arena base is an `mmap`/`VirtualAlloc` reservation and so is
        // page-aligned today (4 KiB on Linux, 64 KiB on Windows), both of which
        // make the two forms agree — so this would be a latent bug and not a
        // live one. `jit/src/x64/objects.rs` makes the same subtraction for the
        // inline barrier's region test and states the rule this follows:
        // "neither is guaranteed by anything the collector promises ... correct
        // for ANY base, which is the property to preserve".
        let rel = addr - self.base;
        if rel & ((1 << G1_BLOCK_GRAIN_SHIFT) - 1) != 0 {
            // W5-B — AND THIS DISARMS, which it did not when refusing the
            // offset was the whole response. Silently dropping one producer's
            // offset is not a safe no-op: if a second object starts in the
            // same card and its producer DOES record, the card's surviving
            // entry is the higher of the two and the walk jumps over the
            // first. That is the hole `BLOCK_OFFSETS_DISARMED` exists for, and
            // the unaligned case is the same hole as the unvouched one.
            //
            // It is close to unreachable now: `ObjectStart::of_header` derives
            // its address from a `&ObjectHeader`, whose type alignment is 8,
            // and `of_object`'s `ObjectRef` is alignment-checked on
            // construction. What remains is a corrupted `ObjectRef` in a
            // release build — which is exactly the case that must not quietly
            // raise a card's entry.
            report_unvouched_producer(addr, i, "it is not 8-aligned, so it is not an object start");
            return;
        }
        let want = ((rel & (G1_CARD_BYTES - 1)) >> G1_BLOCK_GRAIN_SHIFT) as u8;
        // LOAD-BEFORE-RMW. The load is free — the card byte on the line above
        // has already brought this table's own line into the core — and it
        // turns the overwhelmingly common "this card's minimum is already at or
        // below mine" into a not-taken branch instead of a `lock cmpxchg`.
        if self.starts[i].load(Ordering::Relaxed) > want {
            // W5-B — THE VOUCH, and this is the only place it can go.
            //
            // `starts` gains a value nowhere else, so checking here is the
            // same statement as "every value in the table was vouched when it
            // was installed" — the invariant the source walk's jump actually
            // needs. Putting it above the guard would say the same thing and
            // charge the mutator barrier a header load on every cross-region
            // store; putting it here charges one per card per region
            // incarnation.
            //
            // The header may legitimately straddle the end of the arena's last
            // card only if the arena does not hold whole objects, which it
            // does — but the read is bounds-checked anyway, because a producer
            // naming a bad address is exactly the case this exists for and it
            // must not be the thing that faults.
            // How often the enforcement actually RUNS, which is the number
            // that answers "what did it cost" without a clock. The round's own
            // rule: a measurement is not finished until it reports how many
            // times the thing it measured happened
            // (`orchestrator-wave-1-measurements.md` §7.3). It is one relaxed
            // add beside the `fetch_min` on a path that fires at most once per
            // card per region incarnation — NOT on the barrier's steady state,
            // which is the load and the not-taken branch above.
            BLOCK_VOUCH_CHECKS.fetch_add(1, Ordering::Relaxed);
            if addr + HEADER_SIZE > self.base + self.len || !at.vouched() {
                report_unvouched_producer(addr, i, "its sixteen bytes do not carry GC_FLAG_HEADER");
                return;
            }
            // `fetch_min` and not a plain store: two mutators can be storing
            // into two objects of this same card at this same instant, and a
            // compare-then-store would let the higher of the two win. The
            // reader treats this value as "no object below here in this card
            // holds a cross-region reference", so losing the lower one loses a
            // remembered-set edge. Relaxed for the same reason every other
            // access here is: the table is read at a stop-the-world safepoint,
            // after every mutator has passed a full barrier to park.
            self.starts[i].fetch_min(want, Ordering::Relaxed);
        }
    }

    /// W4-A — the block-offset table's READ side: the address the source walk
    /// may enter card `card_addr`'s object grid at, or `None` when no producer
    /// has named an object start in that card.
    ///
    /// `card_addr` is any address inside the card; the answer is an absolute
    /// address, always inside the same card, always 8-aligned.
    ///
    /// **This is a HINT, not a proof.** It is derived from bytes the mutator
    /// wrote and from a table whose reset is a separate act from the region's;
    /// the caller must validate the address as an object header before it
    /// decodes anything there. See the `starts` field docs.
    #[inline]
    pub fn block_entry_addr(&self, card_addr: usize) -> Option<usize> {
        let i = self.index_of(card_addr)?;
        let off = self.starts[i].load(Ordering::Relaxed);
        if off == G1_BLOCK_UNKNOWN {
            return None;
        }
        let card_base = self.base + (i << G1_CARD_SHIFT);
        Some(card_base + ((off as usize) << G1_BLOCK_GRAIN_SHIFT))
    }

    /// How many cards overlapping `[start, start + span)` carry a recorded
    /// block offset. The table's own coverage census, which is what separates
    /// "the jump never fired because the heap had nothing to skip" from "the
    /// jump never fired because the table was empty".
    ///
    /// LANE W6-A — no longer diagnostic-only. Called over the WHOLE arena
    /// ([`Self::arena_base`], [`Self::arena_len`]) by
    /// [`crate::g1::G1Collector::card_table_census_line`], because that
    /// distinction is precisely the open question against
    /// `CRATONVM_G1_BLOCK_OFFSETS`: the round shipped that lever OFF pending
    /// "whether real heaps have the shape it pays on", and `jumps=0` on the
    /// `[GC] g1 block-offsets:` line could not be told from `entries=0`.
    pub fn block_entries_in(&self, start: usize, span: usize) -> usize {
        self.card_range(start, span)
            .filter(|&i| self.starts[i].load(Ordering::Relaxed) != G1_BLOCK_UNKNOWN)
            .count()
    }

    /// Is the card owning `addr` dirty? `false` for an out-of-arena address.
    #[inline]
    pub fn is_dirty_addr(&self, addr: usize) -> bool {
        match self.index_of(addr) {
            Some(i) => self.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN,
            None => false,
        }
    }

    /// The half-open card index range covering `[start, start + span)`,
    /// clamped to the arena. Empty when the range misses the arena entirely.
    #[inline]
    fn card_range(&self, start: usize, span: usize) -> std::ops::Range<usize> {
        let arena_end = self.base + self.len;
        let lo = start.max(self.base);
        let hi = start.saturating_add(span).min(arena_end);
        if lo >= hi {
            return 0..0;
        }
        let first = (lo - self.base) >> G1_CARD_SHIFT;
        let last = (hi - 1 - self.base) >> G1_CARD_SHIFT;
        first..last + 1
    }

    /// Does any card overlapping `[start, start + span)` carry a dirty mark?
    ///
    /// This is both the whole-region screen (`span` = the region's filled
    /// bytes) and the per-object screen (`span` = one object's size). The
    /// per-object case is the hot one and is normally one or two byte loads,
    /// because objects are small relative to a card.
    #[inline]
    pub fn any_dirty_in(&self, start: usize, span: usize) -> bool {
        let range = self.card_range(start, span);
        // The per-OBJECT case touches one or two cards and would only pay for
        // the summary indirection, so it reads the cards directly. The
        // WHOLE-REGION case is the one the summary exists for: a 1 MiB region
        // that turns out to be entirely clean used to cost 2048 byte loads
        // before it could say so, and a heap whose remembered set has coarsened
        // asks that question of every live region in it, every pause.
        if range.len() <= G1_CARDS_PER_SUMMARY {
            return range
                .clone()
                .any(|i| self.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN);
        }
        !self.for_each_maybe_dirty_card(range, |i| {
            self.cards[i].load(Ordering::Relaxed) == G1_CARD_CLEAN
        })
    }

    /// How many cards in the WHOLE table are dirty.
    ///
    /// The module docs name the residual of "a card goes clean only at region
    /// reset" — a long-lived Old region accumulates dirty cards and eventually
    /// screens nothing — and say plainly what would falsify the choice: "a
    /// measurement showing an old generation whose cards saturate". This is
    /// that measurement's numerator, and [`Self::card_count`] is its
    /// denominator. O(cards): one byte load per card, i.e. 2 KiB of loads per
    /// 1 MiB region, so it belongs on a diagnostic path and not in a pause.
    ///
    /// LANE W6-A — **printed** on `[GC] g1 card-table:` by
    /// [`crate::g1::G1Collector::card_table_census_line`], which
    /// `print_gc_summary` emits once at shutdown. Until wave 6 this accessor
    /// and its denominator were read only by `gc/tests/g1_lane_a_cards.rs` and
    /// this file's own unit tests, so the falsifying measurement the paragraph
    /// above names could not be taken on a shipped binary — the doc described
    /// an experiment nobody could run.
    pub fn dirty_card_total(&self) -> usize {
        self.cards
            .iter()
            .filter(|c| c.load(Ordering::Relaxed) != G1_CARD_CLEAN)
            .count()
    }

    /// How many cards overlapping `[start, start + span)` are dirty. Diagnostic
    /// only — the engagement counter's numerator.
    pub fn dirty_cards_in(&self, start: usize, span: usize) -> usize {
        self.card_range(start, span)
            .filter(|&i| self.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN)
            .count()
    }

    /// How many cards overlap `[start, start + span)` at all — the engagement
    /// counter's denominator.
    pub fn cards_in(&self, start: usize, span: usize) -> usize {
        self.card_range(start, span).len()
    }

    /// Clean every card overlapping `[start, start + span)`.
    ///
    /// The ONLY unconditional cleaner, and its one production caller is
    /// `G1Region::reset`.
    /// See the module docs for why a scanned card is not cleaned: the whole-
    /// region screen in Phase 2 is sound only while "the rset names S" implies
    /// "S has a dirty card", and the rset entry outlives the pause that read it.
    ///
    /// Because a region boundary is always a card boundary relative to `base`
    /// (see the type docs), clearing a region's range never clears a byte that
    /// belongs to its neighbour.
    pub fn clear_range(&self, start: usize, span: usize) {
        let range = self.card_range(start, span);
        for i in range.clone() {
            self.cards[i].store(G1_CARD_CLEAN, Ordering::Relaxed);
            // W4-A — THE BLOCK-OFFSET TABLE IS RESET HERE AND NOWHERE ELSE,
            // and this is the load-bearing half of its soundness.
            //
            // `G1Region::reset` zero-fills the region and hands it back to the
            // allocator, so every object start recorded in it names an address
            // that the region's NEXT incarnation will lay out differently. A
            // stale offset surviving a reset is not an over-scan the way a
            // stale card or a stale rset entry is — it is an address the source
            // walk would enter an object decode at, in bytes that are no longer
            // an object. That is the difference between this table and every
            // other structure here, and it is why the reset is unconditional,
            // is not gated on the lever, and rides on the one statement that
            // already covers a region's whole slice for the whole of its
            // lifetime.
            //
            // A plain store, not a `fetch_min`: this is the only writer that
            // RAISES a value, it runs at a safepoint with every mutator parked,
            // and raising to "unknown" can only ever cost a later walk a linear
            // step it would otherwise have skipped.
            self.starts[i].store(G1_BLOCK_UNKNOWN, Ordering::Relaxed);
        }
        self.refresh_summary(range);
    }

    /// Address of card 0, for the F-08 inline barrier's baked immediate.
    ///
    /// Stable for the table's life: `cards` is a `Box<[AtomicU8]>` allocated
    /// once in [`Self::new`] and never resized.
    #[inline]
    pub fn cards_base_addr(&self) -> usize {
        self.cards.as_ptr() as usize
    }

    /// Arena base — the origin the card index is relative to.
    ///
    /// LANE W6-A — this and [`Self::arena_len`] had **zero callers anywhere in
    /// the workspace**. W3-C's audit listed them; W4-B routed them to the lane
    /// that owned this file in wave 4 and that lane did not take them, so they
    /// survived two sweeps unread. They are not deleted because the whole-arena
    /// span is what [`Self::block_entries_in`] has to be asked for on the
    /// `[GC] g1 card-table:` line, and a reader that has an `&G1CardTable` has
    /// no other way to name the range the table covers.
    #[inline]
    pub fn arena_base(&self) -> usize {
        self.base
    }

    /// Arena length in bytes. See [`Self::arena_base`].
    #[inline]
    pub fn arena_len(&self) -> usize {
        self.len
    }

    /// Total number of cards.
    ///
    /// LANE W6-A — the denominator of [`Self::dirty_card_total`], printed
    /// beside it on `[GC] g1 card-table:`. A saturation figure published
    /// without the card count is rule 5 of the round's method again: an
    /// estimate whose sample size nobody stated.
    #[inline]
    pub fn card_count(&self) -> usize {
        self.cards.len()
    }

    /// Card cleaning — take a snapshot of which cards covering
    /// `[start, start+span)` are dirty right now.
    ///
    /// # Why a snapshot rather than reading the table as the walk goes
    ///
    /// A region walk that cleans a card as it passes it would answer its own
    /// next question wrongly. Cards are 512 bytes and objects are usually
    /// smaller, so several objects share one card: scanning object A, cleaning
    /// the card it sits in, and then asking "is B's card dirty?" reports CLEAN
    /// for a B that was never examined, and B's cross-region reference is lost.
    ///
    /// The snapshot decouples the two: the walk decides what to scan from the
    /// state the table had when the walk began, and the table is rewritten once
    /// at the end ([`Self::clean_and_redirty`]).
    pub fn snapshot(&self, start: usize, span: usize) -> CardSet {
        let range = self.card_range(start, span);
        let mut set = CardSet::empty(self.base, range.clone());
        // A summary chunk that is clean contributes no bits, and the bitset is
        // already all-zero, so the chunk can be skipped whole. That is what
        // makes the snapshot affordable to take on EVERY screened source region
        // rather than only when card cleaning is on: a region with three dirty
        // cards costs three chunk loads plus 64 card loads, not 2048.
        self.for_each_maybe_dirty_card(range, |i| {
            if self.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN {
                set.set_index(i);
            }
            true
        });
        set
    }

    /// An all-clean set over the same cards [`Self::snapshot`] would cover, for
    /// a walk to accumulate the cards it wants kept dirty.
    pub fn empty_set(&self, start: usize, span: usize) -> CardSet {
        CardSet::empty(self.base, self.card_range(start, span))
    }

    /// Card cleaning — clean every card covering `[start, start+span)` and then
    /// re-dirty exactly those in `keep`.
    ///
    /// The caller's contract, and it is the whole soundness argument: it must
    /// have examined **every object overlapping that range** and put into
    /// `keep` the start card of each one that still holds a reference into
    /// another region. A card left clean then means "no object here references
    /// another region", which is what lets a later pause step over it.
    ///
    /// Callers bound `span` by what they actually walked, never by the region's
    /// cursor — a walk that broke early on an unsizeable header has not
    /// examined the bytes past the break, and cleaning those would drop live
    /// edges.
    pub fn clean_and_redirty(&self, start: usize, span: usize, keep: &CardSet) {
        let range = self.card_range(start, span);
        for i in range.clone() {
            let want = if keep.contains_index(i) {
                G1_CARD_DIRTY
            } else {
                G1_CARD_CLEAN
            };
            // LOAD-BEFORE-STORE, deliberately. The overwhelmingly common
            // outcome is "already clean, stays clean": a source region is
            // screened in at all because SOME card in it is dirty, and the
            // other two thousand are not. An unconditional store would take
            // every one of the region's 32 card-table cache lines to Modified
            // — 2 KiB of dirtied metadata per scanned source region per pause,
            // written back to memory for no change in value. The load is free
            // by comparison: `contains_index` has already brought the card's
            // own line in for the `keep` lookup on the line above.
            //
            // This is NOT a correctness-relevant relaxation. The table is only
            // read and written at a stop-the-world safepoint (see
            // `dirty_addr`'s ordering note), so no concurrent store can be lost
            // between the load and the conditional store.
            if self.cards[i].load(Ordering::Relaxed) != want {
                self.cards[i].store(want, Ordering::Relaxed);
            }
        }
        // The summary has to be re-derived, not merely left alone: this is the
        // one writer that makes cards CLEANER, and a summary byte that stays
        // dirty over a now-clean chunk costs the next whole-region screen the
        // 64 card loads the summary exists to save. Re-deriving is what turns
        // card cleaning into a screen that actually gets faster.
        self.refresh_summary(range);
        // W4-A — AND THE BLOCK-OFFSET TABLE IS DELIBERATELY NOT TOUCHED HERE.
        //
        // Card cleaning rewrites which cards are dirty; it does not move a
        // single object. Every offset this range holds still names the start of
        // the same object it named before, so clearing them would throw away
        // correct information and force the next pause to walk linearly to
        // rebuild it — and the value of the table is precisely that it survives
        // from the pause that recorded it to the pause that uses it.
        //
        // Leaving a recorded offset under a card this pass just wrote CLEAN is
        // sound in the only direction that matters. Nothing reads an offset
        // whose card is clean — the walk consults the table only for a card its
        // snapshot says is dirty — and when a later store re-dirties that card
        // the producer's `fetch_min` can only pull the value DOWN, towards an
        // earlier object, i.e. towards over-scanning. The one act that can make
        // a recorded offset name something that is no longer an object is a
        // region reset, and that goes through `clear_range`.
    }
}

/// A dense bitset over a contiguous run of card indices, used by the card
/// cleaning pass as both the "was dirty when the walk began" snapshot and the
/// "must stay dirty" accumulator.
#[derive(Debug, Clone)]
pub struct CardSet {
    base: usize,
    first: usize,
    words: Vec<u64>,
    len: usize,
}

impl CardSet {
    fn empty(base: usize, range: std::ops::Range<usize>) -> Self {
        let len = range.end.saturating_sub(range.start);
        Self {
            base,
            first: range.start,
            words: vec![0u64; len.div_ceil(64)],
            len,
        }
    }

    #[inline]
    fn slot(&self, index: usize) -> Option<(usize, u64)> {
        let rel = index.checked_sub(self.first)?;
        if rel >= self.len {
            return None;
        }
        Some((rel / 64, 1u64 << (rel % 64)))
    }

    #[inline]
    fn set_index(&mut self, index: usize) {
        if let Some((w, bit)) = self.slot(index) {
            self.words[w] |= bit;
        }
    }

    #[inline]
    fn contains_index(&self, index: usize) -> bool {
        match self.slot(index) {
            Some((w, bit)) => self.words[w] & bit != 0,
            None => false,
        }
    }

    #[inline]
    fn index_of_addr(&self, addr: usize) -> usize {
        addr.saturating_sub(self.base) >> G1_CARD_SHIFT
    }

    /// The first card index in this set at or after `index`, or `None` when
    /// every card from there to the end of the covered run is clean.
    ///
    /// Scans SIXTY-FOUR CARDS AT A TIME. That is the whole point of it: a
    /// region walk that asked `any_in_span` per object re-derived a card range
    /// and re-probed the bitset for every one of the thousands of objects in a
    /// 1 MiB region, and the answer for nearly all of them is "no, and not for
    /// the next several kilobytes either". One `trailing_zeros` over a zero-run
    /// of 32 KiB of heap replaces sixty-four of those probes.
    #[inline]
    fn first_set_from_index(&self, index: usize) -> Option<usize> {
        // `saturating_sub` is doing real work: an `index` BELOW `first` means
        // "from the very beginning of the covered run", which is rel 0. A
        // wrapping subtraction would produce an enormous `rel` and report the
        // run as clean — the under-scanning direction.
        let rel = index.saturating_sub(self.first);
        if rel >= self.len {
            return None;
        }
        let mut w = rel / 64;
        // Mask off the bits before `rel` in its own word; whole words after it
        // are taken unmasked.
        let mut word = self.words[w] & (u64::MAX << (rel % 64));
        loop {
            if word != 0 {
                let r = w * 64 + word.trailing_zeros() as usize;
                // Bits past `len` are never set (`slot` refuses them), so this
                // is belt-and-braces rather than a live case.
                return (r < self.len).then(|| self.first + r);
            }
            w += 1;
            if w >= self.words.len() {
                return None;
            }
            word = self.words[w];
        }
    }

    /// The START ADDRESS of the first card in this set that could cover `addr`
    /// or anything after it, or `None` when nothing from `addr` onwards is set.
    ///
    /// # What this replaces, and why it is exactly equivalent
    ///
    /// A linear region walk asks, per object, "is any card overlapping
    /// `[obj, obj+size)` dirty?" — [`Self::any_in_span`]. With a monotone walk
    /// that question has a cheaper equivalent form. Let `d` be the start
    /// address of the first dirty card at or after the object's own card. Then
    /// a card overlapping the object is dirty **iff `d < obj + size`**, because
    /// `d` is a card start and the object's last byte is `obj + size - 1`. The
    /// caller holds `d` across objects and only re-derives it when the walk has
    /// stepped past the card it names (`d + G1_CARD_BYTES <= obj`), so the
    /// total cost over a whole region walk is O(cards), not O(objects * cards
    /// per object).
    ///
    /// The equivalence is exact, not conservative: this is a reindexing of the
    /// same bitset, so it neither adds nor removes a scanned object.
    #[inline]
    pub fn next_dirty_addr(&self, addr: usize) -> Option<usize> {
        self.first_set_from_index(self.index_of_addr(addr))
            .map(|i| self.base + (i << G1_CARD_SHIFT))
    }

    /// The start address of the first dirty card in the whole covered run, or
    /// `None` when the run is entirely clean — the cursor's seed.
    #[inline]
    pub fn first_dirty_addr(&self) -> Option<usize> {
        self.first_set_from_index(self.first)
            .map(|i| self.base + (i << G1_CARD_SHIFT))
    }

    /// Mark the card holding `addr`. Addresses outside the covered run are
    /// ignored, exactly as [`G1CardTable::dirty_addr`] ignores them.
    #[inline]
    pub fn insert_addr(&mut self, addr: usize) {
        if addr >= self.base {
            self.set_index(self.index_of_addr(addr));
        }
    }

    /// Does any card overlapping `[addr, addr+span)` belong to this set?
    #[inline]
    pub fn any_in_span(&self, addr: usize, span: usize) -> bool {
        if addr < self.base {
            return false;
        }
        let first = self.index_of_addr(addr);
        let last = self.index_of_addr(addr.saturating_add(span.max(1) - 1));
        (first..=last).any(|i| self.contains_index(i))
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Is at most `percent` of the covered card run dirty?
    ///
    /// # Why the walk asks this
    ///
    /// The monotone cursor ([`Self::next_dirty_addr`]) and the per-object query
    /// ([`Self::any_in_span`]) decide IDENTICALLY — that is a theorem about a
    /// reindexing of this bitset, cross-checked in both this module's tests and
    /// `gc/tests/g1_lane_a_cards.rs` — so choosing between them is a pure
    /// throughput choice and picking the wrong side can never change what a
    /// pause scans.
    ///
    /// Which is cheaper depends on how dirty the region is. The cursor costs
    /// two address compares per object plus one re-derivation per dirty card,
    /// against the query's one-or-two bitset probes per object. The per-object
    /// term favours the cursor always; the re-derivation term grows with the
    /// dirty-card count, and eats the difference once the clean runs the cursor
    /// exists to skip are shorter than the objects stepping over them.
    ///
    /// # Why this and not the summary table
    ///
    /// [`G1CardTable`]'s summary is one CLEAN/MAYBE-DIRTY byte per 64 cards,
    /// not a count — a count would be a read-modify-write on the write barrier,
    /// which is the one thing that module refuses (see its field docs). So the
    /// summary says WHICH chunks to look at and never HOW FULL they are. This
    /// does: the snapshot is a dense bitset the walk has already paid to build,
    /// so the answer is `count_ones` over thirty-two words for a 1 MiB region.
    ///
    /// An EMPTY covered run answers `false` — "there is no region here" is not
    /// "this region is sparse", and a caller that treated it as sparse would be
    /// deciding a policy from a division by zero.
    #[inline]
    pub fn is_at_most_percent_dirty(&self, percent: usize) -> bool {
        if self.len == 0 {
            return false;
        }
        // Cross-multiplied rather than `count * 100 / len`, so the comparison
        // is exact at every size instead of rounding a small region's density
        // to the nearest whole percent.
        self.count() * 100 <= self.len * percent
    }

    #[inline]
    pub fn covered_cards(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic arena: the table never dereferences the addresses it is
    /// given, so a plausible base is enough.
    const BASE: usize = 0x1000_0000;

    /// W5-B — an [`ObjectStart`] over a synthetic address.
    ///
    /// These tests build a table over `BASE`, which names no mapped memory, so
    /// the runtime vouch cannot read a header there — it would not fail, it
    /// would fault. [`ObjectStart::from_asserted_start`] is the hatch for
    /// exactly this, and it is `unsafe` so that reaching for it in production
    /// code is a decision a reviewer sees. The block-offset table's behaviour
    /// under these addresses is otherwise identical to a real producer's:
    /// `from_asserted_start` vouches, so every case below exercises the
    /// installing path.
    fn start(addr: usize) -> ObjectStart<'static> {
        // SAFETY: a synthetic address in a table over memory that does not
        // exist. Nothing dereferences it; see the doc comment above.
        unsafe { ObjectStart::from_asserted_start(addr) }
    }

    #[test]
    fn a_fresh_table_is_entirely_clean() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        assert_eq!(t.card_count(), 4);
        assert!(!t.any_dirty_in(BASE, 4 * G1_CARD_BYTES));
        assert_eq!(t.dirty_cards_in(BASE, 4 * G1_CARD_BYTES), 0);
    }

    #[test]
    fn dirtying_an_address_dirties_exactly_its_own_card() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr(BASE + G1_CARD_BYTES + 17);
        assert!(!t.is_dirty_addr(BASE));
        assert!(t.is_dirty_addr(BASE + G1_CARD_BYTES));
        assert!(t.is_dirty_addr(BASE + 2 * G1_CARD_BYTES - 1));
        assert!(!t.is_dirty_addr(BASE + 2 * G1_CARD_BYTES));
        assert_eq!(t.dirty_cards_in(BASE, 4 * G1_CARD_BYTES), 1);
    }

    /// The per-object screen: an object straddling a card boundary must be
    /// scanned when EITHER card is dirty, because a reference slot anywhere in
    /// it may be the one the barrier recorded.
    #[test]
    fn a_straddling_span_is_dirty_if_any_card_it_touches_is() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr(BASE + 2 * G1_CARD_BYTES);
        // A span ending one byte inside card 2.
        assert!(t.any_dirty_in(BASE + 2 * G1_CARD_BYTES - 8, 9));
        // The same span one byte shorter stops before card 2.
        assert!(!t.any_dirty_in(BASE + 2 * G1_CARD_BYTES - 8, 8));
    }

    #[test]
    fn clearing_a_range_leaves_its_neighbours_alone() {
        let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
        for c in 0..8 {
            t.dirty_addr(BASE + c * G1_CARD_BYTES);
        }
        t.clear_range(BASE + 2 * G1_CARD_BYTES, 3 * G1_CARD_BYTES);
        assert!(t.is_dirty_addr(BASE + G1_CARD_BYTES));
        assert!(!t.is_dirty_addr(BASE + 2 * G1_CARD_BYTES));
        assert!(!t.is_dirty_addr(BASE + 3 * G1_CARD_BYTES));
        assert!(!t.is_dirty_addr(BASE + 4 * G1_CARD_BYTES));
        assert!(t.is_dirty_addr(BASE + 5 * G1_CARD_BYTES));
        assert_eq!(t.dirty_cards_in(BASE, 8 * G1_CARD_BYTES), 5);
    }

    /// Out-of-arena addresses are silently ignored rather than panicking or
    /// aliasing card 0 — the write barrier hands this table whatever the
    /// mutator stored, including addresses in metaspace or off-heap.
    #[test]
    fn addresses_outside_the_arena_touch_nothing() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr(BASE - 1);
        t.dirty_addr(BASE + 4 * G1_CARD_BYTES);
        t.dirty_addr(0);
        assert!(!t.any_dirty_in(BASE, 4 * G1_CARD_BYTES));
        assert!(!t.is_dirty_addr(BASE - 1));
    }

    /// A span that starts before the arena and ends inside it is clamped, not
    /// wrapped: `card_range` must not compute a huge index from an underflow.
    #[test]
    fn a_span_straddling_the_arena_base_is_clamped() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr(BASE);
        assert!(t.any_dirty_in(BASE - 4096, 4096 + 8));
        assert_eq!(t.cards_in(BASE - 4096, 4096), 0);
    }

    // ── card cleaning ───────────────────────────────────────────────────

    #[test]
    fn a_snapshot_reports_what_the_table_held_when_it_was_taken() {
        let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
        t.dirty_addr(BASE + G1_CARD_BYTES);
        t.dirty_addr(BASE + 5 * G1_CARD_BYTES);
        let snap = t.snapshot(BASE, 8 * G1_CARD_BYTES);
        assert_eq!(snap.count(), 2);
        assert_eq!(snap.covered_cards(), 8);
        assert!(snap.any_in_span(BASE + G1_CARD_BYTES, 8));
        assert!(!snap.any_in_span(BASE, 8));

        // The table moving on does not move the snapshot: that independence is
        // the point — the walk decides from the snapshot while it rewrites the
        // table underneath.
        t.dirty_addr(BASE);
        assert!(!snap.any_in_span(BASE, 8));
        assert!(t.is_dirty_addr(BASE));
    }

    #[test]
    fn clean_and_redirty_leaves_exactly_the_kept_cards_dirty() {
        let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
        for c in 0..8 {
            t.dirty_addr(BASE + c * G1_CARD_BYTES);
        }
        let mut keep = t.empty_set(BASE, 8 * G1_CARD_BYTES);
        keep.insert_addr(BASE + 2 * G1_CARD_BYTES + 9);
        keep.insert_addr(BASE + 6 * G1_CARD_BYTES);
        t.clean_and_redirty(BASE, 8 * G1_CARD_BYTES, &keep);

        for c in 0..8 {
            let want = c == 2 || c == 6;
            assert_eq!(t.is_dirty_addr(BASE + c * G1_CARD_BYTES), want, "card {c}");
        }
    }

    /// The bound matters: a walk that stopped early must not clean past where
    /// it stopped, or it drops edges it never looked at.
    #[test]
    fn clean_and_redirty_touches_no_card_past_the_span_it_was_given() {
        let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
        for c in 0..8 {
            t.dirty_addr(BASE + c * G1_CARD_BYTES);
        }
        let keep = t.empty_set(BASE, 4 * G1_CARD_BYTES);
        t.clean_and_redirty(BASE, 4 * G1_CARD_BYTES, &keep);
        for c in 0..4 {
            assert!(
                !t.is_dirty_addr(BASE + c * G1_CARD_BYTES),
                "card {c} cleaned"
            );
        }
        for c in 4..8 {
            assert!(
                t.is_dirty_addr(BASE + c * G1_CARD_BYTES),
                "card {c} untouched"
            );
        }
    }

    // ── the card cursor ────────────────────────────────────────────────

    #[test]
    fn the_cursor_finds_the_next_dirty_card_and_stops_at_the_end() {
        let t = G1CardTable::new(BASE, 256 * G1_CARD_BYTES);
        t.dirty_addr(BASE + 3 * G1_CARD_BYTES);
        t.dirty_addr(BASE + 200 * G1_CARD_BYTES);
        let snap = t.snapshot(BASE, 256 * G1_CARD_BYTES);

        assert_eq!(snap.first_dirty_addr(), Some(BASE + 3 * G1_CARD_BYTES));
        // From inside card 3 the answer is still card 3: the card COVERS the
        // address, so a walk sitting in it has not stepped past it.
        assert_eq!(
            snap.next_dirty_addr(BASE + 3 * G1_CARD_BYTES + 511),
            Some(BASE + 3 * G1_CARD_BYTES)
        );
        // One byte later it has, and the cursor jumps the 196-card clean run
        // in three word loads.
        assert_eq!(
            snap.next_dirty_addr(BASE + 4 * G1_CARD_BYTES),
            Some(BASE + 200 * G1_CARD_BYTES)
        );
        assert_eq!(snap.next_dirty_addr(BASE + 201 * G1_CARD_BYTES), None);
        // Below the covered run means "from the beginning of it", not "wrap".
        assert_eq!(snap.next_dirty_addr(0), Some(BASE + 3 * G1_CARD_BYTES));
    }

    #[test]
    fn an_entirely_clean_run_has_no_cursor() {
        let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
        let snap = t.snapshot(BASE, 8 * G1_CARD_BYTES);
        assert_eq!(snap.first_dirty_addr(), None);
        assert_eq!(snap.next_dirty_addr(BASE), None);
        // An out-of-arena span produces an empty set, which must answer `None`
        // rather than index card 0.
        let miss = t.empty_set(BASE + 64 * G1_CARD_BYTES, 8);
        assert_eq!(miss.covered_cards(), 0);
        assert_eq!(miss.next_dirty_addr(BASE), None);
    }

    /// The cursor's whole reason to exist: it must decide exactly what
    /// `any_in_span` decides, object for object, or it is a screen that can
    /// drop a live cross-region edge. Cross-check the two over every
    /// (start, size) in a small table.
    #[test]
    fn the_cursor_agrees_with_any_in_span_everywhere() {
        let t = G1CardTable::new(BASE, 16 * G1_CARD_BYTES);
        for c in [0usize, 1, 5, 6, 15] {
            t.dirty_addr(BASE + c * G1_CARD_BYTES + 3);
        }
        let snap = t.snapshot(BASE, 16 * G1_CARD_BYTES);
        // Walk a synthetic object grid with several sizes, maintaining the
        // cursor exactly as the region walk does.
        for size in [8usize, 24, 512, 700, 1536] {
            let mut cursor = snap.first_dirty_addr();
            let mut addr = BASE;
            while addr + size <= BASE + 16 * G1_CARD_BYTES {
                if cursor.is_some_and(|d| d + G1_CARD_BYTES <= addr) {
                    cursor = snap.next_dirty_addr(addr);
                }
                let by_cursor = cursor.is_some_and(|d| d < addr + size);
                let by_span = snap.any_in_span(addr, size);
                assert_eq!(by_cursor, by_span, "size={size} addr=+{:#x}", addr - BASE);
                addr += size;
            }
        }
    }

    // ── the summary ────────────────────────────────────────────────────

    /// The invariant, stated directly against the private state: no chunk may
    /// be CLEAN while any card it covers is dirty. Everything the summary saves
    /// rests on this, and violating it under-scans, which is a use-after-free.
    fn assert_summary_invariant(t: &G1CardTable) {
        for (chunk, s) in t.summary.iter().enumerate() {
            if s.load(Ordering::Relaxed) != G1_CARD_CLEAN {
                continue;
            }
            let lo = chunk << G1_CARD_SUMMARY_SHIFT;
            let hi = (lo + G1_CARDS_PER_SUMMARY).min(t.cards.len());
            for i in lo..hi {
                assert_eq!(
                    t.cards[i].load(Ordering::Relaxed),
                    G1_CARD_CLEAN,
                    "chunk {chunk} claims clean but card {i} is dirty"
                );
            }
        }
    }

    /// The screen must answer exactly what a naive per-card scan answers, and
    /// the summary must stay sound across all three writers.
    #[test]
    fn the_summary_never_hides_a_dirty_card() {
        const N: usize = 5 * G1_CARDS_PER_SUMMARY + 7;
        let t = G1CardTable::new(BASE, N * G1_CARD_BYTES);
        assert_summary_invariant(&t);

        // A spread that crosses chunk boundaries unevenly.
        for c in (0..N).step_by(37) {
            t.dirty_addr(BASE + c * G1_CARD_BYTES + 5);
        }
        assert_summary_invariant(&t);
        let naive = |from: usize, to: usize| {
            (from..to).any(|i| t.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN)
        };
        for start in (0..N).step_by(3) {
            for span_cards in [1usize, 2, 63, 64, 65, 200, N] {
                let end = (start + span_cards).min(N);
                assert_eq!(
                    t.any_dirty_in(BASE + start * G1_CARD_BYTES, (end - start) * G1_CARD_BYTES),
                    naive(start, end),
                    "start={start} span={span_cards}"
                );
            }
        }
        // The snapshot must see the same set the table holds.
        let snap = t.snapshot(BASE, N * G1_CARD_BYTES);
        assert_eq!(snap.count(), t.dirty_card_total());

        // Cleaning re-derives the summary rather than leaving it stale, or the
        // screen never gets faster after a clean.
        let keep = t.empty_set(BASE, N * G1_CARD_BYTES);
        t.clean_and_redirty(BASE, N * G1_CARD_BYTES, &keep);
        assert_summary_invariant(&t);
        assert_eq!(t.dirty_card_total(), 0);
        assert!(
            t.summary
                .iter()
                .all(|s| s.load(Ordering::Relaxed) == G1_CARD_CLEAN),
            "a fully cleaned table must leave no chunk marked dirty"
        );
        assert!(!t.any_dirty_in(BASE, N * G1_CARD_BYTES));
    }

    /// A partial clean must not mark a chunk clean on the strength of the half
    /// of it that was cleaned: `refresh_summary` reads the WHOLE chunk.
    #[test]
    fn a_partial_clean_keeps_a_straddling_chunk_dirty() {
        const N: usize = 2 * G1_CARDS_PER_SUMMARY;
        let t = G1CardTable::new(BASE, N * G1_CARD_BYTES);
        // One card in the first half of chunk 0, one in the second half.
        t.dirty_addr(BASE + 3 * G1_CARD_BYTES);
        t.dirty_addr(BASE + 40 * G1_CARD_BYTES);
        // Clean only the first 20 cards — a strict subset of chunk 0.
        let keep = t.empty_set(BASE, 20 * G1_CARD_BYTES);
        t.clean_and_redirty(BASE, 20 * G1_CARD_BYTES, &keep);
        assert_summary_invariant(&t);
        assert!(!t.is_dirty_addr(BASE + 3 * G1_CARD_BYTES));
        assert!(t.is_dirty_addr(BASE + 40 * G1_CARD_BYTES));
        assert!(t.any_dirty_in(BASE, N * G1_CARD_BYTES));
        // ... and `clear_range` over a strict subset behaves the same way.
        let t2 = G1CardTable::new(BASE, N * G1_CARD_BYTES);
        t2.dirty_addr(BASE + 3 * G1_CARD_BYTES);
        t2.dirty_addr(BASE + 40 * G1_CARD_BYTES);
        t2.clear_range(BASE, 20 * G1_CARD_BYTES);
        assert_summary_invariant(&t2);
        assert!(t2.any_dirty_in(BASE, N * G1_CARD_BYTES));
        assert_eq!(t2.dirty_card_total(), 1);
    }

    #[test]
    fn the_dirty_total_is_the_saturation_numerator() {
        let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
        assert_eq!(t.dirty_card_total(), 0);
        t.dirty_addr(BASE);
        t.dirty_addr(BASE + 7 * G1_CARD_BYTES);
        assert_eq!(t.dirty_card_total(), 2);
        assert_eq!(t.card_count(), 8);
        t.clear_range(BASE, 8 * G1_CARD_BYTES);
        assert_eq!(t.dirty_card_total(), 0);
    }

    #[test]
    fn a_span_straddling_two_cards_is_kept_by_either_of_them() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        let mut keep = t.empty_set(BASE, 4 * G1_CARD_BYTES);
        keep.insert_addr(BASE + 2 * G1_CARD_BYTES);
        assert!(keep.any_in_span(BASE + 2 * G1_CARD_BYTES - 8, 9));
        assert!(!keep.any_in_span(BASE + 2 * G1_CARD_BYTES - 8, 8));
        assert!(!keep.is_empty());
    }

    // ---------------------------------------------------------------------
    // W4-A — the block-offset table
    // ---------------------------------------------------------------------

    #[test]
    fn a_fresh_block_offset_table_names_no_entry_anywhere() {
        let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
        for c in 0..8 {
            assert_eq!(
                t.block_entry_addr(BASE + c * G1_CARD_BYTES),
                None,
                "card {c} claimed an entry before any producer named one"
            );
        }
        assert_eq!(t.block_entries_in(BASE, 8 * G1_CARD_BYTES), 0);
    }

    #[test]
    fn a_producer_that_names_an_object_start_makes_that_card_enterable() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        let obj = BASE + G1_CARD_BYTES + 64;
        t.dirty_addr_at_object_start(start(obj));
        assert!(t.is_dirty_addr(obj), "the card must still be dirtied");
        // The entry is the address the producer named, not the card's base and
        // not a rounded approximation of it: the walk DECODES an object header
        // there, so anything but the exact address is a mis-entry.
        assert_eq!(t.block_entry_addr(obj), Some(obj));
        // Any address inside the card asks the same question.
        assert_eq!(t.block_entry_addr(BASE + G1_CARD_BYTES), Some(obj));
        assert_eq!(
            t.block_entry_addr(BASE + 2 * G1_CARD_BYTES - 1),
            Some(obj),
            "the last byte of the card belongs to the same card"
        );
        // And only that card.
        assert_eq!(t.block_entry_addr(BASE), None);
        assert_eq!(t.block_entry_addr(BASE + 2 * G1_CARD_BYTES), None);
        assert_eq!(t.block_entries_in(BASE, 4 * G1_CARD_BYTES), 1);
    }

    /// THE ONE THAT MATTERS. Two objects start in one card and both take a
    /// cross-region store; the table must name the LOWER of them however the
    /// stores are ordered.
    ///
    /// A plain last-writer-wins byte passes this in one order and fails it in
    /// the other, and the failure is not an over-scan: the source walk enters
    /// at the higher object and never looks at the lower one, whose reference
    /// into the collection set is then a slot left pointing at a freed region.
    /// That is why [`G1CardTable::dirty_addr_at_object_start`] merges with an
    /// atomic minimum and not with a store.
    #[test]
    fn a_card_with_two_named_object_starts_is_entered_at_the_lower_one() {
        let lo = BASE + G1_CARD_BYTES + 32;
        let hi = BASE + G1_CARD_BYTES + 256;

        // Ascending: the later, higher store must not raise the entry.
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr_at_object_start(start(lo));
        t.dirty_addr_at_object_start(start(hi));
        assert_eq!(
            t.block_entry_addr(lo),
            Some(lo),
            "a later store into a HIGHER object raised the entry past an object \
             that still holds a cross-region reference"
        );

        // Descending: the same answer, reached by the branch that does take the
        // read-modify-write.
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr_at_object_start(start(hi));
        t.dirty_addr_at_object_start(start(lo));
        assert_eq!(t.block_entry_addr(lo), Some(lo));
    }

    /// The monotone minimum has to survive repetition, which is the steady
    /// state on the barrier: the same holder takes a store every round.
    #[test]
    fn repeating_a_store_never_moves_the_entry() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        let lo = BASE + 8;
        let hi = BASE + 400;
        t.dirty_addr_at_object_start(start(lo));
        for _ in 0..100 {
            t.dirty_addr_at_object_start(start(hi));
            t.dirty_addr_at_object_start(start(lo));
        }
        assert_eq!(t.block_entry_addr(BASE), Some(lo));
    }

    /// An object starting exactly at a card's base is offset ZERO, which is a
    /// legal value and must not be read as "unknown". This is why the sentinel
    /// is `0xFF`.
    #[test]
    fn an_object_at_the_card_base_is_a_real_entry_and_not_unknown() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        let at_base = BASE + 2 * G1_CARD_BYTES;
        t.dirty_addr_at_object_start(start(at_base));
        assert_eq!(t.block_entry_addr(at_base), Some(at_base));
    }

    /// The conservative producer leaves no entry. It cannot: it was given an
    /// address it does not know to be an object start, and the reader's only
    /// safe answer to "I do not know" is the linear walk.
    #[test]
    fn the_conservative_producer_dirties_the_card_and_names_no_entry() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        let a = BASE + G1_CARD_BYTES + 128;
        t.dirty_addr(a);
        assert!(t.is_dirty_addr(a));
        assert_eq!(t.block_entry_addr(a), None);
    }

    /// AN ARENA BASE THAT IS NOT CARD-ALIGNED. The card index is relative to
    /// `base`, so a block offset must be too; measuring it against the absolute
    /// address instead names an address inside the right card at the wrong
    /// offset, which is a mis-entry.
    ///
    /// Today's arena base is an `mmap`/`VirtualAlloc` reservation and is
    /// page-aligned, so the two forms agree and the bug this catches would be
    /// latent. `G1CardTable`'s own type docs are explicit that region-to-card
    /// alignment holds "whatever `base`'s own alignment is", and
    /// `jit/src/x64/objects.rs` says of the same subtraction that being correct
    /// for ANY base "is the property to preserve". This test is what preserves
    /// it here.
    #[test]
    fn a_base_that_is_not_card_aligned_still_names_the_exact_object() {
        // Deliberately not a multiple of `G1_CARD_BYTES`, and not a multiple of
        // 64 either, so neither the card nor the summary shift can hide it.
        const ODD_BASE: usize = 0x1000_0000 + 8;
        let t = G1CardTable::new(ODD_BASE, 8 * G1_CARD_BYTES);
        // Three objects spread across the covered run, one of them in the first
        // partial card and one just past a card boundary relative to the base.
        for obj in [
            ODD_BASE,
            ODD_BASE + 24,
            ODD_BASE + G1_CARD_BYTES,
            ODD_BASE + 3 * G1_CARD_BYTES + 128,
        ] {
            let t = G1CardTable::new(ODD_BASE, 8 * G1_CARD_BYTES);
            t.dirty_addr_at_object_start(start(obj));
            assert_eq!(
                t.block_entry_addr(obj),
                Some(obj),
                "an odd arena base moved the entry off the object it names"
            );
        }
        // And the minimum still resolves within one card when the card's own
        // run is offset from the address space's.
        let lo = ODD_BASE + G1_CARD_BYTES + 16;
        let hi = ODD_BASE + G1_CARD_BYTES + 400;
        t.dirty_addr_at_object_start(start(hi));
        t.dirty_addr_at_object_start(start(lo));
        assert_eq!(t.block_entry_addr(hi), Some(lo));
    }

    /// An unaligned address is not an object start anywhere in this VM. It must
    /// not be truncated to the grain, because truncation names an address BELOW
    /// the real object — the one direction that mis-enters a walk.
    #[test]
    fn an_unaligned_address_records_no_entry_and_disarms() {
        let _guard = ENFORCEMENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_block_offset_enforcement_for_tests();
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        let a = BASE + G1_CARD_BYTES + 37;
        t.dirty_addr_at_object_start(start(a));
        assert!(t.is_dirty_addr(a), "the card is still dirtied");
        assert_eq!(
            t.block_entry_addr(a),
            None,
            "an unaligned address was rounded down into an entry"
        );
        // W5-B — and it DISARMS. Refusing the offset without disarming is the
        // hole `BLOCK_OFFSETS_DISARMED` exists for: a second object starting
        // in this card would leave the card's surviving entry above this one,
        // and the walk would jump over it.
        assert!(
            block_offsets_disarmed(),
            "an unaligned producer address left the jump armed"
        );
        reset_block_offset_enforcement_for_tests();
    }

    /// REGION RECYCLE IS THE ONE EVENT THAT INVALIDATES AN ENTRY, because it is
    /// the one event that changes which addresses are object starts. If this
    /// ever stops holding, the source walk decodes a header in bytes that a
    /// reset zero-filled and a new incarnation has laid out differently.
    #[test]
    fn clearing_a_range_forgets_its_block_entries() {
        let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
        let inside = BASE + 2 * G1_CARD_BYTES + 64;
        let outside = BASE + 6 * G1_CARD_BYTES + 64;
        t.dirty_addr_at_object_start(start(inside));
        t.dirty_addr_at_object_start(start(outside));
        t.clear_range(BASE, 4 * G1_CARD_BYTES);
        assert_eq!(t.block_entry_addr(inside), None);
        assert_eq!(
            t.block_entry_addr(outside),
            Some(outside),
            "clearing one region's range forgot a neighbour's entry"
        );
    }

    /// Card CLEANING moves no object, so it must not forget an entry — the
    /// whole value of the table is that it survives from the pause that
    /// recorded it to the pause that uses it.
    #[test]
    fn cleaning_a_card_keeps_its_block_entry() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        let obj = BASE + G1_CARD_BYTES + 64;
        t.dirty_addr_at_object_start(start(obj));
        let empty = t.empty_set(BASE, 4 * G1_CARD_BYTES);
        t.clean_and_redirty(BASE, 4 * G1_CARD_BYTES, &empty);
        assert!(!t.is_dirty_addr(obj), "the card was supposed to go clean");
        assert_eq!(
            t.block_entry_addr(obj),
            Some(obj),
            "card cleaning forgot a block entry it had no reason to"
        );
        // And a store that re-dirties the card can only pull the entry DOWN.
        let lower = BASE + G1_CARD_BYTES + 16;
        t.dirty_addr_at_object_start(start(lower));
        assert_eq!(t.block_entry_addr(obj), Some(lower));
    }

    /// Addresses outside the arena touch nothing here either — the twin of
    /// `addresses_outside_the_arena_touch_nothing` for the new table.
    #[test]
    fn a_block_entry_outside_the_arena_is_none() {
        let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
        t.dirty_addr_at_object_start(start(BASE - 8));
        t.dirty_addr_at_object_start(start(BASE + 4 * G1_CARD_BYTES));
        assert_eq!(t.block_entry_addr(BASE - 8), None);
        assert_eq!(t.block_entry_addr(BASE + 4 * G1_CARD_BYTES), None);
        assert_eq!(t.block_entries_in(BASE, 4 * G1_CARD_BYTES), 0);
    }

    // ----------------------------------------------------------------------
    // W5-B — the enforcement: the runtime vouch and the global disarm.
    //
    // These four are the only tests in this file that need REAL memory, and
    // they need it for the reason the whole mechanism exists: the vouch reads
    // sixteen bytes and asks the allocator's own bit whether they are a
    // published header. A synthetic base cannot answer that question.
    //
    // [`BLOCK_OFFSETS_DISARMED`] is a process-wide latch, so the two cases
    // that deliberately trip it would otherwise poison every other test in
    // this binary — cargo runs them on several threads in one process. They
    // take a mutex and reset the latch on the way in.
    // ----------------------------------------------------------------------

    /// Serialises the tests that trip the global disarm latch.
    static ENFORCEMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A miniature arena with REAL object headers in it, at addresses this
    /// test file chooses rather than at whatever the allocator hands back.
    ///
    /// Four cards of backing store, card-aligned, with a fresh
    /// `ObjectHeader::new` written at each requested card-relative offset of
    /// the SECOND card. Controlling the addresses is what lets the
    /// two-objects-in-one-card case below be arithmetic rather than luck: a
    /// `Box<ObjectHeader>` lands wherever the allocator puts it, which may be
    /// eight bytes below a card boundary.
    struct MiniArena {
        _backing: Vec<u64>,
        base: usize,
        table: G1CardTable,
    }

    impl MiniArena {
        fn new() -> Self {
            // Six cards of store so that a four-card, card-ALIGNED arena fits
            // inside it wherever the allocator lands.
            let backing = vec![0u64; 6 * G1_CARD_BYTES / 8];
            let raw = backing.as_ptr() as usize;
            let base = (raw + G1_CARD_BYTES - 1) & !(G1_CARD_BYTES - 1);
            let table = G1CardTable::new(base, 4 * G1_CARD_BYTES);
            Self {
                _backing: backing,
                base,
                table,
            }
        }

        /// Publish a real object header at `base + off` and return its address.
        fn put_header(&self, off: usize) -> usize {
            let addr = self.base + off;
            // SAFETY: `off` is inside the four cards the arena owns and is
            // 8-aligned, and the backing store outlives every use.
            unsafe {
                std::ptr::write(
                    addr as *mut ObjectHeader,
                    ObjectHeader::new(
                        cratonvm_types::ClassId::new(0),
                        cratonvm_types::ObjectKind::Object,
                        cratonvm_types::ArrayElementType::Reference,
                        0,
                        0,
                    ),
                );
            }
            addr
        }

        /// The published header at `addr`, as the producers hold it.
        fn header_at(&self, addr: usize) -> &ObjectHeader {
            // SAFETY: `put_header` published one there.
            unsafe { &*(addr as *const ObjectHeader) }
        }
    }

    /// The ordinary case: `ObjectHeader::new` sets `GC_FLAG_HEADER`, so a
    /// producer naming a real object start records its entry and nothing is
    /// disarmed.
    #[test]
    fn a_real_object_header_vouches_and_its_entry_is_recorded() {
        let _guard = ENFORCEMENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_block_offset_enforcement_for_tests();
        let a = MiniArena::new();
        let addr = a.put_header(G1_CARD_BYTES + 128);
        a.table
            .dirty_addr_at_object_start(ObjectStart::of_header(a.header_at(addr)));
        assert!(a.table.is_dirty_addr(addr));
        assert_eq!(a.table.block_entry_addr(addr), Some(addr));
        // The census is read as a DELTA around this call, not as an absolute.
        // `vouch_checks` is a process-global counter and `ENFORCEMENT_LOCK`
        // only serialises the tests that know about it -- every OTHER test in
        // this binary that dirties an object start bumps it too, on libtest's
        // other threads. Asserting `== 1` therefore passed alone and failed in
        // the full suite: an assertion about test ordering wearing the costume
        // of an assertion about the vouch.
        let (checks, unvouched, disarmed) = block_offset_enforcement_census();
        assert!(
            checks >= 1,
            "a real header must be vouched, saw checks={checks}"
        );
        assert_eq!(unvouched, 0, "a real header must not be counted unvouched");
        assert!(!disarmed, "a real header must not disarm the jump");
        reset_block_offset_enforcement_for_tests();
    }

    /// The case the enforcement exists for. Sixteen bytes without
    /// `GC_FLAG_HEADER` are not an object start whatever the caller believed,
    /// so: the card IS dirtied (a remembered set may only over-remember), NO
    /// entry is recorded, the address is counted, and the jump is disarmed for
    /// the whole process.
    #[test]
    fn a_producer_naming_bytes_without_the_header_flag_records_nothing_and_disarms() {
        let _guard = ENFORCEMENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_block_offset_enforcement_for_tests();
        let a = MiniArena::new();
        let addr = a.put_header(G1_CARD_BYTES + 128);
        // Exactly what a slot address looks like to the vouch: bytes that are
        // not a published header.
        a.header_at(addr).set_gc_flags(0);
        a.table
            .dirty_addr_at_object_start(ObjectStart::of_header(a.header_at(addr)));
        assert!(
            a.table.is_dirty_addr(addr),
            "the card must still be dirty — refusing the OFFSET may not refuse the EDGE"
        );
        assert_eq!(
            a.table.block_entry_addr(addr),
            None,
            "an unvouched address must not become a walk entry"
        );
        // The census is read as a DELTA, not as an absolute. `vouch_checks`
        // and `unvouched` are process-global and `ENFORCEMENT_LOCK` only
        // serialises the tests that KNOW about it -- every other test in this
        // binary that dirties an object start bumps them too, on libtest's
        // other threads. Asserting `== (1, 1, true)` therefore passed when run
        // alone and failed in the full suite: an assertion about test ordering
        // wearing the costume of an assertion about the refusal. This is the
        // same fix the sibling test above already carries.
        let (checks, unvouched, disarmed) = block_offset_enforcement_census();
        assert!(checks >= 1, "the refusal must be counted, saw checks={checks}");
        assert!(
            unvouched >= 1,
            "an unvouched producer must be counted unvouched, saw {unvouched}"
        );
        assert!(disarmed, "an unvouched producer must disarm the jump");
        assert!(block_offsets_disarmed());
        reset_block_offset_enforcement_for_tests();
    }

    /// The disarm is what makes "do not record" SOUND, and this is the shape
    /// that proves it is needed.
    ///
    /// Two objects start in one card. The lower one's producer does not vouch,
    /// so its offset is dropped; the higher one's does. Without the global
    /// disarm the card's entry would name the HIGHER object and the walk would
    /// jump straight past the lower one — a dropped remembered-set edge, which
    /// is a use-after-free. The entry is indeed the higher one; the latch is
    /// what stops anybody acting on it.
    #[test]
    fn dropping_an_unvouched_offset_would_leave_a_higher_entry_which_is_why_it_disarms() {
        let _guard = ENFORCEMENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_block_offset_enforcement_for_tests();
        let a = MiniArena::new();
        // Two object starts in ONE card, 64 bytes apart.
        let lower_addr = a.put_header(G1_CARD_BYTES + 64);
        let higher_addr = a.put_header(G1_CARD_BYTES + 128);
        a.header_at(lower_addr).set_gc_flags(0);

        a.table
            .dirty_addr_at_object_start(ObjectStart::of_header(a.header_at(lower_addr)));
        a.table
            .dirty_addr_at_object_start(ObjectStart::of_header(a.header_at(higher_addr)));

        assert_eq!(
            a.table.block_entry_addr(lower_addr),
            Some(higher_addr),
            "this is the hole: the surviving entry is ABOVE the object whose offset was dropped"
        );
        assert!(
            block_offsets_disarmed(),
            "and this is what closes it: nothing may read that entry again"
        );
        reset_block_offset_enforcement_for_tests();
    }

    /// `ObjectStart::of_header` derives the address FROM the reference, so a
    /// producer cannot pass a header and an unrelated address. This is the
    /// compile-time half of the enforcement, stated as a test because there is
    /// no way to state it as one in the type system's own words.
    #[test]
    fn an_object_starts_address_is_the_headers_own_address() {
        let a = MiniArena::new();
        let addr = a.put_header(G1_CARD_BYTES + 256);
        let at = ObjectStart::of_header(a.header_at(addr));
        assert_eq!(at.addr(), addr);
        assert!(at.vouched());
        a.header_at(addr).set_gc_flags(0);
        assert!(!ObjectStart::of_header(a.header_at(addr)).vouched());
    }

    /// The cursor and the table compose into the thing the walk actually does:
    /// find the next dirty card, then enter its grid. Exercised over a grid
    /// whose object size is known, so the expected answer is arithmetic rather
    /// than a re-derivation of the code under test.
    #[test]
    fn the_cursor_and_the_table_together_name_a_real_object_start() {
        const OBJ: usize = 64;
        let cards = 16;
        let t = G1CardTable::new(BASE, cards * G1_CARD_BYTES);
        // An object grid of 64-byte objects over the whole span, with a
        // cross-region store into every eleventh one.
        let objects = cards * G1_CARD_BYTES / OBJ;
        let mut named = Vec::new();
        for o in (0..objects).step_by(11) {
            let a = BASE + o * OBJ;
            t.dirty_addr_at_object_start(start(a));
            named.push(a);
        }
        let snap = t.snapshot(BASE, cards * G1_CARD_BYTES);

        // Walk the way the source walk does: from the region base, hop to the
        // next dirty card, enter it, and confirm the entry is one of the
        // addresses a producer actually named and is the LOWEST such address in
        // its card.
        let mut here = BASE;
        let mut entered = 0usize;
        while let Some(d) = snap.next_dirty_addr(here) {
            let entry = t
                .block_entry_addr(d)
                .expect("a dirty card with no entry, but every producer named a start");
            assert!(
                named.contains(&entry),
                "entry 0x{entry:x} is not an address any producer named"
            );
            let card_lo = d;
            let card_hi = d + G1_CARD_BYTES;
            let lowest = named
                .iter()
                .copied()
                .filter(|&a| a >= card_lo && a < card_hi)
                .min()
                .unwrap();
            assert_eq!(entry, lowest, "the entry was not the card's lowest start");
            entered += 1;
            here = card_hi;
            if here >= BASE + cards * G1_CARD_BYTES {
                break;
            }
        }
        assert!(entered > 0, "the fixture produced no dirty card at all");
    }
}
