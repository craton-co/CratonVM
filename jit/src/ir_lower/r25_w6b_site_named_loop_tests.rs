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
/// s += 100 / (t % 5 + 1) + t; } return s; }` -- a division that can trap
/// (a pad site naming the loop's phis) inside the loop.
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

/// The switch is default ON since j27 2026-10-28 (`=0` kills). It is a
/// sub-switch: the predicate reads only its own key, and the lowering ANDs
/// it with the other two
/// (`site_named_loop_on`).
#[test]
fn w6b_site_named_loop_values_is_default_on_with_a_zero_kill() {
    // Default ON since j27 2026-10-28 (WARNING j27-user-flip-20261028).
    cratonvm_types::flags::with_thread_overrides(&[(LOOP, None)], || {
        assert!(super::ir_site_named_loop_values_enabled())
    });
    cratonvm_types::flags::with_thread_overrides(&[(LOOP, Some("1"))], || {
        assert!(super::ir_site_named_loop_values_enabled())
    });
}

/// Every arm of the three switches -- all off, the two item-1a switches
/// alone, and all three on -- answers every trip count of each loop as the
/// Java reference does. The loop switch without the other two lowers byte
/// for byte as all-off: it is gated on them.
/// The census marks a value blocked and keeps the FIRST reason it was
/// given; with no reason vector it only marks.
#[test]
fn w6b_mark_site_blocked_keeps_the_first_reason() {
    let mut out = vec![false; 4];
    let mut why = vec![super::HK_NONE; 4];
    {
        let mut w = Some(&mut why);
        super::mark_site_blocked(&mut out, &mut w, 2, super::HK_HEADER);
        super::mark_site_blocked(&mut out, &mut w, 2, super::HK_INSPECT);
        super::mark_site_blocked(&mut out, &mut w, 9, super::HK_RECIPE);
    }
    assert_eq!(out, vec![false, false, true, false]);
    assert_eq!(why[2], super::HK_HEADER, "the first reason stays");
    assert_eq!(why[0], super::HK_NONE);
    let mut none: Option<&mut Vec<u8>> = None;
    super::mark_site_blocked(&mut out, &mut none, 1, super::HK_ENTRY);
    assert!(out[1]);
    assert_eq!(super::HK_NAMES.len(), super::HK_N);
    assert!(usize::from(super::HK_OTHER) < super::HK_N);
}

/// The census classifies by type and residency: with no residency plan an
/// `int` is `no_reg` and a reference `ref`; a counted store lands under its
/// reason, and nothing is counted with the census off.
#[test]
fn w6b_the_census_classifies_and_counts() {
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
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 16384);

    lo.plan_home_keep_census_with(false);
    assert!(lo.home_keep_why.is_empty(), "no census without the switch");
    lo.note_home_keep(sum);
    assert_eq!(lo.home_why_static.iter().sum::<usize>(), 0);

    lo.plan_home_keep_census_with(true);
    assert_eq!(lo.home_keep_why.len(), graph.nodes.len());
    assert_eq!(lo.home_keep_why[sum as usize], super::HK_NO_REG);
    assert_eq!(lo.home_keep_why[null as usize], super::HK_REF);
    lo.cur_lower_block = 0;
    lo.note_home_keep(sum);
    lo.note_home_keep(null);
    assert_eq!(lo.home_why_static[usize::from(super::HK_NO_REG)], 1);
    assert_eq!(lo.home_why_static[usize::from(super::HK_REF)], 1);
    let fields = lo.home_why_fields();
    assert!(fields.contains("no_reg=1/"), "{fields}");
    assert!(fields.contains("ref=1/"), "{fields}");
}

/// The body of the function whose signature line is `head`.
fn fn_body<'s>(src: &'s str, head: &str) -> &'s str {
    src.split(head)
        .nth(1)
        .unwrap_or_else(|| panic!("`{head}` is in this file"))
        .split("\n    fn ")
        .next()
        .expect("the function ends")
}
