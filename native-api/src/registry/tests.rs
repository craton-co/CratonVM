use super::*;
use crate::test_mock::MockNativeContext;

fn dummy_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn dummy_native_2(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(42)))
}

/// SBR round 4, lane compat: an adopted family (`retired_shadow::COMPAT_FAMILIES`)
/// drops its retired rows and its companion stand-ins under `Compatible` over
/// a real JDK with the row in force -- and only then. A `Bridge` of the same
/// class that is not a retired row stays, as it does under `JdkOnly`.
/// nio20 wave 2 (L5): the `ArrayList` family drops its retired rows and
/// its stated stub (`subList`) but not the excepted `ArrayList$Itr`'s
/// retired `remove`; `Object.equals` / `toString` move only once the
/// reflect-type carriers are retired (`set_type_annotation_bytes_real`).
#[test]
fn the_r20_wave2_rows_drop_their_families_and_object_identity_waits_for_the_impls() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    let size = ("java/util/ArrayList", "size", "()I");
    let sub_list = ("java/util/ArrayList", "subList", "(II)Ljava/util/List;");
    let itr_remove = ("java/util/ArrayList$Itr", "remove", "()V");
    let to_string = ("java/lang/Object", "toString", "()Ljava/lang/String;");
    let date = ("java/util/Date", "getYear", "()I");
    let build = |impls: bool| {
        let mut r = NativeMethodRegistry::new();
        r.set_real_jdk(true);
        r.set_type_annotation_bytes_real(impls);
        r.set_hotspot_alignments(
            HotSpotAlignments::NONE
                .with(Row::JdkArrayList, true)
                .with(Row::JdkDate, true)
                .with(Row::JdkObjectIdentityBodies, true),
        );
        {
            r.register(size.0, size.1, size.2, dummy_native, NativeKind::Bridge);
            r.register(
                itr_remove.0,
                itr_remove.1,
                itr_remove.2,
                dummy_native,
                NativeKind::Bridge,
            );
            r.register(
                to_string.0,
                to_string.1,
                to_string.2,
                dummy_native,
                NativeKind::Bridge,
            );
            r.register(date.0, date.1, date.2, dummy_native, NativeKind::Bridge);
        };
        {
            r.register(
                sub_list.0,
                sub_list.1,
                sub_list.2,
                dummy_native,
                NativeKind::SyntheticStub,
            );
        };
        r
    };
    let carriers_left = build(false);
    assert_eq!(carriers_left.kind_of(size.0, size.1, size.2), None);
    assert_eq!(
        carriers_left.kind_of(sub_list.0, sub_list.1, sub_list.2),
        None
    );
    assert_eq!(carriers_left.kind_of(date.0, date.1, date.2), None);
    assert_eq!(
        carriers_left.kind_of(itr_remove.0, itr_remove.1, itr_remove.2),
        Some(NativeKind::SyntheticStub),
        "the excepted cursor keeps its retired row (re-tagged, registered)"
    );
    assert_eq!(
        carriers_left.kind_of(to_string.0, to_string.1, to_string.2),
        Some(NativeKind::SyntheticStub),
        "Object.toString waits for the reflect-type impls"
    );
    let impls = build(true);
    assert_eq!(impls.kind_of(to_string.0, to_string.1, to_string.2), None);
    assert_eq!(impls.kind_of(size.0, size.1, size.2), None);
}

/// Interpreter round i2 wave 69, lane L1 (`JdkLockSupport`,
/// `CRATONVM_COMPAT_JDK_LOCK_SUPPORT`): `Compatible` over a real JDK with
/// the row (the default) registers none of the retired `LockSupport` rows,
/// so the JDK's bodies run as under `JdkOnly`; with the row off, or on the
/// synthetic class library, the stand-ins stay. `Unsafe.park` is untouched.
#[test]
fn the_lock_support_row_leaves_lock_support_to_its_bytecode() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    let park = (
        "java/util/concurrent/locks/LockSupport",
        "park",
        "(Ljava/lang/Object;)V",
    );
    let blocker = (
        "java/util/concurrent/locks/LockSupport",
        "getBlocker",
        "(Ljava/lang/Thread;)Ljava/lang/Object;",
    );
    let unsafe_park = ("jdk/internal/misc/Unsafe", "park", "(ZJ)V");
    let build = |rows: HotSpotAlignments, real_jdk: bool| {
        let mut r = NativeMethodRegistry::new();
        r.set_real_jdk(real_jdk);
        r.set_hotspot_alignments(rows);
        {
            r.register(park.0, park.1, park.2, dummy_native, NativeKind::Bridge);
            r.register(
                blocker.0,
                blocker.1,
                blocker.2,
                dummy_native,
                NativeKind::Bridge,
            );
            r.register(
                unsafe_park.0,
                unsafe_park.1,
                unsafe_park.2,
                dummy_native,
                NativeKind::Bridge,
            );
        };
        r
    };
    assert!(HotSpotAlignments::defaults().contains(Row::JdkLockSupport));
    let on = build(HotSpotAlignments::defaults(), true);
    assert!(on.aligned(Row::JdkLockSupport));
    assert!(on.find(park.0, park.1, park.2).is_none());
    assert!(on.find(blocker.0, blocker.1, blocker.2).is_none());
    assert_eq!(
        on.kind_of(unsafe_park.0, unsafe_park.1, unsafe_park.2),
        Some(NativeKind::Bridge)
    );
    let off = build(
        HotSpotAlignments::defaults().with(Row::JdkLockSupport, false),
        true,
    );
    assert_eq!(
        off.kind_of(park.0, park.1, park.2),
        Some(NativeKind::SyntheticStub)
    );
    assert!(off.find(blocker.0, blocker.1, blocker.2).is_some());
    let synthetic = build(HotSpotAlignments::defaults(), false);
    assert!(!synthetic.aligned(Row::JdkLockSupport));
    assert!(synthetic.find(park.0, park.1, park.2).is_some());
}

/// Interpreter round i2 wave 70, lane L1 (`JdkStampedLock`,
/// `CRATONVM_COMPAT_JDK_STAMPED_LOCK`): `Compatible` over a real JDK with
/// the row (the default) registers none of the `StampedLock` / lock-view
/// `SyntheticStub`s, so the JDK's bodies run as under `JdkOnly`; a
/// `Bridge` on the class would stay (none exists today). With the row
/// off, or on the synthetic class library, the stand-ins stay.
#[test]
fn the_stamped_lock_row_leaves_stamped_lock_to_its_bytecode() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    const SL: &str = "java/util/concurrent/locks/StampedLock";
    const WV: &str = "java/util/concurrent/locks/StampedLock$WriteLockView";
    let build = |rows: HotSpotAlignments, real_jdk: bool| {
        let mut r = NativeMethodRegistry::new();
        r.set_real_jdk(real_jdk);
        r.set_hotspot_alignments(rows);
        {
            r.register(
                SL,
                "readLock",
                "()J",
                dummy_native,
                NativeKind::SyntheticStub,
            );
            r.register(SL, "<init>", "()V", dummy_native, NativeKind::SyntheticStub);
            r.register(WV, "unlock", "()V", dummy_native, NativeKind::SyntheticStub);
        };
        {
            r.register(
                SL,
                "hypotheticalBridge",
                "()V",
                dummy_native,
                NativeKind::Bridge,
            );
        };
        r
    };
    assert!(HotSpotAlignments::defaults().contains(Row::JdkStampedLock));
    let on = build(HotSpotAlignments::defaults(), true);
    assert!(on.aligned(Row::JdkStampedLock));
    assert!(on.find(SL, "readLock", "()J").is_none());
    assert!(on.find(SL, "<init>", "()V").is_none());
    assert!(on.find(WV, "unlock", "()V").is_none());
    assert_eq!(
        on.kind_of(SL, "hypotheticalBridge", "()V"),
        Some(NativeKind::Bridge)
    );
    let off = build(
        HotSpotAlignments::defaults().with(Row::JdkStampedLock, false),
        true,
    );
    assert_eq!(
        off.kind_of(SL, "readLock", "()J"),
        Some(NativeKind::SyntheticStub)
    );
    assert!(off.find(WV, "unlock", "()V").is_some());
    let synthetic = build(HotSpotAlignments::defaults(), false);
    assert!(!synthetic.aligned(Row::JdkStampedLock));
    assert!(synthetic.find(SL, "readLock", "()J").is_some());
}

/// Interpreter round i3 wave 71, lane L1 (`JdkCountDownLatch`,
/// `CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH`): `Compatible` over a real JDK
/// with the row registers none of the `CountDownLatch` `SyntheticStub`s,
/// whether the kind is ambient (`register_concurrent_natives`) or stated
/// per call (`register_with_kind`, the `surefire_cdl` copy); a `Bridge`
/// on the class would stay. With the row off, or on the synthetic class
/// library, the stand-ins stay. The row is set explicitly both ways.
#[test]
fn the_count_down_latch_row_leaves_count_down_latch_to_its_bytecode() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    const CDL: &str = "java/util/concurrent/CountDownLatch";
    let build = |rows: HotSpotAlignments, real_jdk: bool| {
        let mut r = NativeMethodRegistry::new();
        r.set_real_jdk(real_jdk);
        r.set_hotspot_alignments(rows);
        {
            r.register(CDL, "await", "()V", dummy_native, NativeKind::SyntheticStub);
        };
        r.register(
            CDL,
            "countDown",
            "()V",
            dummy_native,
            NativeKind::SyntheticStub,
        );
        {
            r.register(
                CDL,
                "hypotheticalBridge",
                "()V",
                dummy_native,
                NativeKind::Bridge,
            );
        };
        r
    };
    let on = build(
        HotSpotAlignments::defaults().with(Row::JdkCountDownLatch, true),
        true,
    );
    assert!(on.aligned(Row::JdkCountDownLatch));
    assert!(on.find(CDL, "await", "()V").is_none());
    assert!(on.find(CDL, "countDown", "()V").is_none());
    assert_eq!(
        on.kind_of(CDL, "hypotheticalBridge", "()V"),
        Some(NativeKind::Bridge)
    );
    let off = build(
        HotSpotAlignments::defaults().with(Row::JdkCountDownLatch, false),
        true,
    );
    assert_eq!(
        off.kind_of(CDL, "await", "()V"),
        Some(NativeKind::SyntheticStub)
    );
    assert_eq!(
        off.kind_of(CDL, "countDown", "()V"),
        Some(NativeKind::SyntheticStub)
    );
    let synthetic = build(
        HotSpotAlignments::defaults().with(Row::JdkCountDownLatch, true),
        false,
    );
    assert!(!synthetic.aligned(Row::JdkCountDownLatch));
    assert!(synthetic.find(CDL, "await", "()V").is_some());
}

/// Round 14 wave 2 (lane shadow): the ancestor-walk mask answers only
/// under `JdkOnly`, only for a retired row, and never in `Compatible` mode
/// (whose dispatch must stay byte-for-byte unchanged).
/// hcb (JIT round 16): `Compatible` over a real JDK with the
/// `JdkHashCollections` row registers none of the hash family's retired
/// rows and says so through `aligned`; without a real JDK,
/// or without the row, both stay as they were.
#[test]
fn the_hash_collection_row_drops_the_family_only_over_a_real_jdk() {
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
    let get = (
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    );
    let other = ("java/util/concurrent/ConcurrentHashMap", "size", "()I");
    let build = |real_jdk: bool, row: bool| {
        let mut r = NativeMethodRegistry::new();
        r.set_hotspot_alignments(HotSpotAlignments::NONE.with(Row::JdkHashCollections, row));
        r.set_real_jdk(real_jdk);
        r.register(get.0, get.1, get.2, dummy_native, NativeKind::Bridge);
        r.register(other.0, other.1, other.2, dummy_native, NativeKind::Bridge);
        r
    };
    let on = build(true, true);
    assert!(on.aligned(Row::JdkHashCollections));
    assert!(
        on.find(get.0, get.1, get.2).is_none(),
        "the row drops HashMap.get"
    );
    assert!(
        on.find(other.0, other.1, other.2).is_some(),
        "and nothing outside the family"
    );
    for (real_jdk, row) in [(true, false), (false, true)] {
        let r = build(real_jdk, row);
        assert!(
            !r.aligned(Row::JdkHashCollections),
            "real_jdk={real_jdk} row={row}"
        );
        assert!(
            r.find(get.0, get.1, get.2).is_some(),
            "real_jdk={real_jdk} row={row}"
        );
    }
    let mut strict = NativeMethodRegistry::new();
    strict.set_compatibility_mode(CompatibilityMode::JdkOnly);
    assert!(strict.aligned(Row::JdkHashCollections));
}

/// A re-registration that expresses NO opinion about the kind must not
/// downgrade one that did.
///
/// `current_category` defaults to `SyntheticStub`, so a registrar that
/// re-registers a triple purely to win last-write-wins on the CALLBACK —
/// which several deliberately do — used to demote the slot's kind as a
/// side effect. That is not cosmetic: `JdkOnly` refuses a `SyntheticStub`,
/// `CRATONVM_NO_STUBS` drops one, and only a `SyntheticStub` is subject to
/// the yield-to-real-bytecode arbitration.
///
/// Both directions are asserted. Keeping only the first would pass on an
/// implementation that froze the kind at its first value and made
/// `register_with_kind` unable to reclassify anything.
#[test]
fn explicit_reregistration_updates_kind_and_callback() {
    let desc = "()V";
    let mut r = NativeMethodRegistry::new();

    r.register(
        "java/util/Demo",
        "m",
        desc,
        dummy_native,
        NativeKind::Bridge,
    );
    assert_eq!(
        r.kind_of("java/util/Demo", "m", desc),
        Some(NativeKind::Bridge)
    );

    r.register(
        "java/util/Demo",
        "m",
        desc,
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    assert_eq!(
        r.kind_of("java/util/Demo", "m", desc),
        Some(NativeKind::SyntheticStub),
        "explicit SyntheticStub re-registration must reclassify downward"
    );
    let id = r
        .resolve_id("java/util/Demo", "m", desc)
        .expect("registered");
    let cb = r.callback_of(id).expect("callback");
    let mut ctx = MockNativeContext::new();
    assert_eq!(
        cb(&mut ctx, &[]).expect("call"),
        Some(Value::Int(42)),
        "last-registration-wins on the CALLBACK is unchanged"
    );

    r.register(
        "java/util/Demo",
        "m",
        desc,
        dummy_native,
        NativeKind::Intrinsic,
    );
    assert_eq!(
        r.kind_of("java/util/Demo", "m", desc),
        Some(NativeKind::Intrinsic),
        "explicit Intrinsic re-registration must reclassify upward"
    );
}

/// The leaf claim is a property of the registered CALLBACK, and a later
/// phase that re-registers the triple without opting in must take it away.
///
/// This is the whole reason `set_leaf` is a scoped ambient flag rather than
/// a `mark_leaf(class, method, descriptor)` post-pass. `native-builtins`
/// genuinely does re-register triples: `register_atomic_integer_natives`
/// runs twice, and phase 54 installs its own (buggy) atomics in between. A
/// triple-keyed post-pass would keep claiming "this is a leaf" for whichever
/// body happened to win the slot last, and the leaf contract — no
/// allocation, no safepoint, no collection, no JNI exception — would be
/// asserted about code that never agreed to it.
#[test]
fn a_re_registration_that_does_not_opt_in_drops_the_leaf_claim() {
    let mut r = NativeMethodRegistry::new();

    r.set_leaf(true);
    r.register("p/C", "get", "()I", dummy_native, NativeKind::SyntheticStub);
    r.set_leaf(false);
    let id = r
        .resolve_id("p/C", "get", "()I")
        .expect("registered triple resolves");
    assert!(r.is_leaf_id(id), "the opted-in registration claims leaf");

    // A later phase re-registers the SAME triple with a different body and
    // says nothing about leafness.
    r.register(
        "p/C",
        "get",
        "()I",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    assert_eq!(
        r.resolve_id("p/C", "get", "()I"),
        Some(id),
        "re-registration updates the slot in place, so the handle stays valid"
    );
    assert!(
        !r.is_leaf_id(id),
        "the new body never asserted the leaf contract, so the claim must be gone"
    );

    // And restating it brings it back, which is what the second
    // `register_atomic_integer_natives` pass relies on.
    r.set_leaf(true);
    r.register("p/C", "get", "()I", dummy_native, NativeKind::SyntheticStub);
    r.set_leaf(false);
    assert!(r.is_leaf_id(id));
}

/// Nothing is a leaf unless it says so, and `with_leaf` restores.
#[test]
fn the_leaf_claim_defaults_off_and_is_scoped() {
    let mut r = NativeMethodRegistry::new();
    r.register(
        "p/C",
        "plain",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    let plain = r.resolve_id("p/C", "plain", "()V").expect("resolves");
    assert!(!r.is_leaf_id(plain), "default is the full funnel");

    r.with_leaf(true, |r| {
        r.register(
            "p/C",
            "leafy",
            "()V",
            dummy_native,
            NativeKind::SyntheticStub,
        );
    });
    let leafy = r.resolve_id("p/C", "leafy", "()V").expect("resolves");
    assert!(r.is_leaf_id(leafy));
    assert!(!r.current_leaf(), "with_leaf restores the previous claim");

    r.register(
        "p/C",
        "after",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    let after = r.resolve_id("p/C", "after", "()V").expect("resolves");
    assert!(
        !r.is_leaf_id(after),
        "a registration outside the scope is not leaf"
    );

    let leaves = r.leaf_registrations();
    assert_eq!(leaves, vec![("p/C", "leafy", "()V")]);
}

/// A handle from another registry must not read as leaf. `is_leaf_id` is
/// consulted on a dispatch path that would then skip the funnel's GC
/// bookkeeping, so "unknown" has to mean "take the funnel".
#[test]
fn an_out_of_range_handle_is_not_leaf() {
    let mut a = NativeMethodRegistry::new();
    a.set_leaf(true);
    for i in 0..4 {
        a.register(
            "p/C",
            &format!("m{i}"),
            "()V",
            dummy_native,
            NativeKind::SyntheticStub,
        );
    }
    a.set_leaf(false);
    let far = a.resolve_id("p/C", "m3", "()V").expect("resolves");

    let b = NativeMethodRegistry::new();
    assert!(
        !b.is_leaf_id(far),
        "an id that does not index this registry's slots is not a leaf"
    );
}

#[test]
fn native_handle_scope_releases_nested_roots_on_all_rust_exit_paths() {
    fn root_then_return(ctx: &mut dyn NativeContext, object: ObjectRef) {
        let mut scope = NativeHandleScope::new(ctx);
        let handle = scope.root(object);
        assert_eq!(scope.get(&handle), object);
    }

    let mut ctx = MockNativeContext::new();
    let first = ctx.fresh_object_ref();
    root_then_return(&mut ctx, first);
    assert_eq!(ctx.handle_slot_count(), 0);
    assert_eq!(ctx.handle_scope_depth(), 0);

    let outer_object = ctx.fresh_object_ref();
    let inner_object = ctx.fresh_object_ref();
    {
        let mut outer = NativeHandleScope::new(&mut ctx);
        let outer_handle = outer.root(outer_object);
        {
            let mut inner = NativeHandleScope::new(&mut *outer);
            let inner_handle = inner.root(inner_object);
            assert_eq!(inner.get(&inner_handle), inner_object);
        }
        assert_eq!(outer.get(&outer_handle), outer_object);
    }
    assert_eq!(ctx.handle_slot_count(), 0);
    assert_eq!(ctx.handle_scope_depth(), 0);

    let unwind_object = ctx.fresh_object_ref();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut scope = NativeHandleScope::new(&mut ctx);
        let _handle = scope.root(unwind_object);
        panic!("exercise NativeHandleScope::drop during unwind");
    }));
    assert!(panicked.is_err());
    assert_eq!(ctx.handle_slot_count(), 0);
    assert_eq!(ctx.handle_scope_depth(), 0);
}

fn legacy_native_method_hash(class: &str, method: &str, descriptor: &str) -> (u64, u64) {
    fn pass(class: &str, method: &str, descriptor: &str, basis: u64, prime: u64) -> u64 {
        let mut h = basis;
        for (idx, component) in [class, method, descriptor].iter().enumerate() {
            for byte in component.bytes() {
                h ^= byte as u64;
                h = h.wrapping_mul(prime);
            }
            if idx < 2 {
                h ^= b'.' as u64;
                h = h.wrapping_mul(prime);
            }
        }
        h
    }

    (
        fmix64(pass(
            class,
            method,
            descriptor,
            0xcbf29ce484222325,
            0x100000001b3,
        )),
        fmix64(pass(
            class,
            method,
            descriptor,
            0x9e3779b97f4a7c15,
            0x880355f21e6d1965,
        )),
    )
}

// -----------------------------------------------------------------------
// NativeMethodRegistry basics
// -----------------------------------------------------------------------

#[test]
fn register_and_find() {
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/lang/Object",
        "registerNatives",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(registry
        .find("java/lang/Object", "registerNatives", "()V")
        .is_some());
    assert_eq!(registry.len(), 1);
}

#[test]
fn find_missing_returns_none() {
    let registry = NativeMethodRegistry::new();
    assert!(registry
        .find("java/lang/Object", "hashCode", "()I")
        .is_none());
    assert!(registry.is_empty());
}

#[test]
fn new_registry_is_empty() {
    let registry = NativeMethodRegistry::new();
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
}

#[test]
fn default_registry_is_empty() {
    let registry = NativeMethodRegistry::default();
    assert!(registry.is_empty());
}

#[test]
fn register_multiple_methods() {
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/lang/Object",
        "registerNatives",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/Object",
        "hashCode",
        "()I",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/System",
        "currentTimeMillis",
        "()J",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert_eq!(registry.len(), 3);
    assert!(!registry.is_empty());
}

#[test]
fn find_distinguishes_by_class() {
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/lang/Object",
        "toString",
        "()Ljava/lang/String;",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/String",
        "toString",
        "()Ljava/lang/String;",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    assert!(registry
        .find("java/lang/Object", "toString", "()Ljava/lang/String;")
        .is_some());
    assert!(registry
        .find("java/lang/String", "toString", "()Ljava/lang/String;")
        .is_some());
    // Different class, same method + descriptor
    assert!(registry
        .find("java/lang/Integer", "toString", "()Ljava/lang/String;")
        .is_none());
}

#[test]
fn find_distinguishes_by_descriptor() {
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/lang/Math",
        "abs",
        "(I)I",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/Math",
        "abs",
        "(J)J",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    assert!(registry.find("java/lang/Math", "abs", "(I)I").is_some());
    assert!(registry.find("java/lang/Math", "abs", "(J)J").is_some());
    assert!(registry.find("java/lang/Math", "abs", "(D)D").is_none());
}

#[test]
fn method_descriptor_prefilter_ignores_class() {
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/lang/Object",
        "toString",
        "()Ljava/lang/String;",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(registry.might_have_method_descriptor("toString", "()Ljava/lang/String;"));
    assert!(!registry.might_have_method_descriptor("toString", "()I"));
    assert!(!registry.might_have_method_descriptor("hashCode", "()Ljava/lang/String;"));
}

#[test]
fn method_descriptor_prefilter_is_conservative_for_quirky_descriptors() {
    let registry = NativeMethodRegistry::new();
    assert!(registry.might_have_method_descriptor("m", " ()V"));
    assert!(registry.might_have_method_descriptor("m", "()Ljava/lang/String"));
}

#[test]
fn overwrite_same_triple() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("A", "b", "()V", dummy_native, NativeKind::SyntheticStub);
    // Re-registering the same triple should overwrite without panic
    registry.register("A", "b", "()V", dummy_native_2, NativeKind::SyntheticStub);
    assert_eq!(registry.len(), 1);
}

#[test]
fn debug_format_shows_count() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("A", "b", "()V", dummy_native, NativeKind::SyntheticStub);
    let dbg = format!("{:?}", registry);
    assert!(dbg.contains("count: 1"));
}

// -----------------------------------------------------------------------
// Negative-lookup class prefilter
// -----------------------------------------------------------------------

/// The one invariant the prefilter can get wrong in a way that MATTERS:
/// a class that has a surviving registration but is missing from the set
/// makes `slot_for_exact` answer `None` for a native that exists. Assert
/// it over the whole registration log rather than for one sample triple,
/// so a future `register` early-return added above the insert is caught.
#[test]
fn every_registered_class_is_in_the_prefilter() {
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/lang/Object",
        "hashCode",
        "()I",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/String",
        "length",
        "()I",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    {
        let r = &mut registry;
        r.register(
            "sun/misc/Unsafe",
            "getInt",
            "(Ljava/lang/Object;J)I",
            dummy_native,
            NativeKind::Bridge,
        );
    };
    registry.alias_class("java/lang/String", "java/lang/CharSequence");

    for (class, method, descriptor) in registry.registrations.iter() {
        assert!(
            registry
                .classes_with_natives
                .contains(&native_class_hash(class)),
            "{class} is in the registration log but not the prefilter, so \
                 {class}.{method}{descriptor} would resolve to None"
        );
        assert!(
            registry.find(class, method, descriptor).is_some(),
            "{class}.{method}{descriptor} must still resolve through the prefilter"
        );
    }
}

/// A class with no registration at all must miss — that is the whole point
/// — and must miss for every method name, not just the ones tried above.
#[test]
fn unregistered_class_misses_without_finishing_the_digest() {
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/lang/Object",
        "hashCode",
        "()I",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(registry
        .find("org/h2/mvstore/MVMap", "get", "()V")
        .is_none());
    assert!(registry
        .kind_of("org/h2/mvstore/MVMap", "put", "()V")
        .is_none());
    assert!(registry
        .find_with_kind("org/h2/mvstore/MVMap", "hashCode", "()I")
        .is_none());
    // …while a registered triple on a registered class still resolves.
    assert!(registry
        .find("java/lang/Object", "hashCode", "()I")
        .is_some());
    // …and an unregistered METHOD on a registered class still misses,
    // which is the prefilter's false-positive path falling through to the
    // full digest + name verification.
    assert!(registry
        .find("java/lang/Object", "toString", "()V")
        .is_none());
}

/// The split hash must be bit-identical to the one-shot form it replaced:
/// `slot_by_key` entries written by `register` (one-shot) are probed by
/// `slot_for_exact` (split).
#[test]
fn split_hash_matches_the_one_shot_digest() {
    for (c, m, d) in [
        ("java/lang/Object", "hashCode", "()I"),
        ("", "", ""),
        ("a", "b", "c"),
        (
            "jdk/internal/misc/Unsafe",
            "compareAndSetLong",
            "(Ljava/lang/Object;JJJ)Z",
        ),
    ] {
        assert_eq!(
            native_method_hash(c, m, d),
            native_method_hash_from(native_class_hash(c), m, d),
            "split digest diverged for {c}.{m}{d}"
        );
    }
}

// -----------------------------------------------------------------------
// FNV hash function tests
// -----------------------------------------------------------------------

#[test]
fn hash_deterministic() {
    let h1 = native_method_hash("java/lang/Object", "hashCode", "()I");
    let h2 = native_method_hash("java/lang/Object", "hashCode", "()I");
    assert_eq!(h1, h2);
}

#[test]
fn hash_differs_for_different_inputs() {
    let h1 = native_method_hash("java/lang/Object", "hashCode", "()I");
    let h2 = native_method_hash("java/lang/Object", "toString", "()Ljava/lang/String;");
    let h3 = native_method_hash("java/lang/String", "hashCode", "()I");
    assert_ne!(h1, h2);
    assert_ne!(h1, h3);
}

#[test]
fn hash_differs_for_swapped_components() {
    // "A.B.()V" vs "B.A.()V" — the separator dots are included in hashing
    let h1 = native_method_hash("A", "B", "()V");
    let h2 = native_method_hash("B", "A", "()V");
    assert_ne!(h1, h2);
}

#[test]
fn hash_empty_strings() {
    // Edge case: empty strings should not panic
    let h = native_method_hash("", "", "");
    // FNV offset basis processed through the separator dots — both
    // halves should be non-zero.
    assert!(h.0 != 0 && h.1 != 0);
}

#[test]
fn hash_two_halves_independent() {
    // The two 64-bit halves are produced by different hash functions
    // (different multipliers) plus independent finalization mixes, so
    // the same triple yields two distinct 64-bit values.
    let (h1, h2) = native_method_hash("java/lang/Object", "hashCode", "()I");
    assert_ne!(h1, h2);
}

#[test]
fn hash_single_scan_preserves_legacy_keys() {
    let cases = [
            ("java/lang/Object", "hashCode", "()I"),
            (
                "org/hibernate/query/sqm/function/AbstractSqmSelfRenderingFunctionDescriptor",
                "generateSqmExpression",
                "(Ljava/util/List;Lorg/hibernate/query/ReturnableType;Lorg/hibernate/query/spi/QueryEngine;)Lorg/hibernate/query/sqm/tree/expression/SqmExpression;",
            ),
            ("", "methodOnly", "(Ljava/lang/Object;)V"),
        ];

    for (class, method, descriptor) in cases {
        assert_eq!(
            native_method_hash(class, method, descriptor),
            legacy_native_method_hash(class, method, descriptor)
        );
    }
}

#[test]
fn alias_class_copies_registrations() {
    // Regression: `alias_class` used to walk a separate `keys`
    // reverse map. After removing that map, it now walks the
    // `registrations` log instead. Verify the public behavior is
    // unchanged.
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/sql/PreparedStatement",
        "execute",
        "()Z",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/sql/PreparedStatement",
        "close",
        "()V",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    registry.alias_class("java/sql/PreparedStatement", "java/sql/CallableStatement");
    assert!(registry
        .find("java/sql/CallableStatement", "execute", "()Z")
        .is_some());
    assert!(registry
        .find("java/sql/CallableStatement", "close", "()V")
        .is_some());
    // Original registrations still present.
    assert!(registry
        .find("java/sql/PreparedStatement", "execute", "()Z")
        .is_some());
}

#[test]
fn real_layout_mode_drops_enumset_native_surface() {
    let of_two_desc = "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;";

    let mut normal = NativeMethodRegistry::new();
    normal.register(
        "java/util/EnumSet",
        "of",
        of_two_desc,
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(normal
        .find("java/util/EnumSet", "of", of_two_desc)
        .is_some());

    let mut real_layout = NativeMethodRegistry::new();
    real_layout.set_drop_real_layout_synthetic(true);
    real_layout.register(
        "java/util/EnumSet",
        "of",
        of_two_desc,
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(real_layout
        .find("java/util/EnumSet", "of", of_two_desc)
        .is_none());

    real_layout.register(
        "java/util/EnumSet",
        "size",
        "()I",
        dummy_native_2,
        NativeKind::Intrinsic,
    );
    assert!(real_layout
        .find("java/util/EnumSet", "size", "()I")
        .is_none());

    real_layout.register(
        "java/util/HashSet",
        "size",
        "()I",
        dummy_native_2,
        NativeKind::Intrinsic,
    );
    assert!(real_layout
        .find("java/util/HashSet", "size", "()I")
        .is_some());

    real_layout.register(
        "java/io/StringReader",
        "<init>",
        "(Ljava/lang/String;)V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(real_layout
        .find("java/io/StringReader", "<init>", "(Ljava/lang/String;)V")
        .is_none());

    // `java/io/InputStreamReader`: the 2026-09-17 addition. Both the
    // `<init>` that parks a stream in `Reader.lock`'s slot and the partial
    // `read()I` surface that leaves the bulk `read([CII)I` on real bytecode
    // must be refused — registering EITHER on a real layout is what left
    // `sd` null and NPE'd every `readLine()` in the image.
    real_layout.register(
        "java/io/InputStreamReader",
        "<init>",
        "(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(real_layout
        .find(
            "java/io/InputStreamReader",
            "<init>",
            "(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V"
        )
        .is_none());
    real_layout.register(
        "java/io/InputStreamReader",
        "read",
        "()I",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    assert!(real_layout
        .find("java/io/InputStreamReader", "read", "()I")
        .is_none());

    // ... and the same triples DO survive on a registry that is not the
    // real-JDK arm, so the assertions above are the filter talking and not
    // a registration that silently never happens.
    let mut synthetic = NativeMethodRegistry::new();
    synthetic.register(
        "java/io/InputStreamReader",
        "<init>",
        "(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(synthetic
        .find(
            "java/io/InputStreamReader",
            "<init>",
            "(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V"
        )
        .is_some());

    real_layout.register(
        "java/util/concurrent/LinkedBlockingDeque",
        "<init>",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(real_layout
        .find("java/util/concurrent/LinkedBlockingDeque", "<init>", "()V")
        .is_none());

    real_layout.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "<init>",
        "(ILjava/util/concurrent/ThreadFactory;)V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(real_layout
        .find(
            "java/util/concurrent/ScheduledThreadPoolExecutor",
            "<init>",
            "(ILjava/util/concurrent/ThreadFactory;)V",
        )
        .is_none());

    // `keep_real_scheduled_executor_bridge` used to keep exactly this
    // triple alive under `Bridge` here -- the one real-layout survivor
    // this test relied on as its control. It is retired now (both
    // triples it kept are in `RETIRED_SHADOW_L5_TRIPLES` instead,
    // lane-concurrency-forkjoin-scheduler-20260917.md item 2), so `Bridge`
    // no longer saves it: this class drops unconditionally in real-layout
    // mode, same as `LinkedBlockingDeque` above.
    real_layout.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "<init>",
        "(ILjava/util/concurrent/ThreadFactory;Ljava/util/concurrent/RejectedExecutionHandler;)V",
        dummy_native,
        NativeKind::Bridge,
    );
    assert!(real_layout
            .find(
                "java/util/concurrent/ScheduledThreadPoolExecutor",
                "<init>",
                "(ILjava/util/concurrent/ThreadFactory;Ljava/util/concurrent/RejectedExecutionHandler;)V",
            )
            .is_none());

    real_layout.register(
        "java/util/concurrent/Executors",
        "newScheduledThreadPool",
        "(I)Ljava/util/concurrent/ScheduledExecutorService;",
        dummy_native,
        NativeKind::Bridge,
    );
    assert!(real_layout
        .find(
            "java/util/concurrent/Executors",
            "newScheduledThreadPool",
            "(I)Ljava/util/concurrent/ScheduledExecutorService;",
        )
        .is_none());

    real_layout.register(
        "java/util/regex/Pattern",
        "matcher",
        "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
        dummy_native,
        NativeKind::Bridge,
    );
    assert!(real_layout
        .find(
            "java/util/regex/Pattern",
            "matcher",
            "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
        )
        .is_none());

    real_layout.register(
        "java/util/regex/Matcher",
        "matches",
        "()Z",
        dummy_native,
        NativeKind::Bridge,
    );
    assert!(real_layout
        .find("java/util/regex/Matcher", "matches", "()Z")
        .is_none());
}

/// A retirement must not silently disarm a real-JDK `keep_real_*_bridge`.
///
/// `register` re-tags a retired triple `Bridge` -> `SyntheticStub` BEFORE
/// calling `register_inner`, and it does so in every mode, because the
/// table is per-triple. Inside `register_inner`, real-JDK mode keeps a
/// small number of natives it cannot safely run as bytecode, and each keep
/// is a predicate over `effective_category() == NativeKind::Bridge` — which
/// the re-tag has already falsified. A triple that is BOTH retired and
/// keep-listed therefore loses its native in real-JDK mode, which is not
/// what a §1.4 shadow retirement is for and is a mode no retiring lane
/// measures.
///
/// # The discriminator, because "kept" alone does not mean "keep-listed"
///
/// Real-layout mode drops by CLASS+METHOD arms that are hand-written for
/// the classes it knows about. Most retired triples — `CopyOnWriteArrayList`
/// and friends — are named by no arm at all and are simply KEPT in real-JDK
/// mode; they are refused only in `--real-jdk`, by `allowed_in(JdkOnly)`,
/// which is exactly what their retirement is for. So `find(..).is_some()`
/// catches all of those and says nothing. **The first version of this test
/// asserted that and was red on the whole population.**
///
/// What identifies a collision is that the KIND decides the outcome.
/// Register the same triple twice under real-layout mode:
///
/// ```text
///   category        no arm names it   class-wide drop arm   keep-listed
///   Bridge          kept              dropped               KEPT
///   SyntheticStub   kept              dropped               dropped
/// ```
///
/// Only the third column disagrees with itself, and disagreeing is the
/// defect: the re-tag turns the left cell into the right one. So the
/// offender condition is **kept as `Bridge`, dropped as `SyntheticStub`**,
/// which reads the real arms rather than restating their triple lists, and
/// covers a keep arm written tomorrow on the day it lands.
///
/// On 2026-09-10 this caught two: `ScheduledThreadPoolExecutor`'s 3-arg
/// constructor and `getCorePoolSize()I`, which
/// `keep_real_scheduled_executor_bridge` holds for Spring's
/// `ThreadPoolTaskScheduler` anonymous subclass. They were removed from
/// `RETIRED_SHADOW_L5_TRIPLES` before it landed.
///
/// A failure here does NOT mean "delete the keep arm". It means the triple
/// cannot be retired by the table, because the table cannot tell the two
/// modes apart: `drop_real_layout_synthetic` is set in real-JDK AND in
/// `--real-jdk` (`vm_init.rs`), so there is no flag to branch the re-tag
/// on. Take the row out of the table -- or, as the two ForkJoin arms do
/// since SBR wave 4 lane FJP, make the arm honour
/// `retagging_retired_shadow`, which keeps the re-tagged registration in
/// real-JDK `--compatible` (where a `SyntheticStub` dispatches) and leaves
/// the drop to `--real-jdk`'s stub refusal, which runs before the keep arms.
/// The "dropped as `SyntheticStub`" half is therefore taken through
/// `register` itself, the way the VM registers it.
#[test]
fn real_layout_bridge_keeps_are_not_retired_shadows() {
    // One registration into a fresh real-layout registry, under `kind`,
    // reporting whether the native survived.
    fn survives_real_layout(
        kind: NativeKind,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        let mut real_layout = NativeMethodRegistry::new();
        real_layout.set_drop_real_layout_synthetic(true);
        // `register_inner`, not `register`: the point is to observe
        // `register_inner`'s own decision under a category of our
        // choosing, without the re-tag replacing it first.
        real_layout.register_inner(class_name, method_name, descriptor, dummy_native, kind);
        real_layout
            .find(class_name, method_name, descriptor)
            .is_some()
    }
    // The same registration made the way the VM makes it: a `Bridge`
    // through `register`, which re-tags a retired triple to
    // `SyntheticStub` and marks the call as a re-tag
    // (`retagging_retired_shadow`). A keep arm that honours that mark
    // (the two ForkJoin arms, SBR wave 4 lane FJP) keeps the re-tagged
    // registration, so the native survives in real-JDK `--compatible`
    // and only `--real-jdk` drops it; an arm that does not honour it
    // drops it in every real-JDK mode, which is the defect this test
    // exists to catch.
    fn survives_retag_real_layout(class_name: &str, method_name: &str, descriptor: &str) -> bool {
        let mut real_layout = NativeMethodRegistry::new();
        real_layout.set_drop_real_layout_synthetic(true);
        real_layout.register(
            class_name,
            method_name,
            descriptor,
            dummy_native,
            NativeKind::Bridge,
        );
        real_layout
            .find(class_name, method_name, descriptor)
            .is_some()
    }

    let mut offenders: Vec<String> = Vec::new();
    let mut population = 0usize;
    for table in crate::retired_shadow::RETIRED_SHADOW_TABLES {
        for (class_name, method_name, descriptor) in table.iter() {
            population += 1;
            // Every row must be reachable through the predicate, or this
            // gate is scoring rows the VM never retires.
            assert!(
                    crate::retired_shadow::triple_is_retired_shadow(
                        class_name,
                        method_name,
                        descriptor
                    ),
                    "{class_name}.{method_name}{descriptor} is in a retired-shadow table but `triple_is_retired_shadow` says no — the class is outside `RETIRED_SHADOW_PREFIXES`"
                );

            let as_bridge =
                survives_real_layout(NativeKind::Bridge, class_name, method_name, descriptor);
            let as_stub = survives_retag_real_layout(class_name, method_name, descriptor);
            if as_bridge && !as_stub {
                offenders.push(format!("{class_name}.{method_name}{descriptor}"));
            }
        }
    }
    assert!(
            population > 400,
            "the retired population collapsed to {population} — `RETIRED_SHADOW_TABLES` is probably missing a table, which would make this gate quietly vacuous"
        );
    assert!(
            offenders.is_empty(),
            "these retired-shadow triples are KEPT by real-layout mode as a `Bridge` and DROPPED as a `SyntheticStub`, so they are named by a `keep_real_*_bridge` arm and the re-tag in `register` disarms it — the native goes away in real-JDK mode too, which the retiring lane did not measure. Remove them from the table (see this test's doc comment; do not touch the keep arm):
  {}",
            offenders.join("
  ")
        );
}

#[test]
fn deferred_native_ring_name_flush_resolves() {
    // PERF (native-ring lazy name map): with the ring disabled (the test
    // default), `register()` records only a cheap `cb_ptr -> reg_index`
    // index and does NOT eagerly publish the name. Registration semantics
    // are unchanged (`find` works), and `flush_native_ring_names()` then
    // materializes the name on demand so a diagnostic dump resolves it.
    //
    // A dedicated, uniquely-named fn pointer is used so the resolved name
    // is unambiguous in the process-global (first-write-wins) ring name map.
    // Distinct, non-trivial body so the compiler cannot merge this with the
    // module's other `Ok(None)` dummy natives (function merging would alias
    // the `fn` pointer and make the name lookup ambiguous).
    fn ring_flush_probe_native(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(0x52_49_4e_47 ^ args.len() as i32)))
    }

    let mut registry = NativeMethodRegistry::new();
    // Distinctive triple unlikely to be registered elsewhere in the suite.
    registry.register(
        "craton/test/RingFlushProbe",
        "probe",
        "()V",
        ring_flush_probe_native,
        NativeKind::SyntheticStub,
    );
    // Registration semantics preserved: the method is findable regardless
    // of the deferral.
    assert!(registry
        .find("craton/test/RingFlushProbe", "probe", "()V")
        .is_some());

    // Materialize the deferred names, then the ring can resolve the pointer.
    registry.flush_native_ring_names();
    assert_eq!(
        crate::native_ring::name_of(ring_flush_probe_native as usize).as_deref(),
        Some("craton/test/RingFlushProbe.probe()V"),
    );
}

// -----------------------------------------------------------------------
// AnnotationData / AnnotationElementValue
// -----------------------------------------------------------------------

#[test]
fn annotation_data_clone_and_debug() {
    let ann = AnnotationData {
        type_descriptor: "Ljava/lang/Override;".to_string(),
        elements: vec![
            ("value".to_string(), AnnotationElementValue::Int(42)),
            (
                "name".to_string(),
                AnnotationElementValue::StringVal("test".to_string()),
            ),
        ],
    };
    let cloned = ann.clone();
    assert_eq!(cloned.type_descriptor, "Ljava/lang/Override;");
    assert_eq!(cloned.elements.len(), 2);
    // Debug should not panic
    let _ = format!("{:?}", ann);
}

#[test]
fn annotation_element_value_variants() {
    let values: Vec<AnnotationElementValue> = vec![
        AnnotationElementValue::Int(1),
        AnnotationElementValue::Long(2),
        AnnotationElementValue::Float(3.0),
        AnnotationElementValue::Double(4.0),
        AnnotationElementValue::StringVal("s".to_string()),
        AnnotationElementValue::Enum("Lp;".to_string(), "A".to_string()),
        AnnotationElementValue::Class("Lc;".to_string()),
        AnnotationElementValue::Annotation(AnnotationData {
            type_descriptor: "Linner;".to_string(),
            elements: vec![],
        }),
        AnnotationElementValue::Array(vec![AnnotationElementValue::Int(10)]),
    ];
    // All variants should be clonable and debuggable
    for v in &values {
        let _ = v.clone();
        let _ = format!("{:?}", v);
    }
    assert_eq!(values.len(), 9);
}

// -----------------------------------------------------------------------
// StackTraceEntry
// -----------------------------------------------------------------------

#[test]
fn stack_trace_entry_clone_and_debug() {
    let entry = StackTraceEntry {
        class_name: Arc::from("java/lang/Object"),
        method_name: Arc::from("hashCode"),
        method_descriptor: Some(Arc::from("()I")),
        source_file: Some(Arc::from("Object.java")),
        line_number: 42,
        byte_code_index: 17,
        class_id: None,
        method_index: Some(3),
    };
    let cloned = entry.clone();
    assert_eq!(&*cloned.class_name, "java/lang/Object");
    assert_eq!(cloned.line_number, 42);
    assert_eq!(cloned.byte_code_index, 17);
    assert_eq!(cloned.method_index, Some(3));
    let _ = format!("{:?}", entry);
}

#[test]
fn stack_trace_entry_native_method() {
    let entry = StackTraceEntry {
        class_name: Arc::from("java/lang/System"),
        method_name: Arc::from("arraycopy"),
        method_descriptor: Some(Arc::from("(Ljava/lang/Object;ILjava/lang/Object;II)V")),
        source_file: None,
        line_number: -2, // native method
        byte_code_index: -1,
        class_id: None,
        method_index: None,
    };
    assert_eq!(entry.line_number, -2);
    assert!(entry.source_file.is_none());
    assert_eq!(entry.byte_code_index, -1);
    assert!(
        entry.method_index.is_none(),
        "a synthetic/native entry has no backing Class::methods slot"
    );
}

// -----------------------------------------------------------------------
// Native-dispatch memoization: dense handles, digest verification
// -----------------------------------------------------------------------

/// Compare two callbacks by address. `fn`-pointer `==` is unreliable
/// (the compiler may merge or duplicate identical functions, hence the
/// `unpredictable_function_pointer_comparisons` lint), but `dummy_native`
/// and `dummy_native_2` have observably different bodies, so they cannot be
/// merged — and merging identical ones is exactly the equality we want.
fn cb_addr(cb: NativeCallback) -> usize {
    cb as usize
}

#[test]
fn handle_lookup_agrees_with_name_lookup() {
    let mut registry = NativeMethodRegistry::new();
    let triples = [
        ("java/lang/Object", "hashCode", "()I"),
        (
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        ),
        ("java/lang/String", "length", "()I"),
    ];
    registry.register(
        triples[0].0,
        triples[0].1,
        triples[0].2,
        dummy_native,
        NativeKind::SyntheticStub,
    );
    registry.register(
        triples[1].0,
        triples[1].1,
        triples[1].2,
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    registry.register(
        triples[2].0,
        triples[2].1,
        triples[2].2,
        dummy_native,
        NativeKind::SyntheticStub,
    );

    for (class, method, descriptor) in triples {
        let by_name = registry
            .find(class, method, descriptor)
            .expect("registered");
        let id = registry
            .resolve_id(class, method, descriptor)
            .expect("handle resolves");
        let by_handle = registry.callback_of(id).expect("handle redeems");
        assert_eq!(cb_addr(by_name), cb_addr(by_handle), "{class}.{method}");
        assert_eq!(registry.triple_of(id), Some((class, method, descriptor)));
        assert_eq!(
            registry.kind_of_id(id),
            registry.kind_of(class, method, descriptor)
        );
    }

    // Misses agree too.
    assert!(registry.find("java/lang/Object", "nope", "()V").is_none());
    assert!(registry
        .resolve_id("java/lang/Object", "nope", "()V")
        .is_none());
}

#[test]
fn handle_survives_later_registrations() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()V", dummy_native, NativeKind::SyntheticStub);
    let id = registry.resolve_id("a/A", "m", "()V").expect("registered");

    // Registering hundreds of unrelated natives must not move the handle.
    for i in 0..256 {
        let class = format!("filler/C{i}");
        registry.register(
            &class,
            "m",
            "()V",
            dummy_native_2,
            NativeKind::SyntheticStub,
        );
    }
    assert_eq!(registry.resolve_id("a/A", "m", "()V"), Some(id));
    assert_eq!(
        cb_addr(registry.callback_of(id).expect("still redeems")),
        cb_addr(dummy_native as NativeCallback)
    );
    assert_eq!(registry.triple_of(id), Some(("a/A", "m", "()V")));
}

#[test]
fn handle_is_stable_across_reregistration_and_picks_up_new_callback() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()V", dummy_native, NativeKind::SyntheticStub);
    let id = registry.resolve_id("a/A", "m", "()V").expect("registered");
    let generation_before = registry.generation();

    // Re-registering the SAME triple must update the slot in place: the
    // handle stays valid (an already-memoized call site keeps working) and
    // now resolves to the new callback (last-registration-wins).
    registry.register("a/A", "m", "()V", dummy_native_2, NativeKind::SyntheticStub);
    assert_eq!(registry.resolve_id("a/A", "m", "()V"), Some(id));
    assert_eq!(
        registry.generation(),
        generation_before,
        "re-registering an existing triple must not allocate a new slot"
    );
    assert_eq!(
        cb_addr(registry.callback_of(id).expect("redeems")),
        cb_addr(dummy_native_2 as NativeCallback)
    );
    assert_eq!(registry.len(), 1);
}

#[test]
fn generation_advances_only_for_new_triples() {
    let mut registry = NativeMethodRegistry::new();
    // The absolute value is an opaque per-registry token; only the deltas
    // are contractual.
    let empty = registry.generation();
    assert_ne!(empty, 0, "generation 0 is reserved as a cache sentinel");
    registry.register("a/A", "m", "()V", dummy_native, NativeKind::SyntheticStub);
    let after_first = registry.generation();
    assert_ne!(after_first, empty);
    // Re-registering an existing triple must NOT move the generation.
    registry.register("a/A", "m", "()V", dummy_native_2, NativeKind::SyntheticStub);
    assert_eq!(registry.generation(), after_first);
    // A genuinely new triple must.
    registry.register(
        "a/A",
        "other",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert_ne!(registry.generation(), after_first);
}

#[test]
fn distinct_registries_do_not_share_a_generation() {
    // A `static NativeCallSite` in a test binary outlives any single VM.
    // Two registries with the SAME number of registrations must still report
    // different generations, or a memo taken against one would be accepted
    // by the other and redeem a slot index that means something else.
    let mut a = NativeMethodRegistry::new();
    let mut b = NativeMethodRegistry::new();
    assert_ne!(a.generation(), b.generation());
    a.register("a/A", "m", "()V", dummy_native, NativeKind::SyntheticStub);
    b.register("b/B", "m", "()V", dummy_native_2, NativeKind::SyntheticStub);
    assert_eq!(a.len(), b.len());
    assert_ne!(
        a.generation(),
        b.generation(),
        "equal-size registries must not alias"
    );
}

#[test]
fn digest_collision_is_reported_as_a_miss_not_a_wrong_callback() {
    // Regression guard for the `class_manager::name_to_id` defect class: a
    // digest-keyed map with no name verification returns the WRONG entry on
    // a collision (silent type confusion). Inject a collision and prove the
    // full-name check fires.
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "owner/Owner",
        "run",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    let owner_id = registry
        .resolve_id("owner/Owner", "run", "()V")
        .expect("owner registered");

    let victim = ("victim/Victim", "run", "()V");
    assert!(
        registry.find(victim.0, victim.1, victim.2).is_none(),
        "precondition: victim is not registered"
    );
    registry.inject_digest_collision_for_test(("owner/Owner", "run", "()V"), victim);

    // The colliding triple must NOT resolve to the owner's callback.
    assert!(
            registry.find(victim.0, victim.1, victim.2).is_none(),
            "full-name verification did not fire: a digest collision returned another native's callback"
        );
    assert!(registry.resolve_id(victim.0, victim.1, victim.2).is_none());
    assert!(registry.kind_of(victim.0, victim.1, victim.2).is_none());
    assert!(registry
        .find_with_kind(victim.0, victim.1, victim.2)
        .is_none());
    assert!(registry
        .find_by_key(
            NativeMethodKey::new(victim.0, victim.1, victim.2),
            victim.0,
            victim.1,
            victim.2
        )
        .is_none());

    // ...and the legitimate owner is unaffected.
    let owner_cb = registry
        .find("owner/Owner", "run", "()V")
        .expect("owner still resolves");
    assert_eq!(cb_addr(owner_cb), cb_addr(dummy_native as NativeCallback));
    assert_eq!(
        registry.resolve_id("owner/Owner", "run", "()V"),
        Some(owner_id)
    );
}

#[test]
fn precomputed_key_matches_name_lookup() {
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/lang/String",
        "length",
        "()I",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );

    let key = NativeMethodKey::new("java/lang/String", "length", "()I");
    let by_key = registry
        .find_by_key(key, "java/lang/String", "length", "()I")
        .expect("key lookup hits");
    let by_name = registry
        .find("java/lang/String", "length", "()I")
        .expect("name lookup hits");
    assert_eq!(cb_addr(by_key), cb_addr(by_name));
    assert_eq!(
        registry.resolve_id_by_key(key, "java/lang/String", "length", "()I"),
        registry.resolve_id("java/lang/String", "length", "()I")
    );
    assert_eq!(
        registry
            .find_with_kind_by_key(key, "java/lang/String", "length", "()I")
            .map(|(cb, kind)| (cb_addr(cb), kind)),
        registry
            .find_with_kind("java/lang/String", "length", "()I")
            .map(|(cb, kind)| (cb_addr(cb), kind))
    );

    // A key that does not describe the strings simply misses — it can never
    // hand back some other class's native.
    let wrong_key = NativeMethodKey::new("java/lang/Object", "hashCode", "()I");
    assert!(registry
        .find_by_key(wrong_key, "java/lang/String", "length", "()I")
        .is_none());
}

#[test]
fn handles_resolve_through_the_descriptor_quirk_path() {
    // A memoized handle must not silently lose `find`'s compatibility
    // rewrites, or a call site that adopts handles would regress the
    // malformed-descriptor cases the quirk path exists for.
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "q/Q",
        "m",
        "()Ljava/lang/String;",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );

    let quirky = "()Ljava/lang/String"; // missing trailing ';'
    let by_name = registry
        .find("q/Q", "m", quirky)
        .expect("quirk rewrite hits");
    let id = registry
        .resolve_id("q/Q", "m", quirky)
        .expect("quirk rewrite yields a handle");
    assert_eq!(
        cb_addr(by_name),
        cb_addr(registry.callback_of(id).expect("redeems"))
    );
    // The handle names the triple actually registered, not the quirky input.
    assert_eq!(
        registry.triple_of(id),
        Some(("q/Q", "m", "()Ljava/lang/String;"))
    );
}

#[test]
fn kind_travels_with_the_handle() {
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register("k/K", "fast", "()I", dummy_native, NativeKind::Intrinsic);
    };
    {
        let r = &mut registry;
        r.register(
            "k/K",
            "fake",
            "()I",
            dummy_native_2,
            NativeKind::SyntheticStub,
        );
    };
    let fast = registry
        .resolve_id("k/K", "fast", "()I")
        .expect("registered");
    let fake = registry
        .resolve_id("k/K", "fake", "()I")
        .expect("registered");
    assert_eq!(registry.kind_of_id(fast), Some(NativeKind::Intrinsic));
    assert_eq!(registry.kind_of_id(fake), Some(NativeKind::SyntheticStub));
    assert_eq!(
        registry
            .find_with_kind("k/K", "fake", "()I")
            .map(|(_, kind)| kind),
        Some(NativeKind::SyntheticStub)
    );
}

#[test]
fn foreign_handle_does_not_panic() {
    let registry = NativeMethodRegistry::new();
    let bogus = NativeMethodId::from_u32(9_999);
    assert!(registry.callback_of(bogus).is_none());
    assert!(registry.kind_of_id(bogus).is_none());
    assert!(registry.triple_of(bogus).is_none());
}

// -----------------------------------------------------------------------
// Real-JDK wave 2, L10 — real `ThreadPoolExecutor` field initialisation
// -----------------------------------------------------------------------

/// Every `Executors` pool factory, and the descriptor CratonVM registered
/// it under before the L10 drop.
const EXECUTOR_POOL_FACTORIES: &[(&str, &str)] = &[
    (
        "newFixedThreadPool",
        "(I)Ljava/util/concurrent/ExecutorService;",
    ),
    (
        "newCachedThreadPool",
        "()Ljava/util/concurrent/ExecutorService;",
    ),
    (
        "newCachedThreadPool",
        "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/ExecutorService;",
    ),
    (
        "newSingleThreadExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
    ),
    (
        "newScheduledThreadPool",
        "(I)Ljava/util/concurrent/ScheduledExecutorService;",
    ),
    (
        "newSingleThreadScheduledExecutor",
        "()Ljava/util/concurrent/ScheduledExecutorService;",
    ),
];

/// In real-JDK mode no `Executors` pool factory may be registered: the real
/// `java.util.concurrent.Executors` bytecode has to build every executor,
/// so a factory-made pool is genuinely `<init>`-constructed by construction
/// rather than by a fallback that happened not to be taken.
///
/// That is what unblocks L11. The eight `ThreadPoolExecutor.execute`
/// receiver-shape dispatch sites exist to detect an executor CratonVM
/// fabricated; they are deletable only once no such executor can exist, and
/// a native here is the only thing that could make one.
///
/// Asserted in BOTH directions. Without the `drop_real_layout_synthetic ==
/// false` half this test passes on a registry that refuses the triples
/// unconditionally — which would break the synthetic-JDK build, whose
/// two-slot executor model is exactly what those natives are for, while
/// this test went on reading green.
#[test]
fn real_jdk_mode_registers_no_executors_pool_factory() {
    let exec = "java/util/concurrent/Executors";

    let mut compatible = NativeMethodRegistry::new();
    for (name, descriptor) in EXECUTOR_POOL_FACTORIES {
        compatible.register(
            exec,
            name,
            descriptor,
            dummy_native,
            NativeKind::SyntheticStub,
        );
    }
    for (name, descriptor) in EXECUTOR_POOL_FACTORIES {
        assert!(
            compatible.find(exec, name, descriptor).is_some(),
            "synthetic-JDK mode must KEEP Executors.{name}{descriptor}: it is the \
                 only implementation there, and dropping it would leave the synthetic \
                 two-slot executor model with no factory at all"
        );
    }

    let mut real = NativeMethodRegistry::new();
    real.set_drop_real_layout_synthetic(true);
    for (name, descriptor) in EXECUTOR_POOL_FACTORIES {
        real.register(
            exec,
            name,
            descriptor,
            dummy_native,
            NativeKind::SyntheticStub,
        );
    }
    for (name, descriptor) in EXECUTOR_POOL_FACTORIES {
        assert!(
            real.find(exec, name, descriptor).is_none(),
            "real-JDK mode still registers Executors.{name}{descriptor}. A native here \
                 can hand back an executor the real `<init>` never ran on, which is the \
                 receiver shape the eight `ThreadPoolExecutor.execute` dispatch sites exist \
                 to detect — and while one can exist, those sites cannot be deleted (L11)."
        );
    }

    // Negative control: the drop is scoped to the pool factories by name.
    // `callable`/`defaultThreadFactory`/`unconfigurable*` fabricate no
    // executor and must be unaffected, or a future widening of the
    // `matches!` arm would take them out silently.
    for (name, descriptor) in [
        (
            "callable",
            "(Ljava/lang/Runnable;)Ljava/util/concurrent/Callable;",
        ),
        (
            "defaultThreadFactory",
            "()Ljava/util/concurrent/ThreadFactory;",
        ),
        (
            "unconfigurableExecutorService",
            "(Ljava/util/concurrent/ExecutorService;)Ljava/util/concurrent/ExecutorService;",
        ),
    ] {
        real.register(
            exec,
            name,
            descriptor,
            dummy_native,
            NativeKind::SyntheticStub,
        );
        assert!(
            real.find(exec, name, descriptor).is_some(),
            "the L10 drop must not reach Executors.{name}{descriptor} — it builds no \
                 executor of its own"
        );
    }
}

// -----------------------------------------------------------------------
// Real-JDK mode (docs/book/src/user-guide/real-jdk-mode.md §4), 2026-07-31
// -----------------------------------------------------------------------

#[test]
fn allowed_in_rejects_only_synthetic_stubs_under_jdk_only() {
    // §1.3 forbids exactly one kind. Bridges cross VM boundaries and
    // intrinsics are semantics-preserving, so both survive strict mode —
    // pinned here because a well-meaning "strict means fewer natives"
    // tightening of this predicate would silently unboot every real-JDK run.
    for kind in [
        NativeKind::Intrinsic,
        NativeKind::Bridge,
        NativeKind::SyntheticStub,
    ] {
        assert!(
            kind.allowed_in(CompatibilityMode::Compatible),
            "{kind:?} must be allowed in Compatible"
        );
    }
    assert!(NativeKind::Intrinsic.allowed_in(CompatibilityMode::JdkOnly));
    assert!(NativeKind::Bridge.allowed_in(CompatibilityMode::JdkOnly));
    assert!(!NativeKind::SyntheticStub.allowed_in(CompatibilityMode::JdkOnly));
}

#[test]
fn jdk_only_refuses_synthetic_stub_registration_with_provenance() {
    let mut registry = NativeMethodRegistry::new();
    registry.set_compatibility_mode(CompatibilityMode::JdkOnly);
    assert_eq!(registry.compatibility_mode(), CompatibilityMode::JdkOnly);
    {
        let r = &mut registry;
        r.register(
            "j/J",
            "fake",
            "()I",
            dummy_native,
            NativeKind::SyntheticStub,
        );
    };

    // Refused means NOT INSERTED anywhere: no callback, no census row, and
    // `generation()` must not move (a bumped generation would invalidate
    // every live `NativeCallSite` memo for a registration that never
    // happened).
    assert!(registry.find("j/J", "fake", "()I").is_none());
    assert!(registry.census().is_empty());
    assert_eq!(registry.len(), 0);

    match registry.refused_registrations() {
        [JdkOnlyViolation::SyntheticNativeRegistered {
            class,
            method,
            descriptor,
            registered_by,
            survivor,
        }] => {
            assert!(
                survivor.is_none(),
                "nothing was registered for this triple before the refusal, \
                     so the refusal really did retire it: {survivor:?}"
            );
            assert_eq!(class, "j/J");
            assert_eq!(method, "fake");
            assert_eq!(descriptor, "()I");
            // `#[track_caller]` must name the `register(...)` call site in
            // THIS file, not `register`'s own body in registry.rs.
            let site = registered_by.as_deref().expect("provenance recorded");
            assert!(
                site.contains(concat!(file!(), ":")),
                "expected a file:line site, got {site}"
            );
        }
        other => panic!("expected exactly one refusal, got {other:?}"),
    }
}

#[test]
fn jdk_only_keeps_bridges_and_intrinsics_and_compatible_keeps_stubs() {
    let mut strict = NativeMethodRegistry::new();
    strict.set_compatibility_mode(CompatibilityMode::JdkOnly);
    {
        let r = &mut strict;
        r.register("j/J", "syscall", "()I", dummy_native, NativeKind::Bridge);
    };
    {
        let r = &mut strict;
        r.register("j/J", "fast", "()I", dummy_native_2, NativeKind::Intrinsic);
    };
    assert!(strict.find("j/J", "syscall", "()I").is_some());
    assert!(strict.find("j/J", "fast", "()I").is_some());
    assert!(strict.refused_registrations().is_empty());

    // Compatible is the default and must be unchanged (§1: "--real-jdk
    // (default) keeps today's behaviour exactly"), including the ambient
    // `SyntheticStub` category that `register()` inherits when nobody sets
    // one.
    let mut compatible = NativeMethodRegistry::new();
    assert_eq!(
        compatible.compatibility_mode(),
        CompatibilityMode::Compatible
    );
    compatible.register(
        "j/J",
        "fake",
        "()I",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(compatible.find("j/J", "fake", "()I").is_some());
    assert_eq!(
        compatible.kind_of("j/J", "fake", "()I"),
        Some(NativeKind::SyntheticStub)
    );
    assert!(compatible.refused_registrations().is_empty());
}

#[test]
fn record_invocation_is_per_slot_and_ignores_foreign_ids() {
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register("c/C", "a", "()I", dummy_native, NativeKind::Bridge);
        r.register("c/C", "b", "()I", dummy_native_2, NativeKind::Bridge);
    };
    let a = registry.resolve_id("c/C", "a", "()I").expect("registered");
    let b = registry.resolve_id("c/C", "b", "()I").expect("registered");

    assert_eq!(registry.invocations_of_id(a), Some(0));
    registry.record_invocation(a);
    registry.record_invocation(a);
    registry.record_invocation(b);
    assert_eq!(registry.invocations_of_id(a), Some(2));
    assert_eq!(registry.invocations_of_id(b), Some(1));

    // A handle from another registry (or a stale memo) must be a silent
    // no-op, never a panic — this runs on the native-dispatch path.
    let foreign = NativeMethodId::from_u32(9_999);
    registry.record_invocation(foreign);
    assert!(registry.invocations_of_id(foreign).is_none());
    assert_eq!(registry.invocations_of_kind(NativeKind::Bridge), 3);
}

/// gci i20/jt: the sharded counter that replaces the VM's per-call JNI
/// census `AtomicU64` sums every thread's adds exactly, and each shard
/// has a line pair of its own.
#[test]
fn gci_i20jt_a_sharded_counter_sums_every_threads_adds_exactly() {
    use std::sync::atomic::Ordering;
    let counter = std::sync::Arc::new(ShardedCounter::new());
    assert_eq!(counter.load(Ordering::Relaxed), 0);
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let counter = std::sync::Arc::clone(&counter);
            std::thread::spawn(move || {
                for _ in 0..1000 {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("a counting thread does not panic");
    }
    counter.fetch_add(5, Ordering::Relaxed);
    assert_eq!(counter.load(Ordering::Relaxed), 8005);
    assert_eq!(std::mem::align_of::<CounterShard>(), 128);
}

/// Round 11 wave 12 (lane rt): the counters are sharded by thread, and the
/// readers sum the shards. The totals must stay exact however the threads
/// land on the shards, and in every reader (`invocations_of_id`,
/// `invocations_of_kind`, `census`).
#[test]
fn sharded_invocation_counts_sum_exactly_across_threads() {
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register("s/S", "hot", "()I", dummy_native, NativeKind::Bridge);
    };
    let hot = registry
        .resolve_id("s/S", "hot", "()I")
        .expect("registered");
    let per_thread = 5_000u64;
    let threads = 12u64;
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let registry = &registry;
            scope.spawn(move || {
                assert!(invocation_shard() < INVOCATION_SHARDS);
                for _ in 0..per_thread {
                    registry.record_invocation(hot);
                }
            });
        }
    });
    registry.record_invocation(hot);
    let expected = threads * per_thread + 1;
    assert_eq!(registry.invocations_of_id(hot), Some(expected));
    assert_eq!(registry.invocations_of_kind(NativeKind::Bridge), expected);
    let census = registry.census();
    let row = census
        .iter()
        .find(|e| e.name == "hot")
        .expect("a census row for the hot slot");
    assert_eq!(row.invocations, expected);
}

#[test]
fn a_thread_keeps_one_invocation_shard() {
    let first = invocation_shard();
    assert!(first < INVOCATION_SHARDS);
    for _ in 0..16 {
        assert_eq!(invocation_shard(), first);
    }
}

#[test]
fn invocations_are_complete_until_a_bypassing_path_says_otherwise() {
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register("c/C", "counted", "()I", dummy_native, NativeKind::Bridge);
        r.register("c/C", "bypassed", "()I", dummy_native_2, NativeKind::Bridge);
    };
    let counted = registry
        .resolve_id("c/C", "counted", "()I")
        .expect("registered");
    let bypassed = registry
        .resolve_id("c/C", "bypassed", "()I")
        .expect("registered");

    // The default is the claim "nothing bypasses this slot", so a fresh
    // registry reports every slot complete and none incomplete. A default
    // of `false` would be honest about the codebase but would make the
    // column useless — every row would carry the same doubt.
    assert_eq!(registry.invocations_complete(counted), Some(true));
    assert_eq!(registry.invocations_complete(bypassed), Some(true));
    assert_eq!(registry.slots_with_incomplete_invocations(), 0);

    registry.record_invocation(bypassed);
    registry.mark_invocations_incomplete(bypassed);

    // The count is still readable and still exact as a FLOOR — marking a
    // slot incomplete must not zero or otherwise disturb the tally, which
    // is the number a reader falls back on.
    assert_eq!(registry.invocations_of_id(bypassed), Some(1));
    assert_eq!(registry.invocations_complete(bypassed), Some(false));
    assert_eq!(
        registry.invocations_complete(counted),
        Some(true),
        "the flag is per slot, not global"
    );
    assert_eq!(registry.slots_with_incomplete_invocations(), 1);

    // Idempotent: a bind site that runs twice (recompilation, a second
    // call site on the same triple) must not be able to change the answer.
    registry.mark_invocations_incomplete(bypassed);
    assert_eq!(registry.slots_with_incomplete_invocations(), 1);

    // Same foreign-handle contract as `record_invocation`: silently
    // ignored, never a panic, and never a fabricated `Some`.
    let foreign = NativeMethodId::from_u32(9_999);
    registry.mark_invocations_incomplete(foreign);
    assert_eq!(registry.invocations_complete(foreign), None);
    assert_eq!(registry.slots_with_incomplete_invocations(), 1);
}

#[test]
fn the_incomplete_flag_survives_re_registration_and_reaches_the_census() {
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register("s/S", "m", "()I", dummy_native, NativeKind::SyntheticStub);
    };
    let id = registry.resolve_id("s/S", "m", "()I").expect("registered");
    registry.record_invocation(id);
    registry.mark_invocations_incomplete(id);

    // Re-registering the triple updates the slot in place. A call site
    // already bound to the previous callback keeps calling it, so the
    // count keeps being short: clearing the flag here would manufacture a
    // claim of completeness, the one direction this instrument must not
    // err in.
    {
        let r = &mut registry;
        r.register("s/S", "m", "()I", dummy_native_2, NativeKind::Bridge);
    };
    assert_eq!(
        registry.invocations_complete(id),
        Some(false),
        "re-registration must not clear the bypass claim"
    );

    let census = registry.census();
    assert_eq!(census.len(), 2, "one row per registration");
    // The superseded row owns no slot, so its zero is exact and it must
    // NOT inherit the doubt: a floor of zero on a row that can never be
    // dispatched is a total.
    assert!(!census[0].owns_slot);
    assert_eq!(census[0].invocations, 0);
    assert!(
        census[0].invocations_complete,
        "a row that owns no slot reports an exact zero"
    );
    // The surviving row carries both the count and the doubt.
    assert!(census[1].owns_slot);
    assert_eq!(census[1].invocations, 1);
    assert!(
        !census[1].invocations_complete,
        "the slot owner must carry the bypass claim into the census"
    );
}

#[test]
fn invocations_of_kind_follows_the_slots_current_kind() {
    // Documents the deliberate consequence of DERIVING per-kind totals by
    // scanning instead of keeping three global counters (which would add a
    // second contended RMW to every native dispatch): a slot re-registered
    // under a different kind carries its accumulated count over to the new
    // kind. Asserted, not merely commented, so a future switch to
    // maintained counters has to confront the behaviour change.
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register("p/P", "m", "()I", dummy_native, NativeKind::SyntheticStub);
    };
    let id = registry.resolve_id("p/P", "m", "()I").expect("registered");
    registry.record_invocation(id);
    registry.record_invocation(id);
    assert_eq!(registry.invocations_of_kind(NativeKind::SyntheticStub), 2);
    assert_eq!(registry.invocations_of_kind(NativeKind::Bridge), 0);

    // Promote the same triple to a bridge: same slot, same counter.
    {
        let r = &mut registry;
        r.register("p/P", "m", "()I", dummy_native_2, NativeKind::Bridge);
    };
    assert_eq!(
        registry.resolve_id("p/P", "m", "()I"),
        Some(id),
        "re-registration must update the slot in place"
    );
    assert_eq!(registry.invocations_of_kind(NativeKind::SyntheticStub), 0);
    assert_eq!(registry.invocations_of_kind(NativeKind::Bridge), 2);
}

#[test]
fn a_stub_total_of_zero_is_not_a_pass_when_a_stub_slot_is_declared_incomplete() {
    // The L4 gate is `invocations_of_kind(SyntheticStub) == 0`. This is
    // the state in which that assertion PASSES while proving nothing —
    // the quiet failure G33-1 §4 flagged and this test makes loud.
    //
    // It is not hypothetical. MEASURED on `9ae371468` in default mode:
    // the four `java/lang/foreign/DowncallHandle` arms are registered
    // `synthetic-stub` AND listed in `UNCOUNTED_STACKLESS_NATIVES`, the
    // stackless-invoke bypass family, which returns `Handled` before the
    // census increment. G33-1's "neither bypass family currently serves a
    // SyntheticStub" was true when it was written and is not true now.
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register(
            "f/Downcall",
            "invoke",
            "()I",
            dummy_native,
            NativeKind::SyntheticStub,
        );
    };
    let stub = registry
        .resolve_id("f/Downcall", "invoke", "()I")
        .expect("registered");
    registry.mark_invocations_incomplete(stub);

    // The bare total: zero. The old gate reads this and says PASS.
    assert_eq!(registry.invocations_of_kind(NativeKind::SyntheticStub), 0);

    // The honest form refuses, and says which of the two failure modes
    // it hit rather than leaving the reader hunting for a stub.
    let checked = registry.invocations_of_kind_checked(NativeKind::SyntheticStub);
    assert_eq!(checked.total, 0);
    assert_eq!(checked.incomplete_slots, 1);
    assert!(!checked.is_measurable());
    assert!(!checked.is_conclusive_zero());
    assert!(
        !checked.is_clean(),
        "a gate must not pass on total == 0 alone"
    );
    assert!(
        checked.describe().contains("proves nothing"),
        "the failure message must name the blindness, not just the count: {}",
        checked.describe()
    );
}

#[test]
fn a_stub_total_of_zero_with_every_stub_slot_counted_is_a_pass() {
    // The other side of the same predicate, so `is_clean` cannot be
    // satisfied by simply never returning true. Marking a slot of a
    // DIFFERENT kind incomplete — which is the true state of every
    // `--real-jdk` run today, where the JIT and interpreter bypass lists
    // resolve only `Bridge` and `Intrinsic` triples — must not make the
    // stub verdict unmeasurable.
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register("s/S", "m", "()I", dummy_native, NativeKind::SyntheticStub);
    };
    {
        let r = &mut registry;
        r.register("b/B", "m", "()I", dummy_native_2, NativeKind::Bridge);
    };
    let bridge = registry.resolve_id("b/B", "m", "()I").expect("registered");
    registry.mark_invocations_incomplete(bridge);
    registry.record_invocation(bridge);

    let stubs = registry.invocations_of_kind_checked(NativeKind::SyntheticStub);
    assert!(
        stubs.is_clean(),
        "no stub ran and every stub slot is counted"
    );
    assert_eq!(
        registry.incomplete_slots_of_kind(NativeKind::SyntheticStub),
        0
    );

    // The bridge's own total is a floor, and says so.
    let bridges = registry.invocations_of_kind_checked(NativeKind::Bridge);
    assert_eq!(bridges.total, 1);
    assert!(!bridges.is_measurable());
    assert!(bridges.describe().contains("at least 1"));
}

#[test]
fn incomplete_slots_of_kind_follows_the_slots_current_kind() {
    // The same re-registration consequence
    // `invocations_of_kind_follows_the_slots_current_kind` pins for the
    // counter, pinned for the flag — because the two are read together
    // and a divergence between them would be invisible.
    //
    // This is also the one direction that matters for the gate: promoting
    // a bypassed stub to `Bridge` moves BOTH the count and the blindness
    // off the stub verdict, which is correct (the stub was fixed). The
    // reverse — demoting a marked `Bridge` to `SyntheticStub` — makes the
    // stub verdict unmeasurable, which is exactly when a gate must go red.
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register("p/P", "m", "()I", dummy_native, NativeKind::Bridge);
    };
    let id = registry.resolve_id("p/P", "m", "()I").expect("registered");
    registry.mark_invocations_incomplete(id);
    assert_eq!(registry.incomplete_slots_of_kind(NativeKind::Bridge), 1);
    assert_eq!(
        registry.incomplete_slots_of_kind(NativeKind::SyntheticStub),
        0
    );
    assert!(registry
        .invocations_of_kind_checked(NativeKind::SyntheticStub)
        .is_clean());

    // Demote the same triple to a stub. The bit is sticky by design, so
    // the blindness travels with the slot.
    {
        let r = &mut registry;
        r.register("p/P", "m", "()I", dummy_native_2, NativeKind::SyntheticStub);
    };
    assert_eq!(
        registry.resolve_id("p/P", "m", "()I"),
        Some(id),
        "re-registration must update the slot in place"
    );
    assert_eq!(registry.incomplete_slots_of_kind(NativeKind::Bridge), 0);
    assert_eq!(
        registry.incomplete_slots_of_kind(NativeKind::SyntheticStub),
        1
    );
    assert!(
        !registry
            .invocations_of_kind_checked(NativeKind::SyntheticStub)
            .is_clean(),
        "a bypassed slot that becomes a stub must turn the gate red, not quiet"
    );
}

#[test]
fn the_bais_hook_is_absent_until_installed() {
    // The BAIS observer is a process-wide `OnceLock` and this crate never
    // installs one, so `native-io`'s dispatch sites are inert here — the
    // property that makes adding them a no-op for every consumer until
    // the nominated `http_url_connection.rs` hook lands. Asserted through
    // the public surface rather than by reading the static, so it stays
    // true if the storage changes.
    //
    // Deliberately not a behavioural test of a hook: installing one would
    // be irreversible for every other test in this binary. The
    // behavioural coverage lives in `native-io`, thread-armed for exactly
    // that reason.
    assert!(
        BAIS_EVENT_HOOK.get().is_none(),
        "no hook may be installed from within native-api's own tests"
    );
}

#[test]
fn census_reports_provenance_and_zeroes_superseded_rows() {
    let mut registry = NativeMethodRegistry::new();
    {
        let r = &mut registry;
        r.register("s/S", "m", "()I", dummy_native, NativeKind::SyntheticStub);
    };
    {
        let r = &mut registry;
        r.register("s/S", "m", "()I", dummy_native_2, NativeKind::Bridge);
    };
    let id = registry.resolve_id("s/S", "m", "()I").expect("registered");
    registry.record_invocation(id);
    registry.record_invocation(id);
    registry.record_invocation(id);

    let census = registry.census();
    assert_eq!(census.len(), 2, "one row per registration, not per slot");

    // Row 0: the superseded stub. It knows it was first (`overwrote: None`)
    // and reports zero invocations — the count belongs to whoever owns the
    // slot now, and double-attributing it would inflate the CI gate's
    // synthetic-stub total.
    assert_eq!(census[0].class, "s/S");
    assert_eq!(census[0].name, "m");
    assert_eq!(census[0].descriptor, "()I");
    assert_eq!(census[0].kind, NativeKind::SyntheticStub);
    assert_eq!(census[0].overwrote, None);
    assert_eq!(census[0].invocations, 0);
    // ... and it owns no slot, which is the fact that makes the zero above
    // mean "cannot be dispatched" rather than "was not dispatched yet".
    // Adjudicating this row's kind decides nothing.
    assert!(
        !census[0].owns_slot,
        "a superseded registration must not claim to own its slot"
    );

    // Row 1: the current owner. `overwrote` is the history that was
    // previously unrecoverable — it is read before the in-place slot update.
    assert_eq!(census[1].kind, NativeKind::Bridge);
    assert_eq!(census[1].overwrote, Some(NativeKind::SyntheticStub));
    assert_eq!(census[1].invocations, 3);
    assert!(
        census[1].owns_slot,
        "the surviving registration must own its slot"
    );

    for row in &census {
        let site = row.registered_by.as_deref().expect("provenance recorded");
        assert!(
            site.contains("registry/tests.rs:"),
            "expected a file:line site, got {site}"
        );
    }
}

// -----------------------------------------------------------------------
// Capability gate (registration + dispatch chokepoints)
// -----------------------------------------------------------------------

use crate::capability::{Capability, CapabilityKind, CapabilityMode, CapabilitySet, Scope, VmId};

#[test]
fn no_capability_policy_means_no_gate_at_all() {
    // The default must be byte-for-byte today's behaviour: registration
    // accepted, no classification recorded, dispatch check trivially Ok.
    let mut registry = NativeMethodRegistry::new();
    registry.register(
        "java/lang/ProcessBuilder",
        "start",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    let id = registry
        .resolve_id("java/lang/ProcessBuilder", "start", "()V")
        .expect("registered");
    assert!(registry.capabilities().is_none());
    assert_eq!(registry.capability_of_id(id), None);
    assert!(registry.check_dispatch_capability(id).is_ok());
}

#[test]
fn permissive_policy_classifies_but_never_refuses() {
    let mut registry = NativeMethodRegistry::new();
    let caps = std::sync::Arc::new(CapabilitySet::new(
        VmId::from_raw(0xCA9A_0001),
        CapabilityMode::Permissive,
    ));
    registry.set_capabilities(std::sync::Arc::clone(&caps));

    registry.register(
        "java/lang/ProcessBuilder",
        "start",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/util/HashMap",
        "put",
        "()V",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );

    let spawn = registry
        .resolve_id("java/lang/ProcessBuilder", "start", "()V")
        .expect("permissive must accept the registration");
    let benign = registry
        .resolve_id("java/util/HashMap", "put", "()V")
        .expect("registered");

    assert_eq!(
        registry.capability_of_id(spawn),
        Some(CapabilityKind::ProcessSpawn)
    );
    assert_eq!(registry.capability_of_id(benign), None);
    assert!(registry.check_dispatch_capability(spawn).is_ok());
    assert!(registry.check_dispatch_capability(benign).is_ok());

    // Both the registration and the dispatch are in the audit log, so a
    // deployment can see what it would have to grant.
    let report = caps.audit_report();
    assert!(
        report.uses.iter().any(|u| {
            u.capability.kind() == CapabilityKind::NativeRegister
                && u.capability.scope().to_string() == "java/lang/ProcessBuilder.start"
        }),
        "registration must be recorded:\n{report}"
    );
    assert!(
        report
            .uses
            .iter()
            .any(|u| u.capability.kind() == CapabilityKind::ProcessSpawn),
        "dispatch must be recorded:\n{report}"
    );
}

#[test]
fn enforce_refuses_ungranted_registration_without_inserting() {
    let mut registry = NativeMethodRegistry::new();
    let mut set = CapabilitySet::new(VmId::from_raw(0xCA9A_0002), CapabilityMode::Enforce);
    // Grant registration of the HashMap native only.
    set.grant(Capability::NativeRegister(Scope::name(
        "java/util/HashMap.*",
    )));
    registry.set_capabilities(std::sync::Arc::new(set));

    let generation_before = registry.generation();
    registry.register(
        "java/lang/ProcessBuilder",
        "start",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    assert!(
        registry
            .find("java/lang/ProcessBuilder", "start", "()V")
            .is_none(),
        "a refused registration must not be reachable"
    );
    assert_eq!(
        registry.generation(),
        generation_before,
        "a refused registration must not move the registry generation"
    );
    assert!(
        registry.dump_registrations().is_empty(),
        "a refused registration must not appear in the census"
    );

    // The granted one still lands.
    registry.register(
        "java/util/HashMap",
        "put",
        "()V",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    assert!(registry.find("java/util/HashMap", "put", "()V").is_some());
}

#[test]
fn enforce_gates_dispatch_of_a_classified_native() {
    let mut registry = NativeMethodRegistry::new();
    let mut set = CapabilitySet::new(VmId::from_raw(0xCA9A_0003), CapabilityMode::Enforce);
    // Registration of everything is allowed; the *use* of a spawn native
    // is not. This is the layering the design intends: install natives
    // freely, gate what they do.
    set.grant(Capability::NativeRegister(Scope::Any));
    set.grant(Capability::LibraryLoad(Scope::Any));
    registry.set_capabilities(std::sync::Arc::new(set));

    registry.register(
        "java/lang/ProcessBuilder",
        "start",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    registry.register(
        "java/lang/System",
        "loadLibrary",
        "()V",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );

    let spawn = registry
        .resolve_id("java/lang/ProcessBuilder", "start", "()V")
        .expect("registered");
    let load = registry
        .resolve_id("java/lang/System", "loadLibrary", "()V")
        .expect("registered");

    let denied = registry
        .check_dispatch_capability(spawn)
        .expect_err("process-spawn is not granted");
    assert_eq!(denied.capability, CapabilityKind::ProcessSpawn);
    assert_eq!(denied.vm, VmId::from_raw(0xCA9A_0003));
    // `library-load:*` IS granted, so that native dispatches.
    assert!(registry.check_dispatch_capability(load).is_ok());
}

#[test]
fn reregistration_updates_the_slots_capability_classification() {
    // Re-registration rewrites the slot in place (that is what keeps an
    // already-issued NativeMethodId valid), so the classification must
    // track the triple that currently owns the slot — including dropping
    // it when the new owner is not capability-relevant.
    let mut registry = NativeMethodRegistry::new();
    let caps = std::sync::Arc::new(CapabilitySet::new(
        VmId::from_raw(0xCA9A_0004),
        CapabilityMode::Permissive,
    ));
    registry.set_capabilities(caps);

    registry.register(
        "java/lang/Runtime",
        "loadLibrary0",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    let id = registry
        .resolve_id("java/lang/Runtime", "loadLibrary0", "()V")
        .expect("registered");
    assert_eq!(
        registry.capability_of_id(id),
        Some(CapabilityKind::LibraryLoad)
    );

    // Same triple, new callback: the slot is reused and stays classified.
    registry.register(
        "java/lang/Runtime",
        "loadLibrary0",
        "()V",
        dummy_native_2,
        NativeKind::SyntheticStub,
    );
    assert_eq!(
        registry.resolve_id("java/lang/Runtime", "loadLibrary0", "()V"),
        Some(id),
        "re-registration must reuse the slot"
    );
    assert_eq!(
        registry.capability_of_id(id),
        Some(CapabilityKind::LibraryLoad)
    );
}

#[test]
fn two_registries_do_not_share_a_capability_policy() {
    // The whole point of putting the policy in a field: two VMs in one
    // process must not see each other's decisions.
    let mut strict = NativeMethodRegistry::new();
    let mut lax = NativeMethodRegistry::new();
    strict.set_capabilities(std::sync::Arc::new(CapabilitySet::new(
        VmId::from_raw(0xCA9A_0005),
        CapabilityMode::Enforce,
    )));
    lax.set_capabilities(std::sync::Arc::new(CapabilitySet::new(
        VmId::from_raw(0xCA9A_0006),
        CapabilityMode::Permissive,
    )));

    strict.register(
        "java/lang/ProcessBuilder",
        "start",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );
    lax.register(
        "java/lang/ProcessBuilder",
        "start",
        "()V",
        dummy_native,
        NativeKind::SyntheticStub,
    );

    assert!(
        strict
            .find("java/lang/ProcessBuilder", "start", "()V")
            .is_none(),
        "the enforcing registry refuses"
    );
    assert!(
        lax.find("java/lang/ProcessBuilder", "start", "()V")
            .is_some(),
        "the permissive registry is unaffected by the other VM's policy"
    );
}

// ----- G56-1: the slot-indexed raw accessors ---------------------------
//
// `MockNativeContext` overrides neither `get_field_typed` nor
// `get_field_raw`/`set_field_raw`, so every test below exercises the trait
// DEFAULTS — which is the code this lane added and the code every mock in
// the tree will inherit.

fn fresh_object(ctx: &mut MockNativeContext) -> ObjectRef {
    match ctx.new_object("java/lang/Object") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("mock new_object must hand back an object, got {other:?}"),
    }
}

/// The sentinel has to be a byte no class file can produce.
///
/// JVMS §4.3.2: a field descriptor begins with one of `B C D F I J S Z L
/// [`. `RAW_SLOT_DESCRIPTOR` must be outside that set, or `get_field_raw`
/// would decode the slot as whichever type it collided with — silently, and
/// only for slots whose real descriptor differs.
#[test]
fn the_raw_slot_descriptor_is_not_a_jvm_field_descriptor() {
    for d in b"BCDFIJSZL[" {
        assert_ne!(
            RAW_SLOT_DESCRIPTOR, *d,
            "the raw sentinel collided with the real descriptor '{}'",
            *d as char
        );
    }
}

/// A raw read hands back the tag that is stored, not a decoded one.
///
/// The distinction this accessor exists for is `Int(0)` vs `Object(None)`
/// at a slot the class declares `L`: the first is a never-written slot
/// (`gen_heap::read_slot`'s R-niche rule), the second is an explicit null.
/// Every other slot-indexed reader on the trait collapses them.
#[test]
fn get_field_raw_hands_back_the_stored_tag_verbatim() {
    let mut ctx = MockNativeContext::new();
    let obj = fresh_object(&mut ctx);

    for (slot, stored) in [
        Value::Int(0),
        Value::Int(-7),
        Value::Long(1 << 40),
        Value::Float(0.5),
        Value::Double(-2.5),
        Value::Object(None),
        Value::Uninitialized,
    ]
    .into_iter()
    .enumerate()
    {
        ctx.set_field(obj, slot, stored);
        assert_eq!(
            ctx.get_field_raw(obj, slot),
            stored,
            "slot {slot} must read back byte-identically"
        );
    }

    // The falsifier: a reader that always answered null, or always
    // answered the descriptor default, would pass every assertion above
    // for exactly one of these two and fail for the other.
    ctx.set_field(obj, 20, Value::Int(0));
    ctx.set_field(obj, 21, Value::Object(None));
    assert_ne!(
        ctx.get_field_raw(obj, 20),
        ctx.get_field_raw(obj, 21),
        "a raw read must keep `Int(0)` and `Object(None)` distinguishable -- \
             that is the entire capability being added"
    );
}

/// A live reference survives the round trip.
///
/// This is the case that makes a verbatim `Object.clone()` possible and the
/// one where the coercing pair can do real damage: an `Object(Some(_))`
/// sitting in a slot the class declares primitive is truncated to
/// `Int(ptr as i32)` by `coerce_field_value_for_slot`, a stale address no
/// collector will remap (G52-1 §1.6).
#[test]
fn the_raw_pair_round_trips_a_live_reference() {
    let mut ctx = MockNativeContext::new();
    let src = fresh_object(&mut ctx);
    let dst = fresh_object(&mut ctx);
    let payload = fresh_object(&mut ctx);

    ctx.set_field(src, 3, Value::Object(Some(payload)));
    let copied = ctx.get_field_raw(src, 3);
    ctx.set_field_raw(dst, 3, copied);

    assert_eq!(
        ctx.get_field_raw(dst, 3),
        Value::Object(Some(payload)),
        "the copy must hold the SAME reference, not a truncated address"
    );
}

/// The store half is a real store, and it is the one a copy loop uses.
///
/// The default delegates to `set_field`; this pins that the delegation
/// exists and lands in the same slot the raw reader reads. It does NOT
/// claim the production `NativeContextImpl` stores raw — see the method's
/// own doc comment, which says the opposite in as many words.
#[test]
fn set_field_raw_writes_the_slot_the_raw_reader_reads() {
    let mut ctx = MockNativeContext::new();
    let obj = fresh_object(&mut ctx);

    ctx.set_field_raw(obj, 5, Value::Long(0x5EED));
    assert_eq!(ctx.get_field_raw(obj, 5), Value::Long(0x5EED));
    assert_eq!(
        ctx.get_field(obj, 5),
        Value::Long(0x5EED),
        "the raw and ordinary readers must agree about WHICH slot was written"
    );
}

/// A two-line verbatim copy, which is the whole point of the pair.
///
/// `Object.clone()` is specified as a verbatim field copy. Before these
/// accessors existed there was no slot-indexed way to express one from a
/// native: `get_field`/`set_field` resolve and coerce, and the only raw
/// pair was name-keyed (G52-1 §1.4). This is the loop the follow-up in
/// `native-builtins/src/lib.rs` is expected to become.
#[test]
fn the_raw_pair_expresses_a_verbatim_field_copy() {
    let mut ctx = MockNativeContext::new();
    let src = fresh_object(&mut ctx);
    let dst = fresh_object(&mut ctx);

    // A receiver whose slots hold three tags the descriptor pair would
    // rewrite: a never-written reference (raw zero), an explicit null, and
    // a long.
    let planted = [Value::Int(0), Value::Object(None), Value::Long(-1)];
    for (i, v) in planted.iter().enumerate() {
        ctx.set_field(src, i, *v);
    }

    for i in 0..planted.len() {
        let v = ctx.get_field_raw(src, i);
        ctx.set_field_raw(dst, i, v);
    }

    for (i, v) in planted.iter().enumerate() {
        assert_eq!(
            ctx.get_field_raw(dst, i),
            *v,
            "slot {i} of the copy diverged from the original"
        );
    }
}
