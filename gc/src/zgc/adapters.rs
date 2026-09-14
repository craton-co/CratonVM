// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The seam layer between the ZGC submodules — and a standing list of the
//! places where two of them do not agree.
//!
//! # Why this file exists
//!
//! The modules under `crate::zgc::` (`vaddr`, `page`, `barrier`, `forwarding`,
//! `metrics`, `remembered`, `mark`, `generation`, `relocate`, `tlab`,
//! `census`) were written **in parallel by authors who could not see one
//! another's work and could not compile against it**. Each is deliberately
//! decoupled: it declares its own trait and its own plain-data structs rather
//! than importing a sibling's concrete types. That is what let them land
//! independently — and it is also why *the glue between them does not exist*.
//! `gc/tests/zgc_module_integration.rs` enumerated the gap by hand-rolling the
//! missing conversions inside the test binary; this module is those
//! conversions, moved into `src` so there is exactly one of each.
//!
//! # An adapter here is a SYMPTOM, not a design
//!
//! Read every item below as a defect report with a workaround attached. Where
//! two modules model the same thing with two different types, the long-term
//! fix is for **the two modules to agree on one type** and for the adapter to
//! be deleted — not for this file to grow. Specifically:
//!
//! * [`zfwd_size_class_index`] exists only because `page::ZPageSizeClass` is
//!   an enum and `forwarding`'s size class is a bare `u8`. The right fix is
//!   for `forwarding` to take the enum (or for the enum to be `#[repr(u8)]`
//!   with the same discriminants and the constants to be deleted).
//! * [`page_candidate`] was *two* functions, because `page` and `forwarding`
//!   computed live occupancy against **different denominators**. That semantic
//!   disagreement has been settled in favour of the allocated extent — see the
//!   dated 2026-08-07 note on [`ZPageReal::relocation_capacity_bytes`] — so
//!   there is now one mapping and no choice at the call site.
//! * [`ZHeapGenerationContext`] exists because `remembered`'s only shipped
//!   context models the heap as two contiguous ranges and `page` does not lay
//!   the heap out that way.
//! * [`ZTableRememberedSetView`] exists over a trait **this module declares**
//!   ([`ZOldSlotReader`]) because `remembered` and `generation` genuinely
//!   cannot compose today — see that trait's header.
//!
//! # Policy: none
//!
//! Nothing here decides anything. Every function is a projection of data one
//! module already owns into the shape another module already asks for. If a
//! future edit finds itself choosing a threshold, a default, or a fallback
//! behaviour, that is the signal that the two modules disagree and the choice
//! belongs in one of them, with a test, not here.
//!
//! # Address domains — read this before adding anything
//!
//! Three different "addresses" appear at these seams and they are NOT
//! interchangeable:
//!
//! | domain | width | who speaks it |
//! |---|---|---|
//! | **uncolored machine address** | `usize` (or `u64`) | `page` (`base`/`end`/`alloc`), `generation` (`ZGenerationAllocation::address`, `ZGenerationMarker`'s roots), `remembered`'s `ZGenerationContext` and `ZStoreBarrier` |
//! | **42-bit heap offset** | `u64`, masked by `vaddr::Z_OFFSET_MASK` | `vaddr`, `barrier`'s masks, `forwarding`'s `to` field |
//! | **page-relative offset** | `usize`, grain-aligned | `remembered`'s bitmaps (`iterate`, `remember`, `is_remembered`) |
//!
//! **Every adapter in this file works exclusively in the first two of those,
//! and never converts between domain 1 and domain 2.** Doing so requires a
//! heap base that no module at these seams owns, and the 42-bit offset field
//! is already known to be too narrow for a real Linux machine address (see
//! `zgc_module_integration.rs`'s ignored
//! `forwarding_to_field_holds_a_real_zpage_heap_address`). Every parameter
//! below names its domain explicitly; keep that up.
//!
//! # Page-id width
//!
//! `page`, `forwarding` and `generation` key pages by `u64`. `remembered` used
//! to key them by `u32`, which was an aliasing bug on a timer
//! (`ZPageAllocator`'s ids are monotonic and never recycled downward, so a
//! long-running VM reaches `u32::MAX`). As of this writing that has been
//! widened and **everything in this file is written against `u64` page ids**.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::zgc::forwarding::{
    PageCandidate, ZFWD_SIZE_CLASS_LARGE, ZFWD_SIZE_CLASS_MEDIUM, ZFWD_SIZE_CLASS_SMALL,
};
use crate::zgc::generation::{ZGeneration, ZGenerationalHeap, ZRememberedSetView};
use crate::zgc::page::{ZPageReal, ZPageSizeClass};
use crate::zgc::remembered::{ZGenerationContext, ZRememberedSetTable};

// ===========================================================================
// Seam 1: page::ZPageSizeClass  <->  forwarding::ZFWD_SIZE_CLASS_*
// ===========================================================================

/// `page`'s size-class **enum** → `forwarding`'s size-class **`u8`**.
///
/// * Direction: `page` → `forwarding`. Total, infallible, no units involved.
/// * Failure cases: none. The match is exhaustive by construction, so adding a
///   fourth `ZPageSizeClass` variant is a compile error here rather than a
///   silent mis-classification at a call site.
///
/// # Why this is worth a named function
///
/// Getting `Large` wrong is not a cosmetic bug. `forwarding`'s relocation
/// policy has exactly one structural exclusion —
/// `ZRelocationPolicy::relocate_large_pages`, `false` by default — and it is
/// implemented as `PageCandidate::is_large()`, i.e. as a comparison against
/// [`ZFWD_SIZE_CLASS_LARGE`]. A caller that hand-rolls this mapping and
/// mis-numbers `Large` **admits large pages to the relocation set**, which is
/// the single most expensive copy in the heap and buys nothing (a large page
/// holds one object sized to fit, so there is no fragmentation to recover).
/// `page::ZPageReal::is_relocation_candidate` excludes `Large` independently,
/// so the two gates are meant to agree; this function is what makes them
/// agree.
///
/// # The real fix
///
/// Delete this function by making the two modules share one type. `page`'s
/// enum is not `#[repr(u8)]` and carries no discriminants, so today there is
/// no ABI-level identity to lean on and the match is the honest bridge.
#[inline]
pub fn zfwd_size_class_index(class: ZPageSizeClass) -> u8 {
    match class {
        ZPageSizeClass::Small => ZFWD_SIZE_CLASS_SMALL,
        ZPageSizeClass::Medium => ZFWD_SIZE_CLASS_MEDIUM,
        ZPageSizeClass::Large => ZFWD_SIZE_CLASS_LARGE,
    }
}

/// `forwarding`'s size-class **`u8`** → `page`'s size-class **enum**.
///
/// * Direction: `forwarding` → `page`. Partial.
/// * Failure case: `None` for any value that is not one of the three declared
///   constants. `forwarding`'s field is a bare `u8`, so out-of-range values are
///   representable; a caller that needs a total function must decide what an
///   unknown class means, and that decision does not belong in this file.
///
/// The inverse of [`zfwd_size_class_index`] on the three legal inputs. The
/// round trip is asserted in this module's tests, which is what keeps the two
/// numberings pinned together.
#[inline]
pub fn zpage_size_class_of_index(index: u8) -> Option<ZPageSizeClass> {
    match index {
        ZFWD_SIZE_CLASS_SMALL => Some(ZPageSizeClass::Small),
        ZFWD_SIZE_CLASS_MEDIUM => Some(ZPageSizeClass::Medium),
        ZFWD_SIZE_CLASS_LARGE => Some(ZPageSizeClass::Large),
        _ => None,
    }
}

// ===========================================================================
// Seam 2: page::ZPageReal  ->  forwarding::PageCandidate
// ===========================================================================

/// A live [`ZPageReal`] → a [`PageCandidate`], measuring capacity as the
/// **allocated extent** ([`ZPageReal::relocation_capacity_bytes`], i.e. the
/// bump cursor).
///
/// * Direction: `page` → `forwarding`. Total, infallible.
/// * Units: bytes throughout. No address is carried — `PageCandidate` is four
///   integers and identifies the page by id, so no address domain is involved.
/// * Snapshot semantics: `live_bytes` is an `AtomicUsize` read `Relaxed` by
///   `page`, so the candidate is a **snapshot at call time**. Build the whole
///   candidate vector at one point in the cycle (after mark, before selection)
///   rather than re-reading pages during selection.
///
/// # The denominator: decided elsewhere, not chosen here
///
/// `page` and `forwarding` once divided by different denominators, so this
/// file shipped *two* mappings and refused to pick. **`page` has since
/// decided, and the allocated extent won.** The argument is not restated here:
/// it is the dated 2026-08-07 note on
/// [`ZPageReal::relocation_capacity_bytes`], which is the specification for
/// the `capacity_bytes` line below. The page-span measure (`ZPageReal::size()`)
/// is not a supported alternative and its mapping has been deleted — do not
/// reintroduce it.
///
/// With one denominator the two modules' arithmetic is identical on the same
/// page:
///
/// ```text
/// candidate.garbage_bytes()   == page.garbage_bytes()
/// candidate.live_occupancy()  == page.live_ratio()      (for used() > 0)
/// ```
///
/// so `forwarding`'s `max_live_occupancy` and `page`'s
/// `is_relocation_candidate(live_ratio_threshold)` mean the same thing and the
/// two independent gates cannot disagree.
///
/// # And what it costs
///
/// `PageCandidate::live_occupancy` returns `1.0` when `capacity_bytes == 0`,
/// and `ZRelocationSet::select` skips `capacity_bytes == 0` outright. A page
/// with `used() == 0` (freshly reset, or handed out and never allocated into)
/// therefore drops out of selection. That is the right outcome — an empty page
/// must be *freed*, not evacuated — but no module currently performs that
/// sweep. That gap is real and is reported rather than patched.
pub fn page_candidate(page: &ZPageReal) -> PageCandidate {
    PageCandidate {
        page_id: page.id(),
        live_bytes: page.live_bytes(),
        capacity_bytes: page.relocation_capacity_bytes(),
        size_class_index: zfwd_size_class_index(page.size_class()),
    }
}

/// Map a slice of pages through [`page_candidate`].
///
/// A convenience for the common `ZPageAllocator::pages()` /
/// `ZOldGeneration::snapshot()` → `ZRelocationSet::select` path. There is one
/// capacity measure and therefore no choice to make at the call site; for why,
/// see the dated note on [`ZPageReal::relocation_capacity_bytes`].
pub fn page_candidates(pages: &[Arc<ZPageReal>]) -> Vec<PageCandidate> {
    pages.iter().map(|p| page_candidate(p.as_ref())).collect()
}

// ===========================================================================
// Seam 3: generation::ZGenerationalHeap  ->  remembered::ZGenerationContext
// ===========================================================================

/// A [`ZGenerationContext`] (the trait `remembered`'s store barrier consumes)
/// backed by a real [`ZGenerationalHeap`] over real `page` pages.
///
/// * Direction: `generation` → `remembered`.
/// * Address domain: **uncolored machine addresses**, throughout. The trait
///   speaks `u64`; `page` and `generation` speak `usize`. The conversion is
///   checked with `usize::try_from`, so on a 32-bit host an address above
///   `usize::MAX` answers "not mine" rather than truncating into a wrong page.
/// * Failure cases: `is_old`/`is_young` answer `false` for any address the
///   allocator does not own; `page_of` answers `None` for the same.
///
/// # Why `remembered`'s own context cannot do this
///
/// `remembered` ships `ZRangeGenerationContext`, which models the heap as two
/// contiguous address ranges plus a fixed old-page stride. `page` does not lay
/// the heap out that way: young and old pages are **interleaved in one granule
/// pool** and either generation may own any page (`ZGenerationalHeap::new`
/// says so explicitly — the young/old split is a byte *budget*, not a
/// partition of the reservation). No pair of ranges and no stride can describe
/// that, so the shipped context is unusable against a real heap and nothing in
/// `src` bridged them.
///
/// # ⚠️ This is correct but it is NOT the fast path the trait asks for
///
/// `remembered`'s trait doc is explicit that `is_old` and `is_young` sit on
/// **every reference store in the VM** and "must be a handful of instructions
/// with no memory load, no lock, and no allocation". This implementation is
/// none of those: each call is one `ZPageAllocator::page_for` (hash probe plus
/// an `Arc` clone — an atomic increment and a later decrement) followed by up
/// to two more hash probes, each taking a `parking_lot::Mutex` on a generation
/// map. That is three locks and a refcount round trip per reference store.
///
/// That cost is **not fixable in an adapter**, and deliberately is not
/// attempted here. The two candidate fixes both live in the modules:
/// a generation bit in the colored pointer (which `vaddr`'s
/// OpenJDK-non-generational 4-bit metadata field has no room for — bits 62..46
/// are reserved-must-be-zero and are where the room would come from), or a
/// lock-free `granule index → generation` side array owned by `page`. Use this
/// adapter for correctness work, tests, and stop-the-world paths; do not put
/// it under a mutator write barrier and call the result a benchmark.
///
/// # Borrowing
///
/// Holds a shared borrow of the heap, so it is a stack temporary at the call
/// site: `let ctx = ZHeapGenerationContext::new(&heap);`. It performs no
/// interior mutation and takes no lock of its own, so any number of threads
/// may hold one over the same heap.
#[derive(Clone, Copy)]
pub struct ZHeapGenerationContext<'h> {
    heap: &'h ZGenerationalHeap,
}

/// Compact by design: `ZGenerationalHeap`'s own `Debug` impl takes both
/// generation mutexes, and `generation` warns that formatting those types while
/// holding their lock is a `parking_lot` re-entrancy hazard. A barrier context
/// is exactly the thing a panic handler might format at a bad moment, so this
/// one formats nothing that locks.
impl std::fmt::Debug for ZHeapGenerationContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZHeapGenerationContext")
            .field("heap_base", &self.heap.allocator().base())
            .field("heap_end", &self.heap.allocator().end())
            .finish()
    }
}

impl<'h> ZHeapGenerationContext<'h> {
    /// Wrap `heap`.
    #[inline]
    pub fn new(heap: &'h ZGenerationalHeap) -> Self {
        ZHeapGenerationContext { heap }
    }

    /// The heap this context answers for.
    #[inline]
    pub fn heap(&self) -> &'h ZGenerationalHeap {
        self.heap
    }
}

impl ZGenerationContext for ZHeapGenerationContext<'_> {
    /// `addr` is an uncolored machine address. `false` for an address outside
    /// the allocator's reservation, and for an address in a page neither
    /// generation has claimed.
    fn is_old(&self, addr: u64) -> bool {
        match usize::try_from(addr) {
            Ok(a) => self.heap.generation_of_address(a) == Some(ZGeneration::Old),
            Err(_) => false,
        }
    }

    /// `addr` is an uncolored machine address. See [`Self::is_old`].
    fn is_young(&self, addr: u64) -> bool {
        match usize::try_from(addr) {
            Ok(a) => self.heap.generation_of_address(a) == Some(ZGeneration::Young),
            Err(_) => false,
        }
    }

    /// `addr` is an uncolored machine address; the returned offset is
    /// **page-relative, in bytes** (`addr - page.base()`), which is the domain
    /// `ZRememberedSet::remember` and `::iterate` speak.
    ///
    /// Answers for **any** page the allocator owns, young or old — a superset
    /// of what the trait requires (it documents that only old pages need to be
    /// mappable, because the barrier calls this only after `is_old` said yes).
    /// Narrowing it to old pages would be a policy this adapter has no
    /// business adding, and the extra answers are harmless: the caller has
    /// already made the generation decision.
    fn page_of(&self, addr: u64) -> Option<(u64, usize)> {
        let a = usize::try_from(addr).ok()?;
        let page = self.heap.allocator().page_for(a)?;
        // `page_for` resolved it, so `a >= page.base()` holds and the
        // subtraction cannot wrap. Use a checked form anyway: this is the one
        // arithmetic that decides which bit gets set.
        let offset = a.checked_sub(page.base())?;
        Some((page.id(), offset))
    }
}

// ===========================================================================
// Seam 4: remembered::ZRememberedSetTable  ->  generation::ZRememberedSetView
//
// THE ONE THAT DOES NOT COMPOSE.
// ===========================================================================

/// The capability neither `remembered` nor `generation` provides, and without
/// which the two cannot be joined.
///
/// # ⚠️ Read this before using [`ZTableRememberedSetView`]
///
/// The two sides of this seam are not two spellings of one thing. They are two
/// *different facts*:
///
/// * `remembered::ZRememberedSetTable` records **where the reference is** — a
///   bit per 8-byte grain, page-relative, per old page. Iterating it yields
///   *slot locations in old*.
/// * `generation::ZRememberedSetView::iterate_old_to_young` must yield **what
///   the reference points at** — the addresses of young objects, which the
///   minor cycle consumes as extra roots.
///
/// Going from the first to the second requires **dereferencing the old slot**.
/// That read exists in neither module, and in both cases on purpose:
///
/// * `remembered` states that the read is legitimately its own job — its
///   `ZRememberedSetView`-facing rationale in `generation` says "the card scan
///   happens inside the remembered-set module, which legitimately owns old
///   memory" — but it **exposes no API that performs it**. `ZRememberedSet`
///   knows a page id, a page size and a bitmap; it does not know the page's
///   base address and it never touches memory outside its own bitmap words.
/// * `generation` withholds the capability by design: the whole
///   no-tracing-into-old property rests on the minor cycle never being handed
///   anything that can dereference old. Giving the view a raw read would
///   reintroduce the old-generation traversal through the back door, which is
///   precisely what the trait's doc comment says the signature exists to
///   prevent.
///
/// So the adapter is written against **this trait, declared here**, rather
/// than against a raw pointer read. That is not a stylistic preference:
/// `ZPageAllocator` only ever exposes `as_ptr()`-derived addresses as `usize`,
/// so fabricating a `*const u64` from one and reading through it would be UB
/// under Stacked Borrows in addition to being an unsound way to read a slot a
/// mutator may be concurrently writing.
///
/// # What `remembered.rs` must add for this trait to be deletable
///
/// One method, on `ZRememberedSetTable` (or on a new type beside it), of
/// roughly this shape:
///
/// ```text
/// /// Visit every remembered old->young edge as (slot_addr, referent).
/// pub fn iterate_edges(&self, snapshot: bool, f: &mut dyn FnMut(u64, u64));
/// ```
///
/// Three things have to come with it, and all three are decisions only
/// `remembered` can make:
///
/// 1. **A page id → base address map.** `ZRememberedSet` stores `page_id` and
///    `page_size` but not `base`. Either `register_old_page` starts taking the
///    base, or the table gains a resolver callback. Today every caller must
///    supply the map out of band, which is what
///    [`ZTableRememberedSetView::page_bases`] is and why a missing entry is a
///    silently-dropped root.
/// 2. **The load's atomicity and ordering.** The slot is being written by
///    mutators concurrently. It has to be at least a relaxed atomic load of an
///    8-byte word, and the doc has to say whether a torn/stale read is
///    acceptable (it is, if the result is treated as a hint — but that has to
///    be stated, not assumed).
/// 3. **Whether the yielded word is colored.** A reference slot in a real ZGC
///    heap holds a `vaddr::ZColoredWord`, while
///    `ZGenerationMarker::mark_from_roots` documents its roots as "uncolored
///    machine addresses". Somebody has to strip the colour and add the heap
///    base, and neither `remembered` nor `generation` currently claims that
///    step. **This adapter does not do it either** — see
///    [`ZTableRememberedSetView`]'s note. It is the highest-risk unassigned
///    responsibility at this seam.
///
/// Until that method exists, an implementation of this trait has to come from
/// whoever owns the heap storage, and every such implementation is a place the
/// three decisions above get made silently and differently.
pub trait ZOldSlotReader {
    /// Read the reference word at `slot_addr`.
    ///
    /// `slot_addr` is an **uncolored machine address**, grain-aligned, inside
    /// an old page. The returned word is in whatever domain the implementation
    /// stores — see decision 3 above; the adapter passes it through unchanged.
    ///
    /// Returns `None` if the implementation cannot read that address at all.
    /// `Some(0)` means the slot is null.
    fn read_slot(&self, slot_addr: u64) -> Option<u64>;
}

/// A [`ZOldSlotReader`] backed by an explicit `slot address → value` map.
///
/// **This is a test double and a specification, not a production reader.** It
/// models the read exactly — same domain, same failure channel — without
/// touching real memory, which is what lets this module's tests and
/// `gc/tests/zgc_module_integration.rs` exercise the whole seam while the real
/// capability described on [`ZOldSlotReader`] does not exist.
///
/// Keys and values are uncolored machine addresses.
#[derive(Debug, Default, Clone)]
pub struct ZMapSlotReader {
    slots: FxHashMap<u64, u64>,
}

impl ZMapSlotReader {
    /// An empty reader: every slot is unreadable.
    pub fn new() -> Self {
        ZMapSlotReader {
            slots: FxHashMap::default(),
        }
    }

    /// Record that the word at `slot_addr` holds `value`.
    pub fn insert(&mut self, slot_addr: u64, value: u64) {
        self.slots.insert(slot_addr, value);
    }

    /// Builder form of [`Self::insert`].
    pub fn with_slot(mut self, slot_addr: u64, value: u64) -> Self {
        self.insert(slot_addr, value);
        self
    }

    /// Number of recorded slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether any slot has been recorded.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

impl ZOldSlotReader for ZMapSlotReader {
    fn read_slot(&self, slot_addr: u64) -> Option<u64> {
        self.slots.get(&slot_addr).copied()
    }
}

/// Which of `remembered`'s two bitmaps [`ZTableRememberedSetView`] iterates.
///
/// `ZRememberedSet` is double-buffered: mutators dirty one buffer while the
/// collector scans the other. Both are reachable (`iterate` /
/// `iterate_snapshot`), and choosing between them is a caller decision that
/// this module surfaces rather than makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZRemsetBuffer {
    /// The buffer mutators are currently dirtying (`ZRememberedSet::iterate`).
    ///
    /// Races with concurrent `remember` calls. `remembered` documents this
    /// variant as being for tests, diagnostics, and the fully stop-the-world
    /// case.
    Dirty,
    /// The collector's stable snapshot (`ZRememberedSet::iterate_snapshot`) —
    /// the buffer that was current before the last
    /// `ZRememberedSetTable::swap_all`.
    ///
    /// **This is the young collection's root-scan entry point**, and it is
    /// empty until `swap_all` has been called at least once. A minor cycle
    /// that forgets the swap and reads this buffer sees *no* remembered edges
    /// and frees live objects, which is why the two variants are named rather
    /// than defaulted.
    Snapshot,
}

/// Bridges `remembered::ZRememberedSetTable` to
/// `generation::ZRememberedSetView`, using a caller-supplied
/// [`ZOldSlotReader`] for the read neither module provides.
///
/// * Direction: `remembered` → `generation`.
/// * Address domain in: page-relative byte offsets (from the bitmaps) plus
///   uncolored machine page bases (from `page_bases`).
/// * Address domain out: whatever [`ZOldSlotReader::read_slot`] returns,
///   **passed through unchanged**. `generation` expects uncolored machine
///   addresses; if the reader hands back colored words, this adapter does not
///   notice and does not fix it. See decision 3 on [`ZOldSlotReader`].
///
/// # Failure cases, all of which lose roots
///
/// Both are counted rather than panicked, because this sits on a collector
/// path where a panic is worse than a lost root is — but a lost root is a
/// *use-after-free*, so both counters must be asserted zero by any caller that
/// cares about correctness. `generation::ZRememberedSetView` has **no
/// coarsening channel** — there is no way to say "scan this page in full,
/// I could not resolve it" — which is itself a gap worth closing in
/// `generation`.
///
/// * [`Self::unresolved_pages`] — a registered old page whose base address is
///   not in `page_bases`. Every edge on that page is dropped.
/// * [`Self::unreadable_slots`] — a set bit whose slot the reader could not
///   read. That edge is dropped.
///
/// A slot that reads as `Some(0)` is **not** counted as a failure and is not
/// yielded: `remembered`'s own store barrier filters `stored_value == 0`
/// before recording anything, so a null here means the field was overwritten
/// after the bit was set. `generation`'s trait documents entries as hints, and
/// a stale hint is dropped rather than trusted.
pub struct ZTableRememberedSetView<R: ZOldSlotReader> {
    table: Arc<ZRememberedSetTable>,
    /// `page id → page base`, an **uncolored machine address**.
    ///
    /// Required because `ZRememberedSet` yields page-relative offsets and
    /// knows no base — see decision 1 on [`ZOldSlotReader`].
    page_bases: FxHashMap<u64, u64>,
    reader: R,
    buffer: ZRemsetBuffer,
    unresolved_pages: AtomicUsize,
    unreadable_slots: AtomicUsize,
}

impl<R: ZOldSlotReader> ZTableRememberedSetView<R> {
    /// Build a view over `table`, resolving page bases through `page_bases`.
    ///
    /// `page_bases` maps `u64` page id → uncolored machine base address.
    /// [`old_page_bases`] builds it from a [`ZGenerationalHeap`].
    pub fn new(
        table: Arc<ZRememberedSetTable>,
        page_bases: FxHashMap<u64, u64>,
        reader: R,
        buffer: ZRemsetBuffer,
    ) -> Self {
        ZTableRememberedSetView {
            table,
            page_bases,
            reader,
            buffer,
            unresolved_pages: AtomicUsize::new(0),
            unreadable_slots: AtomicUsize::new(0),
        }
    }

    /// Build a view whose page bases come from `heap`'s **old** generation, as
    /// of now.
    ///
    /// The map is a snapshot: a page promoted into old after this call will not
    /// be resolvable and its edges will be counted by
    /// [`Self::unresolved_pages`]. Build the view after the last promotion of
    /// the cycle, or use [`Self::new`] with a map you maintain.
    pub fn over_old_generation(
        heap: &ZGenerationalHeap,
        table: Arc<ZRememberedSetTable>,
        reader: R,
        buffer: ZRemsetBuffer,
    ) -> Self {
        Self::new(table, old_page_bases(heap), reader, buffer)
    }

    /// The table this view reads.
    pub fn table(&self) -> &Arc<ZRememberedSetTable> {
        &self.table
    }

    /// Which bitmap this view iterates.
    pub fn buffer(&self) -> ZRemsetBuffer {
        self.buffer
    }

    /// Registered old pages skipped because `page_bases` had no entry.
    ///
    /// **Every one of these is a set of dropped old→young edges.** Assert this
    /// is zero.
    pub fn unresolved_pages(&self) -> usize {
        self.unresolved_pages.load(Ordering::Relaxed)
    }

    /// Set bits whose slot [`ZOldSlotReader::read_slot`] could not read.
    ///
    /// **Every one of these is a dropped old→young edge.** Assert this is
    /// zero.
    pub fn unreadable_slots(&self) -> usize {
        self.unreadable_slots.load(Ordering::Relaxed)
    }

    /// Zero both failure counters. For a caller that reuses one view across
    /// cycles and asserts per cycle.
    pub fn reset_counters(&self) {
        self.unresolved_pages.store(0, Ordering::Relaxed);
        self.unreadable_slots.store(0, Ordering::Relaxed);
    }
}

impl<R: ZOldSlotReader> ZRememberedSetView for ZTableRememberedSetView<R> {
    fn iterate_old_to_young(&self, f: &mut dyn FnMut(u64)) {
        // `snapshot()` clones the `Arc`s and drops the registry guard before
        // returning — `remembered` is explicit that holding that guard across
        // page work is an ABBA deadlock source in this tree. Do not replace
        // this with anything that keeps the lock.
        for set in self.table.snapshot() {
            let page_id = set.page_id();
            let base = match self.page_bases.get(&page_id) {
                Some(b) => *b,
                None => {
                    self.unresolved_pages.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        target: "zgc::adapters",
                        page_id,
                        bits_set = set.bits_set(),
                        "remembered-set view: no base address for registered old page; \
                         every old->young edge on it is being DROPPED",
                    );
                    continue;
                }
            };

            let visit = |offset: usize| {
                let slot = base.saturating_add(offset as u64);
                match self.reader.read_slot(slot) {
                    // A live edge. Passed through in the reader's domain.
                    Some(value) if value != 0 => f(value),
                    // Null: the field was cleared after the bit was set. A
                    // stale hint, dropped per `generation`'s contract.
                    Some(_) => {}
                    None => {
                        self.unreadable_slots.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(
                            target: "zgc::adapters",
                            page_id,
                            slot,
                            "remembered-set view: slot unreadable; old->young edge DROPPED",
                        );
                    }
                }
            };

            match self.buffer {
                ZRemsetBuffer::Dirty => set.iterate(visit),
                ZRemsetBuffer::Snapshot => set.iterate_snapshot(visit),
            }
        }
    }

    /// Set bits in the selected buffer.
    ///
    /// This over-counts relative to what [`Self::iterate_old_to_young`]
    /// actually yields: a bit whose slot reads null, or whose page has no
    /// base, is counted here and not yielded. `generation` documents this as
    /// "approximate, for reporting only", so the divergence is inside the
    /// contract — but do not derive a correctness assertion from it.
    fn entry_count(&self) -> usize {
        match self.buffer {
            ZRemsetBuffer::Dirty => self.table.total_bits_set(),
            ZRemsetBuffer::Snapshot => self.table.total_bits_set_in_snapshot(),
        }
    }
}

impl<R: ZOldSlotReader> std::fmt::Debug for ZTableRememberedSetView<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZTableRememberedSetView")
            .field("buffer", &self.buffer)
            .field("known_page_bases", &self.page_bases.len())
            .field("unresolved_pages", &self.unresolved_pages())
            .field("unreadable_slots", &self.unreadable_slots())
            .finish()
    }
}

// ===========================================================================
// Small joins between generation and remembered
// ===========================================================================

/// `page id → page base` (uncolored machine address) for every page currently
/// in `heap`'s **old** generation.
///
/// * Direction: `generation`/`page` → `remembered`.
/// * Snapshot: taken under `ZOldGeneration`'s lock and released before return.
///
/// This map exists only because `ZRememberedSet` records a page id and a page
/// size but not a base — decision 1 on [`ZOldSlotReader`]. When
/// `remembered::register_old_page` starts taking the base, delete this.
pub fn old_page_bases(heap: &ZGenerationalHeap) -> FxHashMap<u64, u64> {
    let pages = heap.old().snapshot();
    let mut map: FxHashMap<u64, u64> = FxHashMap::default();
    map.reserve(pages.len());
    for page in pages.iter() {
        map.insert(page.id(), page.base() as u64);
    }
    map
}

/// Register every page currently in `heap`'s old generation with `table`,
/// using each page's own size.
///
/// * Direction: `generation`/`page` → `remembered`.
/// * Returns the number of pages passed to
///   `ZRememberedSetTable::register_old_page`. That call is idempotent, so
///   this is safe to re-run; the return value counts pages *offered*, not
///   pages newly created.
///
/// # Why this is glue and not policy
///
/// `remembered` deliberately does not import the page types: `register_old_page`
/// takes `page_size` as plain data precisely so it has no dependency on `page`.
/// The consequence is that *somebody* has to walk the old generation and pass
/// the sizes across, and nothing did. A page that is never registered has no
/// remembered set, so a store into it is `ZRememberOutcome::Coarsened` at best
/// (and `remembered` logs at `warn` that it "should never execute"), or a
/// dropped edge at worst.
///
/// This does **not** handle the reverse: a page leaving old (freed by a major
/// cycle) should be `remove`d from the table, and no module currently owns
/// that either. `ZGenerationalHeap` has no hook to hang it on. Left unfixed
/// and reported.
pub fn register_old_pages(heap: &ZGenerationalHeap, table: &ZRememberedSetTable) -> usize {
    let pages = heap.old().snapshot();
    for page in pages.iter() {
        table.register_old_page(page.id(), page.size());
    }
    tracing::debug!(
        target: "zgc::adapters",
        pages = pages.len(),
        registered = table.len(),
        "registered old-generation pages with the remembered-set table",
    );
    pages.len()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use crate::zgc::forwarding::{ZRelocationPolicy, ZRelocationSet};
    use crate::zgc::generation::{ZGenerationalConfig, ZPromotionPolicy};
    use crate::zgc::page::{ZPageAllocator, ZPageConfig};
    use crate::zgc::remembered::{ZStoreBarrier, ZStoreBarrierOutcome, Z_REMSET_GRAIN_BYTES};

    /// The 1/512-scale geometry `page.rs`'s own tests and
    /// `gc/tests/zgc_module_integration.rs` use: 4 KiB granule, 8 KiB small
    /// page, 64 KiB medium page, 256 KiB budget. Nothing here commits
    /// hundreds of MiB.
    fn small_geometry() -> ZPageConfig {
        ZPageConfig {
            granule_size: 4096,
            small_page_size: 8192,
            medium_page_size: 65536,
            small_object_limit: 8192 / 8,
            medium_object_limit: 65536 / 8,
            max_capacity: 4096 * 64,
        }
    }

    fn small_allocator() -> Arc<ZPageAllocator> {
        Arc::new(ZPageAllocator::new(small_geometry()).expect("scaled test geometry must validate"))
    }

    fn generational_heap(allocator: Arc<ZPageAllocator>, promotion_age: u32) -> ZGenerationalHeap {
        let config = ZGenerationalConfig {
            promotion: ZPromotionPolicy::with_age(promotion_age),
            ..ZGenerationalConfig::default()
        };
        ZGenerationalHeap::new(allocator, config)
    }

    // -- Seam 1 -------------------------------------------------------------

    /// PINS: that the enum→u8 mapping is a bijection onto the three declared
    /// constants. A drift here silently re-labels every page handed to
    /// relocation-set selection.
    #[test]
    fn size_class_maps_round_trip_through_both_directions() {
        for class in [
            ZPageSizeClass::Small,
            ZPageSizeClass::Medium,
            ZPageSizeClass::Large,
        ] {
            let index = zfwd_size_class_index(class);
            assert_eq!(
                zpage_size_class_of_index(index),
                Some(class),
                "{} did not survive the round trip through index {index}",
                class.as_str(),
            );
        }

        assert_eq!(
            zfwd_size_class_index(ZPageSizeClass::Small),
            ZFWD_SIZE_CLASS_SMALL
        );
        assert_eq!(
            zfwd_size_class_index(ZPageSizeClass::Medium),
            ZFWD_SIZE_CLASS_MEDIUM
        );
        assert_eq!(
            zfwd_size_class_index(ZPageSizeClass::Large),
            ZFWD_SIZE_CLASS_LARGE
        );

        // The three indices must be distinct, or two classes alias.
        assert_ne!(ZFWD_SIZE_CLASS_SMALL, ZFWD_SIZE_CLASS_MEDIUM);
        assert_ne!(ZFWD_SIZE_CLASS_MEDIUM, ZFWD_SIZE_CLASS_LARGE);
        assert_ne!(ZFWD_SIZE_CLASS_SMALL, ZFWD_SIZE_CLASS_LARGE);

        // An index outside the declared set has no class. `forwarding`'s field
        // is a bare u8, so this is representable and must not alias onto one
        // of the three.
        assert_eq!(zpage_size_class_of_index(3), None);
        assert_eq!(zpage_size_class_of_index(u8::MAX), None);
    }

    /// PINS: that `Large` maps to the constant `ZRelocationPolicy` actually
    /// excludes — asserted through `ZRelocationSet::select`, not by comparing
    /// integers.
    ///
    /// This is the assertion that matters. Two candidates differing in
    /// **nothing but the size class** must select differently: the Small one
    /// in, the Large one out. If the mapping ever numbers `Large` as something
    /// the policy does not exclude, the large page is admitted to the
    /// relocation set — the single most expensive copy in the heap, for zero
    /// reclaim.
    #[test]
    fn large_maps_to_the_index_the_relocation_policy_excludes() {
        // Deliberately attractive on every other axis: 1% live occupancy, far
        // under the 25% cutoff and far under the 64 MiB budget. The ONLY
        // reason to reject it is the size class.
        let make = |class: ZPageSizeClass, id: u64| PageCandidate {
            page_id: id,
            live_bytes: 10_000,
            capacity_bytes: 1_000_000,
            size_class_index: zfwd_size_class_index(class),
        };

        let policy = ZRelocationPolicy::default();
        assert!(
            !policy.relocate_large_pages,
            "the default policy must exclude large pages, or this test proves nothing"
        );

        let small = make(ZPageSizeClass::Small, 1);
        let medium = make(ZPageSizeClass::Medium, 2);
        let large = make(ZPageSizeClass::Large, 3);

        assert!(!small.is_large());
        assert!(!medium.is_large());
        assert!(
            large.is_large(),
            "zfwd_size_class_index(Large) does not produce the value \
             PageCandidate::is_large() tests for"
        );

        let set = ZRelocationSet::select(&[small, medium, large], &policy);
        let ids = set.page_ids();
        assert!(ids.contains(&1), "the small page must be selected");
        assert!(ids.contains(&2), "the medium page must be selected");
        assert!(
            !ids.contains(&3),
            "the LARGE page entered the relocation set: zfwd_size_class_index(Large) \
             is not the constant ZRelocationPolicy excludes"
        );
        assert!(!set.contains(3));
        assert_eq!(set.len(), 2);
    }

    // -- Seam 2 -------------------------------------------------------------

    /// PINS: that a candidate built from a real page carries that page's own
    /// id, live bytes and class, and that its capacity is the allocated extent
    /// — the identity that keeps `forwarding`'s gate and `page`'s gate talking
    /// about the same number.
    #[test]
    fn a_page_candidate_carries_the_real_pages_live_and_capacity_bytes() {
        let allocator = small_allocator();
        let page = allocator
            .alloc_page(ZPageSizeClass::Small, 0)
            .expect("small page");

        // Allocate part of the page and mark part of that live, so `used()`
        // and `size()` differ and the wrong denominator would be visible.
        let object_bytes = 512usize;
        let addr = page.alloc(object_bytes, 8).expect("bump allocation fits");
        assert!(page.contains(addr));
        assert_eq!(page.used(), object_bytes);
        assert!(
            page.used() < page.size(),
            "the fixture must leave the page partly filled, or a capacity taken \
             from size() would be indistinguishable from the right one"
        );
        page.set_live_bytes(128);

        let extent = page_candidate(&page);
        assert_eq!(extent.page_id, page.id());
        assert_eq!(extent.live_bytes, 128);
        assert_eq!(extent.capacity_bytes, page.relocation_capacity_bytes());
        assert_eq!(extent.capacity_bytes, page.used());
        assert_eq!(extent.size_class_index, ZFWD_SIZE_CLASS_SMALL);
        assert_eq!(
            zpage_size_class_of_index(extent.size_class_index),
            Some(page.size_class())
        );

        // The allocated-extent measure is the one that agrees with `page.rs`'s
        // own arithmetic. This is the identity that makes `forwarding`'s
        // max_live_occupancy and `page`'s live_ratio_threshold mean the same.
        assert_eq!(extent.garbage_bytes(), page.garbage_bytes());
        assert_eq!(extent.live_occupancy(), page.live_ratio());

        // NOTE: this test used to build a second candidate from `page.size()`
        // and `assert_ne!` that the two measures diverge, as a standing report
        // that the modules disagreed. Those assertions are deliberately gone.
        // The divergence is still perfectly reachable — it is not an
        // unreachable corner — but since the denominator was decided (see the
        // dated 2026-08-07 note on `ZPageReal::relocation_capacity_bytes`) it
        // is a *bug to prevent*, not a fact to preserve. Pinning it would pin
        // the wrong measure back into existence. Do not re-add it.

        let batch = page_candidates(&[Arc::clone(&page)]);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0], extent);
    }

    /// PINS: that a real `Large` page reaches `forwarding` labelled as large,
    /// i.e. that the seam-1 mapping survives contact with a page the allocator
    /// actually handed out.
    #[test]
    fn a_real_large_page_reaches_forwarding_labelled_large() {
        let allocator = small_allocator();
        // Larger than the medium object limit, so `class_for` routes it Large.
        let huge = small_geometry().medium_object_limit + 1;
        let page = allocator
            .alloc_page(ZPageSizeClass::Large, huge)
            .expect("large page");
        assert_eq!(page.size_class(), ZPageSizeClass::Large);

        // The Large gate in `ZRelocationPolicy` is `is_large()`, not capacity,
        // so the denominator is irrelevant to what this test pins.
        let candidate = page_candidate(&page);
        assert_eq!(candidate.size_class_index, ZFWD_SIZE_CLASS_LARGE);
        assert!(candidate.is_large());

        let set = ZRelocationSet::select(&[candidate], &ZRelocationPolicy::default());
        assert!(
            set.is_empty(),
            "a real Large page was admitted to the relocation set"
        );
    }

    // -- Seam 3 -------------------------------------------------------------

    /// PINS: that the generation-context adapter answers per generation, and
    /// answers `None`/`false` outside the heap.
    ///
    /// Both directions matter to `remembered`'s store barrier: a wrong `is_old`
    /// floods the remembered set with old→old edges (a minor cycle that scans
    /// everything), and a wrong `is_young` loses a live object.
    #[test]
    fn the_generation_context_answers_per_generation_and_none_outside_the_heap() {
        let allocator = small_allocator();
        let heap = generational_heap(Arc::clone(&allocator), 64);

        let young = heap.allocate_young(64, 8).expect("young allocation");
        let old = heap.allocate_old(64, 8).expect("old allocation");

        let ctx = ZHeapGenerationContext::new(&heap);

        assert!(ctx.is_young(young.address as u64));
        assert!(!ctx.is_old(young.address as u64));
        assert!(ctx.is_old(old.address as u64));
        assert!(!ctx.is_young(old.address as u64));

        // page_of agrees with the allocator on id and on the page-relative
        // offset. That offset is the one the remembered-set bit is derived
        // from, so it has to be exact, not merely inside the page.
        let old_page = allocator.page_for(old.address).expect("old page resolves");
        let (id, offset) = ctx.page_of(old.address as u64).expect("old address maps");
        assert_eq!(id, old_page.id());
        assert_eq!(id, old.page_id);
        assert_eq!(offset, old.address - old_page.base());

        let young_page = allocator
            .page_for(young.address)
            .expect("young page resolves");
        let (yid, yoffset) = ctx
            .page_of(young.address as u64)
            .expect("the adapter maps young pages too — a documented superset");
        assert_eq!(yid, young_page.id());
        assert_eq!(yoffset, young.address - young_page.base());

        // The two generations must not answer for each other's pages.
        assert_ne!(
            id, yid,
            "young and old allocations landed in the same page; the \
             one-generation-per-page invariant is broken"
        );

        // Outside the reservation: not old, not young, no page. `end()` is
        // exclusive, so `end()` itself is already outside.
        let outside = allocator.end() as u64;
        assert!(!allocator.in_reserved_range(outside as usize));
        assert!(!ctx.is_old(outside));
        assert!(!ctx.is_young(outside));
        assert_eq!(ctx.page_of(outside), None);

        // Null, and an address far above any plausible reservation.
        assert!(!ctx.is_old(0));
        assert!(!ctx.is_young(0));
        assert_eq!(ctx.page_of(0), None);
        assert!(!ctx.is_old(u64::MAX));
        assert!(!ctx.is_young(u64::MAX));
        assert_eq!(ctx.page_of(u64::MAX), None);
    }

    /// PINS: that the adapter is a usable `ZGenerationContext` for
    /// `remembered`'s store barrier, and that an old→young store lands on the
    /// bit the page-relative offset names.
    ///
    /// The bit index is derived from the page base and the grain rather than
    /// hard-coded: hard-coding it is exactly the mistake `remembered` shipped
    /// with once (it was written against `HEADER_SIZE = 24`; it is 16).
    #[test]
    fn a_store_through_the_adapter_dirties_the_grain_the_offset_names() {
        let allocator = small_allocator();
        let heap = generational_heap(Arc::clone(&allocator), 64);

        let old = heap.allocate_old(64, 8).expect("old allocation");
        let young = heap.allocate_young(64, 8).expect("young allocation");
        let old_page = allocator.page_for(old.address).expect("old page resolves");

        let table = Arc::new(ZRememberedSetTable::new());
        assert_eq!(
            register_old_pages(&heap, &table),
            heap.old().page_count(),
            "register_old_pages must offer every old page"
        );
        let set = table
            .get(old_page.id())
            .expect("the old page is registered");
        assert_eq!(set.page_size(), old_page.size());

        let ctx = ZHeapGenerationContext::new(&heap);
        let barrier = ZStoreBarrier::new(Arc::clone(&table));
        let field_addr = old.address as u64 + 16;
        let outcome = barrier.on_reference_store(&ctx, field_addr, young.address as u64);
        assert_eq!(
            outcome,
            ZStoreBarrierOutcome::Precise,
            "an old-slot -> young-value store was not recorded precisely; the \
             adapter and generation.rs disagree about which addresses are old"
        );

        let page_relative = field_addr as usize - old_page.base();
        assert!(set.is_remembered(page_relative));
        assert_eq!(set.bits_set(), 1);
        assert!(
            !set.is_remembered(page_relative - Z_REMSET_GRAIN_BYTES),
            "the adjacent grain was dirtied too"
        );
    }

    // -- Seam 4 -------------------------------------------------------------

    /// A synthetic old page: no allocator, no real memory. The whole point of
    /// [`ZOldSlotReader`] is that the seam is testable without either.
    const FAKE_PAGE_ID: u64 = 7;
    const FAKE_PAGE_BASE: u64 = 0x1_0000;
    const FAKE_PAGE_SIZE: usize = 8192;

    fn fake_bases() -> FxHashMap<u64, u64> {
        let mut map: FxHashMap<u64, u64> = FxHashMap::default();
        map.insert(FAKE_PAGE_ID, FAKE_PAGE_BASE);
        map
    }

    /// Collect what a view yields, in yield order.
    fn collect(view: &dyn ZRememberedSetView) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        view.iterate_old_to_young(&mut |addr| out.push(addr));
        out
    }

    /// PINS: that a bit set at page-relative offset N is read back as the value
    /// the injected reader holds at `base + N` — the whole of seam 4.
    #[test]
    fn the_view_yields_the_injected_read_of_each_remembered_slot() {
        let table = Arc::new(ZRememberedSetTable::new());
        let set = table.register_old_page(FAKE_PAGE_ID, FAKE_PAGE_SIZE);

        let offset_a = 3 * Z_REMSET_GRAIN_BYTES;
        let offset_b = 40 * Z_REMSET_GRAIN_BYTES;
        assert!(set.remember(offset_a));
        assert!(set.remember(offset_b));

        let target_a = 0x9000_0000u64;
        let target_b = 0x9000_0040u64;
        let reader = ZMapSlotReader::new()
            .with_slot(FAKE_PAGE_BASE + offset_a as u64, target_a)
            .with_slot(FAKE_PAGE_BASE + offset_b as u64, target_b);
        assert_eq!(reader.len(), 2);
        assert!(!reader.is_empty());

        let view = ZTableRememberedSetView::new(
            Arc::clone(&table),
            fake_bases(),
            reader,
            ZRemsetBuffer::Dirty,
        );

        let mut got = collect(&view);
        got.sort_unstable();
        assert_eq!(got, vec![target_a, target_b]);
        assert_eq!(view.entry_count(), 2);
        assert_eq!(view.unresolved_pages(), 0);
        assert_eq!(view.unreadable_slots(), 0);
        assert_eq!(view.buffer(), ZRemsetBuffer::Dirty);
        assert_eq!(view.table().len(), 1);
    }

    /// PINS: that the Dirty/Snapshot choice is real — the snapshot buffer is
    /// empty until `swap_all`, and holds the pre-swap edges after it.
    ///
    /// A minor cycle that reads the snapshot without swapping sees NO
    /// remembered edges and frees live objects. That is why the buffer is a
    /// named constructor argument rather than a default.
    #[test]
    fn the_snapshot_buffer_is_empty_until_swap_all_and_carries_the_edges_after() {
        let table = Arc::new(ZRememberedSetTable::new());
        let set = table.register_old_page(FAKE_PAGE_ID, FAKE_PAGE_SIZE);
        let offset = 5 * Z_REMSET_GRAIN_BYTES;
        assert!(set.remember(offset));

        let target = 0xABCD_0000u64;
        let reader = ZMapSlotReader::new().with_slot(FAKE_PAGE_BASE + offset as u64, target);

        let snapshot_view = ZTableRememberedSetView::new(
            Arc::clone(&table),
            fake_bases(),
            reader.clone(),
            ZRemsetBuffer::Snapshot,
        );
        assert_eq!(
            collect(&snapshot_view),
            Vec::<u64>::new(),
            "the snapshot buffer must be empty before swap_all"
        );
        assert_eq!(snapshot_view.entry_count(), 0);

        table.swap_all();

        assert_eq!(collect(&snapshot_view), vec![target]);
        assert_eq!(snapshot_view.entry_count(), 1);

        // ...and the dirty buffer is now the empty one.
        let dirty_view = ZTableRememberedSetView::new(
            Arc::clone(&table),
            fake_bases(),
            reader,
            ZRemsetBuffer::Dirty,
        );
        assert_eq!(collect(&dirty_view), Vec::<u64>::new());
    }

    /// PINS: that a null slot is dropped as a stale hint rather than yielded
    /// as root address 0, and that it is not counted as a read failure.
    #[test]
    fn a_slot_that_reads_null_is_dropped_and_is_not_a_failure() {
        let table = Arc::new(ZRememberedSetTable::new());
        let set = table.register_old_page(FAKE_PAGE_ID, FAKE_PAGE_SIZE);
        let live = 2 * Z_REMSET_GRAIN_BYTES;
        let cleared = 9 * Z_REMSET_GRAIN_BYTES;
        assert!(set.remember(live));
        assert!(set.remember(cleared));

        let target = 0x7777_0000u64;
        let reader = ZMapSlotReader::new()
            .with_slot(FAKE_PAGE_BASE + live as u64, target)
            .with_slot(FAKE_PAGE_BASE + cleared as u64, 0);

        let view = ZTableRememberedSetView::new(
            Arc::clone(&table),
            fake_bases(),
            reader,
            ZRemsetBuffer::Dirty,
        );

        assert_eq!(collect(&view), vec![target]);
        assert_eq!(
            view.unreadable_slots(),
            0,
            "a null read is a stale hint, not a read failure"
        );
        assert_eq!(
            view.entry_count(),
            2,
            "entry_count counts bits, not yielded edges — it is documented as \
             approximate and must not be used as a correctness assertion"
        );
    }

    /// PINS: that both root-dropping failure modes are counted rather than
    /// silently swallowed.
    ///
    /// `generation::ZRememberedSetView` has no coarsening channel, so a view
    /// that cannot resolve a page or read a slot has no way to say so through
    /// the trait. These counters are the only signal, and a caller that does
    /// not assert them is shipping a use-after-free.
    #[test]
    fn dropped_edges_are_counted_for_both_failure_modes() {
        // (a) A registered old page whose base the caller never recorded.
        let table = Arc::new(ZRememberedSetTable::new());
        let set = table.register_old_page(FAKE_PAGE_ID, FAKE_PAGE_SIZE);
        assert!(set.remember(Z_REMSET_GRAIN_BYTES));

        let view = ZTableRememberedSetView::new(
            Arc::clone(&table),
            FxHashMap::default(),
            ZMapSlotReader::new(),
            ZRemsetBuffer::Dirty,
        );
        assert_eq!(collect(&view), Vec::<u64>::new());
        assert_eq!(view.unresolved_pages(), 1);
        assert_eq!(view.unreadable_slots(), 0);

        view.reset_counters();
        assert_eq!(view.unresolved_pages(), 0);

        // (b) A resolvable page whose slot the reader cannot read at all.
        let unreadable = ZTableRememberedSetView::new(
            Arc::clone(&table),
            fake_bases(),
            ZMapSlotReader::new(),
            ZRemsetBuffer::Dirty,
        );
        assert_eq!(collect(&unreadable), Vec::<u64>::new());
        assert_eq!(unreadable.unresolved_pages(), 0);
        assert_eq!(unreadable.unreadable_slots(), 1);
    }

    /// PINS: that `over_old_generation` resolves the bases of real pages, so
    /// the seam-4 adapter works against a real heap and not only against the
    /// synthetic fixture.
    #[test]
    fn over_old_generation_resolves_every_real_old_page_base() {
        let allocator = small_allocator();
        let heap = generational_heap(Arc::clone(&allocator), 64);
        let old = heap.allocate_old(64, 8).expect("old allocation");
        let young = heap.allocate_young(64, 8).expect("young allocation");
        let old_page = allocator.page_for(old.address).expect("old page resolves");

        let bases = old_page_bases(&heap);
        assert_eq!(bases.get(&old_page.id()), Some(&(old_page.base() as u64)));
        assert_eq!(bases.len(), heap.old().page_count());

        let table = Arc::new(ZRememberedSetTable::new());
        register_old_pages(&heap, &table);

        let ctx = ZHeapGenerationContext::new(&heap);
        let barrier = ZStoreBarrier::new(Arc::clone(&table));
        let field_addr = old.address as u64 + 16;
        assert_eq!(
            barrier.on_reference_store(&ctx, field_addr, young.address as u64),
            ZStoreBarrierOutcome::Precise
        );

        let reader = ZMapSlotReader::new().with_slot(field_addr, young.address as u64);
        let view = ZTableRememberedSetView::over_old_generation(
            &heap,
            Arc::clone(&table),
            reader,
            ZRemsetBuffer::Dirty,
        );

        assert_eq!(collect(&view), vec![young.address as u64]);
        assert_eq!(view.unresolved_pages(), 0);
        assert_eq!(view.unreadable_slots(), 0);
    }
}
