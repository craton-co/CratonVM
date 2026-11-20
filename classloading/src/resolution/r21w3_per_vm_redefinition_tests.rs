use super::tests_redefinition_count;
use super::{CachedBytecodeMethod, CachedInvokeTarget, ClassId, InvokeCache, RedefineGate};
use std::sync::Arc;

fn target(declaring: u32) -> CachedInvokeTarget<()> {
    CachedInvokeTarget::Bytecode {
        cached: Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_reader::CachedMethodParts {
                declaring_class_id: ClassId::new(declaring),
                class_name: Arc::from("p/Helper"),
                method_name: Arc::from("pick"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: Arc::from(&[0x04u8, 0xACu8][..]), // iconst_1; ireturn
                exception_table: Arc::from(&[][..]),
                max_stack: 1,
                max_locals: 0,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        )),
        gate: RedefineGate::never_stale(),
    }
}

fn bound() -> Option<(InvokeCache<()>, Arc<crate::RedefinitionLog>)> {
    let log = crate::RedefinitionLog::new_shared();
    let mut cache = InvokeCache::<()>::new();
    cache.bind_redefinition_log(Arc::clone(&log));
    // `CRATONVM_INVOKE_CACHE_PER_VM_REDEFINITIONS=0` in the environment.
    cache.has_redefinition_log().then_some((cache, log))
}

#[test]
fn a_bound_cache_keeps_its_entries_across_another_vms_redefinition() {
    let Some((mut cache, log)) = bound() else {
        return;
    };
    let c = ClassId::new(21_301);
    cache.put(c, 5, false, 0, target(21_302));
    // Another VM redefines a class with the same id: the clock moves three
    // times, this VM's log records nothing.
    for _ in 0..3 {
        tests_redefinition_count::bump_of(c);
    }
    assert!(
        cache.get(c, 5, false, 0).is_some(),
        "another VM's redefinition"
    );
    assert_eq!(cache.redefinitions_seen(), tests_redefinition_count::get());
    // This VM redefines `c`: retired.
    for _ in 0..3 {
        tests_redefinition_count::bump_of(c);
        log.record_for_test(tests_redefinition_count::get(), c);
    }
    assert!(
        cache.get(c, 5, false, 0).is_none(),
        "this VM's redefinition"
    );
    assert_eq!(log.records(), 3);
}

#[test]
fn a_bound_cache_admits_a_fill_that_straddles_only_another_vms_redefinition() {
    let Some((mut cache, log)) = bound() else {
        return;
    };
    let c = ClassId::new(21_311);
    assert!(cache.get(c, 5, false, 0).is_none());
    let as_of = cache.redefinitions_seen();
    tests_redefinition_count::bump_of(ClassId::new(21_312));
    cache.put_as_of(c, 5, false, 0, as_of, target(21_313));
    assert!(
        cache.get(c, 5, false, 0).is_some(),
        "the move was another VM's"
    );

    let as_of = cache.redefinitions_seen();
    tests_redefinition_count::bump_of(ClassId::new(21_314));
    log.record_for_test(tests_redefinition_count::get(), ClassId::new(21_314));
    cache.put_as_of(c, 7, false, 0, as_of, target(21_315));
    assert!(
        cache.get(c, 7, false, 0).is_none(),
        "this VM redefined something"
    );
}

#[test]
fn an_unbound_cache_keeps_the_process_wide_retirement() {
    let mut cache = InvokeCache::<()>::new();
    assert!(!cache.has_redefinition_log());
    let c = ClassId::new(21_321);
    cache.put(c, 5, false, 0, target(21_322));
    tests_redefinition_count::bump_of(c);
    assert!(cache.get(c, 5, false, 0).is_none());
}

#[test]
fn the_log_names_its_own_records_and_refuses_what_it_cannot_answer() {
    let log = crate::RedefinitionLog::new_shared();
    assert!(log.for_each_between(0, 10, |_| panic!("nothing recorded")));
    log.record_for_test(4, ClassId::new(7));
    log.record_for_test(9, ClassId::new(8));
    let mut seen = Vec::new();
    assert!(log.for_each_between(3, 9, |c| seen.push(c)));
    assert_eq!(seen, vec![ClassId::new(8), ClassId::new(7)]);
    let mut seen = Vec::new();
    assert!(
        log.for_each_between(4, 8, |c| seen.push(c)),
        "9 is past `upto`"
    );
    assert!(seen.is_empty());
    assert!(!log.for_each_between(5, 4, |_| {}), "a backwards span");
    // Lap the ring: a span older than every kept record cannot be answered.
    for stamp in 10..10 + 300u64 {
        log.record_for_test(stamp, ClassId::new(9));
    }
    assert!(!log.for_each_between(3, 400, |_| {}));
    assert!(log.for_each_between(305, 400, |_| {}));
}
