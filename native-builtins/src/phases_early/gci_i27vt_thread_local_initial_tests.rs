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

/// File `supplier` for `tl` the way `withInitial` does.
fn file_supplier(ctx: &mut MockNativeContext, vm: usize, tl: ObjectRef, supplier: ObjectRef) {
    let key = crate::gc_stable_weak_lock_key(&*ctx, tl).expect("key");
    let root = ctx.add_global_root(supplier);
    tl_side_tables().lock().suppliers.insert(
        key,
        TlSupplierRow {
            vm,
            root,
            fallback: supplier,
        },
    );
    ANY_TL_SUPPLIER.store(true, std::sync::atomic::Ordering::Release);
}

#[test]
fn gci_i27vt_the_helper_class_is_one_static_supplied_method() {
    let b = tl_initial_class_bytes();
    assert_eq!(&b[..8], &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52]);
    assert_eq!(u16::from_be_bytes([b[8], b[9]]), 20, "constant pool count");
    let parsed = cratonvm_reader::read_class(&b).expect("parses");
    assert_eq!(&*parsed.this_class, TL_INITIAL_CLASS);
    assert_eq!(parsed.methods.len(), 1);
    let m = &parsed.methods[0];
    assert_eq!(&*m.name, TL_INITIAL_METHOD);
    assert_eq!(&*m.descriptor, TL_INITIAL_DESC);
    assert!(m.is_static() && !m.is_native());
}

/// The tail call: the helper is invoked with `(tl, supplier)` while this
/// native's frame is declared transparent, its answer is `get()`'s, and
/// the native stores nothing itself (the helper's `tl.set` does).
#[test]
fn gci_i27vt_a_supplier_miss_tail_calls_the_helper() {
    std::thread::spawn(|| {
        const VM: usize = 0x27f7_0001;
        let _forget = Forget(VM);
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let tl = ctx.fresh_object_ref();
        let supplier = ctx.fresh_object_ref();
        file_supplier(&mut ctx, VM, tl, supplier);
        fn hook(
            _ctx: &mut MockNativeContext,
            class: &str,
            method: &str,
            desc: &str,
            args: &[Value],
        ) -> Option<MethodCallResult> {
            if class != TL_INITIAL_CLASS {
                return None;
            }
            assert_eq!((method, desc), (TL_INITIAL_METHOD, TL_INITIAL_DESC));
            assert_eq!(args.len(), 2);
            let transparent =
                cratonvm_native_api::continuation_pin::transparent_natives() as i32;
            Some(Ok(Some(Value::Int(transparent))))
        }
        ctx.set_invoke_hook(hook);
        let got = native_tl_get(&mut ctx, &[Value::Object(Some(tl))]).expect("get");
        assert_eq!(
            got,
            Some(Value::Int(1)),
            "the helper ran as one transparent tail call, its answer is get()'s"
        );
        assert_eq!(cratonvm_native_api::continuation_pin::transparent_natives(), 0);
        assert!(ctx.class_id_by_name(TL_INITIAL_CLASS).is_some(), "defined once");
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

/// `--real-jdk` (only with `CRATONVM_TL_INTRINSIC=1`) keeps the nested
/// call and defines no class.
#[test]
fn gci_i27vt_jdk_only_keeps_the_nested_call() {
    std::thread::spawn(|| {
        const VM: usize = 0x27f7_0002;
        let _forget = Forget(VM);
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        ctx.set_jdk_only(true);
        let tl = ctx.fresh_object_ref();
        let supplier = ctx.fresh_object_ref();
        file_supplier(&mut ctx, VM, tl, supplier);
        assert!(!tl_initial_tail_call_allowed(&ctx));
        let _ = native_tl_get(&mut ctx, &[Value::Object(Some(tl))]).expect("get");
        assert!(ctx.class_id_by_name(TL_INITIAL_CLASS).is_none());
        assert_eq!(release_current_thread_locals(&mut ctx), 1, "stored here");
    })
    .join()
    .expect("test thread must not panic");
}

/// The miss path asks for the tail call before its nested call.
#[test]
fn gci_i27vt_the_tail_call_comes_before_the_nested_call() {
    const SRC: &str = concat!(include_str!("../phases_early/mod.rs"), "\n", include_str!("../phases_early/sis_read_deadline.rs"), "\n", include_str!("../phases_early/take_inherited_for_thread_object.rs"), "\n", include_str!("../phases_early/register_qname_natives.rs"), "\n", include_str!("../phases_early/initialize_real_thread_pool_executor.rs"), "\n", include_str!("../phases_early/register_phase52_byte_order.rs"), "\n", include_str!("../phases_early/register_synthetic_socket_stubs.rs"), "\n", include_str!("../phases_early/p54_huc_do_request_body.rs"));
    let start = SRC.find("fn native_tl_get(").expect("the native");
    let len = SRC[start..].find("\n\u{7d}\n").expect("its end");
    let body = &SRC[start..start + len];
    let tail = body
        .find("tl_supplied_initial_tail_call(ctx, this, s)")
        .expect("the tail call");
    let nested = body
        .find("ctx.invoke_virtual(s, \"get\"")
        .expect("the nested call");
    assert!(tail < nested);
}
