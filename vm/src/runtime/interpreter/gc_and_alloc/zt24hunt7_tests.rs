use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle).unwrap_or_else(|| panic!("{needle}"))
}

/// The no-soft exit asks first, before the owed payment; the fresh-mark
/// collection carries the full request on every attempt, runs at the
/// rung's site and withdraws the request after.
#[test]
fn the_no_soft_exit_runs_the_fresh_mark_first() {
    let src = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs")).replace("\r\n", "\n");
    let rung = body(&src, "\npub(crate) fn last_ditch_clear_soft_refs(");
    let no_soft = pos(rung, ".has_active_soft_refs() \u{7b}");
    let due = pos(rung, "if zgc_oom_ladder_fresh_mark_due(shared) \u{7b}");
    let fresh = pos(
        rung,
        "return zgc_oom_ladder_fresh_mark(shared, thread, owed);",
    );
    let owed = pos(
        rung,
        "return owed && zgc_refusal_pay_owed(shared, thread);\n    \u{7d}\n    flush",
    );
    assert!(no_soft < due && due < fresh && fresh < owed);
    let f = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_oom_ladder_fresh_mark(",
    );
    let ask = pos(f, "cratonvm_gc::gc_quiescence::request_major_gc();");
    let run = pos(
        f,
        "maybe_gc_forced_collected(shared, thread, ZGC_REFUSAL_SOFT_RUNG_SITE)",
    );
    let withdraw = pos(f, "cratonvm_gc::gc_quiescence::take_major_gc_request();");
    assert!(ask < run && run < withdraw);
    let gate = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_oom_ladder_fresh_mark_due(",
    );
    assert!(gate.contains("zgc_concurrent_mark_active()"));
    assert!(gate.contains("zgc_last_collection_marked_concurrently()"));
}


/// Control: the default stop-the-world ZGC never marks concurrently, so
/// the rung is never due and, with no soft reference and nothing owed,
/// runs no collection (`oom_fresh_marks=0`), as before.
#[test]
#[cfg(feature = "zgc")]
fn the_default_zgc_never_owes_the_fresh_mark() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    });
    if !shared.mem.heap.is_zgc() {
        return;
    }
    let mut thread = JvmThread::new(ThreadId(0), "zt24hunt7-control");
    assert!(!zgc_oom_ladder_fresh_mark_due(&shared));
    // One stop-the-world collection: still not concurrently marked.
    assert!(maybe_gc_forced_collected(
        &shared,
        &mut thread,
        "tlab-alloc-shaped"
    ));
    assert!(!shared.mem.heap.zgc_last_collection_marked_concurrently());
    assert!(!zgc_oom_ladder_fresh_mark_due(&shared));
    assert!(!last_ditch_clear_soft_refs(&shared, &mut thread));
    assert_eq!(
        shared
            .mem
            .tlab_wedge
            .zgc_refusal
            .oom_fresh_marks
            .load(Ordering::Relaxed),
        0
    );
}
