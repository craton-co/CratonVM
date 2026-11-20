use super::*;
use crate::registry::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn native_a(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn native_b(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(7)))
}

fn addr(cb: NativeCallback) -> usize {
    cb as usize
}

#[test]
fn warm_call_site_returns_the_same_callback() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()V", native_a, NativeKind::SyntheticStub);
    let site = NativeCallSite::new();

    assert!(!site.is_warm(&registry));
    let first = site
        .callback(&registry, "a/A", "m", "()V")
        .expect("cold hit");
    assert!(site.is_warm(&registry));
    let second = site
        .callback(&registry, "a/A", "m", "()V")
        .expect("warm hit");
    assert_eq!(addr(first), addr(second));
    assert_eq!(
        addr(first),
        addr(registry.find("a/A", "m", "()V").expect("name lookup"))
    );
}

#[test]
fn memoized_negative_self_heals_when_a_native_is_registered_later() {
    // The `OnceLock<Option<NativeCallback>>` this mechanism replaces caches
    // a `None` forever, which is only sound if registration is finished.
    // The generation check makes it sound at any point in the VM lifetime.
    let mut registry = NativeMethodRegistry::new();
    registry.register("other/O", "x", "()V", native_a, NativeKind::SyntheticStub);

    let site = NativeCallSite::new();
    assert!(site.callback(&registry, "late/L", "m", "()V").is_none());
    assert!(site.is_warm(&registry), "negative results are memoized too");

    registry.register("late/L", "m", "()V", native_b, NativeKind::SyntheticStub);
    assert!(
        !site.is_warm(&registry),
        "a new slot must invalidate the memo"
    );
    let cb = site
        .callback(&registry, "late/L", "m", "()V")
        .expect("stale negative must self-heal");
    assert_eq!(addr(cb), addr(native_b as NativeCallback));
}

#[test]
fn re_registration_is_seen_through_a_warm_call_site() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()V", native_a, NativeKind::SyntheticStub);
    let site = NativeCallSite::new();
    let before = site.callback(&registry, "a/A", "m", "()V").expect("hit");
    assert_eq!(addr(before), addr(native_a as NativeCallback));

    // Same triple, new callback: the slot is updated in place, so the warm
    // memo (which stores the slot index, not the callback) picks it up
    // without any invalidation.
    registry.register("a/A", "m", "()V", native_b, NativeKind::SyntheticStub);
    let after = site.callback(&registry, "a/A", "m", "()V").expect("hit");
    assert_eq!(addr(after), addr(native_b as NativeCallback));
}

#[test]
fn call_site_agrees_with_direct_lookup_including_kind() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("java/lang/String", "length", "()I", native_b, NativeKind::Intrinsic);
    let site = NativeCallSite::new();
    let via_site = site
        .callback_with_kind(&registry, "java/lang/String", "length", "()I")
        .expect("hit");
    let direct = registry
        .find_with_kind("java/lang/String", "length", "()I")
        .expect("hit");
    assert_eq!(addr(via_site.0), addr(direct.0));
    assert_eq!(via_site.1, direct.1);
    assert_eq!(via_site.1, NativeKind::Intrinsic);
}

#[test]
fn resolve_with_key_matches_resolve() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "(J)Ljava/lang/Object;", native_a, NativeKind::SyntheticStub);
    let key = NativeMethodKey::new("a/A", "m", "(J)Ljava/lang/Object;");

    let keyed = NativeCallSite::new();
    let named = NativeCallSite::new();
    assert_eq!(
        keyed.resolve_with_key(&registry, key, "a/A", "m", "(J)Ljava/lang/Object;"),
        named.resolve(&registry, "a/A", "m", "(J)Ljava/lang/Object;")
    );
    // Warm path too.
    assert_eq!(
        keyed.resolve_with_key(&registry, key, "a/A", "m", "(J)Ljava/lang/Object;"),
        named.resolve(&registry, "a/A", "m", "(J)Ljava/lang/Object;")
    );
}

#[test]
fn invalidate_forces_a_re_resolve() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()V", native_a, NativeKind::SyntheticStub);
    let site = NativeCallSite::new();
    assert!(site.callback(&registry, "a/A", "m", "()V").is_some());
    assert!(site.is_warm(&registry));
    site.invalidate();
    assert!(!site.is_warm(&registry));
    assert!(site.callback(&registry, "a/A", "m", "()V").is_some());
}

#[test]
fn clone_preserves_the_memo() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()V", native_a, NativeKind::SyntheticStub);
    let site = NativeCallSite::new();
    assert!(site.callback(&registry, "a/A", "m", "()V").is_some());
    let copy = site.clone();
    assert!(copy.is_warm(&registry));
    assert_eq!(
        copy.resolve(&registry, "a/A", "m", "()V"),
        site.resolve(&registry, "a/A", "m", "()V")
    );
}

#[test]
fn a_memo_from_one_registry_is_not_honoured_by_another() {
    // The `static NativeCallSite` pattern the interpreter adoption plan
    // recommends is process-global, so it can be reached with a different
    // registry than it was filled from (test binaries build several VMs).
    // Generations are banded per registry, so this must re-resolve, not
    // redeem a slot index that means something else.
    let mut first = NativeMethodRegistry::new();
    first.register("a/A", "m", "()V", native_a, NativeKind::SyntheticStub);
    let mut second = NativeMethodRegistry::new();
    second.register("z/Z", "other", "()V", native_b, NativeKind::SyntheticStub);

    let site = NativeCallSite::new();
    assert!(site.callback(&first, "a/A", "m", "()V").is_some());
    assert!(site.is_warm(&first));
    assert!(
        !site.is_warm(&second),
        "a memo taken against one registry must not be warm for another"
    );
    // ...and re-resolving against the second registry gives the second
    // registry's (correct) answer, not the first's slot.
    assert!(site.callback(&second, "a/A", "m", "()V").is_none());

    // A `NativeCallSite` memoizes ONE call site, so the memo is keyed on the
    // registry generation alone — the triple is invariant by construction at
    // a real site and is deliberately not re-checked on a warm hit. Probing
    // a *different* triple therefore needs its own site: the line above just
    // memoized "not found" for `second`'s generation, and reusing `site`
    // here would redeem that negative rather than resolve `z/Z.other`.
    //
    // This is a live footgun for the interpreter adoption plan, which puts
    // `static NativeCallSite` cells at the constant-triple call sites: each
    // such site must have its own static, never a shared one.
    let other_site = NativeCallSite::new();
    assert!(other_site
        .callback(&second, "z/Z", "other", "()V")
        .is_some());
    // And that second site is likewise not warm for the first registry.
    assert!(!other_site.is_warm(&first));
}

// -----------------------------------------------------------------------
// ARCH-2026-07-26 `cross-owner-closeout`: the one-cell-one-triple contract.
//
// The memo is keyed on the registry generation alone; the triple is NOT
// re-checked on a warm hit (that would reinstate the string hashing this
// type exists to remove). So a shared cell is silently wrong. These tests
// pin the debug-only witness that makes it loud instead.
// -----------------------------------------------------------------------

#[test]
fn witness_accepts_the_same_triple_repeatedly() {
    let site = NativeCallSite::new();
    for _ in 0..4 {
        assert!(super::witness_triple(&site, "a/A", "m", "()V"));
    }
}

#[test]
#[cfg(debug_assertions)]
fn witness_rejects_a_second_triple_on_the_same_cell() {
    let site = NativeCallSite::new();
    assert!(
        super::witness_triple(&site, "a/A", "m", "()V"),
        "first query sets it"
    );
    assert!(
        !super::witness_triple(&site, "a/A", "m", "()I"),
        "a differing descriptor alone must be caught"
    );
    // The witness moves to the most recent triple, so a repeat of the new
    // one agrees — the assertion has already fired for the transition.
    assert!(super::witness_triple(&site, "a/A", "m", "()I"));
    assert!(
        !super::witness_triple(&site, "b/B", "m", "()I"),
        "a differing class must be caught"
    );
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "NativeCallSite reused for a second")]
fn sharing_one_cell_between_two_triples_panics_in_debug() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()V", native_a, NativeKind::SyntheticStub);
    registry.register("z/Z", "other", "()V", native_b, NativeKind::SyntheticStub);

    let shared = NativeCallSite::new();
    assert!(shared.callback(&registry, "a/A", "m", "()V").is_some());
    // Without the witness this would silently redeem `a/A.m`'s slot for
    // `z/Z.other` — the failure mode the invariant exists to prevent.
    let _ = shared.callback(&registry, "z/Z", "other", "()V");
}

#[test]
fn a_dedicated_cell_per_triple_is_the_supported_shape() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()V", native_a, NativeKind::SyntheticStub);
    registry.register("z/Z", "other", "()V", native_b, NativeKind::SyntheticStub);

    let site_a = NativeCallSite::new();
    let site_z = NativeCallSite::new();
    assert_eq!(
        addr(site_a.callback(&registry, "a/A", "m", "()V").expect("hit")),
        addr(native_a as NativeCallback)
    );
    assert_eq!(
        addr(
            site_z
                .callback(&registry, "z/Z", "other", "()V")
                .expect("hit")
        ),
        addr(native_b as NativeCallback)
    );
    // ...and stay correct once warm.
    assert_eq!(
        addr(site_a.callback(&registry, "a/A", "m", "()V").expect("hit")),
        addr(native_a as NativeCallback)
    );
    assert_eq!(
        addr(
            site_z
                .callback(&registry, "z/Z", "other", "()V")
                .expect("hit")
        ),
        addr(native_b as NativeCallback)
    );
}

#[test]
fn invalidate_does_not_clear_the_triple_witness() {
    // `invalidate` forgets the *memo*; it does not turn the cell into a
    // different call site, so the contract still holds across it.
    let site = NativeCallSite::new();
    assert!(super::witness_triple(&site, "a/A", "m", "()V"));
    site.invalidate();
    assert!(super::witness_triple(&site, "a/A", "m", "()V"));
}

#[test]
fn clone_carries_the_triple_witness() {
    let site = NativeCallSite::new();
    assert!(super::witness_triple(&site, "a/A", "m", "()V"));
    let copy = site.clone();
    assert!(super::witness_triple(&copy, "a/A", "m", "()V"));
    #[cfg(debug_assertions)]
    assert!(
        !super::witness_triple(&copy, "a/A", "m", "()I"),
        "cloning must not launder the contract into a blank cell"
    );
}

#[test]
fn id_round_trips_through_a_raw_u32() {
    let id = NativeMethodId::from_u32(1234);
    assert_eq!(id.as_u32(), 1234);
    assert_eq!(id.index(), 1234usize);
    assert_eq!(NativeMethodId::from_u32(id.as_u32()), id);
}
