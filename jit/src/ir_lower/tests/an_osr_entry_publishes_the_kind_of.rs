#[allow(unused_imports)]
use super::*;

/// Interpreter round i1 wave 10: every optimizing OSR entry publishes the
/// kind each seeded local is read as, and `ir_osr_entry_kind_mismatch`
/// refuses an offer whose tags disagree. The fixture is the loop above:
/// `n` (int, 0), `a` (double, 1-2), `s` (int, 3), `i` (int, 4).
#[test]
pub(in crate::ir_lower::tests) fn an_osr_entry_publishes_the_kind_of_every_local_it_seeds() {
    use cratonvm_types::{VTAG_DOUBLE, VTAG_INT, VTAG_NULL, VTAG_OBJECT, VTAG_UNINIT};
    let _osr = OsrEntryForce::on();
    let code = [
        0x1a, 0x87, 0x48, 0x03, 0x3e, 0x03, 0x36, 0x04, 0x15, 0x04, 0x1a, 0xa2, 0x00, 0x1a, 0x1d,
        0x27, 0x15, 0x04, 0x87, 0x63, 0x8e, 0x60, 0x3e, 0x1d, 0x27, 0x1d, 0x87, 0x63, 0x8e, 0x60,
        0x3e, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xe6, 0x1d, 0xac, 0, 0,
    ];
    let cm = compile_via_ir(&code, 39, 1, 5).expect("the FP-invariant loop compiles via IR");
    assert_eq!(
        cm.ir_osr_entries.len(),
        cm.ir_osr_entry_kinds.len(),
        "one kind row per entry"
    );
    for &(bci, _, needed) in &cm.ir_osr_entries {
        let row = cm
            .ir_osr_entry_kinds
            .iter()
            .find(|(b, _)| *b == bci)
            .map(|(_, k)| k);
        assert_eq!(row.map(|k| k.len()), Some(needed as usize), "bci {bci}");
    }
    let Some((_addr, needed)) = cm.ir_osr_entry_addr(8) else {
        return; // the header was refused; the rows above are the claim
    };
    let (_, kinds) = cm
        .ir_osr_entry_kinds
        .iter()
        .find(|(b, _)| *b == 8)
        .expect("an entry at 8 has a kind row");
    // `a` is a double read from local 1, and local 2 is its reserved half.
    if needed >= 3 && kinds[1] != 0 {
        assert_eq!(kinds[1], b'D');
        assert_eq!(kinds[2], 0, "the high half of a double is not seeded");
    }
    if needed >= 5 && kinds[4] != 0 {
        assert_eq!(kinds[4], b'I');
    }
    let good = [VTAG_INT, VTAG_DOUBLE, VTAG_UNINIT, VTAG_INT, VTAG_INT];
    assert_eq!(
        cm.ir_osr_entry_kind_mismatch(8, |i| good.get(i).copied()),
        None
    );
    // Any seeded local offered as a reference is refused, at that local.
    for (i, &kind) in kinds.iter().enumerate() {
        if kind == 0 || kind == b'L' {
            continue;
        }
        let mut bad = good;
        bad[i] = if i % 2 == 0 { VTAG_OBJECT } else { VTAG_NULL };
        assert_eq!(
            cm.ir_osr_entry_kind_mismatch(8, |j| bad.get(j).copied()),
            Some(i),
            "local {i} ({}) offered as a reference",
            kind as char
        );
    }
    // A frame shorter than the row is refused rather than read past.
    if kinds.iter().any(|&k| k != 0) {
        assert!(cm.ir_osr_entry_kind_mismatch(8, |_| None).is_some());
    }
    // No row, no constraint.
    assert_eq!(cm.ir_osr_entry_kind_mismatch(9999, |_| None), None);
}

/// The kind vocabulary against the interpreter's tags.
#[test]
pub(in crate::ir_lower::tests) fn the_osr_slot_kinds_admit_exactly_their_own_tags() {
    use crate::ir::IrType;
    use cratonvm_types::{
        VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR,
        VTAG_UNINIT,
    };
    let admits = crate::ir_osr_slot_kind_admits;
    let kind = crate::ir_osr_slot_kind;
    assert!(admits(kind(IrType::Ref), VTAG_OBJECT));
    assert!(admits(kind(IrType::Ref), VTAG_NULL));
    assert!(!admits(kind(IrType::Ref), VTAG_INT));
    assert!(!admits(kind(IrType::Ref), VTAG_LONG));
    assert!(!admits(kind(IrType::Ref), VTAG_UNINIT));
    assert!(admits(kind(IrType::Int), VTAG_INT));
    assert!(!admits(kind(IrType::Int), VTAG_NULL));
    assert!(admits(kind(IrType::Long), VTAG_LONG));
    assert!(!admits(kind(IrType::Long), VTAG_DOUBLE));
    assert!(admits(kind(IrType::Float), VTAG_FLOAT));
    assert!(!admits(kind(IrType::Float), VTAG_INT));
    assert!(admits(kind(IrType::Double), VTAG_DOUBLE));
    assert!(!admits(kind(IrType::Double), VTAG_LONG));
    for tag in [VTAG_INT, VTAG_OBJECT, VTAG_UNINIT, VTAG_RETADDR] {
        assert!(admits(kind(IrType::Void), tag), "no constraint");
    }
    for k in *b"IJFDL" {
        assert!(!admits(k, VTAG_RETADDR));
    }
}

/// **Every shape the eligibility test excuses must have an emission arm.**
///
/// The mechanical half of `osr_entry_can_produce`'s doc comment. The
/// defect it was extracted from was not a wrong predicate -- excusing a
/// constant is right -- but two lists in two places that had to agree and
/// were only checked by hand. This checks them.
///
/// Textual, in the idiom of
/// `every_droppable_op_writes_its_home_once_through_store_rax` above: the
/// alternative is a runtime scan, and there is nothing to scan at runtime
/// -- the failure is a shape that never gets emitted, which is precisely
/// the case that produces no node to look at.
#[test]
pub(in crate::ir_lower::tests) fn an_osr_entry_emits_every_shape_its_eligibility_test_excuses() {
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    );
    // The FIRST occurrence of each needle is the code; the second is this
    // test quoting it.
    let excused_src = src
        .split("fn osr_entry_can_produce(op: &Op) -> bool {")
        .nth(1)
        .expect("`osr_entry_can_produce` is in this file")
        .split("\n    }")
        .next()
        .expect("the function ends");
    let mut excused = std::collections::BTreeSet::new();
    collect_op_names(excused_src, &mut excused);
    assert!(
        !excused.is_empty(),
        "the predicate scan found nothing -- `osr_entry_can_produce` \
             changed shape and this test would now pass vacuously",
    );

    let emitted_src = src
        .split("let (bits, is_fp, is_double) = match self.graph.nodes.get(id as usize) {")
        .nth(1)
        .expect("the seed emission is in this file")
        .split("None => continue,")
        .next()
        .expect("the match ends");
    let mut emitted = std::collections::BTreeSet::new();
    collect_op_names(emitted_src, &mut emitted);

    assert_eq!(
        excused, emitted,
        "the OSR entry excuses {excused:?} from being seeded but emits \
             {emitted:?}. A shape excused and not emitted is a value the \
             entered body reads out of a frame word nothing wrote -- see \
             `an_osr_entry_seeds_the_constants_its_merge_arms_read`.",
    );
}

/// The page's own reproducer, at unit scale: `Min1.clamp` spliced into a
/// counted loop, entered through BOTH doors.
///
/// `int clamp(int v){ int r; if (v < 100) r = 100; else r = v; return r; }`
/// relocated to a combined-buffer offset, which is what
/// `ir-splice-branch` admits and what `SPLICED_MERGE_SEEN` used to bar from
/// the optimizing OSR door. The bar is off; this is what has to hold
/// without it.
///
/// The OSR half is the half that failed. Its two arms are LITERALS, so the
/// merge store reads them from their home words, and the entry stub jumps
/// past the block that writes those words — see
/// `an_osr_entry_seeds_the_constants_its_merge_arms_read` for the same
/// defect with nothing spliced at all.
pub(in crate::ir_lower::tests) fn splice_branch_fixture(
) -> (Vec<u8>, usize, HashMap<usize, crate::ir::IrInlineSite>) {
    // Caller: `int f(int n){ int s=0; for(int i=0;i<n;i++) s += clamp(i);
    //          return s; }`   locals: 0=n, 1=s, 2=i
    //  0: iconst_0   1: istore_1   2: iconst_0   3: istore_2
    //  4: iload_2    5: iload_0    6: if_icmpge 22        <- header, bci 4
    //  9: iload_1   10: iload_2   11: invokestatic #1   14: iadd
    // 15: istore_1  16: iinc 2,1  19: goto 4
    // 22: iload_1   23: ireturn
    let caller = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x10, 0x1b, 0x1c, 0xb8, 0x00, 0x01, 0x60,
        0x3c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xf1, 0x1b, 0xac,
    ];
    const CALLER_LEN: usize = 24;
    // Callee: locals 0=v, 1=r
    //  0: iload_0    1: sipush 100   4: if_icmpge 14
    //  7: sipush 100 10: istore_1   11: goto 16
    // 14: iload_0   15: istore_1
    // 16: iload_1   17: ireturn                       <- single trailing return
    let callee = [
        0x1a, 0x11, 0x00, 0x64, 0xa2, 0x00, 0x0a, 0x11, 0x00, 0x64, 0x3c, 0xa7, 0x00, 0x05, 0x1a,
        0x3c, 0x1b, 0xac,
    ];
    let mut combined = caller.to_vec();
    let base = combined.len();
    combined.extend_from_slice(&callee);
    // The two trailing sentinel bytes every method's code carries.
    combined.extend_from_slice(&[0, 0]);
    let mut sites = HashMap::new();
    sites.insert(
        11,
        crate::ir::IrInlineSite {
            base,
            code_len: callee.len(),
            num_args: 1,
            max_locals: 2,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.clamp:(I)I".to_string(),
            class_id: 0,
        },
    );
    (combined, CALLER_LEN, sites)
}

/// A callee whose loop ECJ rotated, spliced into its caller: built, optimized,
/// lowered and run. `f(n) = sumTo(n) + 1`, `sumTo(n) = 0 + 1 + … + (n-1)`.
#[test]
pub(in crate::ir_lower::tests) fn a_spliced_callee_with_a_rotated_loop_runs() {
    let caller = [0x1a, 0xb8, 0x00, 0x01, 0x04, 0x60, 0xac];
    // sumTo: 0: s = 0; 2: i = 0; 4: goto 14; 7: s += i; 11: iinc 2, 1;
    // 14: iload_2; 15: iload_0; 16: if_icmplt 7; 19: iload_1; 20: ireturn
    let callee = [
        0x03, 0x3c, 0x03, 0x3d, 0xa7, 0x00, 0x0a, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01, 0x1c,
        0x1a, 0xa1, 0xff, 0xf7, 0x1b, 0xac,
    ];
    let mut combined = caller.to_vec();
    let base = combined.len();
    combined.extend_from_slice(&callee);
    combined.extend_from_slice(&[0, 0]);
    let mut sites = HashMap::new();
    sites.insert(
        1,
        crate::ir::IrInlineSite {
            base,
            code_len: callee.len(),
            num_args: 1,
            max_locals: 3,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.sumTo:(I)I".to_string(),
            class_id: 0,
        },
    );
    let mut builder = IrBuilder::new(1, 1);
    builder.set_inline_sites(sites);
    let mut graph = builder
        .build(&combined, caller.len())
        .expect("a callee body with a rotated loop must splice");
    ir_optimize::optimize(&mut graph);
    let schedule = ir_schedule::schedule(&graph);
    let cm = lower(&graph, &schedule, 1, 1, &no_helpers()).expect("the graph must lower");
    // SAFETY: the body was compiled by this test for one `int` argument and runs on this thread with no helper calls.
    let f = |n: i64| unsafe { cm.try_call(&[n]).expect("test JIT call") };
    assert_eq!(f(0), 1, "an empty loop sums to zero");
    assert_eq!(f(1), 1);
    assert_eq!(f(5), 11, "0+1+2+3+4, plus one");
    assert_eq!(f(100), 4951);
}

#[test]
pub(in crate::ir_lower::tests) fn a_spliced_body_may_branch_and_its_phi_carries_a_combined_pc() {
    let (combined, caller_len, sites) = splice_branch_fixture();
    let mut builder = IrBuilder::new(1, 3);
    builder.set_inline_sites(sites);
    let graph = builder
        .build(&combined, caller_len)
        .expect("a spliced body with a forward branch must build");
    let spliced_phi = graph
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::Phi) && n.bytecode_pc.is_some_and(|pc| pc >= caller_len));
    assert!(
        spliced_phi,
        "the callee's if/else must produce a phi, and it must carry the \
             COMBINED-buffer pc every node in a relocated region carries",
    );
    // And no `Op::Call` survives at the spliced site: a body that was
    // spliced AND dispatched would run twice.
    assert!(
        !graph.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
        "the spliced call must be gone, not emitted alongside the body",
    );
}

/// A body the door cannot enter carries no stubs, and the escape hatch
/// puts them back.
///
/// # What this is protecting
///
/// `try_osr`'s admission is `ir_osr_entry_addr(pc).is_some() &&
/// ir_osr_sentinel_free` — BOTH — so every stub in a body that is not
/// sentinel-free is unreachable: bytes in the code cache, a `JMP` to
/// patch, a published `(bci, offset, needed)` triple, and a whole
/// `build_live_model` pass, for a door that will refuse the artifact on
/// the second conjunct. And it is the COMMON case, not a corner:
/// sentinel-free means no deopt stub and no call-exception stub, so any
/// method containing a call fails it.
///
/// The loop below is sentinel-free, so it is the control: it must still
/// carry entries under the production rule. Making a body that is NOT
/// sentinel-free is what `assert` on `ir_osr_sentinel_free` checks, so
/// this test cannot silently degrade into asserting nothing.
#[test]
pub(in crate::ir_lower::tests) fn a_body_the_door_cannot_enter_carries_no_osr_stubs() {
    // `for (i = 0; i < n; i++) s += i; return s;` — no call, no guard, so
    // no deopt stub and no call-exception stub: sentinel-free.
    let sentinel_free_loop = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x82, 0x3c, 0x84, 0x02,
        0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
    ];
    {
        let _on = OsrEntryForce::on_but_not_always();
        let cm = compile_via_ir(&sentinel_free_loop, 21, 1, 3).expect("loop compiles via IR");
        assert!(
            cm.ir_osr_sentinel_free,
            "the control body must be sentinel-free, or this test proves \
                 nothing about the skip"
        );
        assert!(
            !cm.ir_osr_entries.is_empty(),
            "a sentinel-free body still gets its entries — the skip is \
                 about the door's SECOND conjunct, not about turning OSR off"
        );
    }
    // And the escape hatch is what the twenty-odd older stub-shape tests
    // ride on, so it has to actually restore the old behaviour.
    {
        let _always = OsrEntryForce::on();
        let cm = compile_via_ir(&sentinel_free_loop, 21, 1, 3).expect("loop compiles via IR");
        assert!(!cm.ir_osr_entries.is_empty());
    }
}

/// The kill switch really removes them: with `CRATONVM_JIT_IR_OSR_ENTRY=0`
/// an artifact carries no entry stub at all.
///
/// Until 2026-09-05 this asserted the same thing about the DEFAULT, which
/// is what it was. The switch is on now, so the property worth pinning is
/// that turning it off still works — a default-on codegen change is only as
/// good as its way back.
#[test]
pub(in crate::ir_lower::tests) fn the_osr_entry_kill_switch_emits_no_stub() {
    let _off = OsrEntryForce::off();
    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02,
        0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
    ];
    let cm = compile_via_ir(&code, 21, 1, 3).expect("loop compiles via IR");
    assert!(
        cm.ir_osr_entries.is_empty(),
        "with the kill switch set an artifact must carry no entry stub",
    );
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { cm.try_call(&[10]).expect("call") }, 45);
}

/// Round 12 wave 1 (lane osr, irback proposal W18-2): an ordinary OSR
/// entry is planned only at a loop header. The door enters only at the
/// target of a taken backward branch, which here is bci 4 alone; a stub
/// at the loop exit (19) or any other block start is dead code the door
/// can never ask for.
#[test]
pub(in crate::ir_lower::tests) fn r12w1_ordinary_osr_entries_are_planned_only_at_loop_headers() {
    let _always = OsrEntryForce::on();
    // `for (i = 0; i < n; i++) s ^= i; return s;` -- header at 4.
    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x82, 0x3c, 0x84, 0x02,
        0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
    ];
    let cm = compile_via_ir(&code, 21, 1, 3).expect("loop compiles via IR");
    let bcis: Vec<u32> = cm.ir_osr_entries.iter().map(|&(b, _, _)| b).collect();
    assert!(
        !bcis.is_empty(),
        "the header lost its entry too: this fixture no longer asks anything",
    );
    assert!(
        bcis.iter().all(|&b| b == 4),
        "OSR entries at {bcis:?}; bci 4 is the only back-edge target",
    );
}

/// `Lowerer::first_safepoint_index_by_bci` gives, for every bci, the answer
/// of the by-bci scan it replaced: the FIRST snapshot at that bci. That
/// holds when several snapshots share a bci, as an unrolled body's copies
/// do, and for a bci with no snapshot at all.
#[test]
pub(in crate::ir_lower::tests) fn the_safepoint_index_names_the_first_snapshot_at_each_bci() {
    // iconst_0; istore_1; iload_1; ireturn
    let code = [0x03, 0x3c, 0x1b, 0xac, 0, 0];
    let mut graph = IrBuilder::new(1, 2).build(&code, 4).expect("builds");
    for bci in [2usize, 0, 2, 7, 3, 7] {
        graph.safepoints.push(crate::ir::SafepointSnapshot::new(
            bci,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
    }
    let index = Lowerer::first_safepoint_index_by_bci(&graph);
    for bci in 0..10usize {
        assert_eq!(
            index.get(&bci).copied(),
            graph.safepoints.iter().position(|s| s.bci == bci),
            "bci {bci}"
        );
    }
    let distinct: std::collections::HashSet<usize> =
        graph.safepoints.iter().map(|s| s.bci).collect();
    assert_eq!(index.len(), distinct.len(), "one entry per distinct bci");
}

// ── Bottom-tested (rotated) loops ────────────────────────────────────
//
// ECJ, Kotlin and Scala jump forward to a loop's test and branch back to
// its body. The IR builder refused every such method until it learned to
// walk one in reverse post-order. Each test compiles the shape through the
// whole IR pipeline and checks the answer the obvious Java gives. The byte
// layouts are annotated beside the same fixtures in `ir.rs`.

/// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }`, ECJ-shaped.
#[test]
pub(in crate::ir_lower::tests) fn a_rotated_for_loop_compiles_via_ir_and_sums() {
    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0xa7, 0x00, 0x0a, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01, 0x1c,
        0x1a, 0xa1, 0xff, 0xf7, 0x1b, 0xac, 0, 0,
    ];
    let cm = compile_via_ir(&code, 21, 1, 3).expect("a rotated for loop compiles via IR");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let f = |n: i64| unsafe { cm.try_call(&[n]).expect("call") };
    for (n, want) in [(0i64, 0i64), (1, 0), (5, 10), (10, 45), (-3, 0)] {
        assert_eq!(f(n), want, "f({n})");
    }
}

/// `int f(int n) { do { n = n - 3; } while (n > 0); return n; }`: the loop
/// header is pc 0.
#[test]
pub(in crate::ir_lower::tests) fn a_do_while_at_the_method_entry_compiles_via_ir() {
    let code = [
        0x1a, 0x06, 0x64, 0x3b, 0x1a, 0x9d, 0xff, 0xfb, 0x1a, 0xac, 0, 0,
    ];
    let cm = compile_via_ir(&code, 10, 1, 1).expect("a do/while at pc 0 compiles via IR");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let f = |n: i64| unsafe { cm.try_call(&[n]).expect("call") } as i32 as i64;
    for (n, want) in [(10i64, -2i64), (0, -3), (3, 0), (1, -2), (-5, -8)] {
        assert_eq!(f(n), want, "f({n})");
    }
}

/// `int f(int n) { int c = 0; for (int i = 0; i < n; i++) for (int j = 0; j < i; j++) c += j; return c; }`:
/// a javac-shaped outer loop around an ECJ-shaped inner one.
#[test]
pub(in crate::ir_lower::tests) fn a_rotated_loop_nested_in_a_javac_loop_compiles_via_ir() {
    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x1a, 0x03, 0x3e, 0xa7, 0x00, 0x0a, 0x1b,
        0x1d, 0x60, 0x3c, 0x84, 0x03, 0x01, 0x1d, 0x1c, 0xa1, 0xff, 0xf7, 0x84, 0x02, 0x01, 0xa7,
        0xff, 0xe7, 0x1b, 0xac, 0, 0,
    ];
    let cm = compile_via_ir(&code, 34, 1, 4).expect("a nested rotated loop compiles via IR");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let f = |n: i64| unsafe { cm.try_call(&[n]).expect("call") };
    // sum over i < n of (0 + 1 + ... + (i - 1))
    for (n, want) in [(0i64, 0i64), (1, 0), (4, 4), (5, 10), (6, 20)] {
        assert_eq!(f(n), want, "f({n})");
    }
}

/// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) { if ((i & 1) == 0) s += i; else s -= 1; } return s; }`,
/// ECJ-shaped: a forward if/else join inside a rotated body.
#[test]
pub(in crate::ir_lower::tests) fn a_rotated_loop_with_an_if_else_join_compiles_via_ir() {
    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0xa7, 0x00, 0x16, 0x1c, 0x04, 0x7e, 0x9a, 0x00, 0x0a, 0x1b, 0x1c,
        0x60, 0x3c, 0xa7, 0x00, 0x06, 0x84, 0x01, 0xff, 0x84, 0x02, 0x01, 0x1c, 0x1a, 0xa1, 0xff,
        0xeb, 0x1b, 0xac, 0, 0,
    ];
    let cm =
        compile_via_ir(&code, 33, 1, 3).expect("a rotated loop with an if/else compiles via IR");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let f = |n: i64| unsafe { cm.try_call(&[n]).expect("call") } as i32 as i64;
    for (n, want) in [(0i64, 0i64), (1, 0), (2, -1), (3, 1), (5, 4), (6, 3)] {
        assert_eq!(f(n), want, "f({n})");
    }
}

// do-while: the back-edge is an `if_icmplt` (not a goto), and the loop
// header self-loops (the condition is at the bottom). Exercises the
// if-as-back-edge path + a block whose true edge targets its own head.
#[test]
pub(in crate::ir_lower::tests) fn test_lower_do_while_sum() {
    // int f(int n){ int s=0,i=0; do { s+=i; i++; } while(i<n); return s; }
    //  0:iconst_0 1:istore_1 2:iconst_0 3:istore_2
    //  4:iload_1 5:iload_2 6:iadd 7:istore_1 8:iinc 2,1
    // 11:iload_2 12:iload_0 13:if_icmplt 4  16:iload_1 17:ireturn
    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01, 0x1c, 0x1a, 0xa1, 0xff,
        0xf7, 0x1b, 0xac, 0, 0,
    ];
    let cm = compile_via_ir(&code, 18, 1, 3).expect("do-while compiles via IR");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let f = |n: i64| unsafe { cm.try_call(&[n]).expect("call") };
    assert_eq!(f(5), 10, "runs i=0..4 → 0+1+2+3+4 = 10");
    assert_eq!(f(1), 0, "runs once (i=0) → 0");
    assert_eq!(f(0), 0, "do-while runs once even at n=0 → s=0");
}

// Nested counted loops: two loop headers, the inner nested in the outer.

// ── real-frame-deopt fires end-to-end: div-by-zero guard ─────────────
// Proves the trigger half: an IR-compiled `a/b` deopts (instead of the raw
// IDIV faulting) when b==0, with the reconstructed frame carrying the live
// operands at the idiv bci so the interpreter can re-execute and throw
// ArithmeticException. (The VM-side resume is wired separately, gated.)
#[test]
pub(in crate::ir_lower::tests) fn test_lower_div_by_zero_deopts() {
    use crate::deopt::{take_last_deopt, FrameValue};
    // int f(int a, int b){ return a / b; }  — iload_0; iload_1; idiv; ireturn
    let code = [0x1a, 0x1b, 0x6c, 0xac, 0, 0];
    let cm = compile_via_ir(&code, 4, 2, 2).expect("div compiles via IR");

    let _ = take_last_deopt(); // clear any stale state
                               // divisor != 0 → normal result, no deopt.
                               // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let ok = unsafe { cm.try_call(&[20, 4]).expect("call (b != 0)") };
    assert_eq!(ok, 5, "20 / 4 = 5");
    assert!(take_last_deopt().is_none(), "no deopt when divisor != 0");

    // divisor == 0 → deopt (sentinel + reconstructed frame), NOT a #DE fault.
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let sentinel = unsafe { cm.try_call(&[20, 0]).expect("call (b == 0)") };
    assert_eq!(sentinel, i64::MIN, "div by zero → deopt sentinel");
    let frame = take_last_deopt().expect("deopt reconstructed a frame");
    // idiv is at bci 2; resume there with [a, b] live on the operand stack
    // so the interpreter re-executes the division and throws.
    assert_eq!(frame.bci, 2, "resume at the idiv bci");
    assert_eq!(
        frame.stack,
        vec![FrameValue::Int(20), FrameValue::Int(0)],
        "operands restored for re-execution",
    );
}

#[test]
pub(in crate::ir_lower::tests) fn test_lower_ldiv_by_zero_reconstructs_long_frame() {
    use crate::deopt::{take_last_deopt, FrameValue};
    use crate::ir::{IrBuilder, IrType};
    // long f(long a, long b){ return a / b; }
    //   lload_0; lload_2; ldiv; lreturn   (+ 2 trailing padding bytes)
    // Proves a `long` live at the div guard reconstructs as a full-64-bit
    // FrameValue::Long (cat-2 width on resume), not a truncated Int.
    let code = [0x1e, 0x20, 0x6d, 0xad, 0, 0];
    let mut builder = IrBuilder::new(2, 4); // 2 long params (a@0-1, b@2-3)
    builder.set_param_types(&[IrType::Long, IrType::Long]);
    let mut graph = builder.build(&code, 4).expect("ldiv IR build");
    ir_optimize::optimize(&mut graph);
    let schedule = ir_schedule::schedule(&graph);
    let cm = lower(&graph, &schedule, 2, 4, &no_helpers()).expect("lower ldiv");

    let _ = take_last_deopt(); // clear any stale state
                               // divisor != 0 → normal full-64-bit result, no deopt. A 32-bit IDIV
                               // would mishandle this dividend (> i32::MAX).
                               // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let ok = unsafe { cm.try_call(&[0x1_0000_0000, 2]).expect("call (b != 0)") };
    assert_eq!(ok, 0x8000_0000, "0x1_0000_0000 / 2 (genuinely 64-bit)");
    assert!(take_last_deopt().is_none(), "no deopt when divisor != 0");

    // divisor == 0 → deopt; the reconstructed frame must carry the two LONG
    // operands as FrameValue::Long (full 64 bits) so the interpreter resumes
    // at the ldiv bci and re-executes it (throwing ArithmeticException).
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let sentinel = unsafe { cm.try_call(&[0x7_0000_0000, 0]).expect("call (b == 0)") };
    assert_eq!(sentinel, i64::MIN, "ldiv by zero → deopt sentinel");
    let frame = take_last_deopt().expect("deopt reconstructed a frame");
    assert_eq!(frame.bci, 2, "resume at the ldiv bci");
    assert_eq!(
        frame.stack,
        vec![FrameValue::Long(0x7_0000_0000), FrameValue::Long(0)],
        "long operands restored as full-64-bit FrameValue::Long",
    );
    // Locals: a@0 and b@2 are longs (StackSlotLong → Long); the high-half
    // slots 1/3 are dummies (never read by the interpreter).
    assert_eq!(frame.locals[0], FrameValue::Long(0x7_0000_0000));
    assert_eq!(frame.locals[2], FrameValue::Long(0));
}

// ── `[rbp - 0]` sentinel removal + resource bounds ───────────────────

/// `int f(int a) { return a + 1; }` as a hand-built graph, plus its
/// schedule. The smallest shape that has a real data edge to verify.
pub(in crate::ir_lower::tests) fn add_one_graph() -> (Graph, Schedule) {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
    let sum = graph.add(Op::Add, IrType::Int, vec![a, one], None);
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, sum], None);
    let schedule = ir_schedule::schedule(&graph);
    (graph, schedule)
}

/// `void f(int a) { a + 1; }` — the same shape as [`add_one_graph`] with the
/// value dropped from the `Return`, so the only difference between the two
/// lowerings is the void exit itself.
pub(in crate::ir_lower::tests) fn void_return_graph() -> (Graph, Schedule) {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
    let _sum = graph.add(Op::Add, IrType::Int, vec![a, one], None);
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl], None);
    let schedule = ir_schedule::schedule(&graph);
    (graph, schedule)
}

/// Laying out a jump table on an OVERFLOWED buffer must return, not spin.
///
/// `emit_switch_tables` pads to a 4-byte boundary with `emit_byte` until
/// `pos() % 4 == 0`. Once the buffer has overflowed, every write is a
/// sticky no-op and `pos()` is frozen. If it froze at an unaligned offset,
/// the loop never ended. That hung the compiling thread, and with it the
/// whole VM: Spring tests using Mockito's inline mock maker hung in
/// ByteBuddy's agent self-attach (`JarInputStream.<init>` compiling
/// a switch-bearing method at the IR tier) until the watchdog fired.
///
/// Run on a worker with a deadline, so a regression FAILS this test
/// rather than hanging the test binary.
#[test]
pub(in crate::ir_lower::tests) fn a_switch_table_on_an_overflowed_buffer_returns_instead_of_spinning(
) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (graph, schedule) = add_one_graph();
        let graph: &'static Graph = Box::leak(Box::new(graph));
        let schedule: &'static Schedule = Box::leak(Box::new(schedule));
        let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let mut lo = lowerer_over(graph, schedule, plan, 64);
        // Three bytes in, then a write that does not fit: the buffer is
        // overflowed with `pos()` frozen at 3, which no padding can align.
        lo.buf.emit(&[0x90; 3]);
        lo.buf.emit(&[0x90; 128]);
        assert!(lo.buf.overflowed() && lo.buf.pos() % 4 != 0);
        lo.switch_tables.push(SwitchTablePlan {
            lea_patch: 0,
            targets: vec![0, 0],
        });
        lo.emit_switch_tables();
        let _ = tx.send(lo.buf.pos());
    });
    let pos = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("emit_switch_tables did not return on an overflowed buffer: it is spinning");
    assert_eq!(pos, 3, "nothing may be written past the overflow");
}

/// A VOID return must leave a defined value in the return register.
///
/// `i64::MIN` there is the VM-wide "the callee trapped" sentinel, and every
/// consumer reads the raw register: the single-pass inline MIC/PIC
/// cascade's `emit_inline_callee_deopt_check`, the megamorphic hashed
/// stub's, `jit_invoke_virtual_mic`'s `rc == i64::MIN`, and the
/// interpreter's post-JIT drain. A void method has no return value, so
/// unless the exit writes one, RAX carries whatever the last operation left
/// there and can equal the sentinel by accident — on behalf of a call that
/// neither threw nor deopted. The single-pass backend has always zeroed it
/// (`x64/bytecode_walk.rs`, the `0xb1` arm); this backend did not.
///
/// Asserted on the BYTES, and as a DIFFERENCE against the value-returning
/// twin, so it cannot pass by accident: `try_call` returning 0 would be
/// satisfied by an undefined register that merely happened to hold 0.
#[test]
pub(in crate::ir_lower::tests) fn a_void_return_writes_the_return_register() {
    const XOR_EAX_EAX: [u8; 2] = [0x31, 0xC0];

    let (vg, vs) = void_return_graph();
    let void_method = lower(&vg, &vs, 1, 1, &no_helpers()).expect("void method must lower");
    let (ag, as_) = add_one_graph();
    let value_method = lower(&ag, &as_, 1, 1, &no_helpers()).expect("a+1 must lower");

    let void_zeroes = count_seq(void_method._buffer_slice_for_debug(), &XOR_EAX_EAX);
    let value_zeroes = count_seq(value_method._buffer_slice_for_debug(), &XOR_EAX_EAX);
    assert_eq!(
        void_zeroes,
        value_zeroes + 1,
        "the void exit must contribute exactly one `XOR EAX,EAX` the \
             value-returning exit does not (void={void_zeroes}, value={value_zeroes})",
    );
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { void_method.try_call(&[41]) },
        Ok(0),
        "a void method must not hand its caller the deopt sentinel",
    );
}

/// The verifier must not refuse a graph the lowerer handles — otherwise the
/// "bail before emission" fix would silently cost JIT coverage.
#[test]
pub(in crate::ir_lower::tests) fn verification_accepts_a_well_formed_graph() {
    let (graph, schedule) = add_one_graph();
    assert!(verify_data_locations(&graph, &schedule).is_ok());
    let method = lower(&graph, &schedule, 1, 1, &no_helpers()).expect("a+1 must lower");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { method.try_call(&[41]) }, Ok(42));
}

/// Acceptance test for the `[rbp - 0]` defect (report: "replace
/// zero-initialized `node_slot` with verified optional locations").
///
/// A data input that no emitted node defines — here an `Op::Dead` node,
/// which the scheduler never places, exactly like the GVN-collapsed
/// `ArrayLoad(Double)` observed in `DualPivotQuicksort.insertionSort` —
/// must refuse the compile *before* emission, not be detected afterwards by
/// a sticky bit. Before this change the use lowered to `[rbp - 0]`, reading
/// the saved caller frame pointer as a `double`.
#[test]
pub(in crate::ir_lower::tests) fn an_unallocated_data_input_refuses_the_compile_before_emission() {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    // Scheduler omission, modelled: `Op::Dead` is never placed into a
    // block, so nothing ever allocates a location for it.
    let ghost = graph.add(Op::Dead, IrType::Int, vec![], None);
    let sum = graph.add(Op::Add, IrType::Int, vec![a, ghost], None);
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, sum], None);

    let schedule = ir_schedule::schedule(&graph);
    assert_eq!(
        schedule.node_to_block[ghost as usize],
        usize::MAX,
        "precondition: the ghost node is in no emitted block",
    );

    let err = verify_data_locations(&graph, &schedule)
        .expect_err("an unallocated data input must be refused");
    assert_eq!(err.reason, BailoutReason::UnallocatedValue { node: ghost });
    assert_eq!(err.category(), "unallocated_value");

    // …and the public entry point turns that into the historical "run in a
    // lower tier" answer rather than emitting or panicking.
    assert!(lower(&graph, &schedule, 1, 1, &no_helpers()).is_none());
}

/// The slot accessor has no encoding for "unallocated", so it can never
/// hand back `0` — the offset that means `[rbp - 0]`, the saved caller RBP.
#[test]
pub(in crate::ir_lower::tests) fn slot_accessor_never_yields_the_rbp_zero_sentinel() {
    let (graph, schedule) = add_one_graph();
    let a = graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Param(0)))
        .expect("param node") as NodeId;
    let sum = graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Add))
        .expect("add node") as NodeId;

    let empty: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact_fields: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let no_frame_sites = crate::ir::IrInlineFrameSites::default();
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let helpers = no_helpers();
    let plan = plan_slots(&graph, &schedule, None);
    let mut lowerer = Lowerer::new(
        &graph,
        &schedule,
        buf,
        1,
        1,
        &plan,
        &helpers,
        &empty,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact_fields,
        &no_frame_sites,
    );

    // Unallocated is an error, not an offset.
    assert!(lowerer.slot_of_checked(a).is_err());
    assert!(lowerer.node_slot.iter().all(|slot| slot.is_none()));

    let off = lowerer.alloc_slot_checked(a).expect("first spill slot");
    assert!(
        off >= lowerer.first_spill,
        "{off} < {}",
        lowerer.first_spill
    );
    assert_ne!(off, 0);
    assert_eq!(lowerer.slot_of_checked(a).expect("allocated"), off);
    // Non-zero by type, not by convention: every occupied entry is a
    // `NonZeroU32`, so `0` is not a representable location at all.
    assert!(lowerer
        .node_slot
        .iter()
        .flatten()
        .all(|located| located.get() > 0));

    // The infallible façade the emission sites use latches a structured
    // reason and still never returns 0.
    let poison = lowerer.slot_of(sum);
    assert_ne!(poison, 0, "the façade must never emit [rbp - 0]");
    assert!(lowerer.unallocated_slot_use.get());
    let latched = lowerer.take_latched_bailout().expect("a latched reason");
    assert_eq!(
        latched.reason,
        BailoutReason::UnallocatedValue { node: sum }
    );
    // An out-of-range id is an error too — it used to index-panic.
    assert!(lowerer.slot_of_checked(NO_NODE).is_err());
}

/// Report: "bound graph and frame memory before allocation" — an
/// over-budget node count is refused with `GraphTooLarge`.
#[test]
pub(in crate::ir_lower::tests) fn an_over_limit_node_count_bails_with_graph_too_large() {
    assert!(check_graph_size(0, DEFAULT_MAX_NODES).is_ok());
    assert!(check_graph_size(DEFAULT_MAX_NODES, DEFAULT_MAX_NODES).is_ok());
    let err = check_graph_size(DEFAULT_MAX_NODES + 1, DEFAULT_MAX_NODES)
        .expect_err("over the node budget");
    assert_eq!(
        err.reason,
        BailoutReason::GraphTooLarge {
            nodes: DEFAULT_MAX_NODES + 1,
            limit: DEFAULT_MAX_NODES,
        }
    );
    assert_eq!(err.category(), "graph_too_large");
}

// Report: "bound graph and frame memory before allocation" — the frame is
// estimated (spill reservation + locals + staged outgoing args) and
// refused with `FrameTooLarge` before a byte of it is reserved.
// ── Liveness-based frame-slot reuse ──────────────────────────────────

/// An empty graph shell, so each test below only writes the shape it cares
/// about.
pub(in crate::ir_lower::tests) fn empty_graph() -> Graph {
    Graph::empty(0)
}

/// `int f(int a) { a += 1; a += 1; … }` with `len` increments: a chain in
/// which every temporary is dead the instant the next one consumes it. The
/// shape the old one-slot-per-node reservation was worst on.
pub(in crate::ir_lower::tests) fn straight_line_chain(len: usize) -> (Graph, Schedule) {
    let mut graph = empty_graph();
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
    let mut cur = a;
    for _ in 0..len {
        cur = graph.add(Op::Add, IrType::Int, vec![cur, one], None);
    }
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, cur], None);
    let schedule = ir_schedule::schedule(&graph);
    (graph, schedule)
}

/// Re-derive the linear emission positions [`plan_slots`] numbers, so a
/// test can assert *where* a value is live. Deliberately a separate
/// implementation of the numbering the module comment documents: if the two
/// ever disagree, the loop test below is the one that notices.
#[allow(clippy::type_complexity)]
pub(in crate::ir_lower::tests) fn emission_positions(
    graph: &Graph,
    schedule: &Schedule,
) -> (Vec<Option<usize>>, Vec<(usize, usize)>) {
    let mut pos = vec![None; graph.nodes.len()];
    let mut span = Vec::with_capacity(schedule.blocks.len());
    let mut seq = 0usize;
    for block in &schedule.blocks {
        let start = seq;
        for &nid in &block.nodes {
            pos[nid as usize] = Some(seq);
            seq += 1;
        }
        if let Some(term) = block.terminator {
            pos[term as usize] = Some(seq);
            seq += 1;
        }
        span.push((start, seq));
        seq += 1;
    }
    (pos, span)
}

/// How many values the plan gives a slot to at all — the number the old
/// reservation would have needed (one word each, forever).
pub(in crate::ir_lower::tests) fn slotted_value_count(plan: &SlotPlan) -> usize {
    plan.node_color.iter().flatten().count()
}

/// The colouring's whole point: values that are never live at the same time
/// land on the same frame word.
#[test]
pub(in crate::ir_lower::tests) fn non_overlapping_values_share_a_frame_slot() {
    let (graph, schedule) = straight_line_chain(64);
    let plan = plan_slots(&graph, &schedule, None);
    verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");

    let slotted = slotted_value_count(&plan);
    assert_eq!(slotted, 66, "param + const + 64 adds each want a slot");
    // Successive links of the chain overlap at one position (the consumer's
    // own position), so the chain alternates between two words; the
    // constant and the parameter account for the rest.
    assert!(
        plan.slots <= 8,
        "64 chained temporaries should collapse to a handful of slots, got {}",
        plan.slots,
    );
    assert!(
        plan.slots < slotted,
        "no reuse happened at all: {} slots for {slotted} values",
        plan.slots,
    );
    assert!(
        plan.peak_live <= plan.slots,
        "peak live ({}) is the floor the colouring cannot beat, slots = {}",
        plan.peak_live,
        plan.slots,
    );
}

/// …and values that ARE live at the same time never do. The endpoint-
/// inclusive overlap rule means a value whose last use is at position `p`
/// also keeps its word against a value defined at `p`.
#[test]
pub(in crate::ir_lower::tests) fn overlapping_values_never_share_a_frame_slot() {
    // int f(int a, int b) { int s = a + b; return a + s; }  — `a` is live
    // across `s`, so the two cannot share; `b` dies at `s`, so it can be
    // recycled by the value defined last.
    let mut graph = empty_graph();
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let b = graph.add(Op::Param(1), IrType::Int, vec![start], None);
    let s = graph.add(Op::Add, IrType::Int, vec![a, b], None);
    let t = graph.add(Op::Add, IrType::Int, vec![a, s], None);
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, t], None);
    let schedule = ir_schedule::schedule(&graph);

    let plan = plan_slots(&graph, &schedule, None);
    verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");
    let color = |id: NodeId| plan.node_color[id as usize].expect("a coloured value");

    assert_ne!(color(a), color(b), "both live into the first add");
    assert_ne!(color(a), color(s), "a outlives s's definition");
    assert_ne!(color(a), color(t), "a is live where t is defined");
    assert_ne!(color(s), color(t), "s is live where t is defined");
    // Four values, one legitimate sharing pair (b dies before t is defined).
    assert_eq!(slotted_value_count(&plan), 4);
    assert!(
        plan.slots < 4,
        "a dead-before-t value must be recycled, got {} slots",
        plan.slots,
    );

    // End to end, the code still computes a + (a + b).
    let cm = lower(&graph, &schedule, 2, 2, &no_helpers()).expect("lowers");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { cm.try_call(&[3, 4]) }, Ok(10));
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { cm.try_call(&[-1, 5]) }, Ok(3));
}

/// A reference and a primitive never share a frame word.
///
/// `emit_safepoint_map` publishes a slot as a GC root because *some* `Ref`
/// value was defined into it, and `defined_nodes` never un-sets. If a
/// primitive could inherit that word, the collector would follow an `int`
/// as an object pointer. The two free lists are disjoint, so this is a
/// property of the representation, not of the schedule.
#[test]
pub(in crate::ir_lower::tests) fn a_reference_is_never_aliased_with_a_non_reference() {
    // Object f(Object r, int a) { int t = a + 1; int u = t + 1; return r; }
    // `r` is live to the return; `a`, `t`, `u` die in sequence, so the
    // primitive pool recycles while the reference pool does not.
    let mut graph = empty_graph();
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let r = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
    let a = graph.add(Op::Param(1), IrType::Int, vec![start], None);
    let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
    let t = graph.add(Op::Add, IrType::Int, vec![a, one], None);
    let u = graph.add(Op::Add, IrType::Int, vec![t, one], None);
    let v = graph.add(Op::Add, IrType::Int, vec![u, one], None);
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, r], None);
    let schedule = ir_schedule::schedule(&graph);

    let plan = plan_slots(&graph, &schedule, None);
    verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");

    // Reuse did happen among the primitives…
    assert!(
        plan.slots < slotted_value_count(&plan),
        "the primitive chain should have recycled a word",
    );
    // …and no word holds both classes.
    let mut class_of_color: HashMap<u32, SlotClass> = HashMap::new();
    for (id, color) in plan.node_color.iter().enumerate() {
        let (color, class) = match (color, plan.class[id]) {
            (Some(color), Some(class)) => (*color, class),
            _ => continue,
        };
        match class_of_color.get(&color) {
            Some(seen) => assert_eq!(
                *seen, class,
                "slot {color} mixes {seen:?} and {class:?} (n{id})",
            ),
            None => {
                class_of_color.insert(color, class);
            }
        }
    }
    // The reference genuinely got a word of its own, not merely a word no
    // primitive happened to want.
    let r_color = plan.node_color[r as usize].expect("a coloured reference");
    assert_eq!(class_of_color.get(&r_color), Some(&SlotClass::Ref));
    for id in [a, t, u, v] {
        assert_ne!(
            plan.node_color[id as usize],
            Some(r_color),
            "n{id} is a primitive on the reference's word",
        );
    }

    // …and the stale-word oracle's view of the same plan agrees: every
    // primitive's colour is offered as "provably not a reference", and the
    // reference's colour is not. This is what an IR frame publishes as
    // `OopMapEntry::non_oop_stack_slots`, and a colour wrongly listed here
    // would let the residue report call a live root dead storage.
    let prim = plan.prim_colors();
    for id in [a, t, u, v] {
        let c = plan.node_color[id as usize].expect("a coloured primitive");
        assert!(
            prim.contains(&c),
            "n{id}'s word {c} is missing from prim_colors"
        );
    }
    assert!(
        !prim.contains(&r_color),
        "the reference's word {r_color} must never be offered as a non-reference",
    );
}

/// COV-02: an `aaload` result is a GC ROOT, and this is the executed proof.
///
/// `emit_safepoint_map` decides what to publish by scanning
/// `graph.nodes[id].ty != IrType::Ref`, and `plan_slots` decides which pool
/// a node's frame word comes from. Both answers hang on the ONE thing the
/// `aaload` builder arm chooses: the node's `IrType`. Typing it `Int`
/// compiles, passes every value-differential in
/// `jit/tests/ir_vs_singlepass.rs`, and loses the element at the first
/// relocating collection — a failure that surfaces nowhere near here.
///
/// The brief's own suggestion for proving this — run it under
/// `CRATONVM_MOVING_YOUNG` — cannot work: `JIT_PUBLISHES_RELOCATION_CONTRACT`
/// is `false`, so a young collection that meets an unprovable compiled frame
/// falls back to the non-moving sweep instead of relocating, and an
/// unpublished root produces no observable stale pointer. See
/// `cov-02-array-element-access-RETIRED-20260803.md`. So the
/// property is asserted where it is actually decided.
///
/// **Anti-vacuity, executed rather than argued.** The mutation was run:
/// with `0x32`'s arm in `ir.rs` changed to `(MemKind::Ref, IrType::Int)`,
/// this test fails with `left: Int, right: Ref` and nothing else in the
/// crate's 1,869 lib tests notices. That is the edit to repeat if this test
/// is ever suspected of measuring something else.
///
/// It has already been wrong once in the other direction: it first asserted
/// `class == SlotClass::Ref` and failed on an honest tree, because in a
/// method this short the element is still deopt-visible and gets pinned.
/// See the comment at the assertion.
#[test]
pub(in crate::ir_lower::tests) fn an_aaload_result_is_reference_typed_and_takes_a_reference_slot() {
    use crate::ir::{IrBuilder, MemKind};

    // static Object get(Object[] a, int i) { return a[i]; }
    //   aload_0; iload_1; aaload; areturn      (+2 bytes of padding)
    let code = [0x2a, 0x1b, 0x32, 0xb0, 0x00, 0x00];
    let graph = IrBuilder::new(2, 2)
        .build(&code, 4)
        .expect("the aaload corpus must build");

    let loads: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::ArrayLoad(MemKind::Ref)))
        .map(|(id, _)| id as NodeId)
        .collect();
    assert_eq!(
        loads.len(),
        1,
        "precondition: exactly one `aaload` node, or this test is measuring \
             something else"
    );
    let load = loads[0];
    assert_eq!(
        graph.nodes[load as usize].ty,
        IrType::Ref,
        "an `aaload` result is a reference; `emit_safepoint_map` skips every \
             node whose `ty` is not `Ref`, so an `Int` here is an unpublished \
             root at every later safepoint"
    );

    // …and no primitive may ever inherit the word it lands in, because
    // `emit_safepoint_map` will name that word as a root.
    //
    // Asserted as an ALIASING property rather than as `class ==
    // SlotClass::Ref`, which is what this test tried first and which is
    // wrong: in a method this short the element is still on the operand
    // stack at the `areturn` bci, so a safepoint snapshot names it and
    // `plan_slots` pins it (`SlotClass::Pinned` — shares with nothing at
    // all, strictly stronger than the reference pool). Which of the two it
    // gets depends on where the value dies, i.e. on the fixture. What must
    // hold for every fixture is that no `Prim` sits on its colour.
    let schedule = ir_schedule::schedule(&graph);
    let plan = plan_slots(&graph, &schedule, None);
    let class = plan.class[load as usize].expect("the element must get a frame word");
    assert_ne!(
        class,
        SlotClass::Prim,
        "an `aaload` result took a word from the PRIMITIVE pool; the \
             collector would then follow whatever int recycled it as an object \
             pointer"
    );
    let color = plan.node_color[load as usize].expect("a coloured element");
    for (id, other) in plan.node_color.iter().enumerate() {
        if id == load as usize || *other != Some(color) {
            continue;
        }
        assert_ne!(
            plan.class[id],
            Some(SlotClass::Prim),
            "n{id} is a primitive sharing the `aaload` element's frame word",
        );
    }
}

/// Every value a deopt frame names keeps a dedicated slot: the deopt
/// producer reads it at a native offset chosen at run time, so "dead by
/// then" is not a question this file can answer.
#[test]
pub(in crate::ir_lower::tests) fn a_deopt_visible_value_keeps_a_dedicated_slot() {
    let (mut graph, _) = straight_line_chain(8);
    // Name the first two adds in a snapshot, exactly as `IrBuilder` would
    // for a bytecode boundary whose locals hold them.
    let adds: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::Add))
        .map(|(id, _)| id as NodeId)
        .take(2)
        .collect();
    graph.safepoints.push(SafepointSnapshot::new(
        0,
        vec![adds[0]],
        vec![adds[1]],
        Vec::new(),
    ));
    let schedule = ir_schedule::schedule(&graph);
    let plan = plan_slots(&graph, &schedule, None);
    verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");

    for &pinned in &adds {
        assert_eq!(
            plan.class[pinned as usize],
            Some(SlotClass::Pinned),
            "n{pinned} is named by a deopt frame and must not share",
        );
        let color = plan.node_color[pinned as usize].expect("a coloured value");
        let sharers = plan
            .node_color
            .iter()
            .filter(|c| **c == Some(color))
            .count();
        assert_eq!(sharers, 1, "n{pinned}'s word is shared by {sharers} values");
    }
}

/// Round 14 wave 1 (lane chain): a value only a CHAIN snapshot names is
/// not pinned (`CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS`, default on);
/// its colouring range reaches the node that could publish the chain, and
/// no value whose range meets that one takes its word. With the switch at
/// `0` it is pinned, as before.
/// The colouring verifier is a real check, not a comment: each way a
/// colouring could go wrong is refused.
#[test]
pub(in crate::ir_lower::tests) fn the_colouring_verifier_rejects_an_aliased_plan() {
    let (graph, schedule) = straight_line_chain(8);
    let good = plan_slots(&graph, &schedule, None);
    assert!(verify_slot_colouring(&graph, &schedule, &good).is_ok());

    let adds: Vec<usize> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::Add))
        .map(|(id, _)| id)
        .collect();

    // (a) Two values that ARE live together forced onto one word — the
    //     silent-wrong-code case.
    let mut aliased = plan_slots(&graph, &schedule, None);
    let (first, second) = (adds[0], adds[1]);
    assert!(
        aliased.range[first]
            .expect("range")
            .overlaps(aliased.range[second].expect("range")),
        "precondition: consecutive chain links overlap",
    );
    let (donor_color, donor_class) = (aliased.node_color[first], aliased.class[first]);
    aliased.node_color[second] = donor_color;
    aliased.class[second] = donor_class;
    let err = verify_slot_colouring(&graph, &schedule, &aliased)
        .expect_err("an aliased pair must be refused");
    assert_eq!(err.category(), "internal");
    assert!(
        err.to_string().contains("simultaneously live"),
        "unexpected reason: {err}",
    );

    // (b) A reference on a primitive's word.
    let mut mixed = plan_slots(&graph, &schedule, None);
    let shared = mixed.node_color[adds[0]];
    mixed.node_color[adds[2]] = shared;
    mixed.class[adds[2]] = Some(SlotClass::Ref);
    mixed.class[adds[0]] = Some(SlotClass::Prim);
    let err = verify_slot_colouring(&graph, &schedule, &mixed)
        .expect_err("a mixed-class word must be refused");
    assert!(
        err.to_string().contains("reference and primitive"),
        "unexpected reason: {err}",
    );

    // (c) A value the lowerer will allocate for, left uncoloured — the
    //     scheduler-omission class `verify_data_locations` guards, restated
    //     for the plan.
    let mut missing = plan_slots(&graph, &schedule, None);
    missing.node_color[adds[0]] = None;
    let err = verify_slot_colouring(&graph, &schedule, &missing)
        .expect_err("an uncoloured scheduled value must be refused");
    assert_eq!(err.category(), "unallocated_value");

    // (d) A colour outside the reservation the frame was sized from.
    let mut oversized = plan_slots(&graph, &schedule, None);
    let beyond = oversized.slots as u32 + 1;
    oversized.node_color[adds[0]] = Some(beyond);
    assert!(verify_slot_colouring(&graph, &schedule, &oversized).is_err());
}

/// The measurement the report asks for: frame bytes against node count on a
/// long straight-line method.
#[test]
pub(in crate::ir_lower::tests) fn frame_bytes_track_peak_liveness_not_node_count() {
    let plain = FrameNeeds {
        needs_context: false,
        max_call_args: 0,
        vector_xmm_saves: false,
    };
    let (graph, schedule) = straight_line_chain(512);
    let plan = plan_slots(&graph, &schedule, None);
    verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");

    let before = estimate_frame_bytes(0, graph.nodes.len(), &plain);
    let after = estimate_frame_bytes(0, plan.slots, &plain);
    assert!(
        after * 8 < before,
        "frame bytes only fell from {before} to {after} on a 512-link chain",
    );
    // The residual is the fixed part of the frame — bookkeeping slots, the
    // stack-arg reserve, the ABI shadow space and, since the register file
    // went default-on (2026-09-02), the estimate's reservation for the
    // callee-saved GP save area — not spill.
    // The 2026-09-05 default-on of the deopt register image adds its 256
    // bytes to the same fixed part. Read from the function the frame is
    // laid out with rather than folded into the literal, so the bound
    // still means "no spill" on a configuration that reserves neither.
    //
    // The GP save area is now read the same way, for the same reason: it
    // is 40 bytes on System V and 56 on Win64, where RSI/RDI joined
    // `IR_GP_LINEAR_SCAN` (2026-09-10). It had been folded into the 192,
    // which made this bound a Windows-vs-Linux coin the moment the file
    // stopped being the same width on both.
    let fixed = 152 + ir_saved_gpr_bytes() as usize + ir_deopt_regs_bytes() as usize;
    assert!(
        after <= fixed,
        "{after} bytes for a 4-slot working set (fixed part is {fixed})",
    );
}

/// The node ceiling the frame bound implies, before and after.
///
/// One 8-byte slot per node put the ceiling at ~4 000 nodes: past that the
/// frame estimate crossed `DEFAULT_MAX_FRAME_BYTES` and the optimizing tier
/// declined the method outright. Sized by peak liveness instead, a long
/// method is bounded by `DEFAULT_MAX_NODES` again.
#[test]
pub(in crate::ir_lower::tests) fn a_previously_declined_large_method_now_compiles() {
    let plain = FrameNeeds {
        needs_context: false,
        max_call_args: 0,
        vector_xmm_saves: false,
    };

    // Where the old accounting stopped: the largest node count whose
    // one-slot-per-node frame still fit.
    let old_ceiling = (1..=DEFAULT_MAX_NODES)
        .take_while(|&n| estimate_frame_bytes(0, n, &plain) <= DEFAULT_MAX_FRAME_BYTES)
        .last()
        .expect("some node count fits");
    assert!(
        (4000..4200).contains(&old_ceiling),
        "the old ceiling was ~4 000 nodes, computed {old_ceiling}",
    );

    let (graph, schedule) = straight_line_chain(5_000);
    assert!(
        graph.nodes.len() > old_ceiling,
        "precondition: this method was over the old ceiling",
    );
    assert!(
        check_frame_size(
            estimate_frame_bytes(0, graph.nodes.len(), &plain),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .is_err(),
        "precondition: one slot per node would still be refused",
    );

    let plan = plan_slots(&graph, &schedule, None);
    verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");
    assert!(
        check_frame_size(
            estimate_frame_bytes(1, plan.slots, &plain),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .is_ok(),
        "{} slots should fit comfortably",
        plan.slots,
    );

    // And it really compiles and really runs.
    let cm = lower(&graph, &schedule, 1, 1, &no_helpers()).expect("a 5 000-node method lowers");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { cm.try_call(&[0]) }, Ok(5_000));
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { cm.try_call(&[42]) }, Ok(5_042));
}

// ── Phi parallel copy (the swap-loop wrong-code fix) ─────────────────
//
// `prealloc_phi_slots` gives every phi its OWN frame word, so the copies on
// one CFG edge are a parallel assignment over distinct words. Emitted in
// gather order through RAX — which is what this file did — a loop that
// swaps two locals makes each phi the other's back-edge value, the second
// copy reads the word the first just overwrote, and both locals end up
// holding the same value. These tests execute the emitted bytes, because
// that bug is invisible to any assertion about the copy *list*.

/// Value seeded into the reserved scratch word before every harness run, so
/// a test can prove an acyclic copy never touched it.
pub(in crate::ir_lower::tests) const SCRATCH_SENTINEL: i64 = 0x0BAD_5C7A_0000_1234;

/// Emit `push rbp; mov rbp,rsp; sub rsp,frame`, seed the named frame words
/// (plus the scratch, with [`SCRATCH_SENTINEL`]), run `copies` through the
/// REAL sequencer and the REAL emitter, load one word into RAX and return.
/// Then execute it.
///
/// `read_back == None` reads the reserved scratch word itself.
///
/// The frame is a genuine `Lowerer` frame (so the offsets, the scratch
/// reservation and `frame_size` are the ones production uses); only the
/// prologue/epilogue are hand-rolled, because the real ones fetch a thread
/// and touch the shadow stack that this fixture has no helpers for.
pub(in crate::ir_lower::tests) fn parallel_copy_harness(
    seed: &[(i32, i64)],
    copies: &[(i32, i32)],
    read_back: Option<i32>,
) -> (i64, Vec<CopyOp>) {
    // Eight locals so the harness has plenty of frame words BELOW the
    // bookkeeping reservation to use as stand-in phi slots.
    const HARNESS_LOCALS: usize = 8;

    let (graph, schedule) = add_one_graph();
    let plan = plan_slots(&graph, &schedule, None);
    let empty: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let no_frame_sites = crate::ir::IrInlineFrameSites::default();
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let helpers = no_helpers();
    let mut lowerer = Lowerer::new(
        &graph,
        &schedule,
        buf,
        0,
        HARNESS_LOCALS,
        &plan,
        &helpers,
        &empty,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
        &no_frame_sites,
    );
    let scratch = lowerer.phi_copy_scratch_slot_off;
    let frame = lowerer.frame_size;
    for &(off, _) in seed {
        assert!(
            off > 0 && off < frame && off != scratch,
            "the fixture must name a real frame word other than the scratch: {off}",
        );
    }

    // push rbp ; mov rbp, rsp ; sub rsp, frame_size
    lowerer.buf.emit_byte(0x55);
    lowerer.buf.emit(&[0x48, 0x89, 0xE5]);
    lowerer.buf.emit(&[0x48, 0x81, 0xEC]);
    lowerer.buf.emit(&frame.to_le_bytes());

    let scratch_seed = [(scratch, SCRATCH_SENTINEL)];
    for &(off, val) in seed.iter().chain(scratch_seed.iter()) {
        lowerer.emit_mov_reg_imm64(RAX, val as u64);
        lowerer.store_rax(off);
    }

    let ops = phi_copy_sequence(copies).expect("the fixture's copies sequentialise");
    for op in ops.iter().copied() {
        lowerer
            // No `defer_publish`: this fixture drives the memory path only
            // (`src_node_of`/`phi_of_dst` are empty, so nothing publishes a
            // register and nothing can alias one).
            .emit_copy_op(op, &HashMap::new(), &HashMap::new(), &[], &mut Vec::new())
            .expect("every location in this backend is a frame word");
    }

    lowerer.load_to_rax(read_back.unwrap_or(scratch));
    // add rsp, frame_size ; pop rbp ; ret
    lowerer.buf.emit(&[0x48, 0x81, 0xC4]);
    lowerer.buf.emit(&frame.to_le_bytes());
    lowerer.buf.emit_byte(0x5D);
    lowerer.buf.emit_byte(0xC3);
    assert!(!lowerer.buf.overflowed(), "the harness body must fit");

    let cm = CompiledMethod::new(lowerer.buf);
    // SAFETY: the emitted body takes no arguments, reads only its own
    // frame, calls nothing and returns an i64 in RAX.
    let value = unsafe { cm.try_call(&[]) }.expect("the harness body runs");
    (value, ops)
}

pub(in crate::ir_lower::tests) fn count_kind(ops: &[CopyOp], f: impl Fn(&CopyOp) -> bool) -> usize {
    ops.iter().copied().filter(|o| f(o)).count()
}

/// The swap loop's back edge, at the machine level: `a <- b, b <- a`.
///
/// Sequential emission through RAX put `b`'s value in BOTH words. This is
/// the wrong-code witness, asserted by running the bytes.
#[test]
pub(in crate::ir_lower::tests) fn a_two_element_phi_cycle_really_swaps_the_two_frame_words() {
    let (a, b) = (8i32, 16i32);
    let seed = [(a, 0x1111_1111i64), (b, 0x2222_2222i64)];
    let copies = [(a, b), (b, a)];

    let (got_a, ops) = parallel_copy_harness(&seed, &copies, Some(a));
    let (got_b, _) = parallel_copy_harness(&seed, &copies, Some(b));
    assert_eq!(got_a, 0x2222_2222, "a must receive b's PRE-copy value");
    assert_eq!(
        got_b, 0x1111_1111,
        "b must receive a's PRE-copy value — 0x22222222 here is the old \
             sequential-emission bug, in which both words ended up holding b",
    );

    // One save/restore pair breaks the cycle; the third op is the move.
    assert_eq!(ops.len(), 3, "{ops:?}");
    assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Save { .. })), 1);
    assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Restore { .. })), 1);
    assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Move { .. })), 1);

    // …and the scratch word really is where the parked value went.
    let (scratch_after, _) = parallel_copy_harness(&seed, &copies, None);
    assert_ne!(
        scratch_after, SCRATCH_SENTINEL,
        "a cycle must go through the reserved scratch word",
    );
}

/// A three-element cycle rotates; the last move IS the restore, so it costs
/// one save, two moves and one restore.
#[test]
pub(in crate::ir_lower::tests) fn a_three_element_phi_cycle_rotates_the_frame_words() {
    let (a, b, c) = (8i32, 16i32, 24i32);
    let seed = [(a, 1i64), (b, 2i64), (c, 3i64)];
    // a <- b, b <- c, c <- a
    let copies = [(a, b), (b, c), (c, a)];

    let (got_a, ops) = parallel_copy_harness(&seed, &copies, Some(a));
    let (got_b, _) = parallel_copy_harness(&seed, &copies, Some(b));
    let (got_c, _) = parallel_copy_harness(&seed, &copies, Some(c));
    assert_eq!((got_a, got_b, got_c), (2, 3, 1), "{ops:?}");

    assert_eq!(ops.len(), 4, "one save, two moves, one restore: {ops:?}");
    assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Save { .. })), 1);
    assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Restore { .. })), 1);
}

/// The acyclic majority — every merge that is not a permutation — must be
/// unchanged: one load + one store per copy, and the scratch word untouched.
#[test]
pub(in crate::ir_lower::tests) fn an_acyclic_phi_chain_emits_the_minimal_moves_and_no_scratch() {
    let (a, b, c) = (8i32, 16i32, 24i32);
    let seed = [(a, 1i64), (b, 2i64), (c, 3i64)];
    // b <- a, c <- b. Gather order would overwrite b before c reads it.
    let copies = [(b, a), (c, b)];

    let (got_b, ops) = parallel_copy_harness(&seed, &copies, Some(b));
    let (got_c, _) = parallel_copy_harness(&seed, &copies, Some(c));
    assert_eq!(got_b, 1, "b <- a");
    assert_eq!(got_c, 2, "c must read b's PRE-copy value: {ops:?}");

    assert_eq!(
        ops,
        vec![
            CopyOp::Move {
                from: frame_word_loc(b).expect("a frame word"),
                to: frame_word_loc(c).expect("a frame word"),
            },
            CopyOp::Move {
                from: frame_word_loc(a).expect("a frame word"),
                to: frame_word_loc(b).expect("a frame word"),
            },
        ],
        "an acyclic web must cost exactly one move per copy",
    );

    let (scratch_after, _) = parallel_copy_harness(&seed, &copies, None);
    assert_eq!(
        scratch_after, SCRATCH_SENTINEL,
        "an acyclic web must not touch the reserved scratch word",
    );
}

/// A copy to itself is not a move: it must emit nothing at all, so a merge
/// whose phi already lives in its source's word costs zero instructions.
#[test]
pub(in crate::ir_lower::tests) fn a_self_copy_emits_nothing() {
    let ops = phi_copy_sequence(&[(8, 8), (16, 16)]).expect("sequentialises");
    assert!(ops.is_empty(), "{ops:?}");
}

/// ONE scratch word is enough however many cycles an edge contains: the
/// sequencer always consumes a save before it starts the next cycle. If
/// that ever stopped being true the saves would nest and the single
/// reserved word would be clobbered, so pin it here — this file's frame
/// reservation is what depends on it.
#[test]
pub(in crate::ir_lower::tests) fn disjoint_phi_cycles_never_nest_their_saves() {
    let (a, b, c, d) = (8i32, 16i32, 24i32, 32i32);
    let seed = [(a, 1i64), (b, 2i64), (c, 3i64), (d, 4i64)];
    let copies = [(a, b), (b, a), (c, d), (d, c)];

    let mut saved = false;
    let (got_a, ops) = parallel_copy_harness(&seed, &copies, Some(a));
    for op in &ops {
        match op {
            CopyOp::Save { .. } => {
                assert!(!saved, "a second save before the first restore: {ops:?}");
                saved = true;
            }
            CopyOp::Restore { .. } => {
                assert!(saved, "a restore with nothing saved: {ops:?}");
                saved = false;
            }
            CopyOp::Move { .. } => {}
        }
    }
    assert!(!saved, "an unconsumed save: {ops:?}");
    assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Save { .. })), 2);

    let (got_b, _) = parallel_copy_harness(&seed, &copies, Some(b));
    let (got_c, _) = parallel_copy_harness(&seed, &copies, Some(c));
    let (got_d, _) = parallel_copy_harness(&seed, &copies, Some(d));
    assert_eq!((got_a, got_b, got_c, got_d), (2, 1, 4, 3));
}

/// A register is representable in the shared `ValueLoc` but unreachable
/// here — this backend keeps every value in its home frame word. Refused,
/// not mis-emitted as an offset.
#[test]
pub(in crate::ir_lower::tests) fn a_register_location_is_refused_rather_than_emitted_as_an_offset()
{
    let err = frame_word_off(ValueLoc::Reg(crate::regalloc::PhysReg::gp(3)))
        .expect_err("a register is not a frame word");
    assert!(matches!(err.reason, BailoutReason::Internal(_)), "{err:?}",);
    // …and offset 0 (`[rbp - 0]` is the saved caller RBP) is not a location.
    assert!(frame_word_loc(0).is_err());
    assert!(frame_word_loc(-8).is_err());
}

/// The scratch is a BOOKKEEPING word: it sits below `first_spill`, so no
/// colour `plan_slots` hands out can ever land on it, and the five reserved
/// words are contiguous with no gap for a coloured slot to hide in.
///
/// Also the frame arithmetic: `estimate_frame_bytes` (which `lower_inner`
/// bounds against) must still equal the frame `Lowerer::new` lays out. The
/// constructor pins that with a `debug_assert_eq!`; this asserts it in a
/// release build too, and from the published offsets rather than from a
/// repetition of the same sum.
#[test]
pub(in crate::ir_lower::tests) fn the_phi_scratch_word_is_reserved_below_the_spill_band() {
    for &num_locals in &[0usize, 1, 8] {
        let (graph, schedule) = add_one_graph();
        let plan = plan_slots(&graph, &schedule, None);
        let empty: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let no_frame_sites = crate::ir::IrInlineFrameSites::default();
        let buf = ExecutableBuffer::new(4096).expect("executable buffer");
        let helpers = no_helpers();
        let lowerer = Lowerer::new(
            &graph,
            &schedule,
            buf,
            0,
            num_locals,
            &plan,
            &helpers,
            &empty,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            &no_frame_sites,
        );

        // The five bookkeeping words are contiguous and end at first_spill.
        assert_eq!(lowerer.shadow_thread_slot_off, lowerer.sp_id_slot_off + 8);
        assert_eq!(
            lowerer.shadow_savebase_slot_off,
            lowerer.shadow_thread_slot_off + 8
        );
        assert_eq!(
            lowerer.shadow_savetop_slot_off,
            lowerer.shadow_savebase_slot_off + 8
        );
        assert_eq!(
            lowerer.phi_copy_scratch_slot_off,
            lowerer.shadow_savetop_slot_off + 8,
            "the scratch is the fifth reserved word",
        );
        assert_eq!(
            lowerer.first_spill,
            lowerer.phi_copy_scratch_slot_off + 8,
            "the spill band starts one word above the scratch",
        );

        // No coloured slot can collide with it.
        for color in 0..plan.slots as i32 {
            assert_ne!(
                lowerer.first_spill + color * 8,
                lowerer.phi_copy_scratch_slot_off,
            );
        }
        assert!(lowerer.first_spill <= lowerer.spill_cap_off);

        // The estimate `lower_inner` bounds against is the frame that got
        // built — including the fifth reserved word.
        let needs = scan_frame_needs(&graph, &helpers);
        assert_eq!(
            lowerer.frame_size,
            estimate_frame_bytes(num_locals, plan.slots, &needs) as i32,
        );
        // …and that number really does account for locals + 5 bookkeeping
        // words + the spill band + the callee-saved save bands + the
        // 16-byte stack-arg reserve + the 32-byte ABI shadow.
        // `add_one_graph` needs no context slot and stages no call
        // arguments.
        //
        // The two save bands are read from the same functions the frame
        // was laid out with rather than spelled as literals: they are
        // platform- and flag-dependent (`IR_LOWER_SAVED_XMMS` is empty on
        // System V; both are empty with the linear-scan path off), and a
        // literal here would pin this test to one configuration.
        let locals_size = (num_locals as i32) * 8;
        let bookkeeping = 8 * 5;
        // `ir_deopt_regs_bytes` joined the other two on 2026-09-05, when
        // the register image went default-on. Read, not spelled, for the
        // reason the comment above gives.
        let saved = ir_saved_xmm_bytes() + ir_saved_gpr_bytes() + ir_deopt_regs_bytes();
        let tail = 32 + 16;
        assert_eq!(
            lowerer.frame_size,
            ((locals_size + bookkeeping + (plan.slots as i32) * 8 + saved + tail) + 15) & !15,
        );
        assert_eq!(lowerer.spill_cap_off, lowerer.frame_size - tail - saved);
        // Offsets are 1-based — `[rbp - 0]` is the saved caller RBP — so the
        // first spill word sits one word past the locals + bookkeeping bytes.
        assert_eq!(lowerer.first_spill, locals_size + bookkeeping + 8);
    }
}

/// End to end, on the shape the fix exists for.
///
/// ```java
/// static int f(int a, int b, int n) {
///     for (int i = 0; i < n; i++) { int t = a; a = b; b = t; }
///     return a - b;
/// }
/// ```
///
/// The two loop-header phis are each other's back-edge value, so the back
/// edge's parallel copy is a two-element cycle. Before the scratch word this
/// method REFUSED to compile (`UnsupportedShape("cyclic phi parallel
/// copy")`) and dropped to a lower tier; before that it compiled to wrong
/// code. `a - b` is the sharp readout: `0` is what both duplication bugs
/// (both words ending up with `a`, or both with `b`) produce.
#[test]
pub(in crate::ir_lower::tests) fn the_swap_loop_compiles_and_swaps() {
    //  0: iconst_0     03
    //  1: istore_3     3e          i = 0
    //  2: iload_3      1d      ← loop header
    //  3: iload_2      1c
    //  4: if_icmpge 17 a2 00 0d
    //  7: iload_1      1b          swap through the operand stack:
    //  8: iload_0      1a          stack = [b, a]
    //  9: istore_1     3c          b = a
    // 10: istore_0     3b          a = b
    // 11: iinc 3,1     84 03 01
    // 14: goto 2       a7 ff f4
    // 17: iload_0      1a
    // 18: iload_1      1b
    // 19: isub         64
    // 20: ireturn      ac
    let code = [
        0x03, 0x3e, 0x1d, 0x1c, 0xa2, 0x00, 0x0d, 0x1b, 0x1a, 0x3c, 0x3b, 0x84, 0x03, 0x01, 0xa7,
        0xff, 0xf4, 0x1a, 0x1b, 0x64, 0xac, 0, 0,
    ];
    let cm = compile_via_ir(&code, 21, 3, 4).expect(
        "the swap loop must COMPILE — a None here is the cyclic-parallel-copy \
             refusal coming back, which drops the method a tier",
    );
    // `ireturn` leaves a 32-bit value in the return register, so the raw
    // i64 readout carries an untouched upper half and a negative result
    // reads as its u32 bit pattern (-4 as 0xFFFF_FFFC). Sign-extend before
    // comparing, or a correct swap reports as a failure.
    let f = |a: i64, b: i64, n: i64| {
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let raw = unsafe { cm.try_call(&[a, b, n]).expect("call") };
        raw as i32 as i64
    };
    assert_eq!(f(3, 7, 0), -4, "no iterations: 3 - 7");
    assert_eq!(f(3, 7, 1), 4, "one swap: 7 - 3 (0 would mean a == b)");
    assert_eq!(f(3, 7, 2), -4, "two swaps: back to 3 - 7");
    assert_eq!(f(3, 7, 3), 4, "three swaps");
    assert_eq!(f(-9, 5, 1), 14, "5 - (-9)");
}

// ── Inlined caller scopes → `FrameState::caller` ─────────────────────

/// A trivially lowerable graph (`Start → Proj ctrl/mem → Param(0) →
/// Const(7) → Return`) with no safepoints; the caller pushes whatever
/// snapshots the test needs. Returns the `Const` node id, which resolves to
/// `FrameValue::Int(7)` with no machine location — the simplest slot a
/// snapshot can name.
pub(in crate::ir_lower::tests) fn scope_fixture_graph() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p0 = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let k = g.add(Op::Const(7), IrType::Int, vec![], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, p0], None);
    g.exit = ret;
    (g, k)
}

/// Resolve `graph.safepoints[index]` through a real `Lowerer`, i.e. exactly
/// the path `build_deopt_points` takes.
pub(in crate::ir_lower::tests) fn resolve_snapshot(graph: &Graph, index: usize) -> FrameState {
    let schedule = ir_schedule::schedule(graph);
    let plan = plan_slots(graph, &schedule, None);
    let empty: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let helpers = no_helpers();
    let no_frame_sites = crate::ir::IrInlineFrameSites::default();
    let lowerer = Lowerer::new(
        graph,
        &schedule,
        buf,
        1,
        1,
        &plan,
        &helpers,
        &empty,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
        &no_frame_sites,
    );
    lowerer.resolve_frame_state(index, &graph.safepoints[index])
}

/// A held monitor lowers with `relock` saying whether the compiled code
/// took the lock itself. A live monitor op on the object ⇒ `false` (the
/// thread already holds it). Every monitor op on it killed, the way
/// `apply_ea_to_ir`'s lock elision leaves them ⇒ `true` (the resume must
/// acquire it).
#[test]
pub(in crate::ir_lower::tests) fn a_held_monitor_is_marked_relock_only_when_its_ops_were_elided() {
    // aload_0; monitorenter; aload_0; monitorexit; return
    let code = [0x2a, 0xc2, 0x2a, 0xc3, 0xb1, 0, 0];
    let live = IrBuilder::new(1, 1).build(&code, 5).expect("build");
    let index = live
        .safepoints
        .iter()
        .position(|s| s.bci == 3)
        .expect("a snapshot at the monitorexit");

    let fs = resolve_snapshot(&live, index);
    assert_eq!(fs.monitors.len(), 1, "the lock is recorded");
    assert_eq!(fs.monitors[0].lock_depth, 1);
    assert!(!fs.monitors[0].relock, "a live lock is not re-acquired");

    let mut elided = IrBuilder::new(1, 1).build(&code, 5).expect("build");
    let monitor_ops: Vec<NodeId> = elided
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::MonitorEnter | Op::MonitorExit))
        .map(|(id, _)| id as NodeId)
        .collect();
    assert_eq!(monitor_ops.len(), 2);
    for id in monitor_ops {
        elided.kill(id);
    }
    let fs = resolve_snapshot(&elided, index);
    assert_eq!(fs.monitors.len(), 1, "an elided lock is still recorded");
    assert!(
        fs.monitors[0].relock,
        "no live monitor op names the object, so the resume must acquire it"
    );
}

/// Re-entrant locking of one object is ONE `MonitorInfo` with
/// `lock_depth` 2 — the install verifier refuses two entries on one object.
#[test]
pub(in crate::ir_lower::tests) fn a_reentrant_lock_coalesces_into_one_entry_with_its_depth() {
    // aload_0; monitorenter; aload_0; monitorenter; aload_0; monitorexit;
    // aload_0; monitorexit; return
    let code = [0x2a, 0xc2, 0x2a, 0xc2, 0x2a, 0xc3, 0x2a, 0xc3, 0xb1, 0, 0];
    let g = IrBuilder::new(1, 1).build(&code, 9).expect("build");
    let index = g
        .safepoints
        .iter()
        .position(|s| s.bci == 5)
        .expect("a snapshot at the inner monitorexit");
    assert_eq!(g.safepoints[index].monitors.len(), 2, "two enters recorded");
    let fs = resolve_snapshot(&g, index);
    assert_eq!(fs.monitors.len(), 1);
    assert_eq!(fs.monitors[0].lock_depth, 2);
}

/// Round 11 wave 14 (lane fib): once `ir_optimize::elide_nested_monitors`
/// deleted the inner region, a frame inside it is TWO entries on the one
/// object -- the level the compiled code holds (`relock == false`), then
/// the elided one (`relock == true`) -- and a frame outside it is the
/// ordinary single held entry. The resume enters the second entry once, so
/// the thread holds the object twice, as the interpreter frame expects.
#[test]
pub(in crate::ir_lower::tests) fn r11w14_a_nested_elided_lock_splits_into_held_and_relock_entries()
{
    let code = [0x2a, 0xc2, 0x2a, 0xc2, 0x2a, 0xc3, 0x2a, 0xc3, 0xb1, 0, 0];
    let mut g = IrBuilder::new(1, 1).build(&code, 9).expect("build");
    assert_eq!(crate::ir_optimize::elide_nested_monitors(&mut g), 1);
    let inner = g
        .safepoints
        .iter()
        .position(|s| s.bci == 5)
        .expect("a snapshot at the inner monitorexit");
    let fs = resolve_snapshot(&g, inner);
    assert_eq!(fs.monitors.len(), 2, "held and elided levels are split");
    assert_eq!(fs.monitors[0].lock_depth, 1);
    assert!(!fs.monitors[0].relock, "the outer level is still held");
    assert_eq!(fs.monitors[1].lock_depth, 1);
    assert!(
        fs.monitors[1].relock,
        "the elided level must be re-acquired"
    );
    assert_eq!(fs.monitors[0].object, fs.monitors[1].object);

    let outer = g
        .safepoints
        .iter()
        .position(|s| s.bci == 7)
        .expect("a snapshot at the outer monitorexit");
    let fs = resolve_snapshot(&g, outer);
    assert_eq!(fs.monitors.len(), 1);
    assert_eq!(fs.monitors[0].lock_depth, 1);
    assert!(!fs.monitors[0].relock);
}

/// Every deopt frame this tier lowers is FLAT — one scope, `caller: None`.
///
/// That was already true of every compile before round 10 wave 9; what
/// changed is that it is now true by construction rather than because the
/// scope table nobody populated happened to be empty. `ir::InlineScopeTable`
/// and `Lowerer::caller_chain_for` were retired together (the read side was
/// wired to production and the write side had no caller outside
/// `#[cfg(test)]`, which made a `rg` for the table report inlined scope
/// support this tier has never had). The tests that exercised chain
/// building went with them; what they were really covering — that an
/// unresumable CALLER sinks the whole point, and that the walk is bounded —
/// is `deopt.rs`'s own coverage of `frame_state_is_resumable`, which still
/// walks `FrameState::caller` because the single-pass producer can build
/// one.
#[test]
pub(in crate::ir_lower::tests) fn every_lowered_frame_is_flat() {
    let (mut g, _k) = scope_fixture_graph();
    g.safepoints
        .push(SafepointSnapshot::new(4, vec![NO_NODE], vec![], Vec::new()));
    let fs = resolve_snapshot(&g, 0);
    assert_eq!(fs.bci, 4);
    assert!(
        fs.caller.is_none(),
        "this tier publishes no caller chains, and says so in \
             `resolve_frame_state`"
    );
    assert!(crate::deopt::frame_state_is_resumable(&fs));
}

/// Source witness: no `DeoptimizationPoint` literal in this file may omit
/// `semantics`. The two guard sites bake their point's *address* into
/// emitted code and are never handed back to a test, so the field's
/// presence there cannot be asserted behaviourally.
#[test]
pub(in crate::ir_lower::tests) fn every_deopt_point_literal_names_its_semantics() {
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    );
    let literals = src.matches("DeoptimizationPoint {").count();
    let stamped = src.matches("semantics: ResumeSemantics::").count();
    assert!(
        stamped >= literals,
        "{literals} `DeoptimizationPoint` literal(s) but only {stamped} \
             `semantics:` initialiser(s) — a construction site is inferring \
             the re-execute convention again instead of recording it"
    );
}

// ── Linear-scan register residency ──────────────────────────────────

/// `double f(double a) { return a * a * 2.0 + a; }` as a hand-built graph.
///
/// Hand-built rather than compiled from `dload_0; dmul; …`, because the
/// point of these tests is the ALLOCATION, and a bytecode-built graph
/// carries a `SafepointSnapshot` per bci whose interaction with pinning is
/// the subject of its own test below.
pub(in crate::ir_lower::tests) fn fp_chain_graph() -> (Graph, Schedule) {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Double, vec![start], Some(0));
    let sq = graph.add(Op::Mul, IrType::Double, vec![a, a], Some(1));
    let two = graph.add(
        Op::ConstF(2.0f64.to_bits()),
        IrType::Double,
        vec![],
        Some(2),
    );
    let scaled = graph.add(Op::Mul, IrType::Double, vec![sq, two], Some(3));
    let sum = graph.add(Op::Add, IrType::Double, vec![scaled, a], Some(4));
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(5));
    let schedule = ir_schedule::schedule(&graph);
    (graph, schedule)
}

/// A RIP-relative displacement must resolve to the word it names.
///
/// The one property both RIP emitters rest on, and the one whose failure
/// is silent: x86-64 adds the displacement to the address of the NEXT
/// instruction, so `here + len + disp` has to BE the target. A `len` that
/// forgets the trailing immediate — 6 instead of 7 for the poll's `imm8`,
/// or 6 instead of 10 for the guard's `imm32` — still assembles, still
/// runs, and reads a neighbouring global forever.
#[test]
pub(in crate::ir_lower::tests) fn a_rip_displacement_resolves_to_its_target() {
    // Both encoded lengths in use: the poll's `F6 05 <disp32> <imm8>` and
    // the epoch guard's `81 3D <disp32> <imm32>`.
    for len in [7usize, 10] {
        for (here, target) in [
            (0x1_0000usize, 0x2_0000usize), // forward
            (0x2_0000, 0x1_0000),           // backward
            (0x1000, 0x1000),               // degenerate: names itself
            (0usize, i32::MAX as usize),    // the far positive edge
        ] {
            let disp = rip_disp32(here, len, target)
                .unwrap_or_else(|| panic!("len={len} here={here:#x} should be in reach"));
            let next = here.wrapping_add(len);
            // Cast: two's-complement re-add of a signed displacement.
            let resolved = (next as i64).wrapping_add(disp as i64) as usize;
            assert_eq!(
                resolved, target,
                "len={len}: disp {disp} from {here:#x} resolved to {resolved:#x}, \
                     not {target:#x}",
            );
        }
    }
    // Out of ±2GB in both directions is refused, so the caller emits its
    // materialize-the-address fallback rather than a truncated disp32.
    assert!(rip_disp32(0, 10, u32::MAX as usize + 1).is_none());
    assert!(rip_disp32(usize::MAX / 2, 10, 0).is_none());
}

/// The GP file may not name a register this emitter uses for a FIXED
/// purpose, on either ABI.
///
/// The argument that RSI/RDI are promotable on Win64 rests entirely on
/// them being neither an argument register nor a deopt-trampoline
/// argument THERE, while being both on System V — a claim about six
/// `#[cfg]`-selected constants that no compiler checks and that a reader
/// has to assemble by hand. It is checked here instead, and it is checked
/// on whichever platform the tests run on, so the arm that is wrong is the
/// arm that fails.
///
/// This is the invariant whose absence produced the two clobber gaps
/// `docs/jit/linear-scan-wiring.md` records under "Calls the op model does
/// not see": a register the allocator believed survived a call, destroyed
/// by an emitter that never told the model it writes there.
#[test]
pub(in crate::ir_lower::tests) fn the_gp_file_names_no_register_this_emitter_reserves() {
    let mut reserved: Vec<(u8, &str)> = vec![
        (RAX, "RAX — the GP value tier"),
        (RCX, "RCX — the GP value tier"),
        (RDX, "RDX — IDIV and FCmp's SETcc"),
        (R10, "R10 — thread pointer / shadow-stack scratch"),
        (R11, "R11 — safepoint flag and epoch-guard scratch"),
        (DEOPT_ARG0, "DEOPT_ARG0"),
        (DEOPT_ARG1, "DEOPT_ARG1"),
        (DEOPT_ARG2, "DEOPT_ARG2"),
    ];
    for &r in CALL_ARG_REGS.iter() {
        reserved.push((r, "CALL_ARG_REGS"));
    }
    for &r in ENTRY_ABI_REGS.iter() {
        reserved.push((r, "ENTRY_ABI_REGS"));
    }
    for &f in IR_LOWER_LS_GPRS.iter() {
        if let Some((_, why)) = reserved.iter().find(|(r, _)| *r == f) {
            panic!(
                "IR_GP_LINEAR_SCAN names r{f}, which this emitter reserves: {why}. \
                     A value promoted there is destroyed by an emission the \
                     MachineModel does not model as a clobber."
            );
        }
    }
    // …and the file is exactly as wide as the platform's callee-saved set
    // allows, so a future edit that adds a register has to come here.
    let expected = if cfg!(windows) { 7 } else { 5 };
    assert_eq!(
        IR_LOWER_LS_GPRS.len(),
        expected,
        "the GP file is {} on this platform, expected {expected}",
        IR_LOWER_LS_GPRS.len(),
    );
    // The narrow prefix the kill switch restores must be a PREFIX, or
    // `ir_gp_file()`'s slice is not the historical file.
    assert_eq!(
        &IR_LOWER_LS_GPRS[..crate::regalloc::xmm_roles::IR_GP_LINEAR_SCAN_NARROW],
        &[3u8, 12, 13, 14, 15][..],
        "the narrow prefix is no longer RBX + R12-R15",
    );
}

// ── Phi residency ────────────────────────────────────────────────
//
// A loop counter and a loop accumulator are `Op::Phi` at the loop header.
// While phis were excluded from the residency plan, the optimizing tier
// read both out of a frame word on every iteration while the baseline tier
// it supersedes coloured them into callee-saved GPRs -- the measured
// inversion `docs/JIT_OPTIMIZATION.md` tabulates at 2.7-3.2x on array
// kernels. These five tests pin what admitting them means, in the order the
// obligations fall due: the population moves, the sub-flag still takes it
// back, the GC obligation does not move, the parallel copy still leaves one
// home and one register per phi, and the emitted code computes the same
// answer either way.

/// `int sum(int n){ int s=0; for(int i=0;i<n;i++) s+=i; return s; }`
///
/// The smallest program with the shape this whole line of work is about:
/// exactly two loop-carried values, and both of them a phi at the header.
pub(in crate::ir_lower::tests) const COUNTED_LOOP_SUM: [u8; 23] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01,
    0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
];

/// `int f(int n){ int a=1,b=2; for(int i=0;i<n;i++){ int t=a; a=b; b=t; } return a; }`
///
/// The swap runs on the operand stack (`iload_1; iload_2; istore_1;
/// istore_2`), so it needs no temporary local -- which makes the loop
/// header's two phis each other's back-edge value, i.e. a copy CYCLE.
/// `f(n)` is 2 for odd `n` and 1 for even `n`.
pub(in crate::ir_lower::tests) const SWAP_LOOP: [u8; 25] = [
    0x04, 0x3c, // 0: iconst_1; istore_1        a = 1
    0x05, 0x3d, // 2: iconst_2; istore_2        b = 2
    0x03, 0x3e, // 4: iconst_0; istore_3        i = 0
    0x1d, 0x1a, // 6: iload_3; iload_0          loop header
    0xa2, 0x00, 0x0d, // 8: if_icmpge 21
    0x1b, 0x1c, // 11: iload_1; iload_2         push a, then b
    0x3c, 0x3d, // 13: istore_1; istore_2       a = b; b = old a
    0x84, 0x03, 0x01, // 15: iinc 3, 1
    0xa7, 0xff, 0xf4, // 18: goto 6
    0x1b, 0xac, // 21: iload_1; ireturn
    0, 0,
];

/// Every VALUE-typed `Op::Phi` in `graph`, in node order.
///
/// `is_value_ty` and not "every phi": `IrBuilder` builds a `Memory` phi at
/// every merge it builds a value phi at, and a counted loop therefore has
/// one at its header. It carries no runtime value (`RegClass::of` answers
/// `None` for `Memory`), it is never a residency candidate, and
/// `PhiCensus` excludes it for the same reason -- so a test that counted it
/// would be asserting against a different set from the one the census
/// reports.
pub(in crate::ir_lower::tests) fn phis_of(graph: &Graph) -> Vec<usize> {
    (0..graph.nodes.len())
        .filter(|&id| matches!(graph.nodes[id].op, Op::Phi) && is_value_ty(graph.nodes[id].ty))
        .collect()
}

/// Round 13 wave 13 (lane jitfix;
/// `r13w12-snapliv-loop-header-snapshot-doubles-as-the-osr-seed-list`): an
/// OSR entry at the header of `COUNTED_LOOP_SUM` is admitted while the
/// header snapshot names (seeds) every value the in-loop frame at bci 9
/// names, and refused once one of them is not seeded -- the shape of a
/// pruned dead header local whose pre-loop value an in-loop frame still
/// names. A frame in a block the entry cannot reach is never asked.
#[test]
pub(in crate::ir_lower::tests) fn r13w13_an_osr_entry_is_refused_when_a_reachable_frame_names_a_skipped_value(
) {
    let graph = IrBuilder::new(1, 3)
        .build(&COUNTED_LOOP_SUM, 21)
        .expect("the counted loop builds");
    let schedule = ir_schedule::schedule(&graph);
    let header = schedule
        .blocks
        .iter()
        .position(|b| {
            let ctrl = &graph.nodes[b.ctrl as usize];
            ctrl.bytecode_pc == Some(4) && matches!(ctrl.op, Op::Merge | Op::Region)
        })
        .expect("the loop header is a block");
    // Block 0 is the dominator tree's root, the method entry.
    let start = 0usize;
    assert_ne!(start, header, "the header is not the method entry");
    let flat_at = |bci: usize| {
        graph
            .safepoints
            .iter()
            .position(|sp| sp.bci == bci && !sp.is_splice_state())
            .expect("a snapshot at that bci")
    };
    let (at_header, in_loop) = (flat_at(4), flat_at(9));
    let named: std::collections::HashSet<NodeId> = graph.safepoints[at_header]
        .locals
        .iter()
        .copied()
        .filter(|&v| v != NO_NODE)
        .collect();
    let n = graph.safepoints[in_loop].locals[0];
    assert!(named.contains(&n), "`n` is named at the header");
    let ask = |sites: &[(usize, usize)], seeded: &std::collections::HashSet<NodeId>| {
        Lowerer::osr_entry_skipped_frame_value(
            &graph, &schedule, None, sites, header, header, false, seeded,
        )
    };
    let sites = [(header, in_loop)];
    assert_eq!(
        ask(&sites, &named),
        None,
        "every value the in-loop frame names is seeded"
    );
    let mut pruned = named.clone();
    pruned.remove(&n);
    assert_eq!(
        ask(&sites, &pruned),
        Some(n),
        "`n` is defined before the header (or is its unseeded φ) and nothing seeds it"
    );
    assert_eq!(
        ask(&[], &pruned),
        None,
        "no frame, no refusal (the kill switch)"
    );
    assert_eq!(
        ask(&[(start, in_loop)], &std::collections::HashSet::new()),
        None,
        "a frame in the method's entry block is not reachable from the header"
    );
}

/// Round 13 wave 13 (lane jitfix): the precise half of the same check. An
/// entry at the INNER header of a nested loop reaches the outer body's
/// frames only through the outer header, whose φs are then written by its
/// back edge -- so a frame there naming the outer `s` φ, which the inner
/// header does not name, must not refuse the entry (dominance alone would).
///
/// `static int f(int n){ int s=0; for(int i=0;i<n;i++) for(int j=0;j<n;j++) s+=j; return s; }`
#[test]
pub(in crate::ir_lower::tests) fn r13w13_an_inner_loop_entry_keeps_an_outer_phi_its_frames_name() {
    //  0: iconst_0  1: istore_1  2: iconst_0  3: istore_2
    //  4: iload_2   5: iload_0   6: if_icmpge 32
    //  9: iconst_0 10: istore_3
    // 11: iload_3  12: iload_0  13: if_icmpge 26
    // 16: iload_1  17: iload_3  18: iadd  19: istore_1
    // 20: iinc 3,1 23: goto 11
    // 26: iinc 2,1 29: goto 4   32: iload_1 33: ireturn
    let code: [u8; 36] = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x1a, 0x03, 0x3e, 0x1d, 0x1a, 0xa2, 0x00,
        0x0d, 0x1b, 0x1d, 0x60, 0x3c, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf4, 0x84, 0x02, 0x01, 0xa7,
        0xff, 0xe7, 0x1b, 0xac, 0, 0,
    ];
    let graph = IrBuilder::new(1, 4)
        .build(&code, 34)
        .expect("the nested loop builds");
    let schedule = ir_schedule::schedule(&graph);
    let header_at = |pc: usize| {
        schedule
            .blocks
            .iter()
            .position(|b| {
                let ctrl = &graph.nodes[b.ctrl as usize];
                ctrl.bytecode_pc == Some(pc) && matches!(ctrl.op, Op::Merge | Op::Region)
            })
            .expect("a loop header block")
    };
    let inner = header_at(11);
    let flat_at = |bci: usize| {
        graph
            .safepoints
            .iter()
            .position(|sp| sp.bci == bci && !sp.is_splice_state())
            .expect("a snapshot at that bci")
    };
    let named: std::collections::HashSet<NodeId> = graph.safepoints[flat_at(11)]
        .locals
        .iter()
        .copied()
        .filter(|&v| v != NO_NODE)
        .collect();
    // The outer body, between the outer test and the inner header.
    let outer_body = schedule
        .dom
        .idom(inner)
        .expect("the inner header has an idom");
    let at_9 = flat_at(9);
    let s_outer = graph.safepoints[at_9].locals[1];
    assert_eq!(
        graph.nodes[s_outer as usize].op,
        Op::Phi,
        "`s` at bci 9 is the outer φ"
    );
    assert!(
        !named.contains(&s_outer),
        "the inner header names its own `s` φ instead"
    );
    assert!(
        !schedule
            .dom
            .dominates(inner, schedule.node_to_block[s_outer as usize]),
        "the fast (dominance) path cannot accept the outer φ"
    );
    // Ask about `s_outer` alone: every other value the frame names counts
    // as seeded, whatever shape the builder gave it.
    let sp9 = &graph.safepoints[at_9];
    let mut seeded = named.clone();
    seeded.extend(
        sp9.locals
            .iter()
            .chain(sp9.stack.iter())
            .chain(sp9.monitors.iter())
            .copied()
            .filter(|&v| v != NO_NODE && v != s_outer),
    );
    let got = Lowerer::osr_entry_skipped_frame_value(
        &graph,
        &schedule,
        None,
        &[(outer_body, at_9)],
        inner,
        inner,
        false,
        &seeded,
    );
    assert_eq!(
        got, None,
        "the outer body is reached only through the outer header"
    );
    // The same frame at a site the entered path reaches BEFORE it re-enters
    // the outer header (the inner header block itself) does refuse.
    let before_the_outer_header = Lowerer::osr_entry_skipped_frame_value(
        &graph,
        &schedule,
        None,
        &[(inner, at_9)],
        inner,
        inner,
        false,
        &seeded,
    );
    assert_eq!(before_the_outer_header, Some(s_outer));
}

/// JIT round 18 wave 11 close (lane testfix18;
/// `r18-osr-frame-premise-withdraws-protected-loop-entry-FIXED-20261006.md`): a
/// frame listed at a block its values do not dominate is not a value the
/// entry skips. Same nested loop as above, entered at the OUTER header:
/// the inner header's frame (its `j` / `s` φs) paired with the outer body
/// block BEFORE the inner header -- a pair no honest site publishes, and
/// one the method-entry path misses just the same -- keeps the entry. The
/// round-17 premise check refused it (the walk from the outer header
/// reaches the outer body without entering the inner header).
#[test]
pub(in crate::ir_lower::tests) fn r18_testfix18_a_frame_its_site_does_not_dominate_keeps_the_entry()
{
    let code: [u8; 36] = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x1a, 0x03, 0x3e, 0x1d, 0x1a, 0xa2, 0x00,
        0x0d, 0x1b, 0x1d, 0x60, 0x3c, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf4, 0x84, 0x02, 0x01, 0xa7,
        0xff, 0xe7, 0x1b, 0xac, 0, 0,
    ];
    let graph = IrBuilder::new(1, 4)
        .build(&code, 34)
        .expect("the nested loop builds");
    let schedule = ir_schedule::schedule(&graph);
    let header_at = |pc: usize| {
        schedule
            .blocks
            .iter()
            .position(|b| {
                let ctrl = &graph.nodes[b.ctrl as usize];
                ctrl.bytecode_pc == Some(pc) && matches!(ctrl.op, Op::Merge | Op::Region)
            })
            .expect("a loop header block")
    };
    let (outer, inner) = (header_at(4), header_at(11));
    let outer_body = schedule
        .dom
        .idom(inner)
        .expect("the inner header has an idom");
    let flat_at = |bci: usize| {
        graph
            .safepoints
            .iter()
            .position(|sp| sp.bci == bci && !sp.is_splice_state())
            .expect("a snapshot at that bci")
    };
    let at_inner = flat_at(11);
    let named: std::collections::HashSet<NodeId> = graph.safepoints[flat_at(4)]
        .locals
        .iter()
        .copied()
        .filter(|&v| v != NO_NODE)
        .collect();
    let inner_phis: Vec<NodeId> = graph.safepoints[at_inner]
        .locals
        .iter()
        .copied()
        .filter(|&v| {
            v != NO_NODE
                && !named.contains(&v)
                && graph.nodes[v as usize].op == Op::Phi
                && schedule.node_to_block[v as usize] == inner
        })
        .collect();
    assert!(
        !inner_phis.is_empty(),
        "the inner header names φs of its own"
    );
    assert!(
        !schedule.dom.dominates(inner, outer_body),
        "the pair breaks the round-17 premise"
    );
    // Everything else the frame names counts as seeded.
    let mut seeded = named.clone();
    seeded.extend(
        graph.safepoints[at_inner]
            .locals
            .iter()
            .chain(graph.safepoints[at_inner].stack.iter())
            .chain(graph.safepoints[at_inner].monitors.iter())
            .copied()
            .filter(|&v| v != NO_NODE && !inner_phis.contains(&v)),
    );
    let got = Lowerer::osr_entry_skipped_frame_value(
        &graph,
        &schedule,
        None,
        &[(outer_body, at_inner)],
        outer,
        outer,
        false,
        &seeded,
    );
    assert_eq!(
        got, None,
        "the outer header dominates the inner φs: no skipped value"
    );
}

/// Interpreter round i2 wave 58, lane L2
/// (`CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_ORDER_USES`): at the INNER header of
/// the nested loop above, a primitive value defined in the outer body and
/// read past the entry only through memory-token slots owes the entry no
/// seed; a value read in a block the entered path reaches first, a value
/// φ reader, a reference, or a value with no token reader at all does not
/// qualify.
#[test]
pub(in crate::ir_lower::tests) fn i58_l2_a_value_read_past_an_inner_entry_only_as_a_memory_token_owes_no_seed(
) {
    // `static int f(int n){ int s=0; for(int i=0;i<n;i++) for(int j=0;j<n;j++) s+=j; return s; }`
    let code: [u8; 36] = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x1a, 0x03, 0x3e, 0x1d, 0x1a, 0xa2, 0x00,
        0x0d, 0x1b, 0x1d, 0x60, 0x3c, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf4, 0x84, 0x02, 0x01, 0xa7,
        0xff, 0xe7, 0x1b, 0xac, 0, 0,
    ];
    // A fresh graph and schedule, the inner header and the outer body, and
    // `place` to put a hand-added node in a block.
    let fresh = || {
        let graph = IrBuilder::new(1, 4)
            .build(&code, 34)
            .expect("the nested loop builds");
        let schedule = ir_schedule::schedule(&graph);
        let inner = schedule
            .blocks
            .iter()
            .position(|b| {
                let ctrl = &graph.nodes[b.ctrl as usize];
                ctrl.bytecode_pc == Some(11) && matches!(ctrl.op, Op::Merge | Op::Region)
            })
            .expect("the inner header is a block");
        let outer_body = schedule
            .dom
            .idom(inner)
            .expect("the inner header has an idom");
        (graph, schedule, inner, outer_body)
    };
    fn place(schedule: &mut crate::ir_schedule::Schedule, id: NodeId, b: usize) {
        let i = id as usize;
        if schedule.node_to_block.len() <= i {
            schedule.node_to_block.resize(i + 1, usize::MAX);
        }
        schedule.node_to_block[i] = b;
    }
    let volatile_read = |ty: IrType, tag: u8| Op::LoadStatic {
        class_id: 1,
        field_index: 0,
        type_tag: tag,
        is_volatile: ty == IrType::Int,
    };
    // `stop`, read in the outer body; `burnt`'s read names it as its
    // memory token inside the inner header block (reached first: a token
    // read anywhere is fine).
    let build = |ty: IrType| {
        let (mut graph, mut schedule, inner, outer_body) = fresh();
        let ctrl = schedule.blocks[outer_body].ctrl;
        let stop = graph.add(volatile_read(ty, b'Z'), ty, vec![ctrl, ctrl], None);
        place(&mut schedule, stop, outer_body);
        let inner_ctrl = schedule.blocks[inner].ctrl;
        let burnt = graph.add(
            volatile_read(IrType::Long, b'J'),
            IrType::Long,
            vec![inner_ctrl, stop],
            None,
        );
        place(&mut schedule, burnt, inner);
        (graph, schedule, inner, outer_body, stop)
    };
    let ask = |graph: &Graph, schedule: &crate::ir_schedule::Schedule, id: NodeId, entry: usize| {
        Lowerer::osr_entry_value_read_only_as_memory_order(graph, schedule, id, entry)
    };

    let (graph, schedule, inner, _, stop) = build(IrType::Int);
    assert!(
        ask(&graph, &schedule, stop, inner),
        "token readers only: no seed owed"
    );

    // A value reader in the value's own block runs only after the value is
    // computed again on every entered path.
    let (mut graph, mut schedule, inner, outer_body, stop) = build(IrType::Int);
    let test = graph.add(Op::Add, IrType::Int, vec![stop, stop], None);
    place(&mut schedule, test, outer_body);
    assert!(
        ask(&graph, &schedule, stop, inner),
        "the outer test reads a recomputed value"
    );

    // A value reader the entered path reaches first: refused.
    let (mut graph, mut schedule, inner, _, stop) = build(IrType::Int);
    let early = graph.add(Op::Add, IrType::Int, vec![stop, stop], None);
    place(&mut schedule, early, inner);
    assert!(
        !ask(&graph, &schedule, stop, inner),
        "a value read inside the inner loop"
    );

    // A value φ reader: refused.
    let (mut graph, mut schedule, inner, outer_body, stop) = build(IrType::Int);
    let ctrl = schedule.blocks[outer_body].ctrl;
    let phi = graph.add(Op::Phi, IrType::Int, vec![ctrl, stop], None);
    place(&mut schedule, phi, outer_body);
    assert!(
        !ask(&graph, &schedule, stop, inner),
        "a value φ reads it on an edge"
    );

    // A reference: refused, whatever its readers.
    let (graph, schedule, inner, _, stop) = build(IrType::Ref);
    assert!(
        !ask(&graph, &schedule, stop, inner),
        "a reference is never left unseeded"
    );

    // No token reader at all: not this excuse's shape.
    let (mut graph, mut schedule, inner, outer_body) = fresh();
    let ctrl = schedule.blocks[outer_body].ctrl;
    let lone = graph.add(Op::Add, IrType::Int, vec![ctrl, ctrl], None);
    place(&mut schedule, lone, outer_body);
    assert!(
        !ask(&graph, &schedule, lone, inner),
        "no memory-token reader"
    );
}

// A value live across a helper call must not keep a caller-saved register.
//
// FP `Op::Rem` is the sharp case: it lowers to `CALL jit_drem`, and
// ── Shadow instruction selection ─────────────────────────────────
//
// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
// increment 0.
//
// Increment 0's whole claim is "turning this on changes no emitted byte".
// These are what hold it up. Note they drive the flag through
// `flags::with_thread_overrides` and NOT `std::env::set_var`: the flag is
// declared (`types/src/flag_groups.rs`, `jit/ir-isel-shadow`), so the
// override mechanism reaches it, and a process-wide env write would race
// every other test in this binary.

/// With the flag OFF — the default, and every compile today — the pass does
/// not run at all.
///
/// The counter, not the bytes: "changed no byte" and "did not execute" are
/// different claims and increment 0 makes both.
#[test]
pub(in crate::ir_lower::tests) fn shadow_selection_is_off_by_default() {
    let _guard = crate::x64::isel::SHADOW_TEST_LOCK.lock();
    crate::x64::isel::reset_shadow_totals();

    let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0xac, 0, 0];
    compile_via_ir(&code, 6, 2, 2).expect("compiles");

    let (methods, stats) = crate::x64::isel::shadow_totals();
    assert_eq!(methods, 0, "the shadow pass ran without being asked");
    assert_eq!(stats, crate::x64::isel::ShadowStats::default());
}

// ── Increment 2: the allocation over the tile list ───────────────

/// The machine list gets an allocation, and `verify_allocation` accepts it.
///
/// The point of the increment is *which* verifier: the allocator's own,
/// unmodified. A second checker written for the level-2 form would be a
/// second model of the same program, and the whole reason the frame-homed
/// allocation is describable at all is that `Allocation` already has a
/// shape for "everything in its home word".
///
/// Not vacuous by construction — `located` is asserted non-zero, which is
/// the difference between "verified" and "there was nothing to verify".
#[test]
pub(in crate::ir_lower::tests) fn the_machine_list_carries_an_allocation_that_verify_allocation_accepts(
) {
    let (graph, schedule) = mir_alu_graph();
    let plan = plan_slots(&graph, &schedule, None);
    let mir = build_mir_plan(&graph, &schedule).expect("selection covers every block");

    let verdict =
        verify_mir_allocation(&graph, &schedule, &plan, &mir).expect("the allocation verifies");
    match verdict {
        MirAllocVerdict::Verified { located, .. } => assert!(
            located >= 3,
            "`(a + b) * a` has two params and two results; the allocation \
                 located only {located} values, so this test is close to vacuous"
        ),
        other => panic!("expected a verified allocation, got {other:?}"),
    }
}

/// The verifier is REACHED, and it can say no.
///
/// A verification step nothing can fail is indistinguishable from no
/// verification step. This one runs over an allocation whose every segment
/// is `reg: None` — the arm where most of `verify_allocation`'s rules go
/// quiet — so what gets injected against is the rule that does *not* go
/// quiet: two values whose live ranges overlap, pointed at one home word.
///
/// The violation is planted in the `SlotPlan`, UPSTREAM of
/// `verify_mir_allocation`, so the whole path is under test rather than
/// `verify_allocation` in isolation. Only the colour's *value* changes,
/// never its `Some`-ness, so the liveness-agreement precheck still passes
/// and the rejection can only have come from the verifier.
///
/// The exact edit that trips it: drop the `verify_allocation` call from
/// `verify_mir_allocation`.
#[test]
pub(in crate::ir_lower::tests) fn an_aliased_home_word_is_rejected_by_the_allocation_verifier() {
    use crate::regalloc::build_live_model;

    let (graph, schedule) = mir_alu_graph();
    let plan = plan_slots(&graph, &schedule, None);
    let mir = build_mir_plan(&graph, &schedule).expect("selection covers every block");
    // Precondition: unaliased, this graph verifies. Without it a broken
    // fixture would make the rejection below meaningless.
    assert!(
        matches!(
            verify_mir_allocation(&graph, &schedule, &plan, &mir),
            Ok(MirAllocVerdict::Verified { .. })
        ),
        "the fixture must verify BEFORE the injection"
    );

    // Two values genuinely live at the same position, so the rejection is
    // the aliasing rule and not some other one.
    let live = build_live_model(&graph, &schedule);
    let located: Vec<usize> = (0..graph.nodes.len())
        .filter(|&id| plan.node_color.get(id).copied().flatten().is_some())
        .filter(|&id| live.range.get(id).copied().flatten().is_some())
        .collect();
    let mut pair = None;
    'outer: for (i, &a) in located.iter().enumerate() {
        let ra = live.range[a].expect("located");
        for &b in &located[i + 1..] {
            let rb = live.range[b].expect("located");
            if ra.lo <= rb.hi && rb.lo <= ra.hi {
                pair = Some((a, b));
                break 'outer;
            }
        }
    }
    let (a, b) = pair.expect("this graph has two simultaneously live values");

    let mut aliased = plan_slots(&graph, &schedule, None);
    aliased.node_color[b] = aliased.node_color[a];
    let err = verify_mir_allocation(&graph, &schedule, &aliased, &mir)
        .expect_err("two values live together in one home word");
    let text = format!("{err:?}");
    assert!(
        text.contains("home word") || text.contains("slot pools"),
        "rejected for the wrong reason: {text}"
    );
}

/// The oracle can FAIL. Injected, because a guard nobody has seen fail is a
/// guard of unknown polarity — and this one's whole job is to be believed
/// when it stays silent over a corpus.
///
/// Both halves matter: verify mode must *count* the disagreement, and the
/// compile must be *refused* rather than shipped with a body nothing
/// checked. Increment 2 is where fail-closed returns.
#[test]
pub(in crate::ir_lower::tests) fn an_injected_wrong_byte_is_caught_by_the_oracle() {
    let _guard = mir_totals::TEST_LOCK.lock();
    mir_totals::reset();

    let _inject = MirInject::on();
    let _force = MirForce::set(MirMode::Verify);
    let refused = compile_via_ir(&MIR_ALU_CODE, 6, 2, 2);
    assert!(
        refused.is_none(),
        "a disagreeing method must lose its optimized body, not ship it"
    );
    let (_, _, mismatches) = mir_totals::read();
    assert!(
        mismatches >= 1,
        "the byte-equality oracle did not notice a corrupted tile"
    );
}

/// With both flags off — the default, and every compile today — the machine
/// list is not built at all.
///
/// The counter, not the bytes: "changed no byte" and "did not execute" are
/// different claims, and the equality tests above would pass vacuously on a
/// path that never ran.
#[test]
pub(in crate::ir_lower::tests) fn the_machine_level_is_off_by_default() {
    let _guard = mir_totals::TEST_LOCK.lock();
    mir_totals::reset();

    compile_via_ir(&MIR_ALU_CODE, 6, 2, 2).expect("compiles");

    assert_eq!(
        mir_totals::read(),
        (0, 0, 0),
        "the machine list was built without being asked"
    );
}

/// A tile that absorbed another node must never reach the encoder.
///
/// `Rule::AluReg` covers exactly its own root, so this guard is insurance
/// against a future rule rather than a live filter — which is precisely why
/// it is tested directly instead of through a compile. Driven through one it
/// would be vacuous: the rules that DO absorb (`Lea`, `AluImm`, the fused
/// branches) are already refused a line earlier, by rule.
///
/// What it prevents: the caller skips every node a tile covers, so a tile
/// that absorbed a node the encoder does not actually fold leaves that node
/// computed nowhere. That is the same failure `BlockSelection::covers`
/// exists to make unrepresentable, one level lower down.
///
/// The exact edit that trips it: delete the `covered.as_slice() == [id]`
/// clause from `mir_tile_is_emittable`.
#[test]
pub(in crate::ir_lower::tests) fn the_encoder_refuses_a_tile_that_absorbed_another_node() {
    use crate::x64::isel::{MInst, Op as SelOp, Rule, Tile, Ty};

    let alu = |dst: NodeId, lhs: NodeId, rhs: NodeId| MInst::AluRR {
        op: SelOp::Add,
        ty: Ty::I32,
        dst,
        lhs,
        rhs,
    };
    // The shape the encoder handles: one root, nothing absorbed.
    let plain = Tile::for_test(5, vec![5], vec![alu(5, 3, 4)], Rule::AluReg);
    assert!(mir_tile_is_emittable(&plain, 5));

    // The same tile, having absorbed node 4.
    let absorbing = Tile::for_test(5, vec![5, 4], vec![alu(5, 3, 4)], Rule::AluReg);
    assert!(
        !mir_tile_is_emittable(&absorbing, 5),
        "a tile that absorbed node 4 would leave it computed nowhere"
    );

    // And a rule the encoder has not been proved byte-equal for, however
    // ordinary its cover list.
    let other = Tile::for_test(5, vec![5], vec![alu(5, 3, 4)], Rule::AluImm);
    assert!(!mir_tile_is_emittable(&other, 5));
    // A tile rooted somewhere else is never this node's answer.
    assert!(!mir_tile_is_emittable(&plain, 4));
}

/// The two frame-access encoders are the ONE producer of these bytes.
///
/// `load_to_rax` / `load_to_rcx` / `store_rax` delegate to them, so the
/// level-2 encoder and the per-opcode arms cannot drift apart. This pins the
/// literals those methods used to hold inline, in both displacement forms.
#[test]
pub(in crate::ir_lower::tests) fn the_frame_access_encoders_reproduce_the_inline_literals() {
    let bytes = |f: fn(u8, i32, &mut FrameAccess), reg: u8, off: i32| -> Vec<u8> {
        let mut a = FrameAccess::new();
        f(reg, off, &mut a);
        a.as_slice().to_vec()
    };
    // disp8: mod=01, r/m=RBP(101).
    assert_eq!(bytes(enc_frame_load, RAX, 8), vec![0x48, 0x8B, 0x45, 0xF8]);
    assert_eq!(bytes(enc_frame_load, RCX, 8), vec![0x48, 0x8B, 0x4D, 0xF8]);
    assert_eq!(bytes(enc_frame_store, RAX, 8), vec![0x48, 0x89, 0x45, 0xF8]);
    // disp32: mod=10.
    let mut want = vec![0x48u8, 0x8B, 0x85];
    want.extend_from_slice(&(-4096i32).to_le_bytes());
    assert_eq!(bytes(enc_frame_load, RAX, 4096), want);
    let mut want = vec![0x48u8, 0x89, 0x85];
    want.extend_from_slice(&(-4096i32).to_le_bytes());
    assert_eq!(bytes(enc_frame_store, RAX, 4096), want);
    // A register these encodings cannot express is a refusal, not a
    // silently truncated `reg & 7` — which would encode R8 as RAX.
    assert!(bytes(enc_frame_load, 8, 8).is_empty());
    assert!(bytes(enc_frame_store, 8, 8).is_empty());
}

// ── The three `ir::Op` enumerations, checked against each other ──
//
// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
// "The cheap alternative". One set of operations is enumerated
// in three places, in two different vocabularies:
//
//   1. `lower_data_node`'s match arms          — 48 of 53 `ir::Op` variants
//   2. `op_defines_result_slot`                — 37
//   3. `ir::ir_compatible`                     — in the BYTECODE's
//      vocabulary (`scan.anewarray_ops`, …), not `Op`'s
//
// (1) and (2) are checked here. (3) cannot be: it answers a different
// question about a different type, which is precisely why the catch-all's
// old claim — "bail in `ir_compatible` prevents reaching here" — was not
// checkable where it was written, and why `lower_data_node`'s final arm now
// refuses instead of asserting.
//
// WHAT TRIPS THESE (rule 5 of `docs/feature-designs/jit-ir-optimizations.md`: write down
// the exact edit, or the test is decoration):
//
//   * Add a variant to `ir::Op`. `declared_lowering` is an exhaustive match
//     with NO wildcard arm, so the crate stops compiling until the author
//     classifies it. That is the primary forcing function and it fires at
//     build time, not test time.
//   * Classify it `Lowered*` but forget the `lower_data_node` arm.
//     `every_ir_op_is_lowered_or_declared_unlowerable` fails: the source
//     scan finds no arm naming it.
//   * Add the arm but forget `op_defines_result_slot`.
//     `op_defines_result_slot_matches_the_arms_that_allocate` fails.
//   * Remove a variant from `ir::Op` and leave it here.
//     `the_op_representatives_cover_every_declared_variant` fails.
//
// All three scans read the source with `include_str!`, the same idiom as
// `the_register_read_path_is_gated_on_publication` above.

/// What `lower_data_node` does with one `ir::Op`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower::tests) enum OpLowering {
    /// Has an arm, and that arm allocates a result slot. Must therefore
    /// also be named by `op_defines_result_slot`.
    LoweredValue,
    /// Has an arm, but produces no value anyone can read: a control node, a
    /// memory-only effect, or a terminator handled by `lower_terminator`.
    LoweredEffect,
    /// No arm. Reaching `lower_data_node` with one refuses the compile.
    Unlowerable,
}

/// The variants with no lowering arm — the explicit list the catch-all
/// used to leave implicit.
///
/// Each is unreachable from a real compile today (see the catch-all's own
/// comment for the evidence per op). This is a statement about the tree,
/// not a permission: a variant added here is a variant the optimizing tier
/// silently declines to compile, and the edit should be visible in review.
///
/// `NewArray` left this list in cov-06: it now has a real arm below (the
/// shared `emit_new_array_stub`, mirroring `Op::New`).
pub(in crate::ir_lower::tests) const UNLOWERABLE: [&str; 3] = ["I2B", "I2C", "I2S"];

/// One representative value per `ir::Op` variant, with the variant's name.
///
/// The names are what the source scans compare against;
/// `the_op_representatives_cover_every_declared_variant` proves the list is
/// complete against `ir.rs` itself, so it cannot quietly fall behind.
pub(in crate::ir_lower::tests) fn op_representatives() -> Vec<(&'static str, Op)> {
    use crate::ir::CmpOp;
    vec![
        ("Start", Op::Start),
        ("Return", Op::Return),
        ("If", Op::If),
        ("Switch", Op::Switch { low: 0, high: 1 }),
        ("Merge", Op::Merge),
        ("Region", Op::Region),
        ("Proj", Op::Proj(0)),
        ("Const", Op::Const(0)),
        ("ConstF", Op::ConstF(0)),
        ("Param", Op::Param(0)),
        ("Phi", Op::Phi),
        ("ScalarIntrinsic", Op::ScalarIntrinsic(ScalarOp::MaxI)),
        (
            "Unbox",
            Op::Unbox {
                op: crate::ir::UnboxOp::LongValue,
                class_id: 1,
            },
        ),
        ("Add", Op::Add),
        ("Sub", Op::Sub),
        ("Mul", Op::Mul),
        ("Div", Op::Div),
        ("Rem", Op::Rem),
        ("Neg", Op::Neg),
        ("And", Op::And),
        ("Or", Op::Or),
        ("Xor", Op::Xor),
        ("Shl", Op::Shl),
        ("Shr", Op::Shr),
        ("UShr", Op::UShr),
        ("Cmp", Op::Cmp(CmpOp::Eq)),
        ("LCmp", Op::LCmp),
        (
            "FCmp",
            Op::FCmp {
                double: false,
                nan_greater: false,
            },
        ),
        ("I2L", Op::I2L),
        ("L2I", Op::L2I),
        ("I2F", Op::I2F),
        ("I2D", Op::I2D),
        ("L2F", Op::L2F),
        ("L2D", Op::L2D),
        ("F2I", Op::F2I),
        ("F2L", Op::F2L),
        ("F2D", Op::F2D),
        ("D2I", Op::D2I),
        ("D2L", Op::D2L),
        ("D2F", Op::D2F),
        ("I2B", Op::I2B),
        ("I2C", Op::I2C),
        ("I2S", Op::I2S),
        ("Load", Op::Load(MemKind::Int)),
        ("Store", Op::Store(MemKind::Int)),
        ("ArrayLength", Op::ArrayLength),
        ("ArrayLoad", Op::ArrayLoad(MemKind::Int)),
        ("ArrayStore", Op::ArrayStore(MemKind::Int)),
        ("StrCharDecode", Op::StrCharDecode),
        ("ByteArrayCopy", Op::ByteArrayCopy),
        (
            "New",
            Op::New {
                class_id: 0,
                num_fields: 0,
            },
        ),
        (
            "NewArray",
            Op::NewArray {
                element_type: 10,
                component_class_id: 0,
            },
        ),
        ("Call", Op::Call { info_ptr: 0 }),
        (
            "ConstString",
            Op::ConstString {
                holder_class_id: 0,
                cp_idx: 0,
                slot_addr: 0,
            },
        ),
        (
            "ConstClass",
            Op::ConstClass {
                holder_class_id: 0,
                cp_idx: 0,
                slot_addr: 0,
            },
        ),
        (
            "LoadStatic",
            Op::LoadStatic {
                class_id: 0,
                field_index: 0,
                type_tag: b'I',
                is_volatile: false,
            },
        ),
        (
            "StoreStatic",
            Op::StoreStatic {
                class_id: 0,
                field_index: 0,
                type_tag: b'I',
                is_volatile: false,
            },
        ),
        ("LambdaIntToDouble", Op::LambdaIntToDouble),
        ("IndyBridge", Op::IndyBridge { site: 0 }),
        ("MultiNewArray", Op::MultiNewArray { site: 0 }),
        (
            "NewCp",
            Op::NewCp {
                holder_class_id: 0,
                cp_idx: 0,
            },
        ),
        (
            "InstanceOf",
            Op::InstanceOf {
                name_ptr: 0,
                name_len: 0,
                seen_cell: 0,
                header_only: false,
            },
        ),
        (
            "CheckCast",
            Op::CheckCast {
                name_ptr: 0,
                name_len: 0,
            },
        ),
        ("MonitorEnter", Op::MonitorEnter),
        ("MonitorExit", Op::MonitorExit),
        ("Throw", Op::Throw),
        ("Guard", Op::Guard { bci: 0 }),
        ("ExactClassIs", Op::ExactClassIs { class_id: 1 }),
        ("ClassWordOf", Op::ClassWordOf),
        ("OsrEntryFlag", Op::OsrEntryFlag { header_bci: 4 }),
        ("OsrLocal", Op::OsrLocal(1)),
        ("OsrMemory", Op::OsrMemory),
        (
            "CatchEntryFlag",
            Op::CatchEntryFlag {
                call_bci: 4,
                site: 0,
            },
        ),
        ("CaughtException", Op::CaughtException),
        (
            "ClassInitTest",
            Op::ClassInitTest {
                state_addr: 0x1000,
                initialized: 2,
            },
        ),
        ("KeepAlive", Op::KeepAlive),
        ("CurrentThreadMirror", Op::CurrentThreadMirror { offset: 8 }),
        (
            "VthreadBoundaryTest",
            Op::VthreadBoundaryTest { offset: 8, mask: 1 },
        ),
        ("IndyRootRef", Op::IndyRootRef { slot_addr: 0x1000 }),
        ("CallThrew", Op::CallThrew { site: 0, index: 0 }),
        ("Dead", Op::Dead),
    ]
}

/// `op_home_is_one_store_rax` claims a property of SOURCE the compiler
/// cannot check, and it is the whole safety argument for dropping the home
/// word of a value that is not a phi: the arm writes its result home
/// exactly once, through `store_rax`, with RAX holding the value. So check
/// the source.
///
/// A second home write on another path would publish the register on one
/// path and leave it stale on the other; a home reached by
/// `gp_store_value` or `emit_store_frame_imm32` instead would not go
/// through `publish_def_at_store` at all, so nothing would publish the
/// register and the value would exist nowhere. `fp_store_value` is allowed
/// and deliberately so — `Op::Div`, `Op::Rem` and `Op::Neg` take an FP path
/// that returns early, and `value_home_droppable` admits only `Int` and
/// `Long`, so that path is unreachable for a dropped home.
///
/// A failure here is not cosmetic. It says the allowlist has drifted from
/// the arms and `CRATONVM_JIT_IR_DROP_HOME` would emit a body that never
/// writes a value it later reads.
/// Every op [`op_preserves_rcx`] claims must have an arm that cannot write
/// RCX — checked against the arm's SOURCE, not against the op's name.
///
/// The reason this test exists is a concrete near-miss: the first version of
/// that allowlist claimed `Add`, `Sub`, `And`, `Or`, `Xor` and `Neg`, and
/// **all six write RCX** — the binary family through
/// `gp_load_value(RCX, node.inputs[1])`, `Neg` through the sign mask in its
/// `IrType::Double` arm. The full unit suite and the `ir_vs_singlepass`
/// differential suite both passed with it in, because a deferred carry that
/// is never honoured fails CLOSED (`slot_of_checked` refuses a dropped
/// home) rather than miscompiling. Nothing would have said so.
///
/// Two checks, because either alone can be fooled:
///
///   * no `RCX` identifier anywhere in the arm — catches every helper call
///     that names the register;
///   * the arm's raw `buf.emit(&[..])` byte literals are exactly the ones
///     recorded here — catches a ModRM byte that encodes RCX as a
///     destination, which no identifier scan can see.
/// `cmp_reg_reg_bytes` against hand-checked encodings.
///
/// A swapped ModRM field here compares two registers that both exist, so
/// the failure is a plausible wrong branch and not a fault — which is why
/// the vectors are written out rather than derived by the same arithmetic
/// the function uses.
///
/// The first two are the sequences this replaced, so a regression that
/// changes the low-register case shows up as a byte difference.
#[test]
pub(in crate::ir_lower::tests) fn cmp_reg_reg_encodes_the_known_forms() {
    // CMP EAX, ECX — the exact two bytes the fused compare emitted before.
    assert_eq!(cmp_reg_reg_bytes(RAX, RCX, false), ([0x39, 0xC8, 0], 2));
    // CMP RAX, RCX — and the exact three of the ref/64-bit form.
    assert_eq!(cmp_reg_reg_bytes(RAX, RCX, true), ([0x48, 0x39, 0xC8], 3));
    // CMP EBX, R12D — REX.R extends the SECOND operand into r8..r15.
    assert_eq!(cmp_reg_reg_bytes(3, 12, false), ([0x44, 0x39, 0xE3], 3));
    // CMP R13, RBX — REX.B extends the FIRST operand.
    assert_eq!(cmp_reg_reg_bytes(13, 3, true), ([0x49, 0x39, 0xDD], 3));
    // Both extended, 64-bit: REX.W|REX.R|REX.B.
    assert_eq!(cmp_reg_reg_bytes(14, 15, true), ([0x4D, 0x39, 0xFE], 3));
}

/// The operand ORDER must survive the r/m-versus-reg inversion: `CMP a, b`
/// sets the flags of `a - b`, which is what `CmpCond::x64_cc` reads.
///
/// Stated as a distinct test because the encoding one above would still
/// pass if both the function and its vectors were transposed together.
/// `CMP EAX, ECX` is `39 C8` in every reference; `CMP ECX, EAX` is `39 C1`.
#[test]
pub(in crate::ir_lower::tests) fn cmp_reg_reg_puts_the_first_operand_in_rm() {
    assert_eq!(cmp_reg_reg_bytes(RAX, RCX, false), ([0x39, 0xC8, 0], 2));
    assert_eq!(cmp_reg_reg_bytes(RCX, RAX, false), ([0x39, 0xC1, 0], 2));
}

/// `cmp_reg_imm_bytes` against hand-checked encodings.
///
/// The `/digit` is the whole risk and it is a silent one: `/7` is CMP, but
/// `/0` in the same two opcodes is ADD and `/5` is SUB, and both of those
/// WRITE the register. A transposed digit would not fault — it would
/// increment the loop counter under the comparison.
#[test]
pub(in crate::ir_lower::tests) fn cmp_reg_imm_encodes_the_known_forms() {
    // CMP EBX, 100 — the `i < 100` this exists for. `83 /7 ib`.
    assert_eq!(
        &cmp_reg_imm_bytes(3, 100, false).0[..3],
        &[0x83, 0xFB, 0x64]
    );
    // CMP EAX, 0 — the low-register, zero-constant corner still uses the
    // short form and emits no REX.
    assert_eq!(
        &cmp_reg_imm_bytes(RAX, 0, false).0[..3],
        &[0x83, 0xF8, 0x00]
    );
    // CMP RBX, 100 — REX.W only.
    assert_eq!(
        &cmp_reg_imm_bytes(3, 100, true).0[..4],
        &[0x48, 0x83, 0xFB, 0x64]
    );
    // CMP R12D, 100 — REX.B extends the r/m field, which is where the sole
    // operand lives. REX.R is never set here: `/7` occupies `reg`.
    assert_eq!(
        &cmp_reg_imm_bytes(12, 100, false).0[..4],
        &[0x41, 0x83, 0xFC, 0x64]
    );
    // CMP EAX, 4096 — too wide for `imm8`, so `81 /7 id`, six bytes.
    assert_eq!(
        cmp_reg_imm_bytes(RAX, 4096, false),
        ([0x81, 0xF8, 0x00, 0x10, 0x00, 0x00, 0x00], 6)
    );
    // -128 is the last `imm8`; -129 is not. The boundary is worth pinning
    // because an off-by-one there emits a six-byte instruction where a
    // three-byte one was correct, which no test of behaviour would catch.
    assert_eq!(cmp_reg_imm_bytes(RAX, -128, false).1, 3);
    assert_eq!(cmp_reg_imm_bytes(RAX, -129, false).1, 6);
}

/// `cmp_frame_imm_bytes` against hand-checked encodings — the form that
/// names no register at all.
#[test]
pub(in crate::ir_lower::tests) fn cmp_frame_imm_encodes_the_known_forms() {
    // CMP DWORD [RBP-60h], 100. mod=01, /7, r/m=RBP, disp8 = -0x60.
    assert_eq!(
        &cmp_frame_imm_bytes(0x60, 100, false).0[..4],
        &[0x83, 0x7D, 0xA0, 0x64]
    );
    // CMP QWORD [RBP-60h], 100 — REX.W, and nothing else changes.
    assert_eq!(
        &cmp_frame_imm_bytes(0x60, 100, true).0[..5],
        &[0x48, 0x83, 0x7D, 0xA0, 0x64]
    );
    // A slot past the disp8 reach takes mod=10 and a disp32.
    assert_eq!(
        &cmp_frame_imm_bytes(0x200, 1, false).0[..7],
        &[0x83, 0xBD, 0x00, 0xFE, 0xFF, 0xFF, 0x01]
    );
    // Both fields long: disp32 and imm32, eleven bytes with REX.W.
    assert_eq!(cmp_frame_imm_bytes(0x200, 4096, true).1, 11);
}

/// `lea_reg_base_disp_bytes` against hand-checked encodings.
///
/// Two encodings here are wrong in ways that do not fault, which is why
/// they are written out rather than derived: an R12 base without its SIB
/// byte reads `[disp]` through a different addressing form, and an R13
/// base with `mod=00` is RIP-relative.
#[test]
pub(in crate::ir_lower::tests) fn lea_base_disp_encodes_the_known_forms() {
    // LEA R14D, [RBX+1] — the `i + 1` this exists for.
    assert_eq!(
        &lea_reg_base_disp_bytes(14, 3, 1, false).0[..4],
        &[0x44, 0x8D, 0x73, 0x01]
    );
    // LEA R14, [RBX+1] — the `long` form, REX.W added.
    assert_eq!(
        &lea_reg_base_disp_bytes(14, 3, 1, true).0[..4],
        &[0x4C, 0x8D, 0x73, 0x01]
    );
    // LEA EAX, [RBX-1] — `x - k` arrives here as a negative displacement.
    assert_eq!(
        &lea_reg_base_disp_bytes(RAX, 3, -1, false).0[..3],
        &[0x8D, 0x43, 0xFF]
    );
    // LEA EBX, [R12+1] — base r/m 100 means "SIB follows", so one must.
    assert_eq!(
        &lea_reg_base_disp_bytes(3, 12, 1, false).0[..5],
        &[0x41, 0x8D, 0x5C, 0x24, 0x01]
    );
    // LEA EBX, [R13+0] — r/m 101 has no mod=00 form, so the zero
    // displacement is emitted rather than elided.
    assert_eq!(
        &lea_reg_base_disp_bytes(3, 13, 0, false).0[..4],
        &[0x41, 0x8D, 0x5D, 0x00]
    );
    // A displacement past `imm8` takes mod=10 and four bytes.
    assert_eq!(lea_reg_base_disp_bytes(RAX, 3, 4096, false).1, 6);
    assert_eq!(lea_reg_base_disp_bytes(RAX, 3, 127, false).1, 3);
    assert_eq!(lea_reg_base_disp_bytes(RAX, 3, 128, false).1, 6);
}

/// `LEA` must not be transposed: the destination is the `reg` field and the
/// base is `r/m`, the OPPOSITE assignment from `cmp_reg_imm_bytes`, where
/// the sole operand is the `r/m` and `reg` is an opcode extension.
///
/// Stated separately because the vectors above would all still pass if the
/// function and its expectations were transposed together.
#[test]
pub(in crate::ir_lower::tests) fn lea_puts_the_destination_in_reg_and_the_base_in_rm() {
    // LEA EAX, [RCX+0] is `8D 41 00`; LEA ECX, [RAX+0] is `8D 48 00`.
    assert_eq!(
        &lea_reg_base_disp_bytes(RAX, RCX, 0, false).0[..3],
        &[0x8D, 0x41, 0x00]
    );
    assert_eq!(
        &lea_reg_base_disp_bytes(RCX, RAX, 0, false).0[..3],
        &[0x8D, 0x48, 0x00]
    );
    // And the REX bits follow the same split: extending the DESTINATION
    // sets REX.R (0x44), extending the BASE sets REX.B (0x41).
    assert_eq!(lea_reg_base_disp_bytes(14, 3, 0, false).0[0], 0x44);
    assert_eq!(lea_reg_base_disp_bytes(3, 14, 0, false).0[0], 0x41);
}

/// JIT round 25 item 1a: the scan that keeps a branch from entering a
/// tail carry's read ignores exactly the rel32 fields it is told to (the
/// post-call exit stubs' way back) and still sees every other branch.
#[test]
pub(in crate::ir_lower::tests) fn r25_1a_the_branch_scan_exempts_only_the_named_fields() {
    // 0: JMP rel32 -> 10; 5: JZ rel32 (0F 84) -> 10; 11..: NOPs.
    let mut code = vec![0xE9, 5, 0, 0, 0, 0x0F, 0x84, 0, 0, 0, 0];
    code.extend_from_slice(&[0x90; 8]);
    // The JZ's field is at 7: 7 + 4 + rel = 10 -> rel = -1.
    code[7..11].copy_from_slice(&(-1i32).to_le_bytes());
    let wanted: std::collections::HashSet<usize> = [10usize].into_iter().collect();
    // A base no 8-byte window of this code can equal.
    let base = 0x7000_0000_0000u64;
    let all = super::home_reload_targets(&code, base, &wanted);
    assert!(all.contains(&10), "the plain scan sees the branches to 10");
    let jz_only: std::collections::HashSet<usize> = [7usize].into_iter().collect();
    let still = super::home_reload_targets_except(&code, base, &wanted, &jz_only);
    assert!(
        still.contains(&10),
        "exempting the JZ's field must not hide the JMP that also reaches 10"
    );
    let both: std::collections::HashSet<usize> = [1usize, 7].into_iter().collect();
    let none = super::home_reload_targets_except(&code, base, &wanted, &both);
    assert!(
        none.is_empty(),
        "both fields exempt: nothing reaches 10 ({none:?})"
    );
}

/// JIT round 25 item 1a, review fix: the tail-carry plan leaves the home
/// undropped (the inline prefixes ask `home_dropped` before the store),
/// and `store_rax` drops it only where it elides, never after a prefix.
#[test]
pub(in crate::ir_lower::tests) fn r25_1a_a_tail_carry_drops_its_home_only_at_the_elided_store() {
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    )
    .replace("\r\n", "\n");
    let plan = src
        .split("fn plan_tail_carries(&mut self) {")
        .nth(1)
        .expect("plan_tail_carries is in this file")
        .split("\n    }\n")
        .next()
        .expect("the function ends");
    assert!(
        !plan.contains("self.home_dropped[i] = true"),
        "the plan must not drop a tail carry's home before its store"
    );
    let store = src
        .split("fn store_rax(&mut self, offset: i32) {")
        .nth(1)
        .expect("store_rax is in this file")
        .split("\n    }\n")
        .next()
        .expect("the function ends");
    let gate = store
        .find("let mut prefixed = self.call_prefix_join.is_some();")
        .expect("the prefix gate");
    // JIT round 26: a prefix lifts the gate only through the byte proof
    // that its hit edge carries the result in RAX.
    let proof = store
        .find("self.prefix_hit_proves_rax(offset)")
        .expect("the prefix's RAX proof");
    let drop = store
        .find("if let Some(cell) = self.home_dropped.get_mut(id as usize) {")
        .expect("the drop at the elision");
    assert!(
        gate < proof && proof < drop,
        "the elision is gated on no unproven prefix before it drops the home"
    );
}

/// JIT round 25 item 1a (item-2 review): the fall-through back-edge poll
/// -- the one `lower_terminator` does not emit -- stores (or refuses) the
/// site-named values its `cur_bci` names before the poll. Pinned in the
/// source because the shape (a `goto` latch with no terminator) is what
/// `lower_block`'s no-terminator arm emits, and a poll moved in front of
/// the spill would read a home nothing wrote.
#[test]
pub(in crate::ir_lower::tests) fn r25_1a_a_fallthrough_back_edge_poll_spills_first() {
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    )
    .replace("\r\n", "\n");
    let block = src
        .split("fn lower_block(&mut self, block_idx: usize) {")
        .nth(1)
        .expect("lower_block is in this file")
        .split("\n    }\n")
        .next()
        .expect("the function ends");
    let spill = block
        .find("self.emit_fallthrough_poll_spills(block_idx);")
        .expect("the fall-through poll spills");
    let poll = block
        .find("self.emit_safepoint_poll(Some(block_idx), Some(succ_block));")
        .expect("the fall-through back-edge poll");
    assert!(spill < poll, "the spill must precede the poll");
    // And it is the only back-edge poll outside `lower_terminator`.
    let code = src.as_str();
    let polls = code
        .matches("self.emit_safepoint_poll(Some(block_idx),")
        .count();
    assert_eq!(
        polls, 2,
        "a new back-edge poll emitter needs its inspection spills"
    );
}

/// JIT round 25 item 1a, wave 2: only ops whose traps leave through a pad
/// let a site-named value be named at their bci.
#[test]
pub(in crate::ir_lower::tests) fn r25_1a_w2_pad_only_traps_are_guards_and_primitive_accesses() {
    assert!(super::op_traps_only_through_pads(&Op::ArrayLength));
    assert!(super::op_traps_only_through_pads(&Op::Div));
    assert!(super::op_traps_only_through_pads(&Op::Rem));
    assert!(super::op_traps_only_through_pads(&Op::ArrayLoad(
        MemKind::Int
    )));
    assert!(super::op_traps_only_through_pads(&Op::Load(MemKind::Int)));
    assert!(!super::op_traps_only_through_pads(&Op::ArrayLoad(
        MemKind::Ref
    )));
    assert!(!super::op_traps_only_through_pads(&Op::Load(MemKind::Ref)));
    assert!(!super::op_traps_only_through_pads(&Op::Store(MemKind::Ref)));
    assert!(!super::op_traps_only_through_pads(&Op::Call {
        info_ptr: 0
    }));
    assert!(!super::op_traps_only_through_pads(&Op::MonitorEnter));
}

/// JIT round 25 item 1a, wave 2: the site liveness rule. Two blocks,
/// positions 0..=3 and 4..=7, block 0 -> block 1. `v` (node 0) is defined
/// at 1 and read at 3; `w` (node 1) is defined at 0, live into block 1 and
/// read at 5.
#[test]
pub(in crate::ir_lower::tests) fn r25_1a_w2_a_site_names_a_value_only_where_the_model_has_it_live()
{
    let mut facts = HashMap::new();
    facts.insert(0 as NodeId, (vec![3usize], vec![false, false]));
    facts.insert(1 as NodeId, (vec![5usize], vec![false, true]));
    let sl = super::SiteLive {
        pos_of: vec![Some(1), Some(0)],
        span: vec![(0, 3), (4, 7)],
        block_of_pos: vec![0, 0, 0, 0, 1, 1, 1, 1],
        facts,
    };
    let block = |id: usize, successors: Vec<usize>| crate::ir_schedule::Block {
        id,
        ctrl: 0,
        nodes: Vec::new(),
        terminator: None,
        successors,
        predecessors: Vec::new(),
    };
    let blocks = vec![block(0, vec![1]), block(1, Vec::new())];
    let live = |v: NodeId, p: usize| super::site_value_live_at(&sl, &blocks, v, p);
    // At its own definition, and at its last read, `v` is not provably in
    // its register: the node being lowered may take it.
    assert!(!live(0, 1));
    assert!(live(0, 2));
    assert!(!live(0, 3));
    assert!(!live(0, 5), "dead in a block it never reaches");
    // `w` is live out of block 0 and up to (not at) its read in block 1.
    assert!(live(1, 2));
    assert!(live(1, 4));
    assert!(!live(1, 5));
    assert!(!live(1, 6));
    // A value the plan did not make resident has no facts.
    assert!(!live(7, 2));
}

/// `emit_add_lea`'s direct form is the one home-writing path in a claimed
/// arm that does NOT go through `store_rax`, so the property
/// `publish_def_at_store` would have provided has to be proved of it
/// separately — against its SOURCE, for the same reason the test above
/// reads source: nothing in the type system says a register was published.
///
/// The property: on the path that can SKIP the home store
/// (`home_dropped`), the value must already exist in its own register and
/// the arm must have said so. Both halves matter —
/// `lower_data_node_tracked` refuses the compile when a dropped home meets
/// `cur_def_published == false`, so a missing flag is a coverage loss; a
/// missing `mark_gp_reg_live` is worse, because the value would then be
/// unreadable from a register nothing recorded it in.
///
/// A failure here says the direct form drifted and
/// `CRATONVM_JIT_IR_DROP_HOME` would emit a body that never writes a value
/// it later reads — the same failure the store-once test exists to prevent,
/// through the door that test now lets through.
#[test]
pub(in crate::ir_lower::tests) fn the_lea_add_form_publishes_what_it_does_not_store() {
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    )
    .replace("\r\n", "\n");
    let body = src
        .split("fn emit_add_lea(&mut self, id: NodeId, slot: i32, sub: bool, wide: bool)")
        .nth(1)
        .expect("emit_add_lea is in this file")
        .split("\n    fn ")
        .next()
        .expect("the function ends");
    // The direct form: from the `assigned_gpr` gate to the `AddForm::Done`
    // it returns. Everything after that is the accumulator form, which
    // owes nothing because its caller's `store_rax` owes everything.
    let direct = body
        .split("if let Some(dst) = self.assigned_gpr(id) {")
        .nth(1)
        .expect("the direct form gates on an assigned register")
        .split("AddForm::Done")
        .next()
        .expect("the direct form returns AddForm::Done");
    for required in [
        "self.mark_gp_reg_live(id);",
        "self.cur_def_published = true;",
        "self.store_abi_reg(dst, slot);",
    ] {
        assert!(
            direct.contains(required),
            "`emit_add_lea`'s direct form no longer contains `{required}` — it \
                 skips the home store on the `home_dropped` path, so it must \
                 publish the register and say that it did"
        );
    }
    // The publish must PRECEDE the branch that decides whether to store, so
    // there is no ordering in which a dropped home is skipped by a value
    // that was never published.
    let publish = direct
        .find("self.mark_gp_reg_live(id);")
        .expect("checked above");
    let decide = direct.find("home_dropped").expect("the drop test is here");
    assert!(
        publish < decide,
        "`emit_add_lea` decides whether to skip the home store before it \
             publishes the register"
    );
    // And the accumulator form must NOT publish: its value is in RAX, and
    // `store_rax` is what publishes and stores it.
    let acc = body
        .split("return AddForm::Done;")
        .nth(1)
        .expect("the direct form returns")
        .split("AddForm::InRax")
        .next()
        .expect("the accumulator form returns");
    assert!(
        !acc.contains("mark_gp_reg_live"),
        "`emit_add_lea`'s accumulator form publishes a register, which \
             `store_rax` is about to publish again from RAX"
    );
}

/// Every `Op::X` named in `op_defines_result_slot`'s body.
pub(in crate::ir_lower::tests) fn ops_that_define_a_result_slot(
) -> std::collections::BTreeSet<String> {
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    );
    let body = src
        .split("fn op_defines_result_slot(op: &Op) -> bool {")
        .nth(1)
        .expect("op_defines_result_slot is in this file")
        .split("\n}")
        .next()
        .expect("the function ends");
    let mut out = std::collections::BTreeSet::new();
    collect_op_names(body, &mut out);
    assert!(!out.is_empty(), "the slot scan found nothing");
    out
}

/// Every variant declared in `ir::Op` itself — the ground truth.
///
/// The `\r` strip is load-bearing, not tidiness. `include_str!` returns the
/// file's bytes verbatim, and a CRLF checkout (the Windows default, and
/// what `core.autocrlf=true` produces) makes a `}` line read as `\r\n}\r\n`
/// — so the `"\n}\n"` terminator below matches nothing and the "enum block"
/// silently runs to the end of `ir.rs`, sweeping up every variant of every
/// other enum in the file. That is the whole check reading garbage, and it
/// was doing so on Windows before cov-01 (verified against an unmodified
/// `dev`); it passed on Linux only because the checkout is LF there.
pub(in crate::ir_lower::tests) fn declared_op_variants() -> std::collections::BTreeSet<String> {
    let src = concat!(
        include_str!("../../ir/mod.rs"),
        "\n",
        include_str!("../../ir/node_id.rs"),
        "\n",
        include_str!("../../ir/safepoints.rs"),
        "\n",
        include_str!("../../ir/loop_phis.rs"),
        "\n",
        include_str!("../../ir/set_osr_requested_header.rs"),
        "\n",
        include_str!("../../ir/splice_op_cannot_raise.rs"),
        "\n",
        include_str!("../../ir/narrow_int_return.rs"),
        "\n",
        include_str!("../../ir/is_jdk_throwable_init_candidate.rs"),
        "\n",
        include_str!("../../ir/attach_sb_virtual_builders.rs"),
        "\n",
        include_str!("../../ir/field_load_memo_mode.rs"),
        "\n",
        include_str!("../../ir/ir_splice_store_fence_forward_branch.rs"),
        "\n",
        include_str!("../../ir/ir_trap_census.rs")
    )
    .replace("\r\n", "\n");
    let block = src
        .split("\npub enum Op {")
        .nth(1)
        .expect("ir.rs declares `pub enum Op`")
        .split("\n}\n")
        .next()
        .expect("the enum ends");
    let mut out = std::collections::BTreeSet::new();
    for line in block.lines() {
        // Variants sit at exactly four spaces; struct-variant FIELDS sit at
        // eight and are lower-case, doc comments start with `/`.
        let rest = match line.strip_prefix("    ") {
            Some(r) => r,
            None => continue,
        };
        if !rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !name.is_empty() {
            out.insert(name);
        }
    }
    assert!(!out.is_empty(), "the `ir::Op` scan found nothing");
    out
}

pub(in crate::ir_lower::tests) fn collect_op_names(
    text: &str,
    out: &mut std::collections::BTreeSet<String>,
) {
    let mut rest = text;
    while let Some(i) = rest.find("Op::") {
        let after = &rest[i + 4..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !name.is_empty() && name.starts_with(|c: char| c.is_ascii_uppercase()) {
            out.insert(name);
        }
        rest = after;
    }
}

/// The representative list is complete against `ir::Op`'s own declaration.
///
/// Trips when a variant is REMOVED from `ir::Op` and left here — the case
/// `declared_lowering`'s exhaustiveness cannot catch, because deleting a
/// variant makes an arm unreachable, not missing.
#[test]
pub(in crate::ir_lower::tests) fn the_op_representatives_cover_every_declared_variant() {
    let declared = declared_op_variants();
    let listed: std::collections::BTreeSet<String> = op_representatives()
        .into_iter()
        .map(|(n, _)| n.to_string())
        .collect();
    assert_eq!(
        declared, listed,
        "`op_representatives` and `ir::Op` disagree — left is what ir.rs \
             declares, right is what this test enumerates"
    );
}

/// Every `Op::X` named in `regalloc::ir_op_defines_value`'s body.
///
/// Read out of the other file's source for the same reason
/// [`ops_that_define_a_result_slot`] is read out of this one: the function
/// is private, and a copy of its list maintained here would be a fourth
/// enumeration of the same question.
pub(in crate::ir_lower::tests) fn ops_regalloc_calls_value_defining(
) -> std::collections::BTreeSet<String> {
    let src = concat!(
        include_str!("../../regalloc/mod.rs"),
        "\n",
        include_str!("../../regalloc/local_gpr_pool.rs"),
        "\n",
        include_str!("../../regalloc/build_live_model_legacy_impl.rs"),
        "\n",
        include_str!("../../regalloc/verify_allocation.rs")
    );
    let body = src
        .split("fn ir_op_defines_value(op: &Op) -> bool {")
        .nth(1)
        .expect("ir_op_defines_value is in regalloc.rs")
        .split("\n}")
        .next()
        .expect("the function ends");
    let mut out = std::collections::BTreeSet::new();
    collect_op_names(body, &mut out);
    assert!(
        !out.is_empty(),
        "the regalloc scan found nothing — `ir_op_defines_value`'s shape \
             changed and this test would now pass vacuously"
    );
    out
}

/// The two enumerations of "does this op define a value" are the SAME set.
///
/// `regalloc::ir_op_defines_value` calls itself a "verbatim mirror" of
/// `op_defines_result_slot`, and until 2026-09-02 it was not one: it omitted
/// `Op::ArrayLength` and `Op::NewArray`, and its own doc comment asserted
/// they were absent from both. The comment written to prevent the drift was
/// the drift.
///
/// **What that cost was invisible, which is why this test exists rather
/// than a stricter comment.** `plan_register_residency` compares
/// `wants_loc` (built from the regalloc predicate) against `node_color`
/// (built from this file's), and ONE disagreement declines register
/// residency for the whole method. Every counted loop written
/// `for (i = 0; i < a.length; i++)` contains an `arraylength`, so every one
/// of them declined — silently, because the flag reported only successes.
/// Nothing was miscompiled; the optimization was simply unavailable
/// wherever arrays are, which is most places.
///
/// Compared as sets of names parsed from both sources, so adding an arm to
/// one list and forgetting the other fails here instead of turning up as an
/// unexplained refusal months later.
#[test]
pub(in crate::ir_lower::tests) fn the_two_value_defining_enumerations_agree() {
    let here = ops_that_define_a_result_slot();
    let there = ops_regalloc_calls_value_defining();

    let missing_in_regalloc: Vec<&String> = here.difference(&there).collect();
    let missing_here: Vec<&String> = there.difference(&here).collect();

    assert!(
        missing_in_regalloc.is_empty(),
        "`op_defines_result_slot` names these and `regalloc::ir_op_defines_value` \
             does not: {missing_in_regalloc:?} — the colourer gives them a home the \
             liveness model does not know about, so `plan_register_residency` \
             declines residency for EVERY method containing one",
    );
    assert!(
        missing_here.is_empty(),
        "`regalloc::ir_op_defines_value` names these and `op_defines_result_slot` \
             does not: {missing_here:?} — the liveness model expects a home the \
             colourer never allocates, which is the direction that has no slot to \
             spill to",
    );
}

/// A counted loop over `a.length` reaches the register allocator.
///
/// The end-to-end form of [`the_two_value_defining_enumerations_agree`],
/// and the one that names the consequence rather than the cause.
/// `plan_register_residency`'s agreement check compares `wants_loc`
/// (`regalloc::ir_op_defines_value`) against `node_color`
/// (`op_defines_result_slot`) and declines register residency for the
/// WHOLE method on a single disagreement. `Op::ArrayLength` was in the
/// second list and not the first, so this shape — the most ordinary
/// counted loop in Java — declined every time, and the flag reported
/// nothing because it printed only on success.
///
/// The bytecode is `static int f(int[] a) { int s = 0; for (int i = 0; i <
/// a.length; i++) s += a[i]; return s; }`, assembled by hand so the
/// `arraylength` is unmistakably present rather than incidental to a
/// fixture.
///
/// Asserted on the AGREEMENT, not on a promotion count: whether this
/// particular graph ends up with a register is the allocator's business and
/// may legitimately change, but the two models must never disagree about
/// which values want a home.
#[test]
pub(in crate::ir_lower::tests) fn a_counted_loop_over_array_length_reaches_the_allocator() {
    use crate::regalloc::build_live_model;

    // 0: iconst_0            s = 0
    // 1: istore_1
    // 2: iconst_0            i = 0
    // 3: istore_2
    // 4: iload_2         <-- loop head
    // 5: aload_0
    // 6: arraylength         THE OP THAT USED TO DECLINE THE METHOD
    // 7: if_icmpge +15  --> 22
    // 10: iload_1
    // 11: aload_0
    // 12: iload_2
    // 13: iaload
    // 14: iadd
    // 15: istore_1
    // 16: iinc 2, 1
    // 19: goto -15      --> 4
    // 22: iload_1
    // 23: ireturn
    let code: [u8; 24] = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x2a, 0xbe, 0xa2, 0x00, 0x0f, 0x1b, 0x2a, 0x1c, 0x2e, 0x60,
        0x3b, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xf1, 0x1b, 0xac,
    ];
    let graph = IrBuilder::new(1, 3)
        .build(&code, code.len())
        .expect("the loop builds");
    assert!(
        graph.nodes.iter().any(|n| matches!(n.op, Op::ArrayLength)),
        "the fixture must contain an ArrayLength, or this test proves nothing"
    );

    let schedule = ir_schedule::schedule(&graph);
    let plan = plan_slots(&graph, &schedule, None);
    let live = build_live_model(&graph, &schedule);

    assert_eq!(
        live.wants_loc.len(),
        plan.node_color.len(),
        "the two models disagree about how many nodes there are"
    );
    let disagreeing: Vec<(usize, String)> = live
        .wants_loc
        .iter()
        .zip(plan.node_color.iter())
        .enumerate()
        .filter(|(_, (wants, color))| **wants != color.is_some())
        .map(|(id, _)| {
            (
                id,
                graph
                    .nodes
                    .get(id)
                    .map(|n| format!("{:?}", n.op))
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert!(
        disagreeing.is_empty(),
        "liveness and colourer disagree on {disagreeing:?} — \
             `plan_register_residency` declines residency for the whole method \
             on any one of these, so this ordinary counted loop gets no \
             registers at all",
    );
}

/// `MachineModel::for_graph` cannot see the safepoint poll's slow-path
/// call, because it belongs to no node. `ir_lower_machine_model` adds it.
///
/// Without this a loop-carried value would be handed a register the poll
/// destroys on every back edge. Since round 11 wave 6 the XMM bank is left
/// out when `ir_poll_preserves_fp` (the slow path reloads it), so the
/// clobber is asserted on a caller-saved GP register, and the XMM half is
/// asserted to follow the switch.
#[test]
pub(in crate::ir_lower::tests) fn the_machine_model_clobbers_the_back_edge_poll() {
    use crate::regalloc::{build_live_model, MachineModel, PhysReg, RegFile, RegSpec};

    // int sum(int n){ int s=0; for(int i=0;i<n;i++) s+=i; return s; }
    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02,
        0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
    ];
    // Unoptimized on purpose: the schedule's back edges are the subject,
    // and the optimizer is free to reshape them.
    let builder = IrBuilder::new(1, 3);
    let graph = builder.build(&code, 21).expect("IR build");
    let schedule = ir_schedule::schedule(&graph);
    let live = build_live_model(&graph, &schedule);
    assert!(live.converged);

    let file = || {
        RegFile::from_specs(
            std::iter::once(RegSpec {
                reg: PhysReg::gp(RAX),
                caller_saved: true,
            })
            .chain(IR_LOWER_LS_XMMS.iter().map(|&n| RegSpec {
                reg: PhysReg::xmm(n),
                caller_saved: true,
            })),
        )
    };
    let bare = MachineModel::for_graph(&graph, &schedule, &live, file());
    let ours = ir_lower_machine_model(&graph, &schedule, &live, file(), None);

    let back_edges: Vec<usize> = schedule
        .blocks
        .iter()
        .enumerate()
        .filter(|(b, block)| block.successors.iter().any(|&s| s <= *b))
        .filter_map(|(b, _)| live.span.get(b).map(|&(_, edge)| edge))
        .collect();
    assert!(
        !back_edges.is_empty(),
        "a counted loop must have a back edge to poll on"
    );

    let (gp0, xmm2) = (PhysReg::gp(RAX), PhysReg::xmm(2));
    let holds = |model: &MachineModel, pos: usize, reg: PhysReg| {
        model
            .clobbers
            .iter()
            .any(|(p, regs)| *p == pos && regs.contains(&reg))
    };
    let xmm_clobbered = !ir_poll_preserves_fp();
    for edge in back_edges {
        assert!(
            holds(&ours, edge, gp0),
            "the poll at the outgoing edge of a back-edge block calls its \
                 slow path; position {edge} must be a clobber"
        );
        assert!(
            !holds(&bare, edge, gp0) && !holds(&bare, edge, xmm2),
            "this test is only meaningful while `for_graph` still misses \
                 the poll — if it now models it, delete the augmentation"
        );
        assert_eq!(
            holds(&ours, edge, xmm2),
            xmm_clobbered,
            "the XMM bank is clobbered at the poll exactly when its slow \
                 path does not reload it (`ir_poll_preserves_fp`)"
        );
    }

    // `MachineModel::clobbered_at` binary-searches this list.
    assert!(
        ours.clobbers.windows(2).all(|w| w[0].0 < w[1].0),
        "the clobber list must stay sorted with one entry per position"
    );
}

/// The finding that decides whether any of this does anything on real
/// code: `IrBuilder` snapshots the frame at every bci, so `build_live_model`
/// pins the whole value set and the allocator promotes nothing until the
/// deopt pins are released.
#[test]
pub(in crate::ir_lower::tests) fn a_bytecode_graph_pins_everything_until_the_deopt_pins_are_released(
) {
    use crate::regalloc::{allocate_linear_scan, build_live_model, MachineModel, RegFile};

    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02,
        0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
    ];
    let builder = IrBuilder::new(1, 3);
    let graph = builder.build(&code, 21).expect("IR build");
    let schedule = ir_schedule::schedule(&graph);

    let mut live = build_live_model(&graph, &schedule);
    assert!(live.converged);
    assert!(
        !graph.safepoints.is_empty(),
        "the IR builder records a frame state per bci"
    );

    let wants = live.wants_loc.iter().filter(|w| **w).count();
    let pinned_before = live
        .wants_loc
        .iter()
        .zip(live.pinned.iter())
        .filter(|(w, p)| **w && **p)
        .count();
    assert!(
        wants > 0 && pinned_before * 2 > wants,
        "most values ({pinned_before} of {wants}) should be pinned by the \
             per-bci frame states — that is the whole finding"
    );

    let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
    let before = allocate_linear_scan(&graph, &live, &model).expect("allocates");

    let released = live.release_deopt_pins(&graph);
    assert!(released > 0, "there were pins to release");
    let after = allocate_linear_scan(&graph, &live, &model).expect("allocates");
    crate::regalloc::verify_allocation(&graph, &live, &model, &after).expect("verifies");
    assert!(
        after.promoted > before.promoted,
        "releasing the deopt pins is what makes the allocator do anything \
             on a real method ({} → {})",
        before.promoted,
        after.promoted
    );
    // Phi pins are structural and must survive.
    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::Phi) && live.wants_loc[id] {
            assert!(
                live.pinned[id],
                "phi n{id} lost its pin; its home is what the edge copies write"
            );
        }
    }
}

/// **XMM8 must encode as XMM8, in every form that can receive one.**
///
/// The ModRM `reg` and `r/m` fields are three bits each, so a register
/// number of 8 or more needs a REX byte and *without* one truncates to the
/// corresponding low register with no diagnostic anywhere: XMM8 becomes
/// XMM0, a `double` is read out of the scratch pair, and the answer is
/// plausible and wrong. That was the state of `fp_load`, `fp_store`,
/// `fp_binop` and `emit_xmm_frame_move` until R14.
///
/// # Why XMM0 and XMM8 rather than XMM7 and XMM8
///
/// `0 & 7 == 8 & 7`, so the two share every ModRM bit and the REX byte is
/// the *only* legitimate difference between their encodings. That turns a
/// vague "they differ" into three separate claims, each of which fails a
/// different real defect:
///
///   * **one byte longer.** A missing REX makes the two encodings
///     byte-identical — which IS the truncation, stated as a test.
///   * **the extra byte is at `rex_at`, and is `rex`.** The mandatory SSE
///     prefix (`F2`/`F3`) must come first and REX second, because a REX
///     that is not the prefix immediately preceding the opcode is ignored
///     and the extension is silently lost. A REX emitted in front of the
///     `F2` puts `0xF2` at `rex_at` instead, and fails here. `MOVAPS` and
///     `MOVUPS` carry no mandatory prefix, so their `rex_at` is 0 — getting
///     THOSE the other way round fails too.
///   * **deleting it leaves the XMM0 encoding exactly.** Nothing else about
///     the instruction may change, which is what rules out "REX.B set where
///     REX.R was meant" reaching the ModRM byte instead.
///
/// A low-register regression is checked separately, at the bottom: XMM7
/// must still emit no REX at all, so this cannot be passed by emitting a
/// REX unconditionally.
#[test]
pub(in crate::ir_lower::tests) fn every_xmm_encoder_reaches_xmm8_through_rex() {
    // (what, index of the REX byte, its value, the emitter)
    //
    // `0x44` is REX.R (the ModRM `reg` field), `0x41` REX.B (`r/m`), and
    // `0x45` both. Which one an emitter needs is decided by which field it
    // puts the extended register in, and getting that wrong is exactly what
    // the third claim above catches.
    let cases: Vec<(&str, usize, u8, Box<dyn Fn(&mut Lowerer<'static>, u8)>)> = vec![
        (
            "fp_load double (F2 0F 10)",
            1,
            0x44,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.fp_load(r, 32, true)),
        ),
        (
            "fp_load float (F3 0F 10)",
            1,
            0x44,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.fp_load(r, 32, false)),
        ),
        (
            "fp_store double (F2 0F 11)",
            1,
            0x44,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.fp_store(32, r, true)),
        ),
        (
            "fp_store float (F3 0F 11)",
            1,
            0x44,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.fp_store(32, r, false)),
        ),
        (
            "emit_movsd_frame_from_xmm (F2 0F 11)",
            1,
            0x44,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.emit_movsd_frame_from_xmm(32, r)),
        ),
        (
            "fp_binop dst (F2 0F 58 /r)",
            1,
            0x44,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.fp_binop(0x58, r, XMM1, true)),
        ),
        (
            "fp_binop src (F2 0F 58 /r)",
            1,
            0x41,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.fp_binop(0x58, XMM1, r, true)),
        ),
        (
            "emit_xmm_frame_move store (MOVUPS, no prefix)",
            0,
            0x44,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.emit_xmm_frame_move(r, 32, true)),
        ),
        (
            "emit_xmm_frame_move restore (MOVUPS, no prefix)",
            0,
            0x44,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.emit_xmm_frame_move(r, 32, false)),
        ),
        (
            "fp_reg_move dst (MOVAPS, no prefix)",
            0,
            0x44,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.fp_reg_move(r, XMM1)),
        ),
        (
            "fp_reg_move src (MOVAPS, no prefix)",
            0,
            0x41,
            Box::new(|lo: &mut Lowerer<'static>, r: u8| lo.fp_reg_move(XMM1, r)),
        ),
    ];

    for (what, rex_at, rex, emit) in &cases {
        let low = xmm_encoding(0, emit.as_ref());
        let high = xmm_encoding(8, emit.as_ref());
        assert_eq!(
            high.len(),
            low.len() + 1,
            "{what}: xmm8 encodes in {} bytes and xmm0 in {} — a truncating \
                 encoder makes the two IDENTICAL, and xmm8 then executes as xmm0 \
                 ({low:02x?} vs {high:02x?})",
            high.len(),
            low.len(),
        );
        let mut without = high.clone();
        let emitted = without.remove(*rex_at);
        assert_eq!(
            emitted, *rex,
            "{what}: byte {rex_at} of {high:02x?} is {emitted:#04x}, not the \
                 expected REX {rex:#04x} — a REX placed before the mandatory SSE \
                 prefix is ignored by the CPU and the extension is lost silently",
        );
        assert_eq!(
            without, low,
            "{what}: removing the REX from the xmm8 encoding must leave the \
                 xmm0 encoding exactly; nothing else about the instruction may \
                 differ ({high:02x?} vs {low:02x?})",
        );
    }

    // The low half is unchanged, so this cannot be satisfied by emitting a
    // REX unconditionally — which would be a correct-but-larger encoding
    // for every register the residency file actually hands out today.
    for (what, _, _, emit) in &cases {
        let seven = xmm_encoding(7, emit.as_ref());
        let zero = xmm_encoding(0, emit.as_ref());
        assert_eq!(
            seven.len(),
            zero.len(),
            "{what}: xmm7 must need no REX ({seven:02x?})",
        );
    }
}

/// Two values sharing one physical register: publishing the second must
/// EVICT the first, or `resident_gpr` keeps naming a register that no
/// longer holds it.
///
/// The residency plan is allowed to hand one register to any number of
/// values whose ranges do not overlap, so this is not a hypothetical: it
/// is the ordinary output of the allocator. Before `gp_reg_owner` existed,
/// `gp_reg_live` was set at every publish and cleared nowhere, so the
/// assertion below read `Some(RBX)` for BOTH nodes -- and the phi-copy
/// edge read, which asks about a value far from its own definition, turned
/// that stale answer into wrong code (Byte Buddy's ASM writer, through
/// Spring Boot's `NestedUrlConnectionTests`).
///
/// Asserted on `resident_gpr`, not on `gp_reg_live`, because the predicate
/// is what every reader actually calls.
#[test]
pub(in crate::ir_lower::tests) fn publishing_into_a_shared_register_evicts_its_previous_owner() {
    const RBX: u8 = 3;
    let mut lo = lowerer_with_resident_gpr(16384, RBX);
    // A second node in the SAME register — what a non-overlapping pair
    // looks like coming out of the allocator.
    if lo.gp_reg_of.len() < 2 {
        lo.gp_reg_of.resize(2, None);
        lo.gp_reg_live.resize(2, false);
    }
    lo.gp_reg_of[1] = Some(RBX);

    lo.mark_gp_reg_live(0);
    assert_eq!(lo.resident_gpr(0), Some(RBX), "node 0 published into RBX");
    assert_eq!(lo.resident_gpr(1), None, "node 1 has not published yet");

    lo.mark_gp_reg_live(1);
    assert_eq!(lo.resident_gpr(1), Some(RBX), "node 1 now occupies RBX");
    assert_eq!(
        lo.resident_gpr(0),
        None,
        "node 0 must NOT still claim RBX: node 1's publish overwrote it"
    );

    // Re-publishing the current occupant is idempotent, not self-eviction.
    lo.mark_gp_reg_live(1);
    assert_eq!(lo.resident_gpr(1), Some(RBX));
}

/// The eviction is per REGISTER, not global: a value in a different
/// register is untouched by someone else's publish. Without this the
/// "fix" could pass the test above by simply clearing everything.
#[test]
pub(in crate::ir_lower::tests) fn publishing_does_not_evict_a_value_in_a_different_register() {
    const RBX: u8 = 3;
    const R12: u8 = 12;
    let mut lo = lowerer_with_resident_gpr(16384, RBX);
    if lo.gp_reg_of.len() < 2 {
        lo.gp_reg_of.resize(2, None);
        lo.gp_reg_live.resize(2, false);
    }
    lo.gp_reg_of[1] = Some(R12);

    lo.mark_gp_reg_live(0);
    lo.mark_gp_reg_live(1);
    assert_eq!(lo.resident_gpr(0), Some(RBX));
    assert_eq!(lo.resident_gpr(1), Some(R12));
}

/// The end-to-end one: a value that exists ONLY in a register at the trap
/// comes back out of the reconstructed frame.
///
/// This runs the emitted bytes. Nothing else proves the spill offsets: the
/// region's arithmetic (`gpr[r]` at `[rbp - (base - 8r)]`), the `LEA` that
/// turns `deopt_regs_base` into a `*const SavedRegisters`, the third
/// argument register on this ABI, and `ir_deopt_entry`'s dereference of it
/// are four independent chances to be off by an offset, and each of them
/// fails by producing a *plausible number* rather than a crash.
///
/// `RegisterLong` deliberately, not `Register`: the latter resolves
/// truncated to 32 bits, so a sentinel with a distinctive HIGH half would
/// pass a broken 64-bit read.
#[test]
pub(in crate::ir_lower::tests) fn a_deopt_frame_reads_a_register_the_stub_spilled() {
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    const RBX: u8 = 3;
    // Distinctive in both halves, and not a plausible stack value.
    const SENTINEL: i64 = 0x5EED_1234_0BAD_C0DEu64 as i64;

    let mut lo = lowerer_with_resident_gpr(16384, RBX);
    assert!(
        lo.deopt_regs_base > 0,
        "the flag is on, so the frame must have reserved the region",
    );

    // LEAK(intentional): the stub bakes this pointer as an immediate and
    // the entry dereferences it during the call.
    let point: &'static DeoptimizationPoint = Box::leak(Box::new(DeoptimizationPoint {
        native_offset: 0,
        bci: 7,
        reason: DeoptReason::TransferToInterpreter,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 7,
            locals: vec![FrameValue::RegisterLong(RBX)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(DeoptReason::TransferToInterpreter),
    }));

    // push rbp ; mov rbp, rsp ; sub rsp, frame_size
    lo.buf.emit_byte(0x55);
    lo.buf.emit(&[0x48, 0x89, 0xE5]);
    lo.buf.emit(&[0x48, 0x81, 0xEC]);
    let frame = lo.frame_size;
    lo.buf.emit(&frame.to_le_bytes());
    // Save what the stub's teardown will restore. Without this the stub
    // hands the RUST caller garbage in RBX/R12-R15 on the way out, which is
    // a corrupted test process rather than a failed assertion.
    let gpr_saves: Vec<(u8, i32)> = lo.saved_gpr_regs().collect();
    for (reg, off) in gpr_saves {
        lo.emit_gpr_frame_move(reg, off, true);
    }
    let xmm_saves: Vec<(u8, i32)> = lo.saved_xmm_regs().collect();
    for (reg, off) in xmm_saves {
        lo.emit_xmm_frame_move(reg, off, true);
    }
    // The value under test lives in RBX and in NO frame word.
    // Cast: a fixed test sentinel.
    lo.emit_mov_reg_imm64(RBX, SENTINEL as u64);
    // Cast: a leaked pointer baked as the guard's argument.
    lo.emit_mov_reg_imm64(DEOPT_ARG0, point as *const DeoptimizationPoint as u64);
    // JMP <stub>, patched by `emit_deopt_stub` exactly as a guard's is.
    lo.buf.emit_byte(0xE9);
    let patch = lo.buf.pos();
    lo.buf.emit(&[0, 0, 0, 0]);
    lo.deopt_stub_patches.push(patch);
    lo.emit_deopt_stub();
    assert!(!lo.buf.overflowed(), "the harness body must fit");

    let cm = CompiledMethod::new(lo.buf);
    // SAFETY: the body takes no arguments, builds and tears down its own
    // frame, and calls only `ir_deopt_entry`.
    let ret = unsafe { cm.try_call(&[]) }.expect("the harness body runs");
    assert_eq!(ret, i64::MIN, "the stub returns the deopt sentinel");

    let frame = crate::deopt::take_last_deopt().expect("the entry stashed a frame");
    assert_eq!(frame.bci, 7);
    assert_eq!(
        frame.locals,
        vec![FrameValue::Long(SENTINEL)],
        "the reconstructed local must be the value RBX held at the trap, \
             read out of the spilled register image",
    );
}

/// With the flag off the stub keeps its historical two-argument shape, and
/// the frame grows by nothing.
#[test]
pub(in crate::ir_lower::tests) fn without_the_flag_no_region_is_reserved_and_nothing_is_spilled() {
    let _ls = LsForce::on();
    // Default-ON since 2026-09-05, so "without the flag" is now something
    // this has to ask for. That is the point of the test: the way back has
    // to keep working.
    let _dr = DeoptRegsForce::off();
    let mut lo = lowerer_with_resident_gpr(16384, 3);
    assert_eq!(
        lo.deopt_regs_base, 0,
        "no region may be reserved when the flag is off",
    );
    lo.buf.emit(&[0, 0, 0, 0]);
    lo.deopt_stub_patches.push(0);
    let before = lo.buf.pos();
    lo.emit_deopt_stub();
    let with_flag_off = lo.buf.pos() - before;

    let _dr = DeoptRegsForce::on();
    let mut on = lowerer_with_resident_gpr(16384, 3);
    on.buf.emit(&[0, 0, 0, 0]);
    on.deopt_stub_patches.push(0);
    let before = on.buf.pos();
    on.emit_deopt_stub();
    let with_flag_on = on.buf.pos() - before;

    assert!(
        with_flag_on > with_flag_off,
        "the flag must add the 32 spill stores and the LEA: {with_flag_on} \
             bytes on against {with_flag_off} off",
    );
}

/// Naming is by EXCLUSIVE ownership, not by residency.
///
/// Two values sharing a register is the case that reconstructs a
/// confidently wrong value: the allocator gives each of them the register
/// over its own live range, `release_deopt_pins` means a frame state can
/// name either one outside that range, and there is no position mapping in
/// this file that could tell which. Both must be refused.
#[test]
pub(in crate::ir_lower::tests) fn two_values_sharing_a_register_are_both_unnameable() {
    let _ls = LsForce::on();
    let _dr = DeoptRegsForce::on();
    let mut lo = lowerer_with_resident_xmm(4096, None);
    let n = lo.graph.nodes.len().max(4);
    let mut gp_reg_of = vec![None; n];
    gp_reg_of[0] = Some(3); // RBX, shared…
    gp_reg_of[1] = Some(3); // …with this one
    gp_reg_of[2] = Some(12); // R12, exclusively
    lo.set_residency(RegResidency {
        reg_of: vec![None; n],
        gp_reg_of,
        promoted: 3,
        demoted: 0,
        peak_live: 0,
        splits: LsSplitPlan::default(),
        phi_census: PhiCensus::default(),
        poll_fp_reloads: HashMap::new(),
        caller_saved_republish: Vec::new(),
    });
    assert!(!lo.deopt_nameable[0], "a shared register is not nameable");
    assert!(!lo.deopt_nameable[1], "and neither is its other holder");
    assert!(lo.deopt_nameable[2], "an exclusive register is nameable");
    assert_eq!(
        lo.register_frame_value(2, IrType::Long),
        Some(FrameValue::RegisterLong(12)),
    );
    assert_eq!(
        lo.register_frame_value(0, IrType::Long),
        None,
        "the shared holder must fall through to its home word",
    );
    assert_eq!(
        lo.register_frame_value(2, IrType::Ref),
        None,
        "without reference residency and a live owner, a reference falls through to its home",
    );
    lo.ref_residency_on = true;
    lo.gp_reg_live = vec![false, false, true];
    lo.gp_reg_owner[12] = Some(2);
    assert_eq!(
        lo.register_frame_value(2, IrType::Ref),
        Some(FrameValue::RegisterRef(12)),
        "with reference residency active and R12 live for node 2, a guard names RegisterRef(12)",
    );
    lo.building_table_points.set(true);
    assert_eq!(
        lo.register_frame_value(2, IrType::Ref),
        None,
        "table points keep StackSlotRef when the home was not dropped",
    );
    lo.building_table_points.set(false);
}

/// gen r5w4/jit8: the fresh-frame zeroing loop is R10/R11-only, walks down
/// from RBP in 8-byte steps, and its `JA` lands on its own `SUB` (a wrong
/// rel8 would either spin forever or fall into the parameter stores with
/// the frame half-cleared).
#[test]
pub(in crate::ir_lower::tests) fn the_fresh_frame_zeroing_loop_is_self_contained() {
    let mut lo = lowerer_with_resident_xmm(256, None);
    let start = lo.buf.pos();
    lo.emit_zero_whole_frame();
    let code = lo.buf.as_slice()[start..].to_vec();
    assert_eq!(
        code,
        vec![
            0x45, 0x31, 0xDB, // XOR R11D, R11D
            0x49, 0x89, 0xEA, // MOV R10, RBP
            0x49, 0x83, 0xEA, 0x08, // SUB R10, 8
            0x4D, 0x89, 0x1A, // MOV [R10], R11
            0x49, 0x39, 0xE2, // CMP R10, RSP
            0x77, 0xF4, // JA rel8
        ],
    );
    // The branch target: the byte after `JA` plus rel8 is the `SUB`.
    let after_ja = code.len() as i64;
    let rel8 = i64::from(code[code.len() - 1] as i8);
    assert_eq!(after_ja + rel8, 6, "JA must land on SUB R10, 8");
}

/// **The** invariant: every exit restores exactly what the prologue saved.
///
/// All three exits are driven, not one — `emit_deopt_stub` inlines its own
/// teardown instead of calling `emit_epilogue` (it must not restore the
/// shadow `top`), so it is the one that silently keeps this frame's XMM6 in
/// the caller's register file if the restore is left out of it. Deleting
/// `emit_callee_saved_restore` from any of the three fails this test.
#[test]
pub(in crate::ir_lower::tests) fn every_exit_restores_exactly_what_the_prologue_saved() {
    let _flag = LsForce::on();
    let expect = usize::from(!IR_LOWER_SAVED_XMMS.is_empty());
    let reg = *IR_LOWER_SAVED_XMMS.first().unwrap_or(&6);
    // Closures rather than fixed byte needles: the displacement form depends
    // on the frame depth, so the count has to accept either one.
    let saves_in = |c: &[u8]| count_movups_frame(c, reg, true);
    let restores_in = |c: &[u8]| count_movups_frame(c, reg, false);

    // ── exit 1: the method epilogue ──────────────────────────────
    let mut lo = lowerer_with_resident_xmm(4096, Some(reg));
    lo.emit_prologue();
    let after_prologue = lo.buf.pos();
    lo.emit_epilogue();
    let code = lo.buf.as_slice().to_vec();
    let saves = saves_in(&code[..after_prologue]);
    assert_eq!(
        saves, expect,
        "the prologue saved {saves} of xmm{reg}, expected {expect} on this target",
    );
    assert_eq!(
        restores_in(&code[after_prologue..]),
        saves,
        "the epilogue did not restore what the prologue saved",
    );

    // ── exit 2: the shared call-exception bail stub ──────────────
    let mut lo = lowerer_with_resident_xmm(4096, Some(reg));
    lo.emit_prologue();
    let after_prologue = lo.buf.pos();
    lo.buf.emit(&[0, 0, 0, 0]);
    // `(patch_offset, throw_bci)` — the stub is emitted once per distinct
    // bci and stamps it. Which bci this exit carries is irrelevant to what
    // the test measures (that the stub restores what the prologue saved),
    // so 0.
    lo.call_exc_patches.push((after_prologue, 0));
    lo.emit_call_exc_stub();
    let code = lo.buf.as_slice().to_vec();
    assert_eq!(
        restores_in(&code[after_prologue..]),
        saves,
        "the call-exception bail stub returns the sentinel without \
             restoring the caller's registers",
    );

    // ── exit 3: the deopt stub's inlined teardown ────────────────
    let mut lo = lowerer_with_resident_xmm(4096, Some(reg));
    lo.emit_prologue();
    let after_prologue = lo.buf.pos();
    lo.buf.emit(&[0, 0, 0, 0]);
    lo.deopt_stub_patches.push(after_prologue);
    lo.emit_deopt_stub();
    let code = lo.buf.as_slice().to_vec();
    assert_eq!(
        restores_in(&code[after_prologue..]),
        saves,
        "the deopt stub inlines its own teardown and skipped the restore",
    );
}

/// A method that parks nothing in a callee-saved register emits no save.
///
/// The save area costs frame bytes whenever the flag is on; it must not
/// also cost two `MOVUPS` per call in the overwhelmingly common case where
/// the allocation used none of them. `reg_of` naming only XMM2 is exactly
/// that case.
#[test]
pub(in crate::ir_lower::tests) fn a_method_that_used_no_callee_saved_register_emits_no_save() {
    let _flag = LsForce::on();
    for plan in [None, Some(2u8)] {
        let mut lo = lowerer_with_resident_xmm(4096, plan);
        lo.emit_prologue();
        lo.emit_epilogue();
        let code = lo.buf.as_slice().to_vec();
        for &reg in IR_LOWER_SAVED_XMMS {
            assert_eq!(
                count_movups_frame(&code, reg, true),
                0,
                "saved xmm{reg} for a plan that never used it ({plan:?})",
            );
        }
    }
}

/// A method with a save area publishes no OSR entry table.
///
/// The single-pass trampoline enters *past* `emit_prologue` and builds the
/// frame itself, so a method of this tier entered through it would
/// restore XMM6/XMM7 from words the save never wrote. This tier's OSR
/// entries go through its own stubs, which save what the prologue saves
/// (`gcg_g3v_an_ir_osr_entry_stub_saves_what_the_prologue_saves`); this
/// pins the other half, that no artifact of this tier offers the
/// trampoline's table, on a method compiled with the linear-scan path on.
/// (gcg g3/v: the `debug_assert!` this doc used to cite tested the same
/// table at the publication site and was removed as vacuous.)
#[test]
pub(in crate::ir_lower::tests) fn an_ir_artifact_never_offers_an_osr_entry_the_save_area_would_break(
) {
    let _flag = LsForce::on();
    let cm = compile_via_ir(&MIR_ALU_CODE, 6, 2, 2).expect("compiles");
    assert!(
        cm.osr_pc_to_native.is_none(),
        "the IR tier published an OSR entry table; `osr_callee_saved_xmms` \
             and `osr_xmm_saved_base` must be published with it",
    );
}
