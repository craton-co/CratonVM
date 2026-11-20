use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::vm::SharedVm;

fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm,
        ..VmConfig::default()
    })
}

fn count(shared: &SharedVm) -> u64 {
    shared.mem.gc_barrier.safepoint_stats().count
}

/// The fold is held around the G1 block's cycle and compaction only:
/// after the door's own pause (whose line and count are the operation's)
/// and inside `if shared.mem.heap.is_g1()`.
#[test]
fn gci_i47w_xl47_the_fold_covers_the_cycle_and_the_compaction() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let at = src
        .find("fn force_full_collection(")
        .expect("force_full_collection");
    let f = &src[at..];
    let f = &f[..f.find("\n\u{7d}\n").expect("its end")];
    let pause = f
        .find("run_collection_pause(shared, thread, door)")
        .expect("the door's pause");
    let g1 = f
        .find("if shared.mem.heap.is_g1() \u{7b}")
        .expect("the G1 block");
    let fold = f
        .find("let _one_operation = super::gc_events::g1_door_safepoint_fold(shared);")
        .expect("the fold");
    let cycle = f
        .find("g1_force_full_cycle_stw(shared, thread);")
        .expect("the cycle");
    let compaction = f
        .find("g1_full_compaction_pause(shared, thread, door);")
        .expect("the compaction");
    assert!(pause < g1 && g1 < fold && fold < cycle && cycle < compaction);
    assert_eq!(f.matches("g1_door_safepoint_fold(").count(), 1);
}
