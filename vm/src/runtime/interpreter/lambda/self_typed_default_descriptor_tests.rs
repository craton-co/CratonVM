use super::is_self_typed_default_descriptor;

const FUNCTION: &str = "java/util/function/Function";
const COMPARATOR: &str = "java/util/Comparator";

#[test]
fn accepts_the_two_self_typed_shapes() {
    assert!(is_self_typed_default_descriptor(
        FUNCTION,
        "(Ljava/util/function/Function;)Ljava/util/function/Function;"
    ));
    assert!(is_self_typed_default_descriptor(
        COMPARATOR,
        "()Ljava/util/Comparator;"
    ));
}

/// The overload the old two-shape probe could confuse: the call's own
/// descriptor takes a `Function`, and must not be served by a native
/// registered for the `(Comparator)` shape.
#[test]
fn rejects_another_overload_of_the_same_name() {
    assert!(!is_self_typed_default_descriptor(
        COMPARATOR,
        "(Ljava/util/function/Function;)Ljava/util/Comparator;"
    ));
    assert!(!is_self_typed_default_descriptor(
        COMPARATOR,
        "(Ljava/util/function/Function;Ljava/util/Comparator;)Ljava/util/Comparator;"
    ));
}

#[test]
fn rejects_other_return_types_and_malformed_input() {
    assert!(!is_self_typed_default_descriptor(
        FUNCTION,
        "(Ljava/util/function/Function;)Ljava/lang/Object;"
    ));
    assert!(!is_self_typed_default_descriptor(FUNCTION, "()V"));
    assert!(!is_self_typed_default_descriptor(FUNCTION, "()I"));
    assert!(!is_self_typed_default_descriptor(FUNCTION, ""));
    assert!(!is_self_typed_default_descriptor(
        FUNCTION,
        "Ljava/util/function/Function;)Ljava/util/function/Function;"
    ));
    // A prefix of the interface name is not the interface.
    assert!(!is_self_typed_default_descriptor(
        "java/util/function/Func",
        "()Ljava/util/function/Function;"
    ));
}

/// The self-typed default door runs a registered native in front of
/// concrete `java.base` bytecode, so it must be admitted through the §7
/// force-intercept helper and called through `safe_native_call` — never a
/// bare `find` + callback, which had no policy, no census and no pinning.
/// Structural, like `native_override`'s dial-door scans: a door that stops
/// asking the policy is invisible to every counter.
#[test]
fn self_typed_default_door_is_policy_routed() {
    let src = include_str!("../lambda.rs");
    let start = src
        .find("if is_self_typed_default_descriptor(iface, method_descriptor) {")
        .expect("the self-typed default door moved; this scan reads the wrong text");
    let rest = &src[start..];
    let end = rest
        .find("// Not found in native registry")
        .expect("the door's end marker moved");
    let door = &rest[..end];
    assert!(door.contains("native_override::admit_forced_native("));
    assert!(door.contains("crate::vm::safe_native_call("));
    assert!(
        !door.contains(".native_methods"),
        "the door probes the registry directly again, bypassing dispatch policy"
    );
}
