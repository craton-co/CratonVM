use super::*;

fn not_listed(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    Ok(None)
}

/// Every listed body answers yes; an unlisted one (including a body that
/// MUTATES its argument, `StringBuilder`-style) answers no.
#[test]
fn the_list_admits_exactly_its_bodies() {
    for f in restartable_bodies() {
        assert!(native_is_oom_restartable(f));
    }
    assert!(!native_is_oom_restartable(not_listed));
    // gci i31/om: the `Class.getName` LEAF body is the funnel's callback
    // under `CRATONVM_CLASS_GET_NAME_MEMO` (default ON).
    assert!(native_is_oom_restartable(
        crate::lang_class::native_class_get_name_leaf
    ));
    assert!(!native_is_oom_restartable(
        crate::lang_string::native_string_value_of_object
    ));
}

/// No body is listed twice (a duplicate would hide a deletion in review).
#[test]
fn no_body_is_listed_twice() {
    let mut addrs: Vec<usize> = restartable_bodies().iter().map(|&f| f as usize).collect();
    addrs.sort_unstable();
    let n = addrs.len();
    addrs.dedup();
    assert_eq!(addrs.len(), n);
}

/// gci i47w/oe47: `Integer.valueOf(I)` / `Long.valueOf(J)` are restartable
/// through BOTH doors that reach the funnel -- the registry body and the
/// interpreter intrinsic table's trampoline (the callback a cached
/// interpreted `invokestatic` site passes) -- while the table's
/// argument-mutating `StringBuilder` trampolines stay out.
#[test]
fn gci_i47w_oe47_boxing_value_of_is_restartable_through_both_doors() {
    use crate::intrinsics::callback_for;
    use cratonvm_native_api::InterpIntrinsic;
    assert!(native_is_oom_restartable(
        crate::lang_math::native_integer_value_of
    ));
    assert!(native_is_oom_restartable(
        crate::lang_math::native_long_value_of
    ));
    assert!(native_is_oom_restartable(callback_for(
        InterpIntrinsic::IntegerValueOf
    )));
    assert!(native_is_oom_restartable(callback_for(
        InterpIntrinsic::LongValueOf
    )));
    // The trampoline is one function per kind: asking twice answers the
    // same address the funnel compares against.
    assert_eq!(
        callback_for(InterpIntrinsic::IntegerValueOf) as usize,
        callback_for(InterpIntrinsic::IntegerValueOf) as usize
    );
    assert!(!native_is_oom_restartable(callback_for(
        InterpIntrinsic::StringBuilderAppendString
    )));
    assert!(!native_is_oom_restartable(callback_for(
        InterpIntrinsic::IntegerIntValue
    )));
}
