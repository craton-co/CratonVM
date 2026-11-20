use super::*;

/// A slot oracle with a real superclass chain — which
/// `MockNativeContext` cannot provide, because its `superclass_of` returns
/// `None` unconditionally.
struct ChainOracle {
    /// `(class name, superclass index or usize::MAX, own fields as
    /// (absolute slot, name))`
    classes: Vec<(&'static str, usize, Vec<(usize, &'static str)>)>,
}

impl SlotOracle for ChainOracle {
    fn declared_at(&self, class_id: ClassId) -> Vec<FieldMetadata> {
        let idx = class_id.as_u32() as usize;
        self.classes
            .get(idx)
            .map(|(_, _, fields)| {
                fields
                    .iter()
                    .map(|(slot, name)| FieldMetadata {
                        name: (*name).to_string(),
                        descriptor: "I".to_string(),
                        access_flags: 0,
                        slot_index: *slot,
                        declaring_class_id: class_id,
                        is_static: false,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    fn super_of(&self, class_id: ClassId) -> Option<ClassId> {
        let idx = class_id.as_u32() as usize;
        match self.classes.get(idx) {
            Some((_, s, _)) if *s != usize::MAX => Some(ClassId::new(*s as u32)),
            _ => None,
        }
    }
    fn total_width(&self, class_id: ClassId) -> usize {
        let mut cursor = Some(class_id);
        let mut width = 0usize;
        while let Some(cid) = cursor {
            let idx = cid.as_u32() as usize;
            let Some((_, _, fields)) = self.classes.get(idx) else {
                break;
            };
            width += fields.len();
            cursor = self.super_of(cid);
        }
        width
    }
    fn name_of(&self, class_id: ClassId) -> Option<String> {
        self.classes
            .get(class_id.as_u32() as usize)
            .map(|(n, _, _)| (*n).to_string())
    }
}

/// The real JDK 25 `java.nio.DirectByteBuffer` chain, read with `javap -p`
/// against Eclipse Adoptium 25.0.3.9 on this host: superclass first,
/// declaration order within a class, `static` excluded.
///
/// `Buffer` — mark, position, limit, capacity, address, segment.
/// `ByteBuffer` — hb, offset, isReadOnly, bigEndian, nativeByteOrder.
/// `MappedByteBuffer` — fd, isSync. `DirectByteBuffer` — cleaner, att.
fn direct_byte_buffer_chain() -> (ChainOracle, ClassId) {
    let oracle = ChainOracle {
        classes: vec![
            ("java/lang/Object", usize::MAX, vec![]),
            (
                "java/nio/Buffer",
                0,
                vec![
                    (0, "mark"),
                    (1, "position"),
                    (2, "limit"),
                    (3, "capacity"),
                    (4, "address"),
                    (5, "segment"),
                ],
            ),
            (
                "java/nio/ByteBuffer",
                1,
                vec![
                    (6, "hb"),
                    (7, "offset"),
                    (8, "isReadOnly"),
                    (9, "bigEndian"),
                    (10, "nativeByteOrder"),
                ],
            ),
            (
                "java/nio/MappedByteBuffer",
                2,
                vec![(11, "fd"), (12, "isSync")],
            ),
            (
                "java/nio/DirectByteBuffer",
                3,
                vec![(13, "cleaner"), (14, "att")],
            ),
        ],
    };
    (oracle, ClassId::new(4))
}

/// THE CALIBRATION CASE. `bb_state`'s pre-2026-08-12 body read slot 0 of a
/// real `java.nio.DirectByteBuffer` and called it `hb`. If the instrument
/// does not name `mark` here, the widening did not work and every census
/// this module produces is unsupported.
///
/// The finding must be `WrongField`, **not** `SlotAbsent`: `mark` is
/// declared four classes up the chain, so an oracle that fails to walk
/// `superclass_of` answers "slot 0 does not exist" — quieter, and wrong.
#[test]
fn bb_state_reading_slot_0_of_a_real_direct_byte_buffer_as_hb_is_flagged() {
    let (oracle, dbb) = direct_byte_buffer_chain();
    let answer = field_name_at(&oracle, dbb, 0);
    assert_eq!(
        answer,
        SlotAnswer::Named("mark".to_string()),
        "slot 0 of a real DirectByteBuffer is java.nio.Buffer.mark; an oracle that \
         does not walk the superclass chain answers Absent here and the whole \
         instrument goes quiet on exactly the defect it was built for"
    );
    assert_eq!(
        classify_read("hb", &answer),
        Some(ReadFinding::WrongField {
            actual: "mark".to_string()
        })
    );
}

/// The other half of the historical comment: it said the fallback slot was
/// "the real-JDK HeapByteBuffer slot (`hb` @ 5)". On JDK 25, 5 is
/// `Buffer.segment` and `hb` is 6 — wrong twice over.
#[test]
fn the_historical_hb_at_5_comment_is_flagged_too() {
    let (oracle, dbb) = direct_byte_buffer_chain();
    assert_eq!(
        classify_read("hb", &field_name_at(&oracle, dbb, 5)),
        Some(ReadFinding::WrongField {
            actual: "segment".to_string()
        })
    );
    // And the corrected index is clean, so the instrument is not simply
    // "always fires" — the vacuous shape this project keeps re-buying.
    assert_eq!(classify_read("hb", &field_name_at(&oracle, dbb, 6)), None);
}

/// CratonVM's own `java/nio/ByteBuffer` slot map, from
/// `native-io/src/lib.rs`'s `BB_FIELD_*` constants, checked slot by slot
/// against the real layout. This is the first census row and it is
/// derived, not asserted by hand.
///
/// The name says three and the assertion says two, and both are right: the
/// third disagreeing slot is 6, which `bb_resolve_heap_offset` reads and
/// which the PUBLISHED map did not carry at all until W7-76 added it. It
/// has its own test immediately below. The six replicated here are the six
/// the map held when this was written; the name is left alone because it
/// is the trail to W7-76-bytebuffer-alias-residuals.md, where the count is
/// reconciled and the published map is made complete.
#[test]
fn the_cratonvm_byte_buffer_slot_map_disagrees_at_three_of_six_slots() {
    let (oracle, dbb) = direct_byte_buffer_chain();
    // (slot, what native-io believes is there)
    let craton: &[(usize, &str)] = &[
        (0, "hb"),       // BB_FIELD_ARRAY
        (1, "position"), // BB_FIELD_POS
        (2, "limit"),    // BB_FIELD_LIMIT
        (3, "capacity"), // BB_FIELD_CAPACITY
        (4, "mark"),     // BB_FIELD_MARK
        (5, "segment"),  // BB_SEGMENT_SLOT — correct, and for the stated reason
    ];
    let wrong: Vec<(usize, ReadFinding)> = craton
        .iter()
        .filter_map(|(slot, field)| {
            classify_read(field, &field_name_at(&oracle, dbb, *slot)).map(|f| (*slot, f))
        })
        .collect();
    assert_eq!(
        wrong,
        vec![
            (
                0,
                ReadFinding::WrongField {
                    actual: "mark".to_string()
                }
            ),
            (
                4,
                ReadFinding::WrongField {
                    actual: "address".to_string()
                }
            ),
        ],
        "slots 1, 2, 3 agree by accident of declaration order and slot 5 agrees on \
         purpose; 0 and 4 are the two the ByteBuffer lane found by hand"
    );
}

/// `bb_resolve_heap_offset`'s slot-6 fallback expects `offset`. On the real
/// layout 6 is `hb` — an array reference where an `Int` is read.
#[test]
fn the_heap_offset_fallback_slot_names_hb_not_offset() {
    let (oracle, dbb) = direct_byte_buffer_chain();
    assert_eq!(
        classify_read("offset", &field_name_at(&oracle, dbb, 6)),
        Some(ReadFinding::WrongField {
            actual: "hb".to_string()
        })
    );
}

/// Past the end of the class is a different finding from the wrong field,
/// and must stay different: the out-of-range one is the half the
/// `gc::guard` out-of-bounds reads can already see.
#[test]
fn a_slot_past_the_class_is_absent_not_wrong() {
    let (oracle, dbb) = direct_byte_buffer_chain();
    assert_eq!(
        field_name_at(&oracle, dbb, 99),
        SlotAnswer::Absent { width: 15 }
    );
    assert_eq!(
        classify_read("whatever", &SlotAnswer::Absent { width: 15 }),
        Some(ReadFinding::SlotAbsent { declared_width: 15 })
    );
}

/// A class with no field metadata is UNMEASURED, not clean — the same
/// discipline `layout_alias` applies to `declared == 0`. Reporting it would
/// make every not-yet-loaded class a finding, which is precisely how the
/// registration-time design fails.
#[test]
fn an_unloaded_class_is_unknown_and_never_reported() {
    let oracle = ChainOracle {
        classes: vec![("java/nio/DirectByteBuffer", usize::MAX, vec![])],
    };
    assert_eq!(
        field_name_at(&oracle, ClassId::new(0), 0),
        SlotAnswer::Unknown
    );
    assert_eq!(classify_read("hb", &SlotAnswer::Unknown), None);
}

/// The agreement case answers `None`, so a clean slot map is silent.
#[test]
fn agreement_is_not_a_finding() {
    let (oracle, dbb) = direct_byte_buffer_chain();
    for (slot, name) in [
        (1, "position"),
        (2, "limit"),
        (3, "capacity"),
        (13, "cleaner"),
    ] {
        assert_eq!(
            classify_read(name, &field_name_at(&oracle, dbb, slot)),
            None,
            "slot {slot} really is {name}"
        );
    }
}

/// A published map is idempotent by pointer and readable back, so the sweep
/// cannot double-count a registrar that runs twice.
#[test]
fn declaring_a_slot_map_twice_publishes_it_once() {
    static MAP: SlotMap = SlotMap {
        class: "java/nio/ByteBuffer",
        slots: &[(0, "hb")],
        origin: "read_alias unit test",
    };
    declare_slot_map(&MAP);
    declare_slot_map(&MAP);
    let count = declared_slot_maps()
        .iter()
        .filter(|m| std::ptr::eq(**m, &MAP))
        .count();
    assert_eq!(count, 1);
}
