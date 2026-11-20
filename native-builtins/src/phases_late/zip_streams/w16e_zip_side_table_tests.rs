use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::MethodCallResult;

thread_local! {
    static FALLBACK_SAW_LOCK_FREE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
    /// `(old, new)` address of the object the hook moves.
    static MOVE: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// Two VMs, the same identity hash: two rows, and one VM's teardown drops
/// only its own. The key used to be the bare hash, so the second VM's
/// stream read (and closed) the first VM's row.
#[test]
fn two_vms_with_equal_identity_hashes_keep_their_own_rows() {
    const VM_A: usize = 0xE16_0401;
    const VM_B: usize = 0xE16_0402;
    struct LockKeyTeardown;
    impl Drop for LockKeyTeardown {
        fn drop(&mut self) {
            crate::forget_vm_lock_keys(VM_A);
            crate::forget_vm_lock_keys(VM_B);
        }
    }
    let _teardown = LockKeyTeardown;
    let mut a = mock_ctx();
    a.set_vm_identity(VM_A);
    let b = mock_ctx();
    b.set_vm_identity(VM_B);
    let obj = a.fresh_object_ref();
    assert_eq!(
        a.identity_hash_code(obj),
        b.identity_hash_code(obj),
        "same object, same identity hash"
    );
    let key_a = zo_buf_key(&a, obj);
    let key_b = zo_buf_key(&b, obj);
    assert_ne!(key_a.0, key_b.0, "the VM is part of the key");
    // gc-common w29-c: and each VM mints its own weak lock key.
    assert_ne!(key_a.1, key_b.1, "each VM keys the object itself");

    zo_bufs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key_a, b"from A".to_vec());
    zo_bufs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key_b, b"from B".to_vec());
    assert!(forget_vm_zip_side_tables(VM_A) >= 1);
    {
        let bufs = zo_bufs().lock().unwrap_or_else(|e| e.into_inner());
        assert!(!bufs.contains_key(&key_a), "A's row goes with A");
        assert_eq!(bufs.get(&key_b).map(Vec::as_slice), Some(&b"from B"[..]));
    }
    forget_vm_zip_side_tables(VM_B);
    assert!(!zo_bufs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key_b));
}

fn record_lock_state_on_read(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "read" {
        return None;
    }
    // Another test may hold the table lock for a moment; the defect held
    // it on THIS thread for the whole call, so no attempt could succeed.
    let free = (0..10_000).any(|_| {
        let got = inflater_fast_states().try_lock().is_ok();
        if !got {
            std::thread::yield_now();
        }
        got
    });
    FALLBACK_SAW_LOCK_FREE.with(|c| c.set(Some(free)));
    Some(Ok(Some(Value::Int(-1))))
}

/// A stream with no fast-path row falls back to its bytecode `read` with
/// the process-wide table lock RELEASED. It used to be held across the
/// whole Java call.
#[test]
fn the_bytecode_read_fallback_runs_without_the_table_lock() {
    FALLBACK_SAW_LOCK_FREE.with(|c| c.set(None));
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xE16_0403);
    ctx.set_invoke_virtual_hook(record_lock_state_on_read);
    let this = ctx.fresh_object_ref();
    let target = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let r = native_inflater_input_stream_read(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(target)),
            Value::Int(0),
            Value::Int(4),
        ],
    );
    assert_eq!(r.expect("the fallback answers"), Some(Value::Int(-1)));
    assert_eq!(
        FALLBACK_SAW_LOCK_FREE.with(std::cell::Cell::get),
        Some(true),
        "the fallback must run with the inflater table unlocked"
    );

    FALLBACK_SAW_LOCK_FREE.with(|c| c.set(None));
    let r = native_inflater_input_stream_read_byte(&mut ctx, &[Value::Object(Some(this))]);
    assert_eq!(r.expect("the fallback answers"), Some(Value::Int(-1)));
    assert_eq!(
        FALLBACK_SAW_LOCK_FREE.with(std::cell::Cell::get),
        Some(true)
    );
}

fn move_stream_on_write(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "write" {
        return None;
    }
    let (old, new) = MOVE.with(std::cell::Cell::get);
    ctx.remap_native_pin_addr_for_test(old, new);
    Some(Ok(None))
}

fn field(
    cid: cratonvm_types::ClassId,
    name: &str,
    slot_index: usize,
) -> cratonvm_native_api::FieldMetadata {
    cratonvm_native_api::FieldMetadata {
        name: name.to_string(),
        descriptor: "Ljava/lang/Object;".to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: cid,
        is_static: false,
    }
}

/// `DataOutputStream.writeInt` & co. over a sink with no `buf`: each byte
/// is a Java `write`, which here moves the stream. `written` must be
/// updated on the moved stream, not through the address taken before.
#[test]
fn primitive_writes_count_on_the_stream_their_writes_moved() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xE16_0404);
    let cid = ctx.ensure_class_initialized("w16e/DataOutShape").unwrap();
    ctx.set_declared_fields(cid, vec![field(cid, "out", 0), field(cid, "written", 1)]);
    let this = ctx.alloc_object(cid, 2);
    let moved = ctx.alloc_object(cid, 2);
    let sink = ctx.fresh_object_ref();
    for stream in [this, moved] {
        ctx.set_field(stream, 0, Value::Object(Some(sink)));
        ctx.set_field(stream, 1, Value::Long(5));
    }
    MOVE.with(|c| c.set((this.as_ptr() as usize, moved.as_ptr() as usize)));
    ctx.set_invoke_virtual_hook(move_stream_on_write);

    zip_output_write_primitive(&mut ctx, this, &[1, 2, 3]).expect("the writes succeed");

    assert_eq!(
        ctx.get_field(moved, 1),
        Value::Long(8),
        "counted on the moved stream"
    );
    assert_eq!(
        ctx.get_field(this, 1),
        Value::Long(5),
        "the vacated copy is untouched"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
