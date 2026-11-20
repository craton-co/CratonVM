use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn h2_test_object(ctx: &mut MockNativeContext, class_name: &str, fields: usize) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized(class_name)
        .expect("H2 test class id");
    ctx.alloc_object(cid, fields)
}

#[test]
fn h2_column_equals_matches_h2_table_and_name_rules() {
    let mut ctx = MockNativeContext::new();
    let table_a = h2_test_object(&mut ctx, "org/h2/table/Table", 1);
    let table_b = h2_test_object(&mut ctx, "org/h2/table/Table", 1);
    let col_a = h2_test_object(&mut ctx, H2_COLUMN, 2);
    let col_same = h2_test_object(&mut ctx, H2_COLUMN, 2);
    let col_other_table = h2_test_object(&mut ctx, H2_COLUMN, 2);
    let col_null_name = h2_test_object(&mut ctx, H2_COLUMN, 2);
    let not_column = h2_test_object(&mut ctx, "java/lang/Object", 1);
    let name_a = ctx.create_string("ID");
    let name_b = ctx.create_string("ID");

    ctx.set_field_by_name(col_a, "table", Value::Object(Some(table_a)));
    ctx.set_field_by_name(col_a, "name", Value::Object(Some(name_a)));
    ctx.set_field_by_name(col_same, "table", Value::Object(Some(table_a)));
    ctx.set_field_by_name(col_same, "name", Value::Object(Some(name_b)));
    ctx.set_field_by_name(col_other_table, "table", Value::Object(Some(table_b)));
    ctx.set_field_by_name(col_other_table, "name", Value::Object(Some(name_a)));
    ctx.set_field_by_name(col_null_name, "table", Value::Object(Some(table_a)));

    assert_eq!(
        h2_column_equals(
            &mut ctx,
            &[
                Value::Object(Some(col_null_name)),
                Value::Object(Some(col_null_name))
            ]
        )
        .expect("same column")
        .expect("return"),
        Value::Int(1),
        "Column.equals returns true for same object before null-field checks"
    );
    assert_eq!(
        h2_column_equals(
            &mut ctx,
            &[Value::Object(Some(col_a)), Value::Object(Some(col_same))]
        )
        .expect("same name/table")
        .expect("return"),
        Value::Int(1)
    );
    assert_eq!(
        h2_column_equals(
            &mut ctx,
            &[
                Value::Object(Some(col_a)),
                Value::Object(Some(col_other_table))
            ]
        )
        .expect("different table")
        .expect("return"),
        Value::Int(0)
    );
    assert_eq!(
        h2_column_equals(
            &mut ctx,
            &[
                Value::Object(Some(col_a)),
                Value::Object(Some(col_null_name))
            ]
        )
        .expect("null name")
        .expect("return"),
        Value::Int(0)
    );
    assert_eq!(
        h2_column_equals(
            &mut ctx,
            &[Value::Object(Some(col_a)), Value::Object(Some(not_column))]
        )
        .expect("wrong type")
        .expect("return"),
        Value::Int(0)
    );
}

#[test]
fn h2_db_object_equals_matches_subclasses_by_id() {
    let mut ctx = MockNativeContext::new();
    let db_cid = ctx
        .ensure_class_initialized(H2_DB_OBJECT)
        .expect("DbObject class id");
    let table_cid = ctx
        .ensure_class_initialized("org/h2/table/Table")
        .expect("Table class id");
    ctx.set_superclass(table_cid, db_cid);

    let db_obj = ctx.alloc_object(db_cid, 1);
    let table_alias = ctx.alloc_object(table_cid, 1);
    let different = ctx.alloc_object(table_cid, 1);
    ctx.set_field_by_name(db_obj, "id", Value::Int(7));
    ctx.set_field_by_name(table_alias, "id", Value::Int(7));
    ctx.set_field_by_name(different, "id", Value::Int(8));
    assert_eq!(
        h2_db_object_equals(
            &mut ctx,
            &[
                Value::Object(Some(db_obj)),
                Value::Object(Some(table_alias))
            ]
        )
        .expect("subclass with same id")
        .expect("return"),
        Value::Int(1)
    );
    assert_eq!(
        h2_db_object_equals(
            &mut ctx,
            &[Value::Object(Some(db_obj)), Value::Object(Some(different))]
        )
        .expect("subclass with different id")
        .expect("return"),
        Value::Int(0)
    );
}
