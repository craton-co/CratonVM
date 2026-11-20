use super::*;
use crate::test_utils::mock_ctx;
// `alloc_object` lives on `NativeHeapAccess`, one of the traits
// `NativeContext` composes -- so importing `NativeContext` alone does NOT
// bring it into scope on a concrete `MockNativeContext`. This is the same
// import block every other test module in the crate uses; see
// `lang_class.rs`'s `mod tests`.
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

/// The VM identity every registry test in this module runs as.
const HOOK_TEST_VM: usize = 0xE16_0301;

fn hook_ctx() -> crate::test_utils::MockNativeContext {
    let ctx = mock_ctx();
    ctx.set_vm_identity(HOOK_TEST_VM);
    ctx
}

/// Serialises every test in this module against their shared registry
/// row. Poisoning is ignored on purpose: a panicking test must not turn
/// the rest of the module red for the wrong reason.
fn hook_registry_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Put the test VM's row back into the state a fresh VM has.
fn reset_registry() {
    SHUTDOWN_STATE.forget(HOOK_TEST_VM);
}

fn hook_count() -> usize {
    registered_shutdown_hooks(HOOK_TEST_VM).len()
}

/// Unwrap to the `RuntimeError` and assert on the VARIANT and the message
/// STRING, never on `format!("{failed:?}")`.
///
/// Two reasons, and the first one is a trap this test module fell into on
/// its first draft: `Debug` escapes the embedded quotes, so
/// `HOOK_MSG_NULL_ADD` — which contains `"java.lang.Thread.isAlive()"` —
/// never appears verbatim in a `Debug` rendering and a `.contains()` check
/// against it silently fails no matter what the VM does. The second is
/// that `Debug` of `Option<String>` is the only way to tell
/// `getMessage() == null` from `getMessage() == ""` here, and asserting on
/// the `Option` itself says it directly.
fn runtime_error(failed: MethodCallFailed) -> RuntimeError {
    match failed {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(e)) => e,
        other => panic!("expected a RuntimeError, got {other:?}"),
    }
}

// -- the four messages, transcribed --------------------------------

#[test]
fn refusal_messages_are_the_measured_hotspot_text() {
    assert_eq!(HOOK_MSG_SHUTDOWN_IN_PROGRESS, "Shutdown in progress");
    assert_eq!(HOOK_MSG_ALREADY_RUNNING, "Hook already running");
    assert_eq!(HOOK_MSG_PREVIOUSLY_REGISTERED, "Hook previously registered");
    assert_eq!(
        HOOK_MSG_NULL_ADD,
        "Cannot invoke \"java.lang.Thread.isAlive()\" because \"hook\" is null"
    );
}

/// HANDOFF-20260814 §7: a non-ASCII byte in a compared string makes the
/// oracle comparison depend on the Windows console code page, and one
/// differential once failed on a single em-dash with every assertion
/// passing. These four strings cross that boundary, so they are checked
/// rather than eyeballed.
#[test]
fn refusal_messages_are_pure_ascii() {
    for m in [
        HOOK_MSG_SHUTDOWN_IN_PROGRESS,
        HOOK_MSG_NULL_ADD,
        HOOK_MSG_ALREADY_RUNNING,
        HOOK_MSG_PREVIOUSLY_REGISTERED,
    ] {
        assert!(m.is_ascii(), "non-ASCII in a compared message: {m:?}");
    }
}

// -- argument decoding ---------------------------------------------

/// A present-but-null slot 1 is a JAVA null and must reach the NPE arm; a
/// short vector is a VM-side decode fault and must NOT be turned into one.
/// Before G11-1 both were the same silent `Ok(None)`.
#[test]
fn a_java_null_and_a_short_vector_decode_differently() {
    let mut ctx = mock_ctx();
    let receiver = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
    let hook = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);

    assert_eq!(
        shutdown_hook_argument(
            &[Value::Object(Some(receiver)), Value::Object(Some(hook))],
            "t"
        ),
        Some(Some(hook)),
        "slot 1 holding a reference is the hook"
    );
    assert_eq!(
        shutdown_hook_argument(&[Value::Object(Some(receiver)), Value::Object(None)], "t"),
        Some(None),
        "slot 1 holding null is a Java null, not a decode failure"
    );
    assert_eq!(
        shutdown_hook_argument(&[Value::Object(Some(receiver))], "t"),
        None,
        "a one-slot vector is not [receiver, Thread] and must not be decided"
    );
    assert_eq!(
        shutdown_hook_argument(&[], "t"),
        None,
        "an empty vector likewise"
    );
    assert_eq!(
        shutdown_hook_argument(&[Value::Object(Some(receiver)), Value::Int(7)], "t"),
        None,
        "slot 1 holding a primitive is not a Thread reference"
    );
}

// -- the refusal ladder --------------------------------------------

#[test]
fn null_add_throws_npe_with_the_helpful_message() {
    let _g = hook_registry_guard();
    reset_registry();
    let mut ctx = hook_ctx();

    let err = shutdown_hook_add(&mut ctx, None).expect_err("null must be refused");
    match runtime_error(err) {
        RuntimeError::NullPointerException { message } => assert_eq!(
            message.as_deref(),
            Some(HOOK_MSG_NULL_ADD),
            "add's NPE must carry HotSpot's helpful message verbatim"
        ),
        other => panic!("expected NullPointerException, got {other:?}"),
    }
    assert_eq!(hook_count(), 0, "a refused hook must not be registered");

    reset_registry();
}

/// `remove`'s NPE is a bare `new NullPointerException()` (pc 20), so its
/// `getMessage()` is null — not the empty string, and not `add`'s helpful
/// text. MEASURED: `NULL-REMOVE threw=java.lang.NullPointerException
/// msg=null`.
#[test]
fn null_remove_throws_npe_with_no_message_at_all() {
    let _g = hook_registry_guard();
    reset_registry();
    let mut ctx = hook_ctx();

    let err = shutdown_hook_remove(&mut ctx, None).expect_err("null must be refused");
    match runtime_error(err) {
        // `None`, NOT `Some("")`. `msg=null` and `msg=` are different
        // cells and the differential prints them differently.
        RuntimeError::NullPointerException { message } => assert_eq!(
            message, None,
            "remove's NPE is a no-arg `new NullPointerException()` (pc 20)"
        ),
        other => panic!("expected NullPointerException, got {other:?}"),
    }

    reset_registry();
}

/// The JDK tests `hooks == null` at pc 0, BEFORE it touches the argument
/// at pc 16/17. So a null hook offered during shutdown gets the ISE, not
/// the NPE — on both methods. This is why the null check lives inside
/// `shutdown_hook_add`/`_remove` rather than at the registration site.
#[test]
fn shutdown_in_progress_outranks_the_null_check() {
    let _g = hook_registry_guard();
    reset_registry();
    SHUTDOWN_STATE.with(HOOK_TEST_VM, |s| s.in_progress = true);
    let mut ctx = hook_ctx();

    let added = shutdown_hook_add(&mut ctx, None).expect_err("must refuse");
    match runtime_error(added) {
        RuntimeError::IllegalStateException { message } => {
            assert_eq!(message, HOOK_MSG_SHUTDOWN_IN_PROGRESS)
        }
        other => panic!("add during shutdown must answer ISE first, got {other:?}"),
    }

    let removed = shutdown_hook_remove(&mut ctx, None).expect_err("must refuse");
    match runtime_error(removed) {
        RuntimeError::IllegalStateException { message } => {
            assert_eq!(message, HOOK_MSG_SHUTDOWN_IN_PROGRESS)
        }
        other => panic!("remove during shutdown must answer ISE first, got {other:?}"),
    }

    reset_registry();
}

/// The positive path plus the duplicate refusal, in one test because the
/// second only means anything after the first: registering twice must
/// throw, and the registry must still hold exactly one entry afterwards.
/// The two `remove` rows are `RShutdownHooks`' first two checks, MEASURED
/// on the oracle as `REMOVE-1 true` / `REMOVE-2 false`.
#[test]
fn a_second_registration_of_the_same_hook_throws_and_does_not_grow_the_list() {
    let _g = hook_registry_guard();
    reset_registry();
    let mut ctx = hook_ctx();
    let hook = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);

    shutdown_hook_add(&mut ctx, Some(hook)).expect("first registration must be accepted");
    assert_eq!(hook_count(), 1);

    let err = shutdown_hook_add(&mut ctx, Some(hook)).expect_err("duplicate must be refused");
    match runtime_error(err) {
        RuntimeError::IllegalArgumentException { message } => {
            assert_eq!(message, HOOK_MSG_PREVIOUSLY_REGISTERED)
        }
        other => panic!("expected the duplicate IAE, got {other:?}"),
    }
    assert_eq!(
        hook_count(),
        1,
        "a refused registration must not leave a second entry"
    );

    assert!(shutdown_hook_remove(&mut ctx, Some(hook)).expect("must not throw"));
    assert!(!shutdown_hook_remove(&mut ctx, Some(hook)).expect("must not throw"));
    assert_eq!(hook_count(), 0);

    reset_registry();
}

/// gc-common w16-e: the registry is per VM. A hook one VM registers is not
/// in another VM's list, one VM's shutdown does not refuse another VM's
/// registration, and teardown drops the VM's row.
#[test]
fn one_vms_shutdown_neither_blocks_nor_sees_another_vms_registry() {
    const OTHER_VM: usize = 0xE16_0302;
    let _g = hook_registry_guard();
    reset_registry();
    SHUTDOWN_STATE.forget(OTHER_VM);
    let mut a = hook_ctx();
    let mut b = mock_ctx();
    b.set_vm_identity(OTHER_VM);

    let hook_a = a.alloc_object(cratonvm_types::ClassId::new(0), 4);
    shutdown_hook_add(&mut a, Some(hook_a)).expect("VM A registers its hook");
    assert_eq!(hook_count(), 1);
    assert!(
        registered_shutdown_hooks(OTHER_VM).is_empty(),
        "VM A's hook must not be in VM B's list"
    );

    // VM A's shutdown begins.
    SHUTDOWN_STATE.with(HOOK_TEST_VM, |s| s.in_progress = true);
    let refused = shutdown_hook_add(&mut a, None).expect_err("A is shutting down");
    assert!(matches!(
        runtime_error(refused),
        RuntimeError::IllegalStateException { .. }
    ));
    let hook_b = b.alloc_object(cratonvm_types::ClassId::new(0), 4);
    shutdown_hook_add(&mut b, Some(hook_b))
        .expect("VM A's shutdown must not refuse VM B's registration");
    assert_eq!(registered_shutdown_hooks(OTHER_VM).len(), 1);

    // Teardown drops the row.
    forget_vm_system_singletons(OTHER_VM);
    assert!(registered_shutdown_hooks(OTHER_VM).is_empty());
    assert!(!SHUTDOWN_STATE.has_row(OTHER_VM));

    reset_registry();
}
