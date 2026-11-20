use super::{cast_target_excludes_arrays, sp_receiver_cast_pc};

/// `aload_1 ; checkcast #2 ; invokeinterface #3, 1, 0` -- the
/// `((Shape) o).sides()` shape; the call is at pc 4.
const CAST_THEN_CALL: [u8; 9] = [0x2b, 0xc0, 0x00, 0x02, 0xb9, 0x00, 0x03, 0x01, 0x00];

fn targets(len: usize, marked: &[usize]) -> Vec<bool> {
    let mut t = vec![false; len + 1];
    t[0] = true;
    for &m in marked {
        t[m] = true;
    }
    t
}

#[test]
fn a_receiver_only_call_right_after_a_checkcast_names_that_cast() {
    let bt = targets(CAST_THEN_CALL.len(), &[]);
    assert_eq!(sp_receiver_cast_pc(&CAST_THEN_CALL, 9, 4, 1, &bt), Some(1));
}

#[test]
fn a_call_that_is_a_branch_target_has_another_predecessor() {
    let bt = targets(CAST_THEN_CALL.len(), &[4]);
    assert_eq!(sp_receiver_cast_pc(&CAST_THEN_CALL, 9, 4, 1, &bt), None);
}

#[test]
fn a_call_with_arguments_does_not_take_the_cast_as_its_receiver() {
    let bt = targets(CAST_THEN_CALL.len(), &[]);
    assert_eq!(sp_receiver_cast_pc(&CAST_THEN_CALL, 9, 4, 2, &bt), None);
}

#[test]
fn a_checkcast_byte_inside_an_operand_is_not_a_cast() {
    // `sipush 0xC000 ; aload_1 ; invokeinterface ...`: code[1] == 0xc0 is
    // the sipush operand, three bytes before the call at pc 4.
    let code = [0x11, 0xc0, 0x00, 0x2b, 0xb9, 0x00, 0x03, 0x01, 0x00];
    let bt = targets(code.len(), &[]);
    assert_eq!(code[4 - 3], 0xc0);
    assert_eq!(sp_receiver_cast_pc(&code, 9, 4, 1, &bt), None);
}

#[test]
fn a_call_past_the_code_bound_or_too_early_declines() {
    let bt = targets(CAST_THEN_CALL.len(), &[]);
    assert_eq!(sp_receiver_cast_pc(&CAST_THEN_CALL, 4, 4, 1, &bt), None);
    assert_eq!(sp_receiver_cast_pc(&CAST_THEN_CALL, 9, 2, 1, &bt), None);
}

#[test]
fn only_targets_no_array_is_assignable_to_prove_a_plain_receiver() {
    assert!(cast_target_excludes_arrays(Some("pkg/Shape")));
    assert!(cast_target_excludes_arrays(Some("java/lang/Number")));
    for admits in [
        "java/lang/Object",
        "java/lang/Cloneable",
        "java/io/Serializable",
        "[Lpkg/Shape;",
        "[I",
        "",
    ] {
        assert!(!cast_target_excludes_arrays(Some(admits)), "{admits:?}");
    }
    assert!(!cast_target_excludes_arrays(None));
}
