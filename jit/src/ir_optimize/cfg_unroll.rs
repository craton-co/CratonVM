//! JIT round 24 item C (`jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`;
//! `CRATONVM_JIT_IR_UNROLL_CFG_BODY`, default ON since j27 2026-10-28): fully unroll a constant-trip
//! counted loop whose body is NOT one block.
//!
//! # Why
//!
//! The full unroller (`unroll_in`) takes only the single-block shape
//! `region -> If -> {back, exit}`: its recogniser answers "not counted" as
//! soon as the back edge is not a projection of the header's test
//! (`analyze_counted_loop`), which is every loop whose body branches. An 8-trip
//! `for (k = 0; k < 8; k++) x = (k & 1) == 0 ? a : b;` kept its loop, its
//! branch and its phi per trip, where HotSpot unrolls it fully and every copy's
//! `(k & 1) == 0` folds to a constant (the zki `volatile-store` row's inner
//! loop; `CRATONVM_DBG_UNROLL` now names that refusal).
//!
//! # What
//!
//! A loop `H: Merge(entry, back)` whose only control user is its exit test
//! `T: If(Cmp(cc, iv, bound))` with a stay projection `S` on the path to the
//! back edge and an exit projection `X`; `iv` a header φ from a constant by a
//! constant stride, `bound` a constant; `N` trips (simulated at the
//! induction variable's own width, at most [`MAX_TRIP`]); one exit (`X`); no
//! inner loop. Every body node is a control node (`If`, `Proj`, `Merge`), a φ,
//! or a value node that cannot transfer to the interpreter
//! (`ir_lower::can_deopt` is false: pure arithmetic, a direct non-volatile
//! `getstatic`, a store into a fresh object) or a direct primitive
//! `putstatic`. The rewrite clones the body `N` times in program order, each
//! copy's `S` (and anything anchored at `H`) re-pointed at the previous copy's
//! latch (the loop entry for copy 0) and each header φ replaced by its value
//! after the previous copy; then `X`'s users take the last latch and the φs'
//! users their exit values, and the loop is deleted. The caller's cleanup folds
//! each copy's branch on its now-constant induction value.
//!
//! # Why the frames stay right
//!
//! Nothing cloned can transfer, so a copy never reads a frame: copies carry no
//! bytecode pc (the pure-body unroll's rule). Every flat snapshot that named a
//! cloned value -- and the header's, which now names the exit values -- is
//! unreachable afterwards and joins `UNROLL_STRANDED_BCIS`, the lowering's net
//! (a by-bci transfer at one refuses the compile). A chain snapshot naming a
//! cloned value, or a frame outside the loop's pc range naming one, refuses.
//! Not an OSR graph's own loop (its header is an entry), as for every
//! loop-deleting pass.

use super::{
    build_users, collapse_trivial_merges, const_i64, eval_cmp_i64, header_is_spliced,
    lcmp_operands_of_zero_test, loop_body_with, loop_headers, FxHashMap, FxHashSet,
    UNROLL_STRANDED_BCIS,
};
use crate::ir::{Graph, IrType, Node, NodeId, Op, NO_NODE};

/// Largest constant trip count this pass retires a loop for.
const MAX_TRIP: i64 = 16;

/// Largest `trip x cloned nodes` product: the code size the copies may cost.
const MAX_CLONED_NODES: usize = 320;

/// `CRATONVM_JIT_IR_UNROLL_CFG_BODY` (JIT round 24 item C):
/// **default ON since j27 2026-10-28** (perf, `=0` kills). Read once per optimize (no `static`).
pub(super) fn cfg_unroll_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_UNROLL_CFG_BODY")
}

/// Retire every loop of `graph` this module's section describes. Returns how
/// many were unrolled.
pub(super) fn unroll_cfg_loops(graph: &mut Graph) -> usize {
    let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_UNROLL");
    collapse_trivial_merges(graph);
    let mut done = 0usize;
    let mut tried: FxHashSet<NodeId> = FxHashSet::default();
    // An unroll rewires its loop's exit to its entry, which changes every
    // enclosing loop's body: the loop list is taken again after each one.
    loop {
        let Some((region, entry_pred, back_ctrls)) = loop_headers(graph)
            .into_iter()
            .find(|l| !tried.contains(&l.0))
        else {
            break;
        };
        tried.insert(region);
        match unroll_one(graph, region, entry_pred, &back_ctrls) {
            Ok(trips) => {
                done += 1;
                if dbg || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[DBG_UNROLL] region {region}: control-flow body fully unrolled ({trips} trip(s))"
                    );
                }
            }
            Err(why) => {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: control-flow unroll refused -- {why}");
                }
            }
        }
    }
    done
}

/// May `n`, a non-control body node, be cloned into a pc-less copy?
fn clonable(graph: &Graph, n: &Node) -> bool {
    if n.op == Op::Phi {
        return true;
    }
    if !crate::ir_lower::can_deopt(graph, n) {
        return true;
    }
    // A direct primitive `putstatic` of an initialised class: one store into
    // its statics cell, no call, no map, no sentinel (`emit_inline_putstatic`).
    matches!(n.op, Op::StoreStatic { .. })
        && graph
            .direct_static_access(n)
            .is_some_and(|(_, is_write)| is_write)
}

/// The induction variable, stride and trip count of the loop at `region`
/// whose header test is `t`, or why not. `entry_slot` / `back_slot` are the
/// header φs' input slots; `stay_is_true` says which projection of `t` stays.
fn trip_count(
    graph: &Graph,
    carried: &[NodeId],
    t: NodeId,
    entry_slot: usize,
    back_slot: usize,
    stay_is_true: bool,
) -> Result<i64, &'static str> {
    let cond = graph
        .node_opt(t)
        .and_then(|n| n.input_opt(1))
        .ok_or("no test condition")?;
    let cn = graph.node_opt(cond).ok_or("no test condition")?;
    let Op::Cmp(cc) = cn.op else {
        return Err("the test is not a compare");
    };
    let [a, b] = *cn.inputs.as_slice() else {
        return Err("the test is not a compare");
    };
    let [a, b] = lcmp_operands_of_zero_test(graph, a, b).unwrap_or([a, b]);
    for &p in carried {
        let iv_left = if a == p {
            true
        } else if b == p {
            false
        } else {
            continue;
        };
        let pn = graph.node_opt(p).ok_or("a header phi is gone")?;
        let init = const_i64(graph, pn.input_opt(entry_slot).unwrap_or(NO_NODE))
            .ok_or("the induction variable's entry value is not a constant")?;
        let next = pn.input_opt(back_slot).unwrap_or(NO_NODE);
        let nn = graph.node_opt(next).ok_or("no induction step")?;
        let stride = match (&nn.op, nn.inputs.as_slice()) {
            (Op::Add, [x, y]) if *x == p => const_i64(graph, *y),
            (Op::Add, [x, y]) if *y == p => const_i64(graph, *x),
            (Op::Sub, [x, y]) if *x == p => const_i64(graph, *y).and_then(i64::checked_neg),
            _ => None,
        }
        .filter(|&s| s != 0)
        .ok_or("the induction variable does not step by a constant")?;
        let bound =
            const_i64(graph, if iv_left { b } else { a }).ok_or("the bound is not a constant")?;
        let int = pn.ty == IrType::Int;
        if int
            && [init, stride, bound]
                .iter()
                .any(|&v| i64::from(v as i32) != v)
        {
            return Err("an int induction value out of range");
        }
        let mut i = init;
        let mut trips = 0i64;
        loop {
            let raw = if iv_left {
                eval_cmp_i64(cc, i, bound)
            } else {
                eval_cmp_i64(cc, bound, i)
            };
            if raw != stay_is_true {
                return Ok(trips);
            }
            trips += 1;
            if trips > MAX_TRIP {
                return Err("more trips than the cap");
            }
            i = if int {
                // Cast: both checked to be exact i32 values above.
                i64::from(
                    (i as i32)
                        .checked_add(stride as i32)
                        .ok_or("the int induction wraps")?,
                )
            } else {
                i.checked_add(stride).ok_or("the induction wraps")?
            };
        }
    }
    Err("the test names no constant-stride header phi")
}

/// Unroll the loop at `region` fully, or say why not (the graph untouched).
fn unroll_one(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> Result<i64, &'static str> {
    let [back] = *back_ctrls else {
        return Err("not exactly one back edge");
    };
    if (graph.optimize_osr_route || graph.osr_compile) && !header_is_spliced(graph, region) {
        return Err("an OSR graph's own loop");
    }
    let header = graph.node_opt(region).ok_or("no header")?;
    let header_bci = header.bytecode_pc.ok_or("a header with no bytecode pc")?;
    let rin = header.inputs.clone();
    if rin.len() != 2 {
        return Err("not a two-input header");
    }
    let entry_slot = 1 + rin
        .iter()
        .position(|&c| c == entry_pred)
        .ok_or("no entry slot")?;
    let back_slot = 1 + rin.iter().position(|&c| c == back).ok_or("no back slot")?;
    let users = build_users(graph);
    let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);

    // The header's one control user, its exit test, and the test's two
    // projections: the stay edge (in the body) and the exit (outside).
    let ctrl_users: Vec<NodeId> = users[region as usize]
        .iter()
        .copied()
        .filter(|&u| graph.node_opt(u).is_some_and(|n| n.op.is_control()))
        .collect();
    let [t] = *ctrl_users.as_slice() else {
        return Err("the header has another control user than its test");
    };
    if graph.node_opt(t).map(|n| &n.op) != Some(&Op::If) {
        return Err("the header's control user is not an If");
    }
    let projs: Vec<NodeId> = users[t as usize].clone();
    let [p0, p1] = *projs.as_slice() else {
        return Err("the test has users besides its two projections");
    };
    let (stay, exit) = match (body.contains(&p0), body.contains(&p1)) {
        (true, false) => (p0, p1),
        (false, true) => (p1, p0),
        _ => return Err("no single stay and exit projection"),
    };
    let stay_is_true = match graph.node_opt(stay).map(|n| &n.op) {
        Some(Op::Proj(0)) => true,
        Some(Op::Proj(1)) => false,
        _ => return Err("the stay edge is not a projection"),
    };
    if back == stay {
        return Err("a single-block body (the full unroller's shape)");
    }

    // The header φs.
    let carried: Vec<NodeId> = users[region as usize]
        .iter()
        .copied()
        .filter(|&u| {
            graph
                .node_opt(u)
                .is_some_and(|n| n.op == Op::Phi && n.input_opt(0) == Some(region))
        })
        .collect();
    if carried
        .iter()
        .any(|&p| graph.node_opt(p).is_none_or(|n| n.inputs.len() != 3))
    {
        return Err("a header phi without two values");
    }
    let carried_set: FxHashSet<NodeId> = carried.iter().copied().collect();
    let trips = trip_count(graph, &carried, t, entry_slot, back_slot, stay_is_true)?;

    // The clone set: every body node but the header, its test and stay edge
    // and the header φs; the φs of the body's own merges; and every pure node
    // computed from those (or from a header φ), which floats outside `body`.
    let mut clone: FxHashSet<NodeId> = body
        .iter()
        .copied()
        .filter(|&n| n != region && n != t && n != stay && !carried_set.contains(&n))
        .collect();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        let id = id as NodeId;
        if n.op == Op::Phi
            && !carried_set.contains(&id)
            && n.input_opt(0)
                .is_some_and(|c| c != region && body.contains(&c))
        {
            clone.insert(id);
        }
    }
    let mut work: Vec<NodeId> = clone
        .iter()
        .copied()
        .chain(carried.iter().copied())
        .collect();
    while let Some(v) = work.pop() {
        for &u in &users[v as usize] {
            if clone.contains(&u) || carried_set.contains(&u) || u == t || u == region {
                continue;
            }
            let Some(un) = graph.node_opt(u) else {
                continue;
            };
            if un.op == Op::Dead {
                continue;
            }
            if un.op.is_pure() && !un.op.is_control() && un.op != Op::Phi {
                clone.insert(u);
                work.push(u);
            }
        }
    }
    // Nothing a copy computes may be read past the loop but through a header
    // φ, and only the exit leaves it.
    for &c in &clone {
        let cn = graph.node_opt(c).ok_or("a body node is gone")?;
        if cn.op.is_control() {
            if !matches!(cn.op, Op::If | Op::Proj(_) | Op::Merge | Op::Region) {
                return Err("a body control node that is no branch or join");
            }
        } else if !clonable(graph, cn) {
            return Err("a body node that can transfer to the interpreter");
        }
        for &u in &users[c as usize] {
            let in_loop = clone.contains(&u) || carried_set.contains(&u) || u == region || u == t;
            if !in_loop && graph.node_opt(u).is_some_and(|n| n.op != Op::Dead) {
                return Err("a per-trip value or control edge leaves the loop");
            }
        }
    }
    if clone.len().saturating_mul(trips as usize) > MAX_CLONED_NODES {
        return Err("the copies would exceed the size budget");
    }

    // Program order: every input inside the clone set first. A cycle is an
    // inner loop.
    let mut indeg: FxHashMap<NodeId, usize> = FxHashMap::default();
    for &c in &clone {
        let n = graph.node_opt(c).ok_or("a body node is gone")?;
        let d = n.inputs.iter().filter(|&&i| clone.contains(&i)).count();
        indeg.insert(c, d);
    }
    let mut ready: Vec<NodeId> = indeg
        .iter()
        .filter(|&(_, &d)| d == 0)
        .map(|(&c, _)| c)
        .collect();
    ready.sort_unstable_by(|a, b| b.cmp(a));
    let mut order: Vec<NodeId> = Vec::with_capacity(clone.len());
    while let Some(c) = ready.pop() {
        order.push(c);
        let mut next: Vec<NodeId> = Vec::new();
        for &u in &users[c as usize] {
            if let Some(d) = indeg.get_mut(&u) {
                // Each edge from `c` to `u` was counted once per input slot.
                let slots = graph
                    .node_opt(u)
                    .map_or(0, |n| n.inputs.iter().filter(|&&i| i == c).count());
                *d = d.saturating_sub(slots);
                if *d == 0 && !order.contains(&u) && !next.contains(&u) && !ready.contains(&u) {
                    next.push(u);
                }
            }
        }
        ready.extend(next);
        ready.sort_unstable_by(|a, b| b.cmp(a));
    }
    if order.len() != clone.len() {
        return Err("an inner cycle (a nested loop)");
    }

    // Frames: the loop's pc range, then every snapshot naming a cloned value.
    let mut lo = header_bci;
    let mut hi = header_bci;
    for &c in clone.iter().chain([t, stay].iter()) {
        if let Some(pc) = graph.node_opt(c).and_then(|n| n.bytecode_pc) {
            lo = lo.min(pc);
            hi = hi.max(pc);
        }
    }
    let mut stranded: Vec<usize> = vec![header_bci];
    for sp in graph.safepoints.iter() {
        let names_clone = sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .any(|&s| s != NO_NODE && clone.contains(&s));
        if !names_clone {
            continue;
        }
        if sp.is_splice_state() {
            return Err("a chain frame names a per-trip value");
        }
        if sp.bci < lo || sp.bci > hi {
            return Err("a frame outside the loop names a per-trip value");
        }
        if !stranded.contains(&sp.bci) {
            stranded.push(sp.bci);
        }
    }

    // ── Commit ──
    let mut cur: FxHashMap<NodeId, NodeId> = carried
        .iter()
        .map(|&p| (p, graph.nodes[p as usize].inputs[entry_slot]))
        .collect();
    let mut prev_ctrl = entry_pred;
    for _ in 0..trips {
        let mut subst: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        subst.insert(region, prev_ctrl);
        subst.insert(stay, prev_ctrl);
        for &p in &carried {
            subst.insert(p, cur[&p]);
        }
        for &c in &order {
            let (op, ty, volatile, inputs) = {
                let n = &graph.nodes[c as usize];
                (n.op.clone(), n.ty, n.volatile_access, n.inputs.clone())
            };
            let inputs: Vec<NodeId> = inputs
                .iter()
                .map(|i| subst.get(i).copied().unwrap_or(*i))
                .collect();
            let copy = graph.add(op, ty, inputs, None);
            if volatile {
                graph.set_volatile_access(copy);
            }
            subst.insert(c, copy);
        }
        prev_ctrl = subst.get(&back).copied().unwrap_or(back);
        let next: Vec<(NodeId, NodeId)> = carried
            .iter()
            .map(|&p| {
                let b = graph.nodes[p as usize].inputs[back_slot];
                (p, subst.get(&b).copied().unwrap_or(b))
            })
            .collect();
        for (p, v) in next {
            cur.insert(p, v);
        }
    }
    graph.replace_all_uses(exit, prev_ctrl);
    for &p in &carried {
        let fin = cur[&p];
        if fin != p {
            graph.replace_all_uses(p, fin);
        }
    }
    graph.kill(t);
    graph.kill(stay);
    graph.kill(exit);
    graph.kill(region);
    for &p in &carried {
        graph.kill(p);
    }
    for &c in order.iter().rev() {
        graph.kill(c);
    }
    UNROLL_STRANDED_BCIS.with(|s| s.borrow_mut().flat.extend(stranded));
    Ok(trips)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::CmpOp;

    /// `for (k = 0; k < trips; k++) acc += (k & 1) == 0 ? a : b; return acc;`
    /// by hand: header `H` at pc 2 with φs `k`, `acc`; test `If(k < trips)`;
    /// the body branches on `(k & 1) == 0` and joins at `M`, the latch.
    fn diamond_loop(trips: i64) -> (Graph, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let n = g.add(Op::Const(trips), IrType::Int, vec![], None);
        let h = g.add(Op::Merge, IrType::Control, vec![entry, NO_NODE], Some(2));
        let k = g.add(Op::Phi, IrType::Int, vec![h, zero, NO_NODE], Some(2));
        let acc = g.add(Op::Phi, IrType::Int, vec![h, zero, NO_NODE], Some(2));
        let test = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![k, n], Some(5));
        let iff = g.add(Op::If, IrType::Control, vec![h, test], Some(5));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(5));
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(5));
        let bit = g.add(Op::And, IrType::Int, vec![k, one], Some(9));
        let even = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![bit, zero], Some(11));
        let br = g.add(Op::If, IrType::Control, vec![stay, even], Some(11));
        let t_arm = g.add(Op::Proj(0), IrType::Control, vec![br], Some(11));
        let f_arm = g.add(Op::Proj(1), IrType::Control, vec![br], Some(11));
        let m = g.add(Op::Merge, IrType::Control, vec![t_arm, f_arm], Some(18));
        let x = g.add(Op::Phi, IrType::Int, vec![m, a, b], Some(18));
        let acc_next = g.add(Op::Add, IrType::Int, vec![acc, x], Some(19));
        let k_next = g.add(Op::Add, IrType::Int, vec![k, one], Some(21));
        assert!(g.set_input(h, 1, m));
        assert!(g.set_input(k, 2, k_next));
        assert!(g.set_input(acc, 2, acc_next));
        let ret = g.add(Op::Return, IrType::Void, vec![exit, acc], Some(30));
        g.exit = ret;
        (g, h, ret)
    }

    const SWITCH: &str = "CRATONVM_JIT_IR_UNROLL_CFG_BODY";

    /// The diamond loop is unrolled into straight-line copies, and after the
    /// cleanup folds each copy's branch the return reads `a + b + a + b`.
    #[test]
    fn r24_c_a_branching_constant_loop_is_fully_unrolled() {
        let _ = super::super::take_unroll_stranded_frames();
        let (mut g, h, ret) = diamond_loop(4);
        let n = unroll_cfg_loops(&mut g);
        assert_eq!(n, 1);
        assert_eq!(g.nodes[h as usize].op, Op::Dead, "the header is gone");
        super::super::prune_constant_ifs(&mut g);
        super::super::scalar_cleanup(&mut g);
        super::super::prune_constant_ifs(&mut g);
        super::super::scalar_cleanup(&mut g);
        assert!(
            !g.nodes.iter().any(|n| n.op == Op::If),
            "every copy's branch folded on its constant index"
        );
        // The value: an Add tree over the parameters, `a` twice and `b` twice
        // (one per copy, each copy's arm chosen by its constant index).
        let mut leaves: Vec<NodeId> = Vec::new();
        let mut stack = vec![g.nodes[ret as usize].inputs[1]];
        while let Some(v) = stack.pop() {
            match g.nodes[v as usize].op {
                Op::Add => stack.extend(g.nodes[v as usize].inputs.iter().copied()),
                Op::Param(_) => leaves.push(v),
                ref other => panic!("unexpected {other:?} in the folded sum"),
            }
        }
        let count = |want: u32| {
            leaves
                .iter()
                .filter(|&&l| matches!(g.nodes[l as usize].op, Op::Param(i) if i as u32 == want))
                .count()
        };
        assert_eq!((count(0), count(1)), (2, 2), "a + b + a + b");
        let stranded = super::super::take_unroll_stranded_frames();
        assert!(stranded.flat.contains(&2), "the header's frame is stranded");
    }

    /// Over the trip cap the loop stays; and the pass is on unless `=0`
    /// (default ON since j27 2026-10-28).
    #[test]
    fn r24_c_a_long_loop_keeps_its_header_and_the_switch_is_default_on() {
        let (mut g, h, _) = diamond_loop(MAX_TRIP + 1);
        assert_eq!(unroll_cfg_loops(&mut g), 0);
        assert_eq!(g.nodes[h as usize].op, Op::Merge);
        let unset =
            cratonvm_types::flags::with_thread_overrides(&[(SWITCH, None)], cfg_unroll_enabled);
        assert!(unset);
        let off =
            cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("0"))], cfg_unroll_enabled);
        assert!(!off);
    }

    /// A body node that can transfer (a field read) keeps the loop.
    #[test]
    fn r24_c_a_transferring_body_node_keeps_the_loop() {
        let (mut g, h, _) = diamond_loop(4);
        let start = g.entry;
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(Op::Param(2), IrType::Ref, vec![start], None);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        // Anchored at the merge (the latch): a body node.
        let m = g.nodes[h as usize].inputs[1];
        let ld = g.add(
            Op::Load(crate::ir::MemKind::Int),
            IrType::Int,
            vec![m, mem, p, off],
            Some(20),
        );
        let ret = g.exit;
        let acc = g.nodes[ret as usize].inputs[1];
        let acc_next = g.nodes[acc as usize].inputs[2];
        let bumped = g.add(Op::Add, IrType::Int, vec![acc_next, ld], Some(20));
        assert!(g.set_input(acc, 2, bumped));
        assert_eq!(unroll_cfg_loops(&mut g), 0);
        assert_eq!(g.nodes[h as usize].op, Op::Merge);
    }
}
