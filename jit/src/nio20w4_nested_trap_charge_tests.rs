use super::*;

fn body(class: &str, method: &str) -> InlineSite {
    InlineSite {
        class_name: class.to_string(),
        method_name: method.to_string(),
        descriptor: "()V".to_string(),
        ..InlineSite::default()
    }
}

#[test]
fn the_id_is_stable_named_and_never_the_wildcard() {
    let a = nested_trap_speculation_id("p/A.f:()V", 7);
    assert_eq!(a, nested_trap_speculation_id("p/A.f:()V", 7));
    assert_ne!(a, deopt::SPECULATION_ID_ANY);
    assert_ne!(a, nested_trap_speculation_id("p/A.f:()V", 8));
    assert_ne!(a, nested_trap_speculation_id("p/B.f:()V", 7));
}

/// Only the row whose (enclosing key, bci) id is withdrawn at the outer
/// site goes; a sibling row, a deeper row and an untagged row stay.
#[test]
fn a_withdrawn_nested_trap_drops_its_row_only() {
    let mut deeper = body("p/B", "g");
    deeper.nested_sites.push(NestedInlineSite {
        bimorphic_second: None,
        callee_pc: 3,
        guard_class_id: 9,
        site: body("p/C", "h"),
    });
    let mut outer = body("p/A", "f");
    outer.nested_sites.push(NestedInlineSite {
        bimorphic_second: None,
        callee_pc: 4,
        guard_class_id: 7,
        site: body("p/X", "x"),
    });
    outer.nested_sites.push(NestedInlineSite {
        bimorphic_second: None,
        callee_pc: 10,
        guard_class_id: 8,
        site: deeper,
    });
    outer.nested_sites.push(NestedInlineSite {
        bimorphic_second: None,
        callee_pc: 4,
        guard_class_id: 0,
        site: body("p/S", "s"),
    });
    let registry = deopt::DespecRegistry::new();
    // Charged at the outer call site pc 12 of `T.run:()V`, for the row at
    // bci 4 of `p/A.f`.
    registry.insert("T.run:()V", 12, nested_trap_speculation_id("p/A.f:()V", 4));
    let mut site = outer.clone();
    assert_eq!(
        ir_splice_strip_withdrawn_nested_traps(&mut site, &registry, "T.run:()V", 12),
        1
    );
    let pcs: Vec<(usize, u32)> = site
        .nested_sites
        .iter()
        .map(|n| (n.callee_pc, n.guard_class_id))
        .collect();
    assert_eq!(pcs, vec![(10, 8), (4, 0)]);
    assert_eq!(site.nested_sites[0].site.nested_sites.len(), 1);
    // Another call site, or another method, withdraws nothing.
    let mut site = outer.clone();
    assert_eq!(
        ir_splice_strip_withdrawn_nested_traps(&mut site, &registry, "T.run:()V", 13),
        0
    );
    assert_eq!(
        ir_splice_strip_withdrawn_nested_traps(&mut site, &registry, "T.go:()V", 12),
        0
    );
    // The deeper row, keyed by ITS enclosing body `p/B.g`.
    registry.insert("T.run:()V", 12, nested_trap_speculation_id("p/B.g:()V", 3));
    let mut site = outer;
    assert_eq!(
        ir_splice_strip_withdrawn_nested_traps(&mut site, &registry, "T.run:()V", 12),
        2
    );
    assert!(site.nested_sites[0].site.nested_sites.is_empty());
}
