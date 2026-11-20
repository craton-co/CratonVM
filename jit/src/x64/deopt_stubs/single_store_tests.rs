//! `every-x64-deopt-point-is-stored-twice`: the stub bakes the published
//! `deopt_points` element, reached through the identity box.
use super::*;
use crate::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ResumeSemantics,
};

fn point(native_offset: u32, bci: u32, locals: usize) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset,
        bci,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "T.m:()V".to_string(),
            bci,
            locals: vec![crate::deopt::FrameValue::Int(1); locals],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    }
}

fn identity(p: &DeoptimizationPoint) -> Box<DeoptimizationPoint> {
    let mut id = p.clone();
    id.frame_state.locals.clear();
    id.frame_state.method_key.clear();
    Box::new(id)
}

#[test]
fn resolves_the_box_key_to_the_published_element() {
    let points = vec![point(10, 1, 2), point(20, 5, 3)];
    let boxes: Vec<Box<DeoptimizationPoint>> = points.iter().map(identity).collect();
    let index: FxHashMap<usize, usize> = boxes
        .iter()
        .enumerate()
        .map(|(i, b)| (std::ptr::addr_of!(**b) as usize, i))
        .collect();
    let key = std::ptr::addr_of!(*boxes[1]);
    let got = resolve_baked_point(&index, &boxes, &points, key).expect("resolves");
    assert!(
        std::ptr::eq(got, &points[1]),
        "bakes the published element, not the box"
    );
    assert_eq!(
        // SAFETY: `got` points into `points`, alive for this scope.
        unsafe { &*got }.frame_state.locals.len(),
        3,
        "the full frame state"
    );
}

#[test]
fn unknown_key_or_mismatched_pair_fails_closed() {
    let points = vec![point(10, 1, 2)];
    let boxes: Vec<Box<DeoptimizationPoint>> = points.iter().map(identity).collect();
    let index: FxHashMap<usize, usize> = boxes
        .iter()
        .enumerate()
        .map(|(i, b)| (std::ptr::addr_of!(**b) as usize, i))
        .collect();
    let stray = point(10, 1, 0);
    assert!(resolve_baked_point(&index, &boxes, &points, &stray).is_none());
    // Same index, different point: the scalar witness refuses the pair.
    let other = vec![point(99, 1, 2)];
    let key = std::ptr::addr_of!(*boxes[0]);
    assert!(resolve_baked_point(&index, &boxes, &other, key).is_none());
}

#[test]
fn point_table_check_detects_a_moved_or_resized_buffer() {
    let mut points = vec![point(10, 1, 1), point(20, 2, 1)];
    points.shrink_to_fit();
    assert!(
        baked_point_table_is(None, &points),
        "nothing recorded: nothing baked"
    );
    let record = Some(baked_point_record(&points));
    let moved = points; // a move keeps the heap buffer
    assert!(baked_point_table_is(record, &moved));
    let mut grown = moved;
    let record = Some(baked_point_record(&grown));
    grown.push(point(30, 3, 1));
    assert!(
        !baked_point_table_is(record, &grown),
        "a push after baking is refused"
    );
}
