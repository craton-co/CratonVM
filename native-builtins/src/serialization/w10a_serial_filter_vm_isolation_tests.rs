use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeInvokeAccess};

const VM_A: usize = 0xA10_5E01;
const VM_B: usize = 0xA10_5E02;

struct ForgetBoth;
impl Drop for ForgetBoth {
    fn drop(&mut self) {
        forget_vm_serial_filter(VM_A);
        forget_vm_serial_filter(VM_B);
    }
}

/// A VM row seeded with "no filter", whatever `jdk.serialFilter` says in
/// the environment the tests run in.
fn vm_ctx(vm: usize) -> MockNativeContext {
    PROCESS_SERIAL_FILTER.with(vm, |st| {
        *st = ProcessSerialFilter {
            seeded: true,
            ..ProcessSerialFilter::default()
        }
    });
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn filter(ctx: &mut MockNativeContext, status: FilterStatus) -> ObjectRef {
    let f = ctx.fresh_object_ref();
    ctx.set_field(f, 0, Value::Int(status as i32));
    f
}

#[test]
fn serial_filter_is_per_vm() {
    let _forget = ForgetBoth;
    let mut r = NativeMethodRegistry::new();
    register_object_input_filter(&mut r);
    let cfg = "java/io/ObjectInputFilter$Config";
    let set = r
        .find(cfg, "setSerialFilter", "(Ljava/io/ObjectInputFilter;)V")
        .expect("setSerialFilter is registered");
    let get = r
        .find(cfg, "getSerialFilter", "()Ljava/io/ObjectInputFilter;")
        .expect("getSerialFilter is registered");
    let mut a = vm_ctx(VM_A);
    let mut b = vm_ctx(VM_B);

    let fa = filter(&mut a, FilterStatus::Rejected);
    assert!(set(&mut a, &[Value::Object(Some(fa))]).is_ok());
    assert_eq!(
        get(&mut a, &[]).ok().flatten(),
        Some(Value::Object(Some(fa)))
    );
    assert_eq!(
        get(&mut b, &[]).ok().flatten(),
        Some(Value::Object(None)),
        "VM B's getSerialFilter() must not answer VM A's filter"
    );

    // B may still install its own; A's is set once and stays set.
    let fb = filter(&mut b, FilterStatus::Allowed);
    assert!(
        set(&mut b, &[Value::Object(Some(fb))]).is_ok(),
        "VM A's filter must not make VM B's first setSerialFilter throw"
    );
    assert!(
        set(&mut a, &[Value::Object(Some(fa))]).is_err(),
        "set-once holds per VM"
    );

    // Each VM's deserialization is judged by its own filter.
    let addr = 0xA10_5E00_usize; // no per-stream filter at this address
    assert_eq!(
        evaluate_serial_filters(VM_A, addr, "com/example/Anything"),
        FilterStatus::Rejected
    );
    assert_eq!(
        evaluate_serial_filters(VM_B, addr, "com/example/Anything"),
        FilterStatus::Allowed
    );

    forget_vm_serial_filter(VM_A);
    assert!(!PROCESS_SERIAL_FILTER.has_row(VM_A));
    assert_eq!(
        get(&mut b, &[]).ok().flatten(),
        Some(Value::Object(Some(fb)))
    );
}
