use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Each test owns its VM pair: the tests run in parallel, and one pair
// shared by all four let a teardown (or a second enqueue into VM A) land
// in the middle of another test's assertions (gc-common w25-b).
const VM_A: usize = 0x10D0_2001;
const VM_B: usize = 0x10D0_2002;
const DRAIN_VM_A: usize = 0x10D0_2003;
const DRAIN_VM_B: usize = 0x10D0_2004;
const EQE_VM_A: usize = 0x10D0_2005;
const EQE_VM_B: usize = 0x10D0_2006;
const HOOK_VM_A: usize = 0x10D0_2007;
const HOOK_VM_B: usize = 0x10D0_2008;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced here: queued, scanned, remapped, popped.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Drops the test's own VMs' rows even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_wildfly_jboss_state(vm);
        }
    }
}

#[test]
fn pending_runnables_are_per_vm_rooted_and_remapped() {
    let _t = Teardown(&[VM_A, VM_B]);
    let r = at(0x10D0_3000);
    enqueue_pending_runnable(VM_A, r);

    let scan = |vm: usize| {
        let mut out = Vec::new();
        gc_scan_eqe_pending_roots(vm, &mut out);
        out
    };
    assert_eq!(scan(VM_A), vec![r], "A's queued Runnable is A's root");
    assert!(scan(VM_B).is_empty(), "and not B's");

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x10D0_3000, 0x10D0_4000);
    gc_update_eqe_pending_refs(VM_B, &map);
    assert_eq!(scan(VM_A), vec![r], "B's map must not move A's entry");
    gc_update_eqe_pending_refs(VM_A, &map);
    assert_eq!(
        pop_pending_runnable(VM_A),
        Some(at(0x10D0_4000)),
        "pops the moved address"
    );
    assert_eq!(pop_pending_runnable(VM_A), None);
}

/// Enqueue in A, drain in B: B runs nothing (its queue is empty, so it
/// never dispatches), and A's drain still finds the task.
#[test]
fn a_drain_in_another_vm_runs_nothing() {
    let _t = Teardown(&[DRAIN_VM_A, DRAIN_VM_B]);
    let r = at(0x10D0_5000);
    enqueue_pending_runnable(DRAIN_VM_A, r);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(DRAIN_VM_B);
    drain_all_pending_runnables(&mut b);
    assert_eq!(pop_pending_runnable(DRAIN_VM_B), None);
    assert_eq!(
        pop_pending_runnable(DRAIN_VM_A),
        Some(r),
        "A's task survives B's drain"
    );
}

#[test]
fn eqe_shutdown_flags_are_per_vm() {
    let _t = Teardown(&[EQE_VM_A, EQE_VM_B]);
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(EQE_VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(EQE_VM_B);
    // The mock's identity hash is the address, the same in both VMs.
    let exec = [Value::Object(Some(at(0x10D0_6000)))];
    native_eqe_shutdown(&mut a, &exec).unwrap();
    assert!(matches!(
        native_eqe_is_shutdown(&mut a, &exec),
        Ok(Some(Value::Int(1)))
    ));
    assert!(
        matches!(
            native_eqe_is_shutdown(&mut b, &exec),
            Ok(Some(Value::Int(0)))
        ),
        "A's shutdown() must not shut B's executor with the same hash"
    );
}

/// gc-common w29-b: two live executors of ONE VM with one identity hash
/// (4 GiB apart: the mock hash is the address truncated to `i32`) keep
/// their own flag, and a dead executor's flag goes with its lock key.
#[test]
fn eqe_shutdown_flags_tell_one_vms_hash_colliders_apart() {
    const VM: usize = 0xB29_4001;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            crate::forget_vm_lock_keys(VM);
            forget_vm_wildfly_jboss_state(VM);
        }
    }
    let _t = Forget;
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM);
    let (down, up) = (at(0x1_B2A0_6000), at(0x2_B2A0_6000));
    assert_eq!(
        c.identity_hash_code(down),
        c.identity_hash_code(up),
        "premise"
    );
    native_eqe_shutdown(&mut c, &[Value::Object(Some(down))]).unwrap();
    assert!(matches!(
        native_eqe_is_shutdown(&mut c, &[Value::Object(Some(down))]),
        Ok(Some(Value::Int(1)))
    ));
    assert!(
        matches!(
            native_eqe_is_shutdown(&mut c, &[Value::Object(Some(up))]),
            Ok(Some(Value::Int(0)))
        ),
        "one executor's shutdown() shut its hash collider"
    );
    let up_addr = up.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|x| x == up_addr);
    assert_eq!(
        EQE_SHUTDOWN_FLAGS.peek(VM, |flags| flags.len()),
        Some(0),
        "the dead executor's flag survived"
    );
}

/// B's thread with A's thread id must not run (or unroot) A's hooks.
#[test]
fn exit_hooks_are_per_vm() {
    let _t = Teardown(&[HOOK_VM_A, HOOK_VM_B]);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(HOOK_VM_B);
    let tid = b.thread_id();
    JBOSS_EXIT_HOOKS.with(HOOK_VM_A, |hooks| {
        hooks.entry(tid).or_default().push(0x10D0)
    });
    jboss_run_exit_hooks(&mut b);
    assert_eq!(
        JBOSS_EXIT_HOOKS.peek(HOOK_VM_A, |hooks| hooks.get(&tid).cloned()),
        Some(Some(vec![0x10D0])),
        "A's hook stays queued for A's own thread"
    );
}
