use super::*;
use cratonvm_native_api::NativeKind;
use cratonvm_types::compat::CompatibilityMode;

const ROWS: &[(&str, &str, &str)] = &[
    ("java/lang/ThreadLocal", "get", "()Ljava/lang/Object;"),
    ("java/lang/ThreadLocal", "set", "(Ljava/lang/Object;)V"),
    ("java/lang/ThreadLocal", "remove", "()V"),
    (
        "java/lang/InheritableThreadLocal",
        "get",
        "()Ljava/lang/Object;",
    ),
    ("java/lang/InheritableThreadLocal", "<init>", "()V"),
];