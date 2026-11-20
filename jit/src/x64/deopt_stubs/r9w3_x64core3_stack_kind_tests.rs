//! Round 9 wave 3 (`x64core3`): the self-call shape in the typed operand
//! stack, and the descriptor-aware half of the `wide_fp` gate
//! (`deopt-stack-width-gate-misses-field-invoke-and-ldc-values-20260918.md`).

use crate::x64::stack_kinds::StackKind;
use crate::x64::Compiler;

fn test_compiler() -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    Compiler::new(
        "r9w3-x64core3-test".to_string(),
        crate::ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        cratonvm_jit_api::JitRuntimeHelpers::default(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    )
}

/// `static int itemCheck(Node n)` from `CratonBench` (javac output):
/// `n.left == null ? 1 : 1 + itemCheck(n.left) + itemCheck(n.right)`.
const ITEM_CHECK: [u8; 27] = [
    0x2a, // 0: aload_0
    0xb4, 0x00, 0x5c, // 1: getfield left
    0xc7, 0x00, 0x05, // 4: ifnonnull -> 9
    0x04, // 7: iconst_1
    0xac, // 8: ireturn
    0x04, // 9: iconst_1
    0x2a, // 10: aload_0
    0xb4, 0x00, 0x5c, // 11: getfield left
    0xb8, 0x00, 0x62, // 14: invokestatic itemCheck (self)
    0x60, // 17: iadd
    0x2a, // 18: aload_0
    0xb4, 0x00, 0x66, // 19: getfield right
    0xb8, 0x00, 0x62, // 22: invokestatic itemCheck (self)
    0x60, // 25: iadd
    0xac, // 26: ireturn
];

fn item_check_compiler(field_tag: u8) -> Compiler {
    let mut c = test_compiler();
    c.method_key = "CratonBench.itemCheck:(LCratonBench$Node;)I".to_string();
    c.num_params = 1;
    c.field_info = vec![(1, 0, b'L'), (11, 0, b'L'), (19, 1, field_tag)];
    c
}

#[test]
fn a_self_recursive_call_is_typed_from_the_method_descriptor() {
    let mut c = item_check_compiler(b'L');
    c.analyze_stack_kinds(&ITEM_CHECK, ITEM_CHECK.len());
    assert_eq!(
        c.stack_kinds.get(22),
        Some(&[StackKind::Int, StackKind::Ref][..]),
        "the second self call's receiver-less args sit on `1 + itemCheck(left)`"
    );
    assert_eq!(
        c.stack_kinds.get(25),
        Some(&[StackKind::Int, StackKind::Int][..])
    );
    assert!(
        !c.uses_long_float_double,
        "ref fields and an int self call must keep the method int-only"
    );
}

#[test]
fn with_no_method_key_a_self_call_still_poisons() {
    let mut c = item_check_compiler(b'L');
    c.method_key = String::new();
    c.analyze_stack_kinds(&ITEM_CHECK, ITEM_CHECK.len());
    assert_eq!(
        c.stack_kinds.get(14),
        Some(&[StackKind::Int, StackKind::Ref][..])
    );
    assert_eq!(
        c.stack_kinds.get(17),
        None,
        "downstream of an unmodelled call"
    );
}

#[test]
fn an_unnamed_wide_value_flips_the_gate() {
    // A `J` field where `right` was: nothing in the method NAMES a long.
    let mut c = item_check_compiler(b'J');
    c.analyze_stack_kinds(&ITEM_CHECK, ITEM_CHECK.len());
    assert!(c.uses_long_float_double);
    // A getfield with no resolved metadata at all flips it too.
    let mut c = item_check_compiler(b'L');
    c.field_info.pop();
    c.analyze_stack_kinds(&ITEM_CHECK, ITEM_CHECK.len());
    assert!(c.uses_long_float_double);
    // The gate only ever goes false -> true.
    let mut c = item_check_compiler(b'L');
    c.uses_long_float_double = true;
    c.analyze_stack_kinds(&ITEM_CHECK, ITEM_CHECK.len());
    assert!(c.uses_long_float_double);
}
