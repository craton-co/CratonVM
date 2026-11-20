use super::*;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use std::sync::Arc;

fn test_shared() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(crate::VmConfig::default()))
}

fn forward_header(from: ObjectRef, to: ObjectRef) {
    unsafe {
        (*(from.as_ptr() as *const cratonvm_types::ObjectHeader))
            .set_forwarding_address(to.as_ptr());
    }
}

#[test]
fn class_id_of_object_and_heap_queries_forward_stale_object_ref() {
    let shared = test_shared();
    let tid = ThreadId(0x8801);
    let mut thread = JvmThread::new(tid, "boundary-fwd-class-id");

    let from_obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let to_obj = shared.mem.heap.alloc_object(ClassId::new(7), 3);
    forward_header(from_obj, to_obj);

    let ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    assert_eq!(ctx.class_id_of_object(from_obj), ClassId::new(7));
    assert_eq!(ctx.object_num_fields(from_obj), 3);
    assert_eq!(ctx.heap_kind_of(from_obj), ObjectKind::Object);
}

#[test]
fn read_string_and_string_helpers_forward_stale_string_and_backing_array() {
    let shared = test_shared();
    let tid = ThreadId(0x8802);
    let mut thread = JvmThread::new(tid, "boundary-fwd-string");

    let real_str = crate::vm::create_java_string_uninterned(&shared, "MVStoreTx");
    let stale_str = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    forward_header(stale_str, real_str);

    let ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    assert_eq!(ctx.read_string(stale_str).as_deref(), Some("MVStoreTx"));
    assert_eq!(
        ctx.read_string_units(stale_str),
        ctx.read_string_units(real_str)
    );
    assert_eq!(
        ctx.java_string_hash_code(stale_str),
        ctx.java_string_hash_code(real_str)
    );
    assert_eq!(ctx.java_strings_equal(stale_str, real_str), Some(true));
}

#[test]
fn set_field_volatile_and_compare_and_swap_field_forward_values_and_receiver() {
    let shared = test_shared();
    let tid = ThreadId(0x8803);
    let mut thread = JvmThread::new(tid, "boundary-fwd-volatile-cas");

    let holder_from = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let holder_to = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    forward_header(holder_from, holder_to);

    let val_a_from = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let val_a_to = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    forward_header(val_a_from, val_a_to);

    let val_b_from = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let val_b_to = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    forward_header(val_b_from, val_b_to);

    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    // Volatile store with stale receiver AND stale value must store val_a_to into holder_to.
    ctx.set_field_volatile(holder_from, 0, Value::Object(Some(val_a_from)));
    assert_eq!(
        ctx.get_field_volatile(holder_to, 0),
        Value::Object(Some(val_a_to))
    );

    // CAS with stale receiver, stale expected, and stale new_val must succeed and store val_b_to.
    assert!(ctx.compare_and_swap_field(
        holder_from,
        0,
        Value::Object(Some(val_a_from)),
        Value::Object(Some(val_b_from)),
    ));
    assert_eq!(
        ctx.get_field_volatile(holder_to, 0),
        Value::Object(Some(val_b_to))
    );
}

#[test]
fn bulk_array_accessors_forward_stale_array_refs() {
    let shared = test_shared();
    let tid = ThreadId(0x8804);
    let mut thread = JvmThread::new(tid, "boundary-fwd-bulk-array");

    let arr_from = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 4);
    let arr_to = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 4);
    forward_header(arr_from, arr_to);

    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    assert!(ctx.write_byte_array_from(arr_from, 0, &[10, 20, 30, 40]));
    let mut out = [0u8; 4];
    assert_eq!(ctx.read_byte_array_into(arr_from, 0, &mut out), 4);
    assert_eq!(out, [10, 20, 30, 40]);
    assert_eq!(ctx.read_byte_array_into(arr_to, 0, &mut out), 4);
    assert_eq!(out, [10, 20, 30, 40]);
}

#[test]
fn finish_jni_onload_rejects_err_and_rolls_back_state() {
    let _ = crate::native::jni::take_jni_pending_exception();
    let shared = test_shared();
    let tid = ThreadId(0x8805);
    let mut thread = JvmThread::new(tid, "jni-onload-validation");

    let baseline = shared.natives.jni_native_methods.read().clone();
    assert_eq!(
        finish_jni_onload(
            &shared,
            &mut thread,
            crate::native::jni::JNI_VERSION_1_8,
            "/tmp/libok.so",
            false,
            baseline.clone(),
        )
        .unwrap(),
        false
    );

    shared
        .natives
        .netty_tcnative_real
        .store(true, std::sync::atomic::Ordering::Release);
    shared.natives.native_methods.mute_netty_tcnative_stubs();
    crate::native::jni::register_jni_native(
        &shared.natives,
        "dev/ludovic/netlib/blas/JNIBLAS",
        "dgemm",
        "()V",
        0xdead_beef,
    );
    let err = finish_jni_onload(
        &shared,
        &mut thread,
        crate::native::jni::JNI_ERR,
        "/tmp/libnetlib.so",
        true,
        baseline,
    )
    .expect_err("JNI_ERR must fail JNI_OnLoad");
    match err {
        crate::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                crate::error::RuntimeError::UnsatisfiedLinkError { message },
            ),
        ) => {
            assert_eq!(
                message,
                "unsupported JNI version 0xFFFFFFFF required by /tmp/libnetlib.so"
            );
        }
        other => panic!("expected UnsatisfiedLinkError, got {other:?}"),
    }
    assert!(
        !shared
            .natives
            .netty_tcnative_real
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert!(!shared.natives.native_methods.netty_tcnative_stubs_muted());
    assert_eq!(
        crate::native::jni::find_jni_native_in(
            &shared.natives.jni_native_methods,
            "dev/ludovic/netlib/blas/JNIBLAS",
            "dgemm",
            "()V",
        ),
        None,
        "RegisterNatives entry from failed JNI_OnLoad must be rolled back"
    );
}
