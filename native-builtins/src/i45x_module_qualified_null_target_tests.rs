use super::*;

/// Interpreter round i45x, lane L5: a null or absent `other` is what the
/// JDK's `Objects.requireNonNull(other)` refuses.
#[test]
fn i45x_a_null_module_argument_is_detected() {
    assert!(module_arg_is_null(&[], 2));
    assert!(module_arg_is_null(&[Value::Int(0), Value::Object(None)], 1));
    assert!(module_arg_is_null(
        &[Value::Int(0), Value::Int(0), Value::Object(None)],
        2
    ));
}

/// Both qualified natives ask it after the package check and before the
/// target is read as a name (a source-shape pin, as the neighbouring
/// tests of this file do for registrations a mock cannot run).
#[test]
fn i45x_both_qualified_module_natives_refuse_a_null_target() {
    let src = include_str!("lib_parts/surefire_channel_can_use.rs").replace("\r\n", "\n");
    for signature in [
        "\"isExported\",\n        \"(Ljava/lang/String;Ljava/lang/Module;)Z\"",
        "\"isOpen\",\n        \"(Ljava/lang/String;Ljava/lang/Module;)Z\"",
    ] {
        let start = src.find(signature).expect("the registration");
        let body: String = src[start..].chars().take(3000).collect();
        let pkg = body.find("module_pkg_arg(ctx, args)").expect("package read");
        let null = body.find("module_arg_is_null(args, 2)").expect("null check");
        let name = body.find("module_name_of_mirror(ctx, *o)").expect("target name");
        assert!(pkg < null && null < name, "{signature}");
    }
}
