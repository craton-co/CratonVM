use super::*;

/// A class that cannot be resolved carries no known real fields, so the
/// base collapses to 0 and the private map is the layout — the same answer
/// the stub arm gives. Both arms must reach it, because a base that
/// silently became non-zero for an unresolvable class would push every
/// private slot past the object.
#[test]
fn an_unresolvable_class_has_a_zero_base() {
    let mut ctx = crate::test_mock::MockNativeContext::new();
    assert_eq!(base_for_class(&mut ctx, "does/not/Exist"), 0);
}

/// A class that IS loaded is answered from the loaded class, without
/// `ensure_class_initialized`.
///
/// This is a positive control for the arm, not merely for the number. The
/// mock's `ensure_class_initialized` answers `ClassId::new(0)` for every
/// name — an id it never declares — so the fallback path can only ever
/// return 0 here. Deleting the `class_id_by_name` arm turns this assertion
/// from 3 into 0. That is the whole point: a fast path that is never taken
/// looks exactly like a correct one when both arms agree, and this file's
/// sibling `scripts/unpinned-native-local-audit.py` carries the same lesson
/// written three times.
#[test]
fn a_loaded_class_is_answered_without_initialising_it() {
    let mut ctx = crate::test_mock::MockNativeContext::new();
    ctx.declare_class(
        "p/Loaded",
        &[("a", "I"), ("b", "J"), ("c", "Ljava/lang/Object;")],
    );
    assert_eq!(
        base_for_class(&mut ctx, "p/Loaded"),
        3,
        "the loaded arm must answer from the loaded class id"
    );
}

/// [`base_for_class_id`] gives the same base as [`base_for_class`] for the
/// same class, and 0 for an id that names nothing.
///
/// The two must not be allowed to drift: an accessor on the id form and an
/// allocator on the name form disagreeing about the base is the
/// two-layouts-on-one-class condition this module exists to prevent.
#[test]
fn the_id_form_and_the_name_form_agree() {
    let mut ctx = crate::test_mock::MockNativeContext::new();
    let cid = ctx.declare_class("p/Both", &[("x", "I"), ("y", "I")]);
    assert_eq!(base_for_class_id(&ctx, cid), 2);
    assert_eq!(
        base_for_class_id(&ctx, cid),
        base_for_class(&mut ctx, "p/Both")
    );
    assert_eq!(
        base_for_class_id(&ctx, cratonvm_types::ClassId::new(9999)),
        0,
        "an id naming no loaded class must collapse to 0, not index past the object"
    );
}

/// The width guard still collapses the base on a receiver too narrow to
/// carry the private map — the direction that keeps a foreign receiver
/// reading exactly the slots it read before this module existed.
///
/// Asserted through [`base_for_object`] rather than by inspection because
/// that function no longer round-trips the class NAME, and the guard is the
/// only thing left standing between a wide base and an out-of-range read.
#[test]
fn a_receiver_too_narrow_for_the_private_map_collapses_to_zero() {
    let mut ctx = crate::test_mock::MockNativeContext::new();
    let cid = ctx.declare_class("p/Narrow", &[("a", "I"), ("b", "I")]);
    // Allocated at the bare declared width: no room for `base + width`.
    let narrow = ctx.alloc_object(cid, 2);
    assert_eq!(base_for_object(&ctx, narrow, 3), 0);
    // Allocated the way this module's allocators do — `base + width`.
    let wide = ctx.alloc_object(cid, 2 + 3);
    assert_eq!(base_for_object(&ctx, wide, 3), 2);
}
