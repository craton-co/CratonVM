use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::RefCell;

const VM_A: usize = 0xA10_5A01;
const VM_B: usize = 0xA10_5A02;

struct ForgetBoth;
impl Drop for ForgetBoth {
    fn drop(&mut self) {
        forget_vm_signal_handlers(VM_A);
        forget_vm_signal_handlers(VM_B);
    }
}

thread_local! {
    /// `(receiver address, descriptor)` of every `handle` upcall.
    static RAISED: RefCell<Vec<(usize, String)>> = const { RefCell::new(Vec::new()) };
}

fn record_handle(
    _ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method != "handle" {
        return None;
    }
    RAISED.with(|r| {
        r.borrow_mut()
            .push((receiver.as_ptr() as usize, descriptor.to_string()))
    });
    Some(Ok(None))
}

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_signal_natives(&mut r);
    r
}

fn call(
    reg: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    class: &str,
    method: &str,
    desc: &str,
    args: &[Value],
) -> Option<Value> {
    let cb = reg
        .find(class, method, desc)
        .unwrap_or_else(|| panic!("{class}.{method}{desc} should be registered"));
    cb(ctx, args).ok().flatten()
}

fn signal(reg: &NativeMethodRegistry, ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).expect("declare Signal");
    let sig = ctx.alloc_object(cid, 4);
    let name = ctx.create_string("INT");
    call(
        reg,
        ctx,
        class,
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sig)), Value::Object(Some(name))],
    );
    sig
}

const SUN_HANDLE: &str = "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;";

#[test]
fn signal_handlers_are_per_vm() {
    let _forget = ForgetBoth;
    let reg = registry();
    let mut a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    a.set_invoke_virtual_hook(record_handle);
    let mut b = MockNativeContext::new();
    b.set_vm_identity(VM_B);
    b.set_invoke_virtual_hook(record_handle);

    let sig_a = signal(&reg, &mut a, "sun/misc/Signal");
    let sig_b = signal(&reg, &mut b, "sun/misc/Signal");
    let ha = a.fresh_object_ref();
    let hb = b.fresh_object_ref();

    let sig_class = "sun/misc/Signal";
    let args_a = [Value::Object(Some(sig_a)), Value::Object(Some(ha))];
    let prev = call(&reg, &mut a, sig_class, "handle", SUN_HANDLE, &args_a);
    assert_eq!(prev, Some(Value::Object(None)));
    let args_b = [Value::Object(Some(sig_b)), Value::Object(Some(hb))];
    let prev = call(&reg, &mut b, sig_class, "handle", SUN_HANDLE, &args_b);
    assert_eq!(
        prev,
        Some(Value::Object(None)),
        "VM B's first handle(SIGINT) must not return VM A's handler"
    );

    RAISED.with(|r| r.borrow_mut().clear());
    let raise = "(Lsun/misc/Signal;)V";
    call(
        &reg,
        &mut b,
        sig_class,
        "raise",
        raise,
        &[Value::Object(Some(sig_b))],
    );
    call(
        &reg,
        &mut a,
        sig_class,
        "raise",
        raise,
        &[Value::Object(Some(sig_a))],
    );
    let raised: Vec<usize> = RAISED.with(|r| r.borrow().iter().map(|(p, _)| *p).collect());
    assert_eq!(
        raised,
        vec![hb.as_ptr() as usize, ha.as_ptr() as usize],
        "each VM's raise must call its own handler"
    );

    forget_vm_signal_handlers(VM_A);
    let clear_b = [Value::Object(Some(sig_b)), Value::Object(None)];
    let prev = call(&reg, &mut b, sig_class, "handle", SUN_HANDLE, &clear_b);
    assert_eq!(
        prev,
        Some(Value::Object(Some(hb))),
        "A's teardown left B's handler"
    );
}

#[test]
fn a_jdk_internal_signal_handler_is_called_with_its_own_descriptor() {
    const VM: usize = 0xA10_5A03;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_signal_handlers(VM);
        }
    }
    let _forget = Forget;
    let reg = registry();
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    ctx.set_invoke_virtual_hook(record_handle);
    let class = "jdk/internal/misc/Signal";
    let sig = signal(&reg, &mut ctx, class);
    let h = ctx.fresh_object_ref();
    let handle = "(Ljdk/internal/misc/Signal;Ljdk/internal/misc/SignalHandler;)\
                  Ljdk/internal/misc/SignalHandler;";
    let args = [Value::Object(Some(sig)), Value::Object(Some(h))];
    call(&reg, &mut ctx, class, "handle", handle, &args);

    RAISED.with(|r| r.borrow_mut().clear());
    let raise = "(Ljdk/internal/misc/Signal;)V";
    call(
        &reg,
        &mut ctx,
        class,
        "raise",
        raise,
        &[Value::Object(Some(sig))],
    );
    let raised = RAISED.with(|r| r.borrow().clone());
    assert_eq!(
        raised,
        vec![(
            h.as_ptr() as usize,
            "(Ljdk/internal/misc/Signal;)V".to_string()
        )],
        "a jdk.internal.misc handler must be called with the jdk.internal.misc descriptor"
    );
}

/// `Beans.setDesignTime` / `setGuiAvailable` in one VM leave the other
/// VM's answers alone.
#[test]
fn beans_state_is_per_vm() {
    const VM_C: usize = 0xA10_5A04;
    const VM_D: usize = 0xA10_5A05;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_beans_state(VM_C);
            forget_vm_beans_state(VM_D);
        }
    }
    let _forget = Forget;
    let mut r = NativeMethodRegistry::new();
    register_deprecated_internal_natives(&mut r);
    let beans = "java/beans/Beans";
    let mut c = MockNativeContext::new();
    c.set_vm_identity(VM_C);
    let mut d = MockNativeContext::new();
    d.set_vm_identity(VM_D);

    call(&r, &mut c, beans, "setDesignTime", "(Z)V", &[Value::Int(1)]);
    call(
        &r,
        &mut c,
        beans,
        "setGuiAvailable",
        "(Z)V",
        &[Value::Int(1)],
    );
    assert_eq!(
        call(&r, &mut c, beans, "isDesignTime", "()Z", &[]),
        Some(Value::Int(1))
    );
    assert_eq!(
        call(&r, &mut c, beans, "isGuiAvailable", "()Z", &[]),
        Some(Value::Int(1))
    );
    assert_eq!(
        call(&r, &mut d, beans, "isDesignTime", "()Z", &[]),
        Some(Value::Int(0)),
        "VM C's setDesignTime(true) must not reach VM D"
    );
    assert_eq!(
        call(&r, &mut d, beans, "isGuiAvailable", "()Z", &[]),
        Some(Value::Int(0))
    );

    forget_vm_beans_state(VM_C);
    assert_eq!(
        call(&r, &mut c, beans, "isDesignTime", "()Z", &[]),
        Some(Value::Int(0))
    );
}
