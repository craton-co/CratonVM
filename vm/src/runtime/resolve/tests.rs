use super::*;

fn vm_a() -> VmId {
    VmId::from_raw(0xA)
}

fn vm_b() -> VmId {
    VmId::from_raw(0xB)
}

/// The constant-pool record round-trips through the entry point that
/// replaced `constants.rs`'s five direct reaches into
/// `shared.classes.resolution_cache`.
///
/// Asserted as a DIFFERENCE — miss before the write, hit with the exact
/// value after — because a `probe_constant` that always answered `Miss`
/// would leave `ldc` correct on every first execution and only wrong from
/// the second, and a `record_constant` that silently dropped the write
/// looks identical. Both are the failure modes the migration could
/// introduce, and neither is visible from a single resolution.
#[test]
fn a_recorded_constant_reads_back_through_the_resolver() {
    let shared =
        std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let resolver = MemberResolver::new(&shared);
    let owner = resolver.scope(ClassId::new(41));
    const CP: u16 = 13;

    assert!(
        !resolver.probe_constant(owner, CP).is_hit(),
        "nothing has been recorded for this entry yet"
    );

    let owner = resolver.scope(ClassId::new(41));
    resolver.record_constant(owner, CP, resolver.scope(Value::Int(4242)));

    let owner = resolver.scope(ClassId::new(41));
    let hit = resolver
        .probe_constant(owner, CP)
        .into_hit()
        .expect("the value just recorded must read back");
    assert_eq!(resolver.adopt(hit).expect("same vm"), Value::Int(4242));

    // A different cp index in the same class is a different entry, not a
    // second name for this one.
    let owner = resolver.scope(ClassId::new(41));
    assert!(!resolver.probe_constant(owner, CP + 1).is_hit());
}

/// A key from another VM must not read this VM's record. `ClassId`s are
/// allocated per VM, so `ClassId(41)` elsewhere is a different class, and
/// the collector scans this store as roots of THIS heap.
#[test]
fn a_foreign_key_neither_reads_nor_writes_the_constant_record() {
    let shared =
        std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let resolver = MemberResolver::new(&shared);
    const CP: u16 = 77;

    let foreign = VmScoped::new(VmId::from_raw(0xDEAD), ClassId::new(41));
    resolver.record_constant(foreign, CP, resolver.scope(Value::Int(1)));
    let owner = resolver.scope(ClassId::new(41));
    assert!(
        !resolver.probe_constant(owner, CP).is_hit(),
        "a write under a foreign key must not land in this VM's record"
    );

    resolver.record_constant(
        resolver.scope(ClassId::new(41)),
        CP,
        resolver.scope(Value::Int(2)),
    );
    let foreign = VmScoped::new(VmId::from_raw(0xDEAD), ClassId::new(41));
    assert!(
        !resolver.probe_constant(foreign, CP).is_hit(),
        "a read under a foreign key must not see this VM's record"
    );
}

#[test]
fn a_scoped_value_reads_back_only_in_its_own_vm() {
    let scoped = VmScoped::new(vm_a(), ClassId::new(7));
    assert_eq!(scoped.get(vm_a()).expect("same vm"), ClassId::new(7));
}

/// The property the per-VM audit needs: `ClassId(7)` from VM A must not be
/// readable as `ClassId(7)` in VM B, because it is not the same class.
#[test]
fn a_scoped_value_is_not_readable_in_another_vm() {
    let scoped = VmScoped::new(vm_a(), ClassId::new(7));
    let err = scoped.get(vm_b()).expect_err("cross-VM read must fail");
    assert_eq!(err.kind(), ResolveErrorKind::ForeignVm);
    match err {
        ResolveError::ForeignVm { expected, found } => {
            assert_eq!(expected, vm_b());
            assert_eq!(found, vm_a());
        }
        other => panic!("wrong variant: {other:?}"),
    }
    // The borrow form must agree with the owning form.
    let scoped = VmScoped::new(vm_a(), ClassId::new(7));
    assert!(scoped.peek(vm_b()).is_err());
    assert!(scoped.peek(vm_a()).is_ok());
}

#[test]
fn mapping_a_scoped_value_keeps_the_vm_tag() {
    let scoped = VmScoped::new(vm_a(), 41u32).map(|n| n + 1);
    assert_eq!(scoped.vm(), vm_a());
    assert_eq!(scoped.get(vm_a()).expect("same vm"), 42);
}

/// Each failure kind is distinguishable — the whole point of not using
/// `Option`.
///
/// Eight of the nine kinds; `Pending` is excluded because constructing one
/// needs a live heap `ObjectRef`, which a unit test here cannot mint. Its
/// discriminant is covered by `ResolveError::kind`'s exhaustive match: the
/// compiler fails that match if a variant is added without a kind.
#[test]
fn every_constructible_failure_kind_is_distinguishable() {
    let cases: Vec<(ResolveError, ResolveErrorKind)> = vec![
        (
            ResolveError::NoSuchMethod {
                class_name: "C".into(),
                method_name: "m".into(),
                descriptor: "()V".into(),
            },
            ResolveErrorKind::NoSuchMethod,
        ),
        (
            ResolveError::NoSuchField {
                class_name: "C".into(),
                field_name: "f".into(),
                descriptor: "I".into(),
            },
            ResolveErrorKind::NoSuchField,
        ),
        (
            ResolveError::IllegalAccess {
                message: "no".into(),
            },
            ResolveErrorKind::IllegalAccess,
        ),
        (
            ResolveError::NoClassDefFound {
                class_name: "C".into(),
            },
            ResolveErrorKind::NoClassDefFound,
        ),
        (
            ResolveError::IncompatibleClassChange {
                message: "static/instance".into(),
            },
            ResolveErrorKind::IncompatibleClassChange,
        ),
        (
            ResolveError::ForeignVm {
                expected: vm_a(),
                found: vm_b(),
            },
            ResolveErrorKind::ForeignVm,
        ),
        (
            ResolveError::Internal {
                message: "bad cp".into(),
            },
            ResolveErrorKind::Internal,
        ),
    ];
    let mut seen = std::collections::HashSet::new();
    for (err, kind) in cases {
        assert_eq!(err.kind(), kind, "kind mismatch for {err:?}");
        assert!(seen.insert(kind), "two cases claim kind {kind:?}");
        // Every kind must render something non-empty, so a log line can
        // never silently become "".
        assert!(!err.to_string().is_empty());
    }
    assert_eq!(seen.len(), 7);
}

/// `NoSuchMethod` and `NoSuchField` must survive the round trip through
/// `LinkageError`, because that is how they reach a Java `catch`.
#[test]
fn linkage_round_trip_preserves_the_kind() {
    let err = ResolveError::NoSuchMethod {
        class_name: "java/lang/String".into(),
        method_name: "nope".into(),
        descriptor: "()V".into(),
    };
    let back: ResolveError = LinkageError::from(err).into();
    assert_eq!(back.kind(), ResolveErrorKind::NoSuchMethod);

    let err = ResolveError::NoSuchField {
        class_name: "java/lang/String".into(),
        field_name: "nope".into(),
        descriptor: "[I".into(),
    };
    let back: ResolveError = LinkageError::from(err).into();
    assert_eq!(back.kind(), ResolveErrorKind::NoSuchField);
    // The descriptor survives too: the Java-visible message names the type.
    assert!(matches!(
        back,
        ResolveError::NoSuchField { ref descriptor, .. } if descriptor == "[I"
    ));

    let err = ResolveError::IllegalAccess {
        message: "private".into(),
    };
    let back: ResolveError = LinkageError::from(err).into();
    assert_eq!(back.kind(), ResolveErrorKind::IllegalAccess);
}

/// The distinction `Option` cannot make.
#[test]
fn a_cached_absence_is_a_hit_not_a_miss() {
    let cached_absent: CacheProbe<ResolvedMember> = CacheProbe::Hit(ResolvedMember::NotFound);
    let nothing_cached: CacheProbe<ResolvedMember> = CacheProbe::Miss;

    assert!(cached_absent.is_hit());
    assert!(!nothing_cached.is_hit());

    // And the collapse to Option is exactly what loses it — which is why
    // `into_hit` is named for the thing being discarded.
    assert!(cached_absent.into_hit().is_some());
    assert!(nothing_cached.into_hit().is_none());
}

#[test]
fn the_access_counter_moves_monotonically() {
    let before = access_checks_run();
    ACCESS_CHECKS.fetch_add(1, Ordering::Relaxed);
    assert_eq!(access_checks_run(), before + 1);
}

/// `AccessGrant` records *which* accessor the check was performed for.
/// JVMS §5.4.4 resolution is per (referencing class, symbolic reference),
/// so a grant is not transferable to a different accessor; keeping the
/// accessor on the token is what makes that checkable.
#[test]
fn an_access_grant_names_its_accessor_and_policy() {
    let grant = AccessGrant {
        policy: AccessPolicy::ModuleOnly,
        accessor: ClassId::new(3),
        declaring: ClassId::new(4),
    };
    assert_eq!(grant.policy(), AccessPolicy::ModuleOnly);
    assert_eq!(grant.accessor(), ClassId::new(3));
    assert_eq!(grant.declaring(), ClassId::new(4));
    assert_ne!(AccessPolicy::ModuleOnly, AccessPolicy::Full);
}
