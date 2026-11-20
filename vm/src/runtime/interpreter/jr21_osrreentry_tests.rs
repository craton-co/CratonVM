use super::{loop_work_retry_boundary_crossed, osr_early_reentry_credit_enabled};
use crate::jit::profile::LOOP_WORK_PER_INVOCATION;



/// Text pin: only an EARLY entry that ENDS the activation (a return or a
/// throw out) raises the frame's count, to the schedule's threshold, and
/// it does so before both credits read the count (the entry credit in
/// this function and the frame pop's after it).
#[test]
fn only_an_activation_ending_early_entry_is_credited_before_the_credits() {
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
        .find("\npub(in crate::runtime::interpreter) fn try_osr_offer(")
        .expect("try_osr_offer");
    let body = &src[body_at..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let gate = flat
        .find(
            "if early_entry && (osr_throw.is_some() || osr_result.is_some()) \
             && osr_early_reentry_credit_enabled() {",
        )
        .expect("the early credit's gate");
    let raise = flat[gate..]
        .find("f.backward_count = f.backward_count.max(osr_backedge_threshold);")
        .expect("the raise")
        + gate;
    let entry_credit = flat
        .find("if (osr_throw.is_some() || osr_result.is_some()) && osr_entry_tierup_armed() {")
        .expect("the entry credit");
    let pop = flat
        .find("pop_and_recycle_frame(shared, thread);")
        .expect("the pop");
    assert!(raise < entry_credit);
    assert!(raise < pop);
}
