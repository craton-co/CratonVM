use super::*;

/// Every class the allow-list is supposed to protect is still protected.
///
/// This replaces `real_protected_stub_paths_diverge_on_exactly_stringjoiner`
/// (2026-07-10 → 2026-08-04), which froze the one-class divergence between
/// the warm and cold dispatch paths. There is now a single predicate, so
/// there is nothing left to compare the paths against — an "the paths
/// agree" assertion over one function would be a guard that cannot fail.
/// What can still regress is a class silently leaving the list, so that is
/// what is asserted, with a floor on the corpus size so that emptying the
/// corpus does not make the test vacuous either.
///
/// It does not cover the `CRATONVM_REAL` env selection, which the predicate
/// ORs in from `real_bytecode_selector()`; that widens the list, never
/// narrows it.
#[test]
fn every_allowlisted_class_is_protected() {
    assert!(
        REAL_PROTECTED_STUB_CORPUS.len() >= 12,
        "the corpus shrank to {} entries; a class was removed from the \
         real-protected-stub allow-list. That hands its `SyntheticStub` natives \
         back to a synthetic field layout over the real JDK class — for \
         `StringJoiner` that was a silently empty join, not a crash. If the \
         removal is intended, lower this floor deliberately and say why.",
        REAL_PROTECTED_STUB_CORPUS.len()
    );
    for class in REAL_PROTECTED_STUB_CORPUS {
        assert!(
            real_protected_stub_class(class),
            "{class} is in REAL_PROTECTED_STUB_CORPUS but the allow-list no longer \
             protects it; restore it to `real_protected_stub_class_common` or \
             remove it from the corpus and lower the floor above"
        );
    }
}
