use super::tests_redefinition_count;
use super::{ClassId, ResolutionCache};

#[test]
fn r21w5_a_bound_resolution_cache_judges_fills_by_its_own_vm() {
    let log = crate::RedefinitionLog::new_shared();
    let mut cache = ResolutionCache::with_redefinition_log(std::sync::Arc::clone(&log));
    let mut unbound = ResolutionCache::new();
    assert!(!unbound.has_redefinition_log());
    let c = ClassId::new(21_501);
    let answer = ClassId::new(21_502);

    let as_of = ResolutionCache::fill_snapshot();
    // Another VM redefines a class with `c`'s id: the process ring names
    // `c`, this VM's log does not.
    tests_redefinition_count::bump_of(c);
    assert_eq!(unbound.put_class_entry_as_of(c, 3, answer, as_of), None);
    let admitted = cache.put_class_entry_as_of(c, 3, answer, as_of);
    if cache.has_redefinition_log() {
        assert_eq!(admitted, Some(answer), "the move was another VM's");
    } else {
        // `CRATONVM_INVOKE_CACHE_PER_VM_REDEFINITIONS=0` in the environment.
        assert_eq!(admitted, None);
        return;
    }

    // This VM redefines `c`: a fill that straddles it is refused.
    let as_of = ResolutionCache::fill_snapshot();
    tests_redefinition_count::bump_of(c);
    log.record_for_test(tests_redefinition_count::get(), c);
    assert_eq!(cache.put_class_entry_as_of(c, 4, answer, as_of), None);
}
