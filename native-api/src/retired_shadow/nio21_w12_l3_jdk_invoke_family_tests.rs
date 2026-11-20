use super::*;
use cratonvm_types::HotSpotAlignment as Row;

/// The `JdkInvoke` family is `--real-jdk`'s registration policy for
/// `java/lang/invoke/`: every retired row there (the finders, the
/// combinators, `MethodType`, `MemberName`) and every stub, and none of the
/// `ACC_NATIVE` doors a real handle and a carrier both go through.
#[test]
fn the_invoke_family_takes_the_retired_rows_and_leaves_the_native_doors() {
    for (c, m, d) in [
        (
            "java/lang/invoke/MethodHandles$Lookup",
            "findVarHandle",
            "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        ),
        (
            "java/lang/invoke/MethodHandles$Lookup",
            "findVirtual",
            "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        ),
        // (`MethodHandle.asType` was this row until 3d31008561 made it the
        // reviewed FFM-carrier `Intrinsic`; an `Intrinsic` is no family row.)
        (
            "java/lang/invoke/MethodHandle",
            "asFixedArity",
            "()Ljava/lang/invoke/MethodHandle;",
        ),
    ] {
        assert!(triple_is_retired_shadow(c, m, d), "{c}.{m}{d} is a retired row");
        assert_eq!(compat_row_of_shadow(c, m, d), Some(Row::JdkInvoke), "{c}.{m}{d}");
    }
    assert_eq!(
        compat_row_of_stub_class("java/lang/invoke/MethodHandleProxies"),
        Some(Row::JdkInvoke)
    );
    for (c, m, d) in [
        (
            "java/lang/invoke/MethodHandle",
            "invokeExact",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            "java/lang/invoke/MethodHandle",
            "invoke",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            "java/lang/invoke/VarHandle",
            "get",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            "java/lang/invoke/VarHandle",
            "compareAndSet",
            "([Ljava/lang/Object;)Z",
        ),
    ] {
        assert!(
            !triple_is_retired_shadow(c, m, d),
            "{c}.{m}{d} is a native door"
        );
        assert_eq!(compat_row_of_shadow(c, m, d), None, "{c}.{m}{d}");
    }
    assert!(Row::JdkInvoke.default_on() && Row::JdkInvoke.needs_real_jdk());
}
