// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Where the Java heap actually IS, asked as its own question.
//!
//! # Why a fifth table, and why this one is different
//!
//! `gen_heap.rs` carries four process-global six-word geometry tables, and the
//! comments on them tell the story of how they got there:
//!
//! * [`crate::gen_heap::JIT_REGION_BOUNDS`] answers TWO questions at once --
//!   its documented "is this address mapped, so a raw load cannot fault", and
//!   its load-bearing "may an inline reference store skip the collector's write
//!   barrier". G1 and ZGC answer the second one by leaving the table EMPTY, so
//!   filling it to make loads inline would silently re-enable those stores.
//! * [`crate::gen_heap::JIT_READ_BOUNDS`] exists because of exactly that: "One
//!   table, two questions, opposite answers... Hence the sibling."
//! * [`crate::gen_heap::MOVABLE_BOUNDS`] answers "may an object here move".
//! * G1's own barrier table answers "what are the numbers an inline barrier
//!   needs", and its doc opens `This is the same lesson a third time`.
//!
//! Every one of those is a question the JIT asks about a *capability*. None of
//! them is the question `where is the heap`, and that is why asking it has kept
//! going wrong:
//!
//! * [`crate::compressed_oops::enable_for_live_heap`] needs a base and a limit
//!   to fix the narrow-oop window. It read `JIT_REGION_BOUNDS` -- the one table
//!   G1 may never fill -- so it answered "no live heap regions published
//!   (non-generational backend?)" for G1 and for ZGC, and compressed oops was
//!   refused on the DEFAULT collector for a reason that has nothing to do with
//!   compressed oops.
//! * `VmHeap::conservative_addr_span`'s ZGC arm returned `None` for months on
//!   the stated grounds that "ZGC keeps live bases in a registry, not a
//!   contiguous arena" -- a false premise, and, in that arm's own words, "the
//!   reason this went unfixed". The envelope existed the whole time; nothing
//!   published it where the asker was looking.
//!
//! This table answers only that one question, it carries no capability meaning
//! whatsoever, and **every backend publishes into it**. A reader wanting to know
//! whether an inline store may skip a barrier must still ask the table that
//! means that; there is deliberately nothing here for them.
//!
//! # What a slot means
//!
//! `[base, end)` of one contiguous span of address space this heap has reserved
//! for Java objects. Reserved, not committed: the narrow-oop window has to cover
//! every address the heap can EVER produce, so a bound that tracked the
//! committed prefix would be re-published as the heap grew and would invalidate
//! every reference already encoded against it.
//!
//! Three slots because the generational backend has three arenas. A backend with
//! one contiguous arena uses slot 0 and leaves the rest zero.
//!
//! An unpublished slot is `(0, 0)`, and every reader treats the all-zero table
//! as "this backend has not said", which is the same fail-safe every other table
//! in this family uses.
//!
//! # One table per PROCESS, not per heap
//!
//! Like every other table in this family, and with the same consequence: two
//! `VmHeap`s in one process overwrite each other's spans. That happens in
//! tests AND in a shipping process: an embedder (`libcratonvm`,
//! `cratonvm-embed`) may host several VMs (gc-common w7-f corrected this
//! sentence, which said "not in a shipping VM"; wave 6 serialised the VMs'
//! pauses for the same reason). It is tolerable for the one consumer this has
//! -- `enable_for_live_heap` reads it once, at the first VM's init, and an
//! envelope is a filter whose worst error is admitting an address a later
//! screen rejects -- and it is not tolerable for a consumer that would treat a
//! span as proof of ownership. Do not add one without giving this table a heap
//! identity first. (`enable_for_live_heap`'s own multi-VM gap -- a second VM
//! is handed the first VM's narrow-oop window without its spans being checked
//! against it -- is `docs/feature-designs/generative-gc-architecture.md`.
//! Since gc-common w8-f the fit check exists, and its intended input is not
//! this table but the heap's own `VmHeap::reserved_address_envelope`, passed
//! to `compressed_oops::admit_heap`; `enable_for_live_heap` still reads this
//! table until `vm_init` switches over.)

use std::sync::atomic::{AtomicUsize, Ordering};

/// Number of spans a backend may publish. Three, for the generational heap's
/// young-from / young-to / old-gen arenas.
pub const HEAP_SPAN_SLOTS: usize = 3;

/// `[b0, e0, b1, e1, b2, e2]`.
#[repr(C)]
pub struct HeapSpanTable {
    pub words: [AtomicUsize; HEAP_SPAN_SLOTS * 2],
}

/// The reserved address spans of the live heap. See the module doc.
pub static HEAP_SPANS: HeapSpanTable = HeapSpanTable::new();

impl HeapSpanTable {
    /// An all-unpublished table.
    ///
    /// gce e2/o (`gce-e1x-heap-geometry-envelope-test-flakes-FIXED-20260929.md`): the
    /// table's logic is methods on the table, so the unit tests below run it on
    /// a table of their own. The process-global [`HEAP_SPANS`] is rewritten by
    /// every `GenerationalHeap` any test in the crate builds (all three slots,
    /// with no lock these tests could share), which is how
    /// `the_envelope_spans_every_published_slot` flaked.
    pub const fn new() -> Self {
        Self {
            words: [
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ],
        }
    }

    /// [`publish_heap_span`] on this table.
    pub fn publish(&self, slot: usize, base: usize, end: usize) {
        if slot >= HEAP_SPAN_SLOTS {
            return;
        }
        let (base, end) = if end > base { (base, end) } else { (0, 0) };
        // Base first, then end. A reader racing a RE-publish can pair the new base
        // with the old end, or (reading base before the store lands and end after)
        // the old base with the new end. Neither pairing is "narrower" in general
        // -- gen r4/alloc (2026-09-23) corrected this comment, which claimed so:
        // a span that moved DOWN pairs its new base with the old end and reads
        // WIDER, covering the gap between the two backings, and one that grew in
        // place pairs the old base with the new end, which is merely the new span.
        // An inverted pairing (a span that moved UP past its old end) reads as
        // `end <= base` and is skipped as unpublished.
        //
        // That is tolerable only because of what the table is for, and the module
        // doc already says it: an ENVELOPE, whose every consumer re-screens a hit
        // (`enable_for_live_heap` runs once at VM init before any republish; a
        // conservative scan's range test is followed by the object-start screen).
        // A consumer that would treat a span as proof of ownership needs a
        // seqlock here first, not just the heap identity the module doc asks for.
        self.words[slot * 2].store(base, Ordering::Release);
        self.words[slot * 2 + 1].store(end, Ordering::Release);
    }

    /// [`heap_span`] on this table.
    pub fn span(&self, slot: usize) -> (usize, usize) {
        if slot >= HEAP_SPAN_SLOTS {
            return (0, 0);
        }
        (
            self.words[slot * 2].load(Ordering::Acquire),
            self.words[slot * 2 + 1].load(Ordering::Acquire),
        )
    }

    /// [`heap_envelope`] on this table.
    pub fn envelope(&self) -> Option<(usize, usize)> {
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        for slot in 0..HEAP_SPAN_SLOTS {
            let (base, end) = self.span(slot);
            if base == 0 || end <= base {
                continue;
            }
            lo = lo.min(base);
            hi = hi.max(end);
        }
        (lo != usize::MAX).then_some((lo, hi))
    }
}

impl Default for HeapSpanTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Publish `[base, end)` as span `slot`.
///
/// Idempotent, and safe to call from any backend at any time -- the read side
/// only ever derives an envelope from it. An empty or inverted range clears the
/// slot rather than publishing nonsense.
pub fn publish_heap_span(slot: usize, base: usize, end: usize) {
    HEAP_SPANS.publish(slot, base, end);
}

/// Read back one `[base, end)` pair. `(0, 0)` for an out-of-range or
/// unpublished slot.
pub fn heap_span(slot: usize) -> (usize, usize) {
    HEAP_SPANS.span(slot)
}

/// The smallest `[lo, hi)` containing every published span, or `None` when no
/// backend has published one.
///
/// This is what a narrow-oop window is derived from, and what a conservative
/// scanner can reject a stack word with. It is an ENVELOPE and never an answer:
/// a word inside it may still be an object interior, a free block, or an
/// uncommitted granule. The same warning `VmHeap::conservative_addr_span`
/// carries applies verbatim -- accepting a word on the strength of the range
/// test alone is the unsoundness that arm was fixed for.
pub fn heap_envelope() -> Option<(usize, usize)> {
    HEAP_SPANS.envelope()
}

/// Every published span, in slot order, skipping the unpublished ones.
///
/// [`heap_envelope`] collapses the spans into one range and therefore covers
/// whatever sits BETWEEN two arenas as well. A caller that needs to know the
/// heap owns an address -- rather than that it might -- wants these.
pub fn heap_spans() -> impl Iterator<Item = (usize, usize)> {
    (0..HEAP_SPAN_SLOTS)
        .map(heap_span)
        .filter(|&(base, end)| base != 0 && end > base)
}

/// Has any backend published its geometry?
pub fn published() -> bool {
    heap_envelope().is_some()
}

#[cfg(test)]
mod tests;
