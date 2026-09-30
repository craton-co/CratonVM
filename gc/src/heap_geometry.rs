// SPDX-License-Identifier: Apache-2.0
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
//! against it -- is `docs/internal/gc-common-round-20260923/common-w7f-compressed-oops-second-vm-skips-the-fit-check-FIXED-20260923.md`.
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
mod tests {
    use super::*;

    // gce e2/o (`gce-e1x-heap-geometry-envelope-test-flakes-FIXED-20260929.md`): the
    // table logic is tested on a LOCAL `HeapSpanTable`. The process-global
    // `HEAP_SPANS` is rewritten, all three slots, by every `GenerationalHeap`
    // any test in this crate builds (`gen_heap` publishes each arena), with no
    // lock a test here could share; the old slot-2 tests read it back between
    // their own publish and their assertion and flaked (1 in 4 Windows runs of
    // `the_envelope_spans_every_published_slot`), and their `restore` could
    // write a stale span over a live heap's. Only the out-of-range test below
    // still touches the global, through a sentinel no real heap can hold.

    #[test]
    fn an_unpublished_slot_reads_as_zero_and_contributes_nothing() {
        let t = HeapSpanTable::new();
        assert_eq!(t.span(2), (0, 0));
        assert_eq!(t.envelope(), None, "an all-zero table has no envelope");
        t.publish(2, 0x1000, 0x2000);
        t.publish(2, 0, 0);
        assert_eq!(t.span(2), (0, 0));
        assert_eq!(t.envelope(), None);
    }

    #[test]
    fn an_inverted_or_empty_span_clears_rather_than_publishing_nonsense() {
        let t = HeapSpanTable::new();
        t.publish(2, 0x9000, 0x1000);
        assert_eq!(t.span(2), (0, 0), "inverted span must not stand");
        t.publish(2, 0x1000, 0x1000);
        assert_eq!(t.span(2), (0, 0), "empty span must not stand");
    }

    #[test]
    fn an_out_of_range_slot_is_declined_rather_than_wrapping_onto_another() {
        // On a local table first: exact, no neighbour can write it.
        let t = HeapSpanTable::new();
        for out_of_range in [HEAP_SPAN_SLOTS, HEAP_SPAN_SLOTS + 1, usize::MAX] {
            t.publish(out_of_range, 0xDEAD_0000, 0xDEAD_1000);
        }
        assert!((0..HEAP_SPAN_SLOTS).all(|s| t.span(s) == (0, 0)));
        assert_eq!(t.span(usize::MAX), (0, 0));
        // Then on the global, through the public functions:
        // The failure this rules out is a write that lands on slot 0 by
        // arithmetic -- `HEAP_SPAN_SLOTS % HEAP_SPAN_SLOTS` is 0 -- and
        // silently redescribes the live young generation.
        //
        // Asserted with a SENTINEL rather than by snapshotting the table before
        // and after, and that is the whole of the 2026-09-10 rewrite. The old
        // form compared `(0..HEAP_SPAN_SLOTS).map(heap_span)` across the two
        // out-of-range publishes -- a test that reads more of the global than
        // it wrote: slots 0..2 belong to whatever heap another test in this
        // process is building (`gen_heap` publishes one per generation, `zgc`
        // publishes slot 0), and no lock here serialises against those.
        //
        // It flaked exactly there under full-workspace load: slots 0 and 1 came
        // back holding EACH OTHER's spans -- one concurrent generational heap
        // build, not a wrap. Sorting the two sides would have made it pass by
        // destroying the slot-to-value binding the assertion exists to check,
        // which is why it is not what this does.
        //
        // A span no real heap can hold answers the actual question -- did the
        // out-of-range write land anywhere in range? -- and a sibling
        // publishing a genuine heap cannot perturb it.
        const SENTINEL: (usize, usize) = (0xDEAD_0000, 0xDEAD_1000);
        for out_of_range in [HEAP_SPAN_SLOTS, HEAP_SPAN_SLOTS + 1, usize::MAX] {
            publish_heap_span(out_of_range, SENTINEL.0, SENTINEL.1);
        }
        for slot in 0..HEAP_SPAN_SLOTS {
            assert_ne!(
                heap_span(slot),
                SENTINEL,
                "an out-of-range publish wrapped onto slot {slot}",
            );
        }
        // The read side declines too, and for the same reason: an out-of-range
        // READ must not fold onto an in-range slot and report its span.
        assert_eq!(heap_span(HEAP_SPAN_SLOTS), (0, 0));
        assert_eq!(heap_span(usize::MAX), (0, 0));
    }

    /// **Every backend must publish.** A source witness, and it has to be one.
    ///
    /// The defect this guards is not a wrong value, it is a call that does not
    /// happen — and that is precisely the failure this whole module exists to
    /// undo. `compressed_oops::enable_for_live_heap` answered "no live heap
    /// regions published" for G1 and ZGC not because anything was broken but
    /// because nobody had written the publish, and the symptom surfaced as a
    /// statement about compressed oops rather than about a missing call. No
    /// runtime assertion can see a call that was never made.
    ///
    /// It cannot be a behavioural test either: this table is process-global and
    /// this crate's tests share one process, so a test that constructed a G1
    /// heap and read the table back could have its slot overwritten by a
    /// generational heap another test built a microsecond later. The tests
    /// above use a local `HeapSpanTable` precisely because of that (gce e2/o).
    #[test]
    fn every_backend_publishes_its_geometry() {
        for (name, src) in [
            ("gen_heap", include_str!("gen_heap.rs")),
            ("g1", include_str!("g1.rs")),
            #[cfg(feature = "zgc")]
            ("zgc", include_str!("zgc.rs")),
        ] {
            assert!(
                src.contains("heap_geometry::publish_heap_span("),
                "{name} no longer publishes its heap geometry. Every consumer of \
                 `heap_geometry` — the narrow-oop window, a conservative \
                 scanner's range prefilter — then reads this backend as though \
                 it had no heap, which is the exact shape of the defect this \
                 table was added to remove."
            );
        }
    }

    #[test]
    fn the_envelope_spans_every_published_slot() {
        let t = HeapSpanTable::new();
        t.publish(2, 0x4000_0000, 0x5000_0000);
        assert_eq!(t.envelope(), Some((0x4000_0000, 0x5000_0000)));
        // Every slot counts; the gap between two arenas is inside the envelope.
        t.publish(0, 0x1000_0000, 0x1100_0000);
        t.publish(1, 0x6000_0000, 0x6800_0000);
        assert_eq!(t.envelope(), Some((0x1000_0000, 0x6800_0000)));
        // An unpublished slot contributes nothing.
        t.publish(1, 0, 0);
        assert_eq!(t.envelope(), Some((0x1000_0000, 0x5000_0000)));
    }
}
