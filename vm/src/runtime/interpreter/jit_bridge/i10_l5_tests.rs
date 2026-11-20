use super::{
    osr_optimizing_cache_disabled, osr_optimizing_refusal_key,
    remember_osr_optimizing_artifact, OsrBuildStamps,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::Arc;

/// Item 4, narrowed: once the VM's code-state epoch has moved, the next
/// insert purges every entry compiled before it -- including a loop's that
/// is never looked up again -- and keeps the current ones.
#[test]
fn an_insert_after_an_install_epoch_move_purges_the_stale_optimizing_bodies() {
    if osr_optimizing_cache_disabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let class_id = ClassId::new(0x9a10);
    let code: Arc<[u8]> = Arc::from(&[0xb1u8][..]);
    let pins: Arc<[cratonvm_jit::RetainedCode]> = Vec::new().into();
    let body = || {
        let buf = cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests");
        Arc::new(cratonvm_jit::CompiledMethod::new(buf))
    };
    let stale_key = osr_optimizing_refusal_key(shared.vm_identity, class_id, "a", "()V", 4);
    let fresh_key = osr_optimizing_refusal_key(shared.vm_identity, class_id, "b", "()V", 4);
    // Stamped before the flush below.
    let before = OsrBuildStamps::read(&shared, class_id);
    remember_osr_optimizing_artifact(&shared, stale_key, &body(), &pins, before, &code, false);
    let has = |key| {
        shared
            .jit
            .bridge
            .osr_optimizing_accepted
            .lock()
            .map(|m| m.contains_key(&key))
            .unwrap_or(false)
    };
    assert!(has(stale_key), "the purge runs before the insert");
    // A code-cache flush of THIS VM moves its code-state epoch.
    shared.jit.flush_code_cache();
    let after = OsrBuildStamps::read(&shared, class_id);
    assert!(after.code_state_epoch > before.code_state_epoch);
    remember_osr_optimizing_artifact(&shared, fresh_key, &body(), &pins, after, &code, false);
    assert!(
        !has(stale_key),
        "a body older than the code-state epoch is purged"
    );
    assert!(has(fresh_key), "a current body is kept");
}

/// `i1-L7-process-global-jit-bridge-state-not-vm-scoped`, the deferred-`new`
/// memo: a retry armed in one VM is neither held by nor granted to another
/// VM in the process, and the arming VM gets it back with the declaring
/// class the recorder named.
#[test]
fn a_deferred_new_retry_armed_in_one_vm_is_not_another_vms() {
    let a = SharedVm::new(VmConfig::default());
    let b = SharedVm::new(VmConfig::default());
    let (c, m, d) = ("cratonvm/test/DeferredNewHolder", "make", "()V");
    let recorded = ClassId::new(0x9a11);
    a.jit
        .bridge
        .deferred_new_retries
        .note_bail(recorded, c, m, d, &[]);
    assert_eq!(b.jit.bridge.deferred_new_retries.held_count(), 0);
    assert!(!b
        .jit
        .bridge
        .deferred_new_retries
        .take_retry(c, m, d, &|_, _| true));
    let held = a.jit.bridge.deferred_new_retries.held_methods();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].0, recorded);
    assert!(a
        .jit
        .bridge
        .deferred_new_retries
        .take_retry(c, m, d, &|_, _| true));
    assert_eq!(a.jit.bridge.deferred_new_retries.held_count(), 0);
}

/// Item 3: `try_osr` asks the optimizing entry's slot kinds before it
/// enters that body. A text pin: a `CompiledMethod` with an entry stub and
/// a live interpreter frame to offer it are not buildable here; the kind
/// rule itself is tested in the jit crate
/// (`an_osr_entry_publishes_the_kind_of_every_local_it_seeds`).
#[test]
fn try_osr_checks_the_optimizing_entrys_slot_kinds_before_entering() {
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
    let body = &production[production
        .find("\npub(in crate::runtime::interpreter) fn try_osr(")
        .expect("try_osr exists")..];
    let check = body
        .find(".ir_osr_entry_kind_mismatch(")
        .expect("try_osr asks the slot kinds");
    let enter = body
        .find("compiled.ir_osr_enter(")
        .expect("try_osr enters the optimizing stub");
    assert!(
        check < enter,
        "the kinds are asked before the stub is entered"
    );
}
