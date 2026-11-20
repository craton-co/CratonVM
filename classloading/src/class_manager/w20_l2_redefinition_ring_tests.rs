use super::*;

/// A class-scoped bump is named by the ring; an unattributed one, a span
/// longer than the ring, and a backwards span are not answerable. Other
/// tests of this crate redefine classes concurrently (attributed), so the
/// naming half is asserted only when the span was answerable.
#[test]
fn the_ring_names_attributed_redefinitions_and_refuses_the_rest() {
    let class = ClassId::new(0x0202_A1);
    let before = class_redefinition_count();
    bump_class_redefinition_count_of(class);
    let after = class_redefinition_count();
    assert!(after > before);
    let mut named = false;
    if for_each_redefined_class_between(before, after, |c| named |= c == class) {
        assert!(named, "the ring names the bumped class");
    }

    let before = class_redefinition_count();
    bump_class_redefinition_count();
    assert!(
        !for_each_redefined_class_between(before, class_redefinition_count(), |_| {}),
        "a bump that names no class retires everything"
    );
    let now = class_redefinition_count();
    assert!(!for_each_redefined_class_between(
        now.wrapping_sub(REDEFINITION_RING_LEN as u64 + 1),
        now,
        |_| {}
    ));
    assert!(!for_each_redefined_class_between(
        now,
        now.wrapping_sub(1),
        |_| {}
    ));
    assert!(
        for_each_redefined_class_between(now, now, |_| {}),
        "an empty span"
    );
}
