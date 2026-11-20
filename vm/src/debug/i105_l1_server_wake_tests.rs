use super::*;

fn thread_start(thread_id: u64) -> DebugEvent {
    DebugEvent {
        kind: events::EventKind::ThreadStart,
        request_id: 1,
        suspend_policy: events::SuspendPolicy::None,
        thread_id,
        class_id: 0,
        method_id: 0,
        offset: 0,
        extra: Vec::new(),
        set_follows: false,
    }
}

#[test]
fn a_burst_of_event_sets_wakes_the_server_once() {
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let (tx, rx) = std::sync::mpsc::channel::<DebugEvent>();
    *shared.debug.debug_event_tx.lock().expect("event channel") = Some(tx);
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<ServerInput>();
    *shared.debug.jdwp_server_wake.lock().expect("wake") = Some(wake_tx);

    send_event_set(&shared, vec![thread_start(5)]);
    send_event_set(&shared, vec![thread_start(6)]);
    assert_eq!(rx.try_iter().count(), 2, "both sets queued");
    let wakes = wake_rx
        .try_iter()
        .filter(|w| matches!(w, ServerInput::Wake))
        .count();
    assert_eq!(wakes, 1, "one wake in flight");

    // The loop took it (and cleared the flag before draining): the next
    // set wakes it again.
    shared
        .debug
        .jdwp_server_wake_pending
        .store(false, std::sync::atomic::Ordering::Release);
    send_event_set(&shared, vec![thread_start(7)]);
    assert_eq!(rx.try_iter().count(), 1);
    assert_eq!(wake_rx.try_iter().count(), 1);
}
