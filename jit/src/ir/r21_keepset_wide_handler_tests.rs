use super::*;
use std::collections::HashMap;

/// `static void f(Object a) { Object d = a; try { h(); } catch (Throwable
/// ex) { use(d, ex); } }`: the handler at 8 reads `d` (stored before the
/// `try`), runs code without an `athrow` and falls into the join at 13.
const FALLS_IN: [u8; 14] = [
    0x2a, // 0: aload_0
    0x4c, // 1: astore_1        d
    0xb8, 0x00, 0x02, // 2: invokestatic #2
    0xa7, 0x00, 0x08, // 5: goto 13
    0x4d, // 8: astore_2        ex (handler)
    0x2b, // 9: aload_1         reads d
    0x2c, // 10: aload_2
    0x57, // 11: pop
    0x57, // 12: pop
    0xb1, // 13: return
];

/// The same handler ending in its own `goto` to the join.
const GOES_TO: [u8; 17] = [
    0x2a, // 0: aload_0
    0x4c, // 1: astore_1
    0xb8, 0x00, 0x02, // 2: invokestatic #2
    0xa7, 0x00, 0x0b, // 5: goto 16
    0x4d, // 8: astore_2 (handler)
    0x2b, // 9: aload_1
    0x2c, // 10: aload_2
    0x57, // 11: pop
    0x57, // 12: pop
    0xa7, 0x00, 0x03, // 13: goto 16
    0xb1, // 16: return
];

/// A `checkcast` inside the `try` range.
const CASTS: [u8; 19] = [
    0x2a, // 0: aload_0
    0x4c, // 1: astore_1
    0x2b, // 2: aload_1
    0xc0, 0x00, 0x03, // 3: checkcast #3
    0x57, // 6: pop
    0xb8, 0x00, 0x02, // 7: invokestatic #2
    0xa7, 0x00, 0x08, // 10: goto 18
    0x4d, // 13: astore_2 (handler)
    0x2b, // 14: aload_1
    0x2c, // 15: aload_2
    0x57, // 16: pop
    0x57, // 17: pop
    0xb1, // 18: return
];

#[test]
fn r21_keepset_the_wide_plan_admits_a_handler_that_runs_code_and_reads_pre_try_locals() {
    assert!(
        splice_callee_handler_plan(&FALLS_IN, FALLS_IN.len(), &[(2, 5, 8)]).is_none(),
        "the narrow plan refuses it (reads d, no athrow)"
    );
    let plan = splice_callee_handler_plan_with(&FALLS_IN, FALLS_IN.len(), &[(2, 5, 8)], true)
        .expect("wide: admitted");
    assert_eq!(plan.runs, vec![(8, 13)]);
    assert_eq!(&plan.code[8..13], &[0; 5], "the handler is masked");
    assert_eq!(&plan.code[..8], &FALLS_IN[..8]);
    assert_eq!(&plan.code[13..], &FALLS_IN[13..]);

    assert!(splice_callee_handler_plan(&GOES_TO, GOES_TO.len(), &[(2, 5, 8)]).is_none());
    let plan = splice_callee_handler_plan_with(&GOES_TO, GOES_TO.len(), &[(2, 5, 8)], true)
        .expect("wide: a run ending in a goto to the join");
    assert_eq!(plan.runs, vec![(8, 16)]);
    assert_eq!(&plan.code[8..16], &[0; 8]);

    assert!(
        splice_callee_handler_plan(&CASTS, CASTS.len(), &[(2, 10, 13)]).is_none(),
        "the narrow range list has no checkcast"
    );
    let plan = splice_callee_handler_plan_with(&CASTS, CASTS.len(), &[(2, 10, 13)], true)
        .expect("wide: checkcast in the range");
    assert_eq!(plan.runs, vec![(13, 18)]);

    // A conditional branch inside the handler is still refused.
    let branchy = [
        0xb8, 0x00, 0x02, // 0: invokestatic h()V
        0xa7, 0x00, 0x09, // 3: goto 12
        0x4b, // 6: astore_0 (handler)
        0x03, // 7: iconst_0
        0x99, 0x00, 0x03, // 8: ifeq 11
        0x00, // 11: nop
        0xb1, // 12: return (normal join)
    ];
    assert!(cratonvm_reader::verified_code(&branchy).is_ok());
    // Conditional branches inside handlers are now admitted (Unit 2B).
    assert!(splice_callee_handler_plan_with(&branchy, branchy.len(), &[(0, 3, 6)], true).is_some());
    let on = cratonvm_types::flags::with_thread_overrides(
        &[
            (
                concat!("CRATON", "VM_JIT_IR_SPLICE_CALLEE_HANDLERS"),
                Some("1"),
            ),
            (
                concat!("CRATON", "VM_JIT_IR_SPLICE_CALLEE_HANDLERS_WIDE"),
                Some("1"),
            ),
        ],
        ir_splice_callee_handlers_wide_enabled,
    );
    assert!(on);
    assert!(
        ir_splice_callee_handlers_wide_enabled(),
        "default ON since JIT r24"
    );
}

/// JIT r23 wave 11 (lane L5): `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLER_TRAPS`
/// alone admits a handler body (widened), covers it with the spliced
/// edges without the method-level call-site edges, and takes the
/// posting-mode JVMTI rule. It is on by default since R23-W11-FLIP; with
/// it off (`=0`) every combinator answers what the family answers.
#[test]
fn r23_w11_l5_the_handler_trap_switch_admits_the_uniapplynow_tree_alone() {
    let traps = concat!("CRATON", "VM_JIT_IR_SPLICE_CALLEE_HANDLER_TRAPS");
    // Default ON since R23-W11-FLIP.
    cratonvm_types::flags::with_thread_overrides(&[(traps, None)], || {
        assert!(ir_splice_callee_handler_traps_enabled(), "default ON");
        assert!(ir_splice_callee_handlers_admitted());
        assert!(ir_splice_callee_handlers_wide_admitted());
        assert!(ir_splice_callee_handler_edges_admitted());
        // Posting mode admits; an armed listener refuses.
        assert!(ir_splice_callee_handler_events_admitted(true, false));
        assert!(!ir_splice_callee_handler_events_admitted(true, true));
    });
}

/// JIT r23 wave 11 (lane L5): a source pin. With the trap switch alone,
/// the builder's spliced call-site edges must read only the callee's rows,
/// never the method-level switch. They then build the graph that
/// `the_handler_entry_trap_ends_its_block_with_a_throw` checks under the
/// family: an edge from the covered call into the handler, and the wave-10
/// trap (guard plus `Throw`) at the handler entry.
#[test]
fn r23_w11_l5_the_trap_switch_needs_no_method_level_edges_in_the_builder() {
    let src = concat!(
        include_str!("../ir/mod.rs"),
        "\n",
        include_str!("../ir/node_id.rs"),
        "\n",
        include_str!("../ir/safepoints.rs"),
        "\n",
        include_str!("../ir/loop_phis.rs"),
        "\n",
        include_str!("../ir/set_osr_requested_header.rs"),
        "\n",
        include_str!("../ir/splice_op_cannot_raise.rs"),
        "\n",
        include_str!("../ir/narrow_int_return.rs"),
        "\n",
        include_str!("../ir/is_jdk_throwable_init_candidate.rs"),
        "\n",
        include_str!("../ir/attach_sb_virtual_builders.rs"),
        "\n",
        include_str!("../ir/field_load_memo_mode.rs"),
        "\n",
        include_str!("../ir/ir_splice_store_fence_forward_branch.rs"),
        "\n",
        include_str!("../ir/ir_trap_census.rs")
    );
    let plant = src
        .split("fn plant_call_throw_edges(")
        .nth(1)
        .and_then(|s| s.split("\n    fn ").next())
        .expect("plant_call_throw_edges");
    assert!(
        !plant.contains("ir_call_site_exception_edges_enabled()"),
        "the spliced edges must not depend on the method-level switch"
    );
    assert!(plant.contains("splice_handler_rows"));
}

/// The frame-site table carries each body's rows rebased to the body,
/// for the chain-snapshot liveness; a row outside the body leaves an
/// empty entry (prune nothing).
#[test]
fn r21_keepset_frame_sites_rebase_a_body_s_handler_rows() {
    let site = |base: usize| IrInlineSite {
        base,
        code_len: 14,
        num_args: 1,
        max_locals: 3,
        arg_local_slots: vec![0],
        returns_value: false,
        receiver_is_arg0: false,
        method_key: "T.f:(Ljava/lang/Object;)V".to_string(),
        class_id: 0,
    };
    let mut sites: HashMap<usize, IrInlineSite> = HashMap::new();
    sites.insert(3, site(40));
    sites.insert(9, site(60));
    let row = |s: usize, e: usize, h: usize| IrSpliceHandlerRow {
        start_pc: s,
        end_pc: e,
        handler_pc: h,
        catch_name: "java/lang/Throwable",
        declaring_class_id: 7,
        bare_athrow: false,
    };
    let mut rows: HashMap<usize, Vec<IrSpliceHandlerRow>> = HashMap::new();
    rows.insert(40, vec![row(42, 45, 48)]);
    rows.insert(60, vec![row(62, 99, 68)]); // past the body's 14 bytes
    let table = IrInlineFrameSites::from_sites(&sites).with_handler_rows(&rows);
    assert_eq!(table.handler_rows(40), Some(&[(2, 5, 8)][..]));
    assert_eq!(
        table.handler_rows(60),
        Some(&[][..]),
        "unrebasable: model nothing"
    );
    assert_eq!(
        IrInlineFrameSites::from_sites(&sites).handler_rows(40),
        None
    );
}
