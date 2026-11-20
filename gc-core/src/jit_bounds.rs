use std::sync::Arc;
pub use cratonvm_types::jit_g1_young::{JIT_G1_YOUNG, JIT_G1_YOUNG_CONTENDED};
use parking_lot::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

pub struct JitReadBoundsTable {
    pub words: [AtomicUsize; 6],
}

pub static JIT_READ_BOUNDS: JitReadBoundsTable = JitReadBoundsTable {
    words: [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ],
};

/// Address of [`JIT_READ_BOUNDS`] for the JIT helpers table
/// (`JitRuntimeHelpers::read_bounds_addr`).
pub fn jit_read_bounds_addr() -> usize {
    &JIT_READ_BOUNDS as *const _ as usize
}

// ---------------------------------------------------------------------------
// F-08 - G1's own JIT barrier table
// ---------------------------------------------------------------------------

/// Process-global description of G1's heap geometry, for the JIT's inline G1
/// post-write barrier (F-08).
///
/// # Why a THIRD table, and not either of the two above
///
/// [`JIT_REGION_BOUNDS`]'s emptiness under G1 is load-bearing: it is what
/// closes defect G1-2 (`g1-audit.md` 8.1), by making every
/// generational-style barrier-free inline reference store unreachable there.
/// Nothing may be written into it for G1, ever, and the sibling
/// [`JIT_READ_BOUNDS`] exists precisely because the previous attempt to reuse
/// one table for two questions would have unblocked those stores. This is the
/// same lesson a third time: the question here is neither "is this address
/// mapped" nor "may an inline store skip the barrier" (the answer to which
/// stays NO) but "what are the numbers an inline barrier needs to DO its job".
///
/// # Layout
///
/// * `[0]` `arena_base` - G1's single contiguous arena base.
/// * `[1]` `arena_len` - its length in bytes. Doubles as the liveness flag:
///   zero means no G1 collector has published, and the emitter then emits no
///   inline barrier at all.
/// * `[2]` `region_mask` - `!(region_size - 1)`. The same-region test is
///   `((obj - arena_base) ^ (val - arena_base)) & region_mask == 0`, which is
///   `(obj - base) >> region_shift == (val - base) >> region_shift` written
///   without a shift. A MASK rather than a shift because the emitted sequence
///   then needs no `CL` and no variable-shift encoding.
///
///   The base subtraction is not decoration. G1's arena comes from a `Vec<u8>`
///   and is only malloc-aligned, so an aligned `region_size` block of the
///   address space is NOT a region: exactly one region boundary falls inside
///   each such block. Testing `(obj ^ val) & region_mask` without subtracting
///   the base would call two addresses straddling that boundary "same region",
///   skip the barrier, and lose the edge - the use-after-free this barrier
///   exists to prevent.
/// * `[3]` `card_table_base`, `[4]` `card_shift` - the F-05 card table
///   (`crate::g1_cards`). **Published but not read by any emitter today**, and
///   deliberately so rather than by oversight: dirtying a card inline saves
///   nothing while the REMEMBERED-SET ENTRY still has to be recorded, because
///   that entry is a hash-map insert keyed on a (source, target) region pair
///   and there is no inline form of it. The inline barrier therefore filters
///   (same region, null store) and calls out for everything else, and the
///   callee dirties the card on the way through. These two words are what a
///   future all-inline barrier would need, and it would additionally need
///   Phase 2 to take its source set from the card table rather than from the
///   region-index remembered set - which is a policy change, not an emitter
///   change. See `feature`-level discussion in the F-08 commit.
///
/// All-zero means "no G1 collector is live", which is the state under every
/// other backend and before construction, and the emitter degrades to the
/// helper call it makes today.
///
/// # One owner per process, and the POISONED state (r23 wave 3 review)
///
/// The emitters bake the table's ADDRESS, check liveness (`[1]`) only at
/// compile time (`licm::g1_barrier_table_live`), and load `[0]` and `[2]` at
/// RUN time. A table that a second G1 collector overwrote, or that a dropped
/// collector zeroed, is therefore read by the other collector's compiled code:
/// a zero mask answers "same region" for every store (no remembered-set edge
/// at all), and a foreign base that is not congruent to its own modulo the
/// region size (the arena is not region-aligned) misjudges pairs straddling a
/// region boundary. Retired page:
/// `docs/feature-designs/concurrent-gc-maturation.md`.
///
/// So the table is CLAIMED ([`JIT_G1_BARRIER_OWNER`], compare-and-swap from
/// `0` to the arena base). A second claimant, or a teardown, POISONS it
/// instead of overwriting or zeroing it: `[1] = 0` (no new compile emits an
/// inline arm), then `[2] = usize::MAX`, then `[0] = 0`. Poisoned, every
/// run-time same-region test in the emitters -- `((obj - base) ^ (val -
/// base)) & mask` (`x64::objects::emit_g1_barrier_filter`,
/// `ir_lower::emit_g1_ir_ref_putfield`, `ir_lower::g1_aastore_screen_bytes`)
/// and `((a - base) ^ (s - base)) & mask` (`op_invoke` /
/// `concat_emit` `emit_*g1_same_region_check`) -- is zero only for `obj ==
/// val`, the same object, which needs no edge. The mask is written BEFORE the
/// base: every sequence loads the base before the mask, so a reader that sees
/// the poisoned base also sees the poisoned mask, and the pair (zero base, old
/// mask) -- a raw, base-less same-region test -- is never observable. A second
/// claimant leaves the owner word [`JIT_G1_BARRIER_CONTENDED`] for the rest of
/// the process; every compiled store then calls `jit_g1_post_write_barrier`,
/// which resolves its collector from `vm_ptr`.
#[repr(C)]
pub struct JitG1BarrierTable {
    pub words: [AtomicUsize; 5],
}

pub static JIT_G1_BARRIER: JitG1BarrierTable = JitG1BarrierTable {
    words: [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ],
};

/// Address of [`JIT_G1_BARRIER`] for the JIT helpers table
/// (`JitRuntimeHelpers::g1_barrier_addr`).
pub fn jit_g1_barrier_addr() -> usize {
    &JIT_G1_BARRIER as *const _ as usize
}

/// The claim on [`JIT_G1_BARRIER`]: `0` free, the owning collector's arena
/// base, or [`JIT_G1_BARRIER_CONTENDED`] (sticky). See the table's "One owner
/// per process".
pub static JIT_G1_BARRIER_OWNER: AtomicUsize = AtomicUsize::new(0);

/// [`JIT_G1_BARRIER_OWNER`] once two G1 collectors have claimed the table in
/// one process: the table stays poisoned for the rest of the process.
pub const JIT_G1_BARRIER_CONTENDED: usize = usize::MAX;

/// The POISONED table's region mask: with a zero base, the emitters'
/// same-region test is zero only for `obj == val`.
pub const JIT_G1_BARRIER_POISON_MASK: usize = usize::MAX;

/// Publish G1's geometry. Called once from `G1Collector::new`.
///
/// Claims the table first; a table another live collector holds (or one
/// already contended) is POISONED instead and stays so (see the table's "One
/// owner per process"). `arena_len` is stored LAST, so a reader that sees a
/// non-zero length has necessarily seen the other four words: the length is
/// the liveness flag and it must not become visible ahead of the numbers it
/// vouches for. The claimant re-checks its claim after the length store; a
/// poison that raced the fill is re-applied, so whichever runs last leaves
/// the table poisoned (SeqCst throughout).
pub fn publish_jit_g1_barrier(
    arena_base: usize,
    arena_len: usize,
    region_mask: usize,
    card_table_base: usize,
    card_shift: u32,
) {
    publish_jit_g1_barrier_in(
        &JIT_G1_BARRIER.words,
        &JIT_G1_BARRIER_OWNER,
        arena_base,
        arena_len,
        region_mask,
        card_table_base,
        card_shift,
    );
}

/// [`publish_jit_g1_barrier`] against any table and owner word (unit tests
/// drive the protocol on their own pair; the process statics are shared by
/// every collector a test binary builds).
pub fn publish_jit_g1_barrier_in(
    words: &[AtomicUsize; 5],
    owner: &AtomicUsize,
    arena_base: usize,
    arena_len: usize,
    region_mask: usize,
    card_table_base: usize,
    card_shift: u32,
) {
    if arena_base == 0 || arena_base == JIT_G1_BARRIER_CONTENDED {
        return;
    }
    if owner
        .compare_exchange(0, arena_base, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // Owned by another live collector, or already contended.
        owner.store(JIT_G1_BARRIER_CONTENDED, Ordering::SeqCst);
        poison_jit_g1_barrier_words(words);
        return;
    }
    words[0].store(arena_base, Ordering::SeqCst);
    words[2].store(region_mask, Ordering::SeqCst);
    words[3].store(card_table_base, Ordering::SeqCst);
    words[4].store(card_shift as usize, Ordering::SeqCst);
    words[1].store(arena_len, Ordering::SeqCst);
    if owner.load(Ordering::SeqCst) != arena_base {
        // A second collector poisoned the table while this one filled it.
        poison_jit_g1_barrier_words(words);
    }
}

/// Poison the table (see "One owner per process"): the length first (no new
/// compile emits an inline arm), then the mask, then the base, so no reader
/// pairs the zero base with a real mask.
pub fn poison_jit_g1_barrier_words(words: &[AtomicUsize; 5]) {
    words[1].store(0, Ordering::SeqCst);
    words[2].store(JIT_G1_BARRIER_POISON_MASK, Ordering::SeqCst);
    words[0].store(0, Ordering::SeqCst);
    words[3].store(0, Ordering::SeqCst);
    words[4].store(0, Ordering::SeqCst);
}

/// Owner-checked teardown, for exactly the reason
/// [`clear_jit_read_bounds_owned_by`] is owner-checked: a dropped collector
/// must not leave geometry behind that a later one would be described by, and
/// must not clobber a LIVE collector's geometry when two exist (embedding,
/// unit tests).
///
/// The owner POISONS rather than zeroes (a zero mask would answer "same
/// region" for every store of any code still reading the table), then gives
/// the claim back -- unless a second collector marked it contended, which
/// stays for the process. A non-owner does nothing.
pub fn clear_jit_g1_barrier_owned_by(owned_base: usize) {
    clear_jit_g1_barrier_owned_by_in(&JIT_G1_BARRIER.words, &JIT_G1_BARRIER_OWNER, owned_base);
}

/// [`clear_jit_g1_barrier_owned_by`] against any table and owner word.
pub fn clear_jit_g1_barrier_owned_by_in(
    words: &[AtomicUsize; 5],
    owner: &AtomicUsize,
    owned_base: usize,
) {
    if owned_base == 0 || owner.load(Ordering::SeqCst) != owned_base {
        return;
    }
    poison_jit_g1_barrier_words(words);
    let _ = owner.compare_exchange(owned_base, 0, Ordering::SeqCst, Ordering::SeqCst);
}

/// Publish [`JIT_G1_YOUNG`] for the collector whose arena starts at
/// `arena_base`: claim the table, fill it, the shift (the liveness word) last.
/// A table another live collector holds is POISONED instead (see the table's
/// "One owner per process"). SeqCst throughout: the claimant's re-check after
/// its shift store and the poisoner's claim-then-shift stores must not pass
/// each other, so whichever runs last leaves the shift at `0`.
pub fn publish_jit_g1_young_with_layout(regions_ptr: usize, arena_base: usize, region_shift: u32, count: usize, region_stride: usize, region_type_offset: usize, region_young_max: u8) {
    use cratonvm_types::jit_g1_young::{
        W_BASE, W_CLAIM, W_COUNT, W_REGIONS, W_SHIFT, W_STRIDE, W_TYPE_OFFSET, W_YOUNG_MAX,
    };
    if region_shift == 0 || regions_ptr == 0 || count == 0 || arena_base == 0 {
        return;
    }
    let words = &JIT_G1_YOUNG.words;
    let claim = &words[W_CLAIM];
    if claim
        .compare_exchange(0, arena_base, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // Owned by another live collector, or already contended.
        claim.store(JIT_G1_YOUNG_CONTENDED, Ordering::SeqCst);
        words[W_SHIFT].store(0, Ordering::SeqCst);
        return;
    }
    words[W_REGIONS].store(regions_ptr, Ordering::SeqCst);
    words[W_BASE].store(arena_base, Ordering::SeqCst);
    words[W_COUNT].store(count, Ordering::SeqCst);
    words[W_STRIDE].store(region_stride, Ordering::SeqCst);
    words[W_TYPE_OFFSET].store(region_type_offset, Ordering::SeqCst);
    words[W_YOUNG_MAX].store(usize::from(region_young_max), Ordering::SeqCst);
    words[W_SHIFT].store(region_shift as usize, Ordering::SeqCst);
    if claim.load(Ordering::SeqCst) != arena_base {
        // A second collector poisoned the table while this one filled it.
        words[W_SHIFT].store(0, Ordering::SeqCst);
    }
}

/// Owner-checked teardown of [`JIT_G1_YOUNG`], as
/// [`clear_jit_g1_barrier_owned_by`]: the shift first, so no reader sees a
/// live shift beside a stale buffer; then the claim is released, unless a
/// second collector marked the table contended (that stays for the process).
pub fn clear_jit_g1_young_owned_by(owned_base: usize) {
    use cratonvm_types::jit_g1_young::{W_BASE, W_CLAIM, W_COUNT, W_REGIONS, W_SHIFT};
    if owned_base == 0 || JIT_G1_YOUNG.words[W_BASE].load(Ordering::SeqCst) != owned_base {
        return;
    }
    // The shift first; the geometry words `[5]`-`[7]` are build constants
    // and stay (a compile that reads them still needs a live shift).
    JIT_G1_YOUNG.words[W_SHIFT].store(0, Ordering::SeqCst);
    JIT_G1_YOUNG.words[W_REGIONS].store(0, Ordering::SeqCst);
    JIT_G1_YOUNG.words[W_BASE].store(0, Ordering::SeqCst);
    JIT_G1_YOUNG.words[W_COUNT].store(0, Ordering::SeqCst);
    let _ = JIT_G1_YOUNG.words[W_CLAIM].compare_exchange(
        owned_base,
        0,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
}

/// Publish one `[base, end)` pair into [`JIT_READ_BOUNDS`] slot `slot` (0..3).
///
/// Separate from the generational publisher because G1 has no `store_region_
/// bounds_locked` and no three arenas — it has one span and publishes once.
pub fn publish_jit_read_bounds(slot: usize, base: usize, end: usize) {
    if slot >= 3 {
        return;
    }
    JIT_READ_BOUNDS.words[slot * 2].store(base, Ordering::Release);
    JIT_READ_BOUNDS.words[slot * 2 + 1].store(end, Ordering::Release);
}

/// Read back one `[base, end)` pair from [`JIT_READ_BOUNDS`]. `(0, 0)` for an
/// out-of-range slot or one that was never published.
///
/// F-16 needs this: under a reserved-and-committed-on-demand heap the published
/// bound must never outrun the committed prefix, and that is only checkable by
/// reading what was published.
pub fn jit_read_bounds_slot(slot: usize) -> (usize, usize) {
    if slot >= 3 {
        return (0, 0);
    }
    (
        JIT_READ_BOUNDS.words[slot * 2].load(Ordering::Acquire),
        JIT_READ_BOUNDS.words[slot * 2 + 1].load(Ordering::Acquire),
    )
}

/// Zero the whole read table — the teardown counterpart, so a dropped heap can
/// never leave bounds that would admit a load into freed arena memory.
pub fn clear_jit_read_bounds() {
    for w in JIT_READ_BOUNDS.words.iter() {
        w.store(0, Ordering::Release);
    }
}

/// Clear the read table only if slot 0 still names `owned_base` -- the
/// owner-checked form, for the same reason [`clear_movable_bounds_owned_by`]
/// exists. `GenerationalHeap::drop` already open-codes this test; ZGC's `Drop`
/// called the unconditional form until 2026-08-21.
pub fn clear_jit_read_bounds_owned_by(owned_base: usize) {
    if JIT_READ_BOUNDS.words[0].load(Ordering::Acquire) == owned_base {
        clear_jit_read_bounds();
    }
}

/// Process-global mirror of the address range a **relocating collection may
/// move**, for collectors that do not fill [`JIT_REGION_BOUNDS`].
///
/// # Why a THIRD table
///
/// The band verifier
/// (`conservative_roots::moving_young_unpublished_frame_oop_present`) has to
/// classify a stack word as "an object a moving cycle could relocate" before it
/// can ask "is it published in the precise map?". It asked
/// [`addr_in_published_young_regions`], which reads [`JIT_REGION_BOUNDS`] —
/// and that table is generational-only, so on G1 and ZGC the predicate answered
/// `false` for every address in the process and the verifier passed without
/// having verified anything. [`published_young_regions_are_live`] exists so a
/// caller can fail closed on exactly that, and it does.
///
/// Filling [`JIT_REGION_BOUNDS`] would fix the verifier and break something
/// else: that table's load-bearing second job is the STORE-side question "may
/// an inline reference store skip the collector's write barrier", which G1 and
/// ZGC answer by leaving it empty (`g1-audit.md` §8.1, G1-2). One table,
/// two questions, opposite answers — which is the same reason
/// [`JitReadBoundsTable`] exists rather than the read side being folded in.
/// This is the third question and it gets the third table.
///
/// # What "movable" means here
///
/// Whatever the running collector's relocating phase may move, and it is safe
/// for this to be a SUPERSET. An address wrongly called movable costs one
/// diverted (non-moving) collection; an address wrongly called immovable is how
/// a verifier reports a clean frame it never inspected. So a collector that
/// cannot cheaply name its exact movable set should publish its whole arena.
///
/// # Who publishes
///
/// * **Generational** — nobody. It already answers through
///   [`addr_in_published_young_regions`], and [`addr_is_movable`] consults both.
///   Leaving it out keeps its behaviour byte-identical.
/// * **ZGC** — its arena envelope, in slot 0, at construction, cleared on
///   `Drop`. The envelope is captured once and the arena is never grown, so the
///   bounds cannot go stale (the same argument `conservative_addr_span` and the
///   object-start bitmap already rest on).
/// * **G1** — its arena's COMMITTED prefix, in slot 0, republished when the
///   commit grows (`g1.rs`). (gen r5w1/young5: this line used to say "nobody
///   yet"; G1 has published here since the band verifier's §18 bound.)
///
/// Because a ZGC envelope can cover decommitted granules, a reader about to
/// dereference a word this table admits should first ask
/// [`movable_range_committed`], the commit screen a backend may publish beside
/// it (gen r5w1/young5).
#[repr(C)]
pub struct MovableBoundsTable {
    pub words: [AtomicUsize; 6],
}

pub static MOVABLE_BOUNDS: MovableBoundsTable = MovableBoundsTable {
    words: [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ],
};

/// Publish `[base, end)` into slot `slot` (0..3) of [`MOVABLE_BOUNDS`].
pub fn publish_movable_bounds(slot: usize, base: usize, end: usize) {
    if slot >= 3 {
        return;
    }
    MOVABLE_BOUNDS.words[slot * 2].store(base, Ordering::Release);
    MOVABLE_BOUNDS.words[slot * 2 + 1].store(end, Ordering::Release);
}

/// Zero the whole movable table — the teardown counterpart, so a dropped heap
/// cannot leave bounds naming freed arena memory.
///
/// Prefer [`clear_movable_bounds_owned_by`]: this table is PROCESS-GLOBAL and a
/// heap that clears it unconditionally wipes the bounds a different, still-live
/// heap published. `GenerationalHeap::drop` learned that the hard way for
/// [`JIT_REGION_BOUNDS`] — and the cost is not merely a slower fast path here,
/// because an empty table makes the frame-band verifier vacuous, which is the
/// whole reason this table exists.
pub fn clear_movable_bounds() {
    for w in MOVABLE_BOUNDS.words.iter() {
        w.store(0, Ordering::Release);
    }
}

/// Clear the movable table only if slot 0 still names `owned_base`.
///
/// The discriminator is the publisher's own arena base: a table still naming it
/// is a table this heap published and nobody has replaced. Exactly the shape
/// `GenerationalHeap::drop` uses for the other two tables, and for the same
/// measured reason — a short-lived heap (a sizing probe, an init-time heap
/// replaced once `-Xmx` is parsed) must not wipe a live heap's bounds.
pub fn clear_movable_bounds_owned_by(owned_base: usize) {
    if MOVABLE_BOUNDS.words[0].load(Ordering::Acquire) == owned_base {
        clear_movable_bounds();
    }
}

/// Whether [`MOVABLE_BOUNDS`] carries any pair at all.
pub fn movable_bounds_published() -> bool {
    (0..3).any(|i| MOVABLE_BOUNDS.words[i * 2].load(Ordering::Acquire) != 0)
}

/// Is `addr` inside a published movable range?
pub fn addr_in_movable_bounds(addr: usize) -> bool {
    for i in 0..3 {
        let base = MOVABLE_BOUNDS.words[i * 2].load(Ordering::Acquire);
        let end = MOVABLE_BOUNDS.words[i * 2 + 1].load(Ordering::Acquire);
        if base != 0 && addr >= base && addr < end {
            return true;
        }
    }
    false
}

/// The commit screen beside [`MOVABLE_BOUNDS`]: the per-granule commit maps of
/// the arenas a backend published there, as one immutable snapshot (the same
/// [`CommitScreenTable`] the young screen uses).
///
/// gen r5w1/young5 (2026-09-26), item 2 of
/// `docs/feature-designs/generative-gc-architecture.md`:
/// the band verifier's object screen (`conservative_roots::band_word_is_an_object`)
/// reads the header of every word [`addr_is_movable`] admits, and
/// [`published_young_range_committed`] only screens the GENERATIONAL young
/// tables. ZGC publishes its whole arena envelope into [`MOVABLE_BOUNDS`], and
/// with `CRATONVM_ZGC_READABLE_GIVE_BACK=0` (or its JIT read bounds flipped)
/// parts of that envelope are decommitted, so a stale frame word there faults
/// in the verifier. This is the backend-neutral hook: a backend whose envelope
/// can cover unmapped granules publishes their maps here with
/// [`publish_movable_commit_screens`], and the screen consults it.
///
/// Nobody publishes into it at gen r5w1 (the ZGC call is a cross-lane request;
/// G1's envelope is its committed prefix, so it needs none), so every reader
/// gets `None` and behaves exactly as before.
pub static MOVABLE_COMMIT_SCREENS: CommitScreenTable<3> = CommitScreenTable::new();

/// Publish the commit maps of the arenas in [`MOVABLE_BOUNDS`], one snapshot
/// for all three slots. A slot with `base == 0` is no arena; an arena whose
/// map is `None` is wholly committed. Call it wherever the backend publishes
/// or republishes [`MOVABLE_BOUNDS`] (and whenever an arena is re-backed).
pub fn publish_movable_commit_screens(arenas: [(usize, usize, Option<Arc<[AtomicU64]>>); 3]) {
    MOVABLE_COMMIT_SCREENS.publish(
        arenas.map(|(base, end, bits)| (base != 0).then_some(CommitScreen { base, end, bits })),
    );
}

/// Unpublish the movable commit screen if it names an arena starting at
/// `owned_base` — the `Drop` counterpart, owner-checked like
/// [`clear_movable_bounds_owned_by`].
pub fn clear_movable_commit_screens_owned_by(owned_base: usize) {
    MOVABLE_COMMIT_SCREENS.clear_if_names(&[owned_base]);
}

/// Is `[addr, addr + len)` committed, per the movable commit screen?
///
/// `None` when no backend published a screen for an arena containing `addr`
/// (the caller then knows nothing more than before); `Some(false)` when the
/// span touches a decommitted granule — nothing there is an object, so do not
/// read it — or when the reader kept losing races with republishes.
pub fn movable_range_committed(addr: usize, len: usize) -> Option<bool> {
    MOVABLE_COMMIT_SCREENS.lookup(addr, len).flatten()
}

/// **Can a relocating collection move something at `addr`?** — the union of the
/// generational young table and [`MOVABLE_BOUNDS`].
///
/// This is what the band verifier must ask. Asking
/// [`addr_in_published_young_regions`] alone is what made the verifier vacuous
/// on every collector but one.
pub fn addr_is_movable(addr: usize) -> bool {
    addr_in_published_young_regions(addr) || addr_in_movable_bounds(addr)
}

/// Does `[addr, addr + len)` fit entirely inside one of the published young
/// regions?
#[inline]
pub fn young_span_in_published_regions(addr: usize, len: usize) -> bool {
    for i in 0..2 {
        let base = JIT_REGION_BOUNDS.words[i * 2].load(Ordering::Acquire);
        let end = JIT_REGION_BOUNDS.words[i * 2 + 1].load(Ordering::Acquire);
        if base != 0 && addr >= base && addr.checked_add(len).is_some_and(|hi| hi <= end) {
            return true;
        }
    }
    false
}

/// Does `[addr, addr + len)` fit entirely inside one of the published movable
/// bounds?
#[inline]
pub fn movable_span_in_published_bounds(addr: usize, len: usize) -> bool {
    for i in 0..3 {
        let base = MOVABLE_BOUNDS.words[i * 2].load(Ordering::Acquire);
        let end = MOVABLE_BOUNDS.words[i * 2 + 1].load(Ordering::Acquire);
        if base != 0 && addr >= base && addr.checked_add(len).is_some_and(|hi| hi <= end) {
            return true;
        }
    }
    false
}

/// Is `[addr, addr + len)` inside a published movable region and backed by
/// committed memory?
#[inline]
pub fn span_is_movable_and_committed(addr: usize, len: usize) -> bool {
    if young_span_in_published_regions(addr, len) {
        return published_young_range_committed(addr, len);
    }
    movable_span_in_published_bounds(addr, len)
}

/// Whether ANY collector has published a movable range this verifier can test
/// against — the fail-closed gate.
///
/// A predicate built on [`addr_is_movable`] is vacuous while this is `false`,
/// exactly as [`published_young_regions_are_live`] documents for its own table,
/// and a caller that reads a quiet verifier as a clean one is measuring an
/// unpublished table.
///
/// The third term is the LIVE-HEAP registry. A published table is only an
/// answer about the heap it names, and slot-0 ownership makes each table
/// single-tenant, so with two heaps alive the one that lost the slot gets
/// `false` for every address — published, fresh, and about somebody else. See
/// [`RELOCATABLE_HEAPS_LIVE`] for why that is the same vacuous pass an empty
/// table produces, arrived at from the other side.
pub fn movable_bounds_are_live() -> bool {
    published_bounds_represent_every_live_heap()
        && (published_young_regions_are_live() || movable_bounds_published())
}

// ---------------------------------------------------------------------------
// The live-heap registry
// ---------------------------------------------------------------------------

/// How many heaps whose objects a relocating cycle could move are alive right
/// now.
///
/// # Why a count is needed at all
///
/// [`JIT_REGION_BOUNDS`] and [`MOVABLE_BOUNDS`] are process-global and
/// **last-writer-wins**, and both are discriminated by slot 0 — a table
/// "belongs to" whichever heap's base it currently names, which is what makes
/// the owner-checked clears in the three `Drop`s correct. That discrimination
/// has a consequence the clears do not: a table can describe **exactly one**
/// heap. Construct a second heap and the first is silently unrepresented in
/// it.
///
/// That was recorded as a known limitation for as long as the only reader was
/// the JIT's guarded inline `getfield`, where an unrepresented arena costs a
/// helper call. It stopped being a limitation and became the same defect one
/// step over once
/// `conservative_roots::moving_young_unpublished_frame_oop_present` started
/// asking [`addr_is_movable`] whether a compiled frame's word could be
/// relocated: for the unrepresented heap that predicate answers `false` for
/// **every address in the process**, so the verifier walks every verifiable
/// slot, classifies none of them as movable, and returns "nothing unpublished"
/// without having inspected anything — a vacuous pass, which is exactly the
/// chain `bug-g1-evacuates-live-jit-reference-20260819-FIXED.md` traces from an
/// empty table to an evacuated live reference.
///
/// The difference is only in how the table came to be useless. An EMPTY table
/// is caught by [`movable_bounds_are_live`], because nothing is published. A
/// table naming the OTHER heap is not: it is published, it is fresh, it is
/// simply about somebody else. `pin_addrs=0` read as "there are no JIT roots";
/// this reads as "no word in this frame is movable". Same false verdict, same
/// consumer, and no instrument between them.
///
/// # Why a count rather than a wider table
///
/// Widening the tables to hold N heaps was considered and is the wrong trade.
/// The tables' addresses are baked into compiled code as immediates and their
/// six-word layout is the shape the emitted containment sequence walks
/// (`emit_guarded_getfield_receiver_check`); a variable-length table would put
/// a loop bound on the JIT's hottest guard to serve a case a running VM never
/// has. One heap is the shape of every production process — `vm_init` builds
/// exactly one — so the honest engineering answer is to keep the fast shape
/// and REFUSE to answer when it cannot represent the process, which is what
/// every other "cannot inspect" case in this verifier already does.
///
/// Registration is RAII ([`RelocatableHeapRegistration`]) rather than a
/// matched pair of calls, so a heap cannot forget to deregister and an unwind
/// out of a partially-built heap cannot leak a count that would disable
/// moving-young for the life of the process.
///
/// # It is process-global under `cfg(test)` too, deliberately
///
/// Most counters in this crate are thread-local under `cfg(test)`. This one is
/// not, because the true count is the whole point: a test that saw only its own
/// thread's heaps would report `1` in a `libtest` binary that has two hundred
/// heaps alive across a thread per core, and would therefore assert the gate
/// OPEN in exactly the state the gate exists to close. The consequence is the
/// one `gc/tests/published_bounds_isolation.rs` was created for and documents in
/// its header: **a test that observes this count, or any gate built on it,
/// belongs in that integration file** — Cargo gives it a process, which is the
/// only mechanism that removes the peer constructions.
pub static RELOCATABLE_HEAPS_LIVE: AtomicUsize = AtomicUsize::new(0);

/// A live heap's entry in the registry [`RELOCATABLE_HEAPS_LIVE`] counts.
///
/// Held as a field by every heap that can own relocatable objects
/// ([`GenerationalHeap`], `zgc::ZgcRealHeap`, `g1::G1Collector`), so the count
/// tracks heap lifetimes exactly and without a teardown path to forget.
#[derive(Debug)]
pub struct RelocatableHeapRegistration {
    pub _private: (),
}

impl RelocatableHeapRegistration {
    /// Register a newly-constructed heap. Call once, from the constructor.
    pub fn new() -> Self {
        RELOCATABLE_HEAPS_LIVE.fetch_add(1, Ordering::AcqRel);
        Self { _private: () }
    }
}

impl Default for RelocatableHeapRegistration {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RelocatableHeapRegistration {
    fn drop(&mut self) {
        RELOCATABLE_HEAPS_LIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How many heaps are currently registered. Diagnostics and tests.
pub fn live_relocatable_heaps() -> usize {
    RELOCATABLE_HEAPS_LIVE.load(Ordering::Acquire)
}

/// **Can the published-bounds tables describe every live heap?**
///
/// `false` once a second heap exists, because slot-0 ownership makes each
/// table single-tenant — see [`RELOCATABLE_HEAPS_LIVE`]. Zero heaps answers
/// `true`: there is nothing to misdescribe, and the emptiness of the tables is
/// then a faithful report that [`movable_bounds_are_live`] already refuses on.
pub fn published_bounds_represent_every_live_heap() -> bool {
    RELOCATABLE_HEAPS_LIVE.load(Ordering::Acquire) <= 1
}

/// # What this is, and what it deliberately is not
///
/// It is not a barrier. Every byte here names a PREFIX of a barrier helper's
/// own control flow, read from the same state the helper reads. Compiled code
/// may take a gate's "no work" answer and skip the call; on any other answer it
/// calls the same helper it calls today, so no collector's remembered-set or
/// snapshot contract moves into the emitter.
///
/// This is the question [`JIT_REGION_BOUNDS`] is not asking. That table asks
/// "is the receiver inside a published young region" — a *generational*
/// question that G1 and ZGC answer by publishing nothing, which left the
/// emitter laying down six containment compares that could never pass and then
/// calling the helper anyway.
///
/// # The safe direction of every byte
///
/// Each gate is allowed to be **conservative but never permissive**:
///
/// * `pre_active` / `post_active`: may read 1 while the truth is 0 (compiled
///   code pays a call it did not need). Must never read 0 while the truth is 1.
///   Publishers therefore SET the mirror before the truth and CLEAR it after.
/// * `young_floor`: may be too LOW (fewer stores skip). Must never be too high.
///
/// All-zero means "no collector published a plan", and every emitter arm keeps
/// the full-helper path — exactly today's behaviour.
#[repr(C)]
pub struct JitRefStoreGates {
    /// Non-zero when the SATB pre-write barrier may have work to do.
    pub pre_active: AtomicU8,
    /// Non-zero when the post-write barrier may have work to do.
    pub post_active: AtomicU8,
    /// A receiver whose `GC_FLAGS_BYTE_OFFSET` byte is unsigned-less-than this
    /// provably needs no post barrier. `0` disables the test.
    ///
    /// That byte holds `gc_age` in bits 4..7 and the GC flags in bits 0..3, so
    /// a value of `age << 4` is an exact unsigned test of `gc_age < age`: the
    /// flags nibble is at most 15 and cannot carry `a << 4` up to `(a+1) << 4`.
    pub young_floor: AtomicU8,
    /// A receiver none of whose `GC_FLAGS_BYTE_OFFSET` bits fall in this mask
    /// provably needs no post barrier. `0` disables the test.
    ///
    /// **The alternative to [`Self::young_floor`], not a refinement of it**, and
    /// the reason the generational collector could not use the floor at all.
    /// The floor is an unsigned `<` on `(gc_age << 4) | gc_flags`, so it can
    /// only express "young enough". Generational's post barrier asks a
    /// different question — *is the receiver in the old generation* — which it
    /// answers with `GC_FLAG_OLD_GEN`, a bit in the flags nibble. Those do not
    /// order: an object allocated directly in old gen has `gc_age == 0` and so
    /// a flags byte of `0x01`, BELOW the age-zero floor `0x10`, while a young
    /// object that has survived three collections is `0x30`, above it. A single
    /// threshold would therefore have told compiled code to skip the card on
    /// exactly the receivers that need one.
    ///
    /// Same conservative direction as every other gate: a set bit means "there
    /// may be work" (call the collector's own barrier), never "no work".
    pub post_skip_mask: AtomicU8,
    /// Whether a plan is published at all. Separate from the three gates so
    /// "published, and all three currently say no work" is distinguishable
    /// from "nobody published", which is the difference between an emitted
    /// fast path and no fast path.
    pub published: AtomicU8,
}

impl JitRefStoreGates {
    /// All-zero: nothing published, every emitter arm keeps the full helper.
    pub const fn unpublished() -> Self {
        Self {
            pre_active: AtomicU8::new(0),
            post_active: AtomicU8::new(0),
            young_floor: AtomicU8::new(0),
            post_skip_mask: AtomicU8::new(0),
            published: AtomicU8::new(0),
        }
    }

    /// gen r4w4/cards4 — publish the MASK-shaped post-barrier plan into THIS
    /// block: post gate armed, floor zero, mask set, then `published`. The
    /// order is the conservative one every publisher follows (the gate is
    /// armed before the plan becomes visible). `pre_active` is not touched: a
    /// per-heap block's pre byte is unused — see
    /// [`GenerationalHeap::jit_ref_store_plan`].
    pub fn publish_masked(&self, post_skip_mask: u8) {
        self.post_active.store(1, Ordering::Release);
        self.young_floor.store(0, Ordering::Release);
        self.post_skip_mask.store(post_skip_mask, Ordering::Release);
        self.published.store(1, Ordering::Release);
    }
}

/// The shared SATB pre gate for Generational/G1 queues, plus a legacy
/// VM-less reference-store test plan. Production ZGC compilers obtain both
/// gates from `ZgcRealHeap::jit_ref_store_plan`; ZGC construction, marking,
/// generational resets, promotion and destruction never mutate this block.
pub static JIT_REF_STORE_GATES: JitRefStoreGates = JitRefStoreGates::unpublished();

/// Active SATB queues (Generational and G1), across every heap in the
/// process, plus the legacy VM-less test gate.
///
/// gc-common w5-e: the one arming source is `SatbQueue` (`satb.rs`,
/// `jit_gate_armed`), which takes a count BEFORE it stores ACTIVE and gives it
/// back AFTER it stores INACTIVE. `ConcurrentGcState::set_phase` used to hold a
/// second count around the marking phases and no longer touches this.
///
/// [`set_jit_ref_store_pre_active`] is a plain boolean and has exactly one
/// writer in legacy VM-less test builds; production ZGC owns its pre byte. `SatbQueue` does not: it is used by the
/// generational collector AND G1, and a process can hold several heaps at once
/// (the unit-test binaries routinely do). A second boolean writer would let one
/// heap leaving its mark phase CLEAR the gate while another is still marking —
/// compiled code would then skip an SATB pre-barrier that is live, which loses
/// the overwritten reference from the snapshot. Counting markers instead makes
/// the gate "armed while ANY marker is armed", which is the only direction that
/// is safe to be wrong in.
///
/// gci i12/wb (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 2): the
/// count, the legacy VM-less boolean and the number of refreshes in flight are
/// ONE word, so a refresh's snapshot and its entry into the in-flight set are a
/// single atomic step ([`pre_state_armed`] reads the layout). It used to be two
/// statics read by a refresh that then stored the byte: heap B's disarm could
/// read count 0, heap A's arm then count 1 and store 1 and activate its queue,
/// and B's late store of 0 cleared the byte under A's live marking.
pub static JIT_REF_STORE_PRE_STATE: AtomicU64 = AtomicU64::new(0);

/// [`JIT_REF_STORE_PRE_STATE`]: the marker count's bits.
pub const PRE_COUNT_MASK: u64 = (1 << 32) - 1;
/// [`JIT_REF_STORE_PRE_STATE`]: the legacy VM-less boolean.
pub const PRE_LEGACY_BIT: u64 = 1 << 32;
/// [`JIT_REF_STORE_PRE_STATE`]: one refresh in flight (bits 33 and up).
pub const PRE_INFLIGHT_ONE: u64 = 1 << 33;

/// Is the gate owed "armed" in state word `s`?
#[inline]
pub fn pre_state_armed(s: u64) -> bool {
    (s & PRE_COUNT_MASK) != 0 || (s & PRE_LEGACY_BIT) != 0
}

/// Recompute the published `pre_active` byte from the state word.
///
/// The snapshot is taken by the same RMW that enters the in-flight set, and the
/// refresh leaves it after its store. A refresh can therefore publish a stale
/// "disarmed" only while it is in flight, which is what
/// [`refresh_jit_ref_store_pre_active_armed`] waits out. A stale "armed" is the
/// safe direction and needs nothing.
pub fn refresh_jit_ref_store_pre_active() {
    let snapshot = JIT_REF_STORE_PRE_STATE.fetch_add(PRE_INFLIGHT_ONE, Ordering::SeqCst);
    store_jit_satb_pre_byte(u8::from(pre_state_armed(snapshot)));
    JIT_REF_STORE_PRE_STATE.fetch_sub(PRE_INFLIGHT_ONE, Ordering::SeqCst);
}

/// t1 lane R (`t1-D-proposal-gate-bytes-in-rip-reach-of-the-code-cache`):
/// the code-adjacent COPY of `JIT_REF_STORE_GATES.pre_active`, or null.
///
/// # Why a copy exists
///
/// The pre byte is a `static`, so it sits in the executable image
/// (`0x7FF7_xxxx_xxxx` on Windows, the PIE image at `0x55..` on Linux), while
/// compiled code lives in anonymous mappings (`0x2xx_xxxx_xxxx` /
/// `0x7f..`): terabytes apart, so every RIP-relative gate form
/// (`CMP byte [rip+d32], 0`) declined and fell back to `MOV R11, imm64;
/// CMP [R11]` — measured 121/121 `rip=false` sites on Windows. The VM
/// installs one cell from `jit::platform::alloc_code_adjacent_cell` (the same
/// OS primitive the code cache uses, so the same region of the address space)
/// and the gate addresses handed to compilers name it instead.
///
/// # Why this is sound
///
/// * EVERY store to the pre byte goes through [`store_jit_satb_pre_byte`],
///   which writes the static and the copy, inside the refresh's in-flight
///   window — so the arming protocol of [`JIT_REF_STORE_PRE_STATE`] covers
///   both bytes, and code compiled before the install (reading the static)
///   and after it (reading the copy) both see the true disjunction.
/// * The copy is initialised ARMED before its address is published and is
///   then refreshed by an arming-style refresh (which waits out in-flight
///   refreshes that may have read a null pointer), so no reader can observe
///   a permissive copy that the static would not also have shown.
/// * The cell is never unmapped (`alloc_code_adjacent_cell`'s lifetime), so
///   its address may be baked into code as an immediate.
///
/// One cell per process, deliberately, and not a new kind of state: it is a
/// placement of the pre-existing process-wide marker disjunction (armed while
/// ANY heap's SATB queue is), which every heap in the process must read. A
/// per-VM copy would need every marker to arm every VM's copy.
pub static JIT_SATB_PRE_GATE_CELL: std::sync::atomic::AtomicPtr<AtomicU8> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Write the SATB pre byte: the static and, once installed, its code-adjacent
/// copy ([`JIT_SATB_PRE_GATE_CELL`]). The only writer of either.
pub fn store_jit_satb_pre_byte(v: u8) {
    JIT_REF_STORE_GATES.pre_active.store(v, Ordering::SeqCst);
    let cell = JIT_SATB_PRE_GATE_CELL.load(Ordering::SeqCst);
    if !cell.is_null() {
        // SAFETY: only `install_jit_satb_pre_gate_cell` stores a non-null
        // pointer, from a `&'static AtomicU8`.
        unsafe { &*cell }.store(v, Ordering::SeqCst);
    }
}

/// t1 lane R: install the code-adjacent copy of the SATB pre byte (see
/// [`JIT_SATB_PRE_GATE_CELL`]). First install wins; returns whether `cell` is
/// now the installed copy. Call before the first compilation of the VM, so
/// its compiled reference stores name the copy; a later call is still sound
/// (code compiled earlier keeps reading the static, which stays maintained).
pub fn install_jit_satb_pre_gate_cell(cell: &'static AtomicU8) -> bool {
    let ptr = cell as *const AtomicU8 as *mut AtomicU8;
    let current = JIT_SATB_PRE_GATE_CELL.load(Ordering::SeqCst);
    if !current.is_null() {
        return current == ptr;
    }
    // Armed before it is reachable: the safe direction for a gate byte. (A
    // racing installer that wins leaves this cell unpublished, so the store
    // cannot reach any compiled code.)
    cell.store(1, Ordering::SeqCst);
    match JIT_SATB_PRE_GATE_CELL.compare_exchange(
        std::ptr::null_mut(),
        ptr,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => {
            // Waits out any refresh that read the null pointer, then stores
            // the true disjunction into both bytes.
            refresh_jit_ref_store_pre_active_armed();
            true
        }
        Err(existing) => existing == ptr,
    }
}

/// The refresh after an ARMING change (count up, legacy bit set): wait until
/// no refresh that started before it is still in flight, then refresh. Every
/// refresh that starts later snapshots the arming, so once this one stores, no
/// writer can still clear the byte on a stale snapshot. The wait is bounded by
/// the refreshes already running (two atomics and a byte store each; none of
/// them can be at a safepoint).
pub fn refresh_jit_ref_store_pre_active_armed() {
    let mut spins = 0u32;
    while JIT_REF_STORE_PRE_STATE.load(Ordering::SeqCst) >= PRE_INFLIGHT_ONE {
        spins = spins.wrapping_add(1);
        if spins & 63 == 0 {
            std::thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
    refresh_jit_ref_store_pre_active();
}

/// A marker is entering a phase whose SATB barrier must run.
///
/// Call BEFORE the phase becomes observable, so the gate is armed no later than
/// the barrier it mirrors. See [`JIT_REF_STORE_PRE_STATE`].
pub fn arm_jit_ref_store_marker() {
    let _ = JIT_REF_STORE_PRE_STATE.try_update(Ordering::SeqCst, Ordering::SeqCst, |s| {
        let count = (s & PRE_COUNT_MASK).saturating_add(1).min(PRE_COUNT_MASK);
        Some((s & !PRE_COUNT_MASK) | count)
    });
    refresh_jit_ref_store_pre_active_armed();
}

/// A marker has left such a phase.
///
/// Call AFTER the phase has stopped being observable, so the gate is disarmed
/// no earlier than the barrier it mirrors.
pub fn disarm_jit_ref_store_marker() {
    // Saturating rather than `fetch_sub`: an unmatched disarm would wrap the
    // count to a huge value, i.e. a gate armed for the life of the process —
    // safe, but it would silently withdraw the fast path everywhere and look
    // like the feature simply not paying.
    let _ = JIT_REF_STORE_PRE_STATE.try_update(Ordering::SeqCst, Ordering::SeqCst, |s| {
        let count = (s & PRE_COUNT_MASK).saturating_sub(1);
        Some((s & !PRE_COUNT_MASK) | count)
    });
    refresh_jit_ref_store_pre_active();
}

/// Markers currently counted as armed. Diagnostics and tests.
pub fn jit_ref_store_armed_markers() -> usize {
    (JIT_REF_STORE_PRE_STATE.load(Ordering::Acquire) & PRE_COUNT_MASK) as usize
}

/// `gc_age == 0` expressed in the `GC_FLAGS_BYTE_OFFSET` byte's units.
///
/// The floor a collector may publish **without any ordering obligation**: an
/// object that has survived zero collections is younger than every promotion
/// age (which is clamped to at least 1), so this bound stays true no matter how
/// the collector retunes. A larger, dynamic floor is possible and would catch
/// more stores, but it must be LOWERED before the promotion age it mirrors is
/// lowered, or a store into a newly-old object skips its card — so it is a
/// separate change with its own ordering argument, not a constant.
///
/// It is also the case that matters: in allocation-heavy code the receiver of a
/// reference store is overwhelmingly an object allocated moments earlier.
pub const JIT_YOUNG_FLOOR_AGE_ZERO: u8 = 1 << 4;

/// g1put (2026-10-05): the address of the process-wide SATB pre-gate byte,
/// `JIT_REF_STORE_GATES.pre_active`, whether or not any plan is published.
///
/// For a collector that publishes no reference-store PLAN but whose SATB queue
/// arms this byte: G1. Its `SatbQueue` counts itself into
/// [`JIT_REF_STORE_PRE_STATE`] BEFORE it stores ACTIVE and out AFTER it
/// stores INACTIVE (`satb.rs`, `jit_gate_armed`), exactly as the generational
/// queue does, so a zero here proves `satb_pre_barrier_required()` is false
/// for G1 too. G1 activates its queue only inside `start_concurrent_mark`,
/// which holds a `StopTheWorldToken`; a thread that reads the byte and stores with no
/// safepoint in between cannot straddle it unless the takeover freezes it there, which
/// the activating pause covers by scanning what frozen peers hold (gci i12/wb,
/// `G1Collector::scan_frozen_held_objects`).
pub fn jit_satb_pre_gate_addr() -> usize {
    // t1 lane R: the code-adjacent copy once the VM installed one, so the
    // compiled gate's RIP-relative form is in reach (`JIT_SATB_PRE_GATE_CELL`).
    let cell = JIT_SATB_PRE_GATE_CELL.load(Ordering::SeqCst);
    if !cell.is_null() {
        return cell as usize;
    }
    &JIT_REF_STORE_GATES.pre_active as *const AtomicU8 as usize
}

/// Addresses of the three gate bytes, for `JitRuntimeHelpers`.
/// Returns `(pre, post, young_floor)`, or all-zero when nothing is published.
pub fn jit_ref_store_gate_addrs() -> (usize, usize, usize) {
    if JIT_REF_STORE_GATES.published.load(Ordering::Acquire) == 0 {
        return (0, 0, 0);
    }
    let base = &JIT_REF_STORE_GATES;
    (
        &base.pre_active as *const _ as usize,
        &base.post_active as *const _ as usize,
        // The floor's ADDRESS only when this plan actually uses the floor.
        //
        // The address of a `static` is non-zero whatever the byte holds, so
        // handing it over unconditionally would tell the emitter that BOTH
        // post-barrier shapes are published — and it declines that, because for
        // a mask publisher the floor is not merely redundant but WRONG (an
        // old-gen object with `gc_age == 0` sits below it). Gating on the value
        // keeps "which shape is this" answerable from the three words the
        // helper table carries.
        if JIT_REF_STORE_GATES.young_floor.load(Ordering::Acquire) == 0 {
            0
        } else {
            &base.young_floor as *const _ as usize        },
    )
}

/// The published post-barrier skip MASK, as a value rather than an address.
///
/// Baked into generated code as an immediate, which is why it is read as a
/// value: the mask is a property of which collector is running, and a process
/// does not change collector after start-up. The gate BYTES stay addresses
/// because they change while the process runs; this does not.
///
/// `0` means no mask is published — either nothing is published at all, or the
/// publisher uses [`JitRefStoreGates::young_floor`] instead.
pub fn jit_ref_store_post_skip_mask() -> u8 {
    if JIT_REF_STORE_GATES.published.load(Ordering::Acquire) == 0 {
        return 0;
    }
    JIT_REF_STORE_GATES.post_skip_mask.load(Ordering::Acquire)
}

/// Announce that this process's collector maintains the gates, using the
/// unsigned age FLOOR to rule the post barrier out.
///
/// The POST gate is armed here and only ever relaxed by its owner, so a plan
/// can never be observed permissive about old objects before that owner has
/// run once.
///
/// The PRE gate is published as the true disjunction of its two sources
/// instead, via [`refresh_jit_ref_store_pre_active`]. It used to be a hard `1`
/// on the same "starts armed" reasoning, and that was wrong in a way nothing
/// could see: `pre_active` is only ever recomputed by a marker arming or
/// disarming, so on a run whose collector never enters a concurrent mark phase
/// there is no later call to relax it. The gate then reads ARMED for the life
/// of the process and every compiled reference store takes the helper — which
/// is exactly what a run-time path census found, on both collectors:
/// 16,384,000 stores, `inline=0`, all of them bailing at this gate.
///
/// The disjunction is safe at publication for the reason the hard `1` was
/// reaching for: the heap being constructed is not yet reachable, so it has no
/// marking of its own, and any OTHER heap's marking is already counted in
/// `JIT_REF_STORE_PRE_STATE`. The result can only be conservatively armed,
/// never falsely permissive.
pub fn publish_jit_ref_store_plan(young_floor: u8) {
    refresh_jit_ref_store_pre_active();
    JIT_REF_STORE_GATES.post_active.store(1, Ordering::Release);
    JIT_REF_STORE_GATES
        .post_skip_mask
        .store(0, Ordering::Release);
    JIT_REF_STORE_GATES
        .young_floor
        .store(young_floor, Ordering::Release);
    JIT_REF_STORE_GATES.published.store(1, Ordering::Release);
}

/// Announce a plan that rules the post barrier out by a flags MASK rather than
/// an age floor — see [`JitRefStoreGates::post_skip_mask`] for why the
/// generational collector needs this shape and cannot use the floor.
///
/// The floor is explicitly zeroed: publishing both would emit two independent
/// skips for one question, and the age floor is WRONG for a mask publisher (an
/// old-gen object with `gc_age == 0` sits below it).
pub fn publish_jit_ref_store_plan_masked(post_skip_mask: u8) {
    // The true disjunction, not a hard `1` -- see `publish_jit_ref_store_plan`
    // for the census that found the hard `1` pinning this gate armed forever.
    refresh_jit_ref_store_pre_active();
    JIT_REF_STORE_GATES.post_active.store(1, Ordering::Release);
    JIT_REF_STORE_GATES.young_floor.store(0, Ordering::Release);
    JIT_REF_STORE_GATES
        .post_skip_mask
        .store(post_skip_mask, Ordering::Release);
    JIT_REF_STORE_GATES.published.store(1, Ordering::Release);
}

/// Withdraw the plan — the teardown counterpart. Leaves both gates ARMED so
/// that any already-compiled body still takes its helper path.
pub fn clear_jit_ref_store_plan() {
    store_jit_satb_pre_byte(1);
    JIT_REF_STORE_GATES.post_active.store(1, Ordering::Release);
    JIT_REF_STORE_GATES.young_floor.store(0, Ordering::Release);
    JIT_REF_STORE_GATES
        .post_skip_mask
        .store(0, Ordering::Release);
    JIT_REF_STORE_GATES.published.store(0, Ordering::Release);
}

/// Mirror the SATB barrier's arming state. See [`JitRefStoreGates`] for why the
/// caller must set this BEFORE arming the real flag and clear it AFTER
/// disarming.
pub fn set_jit_ref_store_pre_active(active: bool) {
    if active {
        JIT_REF_STORE_PRE_STATE.fetch_or(PRE_LEGACY_BIT, Ordering::SeqCst);
        refresh_jit_ref_store_pre_active_armed();
    } else {
        JIT_REF_STORE_PRE_STATE.fetch_and(!PRE_LEGACY_BIT, Ordering::SeqCst);
        refresh_jit_ref_store_pre_active();
    }
}

/// Mirror "this heap contains at least one old object". Same ordering rule as
/// [`set_jit_ref_store_pre_active`].
pub fn set_jit_ref_store_post_active(active: bool) {
    JIT_REF_STORE_GATES
        .post_active
        .store(u8::from(active), Ordering::Release);
}



/// Whether [`JIT_REGION_BOUNDS`] carries any young pair at all.
///
/// [`addr_in_published_young_regions`] answers "is this address young?" by
/// testing that table, so when the table is EMPTY it answers `false` for every
/// address in the process — indistinguishable, at the call site, from "this
/// address is not young". A caller that reads that as good news is measuring an
/// unpublished table, not a clean frame.
///
/// The table has exactly one writer, `store_region_bounds_locked`, which is
/// generational-only: G1 deliberately leaves it empty (see the `G1Collector`
/// constructor's note on G1-2 — publishing it would make an inline reference
/// STORE fast path reachable and cost a CSet-excluded region its remembered-set
/// edge), and ZGC publishes neither table. So this returns `false` on every
/// collector except the generational one, and any predicate built on
/// `addr_in_published_young_regions` is VACUOUS there.
///
/// Exposed so such a predicate can fail closed instead of passing silently —
/// see `conservative_roots::moving_young_unpublished_frame_oop_present`.
pub fn published_young_regions_are_live() -> bool {
    (0..2).any(|i| JIT_REGION_BOUNDS.words[i * 2].load(Ordering::Acquire) != 0)
}

/// Does `addr` fall inside either published YOUNG semispace `[base, end)`?
///
/// A *containment* test, deliberately not an object-header validation: it is
/// the cheap, lock-free, `&VmHeap`-free predicate the moving-young coverage
/// verifier needs (`conservative_roots::moving_young_unpublished_frame_oop_present`),
/// which runs on the current thread's own JIT frame bands and must be callable
/// from `refresh_moving_young_coverage_for_current_thread` — a function with no
/// heap handle, reached from three different deposit sites.
///
/// Only the young generation matters there: a moving young cycle relocates
/// young objects only, so an old-gen or off-heap word in a compiled frame slot
/// is never left stale by it. Being a superset of "is a young object address"
/// is the safe direction — an interior/stale/coincidental word merely costs one
/// diverted (non-moving) collection, never a wrong relocation.
///
/// Returns `false` before the first publish and after the heap's `Drop` re-zeroes
/// the table (see [`JitRegionBoundsTable`]); both are states in which the
/// generational young semispaces do not exist, so nothing can be young.
///
/// gen r4/plumbing (2026-09-23): this doc block and the `#[inline]` below used
/// to sit on [`published_young_regions_are_live`] — an attribute and a doc
/// comment stranded one item early — so `cargo doc` described the table-empty
/// probe as the containment test, and the per-word test the verifier runs in a
/// loop was the one left without the inline hint.
#[inline]
pub fn addr_in_published_young_regions(addr: usize) -> bool {
    // words = [yf_base, yf_end, yt_base, yt_end, og_base, og_end]
    for i in 0..2 {
        let base = JIT_REGION_BOUNDS.words[i * 2].load(Ordering::Acquire);
        let end = JIT_REGION_BOUNDS.words[i * 2 + 1].load(Ordering::Acquire);
        if base != 0 && addr >= base && addr < end {
            return true;
        }
    }
    false
}

/// A published commit screen: one `AtomicPtr` to the current
/// [`CommitScreens`], plus the registry that owns every snapshot ever
/// published.
///
/// Snapshots are freed only when the TABLE drops, so a reader (which borrows
/// the table for the length of one call and never keeps the snapshot past it)
/// can never be handed freed memory. The process-global table is a `static`
/// and never drops; a heap's own table drops with the heap, when no `&heap`
/// reader can exist. Snapshots are deduplicated by content, so the registry
/// is bounded by the number of distinct arena tuples: the two young arenas in
/// both orders (a moving flip swaps them), plus one or two per `grow` or
/// re-backing. Each is a few words plus the reference that keeps a small
/// bitmap (one bit per 2 MiB granule) alive.
pub struct CommitScreenTable<const N: usize> {
    pub current: std::sync::atomic::AtomicPtr<CommitScreens<N>>,
    pub registry: Mutex<Vec<Box<CommitScreens<N>>>>,
}

impl<const N: usize> CommitScreenTable<N> {
    pub const fn new() -> Self {
        Self {
            current: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            registry: Mutex::new(Vec::new()),
        }
    }

    /// Publish every arena's screen as one snapshot.
    pub fn publish(&self, arenas: [Option<CommitScreen>; N]) {
        let mut registry = self.registry.lock();
        let idx = match registry.iter().position(|s| s.same_as(&arenas)) {
            Some(i) => i,
            None => {
                registry.push(Box::new(CommitScreens { arenas }));
                registry.len() - 1
            }
        };
        // The `Box`'s heap allocation does not move when the `Vec` grows, so
        // this pointer stays valid for as long as the registry owns it.
        let snapshot: *const CommitScreens<N> = &*registry[idx];
        self.current
            .store(snapshot as *mut CommitScreens<N>, Ordering::Release);
    }

    /// Unpublish the current snapshot if it names an arena starting at one of
    /// `bases` (a heap dropping its arenas). The snapshot itself stays in the
    /// registry, so a reader already holding it reads live memory.
    pub fn clear_if_names(&self, bases: &[usize]) {
        let _registry = self.registry.lock();
        let p = self.current.load(Ordering::Acquire);
        if p.is_null() {
            return;
        }
        // SAFETY: every non-null pointer `current` has ever held points into
        // a `Box` that `registry` owns; boxes are freed only when the table
        // drops, which cannot happen while `&self` is borrowed.
        let snapshot = unsafe { &*p };
        if snapshot.names_any_base(bases) {
            self.current.store(std::ptr::null_mut(), Ordering::Release);
        }
    }

    /// Is `[addr, addr + len)` committed, as the current snapshot says?
    ///
    /// `true` before anything is published. `outside` for an `addr` in none
    /// of the snapshot's arenas: `true` for the process-global young screen
    /// (not its question), `false` for a heap's own screen (the caller found
    /// `addr` inside a region whose bounds this snapshot was published with,
    /// so a miss means bounds and bitmap came from different publishes — the
    /// "do not read" direction). `false` for a reader that kept losing races
    /// with republishes.
    #[inline]
    pub fn range_committed_or(&self, addr: usize, len: usize, outside: bool) -> bool {
        match self.lookup(addr, len) {
            None => true,
            Some(answer) => answer.unwrap_or(outside),
        }
    }

    /// The raw read behind [`Self::range_committed_or`]: `None` when nothing
    /// is published, `Some(None)` when `addr` is in none of the current
    /// snapshot's arenas, `Some(Some(committed))` otherwise, and
    /// `Some(Some(false))` for a reader that kept losing races with
    /// republishes.
    #[inline]
    pub fn lookup(&self, addr: usize, len: usize) -> Option<Option<bool>> {
        for _ in 0..YOUNG_COMMIT_SCREEN_READ_ATTEMPTS {
            let p = self.current.load(Ordering::Acquire);
            if p.is_null() {
                return None;
            }
            // SAFETY: as in `clear_if_names`: a published snapshot is
            // immutable and lives as long as the table.
            let snapshot = unsafe { &*p };
            let answer = snapshot.range_committed(addr, len);
            // A republish between the two loads means this answer may describe
            // arenas that were just re-backed. Only a reader overlapping a
            // pause can see that (the one writer runs inside the pause); ask
            // the new snapshot instead.
            if self.current.load(Ordering::Acquire) == p {
                return Some(answer);
            }
        }
        Some(Some(false))
    }

    /// See [`published_young_range_committed`].
    pub fn range_committed(&self, addr: usize, len: usize) -> bool {
        self.range_committed_or(addr, len, true)
    }
}

/// Is `[addr, addr + len)` backed by committed pages, for an `addr` inside a
/// published young region?
///
/// `true` for an address outside both young regions (not this screen's
/// question) and for a region that publishes no bitmap (a wholly committed
/// store). `false` for a span that touches a decommitted granule — nothing
/// there can be an object, so a caller about to read a header must not — and
/// for a reader that kept losing races with republishes (the "do not read"
/// direction).
pub fn published_young_range_committed(addr: usize, len: usize) -> bool {
    YOUNG_COMMIT_SCREENS.range_committed(addr, len)
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

/// The two young arenas' commit bitmaps, `(ptr, words)` per slot, mirrored
/// beside [`JIT_REGION_BOUNDS`] by the same writer at the same moments.
///
/// gen r4w4 (2026-09-24). The published young span is the arena's whole
/// `[base, base + capacity)`, but `uncommit_evacuated_young` (default ON)
/// hands the evacuated semi-space's unbumped middle and free blocks back to
/// the OS after a flip. A stale compiled-frame word that still points into that
/// half is inside the bounds and on an unmapped page, so a reader that treats
/// "inside the published young span" as "safe to dereference" faults.
/// `GenR4CardStorePathsProbe` under `gc-stress` hit exactly that in the band
/// verifier's object screen (`[word + 8]`, about one run in seven). The per-heap
/// screen (`region_range_committed`) needs a heap handle; the band verifier's
/// callers have none, so this is the process-global counterpart.
///
/// The bitmaps are the reservations' LIVE maps, updated at every commit and
/// decommit, so a republish is needed only when an arena is swapped or
/// re-backed — which is when `store_region_bounds_locked` runs.
///
/// gen r4w6/review6 (2026-09-24): ONE pointer to an immutable, never-freed
/// snapshot ([`CommitScreens`]) instead of a `(ptr, words)` pair per slot
/// read against [`JIT_REGION_BOUNDS`]. The pair could tear three ways
/// (`docs/feature-designs/generative-gc-architecture.md`
/// item 1): one publish's pointer with another's length, a pointer whose `Arc`
/// the writer dropped under the reader, and a slot whose bounds and bitmap
/// named different arenas after a flip. Now one `Acquire` load hands the reader
/// both arenas' bounds and bitmaps from one publish, and nothing it can reach
/// is ever freed.
pub static YOUNG_COMMIT_SCREENS: CommitScreenTable<2> = CommitScreenTable::new();

/// One arena's commit screen, as one publish saw it.
///
/// gen r5w1/young5 (2026-09-26): generic over the table width, so the
/// process-global young screen (two arenas) and the per-heap screen
/// `GenerationalHeap::commit_bits` (three: young from, young to, old) are one
/// type with one reader. The per-heap one used to be a `(ptr, words)` pair per
/// slot with the same tearing shapes the global one had before gen r4w6.
#[derive(Debug)]
pub struct CommitScreen {
    /// The arena's `[base, end)`: the same span `store_region_bounds_locked`
    /// publishes in [`JIT_REGION_BOUNDS`] / `region_bounds` for this arena.
    pub base: usize,
    pub end: usize,
    /// The arena reservation's live commit map, or `None` for a wholly
    /// committed store (nothing to screen). Its length is ITS OWN, so no
    /// other publish can pair it with a different length.
    pub bits: Option<Arc<[AtomicU64]>>,
}

/// `N` arenas' screens from one publish. Immutable once published, and never
/// freed while its table lives (see [`CommitScreenTable`]).
#[derive(Debug)]
pub struct CommitScreens<const N: usize> {
    pub arenas: [Option<CommitScreen>; N],
}

impl<const N: usize> CommitScreens<N> {
    /// `Some(committed)` for an `addr` inside one of the arenas, `None`
    /// outside all of them.
    #[inline]
    pub fn range_committed(&self, addr: usize, len: usize) -> Option<bool> {
        self.arenas
            .iter()
            .flatten()
            .find_map(|s| s.range_committed(addr, len))
    }

    /// The same arenas, in the same slots, with the same bitmaps?
    pub fn same_as(&self, other: &[Option<CommitScreen>; N]) -> bool {
        self.arenas
            .iter()
            .zip(other.iter())
            .all(|(a, b)| match (a, b) {
                (None, None) => true,
                (Some(a), Some(b)) => a.same_as(b),
                _ => false,
            })
    }

    /// Does any arena start at one of `bases`?
    pub fn names_any_base(&self, bases: &[usize]) -> bool {
        self.arenas
            .iter()
            .flatten()
            .any(|s| bases.contains(&s.base))
    }
}

/// How often [`CommitScreenTable::range_committed_or`] re-reads a snapshot
/// that a republish replaced while it was being consulted, before it answers
/// `false` ("do not read").
pub const YOUNG_COMMIT_SCREEN_READ_ATTEMPTS: usize = 4;


impl CommitScreen {
    /// `Some(committed)` for an `addr` inside this arena, `None` outside it.
    #[inline]
    pub fn range_committed(&self, addr: usize, len: usize) -> Option<bool> {
        if addr < self.base || addr >= self.end {
            return None;
        }
        Some(match &self.bits {
            Some(bits) => crate::reservation::range_committed(bits, addr - self.base, len),
            None => true,
        })
    }

    /// The same arena bounds with the same bitmap?
    pub fn same_as(&self, other: &CommitScreen) -> bool {
        self.base == other.base
            && self.end == other.end
            && match (&self.bits, &other.bits) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

