use super::*;
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use cratonvm_native_api::NativeKind;
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;

pub const OWNER: &str = "cratonvm/r21compat/Owner";
pub const WITH_CODE: &str = "r21CompatBody";
pub const NO_DECL: &str = "r21CompatUndeclared";
pub const DESC: &str = "()I";

pub fn never_called(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[cratonvm_types::Value],
) -> Result<Option<cratonvm_types::Value>, cratonvm_types::error::MethodCallFailed> {
    Ok(None)
}

/// `name()I` with the body `iconst_1; ireturn`.
pub fn method_with_code(name: &str) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: std::sync::Arc::from(name),
        descriptor: std::sync::Arc::from(DESC),
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0xac]),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

/// Define `OWNER` (application loader) declaring `WITH_CODE()I` only.
pub fn define_owner(shared: &SharedVm) -> ClassId {
    let loader = ClassLoaderId::Application;
    let mut cm = shared.classes.class_manager.write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: loader,
        name: cratonvm_types::intern_arc(OWNER),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![method_with_code(WITH_CODE)],
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(loader, OWNER, id);
    id
}

pub fn vm_with(kind: NativeKind) -> SharedVm {
    let mut shared = SharedVm::new(crate::config::VmConfig::default());
    for name in [WITH_CODE, NO_DECL] {
        shared
            .natives
            .native_methods
            .register(OWNER, name, DESC, never_called, kind);
    }
    shared
}

/// A `Bridge` in front of loaded concrete bytecode: the tail
/// (`invoke_or_native`, via `registered_native_yields_to_loaded_bytecode`)
/// yields it, so the thin-helper gate must refuse it. The same `Bridge`
/// with no declaration behind it is what the tail runs, so it is admitted,
/// and so is a class that is not loaded at all.
#[test]
pub fn a_bridge_over_loaded_bytecode_is_refused_like_the_tail_yields_it() {
    let shared = vm_with(NativeKind::Bridge);
    // Not loaded yet: the tail admits (no declaration to prefer).
    assert!(!thin_helper_resolver_yields(
        &shared,
        OWNER,
        WITH_CODE,
        DESC,
        NativeKind::Bridge
    ));
    define_owner(&shared);
    assert!(
        thin_helper_resolver_yields(&shared, OWNER, WITH_CODE, DESC, NativeKind::Bridge),
        "resolve_dispatch step 3: concrete bytecode beats a Bridge in both modes"
    );
    assert!(
        crate::vm::registered_native_yields_to_loaded_bytecode(
            &shared,
            OWNER,
            WITH_CODE,
            DESC,
            never_called,
            NativeKind::Bridge,
        ),
        "the tail's own question gives the same answer"
    );
    assert!(
        !thin_helper_resolver_yields(&shared, OWNER, NO_DECL, DESC, NativeKind::Bridge),
        "no declaration: nothing to yield to, the tail runs the Bridge"
    );
    if thin_helper_asks_resolver_enabled(&shared) {
        assert!(
            direct_helper_refusal(&shared, OWNER, WITH_CODE, DESC, None, Some(NativeKind::Bridge)),
            "the gate refuses what the tail yields"
        );
    }
    assert!(
        !direct_helper_refusal(&shared, OWNER, NO_DECL, DESC, None, Some(NativeKind::Bridge)),
        "and still admits the Bridge the tail runs"
    );
}

/// A reviewed `Intrinsic` may shadow concrete bytecode (step 2), so the
/// gate keeps admitting it over loaded bytecode, and the resolver is not
/// even asked outside the opt-in pure-bytecode probe.
#[test]
pub fn a_reviewed_intrinsic_over_loaded_bytecode_is_still_admitted() {
    let shared = vm_with(NativeKind::Intrinsic);
    define_owner(&shared);
    assert!(!thin_helper_resolver_yields(
        &shared,
        OWNER,
        WITH_CODE,
        DESC,
        NativeKind::Intrinsic
    ));
    assert!(!direct_helper_refusal(
        &shared,
        OWNER,
        WITH_CODE,
        DESC,
        None,
        Some(NativeKind::Intrinsic)
    ));
}

/// COMPAT21-2: an unclassified gate memoises under any installed policy;
/// a classified one only with none installed (its answer is the funnel's
/// recording capability check). Before the fix the rule was "no policy
/// installed" for every gate, and `vm_init` always installs one.
#[test]
pub fn the_gate_memo_engages_for_unclassified_gates_under_an_installed_policy() {
    assert!(gate_memo_policy_admits(false, true), "the production case");
    assert!(gate_memo_policy_admits(false, false));
    assert!(gate_memo_policy_admits(true, false));
    assert!(
        !gate_memo_policy_admits(true, true),
        "a classified gate's per-call capability check records, never memoised"
    );
}

/// JIT r23 wave 8 (lane W8-L2, CF-W8-1): the split path for a classified
/// gate asks the capability half on every call, BEFORE any memo read, and
/// memoises only the policy half (`direct_helper_refusal` with no
/// capability question). A source pin, because the order is the whole
/// correctness argument and a VM-free unit test cannot reach the cell.
#[test]
pub fn the_classified_gate_split_asks_the_capability_half_before_the_memo() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = src
        .find("\npub fn classified_gate_refuses_split(")
        .expect("classified_gate_refuses_split must exist");
    let body = &src[start..start + src[start..].find("\n}\n").expect("closed item")];
    let cap = body
        .find("thin_helper_capability_admits(")
        .expect("the per-call capability question");
    let memo = body.find("GATE_DECISION_MEMO").expect("the memo read");
    assert!(cap < memo, "the capability half must run before the memo read");
    assert!(
        body.contains("direct_helper_refusal(vm, class_name, method_name, descriptor, None, kind)"),
        "only the policy half may be memoised"
    );
    assert!(body.contains("thin_helper_answer_settled("));
}

/// COMPAT21-2: the memo key moves with the registry generation (a new
/// triple), so a memoised answer never outlives the registration it read.
#[test]
pub fn the_gate_memo_key_moves_with_the_registry_generation() {
    let mut shared = vm_with(NativeKind::Bridge);
    let before = gate_memo_key(&shared);
    assert_eq!(before.1, shared.jit.jit_cache.redefine_epoch());
    shared.natives.native_methods.register(
        OWNER,
        "r21CompatAnother",
        DESC,
        never_called,
        NativeKind::Bridge,
    );
    assert_ne!(gate_memo_key(&shared).0, before.0);
}

/// COMPAT21-1/2: an answer the resolver term may still change is not
/// memoised. A `Bridge` on a class that is not loaded is unsettled; once
/// the declaration with its decoded `Code` is loaded it is settled. An
/// `Intrinsic` (outside the pure probe) and an unregistered triple never
/// reach the term, so they are settled at once with no class lookup.
#[test]
pub fn only_a_settled_resolver_answer_is_memoised() {
    let shared = vm_with(NativeKind::Bridge);
    if thin_helper_asks_resolver_enabled(&shared) {
        assert!(!thin_helper_answer_settled(
            &shared,
            OWNER,
            WITH_CODE,
            DESC,
            Some(NativeKind::Bridge)
        ));
    }
    define_owner(&shared);
    assert!(thin_helper_answer_settled(
        &shared,
        OWNER,
        WITH_CODE,
        DESC,
        Some(NativeKind::Bridge)
    ));
    assert!(
        thin_helper_answer_settled(&shared, OWNER, NO_DECL, DESC, Some(NativeKind::Bridge)),
        "a loaded class that declares nothing for the triple is final"
    );
    assert!(thin_helper_answer_settled(
        &shared,
        "cratonvm/r21compat/NotLoaded",
        WITH_CODE,
        DESC,
        Some(NativeKind::Intrinsic)
    ));
    assert!(thin_helper_answer_settled(
        &shared,
        "cratonvm/r21compat/NotLoaded",
        WITH_CODE,
        DESC,
        None
    ));
}

/// COMPAT21-3: the compile gates' question, without its opt-in switch. A
/// `Bridge` in front of loaded concrete bytecode yields (the interpreter
/// and the tail run the bytecode, so it is no shadow); the same `Bridge`
/// with no declaration behind it runs; an `Intrinsic` is never yielded.
#[test]
pub fn will_run_asks_the_resolver_for_non_intrinsic_registrations() {
    use crate::runtime::interpreter::registration_yields_to_loaded_bytecode as yields;
    let bridge = vm_with(NativeKind::Bridge);
    define_owner(&bridge);
    assert!(yields(&bridge, OWNER, WITH_CODE, DESC, never_called, NativeKind::Bridge));
    assert!(!yields(&bridge, OWNER, NO_DECL, DESC, never_called, NativeKind::Bridge));
    let intrinsic = vm_with(NativeKind::Intrinsic);
    define_owner(&intrinsic);
    assert!(!yields(
        &intrinsic,
        OWNER,
        WITH_CODE,
        DESC,
        never_called,
        NativeKind::Intrinsic
    ));
    // The switch is default ON since r23 (R23-AB-FLIP); `=0` restores the
    // allow-list answer (a `Bridge` "runs").
    if !crate::runtime::interpreter::will_run_asks_resolver_enabled(&bridge) {
        assert!(crate::runtime::interpreter::registered_native_will_run(
            &bridge, OWNER, WITH_CODE, DESC
        ));
    } else {
        assert!(!crate::runtime::interpreter::registered_native_will_run(
            &bridge, OWNER, WITH_CODE, DESC
        ));
    }
}

/// COMPAT21-4: the force interceptor runs a forced triple's native only
/// for an `Intrinsic`, and the pure-bytecode probe yields even that under
/// `--real-jdk` -- `resolve_native_dispatch_wave1(.., true, true)` arm for
/// arm. The seal arm asks the same.
#[test]
pub fn the_forced_seal_asks_what_the_interceptor_would_run() {
    use crate::runtime::interpreter::{forced_kind_would_run, forced_native_would_run};
    use cratonvm_native_api::unretire::dial_kind_decision;
    for jdk_only in [false, true] {
        for pure in [false, true] {
            for kind in [NativeKind::Bridge, NativeKind::SyntheticStub, NativeKind::Intrinsic] {
                let expected = match kind {
                    NativeKind::Intrinsic => !(jdk_only && pure),
                    _ => false,
                };
                assert_eq!(
                    forced_kind_would_run(jdk_only, kind, pure),
                    expected,
                    "forced_kind_would_run mismatch for jdk_only={jdk_only}, kind={:?}, pure={pure}",
                    kind
                );
                if kind == NativeKind::Intrinsic {
                    assert_eq!(!dial_kind_decision(kind, pure) || !jdk_only, expected);
                }
            }
        }
    }

    let bridge = vm_with(NativeKind::Bridge);
    assert!(!forced_native_would_run(&bridge, OWNER, WITH_CODE, DESC));
    let stub = vm_with(NativeKind::SyntheticStub);
    assert!(!forced_native_would_run(&stub, OWNER, WITH_CODE, DESC));
    let intrinsic = vm_with(NativeKind::Intrinsic);
    assert!(forced_native_would_run(&intrinsic, OWNER, WITH_CODE, DESC));
    assert!(!forced_native_would_run(
        &intrinsic,
        OWNER,
        "r21CompatUnregistered",
        DESC
    ));    }