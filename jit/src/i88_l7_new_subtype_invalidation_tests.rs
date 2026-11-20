use super::*;

/// A body with `inlined` as its dependencies; `None` (after a failed
/// assertion) when no executable buffer could be mapped. No panicking
/// construct: `jit/tests/panic_free_compile_ratchet.rs` counts them.
fn body(inlined: &[(&str, &str, &str)]) -> Option<CompiledMethod> {
    let buffer = ExecutableBuffer::new(64);
    assert!(buffer.is_some(), "an executable test buffer");
    let mut cm = CompiledMethod::new(buffer?);
    cm.inlined_methods = inlined
        .iter()
        .map(|(c, m, d)| (c.to_string(), m.to_string(), d.to_string()))
        .collect();
    Some(cm)
}

const CID: cratonvm_types::ClassId = cratonvm_types::ClassId::new(0);

fn fixed(pairs: &[(&str, &str)]) -> Arc<[(Arc<str>, Arc<str>)]> {
    pairs
        .iter()
        .map(|(m, d)| (Arc::<str>::from(*m), Arc::<str>::from(*d)))
        .collect()
}

#[test]
fn a_constructor_chain_survives_a_new_subclass() {
    let cache = JitCache::new();
    let Some(leaf) = body(&[("p/Base", "<init>", "()V"), ("java/lang/Object", "<init>", "()V")])
    else {
        return;
    };
    cache.put("p/Leaf".into(), "<init>".into(), "()V".into(), CID, leaf);
    assert_eq!(cache.invalidate_for_new_subtype("p/Base", fixed(&[])), 0);
    assert_eq!(
        cache.invalidate_for_new_subtype("java/lang/Object", fixed(&[])),
        0
    );
    assert_eq!(cache.len(), 1, "a constructor-only dependency was withdrawn");
    assert!(!cache.any_body_depends_on_subtype("java/lang/Object", &[]));
}

/// Skipping a scan must still stop an in-flight virtual speculation from
/// publishing after the hierarchy change. The constructor already in the
/// cache makes this exercise the member summary rather than a missing name.
#[test]
fn member_summary_early_return_still_refuses_an_in_flight_virtual_body() {
    let cache = JitCache::new();
    let (Some(ctor), Some(virtual_dep)) = (
        body(&[("p/Base", "<init>", "()V")]),
        body(&[("p/Base", "v", "()I")]),
    ) else {
        return;
    };
    cache.put("p/Caller".into(), "ctor".into(), "()V".into(), CID, ctor);
    assert!(!cache.any_body_depends_on_subtype("p/Base", &[]));
    assert_eq!(cache.invalidate_for_new_subtype("p/Base", fixed(&[])), 0);
    cache.put("p/Caller".into(), "v".into(), "()V".into(), CID, virtual_dep);
    assert_eq!(cache.len(), 1, "the stale virtual dependency must not publish");
}

#[test]
fn a_final_or_static_member_survives_and_a_virtual_one_does_not() {
    let cache = JitCache::new();
    let (Some(final_dep), Some(virtual_dep)) =
        (body(&[("p/Base", "f", "()I")]), body(&[("p/Base", "v", "()I")]))
    else {
        return;
    };
    cache.put("p/Caller".into(), "a".into(), "()V".into(), CID, final_dep);
    cache.put("p/Caller".into(), "b".into(), "()V".into(), CID, virtual_dep);
    let base_fixed = fixed(&[("f", "()I")]);
    assert_eq!(
        cache.invalidate_for_new_subtype("p/Base", Arc::clone(&base_fixed)),
        1,
        "exactly the body that depends on the overridable `v`"
    );
    assert_eq!(cache.len(), 1);
}

#[test]
fn the_log_record_judges_a_running_compile_by_the_same_rule() {
    let (Some(ctor_only), Some(virtual_dep)) = (
        body(&[("p/Base", "<init>", "()V")]),
        body(&[("p/Base", "v", "()I")]),
    ) else {
        return;
    };
    let record = InvalidationRecord::NewSubtypeOf {
        class_name: Arc::from("p/Base"),
        fixed: fixed(&[]),
    };
    let key = JitKey {
        class_name: Arc::from("p/Caller"),
        method_name: Arc::from("m"),
        descriptor: Arc::from("()V"),
        declaring_class_id: CID,
    };
    assert!(!record.withdraws(&key, &ctor_only));
    assert!(record.withdraws(&key, &virtual_dep));
}

/// JIT round 21 wave 17 (w16 review item 7): a caller that spliced the
/// inherited `p/A.d1` through `p/K3` records `(p/K3, d1)` and the
/// replacement-only marker for `p/A`. A define that replaces `p/A` by name
/// withdraws it; a new subtype of `p/A` does not (the wave-15 point), and
/// a new subtype of `p/K3` still does.
#[test]
fn the_declaring_class_marker_answers_replacement_but_not_a_new_subtype() {
    let triples = [
        ("p/K3", "d1", "()I"),
        ("p/A", DECLARING_CLASS_REPLACE_MARKER, "()I"),
    ];
    let Some(first) = body(&triples) else {
        return;
    };
    let cache = JitCache::new();
    cache.put("p/Caller".into(), "m".into(), "()V".into(), CID, first);
    assert_eq!(cache.invalidate_for_new_subtype("p/A", fixed(&[])), 0);
    assert_eq!(cache.len(), 1, "a new subtype of the declaring class");
    assert_eq!(cache.invalidate_for_class("p/A"), 1, "a replacement by name");
    assert_eq!(cache.len(), 0);
    // A fresh cache for the `p/K3` half: a body built before the `p/A`
    // replacement above (its `invalidation_epoch` predates the logged
    // record) and naming `p/A` is REFUSED at publication by that record
    // (`dependencies_are_current`) -- the marker working, not a miss.
    let Some(second) = body(&triples) else {
        return;
    };
    let fresh = JitCache::new();
    fresh.put("p/Caller".into(), "m".into(), "()V".into(), CID, second);
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh.invalidate_for_new_subtype("p/K3", fixed(&[])), 1);
}
