use super::*;

fn static_target(method: &str, desc: &str) -> InlineInvokeTarget {
    InlineInvokeTarget {
        class_name: "P".to_string(),
        method_name: method.to_string(),
        descriptor: desc.to_string(),
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 7,
        ..Default::default()
    }
}

/// `static int viaCall(int[] a, int i)` with `code`, one surviving
/// `invokestatic inc()V` at `call_pc`.
fn via_call(code: Vec<u8>, call_pc: usize) -> InlineSite {
    InlineSite {
        callee_code_len: code.len(),
        callee_code: code,
        callee_max_locals: 2,
        callee_num_args: 2,
        callee_is_static: true,
        return_type: b'I',
        class_name: "P".to_string(),
        class_id: 7,
        method_name: "viaCall".to_string(),
        descriptor: "([II)I".to_string(),
        invoke_targets: vec![(call_pc, static_target("inc", "()V"))],
        ..Default::default()
    }
}

/// `R11W15IrSpliceReplay`'s `call` arm: `inc(); return a[i];`. The site is
/// refused, so the caller compiles with the call to `viaCall` in place.
#[test]
fn the_probe_shape_refuses_the_site() {
    let site = via_call(vec![0xb8, 0x00, 0x02, 0x2a, 0x1b, 0x2e, 0xac], 0);
    assert!(ir_splice_body_may_trap_after_a_call(&site, None));
}

/// The trap first, the call last: nothing re-runs the call.
#[test]
fn a_trap_before_the_call_is_admitted() {
    let site = via_call(vec![0x2a, 0x1b, 0x2e, 0xb8, 0x00, 0x02, 0xac], 3);
    assert!(!ir_splice_body_may_trap_after_a_call(&site, None));
}

/// A void call followed only by code that cannot trap is admitted.
#[test]
fn a_void_call_with_a_trap_free_tail_is_admitted() {
    let site = via_call(vec![0xb8, 0x00, 0x02, 0x1b, 0x04, 0x60, 0xac], 0);
    assert!(!ir_splice_body_may_trap_after_a_call(&site, None));
}

/// A call SPLICED in is not a call -- unless its own body leaves one.
#[test]
fn a_nested_splice_counts_only_when_it_leaves_a_call() {
    // inc(): `iconst_1; pop; return` (no call), and its twin that calls.
    let quiet = InlineSite {
        callee_code: vec![0x04, 0x57, 0xb1],
        callee_code_len: 3,
        callee_is_static: true,
        return_type: b'V',
        class_name: "P".to_string(),
        class_id: 7,
        method_name: "inc".to_string(),
        descriptor: "()V".to_string(),
        ..Default::default()
    };
    let mut site = via_call(vec![0xb8, 0x00, 0x02, 0x2a, 0x1b, 0x2e, 0xac], 0);
    site.nested_sites = vec![NestedInlineSite {
        callee_pc: 0,
        guard_class_id: 0,
        site: quiet.clone(),
        bimorphic_second: None,
    }].into();
    assert!(!ir_splice_body_may_trap_after_a_call(&site, None));
    let mut loud = quiet;
    loud.callee_code = vec![0xb8, 0x00, 0x09, 0xb1];
    loud.callee_code_len = 4;
    loud.invoke_targets = vec![(0, static_target("log", "()V"))];
    site.nested_sites[0].site = loud;
    assert!(ir_splice_body_may_trap_after_a_call(&site, None));
}

/// `static int f(int[] a, int i) { return f(a, i) + a[i]; }`, its self
/// call at pc 2 carrying `depth` more nested copies.
fn self_site(depth: usize) -> InlineSite {
    let code = vec![0x2a, 0x1b, 0xb8, 0x00, 0x01, 0x2a, 0x1b, 0x2e, 0x60, 0xac];
    let mut site = via_call(code, 2);
    site.method_name = "f".to_string();
    site.invoke_targets = vec![(2, static_target("f", "([II)I"))];
    if depth > 0 {
        site.nested_sites = vec![NestedInlineSite {
            callee_pc: 2,
            guard_class_id: 0,
            site: self_site(depth - 1),
            bimorphic_second: None,
        }].into();
    }
    site
}

/// The HARD constraint: a self copy's leaf is replay-safe, so a trap
/// after it (`itemCheck`'s `getfield`) does not refuse the copy. Without
/// the plan the same tree's deepest call is an ordinary surviving call.
#[test]
fn a_self_copy_leaf_is_not_a_call() {
    let row = (0xD1_0000usize, 2usize, b'I');
    let top = ir::IrSelfRecursion::new("P", "f", "([II)I", 7, row);
    let ir::IrSelfSplice::Splice(first) = top.classify("P", "f", "([II)I", 7, true) else {
        panic!("the top-level self site is the first copy");
    };
    assert!(!ir_splice_body_may_trap_after_a_call(
        &self_site(3),
        Some(&first)
    ));
    assert!(ir_splice_body_may_trap_after_a_call(&self_site(1), None));
}

#[test]
fn the_after_call_opcodes_cannot_trap() {
    for op in [
        0x2e, 0x4f, 0x6c, 0x99, 0xa7, 0xb4, 0xb5, 0xb6, 0xb8, 0xbb, 0xbe, 0xc0, 0xc2,
    ] {
        assert!(!ir_splice_opcode_cannot_trap(op), "{op:#04x}");
    }
    for op in [
        0x00, 0x12, 0x1b, 0x2a, 0x36, 0x57, 0x59, 0x60, 0x84, 0x94, 0xac, 0xb1,
    ] {
        assert!(ir_splice_opcode_cannot_trap(op), "{op:#04x}");
    }
}
