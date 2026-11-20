use super::try_jit_compile_callee_slow;
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_types::{ClassId, ClassLoaderId};
use std::sync::Arc;

const SPLIT: &str = "cratonvm/test/SplitCalleeHolder";

fn bodiless_m() -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from("m"),
        descriptor: Arc::from("()V"),
        access_flags: MethodAccessFlags::PUBLIC,
        attributes: vec![],
    }
}

/// Two definitions of `SPLIT` — the application loader's and a forked
/// loader's — each declaring a body-less `m()V`, built the way
/// `helpers::tests::a_compiled_invokestatic_resolves_its_owner_in_the_callers_own_loader`
/// builds its two copies (`ensure_synthetic_class` dedupes by name, so the
/// second is fabricated under its own name and renamed). Returns
/// `(app_copy, fork_copy, other)`, `other` being a class of another name.
fn two_copies(shared: &SharedVm) -> (ClassId, ClassId, ClassId) {
    const FORK: ClassLoaderId = ClassLoaderId::UserDefined(4343);
    let mut cm = shared.classes.class_manager.write();
    let app_copy = cm
        .try_ensure_synthetic_class(SPLIT, 0)
        .expect("Compatible mode fabricates");
    cm.class_store
        .get_mut(app_copy)
        .expect("fabricated")
        .methods
        .push(bodiless_m());
    let fork_copy = cm
        .try_ensure_synthetic_class("cratonvm/test/SplitCalleeHolder$Fork", 0)
        .expect("Compatible mode fabricates");
    {
        let class = cm.class_store.get_mut(fork_copy).expect("fabricated");
        class.name = cratonvm_types::intern_arc(SPLIT);
        class.loader_id = FORK;
        class.methods.push(bodiless_m());
    }
    cm.register_class_name(FORK, SPLIT, fork_copy);
    let other = cm
        .try_ensure_synthetic_class("cratonvm/test/SplitCalleeOther", 0)
        .expect("Compatible mode fabricates");
    (app_copy, fork_copy, other)
}

/// The callee door refuses a class name two loaders define when it is
/// asked by name, and resolves exactly the named copy when it is given
/// that copy's id — the id the MIC/PIC and virtual dispatch-helper doors
/// now pass (the receiver's class) — reaching the method itself, whose
/// missing `Code` attribute is then the (policy) refusal.
#[test]
fn a_class_name_two_loaders_define_compiles_for_the_named_copy() {
    let shared = SharedVm::new(VmConfig::default());
    let (app_copy, fork_copy, other) = two_copies(&shared);
    assert_ne!(app_copy, fork_copy);
    assert!(
        matches!(
            shared
                .classes
                .class_manager
                .read()
                .classify_loaded_name(SPLIT),
            cratonvm_classloading::NameResolution::Ambiguous { .. }
        ),
        "the fixture must define the name twice, or this measures nothing"
    );
    let probe = |hint: Option<ClassId>| -> (bool, bool) {
        let (mut cache_negative, mut policy_declined) = (true, false);
        let compiled = try_jit_compile_callee_slow(
            &shared,
            hint,
            SPLIT,
            "m",
            "()V",
            false,
            &mut cache_negative,
            &mut policy_declined,
            false,
            false,
        );
        assert!(compiled.is_none(), "the method has no body");
        (cache_negative, policy_declined)
    };
    // By name: ambiguous, refused before any method is looked at, and
    // cacheable while both loaders stay loaded.
    assert_eq!(probe(None), (true, false));
    // A hint naming another class is not honoured: the same refusal.
    assert_eq!(probe(Some(other)), (true, false));
    // Either copy's own id resolves that copy.
    assert_eq!(probe(Some(fork_copy)), (true, true));
    assert_eq!(probe(Some(app_copy)), (true, true));
}

/// The receiver-class doors in `jit_invoke_virtual_mic` compile for the
/// receiver's class id and are no longer gated on the receiver's name being
/// globally unique. A text pin: the helper cannot be driven without
/// compiled code.
#[test]
fn the_mic_doors_compile_for_the_receivers_own_class() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let wrapper = src
        .find("\npub unsafe fn jit_invoke_virtual_mic_body(")
        .expect("the MIC wrapper exists");
    let start = src[wrapper..]
        .find("\nunsafe fn jit_invoke_virtual_mic_body_inner(")
        .map(|offset| wrapper + offset)
        .expect("the MIC body exists");
    assert!(src[wrapper..start].contains("jit_invoke_virtual_mic_body_inner("));
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)];
    assert_eq!(
        body.matches("try_jit_compile_callee_for_class(").count(),
        2,
        "both compile probes name the receiver's class"
    );
    assert!(!body.contains("try_jit_compile_callee("));
    assert!(!body.contains("!globally_named") && !body.contains("&& globally_named"));
}

/// `execute`'s first-call door enters a compiled body without checking or
/// running its static-field owners' initializers: compiled static accesses
/// initialize at the bytecode (`i1-L3-compiled-entry-initializes-static-owners-eagerly`).
#[test]
fn the_first_call_door_has_no_static_owner_check() {
    let src = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let start = src.find("\npub fn execute(").expect("execute exists") + 1;
    let rest = &src[start..];
    let execute = &rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)];
    assert!(!execute.contains("compiled.static_init_classes"));
    assert!(!execute.contains("static_inits_done"));
}

/// Both optimizing-OSR sites — the memo read and a fresh build — refuse a
/// body compiled before the last move of THIS VM's code-state epoch
/// (interpreter round i1 wave 14: the process install epoch before).
#[test]
fn the_optimizing_osr_memo_refuses_bodies_older_than_the_install_epoch() {
    let src = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    let production = src.as_str();
    assert!(production
        .contains("if osr_optimizing_artifact_is_stale(shared, entry.code_state_epoch) \x7b"));
    assert!(production
        .contains("if osr_optimizing_artifact_is_stale(shared, stamps.code_state_epoch) \x7b"));
    assert!(production.contains("stamp < shared.jit.code_state_epoch()"));
}
