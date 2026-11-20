use super::*;

/// A three-byte method: both spaces hold `orig_code_len + 1 == 4` entries.
const ORIG: usize = 3;

#[test]
fn the_identity_conversion_accepts_a_correctly_sized_vector() {
    let out = OutPcIndexed::new(vec![-1i32, 0x10, -1, -1], "osr_pc_to_native");
    let bci = out.into_bci_by_identity(ORIG).expect("lengths agree");
    assert_eq!(bci.len(), ORIG + 1);
    assert_eq!(bci.into_inner(), vec![-1, 0x10, -1, -1]);
}

/// The edit this module exists for: a vector sized from the OUTPUT code
/// length reaching the identity arm, which used to be a bare `None =>`
/// match arm that took anything.
#[test]
fn the_identity_conversion_refuses_an_output_sized_vector() {
    // A rewrite that duplicated the loop body would leave the emitter's
    // vector longer than the original method.
    let out = OutPcIndexed::new(vec![-1i32; ORIG + 4], "osr_pc_to_native");
    assert_eq!(
        out.into_bci_by_identity(ORIG),
        Err(CoordinateMismatch {
            what: "osr_pc_to_native",
            via: "identity",
            got: ORIG + 4,
            expected: ORIG + 1,
        })
    );
}

/// And the short direction, which is the silently *unsound* one downstream:
/// `can_osr_enter_with` reads the dead mask through `unwrap_or(0)`, so a
/// short mask reads as "no dead locals" for every bci in the tail.
#[test]
fn the_identity_conversion_refuses_a_short_vector() {
    let out = OutPcIndexed::new(vec![0u64; ORIG], "osr_dead_mask");
    let err = out.into_bci_by_identity(ORIG).expect_err("too short");
    assert_eq!(err.got, ORIG);
    assert_eq!(err.expected, ORIG + 1);
}

#[test]
fn a_translated_vector_is_checked_against_the_original_length() {
    assert!(BciIndexed::from_translated(vec![0u64; ORIG + 1], ORIG, "osr_dead_mask").is_ok());
    let err = BciIndexed::from_translated(vec![0u64; ORIG + 2], ORIG, "osr_dead_mask")
        .expect_err("a translation that lands in the wrong space is refused");
    assert_eq!(err.via, "translated");
    assert_eq!(err.what, "osr_dead_mask");
}

/// The diagnostic names the vector and both lengths, because a coordinate
/// bug's first question is always "by how much".
#[test]
fn the_message_names_the_vector_and_both_lengths() {
    let m = CoordinateMismatch {
        what: "osr_pc_to_native",
        via: "identity",
        got: 9,
        expected: 4,
    };
    let s = m.to_string();
    assert!(s.contains("osr_pc_to_native"), "{s}");
    assert!(s.contains('9') && s.contains('4'), "{s}");
}

#[test]
fn a_noted_mismatch_is_counted() {
    reset_osr_coordinate_mismatches();
    assert_eq!(osr_coordinate_mismatches(), 0);
    note_mismatch(
        &CoordinateMismatch {
            what: "osr_dead_mask",
            via: "identity",
            got: 1,
            expected: 2,
        },
        "Test.m()V",
    );
    assert_eq!(osr_coordinate_mismatches(), 1);
    reset_osr_coordinate_mismatches();
}
