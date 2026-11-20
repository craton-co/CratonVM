//! gc-common round 2026-09-23, wave 3, lane D: the reference-delivery
//! thread's gate and its no-owner fallback. The thread itself needs a VM
//! built by `Vm::new` and is exercised by the probes the w3-d report lists.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::vm::SharedVm;

fn fixture_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

/// zt t13/x10: this file's text, for the source pins of
/// `zt12gn4_undeliverable_zgc_doors_hand_off_under_the_compatible_rule`
/// (t12/gn4 called `source()` / `body()` here without defining them: they
/// are private to `w7g_door_tests`).
fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

/// The item starting at `sig` at the start of a line, up to the next
/// column-0 closing brace (`\u{7d}`), as `w7g_door_tests::body`.
fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// Unset is the §12.6-only default; explicit off words are the pre-w3
/// inline drain; any other value (the grouped spelling included) is the
/// full hand-off. gc-common w4-d.
#[test]
fn the_flag_value_rule() {
    assert_eq!(
        finalizer_delivery_from(None),
        FinalizerDelivery::WhenLockHeld
    );
    for off in ["0", "false", "off", "no", " 0 "] {
        assert_eq!(
            finalizer_delivery_from(Some(off)),
            FinalizerDelivery::Inline,
            "{off:?} must read inline"
        );
    }
    for on in ["1", "true", "on", ""] {
        assert_eq!(
            finalizer_delivery_from(Some(on)),
            FinalizerDelivery::Always,
            "{on:?} must read the full hand-off"
        );
    }
}

/// A thread with nothing published in its JMX lock lists holds no
/// user-visible lock, so the default policy keeps the inline drain for it
/// and starts no thread; an unregistered thread answers the same.
/// gc-common w4-d.
#[test]
fn a_lock_free_thread_keeps_the_inline_finalizer_drain() {
    let shared = fixture_vm();
    let thread = JvmThread::new(crate::ThreadId(4242), "w4d-lock-free");
    assert!(!thread_holds_user_locks(&shared, &thread));
    if finalizer_delivery_policy() == FinalizerDelivery::WhenLockHeld {
        assert!(!finalizer_thread_takes_finalizers(&shared, &thread));
    }
    assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
}

/// With no delivery thread, nothing waits on our lock.
#[test]
fn no_delivery_thread_is_never_blocked_on_the_caller() {
    let shared = fixture_vm();
    assert!(!delivery_thread_waits_on_our_lock(
        &shared,
        crate::ThreadId(1)
    ));
}

/// A published monitor is a held lock; with a finalizer queued the
/// default policy then asks for the delivery thread, and a VM that cannot
/// start one (no owning `Arc`) falls back to the inline drain. The
/// deadlock probe sees the delivery thread blocked entering the caller's
/// monitor. The object is never dereferenced (the lists hold raw refs).
/// gc-common w4-d.
#[test]
fn a_held_monitor_is_seen_and_a_blocked_delivery_thread_detected() {
    let shared = fixture_vm();
    let registry = &shared.threads.thread_registry;
    let caller = crate::ThreadId(4301);
    let delivery = crate::ThreadId(4302);
    registry.register(caller, "w4d-holder", None);
    registry.register(delivery, "w4d-delivery", None);
    // SAFETY: never dereferenced — only compared by address.
    let lock = unsafe { ObjectRef::from_raw(0x10_0000 as *mut u8) };
    let thread = JvmThread::new(caller, "w4d-holder");
    assert!(!thread_holds_user_locks(&shared, &thread));
    registry.complete_jmx_monitor_enter(caller, lock);
    assert!(thread_holds_user_locks(&shared, &thread));

    shared.mem.finalizer_thread.enqueue_unfinalized(0x20_0000);
    if finalizer_delivery_policy() == FinalizerDelivery::WhenLockHeld {
        assert!(
            !finalizer_thread_takes_finalizers(&shared, &thread),
            "no owner: the start fails and the inline drain is kept"
        );
        assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
    }

    shared.mem.finalizer_thread.set_delivery_thread(delivery.0);
    assert!(!delivery_thread_waits_on_our_lock(&shared, caller));
    registry.set_jmx_contended_monitor(delivery, lock);
    assert!(delivery_thread_waits_on_our_lock(&shared, caller));
    registry.remove_jmx_locked_monitor(caller, lock);
    assert!(!delivery_thread_waits_on_our_lock(&shared, caller));
    assert!(!thread_holds_user_locks(&shared, &thread));
}

/// A `SharedVm` with no owning `Arc` (every unit fixture) cannot hand a
/// thread a handle to itself: the start refuses, hands the claim back, and
/// registers nothing, so the caller keeps the inline drain.
#[test]
fn a_vm_without_an_owner_keeps_the_inline_drain() {
    let shared = fixture_vm();
    let alive_before = shared.threads.thread_registry.alive_count();
    assert!(!start_finalizer_thread(&shared));
    assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
    assert_eq!(shared.mem.finalizer_thread.delivery_thread(), None);
    assert_eq!(shared.threads.thread_registry.alive_count(), alive_before);
}

/// With no delivery thread to take them, a reference pass's queue
/// wake-ups are dropped at once instead of being kept rooted (through
/// `CleanerThread::pending_addresses`) forever.
#[test]
fn queue_wake_ups_with_no_taker_are_not_retained() {
    let shared = fixture_vm();
    gc_enqueued_queues_hand_off(&shared, &[0x1000, 0x2000]);
    assert!(!shared.mem.cleaner_thread.has_pending_work());
    assert!(shared.mem.cleaner_thread.pending_addresses().is_empty());
}

/// gc-common w5-d: queue wake-ups are recorded under the full hand-off,
/// and under the default policy exactly for a `--real-jdk` VM — never for
/// a `--compatible` one (whose natives poll), never on the inline
/// bisection arm.
#[test]
fn queue_wake_ups_follow_the_vms_own_policy() {
    use super::FinalizerDelivery::{Always, Inline, WhenLockHeld};
    assert!(queue_wake_ups_recorded_for(Always, false));
    assert!(queue_wake_ups_recorded_for(Always, true));
    assert!(queue_wake_ups_recorded_for(WhenLockHeld, true));
    assert!(!queue_wake_ups_recorded_for(WhenLockHeld, false));
    assert!(!queue_wake_ups_recorded_for(Inline, true));
    assert!(!queue_wake_ups_recorded_for(Inline, false));
    // The fixture is `--compatible` (the embedding default): whatever the
    // process flag says, nothing is recorded unless the full hand-off is on.
    let shared = fixture_vm();
    assert!(!shared.config.is_jdk_only());
    assert_eq!(queue_wake_ups_recorded(&shared), finalizer_thread_enabled());
}

/// gc-common w5-d: `waitForReferenceProcessing()` answers `false` at once
/// when nothing is queued or in flight — and, with the flag unset (the
/// default this suite runs), always, so `--compatible` is unchanged. A
/// fixture VM has no owner to start a delivery thread with either way.
#[test]
fn wait_for_reference_processing_is_false_with_no_delivery_work() {
    let shared = fixture_vm();
    let mut thread = JvmThread::new(crate::ThreadId(1), "w5d-wfrp");
    let start = std::time::Instant::now();
    assert!(!wait_for_reference_processing_for_runtime(
        &shared,
        &mut thread
    ));
    assert!(
        start.elapsed() < std::time::Duration::from_millis(500),
        "no work: must not wait"
    );
    assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
}

/// gc-common w6-d / w7-d: GC notifications leave the allocating mutator
/// under the full hand-off and, under the default policy, always for a
/// `--real-jdk` VM and only under a held lock for a `--compatible` one;
/// never on the inline bisection arm.
#[test]
fn gc_notifications_follow_the_vms_own_policy() {
    use super::FinalizerDelivery::{Always, Inline, WhenLockHeld};
    use super::GcNotificationDelivery as N;
    assert_eq!(gc_notification_delivery_for(Always, false), N::OffMutator);
    assert_eq!(gc_notification_delivery_for(Always, true), N::OffMutator);
    assert_eq!(
        gc_notification_delivery_for(WhenLockHeld, true),
        N::OffMutator
    );
    assert_eq!(
        gc_notification_delivery_for(WhenLockHeld, false),
        N::OffMutator
    );
    assert_eq!(gc_notification_delivery_for(Inline, true), N::Inline);
    assert_eq!(gc_notification_delivery_for(Inline, false), N::Inline);
    let shared = fixture_vm();
    assert!(!shared.config.is_jdk_only());
    let expected = if finalizer_thread_enabled() {
        N::OffMutator
    } else if finalizer_delivery_policy() == WhenLockHeld {
        N::OffMutator
    } else {
        N::Inline
    };
    assert_eq!(gc_notification_delivery(&shared), expected);
}

#[test]
fn a_lock_free_compatible_drain_cannot_steal_a_set_time_sensor_handoff() {
    use cratonvm_gc::gc_metrics::{JmxPoolUsage, PoolThresholdKind};
    let shared = fixture_vm();
    assert!(!shared.config.is_jdk_only());
    if !shared.mem.heap.is_generational() {
        panic!("Generational fixture");
    };
    let sensors = shared.mem.heap.gen_gc_notifications().unwrap().pool_thresholds();
    sensors.set("Tenured Gen", PoolThresholdKind::Usage, 1);
    sensors.check(
        &[JmxPoolUsage {
            name: "Tenured Gen",
            init: None,
            used: 8,
            committed: 16,
            max: Some(16),
            collection_used: None,
            managers: &[],
        }],
        &[],
    );
    assert!(sensors.has_pending());
    let worker = &shared.mem.finalizer_thread;
    assert!(worker.claim_delivery_thread_start());
    worker.set_delivery_thread(55103);
    let caller = JvmThread::new(crate::ThreadId(55102), "set-time-mutator");
    assert!(!thread_holds_user_locks(&shared, &caller));
    let asynchronous = gc_notification_delivery(&shared) != GcNotificationDelivery::Inline;
    assert_eq!(
        hand_set_time_threshold_crossing_to_delivery_thread(&shared),
        asynchronous
    );
    assert_eq!(
        gc_notifications_go_to_delivery_thread(&shared, &caller),
        asynchronous,
        "a later allocation door must leave the handed-off sensor to its worker"
    );
    assert!(
        sensors.has_pending(),
        "routing must not consume the sensor request"
    );
}

/// gc-common w7-d: under the `--compatible` lock-held rule, a lock-free
/// collecting thread keeps its inline delivery and leaves no mark; a
/// lock-holding one marks the hand-off, and when no delivery thread can
/// be started (a fixture has no owning `Arc`) takes the mark back and
/// keeps the inline delivery too. The lock is never dereferenced.
#[test]
fn compatible_gc_notifications_move_only_under_a_held_lock() {
    let shared = fixture_vm();
    let registry = &shared.threads.thread_registry;
    let tid = crate::ThreadId(4401);
    registry.register(tid, "w7d-allocator", None);
    let thread = JvmThread::new(tid, "w7d-allocator");
    let ft = &shared.mem.finalizer_thread;
    assert!(!gc_notifications_handed_off_under_lock(&shared, &thread));
    assert!(!ft.take_gc_notifications_handed_off(), "no lock: no mark");
    // SAFETY: never dereferenced — only compared by address.
    let lock = unsafe { ObjectRef::from_raw(0x10_0000 as *mut u8) };
    registry.complete_jmx_monitor_enter(tid, lock);
    assert!(
        !gc_notifications_handed_off_under_lock(&shared, &thread),
        "no owner: the start fails and the door delivers inline"
    );
    assert!(
        !ft.take_gc_notifications_handed_off(),
        "the mark is taken back"
    );
    assert!(!ft.delivery_thread_claimed());
    // The delivery thread itself, holding a lock, keeps the batch's own
    // delivery and leaves the mark for its batch.
    ft.set_delivery_thread(tid.0);
    assert!(gc_notifications_handed_off_under_lock(&shared, &thread));
    assert!(ft.take_gc_notifications_handed_off());
    registry.remove_jmx_locked_monitor(tid, lock);
}

/// gc-common w36-e: the allocation-failure door hands queued notifications
/// to a claimed delivery thread when the collecting thread holds a lock --
/// the only door a compiled allocation loop reaches. Nothing queued, or no
/// lock under the `--compatible` rule: nothing moves. Follows the latched
/// process policy, as the neighbouring tests do.
#[test]
fn w36e_the_forced_door_hands_off_notifications_under_a_held_lock() {
    use cratonvm_gc::gc_metrics::{
        GcNotificationRecord, GC_ACTION_MINOR, JMX_SERIAL_YOUNG_COLLECTOR,
    };
    let shared = fixture_vm();
    let registry = &shared.threads.thread_registry;
    let tid = crate::ThreadId(4402);
    registry.register(tid, "w36e-allocator", None);
    let thread = JvmThread::new(tid, "w36e-allocator");
    let ft = &shared.mem.finalizer_thread;
    // A serving delivery thread (claimed: a request is a counter bump).
    assert!(ft.claim_delivery_thread_start());
    ft.set_delivery_thread(4403);
    // SAFETY: never dereferenced -- only compared by address.
    let lock = unsafe { ObjectRef::from_raw(0x10_0008 as *mut u8) };
    registry.complete_jmx_monitor_enter(tid, lock);
    // Nothing queued: nothing to hand off, no mark.
    assert!(!forced_door_hands_off_gc_notifications(&shared, &thread));
    assert!(!ft.take_gc_notifications_handed_off());

    if !shared.mem.heap.is_generational() {
        panic!("the fixture is Generational");
    };
    let queue = shared.mem.heap.gen_gc_notifications().unwrap();
    assert!(queue.set_enabled(JMX_SERIAL_YOUNG_COLLECTOR, true));
    let now = std::time::Instant::now();
    queue.record(GcNotificationRecord {
        collector: JMX_SERIAL_YOUNG_COLLECTOR,
        action: GC_ACTION_MINOR,
        cause: "Allocation Failure",
        id: 1,
        start: now,
        end: now,
        timestamp_ms: 0,
        before: Vec::new(),
        after: Vec::new(),
    });
    assert!(queue.has_pending(), "the listener's bean is enabled");
    let policy = gc_notification_delivery(&shared);
    let handed = forced_door_hands_off_gc_notifications(&shared, &thread);
    assert_eq!(
        handed,
        policy != GcNotificationDelivery::Inline,
        "{policy:?}: a lock-holding forced door hands off unless delivery is inline"
    );
    assert_eq!(
        ft.take_gc_notifications_handed_off(),
        policy == GcNotificationDelivery::WhenLockHeld,
        "the --compatible rule marks the batch it hands over"
    );
    registry.remove_jmx_locked_monitor(tid, lock);
    if policy == GcNotificationDelivery::WhenLockHeld {
        assert!(
            !forced_door_hands_off_gc_notifications(&shared, &thread),
            "no lock: the next drain point delivers inline, as before"
        );
        assert!(!ft.take_gc_notifications_handed_off());
    }
}

/// zt t12/gn4 (`CRATONVM_ZGC_UNDELIVERABLE_NOTIFICATIONS_OFF_THREAD`): a
/// door that cannot deliver hands its queued notifications off only on a
/// ZGC heap, with the switch on, under the `--compatible` rule. On the
/// Generational fixture a queued notification with no lock held stays
/// queued and unmarked (G1 and Generational keep the w7-d rule), and the
/// three doors call the hand-off where their delivery cannot happen.
#[test]
fn zt12gn4_undeliverable_zgc_doors_hand_off_under_the_compatible_rule() {
    use super::GcNotificationDelivery as N;
    use cratonvm_gc::gc_metrics::{
        GcNotificationRecord, GC_ACTION_MINOR, JMX_SERIAL_YOUNG_COLLECTOR,
    };
    assert!(zgc_undeliverable_door_hands_off_for(true, true, N::WhenLockHeld));
    assert!(
        !zgc_undeliverable_door_hands_off_for(true, false, N::WhenLockHeld),
        "=0 kill: the next drain point, as before"
    );
    assert!(
        !zgc_undeliverable_door_hands_off_for(false, true, N::WhenLockHeld),
        "off ZGC: the w7-d rule"
    );
    assert!(
        !zgc_undeliverable_door_hands_off_for(true, true, N::OffMutator),
        "--real-jdk already hands off at every door"
    );
    assert!(
        !zgc_undeliverable_door_hands_off_for(true, true, N::Inline),
        "the inline bisection arm hands nothing off"
    );

    let shared = fixture_vm();
    let registry = &shared.threads.thread_registry;
    let tid = crate::ThreadId(4404);
    registry.register(tid, "zt12gn4-allocator", None);
    let thread = JvmThread::new(tid, "zt12gn4-allocator");
    let ft = &shared.mem.finalizer_thread;
    assert!(
        !zgc_undeliverable_door_hands_off_gc_notifications(&shared, &thread),
        "nothing queued"
    );
    if !shared.mem.heap.is_generational() {
        panic!("the fixture is Generational");
    };
    let queue = shared.mem.heap.gen_gc_notifications().unwrap();
    assert!(queue.set_enabled(JMX_SERIAL_YOUNG_COLLECTOR, true));
    let now = std::time::Instant::now();
    queue.record(GcNotificationRecord {
        collector: JMX_SERIAL_YOUNG_COLLECTOR,
        action: GC_ACTION_MINOR,
        cause: "Allocation Failure",
        id: 1,
        start: now,
        end: now,
        timestamp_ms: 0,
        before: Vec::new(),
        after: Vec::new(),
    });
    assert!(queue.has_pending());
    assert!(
        !zgc_undeliverable_door_hands_off_gc_notifications(&shared, &thread),
        "not a ZGC heap"
    );
    assert!(!ft.take_gc_notifications_handed_off(), "no mark off ZGC");
    assert!(queue.has_pending(), "still queued for the next drain point");

    let src = source();
    let forced = body(&src, "fn forced_door_hands_off_gc_notifications(");
    let pin = "|| zgc_undeliverable_door_hands_off_gc_notifications(shared, thread)";
    assert!(forced.contains(pin));
    let door = body(&src, "fn maybe_gc(");
    let rule = door
        .rfind("!gc_notifications_go_to_delivery_thread(shared, thread)")
        .expect("the door's own rule");
    let jit = door
        .rfind("!zgc_jit_borrowed_door_hands_off_gc_notifications(shared, thread)")
        .expect("the JIT-borrow hand-off");
    let inline = door
        .rfind("run_gc_notifications(shared, thread, false);")
        .expect("the inline drain");
    assert!(rule < jit && jit < inline);
    let cold = body(&src, "fn drain_ref_work_queued_elsewhere_cold(");
    let borrowed = cold
        .find("if crate::jit::helpers::is_jit_thread_set()")
        .expect("the borrow check");
    let hand = cold
        .find("zgc_undeliverable_door_hands_off_gc_notifications(shared, thread)")
        .expect("the hand-off under the borrow");
    let take = cold
        .find("if shared.mem.gc_barrier.take_ref_work_queued()")
        .expect("the hint");
    assert!(borrowed < hand && hand < take, "before the borrowed return");
}

/// gci i47w/nt47: the metadata door (which never runs Java) hands its
/// notifications off under every delivery rule but the inline arm -- the
/// `--compatible` rule included, lock or no lock.
#[test]
fn gci_i47w_nt47_the_drainless_door_hands_off_under_every_rule_but_inline() {
    use super::GcNotificationDelivery as N;
    assert!(!drainless_door_hands_off_gc_notifications(N::Inline));
    assert!(drainless_door_hands_off_gc_notifications(N::WhenLockHeld));
    assert!(drainless_door_hands_off_gc_notifications(N::OffMutator));
}

/// gci i47w/nt47: with notifications queued and a serving delivery
/// thread, a LOCK-FREE thread's metadata door hands them over (the
/// `--compatible` rule marks the batch); nothing queued, nothing moves.
/// Before nt47 a lock-free `--compatible` define left them for an inline
/// drain point a compiled define loop never reaches (`notif_delivered=0`).
/// Follows the latched process policy, as the neighbouring tests do.
#[test]
fn gci_i47w_nt47_the_metadata_door_hands_off_without_a_lock() {
    use cratonvm_gc::gc_metrics::{
        GcNotificationRecord, GC_ACTION_MINOR, JMX_SERIAL_YOUNG_COLLECTOR,
    };
    let shared = fixture_vm();
    let registry = &shared.threads.thread_registry;
    let tid = crate::ThreadId(4471);
    registry.register(tid, "nt47-definer", None);
    let thread = JvmThread::new(tid, "nt47-definer");
    let ft = &shared.mem.finalizer_thread;
    assert!(ft.claim_delivery_thread_start());
    ft.set_delivery_thread(4472);
    // Nothing queued: nothing to hand off, no mark.
    assert!(!metadata_door_hands_off_gc_notifications(&shared, &thread));
    assert!(!ft.take_gc_notifications_handed_off());

    if !shared.mem.heap.is_generational() {
        panic!("the fixture is Generational");
    };
    let queue = shared.mem.heap.gen_gc_notifications().unwrap();
    assert!(queue.set_enabled(JMX_SERIAL_YOUNG_COLLECTOR, true));
    let now = std::time::Instant::now();
    queue.record(GcNotificationRecord {
        collector: JMX_SERIAL_YOUNG_COLLECTOR,
        action: GC_ACTION_MINOR,
        cause: "Metadata GC Threshold",
        id: 1,
        start: now,
        end: now,
        timestamp_ms: 0,
        before: Vec::new(),
        after: Vec::new(),
    });
    assert!(queue.has_pending(), "the listener's bean is enabled");
    assert!(!thread_holds_user_locks(&shared, &thread), "no lock held");
    let policy = gc_notification_delivery(&shared);
    assert_eq!(
        metadata_door_hands_off_gc_notifications(&shared, &thread),
        policy != GcNotificationDelivery::Inline,
        "{policy:?}: the metadata door hands off unless delivery is inline"
    );
    assert_eq!(
        ft.take_gc_notifications_handed_off(),
        policy == GcNotificationDelivery::WhenLockHeld,
        "the --compatible rule marks the batch it hands over"
    );
    // The ordinary lock-held rule is unchanged: a lock-free door keeps
    // its inline delivery there.
    if policy == GcNotificationDelivery::WhenLockHeld {
        assert!(!gc_notifications_handed_off_under_lock(&shared, &thread));
        assert!(!ft.take_gc_notifications_handed_off());
    }
}

/// gc-common w6-d: a `ReferenceQueue.remove` waiter may sleep on `lock`
/// only once a delivery thread is claimed to deliver the recorded
/// wake-ups; before that — and in a VM that records none — it polls.
#[test]
fn queue_wake_ups_are_delivered_only_with_a_claimed_delivery_thread() {
    let shared = fixture_vm();
    assert!(
        !queue_wake_up_delivery_serves(&shared),
        "no delivery thread has been claimed"
    );
    assert!(shared.mem.finalizer_thread.claim_delivery_thread_start());
    assert_eq!(
        queue_wake_up_delivery_serves(&shared),
        queue_wake_ups_recorded(&shared),
        "claimed: delivered exactly when this VM records its wake-ups"
    );
    shared.mem.finalizer_thread.release_delivery_thread_start();
    assert!(!queue_wake_up_delivery_serves(&shared));
}
