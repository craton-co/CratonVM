use super::*;

/// FUNNEL2-19-1: a set is `{i, i ^ 128}`; the primary slot is the
/// direct-mapped one, so the switch off probes what it always probed.
#[test]
fn the_two_ways_of_a_set_share_the_primary_slots_low_bits() {
    for addr in [0usize, 0x10, 0x7ff0, 0x1234_5670, usize::MAX & !0xF] {
        let p = lean_native_slot(addr);
        let a = lean_native_alt_slot(p);
        assert!(p < LEAN_NATIVE_SLOTS && a < LEAN_NATIVE_SLOTS);
        assert_ne!(p, a);
        assert_eq!(lean_native_alt_slot(a), p);
        assert_eq!(
            p & (LEAN_NATIVE_SLOTS / 2 - 1),
            a & (LEAN_NATIVE_SLOTS / 2 - 1)
        );
    }
}

/// FUNNEL2-19-1: the fill rewrites a record in place, takes an empty way
/// before evicting, and on a full set keeps the newest record in the
/// primary way (the older one moves to the alternate).
#[test]
fn the_two_way_fill_policy() {
    use LeanPlace::*;
    let site = 0x5170_0010usize;
    let other = 0x5170_0810usize;
    let empty = (0usize, 0u32);
    // The same record, wherever it is.
    assert_eq!(
        lean_native_two_way_place((site, 7), (other, 1), (site, 7)),
        Primary
    );
    assert_eq!(
        lean_native_two_way_place((other, 1), (site, 7), (site, 7)),
        Alternate
    );
    // Empty ways first.
    assert_eq!(
        lean_native_two_way_place(empty, (other, 1), (site, 7)),
        Primary
    );
    assert_eq!(
        lean_native_two_way_place((other, 1), empty, (site, 7)),
        Alternate
    );
    // One site, two receiver classes: two records.
    assert_eq!(
        lean_native_two_way_place((site, 7), empty, (site, 9)),
        Alternate
    );
    assert_eq!(
        lean_native_two_way_place((site, 7), (site, 9), (site, 9)),
        Alternate
    );
    // A full set of other records: demote.
    assert_eq!(
        lean_native_two_way_place((other, 1), (site, 7), (site, 9)),
        Demote
    );
}

/// FUNNEL2-19-2: every kind `try_jit_arraylist_leaf` answers for a
/// validated receiver is fileable; the plain callback is filed anyway, and
/// `Thread.currentThread` (a static site) never.
#[test]
fn the_fileable_leaf_kinds() {
    for kind in [
        LeafNativeKind::ArrayListGet,
        LeafNativeKind::ArrayListSize,
        LeafNativeKind::ChmGet,
        LeafNativeKind::IntWrapperHashCode,
        LeafNativeKind::IntWrapperEquals,
        LeafNativeKind::ObjectGetClass,
        LeafNativeKind::ReferenceRefersTo,
        LeafNativeKind::ClassIsInstance,
    ] {
        assert!(lean_leaf_kind_fileable(kind));
    }
    assert!(!lean_leaf_kind_fileable(LeafNativeKind::Callback));
    assert!(!lean_leaf_kind_fileable(
        LeafNativeKind::ThreadCurrentThread
    ));
    assert!(lean_leaf_kind_takes_reference(LeafNativeKind::ChmGet));
    assert!(lean_leaf_kind_takes_reference(
        LeafNativeKind::IntWrapperEquals
    ));
    assert!(lean_leaf_kind_takes_reference(
        LeafNativeKind::ReferenceRefersTo
    ));
    assert!(lean_leaf_kind_takes_reference(
        LeafNativeKind::ClassIsInstance
    ));
    assert!(!lean_leaf_kind_takes_reference(
        LeafNativeKind::ArrayListGet
    ));
}

/// The door's order, pinned by text (it cannot be driven without compiled
/// code): every guard and the receiver-class selection run before the
/// record is read out, and a leaf kind's probe runs after the thread is
/// taken and before the parameter decode.
#[test]
fn the_lean_door_reads_the_record_after_the_way_is_chosen() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = src
        .find("unsafe fn try_lean_native_door(")
        .expect("the door");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("end of the door")];
    let guards = body
        .find("lean_native_guards_hold(slot, vm)")
        .expect("guards");
    let way = body.find("slot = other;").expect("the second way");
    let read_out = body
        .find("let callback = (*slot).callback?;")
        .expect("read-out");
    let thread = body.find("jit_thread_mut()?").expect("thread");
    let probe = body.find("try_jit_arraylist_leaf(").expect("leaf probe");
    let decode = body
        .find("for (i, &tag) in params[..nparams]")
        .expect("decode");
    assert!(guards < way && way < read_out && read_out < thread);
    assert!(thread < probe && probe < decode);
}
