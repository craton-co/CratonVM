use super::insert_dump_frames;

/// A proxy frame put before index 2 (the caller) moves the caller and
/// everything outer by one; the frames inner to it stay.
#[test]
fn inserted_dump_frames_move_the_outer_indices() {
    let frames = vec!['a', 'b', 'c', 'd'];
    let (out, moved) = insert_dump_frames(frames, vec![(2, vec!['P'])]);
    assert_eq!(out, vec!['a', 'b', 'P', 'c', 'd']);
    assert_eq!(moved, vec![0, 1, 3, 4]);
    let (out, moved) = insert_dump_frames(vec!['a'], vec![(1, vec!['P', 'Q'])]);
    assert_eq!(out, vec!['a', 'P', 'Q']);
    assert_eq!(moved, vec![0]);
}
