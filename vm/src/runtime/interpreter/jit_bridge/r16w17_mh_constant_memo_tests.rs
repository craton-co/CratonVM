use super::*;

/// A recorded handle answers only its own address, collection epoch and
/// call-site descriptor; a full table gives up older epochs' rows.
#[test]
fn a_recorded_handle_answers_only_its_epoch_and_site() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let handle = 0x7_0000_0040u64;
    note_mh_constant_target(&shared, handle, 5, "(I)I", "p/Host", "twice", "(I)I", 0x71);
    let t = mh_constant_target_of(&shared, handle, 5, "(I)I").expect("recorded");
    assert_eq!(
        (&*t.class, &*t.name, &*t.desc, t.class_id),
        ("p/Host", "twice", "(I)I", 0x71)
    );
    assert!(
        mh_constant_target_of(&shared, handle, 6, "(I)I").is_none(),
        "another epoch"
    );
    assert!(
        mh_constant_target_of(&shared, handle, 5, "(J)I").is_none(),
        "another site"
    );
    assert!(
        mh_constant_target_of(&shared, handle + 8, 5, "(I)I").is_none(),
        "another handle"
    );
    for i in 1..MH_CONSTANT_TARGETS_MAX as u64 {
        note_mh_constant_target(
            &shared,
            0x8_0000_0000 + i * 8,
            5,
            "(I)I",
            "p/H",
            "m",
            "(I)I",
            1,
        );
    }
    let fresh = 0x9_0000_0000u64;
    note_mh_constant_target(&shared, fresh, 6, "(I)I", "p/Host", "twice", "(I)I", 0x71);
    assert!(
        mh_constant_target_of(&shared, fresh, 6, "(I)I").is_some(),
        "room was made"
    );
    assert!(
        mh_constant_target_of(&shared, handle, 5, "(I)I").is_none(),
        "the older epoch's rows went"
    );
}

