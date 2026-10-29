// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-5 lane B — what the enforcement DOES when it fires: the block-offset
//! jump disarms itself for the rest of the process, and the pause is still
//! correct.
//!
//! # Why the response has to be global, which is the part that is not obvious
//!
//! The local response to "this producer's address does not vouch" is to keep
//! the card dirty and drop the offset. That is **unsound**, and the hole is
//! the same one the atomic minimum exists for: two objects can start in one
//! card, and if the LOWER one's offset is dropped while the higher one's is
//! kept, the card's entry names the higher object — so the walk jumps past the
//! lower one, which is a dropped remembered-set edge, which is a
//! use-after-free. A minimum has no absorbing element, so there is no value
//! that could be stored to mean "never enter this card"; it would take a
//! second table.
//!
//! So the latch is process-wide: one unvouched producer address and every
//! source-region walk goes back to stepping over every object linearly, which
//! is what it did before W4-A and what it still does with the lever off. The
//! unit tests in `gc/src/g1_cards.rs` prove the mechanism on a synthetic
//! arena; this binary proves it end to end, on a heap, with the lever armed.
//!
//! # What it does, and why it can do it from outside the collector
//!
//! The latch is one process-wide flag, so it can be tripped through any
//! `G1CardTable` — here, a throwaway one built over a local buffer holding
//! sixteen bytes that are deliberately not a published header. The collector
//! built afterwards reads the same latch.
//!
//! The assertion is then two-sided, and both sides are load-bearing:
//!
//! * `jumps == 0` — the jump really is off, so this is not a test that would
//!   pass with the latch ignored;
//! * the graph is INTACT — the fallback is the linear walk and not a
//!   half-disabled feature. On this workload every live object but the root is
//!   reachable only through a remembered-set edge, so a walk that lost its way
//!   loses objects.

mod g1_w2a_walk_common;

use cratonvm_gc::g1_cards::{G1CardTable, ObjectStart, G1_CARD_BYTES};
use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind};

const ARM: &[(&str, Option<&str>)] = &[
    ("CRATONVM_G1_BLOCK_OFFSETS", Some("1")),
    ("CRATONVM_G1_CARD_CLEAN", Some("1")),
];

/// Name an address that is not a published object header, exactly as a
/// producer that had been changed to dirty a SLOT's card would.
fn trip_the_latch() {
    // Six cards of backing store so a four-card, card-ALIGNED table fits
    // inside it wherever the allocator lands.
    let backing = vec![0u64; 6 * G1_CARD_BYTES / 8];
    let base = ((backing.as_ptr() as usize) + G1_CARD_BYTES - 1) & !(G1_CARD_BYTES - 1);
    let table = G1CardTable::new(base, 4 * G1_CARD_BYTES);
    let addr = base + G1_CARD_BYTES + 128;
    // SAFETY: inside the backing store, 8-aligned.
    let header = unsafe {
        std::ptr::write(
            addr as *mut ObjectHeader,
            ObjectHeader::new(
                ClassId::new(0),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                0,
            ),
        );
        &*(addr as *const ObjectHeader)
    };
    // The one thing that separates a published header from arbitrary bytes.
    header.set_gc_flags(0);
    table.dirty_addr_at_object_start(ObjectStart::of_header(header));
}

#[test]
fn an_unvouched_producer_disarms_the_jump_and_the_walk_still_finds_everything() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        cratonvm_gc::g1_cards::reset_block_offset_enforcement_for_tests();
        trip_the_latch();

        let (_vouches, unvouched, disarmed) =
            cratonvm_gc::g1_cards::block_offset_enforcement_census();
        assert_eq!(unvouched, 1, "the fixture did not reach the vouch");
        assert!(disarmed, "an unvouched producer must disarm the jump");

        // The pause runs with the lever set to `1` and the latch down. Both
        // halves of the result matter.
        assert_eq!(
            g1_w2a_walk_common::run(),
            g1_w2a_walk_common::expected(),
            "the linear fallback lost part of the graph"
        );

        let (jumps, bytes, refused) = cratonvm_gc::g1::block_offset_process_census();
        assert_eq!(
            jumps, 0,
            "the jump fired although the table had been disarmed \
             (bytes_jumped={bytes} refused={refused})"
        );

        cratonvm_gc::g1_cards::reset_block_offset_enforcement_for_tests();
    });
}
