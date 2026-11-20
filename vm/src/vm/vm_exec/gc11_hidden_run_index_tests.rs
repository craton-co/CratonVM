use super::{captured_index_at, shift_served_frame_indices};

/// JIT round 19 wave 11, lane gc11: each hidden `Thread.run` frame a
/// `ThreadInfo` stack gets is recorded before the captured entry it sits
/// under, so a monitor a captured frame holds keeps HotSpot's depth.
#[test]
fn hidden_run_frames_record_the_captured_index_they_go_before() {
    // `--compatible`: captured `[lambda, tCold, hotPark]` plus the served
    // bottom `Thread.run` before captured 0.
    let mut inserted = vec![0usize];
    // `Thread.runWith` at position 1, the proxy at 2: both before captured 0.
    for position in [1, 2] {
        let before = captured_index_at(position, 3, &inserted);
        assert_eq!(before, 0, "position {position}");
        inserted.push(before);
        inserted.sort_unstable();
    }
    // `tCold` (captured 1) now sits at position 4: from-bottom 4, as HotSpot.
    assert_eq!(
        shift_served_frame_indices(vec![(1usize, 'f')], &inserted),
        vec![(4, 'f')]
    );
    // `--real-jdk`: captured `[run, runWith, lambda, tCold]`, the proxy at 2.
    assert_eq!(captured_index_at(2, 4, &[]), 2);
    assert_eq!(
        shift_served_frame_indices(vec![(3usize, 'f')], &[2]),
        vec![(4, 'f')]
    );
    // Above every captured entry (the parked leaf frames): nothing shifts.
    assert_eq!(captured_index_at(9, 4, &[]), 4);
}
