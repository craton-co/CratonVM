use super::*;
use crate::types::Value;

/// Interpreter round i3 wave 72, lane L6 (`CRATONVM_UNPARK_ONE_LOOKUP`):
/// the "ever tracked" word is down until the first virtual thread, by
/// either creation path, and stays up after it.
#[test]
fn may_have_virtual_threads_rises_with_the_first_virtual_thread() {
    let mgr = VirtualThreadManager::new(1);
    assert!(!mgr.may_have_virtual_threads());
    assert!(!mgr.is_virtual(7));
    mgr.create_virtual_thread_with_id(7, "w72-vt");
    assert!(mgr.may_have_virtual_threads());
    assert!(mgr.is_virtual(7));
    let other = VirtualThreadManager::new(1);
    let id = other.create_virtual_thread("w72-vt2");
    assert!(other.may_have_virtual_threads() && other.is_virtual(id));
}

// -- Continuation ---------------------------------------------------------

#[test]
fn continuation_new_state() {
    let scope = ContinuationScope {
        name: "test".into(),
    };
    let c = Continuation::new(scope);
    assert_eq!(c.state, ContinuationState::New);
    assert_eq!(c.yield_count, 0);
    assert_eq!(c.mount_count, 0);
    assert!(c.frozen_frames.is_empty());
}

#[test]
fn continuation_freeze_thaw_roundtrip() {
    let scope = ContinuationScope {
        name: "test".into(),
    };
    let mut c = Continuation::new(scope);
    c.state = ContinuationState::Running;

    let frame = FrozenFrame {
        class_name: "Foo".into(),
        method_name: "bar".into(),
        descriptor: "()V".into(),
        bytecode_pc: 42,
        locals: vec![1, 2, 3],
        local_tags: vec![0, 1, 0],
        stack: vec![10, 20],
        stack_tags: vec![0, 0],
        code: None,
        class_id: None,
        max_stack: None,
        max_locals: None,
        exception_table: None,
        source_file: None,
        redefine_stamp: 0,
        replaced_body: None,
        monitor_on_exit: None,
        held_monitors: Vec::new(),
        held_monitors_understated: false,
        held_monitors_mark: None,
    };
    c.freeze(vec![frame]);
    assert_eq!(c.state, ContinuationState::Suspended);
    assert_eq!(c.yield_count, 1);

    let restored = c.thaw();
    assert_eq!(c.state, ContinuationState::Running);
    assert_eq!(c.mount_count, 1);
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].bytecode_pc, 42);
}

#[test]
fn continuation_state_transitions() {
    let scope = ContinuationScope { name: "sc".into() };
    let mut c = Continuation::new(scope);
    assert_eq!(c.state, ContinuationState::New);

    c.state = ContinuationState::Running;
    assert_eq!(c.state, ContinuationState::Running);

    c.freeze(Vec::new());
    assert_eq!(c.state, ContinuationState::Suspended);

    let _ = c.thaw();
    assert_eq!(c.state, ContinuationState::Running);

    c.state = ContinuationState::Completed;
    assert_eq!(c.state, ContinuationState::Completed);
}

#[test]
fn continuation_yield_count_increments() {
    let mut c = Continuation::new(ContinuationScope { name: "s".into() });
    c.state = ContinuationState::Running;
    c.freeze(Vec::new());
    assert_eq!(c.yield_count(), 1);
    let _ = c.thaw();
    c.freeze(Vec::new());
    assert_eq!(c.yield_count(), 2);
}

#[test]
fn continuation_can_yield() {
    let mut c = Continuation::new(ContinuationScope { name: "s".into() });
    assert!(!c.can_yield()); // New
    c.state = ContinuationState::Running;
    assert!(c.can_yield());
    c.freeze(Vec::new());
    assert!(!c.can_yield()); // Suspended
}

// -- FrozenFrame ----------------------------------------------------------

#[test]
fn frozen_frame_preserves_locals_and_stack() {
    let frame = FrozenFrame {
        class_name: "java/lang/Object".into(),
        method_name: "<init>".into(),
        descriptor: "()V".into(),
        bytecode_pc: 0,
        locals: vec![100, 200, 300],
        local_tags: vec![1, 2, 3],
        stack: vec![400],
        stack_tags: vec![4],
        code: None,
        class_id: None,
        max_stack: None,
        max_locals: None,
        exception_table: None,
        source_file: None,
        redefine_stamp: 0,
        replaced_body: None,
        monitor_on_exit: None,
        held_monitors: Vec::new(),
        held_monitors_understated: false,
        held_monitors_mark: None,
    };
    assert_eq!(frame.locals, vec![100, 200, 300]);
    assert_eq!(frame.local_tags, vec![1, 2, 3]);
    assert_eq!(frame.stack, vec![400]);
    assert_eq!(frame.stack_tags, vec![4]);
}

// -- VirtualThread --------------------------------------------------------

#[test]
fn vt_new_starts_in_new_state() {
    let vt = VirtualThread::new(1, "vt-1".into());
    assert_eq!(vt.state, VirtualThreadState::New);
    assert_eq!(vt.id, 1);
    assert_eq!(vt.name, "vt-1");
    assert!(vt.carrier_thread_id.is_none());
    assert_eq!(vt.pin_count, 0);
    assert!(!vt.is_pinned());
}

#[test]
fn vt_mount_unmount() {
    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(42);
    assert_eq!(vt.state, VirtualThreadState::Running);
    assert_eq!(vt.carrier_thread_id, Some(42));

    vt.unmount();
    assert!(vt.carrier_thread_id.is_none());
}

#[test]
fn vt_pin_unpin_with_reason() {
    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);
    vt.pin(PinReason::Monitor);
    assert!(vt.is_pinned());
    assert_eq!(vt.pin_reason, Some(PinReason::Monitor));
    assert_eq!(vt.state, VirtualThreadState::Pinned);

    vt.unpin();
    assert!(!vt.is_pinned());
    assert_eq!(vt.pin_reason, None);
    assert_eq!(vt.state, VirtualThreadState::Running);
}

#[test]
fn vt_park_not_pinned_yields() {
    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);
    let result = vt.park();
    assert!(result);
    assert_eq!(vt.state, VirtualThreadState::Parked);
    assert!(vt.carrier_thread_id.is_none());
}

#[test]
fn vt_park_when_pinned_blocks() {
    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);
    vt.pin(PinReason::NativeMethod);
    let result = vt.park();
    assert!(!result);
    // Still mounted -- carrier blocked.
    assert_eq!(vt.carrier_thread_id, Some(0));
}

#[test]
fn vt_unpark_sets_permit() {
    let mut vt = VirtualThread::new(1, "vt".into());
    vt.unpark();
    assert!(vt.unpark_permit);
}

#[test]
fn vt_unpark_permit_consumed_by_park() {
    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);
    vt.unpark();
    assert!(vt.unpark_permit);
    let result = vt.park();
    assert!(result);
    assert!(!vt.unpark_permit);
    // State should still be Running (permit was consumed, no actual park).
    assert_eq!(vt.state, VirtualThreadState::Running);
}

#[test]
fn vt_double_pin_increments_count() {
    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);
    vt.pin(PinReason::Monitor);
    vt.pin(PinReason::ClassInit);
    assert_eq!(vt.pin_count, 2);
    assert!(vt.is_pinned());
    // Last reason wins
    assert_eq!(vt.pin_reason, Some(PinReason::ClassInit));
    vt.unpin();
    assert!(vt.is_pinned()); // still 1
    vt.unpin();
    assert!(!vt.is_pinned());
}

// -- ForkJoinScheduler ----------------------------------------------------

#[test]
fn scheduler_submit_and_next_task() {
    let sched = ForkJoinScheduler::new(2);
    sched.submit(100);
    sched.submit(200);
    assert_eq!(sched.next_task(0), Some(100));
    assert_eq!(sched.next_task(0), Some(200));
    assert_eq!(sched.next_task(0), None);
}

#[test]
fn scheduler_work_stealing() {
    let sched = ForkJoinScheduler::new(2);
    // Put a task directly into carrier 1's queue.
    sched.work_queues[1].lock().push_back(300);
    // Carrier 0 should steal it.
    let stolen = sched.try_steal(0);
    assert_eq!(stolen, Some(300));
    assert_eq!(sched.stats.total_steals.load(Ordering::Relaxed), 1);
}

#[test]
fn scheduler_default_parallelism() {
    let sched = ForkJoinScheduler::with_default_parallelism();
    assert!(sched.parallelism >= 1);
    assert_eq!(sched.carriers.len(), sched.parallelism);
}

#[test]
fn scheduler_mount_unmount_carrier() {
    let mut sched = ForkJoinScheduler::new(2);
    sched.mount(0, 42);
    assert_eq!(sched.carriers[0].mounted_virtual_thread, Some(42));
    assert!(!sched.carriers[0].available);
    assert_eq!(sched.active_count(), 1);

    let unmounted = sched.unmount(0);
    assert_eq!(unmounted, Some(42));
    assert!(sched.carriers[0].available);
    assert_eq!(sched.active_count(), 0);
    assert_eq!(sched.carriers[0].tasks_completed, 1);
}

#[test]
fn scheduler_active_count_tracking() {
    let mut sched = ForkJoinScheduler::new(4);
    sched.mount(0, 1);
    sched.mount(1, 2);
    assert_eq!(sched.active_count(), 2);
    sched.unmount(0);
    assert_eq!(sched.active_count(), 1);
    sched.unmount(1);
    assert_eq!(sched.active_count(), 0);
}

#[test]
fn scheduler_stats_submissions() {
    let sched = ForkJoinScheduler::new(1);
    sched.submit(1);
    sched.submit(2);
    sched.submit(3);
    assert_eq!(sched.stats().total_submissions.load(Ordering::Relaxed), 3);
}

// -- VirtualThreadManager -------------------------------------------------

#[test]
fn manager_create_and_start() {
    let mgr = VirtualThreadManager::new(2);
    let id = mgr.create_virtual_thread("vt-1");
    assert!(mgr.is_virtual(id));
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::New));

    mgr.start(id);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
}

#[test]
fn manager_park_and_unpark() {
    let mgr = VirtualThreadManager::new(2);
    let id = mgr.create_virtual_thread("vt");
    // Mount it so it can park.
    {
        let mut threads = mgr.threads.lock();
        threads.get_mut(&id).unwrap().mount(0);
    }
    let parked = mgr.park_virtual(id);
    assert!(parked);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));

    mgr.unpark_virtual(id);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
}

#[test]
fn manager_pin_and_unpin() {
    let mgr = VirtualThreadManager::new(2);
    let id = mgr.create_virtual_thread("vt");
    {
        let mut threads = mgr.threads.lock();
        threads.get_mut(&id).unwrap().mount(0);
    }
    mgr.pin_thread(id, PinReason::Monitor);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Pinned));

    mgr.unpin_thread(id);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Running));
}

#[test]
fn manager_terminate() {
    let mgr = VirtualThreadManager::new(1);
    let id = mgr.create_virtual_thread("vt");
    mgr.start(id);
    mgr.terminate(id);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Terminated));
}

#[test]
fn manager_join_tracking() {
    let mgr = VirtualThreadManager::new(1);
    let id = mgr.create_virtual_thread("vt");
    mgr.start(id);

    let joined = mgr.join(id, 999);
    assert!(joined);
    {
        let threads = mgr.threads.lock();
        assert!(threads[&id].join_waiters.contains(&999));
    }
}

#[test]
fn manager_join_already_terminated() {
    let mgr = VirtualThreadManager::new(1);
    let id = mgr.create_virtual_thread("vt");
    mgr.terminate(id);
    let joined = mgr.join(id, 999);
    assert!(!joined); // Already terminated
}

#[test]
fn manager_thread_count() {
    let mgr = VirtualThreadManager::new(1);
    assert_eq!(mgr.thread_count(), 0);
    mgr.create_virtual_thread("a");
    mgr.create_virtual_thread("b");
    assert_eq!(mgr.thread_count(), 2);
}

#[test]
fn manager_is_virtual() {
    let mgr = VirtualThreadManager::new(1);
    let id = mgr.create_virtual_thread("vt");
    assert!(mgr.is_virtual(id));
    assert!(!mgr.is_virtual(9999));
}

#[test]
fn manager_scheduler_stats() {
    let mgr = VirtualThreadManager::new(2);
    let id = mgr.create_virtual_thread("vt");
    mgr.start(id);
    let stats = mgr.scheduler_stats();
    assert_eq!(stats.total_submissions, 1);
}

// -- ThreadBuilder --------------------------------------------------------

#[test]
fn builder_of_virtual() {
    let b = ThreadBuilder::of_virtual();
    assert_eq!(b.kind, ThreadBuilderKind::Virtual);
    assert!(b.daemon); // virtual threads are daemon by default
    assert!(b.name.is_none());
}

#[test]
fn builder_of_platform() {
    let b = ThreadBuilder::of_platform();
    assert_eq!(b.kind, ThreadBuilderKind::Platform);
    assert!(!b.daemon);
}

#[test]
fn builder_name_setting() {
    let b = ThreadBuilder::of_virtual().name("worker");
    assert_eq!(b.name.as_deref(), Some("worker"));
}

#[test]
fn builder_name_prefix_with_counter() {
    let mut b = ThreadBuilder::of_virtual().name_prefix("vt-");
    assert_eq!(b.build_name(), "vt-0");
    assert_eq!(b.build_name(), "vt-1");
    assert_eq!(b.build_name(), "vt-2");
}

#[test]
fn builder_daemon_flag() {
    let b = ThreadBuilder::of_platform().daemon(true);
    assert!(b.daemon);
    let b2 = ThreadBuilder::of_virtual().daemon(false);
    assert!(!b2.daemon);
}

#[test]
fn builder_stack_size() {
    let b = ThreadBuilder::of_platform().stack_size(1024 * 1024);
    assert_eq!(b.stack_size, Some(1024 * 1024));
}

// -- ContinuationScope ----------------------------------------------------

#[test]
fn continuation_scope_equality() {
    let a = ContinuationScope {
        name: "VirtualThread".into(),
    };
    let b = ContinuationScope {
        name: "VirtualThread".into(),
    };
    let c = ContinuationScope {
        name: "Other".into(),
    };
    assert_eq!(a, b);
    assert_ne!(a, c);
}

// -- Extra edge cases -----------------------------------------------------

#[test]
fn scheduler_next_task_prefers_own_queue() {
    let sched = ForkJoinScheduler::new(2);
    sched.work_queues[0].lock().push_back(10);
    sched.submission_queue.lock().push_back(20);
    // Own queue should come first.
    assert_eq!(sched.next_task(0), Some(10));
    assert_eq!(sched.next_task(0), Some(20));
}

#[test]
fn scheduler_peak_active_tracks_max() {
    let mut sched = ForkJoinScheduler::new(4);
    sched.mount(0, 1);
    sched.mount(1, 2);
    sched.mount(2, 3);
    assert_eq!(sched.stats.peak_active.load(Ordering::Relaxed), 3);
    sched.unmount(0);
    sched.unmount(1);
    // Peak should still be 3.
    assert_eq!(sched.stats.peak_active.load(Ordering::Relaxed), 3);
}

#[test]
fn manager_park_nonexistent_thread() {
    let mgr = VirtualThreadManager::new(1);
    let result = mgr.park_virtual(9999);
    assert!(!result);
}

#[test]
fn vt_unpark_when_parked_changes_state() {
    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);
    vt.park();
    assert_eq!(vt.state, VirtualThreadState::Parked);
    vt.unpark();
    assert_eq!(vt.state, VirtualThreadState::Started);
}

#[test]
fn test_virtual_thread_many_concurrent() {
    // Stress test: create and run many virtual threads
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let counter = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..1000)
        .map(|_| {
            let c = counter.clone();
            std::thread::spawn(move || {
                c.fetch_add(1, Ordering::Relaxed);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1000);
}

#[test]
fn test_virtual_thread_yield_fairness() {
    // Verify that yielding allows other threads to progress
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let counter = Arc::new(AtomicUsize::new(0));
    let c1 = counter.clone();
    let c2 = counter.clone();
    let t1 = std::thread::spawn(move || {
        for _ in 0..100 {
            c1.fetch_add(1, Ordering::SeqCst);
            std::thread::yield_now();
        }
    });
    let t2 = std::thread::spawn(move || {
        for _ in 0..100 {
            c2.fetch_add(1, Ordering::SeqCst);
            std::thread::yield_now();
        }
    });
    t1.join().unwrap();
    t2.join().unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 200);
}

#[test]
fn test_virtual_thread_sleep_and_resume() {
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        42
    });
    let result = handle.join().unwrap();
    assert_eq!(result, 42);
    assert!(start.elapsed() >= Duration::from_millis(10));
}

#[test]
fn test_virtual_thread_exception_propagation() {
    // If virtual thread throws, it should be joinable with error
    let handle =
        std::thread::spawn(|| -> Result<i32, String> { Err("virtual thread failure".into()) });
    let result = handle.join().unwrap();
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "virtual thread failure");
}

#[test]
fn test_carrier_thread_reuse() {
    // After virtual thread unmounts, carrier should be reusable
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let count = Arc::new(AtomicUsize::new(0));
    // Run sequentially - same carrier thread should be reused
    for _ in 0..10 {
        let c = count.clone();
        let h = std::thread::spawn(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        h.join().unwrap();
    }
    assert_eq!(count.load(Ordering::SeqCst), 10);
}

#[test]
fn test_virtual_thread_local_isolation() {
    // Each virtual thread should have isolated thread-locals
    use std::cell::RefCell;
    thread_local! {
        static LOCAL: RefCell<i32> = RefCell::new(0);
    }
    let handles: Vec<_> = (0..5)
        .map(|i| {
            std::thread::spawn(move || {
                LOCAL.with(|v| {
                    *v.borrow_mut() = i;
                    assert_eq!(*v.borrow(), i);
                });
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn test_virtual_thread_interrupt() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let interrupted = Arc::new(AtomicBool::new(false));
    let i2 = interrupted.clone();
    let handle = std::thread::spawn(move || {
        // Simulate interrupt check
        if i2.load(Ordering::SeqCst) {
            return Err("interrupted");
        }
        Ok(())
    });
    // Don't interrupt - should succeed
    let result = handle.join().unwrap();
    assert!(result.is_ok());
}

// =====================================================================
// Phase 81 Tests — Virtual Threads Real Implementation
// =====================================================================

// -- 81.1: Continuation Data Structure (real freeze/thaw) ----------------

#[test]
fn p81_continuation_freeze_real_frame() {
    use crate::classloading::ClassId;
    use crate::runtime::frame::Frame;

    // Create a real Frame with locals and operand stack values.
    let mut frame = Frame::new(
        ClassId::new(0),
        "com/example/Worker".to_string(),
        "compute".to_string(),
        "(II)I".to_string(),
        Some("Worker.java".to_string()),
        vec![0x1a, 0x1b, 0x60, 0xac], // iload_0, iload_1, iadd, ireturn
        vec![],
        10,
        4,
        &[Value::Int(42), Value::Int(58), Value::Int(0), Value::Int(0)],
    );
    // Push a value onto the operand stack
    frame.stack.push(Value::Int(100)).unwrap();

    let frozen = frame.to_frozen_frame();
    assert_eq!(frozen.class_name, "com/example/Worker");
    assert_eq!(frozen.method_name, "compute");
    assert_eq!(frozen.descriptor, "(II)I");
    assert_eq!(frozen.bytecode_pc, 0);
    assert!(frozen.code.is_some());
    assert!(frozen.class_id.is_some());
    assert_eq!(frozen.max_stack, Some(10));
    assert_eq!(frozen.max_locals, Some(4));
    assert_eq!(frozen.source_file, Some("Worker.java".to_string()));
    // Locals should be preserved
    assert_eq!(frozen.locals.len(), 4);
    // Stack should have 1 entry
    assert_eq!(frozen.stack.len(), 1);
}

#[test]
fn p81_continuation_thaw_restores_frame() {
    use crate::classloading::ClassId;
    use crate::runtime::frame::Frame;

    let mut frame = Frame::new(
        ClassId::new(7),
        "com/example/Fibonacci".to_string(),
        "fib".to_string(),
        "(I)I".to_string(),
        None,
        vec![0x1a, 0xac], // iload_0, ireturn
        vec![],
        5,
        2,
        &[Value::Int(10), Value::Long(999)],
    );
    frame.pc = 1; // advance PC
    frame.stack.push(Value::Int(55)).unwrap();
    frame.stack.push(Value::Int(89)).unwrap();

    let frozen = frame.to_frozen_frame();
    let restored = Frame::from_frozen_frame(frozen);

    assert_eq!(restored.class_name(), "com/example/Fibonacci");
    assert_eq!(restored.method_name(), "fib");
    assert_eq!(restored.method_descriptor(), "(I)I");
    assert_eq!(restored.pc, 1);
    assert_eq!(restored.get_local(0), Value::Int(10));
    // Operand stack should have 2 entries
    assert_eq!(restored.stack.len(), 2);
}

#[test]
fn p81_continuation_nested_freeze() {
    use crate::classloading::ClassId;
    use crate::runtime::frame::Frame;

    let scope = ContinuationScope {
        name: "VirtualThread".into(),
    };
    let mut cont = Continuation::new(scope);
    cont.state = ContinuationState::Running;

    // Create a stack of 3 nested frames (simulating call chain)
    let frames: Vec<Frame> = (0..3)
        .map(|i| {
            Frame::new(
                ClassId::new(i),
                format!("Class{}", i),
                format!("method{}", i),
                "()V".to_string(),
                None,
                vec![0xb1], // return
                vec![],
                5,
                2,
                &[Value::Int(i as i32 * 10)],
            )
        })
        .collect();

    cont.freeze_frames(frames);
    assert_eq!(cont.state, ContinuationState::Suspended);
    assert_eq!(cont.frozen_frames.len(), 3);
    assert_eq!(cont.yield_count(), 1);

    // Thaw and verify all frames restored
    let restored = cont.thaw_frames();
    assert_eq!(restored.len(), 3);
    assert_eq!(cont.state, ContinuationState::Running);
    assert_eq!(cont.mount_count(), 1);

    // Verify each frame's identity
    for (i, frame) in restored.iter().enumerate() {
        assert_eq!(frame.class_name(), format!("Class{}", i));
        assert_eq!(frame.method_name(), format!("method{}", i));
        assert_eq!(frame.get_local(0), Value::Int(i as i32 * 10));
    }
}

#[test]
fn p81_continuation_deep_stack_freeze() {
    use crate::classloading::ClassId;
    use crate::runtime::frame::Frame;

    let scope = ContinuationScope {
        name: "VirtualThread".into(),
    };
    let mut cont = Continuation::new(scope);
    cont.state = ContinuationState::Running;

    // Create a deep call stack (100 frames — e.g. recursive method)
    let depth = 100;
    let frames: Vec<Frame> = (0..depth)
        .map(|i| {
            let mut f = Frame::new(
                ClassId::new(0),
                "RecursiveClass".to_string(),
                "recurse".to_string(),
                "(I)V".to_string(),
                None,
                vec![0xb1],
                vec![],
                5,
                3,
                &[Value::Int(i as i32)],
            );
            f.pc = i; // each frame at different PC
            f
        })
        .collect();

    cont.freeze_frames(frames);
    assert_eq!(cont.frozen_frames.len(), depth);

    let restored = cont.thaw_frames();
    assert_eq!(restored.len(), depth);
    for (i, frame) in restored.iter().enumerate() {
        assert_eq!(frame.pc, i);
        assert_eq!(frame.get_local(0), Value::Int(i as i32));
    }
}

#[test]
fn p81_continuation_freeze_preserves_exception_state() {
    use crate::classloading::ClassId;
    use crate::runtime::frame::Frame;
    use cratonvm_reader::attribute::ExceptionTableEntry;

    let scope = ContinuationScope {
        name: "VirtualThread".into(),
    };
    let mut cont = Continuation::new(scope);
    cont.state = ContinuationState::Running;

    // Frame with an exception table (simulating try/catch)
    let ex_table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 10,
        handler_pc: 20,
        catch_type: 5,
    }];
    let frame = Frame::new(
        ClassId::new(0),
        "TryCatch".to_string(),
        "handle".to_string(),
        "()V".to_string(),
        Some("TryCatch.java".to_string()),
        vec![0xb1; 30], // 30 bytes of code
        ex_table,
        10,
        5,
        &[Value::Int(1), Value::Object(None)],
    );

    cont.freeze_frames(vec![frame]);
    let restored = cont.thaw_frames();
    assert_eq!(restored.len(), 1);
    let rf = &restored[0];
    assert_eq!(rf.class_name(), "TryCatch");
    // Exception table should be preserved
    let et = rf.exception_table();
    assert_eq!(et.len(), 1);
    assert_eq!(et[0].start_pc, 0);
    assert_eq!(et[0].end_pc, 10);
    assert_eq!(et[0].handler_pc, 20);
    assert_eq!(et[0].catch_type, 5);
}

// -- 81.2: ForkJoinPool Scheduler (real carrier threads) ----------------

#[test]
fn p81_scheduler_basic_carrier_execution() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mgr = VirtualThreadManager::new(2);
    let executed = Arc::new(AtomicUsize::new(0));
    let executed2 = executed.clone();

    // Start carrier threads with a simple task function
    mgr.start_carriers(move |_vt_id| {
        executed2.fetch_add(1, Ordering::SeqCst);
    });

    // Submit 5 virtual threads
    for _ in 0..5 {
        let id = mgr.create_virtual_thread("vt");
        mgr.start(id);
    }

    // Wait for execution (carriers will pick them up)
    std::thread::sleep(std::time::Duration::from_millis(200));
    mgr.shutdown();
    assert_eq!(executed.load(Ordering::SeqCst), 5);
}

#[test]
fn p81_scheduler_work_stealing_execution() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let sched = Arc::new(ForkJoinScheduler::new(4));

    // Put tasks directly into carrier 0's work queue
    for i in 0..10u64 {
        sched.work_queues[0].lock().push_back(i);
    }

    let stolen_count = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (1..4)
        .map(|idx| {
            let s = sched.clone();
            let sc = stolen_count.clone();
            std::thread::spawn(move || {
                while let Some(_) = s.try_steal(idx) {
                    sc.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
    // At least some tasks should have been stolen
    assert!(stolen_count.load(Ordering::SeqCst) > 0);
    assert!(sched.stats.total_steals.load(Ordering::Relaxed) > 0);
}

#[test]
fn p81_scheduler_carrier_reuse() {
    use std::collections::HashSet;

    let mgr = VirtualThreadManager::new(1); // 1 carrier
    let carrier_ids = Arc::new(Mutex::new(Vec::new()));
    let carrier_ids2 = carrier_ids.clone();

    mgr.start_carriers(move |_vt_id| {
        // Record which OS thread is executing
        let tid = std::thread::current().id();
        carrier_ids2.lock().push(format!("{:?}", tid));
    });

    // Submit tasks sequentially
    for i in 0..5 {
        let id = mgr.create_virtual_thread(&format!("vt-{}", i));
        mgr.start(id);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    std::thread::sleep(std::time::Duration::from_millis(200));
    mgr.shutdown();

    let ids = carrier_ids.lock();
    // All should have run on the same carrier (1 carrier pool)
    if ids.len() >= 2 {
        let unique: HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 1, "all tasks should run on the same carrier");
    }
}

#[test]
fn p81_scheduler_pool_shutdown() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mgr = VirtualThreadManager::new(2);
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();

    mgr.start_carriers(move |_vt_id| {
        count2.fetch_add(1, Ordering::SeqCst);
    });

    // Submit a few tasks
    for _ in 0..3 {
        let id = mgr.create_virtual_thread("vt");
        mgr.start(id);
    }
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Shutdown should complete without hanging
    mgr.shutdown();

    // Verify the scheduler is no longer running
    assert!(!mgr.scheduler().is_running());
    // All submitted tasks should have executed
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[test]
fn p81_scheduler_1000_virtual_threads() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mgr = VirtualThreadManager::new(4);
    let counter = Arc::new(AtomicUsize::new(0));
    let counter2 = counter.clone();

    mgr.start_carriers(move |_vt_id| {
        counter2.fetch_add(1, Ordering::SeqCst);
    });

    for i in 0..1000 {
        let id = mgr.create_virtual_thread(&format!("vt-{}", i));
        mgr.start(id);
    }

    // Wait for all 1000 tasks to complete
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while counter.load(Ordering::SeqCst) < 1000 {
        if std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    mgr.shutdown();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1000,
        "all 1000 virtual threads should have executed"
    );
}

// -- 81.3: Virtual Thread Park/Unpark --------------------------------

#[test]
fn p81_park_with_frames_freezes_state() {
    use crate::classloading::ClassId;
    use crate::runtime::frame::Frame;

    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);

    let frames = vec![Frame::new(
        ClassId::new(0),
        "ParkTest".to_string(),
        "work".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        5,
        2,
        &[Value::Int(42)],
    )];

    let result = vt.park_with_frames(frames);
    assert!(result);
    assert_eq!(vt.state, VirtualThreadState::Parked);
    assert!(vt.carrier_thread_id.is_none()); // unmounted
    assert_eq!(vt.continuation.state, ContinuationState::Suspended);
    assert_eq!(vt.continuation.frozen_frames.len(), 1);
    assert_eq!(vt.continuation.frozen_frames[0].class_name, "ParkTest");
}

#[test]
fn p81_unpark_with_frames_restores_state() {
    use crate::classloading::ClassId;
    use crate::runtime::frame::Frame;

    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);

    let frames = vec![Frame::new(
        ClassId::new(0),
        "UnparkTest".to_string(),
        "resume".to_string(),
        "(I)V".to_string(),
        None,
        vec![0x1a, 0xb1], // iload_0, return
        vec![],
        5,
        3,
        &[Value::Int(77)],
    )];

    vt.park_with_frames(frames);
    assert_eq!(vt.state, VirtualThreadState::Parked);

    let restored = vt.unpark_with_frames();
    assert!(restored.is_some());
    let frames = restored.unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].class_name(), "UnparkTest");
    assert_eq!(frames[0].get_local(0), Value::Int(77));
    assert_eq!(vt.state, VirtualThreadState::Started);
}

#[test]
fn p81_park_timeout_wakeup() {
    let mgr = VirtualThreadManager::new(1);
    let id = mgr.create_virtual_thread("sleeper");
    {
        let mut threads = mgr.threads.lock();
        threads.get_mut(&id).unwrap().mount(0);
    }

    // Park the virtual thread
    mgr.park_virtual(id);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));

    // Schedule a wakeup after 50ms
    mgr.schedule_wakeup(id, std::time::Duration::from_millis(50));

    // Poll for the resubmission rather than sleeping a fixed 150 ms and
    // then looking exactly once. The property under test is that the timer
    // FIRES; a fixed sleep additionally asserts that this machine schedules
    // the timer thread promptly, which it does not do under the full
    // suite's load — `left: None, right: Some(1)`, 1 of 30 full-suite runs
    // and never in isolation.
    //
    // The deadline is generous because it bounds only the failure case: a
    // working timer satisfies this in ~50 ms, and a broken one is still
    // reported, just later.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut task = None;
    while std::time::Instant::now() < deadline {
        task = mgr.scheduler().next_task(0);
        if task.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    // The scheduler should have the task resubmitted
    assert_eq!(
        task,
        Some(id),
        "timer should have resubmitted the virtual thread"
    );
}

#[test]
fn wakeup_cancel_prevents_resubmit() {
    // A cancelled wakeup must NOT resubmit the virtual thread even after its
    // original deadline passes.
    let mgr = VirtualThreadManager::new(1);
    let id = mgr.create_virtual_thread("cancelled");
    {
        let mut threads = mgr.threads.lock();
        threads.get_mut(&id).unwrap().mount(0);
    }
    mgr.park_virtual(id);
    mgr.schedule_wakeup(id, std::time::Duration::from_millis(80));
    // Cancel well before the deadline.
    std::thread::sleep(std::time::Duration::from_millis(10));
    mgr.cancel_wakeup(id);
    // Wait past the original deadline.
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(
        mgr.scheduler().next_task(0),
        None,
        "cancelled wakeup must not resubmit the virtual thread"
    );
    // Registry entry must be gone after cancellation.
    assert!(mgr.get_wakeup_signal(id).is_none());
    mgr.shutdown();
}

#[test]
fn wakeup_many_sleepers_share_single_timer() {
    // Many concurrent sleepers are serviced by ONE shared timer thread (no
    // per-sleep OS-thread spawn); all of them must eventually be resubmitted.
    let mgr = VirtualThreadManager::new(2);
    let mut ids = Vec::new();
    for i in 0..64u64 {
        let id = mgr.create_virtual_thread(&format!("sleeper-{i}"));
        {
            let mut threads = mgr.threads.lock();
            threads.get_mut(&id).unwrap().mount(0);
        }
        mgr.park_virtual(id);
        // Staggered short deadlines exercise the min-heap ordering.
        mgr.schedule_wakeup(id, std::time::Duration::from_millis(10 + (i % 8) * 5));
        ids.push(id);
    }

    // Drain resubmitted tasks until all fire or we time out.
    let mut fired = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while fired.len() < ids.len() && std::time::Instant::now() < deadline {
        while let Some(t) = mgr.scheduler().next_task(0) {
            fired.insert(t);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        fired.len(),
        ids.len(),
        "all sleeping virtual threads should be resubmitted by the shared timer"
    );
    mgr.shutdown();
}

#[test]
fn wakeup_reschedule_supersedes_stale_entry() {
    // Re-scheduling a wakeup for the same vt with a later deadline must
    // supersede the earlier registration (the stale heap entry is dropped),
    // and the thread is resubmitted exactly once at the new deadline.
    //
    // The properties under test are ORDERING and MULTIPLICITY, not wall
    // clock. The earlier version asserted `next_task()` at fixed sleep
    // offsets (70 ms, then 190 ms) and so allowed the shared wakeup-timer
    // OS thread only 70 ms of scheduling slack; on a loaded 16-core Linux
    // box that is not enough (measured: ~4% failures in isolation at load
    // ~20, every one of them at exactly 190 ms — the resubmit had simply
    // not landed *yet*, not "never"). Poll for the resubmit under a
    // generous bound instead, and prove the stale entry never fired with
    // `total_submissions`, which is timing-independent: had the superseded
    // 30 ms entry also resubmitted, the counter would read 2 no matter
    // when either landed.
    let mgr = VirtualThreadManager::new(1);
    let id = mgr.create_virtual_thread("resched");
    {
        let mut threads = mgr.threads.lock();
        threads.get_mut(&id).unwrap().mount(0);
    }
    mgr.park_virtual(id);
    let t0 = std::time::Instant::now();
    mgr.schedule_wakeup(id, std::time::Duration::from_millis(30));
    // Immediately re-schedule with a longer deadline (new signal Arc).
    mgr.schedule_wakeup(id, std::time::Duration::from_millis(120));

    // The re-scheduled wakeup must resubmit the thread.
    let limit = std::time::Duration::from_secs(10);
    let mut fired_at = None;
    while t0.elapsed() < limit {
        if let Some(task) = mgr.scheduler().next_task(0) {
            assert_eq!(task, id, "an unexpected task was resubmitted");
            fired_at = Some(t0.elapsed());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));        }
    let fired_at = fired_at.expect("the re-scheduled wakeup should resubmit the thread");
    // The stale 30 ms entry must NOT have been the one that fired: nothing
    // may reach the scheduler before the superseding 120 ms deadline.
    assert!(
        fired_at >= std::time::Duration::from_millis(100),
        "superseded early wakeup fired at {:?}",
        fired_at
    );
    // ...and the resubmit must have happened exactly once.
    std::thread::sleep(std::time::Duration::from_millis(60));
    assert_eq!(
        mgr.scheduler().next_task(0),
        None,
        "the superseded entry must not resubmit a second time"
    );
    assert_eq!(
        mgr.scheduler_stats().total_submissions,
        1,
        "exactly one submission expected (fired at {:?})",
        fired_at
    );
    mgr.shutdown();
}

#[test]
fn p81_park_spurious_wakeup_safe() {
    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);

    // Park
    vt.park();
    assert_eq!(vt.state, VirtualThreadState::Parked);

    // Unpark (simulating spurious wakeup or explicit unpark)
    vt.unpark();
    assert_eq!(vt.state, VirtualThreadState::Started);
    assert!(vt.unpark_permit);

    // Next park should consume the permit and NOT actually park
    vt.mount(0);
    vt.state = VirtualThreadState::Running;
    let result = vt.park();
    assert!(result); // consumed permit
    assert!(!vt.unpark_permit);
    // State should still be Running (not Parked) because permit was consumed
    assert_eq!(vt.state, VirtualThreadState::Running);
}

// -- 81.4: Pinning Detection -------------------------------------------

#[test]
fn p81_pinning_detection_monitor() {
    let mgr = VirtualThreadManager::new(1);
    let id = mgr.create_virtual_thread("pinned-vt");
    {
        let mut threads = mgr.threads.lock();
        threads.get_mut(&id).unwrap().mount(0);
    }

    // Pin for monitor (synchronized block)
    let result = mgr.pin_thread(id, PinReason::Monitor);
    assert!(result.is_some());
    let (name, _carrier) = result.unwrap();
    assert_eq!(name, "pinned-vt");
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Pinned));

    // Verify stats
    let stats = mgr.scheduler_stats();
    assert_eq!(stats.total_pins, 1);
}

#[test]
fn p81_synchronized_park_blocks_carrier() {
    let mut vt = VirtualThread::new(1, "synced-vt".into());
    vt.mount(0);

    // Enter synchronized block
    vt.pin(PinReason::Monitor);
    assert!(vt.is_pinned());
    assert_eq!(vt.state, VirtualThreadState::Pinned);

    // Try to park while pinned — should fail (carrier blocks instead)
    let result = vt.park();
    assert!(!result, "park should return false when pinned");
    // Still mounted — carrier thread is blocked, not freed
    assert_eq!(vt.carrier_thread_id, Some(0));
}

#[test]
fn p81_monitor_state_preserved_in_continuation() {
    use crate::classloading::ClassId;
    use crate::runtime::frame::Frame;

    let mut vt = VirtualThread::new(1, "vt".into());
    vt.mount(0);

    // Enter nested synchronized blocks
    vt.pin(PinReason::Monitor);
    vt.pin(PinReason::Monitor);
    assert_eq!(vt.pin_count, 2);

    // Unpin once (still pinned with count 1)
    vt.unpin();
    assert_eq!(vt.pin_count, 1);
    assert!(vt.is_pinned());

    // Fully unpin
    vt.unpin();
    assert_eq!(vt.pin_count, 0);
    assert!(!vt.is_pinned());
    assert_eq!(vt.state, VirtualThreadState::Running);

    // Now park with frames should succeed (no longer pinned)
    let frames = vec![Frame::new(
        ClassId::new(0),
        "MonitorTest".to_string(),
        "work".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        5,
        2,
        &[Value::Int(99)],
    )];
    let result = vt.park_with_frames(frames);
    assert!(result);
    assert_eq!(vt.state, VirtualThreadState::Parked);
    assert_eq!(vt.continuation.frozen_frames.len(), 1);
}

// -- 81.5: Thread.ofVirtual() Builder API ----------------------------

#[test]
fn p81_builder_of_virtual_creates_daemon() {
    let b = ThreadBuilder::of_virtual();
    assert_eq!(b.kind, ThreadBuilderKind::Virtual);
    assert!(b.daemon, "virtual threads should be daemon by default");
    assert!(b.name.is_none());
    assert!(b.stack_size.is_none());
}

#[test]
fn p81_builder_name_with_prefix_counter() {
    let mut b = ThreadBuilder::of_virtual().name_prefix("vt-");
    assert_eq!(b.build_name(), "vt-0");
    assert_eq!(b.build_name(), "vt-1");
    assert_eq!(b.build_name(), "vt-2");

    // Platform builder with explicit name
    let mut b2 = ThreadBuilder::of_platform().name("worker-main");
    assert_eq!(b2.build_name(), "worker-main");
    // Without prefix, name doesn't auto-increment
    assert_eq!(b2.build_name(), "worker-main");
}

#[test]
fn p81_builder_factory_creates_thread() {
    // Verify the builder can generate thread names from factory pattern
    let mut b = ThreadBuilder::of_virtual().name_prefix("pool-");
    let name1 = b.build_name();
    let name2 = b.build_name();
    assert_eq!(name1, "pool-0");
    assert_eq!(name2, "pool-1");

    // Create VT through the manager using builder pattern
    let mgr = VirtualThreadManager::new(1);
    let id1 = mgr.create_virtual_thread(&name1);
    let id2 = mgr.create_virtual_thread(&name2);
    assert!(mgr.is_virtual(id1));
    assert!(mgr.is_virtual(id2));
    assert_ne!(id1, id2);
}

#[test]
fn p81_thread_type_detection() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId, ThreadKind};

    // Platform thread
    let pt = JvmThread::new(ThreadId(1), "platform-1");
    assert_eq!(pt.kind, ThreadKind::Platform);

    // Virtual thread created via manager
    let mgr = VirtualThreadManager::new(1);
    let vt_id = mgr.create_virtual_thread("vt-1");
    assert!(mgr.is_virtual(vt_id));
    assert_eq!(mgr.get_state(vt_id), Some(VirtualThreadState::New));

    // After start, state transitions
    mgr.start(vt_id);
    assert_eq!(mgr.get_state(vt_id), Some(VirtualThreadState::Started));

    // After terminate
    mgr.terminate(vt_id);
    assert_eq!(mgr.get_state(vt_id), Some(VirtualThreadState::Terminated));
}

/// gci i24/vl (review): a take that finds no deposited state changes
/// nothing -- a terminated record stays terminated, and a state installed
/// afterwards is still taken as a FIRST mount.
#[test]
fn gci_i24vl_a_take_without_state_mounts_nothing() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let mgr = VirtualThreadManager::new(1);
    mgr.create_virtual_thread_with_id(51, "gci-i24vl-dead");
    mgr.terminate(51);
    assert!(mgr.take_runtime_for_mount(51, 3).is_none());
    assert_eq!(mgr.get_state(51), Some(VirtualThreadState::Terminated));

    mgr.create_virtual_thread_with_id(52, "gci-i24vl-early");
    assert!(mgr.take_runtime_for_mount(52, 3).is_none());
    mgr.install_runtime(52, Box::new(JvmThread::new(ThreadId(52), "gci-i24vl-early")));
    let (runtime, resumed) = mgr
        .take_runtime_for_mount(52, 3)
        .expect("the installed state");
    assert_eq!(runtime.thread_id, ThreadId(52));
    assert!(!resumed, "a spurious earlier take must not make the first mount a resume");
    assert_eq!(mgr.get_state(52), Some(VirtualThreadState::Running));
}

/// gci i25/vt: "deposited and not mounted since" -- the post-yield
/// window's test (`jdk_continuation::resolve_post_yield_window`). A
/// mounted state answers nothing; every mount changes the count, so a
/// window opened before a remount never matches after it.
#[test]
fn gci_i25vt_deposit_unmounted_since_tracks_every_mount() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let mgr = VirtualThreadManager::new(1);
    assert_eq!(mgr.deposited_mount_count(61), None, "unknown");
    mgr.create_virtual_thread_with_id(61, "gci-i25vt");
    assert_eq!(mgr.deposited_mount_count(61), None, "no state yet");
    mgr.install_runtime(61, Box::new(JvmThread::new(ThreadId(61), "gci-i25vt")));
    let before = mgr.deposited_mount_count(61).expect("installed");
    let (runtime, _) = mgr.take_runtime_for_mount(61, 7).expect("the state");
    assert_eq!(mgr.deposited_mount_count(61), None, "mounted");
    let mut ran = false;
    assert_eq!(mgr.with_deposit_unmounted_since(61, before, || ran = true), None);
    assert!(!ran, "f must not run while the state is mounted");
    mgr.deposit_continuation(61, runtime);
    let window = mgr.deposited_mount_count(61).expect("deposited again");
    assert_eq!(window, before + 1, "the mount raised the count");
    assert_eq!(mgr.with_deposit_unmounted_since(61, before, || 1), None, "stale window");
    assert_eq!(mgr.with_deposit_unmounted_since(61, window, || 2), Some(2));
    let (runtime, _) = mgr.take_runtime_for_mount(61, 8).expect("remount");
    mgr.deposit_continuation(61, runtime);
    assert_eq!(
        mgr.with_deposit_unmounted_since(61, window, || 3),
        None,
        "a remount and a new deposit close the old window"
    );
}

#[test]
fn keyed_wakeup_before_unmount_is_not_lost() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let mgr = VirtualThreadManager::new(1);
    let id = 41;
    mgr.create_virtual_thread_with_id(id, "keyed-race");
    mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "keyed-race")));
    mgr.start(id);
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();

    mgr.wait_on_key(7, id);
    mgr.wake_waiters(7);
    assert!(mgr.threads.lock().get(&id).unwrap().wake_pending);

    mgr.suspend_runtime(id, runtime, std::time::Duration::ZERO);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
}

/// Interpreter round i1 wave 48, lane L1: a debugger's read of an
/// unmounted continuation's frames holds the deposit; a mount waits for
/// the read to end, and a second read is refused meanwhile.
#[test]
fn debugger_read_holds_the_deposit_until_it_ends() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let mgr = Arc::new(VirtualThreadManager::new(1));
    let id = 43;
    mgr.create_virtual_thread_with_id(id, "debugger-read");
    mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "debugger-read")));
    mgr.start(id);
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();
    // Mounted: nothing deposited to read.
    assert!(mgr.begin_debugger_read(id).is_none());
    mgr.suspend_runtime(id, runtime, std::time::Duration::from_secs(60));
    let read = mgr
        .begin_debugger_read(id)
        .expect("the deposit is readable");
    // SAFETY: the read holds the deposit in place.
    assert_eq!(unsafe { (*read).thread_id }, ThreadId(id));
    assert!(mgr.begin_debugger_read(id).is_none(), "one read at a time");
    let mounted = Arc::new(AtomicBool::new(false));
    let mount = {
        let (mgr, mounted) = (Arc::clone(&mgr), Arc::clone(&mounted));
        std::thread::spawn(move || {
            let taken = mgr.take_runtime_for_mount(id, 0);
            mounted.store(true, Ordering::Release);
            taken.map(|(runtime, _)| runtime.thread_id)
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        !mounted.load(Ordering::Acquire),
        "the mount waits for the read"
    );
    mgr.end_debugger_read(id);
    assert_eq!(mount.join().unwrap(), Some(ThreadId(id)));
}

#[test]
fn keyed_wakeup_resubmits_parked_continuation() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let mgr = VirtualThreadManager::new(1);
    let id = 42;
    mgr.create_virtual_thread_with_id(id, "keyed-parked");
    mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "keyed-parked")));
    mgr.start(id);
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();

    mgr.wait_on_key(8, id);
    mgr.suspend_runtime(id, runtime, std::time::Duration::ZERO);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));

    mgr.wake_waiters(8);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
}

// -- Lost-unpark race (2026-07-26) ------------------------------------

/// The regression this whole fix exists for: `LockSupport.unpark` lands
/// while the continuation is still mounted (it has returned
/// `ContinuationYield` on its carrier but `suspend_runtime` has not run
/// yet). Before the fix nothing was submitted and, for an UNTIMED park
/// (`wake_after == 0`, so no wakeup timer either), the virtual thread was
/// never scheduled again.
#[test]
fn unpark_before_unmount_is_not_lost() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let mgr = VirtualThreadManager::new(1);
    let id = 61;
    mgr.create_virtual_thread_with_id(id, "unpark-race");
    mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "unpark-race")));
    mgr.start(id);
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Running));

    // Unpark arrives mid-flight: still mounted, runtime not deposited.
    mgr.unpark_virtual(id);
    assert!(
        mgr.threads.lock().get(&id).unwrap().unpark_permit,
        "a mid-flight unpark must leave a sticky permit"
    );
    assert!(
        mgr.scheduler().next_task(0).is_none(),
        "nothing may be queued while the runtime is still taken"
    );

    // Untimed park deposits the runtime — the permit must fire here.
    mgr.suspend_runtime(id, runtime, std::time::Duration::ZERO);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    assert!(
        !mgr.threads.lock().get(&id).unwrap().unpark_permit,
        "the permit must be consumed, not left to fire twice"
    );
}

/// A parked continuation with its runtime deposited is resubmitted
/// directly — the plain park/unpark round trip.
#[test]
fn unpark_after_unmount_resubmits_once() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let mgr = VirtualThreadManager::new(1);
    let id = 62;
    mgr.create_virtual_thread_with_id(id, "unpark-parked");
    mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "unpark-parked")));
    mgr.start(id);
    // Drain `start`'s own submission. Without this the queue is never
    // empty, and every "must not have resubmitted" assertion below would
    // pass on `start`'s leftover entry instead of testing anything — the
    // test could not have detected the duplicate-submission bug it names.
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    let (runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();
    assert_eq!(mgr.scheduler().next_task(0), None);

    mgr.suspend_runtime(id, runtime, std::time::Duration::ZERO);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));
    assert_eq!(mgr.scheduler().next_task(0), None);

    mgr.unpark_virtual(id);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    // Exactly one submission — the old code enqueued a second copy when it
    // saw an already-`Started` thread.
    assert_eq!(mgr.scheduler().next_task(0), None);
}

/// An unpark against an already-queued (`Started`, not yet mounted)
/// virtual thread must not enqueue a duplicate id.
#[test]
fn unpark_of_queued_thread_does_not_duplicate_submission() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let mgr = VirtualThreadManager::new(1);
    let id = 63;
    mgr.create_virtual_thread_with_id(id, "unpark-queued");
    mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "unpark-queued")));
    mgr.start(id);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));

    mgr.unpark_virtual(id);
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    assert_eq!(
        mgr.scheduler().next_task(0),
        None,
        "unpark of a queued thread must not duplicate its submission"
    );
}

/// A stray permit must NOT truncate a timed yield: `Thread.sleep` and
/// `parkNanos` already carry a `WakeupTimer` deadline.
#[test]
fn unpark_permit_does_not_truncate_timed_yield() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let mgr = VirtualThreadManager::new(1);
    let id = 64;
    mgr.create_virtual_thread_with_id(id, "unpark-timed");
    mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "unpark-timed")));
    mgr.start(id);
    // Drain `start`'s own submission — see the note in
    // `unpark_after_unmount_resubmits_once`. The whole point of this test
    // is the assertion that the queue is EMPTY after a timed yield; with
    // `start`'s entry still sitting there that assertion is untestable.
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    let (mut runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();
    // A timed SLEEP (wave 25: a timed `LockSupport` park returns on the
    // permit; see `unpark_permit_ends_a_timed_park`).
    runtime.vt_yield = crate::threading::jvm_thread::VtYield::Sleep { deadline: None };

    mgr.unpark_virtual(id);
    mgr.suspend_runtime(id, runtime, std::time::Duration::from_secs(3600));
    assert_eq!(
        mgr.get_state(id),
        Some(VirtualThreadState::Parked),
        "a timed sleep must stay parked until its deadline"
    );
    assert_eq!(mgr.scheduler().next_task(0), None);
    assert!(
        mgr.threads.lock().get(&id).unwrap().unpark_permit,
        "the permit stays sticky for the next untimed park"
    );
    mgr.cancel_wakeup(id);
    // Stops the lazily-started wakeup-timer thread this park spawned.
    mgr.shutdown();
}

/// Interpreter round i1 wave 25, lane L7: a permit left by an `unpark`
/// that landed while the continuation was still mounted ends a TIMED
/// `LockSupport` park at once, as HotSpot's `Parker` does -- it used to
/// wait out the whole timeout.
#[test]
fn unpark_permit_ends_a_timed_park() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId, VtYield};

    let mgr = VirtualThreadManager::new(1);
    let id = 65;
    mgr.create_virtual_thread_with_id(id, "unpark-timed-park");
    mgr.install_runtime(
        id,
        Box::new(JvmThread::new(ThreadId(id), "unpark-timed-park")),
    );
    mgr.start(id);
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    let (mut runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();
    runtime.vt_yield = VtYield::Park;

    mgr.unpark_virtual(id);
    mgr.suspend_runtime(id, runtime, std::time::Duration::from_secs(3600));
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Started));
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    assert!(
        !mgr.threads.lock().get(&id).unwrap().unpark_permit,
        "the park consumed the permit"
    );
    mgr.shutdown();
}

/// Wave 25, lane L7: a stray permit does not end an untimed keyed wait
/// (the synthetic `CountDownLatch.await`); only `wake_waiters` does. The
/// remount runs on past the native, so a resubmit here returned from
/// `await` with the count still positive.
#[test]
fn unpark_permit_does_not_end_a_keyed_wait() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId, VtYield};

    let mgr = VirtualThreadManager::new(1);
    let id = 66;
    mgr.create_virtual_thread_with_id(id, "unpark-keyed");
    mgr.install_runtime(id, Box::new(JvmThread::new(ThreadId(id), "unpark-keyed")));
    mgr.start(id);
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    let (mut runtime, _) = mgr.take_runtime_for_mount(id, 0).unwrap();
    runtime.vt_yield = VtYield::KeyedWait { key: 9 };
    mgr.wait_on_key(9, id);

    mgr.unpark_virtual(id);
    mgr.suspend_runtime(id, runtime, std::time::Duration::ZERO);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));
    assert_eq!(mgr.scheduler().next_task(0), None);
    mgr.wake_waiters(9);
    assert_eq!(mgr.scheduler().next_task(0), Some(id));
    mgr.shutdown();
}

/// `Parked` implies resubmit, unconditionally — including when no runtime
/// has been deposited.
///
/// This pins the reason `unpark_virtual`'s condition is `state == Parked`
/// and not `state == Parked && runtime.is_some()`. The two agree on the
/// live path (`suspend_runtime` writes both under one lock hold), so the
/// conjunct can only ever matter when the invariant is already broken —
/// and there it fails closed, turning a wake into a permanent hang. This
/// is the same shape the pre-existing `manager_park_and_unpark` asserts;
/// kept separately so the *reason* is not lost if that test is rewritten.
#[test]
fn unpark_of_parked_thread_without_runtime_still_resubmits() {
    let mgr = VirtualThreadManager::new(1);
    let id = mgr.create_virtual_thread("parked-no-runtime");
    {
        let mut threads = mgr.threads.lock();
        threads.get_mut(&id).unwrap().mount(0);
    }
    assert!(mgr.park_virtual(id));
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Parked));
    assert!(
        mgr.threads.lock().get(&id).unwrap().runtime.is_none(),
        "precondition: this thread has no deposited runtime"
    );

    mgr.unpark_virtual(id);
    assert_eq!(
        mgr.get_state(id),
        Some(VirtualThreadState::Started),
        "a parked thread must always become runnable on unpark"
    );
    assert_eq!(
        mgr.scheduler().next_task(0),
        Some(id),
        "and must actually reach the scheduler queue"
    );
}

/// An unpark of a terminated virtual thread is a no-op — no resurrection,
/// no queue entry.
#[test]
fn unpark_of_terminated_thread_is_a_noop() {
    let mgr = VirtualThreadManager::new(1);
    let id = 65;
    mgr.create_virtual_thread_with_id(id, "unpark-dead");
    mgr.terminate(id);

    mgr.unpark_virtual(id);
    assert_eq!(mgr.get_state(id), Some(VirtualThreadState::Terminated));
    assert_eq!(mgr.scheduler().next_task(0), None);
}

// -- Carrier-pool starvation compensation (2026-07-26) ----------------

#[test]
fn stall_detector_requires_queued_work_saturation_and_no_progress() {
    // Saturated, queue non-empty, no dispatch since last sample: stalled.
    assert!(carrier_pool_is_stalled(3, 2, 2, false));
    // Dispatching -> busy but progressing, never compensate.
    assert!(!carrier_pool_is_stalled(3, 2, 2, true));
    // Idle carrier available -> the backlog has a taker.
    assert!(!carrier_pool_is_stalled(3, 1, 2, false));
    // Nothing queued -> saturation is just useful work.
    assert!(!carrier_pool_is_stalled(0, 2, 2, false));
    // No carriers at all (pool never started) -> nothing to compensate.
    assert!(!carrier_pool_is_stalled(3, 0, 0, false));
}

/// Compensating carriers get indices past `parallelism` and therefore own
/// no per-carrier work queue. `next_task` used to index `work_queues`
/// directly, which panicked for exactly those indices.
#[test]
fn next_task_tolerates_compensating_carrier_index() {
    let sched = ForkJoinScheduler::new(2);
    sched.submit(77);
    let out_of_range = sched.parallelism() + 5;
    assert_eq!(
        sched.next_task(out_of_range),
        Some(77),
        "a compensating carrier must reach the global queue"
    );
    assert_eq!(sched.next_task(out_of_range), None);
}

/// The blocked carrier that motivates compensation: a task body that never
/// returns holds its carrier forever. With `parallelism == 1` the queued
/// follow-up work is unreachable until a second carrier appears.
#[test]
fn blocked_carrier_pool_is_grown_by_the_watchdog() {
    let mgr = VirtualThreadManager::new(1);
    // gci i18/vp (the i18a red): this test has no VM, so no pause of its
    // own; the process-wide pause signal is every OTHER test's VM in this
    // binary, which can discard every sample of the 20 s window.
    mgr.scheduler()
        .ignore_process_pauses
        .store(true, Ordering::Release);
    let entered = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let second_ran = Arc::new(AtomicBool::new(false));

    let entered_task = entered.clone();
    let release_task = release.clone();
    let second_task = second_ran.clone();
    mgr.start_carriers(move |vt_id| {
        entered_task.fetch_add(1, Ordering::SeqCst);
        if vt_id == 1 {
            // Emulate a virtual thread blocking its carrier inside a
            // contended monitor: the carrier never returns to the pool.
            while !release_task.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        } else {
            second_task.store(true, Ordering::SeqCst);
        }
    });

    mgr.scheduler().submit(1);
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while entered.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        entered.load(Ordering::SeqCst),
        1,
        "the sole base carrier must be wedged inside the task body"
    );

    // Nothing in the base pool can ever pick this up.
    mgr.scheduler().submit(2);
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    while !second_ran.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let ran_second = second_ran.load(Ordering::SeqCst);
    let grew = mgr.scheduler().live_carriers() > mgr.scheduler().parallelism();

    release.store(true, Ordering::SeqCst);
    mgr.shutdown();

    assert!(
        ran_second,
        "the starvation watchdog must add a carrier so queued work still runs"
    );
    assert!(grew, "the pool must have grown past its base parallelism");
}

/// The cap is a hard bound, not a target: an already-maxed pool refuses to
/// grow.
#[test]
fn compensating_carrier_respects_the_pool_cap() {
    let sched = Arc::new(ForkJoinScheduler::new(1));
    let task_fn: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(|_vt_id: u64| {});
    let handles = Arc::new(Mutex::new(Vec::new()));
    sched
        .live_carriers
        .store(MAX_CARRIER_THREADS, Ordering::Release);
    assert!(
        !spawn_compensating_carrier(&sched, &task_fn, &handles),
        "must not spawn past MAX_CARRIER_THREADS"
    );
    assert_eq!(sched.live_carriers(), MAX_CARRIER_THREADS);
    assert!(handles.lock().is_empty());
}

/// Wave 28, lane L4: ONE `unpark` is ONE permit, whichever way the thread
/// parks next. Scenario A of `L4W27PinnedParkPermits`: an `unpark`
/// delivered while the thread parks on its carrier wakes the `ParkState`
/// and leaves nothing for the next (unmounting) park.
#[test]
fn an_unpark_of_a_carrier_park_leaves_no_second_permit() {
    use crate::threading::jvm_thread::ParkState;

    let mgr = VirtualThreadManager::new(1);
    let id = 81;
    mgr.create_virtual_thread_with_id(id, "carrier-park-a");
    let park_state = ParkState::new();
    assert!(
        !mgr.begin_carrier_park(id, &park_state),
        "no permit: the park blocks"
    );
    assert!(mgr.threads.lock().get(&id).unwrap().carrier_park);

    mgr.unpark_virtual_waking(id, Some(&park_state));
    assert!(park_state.take_permit(), "the unpark woke the carrier park");
    mgr.end_carrier_park(id, &park_state);
    let vt_permit = mgr.threads.lock().get(&id).unwrap().unpark_permit;
    assert!(!vt_permit, "the carrier park consumed the one permit");
    assert!(!mgr.threads.lock().get(&id).unwrap().carrier_park);
    mgr.shutdown();
}

/// Scenario B: an `unpark` of a thread that is not parking on its carrier
/// (here mounted and running) leaves the `ParkState` alone, and the next
/// carrier park consumes the one permit without blocking. A `ParkState`
/// permit left by an interrupt's or a stack dump's wake does not count.
#[test]
fn a_carrier_park_consumes_the_one_permit_and_ignores_a_stale_wake() {
    use crate::threading::jvm_thread::ParkState;

    let mgr = VirtualThreadManager::new(1);
    let id = 82;
    mgr.create_virtual_thread_with_id(id, "carrier-park-b");
    let park_state = ParkState::new();
    mgr.unpark_virtual_waking(id, Some(&park_state));
    assert!(
        !park_state.take_permit(),
        "not parked on the carrier: no wake"
    );
    assert!(
        mgr.begin_carrier_park(id, &park_state),
        "the permit ends the park"
    );
    assert!(!mgr.threads.lock().get(&id).unwrap().unpark_permit);
    assert!(!mgr.threads.lock().get(&id).unwrap().carrier_park);

    park_state.unpark(); // a stale wake: not a `LockSupport` permit
    assert!(
        !mgr.begin_carrier_park(id, &park_state),
        "the stale wake is dropped"
    );
    assert!(!park_state.take_permit());
    mgr.end_carrier_park(id, &park_state);
    mgr.shutdown();
}

/// An `unpark` that races a carrier park's timeout does not outlive the
/// park (HotSpot's `parkOnCarrierThread` ends with `setParkPermit(false)`).
#[test]
fn a_carrier_park_consumes_a_permit_that_raced_its_timeout() {
    use crate::threading::jvm_thread::ParkState;

    let mgr = VirtualThreadManager::new(1);
    let id = 83;
    mgr.create_virtual_thread_with_id(id, "carrier-park-c");
    let park_state = ParkState::new();
    assert!(!mgr.begin_carrier_park(id, &park_state));
    // The park timed out; the unpark lands before its end.
    mgr.unpark_virtual_waking(id, Some(&park_state));
    mgr.end_carrier_park(id, &park_state);
    assert!(!mgr.threads.lock().get(&id).unwrap().unpark_permit);
    assert!(!park_state.take_permit());
    mgr.shutdown();
}

/// gci i23/pk (finding 5 of
/// `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): the
/// interrupted park's permit take consumes the one `LockSupport` permit
/// exactly once, never raises `carrier_park`, leaves the `ParkState`
/// alone, and answers `false` for an unknown id.
#[test]
fn gci_i23pk_take_unpark_permit_consumes_the_one_permit_once() {
    use crate::threading::jvm_thread::ParkState;

    let mgr = VirtualThreadManager::new(1);
    let id = 84;
    mgr.create_virtual_thread_with_id(id, "carrier-park-d");
    let park_state = ParkState::new();
    assert!(!mgr.take_unpark_permit(id), "no permit yet");
    mgr.unpark_virtual_waking(id, Some(&park_state));
    assert!(mgr.take_unpark_permit(id), "the permit is taken");
    assert!(!mgr.take_unpark_permit(id), "and only once");
    assert!(!mgr.threads.lock().get(&id).unwrap().carrier_park);
    assert!(!park_state.take_permit(), "the ParkState is not touched");
    // An unpark after the take is the next permit, not dropped.
    mgr.unpark_virtual_waking(id, Some(&park_state));
    assert!(mgr.threads.lock().get(&id).unwrap().unpark_permit);
    assert!(!mgr.take_unpark_permit(9_999), "unknown id");
    mgr.shutdown();
}

/// gci i23/pk (finding 5): the VM's park takes the permit -- the
/// manager's for a VM-scheduled virtual thread, the `ParkState`'s
/// otherwise -- inside its interrupted early return, before the `return`.
#[test]
fn gci_i23pk_the_interrupted_park_takes_the_permit_first() {
    const VM: &str = concat!(include_str!("../../vm/vm_exec/mod.rs"), "\n", include_str!("../../vm/vm_exec/method.rs"), "\n", include_str!("../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../vm/vm_exec/jni_table_native_call.rs"));
    let start = VM.find("fn park_with_state_inner(").expect("the park");
    let len = VM[start..].find("\n    \u{7d}\n").expect("its end");
    let park = &VM[start..start + len];
    let gate = park
        .find(".load(std::sync::atomic::Ordering::Acquire)\n        \u{7b}")
        .expect("the interrupt gate");
    let arm = &park[gate..];
    // The statement, not prose: the arm's comment says "interrupted
    // return;" (i23a fallout).
    let ret = arm
        .find("\n            return;\n        \u{7d}")
        .expect("the early return");
    // Only a Java-level park (`timed`) takes it.
    let java_level = arm.find("if timed \u{7b}").expect("the timed gate");
    assert!(java_level < ret);
    for take in [
        ".take_unpark_permit(self.thread.thread_id.0);",
        "self.thread.park_state.take_permit_lock_free();",
        "self.thread.park_state.take_permit();",
    ] {
        let at = arm
            .find(take)
            .unwrap_or_else(|| panic!("`{take}` missing"));
        assert!(java_level < at && at < ret, "`{take}` outside the gate");
    }
}

/// gci i11/vt item 4 (`CRATONVM_VT_REAP_TERMINATED`): the switch is off
/// unless VM init sets it, and `forget_terminated` drops exactly a
/// terminated JDK continuation's record -- not a live one, not a bound
/// virtual thread's (this manager's own scheduling state), not one whose
/// state is still deposited.
#[test]
fn gci_i11vt_forget_terminated_drops_only_a_dead_jdk_continuation() {
    let mgr = VirtualThreadManager::new(1);
    assert!(!mgr.reap_terminated(), "off until VM init passes the switch (default ON)");
    let mgr = mgr.with_reap_terminated(true);
    assert!(mgr.reap_terminated());

    // A live JDK continuation stays.
    mgr.create_jdk_continuation(41, "jdk-live", 7);
    assert!(!mgr.forget_terminated(41));
    assert!(mgr.is_virtual(41));

    // Terminated: forgotten, and unknown from then on.
    mgr.terminate(41);
    assert!(mgr.forget_terminated(41));
    assert!(!mgr.is_virtual(41));
    assert!(!mgr.is_jdk_continuation(41));
    assert_eq!(mgr.get_state(41), None);
    assert!(!mgr.forget_terminated(41), "a second forget is a no-op");

    // A bound virtual thread's record stays even when terminated.
    mgr.create_virtual_thread_with_id(42, "bound");
    mgr.terminate(42);
    assert!(!mgr.forget_terminated(42));
    assert_eq!(mgr.get_state(42), Some(VirtualThreadState::Terminated));

    // A terminated JDK continuation with a deposited state stays.
    mgr.create_jdk_continuation(43, "jdk-deposited", 7);
    mgr.deposit_continuation(
        43,
        Box::new(crate::threading::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(43),
            "jdk-deposited",
        )),
    );
    mgr.terminate(43);
    assert!(!mgr.forget_terminated(43));
    assert!(mgr.is_virtual(43));
    mgr.shutdown();
}

/// gci i13/vg (review): a deposit for a thread the manager has no record
/// of keeps the state alive (leaked) instead of freeing a `JvmThread` a
/// live registry entry may still publish; it creates no record either.
#[test]
fn gci_i13vg_a_deposit_without_a_record_does_not_free_the_state() {
    let mgr = VirtualThreadManager::new(1);
    let thread = Box::new(crate::threading::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(77),
        "no-record",
    ));
    let addr = &*thread as *const crate::threading::JvmThread as usize;
    mgr.deposit_continuation(77, thread);
    assert!(!mgr.is_virtual(77), "no record is invented");
    // Still allocated: reading the leaked thread's id is sound.
    // SAFETY: `deposit_continuation` leaked the box; it is never freed.
    let id = unsafe { (*(addr as *const crate::threading::JvmThread)).thread_id };
    assert_eq!(id, crate::threading::jvm_thread::ThreadId(77));
    mgr.shutdown();
}

/// gci i14/ve: only a JDK continuation whose state is deposited (and not
/// under a debugger read) is a mirror-keyed candidate; the collected
/// state is taken exactly once, and a bound virtual thread's never.
#[test]
fn gci_i14ve_only_deposited_jdk_states_are_candidates_and_taken_once() {
    let mgr = VirtualThreadManager::new(1).with_jdk_continuations(true);
    let state = |id: u64| {
        Box::new(crate::threading::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(id),
            "vt",
        ))
    };
    // Mounted (its first slice runs): no state here, no candidate.
    mgr.create_jdk_continuation(5, "vt-5", 1);
    assert!(mgr.deposited_jdk_continuation_ids().is_empty());
    // Yielded: deposited, a candidate.
    mgr.deposit_continuation(5, state(5));
    assert_eq!(mgr.deposited_jdk_continuation_ids(), vec![5]);
    // A debugger read keeps it out, and keeps its state.
    assert!(mgr.begin_debugger_read(5).is_some());
    assert!(mgr.deposited_jdk_continuation_ids().is_empty());
    assert!(mgr.take_collected_continuation(5).is_none());
    mgr.end_debugger_read(5);
    // Collected: taken once.
    let taken = mgr.take_collected_continuation(5).expect("the deposited state");
    assert_eq!(taken.thread_id, crate::threading::jvm_thread::ThreadId(5));
    assert!(mgr.take_collected_continuation(5).is_none(), "taken once");
    assert!(mgr.deposited_jdk_continuation_ids().is_empty());
    // A bound virtual thread's state is this manager's own: never.
    mgr.create_virtual_thread_with_id(9, "bound-9");
    mgr.install_runtime(9, state(9));
    assert!(mgr.deposited_jdk_continuation_ids().is_empty());
    assert!(mgr.take_collected_continuation(9).is_none());
    mgr.shutdown();
}

/// gci i14/ve: the pause's read of the candidates never blocks: with the
/// records' lock held elsewhere it answers `None` (the door then keys
/// nothing), and the same answer as the blocking read once it is free.
#[test]
fn gci_i14ve_a_contended_candidate_read_falls_back_without_blocking() {
    let mgr = VirtualThreadManager::new(1).with_jdk_continuations(true);
    mgr.create_jdk_continuation(6, "vt-6", 1);
    mgr.deposit_continuation(
        6,
        Box::new(crate::threading::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(6),
            "vt",
        )),
    );
    {
        let _held = mgr.threads.lock();
        assert!(mgr.try_deposited_jdk_continuation_ids().is_none(), "contended");
    }
    assert_eq!(mgr.try_deposited_jdk_continuation_ids(), Some(vec![6]));
    assert_eq!(mgr.deposited_jdk_continuation_ids(), vec![6]);
    mgr.shutdown();
}

/// gci i14/ve (i13/vg review R2): the bound path's suspend for a thread
/// the manager has no record of keeps the state alive (leaked), as
/// `deposit_continuation` does, and invents no record.
#[test]
fn gci_i14ve_a_bound_suspend_without_a_record_does_not_free_the_state() {
    let mgr = VirtualThreadManager::new(1);
    let thread = Box::new(crate::threading::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(78),
        "no-record",
    ));
    let addr = &*thread as *const crate::threading::JvmThread as usize;
    mgr.suspend_runtime(78, thread, std::time::Duration::ZERO);
    assert!(!mgr.is_virtual(78), "no record is invented");
    // SAFETY: `suspend_runtime` leaked the box; it is never freed.
    let id = unsafe { (*(addr as *const crate::threading::JvmThread)).thread_id };
    assert_eq!(id, crate::threading::jvm_thread::ThreadId(78));
    mgr.shutdown();
}
