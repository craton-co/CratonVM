use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::vm::SharedVm;

fn fixture_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle)
        .unwrap_or_else(|| panic!("missing: {needle}"))
}

/// Nothing queued: no hint. A queued finalizer, or a queued cleaner
/// action, raises it; `=0` raises nothing.
/// The hint is raised after the remark's queues are filled and handed off,
/// in the one round both remarks share.
#[test]
fn gci_i42rp_the_remark_round_raises_the_hint_after_it_queues() {
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let rows = body(&src, "fn remark_reference_rows(");
    let finalize = pos(
        rows,
        "shared.mem.finalizer_thread.enqueue_unfinalized(obj_addr);",
    );
    let cleaners = pos(
        rows,
        "shared.mem.cleaner_thread.submit_action(*action_addr);",
    );
    let hand_off = pos(
        rows,
        "gc_enqueued_queues_hand_off(shared, &gc_enqueued_queues);",
    );
    let hint = pos(rows, "let _ = remark_note_ref_work_queued(shared);");
    assert!(finalize < cleaners && cleaners < hand_off && hand_off < hint);
}

/// An unwind out of the delivery thread retires it: the claim is handed
/// back and its id unpublished. A normal drop (the VM is gone, or no
/// panic) changes nothing.
#[test]
fn gci_i42rp_an_unwinding_delivery_thread_is_retired() {
    let shared = std::sync::Arc::new(fixture_vm());
    assert!(shared.mem.finalizer_thread.claim_delivery_thread_start());
    shared.mem.finalizer_thread.set_delivery_thread(41);
    drop(DeliveryThreadUnwind {
        weak: std::sync::Arc::downgrade(&shared),
        tid: crate::ThreadId(41),
    });
    assert_eq!(shared.mem.finalizer_thread.delivery_thread(), Some(41));
    assert!(shared.mem.finalizer_thread.delivery_thread_claimed());
    let weak = std::sync::Arc::downgrade(&shared);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _unwind = DeliveryThreadUnwind {
            weak,
            tid: crate::ThreadId(41),
        };
        panic!("gci i42/rp: a delivery batch unwinds");
    }));
    assert!(unwound.is_err());
    assert_eq!(shared.mem.finalizer_thread.delivery_thread(), None);
    assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
}

/// The guard is installed before the thread publishes itself, so an
/// unwind anywhere in its life retires it.
#[test]
fn gci_i42rp_the_delivery_thread_installs_its_unwind_guard_first() {
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let main = body(&src, "fn finalizer_thread_main(");
    let guard = pos(main, "let _unwind = DeliveryThreadUnwind \u{7b}");
    let publish = pos(
        main,
        "shared.mem.finalizer_thread.set_delivery_thread(tid.0);",
    );
    let batch = pos(main, "drain_finalizer_thread_batch(&shared, &mut thread);");
    assert!(guard < publish && publish < batch);
}

/// `CRATONVM_DBG_NO_REFPROC`: the pre-collection pass nulls nothing (the
/// restore that would undo it never runs), and the short-circuit of the
/// post-collection pass still relocates the queues and rows it leaves.
#[test]
fn gci_i42rp_the_no_refproc_arm_neither_nulls_nor_strands() {
    let interp = concat!(
        include_str!("../../interpreter/mod.rs"),
        "\n",
        include_str!("../../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../../interpreter/execute.rs"),
        "\n",
        include_str!("../../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let pre = body(&interp, "\npub fn weakref_null_referents_pre_gc(");
    let gate = pos(pre, "if !weakref_clear_enabled() || no_refproc() \u{7b}");
    let lock = pos(pre, "let mut rp = shared.mem.ref_processor.lock();");
    assert!(gate < lock);
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let post = crate::runtime::interpreter::gc_and_alloc::process_references_text(&src);
    let post = post.as_str();
    let short = pos(post, "    if no_refproc() \u{7b}");
    let tail = &post[short..];
    // zt t31/zc11: the short-circuit is in the round's head, which answers
    // whether a round follows.
    let ret = pos(tail, "return false;");
    for step in [
        "shared.mem.cleaner_thread.update_after_gc(pointer_map);",
        "shared.mem.finalizer_thread.update_after_gc(pointer_map);",
        "shared.mem.ref_processor.lock().update_after_gc(pointer_map);",
    ] {
        assert!(pos(tail, step) < ret, "{step}");
    }
}
