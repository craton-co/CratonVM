use super::callee_rerun_refusal_what;
use crate::runtime::interpreter::CalleeHandlerMiss;

#[test]
fn an_unresolved_template_is_named_apart_from_a_frame_decline() {
    assert_eq!(
        callee_rerun_refusal_what(&CalleeHandlerMiss::Unresolved),
        "an exception whose callee handler template did not resolve"
    );
    for kept in [
        CalleeHandlerMiss::Declined,
        CalleeHandlerMiss::Unknown,
        CalleeHandlerMiss::NotCaught,
    ] {
        assert_eq!(
            callee_rerun_refusal_what(&kept),
            "an exception its handler could not resume",
            "the message every other miss always had"
        );
    }
}
