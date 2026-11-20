use super::*;

/// `GenR5W2OsrDeadSlotProbe.cleared(WeakReference)`'s bytecode, as javac
/// 25 emits it: `for (i = 0; i < 3 && ref.get() != null; i++)
/// System.gc(); return ref.get() == null;`.
fn cleared_bytecode() -> Vec<u8> {
    vec![
        0x03, 0x3c, // 0 iconst_0, 1 istore_1
        0x1b, 0x06, // 2 iload_1, 3 iconst_3
        0xa2, 0x00, 0x13, // 4 if_icmpge 23
        0x2a, // 7 aload_0
        0xb6, 0x00, 0x07, // 8 invokevirtual get
        0xc6, 0x00, 0x0c, // 11 ifnull 23
        0xb8, 0x00, 0x0d, // 14 invokestatic System.gc
        0x84, 0x01, 0x01, // 17 iinc 1, 1
        0xa7, 0xff, 0xee, // 20 goto 2
        0x2a, // 23 aload_0
        0xb6, 0x00, 0x07, // 24 invokevirtual get
        0xc7, 0x00, 0x07, // 27 ifnonnull 34
        0x04, 0xa7, 0x00, 0x04, // 30 iconst_1, 31 goto 35
        0x03, 0xac, // 34 iconst_0, 35 ireturn
    ]
}

/// gcj hmg: the measured holder's shape, as a pc graph. An outer loop
/// (header 0, back edge 30 -> 0) runs a first inner loop (header 10, body
/// 12, back edge 14 -> 10) and then a second (header 20, the φ's merge,
/// body 22, back edge 24 -> 20). Only 22 names the second loop's φ. A call
/// in the first loop (12) reaches 22 only through the forward entry
/// 16 -> 20, so the φ is not kept there; a call in the second loop (24)
/// reaches 22 round its own back edge, so it is.

/// gcj hmg: which φs are candidates. A sole-occupant `Ref` φ whose every
/// edge writes its home, at an instruction of the method's own code; not
/// one with a self input, a shared colour, a handler or spliced bci, or a
/// primitive type.
#[test]
fn gcj_hmg_phi_reach_kill_candidates() {
    // iconst_0; istore_0; goto 2 -- instructions at 0, 1, 2.
    let code = [0x03u8, 0x3b, 0xa7, 0x00, 0x00];
    let verified = cratonvm_reader::verified_code(&code).expect("decodes");
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let a = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let b = g.add(Op::Proj(1), IrType::Control, vec![start], None);
    let merge = g.add(Op::Merge, IrType::Control, vec![a, b], Some(2));
    let v1 = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let v2 = g.add(Op::Param(1), IrType::Ref, vec![], None);
    let good = g.add(Op::Phi, IrType::Ref, vec![merge, v1, v2], Some(2));
    let selfie_id = g.nodes.len() as NodeId;
    let selfie = g.add(Op::Phi, IrType::Ref, vec![merge, v1, selfie_id], Some(2));
    let shared = g.add(Op::Phi, IrType::Ref, vec![merge, v1, v2], Some(2));
    let prim = g.add(Op::Phi, IrType::Int, vec![merge, v1, v2], Some(2));
    let mut node_color: Vec<Option<u32>> = vec![None; g.nodes.len()];
    node_color[v1 as usize] = Some(1);
    node_color[v2 as usize] = Some(2);
    node_color[good as usize] = Some(3);
    node_color[selfie as usize] = Some(4);
    node_color[shared as usize] = Some(5);
    node_color[prim as usize] = Some(6);
    let mut occupants = vec![1u32; 8];
    occupants[5] = 2;
    type Rows<'r> = &'r [(usize, usize, usize)];
    let kill = |id: NodeId, spliced: Rows<'_>, handlers: Rows<'_>| {
        phi_reach_kill_pc(
            &g,
            &node_color,
            &occupants,
            spliced,
            handlers,
            &verified,
            id,
        )
    };
    assert_eq!(kill(good, &[], &[]), Some((3, 2)));
    assert_eq!(kill(selfie, &[], &[]), None, "a self input writes nothing");
    assert_eq!(kill(shared, &[], &[]), None, "a shared colour");
    assert_eq!(kill(prim, &[], &[]), None, "a primitive φ");
    assert_eq!(kill(good, &[], &[(0, 2, 2)]), None, "a handler entry");
    assert_eq!(kill(good, &[(2, 5, 1)], &[]), None, "a spliced merge");
}

/// gcd d4/o: the measured holder. A spliced `Reference.get` at bci 8
/// defines colour 5, and the splice's return snapshot (no node carries
/// its pc) names it, credited to bci 8. The union kept 5 at the
/// `System.gc()` of bci 14 through the back edge; the kill drops it. It
/// stays kept for a call AT bci 8 (inside the splice, before the return
/// snapshot), and for a colour a snapshot names before the walk reaches
/// its definition.
#[test]
fn a_recomputed_spliced_value_is_not_kept_across_the_loop_call() {
    let code = cleared_bytecode();
    let verified = cratonvm_reader::verified_code(&code).expect("cleared decodes");
    let named: HashMap<usize, Vec<u64>> = [(20, bits_of(1, &[7]))].into_iter().collect();
    let splice_named: HashMap<usize, Vec<u64>> = [(8, bits_of(1, &[5])), (24, bits_of(1, &[6]))]
        .into_iter()
        .collect();
    let def_pc: HashMap<u32, usize> = [(5, 8), (6, 24), (7, 8)].into_iter().collect();

    let mut at_gc =
        snapshot_colours_reachable_with_splices(&verified, &named, &splice_named, 1, 14);
    assert_eq!(at_gc, bits_of(1, &[5, 6, 7]), "the union keeps all three");
    refine_reach_by_def_kills(&mut at_gc, &def_pc, &verified, &named, &splice_named, 14);
    assert_eq!(
        at_gc,
        bits_of(1, &[7]),
        "5 is redefined at 8 before its reader, 6 at 24; 7 is named at 20 first"
    );

    let mut at_get =
        snapshot_colours_reachable_with_splices(&verified, &named, &splice_named, 1, 8);
    refine_reach_by_def_kills(&mut at_get, &def_pc, &verified, &named, &splice_named, 8);
    assert!(
        bits_contains(&at_get, 5),
        "a call inside the splice at 8 still keeps the splice's own value"
    );

    // The budget: an exhausted walk keeps the colour.
    let mut spent = DEF_KILL_VISIT_BUDGET;
    assert_eq!(
        colour_named_reachable_avoiding(14, 8, |_| false, |pc| verified.successors(pc), &mut spent),
        None
    );
    let mut spent = 0;
    assert_eq!(
        colour_named_reachable_avoiding(
            14,
            8,
            |pc| pc == 11,
            |pc| verified.successors(pc),
            &mut spent
        ),
        Some(false),
        "bci 11 is reachable from 14 only through 8"
    );
    assert!(def_kill_op(&Op::Load(MemKind::Ref)));
    assert!(def_kill_op(&Op::ArrayLoad(MemKind::Ref)));
    assert!(
        !def_kill_op(&Op::Phi),
        "a phi's home is written by edge copies"
    );
    assert!(!def_kill_op(&Op::Proj(0)));
}

/// gen r5w3/live7: a call staging `stage_live` words leaves the rest of the
/// region dead; the region never reaches down into the spill band; a call
/// whose argument count is unknown (`usize::MAX`) claims nothing.
#[test]
fn staging_words_past_this_calls_arguments_are_dead() {
    // Region of 4 words, arg[0] at off 200: offsets 200, 192, 184, 176.
    let (covered, dead) = staging_liveness_words(200, 4, 96, 1);
    assert_eq!(covered, vec![200, 192, 184, 176]);
    assert_eq!(dead, vec![192, 184, 176], "arg[1..] were an earlier call's");
    let (_, dead) = staging_liveness_words(200, 4, 96, 4);
    assert!(dead.is_empty(), "every word is this call's own argument");
    let (_, dead) = staging_liveness_words(200, 4, 96, usize::MAX);
    assert!(dead.is_empty(), "an unknown count keeps every word");
    // A region that would dip to the spill cap stops covering there.
    let (covered, dead) = staging_liveness_words(112, 4, 96, 0);
    assert_eq!(covered, vec![112, 104]);
    assert_eq!(dead, vec![112, 104]);
    // No region at all.
    assert_eq!(
        staging_liveness_words(200, 0, 96, 0),
        (Vec::new(), Vec::new())
    );
}

fn bits_of(words: usize, colours: &[usize]) -> Vec<u64> {
    let mut bits = vec![0u64; words];
    for &c in colours {
        bits_insert(&mut bits, c);
    }
    bits
}

/// gcd d6/u: a call reused as the next call's MEMORY token keeps its value
/// range at its own position; the slot plan's range reaches the token's
/// user, which is how `cleared`'s `ref.get()` home escaped the clear at
/// its `System.gc()`.
#[test]
fn a_memory_token_edge_is_not_a_read_of_the_value() {
    let mut g = Graph::empty(8);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let first = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Ref,
        vec![ctrl, mem],
        Some(8),
    );
    let second = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![ctrl, first],
        Some(14),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl], Some(17));
    let schedule = crate::ir_schedule::schedule(&g);
    let plan = plan_slots(&g, &schedule, None);
    let p_first = plan.pos_of[first as usize].expect("scheduled");
    let p_second = plan.pos_of[second as usize].expect("scheduled");
    assert!(p_first < p_second, "the token orders the calls");
    let plan_range = plan.range[first as usize].expect("coloured");
    assert!(
        plan_range.hi >= p_second,
        "precondition: the slot plan counts the token edge as a read"
    );
    let value = value_use_ranges(&g, &schedule, &plan).expect("converges");
    assert_eq!(
        value[first as usize],
        Some(LiveRange {
            lo: p_first,
            hi: p_first
        }),
        "nothing reads the first call's value"
    );
    assert!(
        value[second as usize].is_none(),
        "a void call is not tracked"
    );
}

/// gcd d5/u: only a `Ref` phi whose every incoming edge writes its home
/// is killed at its merge block's entry -- not one with a self input, an
/// input in its own home, a missing edge value, the wrong arity, or a
/// shared colour, and never a primitive phi.
#[test]
fn a_phi_is_killed_at_its_merge_only_when_every_edge_writes_its_home() {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let a = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let b = g.add(Op::Proj(1), IrType::Control, vec![start], None);
    let merge = g.add(Op::Merge, IrType::Control, vec![a, b], Some(11));
    let v1 = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let v2 = g.add(Op::Param(1), IrType::Ref, vec![], None);
    let p = g.add(Op::Phi, IrType::Ref, vec![merge, v1, v2], Some(11));
    let q_id = g.nodes.len() as NodeId;
    let q = g.add(Op::Phi, IrType::Ref, vec![merge, v1, q_id], Some(11));
    let r = g.add(Op::Phi, IrType::Ref, vec![merge, v1, NO_NODE], Some(11));
    let s = g.add(Op::Phi, IrType::Ref, vec![merge, v1, v2], Some(11));
    let t = g.add(Op::Phi, IrType::Ref, vec![merge, v1], Some(11));
    let u = g.add(Op::Phi, IrType::Int, vec![merge, v1, v2], Some(11));
    let w = g.add(Op::Phi, IrType::Ref, vec![merge, v2, v1], Some(11));
    assert_eq!(q, q_id);
    let mut node_color: Vec<Option<u32>> = vec![None; g.nodes.len()];
    node_color[v1 as usize] = Some(1);
    node_color[v2 as usize] = Some(2);
    node_color[p as usize] = Some(10);
    node_color[q as usize] = Some(11);
    node_color[r as usize] = Some(12);
    // `s` shares its home with its input `v2`: that edge writes nothing.
    node_color[s as usize] = Some(2);
    node_color[t as usize] = Some(14);
    node_color[u as usize] = Some(15);
    // `w`'s colour has a second occupant.
    node_color[w as usize] = Some(16);
    let mut occupants = vec![1u32; 20];
    occupants[16] = 2;
    let block_of_merge: HashMap<NodeId, usize> = [(merge, 1usize)].into_iter().collect();
    let kills = phi_entry_kills(&g, &block_of_merge, &node_color, &occupants, 2);
    assert!(kills[0].is_empty(), "no merge heads block 0");
    assert_eq!(
        kills[1],
        vec![(p, 10)],
        "only `p` has a write on every edge"
    );
    // A merge no block is headed by kills nothing.
    let none = phi_entry_kills(&g, &HashMap::new(), &node_color, &occupants, 2);
    assert!(none.iter().all(Vec::is_empty));
}

/// gcd d5/u: the measured holder's shape. A loop whose header is the join
/// of a guarded splice: the phi (c2) is named by the `ifnull` after the
/// join and recomputed every round. With the entry kill it is dead at the
/// loop's call; without it (gen r5w6), the next round's `ifnull` keeps it
/// live across the call through the back edge.
#[test]
fn a_guarded_splices_result_phi_is_dead_at_the_loops_call() {
    let step = |reads: &[usize], kill: Option<usize>, node: NodeId, is_call: bool| LiveStep {
        reads: bits_of(1, reads),
        kill,
        node,
        is_call,
    };
    // Block 0 (entry): defines c0 (the loop-invariant `ref`) -> 1.
    // Block 1 (the join): the phi's entry kill of c2, then the `ifnull`
    //   whose frame state names c0 and c2 -> 2 (body), 3 (exit).
    // Block 2 (body): `System.gc()` (node 20, whose own state names c0)
    //   -> 1.
    // Block 3 (exit): a node whose state names c0.
    let steps = vec![
        vec![step(&[], Some(0), 10, false)],
        vec![
            step(&[], Some(2), 11, false),
            step(&[0, 2], None, 12, false),
        ],
        vec![step(&[0], None, 20, true)],
        vec![step(&[0], None, 30, false)],
    ];
    let successors = vec![vec![1], vec![2, 3], vec![1], vec![]];
    let after = live_colours_after_calls(&steps, &successors, 1).expect("converges");
    assert_eq!(
        after.get(&20),
        Some(&bits_of(1, &[0])),
        "c2 is rewritten at the join"
    );
    let mut no_entry_kill = steps.clone();
    no_entry_kill[1].remove(0);
    let after = live_colours_after_calls(&no_entry_kill, &successors, 1).expect("converges");
    assert_eq!(
        after.get(&20),
        Some(&bits_of(1, &[0, 2])),
        "without the kill the next round's reader keeps it"
    );
}

/// gcj hmg: an OSR arm's `OsrLocal` names nothing in the keep set when it
/// is its colour's sole occupant; a shared colour, a `Param` and a φ keep
/// the old rule; and the switch is default ON with a `=0` kill.

/// gci i40/os: the step kill. A sole occupant kills its colour, any op;
/// a φ never does; a `Ref` call / allocation / load kills a SHARED colour
/// only under the switch; any other op, a primitive and a parameter keep
/// the sole-occupant rule.

/// gci i40/os, the measured holder's shape (`gci-i31om`, vi38-ds K):
/// `cleared(ref)` spliced into an OSR body. The spliced `ref.get()` result
/// v (colour c3) is named only by chain snapshots, so it shares c3 with an
/// earlier temporary w; the `ifnull` after it reads c3, the loop's
/// `System.gc()` (n20) follows. With the shared-colour kill c3 is dead at
/// the call (the next round rewrites it before its `ifnull` reads it);
/// with the sole-occupant rule the next round's reader keeps this round's
/// value live across the call through the back edge -- the root the
/// census named.
#[test]
fn gci_i40os_a_spliced_loop_value_in_a_shared_colour_is_dead_at_the_loops_call() {
    let mut occupants = vec![1u32; 4];
    occupants[3] = 2;
    let build = |shared: bool| {
        let load = Op::Load(MemKind::Ref);
        let kill = |op: &Op, colour: u32| {
            keep_set_step_kill(op, IrType::Ref, Some(colour), &occupants, shared)
        };
        let step = |reads: &[usize], kill: Option<usize>, node: NodeId, is_call: bool| LiveStep {
            reads: bits_of(1, reads),
            kill,
            node,
            is_call,
        };
        // Block 0 (entry): w (c3) and `ref` (c0, its sole occupant) -> 1.
        // Block 1 (header): `i < 3` -> 2 (body), 4 (exit).
        // Block 2: v = `ref.referent` (c3), the `ifnull` whose chain
        //   snapshot names c0 and c3 -> 3, 4.
        // Block 3: `System.gc()` (n20, its state names c0) -> 1.
        // Block 4 (exit): a node whose state names c0.
        let steps = vec![
            vec![
                step(&[], kill(&load, 3), 10, false),
                step(&[], kill(&load, 0), 11, false),
            ],
            vec![step(&[], None, 12, false)],
            vec![
                step(&[], kill(&load, 3), 13, false),
                step(&[0, 3], None, 14, false),
            ],
            vec![step(&[0], None, 20, true)],
            vec![step(&[0], None, 30, false)],
        ];
        let successors = vec![vec![1], vec![2, 4], vec![3, 4], vec![1], vec![]];
        live_colours_after_calls(&steps, &successors, 1).expect("converges")
    };
    let on = build(true);
    assert_eq!(
        on.get(&20),
        Some(&bits_of(1, &[0])),
        "v is rewritten before the next round's ifnull reads it"
    );
    let off = build(false);
    assert_eq!(
        off.get(&20),
        Some(&bits_of(1, &[0, 3])),
        "the sole-occupant rule keeps the shared word live across the call"
    );
}

/// gci i47w/ks47: the by-pc reads of one step. A φ / pure op at a spliced
/// pc reads nothing under the switch and the full union without it; an op
/// that can exit, and any op at one of the method's own pcs, read the
/// union either way; the read set is the same in every case.
#[test]
fn gci_i47w_ks47_an_exitless_step_at_a_spliced_pc_reads_no_snapshot() {
    // Combined pcs 100..300 are a splice of the invoke at own pc 65.
    let ranges = [(100usize, 300usize, 65usize)];
    let named: HashMap<usize, Vec<u64>> = [
        (177, bits_of(1, &[24])),
        (65, bits_of(1, &[8])),
        (40, bits_of(1, &[3])),
    ]
    .into_iter()
    .collect();
    let reads = |op: &Op, pc: usize, on: bool| {
        let mut read_pcs = std::collections::HashSet::new();
        let r = keep_set_step_pc_reads(
            op,
            Some(pc),
            &named,
            &ranges,
            false,
            on,
            1,
            &mut read_pcs,
            None,
        );
        let mut pcs: Vec<usize> = read_pcs.into_iter().collect();
        pcs.sort_unstable();
        (r, pcs)
    };
    let phi = Op::Phi;
    let konst = Op::Const(0);
    let load = Op::Load(MemKind::Ref);
    let call = Op::Call { info_ptr: 0 };
    assert!(keep_set_step_is_exitless(&phi) && keep_set_step_is_exitless(&konst));
    assert!(keep_set_step_is_exitless(&Op::Cmp(crate::ir::CmpOp::Eq)));
    for op in [&load, &call, &Op::Param(0)] {
        assert!(!keep_set_step_is_exitless(op), "{op:?} is not exitless");
    }
    for op in [&phi, &konst] {
        assert_eq!(
            reads(op, 177, true),
            (bits_of(1, &[]), vec![65, 177]),
            "on: the join's {op:?} reads nothing, both pcs still count as read"
        );
        assert_eq!(
            reads(op, 177, false),
            (bits_of(1, &[8, 24]), vec![65, 177]),
            "=0: the per-pc union, as before"
        );
        assert_eq!(
            reads(op, 40, true),
            (bits_of(1, &[3]), vec![40]),
            "an own pc keeps the union"
        );
    }
    for op in [&load, &call] {
        for on in [false, true] {
            assert_eq!(
                reads(op, 177, on),
                (bits_of(1, &[8, 24]), vec![65, 177]),
                "{op:?} can exit: the union stays"
            );
        }
    }
    let mut none = std::collections::HashSet::new();
    assert_eq!(
        keep_set_step_pc_reads(&phi, None, &named, &ranges, false, true, 1, &mut none, None),
        bits_of(1, &[])
    );
    assert!(none.is_empty(), "no pc, nothing read");
    let on = cratonvm_types::flags::with_thread_overrides(
        &[(
            concat!("CRATON", "VM_JIT_IR_KEEP_SET_SPLICED_EXITLESS_STEPS"),
            None,
        )],
        ir_keep_set_spliced_exitless_steps_enabled,
    );
    assert!(on, "default ON");
}

/// gci i47w/ks47, the measured holder's shape (`gci-i31om` D: colour 24 in
/// `after` at the loop's calls, killed at its definition n65). `cleared(ref)`
/// spliced, its `ref.get()` a GUARDED splice: the hit body's referent load
/// v (c24) or the miss call m; their join φ (c15) sits at the hit body's
/// `areturn` pc, whose chain snapshot names v. The `ifnull` reads the
/// φ, then `System.gc()` (n74) and the back edge. With the join φ reading
/// its pc's union, the MISS edge reads c24 before any definition, so the
/// previous round's v is live across n74; with the exitless rule it is not.
#[test]
fn gci_i47w_ks47_a_guarded_get_join_does_not_keep_the_hit_value_across_the_loops_gc() {
    let ranges = [(100usize, 300usize, 65usize)];
    // 177: the hit body's `areturn` (chain names v = c24); 160: the
    // `ifnull` (chain names the φ, c15); 65: the own invoke (names c8).
    let named: HashMap<usize, Vec<u64>> = [
        (177, bits_of(1, &[24])),
        (160, bits_of(1, &[15])),
        (65, bits_of(1, &[8])),
    ]
    .into_iter()
    .collect();
    let build = |on: bool| {
        let mut read_pcs = std::collections::HashSet::new();
        let mut pc_reads = |op: &Op, pc: usize| {
            keep_set_step_pc_reads(
                op,
                Some(pc),
                &named,
                &ranges,
                false,
                on,
                1,
                &mut read_pcs,
                None,
            )
        };
        let step = |reads: Vec<u64>, kill: Option<usize>, node: NodeId, is_call: bool| LiveStep {
            reads,
            kill,
            node,
            is_call,
        };
        let load = Op::Load(MemKind::Ref);
        let call = Op::Call { info_ptr: 0 };
        // Block 0: entry -> 1. Block 1: header `i < 3` -> 2, 6.
        // Block 2: the receiver guard -> 3 (hit), 4 (miss).
        // Block 3: v = referent load (c24, pc 150) -> 5.
        // Block 4: m = the miss call (n64, pc 150) -> 5.
        // Block 5: the join φ (pc 177) and the `ifnull` (pc 160) -> 7, 6.
        // Block 7: `System.gc()` (n74, pc 170) -> 1. Block 6: exit.
        let steps = vec![
            vec![step(bits_of(1, &[]), None, 1, false)],
            vec![step(bits_of(1, &[]), None, 2, false)],
            vec![step(
                pc_reads(&Op::Cmp(crate::ir::CmpOp::Eq), 65),
                None,
                3,
                false,
            )],
            vec![step(pc_reads(&load, 150), Some(24), 65, false)],
            vec![step(pc_reads(&call, 150), Some(33), 64, true)],
            vec![
                // `phi_entry_kills`: the edge copies rewrite the φ's home.
                step(bits_of(1, &[]), Some(15), 67, false),
                step(pc_reads(&Op::Phi, 177), None, 68, false),
                step(pc_reads(&load, 160), None, 69, false),
            ],
            vec![step(bits_of(1, &[]), None, 90, false)],
            vec![step(pc_reads(&call, 170), None, 74, true)],
        ];
        let successors = vec![
            vec![1],
            vec![2, 6],
            vec![3, 4],
            vec![5],
            vec![5],
            vec![7, 6],
            vec![],
            vec![1],
        ];
        live_colours_after_calls(&steps, &successors, 1).expect("converges")
    };
    let on = build(true);
    assert_eq!(
        on.get(&74),
        Some(&bits_of(1, &[8])),
        "the hit value is dead at the loop's gc: the miss edge no longer reads it"
    );
    assert_eq!(on.get(&64), Some(&bits_of(1, &[8])), "nor at the miss call");
    let off = build(false);
    assert_eq!(
        off.get(&74),
        Some(&bits_of(1, &[8, 24])),
        "=0: the join φ's per-pc union keeps the previous round's value"
    );
    assert_eq!(
        off.get(&64),
        Some(&bits_of(1, &[8, 24])),
        "=0: the miss call keeps it too (the trace's `after` at n64)"
    );
}

/// gci i41/rv (review of the i40/os kill): the shared-colour kill is exact
/// only because a `Ref` definition always WRITES its home -- the dropped-
/// home path (`value_home_droppable`) admits `Int` / `Long` alone -- and
/// because a `Ref` call / `LoadStatic` never RECEIVES a recycled colour
/// (`fresh_only`), so at its own site the colour holds no older occupant.
/// Pin both premises next to the kill: a change that lets a `Ref` home go
/// unwritten, or a call result take a recycled colour, must revisit
/// `keep_set_step_kill` first.
#[test]
fn gci_i41rv_the_shared_colour_kill_premises_hold() {
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
    )
    .replace("\r\n", "\n");
    let droppable = src
        .split("fn value_home_droppable(&self, id: NodeId) -> bool \u{7b}")
        .nth(1)
        .and_then(|s| s.split("\n    \u{7d}\n").next())
        .expect("value_home_droppable is in this file");
    assert!(
        droppable.contains(
            "if !matches!(node.ty, IrType::Int | IrType::Long) \u{7b}\n            return false;"
        ),
        "a dropped home stays primitive-only: {droppable}"
    );
    let plan = src
        .split("fn plan_slots_from_core(")
        .nth(1)
        .and_then(|s| s.split("\n\u{7d}\n").next())
        .expect("plan_slots_from_core is in this file");
    // JIT round 25 wave 3 moved the fresh-only rule into ONE predicate,
    // `colour_is_fresh_only`, shared by `plan_slots_from_core` and
    // `recolour_slot_plan`: pin the predicate's `Ref` call / `LoadStatic`
    // members, and that both colourers route through it.
    let fresh = src
        .split("\npub(crate) fn colour_is_fresh_only(node: &Node) -> bool \u{7b}")
        .nth(1)
        .and_then(|s| s.split("\n\u{7d}\n").next())
        .expect("colour_is_fresh_only is in this file");
    assert!(
        fresh.contains("node.ty == IrType::Ref"),
        "the fresh-only rule is about `Ref` results: {fresh}"
    );
    for op in [
        "Op::Call \u{7b} .. \u{7d}",
        "Op::LoadStatic \u{7b} .. \u{7d}",
    ] {
        assert!(fresh.contains(op), "{op} results take a fresh colour");
    }
    assert!(
        plan.contains("if colour_is_fresh_only(node) \u{7b}\n            fresh_only[id] = true;"),
        "plan_slots_from_core marks fresh-only values by the shared predicate"
    );
    assert!(
        plan.contains("let recycled = if fresh_only[id] \u{7b}\n            None"),
        "a fresh-only value never takes a recycled colour"
    );
    let recolour = src
        .split("fn recolour_slot_plan(")
        .nth(1)
        .and_then(|s| s.split("\n\u{7d}\n").next())
        .expect("recolour_slot_plan is in this file");
    assert!(
        recolour.contains("let recycled = if colour_is_fresh_only(node) \u{7b}\n            None"),
        "nor does the recolouring pass hand one a recycled colour"
    );
}

/// r21 keepset: `int f(int n){ int k = n * 3; int s = 0; for (int i = 0;
/// i < n; i++) s += i; return s + k; }`. The loop header is pc 8 (an
/// `iload_3`: no node there but the header φs), its latch the `goto 8`.
const R21_KEEPSET_LOOP: [u8; 27] = [
    0x1a, 0x06, 0x68, 0x3c, // 0 iload_0, 1 iconst_3, 2 imul, 3 istore_1
    0x03, 0x3d, 0x03, 0x3e, // 4 iconst_0, 5 istore_2, 6 iconst_0, 7 istore_3
    0x1d, 0x1a, // 8 iload_3, 9 iload_0
    0xa2, 0x00, 0x0d, // 10 if_icmpge 23
    0x1c, 0x1d, 0x60, 0x3d, // 13 iload_2, 14 iload_3, 15 iadd, 16 istore_2
    0x84, 0x03, 0x01, // 17 iinc 3 1
    0xa7, 0xff, 0xf4, // 20 goto 8
    0x1c, 0x1b, 0x60, 0xac, // 23 iload_2, 24 iload_1, 25 iadd, 26 ireturn
];

/// The loop above, unoptimised, its snapshots replaced by ONE guard-chain
/// snapshot at the header pc naming `k` (defined before the loop) and a
/// header φ that is not its own back-edge input -- the state a spliced
/// header's mode exit reads -- plus, with `post_call`, a post-call-only
/// chain snapshot at pc 3 (no node there) naming `k` as the post-call
/// state of the `imul`. `(graph, schedule, k, phi, merge)`.
fn r21_keepset_fixture(post_call: bool) -> (Graph, Schedule, NodeId, NodeId, NodeId) {
    let mut graph = crate::ir::IrBuilder::new(1, 4)
        .build(&R21_KEEPSET_LOOP, R21_KEEPSET_LOOP.len())
        .expect("the counted loop builds");
    let merge = graph
        .nodes
        .iter()
        .position(|n| n.op == Op::Merge && n.bytecode_pc == Some(8))
        .expect("the loop header") as NodeId;
    let phi = graph
        .nodes
        .iter()
        .enumerate()
        .position(|(id, n)| {
            n.op == Op::Phi
                && n.ty == IrType::Int
                && n.inputs.first() == Some(&merge)
                && n.inputs
                    .iter()
                    .skip(1)
                    .all(|&v| v != id as NodeId && v != NO_NODE)
        })
        .expect("a header value phi with real inputs") as NodeId;
    let k = graph
        .nodes
        .iter()
        .position(|n| n.op == Op::Mul)
        .expect("k = n * 3") as NodeId;
    graph.safepoints.clear();
    let mut sp = SafepointSnapshot::new(8, vec![k, phi], Vec::new(), Vec::new());
    sp.splice_scope = Some(std::sync::Arc::new(crate::ir::SpliceScopeChain {
        scopes: Vec::new(),
    }));
    graph.safepoints.push(sp);
    if post_call {
        let mut pc = SafepointSnapshot::new(3, vec![k], Vec::new(), Vec::new());
        pc.splice_scope = Some(std::sync::Arc::new(crate::ir::SpliceScopeChain {
            scopes: Vec::new(),
        }));
        pc.post_call_of = k;
        pc.post_call_only = true;
        graph.safepoints.push(pc);
    }
    let schedule = crate::ir_schedule::schedule(&graph);
    (graph, schedule, k, phi, merge)
}

/// The latch of [`r21_keepset_fixture`]'s loop and the header φ's input on
/// its edge.
fn r21_keepset_latch(
    graph: &Graph,
    schedule: &Schedule,
    phi: NodeId,
    merge: NodeId,
) -> (usize, NodeId) {
    let mut chain_by_pc: HashMap<usize, Vec<usize>> = HashMap::new();
    chain_by_pc.insert(8, vec![0]);
    let latch = (0..schedule.blocks.len())
        .find(|&b| spliced_latch_exit_reads(graph, schedule, b, &chain_by_pc, None).is_some())
        .expect("the goto latch of the loop has the exit's shape");
    let k = graph.nodes[merge as usize]
        .inputs
        .iter()
        .position(|&c| ctrl_block_of(graph, schedule, c) == Some(latch))
        .expect("the latch's edge into the header");
    let src = graph.nodes[phi as usize]
        .inputs
        .get(k + 1)
        .copied()
        .expect("the phi's input on the latch edge");
    assert_ne!(src, phi, "precondition: the edge input is another node");
    (latch, src)
}

/// r21 keepset (residual 3 of the gci i40/os page): what the spliced
/// back-edge mode exit reads -- the header snapshot's values, a header φ
/// by its input on the latch edge and never by itself -- and only at a
/// latch of the exit's shape whose header pc holds a chain snapshot.

/// r21 keepset (rev-ir finding 2): the keep set's latch read and the
/// lowering's mode exit answer the shape question through ONE function,
/// `back_edge_exit_shape`. Pinned two ways: on the fixture, every block's
/// `spliced_latch_exit_reads` answer equals "the poll gets a header (no
/// terminator, first successor at or before it) and the shared test
/// admits that edge"; and by source text, the lowering's
/// `back_edge_mode_exit_state` calls the shared function and no longer
/// spells the structural refusals itself.
#[test]
fn r21_keepset_the_latch_read_and_the_mode_exit_share_their_shape_test() {
    let (graph, schedule, _k, _phi, _merge) = r21_keepset_fixture(false);
    let mut chain_by_pc: HashMap<usize, Vec<usize>> = HashMap::new();
    chain_by_pc.insert(8, vec![0]);
    let mut seen_exit = false;
    for b in 0..schedule.blocks.len() {
        let blk = &schedule.blocks[b];
        let lowering = blk.terminator.is_none()
            && blk.successors.first().is_some_and(|&h| {
                h <= b
                    && back_edge_exit_shape(&graph, &schedule, b, h)
                        .is_ok_and(|(_, pc)| chain_by_pc.contains_key(&pc))
            });
        let keep_set = spliced_latch_exit_reads(&graph, &schedule, b, &chain_by_pc, None).is_some();
        assert_eq!(keep_set, lowering, "block {b}: the two predicates disagree");
        seen_exit |= lowering;
    }
    assert!(seen_exit, "the fixture has an exit-shaped latch");
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
    )
    .replace("\r\n", "\n");
    let exit_fn = src
        .split("fn back_edge_mode_exit_state(\n")
        .nth(1)
        .and_then(|s| s.split("\n    fn ").next())
        .expect("back_edge_mode_exit_state is in this file");
    assert!(
        exit_fn.contains("back_edge_exit_shape("),
        "the lowering uses the shared test"
    );
    for why in [
        "pred-ends-in-a-branch",
        "pred-not-a-plain-edge",
        "not-a-dominated-back-edge",
    ] {
        assert!(
            !exit_fn.contains(why),
            "{why} is spelled only in back_edge_exit_shape"
        );
    }
    let latch_fn = src
        .split("fn spliced_latch_exit_reads(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .expect("spliced_latch_exit_reads is in this file");
    assert!(
        latch_fn.contains("back_edge_exit_shape("),
        "the keep set uses the shared test"
    );
}

/// r21 keepset, the defect: inside a splice the header φs are exitless
/// steps (gci i47w/ks47) and read nothing, so before this round no step
/// read the header's chain snapshot and the latch's poll step recorded
/// nothing live -- the poll's slow path zeroed `k`'s home right before the
/// mode exit resumed the callee with it. With the fix the poll step keeps
/// exactly the exit's colours (`k` and the φ's edge input, not the φ, so
/// gcd d5/u's φ kill keeps its effect); `=0` is the old empty set.
#[test]
fn r21_keepset_a_spliced_latch_poll_keeps_what_its_mode_exit_reads() {
    let (graph, schedule, k, phi, merge) = r21_keepset_fixture(false);
    let (latch, src) = r21_keepset_latch(&graph, &schedule, phi, merge);
    let plan = plan_slots(&graph, &schedule, None);
    let words = plan.slots.div_ceil(64).max(1);
    let colour = |v: NodeId| plan.node_color[v as usize].expect("a coloured value") as usize;
    let sp_bits = vec![bits_of(words, &[colour(k), colour(phi)])];
    // Every pc inside one splice whose invoke (1000) holds no snapshot.
    let spliced = [(0usize, R21_KEEPSET_LOOP.len(), 1000usize)];
    let latch_ctrl = schedule.blocks[latch].ctrl;
    let flag = concat!("CRATON", "VM_JIT_IR_SPLICED_LATCH_EXIT_READS");
    let at_poll = |v: Option<&str>| {
        cratonvm_types::flags::with_thread_overrides(&[(flag, v)], || {
            let (after, read) = snapshot_colours_live_after_each_call(
                &graph,
                &schedule,
                &plan,
                &sp_bits,
                &spliced,
                words,
                &[],
                false,
                None,
                None,
            )
            .expect("converges");
            (
                after
                    .get(&latch_ctrl)
                    .cloned()
                    .expect("the latch's poll step"),
                read[0],
            )
        })
    };
    let (fixed, read_fixed) = at_poll(None);
    assert!(
        bits_contains(&fixed, colour(k)),
        "k is kept at the poll: {:?}",
        bits_list(&fixed)
    );
    assert!(
        bits_contains(&fixed, colour(src)),
        "so is the φ's edge input"
    );
    assert_ne!(colour(phi), colour(src), "a φ is pinned");
    assert!(
        !bits_contains(&fixed, colour(phi)),
        "the φ's own home is not read by the exit"
    );
    assert!(read_fixed, "the φs' pc marks the snapshot read");
}

/// r21 keepset, the colouring half: `k`, named only by the header's chain
/// snapshot, keeps its word up to the latch's edge, where the exit reads
/// it. With a header φ at the snapshot's pc that was already so, and the
/// latch read changes no colour and no range (`=0` is byte-identical
/// here); only a φ-less spliced loop gains the read.
#[test]
fn r21_keepset_the_colouring_counts_the_latch_exit_and_is_unchanged_with_a_header_phi() {
    let (graph, schedule, k, phi, merge) = r21_keepset_fixture(false);
    let (latch, _src) = r21_keepset_latch(&graph, &schedule, phi, merge);
    let flag = concat!("CRATON", "VM_JIT_IR_SPLICED_LATCH_EXIT_READS");
    let on = cratonvm_types::flags::with_thread_overrides(&[(flag, None)], || {
        plan_slots(&graph, &schedule, None)
    });
    let off = cratonvm_types::flags::with_thread_overrides(&[(flag, Some("0"))], || {
        plan_slots(&graph, &schedule, None)
    });
    verify_slot_colouring(&graph, &schedule, &on).expect("a sound colouring");
    assert_eq!(on.node_color, off.node_color);
    assert_eq!(on.colouring_ranges(), off.colouring_ranges());
    let edges = block_edge_positions(&schedule, &on.pos_of).expect("the plan's numbering");
    let widened = on.colouring_ranges()[k as usize].expect("k has a colouring range");
    assert!(
        widened.hi >= edges[latch],
        "k's word is held to the latch edge [{}, {}] / {}",
        widened.lo,
        widened.hi,
        edges[latch]
    );
}

/// r21 keepset (residual 1): a post-call chain snapshot is read by a step
/// right after its call, so it counts as read and leaves the
/// bytecode-reachability union; `CRATONVM_JIT_IR_KEEP_SET_MODELLED_CHAIN_READS=0`
/// leaves it unread (in the union), as before.
#[test]
fn r21_keepset_a_post_call_chain_snapshot_is_read_after_its_call() {
    let (graph, schedule, k, _phi, _merge) = r21_keepset_fixture(true);
    assert_eq!(graph.safepoints.len(), 2);
    let plan = plan_slots(&graph, &schedule, None);
    let words = plan.slots.div_ceil(64).max(1);
    let colour = plan.node_color[k as usize].expect("a coloured value") as usize;
    let sp_bits = vec![bits_of(words, &[colour]), bits_of(words, &[colour])];
    let spliced = [(0usize, R21_KEEPSET_LOOP.len(), 1000usize)];
    let flag = concat!("CRATON", "VM_JIT_IR_KEEP_SET_MODELLED_CHAIN_READS");
    let read = |v: Option<&str>| {
        cratonvm_types::flags::with_thread_overrides(&[(flag, v)], || {
            snapshot_colours_live_after_each_call(
                &graph,
                &schedule,
                &plan,
                &sp_bits,
                &spliced,
                words,
                &[],
                false,
                None,
                None,
            )
            .expect("converges")
            .1[1]
        })
    };
    assert!(read(None), "modelled: the post-call exit's read is a step");
}

/// gci i1/fr (`gengc-r5w5-oomjit9-...` item 1): a call always arms the
/// dead-reference facts; an allocation only under
/// `CRATONVM_JIT_IR_ALLOC_SITE_CLEARS`; nothing else ever.

/// gci i1/fr: an armed allocation step records the colours live AT it --
/// its own frame state's reads and what runs after it -- without its own
/// result, which is written only when the allocation returns, and without
/// a value that died before it.
#[test]
fn gci_i1fr_an_allocation_site_keeps_what_runs_after_it_but_not_its_own_result() {
    let step = |reads: &[usize], kill: Option<usize>, node: NodeId, is_call: bool| LiveStep {
        reads: bits_of(1, reads),
        kill,
        node,
        is_call,
    };
    // n10 defines c0, which nothing reads again; n11 (the allocation)
    // defines c1 and its state names c2; n12 reads c1 and c2.
    let steps = vec![vec![
        step(&[], Some(0), 10, false),
        step(&[2], Some(1), 11, true),
        step(&[1, 2], None, 12, false),
    ]];
    let after = live_colours_after_calls(&steps, &[vec![]], 1).expect("converges");
    assert_eq!(after.get(&11), Some(&bits_of(1, &[2])));
    assert_eq!(after.get(&10), None, "an unarmed step records nothing");
}

/// gci i3/ir (`gci-i2sp-ir-callee-map-names-its-nulled-parameter-home`):
/// a reference parameter's prologue word is a zeroing candidate only for
/// index >= 1, the sole `Op::Param` of its index, scheduled in block 0,
/// which must be the entry block and entered from nowhere else.

/// gci i3/ir: the store itself -- one `MOV qword [rbp - home], 0` after
/// the copy of a planned parameter, and nothing for a node the plan did
/// not admit, for a copy that IS the prologue word, or for a dropped
/// copy store.
#[test]
fn gci_i3ir_a_planned_ref_param_home_is_zeroed_after_its_copy_and_only_then() {
    let mut lo = super::tests::lowerer_with_resident_xmm(16384, None);
    let id: NodeId = 0;
    let home = 16i32;
    assert!(home < lo.first_spill, "fixture: a prologue word");
    let copy = lo.first_spill;
    let at = lo.buf.pos();
    lo.zero_ref_param_home_after_copy(id, copy, home);
    assert_eq!(lo.buf.pos(), at, "not planned: nothing");
    lo.ref_param_home_zero = vec![id];
    lo.zero_ref_param_home_after_copy(id, home, home);
    assert_eq!(lo.buf.pos(), at, "the copy is the word itself: nothing");
    lo.home_dropped = vec![true];
    lo.zero_ref_param_home_after_copy(id, copy, home);
    assert_eq!(lo.buf.pos(), at, "a dropped copy store: nothing");
    lo.home_dropped = Vec::new();
    lo.zero_ref_param_home_after_copy(id, copy, home);
    let mut want = vec![0x48, 0xC7, 0x85];
    want.extend_from_slice(&(-home).to_le_bytes());
    want.extend_from_slice(&0i32.to_le_bytes());
    assert_eq!(&lo.buf.as_slice()[at..lo.buf.pos()], want.as_slice());
}

/// gci i4/ir (vi3-ir A VOID): on a REAL schedule `Op::Start` is a control
/// node with no block, so i3's `node_to_block[graph.entry]` lookup answered
/// `usize::MAX` and the parameter plan admitted nothing. The entry block
/// is found by its head, `Proj(0)` of Start, and the plan then admits the
/// parameter the call reads.
#[test]
fn gci_i4ir_the_entry_block_is_found_by_its_head_and_admits_the_parameter() {
    let mut g = Graph::empty(8);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![ctrl, mem, a],
        Some(4),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl], Some(7));
    let schedule = crate::ir_schedule::schedule(&g);
    assert!(
        schedule
            .node_to_block
            .get(start as usize)
            .is_none_or(|&b| b >= schedule.blocks.len()),
        "the premise: Start itself has no block"
    );
    assert_eq!(scheduled_entry_block(&g, &schedule), Some(0));
    assert_eq!(schedule.node_to_block.get(a as usize).copied(), Some(0));
    assert!(schedule.blocks[0].predecessors.is_empty());
    assert_eq!(
        ref_param_home_zero_candidates(&g, &schedule.node_to_block, 0, false, false),
        vec![a]
    );
    // A hand-built schedule that did place Start is taken as it is.
    let mut placed = crate::ir_schedule::schedule(&g);
    placed.node_to_block[start as usize] = 0;
    assert_eq!(scheduled_entry_block(&g, &placed), Some(0));
}

/// gci i4/ir (`gci-i3ir-unarmed-optimizing-gc-points-keep-dead-homes`
/// item 1): a control-pinned non-call GC point with its own bci arms the
/// dead-reference facts only under `CRATONVM_JIT_IR_OTHER_GC_POINT_CLEARS`;
/// an unpinned or bci-less one never does; a call always does.

/// r23 wave 2d, lane L8: `CRATONVM_JIT_IR_DIRECT_STATIC_UNARMED` is default
/// ON with `=0` as its kill.

/// gci i5/ir (`gci-i3ir-unarmed-optimizing-gc-points-keep-dead-homes`
/// item 2): a counted loop's back-edge poll has a header (a dominating
/// merge with a bci), a synthetic site id no node has, and the plan's own
/// edge positions; both keep-set builders record an after-set under that id
/// only under `CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS`; a numbering that
/// disagrees with the plan's arms nothing.
#[test]
fn gci_i5ir_a_back_edge_poll_is_an_armed_site_only_under_its_switch() {
    // int sum(int n){ int s=0; for(int i=0;i<n;i++) s+=i; return s; }
    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02,
        0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
    ];
    let graph = crate::ir::IrBuilder::new(1, 3)
        .build(&code, 21)
        .expect("IR build");
    let schedule = crate::ir_schedule::schedule(&graph);
    let plan = plan_slots(&graph, &schedule, None);

    let edges = block_edge_positions(&schedule, &plan.pos_of).expect("the plan's numbering");
    assert_eq!(edges.len(), schedule.blocks.len());
    for (b, block) in schedule.blocks.iter().enumerate() {
        if let Some(last) = block.nodes.iter().copied().chain(block.terminator).last() {
            assert_eq!(
                plan.pos_of[last as usize].map(|p| p + 1),
                Some(edges[b]),
                "block {b}: one past its last instruction"
            );
        }
    }
    let scheduled = schedule
        .blocks
        .iter()
        .flat_map(|blk| blk.nodes.iter().copied().chain(blk.terminator))
        .next()
        .expect("a scheduled node");
    let mut bad = plan.pos_of.clone();
    bad[scheduled as usize] = Some(usize::MAX);
    assert!(
        block_edge_positions(&schedule, &bad).is_none(),
        "a disagreement arms nothing"
    );

    // The latch: a block with a successor at or before it that is a
    // dominating header with a bci. Not merely the first block with an
    // `s <= b` successor: the layout may put the loop's exit block first
    // (t5a), which makes that forward edge look backward.
    let latch = (0..schedule.blocks.len())
        .find(|&b| back_edge_poll_header(&graph, &schedule, b).is_some())
        .expect("the counted loop has a dominated back edge to a header with a bci");
    let (header, _pc) = back_edge_poll_header(&graph, &schedule, latch).expect("found above");
    assert!(header <= latch);
    assert!(schedule.dom.dominates(header, latch), "a true back edge");
    for b in 0..schedule.blocks.len() {
        if !schedule.blocks[b].successors.iter().any(|&s| s <= b) {
            assert!(
                back_edge_poll_header(&graph, &schedule, b).is_none(),
                "block {b}: no poll"
            );
        }
    }
    let key = poll_site_key(&graph, latch).expect("fits");
    assert!(key as usize >= graph.nodes.len(), "names no node");
    assert_eq!(key as usize, graph.nodes.len() + latch);

    let flag = concat!("CRATON", "VM_JIT_IR_POLL_GC_POINT_CLEARS");
    let builders = |v: Option<&str>| {
        let edits = [(flag, v)];
        cratonvm_types::flags::with_thread_overrides(&edits, || {
            let named: HashMap<usize, Vec<u64>> = HashMap::new();
            let union = snapshot_colours_after_each_call(&graph, &schedule, &named, &[], &[], 1)
                .expect("converges");
            let (precise, _) = snapshot_colours_live_after_each_call(
                &graph,
                &schedule,
                &plan,
                &[],
                &[],
                1,
                &[],
                false,
                None,
                None,
            )
            .expect("converges");
            (
                ir_poll_gc_point_clears_enabled(),
                union.contains_key(&key),
                precise.contains_key(&key),
            )
        })
    };
    assert_eq!(
        builders(None),
        (false, false, false),
        "default OFF: no poll after-set"
    );
}

/// gci i5/ir (`CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO_LAZY`): a candidate's
/// prologue word rides exactly the planned clears of its own home that lie
/// after its copy; one planned clear that does not (or cannot be placed)
/// keeps the eager store; no planned clear at all needs no store.

/// gci i4/ir (i4/so open item 1): the published XMM save range `[lo, hi)`
/// holds every word `saved_xmm_regs`' `MOVUPS` writes (`spill_cap + 8 ..=
/// spill_cap + xmm`), so the top one is no longer admitted to the GPR-image
/// remap, and no GPR save word; `lo` stays on the last spill colour.
#[test]
fn gci_i4ir_the_published_xmm_save_range_covers_each_saved_word_and_no_gpr_save() {
    let _ls = LsForce::on();
    let cm = super::r16w7_frames_tests::lower_fib(&[]);
    let l = &cm.frame_layout;
    let xmm = ir_saved_xmm_bytes();
    if xmm == 0 || l.xmm_saved_hi <= l.xmm_saved_lo {
        return;
    }
    let cap = l.spill_hi;
    assert_eq!(l.xmm_saved_lo, cap, "lo stays on the last spill colour");
    assert_eq!(l.xmm_saved_hi, cap + xmm + 8, "one word past the top save");
    let mut off = cap + 8;
    while off <= cap + xmm {
        assert!(
            off >= l.xmm_saved_lo && off < l.xmm_saved_hi,
            "save word {off} inside"
        );
        off += 8;
    }
    for &(_, o) in cm.ir_saved_gpr_slots.clone().unwrap_or_default().iter() {
        assert!(o >= l.xmm_saved_hi, "GPR save {o} outside the XMM range");
    }
}

/// A colour is dead at `pos` only when no range of its covers `pos` and no
/// reachable snapshot names it.
#[test]
fn a_colour_is_dead_only_outside_its_ranges_and_unnamed() {
    let ranges = vec![
        // 0: live over [2, 5], then again over [9, 12] (recycled).
        vec![LiveRange { lo: 2, hi: 5 }, LiveRange { lo: 9, hi: 12 }],
        // 1: live over [0, 3] only.
        vec![LiveRange { lo: 0, hi: 3 }],
        // 2: live over [0, 20].
        vec![LiveRange { lo: 0, hi: 20 }],
    ];
    let colours = [0u32, 1, 2];
    let none = bits_of(1, &[]);
    assert_eq!(
        dead_ref_colours_at(&ranges, &colours, &none, 2),
        Vec::<u32>::new()
    );
    assert_eq!(dead_ref_colours_at(&ranges, &colours, &none, 4), vec![1]);
    assert_eq!(dead_ref_colours_at(&ranges, &colours, &none, 7), vec![0, 1]);
    assert_eq!(dead_ref_colours_at(&ranges, &colours, &none, 9), vec![1]);
    // Endpoints are inclusive, like the colouring's own overlap test.
    assert_eq!(dead_ref_colours_at(&ranges, &colours, &none, 5), vec![1]);
    assert_eq!(
        dead_ref_colours_at(&ranges, &colours, &none, 13),
        vec![0, 1]
    );
    // A snapshot naming colour 1 keeps it.
    let named_1 = bits_of(1, &[1]);
    assert_eq!(dead_ref_colours_at(&ranges, &colours, &named_1, 7), vec![0]);
    // A colour with no recorded range is never called dead.
    let short = vec![vec![LiveRange { lo: 0, hi: 1 }]];
    assert_eq!(dead_ref_colours_at(&short, &[0, 1], &none, 5), vec![0]);
}

/// `0 aconst_null; 1 astore_1; 2 iinc 2 1; 5 goto 2`: from the loop body
/// (bci 2) the prefix at bci 0 and 1 is unreachable, so a value only a
/// snapshot there names is not kept; everything in the loop is.
#[test]
fn only_snapshots_at_reachable_bcis_keep_a_colour() {
    let code: Vec<u8> = vec![0x01, 0x4c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xfd];
    let verified = cratonvm_reader::verified_code(&code).expect("the loop decodes");
    let mut named: std::collections::HashMap<usize, Vec<u64>> = std::collections::HashMap::new();
    named.insert(0, bits_of(1, &[3]));
    named.insert(1, bits_of(1, &[4]));
    named.insert(5, bits_of(1, &[6]));
    let from_body = snapshot_colours_reachable_from(&verified, &named, 1, 2);
    assert_eq!(from_body, bits_of(1, &[6]));
    let from_entry = snapshot_colours_reachable_from(&verified, &named, 1, 0);
    assert_eq!(from_entry, bits_of(1, &[3, 4, 6]));
}

/// gen r5w5/oomjit9: a combined-buffer pc resolves to the outermost own
/// invoke, through nested splices; an own pc is itself; a cyclic table
/// stops (at a combined pc, which callers then treat as no instruction).
#[test]
fn a_spliced_pc_resolves_to_its_outermost_own_invoke() {
    // Own code is [0, 20). Splice A at [100, 140) was inlined at own
    // invoke 7; splice B at [200, 230) at pc 120, inside A.
    let ranges = [(100, 140, 7), (200, 230, 120)];
    assert_eq!(resolve_spliced_pc(&ranges, 5), 5, "an own pc is itself");
    assert_eq!(resolve_spliced_pc(&ranges, 110), 7);
    assert_eq!(resolve_spliced_pc(&ranges, 210), 7, "B -> 120 in A -> 7");
    assert_eq!(
        resolve_spliced_pc(&ranges, 140),
        140,
        "ranges are half-open"
    );
    // A malformed table that points into itself terminates.
    let cyclic = [(100, 140, 210), (200, 230, 120)];
    let r = resolve_spliced_pc(&cyclic, 110);
    assert!(r >= 100, "a cycle never resolves into own code, got {r}");
}

/// gen r5w5/oomjit9: the frame states of a splice are kept from every call
/// that can reach its enclosing own invoke, and from nowhere else.
#[test]
fn a_splices_frame_states_are_kept_where_its_invoke_is_reachable() {
    // `0 aconst_null; 1 astore_1; 2 iinc 2 1; 5 goto 2` again: pretend a
    // splice was inlined at the own pc 1 (unreachable from the loop) and
    // another at pc 5 (in the loop).
    let code: Vec<u8> = vec![0x01, 0x4c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xfd];
    let verified = cratonvm_reader::verified_code(&code).expect("the loop decodes");
    let named: std::collections::HashMap<usize, Vec<u64>> = std::collections::HashMap::new();
    let mut splice_named: std::collections::HashMap<usize, Vec<u64>> =
        std::collections::HashMap::new();
    splice_named.insert(1, bits_of(1, &[8]));
    splice_named.insert(5, bits_of(1, &[9]));
    let from_body = snapshot_colours_reachable_with_splices(&verified, &named, &splice_named, 1, 2);
    assert_eq!(from_body, bits_of(1, &[9]));
    let from_entry =
        snapshot_colours_reachable_with_splices(&verified, &named, &splice_named, 1, 0);
    assert_eq!(from_entry, bits_of(1, &[8, 9]));
}

/// gen r5w5/oomjit9: a dead colour is zeroed at the first site after its
/// definition and not again until something redefines it; a staging word
/// a call wrote is zeroed at the next site; the entry starts clean and a
/// predecessor-less non-entry block starts with everything stale.
#[test]
fn a_dead_home_is_zeroed_once_per_definition() {
    use StaleEvent::{Def, Site, Staged};
    let all = bits_of(1, &[0, 1, 2]);
    // Block 0 (entry): def c0, def c1, site A (c0 dead), stage arg 0,
    // site B (c0, c1 dead), def c2 -> block 1.
    // Block 1: site C (c0, c1, c2 dead) -> back to block 1 (a loop).
    let blocks = vec![
        StaleBlock {
            events: vec![
                Def(0),
                Def(1),
                Site {
                    node: 10,
                    dead: bits_of(1, &[0]),
                    cleans: true,
                },
                Staged(0),
                Site {
                    node: 11,
                    dead: bits_of(1, &[0, 1]),
                    cleans: true,
                },
                Def(2),
            ],
            predecessors: vec![],
        },
        StaleBlock {
            events: vec![Site {
                node: 12,
                dead: bits_of(1, &[0, 1, 2]),
                cleans: true,
            }],
            predecessors: vec![0, 1],
        },
    ];
    let at = stale_sets_at_sites(&blocks, 0, &all, 1).expect("converges");
    let by_node = |n: NodeId| at.iter().find(|(node, _, _)| *node == n).cloned();
    let (_, homes, stage) = by_node(10).expect("site A");
    assert_eq!(homes, bits_of(1, &[0]), "c0 written, dead here");
    assert_eq!(stage, bits_of(1, &[]), "nothing staged yet");
    let (_, homes, stage) = by_node(11).expect("site B");
    assert_eq!(homes, bits_of(1, &[1]), "c0 already zeroed at A");
    assert_eq!(stage, bits_of(1, &[0]), "the call before staged word 0");
    let (_, homes, stage) = by_node(12).expect("site C");
    assert_eq!(homes, bits_of(1, &[2]), "only c2 was written since");
    assert_eq!(stage, bits_of(1, &[]));

    // The same block 1 with no predecessor and not the entry: everything
    // is stale at its first site.
    let orphan = vec![
        StaleBlock {
            events: vec![],
            predecessors: vec![],
        },
        StaleBlock {
            events: vec![Site {
                node: 20,
                dead: bits_of(1, &[1, 2]),
                cleans: true,
            }],
            predecessors: vec![],
        },
    ];
    let at = stale_sets_at_sites(&orphan, 0, &all, 1).expect("converges");
    assert_eq!(at.len(), 1);
    assert_eq!(at[0].1, bits_of(1, &[1, 2]));
    assert_eq!(at[0].2, vec![!0u64]);
}

/// hga: a site whose call may take an inline prefix zeroes on its call
/// path only, so it leaves its stale homes and staging words stale for the
/// next site: a loop of a prefixed site (the cached `Integer.valueOf`) and
/// a real call clears at the real call what the prefixed one would have.
#[test]
fn hga_a_prefixed_site_leaves_its_homes_stale_for_the_next_site() {
    use StaleEvent::{Def, Site, Staged};
    let all = bits_of(1, &[0, 1]);
    // Block 0 (entry) -> 1. Block 1 (loop): def c0, def c1, prefixed
    // site P (c0, c1 dead), stage arg 0, real site R (c0, c1 dead) -> 1.
    let loop_with = |cleans: bool| {
        vec![
            StaleBlock {
                events: vec![],
                predecessors: vec![],
            },
            StaleBlock {
                events: vec![
                    Def(0),
                    Def(1),
                    Site {
                        node: 30,
                        dead: bits_of(1, &[0, 1]),
                        cleans,
                    },
                    Staged(0),
                    Site {
                        node: 31,
                        dead: bits_of(1, &[0, 1]),
                        cleans: true,
                    },
                ],
                predecessors: vec![0, 1],
            },
        ]
    };
    let at = stale_sets_at_sites(&loop_with(false), 0, &all, 1).expect("converges");
    let by_node = |at: &[(NodeId, Vec<u64>, Vec<u64>)], n: NodeId| {
        at.iter()
            .find(|(node, _, _)| *node == n)
            .cloned()
            .expect("site")
    };
    let (_, homes, stage) = by_node(&at, 30);
    assert_eq!(
        homes,
        bits_of(1, &[0, 1]),
        "the prefixed site still zeroes on its call path"
    );
    assert_eq!(
        stage,
        bits_of(1, &[]),
        "R zeroed the staging word last round"
    );
    let (_, homes, stage) = by_node(&at, 31);
    assert_eq!(homes, bits_of(1, &[0, 1]), "a prefix hit zeroed nothing");
    assert_eq!(stage, bits_of(1, &[0]));
    // The cleaning form (the kill switch): R finds them already zero.
    let at = stale_sets_at_sites(&loop_with(true), 0, &all, 1).expect("converges");
    let (_, homes, _) = by_node(&at, 31);
    assert_eq!(homes, bits_of(1, &[]));
}

/// JIT round 20 wave 17, lane gcDead (I-9): the allocation ops are
/// dead-home sites (and get keep-set entries); nothing else new is.
#[test]
fn i9_allocations_are_dead_home_sites() {
    assert!(is_dead_home_site_op(&Op::Call { info_ptr: 0 }));
    assert!(is_dead_home_site_op(&Op::New {
        class_id: 1,
        num_fields: 0,
    }));
    assert!(is_dead_home_site_op(&Op::NewArray {
        element_type: 8,
        component_class_id: 0,
    }));
    // JIT round 21, lane oom: the remaining helper GC points.
    assert!(is_dead_home_site_op(&Op::NewCp {
        holder_class_id: 1,
        cp_idx: 1,
    }));
    assert!(is_dead_home_site_op(&Op::MonitorEnter));
    assert!(!is_dead_home_site_op(&Op::MonitorExit));
    assert!(!is_dead_home_site_op(&Op::Neg));
    // Every site but a call keeps the plain union (no def kills).
    assert!(!is_plain_union_site_op(&Op::Call { info_ptr: 0 }));
    assert!(is_plain_union_site_op(&Op::New {
        class_id: 1,
        num_fields: 0,
    }));
    assert!(is_plain_union_site_op(&Op::MonitorEnter));
    assert!(!is_plain_union_site_op(&Op::Neg));
}

/// I-9: `paramBox`'s shape. An allocation step keeps its own frame state
/// (what an `OutOfMemoryError` / `NegativeArraySizeException` exit of its
/// helper resumes or rethrows with) and everything read after it, and
/// NOT a loaded value whose last reader ran before it -- `p`, read by
/// `p.length` and then by nothing -- nor its own result's colour.
#[test]
fn i9_an_allocation_site_keeps_its_frame_state_not_a_value_read_before_it() {
    let step = |reads: &[usize], kill: Option<usize>, node: NodeId, is_call: bool| LiveStep {
        reads: bits_of(1, reads),
        kill,
        node,
        is_call,
    };
    // One block: n10 defines c1 (`p = box.a`); n11's frame state names
    // c1 and c0 (the box); n12 is the allocation (defines c2), its frame
    // state names c0 only (bytecode liveness dropped `p`); n13 reads c2
    // and c4 through its frame state.
    let steps = vec![vec![
        step(&[0], Some(1), 10, false),
        step(&[0, 1], None, 11, false),
        step(&[0], Some(2), 12, true),
        step(&[2, 4], None, 13, false),
    ]];
    let after = live_colours_after_calls(&steps, &[vec![]], 1).expect("converges");
    let at_alloc = after.get(&12).expect("the allocation has an entry");
    assert_eq!(
        at_alloc,
        &bits_of(1, &[0, 4]),
        "its frame state and later readers, not c1 or c2"
    );
}

/// I-9: at an allocation site the reachability union takes no def kills.
/// A colour defined at bci 2 and named by a snapshot only behind it (bci
/// 5) is dead at a call at bci 1 (the call orders the load after it) but
/// kept at an allocation there (no memory token: the load may run above
/// the allocation and not again before the snapshot reads it).
#[test]
fn i9_an_allocation_site_keeps_the_union_without_def_kills() {
    // `0 aconst_null; 1 astore_1; 2 iinc 2 1; 5 goto 2`.
    let code: Vec<u8> = vec![0x01, 0x4c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xfd];
    let verified = cratonvm_reader::verified_code(&code).expect("the loop decodes");
    let mut named: std::collections::HashMap<usize, Vec<u64>> = std::collections::HashMap::new();
    named.insert(5, bits_of(1, &[3]));
    let mut after_kept: std::collections::HashMap<NodeId, Vec<u64>> =
        std::collections::HashMap::new();
    after_kept.insert(40, bits_of(1, &[]));
    after_kept.insert(41, bits_of(1, &[]));
    let mut def_pc: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    def_pc.insert(3, 2);
    let mut plan = DeadRefSlotClear {
        ranges: vec![Vec::new(); 4],
        ref_colours: vec![3],
        words: 1,
        named,
        verified,
        reach_memo: std::collections::HashMap::new(),
        splice_named: std::collections::HashMap::new(),
        always_kept: bits_of(1, &[]),
        after_kept,
        def_pc,
        phi_def_pc: std::collections::HashMap::new(),
        alloc_sites: [41].into_iter().collect(),
        plain_reach_memo: std::collections::HashMap::new(),
        chain_only_pcs: std::collections::HashSet::new(),
        site_result: std::collections::HashMap::new(),
    };
    assert_eq!(
        plan.dead_colours_at(0, 1, 40),
        vec![3],
        "a call site kills it"
    );
    assert_eq!(
        plan.dead_colours_at(0, 1, 41),
        Vec::<u32>::new(),
        "an allocation keeps it"
    );
    // Both memos answer again the same way (no cross-talk at one bci).
    assert_eq!(plan.dead_colours_at(0, 1, 40), vec![3]);
    assert_eq!(plan.dead_colours_at(0, 1, 41), Vec::<u32>::new());
}

/// gen r5w6/oomjit10: a value a loop recomputes is dead at the loop's
/// call when every path from the call to the frame state naming it runs
/// through its definition first; a value defined before the loop stays
/// live; the call's own result is dead at the call. Without the kills the
/// recomputed value is live -- the gen r5w5 union's answer, which kept
/// `cleared(ref)`'s `ref.get()` result across its `System.gc()`.
#[test]
fn a_recomputed_value_is_dead_at_the_call_before_its_next_definition() {
    let step = |reads: &[usize], kill: Option<usize>, node: NodeId, is_call: bool| LiveStep {
        reads: bits_of(1, reads),
        kill,
        node,
        is_call,
    };
    // Block 0 (entry): defines c0 (loop-invariant) -> 1.
    // Block 1 (header): defines c1 (node 11), then a branch whose frame
    //   state names c0 and c1 (node 12) -> 2 (body), 3 (exit).
    // Block 2 (body): the call (node 20, defines c2) -> 1.
    // Block 3 (exit): a node whose frame state names c0.
    let steps = vec![
        vec![step(&[], Some(0), 10, false)],
        vec![
            step(&[], Some(1), 11, false),
            step(&[0, 1], None, 12, false),
        ],
        vec![step(&[], Some(2), 20, true)],
        vec![step(&[0], None, 30, false)],
    ];
    let successors = vec![vec![1], vec![2, 3], vec![1], vec![]];
    let after = live_colours_after_calls(&steps, &successors, 1).expect("converges");
    assert_eq!(
        after.get(&20),
        Some(&bits_of(1, &[0])),
        "c1 is redefined before its reader and c2 is the call's own result"
    );
    let no_kills: Vec<Vec<LiveStep>> = steps
        .iter()
        .map(|row| {
            row.iter()
                .map(|s| LiveStep {
                    kill: None,
                    ..s.clone()
                })
                .collect()
        })
        .collect();
    let after = live_colours_after_calls(&no_kills, &successors, 1).expect("converges");
    assert_eq!(after.get(&20), Some(&bits_of(1, &[0, 1])));
}

/// gen r5w6/oomjit10: a call sees its own frame state and whatever a
/// LATER node of its own block reads before a redefinition.
#[test]
fn a_call_keeps_its_own_state_and_a_later_read_in_its_block() {
    let step = |reads: &[usize], kill: Option<usize>, node: NodeId, is_call: bool| LiveStep {
        reads: bits_of(1, reads),
        kill,
        node,
        is_call,
    };
    // One block: call A reads c3; node 2 redefines c4; node 3 reads c4
    // and c5; call B (node 4) reads nothing.
    let steps = vec![vec![
        step(&[3], None, 1, true),
        step(&[], Some(4), 2, false),
        step(&[4, 5], None, 3, false),
        step(&[], None, 4, true),
    ]];
    let after = live_colours_after_calls(&steps, &[vec![]], 1).expect("converges");
    assert_eq!(
        after.get(&1),
        Some(&bits_of(1, &[3, 5])),
        "c4 is redefined first"
    );
    assert_eq!(
        after.get(&4),
        Some(&bits_of(1, &[])),
        "nothing is read after B"
    );
}

/// gen r5w5/oomjit9: `bits_or` is a word-wise union over the shorter
/// operand.
#[test]
fn bits_or_unions_word_by_word() {
    let mut a = bits_of(2, &[1, 70]);
    bits_or(&mut a, &bits_of(2, &[2, 70, 127]));
    assert_eq!(a, bits_of(2, &[1, 2, 70, 127]));
    let mut short = bits_of(1, &[3]);
    bits_or(&mut short, &bits_of(2, &[4, 100]));
    assert_eq!(short, bits_of(1, &[3, 4]));
}
