// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-4 lane A — the block-offset table's LOST-UPDATE hazard, in the shape
//! that produces it.
//!
//! # The hazard
//!
//! A card is 512 bytes and the objects these workloads allocate are tens of
//! bytes, so a card holds ten or twenty object starts. The block-offset table
//! keeps one byte per card, and the source walk enters the card's object grid
//! at whatever that byte names. If two objects start in one card and both hold
//! a reference into the collection set, the byte must name the LOWER of them;
//! naming the higher one does not make the walk over-scan, it makes it step
//! over an object that holds a live cross-region reference, and the slot is
//! then left pointing into a region the pause freed.
//!
//! Naming the lower one is not what a byte does by default. Two producers write
//! it at different times and a plain store leaves whichever ran LAST — so the
//! table is correct under one store order and silently wrong under the other.
//! That is why `G1CardTable::dirty_addr_at_object_start` merges with an atomic
//! minimum rather than a store, and this file is the end-to-end evidence that
//! the merge is the one that ships.
//!
//! # The shape, and why each part of it
//!
//! * **Small holders, so several of them share a card.** 8 reference slots is
//!   about 80 bytes, so a 512-byte card holds five or six; with enough holders
//!   a card with two or more of them is a certainty rather than a hope, and the
//!   assertion at the end says how many the run actually found.
//! * **Tenured**, at `promotion_age: 2`. The source walk is reached only for a
//!   holder the pause does not evacuate. A holder still in Survivor is in the
//!   next collection set, is evacuated, and has its slots walked by the closure
//!   — so a fixture whose holders never tenure passes with the whole walk
//!   deleted. `g1_w2a_walk_common` was written this way first and passed its
//!   own mutant; the region-type assertion below is the same tripwire.
//! * **Stores issued in ASCENDING ADDRESS ORDER.** This is the load-bearing
//!   detail. It puts the HIGHEST offset of each card last, which is exactly the
//!   order under which a last-writer-wins byte leaves the wrong answer. A
//!   fixture that stored in allocation order would exercise the hazard only by
//!   luck.
//! * **The young children are reachable only through the holders' rset edges**,
//!   so an object the walk steps over is lost rather than merely rescanned
//!   later, and the read-back at the end detects it.
//!
//! # Mutation-checked
//!
//! Replacing the guarded `fetch_min` in `dirty_addr_at_object_start` with a
//! plain `store` fails this test — see
//! `docs/feature-designs/concurrent-gc-maturation.md`. That is the whole
//! reason the file exists: the unit test
//! `a_card_with_two_named_object_starts_is_entered_at_the_lower_one` states the
//! rule about the table, and this one states the consequence for a pause.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::region::RegionType;
use cratonvm_gc::G1Collector;
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Slot 0 is the young child; slot 1 is the holder's own id, so a holder that
/// survived can be identified after it has been copied.
const CHILD: usize = 0;
const ID: usize = 1;
const FIELDS: usize = 8;

/// Enough holders to fill many cards several times over. The interesting number
/// is not this one but `sharing` at the end, which counts the cards that
/// actually ended up with two or more holders in them.
const HOLDERS: usize = 3000;

/// Must match `G1_CARD_BYTES`. Not imported, because `gc::g1_cards`'s constants
/// are not part of the crate's public surface and this test only needs to bin
/// addresses — a wrong value here makes the `sharing` assertion weaker, never
/// the correctness assertion wrong.
const CARD_BYTES: usize = 512;



fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        promotion_age: 2,
        ..Default::default()
    }
}


