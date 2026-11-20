use super::{osr_static_gate_refusal, try_jit_compile_wrapped_entry};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_types::ClassId;
use std::sync::Arc;

/// A synthetic class `name` declaring one body-less method `m` with
/// `flags`.
fn holder_with(shared: &SharedVm, name: &str, m: &str, flags: MethodAccessFlags) -> ClassId {
    let mut cm = shared.classes.class_manager.write();
    let holder = cm
        .try_ensure_synthetic_class(name, 0)
        .expect("Compatible mode fabricates");
    cm.class_store
        .get_mut(holder)
        .expect("fabricated")
        .methods
        .push(ClassFileMethod {
            name: Arc::from(m),
            descriptor: Arc::from("()V"),
            access_flags: flags,
            attributes: vec![],
        });
    holder
}

/// A method with no body is a standing policy verdict: the background
/// door reports it as a decline, so it does not spend the method's three
/// compile retries. A class that is merely not loaded yet is not one.
#[test]
fn a_bodiless_method_is_a_policy_decline_and_an_absent_class_is_not() {
    if crate::runtime::env_cache::disable_jit() {
        return; // the door refuses before asking anything
    }
    let shared = SharedVm::new(VmConfig::default());
    let name = "cratonvm/test/PolicyDeclineHolder";
    let holder = holder_with(&shared, name, "bodiless", MethodAccessFlags::PUBLIC);
    let mut policy = false;
    assert!(try_jit_compile_wrapped_entry(
        &shared,
        holder,
        name,
        "bodiless",
        "()V",
        false,
        &mut policy
    )
    .is_none());
    assert!(policy, "no Code attribute is a policy decline");
    let mut policy = false;
    assert!(try_jit_compile_wrapped_entry(
        &shared,
        ClassId::new(0),
        "cratonvm/test/NotLoadedAnywhere",
        "m",
        "()V",
        false,
        &mut policy
    )
    .is_none());
    assert!(
        !policy,
        "a class that is not loaded yet may be on the next attempt"
    );
}

/// Wave 10 (patch P1): `ACC_SYNCHRONIZED` is not an OSR gate for either
/// tier -- the OSR'd interpreter frame owns the method monitor -- so the
/// static gates both routes ask pass a synchronized method, and nothing
/// denies OSR for it on that ground.
#[test]
fn a_synchronized_method_passes_the_static_osr_gates() {
    if crate::runtime::env_cache::disable_jit() {
        // The lever gate refuses every method first.
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let name = "cratonvm/test/OsrSyncHolder";
    let holder = holder_with(&shared, name, "spin", MethodAccessFlags::SYNCHRONIZED);
    if cratonvm_jit::compile_gate::compiled_execution_forbidden(name, "spin") {
        return; // a bisect lever names every method in this run
    }
    let refusal = osr_static_gate_refusal(&shared, holder, name, "spin", "()V");
    assert!(
        refusal.is_none(),
        "refused by `{}`",
        refusal.map_or("", |r| r.census)
    );
    let key = crate::jit::tiered::MethodKey::with_class_id(holder, name, "spin", "()V");
    assert!(!shared.jit.tiered_manager.is_osr_denied(&key));
}

/// The deferred-`new` memo is consumed through ONE helper, which takes the
/// class-manager guard before the memo's lock; no caller may reintroduce a
/// predicate that locks the class manager inside it. A text pin (wave 10:
/// the memo is now this VM's `JitBridgeState::deferred_new_retries`; the
/// process-wide entry point must not come back either).
#[test]
fn the_deferred_new_memo_is_consumed_through_one_lock_ordered_helper() {
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
    assert_eq!(
        production
            .matches("cratonvm_jit::take_deferred_new_retry(")
            .count(),
        0,
        "the process-wide memo is not this VM's"
    );
    assert_eq!(
        production.matches(".take_retry(").count(),
        1,
        "only take_deferred_new_retry_if_resolvable calls the memo"
    );
    let helper = &production[production
        .find("\npub(in crate::runtime::interpreter::jit_bridge) fn take_deferred_new_retry_if_resolvable(")
        .expect("the helper exists")..];
    let helper = &helper[..helper.find("\n\x7d\n").map_or(helper.len(), |e| e + 3)];
    let guard = helper
        .find("let cm = shared.classes.class_manager.read();")
        .expect("the guard is taken in the helper");
    let call = helper
        .find("memo.take_retry(")
        .expect("the memo is called in the helper");
    assert!(guard < call, "the class-manager guard comes first");
}
