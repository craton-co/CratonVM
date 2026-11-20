use super::{seed_profile_replay_for_new_classes, seed_profile_replay_for_new_classes_with};
use cratonvm_jit::profile_store as ps;
use std::sync::Arc;

const LATER: &str = "cratonvm/test/ReplayDefinedLater";

/// A live lookup standing in for a class that declares `run()I` with 16
/// bytes of code: the synthetic class the fixture can define has none.
fn sixteen_bytes(
    _: &crate::classloading::ClassManager,
    _: u32,
    _: &str,
    _: &str,
) -> ps::LiveCode {
    ps::LiveCode::Len(16)
}

/// A saved profile naming `run()I` of `class`, with one branch at pc 3.
fn saved_profile_of(class: &'static str) -> Vec<u8> {
    let source = crate::jit::profile::ProfileStore::new();
    let saved_key = crate::jit::profile::MethodKey {
        class_id: 7,
        method_name: Arc::from("run"),
        descriptor: Arc::from("()I"),
    };
    source.with_method_profile_mut(&saved_key, |p| {
        p.record_branch(3, true);
        p.record_branch(3, true);
        p.record_branch(3, false);
    });
    let (bytes, _) = ps::serialize(&source, &|id: u32| -> Option<String> {
        (id == 7).then(|| class.to_string())
    });
    bytes
}

/// A replay record whose class does not exist at load is parked, the
/// definition hook leaves it parked while the class is still undefined,
/// and seeds it -- under the class's real id -- once the class is defined.
/// The page's "How to verify" unit test, driven through the VM's own
/// hook rather than the JIT-side door.
#[test]
fn a_record_parked_at_load_is_seeded_when_its_class_is_defined() {
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let store = &shared.jit.profile_store;

    // A saved profile naming one method of `LATER`.
    let bytes = saved_profile_of(LATER);

    // Load time: the class is not defined yet.
    let census = ps::load_into_with_policy(
        store,
        &bytes,
        &|_name: &str| -> Option<u32> { None },
        &|_: u32, _: &str, _: &str| ps::LiveCode::Absent,
        &ps::ReplaySeedPolicy::identity(),
    )
    .expect("the file decodes");
    assert_eq!(census.methods_seeded, 0);
    assert_eq!(census.methods_unresolved_class, 1);
    assert_eq!(ps::replay_pending_census(store).0, 1);

    // A hook visit before the definition seeds nothing and keeps it waiting.
    assert!(seed_profile_replay_for_new_classes(&shared));
    assert_eq!(ps::replay_pending_census(store).0, 1);

    // The program defines the class.
    let defined = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(LATER, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");

    assert!(seed_profile_replay_for_new_classes_with(
        &shared,
        sixteen_bytes
    ));
    let (classes, methods, seeded, _, stale) = ps::replay_pending_census(store);
    assert_eq!((classes, methods, seeded, stale), (0, 0, 1, 0));
    let live_key = crate::jit::profile::MethodKey {
        class_id: defined.as_u32(),
        method_name: Arc::from("run"),
        descriptor: Arc::from("()I"),
    };
    let profile = store
        .get_profile(&live_key)
        .expect("seeded under the defined class's id");
    assert!(!profile.branches.is_empty());

    // A later visit has nothing new to do and must not re-seed.
    assert!(seed_profile_replay_for_new_classes(&shared));
    assert_eq!(ps::replay_pending_census(store).2, 1);
}

/// r11-tier-profile-replay-residuals item 4, through the VM's own lookup:
/// the defined class declares no `run()I` with bytecode, so the record is
/// stale and dropped -- not seeded, and not left waiting.
#[test]
fn a_record_for_a_method_the_defined_class_lacks_is_dropped_as_stale() {
    const LACKS: &str = "cratonvm/test/ReplayDefinedWithoutTheMethod";
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let store = &shared.jit.profile_store;
    let bytes = saved_profile_of(LACKS);
    ps::load_into_with_policy(
        store,
        &bytes,
        &|_name: &str| -> Option<u32> { None },
        &|_: u32, _: &str, _: &str| ps::LiveCode::Absent,
        &ps::ReplaySeedPolicy::identity(),
    )
    .expect("the file decodes");
    let defined = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(LACKS, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    assert!(seed_profile_replay_for_new_classes(&shared));
    let (classes, methods, seeded, _, stale) = ps::replay_pending_census(store);
    assert_eq!((classes, methods, seeded, stale), (0, 0, 0, 1));
    let live_key = crate::jit::profile::MethodKey {
        class_id: defined.as_u32(),
        method_name: Arc::from("run"),
        descriptor: Arc::from("()I"),
    };
    assert!(store.get_profile(&live_key).is_none());
}
