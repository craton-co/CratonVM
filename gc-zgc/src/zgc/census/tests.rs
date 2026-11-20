use super::*;

/// Seed the access census with `slots` observations of `shape`, `non_null`
/// of which held a reference, by driving the recorder that SHIPS.
///
/// This replaced `ZCensus::record_access_batch`, which existed only to make
/// these setups terse and had no production caller — see the note where it
/// was deleted. A loop over `record_access` records the same totals (the
/// batch form's own doc said so), and the setup now exercises the code the
/// collector runs.
fn record_access_n(c: &ZSlotCensus, shape: ZSlotShape, slots: u64, non_null: u64) {
    let non_null = non_null.min(slots);
    for i in 0..slots {
        c.record_access(shape, i < non_null);
    }
}

/// **No test in this module asserts an elapsed duration.** Fixed wall-clock
/// bounds are a latent CI flake on a loaded shared host, and this repo has a
/// standing note about exactly that. Every assertion below is on a count, a
/// ratio, a classification or a column arithmetic.
///
/// A view built from an explicit object list. Owns no memory the census
/// could dereference — the census never dereferences anything, and this
/// fake proves it by handing out addresses that are not valid pointers.
struct FakeView {
    objects: Vec<(u64, ClassId, ZCensusObjectKind, Vec<ZSlotObservation>)>,
    statics: Vec<(ClassId, ZSlotObservation)>,
    /// When `Some`, `is_compact` answers this instead of deriving it from
    /// the kind — used to exercise the disagreement counter.
    force_is_compact: Option<bool>,
}

impl FakeView {
    fn new() -> FakeView {
        FakeView {
            objects: Vec::new(),
            statics: Vec::new(),
            force_is_compact: None,
        }
    }

    fn push(
        &mut self,
        addr: u64,
        class_id: u32,
        kind: ZCensusObjectKind,
        slots: Vec<ZSlotObservation>,
    ) {
        self.objects
            .push((addr, ClassId::new(class_id), kind, slots));
    }
}

impl ZCensusHeapView for FakeView {
    fn for_each_live_object(&self, f: &mut dyn FnMut(u64, ClassId, ZCensusObjectKind)) {
        for (addr, class_id, kind, _) in self.objects.iter() {
            f(*addr, *class_id, *kind);
        }
    }

    fn is_compact(&self, addr: u64) -> bool {
        if let Some(forced) = self.force_is_compact {
            return forced;
        }
        self.objects
            .iter()
            .find(|(a, _, _, _)| *a == addr)
            .map(|(_, _, kind, _)| *kind == ZCensusObjectKind::CompactInstance)
            .unwrap_or(false)
    }

    fn reference_slots(&self, addr: u64, f: &mut dyn FnMut(ZSlotObservation)) {
        if let Some((_, _, _, slots)) = self.objects.iter().find(|(a, _, _, _)| *a == addr) {
            for obs in slots.iter() {
                f(*obs);
            }
        }
    }

    fn static_reference_slots(&self, f: &mut dyn FnMut(ClassId, ZSlotObservation)) {
        for (class_id, obs) in self.statics.iter() {
            f(*class_id, *obs);
        }
    }
}

fn census() -> ZSlotCensus {
    let c = ZSlotCensus::new();
    c.enable();
    c
}

// -- the shape table ---------------------------------------------------

#[test]
fn every_shape_has_a_distinct_index_key_and_label() {
    assert_eq!(ZSlotShape::ALL.len(), Z_SLOT_SHAPE_COUNT);
    let mut seen_keys: Vec<&str> = Vec::new();
    let mut seen_labels: Vec<&str> = Vec::new();
    for (i, shape) in ZSlotShape::ALL.iter().enumerate() {
        assert_eq!(shape.index(), i, "{shape:?} index must match ALL position");
        let key = shape.key();
        let label = shape.label();
        assert!(!key.is_empty());
        assert!(!label.is_empty());
        assert!(!seen_keys.contains(&key), "duplicate key {key:?}");
        assert!(!seen_labels.contains(&label), "duplicate label {label:?}");
        // TSV column names are built from `key`; a tab or a space in one
        // would corrupt the header.
        assert!(
            !key.contains('\t') && !key.contains(' '),
            "TSV key {key:?} must be a bare snake_case token",
        );
        seen_keys.push(key);
        seen_labels.push(label);
        assert_eq!(ZSlotShape::from_key(key), Some(*shape));
    }
    assert_eq!(ZSlotShape::from_key("not-a-shape"), None);
}

#[test]
fn the_shape_table_matches_the_study() {
    // Study §0's table, pinned — and this is the change the previous
    // version of this comment predicted: "if a future change makes
    // reference array elements atomic (blast-radius items 1-2), this test
    // is where that fact gets recorded". It did (2026-09-01,
    // `narrow_oop::write_ref_slot`), and recording it here is what stops
    // the doc and the code diverging silently a second time.
    //
    // `LegacyField` moved the OTHER way at the same time, and for a reason
    // that has nothing to do with a new change: the row was simply wrong.
    // It described the mutator-side `read_value_atomic` /
    // `write_value_atomic` pair and never asked what the COLLECTOR side
    // does, which is a plain `ptr::write::<Value>` of the whole 16-byte
    // cell. See `word_is_atomically_accessed_today` for the citations.
    assert!(ZSlotShape::CompactField.word_is_atomically_accessed_today());
    assert!(ZSlotShape::ArrayElement.word_is_atomically_accessed_today());
    // zt t32 (the successor page's section 4): both cell shapes cleared
    // their collector-side debt; the list is empty.
    assert!(ZSlotShape::LegacyField.word_is_atomically_accessed_today());
    assert!(ZSlotShape::StaticField.word_is_atomically_accessed_today());

    // The boolean and the citation must move together. Updating one alone
    // is precisely how this table came to be right about one row and wrong
    // about another, which is the state a reader cannot detect.
    for shape in ZSlotShape::ALL {
        assert_eq!(
            shape.atomicity_debt_note().is_none(),
            shape.word_is_atomically_accessed_today(),
            "atomicity_debt_note and word_is_atomically_accessed_today \
             disagree for {}",
            shape.key(),
        );
    }

    // Study §1.7: only the 16-byte-cell shapes carry a tag to gate on.
    assert!(ZSlotShape::LegacyField.needs_tag_gate());
    assert!(ZSlotShape::StaticField.needs_tag_gate());
    assert!(!ZSlotShape::CompactField.needs_tag_gate());
    assert!(!ZSlotShape::ArrayElement.needs_tag_gate());

    // Statics are outside the collector's registry entirely.
    assert!(!ZSlotShape::StaticField.reachable_from_heap_walk());
    for shape in ZSlotShape::ALL {
        if shape != ZSlotShape::StaticField {
            assert!(shape.reachable_from_heap_walk(), "{shape:?}");
        }
    }

    // The thresholds are barrier-cost thresholds; only the dynamic arm
    // measures barrier cost.
    assert!(ZCensusStrategy::AccessPath.thresholds_apply());
    assert!(!ZCensusStrategy::HeapWalk.thresholds_apply());
}

#[test]
fn object_kind_maps_onto_exactly_one_slot_shape() {
    assert_eq!(
        ZCensusObjectKind::CompactInstance.slot_shape(),
        Some(ZSlotShape::CompactField),
    );
    assert_eq!(
        ZCensusObjectKind::LegacyInstance.slot_shape(),
        Some(ZSlotShape::LegacyField),
    );
    assert_eq!(
        ZCensusObjectKind::ReferenceArray.slot_shape(),
        Some(ZSlotShape::ArrayElement),
    );
    assert_eq!(ZCensusObjectKind::NoReferenceSlots.slot_shape(), None);
}

// -- classification of all four shapes ---------------------------------

#[test]
fn the_walk_classifies_each_of_the_four_shapes() {
    let c = census();
    let mut view = FakeView::new();

    // 1. compact instance: two bare reference words, one null.
    view.push(
        0x1000,
        10,
        ZCensusObjectKind::CompactInstance,
        vec![
            ZSlotObservation::bare(ZSlotShape::CompactField, 0x1010, 0xAAAA),
            ZSlotObservation::bare(ZSlotShape::CompactField, 0x1018, 0),
        ],
    );

    // 2. legacy instance: one written reference cell, one unwritten cell,
    //    one genuinely primitive cell. Only the first is a reference slot.
    view.push(
        0x2000,
        20,
        ZCensusObjectKind::LegacyInstance,
        vec![
            ZSlotObservation::tagged(ZSlotShape::LegacyField, 0x2018, 0xBBBB, VALUE_TAG_OBJECT),
            ZSlotObservation::tagged(ZSlotShape::LegacyField, 0x2028, 0, VALUE_TAG_INT),
            ZSlotObservation::tagged(ZSlotShape::LegacyField, 0x2038, 7, 1),
        ],
    );

    // 3. reference array: three elements, two non-null.
    view.push(
        0x3000,
        30,
        ZCensusObjectKind::ReferenceArray,
        vec![
            ZSlotObservation::bare(ZSlotShape::ArrayElement, 0x3010, 0xC1),
            ZSlotObservation::bare(ZSlotShape::ArrayElement, 0x3018, 0),
            ZSlotObservation::bare(ZSlotShape::ArrayElement, 0x3020, 0xC2),
        ],
    );

    // 4. primitive array: no reference slots at all.
    view.push(0x4000, 40, ZCensusObjectKind::NoReferenceSlots, vec![]);

    // 5. statics: one written, one unwritten.
    view.statics.push((
        ClassId::new(50),
        ZSlotObservation::tagged(ZSlotShape::StaticField, 0x5008, 0xD1, VALUE_TAG_OBJECT),
    ));
    view.statics.push((
        ClassId::new(50),
        ZSlotObservation::tagged(ZSlotShape::StaticField, 0x5018, 0, VALUE_TAG_INT),
    ));

    let walk = c.run_walk(&view).expect("enabled census must walk");
    let t = &walk.totals;

    assert_eq!(t.source, ZCensusStrategy::HeapWalk);
    assert_eq!(t.slots[ZSlotShape::CompactField.index()], 2);
    assert_eq!(t.non_null_slots[ZSlotShape::CompactField.index()], 1);
    assert_eq!(t.slots[ZSlotShape::LegacyField.index()], 1);
    assert_eq!(t.non_null_slots[ZSlotShape::LegacyField.index()], 1);
    assert_eq!(t.slots[ZSlotShape::ArrayElement.index()], 3);
    assert_eq!(t.non_null_slots[ZSlotShape::ArrayElement.index()], 2);
    assert_eq!(t.slots[ZSlotShape::StaticField.index()], 1);
    assert_eq!(t.non_null_slots[ZSlotShape::StaticField.index()], 1);

    // The two non-reference cell populations are kept apart from the
    // reference total (study §1.7) — one unwritten from the instance and
    // one from the statics.
    assert_eq!(t.legacy_unwritten_slots, 2);
    assert_eq!(t.legacy_primitive_slots, 1);

    assert_eq!(t.objects[ZSlotShape::CompactField.index()], 1);
    assert_eq!(t.objects[ZSlotShape::LegacyField.index()], 1);
    assert_eq!(t.objects[ZSlotShape::ArrayElement.index()], 1);
    assert_eq!(t.objects[ZSlotShape::StaticField.index()], 1); // one block
    assert_eq!(t.objects_without_ref_slots, 1);

    assert_eq!(t.total_slots(), 2 + 1 + 3 + 1);
    assert_eq!(walk.statics_blocks, 1);
    assert_eq!(walk.kind_disagreements, 0);
    assert_eq!(walk.class_table_overflow_objects, 0);
    assert_eq!(walk.class_table_overflow_slots, 0);
    assert_eq!(c.walks_completed(), 1);
}

#[test]
fn an_unwritten_legacy_cell_is_not_counted_as_a_reference_slot() {
    // Study §1.7 is the whole reason: object bodies are handed out
    // zero-filled, so an unwritten reference field has tag 0, and counting
    // it as a reference slot would inflate the legacy share with slots that
    // hold nothing.
    let c = census();
    let mut view = FakeView::new();
    view.push(
        0x1000,
        1,
        ZCensusObjectKind::LegacyInstance,
        (0..8u64)
            .map(|i| {
                ZSlotObservation::tagged(
                    ZSlotShape::LegacyField,
                    0x1000 + i * 16 + 8,
                    0,
                    VALUE_TAG_INT,
                )
            })
            .collect(),
    );
    let walk = c.run_walk(&view).unwrap();
    assert_eq!(walk.totals.slots[ZSlotShape::LegacyField.index()], 0);
    assert_eq!(walk.totals.legacy_unwritten_slots, 8);
    assert_eq!(walk.totals.total_slots(), 0);
    assert_eq!(walk.totals.verdict(), ZCensusVerdict::NoData);
}

#[test]
fn observation_helpers_agree_with_the_classifier() {
    let bare = ZSlotObservation::bare(ZSlotShape::ArrayElement, 0x10, 0x99);
    assert!(bare.is_reference_slot());
    assert!(bare.is_non_null());

    let null_bare = ZSlotObservation::bare(ZSlotShape::CompactField, 0x18, 0);
    assert!(null_bare.is_reference_slot());
    assert!(!null_bare.is_non_null());

    let obj = ZSlotObservation::tagged(ZSlotShape::LegacyField, 0x28, 0x77, VALUE_TAG_OBJECT);
    assert!(obj.is_reference_slot());
    assert!(obj.is_non_null());

    let unwritten = ZSlotObservation::tagged(ZSlotShape::LegacyField, 0x38, 0, VALUE_TAG_INT);
    assert!(!unwritten.is_reference_slot());
    assert!(!unwritten.is_non_null());

    let prim = ZSlotObservation::tagged(ZSlotShape::LegacyField, 0x48, 5, 1);
    assert!(!prim.is_reference_slot());
    assert!(!prim.is_non_null());
}

// -- percentages, including the all-zero case --------------------------

#[test]
fn shares_are_zero_not_nan_when_nothing_has_been_observed() {
    let c = ZSlotCensus::new();
    for strategy in [ZCensusStrategy::HeapWalk, ZCensusStrategy::AccessPath] {
        let t = c.totals(strategy);
        assert_eq!(t.total_slots(), 0);
        for shape in ZSlotShape::ALL {
            assert_eq!(t.share(shape), 0.0, "{shape:?}");
            assert!(t.share(shape).is_finite(), "{shape:?}");
        }
        assert_eq!(t.legacy_share(), 0.0);
        assert!(t.legacy_share().is_finite());
        assert!(t.tagged_cell_share().is_finite());
        assert!(t.non_atomic_share().is_finite());
        // And the all-zero case must NOT read as "legacy is negligible" —
        // that would silently answer the study's question with no data.
        assert_eq!(t.verdict(), ZCensusVerdict::NoData);
    }
    assert!(share_of(0, 0).is_finite());
    assert_eq!(share_of(0, 0), 0.0);
    assert_eq!(share_of(1, 4), 0.25);
}

#[test]
fn shares_sum_to_one_and_match_the_counts() {
    let c = census();
    let mut view = FakeView::new();
    // 1 compact + 3 legacy = 4 reference slots.
    view.push(
        0x1000,
        1,
        ZCensusObjectKind::CompactInstance,
        vec![ZSlotObservation::bare(ZSlotShape::CompactField, 0x1010, 1)],
    );
    view.push(
        0x2000,
        2,
        ZCensusObjectKind::LegacyInstance,
        (0..3u64)
            .map(|i| {
                ZSlotObservation::tagged(
                    ZSlotShape::LegacyField,
                    0x2000 + i * 16 + 8,
                    1,
                    VALUE_TAG_OBJECT,
                )
            })
            .collect(),
    );
    let t = c.run_walk(&view).unwrap().totals;
    assert_eq!(t.total_slots(), 4);
    assert_eq!(t.share(ZSlotShape::CompactField), 0.25);
    assert_eq!(t.share(ZSlotShape::LegacyField), 0.75);
    assert_eq!(t.share(ZSlotShape::ArrayElement), 0.0);
    let sum: f64 = ZSlotShape::ALL.iter().map(|s| t.share(*s)).sum();
    assert!((sum - 1.0).abs() < 1e-12, "shares must sum to 1, got {sum}");
    assert_eq!(t.legacy_share(), 0.75);
    assert_eq!(t.tagged_cell_share(), 0.75);
    // The non-atomic set is EMPTY since zt t32 (see
    // `ZSlotShape::word_is_atomically_accessed_today`). It was `LegacyField`
    // + `StaticField` from 2026-09-01 (this line read `0.75`: three legacy
    // slots out of four) and `ArrayElement` + `StaticField` before (`0.0`)
    // -- the expectation moved with the table, not with the fixture, which
    // is unchanged.
    assert_eq!(t.non_atomic_share(), 0.0);
}

#[test]
fn the_verdict_follows_the_documented_decision_rule() {
    let make = |compact: u64, legacy: u64| {
        let mut t = ZShapeTotals::empty(ZCensusStrategy::AccessPath);
        t.slots[ZSlotShape::CompactField.index()] = compact;
        t.slots[ZSlotShape::LegacyField.index()] = legacy;
        t
    };
    // 1 / 1000 = 0.001 < 0.05
    assert_eq!(make(999, 1).verdict(), ZCensusVerdict::LegacyNegligible);
    // exactly 0.05 is NOT below the threshold -> material, not negligible.
    assert_eq!(make(95, 5).verdict(), ZCensusVerdict::LegacyMaterial);
    // exactly 0.25 is NOT above the threshold -> still material.
    assert_eq!(make(75, 25).verdict(), ZCensusVerdict::LegacyMaterial);
    // 0.26 > 0.25
    assert_eq!(make(74, 26).verdict(), ZCensusVerdict::LegacyDominant);
    assert_eq!(make(0, 0).verdict(), ZCensusVerdict::NoData);

    // Each verdict says what it commits to, and the keys are bare tokens
    // (they are TSV values).
    let mut seen: Vec<&str> = Vec::new();
    for v in [
        ZCensusVerdict::NoData,
        ZCensusVerdict::LegacyNegligible,
        ZCensusVerdict::LegacyMaterial,
        ZCensusVerdict::LegacyDominant,
    ] {
        assert!(!v.implication().is_empty());
        let key = v.key();
        assert!(!key.contains('\t') && !key.contains(' '), "{key:?}");
        assert!(!seen.contains(&key));
        seen.push(key);
    }
}

// -- the access path ---------------------------------------------------

#[test]
fn the_access_path_records_only_when_its_own_gate_is_on() {
    let c = census(); // master gate on, access sampling OFF
    assert!(c.is_enabled());
    assert!(!c.access_sampling_enabled());
    c.record_access(ZSlotShape::LegacyField, true);
    record_access_n(&c, ZSlotShape::CompactField, 100, 50);
    assert_eq!(c.access_totals().total_slots(), 0, "the second gate is off");

    c.enable_access_sampling();
    assert!(c.access_sampling_enabled());
    c.record_access(ZSlotShape::LegacyField, true);
    c.record_access(ZSlotShape::LegacyField, false);
    record_access_n(&c, ZSlotShape::CompactField, 100, 50);
    let t = c.access_totals();
    assert_eq!(t.slots[ZSlotShape::LegacyField.index()], 2);
    assert_eq!(t.non_null_slots[ZSlotShape::LegacyField.index()], 1);
    assert_eq!(t.slots[ZSlotShape::CompactField.index()], 100);
    assert_eq!(t.non_null_slots[ZSlotShape::CompactField.index()], 50);
    assert_eq!(t.total_slots(), 102);
    // An access is a slot event: it knows nothing about objects.
    assert_eq!(t.objects, [0; Z_SLOT_SHAPE_COUNT]);

    // A batch cannot report more non-null than slots.
    record_access_n(&c, ZSlotShape::ArrayElement, 4, 9);
    let t = c.access_totals();
    assert_eq!(t.slots[ZSlotShape::ArrayElement.index()], 4);
    assert_eq!(t.non_null_slots[ZSlotShape::ArrayElement.index()], 4);
}

#[test]
fn the_two_strategies_do_not_share_counters() {
    // The whole point of having both is that they can disagree; if they
    // shared storage the disagreement would be invisible.
    let c = census();
    c.enable_access_sampling();
    let mut view = FakeView::new();
    view.push(
        0x1000,
        1,
        ZCensusObjectKind::CompactInstance,
        vec![ZSlotObservation::bare(ZSlotShape::CompactField, 0x1010, 1)],
    );
    c.run_walk(&view).unwrap();
    record_access_n(&c, ZSlotShape::LegacyField, 1_000, 1_000);

    assert_eq!(
        c.walk_totals().legacy_share(),
        0.0,
        "nothing legacy is live"
    );
    assert_eq!(
        c.access_totals().legacy_share(),
        1.0,
        "every read is legacy"
    );
    assert_eq!(
        c.walk_totals().verdict(),
        ZCensusVerdict::LegacyNegligible,
        "the live-set answer",
    );
    assert_eq!(
        c.access_totals().verdict(),
        ZCensusVerdict::LegacyDominant,
        "the barrier-cost answer — this is the disagreement the module exists to expose",
    );
}

// -- the gate ----------------------------------------------------------

#[test]
fn a_disabled_census_records_nothing_at_all() {
    let c = ZSlotCensus::new();
    assert!(!c.is_enabled());
    // Even with the second gate flipped on, the master gate wins.
    c.enable_access_sampling();
    assert!(!c.access_sampling_enabled());
    c.record_access(ZSlotShape::CompactField, true);
    record_access_n(&c, ZSlotShape::LegacyField, 1_000, 1_000);

    let mut view = FakeView::new();
    view.push(
        0x1000,
        1,
        ZCensusObjectKind::CompactInstance,
        vec![ZSlotObservation::bare(ZSlotShape::CompactField, 0x1010, 1)],
    );
    assert!(c.run_walk(&view).is_none(), "a disabled walk must not run");

    assert_eq!(c.walks_completed(), 0);
    assert_eq!(c.walk_totals().total_slots(), 0);
    assert_eq!(c.access_totals().total_slots(), 0);
    assert!(c.top_classes(10).is_empty());
    assert_eq!(c.class_table_overflow(), (0, 0));

    // Enabling makes the same calls land, so the zeroes above were the gate
    // and not a broken fake.
    c.enable();
    assert!(c.access_sampling_enabled());
    c.record_access(ZSlotShape::CompactField, true);
    assert_eq!(c.access_totals().total_slots(), 1);
    assert!(c.run_walk(&view).is_some());
    assert_eq!(c.walks_completed(), 1);

    // And disabling stops it again.
    c.disable();
    c.record_access(ZSlotShape::CompactField, true);
    assert_eq!(c.access_totals().total_slots(), 1);
    assert!(c.run_walk(&view).is_none());
    assert_eq!(c.walks_completed(), 1);
}

// -- the bounded per-class table ---------------------------------------

#[test]
fn the_per_class_table_is_bounded_under_many_distinct_class_ids() {
    let c = census();
    let mut view = FakeView::new();
    let extra: u32 = 500;
    let total = CLASS_TABLE_CAP as u32 + extra;
    for i in 0..total {
        view.push(
            0x1_0000 + u64::from(i) * 0x40,
            i,
            ZCensusObjectKind::LegacyInstance,
            vec![ZSlotObservation::tagged(
                ZSlotShape::LegacyField,
                0x1_0000 + u64::from(i) * 0x40 + 8,
                1,
                VALUE_TAG_OBJECT,
            )],
        );
    }
    let walk = c.run_walk(&view).unwrap();

    // Every slot is still in the TOTALS — the cap costs attribution, never
    // the headline number.
    assert_eq!(
        walk.totals.slots[ZSlotShape::LegacyField.index()],
        u64::from(total),
    );
    assert_eq!(walk.totals.legacy_share(), 1.0);

    // The table itself is bounded, twice: at CLASS_TABLE_CAP while walking
    // and at PUBLISHED_CLASS_ROWS when published.
    assert_eq!(walk.distinct_class_ids, CLASS_TABLE_CAP as u64);
    assert_eq!(walk.classes.len(), PUBLISHED_CLASS_ROWS);
    assert!(c.top_classes(usize::MAX).len() <= PUBLISHED_CLASS_ROWS);
    assert_eq!(c.top_classes(3).len(), 3);

    // And the loss is COUNTED, not silent.
    assert_eq!(walk.class_table_overflow_objects, u64::from(extra));
    assert_eq!(walk.class_table_overflow_slots, u64::from(extra));
    assert_eq!(
        c.class_table_overflow(),
        (u64::from(extra), u64::from(extra)),
    );
    let summary = c.format_summary();
    assert!(summary.contains("OVERFLOWED"), "{summary}");
}

#[test]
fn the_class_table_ranks_by_legacy_slots_and_is_deterministic() {
    let c = census();
    let mut view = FakeView::new();
    let legacy = |addr: u64, class_id: u32, n: u64| {
        (
            addr,
            class_id,
            (0..n)
                .map(|i| {
                    ZSlotObservation::tagged(
                        ZSlotShape::LegacyField,
                        addr + i * 16 + 8,
                        1,
                        VALUE_TAG_OBJECT,
                    )
                })
                .collect::<Vec<ZSlotObservation>>(),
        )
    };
    for (addr, class_id, slots) in [
        legacy(0x1000, 7, 3),
        legacy(0x2000, 9, 11),
        legacy(0x3000, 5, 11),
    ] {
        view.push(addr, class_id, ZCensusObjectKind::LegacyInstance, slots);
    }
    // A compact class with many slots must NOT outrank a legacy one.
    view.push(
        0x4000,
        2,
        ZCensusObjectKind::CompactInstance,
        (0..50u64)
            .map(|i| ZSlotObservation::bare(ZSlotShape::CompactField, 0x4000 + i * 8, 1))
            .collect(),
    );

    let walk = c.run_walk(&view).unwrap();
    let ids: Vec<u32> = walk.classes.iter().map(|r| r.class_id.as_u32()).collect();
    // 11 legacy slots each for 5 and 9 -> tie broken by ascending ClassId;
    // then 3 legacy slots for 7; then the compact class with zero legacy.
    assert_eq!(
        ids,
        vec![5, 9, 7, 2],
        "ranking must be legacy-first and total-ordered"
    );
    assert_eq!(walk.classes[0].legacy_slots(), 11);
    assert_eq!(walk.classes[3].legacy_slots(), 0);
    assert_eq!(walk.classes[3].total_slots(), 50);
    assert_eq!(walk.classes[3].objects, 1);
}

#[test]
fn a_walk_publishes_a_gauge_while_the_counters_accumulate() {
    // The live set is a gauge; summing walks would weight a long-lived
    // object by its lifetime and answer a third question nobody asked.
    let c = census();
    let mut view = FakeView::new();
    view.push(
        0x1000,
        1,
        ZCensusObjectKind::CompactInstance,
        vec![ZSlotObservation::bare(ZSlotShape::CompactField, 0x1010, 1)],
    );
    let first = c.run_walk(&view).unwrap();
    let second = c.run_walk(&view).unwrap();
    assert_eq!(first.totals.total_slots(), 1);
    assert_eq!(second.totals.total_slots(), 1, "each walk is a fresh gauge");
    assert_eq!(c.walk_totals().total_slots(), 2, "the counters accumulate");
    assert_eq!(c.walks_completed(), 2);

    c.reset_for_test();
    assert_eq!(c.walk_totals().total_slots(), 0);
    assert_eq!(c.walks_completed(), 0);
    assert!(c.top_classes(10).is_empty());
}

// -- the is_compact cross-check ----------------------------------------

#[test]
fn a_disagreement_between_is_compact_and_the_kind_is_counted_and_shouted() {
    let c = census();
    let mut view = FakeView::new();
    view.push(
        0x1000,
        1,
        ZCensusObjectKind::LegacyInstance,
        vec![ZSlotObservation::tagged(
            ZSlotShape::LegacyField,
            0x1008,
            1,
            VALUE_TAG_OBJECT,
        )],
    );
    view.force_is_compact = Some(true); // lie
    let walk = c.run_walk(&view).unwrap();
    assert_eq!(walk.kind_disagreements, 1);
    assert_eq!(c.kind_disagreements(), 1);
    let summary = c.format_summary();
    assert!(
        summary.contains("VOID"),
        "a void run must say so: {summary}"
    );
}

// -- statics wiring ----------------------------------------------------

#[test]
fn an_unwired_static_column_reads_not_wired_rather_than_zero() {
    // "Silence is not zero": a structurally-unreachable column that prints
    // 0.0000 is indistinguishable from a measurement of none.
    let c = census();
    assert!(!c.statics_wired());
    let summary = c.format_summary();
    assert!(summary.contains("NOT WIRED"), "{summary}");
    assert!(summary.contains("statics_wired=false"), "{summary}");

    c.set_statics_wired(true);
    assert!(c.statics_wired());
    let summary = c.format_summary();
    assert!(!summary.contains("NOT WIRED"), "{summary}");
}

#[test]
fn a_census_with_no_walk_says_so_instead_of_printing_an_empty_table() {
    let c = census();
    let summary = c.format_summary();
    assert!(summary.contains("classes: NONE"), "{summary}");
    assert!(
        summary.contains("not a finding"),
        "an empty table must not read as a measurement: {summary}",
    );
    assert!(summary.contains("VERDICT=no_data"), "{summary}");
}

#[test]
fn the_summary_labels_which_verdict_the_thresholds_apply_to() {
    // Reading the walk's verdict against the study's barrier-cost
    // thresholds is the single most likely misuse of this instrument, so
    // each block says which one it is.
    let c = census();
    let summary = c.format_summary();
    assert!(
        summary.contains("the study's 5%/25% barrier-cost thresholds apply to THIS number"),
        "{summary}",
    );
    assert!(summary.contains("indicative only"), "{summary}");
    assert!(summary.contains("zgc-census walk"), "{summary}");
    assert!(summary.contains("zgc-census access"), "{summary}");
}

// -- the TSV -----------------------------------------------------------

#[test]
fn tsv_header_and_row_have_equal_column_counts() {
    // A header/row mismatch shifts every value one column left or right and
    // silently corrupts every downstream aggregation without erroring
    // anywhere. This is the assertion that stops it — the same guard
    // `ZgcMetrics` carries, for the same reason.
    let c = census();
    let header = ZSlotCensus::tsv_header();
    let row = c.to_tsv_row();
    let header_cols: Vec<&str> = header.split('\t').collect();
    let row_cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(
        header_cols.len(),
        row_cols.len(),
        "TSV header has {} columns but the row has {}",
        header_cols.len(),
        row_cols.len(),
    );
    // 6 fixed + 4 per shape (walk) + 5 walk extras + 3 per shape (access)
    // + 1 access extra + 2 verdicts + 3 per top class
    // + the appended live-set gauge: 1 `last_walk_ran` + 2 per shape
    //   + 1 total + 1 verdict + 1 `walk_statics_blocks`.
    let expected = 6
        + 4 * Z_SLOT_SHAPE_COUNT
        + 5
        + 3 * Z_SLOT_SHAPE_COUNT
        + 1
        + 2
        + 3 * TSV_TOP_CLASSES
        + 1
        + 2 * Z_SLOT_SHAPE_COUNT
        + 3;
    assert_eq!(header_cols.len(), expected);
    assert_eq!(header_cols.len(), ZSlotCensus::tsv_column_names().len());
    assert_eq!(header_cols[0], "run");

    // The same must still hold once every counter is populated, which is
    // the state the column count is most likely to drift in.
    c.enable_access_sampling();
    c.set_statics_wired(true);
    record_access_n(&c, ZSlotShape::LegacyField, 5, 4);
    let mut view = FakeView::new();
    for i in 0..10u32 {
        view.push(
            0x1000 + u64::from(i) * 0x40,
            i,
            ZCensusObjectKind::LegacyInstance,
            vec![ZSlotObservation::tagged(
                ZSlotShape::LegacyField,
                0x1008 + u64::from(i) * 0x40,
                1,
                VALUE_TAG_OBJECT,
            )],
        );
    }
    c.run_walk(&view).unwrap();
    let row = c.to_tsv_row();
    assert_eq!(row.split('\t').count(), expected);
    assert!(!row.contains('\n'), "a row must be one line");
}

#[test]
fn tsv_column_names_are_unique_and_tab_free() {
    let cols = ZSlotCensus::tsv_column_names();
    let mut seen: Vec<&str> = Vec::new();
    for col in cols.iter() {
        assert!(!col.is_empty());
        assert!(!col.contains('\t') && !col.contains(' '), "{col:?}");
        let s: &str = col.as_str();
        assert!(!seen.contains(&s), "duplicate TSV column {col:?}");
        seen.push(s);
    }
}

#[test]
fn the_run_label_cannot_break_the_row() {
    let c = census();
    c.set_run_label("zgc\treal\nspring/Bean\rTests");
    assert_eq!(c.run_label(), "zgc_real_spring/Bean_Tests");
    let row = c.to_tsv_row();
    assert_eq!(
        row.split('\t').count(),
        ZSlotCensus::tsv_column_names().len(),
    );
    assert!(!row.contains('\n') && !row.contains('\r'));
}

#[test]
fn unfilled_top_class_columns_are_placeholders_not_missing_columns() {
    let c = census();
    let mut view = FakeView::new();
    view.push(
        0x1000,
        77,
        ZCensusObjectKind::LegacyInstance,
        vec![ZSlotObservation::tagged(
            ZSlotShape::LegacyField,
            0x1008,
            1,
            VALUE_TAG_OBJECT,
        )],
    );
    c.run_walk(&view).unwrap();
    let header = ZSlotCensus::tsv_header();
    let row = c.to_tsv_row();
    let names: Vec<&str> = header.split('\t').collect();
    let values: Vec<&str> = row.split('\t').collect();
    let idx = names
        .iter()
        .position(|n| *n == "top1_class_id")
        .expect("top1_class_id column");
    assert_eq!(values[idx], "77");
    let idx2 = names
        .iter()
        .position(|n| *n == "top2_class_id")
        .expect("top2_class_id column");
    assert_eq!(values[idx2], "-1", "an unfilled slot is a placeholder");
    assert_eq!(values.len(), names.len());
}

#[test]
fn the_verdict_columns_carry_bare_tokens() {
    let c = census();
    c.enable_access_sampling();
    record_access_n(&c, ZSlotShape::LegacyField, 1_000, 1_000);
    let header = ZSlotCensus::tsv_header();
    let row = c.to_tsv_row();
    let names: Vec<&str> = header.split('\t').collect();
    let values: Vec<&str> = row.split('\t').collect();
    let idx = names
        .iter()
        .position(|n| *n == "access_verdict")
        .expect("access_verdict column");
    assert_eq!(values[idx], ZCensusVerdict::LegacyDominant.key());
    let widx = names
        .iter()
        .position(|n| *n == "walk_verdict")
        .expect("walk_verdict column");
    assert_eq!(values[widx], ZCensusVerdict::NoData.key());
}
