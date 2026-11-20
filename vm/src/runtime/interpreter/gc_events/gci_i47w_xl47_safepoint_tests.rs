use super::*;
use crate::config::{GcAlgorithm, VmConfig};

fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm,
        ..VmConfig::default()
    })
}

fn body(src: &str, sig: &str) -> String {
    let start = src
        .find(&format!("\n{sig}"))
        .unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start + 1..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// The fold is the calling thread's, on its VM, while held (nested holds
/// stack); another thread and another VM are outside it; off G1 it is
/// nothing.
#[test]
fn gci_i47w_xl47_the_door_fold_is_this_threads_while_held() {
    let g1 = vm(GcAlgorithm::G1);
    let other = vm(GcAlgorithm::G1);
    assert!(!g1_door_folds_this_pause(&g1));
    {
        let _outer = g1_door_safepoint_fold(&g1);
        assert!(g1_door_folds_this_pause(&g1));
        assert!(!g1_door_folds_this_pause(&other), "per VM");
        {
            let _inner = g1_door_safepoint_fold(&g1);
            assert!(g1_door_folds_this_pause(&g1));
        }
        assert!(g1_door_folds_this_pause(&g1), "the outer hold stays");
        let shared = &g1;
        let elsewhere = std::thread::scope(|s| {
            s.spawn(|| g1_door_folds_this_pause(shared))
                .join()
                .expect("joined")
        });
        assert!(!elsewhere, "per OS thread");
    }
    assert!(!g1_door_folds_this_pause(&g1));
    assert!(g1.mem.alloc_ladder.g1_door_safepoint_folds.lock().is_empty());
    let gen = vm(GcAlgorithm::Generational);
    let _hold = g1_door_safepoint_fold(&gen);
    assert!(!g1_door_folds_this_pause(&gen));
    assert!(gen.mem.alloc_ladder.g1_door_safepoint_folds.lock().is_empty());
}

/// A folded collection pause opens no line and is no safepoint of the
/// barrier's count, decided before the line would read the figures; the
/// `gc,jni` line is skipped for it too.
#[test]
fn gci_i47w_xl47_a_folded_pause_opens_no_line_and_counts_nothing() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let start = body(&src, "pub(super) fn gc_event_start(");
    let fold = start
        .find("if g1_door_folds_this_pause(shared) {")
        .expect("the fold test");
    let uncount = start
        .find("shared.mem.gc_barrier.note_pause_not_a_safepoint();")
        .expect("not a safepoint");
    let line = start
        .find("safepoint_line_begin_for(shared, None);")
        .expect("the line");
    assert!(fold < uncount && uncount < line, "the line only in the else arm");
    let by = body(&src, "pub(super) fn gc_event_finish_by(");
    assert!(by.contains("&& !g1_door_folds_this_pause(shared)"));
}

/// The remark's cleanup point splits the `safepoint` line (not for an
/// aborted cycle's remark), and the head prints between the remark's and
/// the cleanup's `gc` lines.
#[test]
fn gci_i47w_xl47_the_remark_line_splits_at_the_cleanup() {
    assert_eq!(G1_PAUSE_CLEANUP_OPERATION, "G1PauseCleanup");
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let cut = body(&src, "pub(super) fn non_collection_pause_cleanup_point(");
    let kind = cut
        .find("if t.kind != NonCollectionPause::G1Remark")
        .expect("G1 remark only");
    let aborted = cut
        .find("if !t.g1_cycle.as_ref().is_some_and(|e| e.aborted) {")
        .expect("not an aborted cycle's");
    let split = cut
        .find("safepoint_line_split(at, G1_PAUSE_CLEANUP_OPERATION);")
        .expect("the split");
    assert!(kind < aborted && aborted < split);
    let edge = body(&src, "fn g1_cycle_edge_finish(");
    let count = edge.find("gc_starts += 1;").expect("counts the start lines");
    let flush = edge
        .find("safepoint_line_flush_head();")
        .expect("the head");
    let write = edge
        .find("log_unified_at(&[tag], LogLevel::Info, &line, at);")
        .expect("the gc lines");
    assert!(count < flush && flush < write);
}
