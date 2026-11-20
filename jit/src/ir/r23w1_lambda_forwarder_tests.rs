use super::*;

const PROXY: u32 = crate::ir_lower::GETCLASS_WORDS_LAMBDA_BASE_ID + 7;
const IMPL_KEY: &str = "H.lambda$m$0:(Ljava/lang/Integer;)Ljava/lang/Integer;";
const FWD_KEY: &str = "H$$Lambda/0x1.apply:(Ljava/lang/Object;)Ljava/lang/Object;";

fn frame(
    base: usize,
    end: usize,
    invoke_pc: usize,
    key: &str,
    frameless: bool,
) -> SpliceFrame {
    SpliceFrame {
        class_id: 0,
        return_pc: invoke_pc + 3,
        end,
        saved_locals: Vec::new(),
        saved_stack: Vec::new(),
        returns_value: true,
        return_narrow: None,
        multi_return: false,
        bimorphic_reopen: None,
        exits: Vec::new(),
        exit_bci: invoke_pc,
        ranges: Vec::new(),
        next_range: 1,
        range_end: end,
        ctor_receiver: None,
        receiver: NO_NODE,
        arg0: NO_NODE,
        param_nodes: Vec::new(),
        receiver_exact_class: 0,
        invoke_pc,
        base,
        method_key: key.to_string(),
        code_len: end - base,
        max_locals: 2,
        sync_monitor: None,
        miss_call: None,
        frameless,
        opaque: false,
        reexec_args: Vec::new(),
        caller_mirror_slot: 0,
        security_walk_ignored: false,
    }
}

#[test]
fn only_the_lambda_proxy_id_range_is_a_forwarder() {
    assert!(!ir_splice_body_is_lambda_forwarder(0));
    assert!(!ir_splice_body_is_lambda_forwarder(42));
    assert!(!ir_splice_body_is_lambda_forwarder(
        crate::ir_lower::GETCLASS_WORDS_LAMBDA_BASE_ID - 1
    ));
    assert!(ir_splice_body_is_lambda_forwarder(
        crate::ir_lower::GETCLASS_WORDS_LAMBDA_BASE_ID
    ));
    assert!(ir_splice_body_is_lambda_forwarder(PROXY));
    assert!(!ir_splice_body_is_lambda_forwarder(u32::MAX), "a sentinel");
}

/// The compiling method calls the forwarder at pc 1 (body [100, 110)), the
/// forwarder calls the implementation at its bci 6 (body [200, 210)).
fn forwarder_sites() -> IrInlineFrameSites {
    let site = |base: usize, key: &str, class_id: u32| IrInlineSite {
        base,
        code_len: 10,
        num_args: 2,
        max_locals: 2,
        arg_local_slots: vec![0, 1],
        returns_value: true,
        receiver_is_arg0: true,
        method_key: key.to_string(),
        class_id,
    };
    IrInlineFrameSites::from_sites(&HashMap::from([
        (1usize, site(100, FWD_KEY, PROXY)),
        (106usize, site(200, IMPL_KEY, 9)),
    ]))
}

/// A trace through the splice names the implementation over the caller,
/// as the interpreter's lambda dispatch (no proxy frame) does.
#[test]
fn the_forwarder_contributes_no_stack_trace_level() {
    let sites = forwarder_sites();
    assert!(
        sites.chain_at(103).is_empty(),
        "inside the forwarder: the caller's own frame"
    );
    let chain = sites.chain_at(204);
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].method_key, IMPL_KEY);
    assert_eq!(chain[0].bci, 4);
    assert_eq!(chain[0].class_id, 9);
    assert_eq!(sites.enclosing_bci_at(204), Some(1));
    assert_eq!(sites.enclosing_bci_at(103), Some(1));
}

/// The chain-snapshot prune's body walk agrees with the builder's chain:
/// the implementation is parked at the forwarder's own caller pc.
#[test]
fn the_prune_walk_reparents_the_implementation_to_the_invoke() {
    let sites = forwarder_sites();
    let bodies = sites.bodies_enclosing(204);
    assert_eq!(bodies, vec![(200, 10, 1, IMPL_KEY)]);
    assert!(sites.bodies_enclosing(103).is_empty());
    assert!(sites.pc_in_lambda_forwarder(103));
    assert!(!sites.pc_in_lambda_forwarder(204));
    assert!(!sites.pc_in_lambda_forwarder(1));
}

#[test]
fn a_chain_skips_the_forwarder_scope() {
    let mut b = IrBuilder::new(1, 1);
    let (param, recv, arg, top) = (3, 4, 5, 6);
    let mut fwd = frame(100, 110, 1, FWD_KEY, true);
    // The compiling method at its `invoke`: local 0, the call's operands popped.
    fwd.saved_locals = vec![param];
    let mut imp = frame(200, 210, 106, IMPL_KEY, false);
    // The forwarder at its call: its receiver and argument, an empty stack.
    imp.saved_locals = vec![recv, arg];
    b.splice.push(fwd);
    b.splice.push(imp);
    b.stack = vec![top];
    let (stack, chain) = b.splice_scope_chain(204).expect("a chain through the forwarder");
    let keys: Vec<&str> = chain.scopes.iter().map(|s| s.method_key.as_str()).collect();
    assert_eq!(keys, vec![IMPL_KEY, ""], "the implementation over the compiling method");
    let bcis: Vec<u32> = chain.scopes.iter().map(|s| s.bci).collect();
    assert_eq!(bcis, vec![4, 1], "the compiling method parked in the forwarder's invoke");
    assert_eq!(stack, vec![top, param], "no forwarder locals in the layout");
    assert_eq!(chain.scopes[1].locals_len, 1);
    assert_eq!(chain.scopes[1].stack_len, 0);
    // A forwarder holding values under its call cannot be skipped.
    b.splice[1].saved_stack = vec![recv];
    assert!(b.splice_scope_chain(204).is_none());
}

/// Stage 2 (wave 2): a forwarder splicing a second call (its boxing) is
/// opaque: no chain through it, and every pc of its subtree may resume
/// flat; a single-call forwarder's implementation may not.
#[test]
fn an_opaque_forwarder_blocks_chains_and_resumes_its_subtree_flat() {
    let site = |base: usize, key: &str, class_id: u32| IrInlineSite {
        base,
        code_len: 10,
        num_args: 1,
        max_locals: 2,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: key.to_string(),
        class_id,
    };
    let opaque = IrInlineFrameSites::from_sites(&HashMap::from([
        (1usize, site(100, FWD_KEY, PROXY)),
        (103usize, site(200, IMPL_KEY, 9)),
        (106usize, site(300, "java/lang/Integer.valueOf:(I)Ljava/lang/Integer;", 11)),
    ]));
    let bases = [100usize];
    assert!(opaque.pc_resumes_flat_under_forwarder(204, &bases), "the implementation");
    assert!(opaque.pc_resumes_flat_under_forwarder(305, &bases), "the boxing call");
    assert!(opaque.pc_resumes_flat_under_forwarder(102, &bases), "the forwarder");
    assert!(!opaque.pc_resumes_flat_under_forwarder(1, &bases), "the compiling method");
    assert!(
        !opaque.pc_resumes_flat_under_forwarder(204, &[]),
        "not recorded opaque: the implementation keeps its chain"
    );
    let transparent = forwarder_sites();
    assert!(transparent.pc_resumes_flat_under_forwarder(103, &[]));
    assert!(!transparent.pc_resumes_flat_under_forwarder(204, &[]));

    let mut b = IrBuilder::new(1, 1);
    let mut fwd = frame(100, 110, 1, FWD_KEY, true);
    fwd.opaque = true;
    fwd.saved_locals = vec![3];
    let mut imp = frame(200, 210, 103, IMPL_KEY, false);
    imp.saved_locals = vec![4, 5];
    b.splice.push(fwd);
    b.splice.push(imp);
    b.stack = vec![6];
    assert!(b.splice_scope_chain(204).is_none());
    b.splice[0].opaque = false;
    assert!(b.splice_scope_chain(204).is_some(), "the control");
}

/// Wave 2d (L2W1-3): a forwarder nested in a spliced body `E` (spliced at
/// the compiling method's pc 1) re-executes `E`'s `invoke` at a trap: scope
/// 0 is `E` at that bci with the call's operands back on its stack, then
/// the compiling method parked at its own `invoke`. A forwarder at depth 0
/// has no such chain (it resumes flat).
#[test]
fn a_nested_forwarder_traps_into_the_enclosing_invoke() {
    const E_KEY: &str = "C.uniApplyNow:(Ljava/util/function/Function;)Ljava/lang/Object;";
    let mut b = IrBuilder::new(1, 1);
    let (param, e_local, e_below, recv, arg) = (3, 4, 5, 6, 7);
    let mut e = frame(100, 140, 1, E_KEY, false);
    e.saved_locals = vec![param];
    let mut fwd = frame(200, 210, 112, FWD_KEY, true);
    fwd.saved_locals = vec![e_local];
    fwd.saved_stack = vec![e_below];
    fwd.reexec_args = vec![recv, arg];
    b.splice.push(e);
    b.splice.push(fwd);
    let (locals, stack, chain) =
        b.forwarder_reexecute_chain(203).expect("a re-execute chain");
    assert_eq!(locals, vec![e_local], "E's locals");
    let keys: Vec<&str> = chain.scopes.iter().map(|s| s.method_key.as_str()).collect();
    assert_eq!(keys, vec![E_KEY, ""]);
    let bcis: Vec<u32> = chain.scopes.iter().map(|s| s.bci).collect();
    assert_eq!(bcis, vec![12, 1], "E at its invoke (112 - 100), the caller at its own");
    assert_eq!(chain.scopes[0].stack_len, 3, "E's stack below the call plus its operands");
    assert_eq!(stack, vec![e_below, recv, arg, param]);
    assert!(chain.split(&locals, &stack).is_some(), "the layout adds up");
    assert!(b.splice_scope_chain(203).is_none(), "the ordinary chain stays refused");
    assert!(b.forwarder_reexecute_chain(103).is_none(), "not in the forwarder");
    // At depth 0 the forwarder resumes flat.
    let mut top = IrBuilder::new(1, 1);
    top.splice.push(frame(200, 210, 1, FWD_KEY, true));
    assert!(top.forwarder_reexecute_chain(203).is_none());
}

/// Wave 2d: only a TOP-LEVEL forwarder's own body resumes flat under a
/// chain-relying graph; a nested one must publish its chain.
#[test]
fn only_a_top_level_forwarder_body_resumes_flat() {
    let site = |base: usize, key: &str, class_id: u32| IrInlineSite {
        base,
        code_len: 10,
        num_args: 1,
        max_locals: 2,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: key.to_string(),
        class_id,
    };
    let sites = IrInlineFrameSites::from_sites(&HashMap::from([
        (1usize, site(100, "C.e:()V", 5)),
        (104usize, site(200, FWD_KEY, PROXY)),
    ]));
    assert!(!sites.pc_in_lambda_forwarder(203));
    assert!(!sites.pc_resumes_flat_under_forwarder(203, &[]));
    let chain = sites.chain_at(203);
    assert_eq!(chain.len(), 1, "the forwarder names no level");
    assert_eq!(chain[0].method_key, "C.e:()V");
    assert_eq!(chain[0].bci, 4);
}

#[test]
fn a_trap_inside_the_forwarder_records_no_chain() {
    let mut b = IrBuilder::new(1, 1);
    b.splice.push(frame(100, 110, 1, FWD_KEY, true));
    b.stack = vec![3];
    assert!(b.splice_scope_chain(103).is_none());
    // The same frame NOT frameless records one (the control).
    b.splice[0].frameless = false;
    assert!(b.splice_scope_chain(103).is_some());
}

