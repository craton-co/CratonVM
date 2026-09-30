// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 11, wave 16, lane `replay`.
//!
//! `docs/internal/fixed-bugs/r11w15-ir-splice-replay-reruns-a-leaf-call-FIXED-20260925.md`:
//! a spliced body may keep calls the resolver did not splice, and a trap after
//! such a call deopts to the CALLER's `invoke` with REEXECUTE, which runs the
//! call a second time. `R11W15IrSpliceReplay`'s `call` arm:
//!
//! ```java
//! static void inc() { COUNTER++; }
//! static int viaCall(int[] arr, int i) { inc(); return arr[i]; }
//! ```
//!
//! counted `COUNTER` twice for every out-of-bounds `i`. The builder now fences
//! a surviving call exactly like a committed store: until the walk is back in
//! the caller only instructions that cannot trap may follow it. The kind-4 leaf
//! self-call of an admitted self copy is exempt (its replay commits nothing a
//! second run could observe), which is what keeps binary-trees' recursive
//! inline.

use cratonvm_jit::ir::{IrBuilder, IrInlineSite, IrType, Op};
use cratonvm_jit::JitInvokeInfo;
use std::collections::HashMap;

/// A live `JitInvokeInfo`, leaked: an `Op::Call` bakes its address.
fn info(method_name: &'static str, descriptor: &'static str, kind: u8, args: usize) -> usize {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name,
        descriptor,
        num_jit_args: args,
        return_type: descriptor.as_bytes().last().copied().unwrap_or(b'V'),
        invoke_kind: kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    info as *const JitInvokeInfo as usize
}

/// `static int viaX(int[] arr, int i)` spliced at pc 2 of the caller.
fn site(base: usize, code_len: usize) -> IrInlineSite {
    IrInlineSite {
        base,
        code_len,
        num_args: 2,
        max_locals: 2,
        arg_local_slots: vec![0, 1],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: "T.viaX:([II)I".to_string(),
        class_id: 0,
    }
}

/// Caller `static int run(int[] a, int i) { return viaX(a, i) <tail>; }`:
/// `aload_0; iload_1; invokestatic viaX; <tail>`, with `body` spliced at pc 2
/// and `rows` (callee-relative pc -> row) installed for the body's calls.
fn build(
    tail: &[u8],
    body: &[u8],
    rows: &[(usize, (usize, usize, u8))],
) -> Option<cratonvm_jit::ir::Graph> {
    let mut combined = vec![0x2a, 0x1b, 0xb8, 0x00, 0x01];
    combined.extend_from_slice(tail);
    let code_len = combined.len();
    let base = code_len;
    combined.extend_from_slice(body);
    combined.extend_from_slice(&[0, 0]);
    let mut b = IrBuilder::new(2, 2);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    b.set_invoke_info(rows.iter().map(|&(pc, row)| (base + pc, row)).collect());
    b.set_inline_sites(HashMap::from([(2usize, site(base, body.len()))]));
    b.build(&combined, code_len)
}

/// `invokestatic inc()V` at callee pc 0, a fresh kind-3 row.
fn inc_row() -> (usize, usize, u8) {
    (info("inc", "()V", 3, 0), 0, b'V')
}

#[test]
fn the_probe_shape_is_refused() {
    // viaCall: inc(); return arr[i];
    let body = [
        0xb8, 0x00, 0x02, // 0: invokestatic inc()V (survives)
        0x2a, // 3: aload_0
        0x1b, // 4: iload_1
        0x2e, // 5: iaload -- bounds/null trap: would re-run inc()
        0xac, // 6: ireturn
    ];
    assert!(
        build(&[0xac], &body, &[(0, inc_row())]).is_none(),
        "a trap after a surviving call inside a splice must refuse the method",
    );
}

#[test]
fn a_trap_before_the_call_builds() {
    // The same instructions with the trap FIRST: its replay re-runs nothing.
    let body = [
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x2e, // 2: iaload
        0xb8, 0x00, 0x02, // 3: invokestatic inc()V
        0xac, // 6: ireturn
    ];
    assert!(
        build(&[0xac], &body, &[(3, inc_row())]).is_some(),
        "the control: the fence is positional, the trap alone does not refuse",
    );
}

#[test]
fn a_void_call_with_nothing_trapping_after_it_builds() {
    // inc(); return i + 1; -- a void leaf followed only by code that cannot
    // trap is still spliced, with its call in the graph.
    let body = [
        0xb8, 0x00, 0x02, // 0: invokestatic inc()V
        0x1b, // 3: iload_1
        0x04, // 4: iconst_1
        0x60, // 5: iadd
        0xac, // 6: ireturn
    ];
    let row = inc_row();
    let graph = build(&[0xac], &body, &[(0, row)]).expect("nothing after the call can trap");
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Call { info_ptr } if info_ptr == row.0)),
        "the surviving call is still built",
    );
}

#[test]
fn the_caller_may_trap_again_once_the_splice_has_returned() {
    // viaX: inc(); return i;  caller: return viaX(a, i) + a[i];
    // The caller's `iaload` resumes at its OWN bci, after the invoke.
    let body = [
        0xb8, 0x00, 0x02, // 0: invokestatic inc()V
        0x1b, // 3: iload_1
        0xac, // 4: ireturn
    ];
    let tail = [
        0x2a, // aload_0
        0x1b, // iload_1
        0x2e, // iaload
        0x60, // iadd
        0xac, // ireturn
    ];
    assert!(
        build(&tail, &body, &[(0, inc_row())]).is_some(),
        "the fence is scoped to the splice that made the call",
    );
}

/// `static int f(int[] a, int i) { return f(a, i) + a[i]; }`: `aload_0;
/// iload_1; invokestatic f; aload_0; iload_1; iaload; iadd; ireturn`, the
/// self call at pc 2, spliced once into itself. `leaf_row` is the row the
/// COPY's own self call gets.
fn build_self_copy(
    caller_row: (usize, usize, u8),
    leaf_row: (usize, usize, u8),
) -> Option<cratonvm_jit::ir::Graph> {
    let code = [0x2a, 0x1b, 0xb8, 0x00, 0x01, 0x2a, 0x1b, 0x2e, 0x60, 0xac];
    let code_len = code.len();
    let mut combined = code.to_vec();
    let base = combined.len();
    combined.extend_from_slice(&code);
    combined.extend_from_slice(&[0, 0]);
    let mut b = IrBuilder::new(2, 2);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    b.set_invoke_info(HashMap::from([(2usize, caller_row), (base + 2, leaf_row)]));
    let mut copy = site(base, code_len);
    copy.method_key = "T.f:([II)I".to_string();
    b.set_inline_sites(HashMap::from([(2usize, copy)]));
    b.build(&combined, code_len)
}

#[test]
fn the_self_copy_leaf_is_still_spliced() {
    // `lib.rs` binds the copy's leaf to the compiling method's OWN kind-4 row
    // (`IrSelfSplice::Leaf`), only inside an admitted copy. A trap after it
    // (`a[i]`, like `itemCheck`'s `getfield`) must not refuse the method.
    let k4 = (info("f", "([II)I", 4, 2), 2, b'I');
    let graph = build_self_copy(k4, k4).expect("the self copy's leaf is replay-safe");
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Call { info_ptr } if info_ptr == k4.0)),
        "the leaf lowers as the direct self-call",
    );
}

#[test]
fn a_call_that_is_not_the_callers_own_row_is_fenced() {
    // The same shape, but the copy's call carries a row of its own (as every
    // surviving call interned by `intern_inline_invoke_targets` does), even of
    // kind 4: it is not the proven leaf, and the trap after it refuses.
    let k4 = (info("f", "([II)I", 4, 2), 2, b'I');
    let other = (info("f", "([II)I", 4, 2), 2, b'I');
    assert!(build_self_copy(k4, other).is_none());
}
