use super::*;
use crate::threading::ThreadId;


/// A door's begin drops the thread's earlier steps (only its own), and
/// the chokepoint's report drains them; nothing is recorded while off.
#[test]
fn r22_memory_a_door_begin_resets_this_threads_steps() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let a = JvmThread::new(ThreadId(7), "r22-ladder-a");
    let b = JvmThread::new(ThreadId(8), "r22-ladder-b");
    let trace = |s: &SharedVm| s.mem.alloc_ladder.oome_ladder_trace.lock().clone();
    // Off (the env key unset): nothing recorded.
    let _ = shared.mem.alloc_ladder.dbg_oome_ladder.set(false);
    oome_ladder_begin(&shared, &a, "door=test");
    oome_ladder_step(&shared, &a, "x");
    assert!(trace(&shared).is_empty());
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let _ = shared.mem.alloc_ladder.dbg_oome_ladder.set(true);
    oome_ladder_step(&shared, &a, "stale");
    oome_ladder_step(&shared, &b, "other");
    oome_ladder_begin(&shared, &a, "door=jit-new");
    oome_ladder_step(&shared, &a, "forced-collection");
    let t = trace(&shared);
    assert_eq!(t.len(), 3);
    assert!(t.iter().any(|(id, s)| *id == 8 && s.starts_with("other")));
    assert!(!t.iter().any(|(_, s)| s.starts_with("stale")));
    oome_ladder_report_throw(&shared, &a, "Java heap space");
    let t = trace(&shared);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].0, 8);
}

/// Source witness: the chokepoint reports, every JIT allocation door
/// begins a record, and the compiled `new` / `anewarray` entry checks
/// stand down under the switch.
#[test]
fn r22_memory_every_door_records_and_the_jit_entry_check_stands_down() {
    let ga = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs"));
    let helpers = crate::jit::helpers::HELPERS_SRC;
    let body = |src: &'static str, head: &str| -> &'static str {
        let at = src.find(head).unwrap_or_else(|| panic!("{head}"));
        let end = src[at..].find("\n\u{7d}\n").expect("fn end");
        &src[at..at + end]
    };
    let dump = body(ga, "pub(crate) fn maybe_dump_heap_on_oom_for(");
    assert!(dump.contains("oome_ladder_report_throw(shared, thread, message);"));
    for (head, door) in [
        ("unsafe fn jit_newarray_body(", "\"door=jit-newarray\""),
        ("unsafe fn jit_new_object_body(", "\"door=jit-new\""),
        ("unsafe fn jit_anewarray_object_body(", "\"door=jit-anewarray\""),
    ] {
        assert!(body(helpers, head).contains(door), "{head}");
    }
    for head in ["unsafe fn jit_new_object_body(", "unsafe fn jit_anewarray_object_body("] {
        assert!(
            body(helpers, head).contains(
                "&& !crate::runtime::interpreter::gc_and_alloc::jit_alloc_latch_after_failure_enabled()"
            ),
            "{head}"
        );
    }
    let new = body(helpers, "unsafe fn jit_new_object_body(");
    let failed = new.find("laddered = true;").expect("failure arm");
    assert!(new[failed..].contains("let mut may_attempt ="));
}
