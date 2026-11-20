use super::*;
use crate::ir::IrBuilder;
use cratonvm_types::flags::with_thread_overrides;


/// `static int rowsum(int[][] a, int i, int n)`: three parameters, five
/// locals, two `int` loop phis (the item-3 module's fixture).
const ROWSUM: [u8; 30] = [
    0x03, 0x3e, 0x03, 0x36, 0x04, 0x15, 0x04, 0x1c, 0xa2, 0x00, 0x12, 0x1d, 0x2a, 0x1b, 0x32,
    0x15, 0x04, 0x2e, 0x60, 0x3e, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xee, 0x1d, 0xac, 0, 0,
];

/// `int sum(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }`
/// (the r16 codegen module's fixture): one parameter, three locals.
const SUM_LOOP: [u8; 23] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02,
    0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
];

fn sum_ref(n: i64) -> i32 {
    (0..n.max(0)).fold(0i32, |s, i| s.wrapping_add(i as i32))
}

fn rowsum_graph() -> Graph {
    let mut builder = IrBuilder::new(3, 5);
    builder.set_param_types(&[IrType::Ref, IrType::Int, IrType::Int]);
    builder.build(&ROWSUM, 28).expect("rowsum builds")
}


fn run(cm: &CompiledMethod, n: i64) -> i32 {
    // SAFETY: an `int sum(int)` body lowered by this module from valid
    // bytecode; it calls no helper.
    let got = unsafe { cm.try_call_with_context(0, &[n]) }.expect("call");
    // Cast: this tier's `int` result is the low 32 bits of RAX.
    got as i32
}


/// The frame's Java-local words: `max_locals` without the switch; the
/// parameters with it; never more than `max_locals`; and one past the
/// highest `Op::Param` the graph names when that is more than the count.
#[test]
fn r25w2_frame_locals_cover_the_parameters_only() {
    let graph = rowsum_graph();
    assert_eq!(ir_frame_locals(&graph, 3, 5, false), 5, "off: max_locals");
    assert_eq!(ir_frame_locals(&graph, 3, 5, true), 3, "on: the parameters");
    assert_eq!(ir_frame_locals(&graph, 3, 2, true), 2, "never past max_locals");
    assert_eq!(
        ir_frame_locals(&graph, 1, 5, true),
        3,
        "every `Op::Param` the graph names keeps its word"
    );
}

/// A hand-built graph: which `Pinned` nodes the claim may name. A phi
/// qualifies only when no reference can reach its home through an edge
/// copy, directly or through another phi; a monitor object and a
/// reference never do; a non-pinned node is not this rule's business.
#[test]
fn r25w2_a_phi_is_claimable_only_when_no_reference_can_reach_it() {
    let mut b = IrBuilder::new(0, 1);
    let m = b.graph.add(Op::Merge, IrType::Control, vec![], None);
    let one = b.graph.add(Op::Const(1), IrType::Int, vec![], None);
    let two = b.graph.add(Op::Const(2), IrType::Int, vec![], None);
    let obj = b.graph.add(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        IrType::Ref,
        vec![],
        None,
    );
    let ok = b.graph.add(Op::Phi, IrType::Int, vec![m, one, two], None);
    let loop_ok = b.graph.add(Op::Phi, IrType::Int, vec![m, one, ok], None);
    // `PHI_TYPE_FALLBACK`: declared `int`, but the edge copy stores `obj`.
    let mixed = b.graph.add(Op::Phi, IrType::Int, vec![m, obj, one], None);
    let downstream = b.graph.add(Op::Phi, IrType::Int, vec![m, mixed, one], None);
    let untyped = b.graph.add(Op::Phi, IrType::Int, vec![m, NO_NODE], None);
    let wide = b.graph.add(Op::Const(9), IrType::Long, vec![], None);
    let locked = b.graph.add(Op::Const(3), IrType::Int, vec![], None);
    let _enter = b.graph.add(Op::MonitorEnter, IrType::Void, vec![m, locked], None);
    let unpinned = b.graph.add(Op::Const(4), IrType::Int, vec![], None);
    // A memory phi's input may be a reference-typed load (the builder
    // threads loads into the chain), but no copy ever writes its home.
    let mem = b.graph.add(Op::Phi, IrType::Memory, vec![m, obj, one], None);
    let graph = &b.graph;
    let n = graph.nodes.len();
    let pinned = [ok, loop_ok, mixed, downstream, untyped, wide, locked, obj, mem];
    let mut node_color = vec![None; n];
    let mut class = vec![None; n];
    let mut next = 0u32;
    for &id in &pinned {
        node_color[id as usize] = Some(next);
        class[id as usize] = Some(SlotClass::Pinned);
        next += 1;
    }
    node_color[unpinned as usize] = Some(next);
    class[unpinned as usize] = Some(SlotClass::Prim);
    let plan = SlotPlan {
        node_color,
        range: vec![Some(LiveRange { lo: 0, hi: 9 }); n],
        class,
        slots: next as usize + 1,
        peak_live: next as usize + 1,
        sr_field_values: Vec::new(),
        pos_of: vec![None; n],
        phi_code_unused: vec![false; n],
        colour_range: Vec::new(),
        chain_ranged: 0,
        snapshot_ranged: None,
    };
    let claimable = claimable_pinned_nodes(graph, &plan);
    assert!(claimable[ok as usize], "an `int` phi of `int` inputs");
    assert!(claimable[loop_ok as usize], "and one fed by such a phi");
    assert!(claimable[wide as usize], "a pinned `long`");
    assert!(!claimable[mixed as usize], "a reference input");
    assert!(!claimable[downstream as usize], "a reference through another phi");
    assert!(!claimable[untyped as usize], "no typed input at all");
    assert!(!claimable[locked as usize], "a monitor object");
    assert!(!claimable[obj as usize], "a reference");
    assert!(!claimable[unpinned as usize], "not pinned: item 3's class");
    assert!(claimable[mem as usize], "a memory phi: its home is never written");
    let colours = plan.claimable_pinned_colors(&claimable);
    assert_eq!(colours, vec![0, 1, 5, 8], "ok, loop_ok, wide and mem, ascending");
}

/// The grouping numbers the claimable `Pinned` colours with the `Prim`
/// ones, each group in its old order, and is idempotent.
#[test]
fn r25w2_grouping_puts_claimable_pinned_colours_with_the_prims() {
    let r = |lo: usize, hi: usize| Some(LiveRange { lo, hi });
    let mut plan = SlotPlan {
        node_color: vec![Some(0), Some(1), Some(2), Some(3), None],
        range: vec![r(0, 9), r(0, 9), r(1, 9), r(3, 9), None],
        class: vec![
            Some(SlotClass::Pinned),
            Some(SlotClass::Ref),
            Some(SlotClass::Prim),
            Some(SlotClass::Pinned),
            None,
        ],
        slots: 4,
        peak_live: 4,
        sr_field_values: Vec::new(),
        pos_of: vec![None; 5],
        phi_code_unused: vec![false; 5],
        colour_range: Vec::new(),
        chain_ranged: 0,
        snapshot_ranged: None,
    };
    // Node 0 is a claimable pinned `int`, node 3 a pinned reference.
    let claimable = [true, false, false, false, false];
    assert!(plan.group_claimed_colours_first(&claimable), "colours move");
    assert_eq!(plan.node_color, vec![Some(0), Some(2), Some(1), Some(3), None]);
    assert_eq!(plan.prim_colors(), vec![1]);
    assert_eq!(plan.claimable_pinned_colors(&claimable), vec![0]);
    assert_eq!(plan.slots, 4, "the frame is not resized");
    assert!(!plan.group_claimed_colours_first(&claimable), "idempotent");
}

/// On a real plan: rowsum's `int` loop phis are claimable, the grouped
/// plan passes `verify_slot_colouring`, every claimed colour comes first,
/// and two nodes share a colour after exactly when they did before.
#[test]
fn r25w2_a_grouped_real_plan_still_verifies() {
    let graph = rowsum_graph();
    let schedule = crate::ir_schedule::schedule(&graph);
    let old = plan_slots(&graph, &schedule, None);
    let mut new = plan_slots(&graph, &schedule, None);
    let claimable = claimable_pinned_nodes(&graph, &new);
    assert!(
        !new.claimable_pinned_colors(&claimable).is_empty(),
        "precondition: rowsum's `int` loop phis are pinned and claimable"
    );
    for (id, &c) in claimable.iter().enumerate() {
        if c {
            assert_eq!(new.class[id], Some(SlotClass::Pinned), "n{id}");
            assert_ne!(graph.nodes[id].ty, IrType::Ref, "n{id}");
        }
    }
    new.group_claimed_colours_first(&claimable);
    verify_slot_colouring(&graph, &schedule, &new).expect("the grouped plan is sound");
    assert_eq!(new.slots, old.slots);
    let mut first = new.prim_colors();
    first.extend(new.claimable_pinned_colors(&claimable));
    first.sort_unstable();
    first.dedup();
    // Cast: a colour count, far below `u32::MAX`.
    assert_eq!(first, (0..first.len() as u32).collect::<Vec<u32>>());
    for a in 0..graph.nodes.len() {
        assert_eq!(new.class[a], old.class[a], "n{a}'s class");
        for b in (a + 1)..graph.nodes.len() {
            let shared =
                |p: &SlotPlan| p.node_color[a].is_some() && p.node_color[a] == p.node_color[b];
            assert_eq!(shared(&new), shared(&old), "n{a} / n{b}");
        }
    }
}



/// The spans the ranging re-derives agree with the plan's numbering: one
/// per block, each block's nodes at consecutive positions from its start,
/// its terminator next and its outgoing edge last.
#[test]
fn r25w3_the_slot_plan_spans_agree_with_its_numbering() {
    let graph = rowsum_graph();
    let schedule = crate::ir_schedule::schedule(&graph);
    let plan = plan_slots(&graph, &schedule, None);
    let span = slot_plan_spans(&schedule, &plan.pos_of).expect("the numbering re-derives");
    assert_eq!(span.len(), schedule.blocks.len());
    for (b, block) in schedule.blocks.iter().enumerate() {
        let (start, edge) = span[b];
        for (k, &nid) in block.nodes.iter().enumerate() {
            assert_eq!(plan.pos_of[nid as usize], Some(start + k), "b{b} node {k}");
        }
        let after = start + block.nodes.len() + usize::from(block.terminator.is_some());
        assert_eq!(edge, after, "b{b}'s edge follows its terminator");
        if b + 1 < span.len() {
            assert_eq!(span[b + 1].0, edge + 1, "b{} starts after b{b}'s edge", b + 1);
        }
    }
    // A plan whose numbering does not match the schedule is refused.
    let mut wrong = plan.pos_of.clone();
    if let Some(first) = schedule.blocks.iter().flat_map(|b| b.nodes.iter()).next() {
        wrong[*first as usize] = Some(usize::MAX);
        assert!(slot_plan_spans(&schedule, &wrong).is_none());
    }
}

/// On a real plan: rowsum's snapshot-named `int` values (not phis, not
/// parameters, not constants) leave the pinned class, the plan still
/// verifies, no reference is ranged, the frame does not grow, and every
/// ranged value's colouring range covers every position the plan recorded
/// as reading it -- which is what the lowerer's check then holds every
/// frame state to.
#[test]
fn r25w3_ranging_a_real_plan_unpins_snapshot_named_ints_and_still_verifies() {
    let graph = rowsum_graph();
    let schedule = crate::ir_schedule::schedule(&graph);
    let old = plan_slots(&graph, &schedule, None);
    let mut new = plan_slots(&graph, &schedule, None);
    assert!(
        apply_snapshot_range_colours(&graph, &schedule, &[], &mut new),
        "precondition: rowsum has snapshot-named `int` values the plan pinned"
    );
    verify_slot_colouring(&graph, &schedule, &new).expect("the ranged plan is sound");
    let ranged = new.snapshot_ranged.as_deref().expect("the facts are kept");
    assert!(!ranged.reads.is_empty());
    assert_eq!(ranged.slots_before, old.slots, "the count before is the old plan's");
    assert!(new.slots <= old.slots, "ranging never adds a colour");
    assert_eq!(new.slots_before_ranging(), old.slots);
    let colouring = new.colouring_ranges();
    for (&id, reads) in &ranged.reads {
        let i = id as usize;
        let node = &graph.nodes[i];
        assert_ne!(node.ty, IrType::Ref, "n{i}: references stay pinned");
        assert!(
            !matches!(node.op, Op::Phi | Op::Param(_) | Op::Const(_) | Op::ConstF(_)),
            "n{i}: {:?} stays pinned",
            node.op
        );
        assert_eq!(old.class[i], Some(SlotClass::Pinned), "n{i} was pinned");
        assert_eq!(new.class[i], Some(SlotClass::Prim), "n{i} is a `Prim` value now");
        assert!(reads.windows(2).all(|w| w[0] < w[1]), "n{i}: sorted, distinct");
        let r = colouring[i].expect("a colouring range");
        assert!(
            reads.iter().all(|&p| r.lo <= p && p <= r.hi),
            "n{i}: every recorded read is inside [{}, {}]",
            r.lo,
            r.hi
        );
        let base = old.range[i].expect("a value range");
        assert!(r.lo <= base.lo && base.hi <= r.hi, "n{i}: the range only widens");
    }
    for id in 0..graph.nodes.len() {
        if !ranged.reads.contains_key(&(id as NodeId)) {
            assert_eq!(
                new.class[id] == Some(SlotClass::Pinned),
                old.class[id] == Some(SlotClass::Pinned),
                "n{id}: only ranged values change class"
            );
        }
    }
}

/// The ranging touches nothing when no value qualifies: a plan without
/// snapshots keeps every colour.
#[test]
fn r25w3_a_graph_without_snapshots_is_left_alone() {
    let mut graph = rowsum_graph();
    graph.safepoints.clear();
    let schedule = crate::ir_schedule::schedule(&graph);
    let mut plan = plan_slots(&graph, &schedule, None);
    let before = plan.node_color.clone();
    assert!(!apply_snapshot_range_colours(&graph, &schedule, &[], &mut plan));
    assert_eq!(plan.node_color, before);
    assert!(plan.snapshot_ranged.is_none());
}


// ── JIT round 25 wave 8 (W8-6): `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE` ──


/// `static Object quad(Object[] a) { Object x = a[0]; x = a[1]; x = a[2];
/// x = a[3]; return x; }`: four `aaload` results through one local. The
/// builder records a snapshot at the top of every bytecode, so each result
/// is named (stack, then local 1) from its definition to the store that
/// replaces it -- pinned in the default plan -- and the first and third
/// (second and fourth) have disjoint read sets.
const QUAD: [u8; 20] = [
    0x2a, 0x03, 0x32, 0x4c, 0x2a, 0x04, 0x32, 0x4c, 0x2a, 0x05, 0x32, 0x4c, 0x2a, 0x06, 0x32,
    0x4c, 0x2b, 0xb0, 0, 0,
];

/// The whole of `QUAD` as one splice (as a spliced callee's combined-buffer
/// range would be), resuming at a pc no snapshot is recorded at.
const QUAD_SPLICE: [(usize, usize, usize); 1] = [(0, 18, 1000)];

/// The wave-8 candidate set alone (no wave-3 primitives).
const SPLICE_ONLY: SnapshotRangeSelect = SnapshotRangeSelect {
    all_prims: false,
    splice_local: true,
    all_refs: false,
};

fn quad_graph() -> Graph {
    let mut builder = IrBuilder::new(1, 2);
    builder.set_param_types(&[IrType::Ref]);
    builder.build(&QUAD, 18).expect("quad builds")
}

/// The safety property of a shared frame word, re-derived from the plan
/// rather than from `verify_slot_colouring`: at every position (so at
/// every safepoint, deopt read and GC point the plan numbers) a colour
/// holds at most one value whose colouring range -- every modelled
/// frame-state read included -- covers it; a colour's occupants are all
/// references or all non-references; and no recorded frame-state read of
/// a ranged value falls inside the range of another value of its colour.
fn assert_no_shared_colour_doubly_live(graph: &Graph, plan: &SlotPlan, what: &str) {
    let colouring = plan.colouring_ranges();
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); plan.slots];
    for (id, c) in plan.node_color.iter().enumerate() {
        if let Some(c) = c {
            members[*c as usize].push(id);
        }
    }
    let last = colouring.iter().flatten().map(|r| r.hi).max().unwrap_or(0);
    for (c, ids) in members.iter().enumerate() {
        if ids.len() < 2 {
            continue;
        }
        let refs = ids
            .iter()
            .filter(|&&id| graph.nodes[id].ty == IrType::Ref)
            .count();
        assert!(
            refs == 0 || refs == ids.len(),
            "{what}: colour {c} mixes references and primitives: {ids:?}"
        );
        for p in 0..=last {
            let live: Vec<usize> = ids
                .iter()
                .copied()
                .filter(|&id| colouring[id].is_some_and(|r| r.lo <= p && p <= r.hi))
                .collect();
            assert!(live.len() <= 1, "{what}: colour {c} holds {live:?} at position {p}");
        }
    }
    if let Some(sr) = plan.snapshot_ranged.as_deref() {
        for (&v, reads) in &sr.reads {
            let vi = v as usize;
            let c = plan.node_color[vi].expect("a ranged value has a colour") as usize;
            for &p in reads {
                for &other in &members[c] {
                    if other == vi {
                        continue;
                    }
                    assert!(
                        !colouring[other].is_some_and(|r| r.lo <= p && p <= r.hi),
                        "{what}: n{vi}'s frame-state read at {p} lands in n{other}'s range \
                         (colour {c})"
                    );
                }
            }
        }
    }
}


/// `splice_local_values`: nothing without a splice; with one, only values
/// built from the splice's pcs that some snapshot names, and none that a
/// snapshot outside every splice names.
#[test]
fn r25w8_splice_local_values_are_the_values_only_splices_name() {
    let graph = quad_graph();
    assert!(
        splice_local_values(&graph, &[]).iter().all(|&l| !l),
        "no splice, no splice-local value"
    );
    let local = splice_local_values(&graph, &QUAD_SPLICE);
    let names = |id: usize| {
        graph.safepoints.iter().any(|sp| {
            sp.locals
                .iter()
                .chain(sp.stack.iter())
                .any(|&v| v as usize == id)
        })
    };
    let refs: Vec<usize> = (0..graph.nodes.len())
        .filter(|&id| local[id] && graph.nodes[id].ty == IrType::Ref)
        .collect();
    assert!(refs.len() >= 2, "precondition: the aaload results are splice-local: {refs:?}");
    for (id, &l) in local.iter().enumerate() {
        if l {
            assert!(
                graph.nodes[id].bytecode_pc.is_some_and(|pc| pc < 18),
                "n{id}: built from the splice's pcs"
            );
            assert!(names(id), "n{id}: named by a snapshot");
        }
    }
    // A splice that ends at bci 8: a value any snapshot at 8.. names is
    // not splice-local.
    let part = splice_local_values(&graph, &[(0, 8, 1000)]);
    for sp in graph.safepoints.iter().filter(|sp| sp.bci >= 8) {
        for &v in sp.locals.iter().chain(sp.stack.iter()) {
            if let Some(&l) = part.get(v as usize) {
                assert!(!l, "n{v} is named at bci {}, outside the splice", sp.bci);
            }
        }
    }
}

/// The point of the switch: the four pinned `aaload` results become `Ref`
/// values coloured by their reads, two pairs of them share a frame word,
/// the plan still verifies, the frame shrinks, and no shared word ever
/// holds two live values.
#[test]
fn r25w8_disjoint_splice_values_share_frame_words_and_the_frame_shrinks() {
    let graph = quad_graph();
    let schedule = crate::ir_schedule::schedule(&graph);
    let old = plan_slots(&graph, &schedule, None);
    let mut new = plan_slots(&graph, &schedule, None);
    assert!(
        apply_snapshot_range_colours_with(&graph, &schedule, &QUAD_SPLICE, &mut new, SPLICE_ONLY),
        "precondition: quad has splice-local values the plan pinned"
    );
    verify_slot_colouring(&graph, &schedule, &new).expect("the shared plan is sound");
    assert!(
        new.slots < old.slots,
        "disjoint splice values share words: {} -> {} colours",
        old.slots,
        new.slots
    );
    assert_eq!(new.slots_before_ranging(), old.slots);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; only the frame arithmetic reads it.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let needs = scan_frame_needs(&graph, &helpers);
    assert!(
        estimate_frame_bytes(2, new.slots, &needs) < estimate_frame_bytes(2, old.slots, &needs),
        "the frame (and its fresh-frame clear) shrinks"
    );
    assert!(
        new.snapshot_ranged_ref_count(&graph) >= 2,
        "references are ranged under the switch"
    );
    // Two formerly pinned references now share a colour.
    let shared_refs = (0..new.slots).any(|c| {
        (0..graph.nodes.len())
            .filter(|&id| {
                new.node_color[id] == Some(c as u32)
                    && graph.nodes[id].ty == IrType::Ref
                    && old.class[id] == Some(SlotClass::Pinned)
            })
            .count()
            >= 2
    });
    assert!(shared_refs, "two pinned references of disjoint reads share a word");
    let ranged = new.snapshot_ranged.as_deref().expect("the facts are kept");
    let colouring = new.colouring_ranges();
    for (&id, reads) in &ranged.reads {
        let i = id as usize;
        let node = &graph.nodes[i];
        assert_eq!(old.class[i], Some(SlotClass::Pinned), "n{i} was pinned");
        let want = if node.ty == IrType::Ref {
            SlotClass::Ref
        } else {
            SlotClass::Prim
        };
        assert_eq!(new.class[i], Some(want), "n{i} joins its own class's pool");
        let r = colouring[i].expect("a colouring range");
        assert!(
            reads.iter().all(|&p| r.lo <= p && p <= r.hi),
            "n{i}: every recorded read is inside [{}, {}]",
            r.lo,
            r.hi
        );
        let base = old.range[i].expect("a value range");
        assert!(r.lo <= base.lo && base.hi <= r.hi, "n{i}: the range only widens");
    }
    for id in 0..graph.nodes.len() {
        if !ranged.reads.contains_key(&(id as NodeId)) {
            assert_eq!(
                new.class[id] == Some(SlotClass::Pinned),
                old.class[id] == Some(SlotClass::Pinned),
                "n{id}: only ranged values change class"
            );
        }
    }
    assert_no_shared_colour_doubly_live(&graph, &new, "quad, splice slot share");
    assert_no_shared_colour_doubly_live(&graph, &old, "quad, default plan");
}

/// Off, or with no splice, nothing changes; the wave-3 entry point still
/// ranges no reference, splice or not.
#[test]
fn r25w8_no_splice_or_no_switch_leaves_the_plan_alone() {
    let graph = quad_graph();
    let schedule = crate::ir_schedule::schedule(&graph);
    let mut plan = plan_slots(&graph, &schedule, None);
    let before = plan.node_color.clone();
    let off = SnapshotRangeSelect {
        all_prims: false,
        splice_local: false,
        all_refs: false,
    };
    assert!(!apply_snapshot_range_colours_with(&graph, &schedule, &QUAD_SPLICE, &mut plan, off));
    assert!(!apply_snapshot_range_colours_with(&graph, &schedule, &[], &mut plan, SPLICE_ONLY));
    assert_eq!(plan.node_color, before, "untouched");
    assert!(plan.snapshot_ranged.is_none());
    let mut wave3 = plan_slots(&graph, &schedule, None);
    let _ = apply_snapshot_range_colours(&graph, &schedule, &QUAD_SPLICE, &mut wave3);
    assert_eq!(wave3.snapshot_ranged_ref_count(&graph), 0, "wave 3 ranges no reference");
    for id in 0..graph.nodes.len() {
        if graph.nodes[id].ty == IrType::Ref && plan.class[id] == Some(SlotClass::Pinned) {
            assert_eq!(wave3.class[id], Some(SlotClass::Pinned), "n{id} stays pinned");
        }
    }
    // A real loop plan with a whole-method fake splice: still sound and
    // never larger.
    let rowsum = rowsum_graph();
    let rs = crate::ir_schedule::schedule(&rowsum);
    let old = plan_slots(&rowsum, &rs, None);
    let mut new = plan_slots(&rowsum, &rs, None);
    if apply_snapshot_range_colours_with(&rowsum, &rs, &[(0, 28, 1000)], &mut new, SPLICE_ONLY) {
        verify_slot_colouring(&rowsum, &rs, &new).expect("sound");
        assert!(new.slots <= old.slots, "ranging never adds a colour");
        assert_no_shared_colour_doubly_live(&rowsum, &new, "rowsum, splice slot share");
    }
}


// ── JIT round 25 wave 9 (W9-4): the census, `..._ALL_REFS`, and why the
// splice-local set alone does not engage ──


/// Every reference, no splice-local restriction, no primitive.
const REFS_ONLY: SnapshotRangeSelect = SnapshotRangeSelect {
    all_prims: false,
    splice_local: false,
    all_refs: true,
};

/// How many coloured values `plan` keeps `Pinned`.
fn pinned_count(plan: &SlotPlan) -> usize {
    (0..plan.class.len())
        .filter(|&id| {
            plan.class[id] == Some(SlotClass::Pinned) && plan.node_color[id].is_some()
        })
        .count()
}



/// The census counts every pinned coloured value exactly once, by origin
/// and type: with no splice `QUAD`'s references are the root's; under a
/// whole-method splice the same values are the splice's and splice-local.
#[test]
fn r25w9_the_census_says_where_the_pinned_colours_come_from() {
    let graph = quad_graph();
    let schedule = crate::ir_schedule::schedule(&graph);
    let plan = plan_slots(&graph, &schedule, None);
    let total = pinned_count(&plan);
    let sum = |c: &PinnedOrigins| {
        c.root_ref + c.root_prim + c.splice_ref + c.splice_prim + c.phi + c.param_const
    };
    let root = pinned_origins(&graph, &plan, &[]);
    assert_eq!(sum(&root), total, "{root:?}");
    assert!(root.root_ref >= 2, "{root:?}");
    assert_eq!((root.splice_ref, root.splice_prim, root.splice_local), (0, 0, 0));
    let spliced = pinned_origins(&graph, &plan, &QUAD_SPLICE);
    assert_eq!(sum(&spliced), total, "{spliced:?}");
    assert_eq!(spliced.root_ref + spliced.splice_ref, root.root_ref);
    assert!(spliced.splice_ref >= 2, "{spliced:?}");
    assert!(spliced.splice_local_ref >= 2, "{spliced:?}");
    assert!(spliced.splice_local >= spliced.splice_local_ref);
    assert_eq!(spliced.chain_ranged, plan.chain_ranged);
    assert_eq!(SnapshotRangeSkip::NoCandidate.label(), "no-candidate");
}



