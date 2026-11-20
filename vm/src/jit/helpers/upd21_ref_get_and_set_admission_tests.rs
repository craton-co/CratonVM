use super::ref_store_plan_words_publish_gates as publishes;

/// The plan words the three collectors publish, and the `x64::ref_store_gates_of`
/// rule they must agree with: both gate bytes, and exactly one post shape.
#[test]
fn upd21_only_a_published_gate_plan_admits_the_reference_get_and_set_arm() {
    // Generational: process pre byte, post byte, no floor, a skip mask.
    assert!(publishes(((0x1000, 0x1001, 0), 0x03)));
    // ZGC: pre, post, an age floor, no mask.
    assert!(publishes(((0x1000, 0x1001, 0x1002), 0)));
    // G1 publishes only the pre byte: no post gate, no floor, no mask.
    assert!(!publishes(((0x1000, 0, 0), 0)));
    // A heap that has not published yet.
    assert!(!publishes(((0, 0, 0), 0)));
    // Both post shapes at once, or neither, is no plan.
    assert!(!publishes(((0x1000, 0x1001, 0x1002), 0x03)));
    assert!(!publishes(((0x1000, 0x1001, 0), 0)));
    // A missing pre or post gate byte is no plan.
    assert!(!publishes(((0, 0x1001, 0), 0x03)));
    assert!(!publishes(((0x1000, 0, 0x1002), 0)));
}
