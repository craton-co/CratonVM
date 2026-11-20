use super::*;

/// A lowerer over `int f(int a) { return a + 1; }`, with nothing planned.
fn bare_lowerer() -> (Lowerer<'static>, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let sum = g.add(Op::Add, IrType::Int, vec![a, one], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], None);
    let schedule = crate::ir_schedule::schedule(&g);
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    (
        super::tests::lowerer_over(graph, schedule, plan, 16384),
        sum,
    )
}

/// The routed producers: `int` / `long` call results and static reads,
/// nothing else.
#[test]
fn w8_4_routed_results_are_int_or_long_calls_and_static_reads() {
    use super::r25_w7a_reg_alloc::routed_result as routed;
    let call = Op::Call { info_ptr: 0 };
    assert!(routed(&call, IrType::Int));
    assert!(routed(&call, IrType::Long));
    assert!(
        !routed(&call, IrType::Ref),
        "a reference home is never dropped"
    );
    assert!(!routed(&call, IrType::Double), "FP lives in the other file");
    assert!(!routed(&call, IrType::Void));
    assert!(
        !routed(&Op::Add, IrType::Int),
        "an ALU op is the default list's"
    );
    assert!(
        !routed(&Op::Load(MemKind::Int), IrType::Int),
        "a field read is fix 2's"
    );
}

/// W7 fix 3: under the allocator an `int` call result is a site-naming
/// producer only when no inline prefix can claim its call: every prefix
/// declines a dropped result home, so dropping it would turn the prefix
/// back into the call. A plain static call stays one; `hashCode()I` (the
/// box-hash prefix's screen) does not; nothing is claimed off.
#[test]
fn w7_fix3_a_call_a_prefix_may_claim_keeps_its_home() {
    let info = |class: &'static str, name: &'static str, desc: &'static str, kind: u8| {
        let leaked: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: class,
            method_name: name,
            descriptor: desc,
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: kind,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        leaked as *const JitInvokeInfo as usize
    };
    let plain = info("pkg/B", "a", "(I)I", 3);
    let prefixed = info("java/lang/Object", "hashCode", "()I", 0);
    let (mut lo, _) = bare_lowerer();
    let mut g = Graph::empty(0);
    let p = g.add(Op::Call { info_ptr: plain }, IrType::Int, vec![], None);
    let q = g.add(Op::Call { info_ptr: prefixed }, IrType::Int, vec![], None);
    let (plain_node, prefixed_node) = (&g.nodes[p as usize], &g.nodes[q as usize]);
    assert!(
        !lo.reg_alloc_home_is_one_store(plain_node),
        "off: nothing claimed"
    );
    lo.reg_alloc_on = true;
    assert!(
        lo.reg_alloc_home_is_one_store(plain_node),
        "a call no prefix screens is a routed producer"
    );
    assert!(
        !lo.reg_alloc_home_is_one_store(prefixed_node),
        "a call a prefix may claim keeps its home"
    );
}

/// The body-end scan refuses a recorded routed result exactly when a
/// branch lands past its publish and up to its node's end, and never with
/// nothing recorded.
#[test]
fn w8_4_a_branch_past_the_publish_refuses_the_value() {
    // NOP x6, then `JMP rel8` at 6 whose target is `8 + rel`.
    let body = |rel: u8| -> Vec<u8> { vec![0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0xEB, rel] };
    for (rel, refused) in [(0xFCu8, true), (0xF9u8, false)] {
        let (mut lo, v) = bare_lowerer();
        let at = lo.buf.pos();
        lo.buf.emit(&body(rel));
        // Published at +2; the node ends at +6. 0xFC lands at +4 (inside),
        // 0xF9 at +1 (before the publish: that route reaches it).
        lo.reg_alloc_routed = vec![(v, at + 2, at + 6)];
        lo.verify_reg_alloc_routed_defs();
        assert_eq!(
            lo.take_latched_bailout().is_some(),
            refused,
            "rel8 {rel:#x}: a landing past the publish refuses, one before it does not"
        );
    }
    let (mut lo, _) = bare_lowerer();
    lo.buf.emit(&body(0xFC));
    lo.verify_reg_alloc_routed_defs();
    assert!(
        lo.take_latched_bailout().is_none(),
        "nothing recorded, nothing refused"
    );
}

/// W7 fix 4: the routed-result scan counts a rel32 field only behind a
/// `CALL` / `JMP` / `Jcc rel32` or RIP-relative `LEA` byte. A frame
/// displacement aimed at the window is no hit any more (the tail
/// carries' scan still counts it); each branch form, a rel8 branch, an
/// absolute word and an exempt field behave as before.
#[test]
fn w7_fix4_the_routed_scan_counts_branch_fields_only() {
    use std::collections::HashSet;
    // The window [4, 8) sits in a NOP run; each case appends one
    // instruction at 16 aimed at 6 (rel = 6 - end of field).
    let target = 6usize;
    let case = |insn: &[u8]| -> Vec<u8> {
        let mut code = vec![0x90u8; 16];
        code.extend_from_slice(insn);
        code
    };
    let rel_to =
        |field_at: usize| -> [u8; 4] { (target as i32 - (field_at as i32 + 4)).to_le_bytes() };
    let wanted: HashSet<usize> = (4..8).collect();
    let none: HashSet<usize> = HashSet::new();
    let scan = |code: &[u8], exempt: &HashSet<usize>| {
        super::branch_targets_except(code, 0x1000_0000, &wanted, exempt)
    };
    // mov rax, [rbp + disp32]: 48 8B 85 <disp32>, field at 19.
    let mut disp = vec![0x48, 0x8B, 0x85];
    disp.extend_from_slice(&rel_to(19));
    let code = case(&disp);
    assert!(
        scan(&code, &none).is_empty(),
        "a frame displacement is no branch"
    );
    assert!(
        !super::home_reload_targets_except(&code, 0x1000_0000, &wanted, &none).is_empty(),
        "the tail carries' scan still counts it (the control of this case)"
    );
    for (name, head) in [
        ("jmp rel32", vec![0xE9u8]),
        ("call rel32", vec![0xE8]),
        ("jz rel32", vec![0x0F, 0x84]),
        ("lea rax, [rip + d]", vec![0x48, 0x8D, 0x05]),
    ] {
        let at = 16 + head.len();
        let mut insn = head.clone();
        insn.extend_from_slice(&rel_to(at));
        let code = case(&insn);
        assert_eq!(scan(&code, &none), [target].into(), "{name} is a hit");
        let exempt: HashSet<usize> = [at].into();
        assert!(
            scan(&code, &exempt).is_empty(),
            "{name}: an exempt field is not"
        );
    }
    // jmp rel8 at 16: target = 18 + rel.
    let code = case(&[0xEB, (target as i8 - 18) as u8]);
    assert_eq!(
        scan(&code, &none),
        [target].into(),
        "a rel8 branch is a hit"
    );
    // An absolute word naming base + 6.
    let code = case(&(0x1000_0000u64 + target as u64).to_le_bytes());
    assert_eq!(
        scan(&code, &none),
        [target].into(),
        "an absolute word is a hit"
    );
}

/// With the allocator off the node check records and refuses nothing; on,
/// a non-routed node is ignored too.
#[test]
fn w8_4_the_node_check_is_inert_off_and_for_other_nodes() {
    let (mut lo, v) = bare_lowerer();
    lo.reg_alloc_publish_at = Some((v, 3));
    lo.reg_alloc_check_routed_def(v, true);
    assert!(lo.reg_alloc_routed.is_empty());
    assert!(
        lo.reg_alloc_publish_at.is_none(),
        "the publish record is taken per node"
    );
    assert!(lo.take_latched_bailout().is_none());
    lo.reg_alloc_on = true;
    lo.reg_alloc_check_routed_def(v, true);
    assert!(lo.reg_alloc_routed.is_empty(), "an Add is no routed result");
    assert!(lo.take_latched_bailout().is_none());
}

/// Source pins: the publish is recorded only under the allocator, the
/// node check runs after the post-call and post-static sites, the scan
/// runs after the tail carries' (every branch patched), and the parameter
/// copy is implied.
#[test]
fn w8_4_every_new_path_is_gated_and_placed() {
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
    let body = |head: &str| -> String {
        src.split(head)
            .nth(1)
            .unwrap_or_else(|| panic!("`{head}` is in this file"))
            .split("\n    pub")
            .next()
            .expect("the function ends")
            .to_string()
    };
    let publish = body("fn publish_def_at_store(&mut self, offset: i32) -> bool {");
    assert!(publish.contains(
        "if self.reg_alloc_on {\n                self.reg_alloc_publish_at = Some((id, self.buf.pos()));"
    ));
    let tracked = body("fn lower_data_node_tracked(&mut self, id: NodeId) {");
    let site = tracked
        .find("self.maybe_emit_post_static_helper_exit_site(id);")
        .expect("the post-static site");
    let check = tracked
        .find("self.reg_alloc_check_routed_def(id, w8_prefixed);")
        .expect("the node check");
    let prefix = tracked
        .find("let w8_prefixed = self.call_prefix_join.is_some();")
        .expect("the prefix flag");
    let take = tracked
        .find("if let Some(p) = self.call_prefix_join.take() {")
        .expect("the prefix join");
    assert!(prefix < take && site < check);
    let inner = src
        .split("fn lower_inner_attempt(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .expect("lower_inner_attempt is in this file");
    let tails = inner
        .find("lowerer.verify_tail_carry_points();")
        .expect("the tail-carry scan");
    let ours = inner
        .find("lowerer.verify_reg_alloc_routed_defs();")
        .expect("the routed scan");
    let patched = inner
        .find("lowerer.patch_branches();")
        .expect("the branch fixups");
    assert!(patched < tails && tails < ours);
    let residency = src
        .split("fn plan_register_residency_with(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .expect("the plan is in this file");
    assert!(residency
        .contains("|| r25_w7a_reg_alloc::implied(reg_alloc, r25_w7a_reg_alloc::PARAM_COPY_KEY)"));
    let check_fn = body("fn reg_alloc_check_routed_def(");
    assert!(check_fn.contains("if !self.reg_alloc_on {"));
}
