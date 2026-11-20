use super::*;
use crate::ir::{IrType, NO_NODE};
use crate::ir_schedule;

fn trusted_static_array_graph(
    length: Option<i32>,
    index: i64,
    masked: bool,
) -> (Graph, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(
        Op::LoadStatic {
            class_id: 579,
            field_index: 2,
            type_tag: b'[',
            is_volatile: false,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    let k = g.add(Op::Const(index), IrType::Int, vec![], None);
    let idx = if masked {
        let arg = g.add(Op::Param(0), IrType::Int, vec![start], None);
        g.add(Op::And, IrType::Int, vec![arg, k], Some(1))
    } else {
        k
    };
    let load = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, arr, arr, idx],
        Some(2),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, load], Some(3));
    if let Some(n) = length {
        g.trusted_static_array_lengths.push(((579, 2), n));
    }
    (g, arr, load)
}

/// MX16-3: a `multianewarray`'s outer array is a fresh, never-null
/// reference, like the other allocations.
#[test]
fn a_multianewarray_is_definitely_non_null() {
    assert!(definitely_non_null(&Op::MultiNewArray { site: 7 }));
    assert!(definitely_non_null(&Op::NewArray {
        element_type: 10,
        component_class_id: 0
    }));
    assert!(!definitely_non_null(&Op::Param(0)));
}

fn empty_graph() -> Graph {
    Graph::empty(0)
}

/// `a[i] + a[i]` -- one array, one index, two loads in one block. The
/// SECOND load's checks are both redundant; the first load's are not.
///
/// This is the shape the pass exists for and the one a dominance-only
/// implementation would miss entirely, because both nodes are in the same
/// block and `dom` is reflexive.
#[test]
fn the_second_access_to_the_same_element_needs_no_checks() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let l1 = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, arr, idx],
        Some(0),
    );
    let l2 = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, arr, idx],
        Some(1),
    );
    let sum = g.add(Op::Add, IrType::Int, vec![l1, l2], Some(2));
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));

    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    assert!(
        !e.null_elided(l1) && !e.bounds_elided(l1),
        "the FIRST access proves the facts; it cannot use them",
    );
    assert!(
        e.null_elided(l2),
        "the second access's base was dereferenced by the first",
    );
    assert!(
        e.bounds_elided(l2),
        "the second access checks the same (base, index) SSA pair",
    );
}

/// A DIFFERENT index against the same array keeps its bounds check and
/// loses only the null check. Getting this wrong is an out-of-bounds read
/// in compiled code, so it gets its own test rather than riding on the one
/// above.
#[test]
fn a_different_index_keeps_its_bounds_check() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let i = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let j = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let l1 = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, arr, i],
        Some(0),
    );
    let l2 = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, arr, j],
        Some(1),
    );
    let sum = g.add(Op::Add, IrType::Int, vec![l1, l2], Some(2));
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));

    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    assert!(
        e.null_elided(l2),
        "the array is the same value and was already dereferenced",
    );
    assert!(
        !e.bounds_elided(l2),
        "a DIFFERENT index is a different bounds question",
    );
}

/// A freshly allocated array is non-null by construction, so even its
/// first access needs no null check -- but it still needs its bounds check,
/// because nothing here knows the length.
#[test]
fn a_fresh_allocation_needs_no_null_check_and_still_needs_bounds() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let len = g.add(Op::Const(4), IrType::Int, vec![], None);
    let arr = g.add(
        Op::NewArray {
            element_type: 10,
            component_class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, len],
        Some(0),
    );
    let idx = g.add(Op::Const(0), IrType::Int, vec![], None);
    let l = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, arr, arr, idx],
        Some(1),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, l], Some(2));

    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    assert!(e.null_elided(l), "an allocation result cannot be null");
    assert!(
        !e.bounds_elided(l),
        "nothing here proves the index is inside the length",
    );
}

/// The kill switch must produce the pre-change answer at every site, or the
/// A/B compares two things.
///
/// # This test used to assert nothing, most of the time
///
/// It reached the gate through `flags::with_thread_overrides`, which is
/// THREAD-scoped, while `enabled()`'s cache is a process-wide `OnceLock`.
/// Two consequences, and the second is the one that bit:
///
///  * whenever another test had already initialised the lock, `enabled()`
///    still answered `true` inside the override, and the body returned
///    early with no assertion at all -- which under a parallel run is the
///    common case, so the kill switch was effectively untested;
///  * whenever THIS test won the race instead, it memoized **false** for
///    the whole binary and every test asserting a check IS elided failed.
///    Six did, together, on 2026-09-07.
///
/// `CheckElimForce` is thread-local, so there is no race to win or lose:
/// the assertion below now runs on every execution, and no other test can
/// see the setting.
#[test]
fn the_kill_switch_elides_nothing() {
    let _off = CheckElimForce::off();
    assert!(
        !enabled(),
        "the force must actually reach the gate, or everything below is \
         asserting the DEFAULT-ON behaviour under a kill-switch name"
    );
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let l1 = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, arr, idx],
        Some(0),
    );
    let l2 = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, arr, idx],
        Some(1),
    );
    let sum = g.add(Op::Add, IrType::Int, vec![l1, l2], Some(2));
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));
    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    assert!(!e.null_elided(l2) && !e.bounds_elided(l2));
}

/// The force is THREAD-local and leaves the process-wide cache alone.
///
/// This is the regression test for the defect itself rather than for the
/// pass: it proves that switching the gate off for one test cannot switch
/// it off for the binary. Without it, a future edit that "simplifies"
/// `CheckElimForce` back into `with_thread_overrides` reintroduces the
/// original failure and nothing here notices -- the six tests it breaks
/// only fail when the interleaving happens to put the kill-switch test
/// first, which is why it took a loaded box to surface.
#[test]
fn the_kill_switch_force_does_not_escape_its_own_thread() {
    {
        let _off = CheckElimForce::off();
        assert!(!enabled(), "the force must apply on this thread");
    }
    // Dropped: this thread sees the real gate again.
    assert!(enabled(), "the force leaked past its guard on this thread");
    // And it was never visible on another thread, whichever order the two
    // ran in.
    let elsewhere = std::thread::spawn(enabled).join().expect("probe thread");
    assert!(
        elsewhere,
        "the force was visible on another thread: the gate's cache has been \
         poisoned for the whole binary, which is the 2026-09-07 defect"
    );
}

/// The range proof has its own switch, and its own force.
///
/// `CRATONVM_JIT_IR_BCE_RANGE=0` keeps the dominating-redundancy pass and
/// drops only the range proof, so the two are separately switchable in
/// production; a test that could not tell them apart could not test that
/// split. The counted loop below is elided by the RANGE proof alone -- with
/// it off, nothing dominates the access and the check must stay.
#[test]
fn the_range_switch_drops_only_the_range_proof() {
    let _off = RangeForce::off();
    assert!(!range_enabled(), "the force must reach the range gate");
    assert!(
        enabled(),
        "and must leave the dominating-redundancy pass alone -- that is \
         what makes this a separate switch"
    );
}

/// The pass reads `Block::nodes` in order and treats that as PROGRAM order
/// within the block. `ir_lower` emits a block by iterating the same vector,
/// so the two agree -- but only for as long as nobody changes one of them.
///
/// Asserting it on the source is the cheapest way to keep the coupling
/// visible: a behavioural test cannot distinguish "the orders agree" from
/// "this graph happens not to care".

/// Build `for (int i = 0; i CMP a.length; i += stride) a[i] = 1;` and
/// return `(graph, schedule, the ArrayStore)`.
///
/// `guard_back_edge` chooses between the two loop shapes the range proof
/// distinguishes. `true` is the ordinary `for` loop, where the bounds test
/// IS the loop test and nothing re-enters the header without passing it.
/// `false` wraps the test in an inner `if` under an unconditional loop --
/// `for (int i = 0; ; i++) if (c) if (i < a.length) a[i] = 1;` -- where the
/// test still dominates the ACCESS but the increment runs on a path that
/// never took it.
fn counted_loop(
    cmp_op: CmpOp,
    stride: i64,
    guard_back_edge: bool,
    bound_is_same_array: bool,
    init_is_const: bool,
) -> (Graph, crate::ir_schedule::Schedule, NodeId) {
    counted_loop_with_header(
        Op::Region,
        cmp_op,
        0,
        stride,
        guard_back_edge,
        bound_is_same_array,
        init_is_const,
    )
}

#[allow(clippy::too_many_arguments)]
fn counted_loop_with_header(
    header: Op,
    cmp_op: CmpOp,
    // Which `Proj` of the bounds branch opens the loop BODY. `0` is the
    // bottom-tested `if_icmplt body` shape; `1` is the top-tested
    // `if_icmpge exit` shape, where the body is the fall-through.
    body_edge: u8,
    stride: i64,
    guard_back_edge: bool,
    bound_is_same_array: bool,
    init_is_const: bool,
) -> (Graph, crate::ir_schedule::Schedule, NodeId) {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let other = g.add(Op::Param(1), IrType::Ref, vec![start], None);

    // Header, back edge patched in once the body exists.
    let region = g.add(header, IrType::Control, vec![c0], None);
    let init = if init_is_const {
        g.add(Op::Const(0), IrType::Int, vec![], None)
    } else {
        g.add(Op::Param(2), IrType::Int, vec![start], None)
    };
    let iv = g.add(Op::Phi, IrType::Int, vec![region, init], None);
    let step = g.add(Op::Const(stride), IrType::Int, vec![], None);
    let iv_next = g.add(Op::Add, IrType::Int, vec![iv, step], None);
    g.nodes[iv as usize].inputs.push(iv_next);

    // The array whose length bounds the loop -- the one being indexed, or
    // a different one.
    let bound_arr = if bound_is_same_array { arr } else { other };
    // `Op::ArrayLength` is [ctrl, mem, array_ref].
    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![region, m0, bound_arr],
        None,
    );

    let (body_ctrl, back_ctrl) = if guard_back_edge {
        // region -> If(i < len) -> true = body, and the body IS the back
        // edge. Nothing reaches the header without the test.
        let cmp = g.add(Op::Cmp(cmp_op), IrType::Int, vec![iv, len], None);
        let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let body = if body_edge == 0 { t } else { f };
        (body, body)
    } else {
        // region -> If(c) -> true -> If(i < len) -> true = body.
        // The BACK EDGE is the outer false edge, which the bounds test
        // does not dominate.
        let c = g.add(Op::Param(3), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let outer_cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![c, zero], None);
        let outer = g.add(Op::If, IrType::Control, vec![region, outer_cmp], None);
        let ot = g.add(Op::Proj(0), IrType::Control, vec![outer], None);
        let of = g.add(Op::Proj(1), IrType::Control, vec![outer], None);
        let cmp = g.add(Op::Cmp(cmp_op), IrType::Int, vec![iv, len], None);
        let iff = g.add(Op::If, IrType::Control, vec![ot, cmp], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let _tf = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        (t, of)
    };
    g.nodes[region as usize].inputs.push(back_ctrl);

    // `a[i] = 1` -- a STORE, so nothing can treat it as dead.
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let st = g.add(
        Op::ArrayStore(MemKind::Int),
        IrType::Void,
        vec![body_ctrl, m0, arr, iv, one],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![body_ctrl, iv], Some(1));

    let sched = ir_schedule::schedule(&g);
    (g, sched, st)
}

/// The shape this pass exists for: `for (int i = 0; i < a.length; i++)`.
/// No check is needed at all, and no earlier access proves it -- which is
/// exactly what the dominating-redundancy pass could never do.
#[test]
fn the_classic_counted_loop_needs_no_bounds_check() {
    let (g, sched, st) = counted_loop(CmpOp::Lt, 1, true, true, true);
    let e = analyze(&g, &sched);
    assert!(
        e.bounds_elided(st),
        "i is a unit-stride induction variable from 0 and the loop test is \
         i < a.length for the SAME array, so 0 <= i < a.length holds",
    );
    assert!(
        e.bounds_range_proved(st),
        "and it was the RANGE pass that proved it, not redundancy -- there \
         is no earlier access to be redundant with",
    );
}

/// The SAME loop with the header op `IrBuilder` actually produces.
///
/// `ensure_merge` builds every merge point -- loop headers included -- as
/// `Op::Merge`; `activate_loop_header` fills in its inputs and never
/// rewrites the op, so `Op::Region` never appears in a graph built from
/// bytecode. A version of this pass that matched only `Op::Region` passed
/// the test above and every other test in this module, and would have
/// eliminated exactly zero bounds checks in a compiled method. This is the
/// test that fails in that case.
#[test]
fn the_header_op_the_builder_actually_emits_is_recognised() {
    let (g, sched, st) = counted_loop_with_header(Op::Merge, CmpOp::Lt, 0, 1, true, true, true);
    let e = analyze(&g, &sched);
    assert!(
        e.bounds_elided(st) && e.bounds_range_proved(st),
        "a loop header is an Op::Merge in every graph the IR builder \
         produces; matching only Op::Region makes this pass inert on real \
         code while its hand-built tests stay green",
    );
}

/// The top-tested shape, where the useful fact is on the FALSE edge.
///
/// javac compiles a `while`/`for` head as `if_icmpge exit`: branch OUT when
/// the index has reached the length, fall through into the body. The
/// builder keeps that polarity, so the comparison is `Ge` and the body is
/// `Proj(1)`. A pass that looked only for `Lt` on `Proj(0)` would find
/// nothing here while every other test in this module stayed green.
#[test]
fn the_bound_is_read_off_the_false_edge_of_a_negated_test() {
    let (g, sched, st) = counted_loop_with_header(Op::Merge, CmpOp::Ge, 1, 1, true, true, true);
    let e = analyze(&g, &sched);
    assert!(
        e.bounds_elided(st) && e.bounds_range_proved(st),
        "not (i >= a.length) is i < a.length, and that is the edge the loop \
         body actually hangs off in javac output",
    );
}

/// A stride of four is REFUSED, and not because four is unusual.
///
/// The non-negativity proof runs: the guard gives `i <= len - 1`, so after
/// the increment `i <= len - 1 + stride`. `len` can be `i32::MAX - 2`, so
/// any stride above three overflows there, and a wrapped `i` is negative --
/// which passes the SIGNED `i < len` test and then indexes behind the
/// object with the check removed. (Until round 12 wave 3 this test used a
/// stride of two, against a cap of `i32::MAX`; see `MAX_PROVEN_STRIDE`.)
#[test]
fn a_stride_the_proof_cannot_close_keeps_its_check() {
    let (g, sched, st) = counted_loop(CmpOp::Lt, 4, true, true, true);
    let e = analyze(&g, &sched);
    assert!(
        !e.bounds_elided(st),
        "a stride above three can carry the induction variable past len and \
         into overflow; the check stays",
    );
    // A negative step from 0 is not an up-count: `i` goes negative at once.
    let (g, sched, st) = counted_loop(CmpOp::Lt, -1, true, true, true);
    assert!(!analyze(&g, &sched).bounds_elided(st));
}

/// The test dominating the ACCESS is not enough: the increment must be
/// dominated too. `for (int i = 0; ; i++) if (c) if (i < a.length) a[i]=1;`
/// has a bounds test on every access and still lets `i` run to `i32::MAX`
/// and wrap, after which that same test passes on a negative index.
#[test]
fn a_back_edge_that_skips_the_test_keeps_its_check() {
    let (g, sched, st) = counted_loop(CmpOp::Lt, 1, false, true, true);
    let e = analyze(&g, &sched);
    assert!(
        !e.bounds_elided(st),
        "the increment is reachable without the bounds test having held, \
         so nothing bounds the induction variable",
    );
}

/// `i < b.length` says nothing about `a[i]`. The pass matches the array
/// SSA node, so two arrays that happen to be the same length are still two
/// arrays.
#[test]
fn a_bound_taken_from_a_different_array_proves_nothing() {
    let (g, sched, st) = counted_loop(CmpOp::Lt, 1, true, false, true);
    let e = analyze(&g, &sched);
    assert!(
        !e.bounds_elided(st),
        "the loop is bounded by another array's length",
    );
}

/// `i <= a.length` leaves `i == a.length` reachable, which is the exact
/// off-by-one the check exists to catch.
#[test]
fn a_non_strict_bound_is_not_a_bound() {
    let (g, sched, st) = counted_loop(CmpOp::Le, 1, true, true, true);
    let e = analyze(&g, &sched);
    assert!(!e.bounds_elided(st), "i <= len still admits i == len");
}

/// A parameter start could be negative, and a negative index PASSES the
/// signed loop test. The upper bound alone is never enough.
#[test]
fn an_induction_variable_from_an_unknown_start_keeps_its_check() {
    let (g, sched, st) = counted_loop(CmpOp::Lt, 1, true, true, false);
    let e = analyze(&g, &sched);
    assert!(
        !e.bounds_elided(st),
        "nothing proves the start is non-negative, and i < len does not",
    );
}

/// Build `if (idx < a.length) a[idx] = 1;` where `idx` is a parameter,
/// optionally wrapped in one narrowing conversion, and return
/// `(graph, schedule, the ArrayStore)`.
///
/// Straight-line, no loop: the upper bound is a plain dominating branch, so
/// the ONLY question left is `idx >= 0` — which is the question the SSA
/// value-range lattice answers and the three hand-written shape arms do not.
fn guarded_access_with(narrow: Option<Op>) -> (Graph, Schedule, NodeId) {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let raw = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let idx = match narrow {
        Some(op) => g.add(op, IrType::Int, vec![raw], None),
        None => raw,
    };
    let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![idx, len], None);
    let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
    let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let st = g.add(
        Op::ArrayStore(MemKind::Int),
        IrType::Void,
        vec![t, m0, arr, idx, one],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![t, idx], Some(1));
    let sched = ir_schedule::schedule(&g);
    (g, sched, st)
}

/// `range_analysis::analyze_graph` — a sound SSA interval lattice with
/// JVMS-correct transfer functions, fully tested — had no call site outside
/// its own `mod tests`, so this tier's `idx >= 0` question was answered by
/// three hand-written shapes and nothing else.
///
/// `(char) p` is `Op::I2C`, whose range is `[0, 65535]`. It is not a
/// constant, not an array length and not a phi, so every shape arm refuses
/// it — and the check on `a[(char) p]` under a dominating `(char) p <
/// a.length` stayed, forever, for an index that cannot be negative.
#[test]
fn a_char_narrowed_index_is_proven_non_negative_by_the_range_lattice() {
    let (g, sched, st) = guarded_access_with(Some(Op::I2C));
    let e = analyze(&g, &sched);
    assert!(
        e.bounds_elided(st),
        "(char) p is in [0, 65535], and the branch bounds it above",
    );
    assert!(
        e.bounds_range_proved(st),
        "and the RANGE pass is what proved it — there is no earlier access \
         for the redundancy pass to work from",
    );
}

/// The twin that differs only in the conversion: `(byte) p` is
/// `[-128, 127]`, which admits a negative index, and a negative index
/// PASSES the signed `idx < a.length` test. The check must stay.
///
/// Without this, the test above would pass just as well against a range
/// arm that answered `true` for everything.
#[test]
fn a_byte_narrowed_index_still_keeps_its_check() {
    let (g, sched, st) = guarded_access_with(Some(Op::I2B));
    let e = analyze(&g, &sched);
    assert!(
        !e.bounds_elided(st),
        "(byte) p can be -1, which satisfies idx < a.length and indexes \
         behind the array",
    );
}

/// And the un-narrowed parameter, to pin that the lattice is what changed
/// the answer rather than the fixture shape.
#[test]
fn an_unconstrained_index_keeps_its_check() {
    let (g, sched, st) = guarded_access_with(None);
    let e = analyze(&g, &sched);
    assert!(!e.bounds_elided(st), "nothing bounds p below");
}

/// `if (i >= 0) { if (i < a.length) a[i] = 1; }` -- B3's shape, built with
/// the SIGN test on the outer branch so the only thing that can prove
/// `i >= 0` is the edge narrowing.
///
/// `sign_op` picks the outer comparison, and `taken` picks which of its two
/// edges the inner branch hangs off, so one fixture covers the true edge,
/// the false edge (where the fact is the NEGATION) and the inverted
/// spelling.
fn nested_guarded_access_vs(
    sign_op: CmpOp,
    bound: i64,
    hang_off_true_edge: bool,
) -> (Graph, Schedule, NodeId) {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);

    let k = g.add(Op::Const(bound), IrType::Int, vec![], None);
    let sign = g.add(Op::Cmp(sign_op), IrType::Int, vec![idx, k], None);
    let outer = g.add(Op::If, IrType::Control, vec![c0, sign], None);
    let ot = g.add(Op::Proj(0), IrType::Control, vec![outer], None);
    let of = g.add(Op::Proj(1), IrType::Control, vec![outer], None);
    let inner_ctrl = if hang_off_true_edge { ot } else { of };

    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![inner_ctrl, m0, arr],
        None,
    );
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![idx, len], None);
    let iff = g.add(Op::If, IrType::Control, vec![inner_ctrl, cmp], None);
    let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let _f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let st = g.add(
        Op::ArrayStore(MemKind::Int),
        IrType::Void,
        vec![t, m0, arr, idx, one],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![t, idx], Some(1));
    let sched = ir_schedule::schedule(&g);
    (g, sched, st)
}

/// The common case: the outer test compares against zero.
fn nested_guarded_access(sign_op: CmpOp, hang_off_true_edge: bool) -> (Graph, Schedule, NodeId) {
    nested_guarded_access_vs(sign_op, 0, hang_off_true_edge)
}

/// **B3.** The shape `NOTES-opts.md` named as the thing the path-INSENSITIVE
/// evaluator "proves nothing about".
///
/// `i` is a bare `Param`: no constant, no array length, no unit-stride phi,
/// and `analyze_graph`'s whole-method answer for it is top. The ONLY thing
/// that makes `a[i]` provably in bounds is that reaching it required
/// `i >= 0` to be true, which is a fact about an `If` EDGE.
/// `an_unconstrained_index_keeps_its_check` is the same fixture without the
/// outer guard and still refuses, which is what stops this from passing for
/// a reason other than the one it names.
#[test]
fn an_index_guarded_non_negative_is_proven_by_edge_narrowing() {
    let (g, sched, st) = nested_guarded_access(CmpOp::Ge, true);
    let e = analyze(&g, &sched);
    assert!(
        e.bounds_elided(st),
        "i >= 0 on the way in, i < a.length at the access: both halves proven",
    );
    assert!(
        e.bounds_range_proved(st),
        "and by the RANGE pass -- there is no earlier access here for the \
         redundancy pass to work from",
    );
}

/// The same fact reached through the FALSE edge of `i < 0`.
#[test]
fn a_fact_can_be_read_off_a_false_edge() {
    let (g, sched, st) = nested_guarded_access(CmpOp::Lt, false);
    let e = analyze(&g, &sched);
    assert!(e.bounds_elided(st), "!(i < 0) is i >= 0, which is enough");
}

/// The arm that tells `CmpOp::negate` from `CmpOp::swapped` -- which the
/// zero-bound fixtures CANNOT. Against `0`, `Lt.negate() == Ge` and
/// `Lt.swapped() == Gt` both prove non-negative, so either would elide and
/// a test built on them passes with the two confused. This was that test
/// for one sitting: it was written claiming to separate them, and an A/B
/// that replaced `negate` with `swapped` did not move it.
///
/// Against `-1` they separate. The false edge of `i < -1` proves
/// `i >= -1`, which still admits `-1`, so the check must STAY. The mirror
/// image would read `i > -1` -- i.e. `i >= 0` -- and delete a check that a
/// value of `-1` needs: `-1` passes the signed `i < a.length` test and
/// indexes behind the array.
#[test]
fn the_false_edge_takes_the_negation_and_not_the_mirror_image() {
    let (g, sched, st) = nested_guarded_access_vs(CmpOp::Lt, -1, false);
    let e = analyze(&g, &sched);
    assert!(
        !e.bounds_elided(st),
        "!(i < -1) is i >= -1, and i == -1 indexes behind the array",
    );
}

/// And the arm that must NOT elide: hanging the access off the TRUE edge of
/// `i < 0` puts it where `i` is provably negative.
///
/// A negative index passes the signed `i < a.length` test and indexes
/// behind the array, which is the out-of-bounds heap write the range
/// module's header opens by naming. If edge narrowing ever reports a fact
/// for the wrong edge, this is the test that fails.
#[test]
fn an_index_guarded_negative_keeps_its_check() {
    let (g, sched, st) = nested_guarded_access(CmpOp::Lt, true);
    let e = analyze(&g, &sched);
    assert!(
        !e.bounds_elided(st),
        "on the true edge of `i < 0` the index is negative and the check is \
         the only thing standing between it and the heap",
    );
}

/// The walk accumulates facts as it goes and queries them with
/// `dominates`, so a dominating block must be processed first. Block
/// indices are POST-layout and frequency-driven layout permutes them, so
/// index order does not guarantee that; this order does.
///
/// Asserted as the helper's contract rather than end-to-end: producing a
/// schedule whose layout actually inverts a dominator pair needs profile
/// frequencies a unit fixture cannot pin, and a behavioural test that
/// happened to get the identity permutation would prove nothing.
#[test]
fn the_block_walk_visits_every_dominator_before_what_it_dominates() {
    for (_g, sched) in [
        {
            let (g, s, _) = counted_loop(CmpOp::Lt, 1, true, true, true);
            (g, s)
        },
        {
            let (g, s, _) = counted_loop(CmpOp::Lt, 1, false, true, true);
            (g, s)
        },
    ] {
        let order = dominator_order(&sched);
        assert_eq!(
            order.len(),
            sched.blocks.len(),
            "every block must be visited exactly once"
        );
        let mut seen = vec![false; sched.blocks.len()];
        for &b in &order {
            assert!(!seen[b], "block {b} visited twice");
            seen[b] = true;
        }
        let position: Vec<usize> = {
            let mut p = vec![0usize; order.len()];
            for (i, &b) in order.iter().enumerate() {
                p[b] = i;
            }
            p
        };
        for b in 0..sched.blocks.len() {
            for d in 0..sched.blocks.len() {
                if d != b && sched.dom.dominates(d, b) {
                    assert!(
                        position[d] < position[b],
                        "block {d} dominates {b} but is walked after it, so \
                         its facts are not yet available when {b} is processed"
                    );
                }
            }
        }
    }
}

#[test]
fn the_block_node_order_is_the_emission_order() {
    let src = concat!(
        include_str!("../ir_lower/mod.rs"),
        "\n",
        include_str!("../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../ir_lower/scoped_session_confined_rows.rs")
    );
    assert!(
        src.contains("for &node_id in &block.nodes")
            || src.contains("for &id in &block.nodes")
            || src.contains("block.nodes.iter()"),
        "ir_lower must still emit a block by walking `block.nodes` in order; \
         this pass's same-block reasoning depends on it",
    );
}

/// A field `Load` proves nothing about its base's nullness (r9).
///
/// When this test was written, `ir_lower`'s helper-less inline getfield
/// answered `0` for a null receiver and CONTINUED, so a `Load` of the
/// interned `null` literal was followed by the next node running with that
/// base still null; with the `Load` recording a dereference fact, the
/// `ArrayLoad` of the same literal below had its null test elided and read
/// `[0 + HEADER + idx*4]`. Since round 9 wave 2 (iropt X3) that inline
/// `Load` deopts (`NullCheck`) on a null receiver instead -- but this pass
/// cannot see which lowering a `Load` gets (a helper-backed one, a spliced
/// one, one whose check a later change relaxes), so it still must not
/// treat the `Load` as proof, and the test still pins that.
#[test]
fn a_field_load_does_not_prove_its_base_non_null() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let field = g.add(Op::Const(0), IrType::Int, vec![], None);
    let idx = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let f = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, null, field],
        Some(0),
    );
    let a = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, f, null, idx],
        Some(1),
    );
    let sum = g.add(Op::Add, IrType::Int, vec![f, a], Some(2));
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));

    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    assert!(
        !e.null_elided(a),
        "a field read of the base is not a null test the array access can lean on"
    );
}

/// An access in a block the entry cannot reach keeps BOTH of its checks.
///
/// `Dominators::dominates(d, b)` answers `true` for every `d` when `b` is
/// unreachable -- its documented contract, matching the dense fixpoint it
/// replaced. Read as a permission that is fail-OPEN: without the
/// reachability test in [`dominates`], the second `ArrayLoad` below sits in
/// a block with no predecessors and had its null test AND its bounds test
/// elided on the strength of the first one, which does not dominate it at
/// all. `ir_lower` emits every block in `schedule.blocks`, so the
/// guard-free bytes are really emitted; only "no branch targets that block"
/// kept it off the wrong-code list.
#[test]
fn unreachable_block_proves_nothing() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let l1 = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, arr, idx],
        Some(0),
    );
    // A block head with no predecessors: the entry cannot reach it.
    let orphan = g.add(Op::Region, IrType::Control, vec![], None);
    let l2 = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![orphan, mem, arr, idx],
        Some(1),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, l1], Some(2));

    let sched = ir_schedule::schedule(&g);
    let lb = sched.node_to_block[l2 as usize];
    assert!(
        lb < sched.blocks.len() && !sched.dom.is_reachable(lb),
        "the second access must land in the unreachable block for this test to \
         mean anything (block {lb} of {})",
        sched.blocks.len(),
    );
    let e = analyze(&g, &sched);
    assert!(
        !e.null_elided(l2),
        "nothing dominates an unreachable block, so no dereference proves its base",
    );
    assert!(
        !e.bounds_elided(l2),
        "nothing dominates an unreachable block, so no earlier check proves this pair",
    );
}

#[test]
fn indy_root_ref_array_with_const_ref_meta_elides_null_and_bounds_checks() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let slot_addr = 0xDEAD_1000;
    let arr = g.add(
        Op::IndyRootRef { slot_addr },
        IrType::Ref,
        vec![ctrl],
        Some(0),
    );
    g.const_ref_meta.insert(
        slot_addr,
        crate::ir::ConstRefMeta {
            non_null: true,
            exact_class_id: Some(77),
            prim_array_tag: None,
            array_length: Some(4),
            static_field: Some((579, 2)),
        },
    );
    let idx = g.add(Op::Const(3), IrType::Int, vec![], None);
    let load = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, arr, idx],
        Some(1),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, load], Some(2));

    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    assert!(
        e.null_elided(load),
        "const_ref_meta.non_null must elide null check"
    );
    assert!(
        e.bounds_elided(load),
        "const_ref_meta.array_length=4 with idx=3 must elide bounds check"
    );
}
