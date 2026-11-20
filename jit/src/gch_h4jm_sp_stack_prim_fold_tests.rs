use super::{FrameLayout, OopMapEntry, SpStackPrimFold};

fn layout() -> FrameLayout {
    FrameLayout {
        spill_lo: 64,
        spill_hi: 160,
        ..FrameLayout::default()
    }
}

fn map(prims: &[i16], named: &[i16], exact: bool) -> OopMapEntry {
    let mut m = OopMapEntry::new(0);
    m.non_oop_stack_slots = prims.to_vec();
    m.frame_slot_offsets = named.to_vec();
    m.stack_marks_exact = exact;
    m
}

fn fold<'a>(maps: &'a [OopMapEntry], l: &FrameLayout) -> Option<&'a [i16]> {
    let mut f = SpStackPrimFold::new();
    for m in maps {
        f.add(m);
    }
    f.finish(l)
}

/// The g5jm shape: one exact map listing operand slots claims them.
#[test]
fn gch_h4jm_an_exact_map_claims_its_primitive_operand_slots() {
    let l = layout();
    let maps = [map(&[72, 80], &[64], true)];
    assert_eq!(fold(&maps, &l), Some(&[72i16, 80][..]));
    let twins = [map(&[80], &[64], true), map(&[80], &[], true)];
    assert_eq!(fold(&twins, &l), Some(&[80i16][..]), "siblings agree");
}

/// Every doubt refuses the whole id.
#[test]
fn gch_h4jm_the_fold_fails_closed() {
    let l = layout();
    assert_eq!(fold(&[], &l), None, "no map");
    assert_eq!(fold(&[map(&[], &[], true)], &l), None, "nothing listed");
    assert_eq!(fold(&[map(&[80], &[], false)], &l), None, "inexact marks");
    assert_eq!(
        fold(&[map(&[80], &[], true), map(&[80], &[], false)], &l),
        None,
        "one inexact sibling"
    );
    assert_eq!(
        fold(&[map(&[80], &[], true), map(&[72], &[], true)], &l),
        None,
        "siblings disagree"
    );
    assert_eq!(fold(&[map(&[80], &[80], true)], &l), None, "also named");
    assert_eq!(fold(&[map(&[88, 80], &[], true)], &l), None, "unsorted");
    assert_eq!(fold(&[map(&[80, 80], &[], true)], &l), None, "duplicate");
    assert_eq!(
        fold(&[map(&[56], &[], true)], &l),
        None,
        "below the spill band"
    );
    assert_eq!(
        fold(&[map(&[160], &[], true)], &l),
        None,
        "above the spill band"
    );
}
