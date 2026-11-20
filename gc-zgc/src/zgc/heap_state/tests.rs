use super::super::*;
use cratonvm_types::flags::with_thread_overrides;

fn heap_with(edits: &[(&str, Option<&str>)]) -> ZgcRealHeap {
    with_thread_overrides(edits, || ZgcRealHeap::with_capacity(16 * 1024 * 1024))
}

/// The deciding test of the zr2rv page: two heaps in one process, built
/// under different values of a default-ON switch, each keep their own
/// answer. With the switch latched in a process `OnceLock` (the old
/// `zgc_rewrite_reference_cells_only`) whichever heap read first decided
/// it for both, so one of the two assertions failed.



/// The zr2rv page's §2 called `gc_quiescence`'s watched-referent set
/// process-wide. It is a `thread_local!`: a set published on one thread
/// is invisible to another, so tests on different libtest threads (and
/// two VMs collecting on different threads) cannot see each other's
/// publish. This pins that, so a move to a process table is caught.
#[test]
fn the_watched_set_is_per_thread_not_per_process() {
    cratonvm_gc_core::gc_quiescence::set_watched_referents(&[0x1000, 0x2000]);
    let other = std::thread::spawn(cratonvm_gc_core::gc_quiescence::watched_referents_snapshot)
        .join()
        .expect("snapshot thread");
    assert!(
        other.is_none(),
        "another thread sees no publish of this one"
    );
    assert_eq!(
        cratonvm_gc_core::gc_quiescence::watched_referents_snapshot().map(|s| s.len()),
        Some(2)
    );
    cratonvm_gc_core::gc_quiescence::set_watched_referents(&[]);
}

