// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Reference-slot census — the one unmeasured number in the ZGC slot-representation study.
//!
//! # Why this module exists
//!
//! `docs/feature-designs/zgc-reference-slot-representation.md` is a completed
//! analysis. Every layout fact in it carries a `file:line`, and its §7 records
//! four corrections to the premise it was commissioned under. It ends with
//! exactly one thing that is still a guess, and it says so in as many words
//! (§6, open question 1: *"This is the only number in this document that is a
//! guess, and it is the one that sizes the work."*).
//!
//! That number is:
//!
//! > **What fraction of live reference slots are legacy 16-byte cells?**
//!
//! and the study's §4 attaches a decision to it:
//!
//! > *Legacy reference slots < ~5%* — the legacy arm of `ref_slot` can be
//! > deferred to a later increment … *Legacy reference slots > ~25%* — the
//! > legacy arm is on the critical path from day one.
//!
//! This module is the instrument that produces it. See
//! [the decision rule](#the-decision-rule) below for what each outcome
//! commits the implementation to, stated **before** the run rather than after.
//!
//! # The four shapes, and why a census is needed at all
//!
//! The study's §0 table (reproduced on [`ZSlotShape`], with its anchors) found
//! four reference-slot shapes, all of them already 8-byte, 8-byte-aligned
//! words:
//!
//! | shape | width | already accessed atomically? |
//! |---|---|---|
//! | compact ref field | 8 B (`types/src/heap_types.rs:185`) | **yes** (`types/src/field_layout.rs:986` / `:1047`) |
//! | legacy ref field | pointer at `cell+8` of a 16-byte cell (`types/src/value.rs:1502-1509`) | **yes** (`types/src/value.rs:1570`, `:1582`) |
//! | ref array element | 8 B always (`types/src/heap_types.rs:176`) | **no** (`types/src/narrow_oop.rs:203`, `:216`) |
//! | static ref field | pointer at `cell+8` (`vm/src/vm/realms/class_realm.rs:43-46`) | **no** |
//!
//! The split between the first two **cannot be derived from source**.
//! Compactness is decided *per object*, at allocation, from the
//! `GC_FLAG_COMPACT` header bit (`types/src/field_layout.rs:1120-1122`, set at
//! `gc/src/zgc.rs` `alloc_object`), and the layout builder structurally refuses
//! it for any class with a padded (descriptor-less) slot — its own comment
//! names the population as *"the untyped `ClassId(0)`-minted synthetic
//! containers … every HashMap/LinkedHashMap node, view backings"*
//! (`classloading/src/class.rs:1608-1626`, refusal at `:1624`). So the ratio is
//! an empirical property of a running workload. Nothing in the tree counts it.
//!
//! # Two strategies, because they answer two different questions
//!
//! This module implements both, and they are **not** interchangeable.
//!
//! ## 1. [`ZCensusStrategy::HeapWalk`] — the live-set composition
//!
//! A pass over the live objects at sweep time ([`ZSlotCensus::run_walk`]),
//! classifying every reference slot of every live object. Answers *"what is in
//! the heap"*. It runs once per collection inside a pause that already exists,
//! costs nothing on any mutator path, and **cannot perturb the workload it is
//! measuring** — which makes it the number to collect first.
//!
//! Its blind spots are named rather than hidden: it cannot see static fields
//! (see [`ZCensusHeapView::static_reference_slots`]) and it weights a slot that
//! is never read the same as one read a billion times.
//!
//! ## 2. [`ZCensusStrategy::AccessPath`] — the dynamic composition
//!
//! A counter bumped on the actual field-read paths ([`ZSlotCensus::record_access`]).
//! Answers *"what does the program actually touch"*.
//!
//! **The study's 5% / 25% thresholds are about barrier cost, and barrier cost
//! is paid per access, not per slot.** So the thresholds should be read against
//! *this* number. The two can disagree by orders of magnitude in the direction
//! that matters most: a handful of `HashMap$Node`s (legacy, by
//! `class.rs:1624`) read inside a hot loop is a live-set share near zero and a
//! dynamic share near one. That is precisely the case where deferring the
//! legacy arm on the strength of the walk alone would be the wrong call.
//!
//! The reverse asymmetry also holds and is why the walk is not redundant: a
//! large legacy population that is *allocated* and never re-read still has to
//! be traversed by the marker, and still needs a correct `ref_slot` arm
//! eventually.
//!
//! ## Which to run first
//!
//! **The walk.** It needs no hot-path edit, it is one call at an existing
//! safepoint, and it bounds the answer: a live-set legacy share of 0.5% or of
//! 60% settles the scheduling question on its own. Run the access-path arm
//! second — always if the walk lands inside the 5–25% band, and always on a
//! `HashMap`-heavy shape regardless of what the walk said, because that is the
//! shape engineered to make the two numbers disagree.
//!
//! # The decision rule
//!
//! Stated here so that whoever runs this knows what they are deciding *before*
//! the run. `legacy_share` is
//! `slots[LegacyField] / Σ slots[all shapes]` — [`ZShapeTotals::share`].
//!
//! | measurement | verdict | what it commits to |
//! |---|---|---|
//! | `legacy_share < 0.05` | [`ZCensusVerdict::LegacyNegligible`] | Build the **compact + array** arms of `ref_slot` (study §3(e)) and ship. The legacy arm degrades to `None` → option (c), non-healing, correct, and invisible in throughput. |
//! | `0.05 ≤ legacy_share ≤ 0.25` | [`ZCensusVerdict::LegacyMaterial`] | Build all three arms, compact + array first. The `tag == 4` gate (study §1.7) is a defensive concern, not yet a primary one. |
//! | `legacy_share > 0.25` | [`ZCensusVerdict::LegacyDominant`] | The legacy arm is critical-path from day one. The `tag == 4` gate becomes a **primary correctness** concern (a miss is silent type confusion, not a crash), and the case for study option (b) — a uniform 8-byte `RefSlot` — strengthens materially. |
//! | no slots observed | [`ZCensusVerdict::NoData`] | Nothing was decided. Do not read a row of zeroes as "no legacy slots". |
//!
//! Three riders, each of which has bitten this tree before:
//!
//! 1. **The share decides *scheduling*, never *whether the arm is optional*.**
//!    If the walk sees any legacy reference slot at all, a `ref_slot` that
//!    returns `None` for that shape is still *correct* (study §3(c): the
//!    non-healing fallback is a two-line degradation that cannot introduce a
//!    correctness bug) — it is only slower. A share below 5% buys a schedule,
//!    not an exemption.
//! 2. **`slots[StaticField] == 0` means "not wired", not "none exist".**
//!    Statics live in `vm/src/vm/realms/class_realm.rs`, outside the `gc`
//!    crate, so the heap walk structurally cannot reach them. Check
//!    [`ZSlotCensus::statics_wired()`] before reading that column; the report
//!    prints `NOT WIRED` rather than `0` when it is false. (Repo convention:
//!    a counter printed only when non-zero hides "never ran".)
//! 3. **If `slots[ArrayElement]` dominates**, the top-priority fix is not the
//!    legacy arm at all: it is `read_prim_element`'s plausibility degrade
//!    (`gc/src/heap.rs:1687-1714`), which turns a colored array element into
//!    `Object(None)` and *silently nulls a live reference*. The study calls
//!    that the single most dangerous unmigrated read path (§3(e), hazard 2).
//!
//! # Cost, and the gate
//!
//! Everything here is behind a runtime [`AtomicBool`] ([`ZSlotCensus::enable`]),
//! **not** `#[cfg(debug_assertions)]`. That is a deliberate match to
//! [`super::barrier::ZBarrierStats`], whose type docs give the reason in full:
//! *"a `cfg` gate makes the behaviour differ between debug and release builds,
//! and this repo has already been burned by exactly that (release test runs
//! silently skipping a debug-only check). A runtime flag behaves identically in
//! both profiles."* The same split is used here — a master gate plus a second
//! gate for the per-access arm, mirroring `ZBarrierStats::hot_counters`.
//!
//! Disabled, [`ZSlotCensus::record_access`] is one relaxed load of a
//! thread-shared `AtomicBool` and a not-taken branch, and
//! [`ZSlotCensus::run_walk`] returns `None` before touching the view.
//!
//! # No process-global state
//!
//! Every counter in this module is **instance-owned**: a [`ZSlotCensus`] is
//! constructed alongside a heap and reached through `&self`. There is no
//! `static`, no `OnceLock`, and no lazily-memoised global anywhere, including
//! for the TSV header — which is why [`ZSlotCensus::tsv_header`] returns an
//! owned `String` where [`super::metrics::ZgcMetrics::tsv_header`] returns a
//! `&'static str` from a `OnceLock`. This tree has documented parallel-test
//! crashes caused by process-global GC caches outliving the VM that filled
//! them; a diagnostic is not worth re-introducing that class of bug.
//!
//! # Correctness is never a function of these counters
//!
//! Every atomic here is `Ordering::Relaxed` and every one is a diagnostic.
//! Nothing in the collector reads one back to make a decision, so a lost
//! increment costs a wrong number in a report and nothing else — the exact
//! argument `barrier.rs` makes for its own counters. Any stronger ordering
//! would be paying for a guarantee that has no consumer. The two orderings
//! that are *not* relaxed do not exist: there are none.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use cratonvm_types::ClassId;

// ---------------------------------------------------------------------------
// Thresholds and bounds
// ---------------------------------------------------------------------------

/// Below this legacy share the legacy `ref_slot` arm may be deferred.
///
/// Verbatim from the study §4 ("*< ~5%*"). Changing it changes what the
/// instrument decides, so it is a named constant rather than an inline literal.
pub const LEGACY_DEFER_THRESHOLD: f64 = 0.05;

/// Above this legacy share the legacy `ref_slot` arm is critical-path.
///
/// Verbatim from the study §4 ("*> ~25%*").
pub const LEGACY_CRITICAL_THRESHOLD: f64 = 0.25;

/// Hard cap on distinct `ClassId`s the per-class breakdown will track in one
/// walk.
///
/// **Bounded by construction.** The per-class table is the useful half of this
/// instrument — it is what turns "37% legacy" into "37% legacy, and it is
/// `HashMap$Node`" — but a workload can mint class ids without limit
/// (`ClassId(0)`-family synthetic containers, redefinitions, per-loader
/// forks). Once the table is full, further class ids are counted in the
/// overflow scalars ([`ZWalkResult::class_table_overflow_objects`]) instead of
/// growing it. 4096 entries × 40 bytes is under 200 KB, allocated per walk and
/// dropped at its end.
pub const CLASS_TABLE_CAP: usize = 4096;

/// How many per-class rows survive into the published table after ranking.
///
/// The whole `CLASS_TABLE_CAP`-sized map is a walk-local allocation; only this
/// many rows are retained on the [`ZSlotCensus`] afterwards, so the instrument's
/// steady-state footprint does not depend on how many classes the workload has.
pub const PUBLISHED_CLASS_ROWS: usize = 64;

/// How many per-class rows the TSV row carries.
///
/// Fixed, because the TSV column count must not depend on the data — a variable
/// column count is exactly the silent corruption
/// [`ZSlotCensus::tsv_header`] exists to prevent.
pub const TSV_TOP_CLASSES: usize = 5;

// ---------------------------------------------------------------------------
// Slot shapes
// ---------------------------------------------------------------------------

/// Number of [`ZSlotShape`] variants. The width of every per-shape array.
pub const Z_SLOT_SHAPE_COUNT: usize = 4;

/// One of the four reference-slot shapes the study enumerated.
///
/// This enum *is* the study's §0 table, pinned into code so that a fifth shape
/// cannot be added to the VM without this file failing to compile against it.
/// The study's own §6 open question 6 ("*Are there reference slots I have not
/// enumerated?*") is why the classification is exhaustive and why
/// [`ZSlotShape::ALL`] is public: a census that silently drops a shape reports a
/// legacy share that is too high and a total that is too low, and neither error
/// announces itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ZSlotShape {
    /// Instance field of an object carrying `GC_FLAG_COMPACT`: a bare 8-byte
    /// pointer at `obj + layout.field_disps[i]`, `0 == null`.
    /// `REF_FIELD_SIZE = 8` (`types/src/heap_types.rs:185`).
    CompactField,
    /// Instance field of an object without `GC_FLAG_COMPACT`: the 8-byte
    /// pointer word at `cell+8` of a 16-byte tagged `Value` cell
    /// (`types/src/heap_types.rs:230`, layout pinned by the const-assertions at
    /// `types/src/value.rs:1502-1509`).
    LegacyField,
    /// Element of a reference array: a bare 8-byte word at
    /// `obj + ARRAY_DATA_OFFSET + i*8`. `REF_ELEMENT_SIZE = 8`
    /// (`types/src/heap_types.rs:176`) — unconditionally, since the compact
    /// layout never applied to arrays.
    ArrayElement,
    /// Static field: the pointer word at `cell+8` of a 16-byte `Value` cell in
    /// a `StaticsBlock` (`vm/src/vm/realms/class_realm.rs:43-46`), which is
    /// leaked heap memory outside the collector's arena entirely.
    StaticField,
}

impl ZSlotShape {
    /// Every shape, in a fixed order. A shape's position here is its index into
    /// every per-shape array and its column order in the TSV.
    pub const ALL: [ZSlotShape; Z_SLOT_SHAPE_COUNT] = [
        ZSlotShape::CompactField,
        ZSlotShape::LegacyField,
        ZSlotShape::ArrayElement,
        ZSlotShape::StaticField,
    ];

    /// Index into [`ZSlotShape::ALL`] and into the per-shape arrays.
    #[inline]
    pub fn index(self) -> usize {
        match self {
            ZSlotShape::CompactField => 0,
            ZSlotShape::LegacyField => 1,
            ZSlotShape::ArrayElement => 2,
            ZSlotShape::StaticField => 3,
        }
    }

    /// Stable snake_case metrics key, used to build TSV column names.
    ///
    /// **External contract.** Renaming one silently breaks every aggregation
    /// already collected — the same rule [`super::metrics::ZgcPhase::key`]
    /// states for its own keys.
    pub fn key(self) -> &'static str {
        match self {
            ZSlotShape::CompactField => "compact_field",
            ZSlotShape::LegacyField => "legacy_field",
            ZSlotShape::ArrayElement => "array_element",
            ZSlotShape::StaticField => "static_field",
        }
    }

    /// Human label for the text report.
    pub fn label(self) -> &'static str {
        match self {
            ZSlotShape::CompactField => "compact instance field (bare 8B word)",
            ZSlotShape::LegacyField => "legacy instance field (16B cell, word at +8)",
            ZSlotShape::ArrayElement => "reference array element (bare 8B word)",
            ZSlotShape::StaticField => "static field (16B cell in StaticsBlock)",
        }
    }

    /// Whether **every** in-tree reader and writer of this shape's reference
    /// word is an ATOMIC access today.
    ///
    /// The study's §0 table, column 3, re-derived against the code on
    /// 2026-09-01 rather than against the sentence that used to stand here.
    /// This is not decoration: study §2.3 shows that mixed atomic/non-atomic
    /// access to one location is a data race and therefore UB regardless of
    /// what x86-64 does, so a `false` here is a named item of atomicity debt
    /// that the load barrier must clear before it may CAS that shape. This
    /// table is the authoritative frame for that work, which is why the
    /// citations below are exact line numbers.
    ///
    /// # HOW THIS TABLE WENT STALE, so the next reader distrusts it correctly
    ///
    /// The previous version cited `types/src/narrow_oop.rs:203` / `:216` for
    /// the array row. Those numbers had already drifted to `:245` / `:258` when
    /// they were written down and are `:353` / `:370` today, and by then the
    /// classification itself had gone false as well. A table that is right
    /// about one row and wrong about another is worse than one that is wrong
    /// about all four, because it gets believed. **Re-open every cited line
    /// before relying on a row**; a stale line number is the cheap failure, a
    /// stale verdict is not.
    ///
    /// # The four rows
    ///
    /// * [`ZSlotShape::CompactField`] — **ATOMIC.** Mutator side:
    ///   `read_compact_field` / `write_compact_field`
    ///   (`types/src/field_layout.rs:1123`, `:1182`), whose `Reference` arm is
    ///   an `AtomicU64` load/store (an `AtomicU32` one under narrow oops) with
    ///   a caller-supplied `Ordering`. Collector side and every other slot
    ///   walker: `narrow_oop::read_ref_slot` / `write_ref_slot`
    ///   (`types/src/narrow_oop.rs:353`, `:370`), relaxed atomics in both
    ///   widths since 2026-09-01.
    ///
    /// * [`ZSlotShape::LegacyField`] — **NOT ATOMIC, and this row used to say
    ///   it was.** The old claim held for the MUTATOR side only:
    ///   `read_value_atomic` / `write_value_atomic`
    ///   (`types/src/value.rs:1607`, `:1619`) read and write the 16-byte cell
    ///   as two relaxed `AtomicU64` words, and ZGC's own accessors use them
    ///   (`ZgcRealHeap::get_field` through `cratonvm_gc_core::heap::read_value_cell_checked`,
    ///   `set_field_no_satb` through `write_value_atomic`). The COLLECTOR side
    ///   does not: the evacuation loops write the whole cell with a plain
    ///   `std::ptr::write::<Value>` at `gc/src/gc.rs:342`, `:806`, `:978` and
    ///   `gc/src/gen_heap.rs:7009`, `:17194`, and `gc/src/g1.rs` writes it
    ///   through `value_to_unaligned_ptr` (`gc/src/g1.rs:48`, called at
    ///   `:14197` and `:21980`), which is `ptr::write_unaligned` and so cannot
    ///   be made atomic at all without first proving alignment — the same
    ///   constraint `narrow_oop::read_ref_slot_unaligned` carries.
    ///
    ///   **Tracked, not blocking**, and that distinction is the useful part of
    ///   the row: those sites are the Generational and G1 evacuation loops,
    ///   which never run over a `ZgcRealHeap`, so they are not slots the ZGC
    ///   barrier can reach and CAS. The row reads `false` anyway because the
    ///   SHAPE is shared — a reader who sees `true` here concludes the 16-byte
    ///   cell is safe everywhere and carries that conclusion into `gc/`. ZGC's
    ///   own two plain readers of this shape were converted on 2026-09-01:
    ///   `ZgcRealHeap::visit_strong_refs_at` and this module's
    ///   `reference_slots` (both `gc/src/zgc.rs`).
    ///
    ///   RESIDUAL on this row: the cell's 4-byte TAG word is still read plain
    ///   by both of those walkers, while `write_value_atomic` stores it as the
    ///   cell's LOW `AtomicU64` — which that `u32` read overlaps. It is off the
    ///   arming path (the barrier CASes the payload word at
    ///   `FIELD_CELL_PAYLOAD64_OFFSET`, never the tag at offset 0), and the
    ///   repair, when someone wants it, is an `AtomicU64` low-word load with
    ///   `w0 as u32` exactly as `read_value_checked_atomic`
    ///   (`types/src/value.rs:1738`) already spells it. NOT an `AtomicU32`
    ///   load: pairing atomics of two different widths on one location is not
    ///   a repair, it is a second spelling of the same hazard.
    ///
    /// * [`ZSlotShape::ArrayElement`] — **ATOMIC since 2026-09-01**, and the
    ///   row this function most recently had wrong. Every reference array
    ///   element is read and written through `narrow_oop::read_ref_slot` /
    ///   `write_ref_slot` (`types/src/narrow_oop.rs:353`, `:370`) — including
    ///   `vm/src/jit/helpers.rs`'s `jit_aastore`, which uses them for both the
    ///   SATB old-value read and the element store — and both are now relaxed
    ///   atomics in the wide (`AtomicU64`) and narrow (`AtomicU32`) arms alike.
    ///   `narrow_oop::read_ref_slot_unaligned` is deliberately NOT converted,
    ///   and its own doc carries the rule that follows: a slot reachable
    ///   through it is a slot the barrier must never CAS.
    ///
    /// * [`ZSlotShape::StaticField`] — **NOT ATOMIC, and the reason is subtler
    ///   than "nobody got to it".** The store is a plain 16-byte `Value` struct
    ///   assignment, `fields[field_index] = value`
    ///   (`vm/src/vm/vm_object.rs:1841`). The load, `StaticsIndex::get`
    ///   (`vm/src/vm/realms/class_realm.rs:267`-`:268`), is two `read_volatile`s
    ///   — and **`read_volatile` is not an atomic access in Rust.** It is an
    ///   optimisation barrier and nothing more: it stops the compiler eliding,
    ///   merging or re-widening the access, and says nothing at all to the
    ///   memory model. It races a `compare_exchange` exactly as a plain read
    ///   does. That site's own comment is careful about it — "the read stays
    ///   unsynchronized against a concurrent `putstatic` ... it is only no
    ///   longer TORN" — but a reader who sees `volatile` and thinks `AtomicU64`
    ///   will classify this row wrong, which is why it is written out here.
    ///
    ///   This one IS on the arming path. Step 5 of the ordered sequence routes
    ///   a barrier through `vm::get_static_shared`, and that step's own text
    ///   already records the consequence: a static slot cannot be CAS-healed,
    ///   so it may need a NON-HEALING barrier kind rather than the
    ///   self-healing one.
    ///
    /// # The ordered sequence this table serves
    ///
    /// **The authority is the doc comment on
    /// `cratonvm_gc::vm_heap::VmHeap::load_ref_slot_barriered`.** Every step's
    /// status is written there and nowhere else.
    ///
    /// This function used to carry a second copy of that list. The two drifted
    /// apart inside three weeks, and on 2026-09-01 each was telling its reader
    /// the other one was stale — while both were behind the code: step 4's
    /// site B had started routing through the seam and step 6's `aastore` gate
    /// had landed. Two copies of an ordered sequence is how that recurs, so the
    /// copy is gone rather than merely corrected. Do not restore it.
    ///
    /// What belongs HERE is only what this table knows and that list does not:
    /// which step each row above was moved by, or is blocked on.
    ///
    /// * [`ZSlotShape::CompactField`] and [`ZSlotShape::ArrayElement`] read
    ///   `true` because of step 1 — `narrow_oop::read_ref_slot` /
    ///   `write_ref_slot` became relaxed atomics on 2026-09-01 — together with
    ///   the three ZGC-internal conversions folded into step 3.
    /// * [`ZSlotShape::LegacyField`] reads `true` since zt t32: the
    ///   collector-side cell writers (the copying and Generational
    ///   evacuation loops, `gc/src/gc.rs` / `gc/src/gen_heap.rs`) store a
    ///   cell as two relaxed atomic words (`write_value_atomic`), as the
    ///   mutator side always did.
    /// * [`ZSlotShape::StaticField`] reads `true` since zt t32: the
    ///   post-collection root fix-up (`update_value_ref_atomic`) and
    ///   `StaticsBlock::grow_to` read and write a cell per word too. A static
    ///   is still a root, never CAS-healed: R5 / R6 remap it inside the pause
    ///   through the pointer map (zt t32's concurrent relocation keeps it
    ///   there).
    ///
    /// Nothing in steps 1-3 changes behaviour on a default run. The barrier is
    /// unarmed for the life of every shipping process
    /// (`RELOCATION_REQUESTED == false`, `set_barrier_color` has no non-test
    /// caller, `barrier_good_mask` never leaves `Z_REMAPPED`), and a `Relaxed`
    /// atomic access to a naturally aligned word lowers to the same single
    /// instruction the plain access did on x86-64 and on aarch64 alike.
    pub fn word_is_atomically_accessed_today(self) -> bool {
        match self {
            ZSlotShape::CompactField
            | ZSlotShape::ArrayElement
            | ZSlotShape::LegacyField
            | ZSlotShape::StaticField => true,
        }
    }

    /// WHERE this shape's atomicity debt is, or `None` when the shape is clear.
    ///
    /// The machine-readable half of
    /// [`ZSlotShape::word_is_atomically_accessed_today`]: the boolean says THAT
    /// a shape is unsound to CAS, this says WHERE, so a report can print the
    /// citation instead of sending its reader back to a doc comment that may
    /// have drifted again. The two must agree, and
    /// `the_shape_table_matches_the_study` asserts that they do — which is what
    /// stops one of them being updated alone, the exact failure that made this
    /// table wrong in the first place.
    ///
    /// The text is a `&'static str` and not a structured type on purpose: it is
    /// read by humans following a citation, and a struct would invite adding
    /// fields to it instead of clearing the debt.
    pub fn atomicity_debt_note(self) -> Option<&'static str> {
        // zt t32: empty -- every shape's words are accessed per word, atomically,
        // on both sides (see `word_is_atomically_accessed_today`).
        match self {
            ZSlotShape::CompactField
            | ZSlotShape::ArrayElement
            | ZSlotShape::LegacyField
            | ZSlotShape::StaticField => None,
        }
    }

    /// Whether resolving this shape's reference word requires the `tag == 4`
    /// gate of study §1.7.
    ///
    /// True for the two 16-byte-cell shapes. Object bodies are handed out
    /// zero-filled and nothing retypes them, so an unwritten cell's tag is `0`
    /// (`Value::Int`), not `4` (`Value::Object`). Healing the payload of such a
    /// cell manufactures a `Value::Int` holding a heap address — a type
    /// confusion that `read_value_checked_atomic` passes, because the
    /// discriminant is in range. The bare-word shapes have no tag and need no
    /// gate.
    pub fn needs_tag_gate(self) -> bool {
        match self {
            ZSlotShape::LegacyField | ZSlotShape::StaticField => true,
            ZSlotShape::CompactField | ZSlotShape::ArrayElement => false,
        }
    }

    /// Whether a heap walk can reach this shape at all.
    ///
    /// `false` only for [`ZSlotShape::StaticField`]: a `StaticsBlock` is leaked
    /// memory owned by `vm/src/vm/realms/class_realm.rs`, not an object in the
    /// collector's registry. See [`ZCensusHeapView::static_reference_slots`].
    pub fn reachable_from_heap_walk(self) -> bool {
        !matches!(self, ZSlotShape::StaticField)
    }

    /// Resolve a shape from its stable [`ZSlotShape::key`].
    pub fn from_key(key: &str) -> Option<ZSlotShape> {
        ZSlotShape::ALL.iter().copied().find(|s| s.key() == key)
    }
}

/// Which of the two collection strategies produced a set of totals.
///
/// Carried on [`ZShapeTotals`] so a snapshot can never be read against the
/// wrong decision rule. The study's 5% / 25% thresholds are barrier-cost
/// thresholds and belong against [`ZCensusStrategy::AccessPath`]; see the
/// module header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZCensusStrategy {
    /// Sweep-time pass over live objects: the live-set composition.
    HeapWalk,
    /// Counters on the field-read paths: the dynamic composition.
    AccessPath,
}

impl ZCensusStrategy {
    /// Stable snake_case key, used as the TSV column prefix.
    pub fn key(self) -> &'static str {
        match self {
            ZCensusStrategy::HeapWalk => "walk",
            ZCensusStrategy::AccessPath => "access",
        }
    }

    /// Human label for the text report.
    pub fn label(self) -> &'static str {
        match self {
            ZCensusStrategy::HeapWalk => "heap walk (live-set composition)",
            ZCensusStrategy::AccessPath => "access path (dynamic composition)",
        }
    }

    /// Whether the study's 5% / 25% barrier-cost thresholds should be read
    /// against this strategy's numbers.
    ///
    /// Only [`ZCensusStrategy::AccessPath`]. A heap-walk verdict is still
    /// computed and still printed — it is the right number for "does this shape
    /// exist in this workload at all" — but it is labelled as an indicative
    /// reading, not the barrier-cost one.
    pub fn thresholds_apply(self) -> bool {
        matches!(self, ZCensusStrategy::AccessPath)
    }
}

// ---------------------------------------------------------------------------
// The heap-side seam
// ---------------------------------------------------------------------------

/// What kind of live object the walk is looking at.
///
/// This decides which [`ZSlotShape`] the object's reference slots belong to
/// *before* any slot is reported, which matters for the two objects a slot
/// stream cannot classify on its own: a compact instance with zero reference
/// fields and an empty reference array both report no slots, and would
/// otherwise be indistinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZCensusObjectKind {
    /// `ObjectKind::Object` carrying `GC_FLAG_COMPACT`. Reference fields are
    /// bare 8-byte words at layout-supplied offsets.
    CompactInstance,
    /// `ObjectKind::Object` without `GC_FLAG_COMPACT`. Every slot is a 16-byte
    /// tagged `Value` cell, and the slot's *tag* is the only thing that says
    /// whether it is a reference.
    LegacyInstance,
    /// `ObjectKind::Array` with `element_type() == Reference`.
    ReferenceArray,
    /// A primitive array or a `HumongousFiller`: structurally no reference
    /// slots. Counted, because "how much of the live set has no reference slots
    /// at all" is the denominator sanity check on everything else.
    NoReferenceSlots,
}

impl ZCensusObjectKind {
    /// The slot shape this object's reference slots have, if any.
    pub fn slot_shape(self) -> Option<ZSlotShape> {
        match self {
            ZCensusObjectKind::CompactInstance => Some(ZSlotShape::CompactField),
            ZCensusObjectKind::LegacyInstance => Some(ZSlotShape::LegacyField),
            ZCensusObjectKind::ReferenceArray => Some(ZSlotShape::ArrayElement),
            ZCensusObjectKind::NoReferenceSlots => None,
        }
    }
}

/// One observed reference slot.
///
/// The **value** travels with the address deliberately. The census must
/// distinguish a null slot from a non-null one (a null reference costs the
/// barrier's fast path nothing — it classifies null as good — so a heap full of
/// null legacy fields is not a heap full of barrier work), and reading the word
/// is the view's job, not the census's: only the heap knows the slot is inside
/// a live allocation. Nothing in this module dereferences a pointer, and that
/// is why there is no `unsafe` in this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZSlotObservation {
    /// Which of the four shapes this slot is.
    pub shape: ZSlotShape,
    /// Address of the 8-byte reference **word** — i.e. already `cell+8` for the
    /// two 16-byte-cell shapes, so that
    /// `slot_addr % 8 == 0` holds for all four (study §2.1).
    pub slot_addr: u64,
    /// The raw 8-byte word as read. `0` is null for every shape
    /// (`types/src/value.rs:1506-1509` pins this for the cell shapes).
    pub raw_word: u64,
    /// For the two 16-byte-cell shapes, the `u32` discriminant at `cell+0`;
    /// `None` for the bare-word shapes, which have no tag.
    ///
    /// `Some(4)` is `Value::Object` and is the only value that makes the slot a
    /// *reference* slot. `Some(0)` is an unwritten cell (`Value::Int(0)` from
    /// the zero-fill) — a slot that may yet become a reference. Anything else
    /// is a genuinely primitive slot. The census keeps the three apart; see
    /// [`ZShapeTotals::legacy_unwritten_slots`].
    pub legacy_tag: Option<u32>,
}

impl ZSlotObservation {
    /// A bare-word observation ([`ZSlotShape::CompactField`] /
    /// [`ZSlotShape::ArrayElement`]).
    pub fn bare(shape: ZSlotShape, slot_addr: u64, raw_word: u64) -> ZSlotObservation {
        ZSlotObservation {
            shape,
            slot_addr,
            raw_word,
            legacy_tag: None,
        }
    }

    /// A 16-byte-cell observation ([`ZSlotShape::LegacyField`] /
    /// [`ZSlotShape::StaticField`]), carrying the cell's discriminant.
    pub fn tagged(shape: ZSlotShape, slot_addr: u64, raw_word: u64, tag: u32) -> ZSlotObservation {
        ZSlotObservation {
            shape,
            slot_addr,
            raw_word,
            legacy_tag: Some(tag),
        }
    }

    /// Whether this observation is a reference slot for census purposes.
    ///
    /// Bare-word shapes always are — the layout's `is_ref` (compact) or the
    /// array's `element_type` (array) already decided it. A cell shape is one
    /// only when its tag is [`VALUE_TAG_OBJECT`]; an unwritten or primitive cell
    /// is counted separately rather than folded into the reference total, which
    /// would inflate the legacy share with slots that hold no reference.
    pub fn is_reference_slot(&self) -> bool {
        match self.legacy_tag {
            None => true,
            Some(tag) => tag == VALUE_TAG_OBJECT,
        }
    }

    /// Whether the slot currently holds a non-null reference.
    pub fn is_non_null(&self) -> bool {
        self.is_reference_slot() && self.raw_word != 0
    }
}

/// The `Value::Object` discriminant, `4`.
///
/// Pinned by a const-assertion in `types/src/value.rs:1480-1483` whose message
/// names `jit/src/x64/objects.rs` as the consumer. Duplicated here as a named
/// constant rather than an inline `4` because study §1.7 makes the *tag* the
/// load-bearing fact of the legacy arm.
pub const VALUE_TAG_OBJECT: u32 = 4;

/// The `Value::Int` discriminant, `0` — the tag of an **unwritten** cell.
///
/// Object bodies are handed out zero-filled (`gc/src/heap.rs:502`,
/// `gc/src/zgc.rs` `alloc_object`) and `StaticsBlock::new` fills with
/// `Value::Int(0)` (`vm/src/vm/realms/class_realm.rs:57-67`), so a reference
/// field that has never been written reads as this, not as `Value::Object(None)`.
pub const VALUE_TAG_INT: u32 = 0;

/// What the census needs from a heap in order to walk it.
///
/// **A trait owned by this module**, for the same reason
/// [`super::barrier::ZBarrierContext`] is owned by `barrier.rs`: the census must
/// be writable and testable without editing `zgc.rs`, and the concrete heap must
/// be able to satisfy it with a mechanical `impl` over code that already exists
/// (`ZgcRealHeap::enumerate_references` computes three of the four things below
/// already — it simply discards nulls and never looks at a tag it does not
/// need).
///
/// The two-phase contract is deliberate and is a *requirement*, not an
/// implementation note: [`ZSlotCensus::run_walk`] drains
/// [`ZCensusHeapView::for_each_live_object`] into a local vector **before**
/// calling [`ZCensusHeapView::reference_slots`] for any object. An implementation
/// may therefore hold its registry lock for the duration of
/// `for_each_live_object` — it will never be re-entered from inside that
/// callback. Doing it the other way round would put a heap lock around a
/// callback that calls back into the heap, which is the shape this tree has
/// already recorded as a lock cycle.
pub trait ZCensusHeapView {
    /// Visit every live object exactly once, in any order, passing its base
    /// address, its `ClassId`, and its [`ZCensusObjectKind`].
    ///
    /// "Live" means whatever the caller's sweep means by it. Called at most
    /// once per [`ZSlotCensus::run_walk`].
    fn for_each_live_object(&self, f: &mut dyn FnMut(u64, ClassId, ZCensusObjectKind));

    /// Whether the object at `addr` is **effectively** compact — that is,
    /// whether the VM's own read path will take its compact arm for this object.
    ///
    /// **Not the raw `GC_FLAG_COMPACT` header bit, and the difference is a real
    /// population, not a nicety.** `compact_object_field_storage`
    /// (`types/src/field_layout.rs:953-965`) refuses in *two* steps: first
    /// `is_compact_object(header)`, then
    /// `class_layout_for_fields(class_id, num_slots)?`. An object that carries
    /// the header bit but whose class has no registered layout (or whose
    /// registered field count disagrees) falls through the `?` and is read by
    /// `ZgcRealHeap::get_field` as a **16-byte tagged cell**. For a census whose
    /// entire purpose is barrier cost, what the read path does is the truth and
    /// the header bit is not; such an object must be reported as
    /// [`ZCensusObjectKind::LegacyInstance`], and this method must agree.
    ///
    /// (The study's §1.3 refusal table lists "no layout registered for the class
    /// at all" as a row, but does not connect it back to the per-object header
    /// bit of §1.3's last paragraph. They are two different gates and both fire.)
    ///
    /// Redundant with [`ZCensusObjectKind::CompactInstance`] **on purpose**:
    /// [`ZSlotCensus::run_walk`] cross-checks the two and counts every
    /// disagreement into [`ZWalkResult::kind_disagreements`]. Compactness is the
    /// single fact this whole census turns on, and an instrument that derives it
    /// twice and compares is an instrument that can be believed. A non-zero
    /// disagreement count invalidates the run.
    fn is_compact(&self, addr: u64) -> bool;

    /// Report every slot of the object at `addr` that could hold a reference.
    ///
    /// Exactly what "could" means differs by kind, and the difference *is* the
    /// study's finding about self-describing cells:
    ///
    /// * [`ZCensusObjectKind::CompactInstance`] — report the slots the class
    ///   layout's `is_ref` / `ref_offsets` oop-map names, and only those. There
    ///   is no ambiguity: an unwritten compact reference field is a zero word,
    ///   which is null. Emit [`ZSlotObservation::bare`].
    /// * [`ZCensusObjectKind::LegacyInstance`] — report **every** slot, with its
    ///   tag, via [`ZSlotObservation::tagged`]. The census, not the view,
    ///   decides which tags count. Filtering to `tag == 4` in the view would
    ///   throw away the unwritten-cell population, which is the difference
    ///   between "this class has no reference fields" and "this class's
    ///   reference fields have not been assigned yet".
    /// * [`ZCensusObjectKind::ReferenceArray`] — report every element, including
    ///   null ones, as [`ZSlotObservation::bare`].
    /// * [`ZCensusObjectKind::NoReferenceSlots`] — report nothing.
    ///
    /// `slot_addr` must be the address of the 8-byte reference **word**: for the
    /// cell shapes that is `cell + FIELD_CELL_PAYLOAD64_OFFSET`, not the cell
    /// base. The census does not dereference it; it is carried so a follow-up
    /// investigation can check alignment and so the report can name an address.
    fn reference_slots(&self, addr: u64, f: &mut dyn FnMut(ZSlotObservation));

    /// Report every static reference slot, grouped by declaring class.
    ///
    /// **Defaults to reporting nothing, and that default is a known blind
    /// spot.** A `StaticsBlock` is `Box::leak`ed memory owned by
    /// `vm/src/vm/realms/class_realm.rs:43-46`, outside the `gc` crate and
    /// outside any collector registry, so no implementation of the methods above
    /// can reach it. Until something in `vm/` overrides this, the
    /// [`ZSlotShape::StaticField`] columns are structurally zero — which is why
    /// [`ZSlotCensus::set_statics_wired`] exists and why the report prints
    /// `NOT WIRED` instead of `0`.
    ///
    /// Implementations should emit one class's statics contiguously; the walk
    /// counts a new statics block each time the `ClassId` changes, and that is
    /// the only figure an interleaved implementation would get wrong.
    fn static_reference_slots(&self, _f: &mut dyn FnMut(ClassId, ZSlotObservation)) {}
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

/// One shape's counters.
///
/// `#[repr(align(64))]` so the four shapes never share a cache line. Under the
/// access-path strategy these are bumped from mutator threads on the field-read
/// path; four hot counters packed into one line would turn the instrument into a
/// cache-line ping-pong benchmark and would change the throughput of the
/// workload it is measuring. The cost is 256 bytes for the whole array.
#[repr(align(64))]
struct ShapeCounters {
    /// Reference slots observed.
    slots: AtomicU64,
    /// Of those, the ones holding a non-null reference.
    non_null_slots: AtomicU64,
    /// Objects observed whose reference slots have this shape.
    objects: AtomicU64,
}

impl ShapeCounters {
    fn new() -> ShapeCounters {
        ShapeCounters {
            slots: AtomicU64::new(0),
            non_null_slots: AtomicU64::new(0),
            objects: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.slots.store(0, Ordering::Relaxed);
        self.non_null_slots.store(0, Ordering::Relaxed);
        self.objects.store(0, Ordering::Relaxed);
    }
}

/// Reduced per-shape totals from one strategy. Plain data — no atomics, no
/// locks — so every derived figure below is a pure function of it and is
/// testable in isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZShapeTotals {
    /// Which strategy produced these numbers. Read the study's thresholds
    /// against this; see [`ZCensusStrategy::thresholds_apply`].
    pub source: ZCensusStrategy,
    /// Reference slots per shape, indexed by [`ZSlotShape::index`].
    pub slots: [u64; Z_SLOT_SHAPE_COUNT],
    /// Non-null reference slots per shape.
    pub non_null_slots: [u64; Z_SLOT_SHAPE_COUNT],
    /// Objects per shape.
    pub objects: [u64; Z_SLOT_SHAPE_COUNT],
    /// 16-byte cells whose tag is [`VALUE_TAG_INT`] and whose payload is zero:
    /// never-written slots.
    ///
    /// **Not** counted as reference slots. They hold no reference, so they cost
    /// the barrier nothing today — but every one of them is a slot that becomes
    /// a [`ZSlotShape::LegacyField`] the moment a reference is stored into it.
    /// A workload whose legacy share is small but whose unwritten count is
    /// enormous is a workload that has not warmed up.
    pub legacy_unwritten_slots: u64,
    /// 16-byte cells whose tag is neither [`VALUE_TAG_OBJECT`] nor
    /// [`VALUE_TAG_INT`]-with-zero-payload: genuinely primitive slots. Reported
    /// so that `reference + unwritten + primitive` reconciles against the total
    /// slot count of the legacy objects walked.
    pub legacy_primitive_slots: u64,
    /// Live objects with no reference slots at all (primitive arrays, fillers,
    /// field-less instances).
    pub objects_without_ref_slots: u64,
}

impl ZShapeTotals {
    /// An all-zero set of totals for `source`.
    pub fn empty(source: ZCensusStrategy) -> ZShapeTotals {
        ZShapeTotals {
            source,
            slots: [0; Z_SLOT_SHAPE_COUNT],
            non_null_slots: [0; Z_SLOT_SHAPE_COUNT],
            objects: [0; Z_SLOT_SHAPE_COUNT],
            legacy_unwritten_slots: 0,
            legacy_primitive_slots: 0,
            objects_without_ref_slots: 0,
        }
    }

    /// Σ reference slots across every shape — the denominator of every share.
    pub fn total_slots(&self) -> u64 {
        self.slots.iter().fold(0u64, |a, b| a.saturating_add(*b))
    }

    /// Σ non-null reference slots across every shape.
    pub fn total_non_null_slots(&self) -> u64 {
        self.non_null_slots
            .iter()
            .fold(0u64, |a, b| a.saturating_add(*b))
    }

    /// Σ objects across every shape, plus those with no reference slots.
    pub fn total_objects(&self) -> u64 {
        self.objects
            .iter()
            .fold(0u64, |a, b| a.saturating_add(*b))
            .saturating_add(self.objects_without_ref_slots)
    }

    /// `slots[shape] / total_slots`, in `0.0..=1.0`.
    ///
    /// `0.0` when nothing has been observed. **Never `NaN`** — a `NaN` in a
    /// summary line is indistinguishable from a parse bug for whoever reads the
    /// log, which is the rule [`super::metrics::ZgcMetrics::stw_share`] already
    /// states for the same reason.
    pub fn share(&self, shape: ZSlotShape) -> f64 {
        share_of(self.slots[shape.index()], self.total_slots())
    }

    /// The legacy 16-byte-cell share — **the number the study is waiting for**.
    ///
    /// [`ZSlotShape::LegacyField`] only. [`ZSlotShape::StaticField`] is also a
    /// 16-byte cell and also needs the tag gate, but it is a separate world
    /// (statics are instance-field-layout-exempt by construction) and the
    /// study's §4 measurement is explicitly about *instance* fields, so folding
    /// the two would answer a different question than the one asked. Use
    /// [`ZShapeTotals::tagged_cell_share`] for the combined figure.
    pub fn legacy_share(&self) -> f64 {
        self.share(ZSlotShape::LegacyField)
    }

    /// The combined share of both tag-gated 16-byte-cell shapes.
    ///
    /// This is the fraction of reference slots for which study §1.7's
    /// `tag == 4` gate is load-bearing.
    pub fn tagged_cell_share(&self) -> f64 {
        let part = self.slots[ZSlotShape::LegacyField.index()]
            .saturating_add(self.slots[ZSlotShape::StaticField.index()]);
        share_of(part, self.total_slots())
    }

    /// The share of reference slots whose word is **not** accessed atomically
    /// today — study §2.3's atomicity debt, as a fraction of the slots the
    /// barrier would have to CAS.
    ///
    /// Composition since zt t32 (2026-10-24): empty -- every shape is atomic
    /// on both sides. From 2026-09-01 to then: [`ZSlotShape::LegacyField`] and
    /// [`ZSlotShape::StaticField`]. [`ZSlotShape::ArrayElement`] LEFT this set
    /// when `narrow_oop::write_ref_slot` became a relaxed atomic, and
    /// `LegacyField` JOINED it when the table was re-derived against the
    /// collector-side writers rather than the mutator-side pair alone — both
    /// changes are argued row by row on
    /// [`ZSlotShape::word_is_atomically_accessed_today`]. The set moved in both
    /// directions on the same day, so a `non_atomic_share` printed by an older
    /// binary is NOT comparable with one printed by a newer one, in either
    /// direction. Compare shapes, not this scalar, across that boundary.
    pub fn non_atomic_share(&self) -> f64 {
        let mut part: u64 = 0;
        for shape in ZSlotShape::ALL {
            if !shape.word_is_atomically_accessed_today() {
                part = part.saturating_add(self.slots[shape.index()]);
            }
        }
        share_of(part, self.total_slots())
    }

    /// Apply the module header's decision rule to [`ZShapeTotals::legacy_share`].
    pub fn verdict(&self) -> ZCensusVerdict {
        if self.total_slots() == 0 {
            return ZCensusVerdict::NoData;
        }
        let share = self.legacy_share();
        if share < LEGACY_DEFER_THRESHOLD {
            ZCensusVerdict::LegacyNegligible
        } else if share > LEGACY_CRITICAL_THRESHOLD {
            ZCensusVerdict::LegacyDominant
        } else {
            ZCensusVerdict::LegacyMaterial
        }
    }
}

/// The decision the census reaches. See the module header's decision-rule
/// table for the full text of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZCensusVerdict {
    /// Nothing observed. **Not** a finding of "no legacy slots".
    NoData,
    /// `< 5%`. The legacy arm of `ref_slot` may be deferred.
    LegacyNegligible,
    /// `5%..=25%`. Build all three arms; compact and array first.
    LegacyMaterial,
    /// `> 25%`. The legacy arm is critical-path from day one.
    LegacyDominant,
}

impl ZCensusVerdict {
    /// Stable key for the TSV column.
    pub fn key(self) -> &'static str {
        match self {
            ZCensusVerdict::NoData => "no_data",
            ZCensusVerdict::LegacyNegligible => "legacy_negligible",
            ZCensusVerdict::LegacyMaterial => "legacy_material",
            ZCensusVerdict::LegacyDominant => "legacy_dominant",
        }
    }

    /// One sentence saying what this verdict commits the implementation to.
    pub fn implication(self) -> &'static str {
        match self {
            ZCensusVerdict::NoData => {
                "NO SLOTS OBSERVED — nothing was decided. A row of zeroes is not a \
                 finding of 'no legacy slots'; check the gate was enabled and the walk ran."
            }
            ZCensusVerdict::LegacyNegligible => {
                "DEFER the legacy arm of ref_slot: build the compact + array arms \
                 (study §3(e)) and ship. The legacy arm degrades to None -> option (c), \
                 non-healing but correct. This buys a SCHEDULE, not an exemption."
            }
            ZCensusVerdict::LegacyMaterial => {
                "BUILD all three arms, compact + array first. The tag==4 gate (study §1.7) \
                 is a defensive concern at this share, not yet a primary one."
            }
            ZCensusVerdict::LegacyDominant => {
                "The legacy arm is CRITICAL-PATH from day one. The tag==4 gate (study §1.7) \
                 becomes a primary correctness concern — a miss is silent type confusion, \
                 not a crash — and the case for study option (b), a uniform 8-byte RefSlot, \
                 strengthens materially."
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-class breakdown
// ---------------------------------------------------------------------------

/// One class's contribution to the live-set census.
///
/// This is the row that turns a percentage into a name. The study predicts
/// which names will be here — `class.rs:1608-1613` says
/// *"every HashMap/LinkedHashMap node, view backings"* plus the `ClassId(0)`
/// synthetic containers, and `gc/src/zgc.rs`'s autobox arm adds
/// `AUTOBOX_CLASS_ID` — so this table is also the check on whether the study's
/// *reasoning* was right, not just its arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZClassRow {
    /// The declaring class.
    pub class_id: ClassId,
    /// Reference slots contributed, per shape.
    pub slots: [u64; Z_SLOT_SHAPE_COUNT],
    /// Live objects of this class the walk visited.
    pub objects: u64,
}

impl ZClassRow {
    /// A zeroed row for `class_id`.
    pub fn new(class_id: ClassId) -> ZClassRow {
        ZClassRow {
            class_id,
            slots: [0; Z_SLOT_SHAPE_COUNT],
            objects: 0,
        }
    }

    /// Σ reference slots across every shape.
    pub fn total_slots(&self) -> u64 {
        self.slots.iter().fold(0u64, |a, b| a.saturating_add(*b))
    }

    /// Reference slots of the legacy 16-byte-cell shape — the ranking key.
    pub fn legacy_slots(&self) -> u64 {
        self.slots[ZSlotShape::LegacyField.index()]
    }
}

/// One completed [`ZSlotCensus::run_walk`]: the live-set gauge.
///
/// Returned **by value** rather than read back off the counters, because the
/// live set is a *gauge* and the counters are *cumulative*. Summing walks would
/// count a long-lived object once per collection and report a legacy share
/// weighted by object lifetime, which is a third measurement neither the study
/// nor this module asked for. Use this for "what is in the heap right now" and
/// [`ZSlotCensus::walk_totals`] for the run-wide accumulation.
#[derive(Debug, Clone, PartialEq)]
pub struct ZWalkResult {
    /// Per-shape totals for this walk alone.
    pub totals: ZShapeTotals,
    /// The top classes of this walk, ranked by legacy slots then total slots
    /// then `ClassId`, truncated to [`PUBLISHED_CLASS_ROWS`].
    pub classes: Vec<ZClassRow>,
    /// Distinct `ClassId`s tracked in the per-class table (`<= CLASS_TABLE_CAP`).
    pub distinct_class_ids: u64,
    /// Object visits whose `ClassId` could not be tracked because the table was
    /// full. Counts *visits*, not distinct ids — tracking distinct overflowed
    /// ids would need the unbounded set this cap exists to avoid.
    pub class_table_overflow_objects: u64,
    /// Reference slots attributed to no per-class row for the same reason. The
    /// per-shape totals above still include them; only the per-class breakdown
    /// is lossy.
    pub class_table_overflow_slots: u64,
    /// Objects where [`ZCensusHeapView::is_compact`] disagreed with the
    /// [`ZCensusObjectKind`] the view reported.
    ///
    /// **Must be zero.** Any other value means the two derivations of the single
    /// fact this census turns on do not agree, and the run is void — see
    /// [`ZCensusHeapView::is_compact`].
    pub kind_disagreements: u64,
    /// Statics blocks observed (a `ClassId` change in the statics stream). `0`
    /// while [`ZCensusHeapView::static_reference_slots`] is the default no-op.
    pub statics_blocks: u64,
}

// ---------------------------------------------------------------------------
// The census
// ---------------------------------------------------------------------------

/// Reference-slot census for one heap.
///
/// Constructed alongside a heap and shared by `&`. Every method takes `&self`;
/// the type is `Send + Sync`. There is no global instance and no global cache —
/// see the module header.
///
/// The collector's side of the contract is three calls:
///
/// ```ignore
/// // once, when the diagnostic is asked for (default: off, costs nothing):
/// census.enable();
/// census.set_run_label("zgc-real/BeanRegistrationsAotContributionTests");
///
/// // once per collection, at sweep time, inside the existing STW token:
/// if let Some(walk) = census.run_walk(self) {
///     tracing::debug!(target: "zgc", legacy_share = walk.totals.legacy_share(), "census");
/// }
///
/// // and, only when the dynamic composition is wanted, on the read paths:
/// census.enable_access_sampling();
/// // ... inside get_field's compact arm:
/// census.record_access(ZSlotShape::CompactField, raw != 0);
/// ```
pub struct ZSlotCensus {
    /// Master gate. Off by default; gates the walk and every counter.
    enabled: AtomicBool,
    /// Second gate for the per-access arm only.
    ///
    /// Split from `enabled` for the same reason
    /// [`super::barrier::ZBarrierStats::hot_counters`] is split from the rest of
    /// that struct: the walk runs ~10 times a second at worst, while
    /// [`ZSlotCensus::record_access`] runs on every reference read in the
    /// program. Being able to take the live-set number without paying the
    /// dynamic one is the difference between a diagnostic you can leave on for a
    /// 1975-class suite run and one you cannot.
    access_sampling: AtomicBool,

    /// Cumulative live-set counters, folded from every [`ZSlotCensus::run_walk`].
    walk: [ShapeCounters; Z_SLOT_SHAPE_COUNT],
    /// Cumulative dynamic counters, bumped by [`ZSlotCensus::record_access`].
    access: [ShapeCounters; Z_SLOT_SHAPE_COUNT],

    /// Cumulative legacy tag breakdown from the walk. See
    /// [`ZShapeTotals::legacy_unwritten_slots`].
    walk_legacy_unwritten: AtomicU64,
    walk_legacy_primitive: AtomicU64,
    /// Cumulative count of live objects with no reference slots.
    walk_objects_without_ref_slots: AtomicU64,

    /// Completed walks.
    walks_completed: AtomicU64,
    /// Cumulative statics blocks observed across every walk. `0` while
    /// [`ZCensusHeapView::static_reference_slots`] is the default no-op.
    walk_statics_blocks: AtomicU64,
    /// Cumulative per-class table overflow. See [`ZWalkResult`].
    class_table_overflow_objects: AtomicU64,
    class_table_overflow_slots: AtomicU64,
    /// Cumulative `is_compact` / [`ZCensusObjectKind`] disagreements. Must stay
    /// zero.
    kind_disagreements: AtomicU64,

    /// Whether anything actually implements
    /// [`ZCensusHeapView::static_reference_slots`] in this run.
    ///
    /// Distinguishes "no static reference slots" from "nobody looked", which is
    /// exactly the confusion the [`ZSlotShape::StaticField`] column would
    /// otherwise create. See the module header, rider 2.
    statics_wired: AtomicBool,

    /// Top classes of the **most recent** walk, ranked and truncated to
    /// [`PUBLISHED_CLASS_ROWS`].
    ///
    /// A gauge, like [`ZWalkResult`], and behind a `Mutex` because it is a
    /// multi-field consistent update. The lock is taken **once per walk, after
    /// the walk has finished**, never during it and never from an access path —
    /// the same discipline `barrier.rs` applies to `mark_live` and for the same
    /// reason.
    top_classes: Mutex<Vec<ZClassRow>>,

    /// The **most recent walk's** totals, verbatim — the live-set gauge.
    ///
    /// # Why this had to exist (2026-09-20)
    ///
    /// [`ZWalkResult`]'s own doc states the rule: *"the live set is a gauge and
    /// the counters are cumulative. Summing walks would count a long-lived
    /// object once per collection and report a legacy share weighted by object
    /// lifetime, which is a third measurement neither the study nor this module
    /// asked for."*
    ///
    /// And then [`ZSlotCensus::to_tsv_row`] — the machine-readable output whose
    /// stated purpose is *"aggregation across a suite … how a number from 1975
    /// classes becomes one answer"*, i.e. the artefact from which the study's
    /// decision will actually be taken — emitted exactly that lifetime-weighted
    /// sum, under the column names `walk_legacy_field_share` and
    /// `walk_verdict`. A reader had no way to tell, and the bias is not random:
    /// a long-lived legacy population (`HashMap$Node`, the very shape the study
    /// predicts) is re-counted on **every** collection, while the short-lived
    /// compact objects that dominate an allocation-churn workload are counted
    /// once each and then die. So the cumulative number reads **high on legacy**
    /// — which is the direction that would push the verdict from
    /// `LegacyNegligible` toward `LegacyDominant` and re-schedule real work off
    /// an artefact of the walk count.
    ///
    /// The cumulative columns are kept (they are a real measurement: the
    /// lifetime-weighted composition, which is the right denominator for
    /// "how much legacy does the *marker* traverse over a run"), the rule that
    /// they are never reordered or removed is kept, and the gauge is **appended**
    /// beside them under `last_walk_*` names. [`ZSlotCensus::format_summary`]
    /// prints both and says which is which.
    ///
    /// `None` until the first walk completes — and that is reported as
    /// `no_data` rather than a row of zeroes, per the module header's rider 1.
    last_walk: Mutex<Option<ZShapeTotals>>,

    /// Label for the TSV `run` column, so a census row from one suite class can
    /// be told apart from another after concatenation.
    run_label: Mutex<String>,
}

impl Default for ZSlotCensus {
    fn default() -> Self {
        ZSlotCensus::new()
    }
}

impl ZSlotCensus {
    /// A fresh, **disabled** census. Costs nothing until [`ZSlotCensus::enable`].
    pub fn new() -> ZSlotCensus {
        ZSlotCensus {
            enabled: AtomicBool::new(false),
            access_sampling: AtomicBool::new(false),
            walk: std::array::from_fn(|_| ShapeCounters::new()),
            access: std::array::from_fn(|_| ShapeCounters::new()),
            walk_legacy_unwritten: AtomicU64::new(0),
            walk_legacy_primitive: AtomicU64::new(0),
            walk_objects_without_ref_slots: AtomicU64::new(0),
            walks_completed: AtomicU64::new(0),
            walk_statics_blocks: AtomicU64::new(0),
            class_table_overflow_objects: AtomicU64::new(0),
            class_table_overflow_slots: AtomicU64::new(0),
            kind_disagreements: AtomicU64::new(0),
            statics_wired: AtomicBool::new(false),
            top_classes: Mutex::new(Vec::new()),
            last_walk: Mutex::new(None),
            run_label: Mutex::new(String::from("zgc-census")),
        }
    }

    // -- gates --------------------------------------------------------------

    /// Turn the census on. Enables the walk; the per-access arm additionally
    /// needs [`ZSlotCensus::enable_access_sampling`].
    pub fn enable(&self) {
        // Relaxed: this only decides whether a diagnostic runs. A thread that
        // observes a stale `false` for a few nanoseconds loses a handful of
        // counts and nothing else — the identical argument
        // `ZBarrierStats::hot_counters_enabled` makes. Nothing downstream reads
        // a counter back to make a decision, so there is no release/acquire
        // relationship to establish.
        self.enabled.store(true, Ordering::Relaxed);
    }

    /// Turn the census off. Counters keep their values; nothing further is
    /// recorded.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
    }

    /// Enable from the environment: `CRATONVM_ZGC_CENSUS` turns the walk on,
    /// `CRATONVM_ZGC_CENSUS_ACCESS` additionally turns on the per-access arm.
    /// Returns whether anything was enabled.
    ///
    /// # Why this exists (2026-09-20)
    ///
    /// Because nothing enabled the census. [`Self::enable`] had **no caller
    /// outside this module's own tests**: `ZgcRealHeap` constructs a
    /// `ZSlotCensus`, gates its sweep-time walk on `is_enabled()`, and provides
    /// `slot_census()` to reach it — and no shipping code path ever turns the
    /// flag on, so `is_enabled()` is `false` for the life of every process,
    /// `run_walk` returns `None` before touching the heap, and
    /// [`Self::format_summary`] is never called at all. A 2 900-line instrument
    /// built to produce *"the only number in this document that is a guess, and
    /// … the one that sizes the work"* could not be run.
    ///
    /// That is exactly the failure mode the module header warns about from the
    /// other side — *"a row of zeroes is not a finding of 'no legacy slots';
    /// check the gate was enabled and the walk ran"* — except that no operator
    /// could have checked, because there was nothing to check.
    ///
    /// An env flag rather than a `VmConfig` field to match every other ZGC
    /// diagnostic switch (`CRATONVM_ZGC_RELOCATE`, `CRATONVM_ZGC_PARMARK`,
    /// `CRATONVM_ZGC_GENERATIONAL`), and **no `OnceLock` cache**, unlike those:
    /// caching would be process-global state in the one file whose header
    /// forbids it. The cost of not caching is one environment read per VM, at a
    /// call site that runs once.
    ///
    /// The two knobs are separate for the reason [`Self::enable_access_sampling`]
    /// gives: the walk is once per collection and the access arm is once per
    /// reference read in the program.
    pub fn enable_from_env(&self) -> bool {
        fn on(name: &str) -> bool {
            match cratonvm_types::flags::runtime_var(name) {
                Ok(raw) => {
                    let v = raw.trim().to_ascii_lowercase();
                    // Same truthiness table as `zgc_generational_enabled` and
                    // every other `CRATONVM_ZGC_*` boolean, so an operator who
                    // knows one knows all of them.
                    !matches!(v.as_str(), "" | "0" | "off" | "false" | "no")
                }
                Err(_) => false,
            }
        }
        let walk = on("CRATONVM_ZGC_CENSUS");
        let access = on("CRATONVM_ZGC_CENSUS_ACCESS");
        // The access arm implies the master gate: asking for the dynamic
        // composition and silently getting nothing because a second variable
        // was unset is the same "enabled and inert" trap the gate exists to
        // make visible.
        if walk || access {
            self.enable();
        }
        if access {
            self.enable_access_sampling();
        }
        walk || access
    }

    /// Whether the census is on.
    #[inline(always)]
    pub fn is_enabled(&self) -> bool {
        // Relaxed: see `enable`.
        self.enabled.load(Ordering::Relaxed)
    }

    /// Turn on the per-access arm. Requires [`ZSlotCensus::enable`] as well.
    ///
    /// This is the expensive one: one relaxed `fetch_add` per reference read.
    /// The per-shape counters are cache-line separated ([`ShapeCounters`]) so
    /// the four shapes do not contend with each other, but threads reading the
    /// *same* shape still share a line. Expect the absolute throughput of a
    /// run with this on to be wrong; expect the **ratios** to be right, which is
    /// all the decision rule consumes.
    pub fn enable_access_sampling(&self) {
        self.access_sampling.store(true, Ordering::Relaxed);
    }

    /// Turn the per-access arm back off.
    pub fn disable_access_sampling(&self) {
        self.access_sampling.store(false, Ordering::Relaxed);
    }

    /// Whether the per-access arm is currently recording.
    #[inline(always)]
    pub fn access_sampling_enabled(&self) -> bool {
        // Two relaxed loads and a `&&`; the compiler keeps both in registers on
        // the hot path and the branch is not taken in the default build.
        self.enabled.load(Ordering::Relaxed) && self.access_sampling.load(Ordering::Relaxed)
    }

    /// Declare that something implements
    /// [`ZCensusHeapView::static_reference_slots`] in this run.
    ///
    /// Until this is `true`, [`ZSlotShape::StaticField`] reads `NOT WIRED` in
    /// the report rather than `0`.
    pub fn set_statics_wired(&self, wired: bool) {
        self.statics_wired.store(wired, Ordering::Relaxed);
    }

    /// Whether the static-field arm is wired. See
    /// [`ZSlotCensus::set_statics_wired`].
    pub fn statics_wired(&self) -> bool {
        self.statics_wired.load(Ordering::Relaxed)
    }

    /// Set the `run` column of [`ZSlotCensus::to_tsv_row`].
    ///
    /// Tabs and newlines are replaced with `_`; either would split or terminate
    /// the row. Same rule as [`super::metrics::ZgcMetrics::set_run_label`].
    pub fn set_run_label(&self, label: &str) {
        let mut slot = self.run_label.lock();
        slot.clear();
        for c in label.chars() {
            if c == '\t' || c == '\n' || c == '\r' {
                slot.push('_');
            } else {
                slot.push(c);
            }
        }
    }

    /// The current TSV run label.
    pub fn run_label(&self) -> String {
        self.run_label.lock().clone()
    }

    // -- strategy 2: access-path sampling -----------------------------------

    /// Record one reference-slot **access** of `shape`.
    ///
    /// Call from the field-read paths — `get_field`'s compact and legacy arms,
    /// `get_array_element`'s reference arm, and the statics read path. This is
    /// the measurement the study's 5% / 25% thresholds are really about, because
    /// barrier cost is paid per access.
    ///
    /// `non_null` should be `raw_word != 0`. A null slot is a fast-path hit for
    /// the barrier (null is a good color by construction), so separating the two
    /// is what lets the reader tell "a lot of legacy reads" from "a lot of legacy
    /// reads that would each have cost a barrier".
    ///
    /// No lock, no hash, no per-class attribution — deliberately. The per-class
    /// breakdown exists only on the walk. Putting a hash lookup on every
    /// reference load in the program is the one thing `barrier.rs`'s module
    /// header forbids outright, and study §3(d) rejects an entire design option
    /// on that ground.
    #[inline(always)]
    pub fn record_access(&self, shape: ZSlotShape, non_null: bool) {
        if !self.access_sampling_enabled() {
            return;
        }
        let counters = &self.access[shape.index()];
        counters.slots.fetch_add(1, Ordering::Relaxed);
        if non_null {
            counters.non_null_slots.fetch_add(1, Ordering::Relaxed);
        }
    }

    // (`record_access_batch(&self, shape, slots, non_null)` was DELETED in
    // round 10. It was the batching twin of `record_access` above, written
    // "for a caller that has already batched -- a bulk array copy, or a
    // replayed log", and no such caller was ever written: its only references
    // in the tree were this module's own tests, which used it as a convenient
    // way to seed the census. Those now call `record_access_n`, a test helper
    // that loops the real recorder, so the setup exercises the code that
    // ships. See
    // docs/feature-designs/zgc-architecture.md.)

    // -- strategy 1: the heap walk ------------------------------------------

    /// Walk the live set and classify every reference slot.
    ///
    /// Returns `None` — touching neither `view` nor any counter — when the
    /// census is disabled.
    ///
    /// Call at sweep time, inside the stop-the-world token the collector already
    /// holds, after the mark phase has decided what is live. Cost is one pass
    /// over the live set plus one transient `Vec` of
    /// `(u64, ClassId, ZCensusObjectKind)` — 24 bytes per live object, so ~24 MB
    /// at a million live objects. That allocation is the price of never holding
    /// a heap lock across a callback that calls back into the heap; see
    /// [`ZCensusHeapView`]'s two-phase contract.
    ///
    /// The returned [`ZWalkResult`] is the live-set **gauge** for this walk. The
    /// same numbers are also folded into this census's cumulative counters,
    /// which is what [`ZSlotCensus::to_tsv_row`] reports.
    pub fn run_walk(&self, view: &dyn ZCensusHeapView) -> Option<ZWalkResult> {
        if !self.is_enabled() {
            return None;
        }

        // Phase 1: drain the object list. The view may hold its registry lock
        // for exactly this long and no longer.
        let mut objects: Vec<(u64, ClassId, ZCensusObjectKind)> = Vec::new();
        view.for_each_live_object(
            &mut |addr: u64, class_id: ClassId, kind: ZCensusObjectKind| {
                objects.push((addr, class_id, kind));
            },
        );

        let mut totals = ZShapeTotals::empty(ZCensusStrategy::HeapWalk);
        let mut classes: FxHashMap<u32, ZClassRow> = FxHashMap::default();
        let mut overflow_objects: u64 = 0;
        let mut overflow_slots: u64 = 0;
        let mut disagreements: u64 = 0;

        // Phase 2: classify. No lock is held here, in this module or (by the
        // contract above) in the view.
        for &(addr, class_id, kind) in objects.iter() {
            // The cross-check that makes the instrument believable: compactness
            // is the single fact the whole census turns on, so derive it twice
            // and compare. See `ZCensusHeapView::is_compact`.
            if view.is_compact(addr) != (kind == ZCensusObjectKind::CompactInstance) {
                disagreements = disagreements.saturating_add(1);
            }

            match kind.slot_shape() {
                Some(shape) => {
                    totals.objects[shape.index()] = totals.objects[shape.index()].saturating_add(1);
                }
                None => {
                    totals.objects_without_ref_slots =
                        totals.objects_without_ref_slots.saturating_add(1);
                }
            }

            let row: Option<&mut ZClassRow> =
                class_row(&mut classes, class_id, &mut overflow_objects);
            let mut local_slots: [u64; Z_SLOT_SHAPE_COUNT] = [0; Z_SLOT_SHAPE_COUNT];
            let mut local_non_null: [u64; Z_SLOT_SHAPE_COUNT] = [0; Z_SLOT_SHAPE_COUNT];
            let mut local_unwritten: u64 = 0;
            let mut local_primitive: u64 = 0;

            view.reference_slots(addr, &mut |obs: ZSlotObservation| {
                classify_into(
                    obs,
                    &mut local_slots,
                    &mut local_non_null,
                    &mut local_unwritten,
                    &mut local_primitive,
                );
            });

            for shape in ZSlotShape::ALL {
                let i = shape.index();
                totals.slots[i] = totals.slots[i].saturating_add(local_slots[i]);
                totals.non_null_slots[i] =
                    totals.non_null_slots[i].saturating_add(local_non_null[i]);
            }
            totals.legacy_unwritten_slots = totals
                .legacy_unwritten_slots
                .saturating_add(local_unwritten);
            totals.legacy_primitive_slots = totals
                .legacy_primitive_slots
                .saturating_add(local_primitive);

            match row {
                Some(row) => {
                    row.objects = row.objects.saturating_add(1);
                    for shape in ZSlotShape::ALL {
                        let i = shape.index();
                        row.slots[i] = row.slots[i].saturating_add(local_slots[i]);
                    }
                }
                None => {
                    for count in local_slots.iter() {
                        overflow_slots = overflow_slots.saturating_add(*count);
                    }
                }
            }
        }

        // Statics, if anything overrode the default no-op. Counted as a new
        // block each time the ClassId changes; see the trait's contract.
        let mut statics_blocks: u64 = 0;
        let mut previous_static_class: Option<ClassId> = None;
        {
            // These four locals shadow the per-object ones deliberately: a
            // static slot belongs to no object, so it must not be attributed to
            // the object loop's `objects[..]` counters.
            let mut static_slots: [u64; Z_SLOT_SHAPE_COUNT] = [0; Z_SLOT_SHAPE_COUNT];
            let mut static_non_null: [u64; Z_SLOT_SHAPE_COUNT] = [0; Z_SLOT_SHAPE_COUNT];
            let mut static_unwritten: u64 = 0;
            let mut static_primitive: u64 = 0;
            let mut static_rows: Vec<(ClassId, [u64; Z_SLOT_SHAPE_COUNT])> = Vec::new();

            view.static_reference_slots(&mut |class_id: ClassId, obs: ZSlotObservation| {
                if previous_static_class != Some(class_id) {
                    previous_static_class = Some(class_id);
                    statics_blocks = statics_blocks.saturating_add(1);
                    static_rows.push((class_id, [0; Z_SLOT_SHAPE_COUNT]));
                }
                let before = static_slots;
                classify_into(
                    obs,
                    &mut static_slots,
                    &mut static_non_null,
                    &mut static_unwritten,
                    &mut static_primitive,
                );
                if let Some(last) = static_rows.last_mut() {
                    for shape in ZSlotShape::ALL {
                        let i = shape.index();
                        last.1[i] = last.1[i].saturating_add(static_slots[i] - before[i]);
                    }
                }
            });

            for shape in ZSlotShape::ALL {
                let i = shape.index();
                totals.slots[i] = totals.slots[i].saturating_add(static_slots[i]);
                totals.non_null_slots[i] =
                    totals.non_null_slots[i].saturating_add(static_non_null[i]);
            }
            totals.legacy_unwritten_slots = totals
                .legacy_unwritten_slots
                .saturating_add(static_unwritten);
            totals.legacy_primitive_slots = totals
                .legacy_primitive_slots
                .saturating_add(static_primitive);
            let static_idx: usize = ZSlotShape::StaticField.index();
            totals.objects[static_idx] = totals.objects[static_idx].saturating_add(statics_blocks);

            for (class_id, slots) in static_rows.into_iter() {
                match class_row(&mut classes, class_id, &mut overflow_objects) {
                    Some(row) => {
                        for shape in ZSlotShape::ALL {
                            let i = shape.index();
                            row.slots[i] = row.slots[i].saturating_add(slots[i]);
                        }
                    }
                    None => {
                        for count in slots.iter() {
                            overflow_slots = overflow_slots.saturating_add(*count);
                        }
                    }
                }
            }
        }

        // Rank and truncate. Deterministic on every key so two runs of the same
        // workload produce byte-identical rows: legacy slots desc, then total
        // slots desc, then ClassId asc.
        let distinct_class_ids = classes.len() as u64;
        let mut ranked: Vec<ZClassRow> = classes.into_values().collect();
        ranked.sort_by(|a, b| {
            b.legacy_slots()
                .cmp(&a.legacy_slots())
                .then_with(|| b.total_slots().cmp(&a.total_slots()))
                .then_with(|| a.class_id.as_u32().cmp(&b.class_id.as_u32()))
        });
        ranked.truncate(PUBLISHED_CLASS_ROWS);

        // Fold into the cumulative counters, then publish the class table.
        for shape in ZSlotShape::ALL {
            let i = shape.index();
            self.walk[i]
                .slots
                .fetch_add(totals.slots[i], Ordering::Relaxed);
            self.walk[i]
                .non_null_slots
                .fetch_add(totals.non_null_slots[i], Ordering::Relaxed);
            self.walk[i]
                .objects
                .fetch_add(totals.objects[i], Ordering::Relaxed);
        }
        self.walk_legacy_unwritten
            .fetch_add(totals.legacy_unwritten_slots, Ordering::Relaxed);
        self.walk_legacy_primitive
            .fetch_add(totals.legacy_primitive_slots, Ordering::Relaxed);
        self.walk_objects_without_ref_slots
            .fetch_add(totals.objects_without_ref_slots, Ordering::Relaxed);
        self.class_table_overflow_objects
            .fetch_add(overflow_objects, Ordering::Relaxed);
        self.class_table_overflow_slots
            .fetch_add(overflow_slots, Ordering::Relaxed);
        self.kind_disagreements
            .fetch_add(disagreements, Ordering::Relaxed);
        self.walk_statics_blocks
            .fetch_add(statics_blocks, Ordering::Relaxed);
        // A view that produced static slots IS wired, whatever anybody
        // remembered to declare. `set_statics_wired` is an out-of-band promise
        // and the walk is the evidence; leaving the flag down while the columns
        // carry real numbers would print `NOT WIRED` over a measurement, which
        // is the same "silence is not zero" failure the flag exists to prevent,
        // pointed the other way. The flag is never lowered here: statics being
        // unreachable on one walk (an empty statics stream) is not evidence
        // that nobody is looking.
        if statics_blocks > 0 {
            self.statics_wired.store(true, Ordering::Relaxed);
        }
        *self.top_classes.lock() = ranked.clone();
        // The gauge, beside the cumulative counters rather than instead of
        // them — see the field docs on `last_walk` for why the TSV needed both.
        *self.last_walk.lock() = Some(totals);
        // Written LAST: a reader that sees `walks_completed == N` has seen at
        // least walk N's fields. Same publication rule as
        // `ZgcMetrics::record_cycle`'s `cycles`.
        self.walks_completed.fetch_add(1, Ordering::Release);

        if disagreements > 0 {
            tracing::warn!(
                target: "zgc",
                disagreements,
                objects = objects.len(),
                "zgc census: is_compact disagreed with the reported object kind — this walk is VOID"
            );
        }
        tracing::debug!(
            target: "zgc",
            objects = objects.len(),
            ref_slots = totals.total_slots(),
            legacy_share = totals.legacy_share(),
            verdict = totals.verdict().key(),
            "zgc census: heap walk complete"
        );

        Some(ZWalkResult {
            totals,
            classes: ranked,
            distinct_class_ids,
            class_table_overflow_objects: overflow_objects,
            class_table_overflow_slots: overflow_slots,
            kind_disagreements: disagreements,
            statics_blocks,
        })
    }

    // -- snapshots ----------------------------------------------------------

    /// Cumulative live-set totals across every walk of this run.
    ///
    /// See [`ZWalkResult`] for why the per-walk gauge is the number to quote and
    /// this one is the aggregate.
    pub fn walk_totals(&self) -> ZShapeTotals {
        let mut totals = ZShapeTotals::empty(ZCensusStrategy::HeapWalk);
        for shape in ZSlotShape::ALL {
            let i = shape.index();
            totals.slots[i] = self.walk[i].slots.load(Ordering::Relaxed);
            totals.non_null_slots[i] = self.walk[i].non_null_slots.load(Ordering::Relaxed);
            totals.objects[i] = self.walk[i].objects.load(Ordering::Relaxed);
        }
        totals.legacy_unwritten_slots = self.walk_legacy_unwritten.load(Ordering::Relaxed);
        totals.legacy_primitive_slots = self.walk_legacy_primitive.load(Ordering::Relaxed);
        totals.objects_without_ref_slots =
            self.walk_objects_without_ref_slots.load(Ordering::Relaxed);
        totals
    }

    /// Cumulative dynamic totals from [`ZSlotCensus::record_access`].
    ///
    /// `objects` is structurally zero here: an access is a slot event and knows
    /// nothing about how many objects produced it.
    pub fn access_totals(&self) -> ZShapeTotals {
        let mut totals = ZShapeTotals::empty(ZCensusStrategy::AccessPath);
        for shape in ZSlotShape::ALL {
            let i = shape.index();
            totals.slots[i] = self.access[i].slots.load(Ordering::Relaxed);
            totals.non_null_slots[i] = self.access[i].non_null_slots.load(Ordering::Relaxed);
        }
        totals
    }

    /// The **most recent walk's** totals — the live-set gauge — or `None` if no
    /// walk has completed.
    ///
    /// This is the number the study's §4 question is about: *what fraction of
    /// the reference slots that exist right now are legacy cells*.
    /// [`ZSlotCensus::walk_totals`] answers a different one — the same fraction
    /// weighted by how many collections each object survived — and the two
    /// diverge exactly on the long-lived legacy population the study predicts.
    /// See the field docs on `last_walk`.
    pub fn last_walk_totals(&self) -> Option<ZShapeTotals> {
        *self.last_walk.lock()
    }

    /// Statics blocks observed across every walk. `0` while
    /// [`ZCensusHeapView::static_reference_slots`] is the default no-op.
    pub fn statics_blocks(&self) -> u64 {
        self.walk_statics_blocks.load(Ordering::Relaxed)
    }

    /// Totals for `strategy`.
    pub fn totals(&self, strategy: ZCensusStrategy) -> ZShapeTotals {
        match strategy {
            ZCensusStrategy::HeapWalk => self.walk_totals(),
            ZCensusStrategy::AccessPath => self.access_totals(),
        }
    }

    /// Completed walks.
    pub fn walks_completed(&self) -> u64 {
        self.walks_completed.load(Ordering::Acquire)
    }

    /// Object visits and reference slots the per-class table could not attribute
    /// because it was at [`CLASS_TABLE_CAP`].
    pub fn class_table_overflow(&self) -> (u64, u64) {
        (
            self.class_table_overflow_objects.load(Ordering::Relaxed),
            self.class_table_overflow_slots.load(Ordering::Relaxed),
        )
    }

    /// Cumulative `is_compact` / kind disagreements. **Must be zero**; see
    /// [`ZWalkResult::kind_disagreements`].
    pub fn kind_disagreements(&self) -> u64 {
        self.kind_disagreements.load(Ordering::Relaxed)
    }

    /// The top `n` classes of the most recent walk, ranked as described on
    /// [`ZWalkResult::classes`]. Bounded by [`PUBLISHED_CLASS_ROWS`] regardless
    /// of `n`.
    pub fn top_classes(&self, n: usize) -> Vec<ZClassRow> {
        let table = self.top_classes.lock();
        table.iter().take(n).copied().collect()
    }

    /// Zero every counter and drop the class table. Tests only — production
    /// counters are monotonic for the life of the heap.
    pub fn reset_for_test(&self) {
        for i in 0..Z_SLOT_SHAPE_COUNT {
            self.walk[i].reset();
            self.access[i].reset();
        }
        for slot in [
            &self.walk_legacy_unwritten,
            &self.walk_legacy_primitive,
            &self.walk_objects_without_ref_slots,
            &self.walks_completed,
            &self.walk_statics_blocks,
            &self.class_table_overflow_objects,
            &self.class_table_overflow_slots,
            &self.kind_disagreements,
        ] {
            slot.store(0, Ordering::Relaxed);
        }
        self.top_classes.lock().clear();
        *self.last_walk.lock() = None;
    }

    // -- text report --------------------------------------------------------

    /// The human-readable end-of-run table.
    ///
    /// `[GC-SUMMARY]`-prefixed to match
    /// [`super::metrics::ZgcMetrics::format_summary`] and `g1`'s
    /// `print_gc_summary`, so a harness that already scrapes summary lines picks
    /// these up too.
    pub fn format_summary(&self) -> String {
        let mut s = String::with_capacity(4096);
        s.push_str(&format!(
            "[GC-SUMMARY] zgc-census run={run} enabled={enabled} access_sampling={acc} \
             walks={walks} statics_wired={statics}\n",
            run = self.run_label(),
            enabled = self.is_enabled(),
            acc = self.access_sampling_enabled(),
            walks = self.walks_completed(),
            statics = self.statics_wired(),
        ));

        // The gauge FIRST, because it is the number the study asked for. The
        // cumulative block below it answers a different question and says so.
        match self.last_walk_totals() {
            Some(gauge) => {
                s.push_str(
                    "[GC-SUMMARY] zgc-census last_walk — live-set GAUGE (the most recent walk \
                     alone; this is the study §4 number)\n",
                );
                s.push_str(&format!(
                    "[GC-SUMMARY] zgc-census last_walk ref_slots={slots} non_null={nn} \
                     objects={objs} legacy_share={legacy:.4} tagged_cell_share={tagged:.4} \
                     VERDICT={verdict}\n",
                    slots = gauge.total_slots(),
                    nn = gauge.total_non_null_slots(),
                    objs = gauge.total_objects(),
                    legacy = gauge.legacy_share(),
                    tagged = gauge.tagged_cell_share(),
                    verdict = gauge.verdict().key(),
                ));
            }
            None => {
                s.push_str(
                    "[GC-SUMMARY] zgc-census last_walk: NONE — no heap walk has completed, so \
                     there is no live-set gauge (this is not a finding of 'no slots')\n",
                );
            }
        }

        for strategy in [ZCensusStrategy::HeapWalk, ZCensusStrategy::AccessPath] {
            s.push_str(&self.format_strategy(strategy));
        }

        let (overflow_objects, overflow_slots) = self.class_table_overflow();
        let top = self.top_classes(16);
        if top.is_empty() {
            // "Silence is not zero": an empty table means no walk has run, and
            // saying so beats printing nothing at all.
            s.push_str(
                "[GC-SUMMARY] zgc-census classes: NONE — no heap walk has completed in this \
                 run (this is not a finding of 'no classes')\n",
            );
        } else {
            s.push_str(&format!(
                "[GC-SUMMARY] zgc-census classes (most recent walk, top {n}, ranked by legacy \
                 slots)\n",
                n = top.len(),
            ));
            s.push_str(&format!(
                "[GC-SUMMARY] zgc-census class {:>10} {:>10} {:>12} {:>12} {:>12} {:>12}\n",
                "class_id", "objects", "compact", "legacy", "array_elem", "static",
            ));
            for row in top.iter() {
                s.push_str(&format!(
                    "[GC-SUMMARY] zgc-census class {:>10} {:>10} {:>12} {:>12} {:>12} {:>12}\n",
                    row.class_id.as_u32(),
                    row.objects,
                    row.slots[ZSlotShape::CompactField.index()],
                    row.slots[ZSlotShape::LegacyField.index()],
                    row.slots[ZSlotShape::ArrayElement.index()],
                    row.slots[ZSlotShape::StaticField.index()],
                ));
            }
        }
        if overflow_objects > 0 || overflow_slots > 0 {
            s.push_str(&format!(
                "[GC-SUMMARY] zgc-census class table OVERFLOWED at cap={cap}: {objs} object \
                 visits and {slots} slots are in the totals but in no class row\n",
                cap = CLASS_TABLE_CAP,
                objs = overflow_objects,
                slots = overflow_slots,
            ));
        }

        let disagreements = self.kind_disagreements();
        if disagreements > 0 {
            s.push_str(&format!(
                "[GC-SUMMARY] zgc-census VOID: is_compact disagreed with the reported object \
                 kind {disagreements} times — do not use these numbers\n",
            ));
        }
        s
    }

    /// The per-strategy block of [`ZSlotCensus::format_summary`].
    fn format_strategy(&self, strategy: ZCensusStrategy) -> String {
        let totals = self.totals(strategy);
        let key = strategy.key();
        let mut s = String::with_capacity(1024);
        s.push_str(&format!(
            "[GC-SUMMARY] zgc-census {key} — {label} [{accumulation}]\n",
            key = key,
            label = strategy.label(),
            accumulation = match strategy {
                // Say it on the line rather than only in a doc comment: a
                // reader scraping this block has no other way to know that a
                // long-lived object is in it once per collection.
                ZCensusStrategy::HeapWalk => {
                    "CUMULATIVE over all walks, so lifetime-weighted — for the live-set \
                     gauge read the last_walk line above"
                }
                ZCensusStrategy::AccessPath => "cumulative over the run; each access counted once",
            },
        ));
        s.push_str(&format!(
            "[GC-SUMMARY] zgc-census {key} shape {:<46} {:>14} {:>14} {:>12} {:>9}\n",
            "name",
            "ref_slots",
            "non_null",
            "objects",
            "share",
            key = key,
        ));
        for shape in ZSlotShape::ALL {
            let i = shape.index();
            // A structurally-unreachable column must say so rather than print a
            // zero that reads as a measurement.
            let unreached =
                shape == ZSlotShape::StaticField && !self.statics_wired() && totals.slots[i] == 0;
            let share_col = if unreached {
                String::from("NOT WIRED")
            } else {
                format!("{:.4}", totals.share(shape))
            };
            // An access is a slot event and knows nothing about how many
            // objects produced it, so this column is structurally absent rather
            // than measured as zero — the same rule as `NOT WIRED` above.
            let objects_col = if strategy == ZCensusStrategy::AccessPath {
                String::from("n/a")
            } else {
                totals.objects[i].to_string()
            };
            s.push_str(&format!(
                "[GC-SUMMARY] zgc-census {key} shape {:<46} {:>14} {:>14} {:>12} {:>9}\n",
                shape.label(),
                totals.slots[i],
                totals.non_null_slots[i],
                objects_col,
                share_col,
                key = key,
            ));
        }
        s.push_str(&format!(
            "[GC-SUMMARY] zgc-census {key} totals: ref_slots={slots} non_null={nn} \
             objects={objs} legacy_unwritten_cells={unwritten} legacy_primitive_cells={prim}\n",
            key = key,
            slots = totals.total_slots(),
            nn = totals.total_non_null_slots(),
            objs = if strategy == ZCensusStrategy::AccessPath {
                String::from("n/a")
            } else {
                totals.total_objects().to_string()
            },
            unwritten = totals.legacy_unwritten_slots,
            prim = totals.legacy_primitive_slots,
        ));
        s.push_str(&format!(
            "[GC-SUMMARY] zgc-census {key} legacy_share={legacy:.4} tagged_cell_share={tagged:.4} \
             non_atomic_share={nonatomic:.4}\n",
            key = key,
            legacy = totals.legacy_share(),
            tagged = totals.tagged_cell_share(),
            nonatomic = totals.non_atomic_share(),
        ));
        let verdict = totals.verdict();
        s.push_str(&format!(
            "[GC-SUMMARY] zgc-census {key} VERDICT={verdict} ({applies}): {implication}\n",
            key = key,
            verdict = verdict.key(),
            applies = if strategy.thresholds_apply() {
                "the study's 5%/25% barrier-cost thresholds apply to THIS number"
            } else {
                "indicative only — the study's thresholds are barrier-cost and belong \
                 against the access-path number"
            },
            implication = verdict.implication(),
        ));
        s
    }

    /// Log the summary at `info` on the `zgc` target.
    ///
    /// `tracing` rather than `eprintln!` so the summary honours the run's log
    /// filter, matching [`super::metrics::ZgcMetrics::log_summary`].
    pub fn log_summary(&self) {
        for line in self.format_summary().lines() {
            tracing::info!(target: "zgc", "{}", line);
        }
    }

    // -- machine-readable ---------------------------------------------------

    /// The TSV column names for [`ZSlotCensus::to_tsv_row`], tab-separated.
    ///
    /// **Intent: aggregation across a suite.** The Spring Boot and H2 harnesses
    /// already concatenate per-class `results.tsv` files, which is how a number
    /// from 1975 classes becomes one answer instead of 1975 log greps — see
    /// [`super::metrics::ZgcMetrics::tsv_header`], which this deliberately
    /// mirrors so the two files join.
    ///
    /// Returns an owned `String`, **not** a `&'static str` from a `OnceLock`, as
    /// `metrics.rs` does. That difference is the module header's no-global rule:
    /// a `static OnceLock` here would be process-global state in a file whose
    /// whole point is instance-owned diagnostics, and this tree has documented
    /// parallel-test crashes from process-global GC caches. The cost is one
    /// small allocation per call, at a call site that runs once per VM.
    ///
    /// Column order is fixed and is the same order [`ZSlotCensus::to_tsv_row`]
    /// emits. **Append at the end only**; never reorder or remove one, and never
    /// let the two functions drift — `tsv_header_and_row_have_equal_column_counts`
    /// is the guard, because a silent off-by-one shifts every value into the
    /// wrong column and corrupts the whole downstream analysis without erroring
    /// anywhere.
    pub fn tsv_header() -> String {
        ZSlotCensus::tsv_column_names().join("\t")
    }

    /// The column names, in order. Split out so the header and the column-count
    /// assertion have exactly one source.
    pub fn tsv_column_names() -> Vec<String> {
        let mut cols: Vec<String> = vec![
            "run".to_string(),
            "walks_completed".to_string(),
            "statics_wired".to_string(),
            "class_table_overflow_objects".to_string(),
            "class_table_overflow_slots".to_string(),
            "kind_disagreements".to_string(),
        ];
        for shape in ZSlotShape::ALL {
            let key = shape.key();
            cols.push(format!("walk_{key}_slots"));
            cols.push(format!("walk_{key}_nonnull_slots"));
            cols.push(format!("walk_{key}_objects"));
            cols.push(format!("walk_{key}_share"));
        }
        cols.push("walk_total_ref_slots".to_string());
        cols.push("walk_total_objects".to_string());
        cols.push("walk_legacy_unwritten_slots".to_string());
        cols.push("walk_legacy_primitive_slots".to_string());
        cols.push("walk_objects_without_ref_slots".to_string());
        for shape in ZSlotShape::ALL {
            let key = shape.key();
            cols.push(format!("access_{key}_slots"));
            cols.push(format!("access_{key}_nonnull_slots"));
            cols.push(format!("access_{key}_share"));
        }
        cols.push("access_total_ref_slots".to_string());
        cols.push("walk_verdict".to_string());
        cols.push("access_verdict".to_string());
        for i in 1..=TSV_TOP_CLASSES {
            cols.push(format!("top{i}_class_id"));
            cols.push(format!("top{i}_legacy_field_slots"));
            cols.push(format!("top{i}_total_ref_slots"));
        }
        // ---- appended 2026-09-20: the live-set GAUGE ------------------------
        //
        // Appended, never interleaved with the `walk_*` columns above, because
        // the rule on `tsv_header` is "append at the end only" and because a
        // suite's existing `results.tsv` files must still join. The `walk_*`
        // columns are cumulative across walks and therefore lifetime-weighted;
        // these are the most recent walk alone, which is the number the study's
        // §4 question actually asks for. See `ZSlotCensus::last_walk`.
        //
        // `last_walk_ran` is `0`/`1` rather than leaving the reader to infer it
        // from a row of zeroes — "silence is not zero", the same rule
        // `statics_wired` follows.
        cols.push("last_walk_ran".to_string());
        for shape in ZSlotShape::ALL {
            let key = shape.key();
            cols.push(format!("last_walk_{key}_slots"));
            cols.push(format!("last_walk_{key}_share"));
        }
        cols.push("last_walk_total_ref_slots".to_string());
        cols.push("last_walk_verdict".to_string());
        cols.push("walk_statics_blocks".to_string());
        cols
    }

    /// One TSV row: this VM's whole-run census.
    ///
    /// Same column order as [`ZSlotCensus::tsv_header`]. Counts are integers;
    /// the shares are the only floats and are formatted with a fixed six-digit
    /// precision so no locale-dependent separator can break a downstream parse.
    /// The verdict columns carry [`ZCensusVerdict::key`], which is a bare token.
    ///
    /// Unfilled top-class columns read `-1` for the class id and `0` for the
    /// counts, so the column count never depends on the data.
    pub fn to_tsv_row(&self) -> String {
        let walk = self.walk_totals();
        let access = self.access_totals();
        let (overflow_objects, overflow_slots) = self.class_table_overflow();
        let top = self.top_classes(TSV_TOP_CLASSES);

        let mut cols: Vec<String> = vec![
            self.run_label(),
            self.walks_completed().to_string(),
            u8::from(self.statics_wired()).to_string(),
            overflow_objects.to_string(),
            overflow_slots.to_string(),
            self.kind_disagreements().to_string(),
        ];
        for shape in ZSlotShape::ALL {
            let i = shape.index();
            cols.push(walk.slots[i].to_string());
            cols.push(walk.non_null_slots[i].to_string());
            cols.push(walk.objects[i].to_string());
            cols.push(format!("{:.6}", walk.share(shape)));
        }
        cols.push(walk.total_slots().to_string());
        cols.push(walk.total_objects().to_string());
        cols.push(walk.legacy_unwritten_slots.to_string());
        cols.push(walk.legacy_primitive_slots.to_string());
        cols.push(walk.objects_without_ref_slots.to_string());
        for shape in ZSlotShape::ALL {
            let i = shape.index();
            cols.push(access.slots[i].to_string());
            cols.push(access.non_null_slots[i].to_string());
            cols.push(format!("{:.6}", access.share(shape)));
        }
        cols.push(access.total_slots().to_string());
        cols.push(walk.verdict().key().to_string());
        cols.push(access.verdict().key().to_string());
        for i in 0..TSV_TOP_CLASSES {
            match top.get(i) {
                Some(row) => {
                    cols.push(row.class_id.as_u32().to_string());
                    cols.push(row.legacy_slots().to_string());
                    cols.push(row.total_slots().to_string());
                }
                None => {
                    cols.push("-1".to_string());
                    cols.push("0".to_string());
                    cols.push("0".to_string());
                }
            }
        }
        // The live-set gauge; see `tsv_column_names`. An absent walk emits
        // `last_walk_ran=0`, zeroed counts and the `no_data` verdict, so the
        // column count never depends on the data and a row of zeroes is never
        // mistaken for a measurement.
        let gauge = self.last_walk_totals();
        cols.push(u8::from(gauge.is_some()).to_string());
        let gauge = gauge.unwrap_or_else(|| ZShapeTotals::empty(ZCensusStrategy::HeapWalk));
        for shape in ZSlotShape::ALL {
            let i = shape.index();
            cols.push(gauge.slots[i].to_string());
            cols.push(format!("{:.6}", gauge.share(shape)));
        }
        cols.push(gauge.total_slots().to_string());
        cols.push(gauge.verdict().key().to_string());
        cols.push(self.statics_blocks().to_string());
        cols.join("\t")
    }
}

// SAFETY-adjacent note: `ZSlotCensus` holds only atomics, `parking_lot::Mutex`
// and owned data, so `Send + Sync` are derived by the compiler. It is stated
// here only because the sibling `ZgcRealHeap` needs hand-written `unsafe impl`s
// for its raw pointers and a reader may expect the same here. It does not.

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `part / whole`, or `0.0` when `whole` is zero.
///
/// **The all-zero case is the common one**, not a corner case: an unenabled
/// census, a run before the first collection, and a shape the workload never
/// produces all land here. `NaN` in a summary line is indistinguishable from a
/// parse bug for whoever reads the log.
fn share_of(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// Fold one observation into the four running tallies.
///
/// Shared by the object loop and the statics loop so the tag rules cannot drift
/// between them.
///
/// * a bare-word observation is always a reference slot;
/// * a cell with tag [`VALUE_TAG_OBJECT`] is a reference slot;
/// * a cell with tag [`VALUE_TAG_INT`] and payload `0` is an **unwritten** slot
///   — the zero-fill state of study §1.7, counted apart because it holds no
///   reference today and will hold one tomorrow;
/// * anything else is a genuinely primitive slot.
fn classify_into(
    obs: ZSlotObservation,
    slots: &mut [u64; Z_SLOT_SHAPE_COUNT],
    non_null: &mut [u64; Z_SLOT_SHAPE_COUNT],
    unwritten: &mut u64,
    primitive: &mut u64,
) {
    let i = obs.shape.index();
    match obs.legacy_tag {
        None => {
            slots[i] = slots[i].saturating_add(1);
            if obs.raw_word != 0 {
                non_null[i] = non_null[i].saturating_add(1);
            }
        }
        Some(VALUE_TAG_OBJECT) => {
            slots[i] = slots[i].saturating_add(1);
            if obs.raw_word != 0 {
                non_null[i] = non_null[i].saturating_add(1);
            }
        }
        Some(VALUE_TAG_INT) if obs.raw_word == 0 => {
            *unwritten = unwritten.saturating_add(1);
        }
        Some(_) => {
            *primitive = primitive.saturating_add(1);
        }
    }
}

/// Get or create the per-class row for `class_id`, or `None` once the table is
/// at [`CLASS_TABLE_CAP`].
///
/// Bumps `overflow_objects` on the refusal so the drop is counted rather than
/// silent — a bounded table that does not say when it saturated reports a
/// top-N that quietly omits the class that mattered.
fn class_row<'a>(
    classes: &'a mut FxHashMap<u32, ZClassRow>,
    class_id: ClassId,
    overflow_objects: &mut u64,
) -> Option<&'a mut ZClassRow> {
    let raw = class_id.as_u32();
    if !classes.contains_key(&raw) && classes.len() >= CLASS_TABLE_CAP {
        *overflow_objects = overflow_objects.saturating_add(1);
        return None;
    }
    Some(
        classes
            .entry(raw)
            .or_insert_with(|| ZClassRow::new(class_id)),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
