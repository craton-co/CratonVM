use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;

fn lucene_list_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method: &str,
    desc: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(receiver));
    if class_name.as_deref() != Some("java/util/ArrayList") {
        return None;
    }
    match (method, desc) {
        ("size", "()I") => Some(Ok(Some(ctx.get_field(receiver, 1)))),
        ("get", "(I)Ljava/lang/Object;") => {
            let index = match args.first() {
                Some(Value::Int(index)) if *index >= 0 => *index as usize,
                _ => return Some(Ok(Some(Value::Object(None)))),
            };
            let array = match ctx.get_field(receiver, 0) {
                Value::Object(Some(array)) => array,
                _ => return Some(Ok(Some(Value::Object(None)))),
            };
            Some(Ok(Some(ctx.get_array_element(array, index))))            }
        _ => None,        }    }

fn new_obj(ctx: &mut MockNativeContext, class_name: &str) -> ObjectRef {
    match ctx
        .new_object(class_name)
        .expect("mock allocation should not throw")
    {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("unexpected allocation result: {other:?}"),
    }
}

fn lucene_doc_with_body_len(ctx: &mut MockNativeContext, body_len: usize) -> ObjectRef {
    ctx.set_invoke_virtual_hook(lucene_list_hook);
    let doc = new_obj(ctx, "org/apache/lucene/document/Document");
    let list = new_obj(ctx, "java/util/ArrayList");
    let field = new_obj(ctx, "org/apache/lucene/document/TextField");
    let body = ctx.create_string(&"x".repeat(body_len));
    let array = ctx.new_array(ArrayElementType::Reference, 1);

    ctx.set_field(field, 0, Value::Object(Some(body)));
    ctx.set_array_element(array, 0, Value::Object(Some(field)));
    ctx.set_field(list, 0, Value::Object(Some(array)));
    ctx.set_field(list, 1, Value::Int(1));
    ctx.set_field(doc, 0, Value::Object(Some(list)));
    doc
}

#[test]
fn lucene_document_ram_usage_tracks_line_file_docs_body_length() {
    let mut ctx = mock_ctx();
    let doc = lucene_doc_with_body_len(&mut ctx, 1023);

    let bytes = estimate_lucene_document_ram_used(&mut ctx, doc)
        .expect("Lucene Document shape should be inspectable");

    assert_eq!(bytes, 2424);
}

#[test]
fn lucene_document_ram_usage_has_hotspot_minimum_floor() {
    let mut ctx = mock_ctx();
    let doc = lucene_doc_with_body_len(&mut ctx, 40);

    let bytes = estimate_lucene_document_ram_used(&mut ctx, doc)
        .expect("Lucene Document shape should be inspectable");

    assert_eq!(bytes, LUCENE_RAM_USAGE_MIN_DOCUMENT_BYTES);
}

#[test]
fn lucene_document_ram_usage_falls_back_for_unknown_shape() {
    let mut ctx = mock_ctx();
    let doc = new_obj(&mut ctx, "org/apache/lucene/document/Document");

    let got = native_lucene_ram_usage_tester_ram_used(&mut ctx, &[Value::Object(Some(doc))])
        .expect("native call should not throw");

    assert_eq!(
        got,
        Some(Value::Long(LUCENE_RAM_USAGE_FALLBACK_DOCUMENT_BYTES))
    );
}
