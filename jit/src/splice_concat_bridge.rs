//! j27 wave 4, lane SPLICECAT (`CRATONVM_JIT_SPLICE_CONCAT_BRIDGE`, opt-in):
//! a spliced body's `StringConcatFactory` bridge sites of any arity as
//! [`crate::InlineSite::ir_indy_rows`] rows.
//!
//! Page: `docs/known-issues/jit/j27-splice-refuses-callee-with-arg-taking-concat-proposal-20261028.md`.
//!
//! Before this, a spliced `invokedynamic` was admitted only as a `(0, L)`
//! constant-word lambda row, so any small callee with `"x=" + v` anywhere in
//! its body kept its call. A concat row is `(callee pc, argument entries > 0,
//! b'L', concat bridge site)`: the VM resolver
//! (`jit_bridge::splice_concat_rows`) admits only a site whose kind tag is
//! `JIT_INDY_SITE_CONCAT`, the pool-built site the compiled body of the callee
//! itself calls. Both tiers then emit the SAME bridge call with the SAME
//! staged operands as that body:
//!
//! * optimizing tier: `IrBuilder::push_indy_bridge_call` builds the
//!   `Op::IndyBridge` (lowered exactly as a method-own bridged site: safepoint
//!   map before the staging, inline-frame row at the return address, the
//!   `i64::MIN` exception edge), and the builder records the node as a call
//!   the splice made (`IrBuilder::note_splice_call`): an operand `toString()`
//!   is arbitrary Java, so a later trap in the same splice may not replay the
//!   invoke, and the successor gets the post-call chain snapshot (the callee
//!   frame with the concat result on its operand stack);
//! * single-pass tier: `x64::inlining`'s `0xba` arm stages the operands and
//!   calls the bridge like the spliced dispatch call, then files the post-call
//!   exit as after any spliced call.
//!
//! The planner counts a concat row as a surviving call
//! ([`concat_row_at`]): a body that may trap after one, or that stores before
//! one, keeps its call rather than costing the caller its build.
//!
//! Opt-in rather than default ON: the change moves where operand
//! `toString()`s and their exceptions run (inside the caller's compiled
//! frame), which only a host run of the probe
//! (`tools/probes/jit/j27/J27SpliceConcatRows.java`) can show matches HotSpot.

/// `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE` (j27 wave 4, lane SPLICECAT; perf,
/// **opt-in**, `=1` enables): may a spliced body's concat bridge site with
/// arguments be an `ir_indy_rows` row, in both tiers? Read per resolution, per
/// rebase and per build (no static); the VM resolver reads the same switch.
pub fn splice_concat_bridge_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_SPLICE_CONCAT_BRIDGE")
}

/// May a spliced `ir_indy_rows` row of this shape be rebased? `(0, L)` is the
/// constant-word lambda row (W10L3-1); `(n > 0, L)` is a concat bridge row,
/// under [`splice_concat_bridge_enabled`] only.
pub fn indy_row_shape_admitted(arg_entries: usize, ret_tag: u8) -> bool {
    ret_tag == b'L' && (arg_entries == 0 || splice_concat_bridge_enabled())
}

/// The concat row of `site` at callee pc `pc` (a row with arguments), if any.
pub fn concat_row_at(site: &crate::InlineSite, pc: usize) -> Option<(usize, usize, u8, usize)> {
    site.ir_indy_rows
        .iter()
        .find(|r| r.0 == pc && r.1 > 0)
        .copied()
}

/// Does `site` hold a concat row? Such a body leaves a call behind.
pub fn site_has_concat_rows(site: &crate::InlineSite) -> bool {
    site.ir_indy_rows.iter().any(|r| r.1 > 0)
}

/// Does `op` commit a store a replay would repeat (a field, static or array
/// element write)? A concat row after one keeps the site a call: the builder's
/// fence admits no call after a committed store, and refusing there costs the
/// whole method.
pub fn commits_a_store(op: u8) -> bool {
    matches!(op, 0x4f..=0x56 | 0xb3 | 0xb5)
}

/// The replay planner's (`ir_splice_body_may_trap_after_a_call`) step for
/// `op` at callee pc `pc`, walked in pc order: `None` when a concat row there
/// follows a store the walk has seen (the site keeps its call), else whether
/// the instruction is a concat call. Records a store in `store_seen`.
pub fn planner_concat_step(
    site: &crate::InlineSite,
    pc: usize,
    op: u8,
    store_seen: &mut bool,
) -> Option<bool> {
    let concat = op == 0xba && concat_row_at(site, pc).is_some();
    if concat && *store_seen {
        return None;
    }
    *store_seen |= commits_a_store(op);
    Some(concat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ir, InlineSite, JitInvokeInfo, IR_INLINE_MAX_TOTAL_BYTES};
    use std::collections::HashMap;

    const SITE: usize = 0x5A5A_1357_9BDF_2468;
    const SWITCH: &str = "CRATONVM_JIT_SPLICE_CONCAT_BRIDGE";
    const INDY: &str = "CRATONVM_JIT_IR_SPLICE_INDY";

    /// `static String tag(int v) { return "v=" + v; }`:
    /// `0: iload_0  1: invokedynamic #2 (I)Ljava/lang/String;  6: areturn`.
    fn tag_body(rows: Vec<(usize, usize, u8, usize)>) -> InlineSite {
        InlineSite {
            callee_code: vec![0x1a, 0xba, 0x00, 0x02, 0x00, 0x00, 0xb0, 0, 0],
            callee_code_len: 7,
            callee_max_locals: 1,
            callee_num_args: 1,
            callee_is_static: true,
            return_type: b'L',
            class_name: "p/Host".to_string(),
            class_id: 7,
            method_name: "tag".to_string(),
            descriptor: "(I)Ljava/lang/String;".to_string(),
            ir_indy_rows: rows,
            ..Default::default()
        }
    }

    /// Append `site` at caller pc 3 of a 10-byte caller, as the method door.
    fn append(site: InlineSite) -> (Result<(), &'static str>, ir::IrInlineTables) {
        let mut combined = vec![0u8; 10];
        let mut tables = ir::IrInlineTables::default();
        let mut owned_strings: Vec<Box<str>> = Vec::new();
        let mut owned_invoke_infos: Vec<Box<JitInvokeInfo>> = Vec::new();
        let mut direct_callee_entries: Vec<(usize, u64)> = Vec::new();
        let mut budget = IR_INLINE_MAX_TOTAL_BYTES;
        let mut vcalls: Vec<(usize, usize)> = Vec::new();
        let mut compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let mut static_init: Vec<u32> = Vec::new();
        let mut direct_calls: Vec<(usize, (usize, bool))> = Vec::new();
        let mut checkcast_seen = false;
        let mut volatile: Vec<usize> = Vec::new();
        let mut exact_nested: Vec<(usize, u32)> = Vec::new();
        let result = crate::append_ir_inline_site(
            site,
            3,
            &mut combined,
            &mut tables,
            &mut owned_strings,
            &mut owned_invoke_infos,
            &mut direct_callee_entries,
            &mut budget,
            &mut vcalls,
            &mut compact,
            false,
            false,
            &mut static_init,
            &mut direct_calls,
            &mut checkcast_seen,
            &mut volatile,
            &mut exact_nested,
            None,
            &|_, _, _| None,
            ir::SpliceFenceRelax::NONE,
            false,
        );
        (result, tables)
    }

    #[test]
    fn splicecat_a_concat_row_is_rebased_only_under_the_switch() {
        cratonvm_types::flags::with_thread_overrides(&[(INDY, None), (SWITCH, Some("1"))], || {
            let (result, tables) = append(tag_body(vec![(1, 1, b'L', SITE)]));
            assert_eq!(result, Ok(()));
            assert_eq!(tables.indy_trap_sites, HashMap::from([(11usize, (1usize, b'L'))]));
            assert_eq!(tables.indy_bridge_sites, HashMap::from([(11usize, SITE)]));
            // A non-reference result is never a concat row.
            let (result, _) = append(tag_body(vec![(1, 1, b'I', SITE)]));
            assert_eq!(result, Err("ir-splice-indy-row-mismatch"));
            // Handler rows keep the call, as for the lambda rows.
            let mut handlers = tag_body(vec![(1, 1, b'L', SITE)]);
            handlers.ir_handler_rows =
                vec![(0, 6, 6, "java/lang/RuntimeException".to_string(), 7)];
            assert_eq!(append(handlers).0, Err("ir-splice-indy-with-handlers"));
        });
        cratonvm_types::flags::with_thread_overrides(&[(INDY, None), (SWITCH, None)], || {
            let (result, tables) = append(tag_body(vec![(1, 1, b'L', SITE)]));
            assert_eq!(result, Err("ir-splice-indy-row-mismatch"), "opt-in: off by default");
            assert!(tables.indy_bridge_sites.is_empty());
        });
    }

    /// `"v=" + v` then `areturn`: nothing can trap after the concat, so the
    /// planner admits the site. With an `idiv` after it (a trap that would
    /// replay the invoke, and with it the concat's `toString()`s), or a
    /// `putfield` before it, the site keeps its call.
    #[test]
    fn splicecat_the_planner_counts_a_concat_row_as_a_call() {
        let site = tag_body(vec![(1, 1, b'L', SITE)]);
        assert!(!crate::ir_splice_body_may_trap_after_a_call(&site, None));
        assert!(crate::ir_splice_tree_leaves_a_call(&site, None));

        // iload_0; invokedynamic (I)String; pop; iload_0; iload_0; idiv; pop; aconst_null; areturn
        let mut trap_after = tag_body(vec![(1, 1, b'L', SITE)]);
        trap_after.callee_code =
            vec![0x1a, 0xba, 0x00, 0x02, 0x00, 0x00, 0x57, 0x1a, 0x1a, 0x6c, 0x57, 0x01, 0xb0, 0, 0];
        trap_after.callee_code_len = 13;
        assert!(crate::ir_splice_body_may_trap_after_a_call(&trap_after, None));

        // aload_1; iload_0; putfield #3; iload_0; invokedynamic; areturn
        let mut store_before = tag_body(vec![(6, 1, b'L', SITE)]);
        store_before.callee_code =
            vec![0x2b, 0x1a, 0xb5, 0x00, 0x03, 0x1a, 0xba, 0x00, 0x02, 0x00, 0x00, 0xb0, 0, 0];
        store_before.callee_code_len = 12;
        assert!(crate::ir_splice_body_may_trap_after_a_call(&store_before, None));
        // The same body without its row: the planner's round-11 answer.
        store_before.ir_indy_rows.clear();
        assert!(!crate::ir_splice_body_may_trap_after_a_call(&store_before, None));

        // The constant-word lambda row keeps its W10L3-1 answer.
        let lambda = tag_body(vec![(1, 0, b'L', SITE)]);
        assert!(!crate::ir_splice_tree_leaves_a_call(&lambda, None));
    }

    #[test]
    fn splicecat_row_shapes() {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, None)], || {
            assert!(indy_row_shape_admitted(0, b'L'));
            assert!(!indy_row_shape_admitted(2, b'L'));
        });
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
            assert!(indy_row_shape_admitted(2, b'L'));
            assert!(!indy_row_shape_admitted(2, b'I'));
            assert!(!indy_row_shape_admitted(0, b'J'));
        });
        let site = tag_body(vec![(1, 0, b'L', SITE), (9, 2, b'L', SITE)]);
        assert_eq!(concat_row_at(&site, 1), None);
        assert_eq!(concat_row_at(&site, 9), Some((9, 2, b'L', SITE)));
        assert!(site_has_concat_rows(&site));
        assert!(commits_a_store(0xb5) && commits_a_store(0x53) && !commits_a_store(0xb4));
    }

    /// The builder: `static String f(int x) { return tag(x); }` with `tag`
    /// spliced. The concat row builds as its `Op::IndyBridge` with the one
    /// operand, the node counts as a call the splice made (its successor gets
    /// the post-call chain snapshot: the callee at `areturn` with the result on
    /// its stack), and without the switch the spliced indy refuses the build.
    #[test]
    fn splicecat_a_spliced_concat_builds_as_its_bridge_with_its_operands() {
        use crate::ir::{IrBuilder, IrInlineSite, IrInlineTables, IrType, Op};
        let build = || {
            // 0: iload_0  1: invokestatic #1 (I)Ljava/lang/String;  4: areturn
            let caller: [u8; 5] = [0x1a, 0xb8, 0x00, 0x01, 0xb0];
            let callee: [u8; 7] = [0x1a, 0xba, 0x00, 0x02, 0x00, 0x00, 0xb0];
            let code_len = caller.len();
            let mut combined = caller.to_vec();
            let base = combined.len();
            combined.extend_from_slice(&callee);
            combined.extend_from_slice(&[0, 0]);
            let mut b = IrBuilder::new(1, 1);
            b.set_param_types(&[IrType::Int]);
            b.graph.scope_method_key = "p/Host.f:(I)Ljava/lang/String;".to_string();
            b.set_spliced_indy_wired(true);
            let mut tables = IrInlineTables {
                sites: HashMap::from([(
                    1usize,
                    IrInlineSite {
                        base,
                        code_len: callee.len(),
                        num_args: 1,
                        max_locals: 1,
                        arg_local_slots: vec![0],
                        returns_value: true,
                        receiver_is_arg0: false,
                        method_key: "p/Host.tag:(I)Ljava/lang/String;".to_string(),
                        class_id: 7,
                    },
                )]),
                ..IrInlineTables::default()
            };
            tables.indy_trap_sites.insert(base + 1, (1, b'L'));
            tables.indy_bridge_sites.insert(base + 1, SITE);
            b.apply_inline_tables(tables);
            b.build(&combined, code_len).map(|g| (g, base))
        };
        let on = [
            (INDY, None),
            (SWITCH, Some("1")),
            ("CRATONVM_JIT_IR_POST_CALL_SPLICE_EXITS", None),
        ];
        cratonvm_types::flags::with_thread_overrides(&on, || {
            let (g, base) = build().expect("the splice with its concat row builds");
            let bridge = g
                .nodes
                .iter()
                .position(|n| n.op == Op::IndyBridge { site: SITE })
                .expect("the spliced concat is the bridge node");
            assert_eq!(g.nodes[bridge].inputs.len(), 3, "ctrl, mem and the one operand");
            assert_eq!(g.nodes[bridge].bytecode_pc, Some(base + 1));
            assert!(
                !g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
                "tag itself was spliced, not called"
            );
            let post = g
                .safepoints
                .iter()
                .find(|sp| sp.post_call_only && sp.post_call_of as usize == bridge)
                .expect("the concat's successor has its post-call chain snapshot");
            let chain = post.splice_scope.as_ref().expect("a chain");
            assert_eq!(chain.scopes[0].bci, 6, "the callee resumes past the concat");
            assert_eq!(post.stack.first().map(|&v| v as usize), Some(bridge));
        });
        cratonvm_types::flags::with_thread_overrides(&[(INDY, None), (SWITCH, None)], || {
            assert!(build().is_none(), "switch off: the spliced concat refuses the build");
        });
    }
}
