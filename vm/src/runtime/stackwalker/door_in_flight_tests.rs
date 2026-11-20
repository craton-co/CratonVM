use super::*;

#[test]
fn the_screen_takes_only_jdk_classes() {
    assert!(door_in_flight_screens("java/lang/Class"));
    assert!(door_in_flight_screens("jdk/internal/misc/Unsafe"));
    assert!(!door_in_flight_screens("javax/swing/JFrame"));
    assert!(!door_in_flight_screens("javafoo/Bar"));
    assert!(!door_in_flight_screens("com/example/Main"));
    assert!(!door_in_flight_screens(""));
}

fn row(interp_depth: u32, noted: bool) -> ReflectiveCallRow {
    ReflectiveCallRow {
        interp_depth,
        jit_depth: 0,
        kind: cratonvm_native_api::ReflectiveCallKind::MethodInvoke,
        check: None,
        raised: None,
        door_save: noted.then_some(None),
        adapter_pin: None,
    }
}

fn door(key: Option<u32>, interp_depth: u32, jit_depth: u32) -> DoorInFlight {
    DoorInFlight {
        key: key.map(|slot| (ClassId::new(9), slot)),
        interp_depth,
        jit_depth,
    }
}

/// Wave 64: the reflective call's own door, keyed, or a door entered
/// inside it keeps the accessor's raise off; an unnoted call, or the
/// call's own record with nothing resolved, does not.
#[test]
fn the_reflective_door_is_read_from_the_record() {
    assert!(reflective_door_keyed(
        &row(3, true),
        Some(door(Some(1), 3, 0))
    ));
    assert!(!reflective_door_keyed(
        &row(3, true),
        Some(door(None, 3, 0))
    ));
    assert!(reflective_door_keyed(&row(3, true), Some(door(None, 5, 0))));
    assert!(reflective_door_keyed(&row(3, true), Some(door(None, 3, 1))));
    assert!(!reflective_door_keyed(&row(3, true), None));
    assert!(!reflective_door_keyed(
        &row(3, false),
        Some(door(Some(1), 3, 0))
    ));
}

/// Wave 64: the door-key memo answers a repeated door, tells a
/// remembered "nothing keyed" from a miss, compares every input, and
/// forgets everything when a class is redefined.
#[test]
fn the_door_key_memo_answers_a_repeated_door() {
    let class = ClassId::new(4);
    let key = Some((class, 17));
    let desc = "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;";
    let mut memo = DoorKeyMemo::new(0);
    let id = DoorKeyOwnerRef::Id(class);
    assert_eq!(memo.lookup(0, id, "forName", desc), None);
    memo.insert(0, id, "forName", desc, key);
    assert_eq!(memo.lookup(0, id, "forName", desc), Some(key));
    assert_eq!(
        memo.lookup(0, id, "forName", "(Ljava/lang/String;)Ljava/lang/Class;"),
        None
    );
    assert_eq!(
        memo.lookup(0, DoorKeyOwnerRef::Id(ClassId::new(5)), "forName", desc),
        None
    );
    // A name door: the requester is part of the key.
    let by_name = DoorKeyOwnerRef::Name("java/lang/Class", Some(ClassId::new(30)));
    memo.insert(0, by_name, "forName", desc, None);
    assert_eq!(memo.lookup(0, by_name, "forName", desc), Some(None));
    let other_requester = DoorKeyOwnerRef::Name("java/lang/Class", Some(ClassId::new(31)));
    assert_eq!(memo.lookup(0, other_requester, "forName", desc), None);
    let other_name = DoorKeyOwnerRef::Name("java/lang/Klass", Some(ClassId::new(30)));
    assert_eq!(memo.lookup(0, other_name, "forName", desc), None);
    // A redefinition empties it.
    assert_eq!(memo.lookup(1, id, "forName", desc), None);
    assert_eq!(memo.lookup(1, by_name, "forName", desc), None);
    memo.insert(1, id, "forName", desc, key);
    assert_eq!(memo.lookup(1, id, "forName", desc), Some(key));
}

#[test]
fn the_record_fits_the_thread_tail_padding() {
    // `JvmThread` ends with an 8-byte field at 3024 inside a 3072-byte,
    // 64-aligned struct (`runtime::layout_ratchet_tests`): 40 bytes of
    // padding. The record must stay inside them.
    assert!(std::mem::size_of::<Option<DoorInFlight>>() <= 24);
    assert!(std::mem::align_of::<Option<DoorInFlight>>() <= 8);
}
