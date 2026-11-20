use super::*;

/// R25-FLIP: the switch is default ON: unset and `=1` on, `=0` off.


/// The copy precedes the site by schedule position inside one block and
/// by dominance across blocks -- never by anything else.
#[test]
fn r25_item2_the_copy_precedes_a_site_by_position_or_dominance() {
    let never = |_: usize, _: usize| false;
    let always = |_: usize, _: usize| true;
    assert!(param_copy_precedes_site((0, 3), (0, 4), never), "later in the block");
    assert!(!param_copy_precedes_site((0, 3), (0, 3), always), "the same position");
    assert!(!param_copy_precedes_site((0, 3), (0, 2), always), "earlier in the block");
    assert!(param_copy_precedes_site((0, 9), (2, 1), always), "a dominated block");
    assert!(
        !param_copy_precedes_site((0, 1), (2, 9), never),
        "a block the copy's does not dominate, whatever the positions"
    );
    assert!(
        param_copy_precedes_site((0, 1), (2, 0), |a, b| a == 0 && b == 2),
        "the dominance query is asked (copy block, site block)"
    );
}

/// `void f(int a) { a + 1; }`: node `a` stands in for a reference
/// parameter (the drop rule reads the pair table, not the node's type) and
/// the `Add` is the armed site after it. Returns the lowerer, `a` and the
/// site.
fn fixture() -> (Lowerer<'static>, NodeId, NodeId) {
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
    let lo = super::tests::lowerer_over(graph, schedule, plan, 16384);
    (lo, a, sum)
}

/// Arm `lo` as the switch would at `site`: `a`'s prologue word is `home`,
/// the copy is emitted into the first spill word, and `site` is the armed
/// GC point at its own schedule position.
fn arm(lo: &mut Lowerer<'static>, a: NodeId, site: NodeId, home: i16) {
    let n = lo.graph.nodes.len();
    lo.ref_param_homes = vec![home];
    lo.shadow_live_param_homes = vec![(a, home)];
    lo.defined_nodes.resize(n, false);
    lo.defined_nodes[a as usize] = true;
    lo.node_slot.resize(n, None);
    let copy = u32::try_from(lo.first_spill).expect("a positive spill base");
    lo.node_slot[a as usize] = std::num::NonZeroU32::new(copy);
    let pos = lo.slot_plan.pos_of[site as usize].expect("the site is scheduled");
    lo.dead_ref_clear_site = Some((pos, 4, site));
}

fn zero_store(home: i16) -> Vec<u8> {
    let mut want = vec![0x48, 0xC7, 0x85];
    want.extend_from_slice(&(-i32::from(home)).to_le_bytes());
    want.extend_from_slice(&0i32.to_le_bytes());
    want
}

/// The lazy case (the default `REF_PARAM_HOME_ZERO_LAZY` took the
/// parameter out of the eager set): the word still holds the parameter, so
/// it is zeroed here, then dropped. The eager case: zero already, dropped
/// with no store.
#[test]
fn r25_item2_a_param_home_after_its_copy_is_zeroed_and_dropped() {
    let home: i16 = 16;
    let (mut lo, a, site) = fixture();
    assert!(i32::from(home) < lo.first_spill, "fixture: a prologue word");
    arm(&mut lo, a, site, home);
    let at = lo.buf.pos();
    let (dropped, zeroed) = lo.shadow_live_param_home_drops();
    assert_eq!(dropped, vec![home]);
    assert_eq!(zeroed, vec![home]);
    assert_eq!(&lo.buf.as_slice()[at..lo.buf.pos()], zero_store(home).as_slice());

    let (mut eager, a, site) = fixture();
    arm(&mut eager, a, site, home);
    eager.ref_param_home_zero = vec![a];
    let at = eager.buf.pos();
    let (dropped, zeroed) = eager.shadow_live_param_home_drops();
    assert_eq!(dropped, vec![home], "eager: still dropped");
    assert!(zeroed.is_empty(), "eager: the copy's own store zeroed it");
    assert_eq!(eager.buf.pos(), at, "eager: no byte");
}

/// Every refusal leaves the map as it was and emits nothing.
#[test]
fn r25_item2_every_refusal_drops_nothing_and_stores_nothing() {
    const HOME: i16 = 16;
    type Spoil = fn(&mut Lowerer<'static>, NodeId);
    fn off(lo: &mut Lowerer<'static>, _: NodeId) {
        lo.shadow_live_param_homes.clear();
    }
    fn unarmed(lo: &mut Lowerer<'static>, _: NodeId) {
        lo.dead_ref_clear_site = None;
    }
    fn poll_key(lo: &mut Lowerer<'static>, _: NodeId) {
        let key = poll_site_key(lo.graph, 0).expect("a key");
        lo.dead_ref_clear_site = lo.dead_ref_clear_site.map(|(p, b, _)| (p, b, key));
    }
    fn block_body(lo: &mut Lowerer<'static>, _: NodeId) {
        lo.frame_block = vec![HOME];
    }
    fn not_emitted(lo: &mut Lowerer<'static>, a: NodeId) {
        lo.defined_nodes[a as usize] = false;
    }
    fn store_dropped(lo: &mut Lowerer<'static>, a: NodeId) {
        let n = lo.graph.nodes.len();
        lo.home_dropped.resize(n, false);
        lo.home_dropped[a as usize] = true;
    }
    fn copy_is_home(lo: &mut Lowerer<'static>, a: NodeId) {
        let w = u32::try_from(i32::from(HOME)).expect("positive");
        lo.node_slot[a as usize] = std::num::NonZeroU32::new(w);
    }
    fn unnamed(lo: &mut Lowerer<'static>, _: NodeId) {
        lo.ref_param_homes.clear();
    }
    fn at_copy(lo: &mut Lowerer<'static>, a: NodeId) {
        let pos = lo.slot_plan.pos_of[a as usize].expect("scheduled");
        lo.dead_ref_clear_site = lo.dead_ref_clear_site.map(|(_, b, s)| (pos, b, s));
    }
    let cases: [(&str, Spoil); 9] = [
        ("off the switch", off),
        ("not an armed site", unarmed),
        ("a poll's synthetic site key", poll_key),
        ("a frame-block body", block_body),
        ("the copy not emitted", not_emitted),
        ("the copy's store dropped", store_dropped),
        ("the copy IS the prologue word", copy_is_home),
        ("not a prologue word the maps name", unnamed),
        ("the site at the copy's own position", at_copy),
    ];
    let home = HOME;
    for (why, spoil) in cases {
        let (mut lo, a, site) = fixture();
        arm(&mut lo, a, site, home);
        spoil(&mut lo, a);
        let at = lo.buf.pos();
        let (dropped, zeroed) = lo.shadow_live_param_home_drops();
        assert!(dropped.is_empty() && zeroed.is_empty(), "{why}: nothing dropped");
        assert_eq!(lo.buf.pos(), at, "{why}: no byte");
    }
}

/// Proposals 1 and 3 add two more switches, default ON since R25-FLIP.


/// Proposal 1: the receiver joins only for an instance method known not
/// to be synchronized and not self-locking, under the switch; a
/// hand-built graph (`method_synchronized == None`) admits nothing.
#[test]
fn r25_item2_p1_the_receiver_is_admitted_only_for_a_known_unsynchronized_instance_method() {
    assert!(shadow_live_receiver_admitted(Some(0), Some(false), false, true));
    for (why, recv, sync, locks, on) in [
        ("off the switch", Some(0), Some(false), false, false),
        ("a static method (parameter 0 is a candidate)", None, Some(false), false, true),
        ("ACC_SYNCHRONIZED (wrapped entry or self-lock)", Some(0), Some(true), false, true),
        ("unknown: a hand-built graph", Some(0), None, false, true),
        ("a self-locking body", Some(0), Some(false), true, true),
    ] {
        assert!(!shadow_live_receiver_admitted(recv, sync, locks, on), "{why}");
    }
    assert_eq!(Graph::empty(0).method_synchronized, None, "hand-built: unknown");
}

/// Proposal 3: the predicate that lets a frame block coexist with the
/// liveness claim.
#[test]
fn r25_item2_p3_a_frame_block_composes_only_when_every_claim_is_backed_by_a_zero() {
    let f = Lowerer::<'static>::frame_block_composes_with_liveness;
    assert!(!f(false, true, true, true), "off the switch: the old refusal");
    assert!(f(true, false, false, false), "no converged plan: no claim at all");
    assert!(f(true, true, true, false), "converged and complete: planned clears");
    assert!(f(true, true, false, true), "converged, incomplete: the dense stores");
    assert!(
        !f(true, true, false, false),
        "a claim with neither clear form would name a non-zero dead word"
    );
}

/// Proposal 3, end to end on `plan_frame_block`: with the liveness claim on
/// the block is refused only with the switch at `=0`; otherwise (default ON
/// since R25-FLIP, or `=1`) the block is planned as without the claim.
#[test]
fn r25_item2_p3_plan_frame_block_takes_the_block_under_the_claim_only_with_the_switch() {
    if !crate::x64::shadow_stack_maps_enabled() {
        return;
    }
    let _force = FrameBlockForce::set(true);
    let key = concat!("CRATON", "VM_JIT_IR_SHADOW_FRAME_BLOCK_PRECISE");
    let planned = |switch: Option<&str>, liveness: bool| -> Vec<i16> {
        let (mut lo, _, _) = fixture();
        lo.get_current_thread = 0x7fff_0000_0000_3000;
        lo.reloc_emit_on = true;
        lo.ref_param_homes = vec![16];
        lo.frame_liveness_on = liveness;
        lo.dead_home_plan_converged = true;
        lo.dead_home_clears_complete = true;
        assert!(
            lo.sp_id_slot_off > 0
                && lo.shadow_thread_slot_off > 0
                && lo.shadow_savebase_slot_off > 0
                && lo.shadow_savetop_slot_off > 0,
            "fixture: a wired frame"
        );
        let edits = [(key, switch)];
        cratonvm_types::flags::with_thread_overrides(&edits, || lo.plan_frame_block());
        lo.frame_block.clone()
    };
    assert_eq!(
        planned(None, false),
        vec![16],
        "no claim: the block, as always"
    );
    assert_eq!(
        planned(None, true),
        vec![16],
        "claim, default ON: the block"
    );
    assert_eq!(
        planned(Some("1"), true),
        vec![16],
        "claim, switch on: the block"
    );
}

/// Wave 3: the lazy clear's switch defaults on and honors thread overrides.
#[test]
fn r25_item2_w3_the_lazy_clear_switch_is_opt_in() {
    let key = concat!("CRATON", "VM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR");
    let read = |v: Option<&str>| {
        let edits = [(key, v)];
        cratonvm_types::flags::with_thread_overrides(
            &edits,
            ir_shadow_frame_block_lazy_clear_enabled,
        )
    };
    assert!(read(None), "default ON");
    assert!(!read(Some("0")), "=0 off");
    assert!(read(Some("1")), "=1 on");
}

fn lb(preds: &[usize], steps: &[LazyClearStep], gc_at_end: bool) -> LazyClearBlock {
    LazyClearBlock {
        preds: preds.to_vec(),
        steps: steps.to_vec(),
        gc_at_end,
    }
}

/// Wave 3: a candidate survives only when written before every GC point on
/// every path; a GC point before its own write kills it, so does a path
/// around the write, so does a back-edge poll reached before it.
#[test]
fn r25_item2_w3_a_word_is_lazy_only_when_written_before_every_gc_point() {
    use super::LazyClearStep::{Gc, Write};
    // Straight line: c0 written, then a call; c1 written by the call
    // itself (its GC point comes first).
    let straight = [lb(&[], &[Write(0), Gc, Gc, Write(1)], false)];
    assert_eq!(
        block_lazy_clear_survivors(&straight, 2, 16),
        Some(vec![true, false]),
        "a call's own result is written after its GC point"
    );
    // A diamond: c0 written on one arm only, a call at the join; c1 in
    // the entry block before the branch.
    let diamond = [
        lb(&[], &[Write(1)], false),
        lb(&[0], &[Write(0)], false),
        lb(&[0], &[], false),
        lb(&[1, 2], &[Gc], false),
    ];
    assert_eq!(
        block_lazy_clear_survivors(&diamond, 2, 16),
        Some(vec![false, true]),
        "written on one arm is not written on every path"
    );
    // A loop: header 1, body 2 (writes c0, then polls at its back edge),
    // the exit 3 leaves from the body, after the write.
    let looped = [
        lb(&[], &[Write(1)], false),
        lb(&[0, 2], &[], false),
        lb(&[1], &[Write(0)], true),
        lb(&[2], &[Gc], false),
    ];
    assert_eq!(
        block_lazy_clear_survivors(&looped, 2, 16),
        Some(vec![true, true]),
        "written in the body before the back-edge poll, and before the exit's call"
    );
    // The exit block (3) leaves from the HEADER, reachable before the
    // body ever ran: c0 is not written there.
    let exit_first = [
        lb(&[], &[Write(1)], false),
        lb(&[0, 2], &[], false),
        lb(&[1], &[Write(0)], true),
        lb(&[1], &[Gc], false),
    ];
    assert_eq!(
        block_lazy_clear_survivors(&exit_first, 2, 16),
        Some(vec![false, true]),
        "the loop exit can run before the body's write"
    );
    assert_eq!(
        block_lazy_clear_survivors(&[lb(&[], &[Gc, Write(0)], true)], 1, 16),
        Some(vec![false]),
        "a terminator GC point at the end after the write still needs the earlier one"
    );
    assert_eq!(
        block_lazy_clear_survivors(&[lb(&[], &[Write(0)], true)], 1, 16),
        Some(vec![true]),
        "a calling terminator after the write"
    );
    assert_eq!(
        block_lazy_clear_survivors(&looped, 2, 0),
        None,
        "no sweep allowed: no convergence, nothing left out"
    );
}

/// Wave 3: the merge into the fresh-frame clear's mask -- the lazy words
/// skipped, a lone one cleared after all, the frame lane's own words kept.
#[test]
fn r25_item2_w3_the_lazy_words_merge_into_the_clear_mask() {
    // A 64-byte frame: words 0..8 at [rbp-8] .. [rbp-64].
    let mut skip: Vec<bool> = Vec::new();
    merge_block_lazy_clear_skip(&mut skip, &[24, 32], 64);
    assert_eq!(skip, vec![false, false, true, true, false, false, false, false]);
    let mut lone: Vec<bool> = Vec::new();
    merge_block_lazy_clear_skip(&mut lone, &[24], 64);
    assert!(lone.is_empty(), "a lone word splits a run for nothing: cleared after all");
    let mut lanes = vec![false, true, false, false, false, false, false, false];
    merge_block_lazy_clear_skip(&mut lanes, &[24], 64);
    assert_eq!(
        lanes,
        vec![false, true, true, false, false, false, false, false],
        "next to a word the frame lane left out: not lone"
    );
    let mut odd = vec![false; 3];
    merge_block_lazy_clear_skip(&mut odd, &[24, 32], 64);
    assert_eq!(odd, vec![false; 3], "a mask of another shape is left alone");
    let mut none: Vec<bool> = Vec::new();
    merge_block_lazy_clear_skip(&mut none, &[], 64);
    assert!(none.is_empty());
}

/// Wave 4: the lazy reference homes' switch defaults on and honors thread overrides.
#[test]
fn r25_item2_w4_the_lazy_ref_homes_switch_is_opt_in() {
    let key = concat!("CRATON", "VM_JIT_IR_LAZY_REF_HOMES");
    let read = |v: Option<&str>| {
        let edits = [(key, v)];
        cratonvm_types::flags::with_thread_overrides(&edits, ir_lazy_ref_homes_enabled)
    };
    assert!(read(None), "default ON");
    assert!(!read(Some("0")), "=0 off");
    assert!(read(Some("1")), "=1 on");
}

/// Wave 4: which nodes a lazy reference's range may hold.
#[test]
fn r25_item2_w4_a_lazy_range_holds_only_transparent_and_hooked_nodes() {
    use super::LazyRefNodeClass::{Blocking, Hookable, Transparent};
    for (op, want) in [
        (Op::Add, Transparent),
        (Op::Cmp(crate::ir::CmpOp::Eq), Transparent),
        (Op::Phi, Transparent),
        (Op::Load(MemKind::Ref), Hookable),
        (Op::Load(MemKind::Int), Hookable),
        (Op::ArrayLength, Hookable),
        (Op::ArrayLoad(MemKind::Int), Hookable),
        (Op::ArrayStore(MemKind::Byte), Hookable),
        (Op::ArrayLoad(MemKind::Ref), Blocking),
        (Op::ArrayStore(MemKind::Ref), Blocking),
        (Op::Store(MemKind::Int), Blocking),
        (Op::Call { info_ptr: 0 }, Blocking),
    ] {
        assert_eq!(lazy_ref_node_class(&op), want, "{op:?}");
    }
}

/// Wave 4: the no-unhooked-call scan -- `FF /2` in any form, inside or
/// outside a hooked span; a plain frame store is no call.
#[test]
fn r25_item2_w4_the_scan_finds_calls_outside_hooked_spans() {
    // MOV [rbp-8], rbx ; CALL RAX ; MOV rbx, [rbp-8]
    let code = [0x48, 0x89, 0x5D, 0xF8, 0xFF, 0xD0, 0x48, 0x8B, 0x5D, 0xF8];
    assert!(code_has_unhooked_call(&code, 100, &[]), "an unhooked CALL RAX");
    assert!(!code_has_unhooked_call(&code, 100, &[(100, 110)]), "inside the span");
    assert!(code_has_unhooked_call(&code, 100, &[(100, 104)]), "the span ends before it");
    assert!(code_has_unhooked_call(&[0x41, 0xFF, 0xD3], 0, &[]), "CALL R11");
    assert!(code_has_unhooked_call(&[0xFF, 0x15, 0, 0, 0, 0], 0, &[]), "CALL [rip]");
    assert!(!code_has_unhooked_call(&[0x48, 0x89, 0x5D, 0xF8], 0, &[]), "a store");
    assert!(!code_has_unhooked_call(&[0xFF, 0xC0], 0, &[]), "INC EAX (FF /0)");
}

/// Wave 4: the source audit the scan leans on. Every `CALL` the field-load
/// routes emit is bracketed by the hook, and none of them writes a
/// `CALL rel32` (which the scan does not look for).
#[test]
fn r25_item2_w4_every_field_load_helper_call_is_hooked() {
    let src = concat!(include_str!("../ir_lower/mod.rs"), "\n", include_str!("../ir_lower/virtual_object_info.rs"), "\n", include_str!("../ir_lower/mir_plan_and_init.rs"), "\n", include_str!("../ir_lower/vector_preloop.rs"), "\n", include_str!("../ir_lower/value_phi_copies.rs"), "\n", include_str!("../ir_lower/field_proof_entry_is_rechecked_osr.rs"), "\n", include_str!("../ir_lower/int_expr_bounds.rs"), "\n", include_str!("../ir_lower/character_to_chars_prefix.rs"), "\n", include_str!("../ir_lower/getfield_legacy_read.rs"), "\n", include_str!("../ir_lower/post_call_exit_site_for.rs"), "\n", include_str!("../ir_lower/entry_fold_node.rs"), "\n", include_str!("../ir_lower/call_loaded_ic_entry_releasing.rs"), "\n", include_str!("../ir_lower/fallthrough_poll_spills.rs"), "\n", include_str!("../ir_lower/ir_ic_cascade_admits_kind.rs"), "\n", include_str!("../ir_lower/implicit_kind_matches_catch.rs"), "\n", include_str!("../ir_lower/scoped_raw_tail.rs"), "\n", include_str!("../ir_lower/frame_needs.rs"), "\n", include_str!("../ir_lower/back_edge_dead_ref_homes.rs"), "\n", include_str!("../ir_lower/block_lazy_clear_survivors.rs"), "\n", include_str!("../ir_lower/promoted_pending_mark.rs"), "\n", include_str!("../ir_lower/site_value_live_at.rs"), "\n", include_str!("../ir_lower/plan_register_residency_with.rs"), "\n", include_str!("../ir_lower/inner_attempt.rs"), "\n", include_str!("../ir_lower/g1_aastore_screen.rs"), "\n", include_str!("../ir_lower/scoped_session_confined_rows.rs"));
    let body = |head: &str| -> &str {
        src.split(head)
            .nth(1)
            .and_then(|s| s.split("\n    pub").next())
            .expect(head)
    };
    let call = concat!("0xFF", ", 0xD0");
    let rel = concat!("0x", "E8");
    let checked = body(concat!("fn emit_getfield", "_checked_helper_call("));
    assert_eq!(checked.matches(call).count(), 1);
    assert!(checked.contains("let lazy = self.lazy_ref_hook_enter();"));
    assert!(checked.contains("self.lazy_ref_hook_exit(lazy);"));
    let tails = body(concat!("fn emit_deferred", "_getfield_tails("));
    assert_eq!(tails.matches(call).count(), 1);
    assert!(tails.contains("self.emit_lazy_ref_spill_stores(&t.lazy_spills);"));
    assert!(tails.contains("self.emit_lazy_ref_reloads(&t.lazy_spills);"));
    let arm = src
        .split(concat!("Op::Load(_)", " => {"))
        .nth(1)
        .and_then(|s| s.split(concat!("Op::Store(kind)", " => {")).next())
        .expect("the field-load arm");
    assert_eq!(arm.matches(call).count(), 1, "the plain helper arm's one call");
    assert!(arm.contains("let lazy = self.lazy_ref_hook_enter();"));
    assert!(arm.contains("self.lazy_ref_hook_exit(lazy);"));
    assert!(!arm.contains(rel));
    for head in [
        concat!("fn emit_inline", "_compact_getfield("),
        concat!("fn emit_getfield", "_legacy_read("),
        concat!("fn emit_proven", "_receiver_getfield("),
        concat!("fn emit_compact", "_prim_field_load_to("),
        concat!("fn emit_field", "_instruction("),
    ] {
        let b = body(head);
        assert_eq!(b.matches(call).count(), 0, "{head}: calls only through the hooked helper");
        assert!(!b.contains(rel), "{head}");
    }
    assert!(!checked.contains(rel));
}
