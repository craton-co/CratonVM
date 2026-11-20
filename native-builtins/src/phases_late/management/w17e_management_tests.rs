//! gc-common w17-e: `-verbose:gc` and the peak live-thread count are per
//! VM, and go at the VM's teardown.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

/// Clears the test's VMs even when an assertion fails. Each test owns
/// its identities: the tests run in parallel and share these tables.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_management_state(vm);
        }
    }
}

/// One VM's high-water mark is not another's, and one VM's reset leaves
/// another's mark alone. The mock reports one live thread.
#[test]
fn w17e_peak_thread_count_is_per_vm() {
    const VM_A: usize = 0xE17_0701;
    const VM_B: usize = 0xE17_0702;
    let _teardown = Teardown(&[VM_A, VM_B]);
    let a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let b = MockNativeContext::new();
    b.set_vm_identity(VM_B);
    assert_eq!(
        a.active_thread_count(),
        1,
        "precondition: the mock's live count"
    );

    // VM A once sampled 50 live threads.
    PEAK_THREAD_COUNT.with(VM_A, |peak| *peak = 50);
    assert_eq!(peak_thread_count(&a), 50);
    assert_eq!(
        peak_thread_count(&b),
        1,
        "VM B's peak is its own threads' high-water mark, not VM A's"
    );

    reset_peak_thread_count(&b);
    assert_eq!(peak_thread_count(&a), 50, "VM B's reset leaves VM A's mark");
    reset_peak_thread_count(&a);
    assert_eq!(peak_thread_count(&a), 1);

    forget_vm_management_state(VM_A);
    assert!(!PEAK_THREAD_COUNT.has_row(VM_A), "teardown drops the mark");
    assert!(PEAK_THREAD_COUNT.has_row(VM_B), "and only that VM's");
}

/// The synthetic `MemoryMXBean.setVerbose` state is per VM.
#[test]
fn w17e_verbose_gc_is_per_vm() {
    const VM_A: usize = 0xE17_0703;
    const VM_B: usize = 0xE17_0704;
    let _teardown = Teardown(&[VM_A, VM_B]);
    let a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let b = MockNativeContext::new();
    b.set_vm_identity(VM_B);
    set_verbose_gc_of(&a, true);
    assert!(verbose_gc_of(&a));
    assert!(!verbose_gc_of(&b), "VM A's -verbose:gc is not VM B's");
    forget_vm_management_state(VM_A);
    assert!(!verbose_gc_of(&a), "teardown restores the default");
}

/// gc-common w18-g: before any `setVerbose`, a VM reports its launch
/// switch (`-verbose:gc` -> `isVerbose() == true`, as on HotSpot); a
/// write wins over it; reading creates no row.
#[test]
fn w18g_verbose_gc_starts_at_the_launch_switch() {
    const VM: usize = 0x18E0_0705;
    const VM_MOCK: usize = 0x18E0_0706;
    let _teardown = Teardown(&[VM, VM_MOCK]);
    assert!(
        verbose_gc_or(VM, || true),
        "a -verbose:gc launch reads true before any setVerbose"
    );
    assert!(
        !verbose_gc_or(VM, || false),
        "no -verbose:gc, no row: false"
    );
    assert!(!VERBOSE_GC_BY_VM.has_row(VM), "reading creates no row");

    // `setVerbose(false)` on a -verbose:gc launch.
    VERBOSE_GC_BY_VM.with(VM, |cell| *cell = false);
    assert!(!verbose_gc_or(VM, || true));
    VERBOSE_GC_BY_VM.with(VM, |cell| *cell = true);
    assert!(
        verbose_gc_or(VM, || panic!(
            "the launch switch is not asked once a row exists"
        )),
        "the write wins"
    );

    // A context that knows no launch switch (the mock) keeps the old
    // answer, and a write through it round-trips.
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_MOCK);
    assert!(!ctx.verbose_gc_at_launch());
    assert!(!verbose_gc_of(&ctx));
    set_verbose_gc_of(&ctx, true);
    assert!(verbose_gc_of(&ctx));
}
