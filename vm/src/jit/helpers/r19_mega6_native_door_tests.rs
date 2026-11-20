use super::*;

fn info(kind: u8) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/Object",
        method_name: "equals",
        descriptor: "(Ljava/lang/Object;)Z",
        num_jit_args: 2,
        return_type: b'Z',
        invoke_kind: kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

/// W4B: a refusal caused by the class manager being write-locked (the
/// receiver's name unreadable) is flagged transient so the door does not
/// remember it now that the native site memos survive publications.
#[test]
fn a_write_locked_class_manager_refusal_is_transient() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if site_cache_mode() == SiteCacheMode::Off {
        return;
    }
    let mut probe = info(0);
    probe.method_name = "w4bNoSuchSpecialCasedName";
    let transient = std::cell::Cell::new(false);
    let guard = shared.classes.class_manager.write();
    let r = resolve_native_site_tracked(&shared, &probe, Some(ClassId::new(41)), &transient);
    drop(guard);
    assert!(r.is_none());
    assert!(transient.get());
    // Unlocked, the same refusal (no such class) is a fact about the site.
    let transient = std::cell::Cell::new(false);
    let r = resolve_native_site_tracked(&shared, &probe, Some(ClassId::new(41)), &transient);
    assert!(r.is_none());
    assert!(!transient.get());
}

/// FUNNEL3-19-1: the keyed refusal is exactly the redefined-owner rule's
/// placeholder for the refusing class, made only for a virtual/interface
/// refusal with a receiver class and only under the switch; everything
/// else is cached as resolved.
#[test]
fn a_refusal_is_receiver_keyed_only_under_the_switch() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let on = shared
        .natives
        .native_methods
        .jit_native_site_refusal_by_receiver();
    let cid = Some(ClassId::new(41));
    for kind in [0u8, 2] {
        let keyed = native_site_refusal_keyed_by_receiver(&shared, &info(kind), cid, None);
        assert_eq!(keyed.is_some(), on, "kind {kind}");
        if let Some(entry) = keyed {
            assert!(entry.refused_for_receiver);
            assert_eq!(entry.receiver_class_id, Some(41));
            // The hit machinery: the refusing class declines, another
            // class re-resolves once, a second refusal is site-wide.
            assert!(!native_site_refill_for_receiver(Some(&entry), Some(41)));
            assert!(native_site_refill_for_receiver(Some(&entry), Some(42)));
            assert!(native_site_refill_outcome(Some(entry)).is_none());
        }
        // No receiver class: nothing to key on.
        assert!(
            native_site_refusal_keyed_by_receiver(&shared, &info(kind), None, None).is_none()
        );
    }
    // Static and `invokespecial` refusals stay site-wide in both arms.
    for kind in [1u8, 3] {
        assert!(
            native_site_refusal_keyed_by_receiver(&shared, &info(kind), cid, None).is_none()
        );
    }
    // A positive resolution is never replaced.
    let positive = NativeSiteCache {
        refused_for_receiver: false,
        ..NativeSiteCache::refusal_for_receiver(41, &info(0), 0)
    };
    let kept = native_site_refusal_keyed_by_receiver(&shared, &info(0), cid, Some(positive));
    assert!(kept.is_some_and(|e| !e.refused_for_receiver));
}

/// FUNNEL3-19-2: the site-cache door hands a validated argument only on a
/// warm call, for a reference-taking kind, from a set mask bit, and only
/// under the switch.
#[test]
fn the_site_door_hands_a_validated_argument_only_when_it_may() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let on = shared
        .natives
        .native_methods
        .jit_leaf_probe_validated_args();
    let args = [0x1000_0000_i64, 0x2000_0000_i64];
    let chm = LeafNativeKind::ChmGet;
    assert_eq!(
        site_leaf_probe_validated_arg(&shared, chm, true, 0b10, &args).is_some(),
        on
    );
    assert!(site_leaf_probe_validated_arg(&shared, chm, false, 0b10, &args).is_none());
    assert!(site_leaf_probe_validated_arg(&shared, chm, true, 0b01, &args).is_none());
    assert!(site_leaf_probe_validated_arg(&shared, chm, true, 0, &args).is_none());
    assert!(site_leaf_probe_validated_arg(&shared, chm, true, 0b10, &args[..1]).is_none());
    assert!(site_leaf_probe_validated_arg(
        &shared,
        LeafNativeKind::ArrayListGet,
        true,
        0b10,
        &args
    )
    .is_none());
}
