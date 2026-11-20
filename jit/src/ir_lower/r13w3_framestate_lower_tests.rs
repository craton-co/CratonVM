use super::*;
use crate::ir::{IrBuilder, IrInlineSite};

const OUTER: &str = "T.outer:(LP;)V";
const A_KEY: &str = "T.a:(LP;)V";
const B_KEY: &str = "P.b:()V";

fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the tests below reach no helper.
    unsafe { std::mem::zeroed() }
}

/// Caller `aload_0; invokestatic A(LP;)V; return`; `A` at 5:
/// `aload_0; invokespecial B()V; return`; `B` at 10: `return`, an
/// instance method whose receiver (the caller's parameter) is not proven
/// non-null, so its splice plants a receiver guard at `A`'s bci 1
/// (combined pc 6). The same fixture as `ir`'s `r13w3_framestate_tests`.
fn nested_graph(switch: &str) -> Graph {
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
        (1usize, site(5, 5, false, A_KEY)),
        (6usize, site(10, 1, true, B_KEY)),
    ]);
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_IR_SPLICE_FRAME_STATES", Some(switch))],
        move || {
            let mut b = IrBuilder::new(1, 1);
            b.set_param_types(&[IrType::Ref]);
            b.set_inline_sites(sites);
            b.graph.scope_method_key = OUTER.to_string();
            b.build(&combined, 5).expect("the nested splice builds")
        },
    )
}

/// The receiver guard's point: `A` at its own bci 1 (the `invokespecial`
/// it re-executes, which then throws the NPE from `A`'s frame), with the
/// receiver still on `A`'s stack, over the compiling method parked at ITS
/// `invoke` (bci 1) with the argument already consumed. The point's `bci`
/// is the innermost scope's, as the install verifier requires.
#[test]
fn a_guard_in_a_nested_splice_publishes_the_callee_frame_over_its_caller() {
    let graph = nested_graph("1");
    let schedule = crate::ir_schedule::schedule(&graph);
    let cm = lower(&graph, &schedule, 1, 1, &no_helpers()).expect("lowers");
    let chains: Vec<&DeoptimizationPoint> = cm
        ._deopt_point_boxes
        .iter()
        .map(|p| &**p)
        .filter(|p| p.frame_state.caller.is_some())
        .collect();
    assert_eq!(chains.len(), 1, "the one guard, with a caller chain");
    let p = chains[0];
    assert_eq!(p.bci, 1);
    assert_eq!(p.frame_state.bci, 1);
    assert_eq!(p.frame_state.method_key, A_KEY);
    assert_eq!(p.frame_state.locals.len(), 1);
    assert_eq!(
        p.frame_state.stack.len(),
        1,
        "the receiver the invoke consumes"
    );
    assert!(p.frame_state.monitors.is_empty());
    assert!(p.semantics.reexecute);
    let caller = p.frame_state.caller.as_deref().expect("a caller scope");
    assert_eq!(caller.method_key, OUTER);
    assert_eq!(caller.bci, 1);
    assert_eq!(caller.locals.len(), 1);
    assert!(caller.stack.is_empty(), "the argument is already popped");
    assert!(caller.caller.is_none());
    assert_eq!(crate::deopt::inlined_chain_refusal(&p.frame_state), None);
    // No table point is built from a chain snapshot: every one names this
    // method's own bytecode.
    assert!(cm
        .deopt_points
        .iter()
        .all(|d| d.frame_state.caller.is_none() && d.bci < 5));
    // Round nio20 wave 2, lane L3 (proposal L1-2): unlisted, the chain
    // guard keeps the generic stamp.
    assert_eq!(p.reason, DeoptReason::UncommonTrap);
}

/// Round nio20 wave 2, lane L3 (proposal L1-2 of
/// `jit-r20-nio-proposals-L1.md`): a chain guard whose combined pc the
/// builder listed in `Graph::class_loading_trap_pcs` (the precise exits for
/// an unloaded class, `IrBuilder::plant_unloaded_typecheck_exit` and the
/// unloaded-static-call exit) carries `DeoptReason::ClassLoading`, which
/// `DeoptHistory::recommend_action` maps to a recompile on the first trap.
/// The listing, not the guard's shape, decides; here it is pinned on the
/// fixture's one chain guard at combined pc 6.
/// JIT round 23 wave 5 (`CRATONVM_JIT_IR_DEOPT_NULL_CONST_AS_OBJECT`): a
/// deopt point whose frame holds the `aconst_null` constant in a local
/// describes it as the null REFERENCE `Object(0)`, not as `Int(0)` (which
/// the resume rebuilt as an `int` in a reference slot).
///
/// `static Object m() { Object x = null; indy(); return x; }`:
/// `aconst_null; astore_0; invokedynamic (trapped); pop; aload_0; areturn`.
#[test]
fn r23w5_a_null_constant_local_deopts_as_a_null_reference() {
    if !crate::ir::ir_site_trap_enabled() || !deopt_null_const_as_object() {
        return;
    }
    let code = [
        0x01, 0x4b, 0xba, 0x00, 0x01, 0x00, 0x00, 0x57, 0x2a, 0xb0, 0, 0,
    ];
    let mut builder = IrBuilder::new(0, 1);
    builder.set_indy_trap_sites(std::collections::HashMap::from([(2usize, (0usize, b'L'))]));
    let Some(graph) = builder.build(&code, 10) else {
        return; // the trap was not admitted in this configuration
    };
    assert_eq!(graph.site_trap_pcs, vec![2], "the indy trap");
    let schedule = crate::ir_schedule::schedule(&graph);
    let cm = lower(&graph, &schedule, 0, 1, &no_helpers()).expect("lowers");
    let at_trap: Vec<&DeoptimizationPoint> = cm
        ._deopt_point_boxes
        .iter()
        .map(|p| &**p)
        .filter(|p| p.frame_state.bci == 2)
        .collect();
    assert!(!at_trap.is_empty(), "a deopt point at the trap");
    for point in at_trap {
        assert_eq!(
            point.frame_state.locals.first(),
            Some(&FrameValue::Object(0)),
            "local 0 is the null reference"
        );
    }
}

#[test]
fn a_listed_precise_splice_exit_asks_for_a_recompile() {
    let mut graph = nested_graph("1");
    graph.class_loading_trap_pcs.push(6);
    let schedule = crate::ir_schedule::schedule(&graph);
    let cm = lower(&graph, &schedule, 1, 1, &no_helpers()).expect("lowers");
    let chain: Vec<&DeoptimizationPoint> = cm
        ._deopt_point_boxes
        .iter()
        .map(|p| &**p)
        .filter(|p| p.frame_state.caller.is_some())
        .collect();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].reason, DeoptReason::ClassLoading);
    assert!(chain[0].semantics.reexecute, "the exit re-runs the site");
    let log = crate::deopt::DeoptimizationLog::new();
    assert_eq!(
        log.recommend_action("T.outer:(LP;)V", DeoptReason::ClassLoading),
        DeoptAction::RecompileAndReinterpret,
        "the first trap recompiles"
    );
}

/// The chain snapshot a guard would read is the only thing the lowerer
/// asks: resolved directly, the snapshot at `B`'s entry is a three-scope
/// chain `B@0 -> A@1 -> outer@1`.
#[test]
fn the_lowerer_resolves_a_three_scope_chain_innermost_first() {
    let graph = nested_graph("1");
    let si = graph
        .safepoints
        .iter()
        .position(|s| s.bci == 10 && s.is_splice_state())
        .expect("a chain snapshot at B's entry");
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
    assert!(
        lo.safepoint_first_by_bci.get(&10).is_none(),
        "a chain snapshot is invisible to the by-bci index"
    );
    assert_eq!(lo.splice_state_first_by_pc.get(&10), Some(&si));
    let fs = lo
        .splice_chain_frame_state(si)
        .expect("a publishable chain");
    let keys: Vec<String> = {
        let mut out = Vec::new();
        let mut scope = Some(&fs);
        while let Some(s) = scope {
            out.push(s.method_key.clone());
            scope = s.caller.as_deref();
        }
        out
    };
    assert_eq!(
        keys,
        vec![B_KEY.to_string(), A_KEY.to_string(), OUTER.to_string()]
    );
    assert_eq!(fs.bci, 0);
    assert_eq!(fs.caller.as_deref().map(|c| c.bci), Some(1));
    assert_eq!(crate::deopt::outermost_scope(&fs).bci, 1);
}
