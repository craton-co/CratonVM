use super::{
    captured_frame_dotted_name, native_stack_trace_element_to_string_jdk,
    stack_trace_element_text_units,
};
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeContext, NativeHandleScope, NativeHeapAccess,
};
use cratonvm_types::{ClassId, Value};

fn u(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[allow(clippy::too_many_arguments)]
fn text(
    class: &str,
    method: &str,
    file: Option<&str>,
    line: i32,
    module: Option<&str>,
    version: Option<&str>,
    loader: Option<&str>,
    format: i32,
) -> String {
    let file = file.map(u);
    let module = module.map(u);
    let version = version.map(u);
    let loader = loader.map(u);
    String::from_utf16(&stack_trace_element_text_units(
        &u(class),
        &u(method),
        file.as_deref(),
        line,
        module.as_deref(),
        version.as_deref(),
        loader.as_deref(),
        format,
    ))
    .unwrap_or_else(|e| panic!("not UTF-16: {e}"))
}

#[test]
fn the_name_from_text_answers_the_by_name_text() {
    let mut ctx = mock_ctx();
    // A VM identity of its own: the dotted-name cache is process-wide,
    // keyed by (vm, class id), and other mock contexts report 0.
    ctx.set_vm_identity(0x7_6000_0002);
    ctx.ensure_class_initialized("com/example/Frame")
        .unwrap_or_else(|e| panic!("no frame class: {e:?}"));
    ctx.ensure_class_initialized("Main")
        .unwrap_or_else(|e| panic!("no Main: {e:?}"));
    for name in [
        "com/example/Frame",
        "Main",
        "com/example/Missing$Inner",
        "com/example/P/0x3",
        "Q/0x1f",
    ] {
        let by_name = captured_frame_dotted_name(&ctx, false, name);
        let from_text = captured_frame_dotted_name(&ctx, true, name);
        assert_eq!(&*from_text, &*by_name, "{name}");
    }
    assert_eq!(
        &*captured_frame_dotted_name(&ctx, true, "com/example/Frame"),
        "com.example.Frame"
    );
    assert_eq!(&*captured_frame_dotted_name(&ctx, true, "Main"), "Main");
    assert_eq!(
        &*captured_frame_dotted_name(&ctx, true, "com/example/P/0x3"),
        "com.example.P/0x3"
    );
}

#[test]
fn the_text_follows_the_jdk_body() {
    // An application class: `app` dropped by the built-in-loader bit.
    assert_eq!(
        text(
            "com.example.Frame",
            "run",
            Some("Frame.java"),
            42,
            None,
            None,
            Some("app"),
            1
        ),
        "com.example.Frame.run(Frame.java:42)"
    );
    // A `java.base` frame: the version dropped by the JDK-module bit.
    assert_eq!(
        text(
            "java.lang.Thread",
            "run",
            Some("Thread.java"),
            1583,
            Some("java.base"),
            Some("25.0.4"),
            None,
            3
        ),
        "java.base/java.lang.Thread.run(Thread.java:1583)"
    );
    // The public constructor's element: format 0 prints every field.
    assert_eq!(
        text(
            "C",
            "m",
            Some("F.java"),
            1,
            Some("m"),
            Some("1.0"),
            Some("app"),
            0
        ),
        "app/m@1.0/C.m(F.java:1)"
    );
    // A named loader of an unnamed module: `loader//`.
    assert_eq!(
        text("p.C", "m", Some("C.java"), 3, None, None, Some("custom"), 0),
        "custom//p.C.m(C.java:3)"
    );
    // Empty names print nothing.
    assert_eq!(
        text(
            "p.C",
            "m",
            Some("C.java"),
            3,
            Some(""),
            Some("1"),
            Some(""),
            0
        ),
        "p.C.m(C.java:3)"
    );
    // A version without a module is never printed.
    assert_eq!(
        text("p.C", "m", None, 3, None, Some("1"), None, 0),
        "p.C.m(Unknown Source)"
    );
    // Native Method wins over a file; a file without a line; no file.
    assert_eq!(
        text("p.C", "m", Some("C.java"), -2, None, None, None, 0),
        "p.C.m(Native Method)"
    );
    assert_eq!(
        text("p.C", "m", Some("C.java"), -1, None, None, None, 0),
        "p.C.m(C.java)"
    );
    assert_eq!(
        text("p.C", "m", None, 7, None, None, None, 0),
        "p.C.m(Unknown Source)"
    );
    assert_eq!(
        text("p.C", "m", Some("C.java"), 0, None, None, None, 0),
        "p.C.m(C.java:0)"
    );
    // Only the version bit: the loader still prints.
    assert_eq!(
        text(
            "p.C",
            "m",
            Some("C.java"),
            5,
            Some("mod"),
            Some("2"),
            Some("ldr"),
            2
        ),
        "ldr/mod/p.C.m(C.java:5)"
    );
}

#[test]
fn the_text_keeps_every_code_unit() {
    let lone = [u16::from(b'A'), 0xD800, u16::from(b'B')];
    let out = stack_trace_element_text_units(&lone, &u("m"), None, -1, None, None, None, 0);
    assert_eq!(&out[..3], &lone[..]);
    assert_eq!(&out[3..], &u(".m(Unknown Source)")[..]);
}

/// The JDK 25 `StackTraceElement` instance fields, in slot order.
const STE_FIELDS: [&str; 9] = [
    "declaringClassObject",
    "classLoaderName",
    "moduleName",
    "moduleVersion",
    "declaringClass",
    "methodName",
    "fileName",
    "lineNumber",
    "format",
];

fn declare_ste(ctx: &mut MockNativeContext) -> ClassId {
    let ste = ctx
        .ensure_class_initialized("java/lang/StackTraceElement")
        .unwrap_or_else(|e| panic!("no StackTraceElement: {e:?}"));
    let fields = STE_FIELDS
        .iter()
        .enumerate()
        .map(|(slot, name)| FieldMetadata {
            name: (*name).to_string(),
            descriptor: match *name {
                "format" => "B",
                "lineNumber" => "I",
                "declaringClassObject" => "Ljava/lang/Class;",
                _ => "Ljava/lang/String;",
            }
            .to_string(),
            access_flags: 0x0002,
            slot_index: slot,
            declaring_class_id: ste,
            is_static: false,
        })
        .collect();
    ctx.set_declared_fields(ste, fields);
    ste
}

#[test]
fn the_intrinsic_renders_the_stored_fields() {
    let mut ctx = mock_ctx();
    let ste_class = declare_ste(&mut ctx);
    let mut scope = NativeHandleScope::new(&mut ctx);
    let ste = scope.alloc_object(ste_class, STE_FIELDS.len());
    let ste_h = scope.root(ste);
    let cls = scope.intern_string("java.lang.Thread");
    let meth = scope.intern_string("run");
    let file = scope.intern_string("Thread.java");
    let module = scope.intern_string("java.base");
    let version = scope.intern_string("25.0.4");
    let ste = scope.get(&ste_h);
    scope.set_field_by_name(ste, "declaringClass", Value::Object(Some(cls)));
    scope.set_field_by_name(ste, "methodName", Value::Object(Some(meth)));
    scope.set_field_by_name(ste, "fileName", Value::Object(Some(file)));
    scope.set_field_by_name(ste, "lineNumber", Value::Int(1583));
    scope.set_field_by_name(ste, "moduleName", Value::Object(Some(module)));
    scope.set_field_by_name(ste, "moduleVersion", Value::Object(Some(version)));
    scope.set_field_by_name(ste, "format", Value::Int(2));
    // Guard against a vacuous test: the stores above must be readable.
    assert_eq!(scope.get_field_by_name(ste, "lineNumber"), Value::Int(1583));
    let got =
        native_stack_trace_element_to_string_jdk(&mut *scope, &[Value::Object(Some(ste))]);
    let Ok(Some(Value::Object(Some(s)))) = got else {
        panic!("no String: {got:?}");
    };
    assert_eq!(
        scope.read_string(s).as_deref(),
        Some("java.base/java.lang.Thread.run(Thread.java:1583)")
    );
    // Format 0: the version prints.
    let ste = scope.get(&ste_h);
    scope.set_field_by_name(ste, "format", Value::Int(0));
    let got =
        native_stack_trace_element_to_string_jdk(&mut *scope, &[Value::Object(Some(ste))]);
    let Ok(Some(Value::Object(Some(s)))) = got else {
        panic!("no String: {got:?}");
    };
    assert_eq!(
        scope.read_string(s).as_deref(),
        Some("java.base@25.0.4/java.lang.Thread.run(Thread.java:1583)")
    );
}

#[test]
fn a_null_declaring_class_throws() {
    let mut ctx = mock_ctx();
    let ste_class = declare_ste(&mut ctx);
    let ste = ctx.alloc_object(ste_class, STE_FIELDS.len());
    let got = native_stack_trace_element_to_string_jdk(&mut ctx, &[Value::Object(Some(ste))]);
    assert!(got.is_err(), "an element nobody filled must throw: {got:?}");
}
