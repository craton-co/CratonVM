use super::*;

const SHAPE: &str = "p/Shape";
const SERIAL: &str = "java/io/Serializable";

struct G {
    g: Graph,
    ctrl: NodeId,
    mem0: NodeId,
    el: NodeId,
}

/// `Shape s = (Shape) a[0]` shape: start, the element load, nothing else.
fn base() -> G {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let el = g.add(Op::ArrayLoad(MemKind::Ref), IrType::Ref, vec![ctrl, mem0, a, zero], Some(3));
    G { g, ctrl, mem0, el }
}

// Arguments are read before the `&mut` borrow (no two-phase borrow for an explicit `&mut t`).
macro_rules! cast {
    ($t:ident, $m:expr, $o:expr, $n:expr) => {{
        let (m, o) = ($m, $o);
        cast_in(&mut $t, m, o, $n)
    }};
}

fn cast_in(t: &mut G, mem: NodeId, obj: NodeId, name: &'static str) -> NodeId {
    t.g.add(
        Op::CheckCast { name_ptr: name.as_ptr() as usize, name_len: name.len() },
        IrType::Ref,
        vec![t.ctrl, mem, obj],
        Some(4),
    )
}

#[test]
fn a_cast_on_the_calls_memory_chain_proves_the_operand_plain() {
    let mut t = base();
    let c = cast!(t, t.mem0, t.el, SHAPE);
    assert!(ic_receiver_cast_excludes_arrays(&t.g, c, t.el), "forwarded operand");
    assert!(ic_receiver_cast_excludes_arrays(&t.g, t.mem0, c), "the cast itself");
    // Not on the chain (the call precedes the cast): the screen stays.
    assert!(!ic_receiver_cast_excludes_arrays(&t.g, t.mem0, t.el));
}

#[test]
fn array_admitting_or_unnamed_targets_prove_nothing() {
    let mut t = base();
    let s = cast!(t, t.mem0, t.el, SERIAL);
    assert!(!ic_receiver_cast_excludes_arrays(&t.g, s, t.el));
    assert!(!ic_receiver_cast_excludes_arrays(&t.g, t.mem0, s));
    let anon = t.g.add(
        Op::CheckCast { name_ptr: 0, name_len: 0 },
        IrType::Ref,
        vec![t.ctrl, t.mem0, t.el],
        Some(4),
    );
    assert!(!ic_receiver_cast_excludes_arrays(&t.g, anon, t.el));
}

#[test]
fn a_cast_of_another_value_proves_nothing() {
    let mut t = base();
    let other = t.g.add(Op::Param(1), IrType::Ref, vec![t.ctrl], None);
    let c = cast!(t, t.mem0, other, SHAPE);
    assert!(!ic_receiver_cast_excludes_arrays(&t.g, c, t.el));
}

#[test]
fn a_memory_phi_needs_the_cast_on_every_input() {
    let mut t = base();
    let c = cast!(t, t.mem0, t.el, SHAPE);
    let half = t.g.add(Op::Phi, IrType::Memory, vec![t.ctrl, c, t.mem0], None);
    assert!(!ic_receiver_cast_excludes_arrays(&t.g, half, t.el));
    let c2 = cast!(t, t.mem0, t.el, SHAPE);
    let both = t.g.add(Op::Phi, IrType::Memory, vec![t.ctrl, c, c2], None);
    assert!(ic_receiver_cast_excludes_arrays(&t.g, both, t.el));
    // A loop phi whose back input returns to itself still needs its entry.
    let lp = t.g.add(Op::Phi, IrType::Memory, vec![t.ctrl, t.mem0, NO_NODE], None);
    t.g.set_input(lp, 2, lp);
    assert!(!ic_receiver_cast_excludes_arrays(&t.g, lp, t.el));
    let lp2 = t.g.add(Op::Phi, IrType::Memory, vec![t.ctrl, c, NO_NODE], None);
    t.g.set_input(lp2, 2, lp2);
    assert!(ic_receiver_cast_excludes_arrays(&t.g, lp2, t.el));
}

#[test]
fn a_cast_with_an_exception_edge_proves_nothing() {
    let mut t = base();
    let c = cast!(t, t.mem0, t.el, SHAPE);
    t.g.add(Op::CallThrew { site: 0, index: 0 }, IrType::Int, vec![t.ctrl, c], None);
    assert!(!ic_receiver_cast_excludes_arrays(&t.g, c, t.el));
    assert!(!ic_receiver_cast_excludes_arrays(&t.g, t.mem0, c));
}

