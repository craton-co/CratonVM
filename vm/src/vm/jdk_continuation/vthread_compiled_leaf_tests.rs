//! Interpreter round i4 wave 92, lane L6: the leaf test of
//! `CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY` (no switch is read here).
use super::{bytecode_cannot_reach_a_yield, VTHREAD_LEAF_MAX_CODE_BYTES};

#[test]
fn an_array_sum_loop_is_a_leaf() {
    // static long sum(int[] a) { long s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }
    let code = [
        0x09, 0x40, 0x03, 0x36, 0x04, 0x15, 0x04, 0x2a, 0xbe, 0xa2, 0x00, 0x11, 0x1f, 0x2a,
        0x15, 0x04, 0x2e, 0x85, 0x61, 0x40, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xee, 0x1f, 0xad,
    ];
    assert!(bytecode_cannot_reach_a_yield(&code));
}

#[test]
fn any_invoke_or_monitor_op_is_not_a_leaf() {
    for op in [0xb6u8, 0xb7, 0xb8] {
        assert!(!bytecode_cannot_reach_a_yield(&[0x2a, op, 0x00, 0x01, 0xb1]));
    }
    assert!(!bytecode_cannot_reach_a_yield(&[0x2a, 0xb9, 0x00, 0x01, 0x01, 0x00, 0xb1]));
    assert!(!bytecode_cannot_reach_a_yield(&[0xba, 0x00, 0x01, 0x00, 0x00, 0xb0]));
    assert!(!bytecode_cannot_reach_a_yield(&[0x2a, 0xc2, 0x2a, 0xc3, 0xb1]));
    // An invoke hidden behind a field access and a branch still counts.
    assert!(!bytecode_cannot_reach_a_yield(&[
        0xb2, 0x00, 0x02, 0x99, 0x00, 0x06, 0xb8, 0x00, 0x03, 0xb1
    ]));
}

#[test]
fn switches_and_wide_forms_are_decoded() {
    // iload_0; tableswitch (pad 2) default low=0 high=1 two offsets; return
    let mut table = vec![0x1a, 0xaa, 0x00, 0x00];
    for word in [0i32, 0, 1, 0, 0] {
        table.extend_from_slice(&word.to_be_bytes());
    }
    table.push(0xb1);
    assert!(bytecode_cannot_reach_a_yield(&table));
    // The same with an invoke after the switch.
    let mut tail = table.clone();
    tail.pop();
    tail.extend_from_slice(&[0xb8, 0x00, 0x01, 0xb1]);
    assert!(!bytecode_cannot_reach_a_yield(&tail));
    // lookupswitch at pc 0 (pad 3), npairs 1.
    let mut lookup = vec![0xab, 0x00, 0x00, 0x00];
    for word in [0i32, 1, 7, 0] {
        lookup.extend_from_slice(&word.to_be_bytes());
    }
    lookup.push(0xb1);
    assert!(bytecode_cannot_reach_a_yield(&lookup));
    // wide iinc, wide iload.
    assert!(bytecode_cannot_reach_a_yield(&[
        0xc4, 0x84, 0x01, 0x00, 0x00, 0x01, 0xc4, 0x15, 0x01, 0x00, 0xac
    ]));
}

#[test]
fn malformed_code_is_not_a_leaf() {
    assert!(!bytecode_cannot_reach_a_yield(&[]));
    // A truncated sipush, a breakpoint, an impdep, a bad wide.
    assert!(!bytecode_cannot_reach_a_yield(&[0x11, 0x00]));
    assert!(!bytecode_cannot_reach_a_yield(&[0xca, 0xb1]));
    assert!(!bytecode_cannot_reach_a_yield(&[0xfe, 0xb1]));
    assert!(!bytecode_cannot_reach_a_yield(&[0xc4, 0x60, 0xb1]));
    // A tableswitch with high < low.
    let mut table = vec![0x1a, 0xaa, 0x00, 0x00];
    for word in [0i32, 2, 1] {
        table.extend_from_slice(&word.to_be_bytes());
    }
    table.push(0xb1);
    assert!(!bytecode_cannot_reach_a_yield(&table));
}

#[test]
fn the_scan_bound_covers_a_short_loop() {
    // `sum` above is 28 bytes; a body at the bound is still scanned.
    let mut code = vec![0x00u8; VTHREAD_LEAF_MAX_CODE_BYTES - 1];
    code.push(0xb1);
    assert!(bytecode_cannot_reach_a_yield(&code));
}

/// Interpreter round i4 wave 93, lane L6
/// (`CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS`): the walk hands each
/// `invokestatic` (pc, constant-pool index) to the caller and still
/// refuses every other invoke and monitor op.
#[test]
fn the_caller_walk_hands_over_invokestatic_only() {
    use super::scan_for_yield_points;
    // iload_0; invokestatic #3; iload_0; invokestatic #0x0104; iadd; ireturn
    let code = [0x1a, 0xb8, 0x00, 0x03, 0x1a, 0xb8, 0x01, 0x04, 0x60, 0xac];
    let mut seen = Vec::new();
    assert!(scan_for_yield_points(&code, &mut |pc, cp| {
        seen.push((pc, cp));
        true
    }));
    assert_eq!(seen, vec![(1, 3), (5, 0x0104)]);
    // The callback's "no" ends the walk with "no".
    let mut calls = 0;
    assert!(!scan_for_yield_points(&code, &mut |_, _| {
        calls += 1;
        false
    }));
    assert_eq!(calls, 1);
    // Every other invoke and the monitor ops are refused without asking.
    for op in [0xb6u8, 0xb7] {
        assert!(!scan_for_yield_points(&[0x2a, op, 0x00, 0x01, 0xb1], &mut |_, _| true));
    }
    assert!(!scan_for_yield_points(
        &[0x2a, 0xb9, 0x00, 0x01, 0x01, 0x00, 0xb1],
        &mut |_, _| true
    ));
    assert!(!scan_for_yield_points(&[0xba, 0x00, 0x01, 0x00, 0x00, 0xb0], &mut |_, _| true));
    assert!(!scan_for_yield_points(&[0x2a, 0xc2, 0x2a, 0xc3, 0xb1], &mut |_, _| true));
    // A truncated invokestatic.
    assert!(!scan_for_yield_points(&[0xb8, 0x00], &mut |_, _| true));
}
