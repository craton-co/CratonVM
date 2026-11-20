use super::shift_served_frame_indices;

/// A captured trace `[task.run, outer, sleep-caller]` (bottom first) that
/// got the bottom `Thread.run` frame: every index moves up by one. A
/// middle frame inserted before captured index 2 moves only 2 and above.
#[test]
fn served_frames_shift_the_indices_above_them() {
    let pairs = vec![(0usize, 'a'), (1, 'b'), (2, 'c')];
    assert_eq!(shift_served_frame_indices(pairs.clone(), &[]), pairs);
    assert_eq!(
        shift_served_frame_indices(pairs.clone(), &[0]),
        vec![(1, 'a'), (2, 'b'), (3, 'c')]
    );
    assert_eq!(
        shift_served_frame_indices(pairs.clone(), &[2]),
        vec![(0, 'a'), (1, 'b'), (3, 'c')]
    );
    assert_eq!(
        shift_served_frame_indices(pairs, &[0, 2]),
        vec![(1, 'a'), (2, 'b'), (4, 'c')]
    );
}

/// With `attribute_locked_monitors`, the depth of the frame that locked a
/// monitor counts the leaf frames above it, and the served bottom frame
/// below it moves nothing.
#[test]
fn a_locked_monitor_keeps_its_depth_once_thread_run_is_served() {
    use crate::runtime::interpreter::held_monitors::attribute_locked_monitors;
    // SAFETY: never dereferenced; the attribution only compares.
    let a = unsafe { crate::types::ObjectRef::from_raw(0x1000 as *mut u8) };
    // Captured: [task.run, outer (locked a), Thread.sleep caller] + served
    // bottom `Thread.run` + three parked leaf frames = 7 entries.
    let shifted = shift_served_frame_indices(vec![(1usize, a)], &[0]);
    let (monitors, depths) = attribute_locked_monitors(7, vec![a], Some(shifted), None);
    assert_eq!(monitors, vec![a]);
    // Innermost first: 3 leaf frames, then the caller, then `outer`.
    assert_eq!(depths, vec![4]);
}
