//! zr z8/nr: a native room skip whose retry fails pays the skipped
//! clear-soft rung before the caller may raise `OutOfMemoryError`
//! (`zgc-zr7rv6-native-room-skip-oome-without-a-clear-soft-collection-20261013.md`).

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_gc::heap::ArrayElementType;
use std::sync::atomic::Ordering;

/// A ZGC VM whose arena just refused 1 MiB and whose door collection
/// then made room for it (`room_for_last_refusal() == Some(true)`).
#[cfg(feature = "zgc")]
fn zgc_vm_with_room_after_refusal(thread: &mut JvmThread) -> Option<SharedVm> {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    });
    if !shared.mem.heap.is_zgc() {
        return None;
    }
    let mut served = 0usize;
    while shared
        .mem
        .heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 1 << 20)
        .is_some()
    {
        served += 1;
        assert!(served < 1 << 16, "the arena never refused");
    }
    let _ = maybe_gc_forced_collected_stamped(&shared, thread, "vm-exec", None);
    assert_eq!(shared.mem.heap.zgc_room_for_last_refusal(), Some(true));
    Some(shared)
}

fn cycles(shared: &SharedVm) -> u64 {
    shared.mem.gc_cycle_count.load(Ordering::Relaxed) as u64
}

/// The defect: before z8/nr the skip left nothing behind, so a failed
/// retry raised after one plain collection. Now the failed retry's repay
/// runs a collection (the skipped rung), once.
#[test]
#[cfg(feature = "zgc")]
fn a_failed_retry_after_a_room_skip_pays_the_clear_soft_rung() {
    let mut thread = JvmThread::new(ThreadId(7), "zr8nr-repay");
    let Some(shared) = zgc_vm_with_room_after_refusal(&mut thread) else {
        return;
    };
    assert!(zgc_native_room_skips_soft_rung_for(&shared, &thread));
    let census = &shared.mem.tlab_wedge.zgc_refusal;
    assert_ne!(census.soft_skip_pending.load(Ordering::Relaxed), 0);
    // The retry failed (the span was taken): the caller asks for the rung.
    let c0 = cycles(&shared);
    assert!(zgc_native_repay_skipped_soft_rung(&shared, &mut thread));
    assert!(cycles(&shared) > c0, "the skipped rung collects");
    assert_eq!(census.soft_skip_repaid.load(Ordering::Relaxed), 1);
    assert_eq!(census.soft_skip_pending.load(Ordering::Relaxed), 0);
    // Once only: a second failed retry raises.
    let c1 = cycles(&shared);
    assert!(!zgc_native_repay_skipped_soft_rung(&shared, &mut thread));
    assert_eq!(cycles(&shared), c1);
    let line = zgc_refusal_line(&shared).expect("a ZGC line");
    assert!(
        line.contains(" soft_skipped_room=1 soft_skip_repaid=1 soft_skip_unpaid=0 "),
        "{line}"
    );
}

/// A served retry releases the skip; another thread's pending skip keeps
/// the rung on this one; an OOME raised with the skip pending is counted.
#[test]
#[cfg(feature = "zgc")]
fn the_pending_skip_is_released_by_the_outcome_and_owned_by_one_thread() {
    let mut thread = JvmThread::new(ThreadId(8), "zr8nr-owner");
    let Some(shared) = zgc_vm_with_room_after_refusal(&mut thread) else {
        return;
    };
    let census = &shared.mem.tlab_wedge.zgc_refusal;
    assert!(zgc_native_room_skips_soft_rung_for(&shared, &thread));
    // Another thread's refusal while this skip is pending: no skip there.
    let other = JvmThread::new(ThreadId(9), "zr8nr-other");
    assert!(!zgc_native_room_skips_soft_rung_for(&shared, &other));
    // ... and that thread cannot pay this one's rung.
    let mut other = other;
    assert!(!zgc_native_repay_skipped_soft_rung(&shared, &mut other));
    // A served native allocation releases it.
    note_native_alloc_succeeded(&shared, &thread);
    assert_eq!(census.soft_skip_pending.load(Ordering::Relaxed), 0);
    assert!(!zgc_native_repay_skipped_soft_rung(&shared, &mut thread));
    // Skip again; an OOME raised without the repay is counted as unpaid.
    assert!(zgc_native_room_skips_soft_rung_for(&shared, &thread));
    note_heap_oome_raised(&shared, &thread, "Java heap space");
    assert_eq!(census.soft_skip_unpaid.load(Ordering::Relaxed), 1);
    assert_eq!(census.soft_skip_pending.load(Ordering::Relaxed), 0);
}


/// zt t2/rv (`zgc-zr11rv9-z8-z9-review-residuals-FIXED-20261019.md` item 1): the
/// release a dying thread runs (`vm_exec::run_thread_exit_shared`) frees
/// its own pending record and never another thread's.
#[test]
#[cfg(feature = "zgc")]
fn zt2rv_the_exit_release_frees_only_the_dying_threads_record() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        ..VmConfig::default()
    });
    let dying = JvmThread::new(ThreadId(12), "zt2rv-dying");
    let other = JvmThread::new(ThreadId(13), "zt2rv-other");
    let pending = &shared.mem.tlab_wedge.zgc_refusal.soft_skip_pending;
    pending.store(oome_debt_key(&other), Ordering::Relaxed);
    zgc_native_room_skip_served(&shared, &dying);
    assert_eq!(
        pending.load(Ordering::Relaxed),
        oome_debt_key(&other),
        "another thread's record stays"
    );
    pending.store(oome_debt_key(&dying), Ordering::Relaxed);
    zgc_native_room_skip_served(&shared, &dying);
    assert_eq!(pending.load(Ordering::Relaxed), 0, "the dying thread's record is freed");
}

/// Off ZGC nothing is recorded and nothing is paid.
#[test]
fn other_collectors_never_skip_or_repay() {
    for gc in [GcAlgorithm::Generational, GcAlgorithm::G1] {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: gc,
            ..VmConfig::default()
        });
        let mut thread = JvmThread::new(ThreadId(11), "zr8nr-other-gc");
        assert!(!zgc_native_room_skips_soft_rung_for(&shared, &thread));
        assert!(!zgc_native_repay_skipped_soft_rung(&shared, &mut thread));
    }
}
