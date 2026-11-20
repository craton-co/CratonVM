use crate::classloading::ClassId;
use crate::config::VmConfig;
use crate::jit::tiered::MethodKey;
use crate::vm::SharedVm;

/// The tiered manager stamps with the realm's epoch: another VM's
/// redefinition or flush neither moves it nor expires an OSR denial, and
/// this VM's own flush does both.
#[test]
fn another_vms_redefinition_does_not_expire_this_vms_osr_denials() {
    let first = SharedVm::new(VmConfig::default());
    let second = SharedVm::new(VmConfig::default());
    let tiered = &first.jit.tiered_manager;
    assert_eq!(tiered.install_epoch(), first.jit.code_state_epoch());
    let key = MethodKey::with_class_id(ClassId::new(0x1403), "l3/Denied", "loop", "()V");
    tiered.mark_osr_denied(key.clone());
    let before = first.jit.code_state_epoch();

    // What `redefine_class_with` and `jit_invalidate_adapter` do to the
    // second VM; both move the process-wide install epoch too.
    second.jit.note_class_redefinition();
    second.jit.flush_code_cache();
    assert_eq!(first.jit.code_state_epoch(), before);
    assert_eq!(tiered.install_epoch(), before, "queued requests stay fresh");
    assert!(tiered.is_osr_denied(&key));

    first.jit.flush_code_cache();
    assert!(first.jit.code_state_epoch() > before);
    assert_eq!(tiered.install_epoch(), first.jit.code_state_epoch());
    assert!(
        !tiered.is_osr_denied(&key),
        "this VM's own flush expires it"
    );
}

/// A layout-upgrade flush (`flush_code_cache`) retires the compile verdicts
/// that depend on compile-time state and the runtime de-speculation
/// verdicts, as a `clear_all` did while they read the process epoch; a
/// redefinition retires every verdict.
#[test]
fn a_flush_retires_this_vms_flush_dependent_verdicts() {
    let shared = SharedVm::new(VmConfig::default());
    let cid = ClassId::new(0x1404);
    shared
        .jit
        .runtime_despec
        .mark(cid, "l3/Spec", "spec", "()V");
    let bailed = || {
        shared
            .jit
            .runtime_despec
            .is_not_compilable(cid, "l3/Spec", "spec", "()V")
    };
    assert!(bailed());
    shared.jit.flush_code_cache();
    assert!(
        !bailed(),
        "a flush retires a runtime de-speculation verdict"
    );

    let verdicts = shared.jit.tiered_manager.verdicts();
    verdicts.mark_bail_listed(cid, "l3/Bailed", "run", "()V");
    assert!(verdicts.is_bail_listed(cid, "l3/Bailed", "run", "()V"));
    shared.jit.note_class_redefinition();
    assert!(!verdicts.is_bail_listed(cid, "l3/Bailed", "run", "()V"));
}

/// Interpreter round i1 wave 15, lane L4: the registries' flush epoch IS
/// the code-state epoch. A flush advances it once and retires the runtime
/// de-speculation verdict through it (`flush_code_cache` no longer calls
/// the registries); a registry's own flush hook does not move it.
#[test]
fn the_verdict_registries_flush_on_the_code_state_epoch() {
    let shared = SharedVm::new(VmConfig::default());
    let cid = ClassId::new(0x1505);
    let despec = &shared.jit.runtime_despec;
    despec.mark(cid, "l4/Spec", "spec", "()V");
    let before = shared.jit.code_state_epoch();
    despec.note_code_cache_flush();
    shared.jit.tiered_manager.verdicts().note_code_cache_flush();
    assert_eq!(shared.jit.code_state_epoch(), before, "owned by the realm");
    assert!(despec.is_not_compilable(cid, "l4/Spec", "spec", "()V"));

    shared.jit.flush_code_cache();
    assert_eq!(
        shared.jit.code_state_epoch(),
        before + 1,
        "one counter, one move"
    );
    assert!(!despec.is_not_compilable(cid, "l4/Spec", "spec", "()V"));
}

/// Interpreter round i1 wave 17, lane L5
/// (`i15-L4-proposal-one-redefine-epoch-per-vm`): both registries share the
/// realm's redefine epoch. Their own redefinition hooks move nothing; one
/// `note_class_redefinition` advances it once and expires a bytecode-only
/// verdict (the bail list, which a flush alone keeps) in the compile
/// registry and the runtime de-speculation verdict, and is counted by the
/// redefinition census.
#[test]
fn the_verdict_registries_share_the_realms_redefine_epoch() {
    use std::sync::atomic::Ordering;
    let shared = SharedVm::new(VmConfig::default());
    let cid = ClassId::new(0x1705);
    let verdicts = shared.jit.tiered_manager.verdicts();
    let despec = &shared.jit.runtime_despec;
    verdicts.mark_bail_listed(cid, "l5/Bailed", "run", "()V");
    despec.mark(cid, "l5/Spec", "spec", "()V");
    let before = shared.jit.redefine_epoch.load(Ordering::Acquire);
    let census_before = verdicts.redefinition_census();

    verdicts.note_redefinition();
    despec.note_redefinition();
    assert_eq!(
        shared.jit.redefine_epoch.load(Ordering::Acquire),
        before,
        "owned by the realm"
    );
    shared.jit.flush_code_cache();
    assert!(
        verdicts.is_bail_listed(cid, "l5/Bailed", "run", "()V"),
        "a flush keeps a bytecode-only verdict"
    );

    shared
        .jit
        .note_class_redefinition_of(cid, "l5/Bailed", false);
    assert_eq!(
        shared.jit.redefine_epoch.load(Ordering::Acquire),
        before + 1,
        "one counter, one move"
    );
    assert!(!verdicts.is_bail_listed(cid, "l5/Bailed", "run", "()V"));
    assert!(!despec.is_not_compilable(cid, "l5/Spec", "spec", "()V"));
    let census = verdicts.redefinition_census();
    assert_eq!(census.0, census_before.0 + 1, "one redefinition counted");
}

/// Interpreter round i1 wave 19, lane L5
/// (`interpreter-L5-c2-supersede-epoch-is-process-wide-FIXED`): the supersede epoch is
/// per VM. A C2 publish in one VM (the worker's `bump_supersede_epoch`)
/// stales that VM's `Jit` invoke-cache gates and leaves another VM's
/// current.
#[test]
fn a_supersede_in_one_vm_leaves_another_vms_jit_entries_current() {
    let publisher = SharedVm::new(VmConfig::default());
    let bystander = SharedVm::new(VmConfig::default());
    let (in_publisher, in_bystander) = (
        publisher.jit.supersede_gate(),
        bystander.jit.supersede_gate(),
    );
    let bystander_before = bystander.jit.supersede_epoch();

    publisher.jit.bump_supersede_epoch();

    assert!(in_publisher.is_stale(), "the publishing VM re-resolves");
    assert!(
        !in_bystander.is_stale(),
        "another VM's entries cannot hold the replaced body"
    );
    assert_eq!(bystander.jit.supersede_epoch(), bystander_before);
    assert!(
        !publisher.jit.supersede_gate().is_stale(),
        "a gate taken after the publish is current"
    );
}

/// Interpreter round i1 wave 17, lane L5: the scope the JIT invalidate
/// hook's adapter reads to leave an in-place redefinition's flush to
/// `redefine_class_with` names exactly the class being redefined, on this
/// thread only, and restores the outer value when it ends.
#[test]
fn the_in_place_redefinition_scope_names_one_class_on_one_thread() {
    use super::{in_place_redefinition_of, InPlaceRedefinitionScope};
    let (redefined, other) = (ClassId::new(0x1706), ClassId::new(0x1707));
    assert!(!in_place_redefinition_of(redefined));
    {
        let _outer = InPlaceRedefinitionScope::enter(redefined);
        assert!(in_place_redefinition_of(redefined));
        assert!(
            !in_place_redefinition_of(other),
            "only the class being redefined"
        );
        let seen_elsewhere = std::thread::spawn(move || in_place_redefinition_of(redefined))
            .join()
            .expect("the probe thread runs");
        assert!(!seen_elsewhere, "only on the redefining thread");
        {
            let _inner = InPlaceRedefinitionScope::enter(other);
            assert!(in_place_redefinition_of(other));
        }
        assert!(
            in_place_redefinition_of(redefined),
            "the outer scope is restored"
        );
    }
    assert!(!in_place_redefinition_of(redefined));
}

/// A one-`RET` body, publishable under any key.
fn i20_l1_body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    cratonvm_jit::CompiledMethod::new(buf)
}

/// Publish a body for `(id, class, method, "()V")` and mark the method
/// compiled at C1 in the tiered manager, as a compile door would.
fn i20_l1_publish(shared: &SharedVm, id: ClassId, class: &str, method: &str) -> MethodKey {
    use crate::jit::tiered::CompilationTier;
    use std::sync::Arc;
    let key = MethodKey::with_class_id(id, class, method, "()V");
    let tiered = &shared.jit.tiered_manager;
    tiered.on_method_invocation(&key);
    tiered.compilation_complete(&key, CompilationTier::C1, 1);
    shared.jit.jit_cache.put(
        Arc::from(class),
        Arc::from(method),
        Arc::from("()V"),
        id,
        i20_l1_body(),
    );
    key
}

/// Interpreter round i1 wave 20, lane L1 (stage 2b of
/// `i14-L3-proposal-class-scoped-redefinition-invalidation`): a
/// redefinition of a class nothing copied withdraws the class's own body
/// and keeps an unrelated one published and at its tier; the withdrawn
/// method goes back to the interpreter tier so it can be compiled again.
#[test]
fn a_scoped_redefinition_keeps_unrelated_bodies_and_demotes_what_it_withdrew() {
    use crate::jit::tiered::CompilationTier;
    if !super::SCOPED_REDEFINITION_EVICTION_ENABLED {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let (redefined, other) = (ClassId::new(0x2041), ClassId::new(0x2042));
    let own = i20_l1_publish(&shared, redefined, "i20/l1/Redefined", "own");
    let kept = i20_l1_publish(&shared, other, "i20/l1/Other", "kept");
    assert!(shared
        .jit
        .scoped_redefinition_admitted(redefined, "i20/l1/Redefined", false));
    let before = shared.jit.code_state_epoch();

    let evicted = shared
        .jit
        .note_class_redefinition_of(redefined, "i20/l1/Redefined", false);

    assert_eq!(evicted, 1, "only the redefined class's body");
    let cache = &shared.jit.jit_cache;
    assert!(cache.get("i20/l1/Other", "kept", "()V", other).is_some());
    assert!(cache
        .get("i20/l1/Redefined", "own", "()V", redefined)
        .is_none());
    let tiered = &shared.jit.tiered_manager;
    assert_eq!(tiered.current_tier(&own), CompilationTier::Interpreter);
    assert_eq!(tiered.current_tier(&kept), CompilationTier::C1);
    assert!(
        shared.jit.code_state_epoch() > before,
        "queued requests still drop (stage 3 would scope this)"
    );
}

/// Round 12 wave 4 (lane withdraw, tier proposal W3-1): a VM's cache is
/// armed with its tiered manager, so a door with no demotion of its own
/// (here the class-hierarchy change `define_class` runs) still sends the
/// method it withdrew back to the interpreter tier.
#[test]
fn a_class_hierarchy_withdrawal_demotes_through_the_vms_own_cache() {
    use crate::jit::tiered::CompilationTier;
    use std::sync::Arc;
    let shared = SharedVm::new(VmConfig::default());
    let jit = &shared.jit;
    if !jit.jit_cache.reports_withdrawals() {
        // `CRATONVM_JIT_WITHDRAWAL_DEMOTES=0` in this process.
        return;
    }
    let id = ClassId::new(0x1204);
    let key = MethodKey::with_class_id(id, "w4/Caller", "hot", "()V");
    jit.tiered_manager.on_method_invocation(&key);
    jit.tiered_manager
        .compilation_complete(&key, CompilationTier::C2, 1);
    let mut body = i20_l1_body();
    body.inlined_methods = vec![("w4/Shape".into(), "area".into(), "()I".into())];
    jit.jit_cache.put(
        Arc::from("w4/Caller"),
        Arc::from("hot"),
        Arc::from("()V"),
        id,
        body,
    );

    assert_eq!(jit.jit_cache.invalidate_for_class_change("w4/Shape"), 1);
    assert_eq!(
        jit.tiered_manager.current_tier(&key),
        CompilationTier::Interpreter
    );
}

/// The redefinitions a scoped eviction cannot bound keep the full flush:
/// `java/lang/Object`, a class with a registered native, and a class some
/// compile copied bytecode of. The full flush also demotes every method
/// it withdrew, which it did not before wave 20 (such a method stayed at
/// its tier with no body, so the tiered path never offered it again).
#[test]
fn a_redefinition_no_body_records_takes_the_full_flush_and_demotes() {
    use crate::jit::tiered::CompilationTier;
    let shared = SharedVm::new(VmConfig::default());
    let (spliced, other) = (ClassId::new(0x2043), ClassId::new(0x2044));
    let jit = &shared.jit;
    assert!(!jit.scoped_redefinition_admitted(other, "java/lang/Object", false));
    assert!(!jit.scoped_redefinition_admitted(other, "i20/l1/Native", true));
    jit.jit_cache.note_bytecode_copied(spliced);
    assert!(!jit.scoped_redefinition_admitted(spliced, "i20/l1/Spliced", false));

    let unrelated = i20_l1_publish(&shared, other, "i20/l1/Unrelated", "run");
    let evicted = jit.note_class_redefinition_of(spliced, "i20/l1/Spliced", false);
    assert_eq!(evicted, 1, "the full flush withdraws the unrelated body");
    assert!(jit.jit_cache.is_empty());
    assert_eq!(
        jit.tiered_manager.current_tier(&unrelated),
        CompilationTier::Interpreter,
        "and sends its method back to the interpreter tier"
    );
}

/// Round 12 wave 5 (lane tier3): only a define that could have replaced a
/// class withdraws by name. A fresh id (at or above the store's next id
/// before the define) sits beside another loader's same-named class; an
/// id that was already in the store is a stub upgraded in place, and
/// `allow_redefine` re-keys the name in its own loader.
#[test]
fn a_define_withdraws_by_name_only_when_it_can_replace_a_class() {
    use super::define_withdraws_by_name_when as withdraws;
    let before = ClassId::new(0x300);
    let fresh = ClassId::new(0x300);
    let fresh_after_supertypes = ClassId::new(0x305);
    let upgraded_stub = ClassId::new(0x2ff);
    assert!(!withdraws(fresh, before, false, true));
    assert!(!withdraws(fresh_after_supertypes, before, false, true));
    assert!(withdraws(upgraded_stub, before, false, true));
    assert!(withdraws(fresh, before, true, true), "allow_redefine");
    // The kill switch restores the by-name withdrawal for every define.
    assert!(withdraws(fresh, before, false, false));
    assert!(withdraws(upgraded_stub, before, false, false));
}

/// Interpreter round i1 wave 24, lane L6: a whole-cache redefinition
/// withdraws a body neither the cache nor the optimizing OSR memo lists --
/// an optimizing OSR body a frame still runs after the memo forgot it --
/// so the safepoint slow path tells its back-edge poll to leave; a body
/// compiled after the redefinition is not withdrawn. Before wave 24 such a
/// body was never marked, and the only bodies marked were the ones a scan
/// reached.
#[test]
fn a_whole_cache_redefinition_withdraws_a_body_nothing_reaches() {
    use std::sync::Arc;
    let shared = SharedVm::new(VmConfig::default());
    let jit = &shared.jit;
    let spliced = ClassId::new(0x2440);
    let running = Arc::new(i20_l1_body());
    assert!(!jit.jit_cache.body_withdrawn_by_redefinition(&running));
    let before = jit.jit_cache.bodies_withdrawn_by_redefinition();
    jit.jit_cache.note_bytecode_copied(spliced);
    assert!(!jit.scoped_redefinition_admitted(spliced, "i24/l6/Spliced", false));

    let _ = jit.note_class_redefinition_of(spliced, "i24/l6/Spliced", false);

    assert!(
        jit.jit_cache.bodies_withdrawn_by_redefinition() > before,
        "the redefinition path's loop-exit handshake trigger moves"
    );
    assert!(
        !running.is_withdrawn_by_redefinition(),
        "no scan reached it"
    );
    assert!(jit.jit_cache.body_withdrawn_by_redefinition(&running));
    assert!(
        running.is_withdrawn_by_redefinition(),
        "the answer is recorded for the exit sinks, which read the mark"
    );
    let fresh = i20_l1_body();
    assert!(!jit.jit_cache.body_withdrawn_by_redefinition(&fresh));
}
