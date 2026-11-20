use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;

/// `static int f(int n) { int s = 0; for (int i = 0; i < n; i++)
/// for (int j = 0; j < n; j++) s += j; return s; }` -- outer header at
/// bci 4, inner header at bci 11, loop exit at bci 32.
const NESTED: [u8; 34] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x1a, 0x03, 0x3e, 0x1d, 0x1a, 0xa2,
    0x00, 0x0d, 0x1b, 0x1d, 0x60, 0x3c, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf4, 0x84, 0x02,
    0x01, 0xa7, 0xff, 0xe7, 0x1b, 0xac,
];

fn nested() -> (Graph, Schedule) {
    let graph = IrBuilder::new(1, 4)
        .build(&NESTED, NESTED.len())
        .expect("the nested loop builds");
    let schedule = ir_schedule::schedule(&graph);
    (graph, schedule)
}

fn bci_of<'g>(graph: &'g Graph, schedule: &'g Schedule) -> impl Fn(usize) -> Option<usize> + 'g {
    move |b| {
        let ctrl = schedule.blocks.get(b)?.ctrl;
        graph.nodes.get(ctrl as usize)?.bytecode_pc
    }
}

fn header_block(graph: &Graph, schedule: &Schedule, bci: usize) -> usize {
    (0..schedule.blocks.len())
        .find(|&b| {
            graph
                .nodes
                .get(schedule.blocks[b].ctrl as usize)
                .is_some_and(|n| matches!(n.op, Op::Merge | Op::Region) && n.bytecode_pc == Some(bci))
        })
        .unwrap_or_else(|| panic!("a header block at bci {bci}"))
}

/// A block strictly dominating `hb` (where a pre-header guard sits).
fn dominator_of(schedule: &Schedule, hb: usize) -> usize {
    (0..schedule.blocks.len())
        .find(|&d| d != hb && schedule.dom.is_reachable(d) && schedule.dom.dominates(d, hb))
        .expect("the header has a dominator")
}

#[test]
fn r25w8_an_inner_header_entry_is_refused_past_an_outer_pre_header_guard() {
    let (graph, schedule) = nested();
    let outer = header_block(&graph, &schedule, 4);
    let inner = header_block(&graph, &schedule, 11);
    let pre = dominator_of(&schedule, outer);
    let bci = bci_of(&graph, &schedule);
    let refused = osr_entries_inside_a_predicated_loop(&schedule, &[(4, pre)], &bci);
    assert!(refused.contains(&outer), "the guarded header itself");
    assert!(
        refused.contains(&inner),
        "the inner header lies inside the guarded loop: its entry skips the guard"
    );
    // Blocks after the loop are dominated by the header but outside it.
    for b in 0..schedule.blocks.len() {
        if bci(b) == Some(32) {
            assert!(!refused.contains(&b), "the loop exit (bci 32) runs nothing the guard licensed");
        }
    }
    // Control: a guard of the INNER loop refuses nothing of the outer one.
    let inner_pre = dominator_of(&schedule, inner);
    let only_inner =
        osr_entries_inside_a_predicated_loop(&schedule, &[(11, inner_pre)], &bci);
    assert!(only_inner.contains(&inner));
    assert!(!only_inner.contains(&outer));
}


fn split_use_ref(n: i32) -> i32 {
    (0..n.max(0))
        .map(|i| i.wrapping_mul(3).wrapping_sub(i ^ 5))
        .fold(0i32, i32::wrapping_add)
}

fn bits_ref(n: i32) -> i32 {
    (0..n.max(0)).fold(0i32, |s, i| {
        // Cast: Java's `>>>` on an int.
        let u = ((i as u32) >> 3) as i32;
        (s.wrapping_add((i << 2) & 60)
            .wrapping_add(i >> 1)
            .wrapping_add(u))
            | 1
    })
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






