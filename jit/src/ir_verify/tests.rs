use super::*;
use crate::ir::{Graph, IrType, MemKind, Op};

/// `Graph::empty` with `exit` spelled `0`, as this module's fixtures
/// always had it (the unbuilt / terminatorless rules read that value).
/// Built through the constructor so a change to `Graph`'s fields is not a
/// change to every fixture
/// (`r11-irback-use-list-invariant-enforced-by-convention-only`, step 2).
fn exit0_graph() -> Graph {
    let mut g = Graph::empty(0);
    g.exit = 0;
    g
}

/// t1 wave 2 (lane Y): `Graph::add` stores an `int` constant
/// sign-extended (a `long` keeps its bits), and the width check rejects an
/// `int` constant rewritten in place with its unsigned 32-bit pattern.
#[test]
fn t1y_int_constants_are_sign_extended_and_checked() {
    let mut g = linear_graph();
    let m1 = g.add(Op::Const(0xFFFF_FFFF), IrType::Int, vec![], None);
    assert_eq!(g.nodes[m1 as usize].op, Op::Const(-1));
    let min = g.add(Op::Const(0x8000_0000), IrType::Int, vec![], None);
    assert_eq!(g.nodes[min as usize].op, Op::Const(i64::from(i32::MIN)));
    let wide = g.add(Op::Const(0xFFFF_FFFF), IrType::Long, vec![], None);
    assert_eq!(g.nodes[wide as usize].op, Op::Const(0xFFFF_FFFF));
    let mut v = Violations::new();
    check_int_const_width(&g, &mut v);
    assert!(v.is_empty(), "every add-built constant is canonical");
    // An in-place rewrite bypasses `add`; the check names it.
    g.nodes[m1 as usize].op = Op::Const(0xFFFF_FFFF);
    let mut v = Violations::new();
    check_int_const_width(&g, &mut v);
    assert!(!v.is_empty(), "a raw unsigned int pattern must be reported");
}

/// A minimal well-formed graph: `Start → Proj(0) → Return(Const)`.
fn linear_graph() -> Graph {
    let mut g = exit0_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let k = g.add(Op::Const(7), IrType::Int, vec![], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, k], None);
    g.entry = start;
    g.exit = ret;
    g
}

/// `Start → If → (Proj, Proj) → Merge → Phi → Return(phi)`.
fn diamond_graph() -> Graph {
    let mut g = exit0_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
    let if_node = g.add(Op::If, IrType::Control, vec![ctrl, cond], None);
    let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
    let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Int, vec![], None);
    let merge = g.add(Op::Merge, IrType::Control, vec![t, f], None);
    let phi = g.add(Op::Phi, IrType::Int, vec![merge, a, b], None);
    let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], None);
    g.entry = start;
    g.exit = ret;
    g
}

fn message(err: &Bailout) -> String {
    match &err.reason {
        BailoutReason::IrVerification(m) => m.clone(),
        other => panic!("expected IrVerification, got {other:?}"),
    }
}

#[test]
fn valid_linear_graph_verifies_clean() {
    let g = linear_graph();
    assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());
}

#[test]
fn valid_diamond_graph_verifies_clean() {
    let g = diamond_graph();
    let r = verify_graph(&g, "test", VerifyOptions::all());
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

/// A control join with no predecessors is the orphan `ir.rs`'s own pre-scan
/// comment calls fatal ("an input-less control node is exactly the kind of
/// orphan the scheduler/lowerer is not prepared for"). It used to verify.
#[test]
fn an_input_less_merge_is_rejected() {
    let mut g = diamond_graph();
    let merge = g.nodes[g.exit as usize].inputs[0];
    g.nodes[merge as usize].inputs.clear();
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("at least 1"), "{m}");
}

/// A `Region` is a LOOP HEADER: entry edge plus back edge. One predecessor
/// means an edge was dropped — which is precisely what
/// `IrBuilder::patch_loop_backedge`'s silent `None => return` produced, and
/// why that shape was invisible to this module until 2026-09-17.
#[test]
fn a_loop_region_with_a_single_predecessor_is_rejected() {
    let mut g = diamond_graph();
    let merge = g.nodes[g.exit as usize].inputs[0];
    g.nodes[merge as usize].op = Op::Region;
    g.nodes[merge as usize].inputs.pop();
    // The φ is trimmed to match, so `check_phis` stays quiet and the arity
    // lane is demonstrably the one that speaks.
    let phi = g.nodes[g.exit as usize].inputs[1];
    g.nodes[phi as usize].inputs.pop();
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("at least 2"), "{m}");
    assert!(
        !m.contains("value input(s)"),
        "the phi lane must be quiet — this is an arity finding: {m}"
    );
}

/// A one-predecessor `Merge` stays legal: branch folding collapses a
/// two-way join into a pass-through before anything re-canonicalises it.
///
/// Round 11 wave 6: the fold is modelled whole — the `If` and BOTH of its
/// projections go, and the join is fed by the `If`'s own control input.
/// The fixture used to pop only the join's second input, leaving the
/// dropped arm's `Proj(1)` live and consumed by nothing, which the
/// branch-completeness lane rightly calls "a branch arm that leads
/// nowhere" (the one failure of the wave-5 `CRATONVM_JIT_VERIFY_BRANCHES=1`
/// evidence run). Both halves are pinned below with the lanes forced.
#[test]
fn a_merge_that_collapsed_to_one_predecessor_still_verifies() {
    let forced = |g: &Graph| {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_VERIFY_BRANCHES", Some("1"))],
            || verify_graph(g, "test", VerifyOptions::all()),
        )
    };
    let mut g = diamond_graph();
    let merge = g.nodes[g.exit as usize].inputs[0];
    let phi = g.nodes[g.exit as usize].inputs[1];
    let t = g.nodes[merge as usize].inputs[0];
    let f = g.nodes[merge as usize].inputs[1];
    let if_node = g.nodes[t as usize].inputs[0];
    let entry_ctrl = g.nodes[if_node as usize].inputs[0];

    // Half a fold: the join and the φ lose the false arm, the arm stays.
    // `diamond_graph` is deterministic, so the ids above name the same
    // nodes in a second copy (`Graph` is not `Clone`).
    let mut half = diamond_graph();
    half.nodes[merge as usize].inputs.pop();
    half.nodes[phi as usize].inputs.pop();
    let m = message(&forced(&half).unwrap_err());
    assert!(m.contains("leads nowhere"), "{m}");

    // The whole fold.
    g.nodes[merge as usize].inputs.pop();
    g.nodes[merge as usize].inputs[0] = entry_ctrl;
    g.nodes[phi as usize].inputs.pop();
    for dead in [t, f, if_node] {
        g.kill(dead);
    }
    let r = verify_graph(&g, "test", VerifyOptions::all());
    assert!(r.is_ok(), "{}", r.unwrap_err());
    let r = forced(&g);
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

/// `ir_lower` selects an `If`'s taken edge by *matching* `Op::Proj(0)`
/// among its users, so an index that names no output leaves a control edge
/// unlowered.
#[test]
fn a_projection_index_must_name_an_output_its_producer_has() {
    let mut g = diamond_graph();
    let t = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Proj(0) && n.ty == IrType::Control)
        .expect("a control projection");
    // Re-point it at the If it already projects, with an index that If
    // does not have.
    g.nodes[t].op = Op::Proj(7);
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("selects output 7"), "{m}");
}

/// …and two `Proj`s naming the same output make "the taken edge" whichever
/// one the scan reaches first.
#[test]
fn two_projections_may_not_name_the_same_output() {
    let mut g = diamond_graph();
    let f = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Proj(1) && n.ty == IrType::Control)
        .expect("the fall-through projection");
    g.nodes[f].op = Op::Proj(0);
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("second Proj(0)"), "{m}");
}

// ── The guard token edge ─────────────────────

/// The `Start` preamble a guarded graph needs, with nothing appended yet.
/// Returns `(graph, start, ctrl, mem)`. The `Op::Return` is added by each
/// test LAST, so arena order holds and `VerifyOptions::all()` — which
/// includes the arena-order lane — is what these tests can use.
fn guarded_head() -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = exit0_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    g.entry = start;
    (g, start, ctrl, mem)
}

/// Close a [`guarded_head`] graph off with `Return(value)`.
fn finish(g: &mut Graph, ctrl: NodeId, value: NodeId) {
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, value], None);
    g.exit = ret;
}

/// `expected_arity` admits the guarded shape of every op `ir::guard_shape`
/// names, and admits exactly one extra input — never two.
///
/// What this catches: an op added to `ir::Op::guard_shape` without the
/// matching arity widening here (the guarded node is then rejected by the
/// arity lane, which in production is a silent fall-back to the
/// single-pass tier), and the reverse — an arity loosened past what the
/// token slot explains.
#[test]
fn the_guarded_arities_admit_exactly_the_token_slot() {
    for op in [
        Op::Load(MemKind::Int),
        Op::ArrayLoad(MemKind::Byte),
        Op::ArrayLength,
        Op::Div,
        Op::Rem,
    ] {
        let shape = op.guard_shape().expect("a guarded op");
        let (min, max) = expected_arity(&op);
        assert!(
            min <= shape.unguarded_arity,
            "{op:?}: the unguarded form must still verify"
        );
        assert_eq!(
            max,
            shape.unguarded_arity + 1,
            "{op:?}: the arity lane must admit the token slot and nothing past it"
        );
    }
    // The store family carries no token, so its arity must NOT have room
    // for one — an extra operand there would be read as the stored value.
    for op in [Op::Store(MemKind::Int), Op::ArrayStore(MemKind::Int)] {
        assert_eq!(op.guard_shape(), None, "{op:?}");
        assert_eq!(expected_arity(&op), (5, 5), "{op:?}");
    }
}

/// A guarded `Load` / `ArrayLoad` / `ArrayLength` verifies clean on every
/// lane.
///
/// **What this catches, and why it is the test that matters.** Drop the
/// guard-token filter in `data_input_indices` and this fails with "is a
/// Void token used as a data value": `check_types` classifies slots `2..`
/// of a memory access as data, and it runs in every build profile at
/// `PHASE_POST_OPTIMIZE`. In production that finding is not a failing test
/// — it makes `ir_verify_reject` true and the method quietly falls back
/// to the single-pass backend. `NOTES-opts7.md`'s recipe for this change
/// did not mention `data_input_indices` at all.
#[test]
fn a_guarded_memory_access_verifies_clean() {
    let (mut g, start, ctrl, mem) = guarded_head();
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let cond = g.add(
        Op::Cmp(crate::ir::CmpOp::Ne),
        IrType::Int,
        vec![base, null],
        None,
    );
    let guard = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, cond], None);
    let off = g.add(Op::Const(3), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, off, guard],
        None,
    );
    let al = g.add(
        Op::ArrayLoad(MemKind::Byte),
        IrType::Int,
        vec![ctrl, load, base, off, guard],
        None,
    );
    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![ctrl, al, base, guard],
        None,
    );
    finish(&mut g, ctrl, len);
    let r = verify_graph(&g, "test", VerifyOptions::all());
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

/// A guarded `Div` verifies clean.
///
/// What this catches: `Op::Div`/`Op::Rem` left inside the `(2, 2)`
/// arithmetic group of `expected_arity`, or inside the homogeneous group
/// of `ir::expected_input_type`. Both are the failure `NOTES-opts7.md`
/// calls "the one that will not show up in a unit test" — because the
/// div-zero token exists only on graphs built from real bytecode. This IS
/// that unit test: it builds the shape by hand.
#[test]
fn a_guarded_division_verifies_clean() {
    let (mut g, _start, ctrl, _mem) = guarded_head();
    let a = g.add(Op::Const(12), IrType::Int, vec![], None);
    let b = g.add(Op::Const(4), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let nz = g.add(
        Op::Cmp(crate::ir::CmpOp::Ne),
        IrType::Int,
        vec![b, zero],
        None,
    );
    let guard = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, nz], None);
    let div = g.add(Op::Div, IrType::Int, vec![a, b, guard], None);
    finish(&mut g, ctrl, div);
    let r = verify_graph(&g, "test", VerifyOptions::all());
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

/// A value sitting in the guard-token slot is reported.
///
/// What this catches: a producer that appends an ordinary operand to a
/// node of the guarded family. Nothing downstream would miscompile it —
/// every consumer reads the base and offset out of fixed slots and the
/// type lane now skips the trailing one — so the operand would simply be
/// dropped, invisibly, which is why the lane exists.
#[test]
fn a_non_guard_in_the_guard_token_slot_is_reported() {
    let (mut g, start, ctrl, mem) = guarded_head();
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(3), IrType::Int, vec![], None);
    let stray = g.add(Op::Const(9), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, off, stray],
        None,
    );
    finish(&mut g, ctrl, load);
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("guard-token slot"), "{m}");
}

/// The unguarded shapes are untouched: the token slot is identified by
/// arity, so widening the arity range must not make the documented form
/// look guarded — nor must it stop the compact hand-built `ArrayLength`
/// form verifying.
#[test]
fn an_unguarded_access_still_verifies_and_names_no_token() {
    let (mut g, start, ctrl, mem) = guarded_head();
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(3), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, off],
        None,
    );
    let compact_len = g.add(Op::ArrayLength, IrType::Int, vec![base], None);
    let sum = g.add(Op::Add, IrType::Int, vec![load, compact_len], None);
    finish(&mut g, ctrl, sum);
    assert_eq!(crate::ir::guard_token_slot(&g.nodes[load as usize]), None);
    assert_eq!(
        crate::ir::guard_token_slot(&g.nodes[compact_len as usize]),
        None
    );
    let r = verify_graph(&g, "test", VerifyOptions::all());
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

// ── The staged operand-type lane ─────────────────────────────────────
//
// Called directly rather than through `verify_graph`, because the lane is
// gated on an explicit `CRATONVM_JIT_VERIFY_TYPES=1` that is latched on
// first read — a test that set it would be a test that depends on which
// other test ran first.

fn operand_findings(g: &Graph) -> Option<String> {
    let mut v = Violations::new();
    check_operand_types(g, &mut v);
    v.into_result("test").err().map(|e| message(&e))
}

/// The defect `NOTES-irfront.md` §4 opens with: `Op::Cmp` is the IR's
/// `if_icmp*` **and** its `if_acmp*`, so it has no fixed operand type and
/// `check_types` therefore required nothing of it — while `ir_lower`
/// selects a 64-bit signed `CMP` whenever either operand is `Ref` or
/// `Long`. One wrongly-`Long`-typed operand inverts an `if_icmplt`,
/// because ints are stored zero-extended and a negative int reads as a
/// large positive 64-bit value.
#[test]
fn a_comparison_of_an_int_against_a_long_is_reported() {
    let mut g = linear_graph();
    let i = g.add(Op::Const(1), IrType::Int, vec![], None);
    let l = g.add(Op::Const(2), IrType::Long, vec![], None);
    let cmp = g.add(Op::Cmp(crate::ir::CmpOp::Lt), IrType::Int, vec![i, l], None);
    g.nodes[g.exit as usize].inputs[1] = cmp;
    let m = operand_findings(&g).expect("a mixed-width compare is a finding");
    assert!(m.contains("input[1]"), "{m}");
    assert!(m.contains("long"), "{m}");
}

/// …and the homogeneous case verifies, at either width. The requirement is
/// sameness, not a particular type — a reference comparison is exactly as
/// legal as an integer one.
#[test]
fn a_comparison_of_two_operands_of_one_type_is_clean() {
    for ty in [IrType::Int, IrType::Long, IrType::Ref] {
        let mut g = linear_graph();
        let a = g.add(Op::Const(1), ty, vec![], None);
        let b = g.add(Op::Const(2), ty, vec![], None);
        let cmp = g.add(Op::Cmp(crate::ir::CmpOp::Eq), IrType::Int, vec![a, b], None);
        g.nodes[g.exit as usize].inputs[1] = cmp;
        assert_eq!(operand_findings(&g), None, "{ty:?}");
    }
}

/// `ScalarOp::input_ty` is documented in `ir.rs` as "the single source of
/// truth" for each intrinsic family's operand types, and nothing consulted
/// it: `ScalarOp::MinL` fed two `Int` nodes verified clean and lowered to a
/// 64-bit `CMP`/`CMOV` over two 32-bit values.
#[test]
fn a_long_intrinsic_fed_int_operands_is_reported() {
    let mut g = linear_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Int, vec![], None);
    let min = g.add(
        Op::ScalarIntrinsic(crate::ir::ScalarOp::MinL),
        IrType::Long,
        vec![a, b],
        None,
    );
    g.nodes[g.exit as usize].inputs[1] = min;
    let m = operand_findings(&g).expect("MinL over two ints is a finding");
    assert!(m.contains("requires long"), "{m}");
}

/// The distance operand of a rotate is an `int` at both widths, which is
/// the asymmetry `ScalarOp::input_ty` exists to record: `Long.rotateLeft`
/// takes a `long` value and an `int` distance.
#[test]
fn a_long_rotate_takes_a_long_value_and_an_int_distance() {
    let mut g = linear_graph();
    let v = g.add(Op::Const(1), IrType::Long, vec![], None);
    let d = g.add(Op::Const(3), IrType::Int, vec![], None);
    let rot = g.add(
        Op::ScalarIntrinsic(crate::ir::ScalarOp::RotateLeftL),
        IrType::Long,
        vec![v, d],
        None,
    );
    g.nodes[g.exit as usize].inputs[1] = rot;
    assert_eq!(operand_findings(&g), None);

    // …and the two swapped is a finding on BOTH slots.
    let mut g = linear_graph();
    let v = g.add(Op::Const(1), IrType::Int, vec![], None);
    let d = g.add(Op::Const(3), IrType::Long, vec![], None);
    let rot = g.add(
        Op::ScalarIntrinsic(crate::ir::ScalarOp::RotateLeftL),
        IrType::Long,
        vec![v, d],
        None,
    );
    g.nodes[g.exit as usize].inputs[1] = rot;
    let m = operand_findings(&g).expect("swapped operands are a finding");
    assert!(m.contains("input[0]"), "{m}");
    assert!(m.contains("input[1]"), "{m}");
}

/// A `Ref` where an array index belongs, and an `Int` where the base
/// belongs. Neither was required of `Op::ArrayLoad` before the table.
#[test]
fn an_array_access_needs_a_reference_base_and_an_int_index() {
    let mut g = linear_graph();
    let ctrl = g.nodes[g.exit as usize].inputs[0];
    let mem = 2; // the Start's memory projection
    let base = g.add(Op::Const(0), IrType::Int, vec![], None);
    let index = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let load = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, index],
        None,
    );
    g.nodes[g.exit as usize].inputs[1] = load;
    let m = operand_findings(&g).expect("a swapped base and index is a finding");
    assert!(m.contains("input[2]"), "{m}");
    assert!(m.contains("requires reference"), "{m}");
    assert!(m.contains("input[3]"), "{m}");
}

/// The lane makes no claim about a call's arguments (their types are the
/// callee descriptor's, which no `&Op` carries) or about a φ's inputs (the
/// join lattice in `check_types` is that requirement). Both are shapes the
/// table knowingly cannot describe, and it answers `TypeReq::Any` rather
/// than guessing — the distinction that keeps a modelling gap from
/// becoming a fleet-wide fallback.
#[test]
fn the_operand_lane_makes_no_claim_about_call_arguments_or_phi_inputs() {
    let mut g = linear_graph();
    let ctrl = g.nodes[g.exit as usize].inputs[0];
    let mem = 2;
    let a = g.add(Op::Const(1), IrType::Ref, vec![], None);
    let b = g.add(Op::Const(2), IrType::Double, vec![], None);
    let call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Int,
        vec![ctrl, mem, a, b],
        None,
    );
    g.nodes[g.exit as usize].inputs[1] = call;
    assert_eq!(operand_findings(&g), None);
}

/// Only `Start` and `If` are multi-output, so a projection of anything else
/// names an output that does not exist.
#[test]
fn a_projection_of_a_single_output_node_is_rejected() {
    let mut g = linear_graph();
    let k = g.nodes[g.exit as usize].inputs[1];
    let bad = g.add(Op::Proj(0), IrType::Int, vec![k], None);
    g.nodes[g.exit as usize].inputs[1] = bad;
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("produces no projections"), "{m}");
}

/// The branch-completeness lane, driven directly so the test does not
/// depend on the ambient `CRATONVM_JIT_VERIFY_BRANCHES` / build profile.
fn branch_findings(g: &Graph) -> Vec<String> {
    let mut v = Violations::new();
    check_branch_completeness(g, &mut v);
    v.list
}

/// A diamond and a dense switch keep every arm; a diamond whose false
/// projection was killed (with the merge and φ trimmed to match, so every
/// OTHER lane is quiet — the shape DCE left behind for an infinite-loop
/// arm) is reported, as is a projection that survives but leads nowhere.
#[test]
fn a_branch_arm_with_no_successor_is_reported_by_the_branch_lane() {
    assert!(branch_findings(&diamond_graph()).is_empty());

    // Dense switch on a parameter: low 0, high 1 → 3 projections.
    let mut s = linear_graph();
    let ctrl = s.nodes[s.exit as usize].inputs[0];
    let key = s.add(Op::Param(0), IrType::Int, vec![], None);
    let sw = s.add(
        Op::Switch { low: 0, high: 1 },
        IrType::Control,
        vec![ctrl, key],
        None,
    );
    let arms: Vec<NodeId> = (0..3u16)
        .map(|k| s.add(Op::Proj(k), IrType::Control, vec![sw], None))
        .collect();
    let join = s.add(Op::Merge, IrType::Control, arms.clone(), None);
    s.nodes[s.exit as usize].inputs[0] = join;
    assert!(branch_findings(&s).is_empty(), "{:?}", branch_findings(&s));

    // Diamond with the false arm deleted.
    let mut g = diamond_graph();
    let merge = g.nodes[g.exit as usize].inputs[0];
    let f = g.nodes[merge as usize].inputs[1];
    g.nodes[merge as usize].inputs.pop();
    let phi = g.nodes[g.exit as usize].inputs[1];
    g.nodes[phi as usize].inputs.pop();
    assert!(
        verify_graph(&g, "test", VerifyOptions::all()).is_ok()
            || branch_completeness_lane_enabled("test"),
        "every pre-existing lane accepts the one-armed diamond — which is the gap",
    );
    let with_dangling_proj = branch_findings(&g);
    assert!(
        with_dangling_proj
            .iter()
            .any(|m| m.contains("leads nowhere")),
        "{with_dangling_proj:?}"
    );
    g.kill(f);
    let with_no_proj = branch_findings(&g);
    assert!(
        with_no_proj.iter().any(|m| m.contains("no successor")),
        "{with_no_proj:?}"
    );
}

/// The type lane is the one that catches a reference merged into an
/// `Int`-typed φ — the shape that is neither zero-initialised by
/// `ir_lower::zero_ref_phi_slots` nor reported to the collector. The module
/// header has argued for years that this module "names it rather than
/// letting it reach the lowerer"; until 2026-09-17 it did not, in any
/// release build, because the lane followed `cfg!(debug_assertions)`.
#[test]
fn the_type_lane_runs_at_post_optimize_in_every_build_profile() {
    assert!(
        VerifyOptions::for_phase(PHASE_POST_OPTIMIZE).check_types,
        "the cheapest lane, and the only one that sees a GC-unsound φ, must \
         not depend on the build profile at the hook where the graph is \
         known clean",
    );
}

/// The exception a `Throw` carries is a value, and it was in no lane at
/// all: `data_input_indices` did not list `Op::Throw`, so nothing required
/// the operand `helpers.throw_exception` receives as an oop to be one.
#[test]
fn a_control_token_used_as_a_thrown_exception_is_rejected() {
    let mut g = linear_graph();
    let ctrl = g.nodes[g.exit as usize].inputs[0];
    let mem = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Proj(1)))
        .expect("memory projection") as NodeId;
    // `[ctrl, mem, exc]` with the control token standing in for the oop.
    let throw = g.add(Op::Throw, IrType::Void, vec![ctrl, mem, ctrl], None);
    g.exit = throw;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("control token used as a data value"), "{m}");
}

#[test]
fn dangling_input_is_rejected() {
    let mut g = linear_graph();
    // Point the Return's value at a node that does not exist.
    let ret = g.exit as usize;
    g.nodes[ret].inputs[1] = 999;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("out of range"), "{m}");
    assert!(m.contains("n999"), "{m}");
    assert_eq!(err.category(), "ir_verification");
    assert_eq!(err.context.as_deref(), Some("phase=test"));
}

#[test]
fn input_referring_to_a_removed_node_is_rejected() {
    let mut g = linear_graph();
    let ret = g.exit;
    let val = g.nodes[ret as usize].inputs[1];
    g.kill(val);
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("removed (Dead) node"), "{m}");
}

#[test]
fn no_node_outside_a_phi_is_rejected() {
    let mut g = linear_graph();
    let ret = g.exit as usize;
    g.nodes[ret].inputs[1] = NO_NODE;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    assert!(message(&err).contains("NO_NODE"), "{}", message(&err));
}

#[test]
fn no_node_inside_a_phi_is_allowed() {
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    g.nodes[phi as usize].inputs[1] = NO_NODE;
    // Structural + frame-state lanes must accept an undefined predecessor
    // value; only the type lane has an opinion, and it skips NO_NODE too.
    assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());
}

#[test]
fn phi_arity_mismatch_is_rejected() {
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    // Drop one value input: the merge still has two predecessors, so the
    // second edge would emit no parallel copy.
    g.nodes[phi as usize].inputs.pop();
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("value input"), "{m}");
    assert!(m.contains("predecessor"), "{m}");
}

#[test]
fn phi_not_anchored_at_a_merge_is_rejected() {
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    g.nodes[phi as usize].inputs[0] = g.entry;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    assert!(
        message(&err).contains("anchored at a Merge/Region"),
        "{}",
        message(&err)
    );
}

#[test]
fn type_inconsistent_phi_is_rejected() {
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    // Make one incoming value a reference while the other stays an int.
    let a = g.nodes[phi as usize].inputs[1];
    g.nodes[a as usize].ty = IrType::Ref;
    let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("incompatible input types"), "{m}");
    assert!(m.contains("reference"), "{m}");
    // The message names the fallback the builder would have recorded, so a
    // reader does not have to know `ir::PHI_TYPE_FALLBACK` by heart.
    assert!(m.contains(type_name(crate::ir::PHI_TYPE_FALLBACK)), "{m}");
    // …and the structural lane alone must NOT reject it: this is a type
    // defect, not a shape defect.
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
}

/// The unified lattice's headline consequence: `ir::join_data_type` rejects
/// an `Int`/`Long` merge, and the verifier — which now folds that exact
/// function — must reject it too. Under the old private `Cat` lattice both
/// were the one `Integer` category and this graph verified clean, so the
/// type lane was proving something weaker than the compiler enforces.
#[test]
fn int_and_long_no_longer_share_a_category() {
    assert!(
        crate::ir::join_data_type(IrType::Int, IrType::Long).is_none(),
        "the compiler's lattice must reject int/long for this lane to"
    );
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    let a = g.nodes[phi as usize].inputs[1];
    g.nodes[a as usize].ty = IrType::Long;
    let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("incompatible input types"), "{m}");
    assert!(m.contains("long"), "{m}");
    assert!(m.contains("int"), "{m}");
}

/// The other half of the same unification: `Float`/`Double` were one
/// "floating-point" category and are now distinct.
#[test]
fn float_and_double_no_longer_share_a_category() {
    assert!(crate::ir::join_data_type(IrType::Float, IrType::Double).is_none());
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    g.nodes[phi as usize].ty = IrType::Float;
    let a = g.nodes[phi as usize].inputs[1];
    let b = g.nodes[phi as usize].inputs[2];
    g.nodes[a as usize].ty = IrType::Float;
    g.nodes[b as usize].ty = IrType::Double;
    let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("incompatible input types"), "{m}");
    assert!(m.contains("double"), "{m}");
}

/// The φ fallback, conflict half: a real type conflict IS a violation, and
/// the always-on lane must still accept the shape. See the module docs.
#[test]
fn phi_type_fallback_on_a_conflict_is_a_violation() {
    assert_eq!(crate::ir::PHI_TYPE_FALLBACK, IrType::Int);
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    // Exactly the shape `phi_data_type` cannot type: a Ref and an Int
    // merging, with the φ carrying the `Int` fallback the builder recorded.
    let a = g.nodes[phi as usize].inputs[1];
    g.nodes[a as usize].ty = IrType::Ref;
    assert_eq!(g.nodes[phi as usize].ty, crate::ir::PHI_TYPE_FALLBACK);
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    assert!(verify_graph(&g, "test", VerifyOptions::default()).is_err());
}

/// The φ fallback, degenerate half: a φ no input of which carries a type is
/// a slot that is dead at the merge, which verified bytecode may have — the
/// always-on lane already accepts `NO_NODE` there. Tolerated: the
/// fallback's `Int` keeps it out of the oop map, the right answer for a slot
/// nothing reads.
#[test]
fn phi_with_no_typed_input_is_tolerated() {
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    let n = g.nodes[phi as usize].inputs.len();
    for i in 1..n {
        g.nodes[phi as usize].inputs[i] = NO_NODE;
    }
    assert_eq!(g.nodes[phi as usize].ty, crate::ir::PHI_TYPE_FALLBACK);
    let r = verify_graph(&g, "test", VerifyOptions::all());
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

#[test]
fn phi_typed_against_its_join_is_rejected() {
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    // Both inputs are references, but the phi claims to be an int — the
    // `phi_data_type` defect the review names.
    for i in 1..g.nodes[phi as usize].inputs.len() {
        let src = g.nodes[phi as usize].inputs[i];
        g.nodes[src as usize].ty = IrType::Ref;
    }
    let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("typed int but joins reference"), "{m}");
}

#[test]
fn reference_flowing_into_integer_arithmetic_is_rejected() {
    let mut g = linear_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Ref, vec![], None);
    let sum = g.add(Op::Add, IrType::Int, vec![a, b], None);
    let ret = g.exit as usize;
    g.nodes[ret].inputs[1] = sum;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("produces int"), "{m}");
    assert!(m.contains("reference"), "{m}");
    // Structural-only must accept it: nothing about the *shape* is wrong.
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
}

#[test]
fn shift_amount_may_be_narrower_than_the_shifted_value() {
    let mut g = linear_graph();
    let a = g.add(Op::Const(1), IrType::Long, vec![], None);
    let b = g.add(Op::Const(2), IrType::Int, vec![], None);
    let sh = g.add(Op::Shl, IrType::Long, vec![a, b], None);
    let ret = g.exit as usize;
    g.nodes[ret].inputs[1] = sh;
    // Not `all()`: the appended nodes outrank the Return in arena order,
    // which is exactly what the (opt-in, heuristic) arena-order lane
    // reports. `Shl` is checked only on operand 0, so the `Int` shift
    // amount against a `Long` result is not an Int/Long non-join finding.
    let r = verify_graph(&g, "test", VerifyOptions::default());
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

#[test]
fn wrong_arity_is_rejected() {
    let mut g = linear_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let bad = g.add(Op::Add, IrType::Int, vec![a], None);
    let ret = g.exit as usize;
    g.nodes[ret].inputs[1] = bad;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("exactly 2"), "{m}");
}

#[test]
fn two_start_nodes_are_rejected() {
    let mut g = linear_graph();
    g.add(Op::Start, IrType::Control, vec![], None);
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    assert!(message(&err).contains("single entry"), "{}", message(&err));
}

#[test]
fn control_taken_from_a_data_node_is_rejected() {
    let mut g = linear_graph();
    let k = g.add(Op::Const(3), IrType::Int, vec![], None);
    let ret = g.exit as usize;
    g.nodes[ret].inputs[0] = k;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    assert!(
        message(&err).contains("produces no control token"),
        "{}",
        message(&err)
    );
}

#[test]
fn severed_terminator_is_rejected() {
    let mut g = linear_graph();
    // Re-anchor the Return on a Merge that nothing reaches.
    let orphan = g.add(Op::Merge, IrType::Control, vec![], None);
    let ret = g.exit as usize;
    g.nodes[ret].inputs[0] = orphan;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    assert!(
        message(&err).contains("reachable from the entry"),
        "{}",
        message(&err)
    );
}

/// A store killed without splicing the memory chain leaves the surviving
/// store's token slot naming a removed node. That is an *ordering* defect,
/// not a wrong-value one: the always-on structural lane must not reject it
/// (see `is_memory_token_input`), and the memory-chain lane must.
///
/// `ir_optimize::eliminate_dead_stores` no longer produces this — it routes
/// every deletion through `kill_store_splicing_memory_chain` — which is
/// exactly why the lane is now safe to enable at `"post-optimize"`. The
/// shape is built by hand here because the pass that used to emit it does
/// not any more.
#[test]
fn removed_memory_token_is_a_chain_finding_not_a_structural_one() {
    let mut g = linear_graph();
    let ctrl = g.nodes[g.exit as usize].inputs[0];
    let mem = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Proj(1)))
        .expect("memory projection") as NodeId;
    let base = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let val = g.add(Op::Const(1), IrType::Int, vec![], None);
    let st1 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, base, off, val],
        None,
    );
    let st2 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, st1, base, off, val],
        None,
    );
    assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());
    g.kill(st1);
    // Structural / type / frame-state lanes: unchanged verdict.
    let lanes_without_the_chain = VerifyOptions {
        check_memory_chain: false,
        ..VerifyOptions::default()
    };
    let r = verify_graph(&g, "test", lanes_without_the_chain);
    assert!(r.is_ok(), "{}", r.unwrap_err());
    // Memory-chain lane: names it.
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("memory token"), "{m}");
    assert!(m.contains(&format!("n{st1}")), "{m}");
    let _ = st2;
}

/// `Op::ArrayLength` is `[ctrl, mem, array_ref]` per `ir::Op`, so its input
/// 1 is a memory token. `ir_optimize::memory_token_slot` has always said so;
/// this lane used to omit it, which meant a removed token in an
/// `ArrayLength` was reported by the *structural* lane as a wrong-value read
/// — the one classification the structural lane may not get wrong, because
/// it is the lane with no opt-out.
#[test]
fn array_length_input_one_is_a_memory_token() {
    let mut g = linear_graph();
    let ctrl = g.nodes[g.exit as usize].inputs[0];
    let mem = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Proj(1)))
        .expect("memory projection") as NodeId;
    let arr = g.add(Op::Const(0), IrType::Ref, vec![], None);
    // A well-typed store (`Int` offset and value), so the operand type
    // lane has nothing to say and the assertion below is about the chain.
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let val = g.add(Op::Const(1), IrType::Int, vec![], None);
    let st = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, arr, off, val],
        None,
    );
    // Deliberately not rewired into the Return: `Graph::add` appends, so a
    // Return consuming it would trip the (heuristic) arena-order lane and
    // muddy what this test is about.
    let _len = g.add(Op::ArrayLength, IrType::Int, vec![ctrl, st, arr], None);
    assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());

    g.kill(st);
    // Structural lane stays quiet: this is an ordering edge, not a value.
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    // The chain lane names it.
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("memory token"), "{m}");
    assert!(m.contains("ArrayLength"), "{m}");
}

/// The compact hand-built `[array]` form has no token to lose, and the
/// arity lane deliberately admits it (`ArrayLength` is `1..=3`). The
/// min-arity guard borrowed from `ir_optimize::memory_token_slot` is what
/// keeps the two forms apart.
#[test]
fn compact_array_length_has_no_memory_token() {
    let compact = Node {
        op: Op::ArrayLength,
        ty: IrType::Int,
        inputs: vec![7].into(),
        bytecode_pc: None,
        frame_snapshot: None,
        volatile_access: false,
    };
    assert!(!is_memory_token_input(&compact, 1));
    let full = Node {
        op: Op::ArrayLength,
        ty: IrType::Int,
        inputs: vec![1, 2, 3].into(),
        bytecode_pc: None,
        frame_snapshot: None,
        volatile_access: false,
    };
    assert!(is_memory_token_input(&full, 1));
    assert!(!is_memory_token_input(&full, 0));
    assert!(!is_memory_token_input(&full, 2));
}

#[test]
fn frame_state_referring_to_a_removed_node_is_rejected() {
    let mut g = linear_graph();
    let k = g.add(Op::Const(9), IrType::Int, vec![], None);
    g.safepoints.push(crate::ir::SafepointSnapshot::new(
        4,
        vec![k, NO_NODE],
        vec![],
        Vec::new(),
    ));
    // Clean while the slot is live…
    assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());
    // …and rejected once it is not.
    g.kill(k);
    let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("local[0]"), "{m}");
    assert!(m.contains("removed (Dead)"), "{m}");
}

/// Both snapshot consumers key on the bci and resolve ties by position, so
/// two snapshots at one bci silently discard one of the two frame states.
/// The survivor then describes a program point that is not the one it
/// resumes at — a wrong frame with no symptom at the point of the defect.
///
/// The nodes carry bci 4 deliberately: a duplicated bci is only a defect
/// when the code actually emits something there, and an UNCLAIMED
/// duplicate is what the check is for. See
/// [`two_claimed_snapshots_at_one_bci_are_a_copied_body`] for the other
/// side of the rule.
#[test]
fn two_snapshots_at_one_bci_are_rejected() {
    let mut g = linear_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], Some(4));
    let b = g.add(Op::Const(2), IrType::Int, vec![], Some(4));
    g.safepoints.push(crate::ir::SafepointSnapshot::new(
        4,
        vec![a],
        vec![],
        Vec::new(),
    ));
    // A second, DIFFERENT frame state at the same bci.
    g.safepoints.push(crate::ir::SafepointSnapshot::new(
        4,
        vec![b],
        vec![],
        Vec::new(),
    ));
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("describe bci 4"), "{m}");
    assert!(m.contains("silently discarded"), "{m}");
}

/// Two snapshots at one bci are FINE when every one of them is named by a
/// node — that is a copied loop body, not a collision.
///
/// The relaxation this check grew for `ir_optimize::unroll`'s per-copy
/// deopt frames, and it is paired with
/// [`two_snapshots_at_one_bci_are_rejected`] on purpose: the two fixtures
/// differ only in whether the nodes claim their snapshots, so neither can
/// pass by the check having quietly stopped running.
#[test]
fn two_claimed_snapshots_at_one_bci_are_a_copied_body() {
    let mut g = linear_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], Some(4));
    let b = g.add(Op::Const(2), IrType::Int, vec![], Some(4));
    g.push_safepoint(crate::ir::SafepointSnapshot::new(
        4,
        vec![a],
        vec![],
        Vec::new(),
    ));
    g.push_safepoint(crate::ir::SafepointSnapshot::new(
        4,
        vec![b],
        vec![],
        Vec::new(),
    ));
    assert!(g.set_node_frame_snapshot(a, 0));
    assert!(g.set_node_frame_snapshot(b, 1));
    assert!(
        verify_graph(&g, "test", VerifyOptions::default()).is_ok(),
        "two snapshots at one bci, each claimed by the node it describes, \
         is what a per-copy unroll produces",
    );

    // Drop ONE claim and the collision is back: nothing names snapshot 1,
    // so the by-bci scan resolves that program point to snapshot 0.
    g.nodes[b as usize].frame_snapshot = None;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    assert!(
        message(&err).contains("describe bci 4"),
        "{}",
        message(&err)
    );
}

/// Round 11 wave 6: LICM's pre-header frame is a claimed COPY pushed after
/// an unclaimed original at the same bci. The original is what every
/// by-position reader finds first, so the pair is accepted; an unclaimed
/// snapshot BEHIND a claimed one, or two unclaimed ones, are not.
#[test]
fn r11w6_one_unclaimed_original_ahead_of_its_claimed_copies_is_accepted() {
    let build = |claim: usize| {
        let mut g = linear_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], Some(4));
        let hoisted = g.add(Op::Const(2), IrType::Int, vec![], Some(4));
        for local in [a, hoisted] {
            g.push_safepoint(crate::ir::SafepointSnapshot::new(
                4,
                vec![local],
                vec![],
                Vec::new(),
            ));
        }
        assert!(g.set_node_frame_snapshot(hoisted, claim as u32));
        (g, a)
    };

    let (g, _) = build(1);
    let r = verify_graph(&g, "test", VerifyOptions::default());
    assert!(
        r.is_ok(),
        "original first, copy claimed: {}",
        r.unwrap_err()
    );

    // The copy AHEAD of the unclaimed snapshot: a by-position reader at
    // bci 4 would be handed the copy's frame.
    let (g, _) = build(0);
    let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
    assert!(m.contains("safepoint(s) [1] of [0, 1]"), "{m}");

    // A second unclaimed snapshot is ambiguous again.
    let (mut g, a) = build(1);
    g.push_safepoint(crate::ir::SafepointSnapshot::new(
        4,
        vec![a],
        vec![],
        Vec::new(),
    ));
    let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
    assert!(m.contains("safepoint(s) [0, 2] of [0, 1, 2]"), "{m}");
}

/// A duplicated bci NO node carries is unconsultable, not ambiguous.
///
/// `build_deopt_points` anchors through `bci_native`, which is populated
/// only from nodes, and every guard resumes at some node's own
/// `bytecode_pc` — so nothing can ever ask about a bci the code does not
/// emit. This is the normal state of a graph after a full unroll, whose
/// post-unroll constant fold retires the very nodes that made the body's
/// bcis real.
#[test]
fn duplicate_snapshots_at_an_unemitted_bci_are_not_a_violation() {
    let mut g = linear_graph();
    let k = g.add(Op::Const(1), IrType::Int, vec![], Some(9));
    for _ in 0..2 {
        g.push_safepoint(crate::ir::SafepointSnapshot::new(
            4,
            vec![k],
            vec![],
            Vec::new(),
        ));
    }
    assert!(
        verify_graph(&g, "test", VerifyOptions::default()).is_ok(),
        "no node carries bci 4, so neither snapshot can be reached",
    );
}

/// Distinct bcis are the normal case and must stay clean, including when
/// they name the same node.
#[test]
fn distinct_snapshot_bcis_are_accepted() {
    let mut g = linear_graph();
    let k = g.add(Op::Const(1), IrType::Int, vec![], None);
    for bci in [2usize, 4, 7] {
        g.safepoints.push(crate::ir::SafepointSnapshot::new(
            bci,
            vec![k],
            vec![],
            Vec::new(),
        ));
    }
    assert!(verify_graph(&g, "test", VerifyOptions::default()).is_ok());
}

/// The duplicate-bci check lives in the frame-state lane, so it follows
/// that lane's opt-in rules rather than silently becoming an always-on
/// structural check.
#[test]
fn the_duplicate_bci_check_is_part_of_the_frame_state_lane() {
    let mut g = linear_graph();
    let k = g.add(Op::Const(1), IrType::Int, vec![], Some(4));
    g.safepoints.push(crate::ir::SafepointSnapshot::new(
        4,
        vec![k],
        vec![],
        Vec::new(),
    ));
    g.safepoints.push(crate::ir::SafepointSnapshot::new(
        4,
        vec![k],
        vec![],
        Vec::new(),
    ));
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    assert!(verify_graph(&g, "test", VerifyOptions::default()).is_err());
}

#[test]
fn frame_state_lane_is_opt_in() {
    let mut g = linear_graph();
    let k = g.add(Op::Const(9), IrType::Int, vec![], None);
    g.safepoints.push(crate::ir::SafepointSnapshot::new(
        4,
        vec![k],
        vec![],
        Vec::new(),
    ));
    g.kill(k);
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    assert!(verify_graph(&g, "test", VerifyOptions::default()).is_err());
}

/// A frame slot holds a value; a memory token there has no interpreter
/// representation (added 2026-09-18).
#[test]
fn a_frame_state_slot_naming_a_token_is_rejected() {
    let mut g = linear_graph();
    // n2 is `Proj(1)` of `Start`: the initial MEMORY token.
    g.safepoints.push(crate::ir::SafepointSnapshot::new(
        4,
        vec![2],
        vec![],
        Vec::new(),
    ));
    let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
    assert!(m.contains("local[0]"), "{m}");
    assert!(m.contains("memory token"), "{m}");
    // Structural-only verification does not run the lane.
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
}

/// An undefined local is legal; an undefined HELD MONITOR is not.
#[test]
fn an_undefined_monitor_in_a_frame_state_is_rejected() {
    let mut g = linear_graph();
    g.safepoints.push(crate::ir::SafepointSnapshot::new(
        4,
        vec![NO_NODE],
        vec![],
        vec![],
    ));
    assert!(verify_graph(&g, "test", VerifyOptions::default()).is_ok());
    g.safepoints[0].monitors.push(NO_NODE);
    let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
    assert!(m.contains("monitor[0] is NO_NODE"), "{m}");
}

/// `MonitorEnter` pins control at input 0 like every other runtime-reaching
/// op; a data node there is reported (added 2026-09-18).
#[test]
fn a_monitor_taking_control_from_a_data_node_is_rejected() {
    let mut ok = linear_graph();
    let obj = ok.add(Op::Param(0), IrType::Ref, vec![0], None);
    // n1 = control, n2 = memory.
    ok.add(Op::MonitorEnter, IrType::Memory, vec![1, 2, obj], None);
    assert!(verify_graph(&ok, "test", VerifyOptions::structural()).is_ok());

    let mut bad = linear_graph();
    let obj = bad.add(Op::Param(0), IrType::Ref, vec![0], None);
    // n3 is the `Const(7)` the return reads — not a control token.
    bad.add(Op::MonitorEnter, IrType::Memory, vec![3, 2, obj], None);
    let m = message(&verify_graph(&bad, "test", VerifyOptions::structural()).unwrap_err());
    assert!(m.contains("MonitorEnter"), "{m}");
    assert!(m.contains("produces no control token"), "{m}");
}

/// A call's arguments are values: a control token marshalled as one is
/// reported by the type lane (added 2026-09-18).
#[test]
fn a_control_token_passed_as_a_call_argument_is_rejected() {
    let mut g = linear_graph();
    g.add(Op::Call { info_ptr: 0 }, IrType::Void, vec![1, 2, 1], None);
    let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
    assert!(m.contains("Call"), "{m}");
    assert!(m.contains("used as a data value"), "{m}");
}

#[test]
fn every_violation_is_collected_not_just_the_first() {
    let mut g = linear_graph();
    let ret = g.exit as usize;
    g.nodes[ret].inputs[0] = 500;
    g.nodes[ret].inputs[1] = 501;
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("n500"), "{m}");
    assert!(m.contains("n501"), "{m}");
    assert!(m.contains("violation(s)"), "{m}");
}

#[test]
fn violation_list_is_capped_but_the_total_is_not() {
    let mut g = linear_graph();
    // Many broken nodes: one violation each.
    for _ in 0..(MAX_REPORTED_VIOLATIONS + 10) {
        g.add(Op::Add, IrType::Int, vec![9999, 9999], None);
    }
    let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("and"), "{m}");
    assert!(m.contains("more"), "{m}");
}

#[test]
fn verifier_never_panics_on_a_shredded_graph() {
    let mut g = Graph::empty(0);
    g.entry = 42;
    g.exit = 77;
    g.safepoints.push(crate::ir::SafepointSnapshot::new(
        0,
        vec![5, NO_NODE, 900],
        vec![1234],
        Vec::new(),
    ));
    g.add(Op::Phi, IrType::Int, vec![NO_NODE, 3], None);
    g.add(Op::Return, IrType::Void, vec![u32::MAX - 1], None);
    // No assertion on the contents — the contract under test is "returns
    // an Err instead of panicking".
    assert!(verify_graph(&g, "shredded", VerifyOptions::all()).is_err());
}

#[test]
fn empty_graph_is_rejected_without_panicking() {
    let g = exit0_graph();
    assert!(verify_graph(&g, "empty", VerifyOptions::all()).is_err());
}

#[test]
fn arena_order_lane_flags_a_use_before_its_definition() {
    let mut g = linear_graph();
    // Append a constant *after* the Return, then make the Return consume
    // it: valid as a graph, impossible as an arena-ordered schedule.
    let later = g.add(Op::Const(5), IrType::Int, vec![], None);
    let ret = g.exit as usize;
    g.nodes[ret].inputs[1] = later;
    assert!(verify_graph(&g, "test", VerifyOptions::default()).is_ok());
    let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
    assert!(
        message(&err).contains("defined after its use"),
        "{}",
        message(&err)
    );
}

/// The split is the point: an arena-order finding must not drag the
/// memory-chain lane in with it, and vice versa. They used to be one
/// `check_schedule` flag, which meant the soundness half could not be
/// enabled without the heuristic half's guaranteed false positives.
#[test]
fn the_two_ordering_lanes_are_independent() {
    let mut g = linear_graph();
    let later = g.add(Op::Const(5), IrType::Int, vec![], None);
    g.nodes[g.exit as usize].inputs[1] = later;

    let arena_only = VerifyOptions {
        check_arena_order: true,
        check_memory_chain: false,
        ..VerifyOptions::structural()
    };
    let chain_only = VerifyOptions {
        check_arena_order: false,
        check_memory_chain: true,
        ..VerifyOptions::structural()
    };
    // An arena-order violation is invisible to the chain lane…
    let r = verify_graph(&g, "test", chain_only);
    assert!(r.is_ok(), "{}", r.unwrap_err());
    assert!(message(&verify_graph(&g, "test", arena_only).unwrap_err())
        .contains("defined after its use"));

    // …and a broken memory token is invisible to the arena-order lane.
    let ctrl = g.nodes[g.exit as usize].inputs[0];
    let mem = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Proj(1)))
        .expect("memory projection") as NodeId;
    let st1 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, later, later, later],
        None,
    );
    let _st2 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, st1, later, later, later],
        None,
    );
    g.kill(st1);
    assert!(
        message(&verify_graph(&g, "test", chain_only).unwrap_err()).contains("memory token")
    );
    let arena_msg = message(&verify_graph(&g, "test", arena_only).unwrap_err());
    assert!(arena_msg.contains("defined after its use"), "{arena_msg}");
    assert!(!arena_msg.contains("memory token"), "{arena_msg}");
}

/// With nothing set, `from_env`'s three real lanes follow the master gate
/// and the arena-order lane does not.
///
/// This replaces `options_from_env_defaults_to_structural_only`, whose name
/// stopped describing the contract on 2026-09-16: `from_env` used to answer
/// "structural only" in every build, which is why a debug test run never
/// type-checked a graph. The three lanes now default to
/// `subchecks_follow_the_master_gate()` and arena order to `false` (see
/// `ARENA_ORDER_FOLLOWS_THE_MASTER_GATE`).
///
/// Written against the *functions*, not against today's values: flipping
/// the kill switch, or building in release, must not require editing this
/// test. Each clause is still conditional on the variable being unset,
/// because an operator running the suite with one of these exported is
/// asking for a different configuration and is not a failure.
///
// REVIEW-NOTE (2026-09-16): `jit/tests/no_presence_only_flag_reads.rs`'s
// `ALLOWED` doc comment names this test by its OLD name,
// `options_from_env_defaults_to_structural_only`. That file is outside this
// change's write scope; its count is unchanged (this test still makes
// exactly one presence-only read, through `unset`), so the ratchet still
// passes — only the row's label is stale. Owner of
// `jit/tests/no_presence_only_flag_reads.rs`: update that one row to
// `sub_check_defaults_follow_the_master_gate`.
#[test]
fn sub_check_defaults_follow_the_master_gate() {
    let unset = |n: &str| cratonvm_types::flags::runtime_var_os(n).is_none();
    let o = VerifyOptions::from_env();
    let gated = subchecks_follow_the_master_gate();
    if unset("CRATONVM_JIT_VERIFY_TYPES") {
        assert_eq!(o.check_types, gated);
    }
    if unset("CRATONVM_JIT_VERIFY_FRAME_STATES") {
        assert_eq!(o.check_frame_states, gated);
    }
    if unset("CRATONVM_JIT_VERIFY_MEMORY_CHAIN") && unset("CRATONVM_JIT_VERIFY_SCHEDULE") {
        assert_eq!(o.check_memory_chain, gated);
    }
    if unset("CRATONVM_JIT_VERIFY_ARENA_ORDER") && unset("CRATONVM_JIT_VERIFY_SCHEDULE") {
        assert_eq!(
            o.check_arena_order,
            ARENA_ORDER_FOLLOWS_THE_MASTER_GATE && gated,
            "the arena-order lane is exempt on the merits — it fires on every \
             optimized graph by construction, and `for_phase` propagates this \
             value unchanged at every phase"
        );
    }
    // The structural lane is unconditional, so `structural()` is still the
    // floor every configuration sits on top of and is unaffected by any of
    // this.
    let floor = VerifyOptions::structural();
    assert!(
        !floor.check_types
            && !floor.check_frame_states
            && !floor.check_memory_chain
            && !floor.check_arena_order
    );
}

/// Was this lane turned on by an operator, rather than by the build
/// profile?
///
/// The distinction `for_phase` rests on since 2026-09-16: an explicit `=1`
/// forces a lane on at any phase, a default-derived `true` runs only where
/// the phase declares the lane clean. Before the sub-check defaults
/// followed the master gate the two were the same thing, which is why the
/// tests below used to spell this `env.check_*`.
fn lane_forced(name: &str) -> bool {
    env_flag(name) == Some(true)
}

/// The builder's own output is checked structurally and nothing more:
/// every optional lane is a claim some later pass is what establishes, and
/// no pass has run. Staging matters here — the type lane defaults to the
/// build profile, so a debug build would otherwise have got a broad new
/// type requirement at the hook that introduced the phase, with a silent
/// fall-back to the single-pass tier as the failure mode.
#[test]
fn the_post_build_hook_runs_the_structural_lane_and_nothing_defaulted() {
    let post_build = VerifyOptions::for_phase(PHASE_POST_BUILD);
    assert_eq!(
        post_build.check_types,
        lane_forced("CRATONVM_JIT_VERIFY_TYPES")
    );
    assert_eq!(
        post_build.check_frame_states,
        lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES")
    );
    assert_eq!(
        post_build.check_memory_chain,
        lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
            || lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE")
    );
    // Propagated unchanged, exactly as at every other phase — the lane is
    // enabled by no phase declaration anywhere.
    assert_eq!(
        post_build.check_arena_order,
        VerifyOptions::from_env().check_arena_order
    );
}

/// The type lane is unconditional at `"post-optimize"` and staged behind
/// an explicit `=1` at `"post-build"`. Stated as a relation between the
/// two phases so that promoting the lane later is one edit here.
#[test]
fn the_type_lane_is_forced_after_optimize_but_only_offered_after_build() {
    assert!(VerifyOptions::for_phase(PHASE_POST_OPTIMIZE).check_types);
    if !lane_forced("CRATONVM_JIT_VERIFY_TYPES") {
        assert!(!VerifyOptions::for_phase(PHASE_POST_BUILD).check_types);
    }
}

/// The lanes `ir_optimize`'s fixes made safe are on at `"post-optimize"`,
/// and only there until `apply_ea_to_ir` catches up. The heuristic
/// arena-order lane is on at no phase.
#[test]
fn for_phase_enables_the_ea_blocked_lanes_only_before_escape_analysis() {
    let post_opt = VerifyOptions::for_phase(PHASE_POST_OPTIMIZE);
    assert!(post_opt.check_frame_states);
    assert!(post_opt.check_memory_chain);
    assert!(!post_opt.check_arena_order);

    for phase in ["post-escape-analysis", "pre-lower"] {
        let o = VerifyOptions::for_phase(phase);
        assert!(!o.check_arena_order, "{phase}");
        // Flipping either constant is the whole change; these state the
        // invariant either way rather than pinning today's value, so a flip
        // needs no test edit.
        // `lane_forced`, not `env.check_*`: a lane on only because this is
        // a debug build must NOT override a phase declaration. It did,
        // briefly, and the cost was that every method whose escape analysis
        // scalar-replaced an allocation live across a safepoint bailed the
        // IR tier -- a silent throughput regression, because the
        // frame-state lane's post-EA finding is an INTENTIONAL state (see
        // `APPLY_EA_ROUTES_ALL_SAFEPOINTS`).
        assert_eq!(
            o.check_frame_states,
            APPLY_EA_ROUTES_ALL_SAFEPOINTS || lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES"),
            "{phase}"
        );
        assert_eq!(
            o.check_memory_chain,
            APPLY_EA_SPLICES_MEMORY_CHAIN
                || lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
                || lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE"),
            "{phase}"
        );
    }
}

/// The two EA constants gate *different* lanes. They were one switch until
/// it became clear they are waiting on different things — a fix versus a
/// model change — and a test that could not tell them apart would let them
/// silently fuse back together.
#[test]
fn the_two_ea_constants_gate_different_lanes() {
    let pre_lower = VerifyOptions::for_phase("pre-lower");
    assert_eq!(
        pre_lower.check_memory_chain,
        APPLY_EA_SPLICES_MEMORY_CHAIN
            || lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
            || lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE")
    );
    assert_eq!(
        pre_lower.check_frame_states,
        APPLY_EA_ROUTES_ALL_SAFEPOINTS || lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES")
    );
    // Neither constant has any effect at the pre-EA hook: both lanes are
    // unconditionally on there.
    let post_opt = VerifyOptions::for_phase(PHASE_POST_OPTIMIZE);
    assert!(post_opt.check_memory_chain && post_opt.check_frame_states);
}

/// An operator's EXPLICIT `=1` may only *add* lanes to a phase's
/// defaults, never subtract: turning the gate down is
/// `CRATONVM_JIT_VERIFY_IR=0`, which disables it outright rather than
/// silently narrowing it.
///
/// # Why this is about `=1` and not about `from_env`
///
/// It used to test `env.check_*`, which was the same question while every
/// lane defaulted to `false`. Since the sub-check defaults follow the
/// master gate, `env.check_frame_states` is `true` in a debug build for
/// nobody in particular, and carrying THAT into a phase whose declaring
/// constant says the lane is unverified there is not "the operator added a
/// lane" -- it is a default overriding a declaration. The property worth
/// keeping is the one about the operator, and it is the one tested here.
#[test]
fn an_explicit_lane_request_is_never_dropped_by_a_phase() {
    for phase in [PHASE_POST_OPTIMIZE, "post-escape-analysis", "pre-lower"] {
        let o = VerifyOptions::for_phase(phase);
        assert!(
            !lane_forced("CRATONVM_JIT_VERIFY_TYPES") || o.check_types,
            "{phase}"
        );
        assert!(
            !lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES") || o.check_frame_states,
            "{phase}"
        );
        assert!(
            !lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN") || o.check_memory_chain,
            "{phase}"
        );
        assert!(
            !lane_forced("CRATONVM_JIT_VERIFY_ARENA_ORDER") || o.check_arena_order,
            "{phase}"
        );
    }
}

/// A lane that is on only because this is a debug build does NOT run at a
/// phase whose declaring constant says it is unverified there.
///
/// The other half of the rule above, and the one with a live cost: the
/// frame-state lane's post-EA finding is an intentional state
/// (`APPLY_EA_ROUTES_ALL_SAFEPOINTS`), so running it there turns every
/// scalar-replacing method into an IR-tier bailout -- silently, as lost
/// throughput with no failing test.
#[test]
fn a_default_derived_lane_respects_the_phase_declaration() {
    for phase in ["post-escape-analysis", "pre-lower"] {
        let o = VerifyOptions::for_phase(phase);
        if !APPLY_EA_ROUTES_ALL_SAFEPOINTS && !lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES") {
            assert!(
                !o.check_frame_states,
                "{phase}: the frame-state lane is declared unverified here \
                 and nobody asked for it explicitly"
            );
        }
        if !APPLY_EA_SPLICES_MEMORY_CHAIN
            && !lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
            && !lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE")
        {
            assert!(!o.check_memory_chain, "{phase}");
        }
    }
    // The pre-EA hook is unaffected: both lanes are unconditionally on
    // there, which is where the 2026-09-16 default flip's value lives.
    let post_opt = VerifyOptions::for_phase(PHASE_POST_OPTIMIZE);
    assert!(post_opt.check_frame_states && post_opt.check_memory_chain);
}

#[test]
fn verify_graph_at_phase_matches_verify_graph_with_for_phase() {
    let g = diamond_graph();
    for phase in [PHASE_POST_OPTIMIZE, "pre-lower"] {
        let a = verify_graph_at_phase(&g, phase);
        let b = verify_graph(&g, phase, VerifyOptions::for_phase(phase));
        assert_eq!(a.is_ok(), b.is_ok(), "{phase}");
    }
}

/// The module doc makes claims about the code it verifies. The ones that are
/// checkable from here are checked here, so the doc cannot rot back into the
/// state this consolidation found it in.
#[test]
fn module_doc_claims_match_observable_behaviour() {
    // "the lattice is `ir::join_data_type`, and it rejects an Int/Long
    // merge" — the claim that retired the private `Cat` lattice.
    assert!(crate::ir::join_data_type(IrType::Int, IrType::Long).is_none());
    assert!(crate::ir::join_data_type(IrType::Float, IrType::Double).is_none());
    assert_eq!(
        crate::ir::join_data_type(IrType::Ref, IrType::Ref),
        Some(IrType::Ref)
    );
    assert!(crate::ir::join_data_type(IrType::Void, IrType::Void).is_none());

    // "`ir::PHI_TYPE_FALLBACK` is `Int`, and it is not a GC root type."
    assert_eq!(crate::ir::PHI_TYPE_FALLBACK, IrType::Int);
    assert_ne!(crate::ir::PHI_TYPE_FALLBACK, IrType::Ref);

    // "the frame-state and memory-chain lanes are on at post-optimize, the
    // arena-order lane at no phase."
    let post_opt = VerifyOptions::for_phase(PHASE_POST_OPTIMIZE);
    assert!(post_opt.check_frame_states && post_opt.check_memory_chain);
    assert!(!post_opt.check_arena_order);
    assert!(!VerifyOptions::for_phase("pre-lower").check_arena_order);

    // "the two EA constants are the switches for the post-EA phases" --
    // together with an explicit operator request, which is the only other
    // thing that reaches those phases.
    let pre_lower = VerifyOptions::for_phase("pre-lower");
    assert_eq!(
        pre_lower.check_frame_states,
        APPLY_EA_ROUTES_ALL_SAFEPOINTS || lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES")
    );
    assert_eq!(
        pre_lower.check_memory_chain,
        APPLY_EA_SPLICES_MEMORY_CHAIN
            || lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
            || lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE")
    );

    // "`VerifyOptions::all` turns everything on."
    let all = VerifyOptions::all();
    assert!(
        all.check_types
            && all.check_frame_states
            && all.check_memory_chain
            && all.check_arena_order
    );
    let none = VerifyOptions::structural();
    assert!(
        !none.check_types
            && !none.check_frame_states
            && !none.check_memory_chain
            && !none.check_arena_order
    );
}

/// The master gate follows the build profile when unset — and, since
/// 2026-09-16, so do the optional lanes it advertises.
///
/// The second half is the new half. `verify_enabled()` answering `true` in
/// a debug build used to mean only "the structural lane runs at the
/// per-pass hooks": every optional lane defaulted to `false` regardless, so
/// a build that reported verification on was not type-checking, not
/// checking frame states and not checking memory tokens. That is the gap
/// this asserts is closed.
///
/// Stated as an implication, not an equality, because the kill switch
/// inside `subchecks_follow_the_master_gate` is allowed to make the
/// sub-checks *narrower* than the master gate; what it must never produce
/// is optional lanes on while the master gate is off.
#[test]
fn verify_enabled_matches_the_build_profile_when_unset() {
    if None::<std::ffi::OsString>.is_none() {
        assert_eq!(verify_enabled(), cfg!(debug_assertions));
        assert!(!pre_lower_verify_disabled());
        assert!(
            !subchecks_follow_the_master_gate() || verify_enabled(),
            "the optional lanes default ON while the master gate is off — a \
             configuration nothing in this module intends"
        );
        // That the lanes actually *take* this default is
        // `sub_check_defaults_follow_the_master_gate`, which has to guard
        // each clause on its own variable being unset and so cannot be
        // folded in here.
    }
}


// ── Lane: projections of an `Op::Switch` ─────────────────────────

/// A three-way switch over `[1, 3]` plus its default, verified clean, so
/// the negative tests below are demonstrably about what they claim.
fn switch_graph(low: i32, high: i32) -> Graph {
    let mut g = exit0_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let key = g.add(Op::Const(0), IrType::Int, vec![], None);
    let sw = g.add(
        Op::Switch { low, high },
        IrType::Control,
        vec![ctrl, key],
        None,
    );
    let outputs = crate::ir::switch_projection_count(low, high);
    let mut edges = Vec::with_capacity(outputs);
    for k in 0..outputs {
        edges.push(g.add(Op::Proj(k as u16), IrType::Control, vec![sw], None));
    }
    let merge = g.add(Op::Merge, IrType::Control, edges, None);
    let ret = g.add(Op::Return, IrType::Void, vec![merge], None);
    g.entry = start;
    g.exit = ret;
    g
}

#[test]
fn a_switch_projects_one_output_per_case_plus_the_default() {
    assert_eq!(projection_arity(&Op::Switch { low: 1, high: 3 }), 4);
    assert_eq!(projection_arity(&Op::Switch { low: -3, high: -1 }), 4);
    assert_eq!(projection_arity(&Op::Switch { low: 7, high: 7 }), 2);
    let g = switch_graph(1, 3);
    let r = verify_graph(&g, "test", VerifyOptions::all());
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

/// The `i64` in `projection_arity`, as a test. `high - low` OVERFLOWS
/// `i32` for a full-`int`-range `tableswitch`: computed at that width the
/// arity would come out as `1`, and every out-of-range `Proj` of the node
/// would pass this lane. Saturated rather than wrapped, so the answer is
/// implausible instead of plausible.
#[test]
fn a_full_range_switchs_projection_arity_does_not_overflow_into_a_small_number() {
    let op = Op::Switch {
        low: i32::MIN,
        high: i32::MAX,
    };
    assert_eq!(projection_arity(&op), u32::MAX as usize);
    assert!(projection_arity(&op) > crate::ir::SWITCH_MAX_PROJECTIONS);
}

/// `ir_lower` resolves a switch edge by MATCHING the projection index, so
/// an index past the last output leaves a case edge unlowered — the same
/// failure an out-of-range `Proj` of an `Op::If` has.
#[test]
fn a_projection_past_a_switchs_default_edge_is_reported() {
    let mut g = switch_graph(1, 3);
    let last = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Proj(3))
        .expect("the default projection");
    g.nodes[last].op = Op::Proj(9);
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("selects output 9"), "{m}");
}

/// A switch is `[ctrl, key]` and nothing else: the case keys are the
/// projection indices, not edges, so a 255-case switch still has two
/// inputs.
/// t1 w9 TCFUSE: a type check takes `[ctrl, mem, obj]` and, once the
/// shared-class-word pass ran, the word as a fourth input; never a fifth.
#[test]
fn a_type_check_takes_three_inputs_or_four_with_a_shared_class_word() {
    let iof = Op::InstanceOf {
        name_ptr: 0,
        name_len: 0,
        seen_cell: 0,
        header_only: false,
    };
    let cast = Op::CheckCast {
        name_ptr: 0,
        name_len: 0,
    };
    assert_eq!(expected_arity(&iof), (3, 4));
    assert_eq!(expected_arity(&cast), (3, 4));
}

#[test]
fn a_switch_takes_exactly_a_control_token_and_a_key() {
    assert_eq!(expected_arity(&Op::Switch { low: 0, high: 9 }), (2, 2));
    let mut g = switch_graph(1, 3);
    let sw = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Switch { .. }))
        .expect("the switch");
    g.nodes[sw].inputs.pop();
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("exactly 2"), "{m}");
}

// ── Round 11 (lane irback) ───────────────────────────────────────────

/// `(graph, ctrl, mem)` from [`linear_graph`].
fn r11_head() -> (Graph, NodeId, NodeId) {
    let g = linear_graph();
    let ctrl = g.nodes[g.exit as usize].inputs[0];
    let mem = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Proj(1)))
        .expect("memory projection") as NodeId;
    (g, ctrl, mem)
}

/// The production `[ctrl, mem, array]` `ArrayLength` has its control slot
/// checked. It fell into `control_input_indices`' catch-all, so a data node
/// there verified clean.
#[test]
fn r11_an_array_length_taking_control_from_a_data_node_is_rejected() {
    let (mut g, _ctrl, mem) = r11_head();
    let arr = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let not_ctrl = g.add(Op::Const(1), IrType::Int, vec![], None);
    let _len = g.add(Op::ArrayLength, IrType::Int, vec![not_ctrl, mem, arr], None);
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("produces no control token"), "{m}");
}

/// Both `ArrayLength` forms still verify clean, and the compact
/// fixture form has no control slot to check.
#[test]
fn r11_both_array_length_forms_still_verify() {
    let (mut g, ctrl, mem) = r11_head();
    let arr = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let _full = g.add(Op::ArrayLength, IrType::Int, vec![ctrl, mem, arr], None);
    let _compact = g.add(Op::ArrayLength, IrType::Int, vec![arr], None);
    let opts = VerifyOptions {
        check_types: true,
        ..VerifyOptions::structural()
    };
    assert!(verify_graph(&g, "test", opts).is_ok());    }

/// The compact `[array]` fixture form is refused at the hook in front of
/// `ir_lower` (which reads `inputs[2]`), and only there.
#[test]
fn r11_a_compact_array_length_is_refused_in_front_of_the_lowerer() {
    let (mut g, _ctrl, _mem) = r11_head();
    let arr = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let _compact = g.add(Op::ArrayLength, IrType::Int, vec![arr], None);
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    let err = verify_graph(&g, PHASE_PRE_LOWER, VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("input[2]"), "{m}");
}

/// The array an `ArrayLength` reads is a data slot: a control token there
/// is reported by the type lane.
#[test]
fn r11_an_array_length_of_a_control_token_is_rejected() {
    let (mut g, ctrl, mem) = r11_head();
    let _len = g.add(Op::ArrayLength, IrType::Int, vec![ctrl, mem, ctrl], None);
    let opts = VerifyOptions {
        check_types: true,
        ..VerifyOptions::structural()
    };
    let err = verify_graph(&g, "test", opts).unwrap_err();
    let m = message(&err);
    assert!(m.contains("control token used as a data value"), "{m}");
}

/// A store read as an arithmetic operand. `ir_lower::verify_data_locations`
/// skips a non-value-typed input rather than refusing it, so the type lane
/// is the only place this is caught.
#[test]
fn r11_a_memory_node_used_as_a_data_value_is_rejected() {
    let (mut g, ctrl, mem) = r11_head();
    let arr = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let val = g.add(Op::Const(1), IrType::Int, vec![], None);
    let st = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, arr, off, val],
        None,
    );
    let _sum = g.add(Op::Add, IrType::Int, vec![st, val], None);
    let opts = VerifyOptions {
        check_types: true,
        ..VerifyOptions::structural()
    };
    let err = verify_graph(&g, "test", opts).unwrap_err();
    let m = message(&err);
    assert!(m.contains("memory token used as a data value"), "{m}");
}

/// Two `Proj(0)`s of one `If` are still reported once the seen-set is a
/// hash set; a switch with many projections verifies clean.
#[test]
fn r11_projection_dedup_survives_the_hash_set() {
    let g = switch_graph(0, 200);
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    let mut g = diamond_graph();
    let f = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Proj(1) && n.ty == IrType::Control)
        .expect("the false projection");
    g.nodes[f].op = Op::Proj(0);
    let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
    let m = message(&err);
    assert!(m.contains("second Proj(0)"), "{m}");
}

// ── Round 11 wave 2 (lane ircore) ────────────────────────────────────

/// A graph with no terminator used to be skipped by EVERY lane when its
/// `exit` was never assigned, which is what a complete `while (true)` method
/// looks like. It is verified now; only an abandoned skeleton is skipped.
#[test]
fn r11w2_a_complete_graph_with_no_terminator_is_verified_not_skipped() {
    let fresh = || Graph::empty(0);
    // The abandoned-build skeleton: still skipped.
    let mut s = fresh();
    let start = s.add(Op::Start, IrType::Control, vec![], None);
    let _c = s.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _m = s.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let _p = s.add(Op::Param(0), IrType::Int, vec![start], None);
    s.entry = start;
    assert!(verify_graph(&s, "test", VerifyOptions::all()).is_ok());

    // A self-looping header and nothing else: a legal endless loop.
    let mut g = fresh();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let header = g.add(Op::Merge, IrType::Control, vec![ctrl], None);
    g.push_input(header, header);
    g.entry = start;
    let r = verify_graph(&g, "test", VerifyOptions::all());
    assert!(r.is_ok(), "{}", r.unwrap_err());

    // …which is now actually checked: a bad projection in it is reported.
    let _bad = g.add(Op::Proj(3), IrType::Control, vec![header], None);
    let m = message(&verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err());
    assert!(m.contains("selects output 3"), "{m}");
}

fn anchor_findings(g: &Graph) -> Vec<String> {
    let mut v = Violations::new();
    check_control_anchors(g, &mut v);
    v.list
}

/// The scheduler maps a control edge to a block only through a block
/// head; a join input or a pinned anchor that is not one, and a join that
/// names one predecessor twice, are reported.
#[test]
fn r11w2_control_anchors_must_be_block_heads() {
    assert!(anchor_findings(&linear_graph()).is_empty());
    assert!(anchor_findings(&diamond_graph()).is_empty());
    assert!(anchor_findings(&switch_graph(0, 3)).is_empty());

    // A store pinned to `Start` itself — a control token, so the always-on
    // lane accepts it, but no block starts there.
    let mut g = diamond_graph();
    let start = g.entry;
    let mem = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Proj(1) && n.ty == IrType::Memory)
        .expect("memory projection") as NodeId;
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![start, mem, base, off, off],
        None,
    );
    assert!(
        verify_graph(&g, "test", VerifyOptions::structural()).is_ok()
            || branch_completeness_lane_enabled("test"),
        "the always-on lane accepts the store — which is the gap"
    );
    let f = anchor_findings(&g);
    assert!(
        f.iter()
            .any(|m| m.contains("Store") && m.contains("not a block head")),
        "{f:?}"
    );

    // A join fed by the `If` itself rather than by its projection.
    let mut g = diamond_graph();
    let merge = g.nodes[g.exit as usize].inputs[0];
    let if_node = g.nodes.iter().position(|n| n.op == Op::If).expect("the If") as NodeId;
    g.nodes[merge as usize].inputs[1] = if_node;
    let f = anchor_findings(&g);
    assert!(f.iter().any(|m| m.contains("drops this CFG edge")), "{f:?}");

    // One predecessor named twice.
    let mut g = diamond_graph();
    let merge = g.nodes[g.exit as usize].inputs[0];
    let t = g.nodes[merge as usize].inputs[0];
    g.nodes[merge as usize].inputs[1] = t;
    let f = anchor_findings(&g);
    assert!(f.iter().any(|m| m.contains("twice")), "{f:?}");

    // Two terminators on one block: the scheduler would keep only one.
    let mut g = diamond_graph();
    let merge = g.nodes[g.exit as usize].inputs[0];
    let k = g.add(Op::Const(3), IrType::Int, vec![], None);
    let _second = g.add(Op::Return, IrType::Void, vec![merge, k], None);
    let f = anchor_findings(&g);
    assert!(f.iter().any(|m| m.contains("both end the block")), "{f:?}");
}

/// Round 11 wave 6: at `"pre-lower"` the anchor check is structural in
/// every build profile; `CRATONVM_JIT_VERIFY_BRANCHES=0` is the one thing
/// that removes it there, and the earlier hooks keep the staging.
#[test]
fn r11w6_the_anchor_check_is_structural_in_front_of_the_scheduler() {
    let mut g = diamond_graph();
    let start = g.entry;
    let mem = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Proj(1) && n.ty == IrType::Memory)
        .expect("memory projection") as NodeId;
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![start, mem, base, off, off],
        None,
    );
    let run = |branches: Option<&str>, phase: &str| {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_VERIFY_BRANCHES", branches)],
            || verify_graph(&g, phase, VerifyOptions::structural()),
        )
    };
    let m = message(&run(None, PHASE_PRE_LOWER).unwrap_err());
    assert!(m.contains("not a block head"), "{m}");
    assert!(run(Some("0"), PHASE_PRE_LOWER).is_ok());
    // `"test"` is a phase no staging names, so only the promotion could
    // have reported there — and it does not reach past `"pre-lower"`.
    assert!(run(None, "test").is_ok());
    assert!(run(Some("1"), "test").is_err());
}

/// A token slot naming a pure value — what an unspliced
/// `replace_all_uses(load, value)` leaves — orders nothing and is
/// reported by the memory-chain lane.
#[test]
fn r11w2_a_memory_token_must_name_a_memory_state() {
    let (mut g, ctrl, mem) = r11_head();
    let start = g.entry;
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    let first = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, off],
        None,
    );
    // A value-node-as-token (`self.mem = load`) is a memory state.
    let _second = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, first, base, off],
        None,
    );
    let r = verify_graph(&g, "test", VerifyOptions::default());
    assert!(r.is_ok(), "{}", r.unwrap_err());

    let _bad = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, off, base, off],
        None,
    );
    let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
    assert!(m.contains("not a memory state"), "{m}");
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
}

/// A node that can deopt at a bci no snapshot describes would resume the
/// interpreter with an empty frame; the frame-state lane names it.
#[test]
fn r11w2_a_node_that_can_deopt_needs_a_snapshot_at_its_resume_bci() {
    // Clears this thread's published splice resume map, so an earlier
    // build on the same test thread cannot change how pcs resolve here.
    let _scope = crate::ir::IrBuildResultsScope::enter();
    let (mut g, ctrl, mem) = r11_head();
    let start = g.entry;
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    let _load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, off],
        Some(5),
    );
    for bci in [2usize, 7] {
        g.push_safepoint(crate::ir::SafepointSnapshot::new(
            bci,
            vec![base],
            vec![],
            Vec::new(),
        ));
    }
    let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
    assert!(m.contains("no safepoint snapshot describes bci 5"), "{m}");
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());

    g.push_safepoint(crate::ir::SafepointSnapshot::new(
        5,
        vec![base],
        vec![],
        Vec::new(),
    ));
    let r = verify_graph(&g, "test", VerifyOptions::default());
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

/// A snapshot slot installed through the public `safepoints` field while
/// the def-use lists were current used to be invisible to `sp_users`, and
/// the frame-state lane (which runs `Graph::verify_use_lists`) named it.
///
/// Round 11 wave 18 closed the hole itself: `Graph::safepoints` is an
/// `ir::Safepoints` whose mutable paths bump the edge epoch, so the same
/// install now makes the lists stop claiming currency — nothing is left
/// for the lane to catch, and this test pins that flip. (The lane's
/// use-list check stays: it is the net for a defect inside `ir.rs`'s own
/// tracked mutators, which `ir::tests` exercises directly.)
#[test]
fn r11w2_the_frame_state_lane_checks_the_snapshot_use_lists() {
    let mut g = linear_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Int, vec![], None);
    g.rebuild_use_lists();
    g.push_safepoint(crate::ir::SafepointSnapshot::new(
        4,
        vec![a],
        vec![],
        Vec::new(),
    ));
    let r = verify_graph(&g, "test", VerifyOptions::default());
    assert!(r.is_ok(), "{}", r.unwrap_err());

    assert!(g.use_lists_valid());

    g.safepoints[0].locals[0] = b;
    assert!(
        !g.use_lists_valid(),
        "an install through the field must demote the lists, not slip past them"
    );
    assert_eq!(g.verify_use_lists(), Ok(()));
    let r = verify_graph(&g, "test", VerifyOptions::default());
    assert!(r.is_ok(), "{}", r.unwrap_err());
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());

    // The tracked installer keeps them current, and the lane agrees.
    g.rebuild_use_lists();
    assert!(g.set_safepoint_slot(0, crate::ir::SafepointSlotKind::Local, 0, a));
    assert!(g.use_lists_valid());
    let r = verify_graph(&g, "test", VerifyOptions::default());
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

// ── Round 11 wave 18 (lane irback): the `sr_map` hand-off ──────────────

/// `linear_graph` plus a snapshot whose local 0 names a REMOVED node, the
/// shape `apply_ea_to_ir` leaves for a scalar-replaced allocation.
fn graph_with_a_dead_snapshot_slot() -> (Graph, NodeId) {
    let mut g = linear_graph();
    let gone = g.add(Op::Const(9), IrType::Int, vec![], None);
    g.kill(gone);
    g.push_safepoint(crate::ir::SafepointSnapshot::new(
        4,
        vec![gone],
        vec![],
        Vec::new(),
    ));
    (g, gone)
}

fn names_a_removed_slot(r: &CompileResult<()>) -> bool {
    match r {
        Ok(()) => false,
        Err(b) => message(b).contains("refers to a removed (Dead) node"),
    }
}

/// The frame-state lane accepts a removed node in a snapshot slot exactly
/// when the caller says EA scalar-replaced it, and reports it otherwise —
/// with no set at all (the pre-EA hooks) and with a set that does not name
/// it (a stranded value after EA).
#[test]
fn r11w18_a_scalar_replaced_slot_is_a_descriptor_and_any_other_dead_slot_is_not() {
    let (g, gone) = graph_with_a_dead_snapshot_slot();
    let named: std::collections::HashSet<NodeId> = [gone].into_iter().collect();
    let empty: std::collections::HashSet<NodeId> = std::collections::HashSet::new();

    let r = verify_graph(&g, "test", VerifyOptions::default());
    assert!(names_a_removed_slot(&r), "no set: the slot is stranded");
    let r = verify_graph_with_scalar_replaced(&g, "test", VerifyOptions::default(), &empty);
    assert!(names_a_removed_slot(&r), "a set without it: still stranded");
    let r = verify_graph_with_scalar_replaced(&g, "test", VerifyOptions::default(), &named);
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

/// With the set in hand the lane also runs at `"pre-lower"` by the
/// build-profile default (debug builds; `CRATONVM_JIT_VERIFY_FRAME_STATES`
/// overrides), and never without it — `verify_graph` keeps its old reach.
#[test]
fn r11w18_the_pre_lower_frame_state_lane_needs_the_scalar_replaced_set() {
    let (g, gone) = graph_with_a_dead_snapshot_slot();
    let named: std::collections::HashSet<NodeId> = [gone].into_iter().collect();
    let empty: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    let on = frame_state_lane_after_ea_by_default(PHASE_PRE_LOWER);
    assert_eq!(
        on,
        env_flag("CRATONVM_JIT_VERIFY_FRAME_STATES").unwrap_or_else(|| {
            subchecks_follow_the_master_gate() || frame_state_lane_after_ea_released()
        })
    );
    assert!(!frame_state_lane_after_ea_by_default(PHASE_POST_OPTIMIZE));
    assert!(!frame_state_lane_after_ea_by_default(PHASE_POST_BUILD));

    let structural = VerifyOptions::structural();
    let r = verify_graph_with_scalar_replaced(&g, PHASE_PRE_LOWER, structural, &empty);
    assert_eq!(names_a_removed_slot(&r), on);
    let r = verify_graph_with_scalar_replaced(&g, PHASE_PRE_LOWER, structural, &named);
    assert!(!names_a_removed_slot(&r));
    let r = verify_graph(&g, PHASE_PRE_LOWER, structural);
    assert!(!names_a_removed_slot(&r), "no set, no lane, as before");
}

/// Item 2 of `r11-irback-release-builds-verify-structure-only`: the
/// operand-type table runs at `"post-optimize"` by the debug-build default,
/// nowhere else by default, and `CRATONVM_JIT_VERIFY_TYPES=0` turns it off.
#[test]
fn r11w18_the_operand_type_lane_is_staged_at_post_optimize_in_debug_builds() {
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_VERIFY_TYPES", None)],
        || {
            let debug = OPERAND_TYPE_LANE_STAGED_IN_DEBUG
                && (subchecks_follow_the_master_gate() || operand_type_lane_released());
            assert_eq!(operand_type_lane_staged_at(PHASE_POST_OPTIMIZE), debug);
            for phase in [
                PHASE_POST_BUILD,
                PHASE_PRE_LOWER,
                "post-escape-analysis",
                "test",
            ] {
                assert!(!operand_type_lane_staged_at(phase), "{phase}");
            }
        },
    );
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_VERIFY_TYPES", Some("0"))],
        || assert!(!operand_type_lane_staged_at(PHASE_POST_OPTIMIZE)),
    );
}

/// And when it runs, it runs the table through `verify_graph`: the
/// mixed-width compare the page names is refused at `"post-optimize"` in a
/// debug build, and only the lane's own finding differs from `"test"`.
#[test]
fn r11w18_a_mixed_width_compare_is_refused_at_post_optimize_when_staged() {
    let mut g = linear_graph();
    let i = g.add(Op::Const(1), IrType::Int, vec![], None);
    let l = g.add(Op::Const(2), IrType::Long, vec![], None);
    let cmp = g.add(Op::Cmp(crate::ir::CmpOp::Lt), IrType::Int, vec![i, l], None);
    g.nodes[g.exit as usize].inputs[1] = cmp;
    let opts = VerifyOptions {
        check_types: true,
        ..VerifyOptions::structural()
    };
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_VERIFY_TYPES", None)],
        || {
            let names_table = |r: CompileResult<()>| match r {
                Ok(()) => false,
                Err(b) => message(&b).contains("operand table requires"),
            };
            let staged = operand_type_lane_staged_at(PHASE_POST_OPTIMIZE);
            assert_eq!(
                names_table(verify_graph(&g, PHASE_POST_OPTIMIZE, opts)),
                staged || operand_type_lane_enabled()
            );
            assert_eq!(
                names_table(verify_graph(&g, "test", opts)),
                operand_type_lane_enabled()
            );
        },
    );
}


// ── Round 11 wave 3 (lane ircore) ────────────────────────────────────

/// The diamond, with its `Return` reading a load pinned in the TRUE arm
/// instead of the φ: a value read on the false path, which never computed
/// it. Every other lane accepts it.
fn diamond_returning_an_arm_load() -> Graph {
    let mut g = diamond_graph();
    let start = g.entry;
    let mem = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Proj(1) && n.ty == IrType::Memory)
        .expect("memory projection") as NodeId;
    let t = g
        .nodes
        .iter()
        .position(|n| {
            n.op == Op::Proj(0)
                && n.ty == IrType::Control
                && n.inputs
                    .first()
                    .is_some_and(|&p| g.nodes[p as usize].op == Op::If)
        })
        .expect("the true arm") as NodeId;
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(8), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![t, mem, base, off],
        None,
    );
    let ret = g.exit as usize;
    g.nodes[ret].inputs[1] = load;
    g
}

/// The dominance check names the read, in the control-structure lane:
/// forced on at any phase by `CRATONVM_JIT_VERIFY_BRANCHES=1`, off
/// everywhere under `=0`, and never part of the always-on lane.
#[test]
fn r11w3_a_read_of_a_sibling_arm_value_is_a_dominance_violation() {
    assert!(dominance_findings(&diamond_graph()).is_empty());
    let g = diamond_returning_an_arm_load();
    let f = dominance_findings(&g);
    assert!(
        f.len() == 1 && f[0].contains("Return") && f[0].contains("does not dominate"),
        "{f:?}"
    );

    let forced = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_VERIFY_BRANCHES", Some("1"))],
        || verify_graph(&g, "test", VerifyOptions::structural()),
    );
    let m = message(&forced.unwrap_err());
    assert!(m.contains("does not dominate"), "{m}");

    let off = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_VERIFY_BRANCHES", Some("0"))],
        || verify_graph(&g, PHASE_PRE_LOWER, VerifyOptions::structural()),
    );
    assert!(off.is_ok(), "{off:?}");
}

fn dominance_findings(g: &Graph) -> Vec<String> {
    let mut v = Violations::new();
    check_dominance(g, true, &mut v);
    v.list
}

/// Round 11 wave 4: evidence for promoting the control-structure lane. The
/// anchor and dominance checks are silent on graphs the bytecode front end
/// builds — a join inside a rotated loop, javac's nested counted loops, a
/// do-while headed at the entry, a lock held across a loop — both as built
/// and after `ir_optimize::optimize`, whose passes rewrite control
/// (`collapse_trivial_merges`, the unrollers, LICM's preheaders).
#[test]
fn r11w4_builder_graphs_pass_the_control_structure_lane() {
    let rotated_if_else: &[u8] = &[
        0x03, 0x3c, 0x03, 0x3d, 0xa7, 0x00, 0x16, 0x1c, 0x04, 0x7e, 0x9a, 0x00, 0x0a, 0x1b,
        0x1c, 0x60, 0x3c, 0xa7, 0x00, 0x06, 0x84, 0x01, 0xff, 0x84, 0x02, 0x01, 0x1c, 0x1a,
        0xa1, 0xff, 0xeb, 0x1b, 0xac, 0, 0,
    ];
    let javac_nested: &[u8] = &[
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x19, 0x03, 0x3e, 0x1d, 0x1a, 0xa2,
        0x00, 0x0c, 0x84, 0x01, 0x01, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf5, 0x84, 0x02, 0x01,
        0xa7, 0xff, 0xe8, 0x1b, 0xac, 0, 0,
    ];
    let do_while_at_entry: &[u8] = &[
        0x1a, 0x06, 0x64, 0x3b, 0x1a, 0x9d, 0xff, 0xfb, 0x1a, 0xac, 0, 0,
    ];
    let lock_across_loop: &[u8] = &[
        0x2a, 0x59, 0x4c, 0xc2, 0x1c, 0x99, 0x00, 0x06, 0xa7, 0xff, 0xfc, 0x2b, 0xc3, 0xb1, 0,
        0,
    ];
    let fixtures: [(&str, &[u8], usize, usize, usize); 4] = [
        ("rotated_if_else", rotated_if_else, 33, 1, 3),
        ("javac_nested", javac_nested, 33, 1, 4),
        ("do_while_at_entry", do_while_at_entry, 10, 1, 1),
        ("lock_across_loop", lock_across_loop, 14, 3, 3),
    ];
    for (name, code, len, params, locals) in fixtures {
        let Some(mut graph) = crate::ir::IrBuilder::new(params, locals).build(code, len) else {
            panic!("{name}: the fixture must build");
        };
        for stage in ["built", "optimized"] {
            if stage == "optimized" {
                crate::ir_optimize::optimize(&mut graph);
            }
            let mut v = Violations::new();
            check_control_anchors(&graph, &mut v);
            check_dominance(&graph, false, &mut v);
            assert!(v.is_empty(), "{name} ({stage}): {:?}", v.list);
        }
    }
}

/// Unset, the control-structure lane runs where the branch lane runs and
/// also at `"pre-lower"` — in debug builds only, and never at a phase
/// name the pipeline does not use.
#[test]
fn r11w3_the_control_structure_lane_adds_pre_lower_to_the_branch_lanes_phases() {
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_VERIFY_BRANCHES", None)],
        || {
            let debug = subchecks_follow_the_master_gate();
            for phase in [PHASE_POST_BUILD, PHASE_POST_OPTIMIZE, PHASE_PRE_LOWER] {
                assert_eq!(control_structure_lane_enabled(phase), debug, "{phase}");
            }
            for phase in ["post-escape-analysis", "test"] {
                assert!(!control_structure_lane_enabled(phase), "{phase}");
            }
            for phase in [
                PHASE_POST_BUILD,
                PHASE_POST_OPTIMIZE,
                PHASE_PRE_LOWER,
                "post-escape-analysis",
            ] {
                assert!(
                    !branch_completeness_lane_enabled(phase)
                        || control_structure_lane_enabled(phase),
                    "the anchor check must run wherever it ran before: {phase}"
                );
            }
        },
    );
}

/// Round 11 wave 8 (irback proposal W7-3): a surviving data φ with an
/// undefined input is named by the post-build check; a memory φ is not a
/// data φ, and the builder's own output for the `HashMap.getNode` shape
/// (whose φs over the unset `k` are retired) is clean.
#[test]
fn r11w8_a_surviving_data_phi_with_an_undefined_input_is_named() {
    let mut g = diamond_graph();
    let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
    let mut v = Violations::new();
    check_builder_phis_have_no_undefined_input(&g, &mut v);
    assert!(v.is_empty(), "twin: a fully defined φ: {:?}", v.list);

    g.nodes[phi as usize].inputs[2] = NO_NODE;
    let mut v = Violations::new();
    check_builder_phis_have_no_undefined_input(&g, &mut v);
    assert_eq!(v.total, 1, "{:?}", v.list);
    assert!(
        v.list[0].contains("value input[2] is NO_NODE"),
        "{:?}",
        v.list
    );
    // Every other phase keeps accepting the placeholder.
    assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());

    g.nodes[phi as usize].ty = IrType::Memory;
    let mut v = Violations::new();
    check_builder_phis_have_no_undefined_input(&g, &mut v);
    assert!(v.is_empty(), "a memory φ is not a data φ: {:?}", v.list);

    // static int f(int[] a, int c) { int k;
    //   if (c > 0) k = a.length; c--; while (c > 0) { k = c; c--; } return 0; }
    let code: [u8; 24] = [
        0x1b, 0x9e, 0x00, 0x06, 0x2a, 0xbe, 0x3d, 0x84, 0x01, 0xff, 0x1b, 0x9e, 0x00, 0x0b,
        0x1b, 0x3d, 0x84, 0x01, 0xff, 0xa7, 0xff, 0xf7, 0x03, 0xac,
    ];
    let mut b = crate::ir::IrBuilder::new(2, 3);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    let Some(built) = b.build(&code, 24) else {
        panic!("the getNode shape must build");
    };
    let mut v = Violations::new();
    check_builder_phis_have_no_undefined_input(&built, &mut v);
    assert!(v.is_empty(), "built: {:?}", v.list);
}

/// Round 11 wave 10 (`r11w9-ircore-anchor-rule-covers-five-of-the-pinned-ops`):
/// a `Guard` anchored at the entry whose condition only the true arm
/// computes would be sunk into that arm — a guard on one path only. The
/// dominance lane names it (it used to name only the five side-effecting
/// ops); the same guard anchored at the true arm is well formed.
#[test]
fn r11w10_a_guard_sunk_below_its_anchor_is_a_dominance_violation() {
    let mut g = diamond_graph();
    let start = g.entry;
    let if_node = g
        .nodes
        .iter()
        .position(|n| n.op == Op::If)
        .expect("the branch") as NodeId;
    let ctrl = g.nodes[if_node as usize].inputs[0];
    let t = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Proj(0) && n.inputs.first() == Some(&if_node))
        .expect("the true arm") as NodeId;
    let mem = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Proj(1) && n.ty == IrType::Memory)
        .expect("memory projection") as NodeId;
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(8), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![t, mem, base, off],
        Some(3),
    );
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let c = g.add(
        Op::Cmp(crate::ir::CmpOp::Ge),
        IrType::Int,
        vec![load, zero],
        Some(4),
    );
    let guard = g.add(Op::Guard { bci: 1 }, IrType::Void, vec![ctrl, c], Some(1));
    let guard_label = label(&g, guard);

    let f = dominance_findings(&g);
    assert!(
        f.len() == 1 && f[0].contains(&guard_label) && f[0].contains("does not dominate"),
        "{f:?}"
    );
    let forced = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_VERIFY_BRANCHES", Some("1"))],
        || verify_graph(&g, "test", VerifyOptions::structural()),
    );
    let m = message(&forced.unwrap_err());
    assert!(m.contains(&guard_label), "{m}");

    assert!(g.set_input(guard, 0, t));
    let f = dominance_findings(&g);
    assert!(f.is_empty(), "twin: anchored where its condition is: {f:?}");
}
