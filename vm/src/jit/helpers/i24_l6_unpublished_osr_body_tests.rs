use super::osr_continuation_body_named;
use std::sync::Arc;

#[test]
fn an_unpublished_osr_body_is_named_by_its_continuation_record() {
    let id = cratonvm_jit::reserve_compile_id();
    assert_ne!(id, 0, "an id is available");
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    let mut body = cratonvm_jit::CompiledMethod::new(buf);
    // What `ir_lower` does with its reservation (`hand_off`); the body
    // is then remembered by the OSR memo, never published.
    body.compile_id = id;
    let body = Arc::new(body);
    // Cast: the continuation record's encoding of the artifact.
    let ptr = Arc::as_ptr(&body) as usize;
    assert_eq!(
        cratonvm_jit::lookup_compile_id(id),
        None,
        "reserved, never bound: the lookup wave 23 relied on misses it"
    );
    assert_eq!(osr_continuation_body_named(1, id), None, "no OSR entry");
    let named =
        crate::runtime::interpreter::jit_bridge::with_osr_continuation_for_test(0, ptr, || {
            (
                osr_continuation_body_named(1, id),
                osr_continuation_body_named(1, id.wrapping_add(1)),
                osr_continuation_body_named(2, id),
                osr_continuation_body_named(0, id),
            )
        });
    assert_eq!(named.0, Some(ptr), "the innermost frame's OSR body");
    assert_eq!(named.1, None, "a compiled callee of it has another id");
    assert_eq!(named.2, None, "the innermost frame is not the OSR'd one");
    assert_eq!(named.3, None, "no frame at all");
    assert_eq!(
        osr_continuation_body_named(1, id),
        None,
        "withdrawn on exit"
    );
    drop(body);
}
