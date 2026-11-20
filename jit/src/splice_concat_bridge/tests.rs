    use super::*;
    use crate::{ir, InlineSite, JitInvokeInfo, IR_INLINE_MAX_TOTAL_BYTES};
    use std::collections::HashMap;

    const SITE: usize = 0x5A5A_1357_9BDF_2468;
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
            &mut Vec::new(),
            None,
            &mut exact_nested,
            None,
            &|_, _, _| None,
            ir::SpliceFenceRelax::NONE,
            false,
        );
        (result, tables)
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

