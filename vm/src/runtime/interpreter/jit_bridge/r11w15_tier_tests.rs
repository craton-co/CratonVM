use super::{
    forget_osr_refused_loop_at, note_osr_refused_activation_at, osr_drain_oom_singleton,
    osr_loop_offer_allowed_at, OSR_REFUSED_ACTIVATIONS_BUDGET,
};
use crate::config::VmConfig;
use crate::error::{MethodCallFailed, RuntimeError, VmError};
use crate::types::ObjectRef;
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

/// The body of the first `fn` definition `sig` names, up to its closing
/// brace, with the tests below this module cut off first.
fn production_fn(sig: &str) -> String {
    let whole = concat!(
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
    let src = &whole;
    let start = src.find(sig);
    assert!(start.is_some(), "`{sig}` not found");
    let start = start.unwrap_or(0);
    let body = &src[start..];
    body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)].to_string()
}

/// A caller-held synchronized CALL has no callee-handler service on either
/// tier, so its target lookup must refuse EVERY callee that declares an
/// exception table, whatever the plain direct-bind switch
/// (`direct_call_exc_table_publish_enabled`) admits. The switch in that
/// lookup was the root cause of `R11TierSyncDoorCompiled`'s escaped
/// `ArithmeticException` under the single-pass OSR door. A text pin: the
/// lookup needs a loaded class and a published body.
#[test]
fn the_sync_direct_target_refuses_every_callee_with_an_exception_table() {
    let body = production_fn("fn sync_direct_target(");
    let flat: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    assert!(
        flat.contains("if!code_attr.exception_table.is_empty(){returnNone;}"),
        "the refusal must be unconditional"
    );
    assert!(
        !flat.contains("direct_call_exc_table_publish_enabled()"),
        "the plain direct-bind switch must not admit a table callee here"
    );
}

/// Round 11 wave 17 (lane tier): a synchronized callee with no published
/// body is compiled at the lookup instead of leaving the caller's site on
/// the dispatch helper forever (the residual bimodal `sync-method-1t`),
/// AFTER every refusal of the lookup (the compile arm is reached only when
/// the body is all that is missing), and under the eager callee chain's
/// three bounds and kill switch. A text pin: the race it closes needs a
/// background worker and a mutator OSR compile.
#[test]
fn an_unpublished_sync_callee_is_compiled_under_the_eager_chain_bounds() {
    let target = production_fn("fn sync_direct_target(");
    let flat: String = target
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let table = flat.find("if!code_attr.exception_table.is_empty(){returnNone;}");
    assert!(table.is_some(), "the table refusal");
    let table = table.unwrap_or(0);
    let compile = flat.find("None=>sync_direct_compile_unpublished_callee(");
    assert!(compile.is_some(), "the cache miss compiles");
    let compile = compile.unwrap_or(0);
    assert!(
        table < compile,
        "every refusal comes before the compile arm"
    );
    let helper = production_fn("fn sync_direct_compile_unpublished_callee(");
    for bound in [
        "jit_eager_callee_chain()",
        "jit_active_compile_depth() > MAX_EAGER_CALLEE_CHAIN_DEPTH",
        "jit_active_compile_contains(callee_class, callee_method, callee_desc)",
        "eager_callee_chain_try_spend()",
        "try_jit_compile_wrapped_entry(",
    ] {
        assert!(helper.contains(bound), "missing `{bound}`");
    }
}

/// The single-pass OSR door's route is live again, and still behind the
/// shared kill switch its gate reads.
#[test]
fn the_osr_door_plans_the_sync_direct_route_through_the_shared_gate() {
    assert!(super::osr_door_sync_direct_enabled());
    let body = production_fn("fn compile_osr_body(");
    assert!(body.contains("single_pass_sync_direct_site_admitted("));
    assert!(body.contains("sync_direct_target(shared, class_id, &osr_cp, cp_idx)"));
}

/// An entry that ran forgets the loop header's refused activations, so a
/// transient refusal never accumulates towards the budget across
/// activations that entered; a header that never enters still spends it.
#[test]
fn an_entered_loop_forgets_its_refused_activations() {
    let shared = SharedVm::new(VmConfig::default());
    let bridge = &shared.jit.bridge;
    let (key, other) = ((9, 0xbeef, 3), (9, 0xbeef, 30));
    let epoch = 11;
    for _ in 1..OSR_REFUSED_ACTIVATIONS_BUDGET {
        note_osr_refused_activation_at(bridge, key, epoch);
    }
    note_osr_refused_activation_at(bridge, other, epoch);
    assert_eq!(bridge.osr_refused_loop_count.load(Ordering::Acquire), 2);
    forget_osr_refused_loop_at(bridge, key);
    assert_eq!(bridge.osr_refused_loop_count.load(Ordering::Acquire), 1);
    // One refusal after the entry starts the count again from one.
    assert_eq!(note_osr_refused_activation_at(bridge, key, epoch), 1);
    assert!(osr_loop_offer_allowed_at(bridge, key, epoch));
    // Forgetting a header with no record changes nothing.
    forget_osr_refused_loop_at(bridge, (9, 0xbeef, 77));
    assert_eq!(bridge.osr_refused_loop_count.load(Ordering::Acquire), 2);
    // The other header was never entered: its record stands.
    assert!(bridge
        .osr_refused_loops
        .lock()
        .is_ok_and(|m| m.get(&other).map(|r| r.refused_activations) == Some(1)));
}

/// Heap exhaustion while the OSR drain builds an AIOOBE becomes the
/// preallocated `OutOfMemoryError` (the drain then routes it as a throw);
/// every other failure, and an OOM before the singleton exists, is handed
/// back unchanged.
#[test]
fn the_osr_aioobe_drain_maps_only_heap_exhaustion_to_the_oom_singleton() {
    let shared = SharedVm::new(VmConfig::default());
    let oom = || {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: "Java heap space".to_string(),
        }))
    };
    assert!(
        osr_drain_oom_singleton(&shared, oom()).is_err(),
        "no singleton yet (boot): the failure stands"
    );
    let mut word = [0u64; 2];
    // SAFETY: non-null and 8-aligned; never dereferenced -- the helper
    // only hands the address back. Cleared again before the VM drops.
    let fake = unsafe { ObjectRef::from_raw(word.as_mut_ptr() as *mut u8) };
    *shared.mem.singleton_oom.write() = Some(fake);
    let mapped = osr_drain_oom_singleton(&shared, oom());
    let other = osr_drain_oom_singleton(
        &shared,
        MethodCallFailed::InternalError(VmError::Internal {
            message: "failed to load exception class".to_string(),
        }),
    );
    *shared.mem.singleton_oom.write() = None;
    assert!(mapped.is_ok_and(|exc| exc == fake));
    assert!(other.is_err(), "a class-load failure is not an OOM");
}

/// gcf f2/o: a VM-limit refusal maps to its own preallocated error when one
/// was built (`CRATONVM_GC_PREALLOCATED_OOME_KINDS`), and a site-detailed
/// heap message to the singleton.
#[test]
fn gcf_f2o_the_osr_drain_maps_each_message_to_its_preallocated_error() {
    use crate::vm::realms::heap_realm::OomeKind;
    let shared = SharedVm::new(VmConfig::default());
    let oom = |m: &str| {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: m.to_string(),
        }))
    };
    let mut words = [0u64; 4];
    // SAFETY: non-null and 8-aligned; never dereferenced. Cleared again
    // before the VM drops.
    let singleton = unsafe { ObjectRef::from_raw(words.as_mut_ptr() as *mut u8) };
    let limit = unsafe { ObjectRef::from_raw(words.as_mut_ptr().add(2) as *mut u8) };
    *shared.mem.singleton_oom.write() = Some(singleton);
    shared
        .mem
        .preallocated_oome
        .write()
        .set(OomeKind::ArraySize, limit);
    let a = osr_drain_oom_singleton(&shared, oom(OomeKind::ArraySize.message()));
    let h = osr_drain_oom_singleton(&shared, oom("Java heap space (alloc_array length 8)"));
    *shared.mem.singleton_oom.write() = None;
    *shared.mem.preallocated_oome.write() = Default::default();
    assert!(a.is_ok_and(|exc| exc == limit));
    assert!(h.is_ok_and(|exc| exc == singleton));
}
