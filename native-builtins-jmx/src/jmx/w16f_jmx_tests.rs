//! gc-common w16-f: per-VM JMM switches, and the MBeanServer registry
//! natives reading their receiver after the key's Java call.
use super::*;
use cratonvm_native_builtins::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

#[test]
fn w16f_jmm_switches_are_per_vm_and_dropped_at_teardown() {
    const VM_A: usize = 0xF16_0401;
    const VM_B: usize = 0xF16_0402;
    let a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let b = MockNativeContext::new();
    b.set_vm_identity(VM_B);

    jmm_switch_set(&a, |s| s.thread_cpu_time = false);
    jmm_switch_set(&a, |s| s.thread_contention_monitoring = true);
    assert!(!jmm_switch(&a, |s| s.thread_cpu_time));
    assert!(
        jmm_switch(&b, |s| s.thread_cpu_time),
        "VM A disabling CPU-time measurement must not disable VM B's"
    );
    assert!(
        !jmm_switch(&b, |s| s.thread_contention_monitoring),
        "VM B's contention monitoring is still off, so its own enable is a transition"
    );

    forget_vm_platform_mbean_server(VM_A);
    forget_vm_platform_mbean_server(VM_B);
    assert!(
        jmm_switch(&a, |s| s.thread_cpu_time),
        "teardown restores the defaults"
    );
    assert!(!jmm_switch(&a, |s| s.thread_contention_monitoring));
}

thread_local! {
    /// (old, new) pairs `getCanonicalName` moves: server, bean, name.
    static W16F_MOVES: Cell<[(usize, usize); 3]> = const { Cell::new([(0, 0); 3]) };
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// `getCanonicalName` -- the key's Java call -- moves every registered
/// object and answers a canonical name.
fn w16f_key_moves_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "getCanonicalName" {
        return None;
    }
    for (old, new) in W16F_MOVES.with(|m| m.get()) {
        if old != 0 {
            ctx.remap_native_pin_addr_for_test(old, new);
        }
    }
    let s = ctx.create_string("w16f:type=Probe");
    Some(Ok(Some(Value::Object(Some(s)))))
}

/// `registerMBean` used to grow the registry arrays of -- and store the
/// bean and name from -- the addresses read before `getCanonicalName()`.
#[test]
fn w16f_register_mbean_writes_the_moved_server_bean_and_name() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF16_0403);
    let server_old = ctx.fresh_object_ref();
    let server_new = ctx.fresh_object_ref();
    let bean_old = ctx.fresh_object_ref();
    let bean_new = ctx.fresh_object_ref();
    let name_old = ctx.fresh_object_ref();
    let name_new = ctx.fresh_object_ref();
    W16F_MOVES.with(|m| {
        m.set([
            (addr(server_old), addr(server_new)),
            (addr(bean_old), addr(bean_new)),
            (addr(name_old), addr(name_new)),
        ])
    });
    ctx.set_invoke_virtual_hook(w16f_key_moves_hook);
    let mut r = NativeMethodRegistry::new();
    register_mbean_server(&mut r);
    let register = r
        .find(
            "javax/management/MBeanServer",
            "registerMBean",
            "(Ljava/lang/Object;Ljavax/management/ObjectName;)Ljavax/management/ObjectInstance;",
        )
        .expect("registerMBean is registered");

    let out = register(
        &mut ctx,
        &[
            Value::Object(Some(server_old)),
            Value::Object(Some(bean_old)),
            Value::Object(Some(name_old)),
        ],
    );
    W16F_MOVES.with(|m| m.set([(0, 0); 3]));

    assert!(matches!(out, Ok(Some(Value::Object(Some(_))))));
    let element = |arr_slot: usize| match ctx.get_field(server_new, arr_slot) {
        Value::Object(Some(arr)) => ctx.get_array_element(arr, 0),
        other => panic!("the moved server must hold the registry arrays, got {other:?}"),
    };
    assert!(matches!(element(MBS_BEANS), Value::Object(Some(b)) if b == bean_new));
    assert!(matches!(element(MBS_ONAMES), Value::Object(Some(n)) if n == name_new));
    assert!(matches!(element(MBS_NAMES), Value::Object(Some(_))));
    assert!(matches!(
        ctx.get_field(server_new, MBS_COUNT),
        Value::Int(1)
    ));
    assert!(
        !matches!(ctx.get_field(server_old, MBS_NAMES), Value::Object(Some(_))),
        "nothing may be written through the vacated address"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}
