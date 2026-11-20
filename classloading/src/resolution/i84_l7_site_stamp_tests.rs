use super::*;

#[test]
fn a_stamp_answers_its_own_site_only() {
    let mut cache = InvokeCache::<()>::new();
    let caller = ClassId::new(400_101);
    assert_eq!(cache.site_stamp(caller, 7, false, 0), None);
    cache.set_site_stamp(caller, 7, false, 0, [1, 2, 3]);
    assert_eq!(cache.site_stamp(caller, 7, false, 0), Some([1, 2, 3]));
    // Another index, the special half of the key, another caller.
    assert_eq!(cache.site_stamp(caller, 8, false, 0), None);
    assert_eq!(cache.site_stamp(caller, 7, true, 0), None);
    assert_eq!(cache.site_stamp(ClassId::new(400_102), 7, false, 0), None);
    // Rewritten in place.
    cache.set_site_stamp(caller, 7, false, 0, [4, 5, 6]);
    assert_eq!(cache.site_stamp(caller, 7, false, 0), Some([4, 5, 6]));
}
