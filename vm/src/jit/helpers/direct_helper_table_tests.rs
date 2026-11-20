use super::*;

#[test]
fn every_direct_helper_is_wired_and_the_long_and_integer_helpers_are_distinct() {
    let t = direct_helper_table();
    assert_ne!(
        t.long_value_of, t.integer_value_of,
        "Long.valueOf is wired to Integer.valueOf's helper"
    );
    assert_ne!(
        t.long_long_value, t.integer_int_value,
        "Long.longValue is wired to Integer.intValue's helper"
    );
    assert_ne!(
        t.long_value_of, t.long_long_value,
        "both Long helpers hold one address"
    );
    assert_ne!(t.monitor_enter, t.monitor_exit);
    for (name, addr) in [
        ("integer_value_of", t.integer_value_of),
        ("thread_current_thread", t.thread_current_thread),
        ("thread_local_get", t.thread_local_get),
        ("thread_local_set", t.thread_local_set),
        ("indy_bridge", t.indy_bridge),
        ("monitor_enter", t.monitor_enter),
        (
            "nio_byte_element_served_class",
            t.nio_byte_element_served_class,
        ),
        ("arm_savebase_watch", t.arm_savebase_watch),
        ("disarm_savebase_watch", t.disarm_savebase_watch),
        ("unsafe_get_byte", t.unsafe_get_byte),
        ("unsafe_get_short", t.unsafe_get_short),
        ("unsafe_get_int", t.unsafe_get_int),
        ("unsafe_get_long", t.unsafe_get_long),
        ("unsafe_get_reference", t.unsafe_get_reference),
        (
            "unsafe_get_reference_volatile",
            t.unsafe_get_reference_volatile,
        ),
        ("unsafe_put_byte", t.unsafe_put_byte),
        ("unsafe_put_short", t.unsafe_put_short),
        ("unsafe_put_int", t.unsafe_put_int),
        ("unsafe_put_long", t.unsafe_put_long),
        ("unsafe_cas_int", t.unsafe_cas_int),
        ("unsafe_cas_long", t.unsafe_cas_long),
        // nio21 wave 3, lane L4.
        ("unsafe_get_int_volatile", t.unsafe_get_int_volatile),
        ("unsafe_get_long_volatile", t.unsafe_get_long_volatile),
        ("unsafe_put_int_volatile", t.unsafe_put_int_volatile),
        ("unsafe_put_long_volatile", t.unsafe_put_long_volatile),
        (
            "unsafe_put_reference_volatile",
            t.unsafe_put_reference_volatile,
        ),
        ("unsafe_cas_reference", t.unsafe_cas_reference),
        ("unsafe_full_fence", t.unsafe_full_fence),
        ("reference_refers_to0", t.reference_refers_to0),
        (
            "phantom_reference_refers_to0",
            t.phantom_reference_refers_to0,
        ),
        (
            "thread_current_carrier_thread",
            t.thread_current_carrier_thread,
        ),
        (
            "scoped_memory_get_int_internal",
            t.scoped_memory_get_int_internal,
        ),
        (
            "scoped_memory_put_int_internal",
            t.scoped_memory_put_int_internal,
        ),
    ] {
        assert_ne!(addr, 0, "{name} is not wired");
    }
    assert_eq!(
        (t.static_base_resolver, t.static_base_resolver_ctx),
        (0, 0),
        "a table built without a VM must not carry a resolver"
    );
    assert_eq!(
        t.class_initialized, 0,
        "a table built without a VM must not carry the class-initialised predicate"
    );
    assert_eq!(
        t.well_known_class_id, 0,
        "a table built without a VM must not carry the well-known-class resolver"
    );
    assert!(t
        .varhandle_read
        .iter()
        .chain(&t.varhandle_write)
        .chain(&t.varhandle_cas)
        .all(|&a| a != 0));
}
