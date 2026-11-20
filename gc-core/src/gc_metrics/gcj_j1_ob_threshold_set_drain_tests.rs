//! gcj j1/ob: the Generational half of the set-time threshold hand-off.
use super::*;

#[test]
fn gcj_j1_ob_set_drain_is_the_serial_queues_alone() {
    // The generational heap's queue reads the key once (default ON).
    assert_eq!(
        GcNotificationQueue::new().serial_threshold_set_drain_enabled(),
        true
    );
    // A backend's queue never answers the Serial key. G1 has its own
    // always-enabled policy; ZGC has its bean-shape switch.
    let backend_queue = GcNotificationQueue::with_collectors(SERIAL_COLLECTORS);
    assert!(!backend_queue.serial_threshold_set_drain_enabled());
    let g1 = BackendGcBeans::with_order(BackendBeanShape::G1, true);
    assert!(!g1.notifications().serial_threshold_set_drain_enabled());
    assert!(g1.threshold_set_drain_enabled());
}
