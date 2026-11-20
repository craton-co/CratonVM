use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::sync::Arc;

/// Addresses of the monitors the registry currently believes `tid` owns.
pub fn owned(shared: &SharedVm, tid: ThreadId) -> Vec<usize> {
    shared
        .threads
        .thread_registry
        .jmx_lock_snapshot(tid)
        .map(|snap| snap.2.iter().map(|o| o.as_ptr() as usize).collect())
        .unwrap_or_default()
}

/// Boot a VM with one registered thread whose JIT thread-pointer is
/// installed, so `jit_monitor_enter`/`jit_monitor_exit` can be called
/// exactly as generated code calls them.
pub fn vm_with_jit_thread() -> (Arc<SharedVm>, Box<JvmThread>, ThreadId) {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let tid = ThreadId(1);
    let thread = Box::new(JvmThread::new(tid, "jit-monitor-probe"));
    shared
        .threads
        .thread_registry
        .register(tid, "jit-monitor-probe", None);
    (shared, thread, tid)
}

#[test]
pub fn the_jit_helpers_publish_and_retract_the_same_monitor() {
    let (shared, mut thread, tid) = vm_with_jit_thread();
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

    let scope = set_jit_thread(&mut thread);
    // SAFETY: `vm_ptr` names the live `SharedVm` above, `obj` is a live
    // heap object, and the JIT thread pointer is installed for this OS
    // thread — the exact contract compiled code satisfies.
    unsafe {
        assert_ne!(
            jit_monitor_enter(vm_ptr, obj.as_ptr() as i64),
            i64::MIN,
            "the helper failed to acquire an uncontended monitor"
        );
        // Prove the RED first: without a publish here there would be
        // nothing for the retract to remove, and the assertion after the
        // exit would pass on a helper that does no bookkeeping at all.
        assert_eq!(
            owned(&shared, tid),
            vec![obj.as_ptr() as usize],
            "a compiled monitorenter must publish JMX ownership"
        );
        assert_ne!(
            jit_monitor_exit(vm_ptr, obj.as_ptr() as i64),
            i64::MIN,
            "the helper failed to release a monitor it holds"
        );
    }
    restore_jit_thread(scope);

    assert!(
        owned(&shared, tid).is_empty(),
        "a compiled monitorexit left its JMX ownership publish standing — \
         the owned-monitor list is scanned on every acquisition, so this is \
         an unbounded list and a quadratic monitorenter, not one stale slot"
    );
}

/// The failure mode is about DISTINCT objects, and one balanced pair cannot
/// see it: the leak only becomes quadratic once the list has more entries
/// than the thread's real lock nesting depth.
#[test]
pub fn locking_many_distinct_objects_leaves_no_residue() {
    const OBJECTS: usize = 64;
    let (shared, mut thread, tid) = vm_with_jit_thread();
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    let objs: Vec<_> = (0..OBJECTS)
        .map(|_| shared.mem.heap.alloc_object(ClassId::new(0), 0))
        .collect();

    let scope = set_jit_thread(&mut thread);
    // SAFETY: as above; every ref comes from this heap and is still live.
    unsafe {
        for obj in &objs {
            jit_monitor_enter(vm_ptr, obj.as_ptr() as i64);
            jit_monitor_exit(vm_ptr, obj.as_ptr() as i64);
        }
    }
    restore_jit_thread(scope);

    assert_eq!(
        owned(&shared, tid).len(),
        0,
        "{OBJECTS} balanced compiled lock/unlock pairs left {} monitors \
         recorded as owned; each one is a permanent GC root and a permanent \
         entry in the scan every subsequent monitorenter runs",
        owned(&shared, tid).len()
    );
}

/// A re-entrant acquisition is still held after the inner release, and
/// retracting there would under-report a monitor the thread really owns.
/// The retract is conditional on `holds` precisely for this case.
#[test]
pub fn a_reentrant_release_keeps_the_publish_until_the_outermost_exit() {
    let (shared, mut thread, tid) = vm_with_jit_thread();
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as i64;

    let scope = set_jit_thread(&mut thread);
    // SAFETY: as above.
    unsafe {
        jit_monitor_enter(vm_ptr, addr);
        jit_monitor_enter(vm_ptr, addr);
        jit_monitor_exit(vm_ptr, addr);
    }
    let after_inner = owned(&shared, tid);
    // SAFETY: as above.
    unsafe {
        jit_monitor_exit(vm_ptr, addr);
    }
    let after_outer = owned(&shared, tid);
    restore_jit_thread(scope);

    assert_eq!(
        after_inner,
        vec![addr as usize],
        "the inner release of a re-entrant lock retracted a monitor the \
         thread still holds"
    );
    assert!(
        after_outer.is_empty(),
        "the outermost release left the publish standing"
    );
}

/// gcd d1/mon: the entry screen of both monitor helpers. A stack address
/// (what the optimizing tier passed when it loaded an unreserved context
/// slot, `[rbp - 0]`), null, the first page and a misaligned word are
/// refused; a live `SharedVm` -- always an `Arc` on the Rust heap -- is
/// admitted. The refusal itself aborts, so only the predicate is run here.
#[test]
pub fn the_monitor_helpers_screen_a_stack_address_posing_as_the_vm() {
    let (shared, _thread, _tid) = vm_with_jit_thread();
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    assert!(
        !jit_monitor_vm_ptr_is_wild(vm_ptr),
        "a live SharedVm must pass the screen"
    );
    let on_stack = 0u64;
    let stack_addr = std::ptr::addr_of!(on_stack) as usize as i64;
    if cratonvm_gc::shadow_stack::current_thread_stack().is_some() {
        assert!(
            jit_monitor_vm_ptr_is_wild(stack_addr),
            "an address in this thread's own stack is never a SharedVm"
        );
    }
    assert!(jit_monitor_vm_ptr_is_wild(0));
    assert!(jit_monitor_vm_ptr_is_wild(0x251));
    assert!(jit_monitor_vm_ptr_is_wild(vm_ptr + 4));
}
