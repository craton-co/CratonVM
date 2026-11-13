// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Share a bimorphic site's receiver-independent leaf body. Both inputs have
//! already passed the VM's receiver-specific dispatch and native-shadow checks.
//! The class-set guard admits only these exact classes; nulls, overrides and
//! later-loaded classes keep the original call. It establishes no exact-class
//! fact inside the shared body (`IrBuilder::cha_guarded_site`).

use super::IrGuardedBody;

pub(super) fn receiver_classes(first: &IrGuardedBody, second: &IrGuardedBody) -> Option<Vec<u32>> {
    let (a, ca) = first;
    let (b, cb) = second;
    let leaf = |s: &super::InlineSite| {
        !s.callee_is_static
            && !s.callee_is_synchronized
            && s.invoke_targets.is_empty()
            && s.nested_sites.is_empty()
            && s.elided_invoke_pcs.is_empty()
            && s.ir_indy_rows.is_empty()
            && s.ir_indy_capture_rows.is_empty()
            && s.ir_indy_capture_sites.is_empty()
            && s.ir_lambda_impl_rows.is_empty()
    };
    // A class id distinguishes identical names defined by different loaders.
    // Keep receiver-specialized call trees separate: a nested target may have
    // been resolved using the first receiver as an exact argument class.
    if a.class_id == 0
        || a.class_id != b.class_id
        || a.class_name != b.class_name
        || a.method_name != b.method_name
        || a.descriptor != b.descriptor
        || a.callee_code_len != b.callee_code_len
        || a.callee_code != b.callee_code
        || !leaf(a)
        || !leaf(b)
    {
        return None;
    }
    // The builder revalidates the set; ClassWordOf sign-extends its int.
    (*ca != 0 && *cb != 0 && ca != cb && *ca <= i32::MAX as u32 && *cb <= i32::MAX as u32)
        .then_some(vec![*ca, *cb])
}

#[cfg(test)]
mod tests {
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
}
