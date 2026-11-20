//! JIT round 16 wave 12 (lane throw3, TP16-14): the callee exception-table
//! verdict answered by the info's identity (the retire epoch it was proved
//! at) before its content.

use super::{CalleeTableVerdict, JitInvokeInfo, INTEGER_INT_VALUE_INFO, INTEGER_VALUE_OF_INFO};

pub const VM: usize = 7;
pub const STAMP: (u64, u64) = (3, 0);

pub fn verdict_for(info: &JitInvokeInfo, retire_epoch: u64) -> CalleeTableVerdict {
    CalleeTableVerdict {
        vm_identity: VM,
        selected: false,
        receiver: 0,
        declaring: info.declaring_class_id,
        owner: info.owner_class_id,
        kind: info.invoke_kind,
        class_name: Box::from(info.class_name),
        method_name: Box::from(info.method_name),
        descriptor: Box::from(info.descriptor),
        stamp: STAMP,
        has_table: true,
        retire_epoch,
    }
}

#[test]
pub fn switch_off_compares_the_names_as_before() {
    let v = verdict_for(&INTEGER_VALUE_OF_INFO, 5);
    assert_eq!(
        v.answer(VM, &INTEGER_VALUE_OF_INFO, false, 0, STAMP, None),
        (Some(true), false)
    );
    let (other, _) = v.answer(VM, &INTEGER_INT_VALUE_INFO, false, 0, STAMP, None);
    assert_eq!(other, None, "another site's names miss");
}

#[test]
pub fn at_the_proved_epoch_the_scalars_decide() {
    let v = verdict_for(&INTEGER_VALUE_OF_INFO, 5);
    assert_eq!(
        v.answer(VM, &INTEGER_VALUE_OF_INFO, false, 0, STAMP, Some(5)),
        (Some(true), false)
    );
    // Every integer field still counts.
    let info = &INTEGER_VALUE_OF_INFO;
    let at5 = |vm: usize, selected: bool, receiver: u32, stamp: (u64, u64)| {
        v.answer(vm, info, selected, receiver, stamp, Some(5)).0
    };
    assert_eq!(at5(VM, false, 0, (4, 0)), None);
    assert_eq!(at5(VM + 1, false, 0, STAMP), None);
    assert_eq!(at5(VM, true, 0, STAMP), None);
    assert_eq!(at5(VM, false, 9, STAMP), None);
}

/// After a retire the address may name another info: the names decide,
/// and a match asks the caller to re-stamp the entry.
#[test]
pub fn after_a_retire_the_names_decide_and_a_match_refreshes() {
    let v = verdict_for(&INTEGER_VALUE_OF_INFO, 5);
    assert_eq!(
        v.answer(VM, &INTEGER_VALUE_OF_INFO, false, 0, STAMP, Some(6)),
        (Some(true), true)
    );
    assert_eq!(
        v.answer(VM, &INTEGER_INT_VALUE_INFO, false, 0, STAMP, Some(6)),
        (None, false)
    );
}
