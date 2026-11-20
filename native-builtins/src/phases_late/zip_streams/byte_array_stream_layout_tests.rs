use super::has_byte_array_stream_layout;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{NativeClassAccess, NativeContext, NativeHeapAccess};

/// Register `java/io/ByteArrayInputStream` in every case, so the subclass
/// arm of the predicate is actually exercised rather than short-circuiting
/// on an unknown name -- a negative that held for the wrong reason would
/// not guard anything.
fn ctx_with_bais() -> (MockNativeContext, cratonvm_types::ClassId) {
    let mut ctx = MockNativeContext::new();
    let bais = ctx
        .ensure_class_initialized("java/io/ByteArrayInputStream")
        .expect("declare ByteArrayInputStream");
    (ctx, bais)
}

/// The shape test this replaced read slots 1 and 3 of any receiver.
/// Tomcat's `CoyoteInputStream` declares exactly one field, so those reads
/// landed past the object -- 16 `zgc real: field index OOB index=1/3
/// num_slots=1 op="get"` warnings per
/// `catalina.servlets.TestDefaultServletRfc9110Section13` run.
#[test]
fn a_one_slot_stream_is_refused_and_never_read_past() {
    let (mut ctx, _bais) = ctx_with_bais();
    let coyote = ctx
        .ensure_class_initialized("org/apache/catalina/connector/CoyoteInputStream")
        .expect("declare CoyoteInputStream");
    let stream = ctx.alloc_object(coyote, 1);
    assert_eq!(ctx.object_num_fields(stream), 1);
    assert!(!has_byte_array_stream_layout(&mut ctx, stream));
}

/// A real `ByteArrayInputStream` still takes the fast path: the point of
/// the fix is to identify the class, not to disable the path.
#[test]
fn a_real_byte_array_input_stream_is_accepted() {
    let (mut ctx, bais) = ctx_with_bais();
    let stream = ctx.alloc_object(bais, 4);
    assert!(has_byte_array_stream_layout(&mut ctx, stream));
}

/// An unrelated stream WIDE ENOUGH for the reads is refused on class
/// identity. The old test accepted it whenever slots 0/1/3 happened to hold
/// `(ref, int, int)`, which is not a distinctive shape.
#[test]
fn a_wide_but_unrelated_stream_is_refused_on_identity() {
    let (mut ctx, _bais) = ctx_with_bais();
    let other = ctx
        .ensure_class_initialized("org/example/ChunkedInputStream")
        .expect("declare ChunkedInputStream");
    let stream = ctx.alloc_object(other, 4);
    assert_eq!(ctx.object_num_fields(stream), 4);
    assert!(!has_byte_array_stream_layout(&mut ctx, stream));
}
