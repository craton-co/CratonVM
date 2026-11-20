//! Interpreter round i6 wave 134, lane L1 (i133-L1,
//! `CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL`): which blocked top frames
//! the window serves, and the error numbers it hands on.
use super::*;

#[test]
fn only_a_monitor_entry_top_frame_is_served() {
    // A `monitorenter` the thread is blocked in.
    assert!(blocked_at_monitor_entry(Some(0xc2), false));
    // A `synchronized` method's frame pushed before its monitor.
    assert!(blocked_at_monitor_entry(Some(0x2a), true));
    assert!(blocked_at_monitor_entry(None, true));
    // An invoke (the caller of a synchronized method whose monitor is
    // entered before the push), anything else, or no code.
    assert!(!blocked_at_monitor_entry(Some(0xb6), false));
    assert!(!blocked_at_monitor_entry(Some(0xc3), false));
    assert!(!blocked_at_monitor_entry(None, false));
}

#[test]
fn jvmti_codes_keep_their_numbers() {
    assert_eq!(jdwp_numbered(ERR_OPAQUE_FRAME), 32);
    assert_eq!(jdwp_numbered(ERR_NO_MORE_FRAMES), 31);
    assert_eq!(jdwp_numbered(-1), 113);
}

/// Source pin: both frame-control slots hand a suspended thread's request
/// to the window route, and that route is taken only with the switch on
/// and only after the parked route answered `NotParked`.
#[test]
fn the_window_route_follows_the_parked_one() {
    let src = concat!(
        include_str!("../native_env/mod.rs"),
        "\n",
        include_str!("../native_env/has_cap.rs"),
        "\n",
        include_str!("../native_env/jit_chain_len.rs"),
        "\n",
        include_str!("../native_env/read_capabilities.rs")
    )
    .replace("\r\n", "\n");
    let start = src
        .find("fn frame_op_on_parked<")
        .expect("frame_op_on_parked");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n}\n").expect("its end")];
    let parked = body.find("run_on_parked_thread(").expect("parked");
    let gate = body
        .find("blocked_entry_frame_control_on(shared)")
        .expect("gate");
    let window = body
        .find("with_blocked_frames(shared, tid, window)")
        .expect("window");
    assert!(parked < gate && gate < window);
    assert!(src.contains("|held| pop_through_window(&shared, held)"));
    assert!(src.contains("|held| forced_return_through_window(&shared, held, kind, forced)"));
}
