use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::{Cell, RefCell};

thread_local! {
    static MOVES: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
    static RESOURCE_CALLS: Cell<usize> = const { Cell::new(0) };
    static FALLBACK_CALL: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

/// `Locale` accessors answer `fr`; the FIRST `getResourceAsStream` moves
/// every object named in `MOVES` (as a collection inside the loader's
/// bytecode would) and serves nothing; `getFallbackLocale` records its
/// receiver and its `Locale` argument.
fn relocating_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "getLanguage" => Some(Ok(Some(Value::Object(Some(ctx.create_string("fr")))))),
        "getCountry" | "getVariant" => {
            Some(Ok(Some(Value::Object(Some(ctx.create_string(""))))))
        }
        "getResourceAsStream" => {
            let call = RESOURCE_CALLS.with(|c| {
                let n = c.get();
                c.set(n + 1);
                n
            });
            if call == 0 {
                for (old, new) in MOVES.with(|m| m.borrow().clone()) {
                    ctx.remap_native_pin_addr_for_test(old, new);
                }
            }
            Some(Ok(Some(Value::Object(None))))
        }
        "getFallbackLocale" => {
            let locale = match args.get(1) {
                Some(Value::Object(Some(l))) => l.as_ptr() as usize,
                _ => 0,
            };
            FALLBACK_CALL.with(|c| c.set(Some((receiver.as_ptr() as usize, locale))));
            Some(Ok(Some(Value::Object(None))))
        }
        _ => None,
    }
}

#[test]
fn get_fallback_locale_is_dispatched_on_the_moved_control_with_the_moved_locale() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xB12_0002);
    let locale_cid = ctx.ensure_class_initialized("java/util/Locale").unwrap();
    let loader_cid = ctx
        .ensure_class_initialized("java/lang/ClassLoader")
        .unwrap();
    let control_cid = ctx
        .ensure_class_initialized("java/util/ResourceBundle$Control")
        .unwrap();
    let locale = ctx.alloc_object(locale_cid, 4);
    let loader = ctx.alloc_object(loader_cid, 4);
    let control = ctx.alloc_object(control_cid, 4);
    let moved_locale = ctx.fresh_object_ref();
    let moved_control = ctx.fresh_object_ref();
    MOVES.with(|m| {
        *m.borrow_mut() = vec![
            (control.as_ptr() as usize, moved_control.as_ptr() as usize),
            (locale.as_ptr() as usize, moved_locale.as_ptr() as usize),
        ]
    });
    RESOURCE_CALLS.with(|c| c.set(0));
    FALLBACK_CALL.with(|c| c.set(None));
    ctx.set_invoke_virtual_hook(relocating_hook);
    let name = ctx.create_string("w12b.messages");

    // getBundle(String, Locale, ClassLoader, Control)
    let result = rb_get_bundle(
        &mut ctx,
        &[
            Value::Object(Some(name)),
            Value::Object(Some(locale)),
            Value::Object(Some(loader)),
            Value::Object(Some(control)),
        ],
    );

    assert!(
        matches!(result, Err(MethodCallFailed::ExceptionThrown(_))),
        "no candidate resolves, so getBundle throws MissingResourceException"
    );
    assert!(
        RESOURCE_CALLS.with(Cell::get) >= 2,
        "both `fr` candidates go through the explicit loader"
    );
    assert_eq!(
        FALLBACK_CALL.with(Cell::get),
        Some((
            moved_control.as_ptr() as usize,
            moved_locale.as_ptr() as usize
        )),
        "getFallbackLocale must reach the MOVED Control with the MOVED Locale"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every root is released");
}
