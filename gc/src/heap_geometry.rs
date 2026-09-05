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

    fn restore(prev: (usize, usize)) {
        publish_heap_span(TEST_SLOT, prev.0, prev.1);
    }

    #[test]
    fn an_unpublished_slot_reads_as_zero_and_contributes_nothing() {
        let prev = heap_span(TEST_SLOT);
        publish_heap_span(TEST_SLOT, 0, 0);
        assert_eq!(heap_span(TEST_SLOT), (0, 0));
        restore(prev);
    }

    #[test]
    fn an_inverted_or_empty_span_clears_rather_than_publishing_nonsense() {
        let prev = heap_span(TEST_SLOT);
        publish_heap_span(TEST_SLOT, 0x9000, 0x1000);
        assert_eq!(heap_span(TEST_SLOT), (0, 0), "inverted span must not stand");
        publish_heap_span(TEST_SLOT, 0x1000, 0x1000);
        assert_eq!(heap_span(TEST_SLOT), (0, 0), "empty span must not stand");
        restore(prev);
    }

    #[test]
    fn an_out_of_range_slot_is_declined_rather_than_wrapping_onto_another() {
        // The failure this rules out is a write that lands on slot 0 by
        // arithmetic and silently redescribes the live young generation.
        let before: Vec<(usize, usize)> = (0..HEAP_SPAN_SLOTS).map(heap_span).collect();
        publish_heap_span(HEAP_SPAN_SLOTS, 0x1000, 0x2000);
        publish_heap_span(usize::MAX, 0x1000, 0x2000);
        let after: Vec<(usize, usize)> = (0..HEAP_SPAN_SLOTS).map(heap_span).collect();
        assert_eq!(before, after);
        assert_eq!(heap_span(HEAP_SPAN_SLOTS), (0, 0));
    }

    #[test]
    fn the_envelope_spans_every_published_slot() {
        let prev = heap_span(TEST_SLOT);
        publish_heap_span(TEST_SLOT, 0x4000_0000, 0x5000_0000);
        let (lo, hi) = heap_envelope().expect("a published slot makes an envelope");
        assert!(lo <= 0x4000_0000, "envelope must contain the published base");
        assert!(hi >= 0x5000_0000, "envelope must contain the published end");
        assert!(published());
        restore(prev);
    }
}
