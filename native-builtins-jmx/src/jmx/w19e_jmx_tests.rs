//! gc-common w19-e: `object_name_key` dispatched `toString()` on the name's
//! address from before `getCanonicalName()` (Java), and the MBeanServer /
//! ObjectName builders wrote through addresses from before their own
//! allocations.
use super::*;
use cratonvm_native_builtins::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old, new): the name `getCanonicalName()` moves.
    static W19E_NAME_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// `getCanonicalName()` moves the name and answers nothing usable, so the
/// key falls through to `toString()`, which answers by where it was
/// dispatched: the moved copy or the vacated one.
fn w19e_canonical_name_moves_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    let (old, new) = W19E_NAME_MOVE.with(|m| m.get());
    match method_name {
        "getCanonicalName" => {
            ctx.remap_native_pin_addr_for_test(old, new);
            Some(Ok(None))
        }
        "toString" => {
            let text = if addr(receiver) == new {
                "w19e:type=Moved"
            } else {
                "w19e:type=Vacated"
            };
            let s = ctx.create_string(text);
            Some(Ok(Some(Value::Object(Some(s)))))
        }
        _ => None,
    }
}

#[test]
fn w19e_object_name_key_asks_the_moved_name() {
    let mut ctx = MockNativeContext::new();
    let name_old = ctx.fresh_object_ref();
    let name_new = ctx.fresh_object_ref();
    W19E_NAME_MOVE.with(|m| m.set((addr(name_old), addr(name_new))));
    ctx.set_invoke_virtual_hook(w19e_canonical_name_moves_hook);

    let key = object_name_key(&mut ctx, Some(name_old));
    W19E_NAME_MOVE.with(|m| m.set((0, 0)));

    assert_eq!(key, "w19e:type=Moved");
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

#[test]
fn w19e_built_mbean_server_and_object_name_are_complete_and_leave_no_pin() {
    let mut ctx = MockNativeContext::new();
    let server = alloc_mbean_server_with_domain(&mut ctx, Some("w19e")).unwrap();
    match ctx.get_field(server, MBS_DOMAIN) {
        Value::Object(Some(s)) => assert_eq!(ctx.read_string(s).as_deref(), Some("w19e")),
        other => panic!("the default domain must be stored, got {other:?}"),
    }
    assert!(matches!(ctx.get_field(server, MBS_COUNT), Value::Int(0)));
    for slot in [MBS_NAMES, MBS_BEANS, MBS_ONAMES] {
        match ctx.get_field(server, slot) {
            Value::Object(Some(arr)) => assert_eq!(ctx.array_length(arr), 0),
            other => panic!("registry slot {slot} must hold an empty array, got {other:?}"),
        }
    }

    let name = object_name_new(&mut ctx, "w19e:type=Probe".to_string()).unwrap();
    assert_eq!(object_name_text(&ctx, name), "w19e:type=Probe");
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
