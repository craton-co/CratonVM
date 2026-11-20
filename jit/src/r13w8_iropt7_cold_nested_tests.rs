use super::*;

fn virtual_row(method: &str) -> InlineInvokeTarget {
    InlineInvokeTarget {
        class_name: "java/util/HashMap".to_string(),
        method_name: method.to_string(),
        descriptor: "()V".to_string(),
        invoke_kind: 0,
        return_type: b'V',
        declaring_class_id: 7,
        ..Default::default()
    }
}

fn void_body(code: Vec<u8>, rows: Vec<(usize, InlineInvokeTarget)>) -> InlineSite {
    InlineSite {
        callee_code_len: code.len(),
        callee_code: code,
        callee_max_locals: 1,
        class_name: "java/util/HashMap".to_string(),
        class_id: 7,
        method_name: "m".to_string(),
        descriptor: "()V".to_string(),
        invoke_targets: rows,
        ..Default::default()
    }
}

/// `head`, padded with `nop` to `len - 1` bytes, then `return`.
fn padded(mut head: Vec<u8>, len: usize) -> Vec<u8> {
    head.resize(len - 1, 0x00);
    head.push(0xb1);
    head
}

fn nested(pc: usize, site: InlineSite) -> NestedInlineSite {
    NestedInlineSite {
        callee_pc: pc,
        guard_class_id: 0,
        site,
        bimorphic_second: None,
    }
}

/// ```text
///  0: aload_0  1: invokevirtual big   4: aload_0  5: ifnull 12
///  8: aload_0  9: invokevirtual cold 12: aload_0 13: invokevirtual small
/// 16: return
/// ```
/// `big` (40 bytes) is on the entry prefix and itself calls a 40-byte body
/// behind its own branch; `cold` (40 bytes) is behind the branch; `small`
/// (10 bytes) is behind it too but under the cold size.
fn tree() -> InlineSite {
    let inner_cold = void_body(padded(Vec::new(), 40), Vec::new());
    // 0: iconst_0  1: ifeq 8  4: aload_0  5: invokevirtual inner  8..: nop
    let mut big = void_body(
        padded(vec![0x03, 0x99, 0x00, 0x07, 0x2a, 0xb6, 0x00, 0x04], 40),
        vec![(5, virtual_row("inner"))],
    );
    big.nested_sites = vec![nested(5, inner_cold)].into();
    let mut top = void_body(
        vec![
            0x2a, 0xb6, 0x00, 0x01, 0x2a, 0xc6, 0x00, 0x07, 0x2a, 0xb6, 0x00, 0x02, 0x2a, 0xb6,
            0x00, 0x03, 0xb1,
        ],
        vec![
            (1, virtual_row("big")),
            (9, virtual_row("cold")),
            (13, virtual_row("small")),
        ],
    );
    top.nested_sites = vec![
        nested(1, big),
        nested(9, void_body(padded(Vec::new(), 40), Vec::new())),
        nested(13, void_body(padded(Vec::new(), 10), Vec::new())),
    ].into();
    top
}

#[test]
fn the_entry_prefix_ends_at_the_first_control_transfer() {
    let top = tree();
    let code = &top.callee_code;
    assert!(ir_splice_pc_in_entry_prefix(code, code.len(), 1));
    // The branch itself runs.
    assert!(ir_splice_pc_in_entry_prefix(code, code.len(), 5));
    assert!(!ir_splice_pc_in_entry_prefix(code, code.len(), 9));
    assert!(!ir_splice_pc_in_entry_prefix(code, code.len(), 13));
    // A pc inside an instruction is never reached.
    assert!(!ir_splice_pc_in_entry_prefix(code, code.len(), 2));
}

/// The large body behind a branch goes, at every level; the wrapper's
/// unconditional worker and the small body stay.
#[test]
fn large_nested_bodies_behind_a_branch_stay_calls() {
    let mut top = tree();
    assert_eq!(ir_splice_prune_cold_nested(&mut top), 2);
    let pcs: Vec<usize> = top.nested_sites.iter().map(|n| n.callee_pc).collect();
    assert_eq!(pcs, vec![1, 13]);
    // `big`'s own cold child went too.
    assert!(top.nested_sites[0].site.nested_sites.is_empty());
    // The dropped pcs keep their call rows.
    assert!(top.invoke_targets.iter().any(|(pc, _)| *pc == 9));
}

// JIT r23 wave 10 (CF-W10-2): a large body behind a branch the
// ENCLOSING body's profile says always falls through is on its dominant
// path, and stays spliced; the static rule still drops the others.
