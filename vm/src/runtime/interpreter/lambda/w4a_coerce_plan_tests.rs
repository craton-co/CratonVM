use super::LambdaCoercePlan;

/// `Function<Type,Type>` through a `this`-capturing private impl: the SAM is
/// erased, the instantiated type narrows it. One bridge cast, no conversion.
#[test]
fn erased_reference_sam_needs_a_cast_but_no_conversion() {
    let plan = LambdaCoercePlan::build(
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "(LType;)LType;",
        "(LType;)LType;",
        true,
        1,
    );
    assert!(!plan.coerces);
    assert_eq!(plan.casts.len(), 1);
    assert_eq!(plan.casts[0].0, 0);
    assert_eq!(&*plan.casts[0].1, "LType;");
}

/// Identical descriptors, no captures: nothing to do at all.
#[test]
fn identical_descriptors_are_a_no_op() {
    let plan = LambdaCoercePlan::build("(I)I", "(I)I", "(I)I", false, 0);
    assert!(!plan.coerces);
    assert!(plan.casts.is_empty());
}

/// A boxed SAM argument over a primitive impl parameter is a real
/// conversion, so the plan defers to the full function.
#[test]
fn unboxing_is_a_conversion() {
    let plan = LambdaCoercePlan::build(
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "(I)I",
        "(Ljava/lang/Integer;)Ljava/lang/Integer;",
        false,
        0,
    );
    assert!(plan.coerces);
}

/// A captured receiver shifts the SAM argument onto the impl's first
/// declared parameter (receiver_present, one capture): `(Object)` vs `(I)`
/// is compared at the right position.
#[test]
fn receiver_and_capture_offsets_line_up() {
    let boxed = LambdaCoercePlan::build(
        "(Ljava/lang/Object;)V",
        "(I)V",
        "(Ljava/lang/Integer;)V",
        true,
        1,
    );
    assert!(boxed.coerces);
    let same = LambdaCoercePlan::build("(Ljava/lang/Object;)V", "(Ljava/lang/Object;)V", "(Ljava/lang/Object;)V", true, 1);
    assert!(!same.coerces && same.casts.is_empty());
}
