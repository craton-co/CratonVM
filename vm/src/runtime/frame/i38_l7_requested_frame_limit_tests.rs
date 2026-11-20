use super::{
    frame_limit_for_stack_bytes, FrameStack, FRAMES_PER_STACK_MIB, MAX_REQUESTED_FRAME_LIMIT,
};

#[test]
fn a_requested_stack_grants_its_frames_per_mib_up_to_the_cap() {
    assert_eq!(frame_limit_for_stack_bytes(0), 0);
    assert_eq!(frame_limit_for_stack_bytes(-5), 0);
    assert_eq!(frame_limit_for_stack_bytes(1 << 20), FRAMES_PER_STACK_MIB);
    assert_eq!(
        frame_limit_for_stack_bytes(256 << 20),
        256 * FRAMES_PER_STACK_MIB
    );
    assert_eq!(
        frame_limit_for_stack_bytes(i64::MAX),
        MAX_REQUESTED_FRAME_LIMIT
    );
}

#[test]
fn the_requested_limit_only_raises_the_vms_limit() {
    let mut fs = FrameStack::new();
    assert!(fs.at_frame_limit(0, 0));
    assert!(!fs.at_frame_limit(0, 1));
    fs.set_requested_frame_limit(3);
    assert!(!fs.at_frame_limit(0, 0));
    assert!(!fs.at_frame_limit(2, 1));
    assert!(fs.at_frame_limit(3, 1));
    // A request below the VM's limit leaves the VM's limit in force.
    fs.set_requested_frame_limit(1);
    assert!(!fs.at_frame_limit(1, 2));
    assert!(fs.at_frame_limit(2, 2));
    assert_eq!(fs.requested_frame_limit(), 1);
}
