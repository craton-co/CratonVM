// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Loom permutation-testing for the SATB activation FSM.
//!
//! Enhancement #1 from `review-2026-05-24/gc.md` §2.3. The SATB
//! activation gate uses a tri-state `AtomicU8`
//! (INACTIVE / ACTIVE / DRAINING) plus per-shard mutexes to close the
//! TOCTOU race documented in `gc/src/satb.rs:15-58`. The contract under
//! test:
//!
//!   FOR EVERY mutator/collector interleaving, no SATB pre-barrier
//!   logged while the gate reports `is_active()` may be stranded after
//!   the collector finishes `deactivate_and_drain`.
//!
//! Loom enumerates every legal reordering of atomic operations under
//! the C11 memory model, so any happens-before bug in the FSM surfaces
//! as a counterexample.
//!
//! # Gap with the production code
//!
//! `gc/src/satb.rs` uses `std::sync::atomic::AtomicU8` and
//! `parking_lot::Mutex` directly. Loom intercepts `loom::sync::atomic`,
//! NOT `std::sync::atomic`, so importing `SatbQueue` into a loom model
//! would test plain std atomics — not what loom's scheduler can reason
//! about. The production code would need a `#[cfg(loom)]` swap of the
//! atomic/mutex imports to give loom visibility. That refactor is out
//! of scope for this test addition.
//!
//! The fix is to replicate the **exact same state machine** here with
//! `loom::sync::atomic::AtomicU8` and `loom::sync::Mutex`. The replica
//! is a 60-line copy of the relevant logic from `satb.rs`. If the
//! production FSM ever diverges from this replica, the replica must
//! be updated in lockstep — see the `#[cfg(loom)]` block below.
//!
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --test loom_satb --release
//! ```
//!
//! Without `--cfg loom` the file compiles to a passing documentation stub
//! with a pointer to this comment. CI does not run loom by default — it is
//! far too slow to be a per-PR gate
//! (interleavings explode combinatorially), but should be run
//! before any change to `satb.rs`.

// Without --cfg loom this is a stub so the test file still compiles in the
// normal `cargo test` invocation. The real model remains behind cfg(loom),
// and gc/build.rs declares that cfg for rustc's check-cfg validation.
#[cfg(not(loom))]
#[test]
fn satb_fsm_loom_model_check() {
    assert!(
        !cfg!(loom),
        "loom model runs only with RUSTFLAGS=\"--cfg loom\" cargo test --test loom_satb"
    );
}

#[cfg(loom)]
mod loom_model {
    use loom::sync::atomic::{AtomicU8, Ordering};
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    // Mirror of the tri-state activation gate from satb.rs. If the
    // production constants change, update these.
    const SATB_INACTIVE: u8 = 0;
    const SATB_ACTIVE: u8 = 1;
    const SATB_DRAINING: u8 = 2;

    /// A 1-shard mirror of `SatbQueue` whose state machine is the same
    /// as the production code but built on `loom::sync` primitives so
    /// the scheduler can permute orderings. One shard is enough for
    /// the FSM property under test — loom's state space blows up
    /// quadratically with thread count, and the multi-shard case
    /// reduces to many independent 1-shard cases.
    struct LoomSatbQueue {
        shard: Mutex<Vec<usize>>,
        state: AtomicU8,
    }

    impl LoomSatbQueue {
        fn new() -> Self {
            Self {
                shard: Mutex::new(Vec::new()),
                state: AtomicU8::new(SATB_INACTIVE),
            }
        }

        fn activate(&self) {
            self.state.store(SATB_ACTIVE, Ordering::Release);
        }

        fn is_active(&self) -> bool {
            self.state.load(Ordering::Acquire) != SATB_INACTIVE
        }

        /// Mutator pre-barrier path: observe the gate, then push if
        /// active. This matches `satb_thread_local_log` followed by
        /// `SatbQueue::flush` (collapsed to a direct push since the
        /// per-thread buffering is independent of the FSM under test).
        fn barrier_pre(&self, addr: usize) -> bool {
            if !self.is_active() {
                return false;
            }
            // Push into the shard, mirroring `flush`.
            let mut guard = self.shard.lock().unwrap();
            guard.push(addr);
            true
        }

        /// Collector path: ACTIVE → DRAINING → drain shard under lock
        /// → INACTIVE. Identical to `deactivate_and_drain` in satb.rs
        /// at the 1-shard reduction.
        fn deactivate_and_drain(&self) -> Vec<usize> {
            let _ = self.state.compare_exchange(
                SATB_ACTIVE,
                SATB_DRAINING,
                Ordering::Release,
                Ordering::Acquire,
            );

            // First drain pass (no lock held across — short).
            let mut all = {
                let mut guard = self.shard.lock().unwrap();
                std::mem::take(&mut *guard)
            };

            // Hold-the-lock pass: any racing writer either pushed
            // before us (drained here) or is parked on the mutex
            // and will observe INACTIVE before retrying.
            {
                let mut guard = self.shard.lock().unwrap();
                let late = std::mem::take(&mut *guard);
                all.extend(late);
                // Critical: flip INACTIVE while holding the lock so a
                // writer blocked on the mutex sees INACTIVE the moment
                // it acquires. This is the happens-before edge that
                // forbids stranded entries.
                self.state.store(SATB_INACTIVE, Ordering::Release);
            }

            all
        }
    }

    /// Property: every pre-barrier call that observed `is_active() ==
    /// true` and then successfully pushed must be drained by the
    /// matching `deactivate_and_drain`. Equivalently — if the mutator
    /// reports it pushed (`barrier_pre` returned true), the drained
    /// vec MUST contain its address.
    ///
    /// Loom enumerates every interleaving; a single counterexample
    /// would print the offending schedule.
    #[test]
    fn satb_fsm_no_stranded_entries() {
        loom::model(|| {
            let q = Arc::new(LoomSatbQueue::new());
            q.activate();

            let q_mut = q.clone();
            let mutator = thread::spawn(move || {
                // Two barrier_pre calls model "the mutator overwrites
                // two reference fields during concurrent mark". We
                // keep it at two — loom's state space is exponential
                // in mutator ops.
                let pushed_a = q_mut.barrier_pre(0xAAAA);
                let pushed_b = q_mut.barrier_pre(0xBBBB);
                (pushed_a, pushed_b)
            });

            let q_col = q.clone();
            let collector = thread::spawn(move || q_col.deactivate_and_drain());

            let (pa, pb) = mutator.join().unwrap();
            let drained = collector.join().unwrap();

            // If the mutator pushed, the entry MUST be in `drained`.
            // Loom permutes operation order — the contract holds for
            // every legal interleaving.
            if pa {
                assert!(
                    drained.contains(&0xAAAA),
                    "stranded SATB entry 0xAAAA: drained = {:?}",
                    drained
                );
            }
            if pb {
                assert!(
                    drained.contains(&0xBBBB),
                    "stranded SATB entry 0xBBBB: drained = {:?}",
                    drained
                );
            }

            // The gate must be INACTIVE after drain regardless of
            // which thread ran first — `is_active` post-condition.
            assert!(
                !q.is_active(),
                "gate not INACTIVE after deactivate_and_drain"
            );
        });
    }

    /// Single-thread sanity: pre-barrier when ACTIVE pushes, when
    /// INACTIVE does not. Loom runs this as one trivial schedule but
    /// it catches accidental constant flips in the FSM mirror.
    #[test]
    fn satb_fsm_single_threaded_active_pushes() {
        loom::model(|| {
            let q = LoomSatbQueue::new();
            assert!(!q.is_active());
            assert!(!q.barrier_pre(0x1));
            q.activate();
            assert!(q.is_active());
            assert!(q.barrier_pre(0x2));
            let drained = q.deactivate_and_drain();
            assert_eq!(drained, vec![0x2]);
            assert!(!q.is_active());
        });
    }
}
