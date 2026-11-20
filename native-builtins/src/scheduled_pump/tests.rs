use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::MethodCallResult;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

static PUMP_RELOC_OLD: AtomicUsize = AtomicUsize::new(0);
static PUMP_RELOC_NEW: AtomicUsize = AtomicUsize::new(0);
static PUMP_RELOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static PUMP_RELOC_FIRST: AtomicUsize = AtomicUsize::new(0);
static PUMP_RELOC_SECOND: AtomicUsize = AtomicUsize::new(0);

fn relocating_run_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "run" || descriptor != "()V" {
        return None;
    }
    let call = PUMP_RELOC_CALLS.fetch_add(1, Ordering::SeqCst);
    match call {
        0 => {
            PUMP_RELOC_FIRST.store(receiver.as_ptr() as usize, Ordering::SeqCst);
            ctx.remap_native_pin_addr_for_test(
                PUMP_RELOC_OLD.load(Ordering::SeqCst),
                PUMP_RELOC_NEW.load(Ordering::SeqCst),
            );
        }
        1 => {
            PUMP_RELOC_SECOND.store(receiver.as_ptr() as usize, Ordering::SeqCst);
        }
        _ => {}
    }
    Some(Ok(None))
}

#[test]
fn registry_singleton() {
    let r1 = registry() as *const _;
    let r2 = registry() as *const _;
    assert_eq!(r1, r2);
}

#[test]
fn cancel_returns_true_only_first_time() {
    let task = Arc::new(ScheduledTask {
        id: 0,
        runnable_ptr: AtomicUsize::new(0),
        start: Instant::now(),
        initial_delay_ms: 0,
        period_ms: 50,
        fired: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        fixed_delay: false,
        last_fire: Mutex::new(None),
        vm: SCHEDULED_UNSCOPED_VM,
    });
    assert!(task.cancel());
    assert!(!task.cancel());
    assert!(task.is_cancelled());
}

#[test]
fn fixed_rate_due_ticks_grow_with_time() {
    let task = ScheduledTask {
        id: 0,
        runnable_ptr: AtomicUsize::new(0),
        start: Instant::now() - Duration::from_millis(150),
        initial_delay_ms: 0,
        period_ms: 50,
        fired: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        fixed_delay: false,
        last_fire: Mutex::new(None),
        vm: SCHEDULED_UNSCOPED_VM,
    };
    // 150ms / 50ms + 1 = 4 ticks (1 immediate + 3 periodic).
    let n = task.due_ticks(Instant::now());
    assert!(n >= 3 && n <= 5, "expected 3..=5, got {n}");
}

#[test]
fn one_shot_caps_at_one_tick() {
    let task = ScheduledTask {
        id: 0,
        runnable_ptr: AtomicUsize::new(0),
        start: Instant::now() - Duration::from_secs(1),
        initial_delay_ms: 50,
        period_ms: 0,
        fired: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        fixed_delay: false,
        last_fire: Mutex::new(None),
        vm: SCHEDULED_UNSCOPED_VM,
    };
    assert_eq!(task.due_ticks(Instant::now()), 1);    }

#[test]
fn pump_re_reads_runnable_pin_after_callback_gc() {
    let old_addr = 0x5100usize;
    let new_addr = 0x6100usize;
    PUMP_RELOC_OLD.store(old_addr, Ordering::SeqCst);
    PUMP_RELOC_NEW.store(new_addr, Ordering::SeqCst);
    PUMP_RELOC_CALLS.store(0, Ordering::SeqCst);
    PUMP_RELOC_FIRST.store(0, Ordering::SeqCst);
    PUMP_RELOC_SECOND.store(0, Ordering::SeqCst);

    let reg = ScheduledRegistry::new();
    let task = Arc::new(ScheduledTask {
        id: 500,
        runnable_ptr: AtomicUsize::new(old_addr),
        start: Instant::now() - Duration::from_millis(10),
        initial_delay_ms: 0,
        period_ms: 1,
        fired: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        fixed_delay: false,
        last_fire: Mutex::new(None),
        vm: SCHEDULED_UNSCOPED_VM,
    });
    reg.tasks.lock().push(task.clone());

    let mut ctx = MockNativeContext::new();
    ctx.set_invoke_virtual_hook(relocating_run_hook);
    reg.pump(&mut ctx);

    assert!(
        PUMP_RELOC_CALLS.load(Ordering::SeqCst) >= 2,
        "test setup must accrue at least two scheduled fires"
    );
    assert_eq!(PUMP_RELOC_FIRST.load(Ordering::SeqCst), old_addr);
    assert_eq!(PUMP_RELOC_SECOND.load(Ordering::SeqCst), new_addr);
    assert_eq!(task.runnable_ptr.load(Ordering::SeqCst), new_addr);
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn gc_drops_cancelled_tasks() {
    let reg = ScheduledRegistry::new();
    // Synthesize a task without a real ObjectRef.
    let id_a = {
        let task_a = Arc::new(ScheduledTask {
            id: 100,
            runnable_ptr: AtomicUsize::new(0),
            start: Instant::now(),
            initial_delay_ms: 0,
            period_ms: 50,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
            vm: SCHEDULED_UNSCOPED_VM,
        });
        reg.tasks.lock().push(task_a.clone());
        task_a.cancel();
        task_a.id
    };
    let task_b = Arc::new(ScheduledTask {
        id: 101,
        runnable_ptr: AtomicUsize::new(0),
        start: Instant::now(),
        initial_delay_ms: 0,
        period_ms: 50,
        fired: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        fixed_delay: false,
        last_fire: Mutex::new(None),
        vm: SCHEDULED_UNSCOPED_VM,
    });
    reg.tasks.lock().push(task_b.clone());
    reg.gc();
    let remaining: Vec<_> = reg.tasks.lock().iter().map(|t| t.id).collect();
    assert!(!remaining.contains(&id_a));
    assert!(remaining.contains(&task_b.id));
}

#[test]
fn gc_update_remaps_runnable_ptr() {
    // The remap helper rewrites stored runnable pointers old->new and
    // leaves unmapped / null pointers untouched. This is the relocation
    // path that prevents the stale-pointer UAF under a moving GC. We
    // assert directly on the per-task pointer (no ObjectRef deref).
    let task_moved = Arc::new(ScheduledTask {
        id: 300,
        runnable_ptr: AtomicUsize::new(0x1000),
        start: Instant::now(),
        initial_delay_ms: 0,
        period_ms: 50,
        fired: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        fixed_delay: false,
        last_fire: Mutex::new(None),
        vm: SCHEDULED_UNSCOPED_VM,
    });
    let task_stable = Arc::new(ScheduledTask {
        id: 301,
        runnable_ptr: AtomicUsize::new(0x2000),
        start: Instant::now(),
        initial_delay_ms: 0,
        period_ms: 50,
        fired: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        fixed_delay: false,
        last_fire: Mutex::new(None),
        vm: SCHEDULED_UNSCOPED_VM,
    });

    let mut map = std::collections::HashMap::new();
    map.insert(0x1000usize, 0x9000usize);
    // 0x2000 deliberately absent.

    // Apply the remap to a local task set (mirrors the global-lock loop
    // body without touching the process-wide registry).
    for task in [&task_moved, &task_stable] {
        let old = task.runnable_ptr.load(Ordering::Acquire);
        if old == 0 {
            continue;
        }
        if let Some(&new) = map.get(&old) {
            task.runnable_ptr.store(new, Ordering::Release);
        }
    }

    assert_eq!(task_moved.runnable_ptr.load(Ordering::Acquire), 0x9000);
    assert_eq!(task_stable.runnable_ptr.load(Ordering::Acquire), 0x2000);
}

#[test]
fn gc_update_global_registry_remaps_and_skips_null() {
    // Drive the real public helper against the process-wide registry.
    // Use a synthetic pointer value that is never dereferenced (the
    // remap path only loads/stores the integer).
    let reg = registry();
    let synthetic = 0x5A5A_0000usize;
    let task = Arc::new(ScheduledTask {
        id: 400,
        runnable_ptr: AtomicUsize::new(synthetic),
        start: Instant::now(),
        initial_delay_ms: 0,
        period_ms: 50,
        fired: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        fixed_delay: false,
        last_fire: Mutex::new(None),
        vm: SCHEDULED_UNSCOPED_VM,
    });
    let null_task = Arc::new(ScheduledTask {
        id: 401,
        runnable_ptr: AtomicUsize::new(0),
        start: Instant::now(),
        initial_delay_ms: 0,
        period_ms: 50,
        fired: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        fixed_delay: false,
        last_fire: Mutex::new(None),
        vm: SCHEDULED_UNSCOPED_VM,
    });
    reg.tasks.lock().push(task.clone());
    reg.tasks.lock().push(null_task.clone());

    let mut map = cratonvm_types::PointerMap::default();
    let relocated = 0x5A5A_1000usize;
    map.insert(synthetic, relocated);
    gc_update_scheduled_refs(0x7A80_0701, &map);

    assert_eq!(task.runnable_ptr.load(Ordering::Acquire), relocated);
    // Null pointer left untouched (no spurious remap).
    assert_eq!(null_task.runnable_ptr.load(Ordering::Acquire), 0);
}

#[test]
fn cancel_all_marks_every_task() {
    let reg = ScheduledRegistry::new();
    for i in 0..3 {
        reg.tasks.lock().push(Arc::new(ScheduledTask {
            id: 200 + i,
            runnable_ptr: AtomicUsize::new(0),
            start: Instant::now(),
            initial_delay_ms: 0,
            period_ms: 50,
            fired: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            fixed_delay: false,
            last_fire: Mutex::new(None),
            vm: SCHEDULED_UNSCOPED_VM,
        }));
    }
    // Confirm pre-state by id.
    let live_before: Vec<u64> = reg
        .tasks
        .lock()
        .iter()
        .filter(|t| !t.is_cancelled() && t.id >= 200)
        .map(|t| t.id)
        .collect();
    assert_eq!(live_before.len(), 3);
    reg.cancel_all();
    let live_after: Vec<u64> = reg
        .tasks
        .lock()
        .iter()
        .filter(|t| !t.is_cancelled() && t.id >= 200)
        .map(|t| t.id)
        .collect();
    assert_eq!(live_after.len(), 0);
}

const W8A_RUNNABLE: usize = 0x7A80_7000;
static W8A_FIRES: AtomicUsize = AtomicUsize::new(0);

fn w8a_counting_run_hook(
    _ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "run" || descriptor != "()V" {
        return None;
    }
    if receiver.as_ptr() as usize == W8A_RUNNABLE {
        W8A_FIRES.fetch_add(1, Ordering::SeqCst);
    }
    Some(Ok(None))
}

/// gc-common w8-a: a task registered by VM A is pumped only by VM A, and
/// only VM A's collections root and remap its runnable.
#[test]
fn w8a_a_task_belongs_to_the_vm_that_registered_it() {
    const VM_A: usize = 0x7A80_0702;
    const VM_B: usize = 0x7A80_0703;
    // SAFETY: never dereferenced; the invoke hook intercepts `run()`.
    let runnable = unsafe { ObjectRef::from_raw(W8A_RUNNABLE as *mut u8) };
    let task = registry().register_in_vm(VM_A, runnable, 0, 1_000_000, false);
    let scan = |vm: usize| {
        let mut roots = Vec::new();
        gc_scan_scheduled_roots(vm, &mut roots);
        roots
            .iter()
            .map(|r| r.as_ptr() as usize)
            .collect::<Vec<usize>>()
    };
    assert!(scan(VM_A).contains(&W8A_RUNNABLE));
    assert!(
        !scan(VM_B).contains(&W8A_RUNNABLE),
        "B must not root A's runnable"
    );

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(W8A_RUNNABLE, W8A_RUNNABLE + 0x1000);
    gc_update_scheduled_refs(VM_B, &map);
    assert_eq!(task.runnable_ptr.load(Ordering::Acquire), W8A_RUNNABLE);

    let mut b = MockNativeContext::new();
    b.set_vm_identity(VM_B);
    b.set_invoke_virtual_hook(w8a_counting_run_hook);
    registry().pump(&mut b);
    assert_eq!(
        W8A_FIRES.load(Ordering::SeqCst),
        0,
        "B's pump must not run A's task"
    );

    let mut a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    a.set_invoke_virtual_hook(w8a_counting_run_hook);
    registry().pump(&mut a);
    assert_eq!(W8A_FIRES.load(Ordering::SeqCst), 1, "A's pump runs it once");

    registry().forget_vm(VM_A);
    assert!(task.is_cancelled());
    assert!(registry().find(task.id).is_none());
}
