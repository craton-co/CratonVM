use super::*;

fn method(name: &str) -> ResolvedMethod {
    ResolvedMethod {
        declaring_class_id: ClassId::new(22_502),
        class_name: Arc::from("i22l5/Owner"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from("()V"),
        num_params: 0,
        native_target: None,
        native_kind: None,
    }
}

/// Interpreter round i1 wave 22, lane L5: a `(class, cp index)` fill whose
/// resolution read the constant pool before a redefinition is not
/// published — the redefinition's sweep already ran, and nothing would
/// sweep the old pool's member again. A fill that saw no redefinition is.
#[test]
fn a_fill_that_straddles_a_redefinition_is_not_published() {
    let mut cache = ResolutionCache::new();
    let caller = ClassId::new(22_501);

    let before = ResolutionCache::fill_snapshot();
    // What `redefine_class` does between the resolver's pool read and
    // its fill: advance the count, sweep, advance it again.
    tests_redefinition_count::bump_of(caller);
    cache.invalidate_class(caller);
    tests_redefinition_count::bump_of(caller);
    assert!(!cache.put_method_as_of(caller, 7, method("oldPoolMember"), before));
    assert!(
        cache.get_method(caller, 7).is_none(),
        "the old pool's member is not cached under the new pool's index"
    );
    let field = ResolvedField {
        declaring_class_id: ClassId::new(22_502),
        field_index: 0,
        is_static: false,
        is_volatile: false,
        is_final: false,
        is_reference: false,
        desc_byte: b'I',
    };
    assert!(!cache.put_field_as_of(caller, 8, field.clone(), before));
    assert!(cache.get_field(caller, 8).is_none());

    let now = ResolutionCache::fill_snapshot();
    assert!(cache.put_method_as_of(caller, 7, method("newPoolMember"), now));
    assert_eq!(
        cache.get_method(caller, 7).map(|m| &*m.method_name),
        Some("newPoolMember")
    );
    assert!(cache.put_field_as_of(caller, 8, field, now));
    assert!(cache.get_field(caller, 8).is_some());
}

/// Interpreter round i1 wave 23, lane L5
/// (`i22-L5-resolution-cache-fills-can-outlive-a-concurrent-redefinition`):
/// the call-site, constant and failure fills have `_as_of` twins, and
/// every twin is class-scoped — a redefinition of ANOTHER class does not
/// skip the fill (a skipped permanent constant would re-run its
/// bootstrap, a skipped failure would let a later attempt succeed), while
/// one of the caller, or a span the ring cannot name, does.
#[test]
fn every_fill_twin_skips_exactly_when_the_callers_pool_may_have_been_replaced() {
    let mut cache = ResolutionCache::new();
    let caller = ClassId::new(23_501);
    let unrelated = ClassId::new(23_502);
    let failure = || ResolutionFailure {
        error_class: Arc::from("java/lang/NoClassDefFoundError"),
        message: Some(Arc::from("i23l5/Missing")),
        cause: None,
    };
    let site = || ResolvedCallSite::StringConcat {
        recipe: Arc::from(&[1u16][..]),
        constant_args: Vec::new(),
        target_descriptor: Arc::from("(I)Ljava/lang/String;"),
        latin1_shape: true,
    };

    // Another class's redefinition: every fill is published.
    let before = ResolutionCache::fill_snapshot();
    tests_redefinition_count::bump_of(unrelated);
    assert!(cache.put_call_site_as_of(caller, 1, site(), before));
    assert!(cache.put_condy_as_of(caller, 2, Value::Int(7), before));
    assert_eq!(
        cache.put_permanent_constant_as_of(caller, 3, Value::Int(8), before),
        Some(Value::Int(8))
    );
    assert!(cache.put_failure_as_of(caller, 4, failure(), before));
    assert!(cache.put_method_as_of(caller, 5, method("kept"), before));
    assert!(cache.get_call_site(caller, 1).is_some());
    assert_eq!(cache.get_condy(caller, 2), Some(&Value::Int(7)));
    assert_eq!(cache.get_condy(caller, 3), Some(&Value::Int(8)));
    assert!(cache.get_failure(caller, 4).is_some());

    // The caller's redefinition between the pool read and the fill: the
    // sweep already ran, so nothing is published and nothing replaced.
    let before = ResolutionCache::fill_snapshot();
    tests_redefinition_count::bump_of(caller);
    cache.invalidate_class(caller);
    tests_redefinition_count::bump_of(caller);
    assert!(!cache.put_call_site_as_of(caller, 11, site(), before));
    assert!(!cache.put_condy_as_of(caller, 12, Value::Int(1), before));
    assert_eq!(
        cache.put_permanent_constant_as_of(caller, 13, Value::Int(2), before),
        None,
        "the old pool's constant is handed to its own execution only"
    );
    assert!(!cache.put_failure_as_of(caller, 14, failure(), before));
    assert!(cache.get_call_site(caller, 11).is_none());
    assert!(cache.get_condy(caller, 12).is_none());
    assert!(cache.get_condy(caller, 13).is_none());
    assert!(
        cache.get_failure(caller, 14).is_none(),
        "the new pool's entry at 14 is not failed with the old entry's error"
    );

    // A span the ring cannot name (an unattributed bump): skipped.
    let before = ResolutionCache::fill_snapshot();
    tests_redefinition_count::bump();
    assert!(!cache.put_failure_as_of(caller, 24, failure(), before));
    assert!(cache.get_failure(caller, 24).is_none());

    // No redefinition at all: published.
    let now = ResolutionCache::fill_snapshot();
    assert!(cache.put_failure_as_of(caller, 34, failure(), now));
    assert!(cache.get_failure(caller, 34).is_some());
}

/// Round 15 wave 5, lane classsite: a resolved `CONSTANT_Class` entry
/// keeps its first answer (a racing or later resolver gets it back), is
/// swept by key when its class is redefined, survives a redefinition of
/// the class it names, and goes when either side is unloaded.
#[test]
fn a_class_entry_keeps_its_first_resolution() {
    let mut cache = ResolutionCache::new();
    let referencing = ClassId::new(25_501);
    let peer = ClassId::new(25_502);
    let app = ClassId::new(25_503);
    let flipped = ClassId::new(25_504);

    let now = ResolutionCache::fill_snapshot();
    assert_eq!(cache.get_class_entry(referencing, 5), None);
    assert_eq!(
        cache.put_class_entry_as_of(referencing, 5, app, now),
        Some(app)
    );
    assert_eq!(
        cache.put_class_entry_as_of(referencing, 5, flipped, now),
        Some(app),
        "the first resolution is the entry's"
    );
    assert_eq!(cache.get_class_entry(referencing, 5), Some(app));
    assert_eq!(cache.put_class_entry_as_of(peer, 5, app, now), Some(app));

    // The named class redefined in place: same identity, entry kept.
    cache.invalidate_class(app);
    assert_eq!(cache.get_class_entry(referencing, 5), Some(app));
    // A layout-only change touches no class entry.
    cache.invalidate_class_field_layout(referencing);
    assert_eq!(cache.get_class_entry(referencing, 5), Some(app));
    // The referencing class redefined: its pool may name another class.
    cache.invalidate_class(referencing);
    assert_eq!(cache.get_class_entry(referencing, 5), None);
    assert_eq!(cache.get_class_entry(peer, 5), Some(app));

    // A fill straddling the referencing class's redefinition: skipped.
    let before = ResolutionCache::fill_snapshot();
    tests_redefinition_count::bump_of(referencing);
    assert_eq!(
        cache.put_class_entry_as_of(referencing, 6, app, before),
        None
    );
    assert_eq!(cache.get_class_entry(referencing, 6), None);

    // The named class unloaded: the entry goes with it.
    cache.forget_classes(|c| c == app);
    assert_eq!(cache.get_class_entry(peer, 5), None);
    let now = ResolutionCache::fill_snapshot();
    assert_eq!(
        cache.put_class_entry_as_of(peer, 7, flipped, now),
        Some(flipped)
    );
    // Kept across `clear` (round 15 wave 5, lane classsite2, finding C4):
    // nothing there moves the epoch a site table admitted it under.
    cache.clear();
    assert_eq!(cache.get_class_entry(peer, 7), Some(flipped));
}

/// t1 W14 (`i1-L2-proposal-per-class-resolved-constant-pool`, the wave-41
/// racing-entry window): one `CONSTANT_Class` entry has ONE outcome, as
/// HotSpot's single resolved-or-error tag gives it. Whichever of a
/// success and a failure is recorded first is the entry's; the other is
/// refused, so no thread can hold one answer while the rest throw.
#[test]
fn a_class_entry_has_one_outcome_success_or_failure() {
    let mut cache = ResolutionCache::new();
    let referencing = ClassId::new(25_701);
    let answer = ClassId::new(25_702);
    let failure = || ResolutionFailure {
        error_class: Arc::from("java/lang/NoClassDefFoundError"),
        message: Some(Arc::from("t1w14/Raced")),
        cause: None,
    };
    let now = ResolutionCache::fill_snapshot();

    // The success first: the racing failure is not recorded.
    assert_eq!(
        cache.put_class_entry_as_of(referencing, 3, answer, now),
        Some(answer)
    );
    assert!(!cache.put_failure_as_of(referencing, 3, failure(), now));
    assert!(!cache.put_failure(referencing, 3, failure()));
    assert!(cache.get_failure(referencing, 3).is_none());
    assert_eq!(cache.get_class_entry(referencing, 3), Some(answer));

    // The failure first: the racing success is not recorded.
    assert!(cache.put_failure_as_of(referencing, 4, failure(), now));
    assert_eq!(cache.put_class_entry_as_of(referencing, 4, answer, now), None);
    assert_eq!(cache.get_class_entry(referencing, 4), None);
    assert!(cache.get_failure(referencing, 4).is_some());
}

/// JIT round 16, lane classsite: `has_class_entries` is the by-name
/// search's gate, so it must say "none" exactly when none is recorded.
#[test]
fn has_class_entries_tracks_the_map() {
    let mut cache = ResolutionCache::new();
    assert!(!cache.has_class_entries());
    let referencing = ClassId::new(25_601);
    let now = ResolutionCache::fill_snapshot();
    if cache
        .put_class_entry_as_of(referencing, 3, ClassId::new(25_602), now)
        .is_none()
    {
        // A redefinition elsewhere straddled the fill: nothing to test.
        return;
    }
    assert!(cache.has_class_entries());
    cache.invalidate_class(referencing);
    assert!(!cache.has_class_entries());
}

/// Interpreter round i6 wave 125, lane L5 (i46-L5 stage 3, step 1):
/// `class_entry_targets_of` lists exactly the referencing class's rows.
#[test]
fn class_entry_targets_of_lists_one_referencing_class() {
    let mut cache = ResolutionCache::new();
    let a = ClassId::new(25_611);
    let b = ClassId::new(25_612);
    let now = ResolutionCache::fill_snapshot();
    if cache.put_class_entry_as_of(a, 3, ClassId::new(25_613), now).is_none()
        || cache.put_class_entry_as_of(a, 9, ClassId::new(25_614), now).is_none()
        || cache.put_class_entry_as_of(b, 3, ClassId::new(25_615), now).is_none()
    {
        // A redefinition elsewhere straddled the fill: nothing to test.
        return;
    }
    let mut rows: Vec<_> = cache.class_entry_targets_of(a).collect();
    rows.sort_by_key(|&(index, _)| index);
    assert_eq!(rows, vec![(3, ClassId::new(25_613)), (9, ClassId::new(25_614))]);
    assert_eq!(
        cache.class_entry_targets_of(b).collect::<Vec<_>>(),
        vec![(3, ClassId::new(25_615))]
    );
    assert_eq!(cache.class_entry_targets_of(ClassId::new(25_616)).count(), 0);
}
