use super::*;

/// Only a plain millisecond clock later than the row counts; a bridge
/// stamp (milliseconds shifted left 20), zero, a negative value and an
/// older clock never do.
#[test]
fn gcg_g6rs_java_soft_timestamp_rule() {
    let now: u64 = 1_790_000_000_000;
    assert!(java_soft_timestamp_is_newer(now as i64, now - 5));
    assert!(!java_soft_timestamp_is_newer(now as i64, now), "not later");
    assert!(
        !java_soft_timestamp_is_newer((now - 5) as i64, now),
        "older"
    );
    assert!(!java_soft_timestamp_is_newer(0, 0));
    assert!(!java_soft_timestamp_is_newer(-1, 0));
    let bridge = cratonvm_gc::reference::soft_touch_stamp(now, 3).unwrap();
    assert!(
        !java_soft_timestamp_is_newer(bridge, 0),
        "a bridge stamp is never read as a clock"
    );
}

/// With discovery at allocation off (the default), the refresh and the
/// clock publication touch nothing: a soft row whose object's slots hold
/// a later plain clock keeps its stamp. Positive control: the same row
/// with the switch on would be refreshed only if the object were a real
/// `SoftReference` (screened by class), so a non-`SoftReference` object
/// keeps its stamp in both arms.
#[test]
fn gcg_g6rs_refresh_is_inert_off_and_screens_by_class_on() {
    use cratonvm_gc::reference::ReferenceType;
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let heap = &shared.mem.heap;
    let obj = heap.alloc_object(ClassId::new(0), 4);
    let referent = heap.alloc_object(ClassId::new(0), 1);
    for i in 0..4 {
        heap.set_field(obj, i, Value::Long(1_790_000_000_000));
    }
    let addr = obj.as_ptr() as usize;
    shared.mem.ref_processor.lock().discover_reference(
        ReferenceType::Soft,
        addr,
        referent.as_ptr() as usize,
        None,
    );
    let stamp = || shared.mem.ref_processor.lock().active_soft_stamps()[0].1;
    let before = stamp();
    shared.mem.ref_discovery.set_engaged(false);
    refresh_soft_rows_from_java_timestamps(&shared);
    publish_soft_reference_clock(&shared, 1_790_000_000_000);
    assert_eq!(stamp(), before, "switch off: untouched");
    shared.mem.ref_discovery.set_engaged(true);
    refresh_soft_rows_from_java_timestamps(&shared);
    assert_eq!(stamp(), before, "not a SoftReference: screened");
}
