use super::*;
use crate::deopt::{DespecRegistry, SPECULATION_ID_ANY};

const KEY: &str = "T.f:([II)I";

/// `for (i = 0; i < n; i += a.length) {}` laid out as javac does: the
/// header merge at bci 10 (a snapshot there naming the φ), the exit test
/// at 12, and `a.length` at bci 14 on the stay edge -- past the test, so
/// the body may run zero times. Returns `(graph, entry ctrl, array, n,
/// zero, in-body length, its user)`.
fn length_in_body() -> (Graph, NodeId, NodeId, NodeId, NodeId, NodeId, NodeId) {
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
    let cond = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(12));
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let len = g.add(Op::ArrayLength, IrType::Int, vec![stay, m0, arr], Some(14));
    let next = g.add(Op::Add, IrType::Int, vec![iv, len], Some(18));
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[region as usize].inputs.push(stay);
    let ret = g.add(Op::Return, IrType::Int, vec![exit, iv], None);
    g.exit = ret;
    g.safepoints
        .push(SafepointSnapshot::new(10, vec![arr, n, iv], vec![], vec![]));
    g.safepoints.push(SafepointSnapshot::new(
        14,
        vec![arr, n, iv],
        vec![arr],
        vec![],
    ));
    (g, c0, arr, n, zero, len, next)
}

#[test]
fn loop_entry_frame_accepts_only_allocations_defined_before_the_loop() {
    for array in [false, true] {
        for inside in [false, true] {
            for via_phi in [false, true] {
                let (mut g, entry, arr, _, _, len, next) = length_in_body();
                let region = g.nodes[next as usize].inputs[0];
                let region = g.nodes[region as usize].inputs[0];
                let memory = g.nodes[len as usize].inputs[1];
                let ctrl = if inside {
                    g.nodes[len as usize].inputs[0]
                } else {
                    entry
                };
                let op = if array {
                    Op::NewArray {
                        element_type: 10,
                        component_class_id: 0,
                    }
                } else {
                    Op::New {
                        class_id: 7,
                        num_fields: 1,
                    }
                };
                let size = g.add(Op::Const(2), IrType::Int, vec![], Some(1));
                let mut inputs = vec![ctrl, memory];
                if array {
                    inputs.push(size);
                }
                let allocation = g.add(op, IrType::Ref, inputs, Some(1));
                let value = if via_phi {
                    g.add(
                        Op::Phi,
                        IrType::Ref,
                        vec![region, allocation, allocation],
                        Some(10),
                    )
                } else {
                    allocation
                };
                g.safepoints[0].locals.push(value);
                let body = loop_body_with(
                    &g,
                    &build_users(&g),
                    region,
                    entry,
                    &[g.nodes[len as usize].inputs[0]],
                );
                let frame = loop_entry_frame(&g, region, 1, &body);
                assert_eq!(
                    frame.is_some(),
                    !inside,
                    "array={array} inside={inside} phi={via_phi}"
                );
                if let Some((_, frame)) = frame {
                    assert!(frame.preserve_allocations);
                    assert_eq!(frame.locals.last(), Some(&allocation));
                    assert_eq!(g.safepoints[0].locals.last(), Some(&value));
                    assert_eq!(frame.locals[0], arr);
                }
            }
        }
    }
}

/// One pre-header `ArrayLength` at the header bci, naming a copy of the
/// header frame with the φ at its entry value; the in-body node is gone
/// and its user reads the new one.
#[test]
fn an_in_body_length_moves_to_the_pre_header_with_the_loop_entry_frame() {
    let (mut g, entry, arr, n, zero, len, next) = length_in_body();
    assert_eq!(
        plant_length_predicates(&mut g, &DespecRegistry::new(), KEY),
        1
    );
    assert_eq!(
        g.nodes[len as usize].op,
        Op::Dead,
        "the in-body read is gone"
    );
    let new = g.nodes[next as usize].inputs[1];
    let nn = &g.nodes[new as usize];
    assert_eq!(nn.op, Op::ArrayLength);
    assert_eq!(nn.inputs[0], entry, "anchored in the pre-header");
    assert_eq!(nn.inputs[2], arr);
    assert_eq!(nn.bytecode_pc, Some(10), "deopts at the loop entry");
    let si = nn.frame_snapshot.expect("names the entry frame");
    let sp = &g.safepoints[si as usize];
    assert_eq!(sp.bci, 10);
    assert_eq!(sp.locals, vec![arr, n, zero], "the φ at its entry value");
    assert_eq!(
        g.safepoints[0].locals[2], g.nodes[next as usize].inputs[0],
        "header untouched"
    );
}

/// A guard on `a.length != 0` in the body (the `%` by the length) moves to
/// the pre-header with the planted length, at the loop entry, and the
/// node it licensed names the new guard.
#[test]
fn a_guard_on_the_planted_length_moves_to_the_pre_header_too() {
    let (mut g, entry, _, n, zero, len, next) = length_in_body();
    let stay = g.nodes[len as usize].inputs[0];
    let ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![len, zero], Some(15));
    let guard = g.add(
        Op::Guard { bci: 15 },
        IrType::Void,
        vec![stay, ne],
        Some(15),
    );
    // `next` stands in for the licensed division: the guard is its token.
    g.nodes[next as usize].inputs.push(guard);
    assert_eq!(
        plant_length_predicates(&mut g, &DespecRegistry::new(), KEY),
        2
    );
    assert_eq!(
        g.nodes[guard as usize].op,
        Op::Dead,
        "the in-body guard is gone"
    );
    let new = *g.nodes[next as usize].inputs.last().expect("the token");
    let gn = &g.nodes[new as usize];
    assert_eq!(gn.op, Op::Guard { bci: 10 });
    assert_eq!(gn.inputs[0], entry, "in the pre-header");
    assert_eq!(gn.inputs[1], ne, "the same condition");
    let cond_len = g.nodes[ne as usize].inputs[0];
    assert_eq!(
        g.nodes[cond_len as usize].inputs[0], entry,
        "over the planted length"
    );
    let si = gn.frame_snapshot.expect("the entry frame");
    assert_eq!(g.safepoints[si as usize].bci, 10);
    assert_eq!(g.safepoints[si as usize].locals[1], n);
}

/// JIT round 20 wave 5 (lane loopopt; `R20FuzzLicm1`): the zero guard of
/// a DIVISION stays at the division's bci. Without the guard token the
/// `Rem` floats ahead of the loop, and the lowering keeps its zero test
/// harmless only while a guard is anchored at the division's own bci
/// (`Lowerer::emit_div_zero_guard`); moved to the header bci, the
/// division deopted at its own bci inside the body and the interpreter
/// threw although the loop never ran.
#[test]
fn jr20_a_division_zero_guard_stays_at_its_division() {
    if crate::ir::guard_token_edges_enabled() {
        return; // the token orders the division after the moved guard
    }
    let (mut g, _, _, _, zero, len, next) = length_in_body();
    let stay = g.nodes[len as usize].inputs[0];
    let iv = g.nodes[next as usize].inputs[0];
    let ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![len, zero], Some(15));
    let guard = g.add(
        Op::Guard { bci: 15 },
        IrType::Void,
        vec![stay, ne],
        Some(15),
    );
    let _rem = g.add(Op::Rem, IrType::Int, vec![iv, len], Some(15));
    // The length moves; the division's guard does not.
    assert_eq!(
        plant_length_predicates(&mut g, &DespecRegistry::new(), KEY),
        1
    );
    assert_eq!(g.nodes[guard as usize].op, Op::Guard { bci: 15 });
    assert_eq!(g.nodes[guard as usize].inputs[0], stay, "still in the body");
}

/// Round nio20 wave 4 (lane L1, W3L4-3): a trap-form class guard on a
/// receiver defined outside the loop (`Guard(ExactClassIs(a))`) is an
/// invariant guard and leaves with the rest; a CONSTANT guard (a site
/// trap, `Guard(Const(0))`) stays where it is -- in the pre-header it
/// would deopt every entry to the loop.
#[test]
fn an_invariant_class_guard_leaves_and_a_constant_guard_stays() {
    let (mut g, entry, arr, _, zero, len, _) = length_in_body();
    let stay = g.nodes[len as usize].inputs[0];
    let exact = g.add(
        Op::ExactClassIs { class_id: 9 },
        IrType::Int,
        vec![arr],
        Some(16),
    );
    let class_guard = g.add(
        Op::Guard { bci: 16 },
        IrType::Void,
        vec![stay, exact],
        Some(16),
    );
    let site_trap = g.add(
        Op::Guard { bci: 17 },
        IrType::Void,
        vec![stay, zero],
        Some(17),
    );
    // The length and the class guard; never the site trap.
    assert_eq!(
        plant_length_predicates(&mut g, &DespecRegistry::new(), KEY),
        2
    );
    assert_eq!(
        g.nodes[class_guard as usize].op,
        Op::Dead,
        "the class guard moved"
    );
    let moved: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| n.op == (Op::Guard { bci: 10 }) && n.inputs.first() == Some(&entry))
        .collect();
    let [moved] = moved.as_slice() else {
        panic!("one pre-header guard, got {}", moved.len());
    };
    assert_eq!(moved.inputs[1], exact);
    assert_eq!(
        g.nodes[site_trap as usize].op,
        Op::Guard { bci: 17 },
        "the trap stays"
    );
    assert_eq!(g.nodes[site_trap as usize].inputs[0], stay);
}

/// A speculation withdrawn at the header withdraws the predicate, and a
/// second run finds nothing left to move.
#[test]
fn a_despecialised_header_keeps_the_length_in_the_body() {
    let (mut g, _, _, _, _, len, _) = length_in_body();
    let despec = DespecRegistry::new();
    despec.insert(KEY, 10, SPECULATION_ID_ANY);
    assert_eq!(plant_length_predicates(&mut g, &despec, KEY), 0);
    assert_eq!(g.nodes[len as usize].op, Op::ArrayLength);
    assert_eq!(g.safepoints.len(), 2, "no snapshot left behind");
    let (mut g, _, _, _, _, _, _) = length_in_body();
    assert_eq!(
        plant_length_predicates(&mut g, &DespecRegistry::new(), KEY),
        1
    );
    assert_eq!(
        plant_length_predicates(&mut g, &DespecRegistry::new(), KEY),
        0
    );
}
