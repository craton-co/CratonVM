//! gcd d2/j: the futile-young backoff of the object doors.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use std::sync::atomic::Ordering;

/// 256 KiB, doubling per verdict, capped at 2 % of the heap and never
/// below the base.
#[test]
fn the_quantum_doubles_up_to_two_percent_of_the_heap() {
    let cap = 64 * 1024 * 1024; // 2 % = 1 342 177 bytes
    assert_eq!(futile_young_quantum(1, cap), 256 * 1024);
    assert_eq!(futile_young_quantum(2, cap), 512 * 1024);
    assert_eq!(futile_young_quantum(3, cap), 1024 * 1024);
    assert_eq!(futile_young_quantum(4, cap), (cap / 50) as u64);
    assert_eq!(futile_young_quantum(u32::MAX, cap), (cap / 50) as u64);
    // A tiny heap: the base, not less.
    assert_eq!(
        futile_young_quantum(5, 1024 * 1024),
        FUTILE_YOUNG_QUANTUM_BASE
    );
}

/// The window closes on bytes or on skipped entries, whichever first.
#[test]
fn the_window_closes_on_either_count() {
    let q = FUTILE_YOUNG_QUANTUM_BASE;
    assert!(futile_young_window_open(q, 0, 1));
    assert!(futile_young_window_open(q, q - 1, 1));
    assert!(!futile_young_window_open(q, q, 1));
    assert!(!futile_young_window_open(
        q,
        0,
        q / FUTILE_YOUNG_MIN_OBJECT_BYTES
    ));
    assert!(futile_young_window_open(
        q,
        0,
        q / FUTILE_YOUNG_MIN_OBJECT_BYTES - 1
    ));
}

/// Idle until a futile verdict; the verdict opens the window; a window
/// that saw its quantum of allocation re-arms the collection; a cycle
/// after which young serves the object clears it. Per VM.
#[test]
fn a_futile_verdict_opens_the_window_and_a_serving_young_clears_it() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let st = &shared.mem.alloc_ladder;
    assert!(
        !futile_young_backoff_skips(&shared),
        "idle before any verdict"
    );
    if !cratonvm_types::flags().gc.futile_young_backoff {
        return; // the process runs with the switch off: nothing to arm
    }
    // A fresh heap serves a small object: no verdict.
    note_forced_young_cycle_outcome(&shared, 64);
    assert_eq!(st.futile_young_streak.load(Ordering::Relaxed), 0);
    // An object young cannot hold at all stands in for a full young.
    note_forced_young_cycle_outcome(&shared, usize::MAX / 2);
    assert_eq!(st.futile_young_streak.load(Ordering::Relaxed), 1);
    assert!(futile_young_backoff_skips(&shared), "the window is open");
    assert_eq!(st.futile_young_skips.load(Ordering::Relaxed), 1);
    // A quantum of allocation since the verdict re-arms the collection.
    shared
        .mem
        .bytes_allocated_total
        .fetch_add(FUTILE_YOUNG_QUANTUM_BASE, Ordering::Relaxed);
    assert!(!futile_young_backoff_skips(&shared), "the quantum re-arms");
    // A second futile cycle doubles the next window.
    note_forced_young_cycle_outcome(&shared, usize::MAX / 2);
    assert_eq!(st.futile_young_streak.load(Ordering::Relaxed), 2);
    assert!(futile_young_backoff_skips(&shared));
    // A cycle after which young serves the object clears the verdict.
    note_forced_young_cycle_outcome(&shared, 64);
    assert_eq!(st.futile_young_streak.load(Ordering::Relaxed), 0);
    assert!(!futile_young_backoff_skips(&shared));
    // Another VM never saw any of it.
    let other = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert!(!futile_young_backoff_skips(&other));    }

/// G1 (and ZGC) never arm it.
#[test]
fn the_backoff_is_generational_only() {
    let g1 = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    note_forced_young_cycle_outcome(&g1, usize::MAX / 2);
    assert_eq!(
        g1.mem
            .alloc_ladder
            .futile_young_streak
            .load(Ordering::Relaxed),
        0
    );
    assert!(!futile_young_backoff_skips(&g1));
}
