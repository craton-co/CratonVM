use super::super::ObjectAt;
use super::*;
use cratonvm_gc_core::heap::{ObjectHeader, HEADER_SIZE, SLOT_SIZE};

fn obj_size(slots: usize) -> usize {
    HEADER_SIZE + SLOT_SIZE * slots
}

/// A legacy object of `slots` slots on a block of its own (offset).
fn own(og: &mut OldGen, slots: u32) -> Option<usize> {
    let p = og.alloc(obj_size(slots as usize), 8)?;
    // SAFETY: a fresh, zeroed block sized for the header and its slots.
    unsafe { (*(p as *mut ObjectHeader)).set_num_slots(slots) };
    Some(p as usize - og.base_ptr() as usize)
}

/// The debug plant makes exactly one walk gap: the walk stops at the
/// planted header, the gap accounts for the rest of the region, the
/// oracle refuses behind it, and a second plant is refused.
#[test]
fn a_planted_walk_break_is_one_gap_and_is_planted_once() {
    let mut og = OldGen::new(64 * 1024);
    og.set_block_offset_table_enabled(true);
    let before = own(&mut og, 2).unwrap();
    let at = og.plant_walk_break().expect("room for the plant");
    let after = own(&mut og, 2).unwrap();
    let base = og.base_ptr() as usize;
    assert_eq!(
        at - base,
        before + obj_size(2),
        "best fit: right behind `before`"
    );
    let (walked, gaps) = og.walk_objects_with_gaps();
    let walked: Vec<usize> = walked.iter().map(|&(p, _)| p as usize - base).collect();
    assert_eq!(walked, vec![before], "the walk stops at the planted header");
    assert_eq!(
        gaps,
        vec![(at - base, after + obj_size(2))],
        "one gap to the region's end"
    );
    assert_eq!(
        og.object_at(base + after),
        ObjectAt::Unknown,
        "behind the break"
    );
    assert!(matches!(og.object_at(base + before), ObjectAt::Base { .. }));
    assert_eq!(og.plant_walk_break(), None, "once per generation");
    assert_eq!(og.sizing_stats().walk_breaks_planted, 1);
}
