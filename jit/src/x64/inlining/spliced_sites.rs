/// `CRATONVM_JIT_SPLICE_STATIC_MINMAX` (j27 wave 2, lane NATMISC; **default
/// ON**, `=0` is the kill switch): the intrinsic entry a spliced `invokestatic`
/// of this triple is emitted as (`try_emit_spliced_static_minmax`), or `None`
/// for a triple the splice still calls. Serves the four `int`/`long` min/max
/// rows (CMOV) and the four `float`/`double` min/max rows (SSE).
pub fn spliced_static_minmax_entry(class: &str, name: &str, descriptor: &str) -> Option<usize> {
    if !true {
        return None;
    }
    let (entry, 2, _) = crate::try_resolve_intrinsic(class, name, descriptor)? else {
        return None;
    };
    [
        crate::MATH_MIN_INT_INTRINSIC,
        crate::MATH_MAX_INT_INTRINSIC,
        crate::MATH_MIN_LONG_INTRINSIC,
        crate::MATH_MAX_LONG_INTRINSIC,
        crate::MATH_MIN_FLOAT_INTRINSIC,
        crate::MATH_MAX_FLOAT_INTRINSIC,
        crate::MATH_MIN_DOUBLE_INTRINSIC,
        crate::MATH_MAX_DOUBLE_INTRINSIC,
    ]
    .contains(&entry)
    .then_some(entry)
}

/// Spliced `Object.getClass()` site: served inline under `CRATONVM_JIT_SPLICE_GETCLASS`
/// (default ON, `=0` kill) when direct helper and table are available.
pub fn spliced_getclass_site(invoke_kind: u8, name: &str, descriptor: &str) -> bool {
    matches!(invoke_kind, 0 | 2)
        && crate::is_object_get_class_site(name, descriptor)
        && crate::object_get_class_direct_enabled()
}

/// Spliced `Integer.valueOf(I)` / `Long.valueOf(J)` site: served inline under
/// `CRATONVM_JIT_SPLICE_VALUE_OF` (default ON, `=0` kill).
pub fn spliced_value_of_site(invoke_kind: u8, class: &str, name: &str, descriptor: &str) -> bool {
    invoke_kind == 3
        && ((class == "java/lang/Integer"
            && name == "valueOf"
            && descriptor == "(I)Ljava/lang/Integer;")
            || (class == "java/lang/Long"
                && name == "valueOf"
                && descriptor == "(J)Ljava/lang/Long;"))
}

/// Spliced String accessor site (`length`, `isEmpty`, `charAt`): served inline under
/// `CRATONVM_JIT_SPLICE_STRING_ACCESSOR` (default ON, `=0` kill).
pub fn spliced_string_accessor_entry(class: &str, name: &str, descriptor: &str) -> Option<u8> {
    if class != "java/lang/String" && class != "java/lang/CharSequence" {
        return None;
    }
    match (name, descriptor) {
        ("length", "()I") => Some(0),
        ("isEmpty", "()Z") => Some(1),
        ("charAt", "(I)C") => Some(2),
        _ => None,
    }
}

/// Spliced `Character.isISOControl` scalar site: served inline under
/// `CRATONVM_JIT_SPLICE_CHAR_SCALAR` (default ON, `=0` kill).
pub fn spliced_char_scalar_site(
    invoke_kind: u8,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    invoke_kind == 3
        && class == "java/lang/Character"
        && name == "isISOControl"
        && (descriptor == "(I)Z" || descriptor == "(C)Z")
}

pub fn spliced_current_thread_site(
    invoke_kind: u8,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    invoke_kind == 3
        && class == "java/lang/Thread"
        && descriptor == "()Ljava/lang/Thread;"
        && ((name == "currentThread")
            || (name == "currentCarrierThread"
                && crate::direct_helpers::carrier_thread_direct_enabled()))
}

pub fn spliced_byte_array_view_site(
    invoke_kind: u8,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    invoke_kind == 3
        && crate::ir_lower::byte_array_view_site_shape_parts(class, name, descriptor).is_some()
}
