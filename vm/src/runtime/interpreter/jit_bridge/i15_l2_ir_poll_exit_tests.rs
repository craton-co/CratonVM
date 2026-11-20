use super::osr_optimizing_transfer_guard_exit;
use crate::config::VmConfig;
use crate::runtime::frame::Frame;
use crate::types::Value;
use crate::vm::SharedVm;
use crate::{JvmThread, ThreadId};
use cratonvm_jit::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue, ReconstructedFrame,
    ResumeSemantics,
};

/// The body of the first `fn` definition `sig` names, up to its closing
/// brace, with the test modules cut off first.
fn production_fn(sig: &str) -> String {
    let whole = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    let src = &whole;
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let body = &src[start..];
    body[..body.find("\n\x7d\n").map_or(body.len(), |e| e + 3)].to_string()
}

fn flat(s: &str) -> String {
    s.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

/// Only the optimizing OSR door asks for the OSR poll mode exits (on the
/// innermost-frame bit); the method-entry door's body is entered by a
/// call, where that bit describes another method's frame. (Since wave 18
/// such a body may leave on the polling-body bit instead, which the IR door
/// decides itself: `i18_l2_ir_entry_exit_tests`.)
#[test]
fn only_the_optimizing_osr_door_compiles_poll_mode_exits() {
    let assembly = flat(&production_fn("fn compile_optimizing_artifact("));
    assert!(assembly.contains("osr_mode_exit_polls:osr_door,"));
    let osr = flat(&production_fn("fn build_osr_optimizing_artifact("));
    assert!(osr.contains("compile_optimizing_artifact(shared,&inputs,true)"));
    let entry = flat(&production_fn("fn try_jit_upgrade_with_gate_uncontained("));
    assert!(entry.contains("compile_optimizing_artifact(shared,&inputs,false)"));
}

/// An optimizing body entered through its own stub (no plan) is counted in
/// `osr_bodies_running` when it carries a poll mode exit, so the loop-exit
/// pause is taken for it.
#[test]
fn a_body_with_a_poll_mode_exit_is_counted_for_the_loop_exit_pause() {
    let try_osr = flat(&production_fn("fn try_osr("));
    assert!(try_osr.contains(
        "letleaves_at_its_polls=plan.is_some()||cratonvm_jit::osr_exit::has_poll_mode_exits(&compiled._deopt_point_boxes);"
    ));
    assert!(try_osr.contains("leaves_at_its_polls.then(||OsrBodyRunning::enter(shared))"));
}

/// `static int f() { int i = 0; while (i < 1000) i++; return i; }`, loop
/// header at 2.
const COUNT: [u8; 17] = [
    0x03, 0x3b, 0x1a, 0x11, 0x03, 0xe8, 0xa2, 0x00, 0x09, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf6, 0x1a,
    0xac,
];

/// A point at the header whose one local is a frame word, as the lowerer
/// describes a counter (the transfer writes the RECONSTRUCTED value).
fn exit_box(reason: DeoptReason) -> Box<DeoptimizationPoint> {
    Box::new(DeoptimizationPoint {
        native_offset: 0,
        bci: 2,
        reason,
        action: DeoptAction::Reinterpret,
        speculation_id: cratonvm_jit::deopt::speculation_id(2, reason),
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 2,
            locals: vec![FrameValue::StackSlot(-8)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(reason),
    })
}

/// The reconstructed frame each exit hands the door.
fn rframe(counter: i64) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: "T.count:()I".to_string(),
        bci: 2,
        locals: vec![FrameValue::Int(counter)],
        semantics: ResumeSemantics::REEXECUTE,
        ..Default::default()
    }
}

/// A body whose only guard's bci the lowerer did not admit: that guard is
/// refused with the live frame untouched, and the poll mode exit at the
/// same header is transferred, parking the frame at the header with the
/// counter the body had reached.
#[test]
fn a_poll_mode_exit_transfers_without_an_admitted_bci() {
    if !super::osr_optimizing_guard_exits_enabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "loop");
    thread.frames.push(Frame::new(
        cratonvm_types::ClassId::new(0),
        "T".to_string(),
        "count".to_string(),
        "()I".to_string(),
        None,
        COUNT.to_vec(),
        vec![],
        2,
        1,
        &[],
    ));
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
    buf.emit(&[0xC3]); // ret; metadata only
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.ir_osr_entries = vec![(2, 0, 0)];
    cm.ir_osr_sentinel_free = true;
    cm.ir_osr_guard_exit_bcis = Some(Vec::<u32>::new().into_boxed_slice());
    cm._deopt_point_boxes = vec![
        exit_box(DeoptReason::OsrExit),
        exit_box(DeoptReason::NullCheck),
    ];
    assert!(cratonvm_jit::osr_exit::has_poll_mode_exits(
        &cm._deopt_point_boxes
    ));
    let poll_exit = &*cm._deopt_point_boxes[0] as *const DeoptimizationPoint as usize;
    let guard = &*cm._deopt_point_boxes[1] as *const DeoptimizationPoint as usize;

    let refused = osr_optimizing_transfer_guard_exit(
        &shared,
        &mut thread,
        0,
        &rframe(9),
        &cm,
        guard,
        &mut false,
        &mut None,
        &mut false,
    );
    assert!(!refused, "a guard at a bci the lowerer did not admit");
    assert_eq!(
        thread.frames[0].pc, 0,
        "a refused exit leaves the frame alone"
    );

    let taken = osr_optimizing_transfer_guard_exit(
        &shared,
        &mut thread,
        0,
        &rframe(6),
        &cm,
        poll_exit,
        &mut false,
        &mut None,
        &mut false,
    );
    assert!(taken, "a poll mode exit needs no admitted bci");
    assert_eq!(thread.frames[0].pc, 2, "parked at the loop header");
    assert_eq!(thread.frames[0].get_local(0), Value::Int(6));
}

/// Interpreter round i1 wave 26, lane L2 (`i9-L5-...`, optimizing item
/// (b)): a poll mode exit whose header holds a lock the compiled code took
/// AND an elided level on the same object (a nested-lock elision's split)
/// is transferred, and the elided level is re-taken on the live frame's
/// thread: afterwards the thread holds the object twice, which the frame's
/// two `monitorexit`s release. (Flip with
/// `jit::osr_exit::OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED`: with it
/// off the point is refused and this test fails by design.)
#[test]
fn a_poll_mode_exit_re_takes_an_elided_lock_level() {
    use cratonvm_jit::deopt::MonitorInfo;
    if !super::osr_optimizing_guard_exits_enabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "loop");
    thread.frames.push(Frame::new(
        cratonvm_types::ClassId::new(0),
        "T".to_string(),
        "count".to_string(),
        "()I".to_string(),
        None,
        COUNT.to_vec(),
        vec![],
        2,
        1,
        &[],
    ));
    let obj = shared
        .mem
        .heap
        .alloc_object(cratonvm_types::ClassId::new(7), 0);
    let tid = thread.thread_id;
    // The level the compiled code took.
    shared.threads.monitors.enter(obj, tid);

    let lock = |object: FrameValue, relock: bool| MonitorInfo {
        object,
        lock_depth: 1,
        relock,
    };
    let mut exit = exit_box(DeoptReason::OsrExit);
    exit.frame_state.monitors = vec![
        lock(FrameValue::StackSlotRef(-16), false),
        lock(FrameValue::StackSlotRef(-16), true),
    ];
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
    buf.emit(&[0xC3]); // ret; metadata only
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.ir_osr_entries = vec![(2, 0, 0)];
    cm.ir_osr_sentinel_free = true;
    cm.ir_osr_guard_exit_bcis = Some(Vec::<u32>::new().into_boxed_slice());
    cm._deopt_point_boxes = vec![exit];
    let poll_exit = &*cm._deopt_point_boxes[0] as *const DeoptimizationPoint as usize;

    let word = FrameValue::Object(obj.as_ptr() as usize as u64); // Cast: the raw word a stash carries
    let mut frame = rframe(6);
    frame.monitors = vec![lock(word.clone(), false), lock(word, true)];
    let taken = osr_optimizing_transfer_guard_exit(
        &shared,
        &mut thread,
        0,
        &frame,
        &cm,
        poll_exit,
        &mut false,
        &mut None,
        &mut false,
    );
    assert!(
        taken,
        "a poll exit holding a lock and its elided level is transferred"
    );
    assert_eq!(thread.frames[0].pc, 2);
    assert!(
        shared.threads.monitors.exit(obj, tid).is_ok(),
        "the elided level"
    );
    assert!(
        shared.threads.monitors.exit(obj, tid).is_ok(),
        "the taken level"
    );
    assert!(
        shared.threads.monitors.exit(obj, tid).is_err(),
        "held exactly twice: one level taken by the code, one re-taken"
    );
}
