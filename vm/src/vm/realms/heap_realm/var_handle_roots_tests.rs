use super::VarHandleRoots;
use crate::types::ObjectRef;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: non-null, 8-byte aligned, never dereferenced (lesson o).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// gc-common w29-d: two live objects with ONE identity hash. The second
/// registration used to overwrite the first (unrooting it); both must stay
/// rooted and the key's answer must stay the first object.
#[test]
fn a_same_hash_second_registration_keeps_the_first_rooted() {
    let mut roots = VarHandleRoots::default();
    let a = fake(0x1_0000_1000);
    let b = fake(0x2_0000_1000);
    let key = 0x1000;
    assert!(roots.insert(key, a));
    assert!(roots.insert(key, b));
    assert_eq!(roots.len(), 2);
    assert_eq!(
        roots.get(&key).copied(),
        Some(a),
        "first registrant keeps the key"
    );
    assert_eq!(roots.bucket(key), &[a, b]);
    let scanned: Vec<ObjectRef> = roots.values().copied().collect();
    assert!(
        scanned.contains(&a) && scanned.contains(&b),
        "both are roots"
    );
}

#[test]
fn re_registering_one_object_is_idempotent() {
    let mut roots = VarHandleRoots::default();
    let a = fake(0x3000);
    assert!(roots.insert(7, a));
    assert!(!roots.insert(7, a));
    assert_eq!(roots.len(), 1);
    assert_eq!(roots.bucket(7), &[a]);
}

/// The remap walks every bucket member, and a re-registration with the
/// post-move address still dedups against the remapped row.
#[test]
fn remap_reaches_every_bucket_member() {
    let mut roots = VarHandleRoots::default();
    let (a, b) = (fake(0x4000), fake(0x5000));
    roots.insert(9, a);
    roots.insert(9, b);
    let (a2, b2) = (fake(0x4_0000_4000), fake(0x4_0000_5000));
    for slot in roots.values_mut() {
        if *slot == a {
            *slot = a2;
        } else if *slot == b {
            *slot = b2;
        }
    }
    assert_eq!(roots.bucket(9), &[a2, b2]);
    assert!(
        !roots.insert(9, a2),
        "the moved object is still the same row"
    );
    assert_eq!(roots.len(), 2);
    assert!(roots.get(&10).is_none());
    assert!(!roots.is_empty());
}
