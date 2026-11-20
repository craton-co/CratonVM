use super::{osr_live_stack_header_retires, LIVE_STACK_RETIRE_KEY};
use crate::config::VmConfig;
use crate::runtime::frame::{Frame, OSR_MAX_ATTEMPTS};
use crate::runtime::interpreter::{try_osr_with_backoff, OsrBackoffOutcome};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::atomic::Ordering;

#[test]
fn only_a_live_stack_retires_and_an_empty_one_never_reads_the_switch() {
    assert!(!osr_live_stack_header_retires(0, || panic!(
        "an empty stack must not read the switch"
    )));
    assert!(osr_live_stack_header_retires(1, || true));
    assert!(osr_live_stack_header_retires(3, || true));
    assert!(!osr_live_stack_header_retires(2, || false));
}

fn live_stack_frame(class_id: u32, method: &str) -> Frame {
    let mut f = Frame::new(
        ClassId::new(class_id),
        "cratonvm/test/J27OsrLiveStackHolder".to_string(),
        method.to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        1,
        &[],
    );
    f.stack.push_int(7).expect("max_stack 1");
    f
}

/// One due offer at a live-stack header retires it for the activation and
/// charges the activation to the method-wide budget, without the
/// background wait (whose `record_osr_background_pending` would zero the
/// back-edge count) and in either compile mode. With the switch off the
/// first offer does not retire.
#[test]
fn a_live_stack_header_is_retired_by_its_first_due_offer() {
    if !crate::runtime::env_cache::osr_backedge_enabled()
        || crate::runtime::env_cache::disable_jit()
    {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "osr-live-stack");
    thread.frames.push(live_stack_frame(0x27a1, "retired"));
    thread.frames.push(live_stack_frame(0x27a2, "offered"));
    let refused = || {
        shared
            .jit
            .bridge
            .osr_refused_loop_count
            .load(Ordering::Acquire)
    };

    thread.frames[0].backward_count = u32::MAX;
    let mut idx = 0;
    let out = cratonvm_types::flags::with_thread_overrides(
        &[(LIVE_STACK_RETIRE_KEY, None)],
        || try_osr_with_backoff(&shared, &mut thread, &mut idx, 0, 0),
    );
    assert!(matches!(out, OsrBackoffOutcome::Skip));
    assert_eq!(thread.frames[0].osr_attempts_at(0), Some(OSR_MAX_ATTEMPTS));
    assert_eq!(
        thread.frames[0].backward_count,
        u32::MAX,
        "no background wait was started"
    );
    assert_eq!(refused(), 1);

    thread.frames[1].backward_count = u32::MAX;
    let mut idx = 1;
    let out = cratonvm_types::flags::with_thread_overrides(
        &[(LIVE_STACK_RETIRE_KEY, Some("0"))],
        || try_osr_with_backoff(&shared, &mut thread, &mut idx, 1, 0),
    );
    assert!(matches!(out, OsrBackoffOutcome::Skip));
    assert_ne!(
        thread.frames[1].osr_attempts_at(0),
        Some(OSR_MAX_ATTEMPTS),
        "=0: the old offer, not a retirement"
    );
    assert_eq!(refused(), 1);
}

/// Source pin: the retirement sits after the method-wide budget (whose
/// first ask records the header) and before the background arm, so both
/// compile modes take it before any request or `try_osr`.
#[test]
fn the_offer_retires_a_live_stack_header_before_both_compile_arms() {
    let src = concat!(
        include_str!("../../interpreter/mod.rs"),
        "\n",
        include_str!("../../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../../interpreter/execute.rs"),
        "\n",
        include_str!("../../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let body = src
        .split("\npub(in crate::runtime::interpreter) fn try_osr_offer(")
        .nth(1)
        .and_then(|rest| rest.split("\n\u{7d}\n").next())
        .expect("try_osr_offer");
    let budget = body
        .find("jit_bridge::osr_loop_offer_allowed(")
        .expect("the method-wide budget");
    let retire = body
        .find("osr_live_stack::retire_live_stack_osr_header(")
        .expect("the live-stack retirement");
    let bg = body
        .find("if crate::runtime::env_cache::bg_compile() {")
        .expect("the background arm");
    let request = body.find("request_osr_classified").expect("the request");
    let inline = body.find("let osr_result = try_osr(").expect("the inline door");
    assert!(budget < retire && retire < bg && bg < request && retire < inline);
}
