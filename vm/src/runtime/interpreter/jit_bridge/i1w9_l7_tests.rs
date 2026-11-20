use super::{
    deny_osr_for_field_site, osr_optimizing_cache_disabled, osr_optimizing_cached,
    osr_optimizing_refusal_key, remember_osr_optimizing_artifact, OsrBuildStamps,
};
use crate::config::VmConfig;
use crate::jit::tiered::MethodKey;
use crate::runtime::frame::{Frame, OSR_MAX_ATTEMPTS};
use crate::runtime::interpreter::{try_osr_with_backoff, OsrBackoffOutcome};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn loop_frame(class_id: u32, method: &str) -> Frame {
    Frame::new(
        ClassId::new(class_id),
        "cratonvm/test/OsrW9Holder".to_string(),
        method.to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        1,
        &[],
    )
}

fn frame_key(frame: &Frame) -> MethodKey {
    MethodKey::with_class_id(
        frame.class_id,
        frame.class_name_arc(),
        frame.method_name_arc(),
        frame.method_descriptor_arc(),
    )
}

/// The background OSR door waits on a held slot but retires the loop
/// header when the manager has given up on the method without denying it
/// (`c2_bailout` here; a spent retry budget is the same answer). It used to
/// re-request every `osr_threshold` back edges for the life of the process
/// (`interpreter-L7-bg-osr-offers-continue-after-the-worker-gives-up-FIXED`).
#[test]
fn a_background_osr_request_the_manager_refuses_retires_the_loop_header() {
    if !crate::runtime::env_cache::bg_compile()
        || !crate::runtime::env_cache::osr_backedge_enabled()
    {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "bg-osr-refused");
    thread.frames.push(loop_frame(0x9a01, "pending"));
    thread.frames.push(loop_frame(0x9a02, "bailed"));
    let bailed = frame_key(&thread.frames[1]);
    let _ = shared.jit.tiered_manager.on_method_invocation(&bailed);
    shared.jit.tiered_manager.on_c2_bailout(&bailed);
    assert!(
        !shared.jit.tiered_manager.is_osr_denied(&bailed),
        "a C2 bailout is not an OSR denial: only the classified request can tell"
    );
    let refused = || {
        shared
            .jit
            .bridge
            .osr_refused_loop_count
            .load(Ordering::Acquire)
    };

    // A fresh method: its request is queued, and the frame waits.
    thread.frames[0].backward_count = u32::MAX;
    let mut idx = 0;
    let out = try_osr_with_backoff(&shared, &mut thread, &mut idx, 0, 0);
    assert!(matches!(out, OsrBackoffOutcome::Skip));
    assert_eq!(
        thread.frames[0].osr_attempts_at(0),
        Some(0),
        "waiting is not a refusal"
    );
    assert_eq!(thread.frames[0].backward_count, 0, "the stride restarts");
    assert_eq!(refused(), 0);

    // A method the manager gave up on: retired for the activation, and the
    // activation charged to the method-wide budget.
    thread.frames[1].backward_count = u32::MAX;
    let mut idx = 1;
    let out = try_osr_with_backoff(&shared, &mut thread, &mut idx, 1, 0);
    assert!(matches!(out, OsrBackoffOutcome::Skip));
    assert_eq!(thread.frames[1].osr_attempts_at(0), Some(OSR_MAX_ATTEMPTS));
    assert!(!thread.frames[1].should_try_osr(0, 1));
    assert_eq!(refused(), 1);
}

/// The nomination's "did THIS credit cross the threshold" subtracts the
/// credit the profile store applied, from the store's own definition.
#[test]
fn the_loop_work_nomination_subtracts_the_stores_own_credit() {
    use crate::jit::profile::{loop_work_credit, ProfileStore, LOOP_WORK_PER_INVOCATION};
    use crate::runtime::interpreter::loop_work_credit_crossed;
    let store = ProfileStore::new();
    let key = cratonvm_jit_api::invoc_key_parts(0x9a03, "scan", "()V");
    let threshold = 10;
    let iterations = LOOP_WORK_PER_INVOCATION * 6;
    let total = store.add_loop_work(key, iterations);
    assert_eq!(total, loop_work_credit(iterations));
    assert!(!loop_work_credit_crossed(total, iterations, threshold));
    let total = store.add_loop_work(key, iterations);
    assert!(
        loop_work_credit_crossed(total, iterations, threshold),
        "6 + 6 carries the counter from below 10 to 12"
    );
    let total = store.add_loop_work(key, iterations);
    assert!(!loop_work_credit_crossed(total, iterations, threshold));
    let src = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    assert!(
        !src.contains("backward_count / 32"),
        "no copy of the jit crate's divisor"
    );
}

/// A field site the OSR body cannot serve is a verdict about the bytecode
/// and the resolved field: it denies OSR for the method, at all seven sites.
#[test]
fn an_osr_field_site_refusal_denies_the_method() {
    let shared = SharedVm::new(VmConfig::default());
    let key = MethodKey::with_class_id(
        ClassId::new(0x9a04),
        "cratonvm/test/OsrFieldSite",
        "spin",
        "()V",
    );
    assert!(!shared.jit.tiered_manager.is_osr_denied(&key));
    deny_osr_for_field_site(&shared, &key, 7, "instance-tag");
    assert!(shared.jit.tiered_manager.is_osr_denied(&key));
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
            .matches("deny_osr_for_field_site(shared, osr_key, pc, ")
            .count(),
        7
    );
    assert!(production.contains(
        "|| osr_gpu_offload_blocks(shared, class_id, method_name, method_descriptor)"
    ));
}

/// An accepted optimizing OSR body is never published through `JitCache`,
/// so the cache's dependency withdrawal cannot reach it: the memo asks the
/// same invalidation log, and forgets a body a later class-hierarchy
/// change withdraws (`i1-L7-osr-optimizing-door-bypasses-osr-admission`,
/// item 4).
#[test]
fn an_optimizing_osr_body_is_forgotten_when_a_class_it_inlined_gains_a_subclass() {
    if osr_optimizing_cache_disabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let class_id = ClassId::new(0x9a05);
    let names = ("cratonvm/test/OsrDepCaller", "spin", "()V");
    let entry_pc = 4usize;
    let key =
        osr_optimizing_refusal_key(shared.vm_identity, class_id, names.1, names.2, entry_pc);
    let buf = cratonvm_jit::ExecutableBuffer::new(64)
        .expect("executable buffer alloc must succeed in tests");
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.inlined_methods.push((
        "cratonvm/test/OsrDepShape".to_string(),
        "area".to_string(),
        "()D".to_string(),
    ));
    cm.ir_osr_entries.push((entry_pc as u32, 0, 0));
    cm.ir_osr_sentinel_free = true;
    // The staleness rule reads this VM's own code-state epoch, which no
    // concurrent test moves: this test is about the dependency rule.
    let artifact = Arc::new(cm);
    let pins: Arc<[cratonvm_jit::RetainedCode]> = Vec::new().into();
    // The bytecode the entering frame runs; served only to the same code.
    let code: Arc<[u8]> = Arc::from(&[0xb1u8][..]);
    let stamps = OsrBuildStamps::read(&shared, class_id);
    remember_osr_optimizing_artifact(&shared, key, &artifact, &pins, stamps, &code, false);
    let cached = |shared: &SharedVm| {
        osr_optimizing_cached(shared, class_id, names, key, entry_pc, &code).is_some()
    };
    assert!(cached(&shared));
    assert_eq!(
        shared
            .jit
            .jit_cache
            .invalidate_for_class_change("cratonvm/test/OsrDepUnrelated"),
        0
    );
    assert!(cached(&shared), "a define it does not depend on");
    // Nothing PUBLISHED names the class, so the cache's own scan withdraws
    // nothing; only the log reaches the memo's body.
    assert_eq!(
        shared
            .jit
            .jit_cache
            .invalidate_for_class_change("cratonvm/test/OsrDepShape"),
        0
    );
    assert!(
        !cached(&shared),
        "a class it inlined from gained a subclass"
    );
    let still_held = shared
        .jit
        .bridge
        .osr_optimizing_accepted
        .lock()
        .map(|m| m.contains_key(&key))
        .unwrap_or(true);
    assert!(!still_held, "the withdrawn body is forgotten, not kept");
}
