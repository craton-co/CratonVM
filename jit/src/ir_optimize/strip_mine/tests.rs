// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 2 (lane STRIPMINE): the strip-mined main loop, run by a small
//! control-flow interpreter against the graph it came from. j27 wave 4 (lane
//! STRIPOSR): the same at `long` width and over array reads.

use super::*;
use crate::ir::CmpOp;
use rustc_hash::FxHashMap;

const F: i64 = 4;

/// The fixture's body (wave 4).
#[derive(Clone, Copy)]
struct ArrayBody {
    /// Copy `k` reads `a[i + k*s + past]`: `0` stays inside the main test's
    /// margin `i < a.length - 3s`, `1` reads past it.
    past: i64,
    /// `i0 = 0` (provably non-negative) rather than the parameter.
    zero_start: bool,
}

/// Which main/post loop [`build`] makes.
#[derive(Clone, Copy)]
struct Spec {
    s: i64,
    /// `Int`, or `Long` (no array body then).
    ty: IrType,
    array: Option<ArrayBody>,
}

impl Spec {
    fn int(s: i64) -> Self {
        Spec {
            s,
            ty: IrType::Int,
            array: None,
        }
    }
}

/// The main/post shape `main_post_unroll_loop` builds for
/// `int f(int i0, int n) { int a = 1; for (int i = i0; i < n; i += s) a = a * 31 + i; return a; }`
/// unrolled by four: licence `lim = n - 3s; if (lim < n)`, the pc-less main
/// header `H` with four body copies, the join `J`, the original loop `R` (pc
/// 2) as the post loop. At `long` width with [`Spec::ty`]; with
/// [`Spec::array`], `a = a * 31 + arr[i + ..]` over `n = arr.length`.
struct Fixture {
    g: Graph,
    main: NodeId,
    main_acc: NodeId,
    post: NodeId,
    post_acc: NodeId,
    /// The main body's element reads (empty without an array body).
    main_loads: Vec<NodeId>,
}

fn add(g: &mut Graph, op: Op, ty: IrType, inputs: Vec<NodeId>) -> NodeId {
    g.add(op, ty, inputs, None)
}

fn fixture(s: i64) -> Fixture {
    build(Spec::int(s))
}

/// The value copy `k`'s accumulator step adds: the index itself, or the
/// element read at it.
fn term(g: &mut Graph, sp: Spec, ctrl: NodeId, mem: NodeId, arr: NodeId, idx: NodeId, pc: Option<usize>) -> NodeId {
    match sp.array {
        None => idx,
        Some(_) => g.add(Op::ArrayLoad(MemKind::Int), IrType::Int, vec![ctrl, mem, arr, idx], pc),
    }
}

fn build(sp: Spec) -> Fixture {
    let (ty, s) = (sp.ty, sp.s);
    let ctl = IrType::Control;
    let mut g = Graph::empty(0);
    let start = add(&mut g, Op::Start, ctl, vec![]);
    g.entry = start;
    let entry = add(&mut g, Op::Proj(0), ctl, vec![start]);
    let mem = add(&mut g, Op::Proj(1), IrType::Memory, vec![start]);
    let p0 = add(&mut g, Op::Param(0), ty, vec![start]);
    let p1 = add(&mut g, Op::Param(1), ty, vec![start]);
    let arr = add(&mut g, Op::Param(2), IrType::Ref, vec![start]);
    let (i0, n) = match sp.array {
        None => (p0, p1),
        Some(a) => {
            let len = add(&mut g, Op::ArrayLength, IrType::Int, vec![entry, mem, arr]);
            let i0 = if a.zero_start { add(&mut g, Op::Const(0), ty, vec![]) } else { p0 };
            (i0, len)
        }
    };
    let past = sp.array.map_or(0, |a| a.past);
    let acc0 = add(&mut g, Op::Const(1), ty, vec![]);
    let k31 = add(&mut g, Op::Const(31), ty, vec![]);
    let ks = add(&mut g, Op::Const(s), ty, vec![]);
    // The licence.
    let d = add(&mut g, Op::Const((F - 1) * s), ty, vec![]);
    let lim = add(&mut g, Op::Sub, ty, vec![n, d]);
    let ok = add(&mut g, Op::Cmp(CmpOp::Lt), IrType::Int, vec![lim, n]);
    let pre = add(&mut g, Op::If, ctl, vec![entry, ok]);
    let into_main = add(&mut g, Op::Proj(0), ctl, vec![pre]);
    let skip = add(&mut g, Op::Proj(1), ctl, vec![pre]);
    // The main loop.
    let h = add(&mut g, Op::Merge, ctl, vec![into_main, NO_NODE]);
    let hi = add(&mut g, Op::Phi, ty, vec![h, i0, NO_NODE]);
    let hacc = add(&mut g, Op::Phi, ty, vec![h, acc0, NO_NODE]);
    let hc = add(&mut g, Op::Cmp(CmpOp::Lt), IrType::Int, vec![hi, lim]);
    let ht = add(&mut g, Op::If, ctl, vec![h, hc]);
    let hs = add(&mut g, Op::Proj(0), ctl, vec![ht]);
    let hx = add(&mut g, Op::Proj(1), ctl, vec![ht]);
    let mut a = hacc;
    let mut main_loads = Vec::new();
    for k in 0..F {
        let off = k * s + past;
        let ik = if off == 0 {
            hi
        } else {
            let offc = add(&mut g, Op::Const(off), ty, vec![]);
            add(&mut g, Op::Add, ty, vec![hi, offc])
        };
        let t = term(&mut g, sp, hs, mem, arr, ik, None);
        if t != ik {
            main_loads.push(t);
        }
        let m = add(&mut g, Op::Mul, ty, vec![a, k31]);
        a = add(&mut g, Op::Add, ty, vec![m, t]);
    }
    let step = add(&mut g, Op::Const(F * s), ty, vec![]);
    let hnext = add(&mut g, Op::Add, ty, vec![hi, step]);
    assert!(g.set_input(h, 1, hs));
    assert!(g.set_input(hi, 2, hnext));
    assert!(g.set_input(hacc, 2, a));
    // The join, then the post loop.
    let j = add(&mut g, Op::Merge, ctl, vec![hx, skip]);
    let ji = add(&mut g, Op::Phi, ty, vec![j, hi, i0]);
    let jacc = add(&mut g, Op::Phi, ty, vec![j, hacc, acc0]);
    let r = g.add(Op::Merge, ctl, vec![j, NO_NODE], Some(2));
    let ri = g.add(Op::Phi, ty, vec![r, ji, NO_NODE], Some(2));
    let racc = g.add(Op::Phi, ty, vec![r, jacc, NO_NODE], Some(2));
    let rc = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![ri, n], Some(5));
    let rt = g.add(Op::If, ctl, vec![r, rc], Some(5));
    let rs = g.add(Op::Proj(0), ctl, vec![rt], Some(5));
    let rx = g.add(Op::Proj(1), ctl, vec![rt], Some(5));
    let rm = g.add(Op::Mul, ty, vec![racc, k31], Some(9));
    let rterm = term(&mut g, sp, rs, mem, arr, ri, Some(8));
    let racc2 = g.add(Op::Add, ty, vec![rm, rterm], Some(10));
    let rnext = g.add(Op::Add, ty, vec![ri, ks], Some(12));
    assert!(g.set_input(r, 1, rs));
    assert!(g.set_input(ri, 2, rnext));
    assert!(g.set_input(racc, 2, racc2));
    g.exit = g.add(Op::Return, ty, vec![rx, racc], Some(20));
    g.partially_unrolled_headers.extend([r, h]);
    Fixture {
        g,
        main: h,
        main_acc: hacc,
        post: r,
        post_acc: racc,
        main_loads,
    }
}

/// What one run did.
#[derive(Debug, Default)]
struct Trace {
    ret: Option<i64>,
    /// `(original iterations so far, accumulator)` after every body trip.
    progress: Vec<(u64, i64)>,
    /// Back edges that polled (into a header not strip-mined).
    polls: usize,
    /// Most inner back edges taken between two polls.
    worst_unpolled: usize,
    /// j27 wave 5 (lane STRIPOSR2): `(original iterations so far, value)` of
    /// the post loop's accumulator at every poll whose header carries it --
    /// its own (the original), or the outer twin of it (the OSR form) -- the
    /// value a promoted cell's write-back pad stores there
    /// (`Lowerer::promoted_cell_poll_writebacks`' `current`).
    poll_cells: Vec<(u64, i64)>,
}

/// The values one run reads: the parameters and the array's elements.
#[derive(Clone, Copy)]
struct Input<'a> {
    params: &'a [i64],
    arr: &'a [i64],
}

/// Java `int` / `long` value of data node `id`.
fn eval(g: &Graph, env: &FxHashMap<NodeId, i64>, inp: Input<'_>, id: NodeId) -> i64 {
    let n = &g.nodes[id as usize];
    let x = |k: usize| eval(g, env, inp, n.inputs[k]);
    let raw = match n.op {
        Op::Const(c) => c,
        Op::Param(i) => inp.params[i as usize],
        Op::Phi => *env.get(&id).unwrap_or_else(|| panic!("phi n{id} read before its merge")),
        Op::Add => x(0).wrapping_add(x(1)),
        Op::Sub => x(0).wrapping_sub(x(1)),
        Op::Mul => x(0).wrapping_mul(x(1)),
        Op::Cmp(cc) => i64::from(super::super::eval_cmp_i64(cc, x(0), x(1))),
        Op::I2L | Op::L2I => x(0),
        Op::ScalarIntrinsic(ScalarOp::MinI | ScalarOp::MinL) => x(0).min(x(1)),
        Op::ScalarIntrinsic(ScalarOp::MaxL) => x(0).max(x(1)),
        // Cast: an array length is far below `int`.
        Op::ArrayLength => inp.arr.len() as i64,
        Op::ArrayLoad(_) => {
            let i = x(3);
            *usize::try_from(i)
                .ok()
                .and_then(|i| inp.arr.get(i))
                .unwrap_or_else(|| panic!("n{id} reads a[{i}] of {}", inp.arr.len()))
        }
        ref op => panic!("unexpected {op:?} at n{id}"),
    };
    // Cast: an `int` value is its low 32 bits, sign-extended.
    if n.ty == IrType::Int {
        i64::from(raw as i32)
    } else {
        raw
    }
}

/// The one control node `cur` hands control to.
fn next_control(g: &Graph, users: &[Vec<NodeId>], cur: NodeId) -> NodeId {
    let mut next: Vec<NodeId> = users[cur as usize]
        .iter()
        .copied()
        .filter(|&u| matches!(g.nodes[u as usize].op, Op::If | Op::Merge | Op::Region | Op::Return))
        .collect();
    next.dedup();
    assert_eq!(next.len(), 1, "n{cur} hands control to {next:?}");
    next[0]
}

/// The interpreter's state over one run.
struct Machine<'g> {
    fx: &'g Fixture,
    g: &'g Graph,
    users: Vec<Vec<NodeId>>,
    inp: Input<'g>,
    env: FxHashMap<NodeId, i64>,
    iters: u64,
    unpolled: usize,
    edges: usize,
    t: Trace,
}

impl Machine<'_> {
    /// Take the edge `pred -> m`: `m`'s phis read their `pred` slot, all at
    /// once; a back edge counts as a poll or an inner trip.
    fn enter_merge(&mut self, m: NodeId, pred: NodeId) {
        let g = self.g;
        let slot = g.nodes[m as usize].inputs.iter().position(|&p| p == pred).expect("an edge into the merge");
        let phis: Vec<NodeId> = self.users[m as usize]
            .iter()
            .copied()
            .filter(|&u| g.nodes[u as usize].op == Op::Phi && g.nodes[u as usize].inputs[0] == m)
            .collect();
        let vals: Vec<i64> = phis
            .iter()
            .map(|&p| eval(g, &self.env, self.inp, g.nodes[p as usize].inputs[slot + 1]))
            .collect();
        self.env.extend(phis.iter().copied().zip(vals));
        // Every header here takes its entry in slot 0; a nested inner
        // header's entry is reachable from it, so slot 0 is never a back edge.
        if slot >= 1 && super::super::forward_control_closure(g, &self.users, m).contains(&pred) {
            self.back_edge(m);
        }
    }

    fn back_edge(&mut self, m: NodeId) {
        self.edges += 1;
        if self.g.strip_mined_inner_headers.contains(&m) {
            self.unpolled += 1;
            self.t.worst_unpolled = self.t.worst_unpolled.max(self.unpolled);
        } else {
            self.t.polls += 1;
            self.unpolled = 0;
        }
        let polled = self.unpolled == 0;
        let (step, acc) = if m == self.fx.main {
            (F as u64, self.fx.main_acc)
        } else if m == self.fx.post {
            (1, self.fx.post_acc)
        } else {
            (0, NO_NODE)
        };
        if step > 0 {
            self.iters += step;
            self.t.progress.push((self.iters, self.env[&acc]));
        }
        if polled {
            self.note_poll_cell(m);
        }
    }

    /// At a poll into `m`: the post accumulator's value as the pad would
    /// resolve it there, when `m` carries it (directly or as its outer twin).
    fn note_poll_cell(&mut self, m: NodeId) {
        let cell = self.fx.post_acc;
        let own = self.g.nodes.get(cell as usize).is_some_and(|n| n.op == Op::Phi && n.inputs[0] == m);
        let at = if own { Some(cell) } else { strip_outer_twin(self.g, cell, m) };
        if let Some(v) = at {
            self.t.poll_cells.push((self.iters, self.env[&v]));
        }
    }
}

/// Run `g` (`fx`'s graph, strip-mined or not) from its entry on `params` for
/// at most `budget` back edges.
fn run(fx: &Fixture, g: &Graph, params: &[i64], budget: usize) -> Trace {
    run_on(fx, g, Input { params, arr: &[] }, budget)
}

/// [`run`] over an array.
fn run_on(fx: &Fixture, g: &Graph, inp: Input<'_>, budget: usize) -> Trace {
    let mut m = Machine {
        fx,
        g,
        users: build_users(g),
        inp,
        env: FxHashMap::default(),
        iters: 0,
        unpolled: 0,
        edges: 0,
        t: Trace::default(),
    };
    drive(&mut m, proj_user(g, g.entry, 0), budget);
    m.t
}

/// Run `m` on from control node `cur` for at most `budget` back edges.
fn drive(m: &mut Machine<'_>, mut cur: NodeId, budget: usize) {
    let (g, inp) = (m.g, m.inp);
    while m.edges < budget {
        let next = next_control(g, &m.users, cur);
        let nn = &g.nodes[next as usize];
        match nn.op {
            Op::If => {
                let taken = eval(g, &m.env, inp, nn.inputs[1]) != 0;
                cur = proj_user(g, next, if taken { 0 } else { 1 });
            }
            Op::Return => {
                m.t.ret = Some(eval(g, &m.env, inp, nn.inputs[1]));
                break;
            }
            _ => {
                m.enter_merge(next, cur);
                cur = next;
            }
        }
    }
}

fn mined(s: i64, iter: i64) -> (Fixture, Graph) {
    mined_spec(Spec::int(s), iter, &[])
}

/// `sp`'s fixture, and a strip-mined copy of its graph under the `switches`
/// (all set at once: a nested override replaces, it does not extend).
fn mined_spec(sp: Spec, iter: i64, switches: &[(&str, Option<&str>)]) -> (Fixture, Graph) {
    let fx = build(sp);
    let mut g = build(sp).g;
    let done = cratonvm_types::flags::with_thread_overrides(switches, || strip_mine_loops(&mut g, iter));
    assert_eq!(done, 1, "the main loop is strip-mined");
    (fx, g)
}

/// The two runs agree at every point both reached the same original
/// iteration count; returns how many points they shared.
fn agree(before: &Trace, after: &Trace) -> usize {
    let at: FxHashMap<u64, i64> = before.progress.iter().copied().collect();
    let mut shared = 0usize;
    for &(k, v) in &after.progress {
        if let Some(&w) = at.get(&k) {
            assert_eq!(v, w, "the accumulator after {k} iterations");
            shared += 1;
        }
    }
    shared
}

/// The strip-mined run matches the original: the same answer and iteration
/// count when the original ends within `budget`, the same accumulator at
/// every shared point when it does not; never more than `iter` inner trips
/// without a poll.
fn same_run(fx: &Fixture, g: &Graph, inp: Input<'_>, iter: i64, budget: usize, what: &str) {
    let before = run_on(fx, &fx.g, inp, budget);
    let after = run_on(fx, g, inp, budget);
    same_trace(&before, &after, iter, what);
}

/// [`same_run`]'s verdict on two traces.
fn same_trace(before: &Trace, after: &Trace, iter: i64, what: &str) {
    if before.ret.is_some() {
        assert_eq!(after.ret, before.ret, "{what}: the answer");
        assert_eq!(
            after.progress.last().map(|p| p.0),
            before.progress.last().map(|p| p.0),
            "{what}: the iteration count"
        );
    } else {
        assert!(after.ret.is_none(), "{what}: javac's loop does not end");
    }
    let shared = agree(before, after);
    assert!(before.progress.is_empty() || shared > 0, "{what}: no shared point");
    assert_eq!(before.worst_unpolled, 0);
    assert!(after.worst_unpolled <= iter as usize, "{what}: {after:?}");
}

const MAX: i64 = i32::MAX as i64;
const MIN: i64 = i32::MIN as i64;

/// Same answer, same iterations, and a poll at least every `iter` inner trips
/// -- for empty, short, strip-crossing and near-`Integer.MAX_VALUE` trips.
#[test]
fn j27_stripmine_main_loop_keeps_its_answer_and_polls_once_per_strip() {
    let iter = 7;
    for s in [1i64, 3] {
        let (fx, g) = mined(s, iter);
        let cases = [(0, 0), (0, 1), (0, 7), (5, 3), (0, 1000), (-5, 3000), (MAX - 999, MAX), (MIN, MIN + 50)];
        for (i0, n) in cases {
            let before = run(&fx, &fx.g, &[i0, n], 1 << 20);
            let after = run(&fx, &g, &[i0, n], 1 << 20);
            assert!(before.ret.is_some(), "s={s} ({i0},{n}) terminates");
            assert_eq!(after.ret, before.ret, "s={s} ({i0},{n}): the answer");
            assert_eq!(
                after.progress.last().map(|p| p.0),
                before.progress.last().map(|p| p.0),
                "s={s} ({i0},{n}): the iteration count"
            );
            agree(&before, &after);
            assert_eq!(before.worst_unpolled, 0);
            assert!(after.worst_unpolled <= iter as usize, "s={s} ({i0},{n}): {after:?}");
        }
        let before = run(&fx, &fx.g, &[0, 3000], 1 << 20);
        let after = run(&fx, &g, &[0, 3000], 1 << 20);
        assert_eq!(after.worst_unpolled, iter as usize, "full strips run");
        assert!(after.polls * 5 < before.polls, "{} polls against {}", after.polls, before.polls);
    }
}

/// j27 wave 4 (lane STRIPOSR): the `int` edges -- `Integer.MIN_VALUE` and
/// `MAX_VALUE` starts and bounds, strides that divide neither the range nor
/// the strip, a one-trip strip, zero-trip loops, and a first strip entered
/// at or past `lim2` (`q >= lim2`: `i0 = MAX - 7`, stride 2, where the main
/// trip would wrap).
#[test]
fn j27_striposr_int_edges_keep_the_answer() {
    for iter in [1i64, 7, 1000] {
        for s in [1i64, 2, 3, 5] {
            let (fx, g) = mined(s, iter);
            let cases = [
                (0, 0),
                (7, 7),
                (9, 3),
                (0, 1001),
                (1, 2999),
                (MIN, MIN),
                (MIN, MIN + 7 * s + 1),
                (MAX, MAX),
                (MAX - 1000, MAX),
                (MAX - 7, MAX),
                (MAX - 4 * s, MAX - 1),
                (-3001, 3001),
            ];
            for (i0, n) in cases {
                same_run(&fx, &g, Input { params: &[i0, n], arr: &[] }, iter, 20_000, &format!("iter={iter} s={s} ({i0},{n})"));
            }
        }
    }
}

const LMAX: i64 = i64::MAX;
const LMIN: i64 = i64::MIN;

fn long_spec(s: i64) -> Spec {
    Spec {
        s,
        ty: IrType::Long,
        array: None,
    }
}

const LONG_ON: (&str, Option<&str>) = ("CRATONVM_JIT_IR_STRIP_MINE_LONG", Some("1"));

/// j27 wave 4 (lane STRIPOSR, item 3): a `long` induction under the
/// saturating strip end -- `Long.MIN_VALUE` / `MAX_VALUE` edges, a range
/// across `2^31` (an `int`-narrowed strip end would be wrong there), a first
/// strip at or past `lim2`, and the wrapping trip the post loop runs.
#[test]
fn j27_striposr_long_induction_keeps_the_answer() {
    let i31 = i64::from(i32::MAX);
    for iter in [1i64, 7, 1000] {
        for s in [1i64, 2, 3] {
            let (fx, g) = mined_spec(long_spec(s), iter, &[LONG_ON]);
            let cases = [
                (0, 0),
                (0, 1),
                (5, 3000),
                (i31 - 100, i31 + 5000),
                (-(1 << 40), -(1 << 40) + 4001),
                (LMIN, LMIN),
                (LMIN, LMIN + 50),
                (LMAX, LMAX),
                (LMAX - 999, LMAX),
                (LMAX - 7, LMAX),
                (LMAX - 7 - 8 * 40, LMAX),
            ];
            for (i0, n) in cases {
                same_run(&fx, &g, Input { params: &[i0, n], arr: &[] }, iter, 20_000, &format!("long iter={iter} s={s} ({i0},{n})"));
            }
        }
    }
}

/// The `long` strip end is the saturating `long` minimum, never narrowed,
/// and the long switch is opt-in: off, the loop is left alone.
#[test]
fn j27_striposr_long_strip_end_is_long_and_opt_in() {
    let (fx, g) = mined_spec(long_spec(1), 1000, &[LONG_ON]);
    let test = g.nodes[fx.main as usize].inputs[1];
    let test = g.nodes[test as usize].inputs[0];
    let cond = &g.nodes[g.nodes[test as usize].inputs[1] as usize];
    let slim = &g.nodes[cond.inputs[1] as usize];
    assert_eq!(slim.op, Op::ScalarIntrinsic(ScalarOp::MinL));
    assert_eq!(slim.ty, IrType::Long);
    let mut off = build(long_spec(1));
    let done = cratonvm_types::flags::with_thread_overrides(&[(LONG_ON.0, None)], || strip_mine_loops(&mut off.g, 1000));
    assert_eq!(done, 0, "a long induction without the switch");
    assert!(off.g.strip_mined_inner_headers.is_empty());
}

const ARRAYS_ON: (&str, Option<&str>) = ("CRATONVM_JIT_IR_STRIP_MINE_ARRAYS", Some("1"));

fn array_spec(past: i64, zero_start: bool) -> Spec {
    Spec {
        s: 1,
        ty: IrType::Int,
        array: Some(ArrayBody { past, zero_start }),
    }
}

fn analysis(g: &Graph) -> CheckElision {
    crate::ir_check_elim::analyze(g, &crate::ir_schedule::schedule(g))
}

fn all_elided(e: &CheckElision, loads: &[NodeId]) -> bool {
    loads.iter().all(|&l| e.null_elided(l) && e.bounds_elided(l))
}

/// j27 wave 4 (lane STRIPOSR, item 2): `a = a * 31 + arr[i + k]` from `i =
/// 0` under the main test `i < arr.length - 3`. The check elimination proves
/// every main read before the strip mine; after it the inner test `i < slim`
/// hides the margin, so without the carried proof the checks would come back
/// (the premise). With it they stay elided, and the strip-mined loop reads the
/// same elements and answers the same, for empty, short, strip-crossing
/// arrays.
#[test]
fn j27_striposr_array_reads_in_the_margin_lose_their_checks_and_keep_the_answer() {
    let iter = 7;
    let fx = build(array_spec(0, true));
    assert_eq!(fx.main_loads.len(), F as usize);
    assert!(all_elided(&analysis(&fx.g), &fx.main_loads), "precondition: proven before the strip mine");
    let (fx, mut g) = mined_spec(array_spec(0, true), iter, &[ARRAYS_ON]);
    let mut carried = g.strip_mine_proven_accesses.clone();
    carried.sort_unstable();
    assert_eq!(carried, fx.main_loads, "every main read, no post-loop read");
    assert!(all_elided(&analysis(&g), &fx.main_loads), "the carried proof");
    for len in [0usize, 1, 3, 4, 5, 29, 1000, 3001] {
        // Cast: a small index.
        let arr: Vec<i64> = (0..len).map(|j| (j as i64) * 7 + 1).collect();
        same_run(&fx, &g, Input { params: &[0, 0, 0], arr: &arr }, iter, 1 << 20, &format!("len={len}"));
    }
    g.strip_mine_proven_accesses.clear();
    assert!(
        !all_elided(&analysis(&g), &fx.main_loads),
        "premise: the strip limit hides the margin from the range pass"
    );
}

/// The reads whose checks must stay keep the loop as it was: a read past the
/// margin (`arr[i + 4]` under `i < arr.length - 3`), a start that may be
/// negative, and any read with the arrays switch off.
#[test]
fn j27_striposr_array_reads_the_range_pass_keeps_leave_the_loop_alone() {
    for (sp, switches, what) in [
        (array_spec(1, true), &[ARRAYS_ON][..], "past the margin"),
        (array_spec(0, false), &[ARRAYS_ON][..], "a start that may be negative"),
        (array_spec(0, true), &[(ARRAYS_ON.0, None)][..], "the arrays switch off"),
    ] {
        let mut fx = build(sp);
        let nodes = fx.g.nodes.len();
        let done = cratonvm_types::flags::with_thread_overrides(switches, || strip_mine_loops(&mut fx.g, 1000));
        assert_eq!(done, 0, "{what}");
        assert_eq!(fx.g.nodes.len(), nodes, "{what}: the graph is untouched");
        assert!(fx.g.strip_mine_proven_accesses.is_empty(), "{what}");
    }
    let fx = build(array_spec(1, true));
    assert!(!all_elided(&analysis(&fx.g), &fx.main_loads), "the past-the-margin read keeps its check");
}

/// A per-copy frame named only by a proven read (what
/// `CRATONVM_JIT_IR_UNROLL_MAIN_POST_DEOPT`'s copies carry) does not refuse
/// the strip mine; a frame looked up by pc that names a loop value does.
#[test]
fn j27_striposr_only_copy_frames_of_inner_reads_may_name_loop_values() {
    let framed = |copy_frame: bool| {
        let mut fx = build(array_spec(0, true));
        let hi = fx.g.nodes[fx.main_loads[0] as usize].inputs[3];
        let mut sp = SafepointSnapshot::new(8, vec![hi], vec![], vec![]);
        sp.copy_frame = copy_frame;
        // Cast: a small snapshot index.
        let si = fx.g.safepoints.len() as u32;
        fx.g.push_safepoint(sp);
        fx.g.nodes[fx.main_loads[0] as usize].frame_snapshot = Some(si);
        cratonvm_types::flags::with_thread_overrides(&[ARRAYS_ON], || strip_mine_loops(&mut fx.g, 1000))
    };
    assert_eq!(framed(true), 1, "a copy frame of an inner read");
    assert_eq!(framed(false), 0, "a by-pc frame naming the induction");
}

/// The shape: `H` keeps its identity as the inner header, recorded; its test
/// reads the strip limit; its entry is a pc-less outer header whose back edge
/// comes from a test on `H`'s exit; the post loop is untouched.
#[test]
fn j27_stripmine_the_inner_header_is_recorded_and_the_outer_has_no_bci() {
    let (fx, g) = mined(1, 1000);
    assert_eq!(g.strip_mined_inner_headers, vec![fx.main]);
    assert!(g.strip_mine_proven_accesses.is_empty(), "a pure body proves nothing");
    let h = &g.nodes[fx.main as usize];
    let outer = h.inputs[0];
    let on = &g.nodes[outer as usize];
    assert_eq!(on.op, Op::Merge);
    assert_eq!(on.bytecode_pc, None, "no OSR entry, no mode exit");
    assert_eq!(on.inputs.len(), 2);
    let ob = &g.nodes[on.inputs[1] as usize];
    assert_eq!(ob.op, Op::Proj(0));
    let ot = &g.nodes[ob.inputs[0] as usize];
    assert_eq!(ot.op, Op::If);
    let inner_exit = &g.nodes[ot.inputs[0] as usize];
    assert_eq!(inner_exit.op, Op::Proj(1));
    let test = inner_exit.inputs[0];
    assert_eq!(g.nodes[test as usize].inputs[0], fx.main);
    let cond = &g.nodes[g.nodes[test as usize].inputs[1] as usize];
    assert_eq!(cond.op, Op::Cmp(CmpOp::Lt));
    assert_eq!(g.nodes[cond.inputs[1] as usize].op, Op::L2I, "the strip limit");
    assert_eq!(
        g.nodes[fx.post as usize].inputs.as_slice(),
        fx.g.nodes[fx.post as usize].inputs.as_slice()
    );
}

/// Run twice, the second finds nothing to do.
#[test]
fn j27_stripmine_is_idempotent() {
    let (_, mut g) = mined(1, 1000);
    let nodes = g.nodes.len();
    assert_eq!(strip_mine_loops(&mut g, 1000), 0);
    assert_eq!(g.nodes.len(), nodes);
}

/// Refused: a header with a bci (an OSR entry), a body that can deopt, a main
/// exit that no longer reaches a post loop. The graph is left as it was.
#[test]
fn j27_stripmine_refusals_leave_the_graph_alone() {
    let mut fx = fixture(1);
    fx.g.nodes[fx.main as usize].bytecode_pc = Some(4);
    assert_eq!(strip_mine_loops(&mut fx.g, 1000), 0, "a header with a bci");

    let mut fx = fixture(1);
    let acc_next = fx.g.nodes[fx.main_acc as usize].inputs[2];
    // Cast: arena index to node id.
    let n = fx.g.nodes.iter().position(|x| x.op == Op::Param(1)).expect("n") as NodeId;
    let div = fx.g.add(Op::Div, IrType::Int, vec![acc_next, n], None);
    assert!(fx.g.set_input(fx.main_acc, 2, div));
    let before = fx.g.nodes.len();
    assert_eq!(strip_mine_loops(&mut fx.g, 1000), 0, "a division by a variable");
    assert_eq!(fx.g.nodes.len(), before);

    let mut fx = fixture(1);
    fx.g.partially_unrolled_headers.retain(|&h| h != fx.post);
    assert_eq!(strip_mine_loops(&mut fx.g, 1000), 0, "no post loop");
    assert!(fx.g.strip_mined_inner_headers.is_empty());
}

/// The lowering's question, on the real schedule: a latch retreating to the
/// inner header polls no more; one retreating to the outer header or the
/// post loop still does.
#[test]
fn j27_stripmine_only_inner_latches_skip_the_poll() {
    let (fx, g) = mined(1, 1000);
    let schedule = crate::ir_schedule::schedule(&g);
    let outer = g.nodes[fx.main as usize].inputs[0];
    let (mut skipped, mut kept) = (0, 0);
    for (b, block) in schedule.blocks.iter().enumerate() {
        let Some(&s) = block.successors.iter().find(|&&s| s <= b) else {
            assert!(!latch_enters_only_strip_inner(&g, &schedule, b));
            continue;
        };
        let head = schedule.blocks[s].ctrl;
        let skip = latch_enters_only_strip_inner(&g, &schedule, b);
        if head == fx.main {
            assert!(skip, "block {b} retreats to the inner header");
            skipped += 1;
        } else {
            assert!([outer, fx.post].contains(&head), "block {b} retreats to n{head}");
            assert!(!skip, "block {b} retreats to n{head} and polls");
            kept += 1;
        }
    }
    assert_eq!((skipped, kept), (1, 2), "one inner latch, the outer and the post latch");
    // No inner header, no question.
    assert!(!latch_enters_only_strip_inner(&fx.g, &crate::ir_schedule::schedule(&fx.g), 0));
}

/// j27 wave 4 (SOUNDREV's first risk): in a block order no validator
/// checked (creation order: `CRATONVM_JIT_IR_RPO_LAYOUT=0`, or a refused
/// layout), the outer back edge may read as forward and poll nowhere; then
/// the inner latch keeps its poll too.
#[test]
fn j27_striposr_an_unvalidated_layout_keeps_every_poll() {
    let (_, g) = mined(1, 1000);
    let mut schedule = crate::ir_schedule::schedule(&g);
    assert!(schedule.layout.applied, "the default layout is validated");
    assert!((0..schedule.blocks.len()).any(|b| latch_enters_only_strip_inner(&g, &schedule, b)));
    schedule.layout.applied = false;
    assert!((0..schedule.blocks.len()).all(|b| !latch_enters_only_strip_inner(&g, &schedule, b)));
}

#[test]
fn j27_stripmine_switch_is_opt_in_and_the_strip_length_parses() {
    let on = |v: Option<&str>| {
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_JIT_IR_STRIP_MINE", v)], strip_mine_enabled)
    };
    assert!(!on(None));
    assert!(on(Some("1")));
    let sub = |key: &str, f: fn() -> bool| {
        (
            cratonvm_types::flags::with_thread_overrides(&[(key, None)], f),
            cratonvm_types::flags::with_thread_overrides(&[(key, Some("1"))], f),
        )
    };
    assert_eq!(sub("CRATONVM_JIT_IR_STRIP_MINE_ARRAYS", strip_mine_arrays_enabled), (false, true));
    assert_eq!(sub("CRATONVM_JIT_IR_STRIP_MINE_LONG", strip_mine_long_enabled), (false, true));
    assert_eq!(parse_iter(None), 1000);
    assert_eq!(parse_iter(Some(" 64 ")), 64);
    assert_eq!(parse_iter(Some("0")), 1);
    assert_eq!(parse_iter(Some("junk")), 1000);
    assert_eq!(parse_iter(Some("99999999")), MAX_ITER);
}

// ── j27 wave 5 (lane STRIPOSR2, item 1): a loop whose header carries a bci ──

const OSR_ON: (&str, Option<&str>) = ("CRATONVM_JIT_IR_STRIP_MINE_OSR", Some("1"));
/// The OSR fixture's header bci, and the bci of a body snapshot.
const OSR_BCI: usize = 2;
const BODY_BCI: usize = 9;

/// The OSR-door graph of
/// `int f(int i0, int n) { int a = 1; for (int i = i0; i < n; i += s) a = a * 31 + i; return a; }`:
/// one counted loop `R` at pc 2, never unrolled. Its header snapshot names the
/// locals `[i0, n, a, i]` = `[p0, n, a phi, i phi]`; a by-pc body snapshot at
/// pc 9 names them too (the builder records one per bytecode boundary), and
/// nothing consults it.
fn osr_fixture(s: i64) -> Fixture {
    osr_fixture_ty(s, IrType::Int)
}

/// [`osr_fixture`] at width `ty` (`Int`, or `Long`: j27 wave 8, lane W8OSR).
fn osr_fixture_ty(s: i64, ty: IrType) -> Fixture {
    let ctl = IrType::Control;
    let mut g = Graph::empty(0);
    g.osr_compile = true;
    let start = add(&mut g, Op::Start, ctl, vec![]);
    g.entry = start;
    let entry = add(&mut g, Op::Proj(0), ctl, vec![start]);
    let p0 = add(&mut g, Op::Param(0), ty, vec![start]);
    let n = add(&mut g, Op::Param(1), ty, vec![start]);
    let acc0 = add(&mut g, Op::Const(1), ty, vec![]);
    let k31 = add(&mut g, Op::Const(31), ty, vec![]);
    let ks = add(&mut g, Op::Const(s), ty, vec![]);
    let pc = Some(OSR_BCI);
    let r = g.add(Op::Merge, ctl, vec![entry, NO_NODE], pc);
    let ri = g.add(Op::Phi, ty, vec![r, p0, NO_NODE], pc);
    let racc = g.add(Op::Phi, ty, vec![r, acc0, NO_NODE], pc);
    let rc = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![ri, n], Some(5));
    let rt = g.add(Op::If, ctl, vec![r, rc], Some(5));
    let rs = g.add(Op::Proj(0), ctl, vec![rt], Some(5));
    let rx = g.add(Op::Proj(1), ctl, vec![rt], Some(5));
    let rm = g.add(Op::Mul, ty, vec![racc, k31], Some(BODY_BCI));
    let racc2 = g.add(Op::Add, ty, vec![rm, ri], Some(10));
    let rnext = g.add(Op::Add, ty, vec![ri, ks], Some(12));
    assert!(g.set_input(r, 1, rs));
    assert!(g.set_input(ri, 2, rnext));
    assert!(g.set_input(racc, 2, racc2));
    g.exit = g.add(Op::Return, ty, vec![rx, racc], Some(20));
    g.push_safepoint(SafepointSnapshot::new(OSR_BCI, vec![p0, n, racc, ri], vec![], vec![]));
    g.push_safepoint(SafepointSnapshot::new(BODY_BCI, vec![p0, n, racc, ri], vec![], vec![]));
    Fixture {
        g,
        main: NO_NODE,
        main_acc: NO_NODE,
        post: r,
        post_acc: racc,
        main_loads: Vec::new(),
    }
}

fn osr_mined(s: i64, iter: i64) -> (Fixture, Graph) {
    let fx = osr_fixture(s);
    let mut g = osr_fixture(s).g;
    let done = cratonvm_types::flags::with_thread_overrides(&[OSR_ON], || strip_mine_loops(&mut g, iter));
    assert_eq!(done, 1, "the loop at a bci is strip-mined");
    (fx, g)
}

/// The one live header carrying `bci`.
fn landing(g: &Graph, bci: usize) -> NodeId {
    let at: Vec<NodeId> = (0..g.nodes.len())
        // Cast: arena index to node id.
        .map(|i| i as NodeId)
        .filter(|&i| {
            let n = &g.nodes[i as usize];
            matches!(n.op, Op::Merge | Op::Region) && n.bytecode_pc == Some(bci)
        })
        .collect();
    assert_eq!(at.len(), 1, "one header at bci {bci}: {at:?}");
    at[0]
}

/// Enter `g` as its OSR stub does: at the header carrying [`OSR_BCI`], every
/// value the first snapshot at that bci names seeded from the interpreter's
/// `locals` (each header phi must be named); then run as [`run_on`] does.
fn run_osr(fx: &Fixture, g: &Graph, params: &[i64], locals: &[i64], budget: usize) -> Trace {
    let head = landing(g, OSR_BCI);
    let sp = g.safepoints.iter().find(|sp| sp.bci == OSR_BCI).expect("a header snapshot");
    let mut m = Machine {
        fx,
        g,
        users: build_users(g),
        inp: Input { params, arr: &[] },
        env: FxHashMap::default(),
        iters: 0,
        unpolled: 0,
        edges: 0,
        t: Trace::default(),
    };
    let is_head_phi = |v: NodeId| g.nodes[v as usize].op == Op::Phi && g.nodes[v as usize].inputs[0] == head;
    for (slot, &v) in sp.locals.iter().enumerate() {
        if is_head_phi(v) {
            m.env.insert(v, locals[slot]);
        }
    }
    let phis = m.users[head as usize].iter().filter(|&&u| is_head_phi(u)).count();
    assert_eq!(m.env.len(), phis, "the header snapshot names every phi of the landing header");
    drive(&mut m, head, budget);
    m.t
}

/// Entered by OSR at the header with a mid-loop induction value, the
/// strip-mined loop runs the same trips to the same answer as the original
/// entered the same way: mid-loop, zero trips left (`i == n`, `i > n`),
/// `lim = Integer.MAX_VALUE` near the top (strides that land on it, strides
/// that wrap past it into a javac loop that never ends), one trip, and starts
/// far below the bound; never more than `iter` inner trips without a poll.
#[test]
fn j27_striposr2_osr_entry_mid_loop_keeps_the_answer_and_the_trip_count() {
    for iter in [1i64, 7, 1000] {
        for s in [1i64, 2, 3, 5] {
            let (fx, g) = osr_mined(s, iter);
            // (i0, n, a at the entry, i at the entry)
            let cases = [
                (0, 3000, 777, 1234),
                (0, 10, 5, 10),
                (0, 10, 5, 50),
                (MIN, MAX, 3, MAX - 2000),
                (MIN, MAX, 9, MAX),
                (MIN, MAX, 9, MAX - 1),
                (0, 1, 1, 0),
                (-5, 5, 1, MIN),
                (MIN, MIN + 3, 4, MIN),
            ];
            for (i0, n, a, i) in cases {
                let what = format!("iter={iter} s={s} f({i0},{n}) entered at a={a} i={i}");
                let before = run_osr(&fx, &fx.g, &[i0, n], &[i0, n, a, i], 20_000);
                let after = run_osr(&fx, &g, &[i0, n], &[i0, n, a, i], 20_000);
                same_trace(&before, &after, iter, &what);
            }
        }
    }
}

/// Full strips: the OSR-entered loop polls once per `iter` trips.
#[test]
fn j27_striposr2_osr_entry_polls_once_per_strip() {
    let iter = 7;
    let (fx, g) = osr_mined(1, iter);
    let before = run_osr(&fx, &fx.g, &[0, 3000], &[0, 3000, 1, 100], 1 << 20);
    let after = run_osr(&fx, &g, &[0, 3000], &[0, 3000, 1, 100], 1 << 20);
    assert!(before.ret.is_some());
    assert_eq!(after.ret, before.ret);
    assert_eq!(after.worst_unpolled, iter as usize, "full strips run");
    assert!(after.polls * 5 < before.polls, "{} polls against {}", after.polls, before.polls);
}

/// The shape: the outer header carries the bci and the inner header none;
/// the header snapshot names the outer phis (the OSR stub's seeds), whose
/// back inputs are the inner phis (what the outer poll's mode exit reads on
/// its edge); the inner test is the trip distance; the outer test is the
/// original compare node; the body snapshot and every body node are as they
/// were.
#[test]
fn j27_striposr2_the_outer_header_takes_the_bci_and_the_header_snapshot() {
    let (fx, g) = osr_mined(1, 1000);
    let r = fx.post;
    assert_eq!(g.strip_mined_inner_headers, vec![r]);
    assert_eq!(g.nodes[r as usize].bytecode_pc, None, "no second OSR entry");
    let outer = g.nodes[r as usize].inputs[0];
    assert_eq!(landing(&g, OSR_BCI), outer);
    let old = &fx.g.safepoints[0];
    let header = &g.safepoints[0];
    assert_eq!(header.locals[..2], old.locals[..2], "the invariant locals");
    for slot in [2usize, 3] {
        let (q, inner) = (header.locals[slot], old.locals[slot]);
        let qn = &g.nodes[q as usize];
        assert_eq!(qn.op, Op::Phi);
        assert_eq!(qn.inputs[0], outer, "slot {slot} names an outer phi");
        assert_eq!(qn.inputs[2], inner, "slot {slot}: the outer back edge carries the inner phi");
        assert_eq!(g.nodes[inner as usize].inputs[1], q, "the inner phi enters from it");
    }
    assert_eq!(g.safepoints[1].locals, fx.g.safepoints[1].locals, "the body frame");
    let rt = g.nodes[g.nodes[r as usize].inputs[1] as usize].inputs[0];
    let original_cond = fx.g.nodes[rt as usize].inputs[1];
    let inner_cond = &g.nodes[g.nodes[rt as usize].inputs[1] as usize];
    assert_eq!(inner_cond.op, Op::Cmp(CmpOp::Lt));
    let trips = &g.nodes[inner_cond.inputs[0] as usize];
    assert_eq!(trips.op, Op::Sub);
    assert_eq!(trips.inputs.as_slice(), &[old.locals[3], header.locals[3]], "i - q");
    assert_eq!(g.nodes[inner_cond.inputs[1] as usize].op, Op::L2I, "the strip distance");
    let outer_back = &g.nodes[g.nodes[outer as usize].inputs[1] as usize];
    let outer_test = &g.nodes[outer_back.inputs[0] as usize];
    assert_eq!(outer_test.inputs[1], original_cond, "the outer test is the original compare");
    let rewritten = [r, old.locals[2], old.locals[3], rt, g.exit];
    for id in 0..fx.g.nodes.len() {
        // Cast: arena index to node id.
        if rewritten.contains(&(id as NodeId)) {
            continue;
        }
        let (a, b) = (&fx.g.nodes[id], &g.nodes[id]);
        assert_eq!(a.op, b.op, "n{id}");
        assert_eq!(a.inputs.as_slice(), b.inputs.as_slice(), "n{id}");
        assert_eq!(a.bytecode_pc, b.bytecode_pc, "n{id}");
    }
}

/// Refused, the graph left as it was: the switch off, a compile that is no
/// OSR compile, promoted field cells at the header, a second snapshot at the
/// header bci, a node naming the header snapshot, a header snapshot naming a
/// body value, a body frame an entry may consult, a body that can deopt.
#[test]
fn j27_striposr2_refusals_leave_the_graph_alone() {
    fn refused(mut fx: Fixture, switches: &[(&str, Option<&str>)], what: &str) {
        let nodes = fx.g.nodes.len();
        let done = cratonvm_types::flags::with_thread_overrides(switches, || strip_mine_loops(&mut fx.g, 1000));
        assert_eq!(done, 0, "{what}");
        assert_eq!(fx.g.nodes.len(), nodes, "{what}: the graph is untouched");
        assert_eq!(fx.g.nodes[fx.post as usize].bytecode_pc, Some(OSR_BCI), "{what}");
        assert!(fx.g.strip_mined_inner_headers.is_empty(), "{what}");
    }
    refused(osr_fixture(1), &[(OSR_ON.0, None)], "the switch off");
    let mut fx = osr_fixture(1);
    fx.g.osr_compile = false;
    refused(fx, &[OSR_ON], "not an OSR compile");
    let (mut fx, store) = promoted_osr_fixture(1);
    let silent = fx.g.add(Op::Store(MemKind::Int), IrType::Memory, vec![], Some(BODY_BCI));
    fx.g.promoted_sunk_stores.push((store, silent));
    refused(fx, &[OSR_ON], "silent promoted stores (a deopt point in the body)");
    let mut fx = osr_fixture(1);
    fx.g.push_safepoint(SafepointSnapshot::new(OSR_BCI, vec![], vec![], vec![]));
    refused(fx, &[OSR_ON], "two snapshots at the header bci");
    let mut fx = osr_fixture(1);
    let ret = fx.g.exit;
    fx.g.nodes[ret as usize].frame_snapshot = Some(0);
    refused(fx, &[OSR_ON], "a node names the header snapshot");
    let mut fx = osr_fixture(1);
    let racc2 = fx.g.nodes[fx.post_acc as usize].inputs[2];
    fx.g.safepoints[0].locals[2] = racc2;
    refused(fx, &[OSR_ON], "the header snapshot names a body value");
    // A join at the body bci: an entry may be planned there.
    let mut fx = osr_fixture(1);
    let rx = fx.g.nodes[fx.g.exit as usize].inputs[0];
    fx.g.add(Op::Region, IrType::Control, vec![rx], Some(BODY_BCI));
    refused(fx, &[OSR_ON], "a consulted body frame");
    let mut fx = osr_fixture(1);
    let acc_next = fx.g.nodes[fx.post_acc as usize].inputs[2];
    // Cast: arena index to node id.
    let n = fx.g.nodes.iter().position(|x| x.op == Op::Param(1)).expect("n") as NodeId;
    let div = fx.g.add(Op::Div, IrType::Int, vec![acc_next, n], None);
    assert!(fx.g.set_input(fx.post_acc, 2, div));
    refused(fx, &[OSR_ON], "a division by a variable");
    // j27 wave 5 (lane REVIEW8): past the loop, a node that may deopt at the
    // header bci with no frame of its own resumes through the header
    // snapshot by pc, which the rewrite renames to the outer phis (the last
    // strip's start there, not the exit state).
    let mut fx = osr_fixture(1);
    let start = fx.g.entry;
    let mem = add(&mut fx.g, Op::Proj(1), IrType::Memory, vec![start]);
    let base = add(&mut fx.g, Op::Param(2), IrType::Ref, vec![start]);
    let rx = fx.g.nodes[fx.g.exit as usize].inputs[0];
    fx.g.add(Op::ArrayLength, IrType::Int, vec![rx, mem, base], Some(OSR_BCI));
    refused(fx, &[OSR_ON], "an after-loop node resumes by pc at the header bci");
}

/// [`osr_fixture`] with the accumulator `a` a promoted field cell, as
/// `loop_field_forward` leaves `counterInt`'s `COUNTER`: the cell lives in
/// `R`'s phi, the in-loop store is gone (a pure body), the exit edge stores
/// the phi back (a store at the header bci naming the header snapshot,
/// recorded under `R` in `Graph::promoted_cell_writebacks`), and a pre-header
/// read keeps its own claimed copy of the header frame
/// (`loop_field_forward::preheader_snapshot`). Returns the exit store.
fn promoted_osr_fixture(s: i64) -> (Fixture, NodeId) {
    let mut fx = osr_fixture(s);
    let g = &mut fx.g;
    let start = g.entry;
    let entry = proj_user(g, start, 0);
    let mem = add(g, Op::Proj(1), IrType::Memory, vec![start]);
    let base = add(g, Op::Param(2), IrType::Ref, vec![start]);
    let idx = add(g, Op::Const(0), IrType::Int, vec![]);
    let pre_read = g.add(Op::ArrayLength, IrType::Int, vec![entry, mem, base], Some(OSR_BCI));
    let header = g.safepoints[0].clone();
    let entry_values: Vec<NodeId> = header
        .locals
        .iter()
        .map(|&v| if g.nodes[v as usize].op == Op::Phi { g.nodes[v as usize].inputs[1] } else { v })
        .collect();
    g.push_safepoint(SafepointSnapshot::new(OSR_BCI, entry_values, vec![], vec![]));
    // Cast: a small snapshot index.
    assert!(g.set_node_frame_snapshot(pre_read, (g.safepoints.len() - 1) as u32));
    let rx = g.nodes[g.exit as usize].inputs[0];
    let store = g.add(Op::Store(MemKind::Int), IrType::Memory, vec![rx, mem, base, idx, fx.post_acc], Some(OSR_BCI));
    assert!(g.set_node_frame_snapshot(store, 0));
    g.promoted_cell_writebacks.push((fx.post, store));
    g.promoted_cell_header_bcis.push(OSR_BCI);
    (fx, store)
}

/// The promoted cell's loop is strip-mined: its write-back records follow
/// the outer header (whose poll is the loop's only one), the exit store names
/// an unrenamed copy of the header frame (it runs where `R`'s phis hold the
/// exit state) and still stores `R`'s phi, the header snapshot stays first at
/// its bci and unclaimed, and the pre-header copy is left as it was.
#[test]
fn j27_striposr2_promoted_cell_records_follow_the_outer_header() {
    let (fx, store) = promoted_osr_fixture(1);
    let mut g = promoted_osr_fixture(1).0.g;
    let done = cratonvm_types::flags::with_thread_overrides(&[OSR_ON], || strip_mine_loops(&mut g, 1000));
    assert_eq!(done, 1, "a promoted cell no longer refuses");
    let outer = g.nodes[fx.post as usize].inputs[0];
    assert_eq!(g.promoted_cell_writebacks, vec![(outer, store)]);
    assert_eq!(g.promoted_cell_header_bcis, vec![OSR_BCI]);
    let ci = g.nodes[store as usize].frame_snapshot.expect("the exit store keeps a frame") as usize;
    assert_ne!(ci, 0);
    assert_eq!(g.safepoints[ci].bci, OSR_BCI);
    assert_eq!(g.safepoints[ci].locals, fx.g.safepoints[0].locals, "the exit frame names R's phis");
    assert_eq!(g.nodes[store as usize].inputs[4], fx.post_acc, "the exit stores R's phi");
    assert!(g.nodes.iter().all(|n| n.op == Op::Dead || n.frame_snapshot != Some(0)), "the header snapshot is unclaimed");
    assert_eq!(g.safepoints.iter().position(|sp| sp.bci == OSR_BCI), Some(0), "still first at its bci");
    let q = g.safepoints[0].locals[2];
    assert_eq!(strip_outer_twin(&g, fx.post_acc, outer), Some(q), "the pad's cell at the outer poll");
    assert_eq!(strip_outer_twin(&g, fx.post_acc, fx.post), None);
    assert_eq!(g.safepoints[2].locals, fx.g.safepoints[2].locals, "the pre-header copy");
}

/// Entered by OSR, the promoted-cell loop answers the same (the exit
/// write-back's value), and at every outer poll the pad would store the cell
/// value the original's poll stored after the same number of iterations.
#[test]
fn j27_striposr2_promoted_cell_state_at_the_poll_and_the_exit() {
    for iter in [1i64, 7, 1000] {
        for s in [1i64, 3] {
            let (fx, _) = promoted_osr_fixture(s);
            let mut g = promoted_osr_fixture(s).0.g;
            let done = cratonvm_types::flags::with_thread_overrides(&[OSR_ON], || strip_mine_loops(&mut g, iter));
            assert_eq!(done, 1);
            for (i0, n, a, i) in [(0, 3000, 777, 1234), (0, 10, 5, 10), (MIN, MAX, 3, MAX - 2000), (-5, 5, 1, MIN)] {
                let what = format!("iter={iter} s={s} f({i0},{n}) entered at a={a} i={i}");
                let before = run_osr(&fx, &fx.g, &[i0, n], &[i0, n, a, i], 20_000);
                let after = run_osr(&fx, &g, &[i0, n], &[i0, n, a, i], 20_000);
                same_trace(&before, &after, iter, &what);
                let at: FxHashMap<u64, i64> = before.poll_cells.iter().copied().collect();
                for &(k, v) in &after.poll_cells {
                    assert_eq!(at.get(&k), Some(&v), "{what}: the cell the pad stores after {k} iterations");
                }
                let long = before.progress.len() > 2 * iter as usize;
                assert!(!long || !after.poll_cells.is_empty(), "{what}: the outer poll carries the cell");
            }
        }
    }
}

/// Run twice, the second finds nothing to do; the switch is opt-in.
#[test]
fn j27_striposr2_is_idempotent_and_opt_in() {
    let (_, mut g) = osr_mined(1, 1000);
    let nodes = g.nodes.len();
    let again = cratonvm_types::flags::with_thread_overrides(&[OSR_ON], || strip_mine_loops(&mut g, 1000));
    assert_eq!(again, 0);
    assert_eq!(g.nodes.len(), nodes);
    let on = |v: Option<&str>| cratonvm_types::flags::with_thread_overrides(&[(OSR_ON.0, v)], strip_mine_osr_enabled);
    assert!(!on(None));
    assert!(on(Some("1")));
}

/// The lowering's question on the real schedule: the inner latch polls no
/// more, the outer latch (the OSR landing's back edge) still does.
#[test]
fn j27_striposr2_only_the_inner_latch_skips_the_poll() {
    let (fx, g) = osr_mined(1, 1000);
    let schedule = crate::ir_schedule::schedule(&g);
    let outer = g.nodes[fx.post as usize].inputs[0];
    let (mut skipped, mut kept) = (0, 0);
    for (b, block) in schedule.blocks.iter().enumerate() {
        let Some(&s) = block.successors.iter().find(|&&s| s <= b) else {
            continue;
        };
        let head = schedule.blocks[s].ctrl;
        let skip = latch_enters_only_strip_inner(&g, &schedule, b);
        if head == fx.post {
            assert!(skip, "block {b} retreats to the inner header");
            skipped += 1;
        } else {
            assert_eq!(head, outer, "block {b} retreats to n{head}");
            assert!(!skip, "block {b} retreats to the outer header and polls");
            kept += 1;
        }
    }
    assert_eq!((skipped, kept), (1, 1));
}

/// An OSR compile of a main/post loop: the main loop is strip-mined first,
/// then its post loop (the header with the bci). Same answers; at most two
/// strips back to back without a poll (the main loop's last, the post loop's
/// first: the main exit does not poll).
#[test]
fn j27_striposr2_an_osr_main_post_graph_mines_the_main_and_the_post_loop() {
    let iter = 7;
    for s in [1i64, 3] {
        let fx = build(Spec::int(s));
        let mut g = build(Spec::int(s)).g;
        g.osr_compile = true;
        let done = cratonvm_types::flags::with_thread_overrides(&[OSR_ON], || strip_mine_loops(&mut g, iter));
        assert_eq!(done, 2, "s={s}");
        let mut inner = g.strip_mined_inner_headers.clone();
        inner.sort_unstable();
        let mut want = vec![fx.main, fx.post];
        want.sort_unstable();
        assert_eq!(inner, want);
        for (i0, n) in [(0, 0), (0, 1), (5, 3), (0, 1000), (-5, 3000), (MAX - 999, MAX), (MIN, MIN + 50), (MAX - 7, MAX)] {
            let inp = Input { params: &[i0, n], arr: &[] };
            let before = run_on(&fx, &fx.g, inp, 20_000);
            let after = run_on(&fx, &g, inp, 20_000);
            same_trace(&before, &after, 2 * iter, &format!("s={s} ({i0},{n})"));
        }
    }
}

/// j27 wave 8 (lane W8OSR): [`osr_fixture_ty`] at `long` width, strip-mined
/// under the OSR and long sub-switches (set together: a nested override
/// replaces).
fn long_osr_mined(s: i64, iter: i64) -> (Fixture, Graph) {
    let fx = osr_fixture_ty(s, IrType::Long);
    let mut g = osr_fixture_ty(s, IrType::Long).g;
    let done = cratonvm_types::flags::with_thread_overrides(&[OSR_ON, LONG_ON], || strip_mine_loops(&mut g, iter));
    assert_eq!(done, 1, "the long loop at a bci is strip-mined");
    (fx, g)
}

/// j27 wave 8 (lane W8OSR): an OSR-entered `long` loop keeps the answer and
/// the trip count under the saturating trip distance -- `lim - q` that would
/// wrap a `long` (`Long.MIN_VALUE` entry under `Long.MAX_VALUE`), entries near
/// the top (a stride landing on the bound, one wrapping past it into a loop
/// that never ends), zero trips left, a range across `2^31`, one trip; never
/// more than `iter` inner trips without a poll. Old code refused the loop
/// (`a long induction at a header with a bci`), so `long_osr_mined` failed.
#[test]
fn j27_w8osr_long_osr_entry_keeps_the_answer_and_the_trip_count() {
    let i31 = i64::from(i32::MAX);
    for iter in [1i64, 7, 1000] {
        for s in [1i64, 2, 3, 5] {
            let (fx, g) = long_osr_mined(s, iter);
            // (i0, n, a at the entry, i at the entry)
            let cases = [
                (0, 3000, 777, 1234),
                (0, 10, 5, 10),
                (0, 10, 5, 50),
                (LMIN, LMAX, 3, LMAX - 2000),
                (LMIN, LMAX, 9, LMAX),
                (LMIN, LMAX, 9, LMAX - 1),
                (LMIN, LMAX, 2, LMIN),
                (0, 1, 1, 0),
                (-5, 5, 1, LMIN + 3),
                (LMIN, LMIN + 3, 4, LMIN),
                (0, i31 + 5000, 1, i31 - 100),
                (0, LMIN, 1, LMAX),
            ];
            for (i0, n, a, i) in cases {
                let what = format!("long iter={iter} s={s} f({i0},{n}) entered at a={a} i={i}");
                let before = run_osr(&fx, &fx.g, &[i0, n], &[i0, n, a, i], 20_000);
                let after = run_osr(&fx, &g, &[i0, n], &[i0, n, a, i], 20_000);
                same_trace(&before, &after, iter, &what);
            }
        }
    }
}

/// j27 wave 8 (lane W8OSR): the `long` trip distance stays `long` (no `L2I`
/// narrowing, which would be wrong for a distance near `2^31`), polls once
/// per full strip, and the long switch stays required: off, the OSR form
/// leaves a `long` loop alone.
#[test]
fn j27_w8osr_long_osr_distance_is_long_full_strips_poll_and_opt_in() {
    let iter = 7;
    let (fx, g) = long_osr_mined(1, iter);
    let r = fx.post;
    let rt = g.nodes[g.nodes[r as usize].inputs[1] as usize].inputs[0];
    let inner_cond = &g.nodes[g.nodes[rt as usize].inputs[1] as usize];
    assert_eq!(inner_cond.op, Op::Cmp(CmpOp::Lt));
    let trips = &g.nodes[inner_cond.inputs[0] as usize];
    assert_eq!((trips.op.clone(), trips.ty), (Op::Sub, IrType::Long), "i - q at long width");
    let d = &g.nodes[inner_cond.inputs[1] as usize];
    assert_eq!((d.op.clone(), d.ty), (Op::Sub, IrType::Long), "d = t - q, never narrowed");
    let before = run_osr(&fx, &fx.g, &[0, 3000], &[0, 3000, 1, 100], 1 << 20);
    let after = run_osr(&fx, &g, &[0, 3000], &[0, 3000, 1, 100], 1 << 20);
    assert!(before.ret.is_some());
    assert_eq!(after.ret, before.ret);
    assert_eq!(after.worst_unpolled, iter as usize, "full strips run");
    let mut off = osr_fixture_ty(1, IrType::Long);
    let nodes = off.g.nodes.len();
    let done = cratonvm_types::flags::with_thread_overrides(&[OSR_ON, (LONG_ON.0, None)], || strip_mine_loops(&mut off.g, 1000));
    assert_eq!(done, 0, "a long OSR loop without the long switch");
    assert_eq!(off.g.nodes.len(), nodes, "the graph is untouched");
}
