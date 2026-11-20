use super::*;
use crate::InlineSite;

fn body(receiver: u32, declaring: u32) -> IrGuardedBody {
    (
        InlineSite {
            class_id: declaring,
            class_name: "Reference".into(),
            method_name: "get".into(),
            descriptor: "()Ljava/lang/Object;".into(),
            callee_code: vec![0x2a, 0xb4, 0, 1, 0xb0],
            callee_code_len: 5,
            ..InlineSite::default()
        },
        receiver,
    )
}

#[test]
fn inherited_leaf_shares_one_body_for_two_exact_classes() {
    assert_eq!(receiver_classes(&body(7, 3), &body(9, 3)), Some(vec![7, 9]));
}

#[test]
fn overrides_and_loader_namesakes_keep_separate_bodies() {
    assert!(receiver_classes(&body(7, 3), &body(9, 4)).is_none());
    let mut changed = body(9, 3);
    changed.0.callee_code[0] = 0x01;
    assert!(receiver_classes(&body(7, 3), &changed).is_none());
}

#[test]
fn receiver_specialized_nested_trees_are_not_shared() {
    let mut nested = body(9, 3);
    nested.0.nested_sites.push(crate::NestedInlineSite {
        ..Default::default()
    });
    assert!(receiver_classes(&body(7, 3), &nested).is_none());
}

#[test]
fn invalid_or_duplicate_receiver_classes_cannot_form_a_set() {
    for second in [0, 7, 0x8000_0001] {
        assert!(receiver_classes(&body(7, 3), &body(second, 3)).is_none());
    }
}
