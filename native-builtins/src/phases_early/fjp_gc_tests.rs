use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a map key / root value only, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn scan(vm: usize) -> Vec<ObjectRef> {
    let mut roots = Vec::new();
    gc_scan_forkjoin_roots(vm, &mut roots);
    roots
}

// gc-common w9-b: a private VM and private addresses instead of clearing
// the process-wide table, which other tests in this binary share while
// running in parallel (the old `fjp_state_clear` could wipe their tasks).
#[test]
fn gc_hooks_scan_result_and_remap_key_and_result() {
    const VM: usize = 0x9B00_0701;
    let task = at(0x9B01_1000);
    let result = at(0x9B01_2000);
    let task_new = at(0x9B01_3000);
    let result_new = at(0x9B01_4000);

    fjp_state_set_done(VM, task, Value::Object(Some(result)));
    let roots = scan(VM);
    assert!(roots.contains(&task) && roots.contains(&result));

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(task.as_ptr() as usize, task_new.as_ptr() as usize);
    map.insert(result.as_ptr() as usize, result_new.as_ptr() as usize);
    gc_update_forkjoin_refs(VM, &map);

    assert!(!fjp_state_get(task).0, "old task key must be retired");
    assert_eq!(
        fjp_state_get(task_new),
        (true, Value::Object(Some(result_new)))
    );
    forget_vm_forkjoin_state(VM);
    assert!(
        !fjp_state_get(task_new).0,
        "teardown drops the VM's entries"
    );
}

/// `common-w8a-forkjoin-and-tls-root-rows-stay-process-global`: each VM's
/// collection roots and remaps only its own tasks and common pool, and
/// two VMs' common pools are two objects.
#[test]
fn forkjoin_rows_and_common_pool_belong_to_their_vm() {
    const VM_A: usize = 0x9B00_0711;
    const VM_B: usize = 0x9B00_0712;
    let (task_a, thrown_a) = (at(0x9B02_1000), at(0x9B02_2000));
    let task_b = at(0x9B02_3000);
    fjp_state_mark_queued_in_vm(VM_A, task_a);
    fjp_state_set_thrown(VM_A, task_a, thrown_a);
    fjp_state_mark_queued_in_vm(VM_B, task_b);

    assert!(scan(VM_A).contains(&task_a) && scan(VM_A).contains(&thrown_a));
    assert!(!scan(VM_B).contains(&task_a) && !scan(VM_B).contains(&thrown_a));
    assert!(scan(VM_B).contains(&task_b) && !scan(VM_A).contains(&task_b));
    // B's queued task counts for B; A's (done) task never did.
    assert!(fjp_queued_task_count(VM_B) >= 1);

    // B's pointer map must not rekey A's task, even if it names it.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(task_a.as_ptr() as usize, 0x9B02_9000);
    gc_update_forkjoin_refs(VM_B, &map);
    assert!(fjp_state_get(task_a).0, "A's entry stays at A's address");

    // An unscoped entry (a VM-less writer) is adopted by the first VM
    // that touches it, and then leaves every other VM's scan.
    let loose = at(0x9B02_4000);
    fjp_state_mark_queued_in_vm(FJP_UNSCOPED_VM, loose);
    assert!(scan(VM_A).contains(&loose) && scan(VM_B).contains(&loose));
    fjp_state_mark_queued_in_vm(VM_B, loose);
    assert!(!scan(VM_A).contains(&loose) && scan(VM_B).contains(&loose));

    // One common pool per VM.
    let (pool_a, pool_b) = (at(0x9B02_5000), at(0x9B02_6000));
    fjp_common_pool_set_in_vm(VM_A, pool_a);
    fjp_common_pool_set_in_vm(VM_B, pool_b);
    assert_eq!(fjp_common_pool_get_in_vm(VM_A), Some(pool_a));
    assert_eq!(fjp_common_pool_get_in_vm(VM_B), Some(pool_b));
    assert!(fjp_is_common_pool(VM_A, pool_a) && !fjp_is_common_pool(VM_A, pool_b));
    assert!(scan(VM_A).contains(&pool_a) && !scan(VM_A).contains(&pool_b));

    forget_vm_forkjoin_state(VM_A);
    forget_vm_forkjoin_state(VM_B);
    assert_eq!(fjp_common_pool_get_in_vm(VM_A), None);
    assert!(!fjp_state_get(task_a).0 && !scan(VM_B).contains(&task_b));
}
