use super::*;

/// The shape `CompletableFuture.tryPushStack` CASes through, and the one
/// this bind exists for.
#[test]
fn a_reference_instance_field_cas_gets_the_reference_slot() {
    assert_eq!(
        varhandle_cas_helper_slot(
            "compareAndSet",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z"
        ),
        Some(8),
    );
    // An array-typed value is a reference too, in the `R` group (round
    // 11 wave 18): an array type is a supertype of no wrapper.
    assert_eq!(
        varhandle_cas_helper_slot(
            "compareAndSet",
            "(Ljava/lang/Object;[Ljava/lang/String;[Ljava/lang/String;)Z"
        ),
        Some(9),
    );
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(Ljava/lang/Object;[I[I)Z"),
        Some(9),
    );
}

/// Round 11 wave 17 (`r11w16-orch-varhandle-string-site-still-cce-in-single-pass`):
/// an operand whose static type is `Number`, `ConstantDesc` or a wrapper
/// is not bound, because the cold arm's synthetic call site would answer
/// a primitive variable's `asType` wrongly for it. Wave 18: a site typed
/// as any other class takes the `R` slot (index 9), whose synthetic site
/// is not an `Object` site.
#[test]
fn wrapper_and_number_typed_operands_are_not_bound() {
    let slot = |d: &str| varhandle_cas_helper_slot("compareAndSet", d);
    // `Node`-typed CAS (`ConcurrentLinkedQueue.casNext`): still bound.
    assert_eq!(slot("(LQ$Node;LQ$Node;LQ$Node;)Z"), Some(9));
    assert_eq!(
        slot("(LQ;Ljava/lang/Comparable;Ljava/io/Serializable;)Z"),
        Some(8)
    );
    assert_eq!(slot("(LQ;Ljava/lang/Long;Ljava/lang/Long;)Z"), None);
    assert_eq!(slot("(LQ;Ljava/lang/Object;Ljava/lang/Number;)Z"), None);
    assert_eq!(
        slot("(LQ;Ljava/lang/constant/ConstantDesc;Ljava/lang/Object;)Z"),
        None
    );
    // A reference and a primitive are still not one CAS shape.
    assert_eq!(slot("(LQ;Ljava/lang/Object;I)Z"), None);
    assert_eq!(slot("(LQ;LQ$Node;I)Z"), None);
    assert_eq!(slot("(LQ;[II)Z"), None);
}

/// Round 11 wave 18: one `R` operand puts the pair on the `R` slot,
/// in either position — `asType` refuses a primitive variable on it
/// before it converts either value.
#[test]
fn a_mixed_object_and_typed_pair_takes_the_typed_slot() {
    let slot = |d: &str| varhandle_cas_helper_slot("compareAndSet", d);
    assert_eq!(slot("(LQ;Ljava/lang/Object;Ljava/lang/String;)Z"), Some(9));
    assert_eq!(slot("(LQ;Ljava/lang/String;Ljava/lang/Object;)Z"), Some(9));
    assert_eq!(slot("(LQ;Ljava/lang/Comparable;[J)Z"), Some(9));
    assert_eq!(slot("(LQ;Ljava/lang/Object;Ljava/lang/Object;)Z"), Some(8));
    // `L`/`R` are slot kinds, never descriptor bytes.
    assert_eq!(slot("(LQ;RR)Z"), None);
    assert_eq!(slot("(LQ;LL)Z"), None);
    assert_eq!(VARHANDLE_CAS_KINDS[9], b'R');
}

/// Primitives get their own slots, in `VARHANDLE_CAS_KINDS` order.
#[test]
fn primitive_values_get_their_own_slots() {
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(Ljava/lang/Object;II)Z"),
        Some(4)
    );
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(Ljava/lang/Object;JJ)Z"),
        Some(5)
    );
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(Ljava/lang/Object;ZZ)Z"),
        Some(0)
    );
}

/// Everything this bind must NOT take, each for its own reason.
#[test]
fn the_shapes_that_are_not_an_instance_field_cas_are_refused() {
    // No coordinate at all: a static-field handle.
    assert_eq!(varhandle_cas_helper_slot("compareAndSet", "(II)Z"), None);
    // Two coordinates: an array-element or byte-view handle.
    assert_eq!(varhandle_cas_helper_slot("compareAndSet", "([BIII)Z"), None);
    // A `MemorySegment` handle's `J` offset coordinate.
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(Ljava/lang/foreign/MemorySegment;JII)Z"),
        None,
    );
    // `expected` and `new` of different kinds is not a CAS shape.
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(Ljava/lang/Object;IJ)Z"),
        None
    );
    // One operand only.
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(Ljava/lang/Object;I)Z"),
        None
    );
    // Three operands.
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(Ljava/lang/Object;III)Z"),
        None
    );
    // A non-boolean return is a different access mode.
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(Ljava/lang/Object;II)I"),
        None,
    );
}

/// Round 11 wave 5 (lane rt): a `)` inside a class name (legal, JVMS
/// 4.2.2) no longer cuts the parameter list short. The first-`)` scan
/// refused all three shapes below; the walk classifies them like their
/// plain-named twins.
#[test]
fn a_paren_in_a_class_name_does_not_cut_the_parameter_list() {
    assert_eq!(
        varhandle_cas_helper_slot("compareAndSet", "(LA)B;II)Z"),
        Some(4)
    );
    assert_eq!(varhandle_write_helper_slot("set", "(LA)B;J)V"), Some(5));
    assert_eq!(varhandle_read_helper_slot("get", "(LA)B;)I"), Some(4));
}

/// The neighbouring access modes are NOT bound with `compareAndSet`, and
/// the slot function is what refuses them.
#[test]
fn the_neighbouring_modes_are_not_bound() {
    for mode in [
        "weakCompareAndSet",
        "weakCompareAndSetPlain",
        "compareAndExchange",
        "set",
    ] {
        assert_eq!(
            varhandle_cas_helper_slot(mode, "(Ljava/lang/Object;II)Z"),
            None,
            "{mode} must not share the compareAndSet bind",
        );
    }
}
