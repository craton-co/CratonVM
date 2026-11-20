use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;

const ABSTRACT_CHANGE: &str = "liquibase/change/core/CreateTableChange";

fn new_column_config(ctx: &mut MockNativeContext) -> ObjectRef {
    let class_id = ctx
        .ensure_class_initialized(LIQUIBASE_COLUMN_CONFIG)
        .expect("ColumnConfig test class id");
    ctx.alloc_object(class_id, 30)
}

fn new_abstract_change_filter(ctx: &mut MockNativeContext) -> (ObjectRef, ObjectRef) {
    let outer_class = ctx
        .ensure_class_initialized(ABSTRACT_CHANGE)
        .expect("AbstractChange test class id");
    let outer = ctx.alloc_object(outer_class, 1);
    let filter_class = ctx
        .ensure_class_initialized(LIQUIBASE_ABSTRACT_CHANGE_FIELD_FILTER)
        .expect("AbstractChange$1 class id");
    let filter = ctx.alloc_object(filter_class, 1);
    ctx.set_field_by_name(filter, "this$0", Value::Object(Some(outer)));
    (filter, outer)
}

fn new_date_object(
    ctx: &mut MockNativeContext,
    class_name: &str,
    millis: i64,
    nanos: Option<i32>,
) -> ObjectRef {
    let date_class = ctx
        .ensure_class_initialized("java/util/Date")
        .expect("Date test class id");
    let class_id = ctx
        .ensure_class_initialized(class_name)
        .expect("date subtype test class id");
    if class_id != date_class {
        ctx.set_superclass(class_id, date_class);
    }
    let fields = if class_name == "java/sql/Timestamp" {
        3
    } else {
        2
    };
    let obj = ctx.alloc_object(class_id, fields);
    ctx.set_field(obj, 0, Value::Long(millis));
    if let Some(nanos) = nanos {
        ctx.set_field(obj, fields - 1, Value::Int(nanos));
    }
    obj
}

fn excluded_field_filters_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(receiver))
        .as_deref()
        != Some(ABSTRACT_CHANGE)
        || method_name != "getExcludedFieldFilters"
        || descriptor != "(Lliquibase/ChecksumVersion;)[Ljava/lang/String;"
    {
        return None;
    }
    let fields = ctx.new_array(ArrayElementType::Reference, 2);
    let catalog_name = ctx.create_string("catalogName");
    let schema_name = ctx.create_string("schemaName");
    ctx.set_array_element(fields, 0, Value::Object(Some(catalog_name)));
    ctx.set_array_element(fields, 1, Value::Object(Some(schema_name)));
    Some(Ok(Some(Value::Object(Some(fields)))))
}

#[test]
fn essential_natives_include_liquibase_checksum_filter_intrinsic() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    assert!(
        registry
            .find(
                LIQUIBASE_ABSTRACT_CHANGE_FIELD_FILTER,
                "include",
                "(Ljava/lang/Object;Ljava/lang/String;Ljava/lang/Object;)Z",
            )
            .is_some(),
        "AbstractChange$1.include must be registered for the checksum/status hot path"
    );
    assert!(
        registry
            .find(
                LIQUIBASE_COLUMN_CONFIG,
                "getSerializableFieldValue",
                "(Ljava/lang/String;)Ljava/lang/Object;",
            )
            .is_some(),
        "ColumnConfig.getSerializableFieldValue must be registered for the checksum/status hot path"
    );
}

#[test]
fn abstract_change_filter_include_scans_excluded_field_array_directly() {
    let mut ctx = MockNativeContext::new();
    ctx.set_invoke_virtual_hook(excluded_field_filters_hook);
    let (filter, owner) = new_abstract_change_filter(&mut ctx);
    let value = ctx.create_string("kc");

    let field_name = ctx.create_string("catalogName");
    assert_eq!(
        native_liquibase_abstract_change_field_filter_include(
            &mut ctx,
            &[
                Value::Object(Some(filter)),
                Value::Object(Some(owner)),
                Value::Object(Some(field_name)),
                Value::Object(Some(value)),
            ],
        )
        .unwrap()
        .unwrap(),
        Value::Int(0)
    );

    let field_name = ctx.create_string("tableName");
    assert_eq!(
        native_liquibase_abstract_change_field_filter_include(
            &mut ctx,
            &[
                Value::Object(Some(filter)),
                Value::Object(Some(owner)),
                Value::Object(Some(field_name)),
                Value::Object(Some(value)),
            ],
        )
        .unwrap()
        .unwrap(),
        Value::Int(1)
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn column_config_get_serializable_field_value_reads_regular_fields_directly() {
    let mut ctx = MockNativeContext::new();
    let column = new_column_config(&mut ctx);
    let name = ctx.create_string("USER_LABEL");
    ctx.set_field_by_name(column, "name", Value::Object(Some(name)));

    let field_name = ctx.create_string("name");
    let value = native_liquibase_column_config_get_serializable_field_value(
        &mut ctx,
        &[Value::Object(Some(column)), Value::Object(Some(field_name))],
    )
    .expect("ColumnConfig.getSerializableFieldValue")
    .expect("field value");

    match value {
        Value::Object(Some(value)) => {
            assert_eq!(ctx.read_string(value).as_deref(), Some("USER_LABEL"));
        }
        other => panic!("unexpected value: {other:?}"),
    }
}

#[test]
fn column_config_get_serializable_field_value_formats_date_fields_without_iso_date_format() {
    let mut ctx = MockNativeContext::new();
    let column = new_column_config(&mut ctx);
    let timestamp = new_date_object(
        &mut ctx,
        "java/sql/Timestamp",
        1_682_944_496_000,
        Some(789_000_000),
    );
    ctx.set_field_by_name(column, "valueDate", Value::Object(Some(timestamp)));

    let field_name = ctx.create_string("valueDate");
    let value = native_liquibase_column_config_get_serializable_field_value(
        &mut ctx,
        &[Value::Object(Some(column)), Value::Object(Some(field_name))],
    )
    .expect("ColumnConfig.getSerializableFieldValue")
    .expect("field value");

    match value {
        Value::Object(Some(value)) => {
            assert_eq!(
                ctx.read_string(value).as_deref(),
                Some("2023-05-01T12:34:56.789")
            );
        }
        other => panic!("unexpected value: {other:?}"),
    }
}

/// Round 14 wave 6 (lane review6): the instant is `getTime()`'s answer,
/// not the raw slot, and a `getTime()` that throws propagates instead of
/// being replaced by the slot.
#[test]
fn column_config_date_field_asks_get_time_and_propagates_its_throw() {
    let mut ctx = MockNativeContext::new();
    let column = new_column_config(&mut ctx);
    // Slot 0 says 1970; `getTime()` says 2023.
    let timestamp = new_date_object(&mut ctx, "java/sql/Timestamp", 0, Some(789_000_000));
    ctx.set_field_by_name(column, "valueDate", Value::Object(Some(timestamp)));
    let field_name = ctx.create_string("valueDate");
    let args = [Value::Object(Some(column)), Value::Object(Some(field_name))];

    ctx.set_invoke_virtual_result(Ok(Some(Value::Long(1_682_944_496_789))));
    let value = native_liquibase_column_config_get_serializable_field_value(&mut ctx, &args)
        .expect("ColumnConfig.getSerializableFieldValue")
        .expect("field value");
    match value {
        Value::Object(Some(value)) => {
            assert_eq!(
                ctx.read_string(value).as_deref(),
                Some("2023-05-01T12:34:56.789")
            );
        }
        other => panic!("unexpected value: {other:?}"),
    }

    ctx.set_invoke_virtual_result(Err(MethodCallFailed::ExceptionThrown(timestamp)));
    assert!(matches!(
        native_liquibase_column_config_get_serializable_field_value(&mut ctx, &args),
        Err(MethodCallFailed::ExceptionThrown(_))
    ));
}
