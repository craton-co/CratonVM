use super::credit_loop_work;
use crate::config::VmConfig;
use crate::runtime::frame::Frame;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

/// The credit is the store's own `add_loop_work` of the frame's
/// interpreted back edges, under the method's invocation key: the same
/// counter the invocation door counts into.
#[test]
fn credit_loop_work_adds_the_frames_back_edges_to_its_methods_counter() {
    use crate::jit::profile::{loop_work_credit, LOOP_WORK_PER_INVOCATION};
    let shared = SharedVm::new(VmConfig::default());
    let mut frame = Frame::new(
        ClassId::new(0x16c1),
        "cratonvm/test/R16ColdpathHolder".to_string(),
        "fill".to_string(),
        "(I)J".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        1,
        &[],
    );
    frame.backward_count = LOOP_WORK_PER_INVOCATION * 20;
    credit_loop_work(&shared, &frame);
    let key = cratonvm_jit_api::invoc_key_parts(0x16c1, "fill", "(I)J");
    let total = shared
        .jit
        .profile_store
        .add_loop_work(key, LOOP_WORK_PER_INVOCATION);
    assert_eq!(total, loop_work_credit(LOOP_WORK_PER_INVOCATION * 20) + 1);
}

/// JIT r20 wave 11 (lane pAlloc, OSRENTRY6 section 3): the OSR offer's
/// background wait credits the back edges its reset discards, so the
/// method's counter holds them once the frame's own count is zero.
#[test]
fn an_osr_background_wait_credits_the_back_edges_it_resets() {
    use crate::jit::profile::{loop_work_credit, LOOP_WORK_PER_INVOCATION};
    if !super::loop_work_tierup_enabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let mut frame = Frame::new(
        ClassId::new(0x20a1),
        "cratonvm/test/R20PAllocHolder".to_string(),
        "spin".to_string(),
        "(I)J".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        1,
        &[],
    );
    frame.backward_count = LOOP_WORK_PER_INVOCATION * 20;
    super::credit_loop_work_before_osr_wait(&shared, &frame);
    frame.record_osr_background_pending();
    assert_eq!(frame.backward_count, 0);
    let key = cratonvm_jit_api::invoc_key_parts(0x20a1, "spin", "(I)J");
    let total = shared
        .jit
        .profile_store
        .add_loop_work(key, LOOP_WORK_PER_INVOCATION);
    assert_eq!(total, loop_work_credit(LOOP_WORK_PER_INVOCATION * 20) + 1);
}

/// The entry credits only an activation the entry ENDS (a return or a
/// throw out), only while the switch is armed, and never beside the pop
/// credit of `CRATONVM_JIT_LOOP_WORK_TIERUP`, which counts the same edges.
#[test]
fn the_osr_entry_credit_is_gated_and_skips_committed_transfers() {
    let src = concat!(
        include_str!("../interpreter/mod.rs"),
        "\n",
        include_str!("../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../interpreter/execute.rs"),
        "\n",
        include_str!("../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let body_at = src
        .find(
            "
pub(in crate::runtime::interpreter) fn try_osr_offer(",
        )
        .expect("try_osr_offer");
    let body = &src[body_at..];
    let body = &body[..body
        .find(
            "
}
",
        )
        .expect("its end")];
    let gate = body
        .find("if (osr_throw.is_some() || osr_result.is_some()) && osr_entry_tierup_armed() {")
        .expect("the entry credit's gate");
    assert!(body[gate..].contains("credit_loop_work(shared, f);"));
    let armed_at = src
        .find(
            "
pub(in crate::runtime::interpreter) fn osr_entry_tierup_armed() -> bool {",
        )
        .expect("the gate fn");
    let armed = &src[armed_at..];
    let armed = &armed[..armed
        .find(
            "
}
",
        )
        .expect("its end")];
    assert!(armed.contains("&& !loop_work_tierup_enabled()"));
}
