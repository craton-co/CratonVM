use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

struct Forget(usize);
impl Drop for Forget {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
        forget_vm_thread_local_side_tables(self.0);
    }
}

fn method(
    cid: cratonvm_types::ClassId,
    name: &str,
    desc: &str,
) -> cratonvm_native_api::MethodMetadata {
    cratonvm_native_api::MethodMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: 0x0004,
        declaring_class_id: cid,
        exceptions: Vec::new(),
        signature: None,
    }
}

/// An instance of `app/Local extends <base>` declaring `methods`.
fn local(ctx: &mut MockNativeContext, base: &str, methods: &[(&str, &str)]) -> ObjectRef {
    let base_cid = ctx.ensure_class_initialized(base).expect("base");
    let cid = ctx.ensure_class_initialized("app/Local").expect("sub");
    ctx.set_superclass(cid, base_cid);
    ctx.set_declared_methods(cid, methods.iter().map(|(n, d)| method(cid, n, d)).collect());
    ctx.alloc_object(cid, 4)
}

fn hook(
    _ctx: &mut MockNativeContext,
    class: &str,
    method: &str,
    desc: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if class != TL_BOOT_INITIAL_CLASS {
        return None;
    }
    assert_eq!((method, desc), (TL_BOOT_INITIAL_METHOD, TL_BOOT_INITIAL_DESC));
    assert_eq!(args.len(), 1);
    let transparent = cratonvm_native_api::continuation_pin::transparent_natives() as i32;
    Some(Ok(Some(Value::Int(transparent))))
}

#[test]
fn gci_i28tl_the_boot_helper_is_one_static_initial_method() {
    let b = tl_boot_initial_class_bytes();
    assert_eq!(&b[..8], &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52]);
    assert_eq!(u16::from_be_bytes([b[8], b[9]]), 18, "constant pool count");
    let parsed = cratonvm_reader::read_class(&b).expect("parses");
    assert_eq!(&*parsed.this_class, TL_BOOT_INITIAL_CLASS);
    assert!(TL_BOOT_INITIAL_CLASS.starts_with("java/lang/"), "same runtime package");
    assert_eq!(parsed.methods.len(), 1);
    let m = &parsed.methods[0];
    assert_eq!(&*m.name, TL_BOOT_INITIAL_METHOD);
    assert_eq!(&*m.descriptor, TL_BOOT_INITIAL_DESC);
    assert!(m.is_static() && !m.is_native());
}

/// An `initialValue()` override: the helper runs as one transparent tail
/// call with `(tl)`, defined in the boot namespace, and the native stores
/// nothing itself.
#[test]
fn gci_i28tl_an_initial_value_override_tail_calls_the_boot_helper() {
    std::thread::spawn(|| {
        const VM: usize = 0x28f1_0001;
        let _forget = Forget(VM);
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let tl = local(
            &mut ctx,
            "java/lang/ThreadLocal",
            &[("initialValue", "()Ljava/lang/Object;")],
        );
        assert_eq!(tl_override_scan(&ctx, tl), Some((true, false)));
        ctx.set_invoke_hook(hook);
        let got = native_tl_get(&mut ctx, &[Value::Object(Some(tl))]).expect("get");
        assert_eq!(got, Some(Value::Int(1)), "one transparent tail call answers get()");
        assert_eq!(cratonvm_native_api::continuation_pin::transparent_natives(), 0);
        assert!(ctx.class_id_by_name(TL_BOOT_INITIAL_CLASS).is_some(), "defined");
        let opts = ctx.last_define_full_opts().expect("define_class_full");
        assert!(opts.bootstrap_namespace, "the boot loader: initialValue is protected");
        let hash = tl_bucket_hash(&ctx, tl);
        assert_eq!(
            TL_MAP.with(|m| m.borrow().get(&hash).map_or(0, Vec::len)),
            0,
            "the native stores nothing: the helper's tl.set does"
        );
    })
    .join()
    .expect("test thread must not panic");
}

/// An `InheritableThreadLocal` subclass takes the same arm.
#[test]
fn gci_i28tl_an_inheritable_override_takes_the_helper_too() {
    std::thread::spawn(|| {
        const VM: usize = 0x28f1_0002;
        let _forget = Forget(VM);
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let tl = local(
            &mut ctx,
            "java/lang/InheritableThreadLocal",
            &[("initialValue", "()Ljava/lang/Object;")],
        );
        assert_eq!(tl_override_scan(&ctx, tl), Some((true, false)));
    })
    .join()
    .expect("test thread must not panic");
}

/// An overriding `set` (`TerminatingThreadLocal`'s shape) keeps the nested
/// call: `setInitialValue` never calls `this.set`.
#[test]
fn gci_i28tl_a_set_override_keeps_the_nested_call() {
    std::thread::spawn(|| {
        const VM: usize = 0x28f1_0003;
        let _forget = Forget(VM);
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let tl = local(
            &mut ctx,
            "java/lang/ThreadLocal",
            &[
                ("initialValue", "()Ljava/lang/Object;"),
                ("set", "(Ljava/lang/Object;)V"),
            ],
        );
        assert_eq!(tl_override_scan(&ctx, tl), Some((true, true)));
        assert!(tl_overridden_initial_tail_call(&mut ctx, tl).is_none());
        assert!(ctx.class_id_by_name(TL_BOOT_INITIAL_CLASS).is_none());
    })
    .join()
    .expect("test thread must not panic");
}

/// No override (the base `initialValue` answers `null` at once), an
/// unnamed class, and `--real-jdk` define nothing.
#[test]
fn gci_i28tl_no_override_unknown_class_or_jdk_only_define_nothing() {
    std::thread::spawn(|| {
        const VM: usize = 0x28f1_0004;
        let _forget = Forget(VM);
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let plain = local(&mut ctx, "java/lang/ThreadLocal", &[]);
        assert_eq!(tl_override_scan(&ctx, plain), Some((false, false)));
        assert!(tl_overridden_initial_tail_call(&mut ctx, plain).is_none());
        let unnamed = ctx.fresh_object_ref();
        assert_eq!(tl_override_scan(&ctx, unnamed), None);
        assert!(tl_overridden_initial_tail_call(&mut ctx, unnamed).is_none());
        ctx.set_jdk_only(true);
        let tl = local(
            &mut ctx,
            "java/lang/ThreadLocal",
            &[("initialValue", "()Ljava/lang/Object;")],
        );
        assert!(tl_overridden_initial_tail_call(&mut ctx, tl).is_none());
        assert!(ctx.class_id_by_name(TL_BOOT_INITIAL_CLASS).is_none());
    })
    .join()
    .expect("test thread must not panic");
}

/// The supplier arm still comes first; the override arm only without a
/// supplier, before the nested call.
#[test]
fn gci_i28tl_the_override_arm_is_asked_only_without_a_supplier() {
    const SRC: &str = concat!(include_str!("../phases_early/mod.rs"), "\n", include_str!("../phases_early/sis_read_deadline.rs"), "\n", include_str!("../phases_early/take_inherited_for_thread_object.rs"), "\n", include_str!("../phases_early/register_qname_natives.rs"), "\n", include_str!("../phases_early/initialize_real_thread_pool_executor.rs"), "\n", include_str!("../phases_early/register_phase52_byte_order.rs"), "\n", include_str!("../phases_early/register_synthetic_socket_stubs.rs"), "\n", include_str!("../phases_early/p54_huc_do_request_body.rs"));
    let start = SRC.find("fn native_tl_get(").expect("the native");
    let len = SRC[start..].find("\n\u{7d}\n").expect("its end");
    let body = &SRC[start..start + len];
    let supplied = body
        .find("tl_supplied_initial_tail_call(ctx, this, s)")
        .expect("supplier arm");
    let overridden = body
        .find("else if let Some(answer) = tl_overridden_initial_tail_call(ctx, this)")
        .expect("override arm");
    let nested = body
        .find("ctx.invoke_virtual(this, \"initialValue\"")
        .expect("nested");
    assert!(supplied < overridden && overridden < nested);
}
