// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-7 lane O — **what shape of heap the block-offset jump pays on**, as an
//! experiment rather than as an argument.
//!
//! # The question, and why `bytes_jumped` cannot answer it
//!
//! `CRATONVM_G1_BLOCK_OFFSETS` ships OFF. Its headline number is a byte count
//! — 2.59 GB of sized-and-stepped-over bytes becoming 10.4 MB (W4-A), 5.21 GB
//! becoming 35.7 MB (W5-B §6.1) — and the open item behind this file is that
//! nobody had established which heaps have the shape those numbers came from.
//! Three reps of `G1ChurnPauseProbe` read **30, 84 and 92** jumps and three of
//! `HumongousRealChurn` read **0, 0 and 22**, which is not a settled answer in
//! either direction.
//!
//! The work the jump removes is the walk's LINEAR STEP: one header read, one
//! `candidate_header_is_plausible` and one `object_total_size` **per object**
//! it would otherwise have stepped over. So the payoff is denominated in
//! OBJECTS SKIPPED, and `bytes_jumped` is that number multiplied by a mean
//! object size the counter does not report. On a heap of small objects the two
//! move together; on a heap of large ones they move in OPPOSITE directions,
//! because fewer, fatter objects leave longer clean runs (more bytes) holding
//! fewer object starts (less work).
//!
//! That is a falsifiable claim about a workload property, and this file is the
//! experiment for it.
//!
//! # The instrument, which already existed and was being read for something else
//!
//! `CRATONVM_G1_BLOCK_OFFSET_AUDIT=1` walks every span a jump skipped and
//! counts the objects STARTING in it — which is, exactly, the objects the jump
//! removed from the walk. W5-B built it as a correctness instrument for a
//! producer naming the wrong object; at period 1 it is also the only
//! measurement of the jump's payoff in the right units. Nothing had read
//! `audited` as a performance number.
//!
//! So each arm below reports `objs_jumped = audited` beside `bytes_jumped`,
//! and the prediction is that they disagree.
//!
//! # The six arms
//!
//! ONE process, SIX heaps, IDENTICAL flags. Nothing about the collector's
//! configuration changes between arms, so a difference between them cannot be
//! a lever resolving differently (the failure
//! `orchestrator-wave-1-measurements.md` §7.1 retracted a measurement for).
//!
//! Arms 1-4 vary the retained object's SIZE at a fixed retained byte budget.
//! Arms 5-6 return to arm 1's heap and vary the DIRTY-CARD density instead,
//! because those are the two terms and confounding them is how this question
//! stayed open.
//!
//! | arm | retained object | count | holder stride | holders move? |
//! |---|---|---|---|---|
//! | `small` | 6-slot reference array (~64 B) | 65 536 | 512 | no |
//! | `medium` | 62-slot reference array (~512 B) | 8 192 | 64 | no |
//! | `large` | 8 190-slot reference array (~64 KiB) | 64 | 8 | no |
//! | `humongous` | 131 070-slot reference array (~1 MiB) | 4 | 1 | no |
//! | `small-rot` | as `small` | 65 536 | 512 | yes |
//! | `small-dense` | as `small` | 65 536 | 8 | yes |
//!
//! The holder stride is declared per arm rather than derived, and that is a
//! correction to this file's first version. Deriving it as `count / 128` put
//! the `large` arm at a stride of 1 — every object a holder, every object's
//! start card dirty, no clean run anywhere — so that arm read zero for a
//! reason that had nothing to do with its object size. A zero with two
//! sufficient causes is not a measurement. At a stride of 8 the `large` arm
//! has clean runs of seven objects and reads a small non-zero number, which is
//! the reading the account predicts and the one that can be compared.
//!
//! The last arm is the interesting one and it is the reason `HumongousRealChurn`
//! reads zero. A humongous span holds exactly ONE object and it starts at the
//! span's base; every producer dirties the START card of the object it
//! records, so the span's only dirty card is card 0 and the only block-offset
//! entry in it names the base. The jump's gate is `entry > here`, and at the
//! start of the walk `here` IS the base — so a humongous source region cannot
//! take a jump, ever, on any workload. `jumps == 0` there is a THEOREM about
//! the region shape, not a reading about this heap.
//!
//! # What it came out at
//!
//! Quoted so a reader knows what the assertions below are protecting.
//! `objs/jump` is `audited / audit_spans` and is the one derived number that
//! decides the lever: it is how many linear steps one jump removes, and its
//! break-even is 1.
//!
//! | arm | dirty | jumps | bytes/jump | objs_jumped | objs/jump |
//! |---|---|---|---|---|---|
//! | `small` | 143 | 1 368 | 32 261 | **683 436** | 499.6 |
//! | `medium` | 130 | 1 500 | 31 748 | 93 012 | 62.0 |
//! | `large` | 9 | 84 | 290 231 | 372 | 4.4 |
//! | `humongous` | 5 | 0 | — | **0** | — |
//! | `small-rot` | 1 039 | 8 524 | 5 132 | 677 388 | 79.5 |
//! | `small-dense` | 8 207 | 12 | 32 784 | **12** | 1.0 |
//!
//! Twelve reps: four arms bit-identical every time, `small` alternating
//! 1368/683436 with 1356/677388 and `small-rot` 8536/679852 with
//! 8524/677388 — under 1% either way, and `objs/jump` stable to 0.02%.
//!
//! Three readings, and the second was a surprise:
//!
//! 1. `objs/jump` is `bytes_per_jump ÷ mean object size`, to three figures on
//!    every arm that took a jump. The payoff is the object GRID.
//! 2. **Saturating the card table does not cost payoff, it fragments it.**
//!    `small-rot` has seven times `small`'s dirty cards, six times the jumps,
//!    one sixth the bytes per jump — and the same payoff to within 0.9%.
//!    Extra dirty cards do not remove skipped objects; they cut the same
//!    skipped run into more, shorter jumps. This file's first version asserted
//!    a decay here and the measurement refused it.
//! 3. It collapses only when the dirty cards' neighbourhoods MEET.
//!    `small-dense` puts a holder in every card and the payoff falls 57 000x,
//!    to exactly one object per jump — the break-even.
//!
//! So `bytes_jumped` misses reading 1 (1.8x in bytes across a 1 837x payoff
//! range) and `bytes_per_jump` misses reading 2 in the opposite direction
//! (6.3x in bytes-per-jump across a 1.009x payoff range). Neither published
//! counter is the payoff; the object count is, and `CRATONVM_G1_BLOCK_OFFSET_AUDIT=1`
//! already reports it.
//!
//! # What is asserted, and what is only printed
//!
//! Asserted: the ordering (objects skipped falls as object size rises), and
//! `humongous` taking zero jumps. Those are the claims. The absolute counts
//! are PRINTED rather than pinned, because they depend on the young-sizing
//! controller's choices and this file must not become a test that fails when a
//! sizing constant moves. `docs/internal/g1-2026-09-20/w7o-*.md` quotes a run
//! of it.
//!
//! # House rules this file is standing on
//!
//! * `block_offset_audit_census` is PROCESS-GLOBAL, so every arm asserts a
//!   DELTA across its own run and never an absolute (round rule 8).
//!   `block_offset_census` is per collector and is read directly.
//! * One `#[test]`, four arms inside it, run sequentially. Separate `#[test]`
//!   functions would run on separate threads in one process and interleave
//!   into the same global counters.
//! * No `debug_assert!` anywhere near this: the arms run real pauses.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Identical for every arm. See the module doc: the independent variable is
/// the heap's object grid, not the collector.
const ARM: &[(&str, Option<&str>)] = &[
    ("CRATONVM_G1_BLOCK_OFFSETS", Some("1")),
    // Period 1. The audit is the instrument here, not a sample of it: at
    // period 1 `audited` is the EXACT count of objects the jumps removed from
    // the walk, which is the quantity this file is about. It costs precisely
    // the work the jump saved, which is the right price for a measurement and
    // the wrong one for production — see `lane_a_block_offset_audit`.
    ("CRATONVM_G1_BLOCK_OFFSET_AUDIT", Some("1")),
];

/// Retained bytes per arm. Four regions' worth at the 1 MiB region size below,
/// so every arm has several source regions and none of them is a single-region
/// special case.
const RETAINED_BYTES: usize = 4 * 1024 * 1024;

/// Reference elements per chunk of the root spine. Small enough that the spine
/// itself is never humongous and so never becomes one of the shapes under
/// test.
const CHUNK: usize = 4096;

/// Pauses per arm. Twelve rather than a handful, because the per-pause series
/// is the instrument for the SECOND variable — the card table's saturation —
/// and a four-pause run cannot show a trend.
const PAUSES: usize = 12;

fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 96 * 1024 * 1024,
        initial_heap_size: 96 * 1024 * 1024,
        region_size: 1024 * 1024,
        // See `g1_w2a_walk_common`: at the default of 15 nothing tenures in a
        // handful of pauses, every holder stays in a Survivor region, every
        // Survivor region is in the next CSet — and the remembered-set source
        // walk, which is the only thing this file measures, is never asked
        // anything at all.
        promotion_age: 2,
        ..Default::default()
    }
}

/// One arm's reading.
struct Arm {
    name: &'static str,
    /// Reference slots in each retained object.
    slots: usize,
    /// How many of them.
    count: usize,
    /// One in this many takes the young store that makes its region a source.
    stride: usize,
    /// Does the holder subset MOVE between pauses?
    rotating: bool,
    jumps: u64,
    bytes_jumped: u64,
    /// Objects the audit walked inside the skipped spans — i.e. the objects
    /// the jump removed from the linear walk. THE payoff number.
    objs_jumped: u64,
    refused: u64,
    violations: u64,
    card_line: String,
    /// `objs_jumped` on each pause in turn. The SECOND variable: G1's card
    /// table is additive and a card is cleared in exactly one place
    /// (`G1Region::reset`), so a long-lived source region's dirty set only
    /// grows. The clean runs the jump skips are what shrinks, and this is the
    /// series that shows it happening.
    per_pause: Vec<u64>,
}

/// Build a heap of `count` reference arrays of `slots` elements, tenure them,
/// hang a fresh young object off a fixed sparse subset each pause, and collect.
///
/// Returns the arm's census. Every retained object is read back afterwards, so
/// an edge the walk drops fails the run rather than scoring it (round rule 6,
/// in its in-process form).
fn measure(name: &'static str, slots: usize, count: usize, stride: usize, rotating: bool) -> Arm {
    let gc = G1Collector::new(config());

    // The root spine: an object whose fields point at chunk arrays, which
    // point at the retained objects. Deliberately NOT one flat array of
    // `count` references — at `count = 65_536` that array is 512 KiB and would
    // itself be humongous, adding a fifth shape to a four-arm experiment.
    let chunks = count.div_ceil(CHUNK);
    let root = gc.alloc_object(ClassId::new(71), chunks);
    let mut spine = Vec::with_capacity(chunks);
    for c in 0..chunks {
        let chunk = gc.alloc_array(ClassId::new(72), ArrayElementType::Reference, CHUNK);
        gc.set_field(root, c, Value::Object(Some(chunk)));
        spine.push(chunk);
    }
    for i in 0..count {
        let obj = gc.alloc_array(ClassId::new(73), ArrayElementType::Reference, slots);
        gc.set_array_element(spine[i / CHUNK], i % CHUNK, Value::Object(Some(obj)))
            .expect("spine store");
    }

    let mut roots = vec![root];
    for _ in 0..6 {
        gc.young_collection(&mut roots, &NoopMonitors);
    }
    let root = roots[0];
    assert!(
        gc.count_regions(cratonvm_gc::region::RegionType::Old) > 0,
        "{name}: nothing tenured, so nothing below reaches the remembered-set \
         source walk — check `config()`'s promotion_age"
    );

    // THE FIXED SPARSE HOLDER SUBSET, and it is load-bearing twice.
    //
    // Sparse, because the jump only has something to skip when most cards are
    // clean; and FIXED, because G1's card table is additive — a probe that
    // rotated which object it stored into would saturate every card within a
    // few dozen pauses and the arms would differ by how fast they saturated
    // rather than by their object grid. This is `G1BlockOffsetProbe`'s own
    // rule, and it is what lets the arms run at the DEFAULT card-cleaning
    // setting instead of needing a second lever to show one effect.
    //
    // The stride is the CALLER's, not derived from `count` — see the module
    // doc's correction. A derived stride bottoms out at 1 on the arms with
    // few objects, which makes every object a holder and gives those arms a
    // zero that says nothing about their object size.
    //
    // ROTATION is the second variable. A fixed subset keeps the dirty set
    // constant, so the arms differ by their object grid and nothing else; a
    // rotating one adds fresh dirty cards every pause, which is what a real
    // long-running old generation does and what G1's additive card table
    // cannot undo without `CRATONVM_G1_CARD_CLEAN`. The rotation step is eight
    // cards' worth of small objects, so a rotated store lands on a card the
    // previous pause did not dirty rather than on the same one.
    let rotate_by = if rotating { 64 } else { 0 };
    let holders_at = |pause: usize| -> Vec<usize> {
        (0..count)
            .step_by(stride)
            .map(|h| (h + pause * rotate_by) % count)
            .collect()
    };

    let before = cratonvm_gc::g1::block_offset_audit_census();
    let mut per_pause = Vec::with_capacity(PAUSES);
    let mut prev_objs = before.1;

    for pause in 0..PAUSES {
        let root = roots[0];
        let holders = holders_at(pause);
        for (k, &h) in holders.iter().enumerate() {
            let Value::Object(Some(chunk)) = gc.get_field(root, h / CHUNK) else {
                panic!(
                    "{name}: spine chunk {} lost before pause {pause}",
                    h / CHUNK
                );
            };
            let Ok(Value::Object(Some(obj))) = gc.get_array_element(chunk, h % CHUNK) else {
                panic!("{name}: retained object {h} lost before pause {pause}");
            };
            let child = gc.alloc_object(ClassId::new(74), 2);
            gc.set_field(child, 1, Value::Int((pause * 1_000_000 + k) as i32));
            // Slot 0 of the retained object. The store is the only thing that
            // makes its region a remembered-set source, and the child is
            // reachable by no other route — so an edge the walk drops is a
            // read-back failure below, not a slow pause.
            gc.set_array_element(obj, 0, Value::Object(Some(child)))
                .expect("holder store");
        }
        // Garbage, so the pause has a real Eden to reclaim.
        for _ in 0..3000 {
            let junk = gc.alloc_object(ClassId::new(75), 2);
            gc.set_field(junk, 0, Value::Int(0));
        }
        gc.young_collection(&mut roots, &NoopMonitors);
        let now = cratonvm_gc::g1::block_offset_audit_census().1;
        per_pause.push(now - prev_objs);
        prev_objs = now;

        // Read every edge back. This is the checksum: a jump that entered a
        // region above an object does not crash, it silently drops that
        // object's remembered-set edge.
        let root = roots[0];
        for (k, &h) in holders.iter().enumerate() {
            let Value::Object(Some(chunk)) = gc.get_field(root, h / CHUNK) else {
                panic!("{name}: spine chunk {} lost by pause {pause}", h / CHUNK);
            };
            let Ok(Value::Object(Some(obj))) = gc.get_array_element(chunk, h % CHUNK) else {
                panic!("{name}: retained object {h} lost by pause {pause}");
            };
            let Ok(Value::Object(Some(child))) = gc.get_array_element(obj, 0) else {
                panic!(
                    "{name}: retained object {h} lost the young child reachable \
                     ONLY through its remembered-set edge, on pause {pause}"
                );
            };
            assert_eq!(
                gc.get_field(child, 1).as_int(),
                Some((pause * 1_000_000 + k) as i32),
                "{name}: object {h}'s child was not evacuated intact on pause {pause}"
            );
        }
    }

    let after = cratonvm_gc::g1::block_offset_audit_census();
    let (jumps, bytes_jumped, refused) = gc.block_offset_census();
    Arm {
        name,
        slots,
        count,
        stride,
        rotating,
        jumps,
        bytes_jumped,
        // DELTA, not absolute: these three are process-global statics shared
        // with every other arm in this binary (round rule 8).
        objs_jumped: after.1 - before.1,
        refused,
        violations: after.2 - before.2,
        card_line: gc.card_table_census_line(),
        per_pause,
    }
}

#[test]
fn the_jump_pays_in_objects_skipped_and_a_humongous_region_can_never_take_one() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        cratonvm_gc::g1_cards::reset_block_offset_enforcement_for_tests();

        // Four heaps, one retained byte budget, descending object count — plus
        // a fifth that repeats the first with the holder subset ROTATING, so
        // the two variables are separated rather than confounded.
        let plan: [(&str, usize, usize, bool); 6] = [
            ("small", 6, 512, false),
            ("medium", 62, 64, false),
            ("large", 8190, 8, false),
            ("humongous", 131_070, 1, false),
            ("small-rot", 6, 512, true),
            ("small-dense", 6, 8, true),
        ];
        let arms: Vec<Arm> = plan
            .iter()
            .map(|&(name, slots, stride, rotating)| {
                // 16-byte header plus 8 bytes a reference slot, which is the
                // arithmetic the table in the module doc is quoting. It does
                // not have to be exact: it sets the retained BUDGET, and the
                // measured counts are printed beside every reading.
                let bytes_each = 16 + 8 * slots;
                let count = (RETAINED_BYTES / bytes_each).max(1);
                measure(name, slots, count, stride, rotating)
            })
            .collect();

        eprintln!(
            "[W7-O] arm        slots   count  stride  rot    jumps   bytes_jumped  bytes/jump   \
             objs_jumped  objs/jump   bytes/obj"
        );
        for a in &arms {
            let per_jump = if a.jumps == 0 {
                0.0
            } else {
                a.bytes_jumped as f64 / a.jumps as f64
            };
            let objs_per_jump = if a.jumps == 0 {
                0.0
            } else {
                a.objs_jumped as f64 / a.jumps as f64
            };
            let bytes_per_obj = if a.objs_jumped == 0 {
                0.0
            } else {
                a.bytes_jumped as f64 / a.objs_jumped as f64
            };
            eprintln!(
                "[W7-O] {:<10} {:>5} {:>7} {:>7} {:>4} {:>8} {:>14} {:>11.0} {:>13} {:>10.1} {:>11.0}",
                a.name,
                a.slots,
                a.count,
                a.stride,
                if a.rotating { "yes" } else { "no" },
                a.jumps,
                a.bytes_jumped,
                per_jump,
                a.objs_jumped,
                objs_per_jump,
                bytes_per_obj
            );
            eprintln!("[W7-O]   {}", a.card_line);
            eprintln!("[W7-O]   objs_jumped by pause: {:?}", a.per_pause);
        }

        // The lever was connected on every arm, and the table was sound on
        // every arm. Without these two the table above is decoration.
        for a in &arms {
            assert_eq!(
                a.refused, 0,
                "{}: a candidate entry failed its header screen — the producer \
                 rule is not holding and the reading above means nothing",
                a.name
            );
            assert_eq!(
                a.violations, 0,
                "{}: an object inside a skipped span holds a cross-region \
                 reference — a producer named the wrong object's start",
                a.name
            );
        }

        let small = &arms[0];
        let humongous = &arms[3];

        // THE CONTROL for the arms that read zero. An arm reading
        // `jumps=0 objs_jumped=0` because the jump never engaged anywhere in
        // this binary is a different fact from one reading zero because its
        // heap has nothing to skip, and only the first arm can tell them
        // apart. W6-A §6.5 is this exact distinction, one level down.
        assert!(
            small.jumps > 0 && small.objs_jumped > 0,
            "the jump never engaged on the arm built for it ({} jumps, {} objects) \
             — every zero below is uninterpretable",
            small.jumps,
            small.objs_jumped
        );

        // THE CLAIM, part 1: a humongous source region cannot take a jump.
        //
        // Its span holds one object, that object starts at the span's base,
        // and every card producer dirties the START card of the object it
        // records — so the only entry in the span names the base and the
        // gate's `entry > here` is false at the only boundary the walk starts
        // from. This is why `HumongousRealChurn` reads 0, and it is a fact
        // about the region shape rather than about that probe.
        assert_eq!(
            humongous.jumps, 0,
            "a humongous source region took {} jumps — either the one-object-per-span \
             reasoning in this file's module doc is wrong, or a producer is naming \
             an interior address",
            humongous.jumps
        );

        // THE CLAIM, part 2: the payoff falls with object COUNT, monotonically,
        // and it is not what `bytes_jumped` says.
        //
        // Stated as a strict ordering rather than as ratios: the ratios are in
        // the printed table and in the page, and pinning them here would make
        // this test fail on a young-sizing change that moves the pause count.
        for w in arms[..4].windows(2) {
            assert!(
                w[0].objs_jumped >= w[1].objs_jumped,
                "objects skipped did not fall from `{}` ({} objects of {} slots) to \
                 `{}` ({} objects of {} slots): {} then {}. The payoff is supposed to \
                 track the object GRID, and this is the assertion that says so.",
                w[0].name,
                w[0].count,
                w[0].slots,
                w[1].name,
                w[1].count,
                w[1].slots,
                w[0].objs_jumped,
                w[1].objs_jumped
            );
        }

        // THE CLAIM, part 3, and the one that retires `bytes_jumped` as the
        // headline: between `small` and `medium` the BYTES do not move while
        // the WORK falls several-fold. The jump's length is set by the spacing
        // of the dirty cards, which is the same on both arms by construction;
        // what changes is how many object starts those same bytes contain.
        //
        // Both directions are asserted because either one alone is weak: bytes
        // staying put while work stays put would be no result at all.
        let medium = &arms[1];
        assert!(
            medium.bytes_jumped * 2 > small.bytes_jumped
                && small.bytes_jumped * 2 > medium.bytes_jumped,
            "the two arms were supposed to jump comparable BYTES ({} and {}); if they \
             do not, the work comparison below is not controlled for jump length",
            small.bytes_jumped,
            medium.bytes_jumped
        );
        assert!(
            small.objs_jumped > medium.objs_jumped * 3,
            "the same bytes were supposed to hold several times more object starts on \
             the small-object arm: {} against {}. If this fails, `bytes_jumped` and the \
             payoff track each other after all and this page's account is wrong.",
            small.objs_jumped,
            medium.objs_jumped
        );

        // And the part that makes `bytes_jumped` the wrong headline: the
        // small-object arm removes vastly more WORK than the large-object one
        // while there is no such gap in BYTES. An order of magnitude is a
        // deliberately weak bound on a gap the printed table shows as three.
        let large = &arms[2];
        assert!(
            small.objs_jumped > large.objs_jumped.saturating_mul(10),
            "the small-object arm skipped {} objects and the large-object arm {} — \
             fewer than 10x apart, which is not the separation the object-grid \
             account predicts",
            small.objs_jumped,
            large.objs_jumped
        );

        // THE CLAIM, part 4 — AND THIS FILE'S FIRST VERSION ASSERTED THE
        // OPPOSITE OF WHAT IT MEASURED.
        //
        // The hypothesis was that the payoff DECAYS as G1's additive card
        // table saturates: a card is cleared in exactly one place
        // (`G1Region::reset`), these regions are not being recycled, and
        // `CRATONVM_G1_CARD_CLEAN` is off by default, so a long-lived old
        // generation's dirty set only grows. `small-rot` is `small`'s heap,
        // object for object, with the same number of stores per pause spread
        // over a MOVING subset, so its dirty set grows and `small`'s does not.
        //
        // It does not decay. Seven times the dirty cards, 0.9% less payoff —
        // and the reason is the thing worth taking away from this file: extra
        // dirty cards do not remove skipped objects, they CUT THE SAME SKIPPED
        // RUN INTO MORE, SHORTER JUMPS. `bytes_per_jump` collapses six-fold
        // and `jumps` rises six-fold while `objs_jumped` sits still. The
        // objects the walk still visits are the dirty cards' own
        // neighbourhoods, and those grow with the HOLDER count, not with the
        // card count.
        //
        // So `bytes_per_jump` is not merely an incomplete proxy for the
        // payoff, it moves six-fold against a payoff that does not move.
        //
        // `small-dense` bounds the other end, because "does not decay" cannot
        // be true forever: at a holder every eighth object every card carries
        // a holder, the neighbourhoods cover the region, and there is nothing
        // left between them to skip.
        let rot = &arms[4];
        let dense = &arms[5];
        let first = *small.per_pause.first().expect("a pause series");
        let last = *small.per_pause.last().expect("a pause series");
        assert!(
            last * 2 > first,
            "the FIXED-holder arm's payoff decayed ({first} then {last}) — it is the \
             control for the rotating arm and it is supposed to hold the dirty set \
             constant, so a decay here means the arms are confounded"
        );
        // The rotating arm really did saturate, or it is not the arm it claims
        // to be. This is the control that stops "no decay" from meaning "no
        // treatment" — round rule 8, and the reason the first version of this
        // block was a wrong assertion rather than a wrong measurement.
        assert!(
            rot.jumps > small.jumps * 2,
            "the rotating arm took {} jumps against the fixed arm's {} — it was \
             supposed to fragment the same skipped bytes into many more, shorter \
             jumps, and if it did not then its dirty set did not actually grow",
            rot.jumps,
            small.jumps
        );
        assert!(
            rot.objs_jumped * 10 > small.objs_jumped * 9,
            "the payoff fell from {} to {} when the dirty set grew sevenfold. That is \
             the decay this file's first version predicted and did not find; if it is \
             back, the account above ('extra dirty cards cut the run into more jumps, \
             they do not remove skipped objects') is wrong and the page must say so.",
            small.objs_jumped,
            rot.objs_jumped
        );
        // …and the other end, where the neighbourhoods meet.
        assert!(
            dense.objs_jumped * 10 < small.objs_jumped,
            "the saturated arm skipped {} objects against the sparse arm's {} — the \
             payoff is supposed to collapse once every card carries a holder, and if \
             it does not then dirty-card density is not a second variable at all",
            dense.objs_jumped,
            small.objs_jumped
        );

        let (_, unvouched, disarmed) = cratonvm_gc::g1_cards::block_offset_enforcement_census();
        assert_eq!(unvouched, 0, "a producer named a non-header address");
        assert!(!disarmed, "a clean run must not disarm the jump");

        cratonvm_gc::g1_cards::reset_block_offset_enforcement_for_tests();
    });
}
