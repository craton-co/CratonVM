use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Each test owns its VM pair: the tests run in parallel, and a guard
// that forgot one shared pair wiped the other test's rows mid-assertion
// (gc-common w25-b).
const VM_A: usize = 0x10D0_4001;
const VM_B: usize = 0x10D0_4002;
const ROOTS_VM_A: usize = 0x10D0_4003;
const ROOTS_VM_B: usize = 0x10D0_4004;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a cache value only, never dereferenced (the mock's identity
    // hash is the address; its var-handle registry is empty).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Drops the test's own VMs' rows even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_module_loader_state(vm);
        }
    }
}

fn ctx_for(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

#[test]
fn module_cache_and_boot_loader_are_per_vm() {
    let _t = Teardown(&[VM_A, VM_B]);
    let (mut a, mut b) = (ctx_for(VM_A), ctx_for(VM_B));
    let (module_a, module_b) = (at(0x10D0_7000), at(0x10D0_7100));

    assert_eq!(publish_module(&mut a, "w10d.module", module_a), module_a);
    assert_eq!(cached_module(&a, "w10d.module"), Some(module_a));
    assert_eq!(
        cached_module(&b, "w10d.module"),
        None,
        "B must not get A's Module"
    );
    // B's own load of the same name publishes B's Module, not A's.
    assert_eq!(publish_module(&mut b, "w10d.module", module_b), module_b);
    assert_eq!(cached_module(&a, "w10d.module"), Some(module_a));

    BOOT_LOADER.with(VM_A, |slot| *slot = Some((1, at(0x10D0_8000))));
    assert_eq!(BOOT_LOADER.peek(VM_B, |slot| *slot).flatten(), None);

    forget_vm_module_loader_state(VM_A);
    assert_eq!(
        cached_module(&a, "w10d.module"),
        None,
        "teardown drops A's cache"
    );
    assert_eq!(
        cached_module(&b, "w10d.module"),
        Some(module_b),
        "and keeps B's"
    );
}

/// A JAR another VM registered must still reach this VM's class path.
#[test]
fn registered_resource_roots_are_per_vm() {
    let _t = Teardown(&[ROOTS_VM_A, ROOTS_VM_B]);
    let (mut a, mut b) = (ctx_for(ROOTS_VM_A), ctx_for(ROOTS_VM_B));
    let jar = vec![PathBuf::from("w10d/per-vm/module.jar")];
    register_resource_roots(&mut a, &jar);
    register_resource_roots(&mut a, &jar);
    assert_eq!(
        a.registered_classpath_snapshot().len(),
        1,
        "deduplicated within A"
    );
    register_resource_roots(&mut b, &jar);
    assert_eq!(
        b.registered_classpath_snapshot().len(),
        1,
        "B must register the JAR on its own class path"
    );
}
