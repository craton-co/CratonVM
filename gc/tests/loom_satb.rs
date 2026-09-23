// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The SATB activation FSM: what is actually verified, and what is not.
//!
//! # READ THIS BEFORE CITING THIS FILE AS ASSURANCE
//!
//! This file used to claim, in its own words, to prove:
//!
//! > FOR EVERY mutator/collector interleaving, no SATB pre-barrier logged
//! > while the gate reports `is_active()` may be stranded after the collector
//! > finishes `deactivate_and_drain`.
//!
//! **That property is false, this file never ran, and the model it contained
//! was not the production state machine.** All three were confirmed by
//! inspection on 2026-09-20 (gengc-mark2 round 2); the page is
//! `docs/internal/gaps/gengc-mark-loom-model-diverges-from-production-20260920.md`.
//! Round-5 / round-9 comments in `gc/src/satb.rs` that lean on "loom covers
//! this" were leaning on nothing. The file is kept, and rewritten, so that
//! what it does and does not establish is legible.
//!
//! ## 1. The property is false, on purpose
//!
//! A mutator reads the activation gate ONCE, before it commits to logging,
//! and never re-reads it. A writer that passed the gate and is then parked on
//! a shard mutex the drain currently owns will push the moment the drain
//! releases that shard — and if the drain has already moved on, that entry is
//! stranded. Holding the lock across the `INACTIVE` store (which the previous
//! model did, and production does not) changes nothing, because the writer
//! never looks at the gate again.
//!
//! This is not a bug awaiting a fix in `satb.rs`; it is a property of a
//! gate-then-deposit barrier without writer-side cooperation, documented in
//! `docs/internal/gaps/gengc-mark-satb-deactivate-is-stw-only-20260920.md`.
//! Closing it needs a per-thread handshake, as HotSpot does.
//!
//! ## 2. The property that IS true, and is what production relies on
//!
//! > If every mutator has completed its barrier work BEFORE the collector
//! > begins `deactivate_and_drain`, no logged entry is stranded.
//!
//! That is the safepoint precondition. Both production callers —
//! `ConcurrentMarker::remark` and `G1::final_remark` — run inside a
//! stop-the-world pause, where no mutator is mid-barrier at all. The
//! quiesced-mutator model below is the one that asserts a hard property, and
//! [`satb_quiesced_mutators_lose_nothing`] verifies it against the REAL
//! `SatbQueue` on every ordinary `cargo test` run.
//!
//! ## 3. What the loom model is for now
//!
//! The `cfg(loom)` model mirrors production's FSM (multi-shard; `INACTIVE`
//! stored AFTER every shard guard is released; the post-`INACTIVE` sweep; the
//! `in_flight` writer counter added 2026-09-20) and asserts only invariants
//! that genuinely hold under arbitrary interleaving — no fabricated entries,
//! no duplicates, a terminal `INACTIVE` gate, and the quiesced property in the
//! configuration where it applies. It does NOT assert the unconditional
//! no-stranding property, because that would be asserting something known to
//! be false, and a test that is expected to fail teaches nobody anything.
//!
//! ## 4. CI runs this nightly, as of 2026-09-21
//!
//! It did not before. `loom` appeared in no file under `.github/workflows` or
//! `ci/`, so without `--cfg loom` the model was compiled out entirely and the
//! only thing that ran was the ordinary-`cargo test` coverage below — which is
//! real, runs everywhere, and is deliberately not called a proof.
//!
//! The lane is `.github/workflows/loom-nightly.yml`: scheduled at 04:41 UTC,
//! `workflow_dispatch`, and on a push to main/dev that touches `gc/src/satb.rs`
//! or this file, because REPLICA DRIFT (see the note at the end of this header)
//! is the failure it exists to catch and that is the diff that causes it. It is
//! BLOCKING — a loom counterexample is a concrete interleaving, not a
//! triageable tool artefact — and it verifies that `mod loom_model` is in the
//! test list, so a dropped `RUSTFLAGS` fails the job instead of passing the
//! non-loom half.
//!
//! First run, 2026-09-21: **3 tests, 0 failures, 10.69 s** on an 8-core
//! x86-64 Linux host. No counterexample.
//!
//! Run it yourself with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p cratonvm-gc --test loom_satb --release
//! ```
//!
//! If the production FSM in `gc/src/satb.rs` changes, the replica must change
//! with it — it is a hand-copy, because loom intercepts `loom::sync::atomic`
//! and not `std::sync::atomic`, so importing `SatbQueue` itself would test
//! plain std atomics that loom's scheduler cannot reason about.

// ---------------------------------------------------------------------------
// Ordinary `cargo test` coverage — this part actually runs.
// ---------------------------------------------------------------------------

#[cfg(not(loom))]
mod production_fsm {
    use cratonvm_gc::satb::{satb_thread_local_log, SatbQueue};
    use std::sync::Arc;

    /// The STW witness these tests stand in for.
    ///
    /// SAFETY: holds vacuously where the test's only mutator is the test
    /// thread. One test below drains WHILE a mutator logs, on purpose, and
    /// says so at its call site.
    fn stw() -> cratonvm_gc::StopTheWorldToken {
        unsafe { cratonvm_gc::StopTheWorldToken::new() }
    }

    /// The property production actually depends on, against the production
    /// type: **with every mutator quiesced before the drain begins, nothing
    /// logged is lost.**
    ///
    /// This is the safepoint shape — `ConcurrentMarker::remark` and
    /// `G1::final_remark` both call `deactivate_and_drain` inside a
    /// stop-the-world pause, so every mutator has finished its barrier work
    /// before the collector starts. Threads are used to spread entries across
    /// the queue's 16 shards and the per-thread buffers, and are JOINED before
    /// the drain: joining is the model of the safepoint.
    #[test]
    fn satb_quiesced_mutators_lose_nothing() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 500;

        let q = Arc::new(SatbQueue::new());
        q.activate();

        let mut expected: Vec<usize> = Vec::new();
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let q = Arc::clone(&q);
            // Addresses are never dereferenced; `0` is the barrier's own
            // early-out, so every value is non-zero.
            let base = (t + 1) * 0x10_0000;
            for i in 0..PER_THREAD {
                expected.push(base + (i + 1) * 8);
            }
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    // The return value says whether this log SPILLED the
                    // thread's bucket into a shard. Either answer is correct
                    // here — a buffered entry is recovered by the drain's
                    // `flush_all_thread_satb_buffers` step.
                    let _ = satb_thread_local_log(&q, base + (i + 1) * 8);
                }
            }));
        }
        // THE SAFEPOINT. Every mutator is done before the collector starts.
        for h in handles {
            h.join().unwrap();
        }

        // SAFETY: the joins above ARE the safepoint -- every mutator thread
        // has finished before the collector runs.
        let snapshot = q.deactivate_and_drain(&stw());

        assert!(!q.is_active(), "the gate must be INACTIVE after the drain");
        assert_eq!(
            q.quiescence_timeouts(),
            0,
            "no writer was in flight, so the quiescence wait must not have expired"
        );
        let got: std::collections::HashSet<usize> = snapshot.iter().copied().collect();
        for addr in &expected {
            assert!(
                got.contains(addr),
                "quiesced mutator's entry {addr:#x} was stranded by deactivate_and_drain"
            );
        }
    }

    /// The drain may be a SUPERSET (an SATB entry is a conservative gray root,
    /// so extra entries are always safe) but it must never invent an address
    /// no mutator logged. A fabricated entry would be marked as a root and
    /// could name anything at all.
    #[test]
    fn satb_drain_invents_nothing() {
        let h = std::thread::spawn(|| {
            let q = SatbQueue::new();
            q.activate();
            let logged: Vec<usize> = (1..=100).map(|i| i * 8).collect();
            for &a in &logged {
                satb_thread_local_log(&q, a);
            }
            // SAFETY: single-threaded -- this test is its own only mutator.
            let snapshot = q.deactivate_and_drain(&stw());
            let allowed: std::collections::HashSet<usize> = logged.into_iter().collect();
            for addr in &snapshot {
                assert!(
                    allowed.contains(addr),
                    "drain produced {addr:#x}, which no mutator logged"
                );
            }
        });
        h.join().unwrap();
    }

    /// The counterexample from the gap page, made explicit as a NEGATIVE
    /// test: a writer that passed the gate and then stalls is NOT guaranteed
    /// to be in the drain's snapshot. This asserts the weak, true property —
    /// the gate ends INACTIVE and nothing is corrupted — and documents in
    /// code that the strong property is not claimed.
    ///
    /// It deliberately does not assert either outcome for the stalled entry.
    /// Both are legal: the `in_flight` quiescence wait added 2026-09-20 makes
    /// capture the overwhelmingly likely result, and the residual window is
    /// what keeps it from being a guarantee.
    #[test]
    fn satb_concurrent_deactivation_is_not_claimed_to_be_lossless() {
        let q = Arc::new(SatbQueue::new());
        q.activate();

        let qm = Arc::clone(&q);
        let mutator = std::thread::spawn(move || {
            for i in 1..=64usize {
                satb_thread_local_log(&qm, i * 8);
                std::thread::yield_now();
            }
        });
        let qc = Arc::clone(&q);
        // SAFETY -- a DELIBERATE violation. The point of this test is to
        // drain while a mutator is still logging, which is exactly what the
        // precondition forbids: it reproduces the residual window that
        // `deactivate_and_drain`'s own doc describes. The token is a witness
        // the caller offers, not a guard the queue enforces, so a test that
        // means to break the precondition has to say so out loud.
        let collector = std::thread::spawn(move || qc.deactivate_and_drain(&stw()));

        mutator.join().unwrap();
        let snapshot = collector.join().unwrap();

        assert!(!q.is_active(), "the gate must be INACTIVE after the drain");
        for addr in &snapshot {
            assert_eq!(addr % 8, 0, "drain produced a value no mutator logged");
            assert!(*addr != 0, "the barrier must never log a null referent");
        }
    }
}

// ---------------------------------------------------------------------------
// The loom model — compiled only under `--cfg loom`, and run by nobody
// automatically. See the module doc.
// ---------------------------------------------------------------------------

#[cfg(loom)]
mod loom_model {
    use loom::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    // Mirror of the tri-state activation gate from satb.rs. If the
    // production constants change, update these.
    const SATB_INACTIVE: u8 = 0;
    const SATB_ACTIVE: u8 = 1;
    const SATB_DRAINING: u8 = 2;

    /// Production has 16 shards. Two is the smallest number that can express
    /// the behaviour that matters — "the drain has moved past the shard I am
    /// blocked on" — and loom's state space is exponential in thread count and
    /// in the number of synchronising objects, so two is what the model uses.
    ///
    /// The previous version of this file used ONE shard, on the stated grounds
    /// that "the multi-shard case reduces to many independent 1-shard cases".
    /// That is false for this FSM: the `INACTIVE` store is a single event
    /// shared by all shards while the exclusive passes are sequential, and the
    /// production residual has no counterpart in a 1-shard model.
    const SHARDS: usize = 2;

    /// A faithful replica of `SatbQueue`'s activation FSM on `loom::sync`
    /// primitives.
    ///
    /// Differences from the previous replica, each of which was a divergence
    /// from production:
    ///
    /// * `SHARDS = 2`, not 1;
    /// * `INACTIVE` is stored AFTER every shard guard has been dropped, as
    ///   `satb.rs` does, not while holding one;
    /// * the post-`INACTIVE` sweep (production step 5) is modelled;
    /// * `in_flight` and the bounded quiescence wait (production step 3b,
    ///   added 2026-09-20) are modelled. The model's wait is UNBOUNDED, which
    ///   is sound here because loom has no descheduled-forever thread — the
    ///   production bound exists only to defend against a VM safepoint
    ///   suspending a writer mid-`flush`.
    struct LoomSatbQueue {
        shards: [Mutex<Vec<usize>>; SHARDS],
        state: AtomicU8,
        in_flight: AtomicUsize,
    }

    impl LoomSatbQueue {
        fn new() -> Self {
            Self {
                shards: [Mutex::new(Vec::new()), Mutex::new(Vec::new())],
                state: AtomicU8::new(SATB_INACTIVE),
                in_flight: AtomicUsize::new(0),
            }
        }

        fn activate(&self) {
            self.state.store(SATB_ACTIVE, Ordering::Release);
        }

        fn is_active(&self) -> bool {
            self.state.load(Ordering::Acquire) != SATB_INACTIVE
        }

        fn drain_all(&self) -> Vec<usize> {
            let mut out = Vec::new();
            for shard in self.shards.iter() {
                let mut guard = shard.lock().unwrap();
                out.append(&mut *guard);
            }
            out
        }

        /// Mutator pre-barrier: read the gate ONCE, then deposit. The single
        /// read is the whole point — a writer never re-checks, which is why
        /// the unconditional no-stranding property fails.
        ///
        /// `shard` stands in for `shard_for_current_thread()`, which is a pure
        /// function of the calling thread.
        fn barrier_pre(&self, shard: usize, addr: usize) -> bool {
            if !self.is_active() {
                return false;
            }
            // `flush`: announce before choosing a shard, exactly as
            // `SatbQueue::flush` does, so a writer parked on a mutex the drain
            // owns is visible to the drain.
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            {
                let mut guard = self.shards[shard].lock().unwrap();
                guard.push(addr);
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            true
        }

        /// Collector path, step for step with `satb.rs`.
        fn deactivate_and_drain(&self) -> Vec<usize> {
            // 1. ACTIVE -> DRAINING.
            let _ = self.state.compare_exchange(
                SATB_ACTIVE,
                SATB_DRAINING,
                Ordering::Release,
                Ordering::Acquire,
            );

            // 2. First drain pass.
            let mut all = self.drain_all();

            // 3. Exclusive per-shard pass. Each guard is dropped at the end of
            //    its iteration — the drain does NOT hold shard 0 while it
            //    visits shard 1, which is exactly the window.
            for shard in self.shards.iter() {
                let mut guard = shard.lock().unwrap();
                all.append(&mut *guard);
            }

            // 3b. Wait for writers that have committed but not yet pushed.
            while self.in_flight.load(Ordering::SeqCst) != 0 {
                thread::yield_now();
            }
            all.extend(self.drain_all());

            // 4. INACTIVE, after every guard is released.
            self.state.store(SATB_INACTIVE, Ordering::Release);

            // 5. One more sweep, now that the gate is off.
            all.extend(self.drain_all());

            all
        }
    }

    /// The property that holds: with the mutator quiesced before the collector
    /// starts, nothing is stranded.
    ///
    /// This is the safepoint shape, and it is the ONLY configuration in which
    /// the strong property is asserted — see the module doc for why the
    /// unconditional version is false.
    #[test]
    fn satb_quiesced_mutator_loses_nothing() {
        loom::model(|| {
            let q = Arc::new(LoomSatbQueue::new());
            q.activate();

            // Mutator runs to completion FIRST: the safepoint.
            assert!(q.barrier_pre(0, 0xAAAA));
            assert!(q.barrier_pre(1, 0xBBBB));

            let q_col = q.clone();
            let collector = thread::spawn(move || q_col.deactivate_and_drain());
            let drained = collector.join().unwrap();

            assert!(
                drained.contains(&0xAAAA) && drained.contains(&0xBBBB),
                "quiesced entries stranded: {drained:?}"
            );
            assert!(!q.is_active());
        });
    }

    /// Under genuine concurrency, the invariants that DO hold for every
    /// interleaving: the drain never fabricates an entry, never duplicates
    /// one, and always leaves the gate INACTIVE.
    ///
    /// Deliberately does NOT assert that a pushed entry is drained. That is
    /// the false property; asserting it here would produce a counterexample
    /// that is already documented, in a file nothing runs.
    #[test]
    fn satb_concurrent_drain_is_sound_if_not_complete() {
        loom::model(|| {
            let q = Arc::new(LoomSatbQueue::new());
            q.activate();

            let q_mut = q.clone();
            let mutator = thread::spawn(move || {
                // Two ops on DIFFERENT shards — the multi-shard case the
                // 1-shard model could not express at all.
                let a = q_mut.barrier_pre(0, 0xAAAA);
                let b = q_mut.barrier_pre(1, 0xBBBB);
                (a, b)
            });

            let q_col = q.clone();
            let collector = thread::spawn(move || q_col.deactivate_and_drain());

            let _pushed = mutator.join().unwrap();
            let drained = collector.join().unwrap();

            for addr in &drained {
                assert!(
                    *addr == 0xAAAA || *addr == 0xBBBB,
                    "drain fabricated {addr:#x}"
                );
            }
            assert!(
                drained.iter().filter(|a| **a == 0xAAAA).count() <= 1
                    && drained.iter().filter(|a| **a == 0xBBBB).count() <= 1,
                "drain duplicated an entry: {drained:?}"
            );
            assert!(
                !q.is_active(),
                "gate not INACTIVE after deactivate_and_drain"
            );
            assert_eq!(q.in_flight.load(Ordering::SeqCst), 0);
        });
    }

    /// Single-thread sanity: the gate admits when ACTIVE and refuses when
    /// INACTIVE. Catches an accidental constant flip in the replica.
    #[test]
    fn satb_fsm_single_threaded_active_pushes() {
        loom::model(|| {
            let q = LoomSatbQueue::new();
            assert!(!q.is_active());
            assert!(!q.barrier_pre(0, 0x1));
            q.activate();
            assert!(q.is_active());
            assert!(q.barrier_pre(0, 0x2));
            let drained = q.deactivate_and_drain();
            assert_eq!(drained, vec![0x2]);
            assert!(!q.is_active());
            // DRAINING must still admit — that is the point of the tri-state.
            q.state.store(SATB_DRAINING, Ordering::Release);
            assert!(q.is_active());
            assert!(q.barrier_pre(1, 0x3));
            assert_eq!(q.deactivate_and_drain(), vec![0x3]);
        });
    }
}
