use super::{
    fill_stack_trace_element_from_record, frame_origin_records, frame_origin_row,
    record_frame_origin, COMPUTED_FORMAT_UNKNOWN,
};
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeContext, NativeHandleScope, NativeHeapAccess,
};
use cratonvm_types::{ClassId, Value};

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
fn a_recorded_class_fills_a_later_element_with_the_same_origin() {
    let mut ctx = mock_ctx();
    ctx.stack_trace_origin_record_flag.set(true);
    let ste_class = declare_ste(&mut ctx);
    let frame_class = ctx
        .ensure_class_initialized("com/example/Frame")
        .unwrap_or_else(|e| panic!("no frame class: {e:?}"));
    let mut scope = NativeHandleScope::new(&mut ctx);
    let first = scope.alloc_object(ste_class, STE_FIELDS.len());
    let first_h = scope.root(first);
    let cls = scope.intern_string("com.example.Frame");
    let module = scope.intern_string("com.example");
    let loader = scope.intern_string("app");
    let first = scope.get(&first_h);
    scope.set_field_by_name(first, "declaringClass", Value::Object(Some(cls)));
    scope.set_field_by_name(first, "moduleName", Value::Object(Some(module)));
    scope.set_field_by_name(first, "classLoaderName", Value::Object(Some(loader)));
    scope.set_field_by_name(first, "format", Value::Int(1));
    // Guard against a vacuous test: the stores above must be readable.
    assert_eq!(scope.get_field_by_name(first, "format"), Value::Int(1));
    assert!(frame_origin_row(&*scope, frame_class).is_none());
    record_frame_origin(&mut scope, &first_h, frame_class, "com.example.Frame");
    let row =
        frame_origin_row(&*scope, frame_class).unwrap_or_else(|| panic!("no row recorded"));
    assert_eq!(&*row.slashed, "com/example/Frame");
    assert_eq!(row.format, 1);
    assert_eq!(row.handles[2], 0, "no version: a null field holds no root");
    assert_eq!(
        row.computed_format
            .load(std::sync::atomic::Ordering::Relaxed),
        COMPUTED_FORMAT_UNKNOWN
    );
    // A second record of the class keeps the first row.
    record_frame_origin(&mut scope, &first_h, frame_class, "other");
    let again = frame_origin_row(&*scope, frame_class).unwrap_or_else(|| panic!("row lost"));
    assert!(std::sync::Arc::ptr_eq(&row, &again));

    let second = scope.alloc_object(ste_class, STE_FIELDS.len());
    let filled = fill_stack_trace_element_from_record(
        &mut *scope,
        second,
        &row,
        frame_class,
        "com.example.Frame",
        "run",
        Some("Frame.java"),
        42,
        None,
    )
    .unwrap_or_else(|_| panic!("the row's handles must resolve"));
    let first = scope.get(&first_h);
    for field in [
        "declaringClass",
        "moduleName",
        "moduleVersion",
        "classLoaderName",
        "format",
    ] {
        assert_eq!(
            scope.get_field_by_name(filled, field),
            scope.get_field_by_name(first, field),
            "{field}"
        );
    }
    assert_eq!(
        scope.get_field_by_name(filled, "lineNumber"),
        Value::Int(42)
    );
    let Value::Object(Some(meth)) = scope.get_field_by_name(filled, "methodName") else {
        panic!("no method name");
    };
    assert_eq!(scope.read_string(meth).as_deref(), Some("run"));
    assert!(matches!(
        scope.get_field_by_name(filled, "declaringClassObject"),
        Value::Object(Some(_))
    ));
    // Interpreter round i4 wave 91, lane L2: a method name handed in
    // already interned is stored as that very String, whatever the text
    // argument says.
    let held = scope.intern_string("run");
    let held_h = scope.root(held);
    let third = scope.alloc_object(ste_class, STE_FIELDS.len());
    let held = scope.get(&held_h);
    let filled = fill_stack_trace_element_from_record(
        &mut *scope,
        third,
        &row,
        frame_class,
        "com.example.Frame",
        "",
        Some("Frame.java"),
        7,
        Some(held),
    )
    .unwrap_or_else(|_| panic!("the row's handles must resolve"));
    let held = scope.get(&held_h);
    assert_eq!(
        scope.get_field_by_name(filled, "methodName"),
        Value::Object(Some(held))
    );
    assert_eq!(scope.get_field_by_name(filled, "lineNumber"), Value::Int(7));
}

#[test]
fn the_switch_off_records_nothing() {
    let mut ctx = mock_ctx();
    ctx.stack_trace_origin_record_flag.set(false);
    let ste_class = declare_ste(&mut ctx);
    let frame_class = ClassId::new(4242);
    let mut scope = NativeHandleScope::new(&mut ctx);
    assert!(frame_origin_records(&*scope).is_none());
    let ste = scope.alloc_object(ste_class, STE_FIELDS.len());
    let ste_h = scope.root(ste);
    let cls = scope.intern_string("X");
    let ste = scope.get(&ste_h);
    scope.set_field_by_name(ste, "declaringClass", Value::Object(Some(cls)));
    scope.set_field_by_name(ste, "format", Value::Int(0));
    record_frame_origin(&mut scope, &ste_h, frame_class, "X");
    assert!(frame_origin_row(&*scope, frame_class).is_none());
}
