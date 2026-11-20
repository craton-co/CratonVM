use super::*;
use crate::event::{event_id, AwtEvent, PeerId};

fn make_edt() -> EventDispatchThread {
    EventDispatchThread::new()
}

#[test]
fn post_and_poll() {
    let edt = make_edt();
    assert!(edt.poll_event().is_none());

    let evt = AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 100);
    edt.post_event(evt);
    assert_eq!(edt.queue_length(), 1);

    let got = edt.poll_event().unwrap();
    assert_eq!(got.id, event_id::WINDOW_OPENED);
    assert_eq!(got.source_peer_id, PeerId(1));
    assert!(edt.poll_event().is_none());
}

#[test]
fn drain_events() {
    let edt = make_edt();
    edt.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
    edt.post_event(AwtEvent::window(event_id::WINDOW_CLOSING, PeerId(1), 1));
    edt.post_event(AwtEvent::window(event_id::WINDOW_CLOSED, PeerId(1), 2));

    let all = edt.drain_events();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, event_id::WINDOW_OPENED);
    assert_eq!(all[1].id, event_id::WINDOW_CLOSING);
    assert_eq!(all[2].id, event_id::WINDOW_CLOSED);
    assert_eq!(edt.queue_length(), 0);
}

#[test]
fn invoke_later_posts_invocation_event() {
    let edt = make_edt();
    edt.invoke_later(42, PeerId(7));

    let evt = edt.poll_event().unwrap();
    assert_eq!(evt.id, event_id::INVOCATION_DEFAULT);
    assert_eq!(evt.source_peer_id, PeerId(7));
    if let AwtEventData::Invocation { callback_id } = &evt.data {
        assert_eq!(*callback_id, 42);
    } else {
        panic!("expected Invocation");
    }
}

#[test]
fn start_stop_lifecycle() {
    let edt = make_edt();
    assert!(!edt.is_running());
    edt.start();
    assert!(edt.is_running());
    edt.stop();
    assert!(!edt.is_running());
}

#[test]
fn wait_event_returns_none_on_timeout() {
    let edt = make_edt();
    let result = edt.wait_event(10); // 10ms timeout
    assert!(result.is_none());
}

#[test]
fn wait_event_returns_existing() {
    let edt = make_edt();
    edt.post_event(AwtEvent::window(event_id::WINDOW_ACTIVATED, PeerId(1), 0));
    let evt = edt.wait_event(100).unwrap();
    assert_eq!(evt.id, event_id::WINDOW_ACTIVATED);
}

#[test]
fn coalesce_paint_events_same_peer() {
    let edt = make_edt();
    let peer = PeerId(10);

    // Post two paint events for the same peer with overlapping rects.
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 0, 0, 0, 50, 50));
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 1, 25, 25, 50, 50));

    edt.coalesce_paint_events();

    assert_eq!(edt.queue_length(), 1);
    let evt = edt.poll_event().unwrap();
    if let AwtEventData::Paint {
        x,
        y,
        width,
        height,
    } = &evt.data
    {
        // Bounding union of (0,0,50,50) and (25,25,50,50) = (0,0,75,75)
        assert_eq!((*x, *y, *width, *height), (0, 0, 75, 75));
    } else {
        panic!("expected Paint");
    }
}

#[test]
fn coalesce_does_not_merge_different_peers() {
    let edt = make_edt();
    edt.post_event(AwtEvent::paint(event_id::PAINT, PeerId(1), 0, 0, 0, 50, 50));
    edt.post_event(AwtEvent::paint(event_id::PAINT, PeerId(2), 1, 0, 0, 50, 50));

    edt.coalesce_paint_events();
    assert_eq!(edt.queue_length(), 2);
}

#[test]
fn coalesce_preserves_non_paint_events() {
    let edt = make_edt();
    let peer = PeerId(1);
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 0, 0, 0, 10, 10));
    edt.post_event(AwtEvent::mouse(
        event_id::MOUSE_CLICKED,
        peer,
        1,
        5,
        5,
        1,
        1,
        0,
    ));
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 2, 20, 20, 10, 10));

    edt.coalesce_paint_events();

    // The mouse event separates the two paints, so they should NOT merge.
    assert_eq!(edt.queue_length(), 3);
    let events = edt.drain_events();
    assert_eq!(events[0].id, event_id::PAINT);
    assert_eq!(events[1].id, event_id::MOUSE_CLICKED);
    assert_eq!(events[2].id, event_id::PAINT);
}

#[test]
fn edt_thread_local_marker() {
    // By default we are not on the EDT.
    assert!(!is_edt());
    assert!(!EventDispatchThread::is_dispatch_thread());

    // Mark this thread.
    mark_as_edt();
    assert!(is_edt());
    assert!(EventDispatchThread::is_dispatch_thread());

    // Clean up for other tests that may run on the same thread.
    IS_EDT.set(false);
}

#[test]
fn invoke_and_wait_from_edt_returns_error_instead_of_panicking() {
    // Round-9 misc fix regression test: calling invokeAndWait from
    // the EDT must NOT panic (which previously unwound across the
    // JNI boundary). It must surface InvokeAndWaitError::OnEdt so
    // the native bridge can throw IllegalStateException on the
    // Java thread instead.
    let edt = make_edt();
    mark_as_edt();
    let result = edt.invoke_and_wait(1, PeerId(0));
    // Clean up for other tests on this thread before asserting.
    IS_EDT.set(false);
    assert_eq!(result, Err(InvokeAndWaitError::OnEdt));
    assert_eq!(
        InvokeAndWaitError::OnEdt.jdk_message(),
        "Cannot call invokeAndWait from the event dispatcher thread"
    );
}

#[test]
fn invoke_and_wait_from_non_edt() {
    let edt = Arc::new(make_edt());
    edt.start();

    // Spawn a consumer that drains the queue via `poll_event` so the
    // invocation-completion signal fires.
    let edt2 = Arc::clone(&edt);
    let consumer = std::thread::spawn(move || {
        while edt2.is_running() {
            if edt2.poll_event().is_some() {
                break; // consumed the invocation event
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    // This should complete once the consumer dequeues the event and
    // poll_event signals the completion handle.
    edt.invoke_and_wait(99, PeerId(1))
        .expect("non-EDT caller must succeed");
    consumer.join().unwrap();
    edt.stop();
}

#[test]
fn invoke_and_wait_releases_on_stop() {
    // If the EDT is stopped before the event is dispatched,
    // `invoke_and_wait` must return instead of hanging.
    let edt = Arc::new(make_edt());
    edt.start();

    let edt2 = Arc::clone(&edt);
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        edt2.stop();
    });

    edt.invoke_and_wait(7777, PeerId(2))
        .expect("must release once EDT stops");
    stopper.join().unwrap();
}

#[test]
fn post_event_coalesces_wakes() {
    // Sanity check: two posts only emit one mpsc wake when the queue
    // was already non-empty for the second push.
    let edt = make_edt();
    edt.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
    edt.post_event(AwtEvent::window(event_id::WINDOW_CLOSING, PeerId(1), 1));

    // Drain the wake channel; only the first post should have sent.
    let recv = edt.wake_receiver.lock();
    assert!(recv.try_recv().is_ok());
    assert!(recv.try_recv().is_err());
}

#[test]
fn wait_event_wakes_on_post() {
    let edt = Arc::new(make_edt());
    let edt2 = Arc::clone(&edt);

    let handle = std::thread::spawn(move || {
        // Post an event after a short delay.
        std::thread::sleep(Duration::from_millis(20));
        edt2.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
    });

    // Should wake up within the timeout thanks to the posted event.
    let evt = edt.wait_event(2000);
    assert!(evt.is_some());
    assert_eq!(evt.unwrap().id, event_id::WINDOW_OPENED);
    handle.join().unwrap();
}

/// Build a dummy global-root handle for tests (VM 0) — never resolved.
fn fake_root_handle(seed: usize) -> VmRoot {
    VmRoot {
        vm: 0,
        handle: seed + 1,
    }
}

/// gc-common w22-a: a displaced (evicted / replaced) Runnable's root is
/// handed back tagged with the VM that registered it, whichever VM's call
/// displaced it; and a VM's teardown drops exactly its own rows, signals
/// their waiters, and leaves every other VM's rows alone.
#[test]
fn w22a_runnable_rows_carry_their_vm_and_go_with_it() {
    const VM_A: usize = 0x0A22_0A08;
    const VM_B: usize = 0x0A22_0B08;
    let edt = make_edt();
    let a1 = VmRoot {
        vm: VM_A,
        handle: 1,
    };
    let b1 = VmRoot {
        vm: VM_B,
        handle: 1,
    };
    let id_a = edt.next_callback_id();
    let id_b = edt.next_callback_id();
    assert!(edt.register_runnable(id_a, a1).is_empty());
    assert!(edt.register_runnable(id_b, b1).is_empty());

    // Re-registering A's id from VM B hands back A's root, tagged with A:
    // B's caller must not release handle 1 in its own table.
    let b2 = VmRoot {
        vm: VM_B,
        handle: 2,
    };
    assert_eq!(edt.register_runnable(id_a, b2), vec![a1]);

    // A waiter registered for B's row is released by B's teardown.
    let handle: CompletionHandle = Arc::new((Mutex::new(false), Condvar::new()));
    edt.pending_invocations
        .lock()
        .insert(id_b, Arc::clone(&handle));
    let mut dropped = edt.forget_vm_runnables(VM_B);
    dropped.sort_unstable();
    let mut expected = vec![id_a, id_b];
    expected.sort_unstable();
    assert_eq!(dropped, expected, "both rows now name VM B's roots");
    assert!(*handle.0.lock(), "the forgotten row's waiter is signalled");
    assert_eq!(edt.take_runnable_root(id_a), None);
    assert_eq!(edt.take_runnable_root(id_b), None);
    assert!(edt.runnables_order.lock().is_empty());

    // VM A's rows survive VM B's teardown.
    let id_a2 = edt.next_callback_id();
    assert!(edt.register_runnable(id_a2, a1).is_empty());
    assert!(edt.forget_vm_runnables(VM_B).is_empty());
    assert_eq!(edt.take_runnable_root(id_a2), Some(a1));
}

/// gc-common w23-a: `close` (the owning VM's teardown) empties the queue,
/// hands back the never-run Runnables, releases an `invoke_and_wait`
/// waiter, and a later `invoke_and_wait` / `wait_event_blocking` returns
/// at once instead of waiting for a dispatch that will never come.
#[test]
fn w23a_close_releases_waiters_and_ends_later_waits() {
    let edt = Arc::new(make_edt());
    let a1 = VmRoot {
        vm: 0x0A23_0A08,
        handle: 3,
    };
    let id = edt.next_callback_id();
    assert!(edt.register_runnable(id, a1).is_empty());
    edt.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));

    let (tx, rx) = mpsc::channel();
    let waiter = {
        let edt = Arc::clone(&edt);
        std::thread::spawn(move || {
            let _ = tx.send(edt.invoke_and_wait(4242, PeerId(0)));
        })
    };
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(edt.close(), vec![a1], "the never-run Runnable comes back");
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(Ok(())));
    waiter.join().unwrap();
    assert_eq!(edt.queue_length(), 0, "queued events are dropped");
    assert!(edt.is_closed());

    // After close nothing blocks.
    assert_eq!(edt.invoke_and_wait(4243, PeerId(0)), Ok(()));
    assert!(edt.pending_invocations.lock().is_empty());
    assert!(edt.wait_event_blocking(u64::MAX, 5, || false).is_none());
    assert!(edt.close().is_empty(), "idempotent");
}

/// gc-common w23-a: every VM has its own queue; identity 0 answers the
/// shared `get_edt` queue; teardown removes a VM's queue and never the
/// shared one.
#[test]
fn w23a_edt_for_vm_is_per_vm() {
    const VM_A: usize = 0x0A23_1A08;
    const VM_B: usize = 0x0A23_1B08;
    let a = edt_for_vm(VM_A);
    let b = edt_for_vm(VM_B);
    assert!(!Arc::ptr_eq(&a, &b));
    assert!(Arc::ptr_eq(&a, &edt_for_vm(VM_A)));
    assert!(std::ptr::eq(&*edt_for_vm(0), get_edt()));
    a.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
    assert!(b.poll_event().is_none());
    let gone = forget_vm_edt(VM_A).expect("A had a queue");
    assert!(Arc::ptr_eq(&gone, &a));
    assert!(!vm_has_edt(VM_A));
    assert!(forget_vm_edt(VM_A).is_none());
    assert!(
        forget_vm_edt(0).is_none(),
        "the shared queue is never removed"
    );
    assert!(forget_vm_edt(VM_B).is_some());
}

#[test]
fn runnable_registry_register_and_take() {
    let edt = make_edt();
    let runnable = fake_root_handle(42);
    let id = edt.next_callback_id();
    // Register and take within the same GC generation (gc_gen == 0).
    assert!(edt.register_runnable(id, runnable).is_empty());
    assert_eq!(edt.take_runnable_root(id), Some(runnable));
    assert_eq!(
        edt.take_runnable_root(id),
        None,
        "second take should be empty"
    );
}

#[test]
fn take_runnable_returns_root_handle_after_gc_window() {
    // GC-safety gate (matches `natives.rs::lookup_peer_source`): if a
    // moving collection has run since the Runnable was registered, the
    // cached pointer may be dangling/relocated, so the checked take must
    // return None (fail closed) and drop the entry rather than handing
    // back a stale pointer.
    let edt = make_edt();
    let runnable = fake_root_handle(99);
    let id = edt.next_callback_id();
    assert!(edt.register_runnable(id, runnable).is_empty());
    // A collection ran: live gen (6) != stamped gen (5) -> fail closed.
    assert_eq!(
        edt.take_runnable_root(id),
        Some(runnable),
        "rooted runnables survive moving-GC generation changes"
    );
    // The stale entry must have been dropped, so even a now-matching
    // generation finds nothing.
    assert_eq!(
        edt.take_runnable_root(id),
        None,
        "root entry must be removed after the first take"
    );
}

#[test]
fn next_callback_id_is_monotonic_and_nonzero() {
    let edt = make_edt();
    let a = edt.next_callback_id();
    let b = edt.next_callback_id();
    assert!(a > 0 && b > a, "ids must be monotonic, got {a} then {b}");
}

#[test]
fn notify_if_invocation_skips_when_runnable_pending() {
    // When a Runnable is registered for the callback id, dequeue must
    // NOT signal completion -- the dispatch native owns the signal.
    let edt = make_edt();
    let runnable = fake_root_handle(7);
    let id = edt.next_callback_id();
    assert!(edt.register_runnable(id, runnable).is_empty());

    // Register a completion handle to detect spurious signalling.
    let handle: CompletionHandle = Arc::new((Mutex::new(false), Condvar::new()));
    edt.pending_invocations
        .lock()
        .insert(id, Arc::clone(&handle));

    edt.post_event(AwtEvent::invocation(PeerId(0), 0, id));
    let _ = edt.poll_event();

    // The Runnable is still registered, so dequeue must have been
    // silent: the completion handle stays un-flipped.
    assert!(
        !*handle.0.lock(),
        "dequeue must not signal while runnable is pending"
    );
}

#[test]
fn signal_invocation_complete_after_dispatch_unblocks_waiter() {
    // Simulate the natives-driven flow: register, post, dequeue (no
    // signal because runnable still pending), then explicitly take +
    // signal as the dispatch native would.
    let edt = Arc::new(make_edt());
    edt.start();
    let runnable = fake_root_handle(123);
    let id = edt.next_callback_id();
    assert!(edt.register_runnable(id, runnable).is_empty());

    let edt2 = Arc::clone(&edt);
    let dispatcher = std::thread::spawn(move || {
        // Wait for the event to land then "dispatch" it.
        loop {
            if let Some(_evt) = edt2.poll_event() {
                assert_eq!(edt2.take_runnable_root(id), Some(runnable));
                // pretend Runnable.run() ran here
                let signalled = edt2.signal_invocation_complete(id);
                assert!(signalled, "waiter should have been registered");
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    edt.invoke_and_wait(id, PeerId(0))
        .expect("non-EDT caller must succeed");
    dispatcher.join().unwrap();
    edt.stop();
}

#[test]
fn coalesce_three_adjacent_paints() {
    let edt = make_edt();
    let peer = PeerId(1);
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 0, 0, 0, 10, 10));
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 1, 10, 10, 10, 10));
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 2, 20, 20, 10, 10));

    edt.coalesce_paint_events();
    assert_eq!(edt.queue_length(), 1);

    let evt = edt.poll_event().unwrap();
    if let AwtEventData::Paint {
        x,
        y,
        width,
        height,
    } = &evt.data
    {
        assert_eq!((*x, *y, *width, *height), (0, 0, 30, 30));
    } else {
        panic!("expected Paint");
    }
}

#[test]
fn coalesce_extreme_paint_rects_does_not_overflow() {
    let edt = make_edt();
    let peer = PeerId(1);
    edt.post_event(AwtEvent::paint(
        event_id::PAINT,
        peer,
        0,
        i32::MAX - 4,
        i32::MAX - 4,
        10,
        10,
    ));
    edt.post_event(AwtEvent::paint(
        event_id::PAINT,
        peer,
        1,
        i32::MAX - 2,
        i32::MAX - 2,
        10,
        10,
    ));

    edt.coalesce_paint_events();
    assert_eq!(edt.queue_length(), 1);
    let evt = edt.poll_event().unwrap();
    if let AwtEventData::Paint { width, height, .. } = evt.data {
        assert_eq!(width, 12);
        assert_eq!(height, 12);
    } else {
        panic!("expected Paint");
    }
}

#[test]
fn coalesce_and_drain_combines_paints_and_drains_in_one_lock() {
    // Round-8 EDT throughput fix: the combined entry point must
    // (a) coalesce adjacent same-peer paints exactly like
    // `coalesce_paint_events` would, and (b) return every remaining
    // event in queue order — leaving the queue empty.
    let edt = make_edt();
    let peer = PeerId(7);
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 0, 0, 0, 10, 10));
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 1, 5, 5, 10, 10));
    edt.post_event(AwtEvent::mouse(
        event_id::MOUSE_CLICKED,
        peer,
        2,
        1,
        1,
        1,
        1,
        0,
    ));
    edt.post_event(AwtEvent::paint(event_id::PAINT, peer, 3, 50, 50, 10, 10));

    let drained = edt.coalesce_and_drain();
    // Two paints merged + one mouse + one trailing paint = 3 events.
    assert_eq!(drained.len(), 3);
    assert_eq!(drained[0].id, event_id::PAINT);
    if let AwtEventData::Paint {
        x,
        y,
        width,
        height,
    } = &drained[0].data
    {
        // Bounding union of (0,0,10,10) and (5,5,10,10) = (0,0,15,15)
        assert_eq!((*x, *y, *width, *height), (0, 0, 15, 15));
    } else {
        panic!("expected Paint");
    }
    assert_eq!(drained[1].id, event_id::MOUSE_CLICKED);
    assert_eq!(drained[2].id, event_id::PAINT);
    assert_eq!(edt.queue_length(), 0);
}
