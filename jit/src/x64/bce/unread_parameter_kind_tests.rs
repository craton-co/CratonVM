use super::*;

/// Interpreter round i1 wave 22, lane L1: only the parameter slots the
/// scan left `Unknown` are seeded; a read parameter keeps its scanned kind,
/// a reference is left to the oop mask, a cat-2 marks its high half, and a
/// home that contradicts the kind or a slot layout the prologue does not
/// write seeds nothing.
#[test]
fn only_unknown_primitive_parameter_slots_are_seeded() {
    // static (IJLx;F)V with `iload_0; pop; return`: slot 0 read as int,
    // slots 1-2 (the long), 3 (the reference) and 4 (the float) never.
    let code: &[u8] = &[0x1a, 0x57, 0xb1];
    let tags = crate::param_slot_kind_tags("(IJLx;F)V", true);
    let slots = [0usize, 1, 3, 4];
    let mut kinds = classify_local_kinds(code, code.len(), 6);
    seed_unread_parameter_kinds(&mut kinds, &tags, &slots, 4, |_, _| true);
    assert_eq!(
        kinds,
        vec![
            LocalKind::Int,
            LocalKind::Long,
            LocalKind::HighHalf,
            LocalKind::Unknown,
            LocalKind::Float,
            LocalKind::Unknown,
        ]
    );

    // A float coloured into a GPR stays unknown.
    let mut kinds = classify_local_kinds(code, code.len(), 6);
    seed_unread_parameter_kinds(&mut kinds, &tags, &slots, 4, |_, k| {
        !matches!(k, LocalKind::Float)
    });
    assert_eq!(kinds[4], LocalKind::Unknown);
    assert_eq!(kinds[1], LocalKind::Long);

    // The legacy layout (no slot list, "argument i is slot i") disagrees
    // with a cat-2 parameter: nothing is seeded.
    let mut kinds = classify_local_kinds(code, code.len(), 6);
    seed_unread_parameter_kinds(&mut kinds, &tags, &[], 4, |_, _| true);
    assert_eq!(kinds[1], LocalKind::Unknown);
    assert_eq!(kinds[4], LocalKind::Unknown);

    // ...and agrees without one.
    let tags = crate::param_slot_kind_tags("(IZ)V", true);
    let mut kinds = classify_local_kinds(code, code.len(), 2);
    seed_unread_parameter_kinds(&mut kinds, &tags, &[], 2, |_, _| true);
    assert_eq!(kinds, vec![LocalKind::Int, LocalKind::Int]);

    // A long whose high half the body reuses as an int is left alone.
    // `iconst_0; istore_1; return` against static (J)V.
    let reuse: &[u8] = &[0x03, 0x3c, 0xb1];
    let tags = crate::param_slot_kind_tags("(J)V", true);
    let mut kinds = classify_local_kinds(reuse, reuse.len(), 2);
    seed_unread_parameter_kinds(&mut kinds, &tags, &[0], 1, |_, _| true);
    assert_eq!(kinds, vec![LocalKind::Unknown, LocalKind::Int]);
}
