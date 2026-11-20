use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame, VirtualObjectState};

fn cached(method_name: &str) -> Arc<CachedBytecodeMethod> {
    Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(0),
            class_name: Arc::from("T"),
            method_name: Arc::from(method_name),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            // invokestatic #1; return
            code: Arc::from(&[0xb8u8, 0x00, 0x01, 0xb1, 0x00, 0x00][..]),
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 8,
            max_locals: 4,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ))
}

fn scope(key: &str, bci: u32, locals: Vec<FrameValue>) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: key.to_string(),
        bci,
        locals,
        semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
        ..Default::default()
    }
}

/// `T.outer` parked in its `invokestatic` at bci 0, over `p/Q.inner`
/// trapping at its bci 2.
fn chain() -> ReconstructedFrame {
    let mut rf = scope("p/Q.inner:()V", 2, vec![FrameValue::Int(5)]);
    rf.caller_frames = vec![scope("T.outer:()V", 0, vec![FrameValue::Int(1)])];
    rf
}

fn artifact() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    cratonvm_jit::CompiledMethod::new(buf)
}

/// A live `T.outer` frame at pc 0, pushed; its index.
fn live_frame(shared: &SharedVm, thread: &mut JvmThread) -> usize {
    let pin_base = thread.native_pin_roots.len();
    let frame = build_deopt_frame_inner(
        shared,
        thread,
        &cached("outer"),
        &scope("T.outer:()V", 0, Vec::new()),
        false,
    )
    .expect("an empty frame rebuilds");
    thread.native_pin_roots.truncate(pin_base);
    thread.frames.push(frame);
    thread.frames.len() - 1
}

/// A chain naming a monitor in ANY scope is refused before the live frame
/// is written or anything is pushed: the OSR door's lock-record rewrite
/// after a guard-exit transfer is not the chain's (the outermost scope's
/// locks are part (c) of the patch page), and admission
/// (`osr_exit::chain_resumable_in_place`) refused the same chain.
#[test]
fn a_chain_guard_exit_holding_a_monitor_writes_nothing() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let idx = live_frame(&shared, &mut thread);
    let before = (thread.frames.len(), thread.frames[idx].pc);
    let lock = MonitorInfo {
        object: FrameValue::Object(0x1000),
        lock_depth: 1,
        relock: false,
    };
    let cm = artifact();
    for in_outer in [false, true] {
        let mut rf = chain();
        if in_outer {
            rf.caller_frames[0].monitors.push(lock.clone());
        } else {
            rf.monitors.push(lock.clone());
        }
        let why = transfer_osr_guard_exit_chain_into_live_frame(
            &shared,
            &mut thread,
            idx,
            &rf,
            &cm,
            &mut false,
        )
        .expect_err("a chain holding a monitor is not transferred");
        assert!(why.contains("monitor"), "{why}");
        assert_eq!((thread.frames.len(), thread.frames[idx].pc), before);
    }
}

/// Both provenance modes of the in-place chain transfer resolve every
/// inner scope before anything is written: a callee that does not resolve
/// (`p/Q`, not loaded) refuses with its own reason and leaves the live
/// frame and the stack as they were.
#[test]
fn a_chain_transfer_that_cannot_resolve_its_callee_writes_nothing() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let idx = live_frame(&shared, &mut thread);
    let before = (thread.frames.len(), thread.frames[idx].pc);
    let cm = artifact();
    let planned = transfer_osr_exit_chain_into_live_frame(
        &shared,
        &mut thread,
        idx,
        &chain(),
        &cm,
        ChainExitProvenance::RecordedBci,
        &mut false,
    )
    .expect_err("an unloaded callee does not resolve");
    let planless = transfer_osr_exit_chain_into_live_frame(
        &shared,
        &mut thread,
        idx,
        &chain(),
        &cm,
        ChainExitProvenance::PointAddress,
        &mut false,
    )
    .expect_err("an unloaded callee does not resolve");
    for why in [&planned, &planless] {
        assert!(!why.contains("not a recorded deopt point"), "{why}");
        assert!(why.contains("p/Q"), "{why}");
    }
    assert_eq!((thread.frames.len(), thread.frames[idx].pc), before);
}

/// A chain whose scalar-replaced object graph cannot be materialised is
/// refused by the run-to-completion sink BEFORE anything is pushed, and
/// leaves no pin: its callers answer `None` with an error and no replay.
#[test]
fn an_unmaterialisable_chain_is_refused_before_the_run() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mut rf = scope("T.inner:()V", 0, vec![FrameValue::VirtualObjectRef(6)]);
    rf.caller_frames = vec![scope(
        "T.outer:()V",
        0,
        vec![FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 4,
            class_id: 5,
            num_fields: 1,
            field_values: vec![FrameValue::Int(9)],
        })],
    )];
    let frame = |name: &str| InlinedChainFrame {
        cached: cached(name),
        locals: vec![Value::Object(None)],
        stack: Vec::new(),
        resume_pc: 3,
        executing_pc: 0,
        monitors: Vec::new(),
    };
    let chain = DeoptFrameChain {
        frames: vec![frame("outer"), frame("inner")],
        virtuals: Some(merge_chain_scopes(&rf)),
        outermost_cp_stamp: None,
        inner_own_source_frames: Vec::new(),
        forced: false,
    };
    let (pins, base) = (thread.native_pin_roots.len(), thread.frames.len());
    assert!(run_deopt_frame_chain_to_completion(&shared, &mut thread, chain).is_none());
    assert_eq!(thread.frames.len(), base, "nothing pushed");
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin left behind");
    // Round 13 wave 11 (lane chain5): the production twin re-allocates
    // before anything is pushed too (a non-heap refusal is `None`, a heap
    // one `OutOfMemoryError`), and leaves no pin of its own.
    let chain = DeoptFrameChain {
        frames: vec![frame("outer"), frame("inner")],
        virtuals: Some(merge_chain_scopes(&rf)),
        outermost_cp_stamp: None,
        inner_own_source_frames: Vec::new(),
        forced: false,
    };
    assert!(run_deopt_frame_chain_to_completion_releasing_pins(
        &shared,
        &mut thread,
        chain,
        pins
    )
    .is_none());
    assert_eq!(thread.frames.len(), base, "nothing pushed");
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin left behind");
}

/// The outermost-scope redefinition rule answers only for a chain, and
/// not at all while no class of the process was ever redefined.
#[test]
fn the_outermost_redefinition_rule_is_a_chain_rule() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let cm = artifact();
    let flat = scope("T.outer:()V", 0, Vec::new());
    assert!(!chain_outer_scope_redefined_since_compile(
        &shared,
        &cm,
        ClassId::new(0),
        &flat
    ));
    if !crate::classloading::any_class_redefined() {
        assert!(!chain_outer_scope_redefined_since_compile(
            &shared,
            &cm,
            ClassId::new(0),
            &chain()
        ));
    }
}

