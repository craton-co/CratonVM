use super::*;
use crate::ir::IrBuilder;

/// Only the compactness guard's OWN speculation, at the field access's
/// pc, of this method, withdraws it; another reason at the same pc (the
/// deopting layout guard's `ClassLoading`) or another method does not.
#[test]
fn only_the_compactness_speculation_at_the_pc_withdraws_it() {
    const KEY: &str = "T.f:(LT;)I";
    // aload_0; getfield #2 (slot 0, `int`); ireturn
    let code = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    let mut fi = std::collections::HashMap::new();
    fi.insert(1usize, (0usize, b'I'));
    b.set_field_info(fi);
    let graph = b.build(&code, 5).expect("an int getfield builds");
    let despec = crate::deopt::DespecRegistry::new();
    assert!(proven_compact_withdrawn_pcs(&graph, &despec, KEY).is_empty());
    despec.insert(
        KEY,
        1,
        crate::deopt::speculation_id(1, crate::deopt::DeoptReason::ClassLoading),
    );
    assert!(
        proven_compact_withdrawn_pcs(&graph, &despec, KEY).is_empty(),
        "the layout guard's withdrawal is not the compactness guard's"
    );
    despec.insert(
        KEY,
        1,
        crate::deopt::speculation_id(1, crate::deopt::DeoptReason::SpeculationFailed),
    );
    assert!(proven_compact_withdrawn_pcs(&graph, &despec, KEY).contains(&1));
    assert!(proven_compact_withdrawn_pcs(&graph, &despec, "U.g:()V").is_empty());
}

/// Residual 1 (spliced withdrawal): a field access at a SPLICED pc (a
/// chain snapshot names the callee `plain` at its bci 7, under the
/// compiling method parked at its invoke, bci 5) is withdrawn by its own
/// chain id recorded at the call site -- not by another scope's or bci's,
/// not by the flat id at the spliced pc -- and by the call site's wildcard.
#[test]
fn a_spliced_site_is_withdrawn_by_its_chain_id_at_the_call_site() {
    use crate::deopt::{chain_compact_speculation_id, DeoptReason, SPECULATION_ID_ANY};
    use crate::ir::{SafepointSnapshot, SpliceScopeChain, SpliceScopeFrame};
    const OUTER: &str = "T.main:([Ljava/lang/String;)V";
    const CALLEE: &str = "T.plain:(LH;)I";
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let h = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let _load = g.add(Op::Load(MemKind::Int), IrType::Int, vec![c0, m0, h], Some(107));
    let mut sp = SafepointSnapshot::new(107, vec![h], vec![], vec![]);
    sp.splice_scope = Some(std::sync::Arc::new(SpliceScopeChain {
        scopes: vec![
            SpliceScopeFrame {
                class_id: 0,
                method_key: CALLEE.to_string(),
                bci: 7,
                code_len: 20,
                max_locals: 1,
                locals_len: 0,
                stack_len: 0,
            },
            SpliceScopeFrame {
                class_id: 0,
                method_key: String::new(),
                bci: 5,
                code_len: 0,
                max_locals: 0,
                locals_len: 0,
                stack_len: 0,
            },
        ],
    }));
    g.safepoints.push(sp);
    let withdrawn = |despec: &crate::deopt::DespecRegistry| {
        proven_compact_withdrawn_pcs(&g, despec, OUTER).contains(&107)
    };
    let despec = crate::deopt::DespecRegistry::new();
    assert!(!withdrawn(&despec));
    despec.insert(OUTER, 5, chain_compact_speculation_id(CALLEE, 8));
    despec.insert(OUTER, 5, chain_compact_speculation_id("T.other:()V", 7));
    despec.insert(OUTER, 107, crate::deopt::speculation_id(107, DeoptReason::SpeculationFailed));
    assert!(
        withdrawn(&despec),
        "the flat id at the spliced pc still withdraws it (the pre-residual reading)"
    );
    let despec = crate::deopt::DespecRegistry::new();
    despec.insert(OUTER, 5, chain_compact_speculation_id(CALLEE, 8));
    despec.insert(OUTER, 5, chain_compact_speculation_id("T.other:()V", 7));
    assert!(!withdrawn(&despec), "another bci's or scope's chain id leaves it");
    despec.insert(OUTER, 5, chain_compact_speculation_id(CALLEE, 7));
    assert!(withdrawn(&despec), "its own chain id at the call site withdraws it");
    assert!(
        proven_compact_withdrawn_pcs(&g, &despec, "U.g:()V").is_empty(),
        "another compiling method's registry row does not"
    );
    let wild = crate::deopt::DespecRegistry::new();
    wild.insert(OUTER, 5, SPECULATION_ID_ANY);
    assert!(withdrawn(&wild), "the call site's wildcard withdraws it too");
}



