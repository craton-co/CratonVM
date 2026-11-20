//! gcd d9/b: the allocation-failure ladder's census (`[GC] oome_ladder:`,
//! `cratonvm_gc::gen_heap::oome_ladder`), fed from this file.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use cratonvm_gc::gen_heap::oome_ladder as l;

fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm,
        ..VmConfig::default()
    })
}

fn census(shared: &SharedVm) -> [u64; l::LEN] {
    match &shared.mem.heap {
        _ if shared.mem.heap.is_generational() => shared.mem.heap.gen_oome_ladder_census().unwrap(),
        _ => [0; l::LEN],
    }
}

/// zr z4/ab (`common-watched-referent-set-is-per-thread-not-per-heap-20261012.md` §3): the
/// referent-restore refusal count is per VM. It was a process static, so
/// the second VM's shutdown line reported the first VM's refusals too.
#[test]
#[cfg(feature = "zgc")]
fn referent_restore_refusals_are_counted_per_vm() {
    let a = vm(GcAlgorithm::Zgc);
    let b = vm(GcAlgorithm::Zgc);
    let b_before = referent_restore_refusals(&b);
    note_referent_restore_refused(&a);
    note_referent_restore_refused(&a);
    assert_eq!(referent_restore_refusals(&a), 2);
    assert_eq!(referent_restore_refusals(&b), b_before, "per VM");
}

/// Every heap OOME a door reports is counted, whatever its detail; the
/// VM-limit refusal is not; the census is per VM; off Generational the
/// feed is a no-op.
#[test]
fn heap_oomes_are_counted_per_vm() {
    let shared = vm(GcAlgorithm::Generational);
    let other = vm(GcAlgorithm::Generational);
    let raiser = JvmThread::new(ThreadId(21), "d9b-raiser");
    note_heap_oome_raised(&shared, &raiser, ARRAY_SIZE_EXCEEDS_VM_LIMIT);
    assert_eq!(
        census(&shared)[l::OOMES],
        0,
        "the VM limit is not a heap OOME"
    );
    note_heap_oome_raised(&shared, &raiser, "Java heap space");
    note_heap_oome_raised(
        &shared,
        &raiser,
        "Java heap space (alloc_object with 5 fields)",
    );
    assert_eq!(census(&shared)[l::OOMES], 2);
    assert_eq!(census(&other)[l::OOMES], 0, "per VM");
    let g1 = vm(GcAlgorithm::G1);
    note_heap_oome_raised(&g1, &raiser, "Java heap space");
    assert_eq!(census(&g1), [0; l::LEN]);
}

/// The deciding majors are one deciding call, one or two ladder majors
/// (the second counted as such), each a forced collection with its time.
#[test]
fn the_deciding_majors_are_counted() {
    let shared = vm(GcAlgorithm::Generational);
    let mut thread = JvmThread::new(ThreadId(0), "d9b-majors");
    assert!(majors_to_decide_oome(&shared, &mut thread, "d9b-test"));
    let c = census(&shared);
    assert_eq!(c[l::DECIDING], 1);
    assert_eq!(c[l::MAJORS], 1 + c[l::SECOND_MAJORS], "{c:?}");
    assert!(
        c[l::FORCED] >= c[l::MAJORS],
        "a ladder major is a forced collection: {c:?}"
    );
    assert_eq!(c[l::MAJORS_LOST], 0, "the sole thread wins its pauses");
    assert_eq!(c[l::OOMES], 0, "no error was raised");
}
