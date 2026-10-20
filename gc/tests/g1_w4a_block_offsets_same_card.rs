// SPDX-License-Identifier: Apache-2.0
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
//! `docs/internal/g1-2026-09-20/w4a-block-offset-table.md`. That is the whole
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

const ARM: &[(&str, Option<&str>)] = &[
    ("CRATONVM_G1_BLOCK_OFFSETS", Some("1")),
    // See `g1_w4a_block_offsets_parallel`: without cleaning, a tenured region's
    // cards only ever accumulate and there is no clean run to jump over.
    ("CRATONVM_G1_CARD_CLEAN", Some("1")),
    // THE LAYOUT PIN. Not a tuning knob — see "WHAT THIS WORKLOAD DOES NOT
    // PROMISE: A JUMP COUNT" in `g1_w2a_walk_common`, which carries the
    // measurements.
    //
    // The short version: the jump count this fixture produces is a property of
    // which Old region each promoted holder lands in, and that is decided by
    // which evacuation WORKER copied it — work stealing, i.e. the OS
    // scheduler. At the default worker count the count is multi-modal under
    // load ({2, 6, 8}) where it reads a flat 8 on an idle host, and the low
    // mode reaches zero. Pinned to one worker it is 6, in 30 runs out of 30,
    // under a synthetic load average of ~49.
    //
    // `=1` rather than `CRATONVM_G1_PARALLEL_EVAC=0`: this keeps the PARALLEL
    // evacuator — its TLAB pool, its shared-destination bump, its disarmed
    // per-object screens — and removes only the interleaving between workers.
    // `g1_w4a_block_offsets_serial` pins the other one, and that pair is the
    // serial/parallel contrast those two files exist for; collapsing both onto
    // the serial arm would delete it.
    //
    // What this does NOT weaken: `entries_refused`, `unvouched` and
    // `violations` are properties of the producer rule rather than of the
    // layout. They read 0 on every arm, every worker count and every heap size
    // measured, and they are what this file is about.
    //
    // LATENT rather than measured, on the same footing as
    // `g1_w4a_block_offsets_parallel`: 25 runs out of 25 passed at load ~46 on
    // 2026-09-21. Pinned because the quantity is the same one and its floor is
    // zero, not because a failure was seen here.
    ("CRATONVM_G1_WORKERS", Some("1")),
];

fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        promotion_age: 2,
        ..Default::default()
    }
}

#[test]
fn two_holders_in_one_card_both_keep_their_young_child() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        let gc = G1Collector::new(config());

        let root = gc.alloc_object(ClassId::new(42), HOLDERS);
        for h in 0..HOLDERS {
            let holder = gc.alloc_object(ClassId::new(41), FIELDS);
            gc.set_field(holder, ID, Value::Int(h as i32));
            gc.set_field(root, h, Value::Object(Some(holder)));
        }
        let mut roots = vec![root];

        // Age the holders into Old. From here a store into one is a
        // cross-region store, which is what dirties its card and records its
        // start in the block-offset table.
        for _ in 0..6 {
            gc.young_collection(&mut roots, &NoopMonitors);
        }
        let root = roots[0];
        assert!(
            gc.count_regions(RegionType::Old) > 0,
            "no holder tenured, so nothing below reaches the remembered-set \
             source walk -- check config()'s promotion_age against the default"
        );

        // Re-read the holders at their POST-EVACUATION addresses. Those are the
        // addresses the card table will see, and the only ones worth sorting.
        let mut holders: Vec<(usize, ObjectRef)> = (0..HOLDERS)
            .map(|h| match gc.get_field(root, h) {
                Value::Object(Some(o)) => (h, o),
                _ => panic!("holder {h} was lost while ageing"),
            })
            .collect();
        holders.sort_by_key(|&(_, o)| o.as_ptr() as usize);

        // How many cards hold two or more of them? This is the fixture's own
        // engagement count: with none, the file is testing nothing and says so
        // rather than passing.
        let mut sharing = 0usize;
        let mut run = 1usize;
        for w in holders.windows(2) {
            let a = w[0].1.as_ptr() as usize / CARD_BYTES;
            let b = w[1].1.as_ptr() as usize / CARD_BYTES;
            if a == b {
                run += 1;
            } else {
                if run > 1 {
                    sharing += 1;
                }
                run = 1;
            }
        }
        if run > 1 {
            sharing += 1;
        }
        assert!(
            sharing > 50,
            "only {sharing} cards hold more than one holder, so this fixture \
             does not pose the lost-update question it exists to pose"
        );

        // TWO ROUNDS OF STORES, AND ONLY EVERY EIGHTH PAIR OF HOLDERS TAKES
        // ONE. Both halves of that sentence are load-bearing and neither is
        // obvious, so:
        //
        // ONLY SOME HOLDERS, because the jump only fires over a run of CLEAN
        // cards. A fixture that stores into every holder dirties every card
        // they sit in, the walk never has a clean run to hop over, the
        // block-offset entry is never consulted, and the test then passes with
        // the table saying anything at all — which is exactly what the first
        // version of this file did, and it passed the mutant. Storing into two
        // ADJACENT holders and then skipping six leaves a dirty card with two
        // named object starts in it, surrounded by clean cards that give the
        // walk a reason to jump INTO it rather than arrive by walking.
        //
        // TWO ROUNDS, because card cleaning is what makes those cards clean. A
        // G1 card goes clean in exactly one other place, `G1Region::reset`,
        // which never runs for a tenured holder region; so the first round's
        // pause is what establishes the clean runs and the second round's is
        // the one where the jump has something to do.
        //
        // The store order inside a pair is ASCENDING ADDRESS ORDER, which is
        // the point of the whole file: it puts the higher of the two holders
        // last, which is the order under which a last-writer-wins byte leaves
        // the card's entry above the lower holder and the walk steps over it.
        let stored: Vec<(usize, ObjectRef)> = holders
            .iter()
            .enumerate()
            .filter(|(rank, _)| rank % 8 < 2)
            .map(|(_, &e)| e)
            .collect();
        assert!(
            stored.len() > 100,
            "only {} holders take a store; the fixture is too small to pose the \
             question",
            stored.len()
        );

        let mut roots = vec![root];
        for round in 0..2 {
            for &(h, holder) in &stored {
                let child = gc.alloc_object(ClassId::new(43), 2);
                gc.set_field(
                    child,
                    ID,
                    Value::Int(1_000_000 + round * 100_000 + h as i32),
                );
                gc.set_field(holder, CHILD, Value::Object(Some(child)));
            }
            // Garbage, so the pause has a real Eden to reclaim and the
            // collection set is not trivially empty.
            for _ in 0..8000 {
                let junk = gc.alloc_object(ClassId::new(44), 2);
                gc.set_field(junk, 0, Value::Int(0));
            }
            gc.young_collection(&mut roots, &NoopMonitors);
        }
        let root = roots[0];

        // THE LEVER'S OWN ENGAGEMENT COUNT, asserted rather than assumed.
        // Without this the file passes identically with the jump never taken,
        // which is the failure `orchestrator-wave-1-measurements.md` §7.3 names
        // as the common cause of this round's three retracted measurements: the
        // experiment did not check that it was exercising the thing it named.
        let (jumps, bytes, refused) = gc.block_offset_census();
        assert!(
            jumps > 0,
            "the block-offset jump never fired, so this run says nothing about \
             it (bytes_jumped={bytes} refused={refused})"
        );
        assert_eq!(
            refused, 0,
            "a candidate entry failed its header screen on a healthy heap -- \
             the block-offset table named an address that is not an object"
        );

        // Read every child back. A holder the source walk stepped over kept a
        // slot pointing at a young object the pause moved and whose region it
        // freed, so this is where a lost edge surfaces.
        let want: std::collections::HashMap<usize, i32> = stored
            .iter()
            .map(|&(h, _)| (h, 1_100_000 + h as i32))
            .collect();
        let mut checked = 0usize;
        for h in 0..HOLDERS {
            let Value::Object(Some(holder)) = gc.get_field(root, h) else {
                panic!("holder {h} was lost by the pause");
            };
            assert_eq!(
                gc.get_field(holder, ID).as_int(),
                Some(h as i32),
                "holder {h} did not survive intact"
            );
            // Only the holders that took a store have a child to lose; the rest
            // are here to keep their neighbours' cards clean.
            let Some(&id) = want.get(&h) else {
                continue;
            };
            checked += 1;
            let Value::Object(Some(child)) = gc.get_field(holder, CHILD) else {
                panic!(
                    "holder {h} lost the young child that was reachable only \
                     through its remembered-set edge -- the source walk was \
                     entered ABOVE this holder"
                );
            };
            assert_eq!(
                gc.get_field(child, ID).as_int(),
                Some(id),
                "holder {h}'s young child was not evacuated intact"
            );
        }
        assert_eq!(
            checked,
            stored.len(),
            "the read-back did not reach every holder that took a store"
        );
    });
}
