use super::*;
use crate::ir::IrBuilder;

/// `CMP ECX, -1; JNE rel8` (the inline `-1` fixup's skip) and `CMP ECX,
/// -1; JE rel32` (the out-of-line one's entry).
const M1_JNE8: [u8; 4] = [0x83, 0xF9, 0xFF, 0x75];
const M1_JE32: [u8; 5] = [0x83, 0xF9, 0xFF, 0x0F, 0x84];
/// `TEST ECX, ECX; JNZ rel32` (the inline zero skip) and `TEST ECX, ECX;
/// JE rel32` (the out-of-line one).
const Z_JNZ32: [u8; 4] = [0x85, 0xC9, 0x0F, 0x85];
const Z_JE32: [u8; 4] = [0x85, 0xC9, 0x0F, 0x84];

fn lower_ints(
    code: &[u8],
    code_len: usize,
    params: usize,
    edits: &[(&str, Option<&str>)],
) -> CompiledMethod {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let mut builder = IrBuilder::new(params, params);
        builder.set_param_types(&vec![IrType::Int; params]);
        let graph = builder.build(code, code_len).expect("IR build");
        let schedule = crate::ir_schedule::schedule(&graph);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; no executed path calls a helper.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        lower_inner(
            &graph,
            &schedule,
            params,
            params,
            &helpers,
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("the division lowers")
    })
}

fn run(cm: &CompiledMethod, args: &[i64]) -> i32 {
    // SAFETY: an `int f(int..)` body lowered above; the arguments keep
    // every executed division off its deopting guard.
    let got = unsafe { cm.try_call_with_context(0, args) }.expect("call");
    // Cast: this tier's `int` result is the low 32 bits of RAX.
    got as i32
}

fn count(code: &[u8], needle: &[u8]) -> usize {
    code.windows(needle.len()).filter(|w| *w == needle).count()
}

/// CG16-4: the outlined poll's map is emitted after the body, so the
/// site's node pc (the key of the safepoint's inline-frame row) is
/// reinstated around it with the bci and defined set, and restored after.
#[test]
fn r16_an_outlined_poll_maps_with_its_own_node_pc() {
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
    let body = src
        .split("fn emit_outlined_polls(&mut self)")
        .nth(1)
        .expect("emit_outlined_polls is in this file")
        .split("\n    pub")
        .next()
        .expect("the function ends");
    let set = body
        .find("std::mem::replace(&mut self.cur_node_pc, pc_at_poll)")
        .expect("the site's pc is reinstated");
    let map = body
        .find("emit_poll_safepoint_map(back_edge_block)")
        .expect("the map is emitted here");
    let restore = body
        .find("self.cur_node_pc = pc;")
        .expect("and the end-of-body pc restored");
    assert!(set < map && map < restore, "reinstate, map, restore");
}

// ── Round 16 wave 3 (CG16-6, CG16-7): the back edge's frame traffic ──

/// `int sum(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }`
/// -- the accumulator is read after the loop, so its single-range interval
/// covers the whole latch and the scan alone can never share its register.
const SUM_LOOP: [u8; 23] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01,
    0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
];

/// `int f(int n) { int s = 7; int i = n; while (i > 0) { s = s - i; i -= 3; } return s; }`
/// -- a `Sub` source and a constant initial value other than zero.
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

fn sum_ref(n: i64) -> i32 {
    (0..n.max(0)).fold(0i32, |s, i| s.wrapping_add(i as i32))
}

fn countdown_ref(n: i64) -> i32 {
    // Cast: the fixture's `int` parameter is the low 32 bits.
    let (mut s, mut i) = (7i32, n as i32);
    while i > 0 {
        s = s.wrapping_sub(i);
        i = i.wrapping_sub(3);
    }
    s
}

const SINK: (&str, Option<&str>) = ("CRATONVM_JIT_IR_SINK_EQUAL_DEPTH", Some("1"));
const COALESCE: (&str, Option<&str>) = ("CRATONVM_JIT_IR_PHI_COALESCE_BACK_EDGE", Some("1"));

fn loop_graph(code: &[u8], code_len: usize) -> Graph {
    let mut builder = IrBuilder::new(1, 3);
    builder.set_param_types(&[IrType::Int]);
    let mut graph = builder.build(code, code_len).expect("IR build");
    crate::ir_optimize::optimize(&mut graph);
    graph
}

fn lower_loop(code: &[u8], code_len: usize, edits: &[(&str, Option<&str>)]) -> CompiledMethod {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let graph = loop_graph(code, code_len);
        let schedule = crate::ir_schedule::schedule(&graph);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; with no safepoint flag the body emits no poll and calls no helper.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        lower_inner(
            &graph,
            &schedule,
            1,
            3,
            &helpers,
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("the loop lowers")
    })
}

/// [`lower_loop`] for a graph the production pipeline marked
/// `loop_carried_regs` (JIT round 23 wave 3b, lane L1).
fn lower_loop_carried(
    code: &[u8],
    code_len: usize,
    edits: &[(&str, Option<&str>)],
) -> CompiledMethod {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let mut graph = loop_graph(code, code_len);
        graph.loop_carried_regs = true;
        let schedule = crate::ir_schedule::schedule(&graph);
        // SAFETY: as in `lower_loop`.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        lower_inner(
            &graph,
            &schedule,
            1,
            3,
            &helpers,
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("the loop lowers")
    })
}

/// JIT round 23 wave 3b (lane L1, L1W3-1, `CRATONVM_JIT_IR_LOOP_CARRIED_REGS`):
/// a graph marked `loop_carried_regs` lowers byte for byte as the explicit
/// round-16 pair (integer equal-depth sinking plus back-edge coalescing)
/// does; explicit `0`s of both underlying switches give back the unmarked
/// code; every arm answers every trip count as the Java reference does.

/// `int f(int n) { float s = 0; for (int i = 0; i < n; i++) s = s / 2f + i; return (int) s; }`
/// -- an FP recurrence with two steps (the `fdiv` single-use, read by the
/// `fadd` that feeds the phi).
const FP_RECURRENCE_LOOP: [u8; 25] = [
    0x0b, 0x44, // 0: fconst_0; fstore_1
    0x03, 0x3d, // 2: iconst_0; istore_2
    0x1c, 0x1a, 0xa2, 0x00, 0x10, // 4: iload_2; iload_0; if_icmpge +16 -> 22
    0x23, 0x0d, 0x6e, // 9: fload_1; fconst_2; fdiv
    0x1c, 0x86, 0x62, 0x44, // 12: iload_2; i2f; fadd; fstore_1
    0x84, 0x02, 0x01, // 16: iinc 2, 1
    0xa7, 0xff, 0xf1, // 19: goto -15 -> 4
    0x23, 0x8b, 0xac, // 22: fload_1; f2i; ireturn
];

fn fp_recurrence_ref(n: i64) -> i32 {
    let mut s = 0f32;
    // Cast: the fixture's `int` parameter is the low 32 bits.
    for i in 0..(n as i32).max(0) {
        s = s / 2.0 + i as f32;
    }
    // `f2i`: saturating, NaN to 0 -- Rust's `as` is the same conversion.
    s as i32
}

// ── Round 16 wave 4 (CG16-8): the prologue's zero runs through RAX ──

/// A `Lowerer` over the `sum` loop, for emitter-level tests.
pub(super) fn emitter_lowerer() -> Lowerer<'static> {
    let graph: &'static Graph = Box::leak(Box::new(loop_graph(&SUM_LOOP, 21)));
    let schedule: &'static Schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let empty: &'static HashMap<usize, bool> = Box::leak(Box::new(HashMap::new()));
    let no_direct: &'static HashMap<usize, (usize, bool)> = Box::leak(Box::new(HashMap::new()));
    let no_ic: &'static HashMap<usize, (usize, usize)> = Box::leak(Box::new(HashMap::new()));
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let no_frame_sites: &'static crate::ir::IrInlineFrameSites =
        Box::leak(Box::new(crate::ir::IrInlineFrameSites::default()));
    // SAFETY: as in `lower_ints`.
    let zeroed: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let helpers: &'static JitRuntimeHelpers = Box::leak(Box::new(zeroed));
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    Lowerer::new(
        graph,
        schedule,
        buf,
        1,
        3,
        plan,
        helpers,
        empty,
        &[],
        None,
        no_direct,
        no_ic,
        &no_compact,
        no_frame_sites,
    )
}

#[test]
fn r16_a_prologue_zero_run_is_one_xor_and_rax_stores() {
    let mut lo = emitter_lowerer();
    // Off (the default): the 11-byte immediate form, unchanged.
    let at = lo.buf.pos();
    lo.emit_zero_frame_slot(8);
    assert_eq!(
        &lo.buf.as_slice()[at..],
        &[0x48, 0xC7, 0x85, 0xF8, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]
    );
    // On: one `XOR EAX, EAX`, then a `MOV [rbp - off], RAX` per word.
    lo.prologue_zero_rax = true;
    let at = lo.buf.pos();
    lo.emit_zero_frame_slot(8);
    lo.emit_zero_frame_slot(16);
    lo.emit_zero_frame_slot(0x200);
    assert_eq!(
        &lo.buf.as_slice()[at..],
        &[
            0x31, 0xC0, // XOR EAX, EAX
            0x48, 0x89, 0x45, 0xF8, // MOV [rbp - 8], RAX
            0x48, 0x89, 0x45, 0xF0, // MOV [rbp - 16], RAX
            0x48, 0x89, 0x85, 0x00, 0xFE, 0xFF, 0xFF, // MOV [rbp - 0x200], RAX
        ]
    );
    // Anything emitted in between ends the run: RAX is re-zeroed.
    lo.buf.emit_byte(0x90);
    let at = lo.buf.pos();
    lo.emit_zero_frame_slot(24);
    assert_eq!(
        &lo.buf.as_slice()[at..],
        &[0x31, 0xC0, 0x48, 0x89, 0x45, 0xE8]
    );
}
