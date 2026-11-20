//! gci i46/hc: the platform `MemoryMXBean` is the JDK helper's singleton,
//! `getGarbageCollectorMXBeans()` lists in HotSpot's order, and the bean /
//! GC-notification strings are fresh, as HotSpot 25 builds them.
use super::*;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use std::cell::Cell;

thread_local! {
    /// The bean the hook below answered `getMemoryMXBean()` with (0: never asked).
    static HELPER_BEAN: Cell<usize> = const { Cell::new(0) };
}

/// `ManagementFactoryHelper.getMemoryMXBean()` as the JDK answers it: a
/// `sun.management.MemoryImpl`.
fn helper_hook(
    ctx: &mut cratonvm_native_builtins::test_utils::MockNativeContext,
    class_name: &str,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if class_name != "sun/management/ManagementFactoryHelper" || method_name != "getMemoryMXBean" {
        return None;
    }
    let bean = match try_alloc_concurrent_synthetic(ctx, "sun/management/MemoryImpl", 1) {
        Ok(bean) => bean,
        Err(e) => return Some(Err(e)),
    };
    HELPER_BEAN.with(|c| c.set(bean.as_ptr() as usize));
    Some(Ok(Some(Value::Object(Some(bean)))))
}

#[test]
fn gci_i46_hc_the_memory_mxbean_is_the_jdk_helpers_singleton() {
    // A private identity: the default mock identity is shared.
    const VM: usize = 0x46C0_0001;
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    ctx.set_invoke_hook(helper_hook);
    HELPER_BEAN.with(|c| c.set(0));
    let first = platform_memory_mxbean(&mut ctx).expect("bean");
    let helper = HELPER_BEAN.with(|c| c.get());
    assert_ne!(helper, 0, "the helper was asked");
    assert_eq!(first.as_ptr() as usize, helper, "the helper's bean, not a minted one");
    HELPER_BEAN.with(|c| c.set(0));
    let again = platform_memory_mxbean(&mut ctx).expect("bean");
    assert_eq!(again.as_ptr(), first.as_ptr(), "one bean per VM");
    assert_eq!(HELPER_BEAN.with(|c| c.get()), 0, "asked once, then cached");
    // Rooted like the minted bean was.
    let mut roots = Vec::new();
    gc_scan_platform_mbean_server_root_for_vm(VM, &mut roots);
    assert!(roots.iter().any(|r| r.as_ptr() == first.as_ptr()));
    forget_vm_platform_mbean_server(VM);
}


#[test]
fn gci_i46_hc_java_string_hash_code_is_javas() {
    assert_eq!(java_string_hash_code(""), 0);
    assert_eq!(java_string_hash_code("a"), 97);
    assert_eq!(java_string_hash_code("ab"), 97 * 31 + 98);
    assert_eq!(java_string_hash_code("hello"), 99_162_322);
    // Wraps as Java's `int` does.
    assert_eq!(java_string_hash_code("polygenelubricants"), i32::MIN);
}


