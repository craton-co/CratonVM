use super::*;

/// A leaked `JitInvokeInfo` naming `class.method`, as `lib.rs`'s planner
/// hands the builder one per resolved invoke site.
fn leaked_info(class: &'static str, method: &'static str) -> usize {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: class,
        method_name: method,
        descriptor: "()V",
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    info as *const JitInvokeInfo as usize
}

/// `static double q(Function f, int i) { return (Double) f.apply(i); }` —
/// the `Dist` adapter run at pc 2..16, built with `tdigest_scalar_kernel`
/// set and the invoke rows `apply_class` decides.
fn build_adapter(apply_class: &'static str) -> Option<Graph> {
    //  0: aload_0   1: iload_1
    //  2: invokestatic  #10        Integer.valueOf
    //  5: invokeinterface #11, 2   <apply_class>.apply
    // 10: checkcast #12            Double
    // 13: invokevirtual #13        Double.doubleValue
    // 16: dreturn
    let code: [u8; 17] = [
        0x2a, 0x1b, 0xb8, 0x00, 0x0a, 0xb9, 0x00, 0x0b, 0x02, 0x00, 0xc0, 0x00, 0x0c, 0xb6,
        0x00, 0x0d, 0xaf,
    ];
    let mut b = IrBuilder::new(2, 2);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    b.tdigest_scalar_kernel = true;
    b.set_invoke_info(std::collections::HashMap::from([
        (2, (leaked_info("java/lang/Integer", "valueOf"), 1, b'L')),
        (5, (leaked_info(apply_class, "apply"), 2, b'L')),
        (
            13,
            (leaked_info("java/lang/Double", "doubleValue"), 1, b'D'),
        ),
    ]));
    b.build(&code, code.len())
}

fn folded(g: &Graph) -> bool {
    g.nodes.iter().any(|n| n.op == Op::LambdaIntToDouble)
}

/// The fold keys on the run's MEMBERS: the real adapter folds, and the same
/// four opcodes calling another functional interface at `pc + 3` do not
/// (the helper would compute `Function.apply`'s answer for a
/// `ToDoubleFunction`).
#[test]
fn the_tdigest_adapter_fold_checks_members_not_just_opcodes() {
    let g = build_adapter("java/util/function/Function").expect("the adapter run builds");
    assert!(folded(&g), "the matching run must still fold");
    if let Some(g) = build_adapter("java/util/function/ToDoubleFunction") {
        assert!(!folded(&g), "a different interface at pc+3 must not fold");
    }
}

/// Round 11 wave 4: the fold null-checks the `Function` with a guard that
/// deopts at the `invokestatic` (bci 2), whose frame still holds
/// `(lambda, index)`, so the interpreter raises the NPE at the
/// `invokeinterface` (bci 5) with that bci's handler and message. Before,
/// the helper raised it with the fold's only bci, 2 — `Integer.valueOf`.
#[test]
fn r11w4_the_tdigest_fold_deopts_a_null_function_at_the_invokestatic() {
    let g = build_adapter("java/util/function/Function").expect("the adapter run builds");
    let fold = g
        .nodes
        .iter()
        .position(|n| n.op == Op::LambdaIntToDouble)
        .expect("the matching run folds");
    let lambda = g.nodes[fold].inputs[2];
    let is_null_test_of_lambda =
        |c: &Node| c.op == Op::Cmp(CmpOp::Ne) && c.inputs.first() == Some(&lambda);
    let guarded = g.nodes.iter().take(fold).any(|n| {
        n.op == (Op::Guard { bci: 2 })
            && n.inputs
                .get(1)
                .and_then(|&c| g.nodes.get(c as usize))
                .is_some_and(is_null_test_of_lambda)
    });
    assert!(guarded, "no null guard on the lambda ahead of the fold");
    assert!(
        g.safepoints
            .iter()
            .any(|s| s.bci == 2 && s.stack.len() == 2 && s.stack.first() == Some(&lambda)),
        "the guard's frame (bci 2) must still hold (lambda, index)"
    );
}

/// Round 11 wave 5: a site the planner chose to SPLICE has no
/// `invoke_info` row, and the member check used to accept it unseen. The
/// inline plan's `method_key` is now read instead: `Double.doubleValue`
/// planned for splicing still folds, `Long.longValue` does not.
#[test]
fn r11w5_a_spliced_adapter_member_is_checked_by_its_inline_key() {
    fn build_with_spliced_tail(key: &str) -> Option<Graph> {
        // The adapter method (pc 0..17), then the planned callee body at
        // 17: `dconst_0; dreturn`.
        let code: [u8; 19] = [
            0x2a, 0x1b, 0xb8, 0x00, 0x0a, 0xb9, 0x00, 0x0b, 0x02, 0x00, 0xc0, 0x00, 0x0c, 0xb6,
            0x00, 0x0d, 0xaf, 0x0e, 0xaf,
        ];
        let mut b = IrBuilder::new(2, 2);
        b.set_param_types(&[IrType::Ref, IrType::Int]);
        b.tdigest_scalar_kernel = true;
        b.set_invoke_info(std::collections::HashMap::from([
            (2, (leaked_info("java/lang/Integer", "valueOf"), 1, b'L')),
            (
                5,
                (leaked_info("java/util/function/Function", "apply"), 2, b'L'),
            ),
        ]));
        b.set_inline_sites(std::collections::HashMap::from([(
            13,
            IrInlineSite {
                base: 17,
                code_len: 2,
                num_args: 1,
                max_locals: 1,
                arg_local_slots: vec![0],
                returns_value: true,
                receiver_is_arg0: true,
                method_key: key.to_string(),
                class_id: 0,
            },
        )]));
        b.build(&code, 17)
    }
    let g = build_with_spliced_tail("java/lang/Double.doubleValue:()D")
        .expect("the matching run folds before the splice is ever opened");
    assert!(folded(&g), "the matching spliced member must still fold");
    if let Some(g) = build_with_spliced_tail("java/lang/Long.longValue:()J") {
        assert!(
            !folded(&g),
            "a spliced Long.longValue at pc+11 must not fold"
        );
    }
}

/// A builder starts with NO String layout, whatever the process-wide
/// latch holds — which, in a process with several VMs, may be another
/// VM's (`r11w2-rt-ir-string-layout-is-process-global`) — and takes only
/// the one its compile hands it. (Other test threads may publish at any
/// moment; the answer must not depend on it.)
#[test]
fn the_per_compile_string_layout_replaces_the_process_seed() {
    let mut b = IrBuilder::new(0, 1);
    assert_eq!(b.string_layout, None, "no process-wide seed");
    b.set_string_layout(None);
    assert_eq!(b.string_layout, None);
}

/// Round 11 wave 3: a frame so wide that one snapshot per walked bci
/// would cost more than `IR_MAX_FRAME_SNAPSHOT_WORDS` is refused before
/// the walk allocates any of it; the same method with a narrow frame
/// builds.
#[test]
fn r11w3_a_frame_too_wide_to_snapshot_per_bci_is_refused_up_front() {
    // 600 × nop; return
    let mut code = vec![0x00u8; 600];
    code.push(0xb1);
    let wide = 60_000usize;
    assert!(wide.saturating_mul(code.len()) > IR_MAX_FRAME_SNAPSHOT_WORDS);
    assert!(IrBuilder::new(0, wide).build(&code, code.len()).is_none());
    assert!(IrBuilder::new(0, 4).build(&code, code.len()).is_some());
}

/// A cycle entered at two points — the `ifeq` jumps into the middle of
/// the loop nest headed at pc 6. Round 11 wave 3 refused it; since wave 5
/// it builds (the loop passes check dominance themselves), its header
/// joins BOTH latches, and the optimized, scheduled graph passes the
/// release checks in front of `ir_lower`. The same nest with the `ifeq`
/// aimed at the exit is the reducible control.
#[test]
fn r11w5_an_irreducible_cycle_builds_and_schedules_cleanly() {
    //  0: iconst_0   1: istore_1   2: iload_0   3: ifeq -> 16 | 25
    //  6: iinc 1 1   9: iload_1   10: bipush 10  12: if_icmplt -> 6
    // 15: nop       16: iinc 1 2  19: iload_1   20: bipush 100
    // 22: if_icmplt -> 6          25: iload_1   26: ireturn
    let code = |ifeq_offset: u8| -> Vec<u8> {
        vec![
            0x03,
            0x3c,
            0x1a,
            0x99,
            0x00,
            ifeq_offset,
            0x84,
            0x01,
            0x01,
            0x1b,
            0x10,
            0x0a,
            0xa1,
            0xff,
            0xfa,
            0x00,
            0x84,
            0x01,
            0x02,
            0x1b,
            0x10,
            0x64,
            0xa1,
            0xff,
            0xf0,
            0x1b,
            0xac,
        ]
    };
    let irreducible = code(13); // 3 + 13 = 16: a second entry into the cycle
    let reducible = code(22); // 3 + 22 = 25: the exit
    let verdict = |c: &[u8]| -> bool {
        let Ok(verified) = cratonvm_reader::verified_code(c) else {
            panic!("the fixture must verify");
        };
        let reachable = normally_reachable_pcs(&verified, c.len());
        cfg_is_reducible(&verified, &reachable, c.len())
    };
    assert!(!verdict(&irreducible), "fixture: the cycle has two entries");
    assert!(verdict(&reducible));
    for c in [&irreducible, &reducible] {
        let mut g = IrBuilder::new(1, 2)
            .build(c, c.len())
            .expect("both nests build");
        // The header at pc 6 joins its fall-through entry and the two
        // latches (pcs 12 and 22).
        assert!(
            g.nodes
                .iter()
                .any(|n| matches!(n.op, Op::Merge | Op::Region)
                    && n.bytecode_pc == Some(6)
                    && n.inputs.len() == 3),
            "the pc-6 header must join three predecessors"
        );
        crate::ir_verify::verify_graph(
            &g,
            "post-build",
            crate::ir_verify::VerifyOptions::structural(),
        )
        .expect("the built graph verifies");
        crate::ir_optimize::optimize(&mut g);
        crate::ir_verify::verify_graph(
            &g,
            "post-optimize",
            crate::ir_verify::VerifyOptions::structural(),
        )
        .expect("the optimized graph verifies");
        let schedule = crate::ir_schedule::schedule(&g);
        assert_eq!(
            schedule.misplaced_data_nodes(&g),
            Vec::<NodeId>::new(),
            "every read is dominated by its definition"
        );
    }
}
