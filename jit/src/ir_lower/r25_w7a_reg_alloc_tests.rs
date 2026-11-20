use super::*;
use crate::ir::IrBuilder;

const MAIN: &str = "CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES";
const SPILL: &str = "CRATONVM_JIT_IR_ELIDE_HOMES_SPILL_AT_INSPECTION";
const LOOP: &str = "CRATONVM_JIT_IR_SITE_NAMED_LOOP_VALUES";

/// `int sum(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }`
const SUM_LOOP: [u8; 23] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01,
    0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
];

/// `int f(int n) { int s = 7; int i = n; while (i > 0) { s = s - i; i -= 3; } return s; }`
const COUNTDOWN_LOOP: [u8; 23] = [
    0x10, 0x07, 0x3c, // 0: bipush 7; istore_1
    0x1a, 0x3d, // 3: iload_0; istore_2
    0x1c, 0x9e, 0x00, 0x0d, // 5: iload_2; ifle +13 -> 19
    0x1b, 0x1c, 0x64, 0x3c, // 9: iload_1; iload_2; isub; istore_1
    0x84, 0x02, 0xfd, // 13: iinc 2, -3
    0xa7, 0xff, 0xf5, // 16: goto -11 -> 5
    0x1b, 0xac, // 19: iload_1; ireturn
    0, 0,
];

/// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) { int t = i * 7;
/// s += 100 / (t % 5 + 1) + t; } return s; }` -- a division pad naming the
/// loop's phis.
const DIV_LOOP: [u8; 37] = [
    0x03, 0x3c, 0x03, 0x3d, // 0: s = 0; i = 0
    0x1c, 0x1a, 0xa2, 0x00, 0x1b, // 4: if (i >= n) goto 33
    0x1c, 0x10, 0x07, 0x68, 0x3e, // 9: t = i * 7
    0x1b, 0x10, 0x64, 0x1d, 0x08, 0x70, 0x04, 0x60, 0x6c, 0x60, // 14: s + 100 / (t % 5 + 1)
    0x1d, 0x60, 0x3c, // 24: ... + t -> s
    0x84, 0x02, 0x01, // 27: i++
    0xa7, 0xff, 0xe6, // 30: goto 4
    0x1b, 0xac, // 33: return s
    0, 0,
];

/// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) { int a = i * 3;
/// int b = i ^ 5; s += a - b; } return s; }` -- `a` is read once, by the
/// subtraction, with `b` scheduled between: out of the carry's window, so
/// a single-use admission of this increment.
const SPLIT_USE_LOOP: [u8; 35] = [
    0x03, 0x3c, 0x03, 0x3d, // 0: s = 0; i = 0
    0x1c, 0x1a, 0xa2, 0x00, 0x19, // 4: if (i >= n) goto 31
    0x1c, 0x06, 0x68, 0x3e, // 9: a = i * 3
    0x1c, 0x08, 0x82, 0x36, 0x04, // 13: b = i ^ 5
    0x1b, 0x1d, 0x15, 0x04, 0x64, 0x60, 0x3c, // 18: s = s + (a - b)
    0x84, 0x02, 0x01, // 25: i++
    0xa7, 0xff, 0xe8, // 28: goto 4
    0x1b, 0xac, // 31: return s
    0, 0,
];

fn sum_ref(n: i32) -> i32 {
    (0..n.max(0)).fold(0i32, |s, i| s.wrapping_add(i))
}

fn countdown_ref(n: i32) -> i32 {
    let (mut s, mut i) = (7i32, n);
    while i > 0 {
        s = s.wrapping_sub(i);
        i = i.wrapping_sub(3);
    }
    s
}

fn div_ref(n: i32) -> i32 {
    (0..n.max(0))
        .map(|i| {
            let t = i.wrapping_mul(7);
            (100 / (t % 5 + 1)).wrapping_add(t)
        })
        .fold(0i32, i32::wrapping_add)
}

fn split_use_ref(n: i32) -> i32 {
    (0..n.max(0))
        .map(|i| i.wrapping_mul(3).wrapping_sub(i ^ 5))
        .fold(0i32, i32::wrapping_add)
}

/// Lower `code` (one `int` parameter, `locals` locals) under `edits`.
fn lower_with(
    code: &[u8],
    len: usize,
    locals: usize,
    edits: &[(&str, Option<&str>)],
) -> CompiledMethod {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let mut builder = IrBuilder::new(1, locals);
        builder.set_param_types(&[IrType::Int]);
        let mut graph = builder.build(code, len).expect("IR build");
        crate::ir_optimize::optimize(&mut graph);
        let schedule = crate::ir_schedule::schedule(&graph);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; with no safepoint flag the body emits no poll and calls no helper.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        lower(&graph, &schedule, 1, locals, &helpers).expect("the loop lowers")
    })
}

/// The single-use admission's population: an `int` read once out of the
/// carry window, with a one-`store_rax` home. Not a constant, not a
/// reference, not a value read twice.
#[test]
fn w7a_single_use_candidates_are_one_store_ints_read_once() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let sum = g.add(Op::Add, IrType::Int, vec![a, one], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], None);
    let schedule = crate::ir_schedule::schedule(&g);
    let cand = |id: NodeId, uses: u32| {
        super::r25_w7a_reg_alloc::single_use_candidate(&g, &schedule, id as usize, uses)
    };
    assert!(!cand(sum, 2), "read twice: the pays rule's own population");
    assert!(!cand(sum, 0), "never read");
    assert!(!cand(one, 1), "a constant is an immediate at every reader");
    assert!(!cand(null, 1), "a reference home is never dropped");
    assert!(!cand(a, 1), "a parameter's home is no `store_rax`");
    // W7-A fix: a value right in front of the terminator that reads it is
    // the tail carry's (item 1a), so no register; anywhere else it is out
    // of every carry's reach and a candidate.
    let tail = schedule.blocks.iter().any(|b| {
        b.nodes.last() == Some(&sum)
            && b.terminator
                .and_then(|t| g.nodes.get(t as usize))
                .is_some_and(|t| t.inputs.contains(&sum))
    });
    assert_eq!(
        super::r25_w7a_reg_alloc::carry_reaches(&g, &schedule, sum as usize),
        tail,
        "the tail carry reaches exactly a value read by the terminator right behind it"
    );
    assert_eq!(
        cand(sum, 1),
        !tail,
        "a one-store int read once, out of every carry"
    );
}

/// W7-A fix 2: the primitive field reads are claimed, a reference, FP or
/// call result is not.
#[test]
fn w7a_primitive_field_reads_write_their_home_once() {
    use super::r25_w7a_reg_alloc::load_writes_its_home_once as once;
    for kind in [
        MemKind::Int,
        MemKind::Long,
        MemKind::Byte,
        MemKind::Char,
        MemKind::Short,
    ] {
        let ty = if kind == MemKind::Long {
            IrType::Long
        } else {
            IrType::Int
        };
        assert!(once(&Op::Load(kind), ty), "{kind:?}");
    }
    assert!(!once(&Op::Load(MemKind::Ref), IrType::Ref));
    assert!(!once(&Op::Load(MemKind::Double), IrType::Double));
    assert!(!once(&Op::Load(MemKind::Float), IrType::Float));
    assert!(
        !once(&Op::Add, IrType::Int),
        "an ALU op is the default list's"
    );
}

/// W7-A fix: the deferred RCX carry's consumer two nodes later reaches its
/// operand, so `(a + 1) ^ (b * 3)` scheduled as `[Mul, Add, Xor]` keeps
/// the `Mul` out of the admission (the fixture of
/// `a_folded_middle_arm_carries_both_operands_and_still_computes_the_answer`).
#[test]
fn w7a_the_deferred_carry_window_is_not_admitted() {
    // iload_0, iconst_1, iadd, iload_1, iconst_3, imul, ixor, ireturn
    let code = [0x1a, 0x04, 0x60, 0x1b, 0x06, 0x68, 0x82, 0xac, 0, 0];
    let mut graph = IrBuilder::new(2, 2).build(&code, 8).expect("IR build");
    crate::ir_optimize::optimize(&mut graph);
    let schedule = crate::ir_schedule::schedule_with_options(
        &graph,
        &crate::ir_schedule::ScheduleOptions {
            pair_single_use_operands: true,
            ..Default::default()
        },
    );
    let mut checked = 0usize;
    for block in &schedule.blocks {
        for (w, &id) in block.nodes.iter().enumerate() {
            let two_later = block
                .nodes
                .get(w + 2)
                .and_then(|&c| graph.nodes.get(c as usize))
                .is_some_and(|c| c.inputs.contains(&id));
            if two_later {
                checked += 1;
                assert!(super::r25_w7a_reg_alloc::carry_reaches(
                    &graph,
                    &schedule,
                    id as usize
                ));
                assert!(!super::r25_w7a_reg_alloc::single_use_candidate(
                    &graph,
                    &schedule,
                    id as usize,
                    1
                ));
            }
        }
    }
    eprintln!("[w7a] deferred-carry shapes checked: {checked}");
}

/// Lower `code` (no parameters, no locals, no optimizer) under `edits`.
fn lower_bare(code: &[u8], len: usize, edits: &[(&str, Option<&str>)]) -> CompiledMethod {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let graph = IrBuilder::new(0, 0).build(code, len).expect("IR build");
        let schedule = crate::ir_schedule::schedule(&graph);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; this body has no poll and calls no helper.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        lower(&graph, &schedule, 0, 0, &helpers).expect("the body lowers")
    })
}

/// The census line names every field and the switch; on a lowerer with
/// no plan nothing is resident, and the coalesced-edge answer is `false`
/// while the allocator is off.
#[test]
fn w7a_census_line_and_the_edge_answer_off() {
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
    let line = lo.reg_alloc_census_line();
    for field in [
        "[cratonvm-jitc] ir-reg-alloc method=",
        " resident=0 ",
        " single_use_resident=0 ",
        " site_named=0 ",
        " phis=0 ",
        " coalesced=0 ",
        " def_stores=",
        " def_stores_dropped=",
        " tail_elided=",
        " need_point_stores=0 ",
        " inspect_spills=0 ",
        " slow_spills=0 ",
        " spills_deduped=0 ",
        " excluded=0 ",
        " phis_int=0 ",
        " phis_resident=0 ",
        " coalesced_src=0 ",
        " single_use_site_named=0 ",
        " single_use_kept= kept= direct_defs=0 kept_phi_stages=0 reg_alloc=off",
        " reg_alloc=off",
    ] {
        assert!(line.contains(field), "{field:?} in {line}");
    }
    assert!(!lo.reg_alloc_coalesced_live_at_edge(sum, 0));
}

/// The new paths are gated on the allocator: the coalesced-source clause of
/// the site-named plan, the edge answer both liveness readers consult, and
/// the residency admission in the plan and the certain-refusal pre-pin.
#[test]
fn w7a_every_new_path_is_gated_on_the_switch() {
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
    let plan = body("fn plan_site_named_homes(&mut self) {");
    assert!(plan.contains(
        "self.phi_coalesced_src.get(id).copied().unwrap_or(false) && !self.reg_alloc_on;"
    ));
    assert!(plan.contains("|| coalesced_refused"));
    let spills = body("fn emit_fallthrough_poll_spills(&mut self, block_idx: usize) {");
    assert!(spills.contains("|| self.reg_alloc_coalesced_live_at_edge(v, block_idx);"));
    let live_at = body("fn site_live_at(&self, v: NodeId, at: NodeId, term: bool) -> bool {");
    assert!(live_at
        .contains("if term && self.reg_alloc_coalesced_live_at_edge(v, self.cur_lower_block) {"));
    let edge = body("fn reg_alloc_coalesced_live_at_edge(");
    assert!(edge.contains("self.reg_alloc_on"));
    let residency = src
        .split("fn plan_register_residency_with(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .expect("the plan is in this file");
    // W9-2: the plan takes the effective switch as an argument.
    assert!(
        residency.contains("    reg_alloc: bool,\n    selected: Option<&MirPlan>,\n) -> CompileResult<Option<RegResidency>> {")
    );
    assert!(residency.contains("let reg_alloc_candidate = reg_alloc"));
    let prepin = src
        .split("fn ls_certain_refusals_with(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .expect("the pre-pin is in this file");
    assert!(prepin.contains("reg_alloc && r25_w7a_reg_alloc::single_use_candidate("));
    // Fix round: the pad-read rule at both liveness readers, and the
    // phi's coalesced-source credit in the spill-cost rule.
    assert!(plan.contains("self.reg_alloc_coalesced_source_freq(v, &candidate)"));
    assert!(live_at.contains("if !term && self.reg_alloc_read_at_site(v, at) {"));
    let blocked = body("fn site_named_blocked(");
    assert!(blocked.contains("&& !self.reg_alloc_read_at_site(v, at)"));
    let read_at = body("        pub(super) fn reg_alloc_read_at_site(");
    assert!(read_at.contains("if !self.reg_alloc_on"));
    // Fix 2: the plan and the census ask the widened producer list.
    assert!(plan.contains("!self.reg_alloc_home_is_one_store(node)"));
    let census = body("fn plan_home_keep_census_with(&mut self, on: bool) {");
    assert!(census.contains("!self.reg_alloc_home_is_one_store(node)"));
    let widened = body("        pub(super) fn reg_alloc_home_is_one_store(");
    assert!(widened
        .contains("|| (self.reg_alloc_on\n                    && (load_writes_its_home_once("));
    let credit = body("        pub(super) fn reg_alloc_coalesced_source_freq(");
    assert!(credit.contains("if !self.reg_alloc_on"));
    // Increment 6: the kept-home staging asks the allocator first.
    let stages = body("        pub(super) fn reg_alloc_stages_kept_phi(");
    assert!(stages.contains("if !self.reg_alloc_on"));
    // Fix 5: the plan leaves out what a fall-through poll names dead, and
    // the prediction is empty with the allocator off.
    assert!(plan.contains("let fallthrough_dead = self.reg_alloc_fallthrough_poll_dead();"));
    assert!(plan.contains("|| fallthrough_dead.get(id).copied().unwrap_or(false)"));
    let dead = body("        pub(super) fn reg_alloc_fallthrough_poll_dead(");
    assert!(dead.contains("if !self.reg_alloc_on {\n                return dead;"));
}

// W7 increment 6: with every phi home kept (`CRATONVM_JIT_IR_DROP_PHI_HOME
// =0`, so the home-dropped staging cannot take them) the allocator stages
// the loops' phi copies in the phi's register and writes the home from
// there. Engagement: the same arm with phi-copy staging forced off
// (`CRATONVM_JIT_IR_PHI_COPY_DIRECT=0`) is longer, the RAX round trip back in every
// staged copy. Every arm answers every trip count as the Java reference
// does. The answers cannot fail a wrong home store (nothing here reads a
// resident phi's home back); `w7_inc6_a_kept_home_copy_stores_the_staged_register`
// pins the store's bytes, and `R25W7PhiHomeDeopt.java` reads
// the homes through a real deopt.
