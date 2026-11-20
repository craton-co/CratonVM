use super::c1_duplicate_unbound_statics;

/// `MOV EAX, 7; RET`.
fn body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]);
    cratonvm_jit::CompiledMethod::new(buf)
}

fn site(invoke_kind: u8) -> Box<cratonvm_jit::JitInvokeInfo> {
    Box::new(cratonvm_jit::JitInvokeInfo {
        class_name: "r20w19/C",
        method_name: "m",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    })
}

/// Only a live single-pass body is a duplicate candidate, and only its
/// statically bound sites still on the dispatch helper (invokespecial 1,
/// invokestatic 3) keep a recompile worth running.
#[test]
fn a_single_pass_body_is_a_duplicate_only_with_no_static_site_on_the_helper() {
    assert_eq!(c1_duplicate_unbound_statics(&body()), Some(0));
    let mut virtuals = body();
    virtuals._jit_invoke_infos.push(site(0));
    virtuals._jit_invoke_infos.push(site(2));
    assert_eq!(
        c1_duplicate_unbound_statics(&virtuals),
        Some(0),
        "virtual and interface sites are the inline caches' either way"
    );
    let mut statics = body();
    statics._jit_invoke_infos.push(site(3));
    statics._jit_invoke_infos.push(site(1));
    statics._jit_invoke_infos.push(site(0));
    assert_eq!(c1_duplicate_unbound_statics(&statics), Some(2));
    let mut optimizing = body();
    optimizing.used_ir_backend = true;
    assert_eq!(c1_duplicate_unbound_statics(&optimizing), None);
    let mut wrapped = body();
    wrapped.requires_wrapped_entry = true;
    assert_eq!(c1_duplicate_unbound_statics(&wrapped), None);
    let retired = body();
    retired
        .retired
        .store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(c1_duplicate_unbound_statics(&retired), None);
    let superseded = body();
    superseded
        .superseded
        .store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(c1_duplicate_unbound_statics(&superseded), None);
}
