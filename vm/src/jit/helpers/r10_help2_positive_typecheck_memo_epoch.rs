use super::*;

pub fn key(n: u32) -> (usize, usize, usize, u32, bool) {
    (7, 0x2000, 3, n, false)
}

/// Mirrors `a_negative_is_served_only_under_the_epoch_it_was_proven_in`
/// for `JIT_TYPECHECK_ANSWER_CACHE`, which used to have no epoch stamp:
/// a `true` proven for `obj_class_id = n` under one epoch must not answer
/// for whatever class a later unload hands that same numeric id to.
#[test]
pub fn a_positive_answer_is_served_only_under_the_epoch_it_was_proven_in() {
    jit_typecheck_answer_cache_put(key(1), 40);
    assert!(jit_typecheck_answer_cached(key(1), 40));
    assert!(
        !jit_typecheck_answer_cached(key(2), 40),
        "a different receiver class id"
    );
    assert!(
        !jit_typecheck_answer_cached(key(1), 41),
        "a class was defined or unloaded since: the true must be re-proven, \
         not served for whatever class now holds this ClassId"
    );
    assert!(
        !jit_typecheck_answer_cached(key(1), 40),
        "the stale memo was emptied, not merely skipped"
    );
}

/// Bounded, least-recently-used eviction; a hit is moved to the front —
/// same shape as the negative memo's own eviction test.
#[test]
pub fn the_memo_is_bounded_and_keeps_the_hot_entry() {
    let epoch = 9_500;
    jit_typecheck_answer_cache_put(key(0), epoch);
    for n in 1..JIT_TYPECHECK_ANSWER_CACHE_CAP as u32 {
        jit_typecheck_answer_cache_put(key(n), epoch);
        assert!(jit_typecheck_answer_cached(key(0), epoch));
    }
    jit_typecheck_answer_cache_put(key(10_000), epoch);
    assert!(
        jit_typecheck_answer_cached(key(0), epoch),
        "hot entry survived"
    );
    assert!(jit_typecheck_answer_cached(key(10_000), epoch));
    let len = JIT_TYPECHECK_ANSWER_CACHE.with(|c| c.borrow().1.len());
    assert_eq!(len, JIT_TYPECHECK_ANSWER_CACHE_CAP);
}

/// The sibling defect in `JIT_SUBTYPE_POSITIVE_CACHE`, exercised through
/// `jit_is_subclass_of_cached` with a real (lightweight) `SharedVm`: poison
/// the table with a `true` for two classes that are NOT actually related,
/// stamped with an epoch behind the live one -- standing in for "this
/// entry was proven before the class that used to hold this id was
/// unloaded and the id was handed to `child`/`parent`" -- and confirm the
/// stale entry is re-proven rather than served.
#[test]
pub fn a_stale_epoch_positive_subtype_entry_is_reproven_not_served() {
    let shared: std::sync::Arc<SharedVm> =
        std::sync::Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let (child, parent) = {
        let mut cm = shared.classes.class_manager.write();
        let mut mk = |name: &str| {
            cm.try_ensure_synthetic_class(name, 0)
                .expect("Compatible mode fabricates; this fixture never runs under --real-jdk")
        };
        (
            mk("cratonvm/test/R10Help2Child"),
            mk("cratonvm/test/R10Help2Parent"),
        )
    };
    // Two freshly fabricated classes share no hierarchy beyond `Object`,
    // so the real answer is `false` -- which is exactly what makes a
    // poisoned `true` entry observable as wrong if it were ever served.
    let real_answer = shared
        .classes
        .class_manager
        .read()
        .is_subclass_of(child, parent);
    assert!(
        !real_answer,
        "fixture assumption: two freshly fabricated classes are not subtypes \
         of each other"
    );
    let live_epoch = typecheck_memo_epoch(&shared);
    let stale_epoch = live_epoch.wrapping_sub(1);
    let key = (shared.vm_identity, child.as_u32(), parent.as_u32());
    JIT_SUBTYPE_POSITIVE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.0 = stale_epoch;
        cache.1.clear();
        cache.1.push(key);
    });
    assert!(
        !jit_is_subclass_of_cached(&shared, child, parent),
        "a positive answer stamped with a stale epoch must be re-proven, not \
         served -- otherwise a ClassId an unload later reused for an unrelated \
         class would inherit someone else's cached `true`"
    );
    let (stamp_after, still_has_poison) = JIT_SUBTYPE_POSITIVE_CACHE.with(|cache| {
        let cache = cache.borrow();
        (cache.0, cache.1.contains(&key))
    });
    assert!(
        stamp_after >= live_epoch,
        "the table must be re-stamped with a current-or-later epoch, not left \
         on the poisoned one"
    );
    assert!(
        !still_has_poison,
        "the poisoned entry must be cleared, not merely skipped and left behind"
    );
}
