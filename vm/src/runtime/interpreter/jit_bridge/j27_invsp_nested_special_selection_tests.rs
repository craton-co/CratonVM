use super::i13_l2_substituted_owner_tests::{define, method};
use super::{classify_invoke_site, SiteBinding};
use crate::classloading::ClassId;
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::constant_pool::ConstantPoolEntry;

const M: &str = "j27invspM";

/// Public classes of one package with real class bytes (a fixture class
/// is lenient by design, `selection::hierarchy_is_real`), linked
/// `(sub, super)` only after the origins are set.
fn hierarchy(
    shared: &SharedVm,
    classes: &[(&str, bool, Option<bool>)],
    links: &[(usize, usize)],
) -> Vec<ClassId> {
    let public = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let ids: Vec<ClassId> = classes
        .iter()
        .map(|&(name, is_abstract, m_abstract)| {
            let flags = if is_abstract {
                public | ClassAccessFlags::ABSTRACT
            } else {
                public
            };
            let abstract_m = MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT;
            let methods = match m_abstract {
                Some(true) => vec![method(M, abstract_m)],
                Some(false) => vec![method(M, MethodAccessFlags::PUBLIC)],
                None => vec![],
            };
            let pool = vec![ConstantPoolEntry::Tombstone];
            define(shared, name, flags, None, methods, pool)
        })
        .collect();
    let mut cm = shared.classes.class_manager.write();
    for &id in &ids {
        cm.class_store.get_mut(id).expect("registered").origin =
            cratonvm_classloading::ClassOrigin::ApplicationClassPath {
                source: std::sync::Arc::from("j27-invsp-test"),
            };
    }
    for &(sub, sup) in links {
        cm.set_superclass(ids[sub], Some(ids[sup]));
    }
    ids
}

fn classify(shared: &SharedVm, caller: ClassId, cp_class: &str) -> SiteBinding {
    let cm = shared.classes.class_manager.read();
    classify_invoke_site(shared, &cm, caller, cp_class, false, M, "()I")
}

/// `C.sup() { invokespecial S.m }` where `abstract S extends G` re-declares
/// `m` abstract: HotSpot throws `AbstractMethodError`, so the body holding
/// the call is not spliced (the lenient walk would answer `G.m`).
#[test]
fn j27_invsp_an_abstract_redeclaration_refuses_the_enclosing_body() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.classes.selection_checks {
        return; // `CRATONVM_SELECTION_CHECKS_EVERY_MODE=0`: lenient everywhere
    }
    let ids = hierarchy(
        &shared,
        &[
            ("cratonvm/j27invsp/G", false, Some(false)),
            ("cratonvm/j27invsp/S", true, Some(true)),
            ("cratonvm/j27invsp/C", false, Some(false)),
        ],
        &[(1, 0), (2, 1)],
    );
    assert_eq!(
        classify(&shared, ids[2], "cratonvm/j27invsp/S"),
        SiteBinding::Refused("invokespecial-selection-error")
    );
}

/// `D.sup() { invokespecial A.m }` (hand-written: javac names the direct
/// superclass) where `D extends B extends A` and `B` overrides `m`: with
/// ACC_SUPER, JVMS 6.5 selects `B.m`, so the row binds `B`, and the nested
/// resolution walks from `B`, not from `A`. Naming the direct superclass
/// itself keeps the constant-pool class.
#[test]
fn j27_invsp_an_ancestor_constant_pool_class_binds_the_selected_owner() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.classes.selection_checks {
        return;
    }
    let ids = hierarchy(
        &shared,
        &[
            ("cratonvm/j27invsp/A", false, Some(false)),
            ("cratonvm/j27invsp/B", false, Some(false)),
            ("cratonvm/j27invsp/D", false, Some(false)),
        ],
        &[(1, 0), (2, 1)],
    );
    match classify(&shared, ids[2], "cratonvm/j27invsp/A") {
        SiteBinding::Owner(name, id) => {
            assert_eq!(&*name, "cratonvm/j27invsp/B");
            assert_eq!(id, ids[1]);
        }
        other => panic!("A.m must not be the spliced body: {other:?}"),
    }
    assert_eq!(
        classify(&shared, ids[2], "cratonvm/j27invsp/B"),
        SiteBinding::ConstantPoolClass
    );
}
