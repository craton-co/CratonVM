use super::{compiled_holds_explain_the_excess, compiled_label_names_frame};
use crate::runtime::frame::Frame;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::sync::Arc;

#[test]
fn compiled_holds_must_account_for_every_unrecorded_acquisition() {
    // A compiled caller's lock, nothing recorded: HotSpot's IMSE.
    assert!(compiled_holds_explain_the_excess(1, 0, 1));
    // A recursive hold split between a record and a compiled frame.
    assert!(compiled_holds_explain_the_excess(2, 1, 1));
    // No compiled hold named: the old leniency.
    assert!(!compiled_holds_explain_the_excess(1, 0, 0));
    // One acquisition nobody describes: may be the frame's own.
    assert!(!compiled_holds_explain_the_excess(2, 0, 1));
}

#[test]
fn a_label_names_a_frame_by_class_and_method() {
    let mut thread = JvmThread::new(ThreadId(132), "i132-l3-label");
    let frame = Frame::new_pooled(
        cratonvm_types::ClassId::new(0),
        Arc::from("p/C"),
        Arc::from("m"),
        Arc::from("(I)V"),
        None,
        Arc::from(&[0xb1u8][..]),
        Arc::from(Vec::new().into_boxed_slice()),
        2,
        2,
        &[],
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    assert!(compiled_label_names_frame("p/C.m:(I)V", &frame));
    // Looser than the walk: another overload still counts as a twin.
    assert!(compiled_label_names_frame("p/C.m:()V", &frame));
    assert!(!compiled_label_names_frame("p/C.n:(I)V", &frame));
    assert!(!compiled_label_names_frame("p/D.m:(I)V", &frame));
    assert!(!compiled_label_names_frame("garbage", &frame));
}
