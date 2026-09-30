// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gen r4w2/concmark (2026-09-23) — at most one generational concurrent cycle
//! may own the heap's SHARED phase and SATB queue at a time.
//!
//! Every driver (`maybe_concurrent_gc_at`) builds its own `ConcurrentMarker`
//! on the heap's shared `ConcurrentGcState` + `SatbQueue`. Before this round
//! nothing stopped two drivers overlapping on them, and whichever remarked
//! first deactivated the queue under the other's mark. Slicing Phase 2 (so the
//! marking thread releases the old-gen lock and polls safepoints) removed the
//! accidental serialisation the whole-phase lock used to provide, which is why
//! the ownership CAS has to be real.
//!
//! This races many "drivers" on one shared state and checks, with an
//! independent counter, that no two ever hold ownership at once — through both
//! exits: abandoning (drop) and completing after a "sweep" returned the phase
//! to `Idle`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use cratonvm_gc::{ConcurrentGcPhase, ConcurrentGcState, ConcurrentMarker, SatbQueue};

#[test]
fn concurrent_cycle_ownership_is_exclusive_under_contention() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 2_000;

    let state = Arc::new(ConcurrentGcState::new());
    let satb = Arc::new(SatbQueue::new());
    let holders = Arc::new(AtomicUsize::new(0));
    let wins = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let state = Arc::clone(&state);
            let satb = Arc::clone(&satb);
            let holders = Arc::clone(&holders);
            let wins = Arc::clone(&wins);
            std::thread::spawn(move || {
                // One marker per driver, exactly like production: a small fake
                // extent is enough, nothing here marks.
                let marker = ConcurrentMarker::with_shared(0x1000, 4096, satb, state.clone());
                for round in 0..ROUNDS {
                    let Some(cycle) = marker.try_open_cycle() else {
                        std::hint::spin_loop();
                        continue;
                    };
                    let now = holders.fetch_add(1, Ordering::AcqRel) + 1;
                    assert_eq!(now, 1, "two drivers owned the shared cycle at once");
                    wins.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                    holders.fetch_sub(1, Ordering::AcqRel);
                    if (round + t) % 2 == 0 {
                        // The sweep's exit: it stores `Idle` itself (modelled by
                        // moving through `ConcurrentSweep` and letting
                        // `complete` perform the CAS), and ownership is
                        // released without another phase write.
                        state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
                        cycle.complete();
                    } else {
                        // A lost initial-mark pause: dropping abandons.
                        drop(cycle);
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("a driver thread panicked");
    }

    assert!(
        wins.load(Ordering::Relaxed) > 0,
        "some driver must have opened a cycle"
    );
    assert_eq!(
        state.phase(),
        ConcurrentGcPhase::Idle,
        "every exit must hand the shared phase back"
    );
    assert!(
        !satb.is_active(),
        "no exit may leave the shared SATB barrier armed"
    );
}
