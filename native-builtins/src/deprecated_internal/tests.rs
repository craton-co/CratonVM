use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Helper: look up a native and call it through the registry.
fn call_native(
    registry: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    class: &str,
    method: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    let cb = registry
        .find(class, method, desc)
        .unwrap_or_else(|| panic!("{class}.{method}{desc} should be registered"));
    cb(ctx, args)
}

fn make_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_deprecated_internal_natives(&mut r);
    r
}

// --- T8.3.1: Beans.instantiate ---

#[test]
fn test_beans_instantiate_success() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // Create a class name string
    let name = ctx.create_string("com.example.MyBean");
    // ClassLoader is null
    let result = call_native(
        &reg,
        &mut ctx,
        "java/beans/Beans",
        "instantiate",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;)Ljava/lang/Object;",
        &[Value::Object(None), Value::Object(Some(name))],
    );
    assert!(result.is_ok());
    let val = result.unwrap();
    assert!(matches!(val, Some(Value::Object(Some(_)))));
}

#[test]
fn test_beans_instantiate_null_name() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let result = call_native(
        &reg,
        &mut ctx,
        "java/beans/Beans",
        "instantiate",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;)Ljava/lang/Object;",
        &[Value::Object(None), Value::Object(None)],
    );
    assert!(result.is_err());
}

#[test]
fn test_beans_is_design_time() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let result = call_native(
        &reg,
        &mut ctx,
        "java/beans/Beans",
        "isDesignTime",
        "()Z",
        &[],
    );
    assert_eq!(result.unwrap(), Some(Value::Int(0)));
}

// --- T8.3.2: RemoteRef.getRefClass ---

#[test]
fn test_remote_ref_get_ref_class_empty() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let result = call_native(
        &reg,
        &mut ctx,
        "java/rmi/server/RemoteRef",
        "getRefClass",
        "(Ljava/io/ObjectOutput;)Ljava/lang/String;",
        &[Value::Object(None)],
    );
    let val = result.unwrap().unwrap();
    if let Value::Object(Some(obj)) = val {
        let s = ctx.read_string(obj).unwrap();
        assert_eq!(s, "");
    } else {
        panic!("expected string object");
    }
}

// --- T8.3.3: Activation classes load ---

#[test]
fn test_activatable_init_no_error() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let result = call_native(
        &reg,
        &mut ctx,
        "java/rmi/activation/Activatable",
        "<init>",
        "()V",
        &[],
    );
    assert!(result.is_ok());
}

/// `ActivationGroup.getSystem()` used to answer a constant null, which the
/// caller could only discover by NPE-ing on it several frames later. It now
/// throws like its `register`/`exportObject` siblings — `java.rmi.activation`
/// was removed in JDK 17 and there is no activation system to return.
#[test]
fn test_activation_group_get_system_throws() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let result = call_native(
        &reg,
        &mut ctx,
        "java/rmi/activation/ActivationGroup",
        "getSystem",
        "()Ljava/rmi/activation/ActivationSystem;",
        &[],
    );
    assert!(result.is_err(), "getSystem must not answer a silent null");
}

#[test]
fn test_activatable_register_throws() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let result = call_native(
        &reg,
        &mut ctx,
        "java/rmi/activation/Activatable",
        "register",
        "(Ljava/rmi/activation/ActivationDesc;)Ljava/rmi/activation/ActivationID;",
        &[Value::Object(None)],
    );
    assert!(result.is_err());
}

// --- T8.4.1: Unsafe.defineClass ---
// The `jdk.internal.misc` twin's CAFEBABE tests went with the twin (zt
// t14/df5); the one `Unsafe` door (`unsafe_natives::native_unsafe_define_class`)
// carries its own tests.

// --- T8.4.2: Unsafe memory lifecycle ---

#[test]
fn test_unsafe_memory_allocate_free() {
    let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // Allocate
    let result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "allocateMemory",
        "(J)J",
        &[Value::Object(None), Value::Long(1024)],
    );
    let addr = match result.unwrap() {
        Some(Value::Long(a)) => a,
        other => panic!("expected Long, got {:?}", other),
    };
    assert!(addr > 0);

    // Free
    let free_result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "freeMemory",
        "(J)V",
        &[Value::Object(None), Value::Long(addr)],
    );
    assert!(free_result.is_ok());
}

// SECURITY FIX (V5): the off-heap natives now resolve to the single
// bounds-checked *arena* store (base 0x10_0000_0000), not the old dual
// "tracked" store this test used to exercise. The arena's free is
// idempotent — use-after-free is caught by the bounds-checked accessors
// (here `setMemory`), not by making the second `freeMemory` throw. So we
// assert the LIVE invariant: after a free, the address is no longer in any
// live arena, and `setMemory` on it is rejected. A live address still
// accepts `setMemory`, proving the rejection is specific to the freed
// range (not a blanket failure). This keeps double-free / UAF detection
// under real test coverage against the consolidated store.
#[test]
fn test_unsafe_memory_double_free() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // Allocate
    let result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "allocateMemory",
        "(J)J",
        &[Value::Object(None), Value::Long(256)],
    );
    let addr = match result.unwrap() {
        Some(Value::Long(a)) => a,
        other => panic!("expected Long, got {:?}", other),
    };

    // While live, an off-heap setMemory into the arena succeeds.
    let set_live = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "setMemory",
        "(Ljava/lang/Object;JJB)V",
        &[
            Value::Object(None), // off-heap target
            Value::Object(None), // null base object => off-heap
            Value::Long(addr),
            Value::Long(16),
            Value::Int(0xAB),
        ],
    );
    assert!(set_live.is_ok(), "setMemory on a live arena must succeed");

    // First free succeeds.
    let free1 = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "freeMemory",
        "(J)V",
        &[Value::Object(None), Value::Long(addr)],
    );
    assert!(free1.is_ok());

    // Second free is idempotent in the consolidated arena store (the
    // address is simply gone) — it must not panic and is a no-op.
    let free2 = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "freeMemory",
        "(J)V",
        &[Value::Object(None), Value::Long(addr)],
    );
    assert!(free2.is_ok());

    // Use-after-free is the real invariant: the freed address is no longer
    // in any live arena, so a bounds-checked off-heap write is rejected.
    let set_freed = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "setMemory",
        "(Ljava/lang/Object;JJB)V",
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(addr),
            Value::Long(16),
            Value::Int(0xCD),
        ],
    );
    assert!(
        set_freed.is_err(),
        "writing to a freed off-heap address must be rejected (UAF detection)"
    );
}

// SECURITY FIX (V5): reallocateMemory now routes to the arena store. The
// arena resizes the block IN PLACE and keeps the same base address (a valid
// realloc outcome — the JDK contract only promises the returned pointer is
// usable, not that it differs). So the old `assert_ne!(new_addr, addr)`
// (a tracked-store artifact that always handed out a fresh address) no
// longer holds. We instead assert the meaningful realloc semantics against
// the live store: the reallocated region is valid and writable up to the
// NEW size, and realloc(NULL, size) behaves like a fresh allocation.
#[test]
fn test_unsafe_memory_realloc() {
    let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // Allocate 128 bytes.
    let result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "allocateMemory",
        "(J)J",
        &[Value::Object(None), Value::Long(128)],
    );
    let addr = match result.unwrap() {
        Some(Value::Long(a)) => a,
        other => panic!("expected Long, got {:?}", other),
    };
    assert!(addr > 0);

    // Realloc to 512.
    let realloc_result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "reallocateMemory",
        "(JJ)J",
        &[Value::Object(None), Value::Long(addr), Value::Long(512)],
    );
    let new_addr = match realloc_result.unwrap() {
        Some(Value::Long(a)) => a,
        other => panic!("expected Long, got {:?}", other),
    };
    assert!(new_addr > 0);

    // The grown region must be valid up to the NEW size: a write at the
    // far end of the 512-byte block must succeed (it would have been out
    // of bounds for the original 128-byte block).
    let set_grown = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "setMemory",
        "(Ljava/lang/Object;JJB)V",
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(new_addr + 256),
            Value::Long(128),
            Value::Int(0x7E),
        ],
    );
    assert!(
        set_grown.is_ok(),
        "the reallocated region must be writable up to the new size"
    );

    // realloc(NULL, size) == alloc(size): returns a fresh, distinct,
    // writable arena.
    let fresh = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "reallocateMemory",
        "(JJ)J",
        &[Value::Object(None), Value::Long(0), Value::Long(64)],
    );
    let fresh_addr = match fresh.unwrap() {
        Some(Value::Long(a)) => a,
        other => panic!("expected Long, got {:?}", other),
    };
    assert!(fresh_addr > 0);
    assert_ne!(
        fresh_addr, new_addr,
        "realloc(NULL) must hand out a distinct arena"
    );

    // Free the reallocated address.
    let free_result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "freeMemory",
        "(J)V",
        &[Value::Object(None), Value::Long(new_addr)],
    );
    assert!(free_result.is_ok());
}

#[test]
fn test_unsafe_memory_negative_size() {
    let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Unsafe",
        "allocateMemory",
        "(J)J",
        &[Value::Object(None), Value::Long(-1)],
    );
    assert!(result.is_err());
}

// --- T8.4.3: Reflection.getCallerClass ---

#[test]
fn test_reflection_get_caller_class_depth_zero() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // depth=0 should return Reflection.class itself
    let result = call_native(
        &reg,
        &mut ctx,
        "sun/reflect/Reflection",
        "getCallerClass",
        "(I)Ljava/lang/Class;",
        &[Value::Int(0)],
    );
    assert!(result.is_ok());
    assert!(matches!(result.unwrap(), Some(Value::Object(Some(_)))));
}

#[test]
fn test_reflection_get_caller_class_no_arg() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // No-arg form: in test context, stack is empty so returns null
    let result = call_native(
        &reg,
        &mut ctx,
        "sun/reflect/Reflection",
        "getCallerClass",
        "()Ljava/lang/Class;",
        &[],
    );
    assert!(result.is_ok());
    // Mock returns empty stack so null is expected
    assert_eq!(result.unwrap(), Some(Value::Object(None)));
}

// --- T8.4.4: Signal ---

#[test]
fn test_signal_name_to_number_mapping() {
    assert_eq!(signal_name_to_number("INT"), Some(2));
    assert_eq!(signal_name_to_number("TERM"), Some(15));
    assert_eq!(signal_name_to_number("HUP"), Some(1));
    assert_eq!(signal_name_to_number("KILL"), Some(9));
    assert_eq!(signal_name_to_number("SIGINT"), Some(2));
    assert_eq!(signal_name_to_number("BOGUS"), None);
}

#[test]
fn test_signal_number_to_name() {
    assert_eq!(signal_number_to_name(2), "INT");
    assert_eq!(signal_number_to_name(15), "TERM");
    assert_eq!(signal_number_to_name(999), "UNKNOWN");
}

#[test]
fn test_signal_init_and_get_number() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let cid = ctx.ensure_class_initialized("sun/misc/Signal").unwrap();
    let signal_obj = ctx.alloc_object(cid, 4);
    let name_str = ctx.create_string("INT");

    let result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Signal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[
            Value::Object(Some(signal_obj)),
            Value::Object(Some(name_str)),
        ],
    );
    assert!(result.is_ok());

    // getNumber should return 2 (SIGINT)
    let num_result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Signal",
        "getNumber",
        "()I",
        &[Value::Object(Some(signal_obj))],
    );
    assert_eq!(num_result.unwrap(), Some(Value::Int(2)));
}

#[test]
fn test_signal_handler_registration() {
    // A private VM id: the handler table is per VM (gc-common w10-a), so
    // this test owns its rows and never clears another test's.
    const VM: usize = 0xA10_5101;
    forget_vm_signal_handlers(VM);

    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);

    // Create a signal object for SIGINT
    let cid = ctx.ensure_class_initialized("sun/misc/Signal").unwrap();
    let signal_obj = ctx.alloc_object(cid, 4);
    let name_str = ctx.create_string("INT");
    call_native(
        &reg,
        &mut ctx,
        "sun/misc/Signal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[
            Value::Object(Some(signal_obj)),
            Value::Object(Some(name_str)),
        ],
    )
    .unwrap();

    // Create a mock handler object
    let handler_cid = ctx
        .ensure_class_initialized("sun/misc/SignalHandler")
        .unwrap();
    let handler_obj = ctx.alloc_object(handler_cid, 2);

    // Register handler — first registration returns null (no previous handler)
    let handle_result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Signal",
        "handle",
        "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;",
        &[
            Value::Object(Some(signal_obj)),
            Value::Object(Some(handler_obj)),
        ],
    );
    assert!(handle_result.is_ok());
    // First registration — no previous handler, so null
    assert_eq!(handle_result.unwrap(), Some(Value::Object(None)));

    // Register another handler — should return previous handler
    let handler2 = ctx.alloc_object(handler_cid, 2);
    let handle_result2 = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Signal",
        "handle",
        "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;",
        &[
            Value::Object(Some(signal_obj)),
            Value::Object(Some(handler2)),
        ],
    );
    assert!(handle_result2.is_ok());
    // Should return the first handler
    assert!(matches!(
        handle_result2.unwrap(),
        Some(Value::Object(Some(_)))
    ));
}

#[test]
fn test_signal_unknown_name_error() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let cid = ctx.ensure_class_initialized("sun/misc/Signal").unwrap();
    let signal_obj = ctx.alloc_object(cid, 4);
    let name_str = ctx.create_string("BOGUS");

    let result = call_native(
        &reg,
        &mut ctx,
        "sun/misc/Signal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[
            Value::Object(Some(signal_obj)),
            Value::Object(Some(name_str)),
        ],
    );
    assert!(result.is_err());
}

#[test]
fn test_jdk_internal_signal_same_behavior() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let cid = ctx
        .ensure_class_initialized("jdk/internal/misc/Signal")
        .unwrap();
    let signal_obj = ctx.alloc_object(cid, 4);
    let name_str = ctx.create_string("TERM");

    let result = call_native(
        &reg,
        &mut ctx,
        "jdk/internal/misc/Signal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[
            Value::Object(Some(signal_obj)),
            Value::Object(Some(name_str)),
        ],
    );
    assert!(result.is_ok());

    let num_result = call_native(
        &reg,
        &mut ctx,
        "jdk/internal/misc/Signal",
        "getNumber",
        "()I",
        &[Value::Object(Some(signal_obj))],
    );
    assert_eq!(num_result.unwrap(), Some(Value::Int(15)));
}
