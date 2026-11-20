use super::{accessor_reads_illegal_argument, ReflectiveAccessor};
use cratonvm_native_api::StackTraceEntry;

fn at(class: &str, method: &str, synthetic: bool) -> StackTraceEntry {
    StackTraceEntry {
        class_name: std::sync::Arc::from(class),
        method_name: std::sync::Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: -1,
        class_id: (!synthetic).then(|| cratonvm_types::ClassId::new(7)),
        method_index: None,
    }
}

/// `java.base` by name, as the VM answers for the JDK's own classes.
fn java_base(e: &StackTraceEntry) -> bool {
    e.class_name.starts_with("java/") || e.class_name.starts_with("jdk/")
}

/// Interpreter round i1 wave 52, lane L4: JDK 25's
/// `AccessorUtils.isIllegalArgument`, row by row.
#[test]
fn the_accessor_on_top_is_an_argument_error_and_a_body_frame_is_not() {
    let dmha = "jdk/internal/reflect/DirectMethodHandleAccessor";
    let m = ReflectiveAccessor::Method;
    // A hidden callee's frame left out: the accessor is the top frame.
    let hidden = [
        at(dmha, "invoke", true),
        at("java/lang/reflect/Method", "invoke", true),
    ];
    assert!(accessor_reads_illegal_argument(&hidden, m, java_base));
    // The body's own (unnamed-module) frame on top.
    let body = [at("p/Vis", "deref", false), at(dmha, "invoke", true)];
    assert!(!accessor_reads_illegal_argument(&body, m, java_base));
    // `Class.cast` on top is skipped; implementation packages are passed.
    let cast = [
        at("java/lang/Class", "cast", false),
        at("java/lang/invoke/LambdaForm$MH", "invoke", false),
        at(dmha, "invoke", true),
    ];
    assert!(accessor_reads_illegal_argument(&cast, m, java_base));
    // A `java.base` frame outside them is the body's.
    let util = [
        at("java/util/Objects", "requireNonNull", false),
        at(dmha, "invoke", true),
    ];
    assert!(!accessor_reads_illegal_argument(&util, m, java_base));
    // The other accessor's frame: the body is a constructor's.
    let ctor = [
        at(
            "jdk/internal/reflect/DirectConstructorHandleAccessor",
            "newInstance",
            true,
        ),
        at(dmha, "invoke", true),
    ];
    assert!(!accessor_reads_illegal_argument(&ctor, m, java_base));
    assert!(accessor_reads_illegal_argument(
        &ctor,
        ReflectiveAccessor::Constructor,
        java_base
    ));
    assert!(!accessor_reads_illegal_argument(&[], m, java_base));
}
