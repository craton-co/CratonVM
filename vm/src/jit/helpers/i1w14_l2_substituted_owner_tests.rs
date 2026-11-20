use super::{jit_substituted_owner, substituted_owner_differs_from_by_name, JitInvokeInfo};
use cratonvm_types::{ClassId, ClassLoaderId};

const OWNER: &str = "cratonvm/test/i1w14l2/SplitAncestor";
const OTHER: &str = "cratonvm/test/i1w14l2/Unrelated";
const FORK: ClassLoaderId = ClassLoaderId::UserDefined(14_214);

fn site(invoke_kind: u8, owner_class_id: u32) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: OWNER,
        method_name: "length",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind,
        declaring_class_id: 0,
        owner_class_id,
    }
}

/// Two definitions of `OWNER`: the application copy the flat name map
/// answers, and a user loader's copy (built as
/// `a_compiled_invokestatic_resolves_its_owner_in_the_callers_own_loader`
/// builds it), plus one unrelated class.
fn two_copies(shared: &crate::vm::SharedVm) -> (ClassId, ClassId, ClassId) {
    let mut cm = shared.classes.class_manager.write();
    let app_copy = cm
        .try_ensure_synthetic_class(OWNER, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    let fork_copy = cm
        .try_ensure_synthetic_class("cratonvm/test/i1w14l2/SplitAncestor$Fork", 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    cm.class_store
        .get_mut(fork_copy)
        .expect("just fabricated")
        .name = cratonvm_types::intern_arc(OWNER);
    cm.class_store
        .get_mut(fork_copy)
        .expect("just fabricated")
        .loader_id = FORK;
    cm.register_class_name(FORK, OWNER, fork_copy);
    let other = cm
        .try_ensure_synthetic_class(OTHER, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    (app_copy, fork_copy, other)
}

/// The recorded id is the answer, even when the flat name map holds the
/// other copy of the name; nothing else is.
#[test]
fn a_substituted_owner_is_taken_by_its_recorded_id() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let (app_copy, fork_copy, other) = two_copies(&shared);
    assert_ne!(app_copy, fork_copy);
    assert_eq!(
        jit_substituted_owner(&shared, &site(1, fork_copy.as_u32())),
        Some(fork_copy),
        "the id the substitution found, not the by-name copy"
    );
    assert_eq!(
        jit_substituted_owner(&shared, &site(1, app_copy.as_u32())),
        Some(app_copy)
    );
    // No substitution: the site resolves its constant-pool name as before.
    assert_eq!(jit_substituted_owner(&shared, &site(1, 0)), None);
    // Only a statically bound site carries one.
    assert_eq!(
        jit_substituted_owner(&shared, &site(0, fork_copy.as_u32())),
        None
    );
    // An id that no longer names a class called `class_name` is ignored.
    assert_eq!(
        jit_substituted_owner(&shared, &site(1, other.as_u32())),
        None
    );
    assert_eq!(jit_substituted_owner(&shared, &site(1, u32::MAX)), None);
}

/// The by-name compiled shortcuts are declined exactly when the flat name
/// map answers another copy than the recorded owner.
#[test]
fn the_by_name_shortcuts_decline_a_substituted_owners_other_copy() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let (app_copy, fork_copy, _) = two_copies(&shared);
    let cm = shared.classes.class_manager.read();
    assert!(
        matches!(
            cm.classify_loaded_name(OWNER),
            cratonvm_classloading::NameResolution::Ambiguous { .. }
        ),
        "the fixture must produce two definitions of one name"
    );
    let by_name = cm.get_loaded_class_id(OWNER);
    let (same, differs) = if by_name == Some(app_copy) {
        (app_copy, fork_copy)
    } else {
        (fork_copy, app_copy)
    };
    assert!(substituted_owner_differs_from_by_name(
        &cm,
        &site(1, differs.as_u32())
    ));
    assert!(!substituted_owner_differs_from_by_name(
        &cm,
        &site(1, same.as_u32())
    ));
    assert!(!substituted_owner_differs_from_by_name(&cm, &site(1, 0)));
}
