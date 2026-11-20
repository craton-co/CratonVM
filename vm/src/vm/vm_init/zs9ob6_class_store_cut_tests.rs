use super::{class_store_cut_with, SharedVm};
use crate::config::VmConfig;

/// zs s9/ob6 (rv6 X2): with the class lock held for write -- a thread
/// stopped mid-define, as a collection edge can find it -- the lock-free
/// census still answers, and with the class store's own figure. Positive
/// control: the s6/ob5 `try_read` census (`lock_free = false`) answers
/// `None` under the same busy lock, which left that collection's cycle
/// `GcInfo` with an undefined `Metaspace` usage.
#[test]
fn zs9ob6_the_metaspace_census_needs_no_class_lock() {
    let shared = SharedVm::new(VmConfig::default());
    let free = class_store_cut_with(&shared, true);
    let locked = class_store_cut_with(&shared, false);
    assert_eq!(free, locked, "both read the store when the lock is free");
    let expected = shared
        .classes
        .class_manager
        .read()
        .class_store()
        .next_id()
        .as_u32();
    assert_eq!(free, Some(expected));
    let guard = shared.classes.class_manager.write();
    assert_eq!(class_store_cut_with(&shared, true), Some(expected));
    assert_eq!(
        class_store_cut_with(&shared, false),
        None,
        "positive control"
    );
    drop(guard);
}
