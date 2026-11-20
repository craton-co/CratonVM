use super::innermost_array_element;
use crate::classloading::ClassId;

/// A fake store: `(id, is_array, component)`.
fn store(
    rows: &[(u32, bool, Option<u32>)],
) -> impl Fn(ClassId) -> Option<(bool, Option<ClassId>)> + '_ {
    move |id: ClassId| {
        rows.iter()
            .find(|(i, _, _)| *i == id.as_u32())
            .map(|&(_, is_array, component)| (is_array, component.map(ClassId::new)))
    }
}

#[test]
fn gci_i12_ul_array_element_follows_array_info_not_the_name() {
    // Two loaders' `p/X` (ids 10 and 20) and each one's `[Lp/X;` (11, 21),
    // and `[[Lp/X;` over 21 (22): each array names ITS element.
    let rows = [
        (10, false, None),
        (11, true, Some(10)),
        (20, false, None),
        (21, true, Some(20)),
        (22, true, Some(21)),
    ];
    let class_of = store(&rows);
    assert_eq!(innermost_array_element(ClassId::new(11), &class_of), Some(ClassId::new(10)));
    assert_eq!(innermost_array_element(ClassId::new(21), &class_of), Some(ClassId::new(20)));
    assert_eq!(innermost_array_element(ClassId::new(22), &class_of), Some(ClassId::new(20)));
}

#[test]
fn gci_i12_ul_array_element_declines_what_it_cannot_prove() {
    // `[I` (30) has no component; `[[I` (31) ends on it; a non-array (10);
    // an array whose element left the store (40 over 41); an id the store
    // does not hold (50). Each answers `None` (the caller's name fallback).
    let rows = [
        (10, false, None),
        (30, true, None),
        (31, true, Some(30)),
        (40, true, Some(41)),
    ];
    let class_of = store(&rows);
    assert_eq!(innermost_array_element(ClassId::new(30), &class_of), None);
    assert_eq!(innermost_array_element(ClassId::new(31), &class_of), None);
    assert_eq!(innermost_array_element(ClassId::new(10), &class_of), None);
    assert_eq!(innermost_array_element(ClassId::new(40), &class_of), None);
    assert_eq!(innermost_array_element(ClassId::new(50), &class_of), None);
    // A malformed self-loop terminates.
    let looped = [(60, true, Some(60))];
    assert_eq!(innermost_array_element(ClassId::new(60), &store(&looped)), None);
}
