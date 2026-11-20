use super::*;

/// The carrier's stashed frames leave for the slice and come back; the
/// slice's leftovers are dropped, never handed to the carrier.
#[test]
fn t1e2_stashes_set_aside_and_restore() {
    let _ = take_deopt_stashes_aside();
    restash_exceptional_frame(ReconstructedFrame::default());
    assert_eq!(exceptional_stash_depth(), 1);
    let aside = take_deopt_stashes_aside();
    assert_eq!(aside.len(), 1);
    assert_eq!(exceptional_stash_depth(), 0, "the slice starts empty");
    restash_exceptional_frame(ReconstructedFrame::default());
    restash_exceptional_frame(ReconstructedFrame::default());
    assert_eq!(restore_deopt_stashes_aside(aside), 2, "slice leftovers dropped");
    assert_eq!(exceptional_stash_depth(), 1, "the carrier's frame is back");
    let _ = take_deopt_stashes_aside();
}
