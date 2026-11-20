use super::{
    jit_hidden_static_owner, jit_substituted_owner, mh_bind_hidden_leaf_info,
    mh_bind_leaf_info, JitInvokeInfo,
};
use cratonvm_native_builtins::lang_invoke::MhCompiledBind;

const SPUN: &str = "cratonvm/test/jr21r4/LambdaForm$MH";
const OTHER: &str = "cratonvm/test/jr21r4/Other";
const DESC: &str = "(Ljava/lang/Object;I)I";

fn site(invoke_kind: u8, owner: u32) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: SPUN,
        method_name: "invoke",
        descriptor: DESC,
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind,
        declaring_class_id: owner,
        owner_class_id: owner,
    }
}

#[test]
fn a_static_site_with_an_owner_id_is_resolved_by_that_id_only() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let (spun, other) = {
        let mut cm = shared.classes.class_manager.write();
        (
            cm.try_ensure_synthetic_class(SPUN, 0)
                .expect("Compatible mode fabricates; this fixture never runs under --real-jdk"),
            cm.try_ensure_synthetic_class(OTHER, 0)
                .expect("Compatible mode fabricates; this fixture never runs under --real-jdk"),
        )
    };
    assert_eq!(
        jit_hidden_static_owner(&shared, &site(3, spun.as_u32())),
        Some(spun)
    );
    assert_eq!(
        jit_substituted_owner(&shared, &site(3, spun.as_u32())),
        Some(spun),
        "every service that asks the substituted owner takes the hidden id"
    );
    // A compiled static site carries no owner id: by name, as before.
    assert_eq!(jit_hidden_static_owner(&shared, &site(3, 0)), None);
    assert_eq!(jit_substituted_owner(&shared, &site(3, 0)), None);
    // A kind-1 substitution is not the hidden leaf, and keeps its answer.
    assert_eq!(
        jit_hidden_static_owner(&shared, &site(1, spun.as_u32())),
        None
    );
    assert_eq!(
        jit_substituted_owner(&shared, &site(1, spun.as_u32())),
        Some(spun)
    );
    // Virtual and interface sites never carry one.
    assert_eq!(
        jit_substituted_owner(&shared, &site(0, spun.as_u32())),
        None
    );
    assert_eq!(
        jit_substituted_owner(&shared, &site(2, spun.as_u32())),
        None
    );
    // An id that names another class (a reused id) is ignored.
    assert_eq!(
        jit_hidden_static_owner(&shared, &site(3, other.as_u32())),
        None
    );
    assert_eq!(jit_hidden_static_owner(&shared, &site(3, u32::MAX)), None);
}

#[test]
fn the_hidden_leaf_is_a_static_call_by_id_interned_apart_from_an_ordinary_leaf() {
    let bind = MhCompiledBind {
        invoke_kind: 3,
        class_id: 4242,
        class: SPUN.to_string(),
        name: "invoke".to_string(),
        desc: DESC.to_string(),
        bound: false,
    };
    let leaf = mh_bind_hidden_leaf_info(0x5eed, &bind, SPUN).expect("interned");
    assert_eq!(leaf.invoke_kind, 3);
    assert_eq!(leaf.owner_class_id, 4242);
    assert_eq!(leaf.declaring_class_id, 4242);
    assert_eq!(
        leaf.num_jit_args, 2,
        "static: one slot per value, no receiver"
    );
    assert_eq!(leaf.return_type, b'I');
    assert_eq!(leaf.class_name, SPUN);
    assert_eq!(leaf.method_name, "invoke");
    assert_eq!(leaf.descriptor, DESC);
    let again = mh_bind_hidden_leaf_info(0x5eed, &bind, SPUN).expect("interned");
    assert!(std::ptr::eq(leaf, again), "one leaf per member");
    // An ordinary leaf for the same bind is another row, without the id.
    let plain = mh_bind_leaf_info(0x5eed, &bind).expect("interned");
    assert!(!std::ptr::eq(leaf, plain));
    assert_eq!(plain.owner_class_id, 0);
}
