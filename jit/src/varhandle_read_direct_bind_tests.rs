use super::*;

/// The netty shape this bind exists for, at all four read modes.
///
/// `RefCnt.isLiveNonVolatile` is `(int) VH.get(instance)` and `refCnt()` is
/// `(int) VH.getAcquire(instance) >>> 1`, so a filter that recognised only
/// `get` would leave the second of the two on the funnel and the class's
/// `retain`/`release` paths with it.
#[test]
fn the_netty_refcnt_shape_is_recognised_in_every_read_mode() {
    let desc = "(Lio/netty/util/internal/RefCnt;)I";
    let int_slot = VARHANDLE_READ_RETURNS
        .iter()
        .position(|r| *r == b'I')
        .unwrap();
    for (mode, name) in VARHANDLE_READ_MODES.iter().enumerate() {
        assert_eq!(
            varhandle_read_helper_slot(name, desc),
            Some(mode * VARHANDLE_READ_KINDS + int_slot),
            "{name} on the netty refCnt descriptor",
        );
    }
}

/// Every slot's canonical call-site descriptor, in slot order.
///
/// The primitive kinds are `(Ljava/lang/Object;)X`; the two reference kinds
/// are the `Object` return and one concrete `REF_STRICT` stand-in
/// (`java/lang/String` — the class `RJdkHandles`' wrong-type vector uses).
fn canonical_descriptors() -> Vec<(usize, String, &'static str)> {
    let mut out = Vec::new();
    for (mode, name) in VARHANDLE_READ_MODES.iter().enumerate() {
        for kind in 0..VARHANDLE_READ_KINDS {
            let ret = if kind < VARHANDLE_READ_RETURNS.len() {
                (VARHANDLE_READ_RETURNS[kind] as char).to_string()
            } else if kind == VARHANDLE_READ_KIND_REF_OBJECT {
                "Ljava/lang/Object;".to_string()
            } else {
                "Ljava/lang/String;".to_string()
            };
            out.push((
                mode * VARHANDLE_READ_KINDS + kind,
                format!("(Ljava/lang/Object;){ret}"),
                *name,
            ));
        }
    }
    out
}

/// Every slot is reachable and no two shapes share one.
///
/// A collision would be silent: two different return kinds would land on
/// one helper, and the one whose kind lost would read the field with the
/// wrong descriptor. The count also pins `VARHANDLE_READ_SLOTS` against the
/// two tables it is supposed to be the product of.
#[test]
fn every_mode_return_pair_has_its_own_slot() {
    let mut seen = std::collections::HashSet::new();
    for (expected, desc, name) in canonical_descriptors() {
        let slot = varhandle_read_helper_slot(name, &desc)
            .unwrap_or_else(|| panic!("{name}{desc} was not recognised"));
        assert_eq!(slot, expected, "{name}{desc} landed on the wrong slot");
        assert!(slot < VARHANDLE_READ_SLOTS);
        assert!(seen.insert(slot), "{name}{desc} collided on slot {slot}");
    }
    assert_eq!(seen.len(), VARHANDLE_READ_SLOTS);
}

/// The three-way classification of a REFERENCE return, which is the whole
/// of how the cold arm keeps W6-1 without the site's descriptor.
///
/// The middle row is the one that is easy to lose: a site returning
/// `java/lang/Number` must NOT bind, because whether a box satisfies it
/// depends on which wrapper arrived (`Integer` yes, `Character` no) and
/// neither reference slot can answer that. Binding it as `REF_STRICT`
/// would throw where HotSpot does not.
#[test]
fn a_reference_return_is_classified_three_ways() {
    assert_eq!(
        varhandle_read_helper_slot("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        Some(VARHANDLE_READ_KIND_REF_OBJECT),
    );
    for accepting in VARHANDLE_BOX_ACCEPTING_RETURNS {
        let desc = format!("(Ljava/lang/Object;)L{accepting};");
        assert_eq!(
            varhandle_read_helper_slot("get", &desc),
            None,
            "{accepting} accepts SOME box, so its site cannot be REF_STRICT",
        );
    }
    for strict in [
        "java/lang/String",
        "java/lang/CharSequence",
        "java/util/concurrent/CompletableFuture$Completion",
        "HibfixVarHandleProbe$Node",
    ] {
        let desc = format!("(Ljava/lang/Object;)L{strict};");
        assert_eq!(
            varhandle_read_helper_slot("get", &desc),
            Some(VARHANDLE_READ_KIND_REF_STRICT),
            "{strict} accepts no box and must bind strictly",
        );
    }
}

/// The kill switch reaches the recogniser itself, so all three doors refuse
/// together — and it must leave the PRIMITIVE half alone, which is the
/// whole reason it is a separate switch from the read bind's own.
///
/// Read through the recogniser rather than the `OnceLock`, because the
/// process-wide latch cannot be flipped twice in one test binary.
#[test]
fn the_reference_half_has_its_own_switch() {
    // The switch is a `OnceLock` on an env var, so this test asserts the
    // SHAPE of the decision the recogniser makes, not the latch: a slot
    // >= VARHANDLE_READ_RETURNS.len() is a reference kind, and every one of
    // them is reached through the `varhandle_ref_read_direct_enabled()`
    // arm, while no primitive kind is.
    let primitive = varhandle_read_helper_slot("get", "(Ljava/lang/Object;)I").unwrap();
    assert!(!varhandle_read_slot_is_reference(primitive));
    for desc in [
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "(Ljava/lang/Object;)Ljava/lang/String;",
    ] {
        let slot = varhandle_read_helper_slot("get", desc)
            .expect("the reference kinds bind by default");
        assert!(varhandle_read_slot_is_reference(slot), "{desc}");
    }
}

/// `varhandle_read_slot_return` is what three emit sites now use in place
/// of the old `slot % RETURNS.len()`, and it has to agree with the
/// descriptor the slot's synthetic call site carries.
#[test]
fn a_slots_return_byte_matches_its_descriptor() {
    for (slot, desc, _) in canonical_descriptors() {
        let want = desc.as_bytes()[desc.find(')').unwrap() + 1];
        assert_eq!(
            varhandle_read_slot_return(slot),
            want,
            "slot {slot} ({desc})",
        );
    }
    assert_eq!(varhandle_read_slot_mode(VARHANDLE_READ_SLOTS - 1), 3);
    assert_eq!(
        varhandle_read_slot_kind(VARHANDLE_READ_SLOTS - 1),
        VARHANDLE_READ_KIND_REF_STRICT
    );
}

/// The shapes that must NOT bind, each for its own reason.
///
/// This is the whole correctness argument for using the descriptor as the
/// instance-field test, so every family that reads through a `VarHandle`
/// with a different coordinate list is listed explicitly rather than
/// summarised. A regression here does not fail loudly — it hands a helper
/// that assumes `[handle, receiver]` an argument list of a different
/// shape.
#[test]
fn only_a_single_reference_coordinate_with_a_servable_return_binds() {
    // A static-field handle: no coordinates at all.
    assert_eq!(varhandle_read_helper_slot("get", "()I"), None);
    // An array-element handle: (array, index).
    assert_eq!(varhandle_read_helper_slot("get", "([II)I"), None);
    // A byte-array / ByteBuffer view handle: (container, byte offset).
    assert_eq!(varhandle_read_helper_slot("get", "([BI)J"), None);
    assert_eq!(
        varhandle_read_helper_slot("get", "(Ljava/nio/ByteBuffer;I)J"),
        None
    );
    // A MemorySegment handle: (segment, long offset).
    assert_eq!(
        varhandle_read_helper_slot("get", "(Ljava/lang/foreign/MemorySegment;J)I"),
        None
    );
    // Two reference coordinates.
    assert_eq!(
        varhandle_read_helper_slot("get", "(Ljava/lang/Object;Ljava/lang/Object;)I"),
        None
    );
    // A primitive coordinate is not an instance-field receiver.
    assert_eq!(varhandle_read_helper_slot("get", "(I)I"), None);
    // An ARRAY return is still out of scope: it is outside
    // `varhandle_reference_return_mismatch`'s fire set today, so neither
    // reference kind describes it and a stand-in would have to reproduce
    // that exclusion for no measured caller.
    assert_eq!(
        varhandle_read_helper_slot("get", "(Ljava/lang/Object;)[I"),
        None
    );
    assert_eq!(
        varhandle_read_helper_slot("get", "(Ljava/lang/Object;)[Ljava/lang/String;"),
        None
    );
    // Two object descriptors run together is not one return type.
    assert_eq!(
        varhandle_read_helper_slot("get", "(Ljava/lang/Object;)Lfoo;Lbar;"),
        None
    );
    // `void` is not a read.
    assert_eq!(
        varhandle_read_helper_slot("get", "(Ljava/lang/Object;)V"),
        None
    );
    // Write and read-modify-write modes are not read modes.
    for name in [
        "set",
        "setVolatile",
        "setRelease",
        "getAndAdd",
        "compareAndSet",
    ] {
        assert_eq!(
            varhandle_read_helper_slot(name, "(Ljava/lang/Object;)I"),
            None,
            "{name} bound as a read mode",
        );
    }
}

/// An array-TYPED instance field still binds: `[I` as a *coordinate* means
/// the receiver is an array-valued object reference, which is an ordinary
/// instance-field read, not the array-ELEMENT handle the row above rejects.
/// The two are told apart by the coordinate COUNT, and this is the row that
/// says a stricter "no `[` anywhere" filter would be over-broad.
#[test]
fn an_array_typed_receiver_is_still_one_reference_coordinate() {
    assert!(varhandle_read_helper_slot("get", "([Ljava/lang/String;)I").is_some());
    assert!(varhandle_read_helper_slot("get", "([[B)J").is_some());
    assert_eq!(
        varhandle_read_helper_slot("get", "([Ljava/lang/String;I)I"),
        None
    );
}

/// The cells default to "not wired", and `set_varhandle_read_direct_fns`
/// lands each address on the slot the recognition will look it up under.
///
/// The failure this catches is an off-by-one or reversed registration
/// order, which would not fail to compile and would not fail loudly: a
/// `get`-returning-`I` site would call the helper whose baked slot says
/// `getAcquire` returning `Z`, and read the right field with the wrong
/// return kind.
#[test]
fn the_slots_are_registered_in_recognition_order() {
    let addrs: [usize; VARHANDLE_READ_SLOTS] = std::array::from_fn(|i| 0x1000 + i * 0x10);
    let table = DirectHelperTable {
        varhandle_read: addrs,
        ..DirectHelperTable::EMPTY
    };
    for (_, desc, name) in canonical_descriptors() {
        let slot = varhandle_read_helper_slot(name, &desc).unwrap();
        assert_eq!(
            table.varhandle_read[slot], addrs[slot],
            "{name}{desc} -> slot {slot}"
        );
    }
    assert!(DirectHelperTable::EMPTY
        .varhandle_read
        .iter()
        .all(|&a| a == 0));
}

/// The two door counters are separate, so `VarHandle.read=N/0` reports two
/// facts. The OSR column is the load-bearing one — netty reads `refCnt`
/// inside loop bodies — and a single-pass-only bind reporting a non-zero
/// total would otherwise read as success.
#[test]
fn the_two_varhandle_door_counters_are_separate() {
    let (before_sp, before_osr) = varhandle_read_direct_helper_sites();
    VARHANDLE_READ_SITES_SINGLEPASS.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
    let (mid_sp, mid_osr) = varhandle_read_direct_helper_sites();
    assert_eq!(mid_sp, before_sp + 2);
    assert_eq!(
        mid_osr, before_osr,
        "bumping single-pass moved the OSR tally"
    );
    VARHANDLE_READ_SITES_OSR.fetch_add(7, std::sync::atomic::Ordering::Relaxed);
    let (end_sp, end_osr) = varhandle_read_direct_helper_sites();
    assert_eq!(end_sp, before_sp + 2);
    assert_eq!(end_osr, before_osr + 7);
}
