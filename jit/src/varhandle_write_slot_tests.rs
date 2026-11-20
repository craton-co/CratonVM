use super::*;

/// The shape `CompletableFuture.tryPushStack` writes through:
/// `NEXT.set(c, h)` on a reference field.
#[test]
fn instance_field_writes_map_to_slots() {
    assert_eq!(
        varhandle_write_helper_slot("set", "(Ljava/lang/Object;Ljava/lang/Object;)V"),
        Some(8)
    );
    assert_eq!(
        varhandle_write_helper_slot("set", "(Ljava/lang/Object;I)V"),
        Some(4)
    );
    assert_eq!(
        varhandle_write_helper_slot("setVolatile", "(Ljava/lang/Object;J)V"),
        Some(VARHANDLE_WRITE_KINDS.len() + 5)
    );
    // An array-TYPED field is still an ordinary reference-valued field,
    // on the `R` slot (round 11 wave 18): a supertype of no wrapper.
    assert_eq!(
        varhandle_write_helper_slot("set", "(Ljava/lang/Object;[Ljava/lang/String;)V"),
        Some(9)
    );
    assert_eq!(
        varhandle_write_helper_slot("setRelease", "(Ljava/lang/Object;[[I)V"),
        Some(2 * VARHANDLE_WRITE_KINDS.len() + 9)
    );
}

/// Round 11 wave 18: an array VALUE must be one descriptor consuming the
/// rest of the parameter list. `(LH;[II)V` is two values; bound with
/// `num_params: 2` it would leave a word on the operand stack.
#[test]
fn an_array_value_followed_by_more_parameters_is_refused() {
    let slot = |d: &str| varhandle_write_helper_slot("set", d);
    assert_eq!(slot("(LH;[II)V"), None);
    assert_eq!(slot("(LH;[Ljava/lang/String;I)V"), None);
    assert_eq!(slot("(LH;[Ljava/lang/String;LH;)V"), None);
    assert_eq!(slot("(LH;[V)V"), None);
    assert_eq!(slot("(LH;[)V"), None);
    // `L`/`R` are slot kinds, never descriptor bytes.
    assert_eq!(slot("(LH;R)V"), None);
    assert_eq!(slot("(LH;L)V"), None);
    assert_eq!(slot("(LH;[R)V"), None);
    assert_eq!(slot("([RI)V"), None);
    assert_eq!(VARHANDLE_WRITE_KINDS[9], b'R');
}

/// Round 11 wave 17 (`r11w16-orch-varhandle-string-site-still-cce-in-single-pass`):
/// the cold arm's synthetic call site is all the native learns about a
/// bound site. A static type whose `asType` verdict on a primitive
/// variable depends on WHICH primitive is not bound; wave 18: a site typed
/// as any other class takes the `R` slot, whose synthetic site is not an
/// `Object` site.
#[test]
fn wrapper_and_number_typed_values_are_not_bound() {
    let slot = |m: &str, d: &str| varhandle_write_helper_slot(m, d);
    assert_eq!(slot("set", "(LH;Ljava/lang/String;)V"), Some(9));
    assert_eq!(slot("set", "(LH;LH$Node;)V"), Some(9));
    assert_eq!(slot("set", "(LH;Ljava/lang/Object;)V"), Some(8));
    assert_eq!(slot("set", "(LH;Ljava/io/Serializable;)V"), Some(8));
    assert_eq!(slot("set", "(LH;Ljava/lang/Comparable;)V"), Some(8));
    assert_eq!(
        slot("setOpaque", "(LH;Ljava/lang/constant/Constable;)V"),
        Some(3 * VARHANDLE_WRITE_KINDS.len() + 8)
    );
    // Their verdict depends on which primitive the variable is: unbound.
    for v in [
        "Ljava/lang/Number;",
        "Ljava/lang/constant/ConstantDesc;",
        "Ljava/lang/Integer;",
        "Ljava/lang/Long;",
        "Ljava/lang/Character;",
        "Ljava/lang/Boolean;",
    ] {
        assert_eq!(slot("set", &format!("(LH;{v})V")), None, "{v}");
    }
    assert_eq!(
        VARHANDLE_WRITE_SLOTS,
        VARHANDLE_WRITE_MODES.len() * VARHANDLE_WRITE_KINDS.len()
    );
    assert_eq!(VARHANDLE_CAS_SLOTS, VARHANDLE_CAS_KINDS.len());
}

/// Everything that is NOT a one-coordinate instance-field write keeps the
/// dispatch it has today.
#[test]
fn other_varhandle_shapes_are_refused() {
    // static field: zero coordinates
    assert_eq!(varhandle_write_helper_slot("set", "(I)V"), None);
    // array element / byte-array view: two coordinates
    assert_eq!(varhandle_write_helper_slot("set", "([BII)V"), None);
    assert_eq!(
        varhandle_write_helper_slot("set", "(Ljava/nio/ByteBuffer;II)V"),
        None
    );
    // a read mode is not a write mode
    assert_eq!(
        varhandle_write_helper_slot("get", "(Ljava/lang/Object;)I"),
        None
    );
    // compareAndSet is not bound here (five arguments; see the header)
    assert_eq!(
        varhandle_write_helper_slot("compareAndSet", "(Ljava/lang/Object;II)Z"),
        None
    );
    // a non-void return is not a write
    assert_eq!(
        varhandle_write_helper_slot("getAndSet", "(Ljava/lang/Object;I)I"),
        None
    );
}
