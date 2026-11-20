use super::*;
use crate::deopt::{DespecRegistry, SPECULATION_ID_ANY};
use crate::ir::MemKind;

const KEY: &str = "T.sum:([II)I";

/// Nodes of [`sum_to_n`] a test asserts on.
struct Fixture {
    g: Graph,
    entry: NodeId,
    mem0: NodeId,
    arr: NodeId,
    n: NodeId,
    zero: NodeId,
    iv: NodeId,
    load: NodeId,
}

/// `static int sum(int[] a, int n) { for (int i = 0; i < n; i++) s += a[i]; }`
/// the way javac lays it out and the builder builds it: the header merge
/// at bci 10 with a snapshot there naming the induction φ, the exit test
/// `if_icmpge` (`Cmp(Ge)`, the body on its FALSE edge, which is also the
/// back edge), and `a[i]` at bci 14. With `on_the_stay_edge == false` the
/// access sits behind an in-body `if` instead.
fn sum_to_n(on_the_stay_edge: bool) -> Fixture {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(10));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0], Some(10));
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let cond = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(12));
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let (access_ctrl, back) = if on_the_stay_edge {
        (stay, stay)
    } else {
        let inner = g.add(Op::If, IrType::Control, vec![stay, n], Some(13));
        let t = g.add(Op::Proj(0), IrType::Control, vec![inner], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![inner], None);
        let join = g.add(Op::Merge, IrType::Control, vec![t, f], Some(18));
        (t, join)
    };
    let load = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![access_ctrl, mem_phi, arr, iv],
        Some(14),
    );
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(18));
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[region as usize].inputs.push(back);
    // The load's value is not live out of the loop (no accumulator φ in
    // this fixture); an `ArrayLoad` is a DCE root on its own.
    let ret = g.add(Op::Return, IrType::Int, vec![exit, iv], None);
    g.exit = ret;
    let at_header = SafepointSnapshot::new(10, vec![arr, n, iv], vec![], vec![]);
    let at_load = SafepointSnapshot::new(14, vec![arr, n, iv], vec![arr, iv], vec![]);
    g.safepoints.push(at_header);
    g.safepoints.push(at_load);
    Fixture {
        g,
        entry: c0,
        mem0: m0,
        arr,
        n,
        zero,
        iv,
        load,
    }
}

fn guards(g: &Graph) -> Vec<NodeId> {
    (0..g.nodes.len() as NodeId)
        .filter(|&id| matches!(g.nodes[id as usize].op, Op::Guard { .. }))
        .collect()
}

/// An empty registry: one `Guard(n <= a.length)` in the pre-header, its
/// `ArrayLength` reading the loop-entry memory, both at the header bci and
/// both naming a copy of the header frame with the φ at its entry value.
/// The header's own snapshot is left alone (the header merge and φs share
/// its bci).
#[test]
fn the_predicate_is_planted_in_the_pre_header_with_the_entry_frame() {
    let mut f = sum_to_n(true);
    let despec = DespecRegistry::new();
    assert_eq!(plant_range_check_predicates(&mut f.g, &despec, KEY), 1);
    let gs = guards(&f.g);
    assert_eq!(gs.len(), 1);
    let guard = &f.g.nodes[gs[0] as usize];
    assert_eq!(guard.op, Op::Guard { bci: 10 });
    assert_eq!(guard.bytecode_pc, Some(10));
    assert_eq!(guard.inputs[0], f.entry, "anchored in the pre-header");
    let cond = &f.g.nodes[guard.inputs[1] as usize];
    assert_eq!(cond.op, Op::Cmp(CmpOp::Le));
    assert_eq!(cond.inputs[0], f.n);
    let len = &f.g.nodes[cond.inputs[1] as usize];
    assert_eq!(len.op, Op::ArrayLength);
    assert_eq!(len.inputs.as_slice(), &[f.entry, f.mem0, f.arr]);
    let si = guard.frame_snapshot.expect("the guard names its frame");
    assert_eq!(len.frame_snapshot, Some(si), "so does its ArrayLength");
    let sp = &f.g.safepoints[si as usize];
    assert_eq!(sp.bci, 10);
    assert_eq!(sp.locals, vec![f.arr, f.n, f.zero], "not the phi {}", f.iv);
    assert_eq!(
        f.g.safepoints[0].locals,
        vec![f.arr, f.n, f.iv],
        "the header's own frame is untouched"
    );
}

/// The consumer half: with the predicate in, `ir_check_elim` proves the
/// body's `a[i]` in bounds; without it, it cannot.
#[test]
fn the_planted_predicate_removes_the_bounds_check() {
    let mut f = sum_to_n(true);
    let before = crate::ir_check_elim::analyze(&f.g, &crate::ir_schedule::schedule(&f.g));
    assert!(
        !before.bounds_elided(f.load),
        "no fact before the predicate"
    );
    assert_eq!(
        plant_range_check_predicates(&mut f.g, &DespecRegistry::new(), KEY),
        1
    );
    let after = crate::ir_check_elim::analyze(&f.g, &crate::ir_schedule::schedule(&f.g));
    assert!(after.bounds_elided(f.load) && after.bounds_range_proved(f.load));
}

/// Once the VM has withdrawn a speculation at the header bci, the next
/// compile plants nothing there; a withdrawal at another bci or for
/// another method changes nothing.
#[test]
fn a_de_specialised_header_gets_no_predicate() {
    let despec = DespecRegistry::new();
    despec.insert(KEY, 10, SPECULATION_ID_ANY);
    let mut f = sum_to_n(true);
    assert_eq!(plant_range_check_predicates(&mut f.g, &despec, KEY), 0);
    assert!(guards(&f.g).is_empty());
    assert_eq!(f.g.safepoints.len(), 2, "no frame copy either");

    let elsewhere = DespecRegistry::new();
    elsewhere.insert(KEY, 14, SPECULATION_ID_ANY);
    elsewhere.insert("T.other:()V", 10, SPECULATION_ID_ANY);
    let mut f = sum_to_n(true);
    assert_eq!(plant_range_check_predicates(&mut f.g, &elsewhere, KEY), 1);
}

/// An access behind an in-body branch may never run, so a failing guard
/// would not stand for a fault: nothing is planted.
#[test]
fn a_conditional_access_gets_no_predicate() {
    let mut f = sum_to_n(false);
    assert_eq!(
        plant_range_check_predicates(&mut f.g, &DespecRegistry::new(), KEY),
        0
    );
}

/// `φ(region; undef, v)` is not the trivial φ `φ(region; v, self)`: the
/// undefined edge never computed `v`, so the φ stays (patch page
/// `r11w7-ircore-ir-optimize-trivial-phi-undef-patch`). The self-edge form
/// still folds.
#[test]
fn a_phi_with_an_undefined_input_is_not_folded_to_the_other() {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let iff = g.add(Op::If, IrType::Control, vec![c0, p], None);
    let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let join = g.add(Op::Merge, IrType::Control, vec![t, f], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let v = g.add(Op::Add, IrType::Int, vec![p, one], None);
    let undef_phi = g.add(Op::Phi, IrType::Int, vec![join, NO_NODE, v], None);
    assert!(try_simplify(&g.nodes, undef_phi).is_none());

    let self_phi = g.add(Op::Phi, IrType::Int, vec![join, v], None);
    g.nodes[self_phi as usize].inputs.push(self_phi);
    assert!(matches!(
        try_simplify(&g.nodes, self_phi),
        Some(Simplified::Node(x)) if x == v
    ));
}

