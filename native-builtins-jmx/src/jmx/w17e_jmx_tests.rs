//! gc-common w17-e (`common-w16f-jmm-switches-and-start-time-are-process-
//! wide`): the VM start origin and `-verbose:gc` are per VM.
use super::*;
use cratonvm_native_builtins::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::time::Duration;

/// Two VMs whose start instants differ report different start times, each
/// stable across queries, and teardown drops the VM's clock.
#[test]
fn w17e_vm_start_origin_is_per_vm_and_stable() {
    const VM_A: usize = 0xE17_0501;
    const VM_B: usize = 0xE17_0502;
    struct Teardown;
    impl Drop for Teardown {
        fn drop(&mut self) {
            forget_vm_platform_mbean_server(VM_A);
            forget_vm_platform_mbean_server(VM_B);
        }
    }
    let _teardown = Teardown;

    let now = Instant::now();
    // VM A started 5 s before VM B. `checked_sub` can fail only on a host
    // whose monotonic clock started less than 5 s ago.
    let Some(a_start) = now.checked_sub(Duration::from_secs(5)) else {
        return;
    };
    let a = clock_for_vm(VM_A, a_start);
    let b = clock_for_vm(VM_B, now);
    let delta = b.start_epoch_ms as i64 - a.start_epoch_ms as i64;
    assert!(
        (4_900..=5_100).contains(&delta),
        "VM B started 5 s after VM A, so its start time is ~5000 ms later (got {delta})"
    );

    // Derived once: a later query answers the same millis.
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(clock_for_vm(VM_A, a_start).start_epoch_ms, a.start_epoch_ms);
    assert!(VM_CLOCKS.has_row(VM_A));

    // A different instant for the same identity is a different VM start
    // and is not answered from the old row.
    let a2 = clock_for_vm(VM_A, now);
    assert!(a2.start_epoch_ms > a.start_epoch_ms);

    forget_vm_platform_mbean_server(VM_A);
    assert!(!VM_CLOCKS.has_row(VM_A), "teardown drops the VM's clock");
    assert!(VM_CLOCKS.has_row(VM_B), "and only that VM's");
}

/// A context that reports no start instant (the mock) reads the
/// process-wide fallback and creates no per-VM row.
#[test]
fn w17e_context_without_a_start_instant_uses_the_process_origin() {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xE17_0503);
    assert!(ctx.vm_start_instant().is_none());
    let clock = vm_clock(&ctx);
    assert!(clock.start == *process_start());
    assert_eq!(clock.start_epoch_ms, process_start_epoch_ms());
    assert_eq!(vm_start_epoch_ms(&ctx), process_start_epoch_ms());
    assert!(!VM_CLOCKS.has_row(0xE17_0503));
}

/// One VM's `setVerbose(true)` leaves another VM's `isVerbose()` false;
/// both surfaces (`jmx.rs` and the synthetic `MemoryMXBean` in
/// `phases_late::management`) read the same per-VM cell; teardown resets.
#[test]
fn w17e_verbose_gc_is_per_vm_and_dropped_at_teardown() {
    const VM_A: usize = 0xE17_0601;
    const VM_B: usize = 0xE17_0602;
    struct Teardown;
    impl Drop for Teardown {
        fn drop(&mut self) {
            forget_vm_platform_mbean_server(VM_A);
            forget_vm_platform_mbean_server(VM_B);
        }
    }
    let _teardown = Teardown;
    let a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let b = MockNativeContext::new();
    b.set_vm_identity(VM_B);

    verbose_gc_set(&a, true);
    assert!(verbose_gc_get(&a));
    assert!(
        cratonvm_native_builtins::phases_late::management::verbose_gc_of(&a),
        "the synthetic MemoryMXBean surface reads the same cell"
    );
    assert!(!verbose_gc_get(&b), "VM A's -verbose:gc is not VM B's");

    cratonvm_native_builtins::phases_late::management::set_verbose_gc_of(&b, true);
    verbose_gc_set(&a, false);
    assert!(!verbose_gc_get(&a));
    assert!(verbose_gc_get(&b), "VM A turning it off leaves VM B's on");

    forget_vm_platform_mbean_server(VM_B);
    assert!(!verbose_gc_get(&b), "teardown restores the default");
}
