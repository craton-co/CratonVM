use super::*;

// Helper: build raw bytes for a StackMapTable
fn build_table(entries_data: &[u8], count: u16) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&count.to_be_bytes());
    data.extend_from_slice(entries_data);
    data
}

// --- Verification type parsing ---

#[test]
fn parse_top_type() {
    let data = [ITEM_TOP];
    let mut pos = 0;
    let vt = parse_verification_type(&data, &mut pos).unwrap();
    assert_eq!(vt, VerificationTypeInfo::Top);
    assert_eq!(pos, 1);
}

#[test]
fn parse_integer_type() {
    let data = [ITEM_INTEGER];
    let mut pos = 0;
    let vt = parse_verification_type(&data, &mut pos).unwrap();
    assert_eq!(vt, VerificationTypeInfo::Integer);
}

#[test]
fn parse_float_type() {
    let data = [ITEM_FLOAT];
    let mut pos = 0;
    let vt = parse_verification_type(&data, &mut pos).unwrap();
    assert_eq!(vt, VerificationTypeInfo::Float);
}

#[test]
fn parse_double_type() {
    let data = [ITEM_DOUBLE];
    let mut pos = 0;
    let vt = parse_verification_type(&data, &mut pos).unwrap();
    assert_eq!(vt, VerificationTypeInfo::Double);
}

#[test]
fn parse_long_type() {
    let data = [ITEM_LONG];
    let mut pos = 0;
    let vt = parse_verification_type(&data, &mut pos).unwrap();
    assert_eq!(vt, VerificationTypeInfo::Long);
}

#[test]
fn parse_null_type() {
    let data = [ITEM_NULL];
    let mut pos = 0;
    let vt = parse_verification_type(&data, &mut pos).unwrap();
    assert_eq!(vt, VerificationTypeInfo::Null);
}

#[test]
fn parse_uninitialized_this_type() {
    let data = [ITEM_UNINITIALIZED_THIS];
    let mut pos = 0;
    let vt = parse_verification_type(&data, &mut pos).unwrap();
    assert_eq!(vt, VerificationTypeInfo::UninitializedThis);
}

#[test]
fn parse_object_type() {
    // tag=7, cpool_index=0x0042
    let data = [ITEM_OBJECT, 0x00, 0x42];
    let mut pos = 0;
    let vt = parse_verification_type(&data, &mut pos).unwrap();
    assert_eq!(vt, VerificationTypeInfo::Object { cpool_index: 0x42 });
    assert_eq!(pos, 3);
}

#[test]
fn parse_uninitialized_type() {
    // tag=8, offset=0x000A
    let data = [ITEM_UNINITIALIZED, 0x00, 0x0A];
    let mut pos = 0;
    let vt = parse_verification_type(&data, &mut pos).unwrap();
    assert_eq!(vt, VerificationTypeInfo::Uninitialized { offset: 10 });
}

#[test]
fn parse_invalid_verification_type_tag() {
    let data = [99]; // invalid tag
    let mut pos = 0;
    assert!(parse_verification_type(&data, &mut pos).is_err());
}

// --- Frame parsing ---

#[test]
fn parse_same_frame() {
    // frame_type=5 → same_frame with offset_delta=5
    let data = build_table(&[5], 1);
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(table.entries.len(), 1);
    assert_eq!(
        table.entries[0],
        StackMapFrame::SameFrame { offset_delta: 5 }
    );
}

#[test]
fn parse_same_frame_zero() {
    let data = build_table(&[0], 1);
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(
        table.entries[0],
        StackMapFrame::SameFrame { offset_delta: 0 }
    );
}

#[test]
fn parse_same_locals_1_stack_item() {
    // frame_type=65 → offset_delta=1, then Integer verification type
    let data = build_table(&[65, ITEM_INTEGER], 1);
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(
        table.entries[0],
        StackMapFrame::SameLocals1StackItem {
            offset_delta: 1,
            stack: VerificationTypeInfo::Integer,
        }
    );
}

#[test]
fn parse_same_locals_1_stack_item_with_object() {
    // frame_type=70 → offset_delta=6, then Object(cpool_index=3)
    let data = build_table(&[70, ITEM_OBJECT, 0x00, 0x03], 1);
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(
        table.entries[0],
        StackMapFrame::SameLocals1StackItem {
            offset_delta: 6,
            stack: VerificationTypeInfo::Object { cpool_index: 3 },
        }
    );
}

#[test]
fn parse_same_locals_1_stack_item_extended() {
    // tag=247, offset_delta=300, stack=Null
    let data = build_table(&[247, 0x01, 0x2C, ITEM_NULL], 1);
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(
        table.entries[0],
        StackMapFrame::SameLocals1StackItemExtended {
            offset_delta: 300,
            stack: VerificationTypeInfo::Null,
        }
    );
}

#[test]
fn parse_chop_frame() {
    // tag=249 → chopped = 251 - 249 = 2, offset_delta=10
    let data = build_table(&[249, 0x00, 0x0A], 1);
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(
        table.entries[0],
        StackMapFrame::ChopFrame {
            offset_delta: 10,
            chopped: 2,
        }
    );
}

#[test]
fn parse_same_frame_extended() {
    // tag=251, offset_delta=500
    let data = build_table(&[251, 0x01, 0xF4], 1);
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(
        table.entries[0],
        StackMapFrame::SameFrameExtended { offset_delta: 500 }
    );
}

#[test]
fn parse_append_frame_1_local() {
    // tag=252 → 1 new local, offset_delta=15, locals=[Integer]
    let data = build_table(&[252, 0x00, 0x0F, ITEM_INTEGER], 1);
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(
        table.entries[0],
        StackMapFrame::AppendFrame {
            offset_delta: 15,
            locals: vec![VerificationTypeInfo::Integer],
        }
    );
}

#[test]
fn parse_append_frame_3_locals() {
    // tag=254 → 3 new locals, offset_delta=20, locals=[Int, Float, Long]
    let data = build_table(&[254, 0x00, 0x14, ITEM_INTEGER, ITEM_FLOAT, ITEM_LONG], 1);
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(
        table.entries[0],
        StackMapFrame::AppendFrame {
            offset_delta: 20,
            locals: vec![
                VerificationTypeInfo::Integer,
                VerificationTypeInfo::Float,
                VerificationTypeInfo::Long,
            ],
        }
    );
}

#[test]
fn parse_full_frame() {
    // tag=255, offset_delta=100
    // 2 locals: Integer, Object(5)
    // 1 stack: Null
    let data = build_table(
        &[
            255,
            0x00,
            0x64, // offset_delta=100
            0x00,
            0x02, // num_locals=2
            ITEM_INTEGER,
            ITEM_OBJECT,
            0x00,
            0x05, // Object(5)
            0x00,
            0x01, // num_stack=1
            ITEM_NULL,
        ],
        1,
    );
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(
        table.entries[0],
        StackMapFrame::FullFrame {
            offset_delta: 100,
            locals: vec![
                VerificationTypeInfo::Integer,
                VerificationTypeInfo::Object { cpool_index: 5 },
            ],
            stack: vec![VerificationTypeInfo::Null],
        }
    );
}

#[test]
fn parse_reserved_tag_rejected() {
    let data = build_table(&[130], 1);
    assert!(StackMapTable::parse(&data).is_err());
}

// --- Multiple frames ---

#[test]
fn parse_multiple_frames() {
    // 3 frames: same_frame(10), same_frame(20), chop_frame(5, chopped=1)
    let data = build_table(
        &[
            10, // same_frame offset_delta=10
            20, // same_frame offset_delta=20
            250, 0x00, 0x05, // chop_frame chopped=1 offset_delta=5
        ],
        3,
    );
    let table = StackMapTable::parse(&data).unwrap();
    assert_eq!(table.entries.len(), 3);
    assert_eq!(
        table.entries[0],
        StackMapFrame::SameFrame { offset_delta: 10 }
    );
    assert_eq!(
        table.entries[1],
        StackMapFrame::SameFrame { offset_delta: 20 }
    );
    assert_eq!(
        table.entries[2],
        StackMapFrame::ChopFrame {
            offset_delta: 5,
            chopped: 1
        }
    );
}

// --- Absolute offsets ---

#[test]
fn absolute_offsets_single_frame() {
    let table = StackMapTable {
        entries: vec![StackMapFrame::SameFrame { offset_delta: 10 }],
    };
    assert_eq!(table.absolute_offsets().unwrap(), vec![10]);
}

#[test]
fn absolute_offsets_multiple_frames() {
    // Frame 1: delta=10 → absolute=10
    // Frame 2: delta=20 → absolute=10+20+1=31
    // Frame 3: delta=5 → absolute=31+5+1=37
    let table = StackMapTable {
        entries: vec![
            StackMapFrame::SameFrame { offset_delta: 10 },
            StackMapFrame::SameFrame { offset_delta: 20 },
            StackMapFrame::ChopFrame {
                offset_delta: 5,
                chopped: 1,
            },
        ],
    };
    assert_eq!(table.absolute_offsets().unwrap(), vec![10, 31, 37]);
}

#[test]
fn absolute_offsets_empty() {
    let table = StackMapTable { entries: vec![] };
    assert_eq!(table.absolute_offsets().unwrap(), Vec::<u16>::new());
}

/// Round 8 reader HIGH: deltas that push the running absolute
/// offset past `u16::MAX` must be rejected with `InvalidClassData`
/// — and the saturating accumulator must NOT panic in debug
/// builds. Construct two `u16::MAX` deltas back-to-back so the
/// release-build sum is `2 * u16::MAX + 1 = 131_071`, well past
/// the bound check.
#[test]
fn absolute_offsets_overflow_rejected_not_panic() {
    let table = StackMapTable {
        entries: vec![
            StackMapFrame::SameFrameExtended {
                offset_delta: u16::MAX,
            },
            StackMapFrame::SameFrameExtended {
                offset_delta: u16::MAX,
            },
        ],
    };
    let err = table.absolute_offsets().unwrap_err();
    match err {
        ClassReaderError::InvalidClassData { message } => {
            assert!(message.contains("exceeds u16::MAX"), "got: {message}");
        }
        other => panic!("expected InvalidClassData, got: {other:?}"),
    }
}

// --- Edge cases ---

#[test]
fn empty_table() {
    let data = build_table(&[], 0);
    let table = StackMapTable::parse(&data).unwrap();
    assert!(table.entries.is_empty());
}

#[test]
fn truncated_data_errors() {
    // Only 1 byte when we need 2 for the count
    let data = [0x00];
    // Actually this will read count=0 from a single u16 attempt → error
    assert!(StackMapTable::parse(&data).is_err());
}

/// Regression (reader B2): `read_u16` must NOT wrap or panic when
/// `*pos + 2` overflows `usize`. With the bare `*pos + 2 > data.len()`
/// compare a near-`usize::MAX` `pos` wraps to a small value that passes
/// the bounds check; the `checked_add` form rejects it as
/// `InvalidClassData` (unexpected end of data) instead.
#[test]
fn read_u16_overflow_returns_error() {
    let data = [0u8; 4];
    // pos + 2 overflows usize::MAX → checked_add returns None.
    let mut pos = usize::MAX - 1;
    let err = read_u16(&data, &mut pos).unwrap_err();
    assert!(matches!(err, ClassReaderError::InvalidClassData { .. }));
    // Failure leaves `pos` untouched so a retry observes the same error.
    assert_eq!(pos, usize::MAX - 1);
}

/// `read_u16` at the exact buffer boundary: `*pos + 2 == data.len()`
/// is the last valid read (non-overflowing, in-bounds); one byte short
/// is rejected.
#[test]
fn read_u16_boundary() {
    let data = [0xAB, 0xCD];
    let mut pos = 0;
    assert_eq!(read_u16(&data, &mut pos).unwrap(), 0xABCD);
    assert_eq!(pos, 2);
    // pos now at end: a further read is out of bounds.
    assert!(read_u16(&data, &mut pos).is_err());
    assert_eq!(pos, 2);
}

// ── Declared-count vs. input-length boundary corpus ──────────────────

/// `number_of_entries` is a `u2` in a two-byte header. Declaring the
/// maximum with no frame bytes must be an error, and — critically —
/// must not reserve 65 535 `StackMapFrame`s first. The must-accept
/// twins below keep this from passing vacuously.
#[test]
fn hostile_frame_count_is_rejected_and_honest_counts_still_parse() {
    // Must reject: 65 535 frames declared, zero frame bytes present.
    assert!(StackMapTable::parse(&u16::MAX.to_be_bytes()).is_err());

    // Must accept: three `same_frame` frames (tags 0..=63), present.
    let table = StackMapTable::parse(&build_table(&[0u8, 1, 2], 3))
        .expect("three same_frames must parse");
    assert_eq!(table.entries.len(), 3);

    // Off-by-one: one more frame declared than present.
    assert!(StackMapTable::parse(&build_table(&[0u8, 1, 2], 4)).is_err());

    // Zero-length: an empty table is legal.
    let empty = StackMapTable::parse(&build_table(&[], 0)).expect("empty table must parse");
    assert!(empty.entries.is_empty());

    // A truncated header (one byte) is an error, not a panic.
    assert!(StackMapTable::parse(&[0x00]).is_err());
    assert!(StackMapTable::parse(&[]).is_err());
}

/// `full_frame` carries two `u2` counts of `verification_type_info`.
/// Each is capped at 65 535 by its width; neither may drive a
/// reservation the attribute cannot fill.
#[test]
fn full_frame_type_counts_are_bounded_by_the_remaining_bytes() {
    // Must reject: num_locals = 65 535 with no type bytes following.
    let mut hostile = vec![255u8, 0, 0]; // tag, offset_delta
    hostile.extend_from_slice(&u16::MAX.to_be_bytes()); // num_locals
    assert!(StackMapTable::parse(&build_table(&hostile, 1)).is_err());

    // Must reject: honest locals, then num_stack = 65 535 with nothing
    // after it. Exercises the second count independently.
    let mut hostile_stack = vec![255u8, 0, 0];
    hostile_stack.extend_from_slice(&1u16.to_be_bytes()); // num_locals = 1
    hostile_stack.push(ITEM_INTEGER);
    hostile_stack.extend_from_slice(&u16::MAX.to_be_bytes()); // num_stack
    assert!(StackMapTable::parse(&build_table(&hostile_stack, 1)).is_err());

    // Must accept: one local, one stack entry, both present.
    let mut ok = vec![255u8, 0, 0];
    ok.extend_from_slice(&1u16.to_be_bytes());
    ok.push(ITEM_INTEGER);
    ok.extend_from_slice(&1u16.to_be_bytes());
    ok.push(ITEM_FLOAT);
    let table =
        StackMapTable::parse(&build_table(&ok, 1)).expect("well-formed full_frame must parse");
    match &table.entries[0] {
        StackMapFrame::FullFrame { locals, stack, .. } => {
            assert_eq!(locals.len(), 1);
            assert_eq!(stack.len(), 1);
        }
        other => panic!("expected FullFrame, got {other:?}"),
    }

    // Zero-length: a full_frame with no locals and no stack is legal.
    let mut empty = vec![255u8, 0, 0];
    empty.extend_from_slice(&0u16.to_be_bytes());
    empty.extend_from_slice(&0u16.to_be_bytes());
    assert!(StackMapTable::parse(&build_table(&empty, 1)).is_ok());
}

/// The delta accumulator in `absolute_offsets` must reject exactly at
/// `u16::MAX + 1` and accept exactly at `u16::MAX`.
#[test]
fn absolute_offset_accumulator_boundary() {
    // Accept: a single frame landing exactly on u16::MAX.
    let at_max = StackMapTable {
        entries: vec![StackMapFrame::SameFrameExtended {
            offset_delta: u16::MAX,
        }],
    };
    assert_eq!(at_max.absolute_offsets().unwrap(), vec![u16::MAX]);

    // Reject: one past it. The second frame's absolute offset is
    // 65535 + 0 + 1 = 65536.
    let past_max = StackMapTable {
        entries: vec![
            StackMapFrame::SameFrameExtended {
                offset_delta: u16::MAX,
            },
            StackMapFrame::SameFrame { offset_delta: 0 },
        ],
    };
    assert!(past_max.absolute_offsets().is_err());

    // Accept: the largest pair that still fits — 65534 then delta 0
    // (65534 + 0 + 1 = 65535).
    let just_fits = StackMapTable {
        entries: vec![
            StackMapFrame::SameFrameExtended {
                offset_delta: u16::MAX - 1,
            },
            StackMapFrame::SameFrame { offset_delta: 0 },
        ],
    };
    assert_eq!(
        just_fits.absolute_offsets().unwrap(),
        vec![u16::MAX - 1, u16::MAX]
    );

    // A long run of maximal deltas saturates rather than overflowing
    // the u32 accumulator in a debug build; the bound check still
    // rejects.
    let many = StackMapTable {
        entries: (0..64)
            .map(|_| StackMapFrame::SameFrameExtended {
                offset_delta: u16::MAX,
            })
            .collect(),
    };
    assert!(many.absolute_offsets().is_err());
}
