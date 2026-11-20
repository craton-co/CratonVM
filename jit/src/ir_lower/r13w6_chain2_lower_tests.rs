use super::*;
use crate::ir::{IrBuilder, IrInlineSite};

fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test below reaches no helper.
    unsafe { std::mem::zeroed() }
}

/// `r13w3_framestate_lower_tests`' fixture: caller `aload_0; invokestatic
/// A; return`, `A` at 5 `aload_0; invokespecial B; return`, `B` at 10
/// `return`. `B`'s splice plants a receiver guard at combined pc 6.
fn nested_graph() -> Graph {
    let combined: Vec<u8> = vec![
        0x2a, 0xb8, 0x00, 0x01, 0xb1, // caller
        0x2a, 0xb7, 0x00, 0x02, 0xb1, // A, base 5
        0xb1, // B, base 10
    ];
    let site = |base: usize, code_len: usize, receiver: bool, key: &str| IrInlineSite {
        base,
        code_len,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: false,
        receiver_is_arg0: receiver,
        method_key: key.to_string(),
        class_id: 0,
    };
    let sites = HashMap::from([
        (1usize, site(5, 5, false, "T.a:(LP;)V")),
        (6usize, site(10, 1, true, "P.b:()V")),
    ]);
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_IR_SPLICE_FRAME_STATES", Some("1"))],
        move || {
            let mut b = IrBuilder::new(1, 1);
            b.set_param_types(&[IrType::Ref]);
            b.set_inline_sites(sites);
            b.graph.scope_method_key = "T.outer:(LP;)V".to_string();
            b.build(&combined, 5).expect("the nested splice builds")
        },
    )
}

#[test]
fn a_chain_snapshot_finds_the_guard_that_publishes_it_at_its_raw_pc() {
    let graph = nested_graph();
    let chain_at = |pc: usize| {
        graph
            .safepoints
            .iter()
            .position(|s| s.bci == pc && s.is_splice_state())
            .expect("a chain snapshot at that pc")
    };
    let (si_guard, si_entry) = (chain_at(6), chain_at(10));
    let guard = graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Guard { bci: 6 }))
        .expect("the receiver guard at A's invokespecial") as NodeId;
    let graph: &'static Graph = Box::leak(Box::new(graph));
    let schedule: &'static Schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let empty: &'static HashMap<usize, bool> = Box::leak(Box::new(HashMap::new()));
    let no_direct: &'static HashMap<usize, (usize, bool)> = Box::leak(Box::new(HashMap::new()));
    let no_ic: &'static HashMap<usize, (usize, usize)> = Box::leak(Box::new(HashMap::new()));
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let no_frame_sites: &'static crate::ir::IrInlineFrameSites =
        Box::leak(Box::new(crate::ir::IrInlineFrameSites::default()));
    let helpers: &'static JitRuntimeHelpers = Box::leak(Box::new(no_helpers()));
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let lo = Lowerer::new(
        graph,
        schedule,
        buf,
        1,
        1,
        plan,
        helpers,
        empty,
        &[],
        None,
        no_direct,
        no_ic,
        &no_compact,
        no_frame_sites,
    );
    let (block, span) = lo.chain_deopt_block_and_span(si_guard, 6);
    assert!(block.is_some(), "the guard's block");
    assert!(
        span.is_some_and(|(first, last)| first <= guard && guard <= last),
        "the guard is a site of its own chain snapshot: {span:?}"
    );
    // The flat frames' lookup never answers for a chain snapshot.
    assert_eq!(lo.deopt_block_for_snapshot(si_guard, 6), None);
    // Nothing deopts at `B`'s entry: its chain snapshot has no site, and
    // an object it named would be refused there (as for a flat frame).
    assert_eq!(lo.chain_deopt_block_and_span(si_entry, 10), (None, None));
}
