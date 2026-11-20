//! JIT round 21 wave 1, lane reflect (`CRATONVM_REFLECT_SIG_POLY_UOE`).
use super::reflective_signature_polymorphic_refusal as refusal;

const MH: &str = "java/lang/invoke/MethodHandle";
const VH: &str = "java/lang/invoke/VarHandle";
const ARGS_OBJECT: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";
const NATIVE_VARARGS: i32 = 0x0100 | 0x0080 | 0x0001 | 0x0010;

#[test]
fn the_two_method_handle_invokers_and_var_handle_modes_refuse() {
    assert_eq!(
        refusal(MH, "invokeExact", ARGS_OBJECT, NATIVE_VARARGS),
        Some("MethodHandle.invokeExact cannot be invoked reflectively")
    );
    assert_eq!(
        refusal(MH, "invoke", ARGS_OBJECT, NATIVE_VARARGS),
        Some("MethodHandle.invoke cannot be invoked reflectively")
    );
    for desc in [ARGS_OBJECT, "([Ljava/lang/Object;)Z", "([Ljava/lang/Object;)V"] {
        assert_eq!(
            refusal(VH, "get", desc, NATIVE_VARARGS),
            Some("VarHandle access mode methods cannot be invoked reflectively")
        );
    }
}

#[test]
fn every_other_method_is_dispatched() {
    // `invokeWithArguments(Object...)` is a plain Java varargs method.
    assert_eq!(refusal(MH, "invokeWithArguments", ARGS_OBJECT, 0x0081), None);
    assert_eq!(refusal(MH, "bindTo", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;", 1), None);
    // A non-native `VarHandle` method, and other classes' natives.
    assert_eq!(refusal(VH, "withInvokeExactBehavior", "()Ljava/lang/invoke/VarHandle;", 1), None);
    assert_eq!(refusal(VH, "x", ARGS_OBJECT, 0x0081), None);
    assert_eq!(refusal("p/MethodHandle", "invokeExact", ARGS_OBJECT, NATIVE_VARARGS), None);
}
