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
//! `VmHeap`s in one process (which happens in tests, not in a shipping VM)
//! overwrite each other's spans. That is tolerable for the consumers this has
//! -- `enable_for_live_heap` runs once at VM init, and an envelope is a filter
//! whose worst error is admitting an address a later screen rejects -- and it is
//! not tolerable for a consumer that would treat a span as proof of ownership.
//! Do not add one without giving this table a heap identity first.

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
pub static HEAP_SPANS: HeapSpanTable = HeapSpanTable {
    words: [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ],
};

/// Publish `[base, end)` as span `slot`.
///
/// Idempotent, and safe to call from any backend at any time -- the read side
/// only ever derives an envelope from it. An empty or inverted range clears the
/// slot rather than publishing nonsense.
pub fn publish_heap_span(slot: usize, base: usize, end: usize) {
    if slot >= HEAP_SPAN_SLOTS {
        return;
    }
    let (base, end) = if end > base { (base, end) } else { (0, 0) };
    // Base first, then end: a reader that samples between the two sees a slot
    // whose end is stale-low, i.e. a NARROWER span. Narrow is the safe
    // direction for every consumer -- an envelope that is too small refuses a
    // legitimate address (one declined optimisation), where one that is too
    // large admits an address the heap does not own.
    HEAP_SPANS.words[slot * 2].store(base, Ordering::Release);
    HEAP_SPANS.words[slot * 2 + 1].store(end, Ordering::Release);
}

/// Read back one `[base, end)` pair. `(0, 0)` for an out-of-range or
/// unpublished slot.
pub fn heap_span(slot: usize) -> (usize, usize) {
    if slot >= HEAP_SPAN_SLOTS {
        return (0, 0);
    }
    (
        HEAP_SPANS.words[slot * 2].load(Ordering::Acquire),
        HEAP_SPANS.words[slot * 2 + 1].load(Ordering::Acquire),
    )
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
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    for slot in 0..HEAP_SPAN_SLOTS {
        let (base, end) = heap_span(slot);
        if base == 0 || end <= base {
            continue;
        }
        lo = lo.min(base);
        hi = hi.max(end);
    }
    (lo != usize::MAX).then_some((lo, hi))
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

    /// The slot this module's own tests may use without colliding with a live
    /// backend: the table is process-global and these tests share a process with
    /// everything else in the crate. Slot 2 is only ever written by the
    /// generational backend's old gen, which no test in this module builds --
    /// and each test restores whatever it found, so a heap constructed
    /// concurrently in another test sees its own value again.
    const TEST_SLOT: usize = 2;

    /// Serialises the tests below against EACH OTHER.
    ///
    /// They share `TEST_SLOT`, and `cargo test` runs them in parallel: one test
    /// snapshots all three slots while another is mid-write, and the snapshot
    /// disagrees with itself. That is not a flaw in the table — it is the
    /// process-global property the module doc states — but it is a flaw in a
    /// test that reads more of the table than it wrote. Taking one lock is
    /// cheaper than teaching each test to tolerate the others.
    ///
    /// It does NOT serialise against a live backend constructing a heap in some
    /// other test; that is why these use slot 2, which only the generational old
    /// gen writes, and why each restores what it found.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn restore(prev: (usize, usize)) {
        publish_heap_span(TEST_SLOT, prev.0, prev.1);
    }

    #[test]
    fn an_unpublished_slot_reads_as_zero_and_contributes_nothing() {
        let _serial = TEST_LOCK.lock();
        let prev = heap_span(TEST_SLOT);
        publish_heap_span(TEST_SLOT, 0, 0);
        assert_eq!(heap_span(TEST_SLOT), (0, 0));
        restore(prev);
    }

    #[test]
    fn an_inverted_or_empty_span_clears_rather_than_publishing_nonsense() {
        let _serial = TEST_LOCK.lock();
        let prev = heap_span(TEST_SLOT);
        publish_heap_span(TEST_SLOT, 0x9000, 0x1000);
        assert_eq!(heap_span(TEST_SLOT), (0, 0), "inverted span must not stand");
        publish_heap_span(TEST_SLOT, 0x1000, 0x1000);
        assert_eq!(heap_span(TEST_SLOT), (0, 0), "empty span must not stand");
        restore(prev);
    }

    #[test]
    fn an_out_of_range_slot_is_declined_rather_than_wrapping_onto_another() {
        let _serial = TEST_LOCK.lock();
        // The failure this rules out is a write that lands on slot 0 by
        // arithmetic -- `HEAP_SPAN_SLOTS % HEAP_SPAN_SLOTS` is 0 -- and
        // silently redescribes the live young generation.
        //
        // Asserted with a SENTINEL rather than by snapshotting the table before
        // and after, and that is the whole of the 2026-09-10 rewrite. The old
        // form compared `(0..HEAP_SPAN_SLOTS).map(heap_span)` across the two
        // out-of-range publishes, which is precisely what `TEST_LOCK`'s doc
        // calls "a test that reads more of the table than it wrote": slots 0
        // and 1 belong to whatever heap another test in this process is
        // building (`gen_heap` publishes one per generation, `zgc` publishes
        // slot 0), and `TEST_LOCK` does not serialise against those.
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
        let prev = heap_span(TEST_SLOT);
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
        restore(prev);
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
    /// generational heap another test built a microsecond later. The `TEST_SLOT`
    /// tests above stay in their own lane precisely because of that.
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
                "{name} no longer publishes its heap geometry. Every consumer of                  `heap_geometry` — the narrow-oop window, a conservative                  scanner's range prefilter — then reads this backend as though                  it had no heap, which is the exact shape of the defect this                  table was added to remove."
            );
        }
    }

    #[test]
    fn the_envelope_spans_every_published_slot() {
        let _serial = TEST_LOCK.lock();
        let prev = heap_span(TEST_SLOT);
        publish_heap_span(TEST_SLOT, 0x4000_0000, 0x5000_0000);
        let (lo, hi) = heap_envelope().expect("a published slot makes an envelope");
        assert!(lo <= 0x4000_0000, "envelope must contain the published base");
        assert!(hi >= 0x5000_0000, "envelope must contain the published end");
        assert!(published());
        restore(prev);
    }
}
