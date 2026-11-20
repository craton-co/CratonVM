use super::action_admits_opcode;
use cratonvm_jit_api::npe_action as a;

/// JIT round 20 (lane throw): the monitor helpers' action codes
/// corroborate exactly their own opcode, so a stale bci at another opcode
/// still declines.
#[test]
fn monitor_actions_admit_only_their_opcode() {
    assert!(action_admits_opcode(a::MONITOR_ENTER, 0xc2));
    assert!(!action_admits_opcode(a::MONITOR_ENTER, 0xc3));
    assert!(!action_admits_opcode(a::MONITOR_ENTER, 0xb4));
    assert!(action_admits_opcode(a::MONITOR_EXIT, 0xc3));
    assert!(!action_admits_opcode(a::MONITOR_EXIT, 0xc2));
    assert!(!action_admits_opcode(a::NONE, 0xc2));
}

/// JIT round 20 wave 7 (lane fuzzC; `R20FuzzCAthrowNull1`): the compiled
/// `athrow`-on-null action corroborates exactly `athrow`, so a stale bci at
/// an invoke (the frame's last call) still declines.
#[test]
fn the_athrow_action_admits_only_athrow() {
    assert!(action_admits_opcode(a::ATHROW, 0xbf));
    assert!(!action_admits_opcode(a::ATHROW, 0xb6));
    assert!(!action_admits_opcode(a::ATHROW, 0xc2));
    assert!(!action_admits_opcode(a::NONE, 0xbf));
}
