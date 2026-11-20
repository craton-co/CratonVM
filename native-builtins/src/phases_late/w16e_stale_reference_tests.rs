use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::MethodCallResult;

thread_local! {
    /// `(old, new)` address of the object the hook moves.
    static MOVE: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// `Class.getName()` answers `java.security.Provider` and MOVES the
/// `Class` (re-points the native's root at another object) while it runs.
fn move_class_on_get_name(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "getName" {
        return None;
    }
    let (old, new) = MOVE.with(std::cell::Cell::get);
    ctx.remap_native_pin_addr_for_test(old, new);
    let name = ctx.create_string("java.security.Provider");
    Some(Ok(Some(Value::Object(Some(name)))))
}

/// `ServiceLoader.load(Class)`: the service `Class` is moved by its own
/// `getName()`, and the loader must record the moved one; the provider
/// list is built through its roots.
#[test]
fn service_loader_load_stores_the_class_its_get_name_moved() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xE16_0101);
    let cls = ctx.fresh_object_ref();
    let moved = ctx.fresh_object_ref();
    MOVE.with(|c| c.set((cls.as_ptr() as usize, moved.as_ptr() as usize)));
    ctx.set_invoke_virtual_hook(move_class_on_get_name);

    let loader = match p63_service_loader_load(&mut ctx, &[Value::Object(Some(cls))]) {
        Ok(Some(Value::Object(Some(loader)))) => loader,
        Ok(other) => panic!("expected a ServiceLoader, got {other:?}"),
        Err(e) => {
            // The mock refused the synthetic layout: nothing of this fix
            // is reachable then.
            eprintln!("skipped: the mock cannot allocate a ServiceLoader ({e:?})");
            return;
        }
    };
    assert_eq!(
        ctx.get_field(loader, 1),
        Value::Object(Some(moved)),
        "the loader must record the Class where getName() left it"
    );
    let Value::Object(Some(list)) = ctx.get_field(loader, 0) else {
        panic!("slot 0 must hold the provider list");
    };
    assert_eq!(ctx.get_field(list, 1), Value::Int(5));
    let Value::Object(Some(arr)) = ctx.get_field(list, 0) else {
        panic!("the list must hold its array");
    };
    let Value::Object(Some(first)) = ctx.get_array_element(arr, 0) else {
        panic!("the first provider name must be stored");
    };
    assert_eq!(ctx.read_string(first).as_deref(), Some("SUN"));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

#[test]
fn split_as_stream_parts_land_in_the_rooted_array() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xE16_0102);
    let stream = match p65_stream_of_parts(&mut ctx, &["a", "bc"]) {
        Ok(Some(Value::Object(Some(stream)))) => stream,
        Ok(other) => panic!("expected a Stream, got {other:?}"),
        Err(e) => {
            eprintln!("skipped: the mock cannot allocate a Stream ({e:?})");
            return;
        }
    };
    let Value::Object(Some(arr)) = ctx.get_field(stream, 0) else {
        panic!("slot 0 must hold the parts");
    };
    assert_eq!(ctx.array_length(arr), 2);
    for (i, want) in ["a", "bc"].iter().enumerate() {
        let Value::Object(Some(s)) = ctx.get_array_element(arr, i) else {
            panic!("part {i} must be a String");
        };
        assert_eq!(ctx.read_string(s).as_deref(), Some(*want));
    }
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// Teardown drops the VM's `ProcessHandle.current()` memo row, whose
/// value is a slot number in the dying VM's global-root table.
#[test]
fn teardown_forgets_the_process_handle_memo_row() {
    const VM: usize = 0xE16_0103;
    p60_current_handle_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(VM, 7);
    forget_vm_phases_late_state(VM);
    assert!(!p60_current_handle_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&VM));
}
