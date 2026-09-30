// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W3-A, wave 3 (2026-09-20) — loom model of `G1Region`'s classification
//! publication protocol.
//!
//! `gc/tests/g1_w3a_region_metadata.rs` tests the STORE ORDER of
//! `G1Region::claim_as_evacuation_destination` on real regions, and it can,
//! because reversing those two stores is observable on an x86 host. What no
//! test on an x86 host can falsify is the choice of memory ORDERINGS: TSO makes
//! a pair of `Relaxed` accesses behave like an acquire/release pair for exactly
//! this shape, so a weakened `set_region_type` would pass that test on every
//! machine this project currently builds on and fail on aarch64. This file is
//! the tool that can tell the difference.
//!
//! The contract under test, which is the one
//! `SharedEvac::tlab_alloc` depends on:
//!
//!   FOR EVERY interleaving of one claiming evacuation worker and one peer
//!   worker, a peer that observes a region's `region_type` as `Survivor` also
//!   observes (a) the survivor `age` the claimer stamped, and (b) the F-16
//!   `commit_through_region` the claimer performed before publishing.
//!
//! Nothing else about the region is in the contract, and deliberately: the
//! `cursor` carries its own `Acquire`/`Release` pair (F-11) and the peers that
//! read it do so for their own reasons.
//!
//! # Gap with the production code — the same one `loom_satb.rs` documents
//!
//! `gc/src/g1.rs` uses `std::sync::atomic::AtomicU8`. Loom intercepts
//! `loom::sync::atomic`, not `std::sync::atomic`, so a model that imported
//! `G1Region` would be testing std atomics and loom's scheduler could say
//! nothing about it. `LoomRegionMetadata` below is therefore a replica of
//! `G1Region::{region_type, set_region_type, age, set_age,
//! claim_as_evacuation_destination}` — five short functions, reproduced
//! verbatim apart from the atomic crate. **If those accessors' orderings change
//! in `g1.rs`, change them here in lockstep**, or this model silently stops
//! describing the collector. That risk is why the ORDER half of the contract is
//! tested against the production function in the non-loom file rather than
//! here.
//!
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p cratonvm-gc --test loom_w3a_region_metadata --release
//! ```
//!
//! Without `--cfg loom` this compiles to a passing stub
//! (`region_metadata_publication_loom_model_check`, which asserts only that
//! loom is NOT enabled), exactly as `loom_satb.rs` does.
//!
//! # CI, as of 2026-09-21
//!
//! `.github/workflows/loom-nightly.yml` runs this model nightly and on a push
//! to main/dev that touches either model file. It did not exist before that
//! date: `loom` appeared in no workflow at all, so the lockstep obligation
//! above had nothing enforcing it. It still has nothing enforcing it — a
//! replica that drifts is a replica that goes on passing — but drift now has a
//! nightly chance of being noticed by whoever reads a red or a suspiciously
//! green night. `gc/src/g1.rs` is deliberately NOT a push trigger for the
//! lane: it is too hot a file to pay a full `--cfg loom` rebuild on, and the
//! nightly closes the gap within a day. The lane's header says so.
//!
//! First run, 2026-09-21: **2 tests, 0 failures, 0.69 s**. No counterexample.
//!
//! # The falsification recipe
//!
//! In [`LoomRegionMetadata`] below, change `set_region_type`'s store to
//! `Ordering::Relaxed` (mirroring the same weakening in `g1.rs`). Loom reports
//! a counterexample on the `age` assertion within the first few hundred
//! schedules. Restore `Release` and the model is exhaustively clean. Measured
//! both ways; see `docs/internal/g1-2026-09-20/w3a-region-metadata-ub.md`.

// Without --cfg loom this is a stub so the test file still compiles in the
// normal `cargo test` invocation. gc/build.rs declares the `loom` cfg for
// rustc's check-cfg validation.
#[cfg(not(loom))]
#[test]
fn region_metadata_publication_loom_model_check() {
    assert!(
        !cfg!(loom),
        "loom model runs only with RUSTFLAGS=\"--cfg loom\" cargo test --test loom_w3a_region_metadata"
    );
}

#[cfg(loom)]
mod loom_model {
    use loom::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use loom::sync::Arc;
    use loom::thread;

    // Mirror of `RegionType`'s discriminants from `gc/src/region.rs`. If those
    // change, change these.
    const FREE: u8 = 5;
    const SURVIVOR: u8 = 1;

    /// A replica of the two `G1Region` fields W3-A made atomic, plus a stand-in
    /// for the F-16 commit state that the type store is also obliged to
    /// publish.
    struct LoomRegionMetadata {
        region_type: AtomicU8,
        age: AtomicU8,
        /// `G1Collector::commit_through_region`'s effect, reduced to the one
        /// bit a peer cares about: "the memory behind this region is mapped".
        /// `Relaxed` on both sides, because the whole point of the claim
        /// protocol is that the TYPE store is what publishes this — if it needed
        /// its own acquire/release pair the protocol would be different and the
        /// `Release` on the type would be redundant.
        committed: AtomicBool,
    }

    impl LoomRegionMetadata {
        fn new() -> Self {
            Self {
                region_type: AtomicU8::new(FREE),
                age: AtomicU8::new(0),
                committed: AtomicBool::new(false),
            }
        }

        /// Replica of `G1Region::region_type`.
        fn region_type(&self) -> u8 {
            self.region_type.load(Ordering::Acquire)
        }

        /// Replica of `G1Region::set_region_type`.
        fn set_region_type(&self, t: u8) {
            self.region_type.store(t, Ordering::Release);
        }

        /// Replica of `G1Region::age`.
        fn age(&self) -> u8 {
            self.age.load(Ordering::Relaxed)
        }

        /// Replica of `G1Region::set_age`.
        fn set_age(&self, age: u8) {
            self.age.store(age, Ordering::Relaxed);
        }

        /// Replica of `G1Region::claim_as_evacuation_destination`.
        fn claim_as_evacuation_destination(&self, dest_type: u8) {
            if dest_type == SURVIVOR {
                self.set_age(1);
            }
            self.set_region_type(dest_type);
        }
    }

    /// Property: a peer that sees `Survivor` sees the age and the commit.
    ///
    /// This is `SharedEvac::tlab_alloc` (the claimer) against
    /// `SharedEvac::shared_dest_alloc` / the `RegionView` screens (the peer),
    /// reduced to the two accesses that carry the obligation. Loom enumerates
    /// every legal C11 interleaving; a counterexample prints the schedule.
    #[test]
    fn a_peer_that_sees_survivor_sees_the_age_and_the_commit() {
        loom::model(|| {
            let r = Arc::new(LoomRegionMetadata::new());

            let claimer = {
                let r = r.clone();
                thread::spawn(move || {
                    // F-16: commit BEFORE retyping. `tlab_alloc` returns early
                    // if this fails, so by the time it publishes, the region is
                    // mapped.
                    r.committed.store(true, Ordering::Relaxed);
                    r.claim_as_evacuation_destination(SURVIVOR);
                })
            };

            let peer = {
                let r = r.clone();
                thread::spawn(move || {
                    if r.region_type() == SURVIVOR {
                        // (a) the survivor age. A peer reading 0 here reads the
                        // age of the FREE region this one used to be. Today
                        // that misreports a diagnostic line rather than
                        // misdirecting a promotion — `should_promote` reads
                        // `ObjectHeader::gc_age`, not this — but it is the same
                        // store as the type, and it is the half of the contract
                        // a test can observe.
                        assert_eq!(
                            r.age(),
                            1,
                            "peer observed Survivor with a stale age — the \
                             Release on set_region_type is not carrying the \
                             age store"
                        );
                        // (b) the commit. A peer reading false here would be a
                        // worker bump-allocating into address space that is not
                        // yet memory.
                        assert!(
                            r.committed.load(Ordering::Relaxed),
                            "peer observed Survivor without the F-16 commit — \
                             the Release on set_region_type is not carrying \
                             commit_through_region"
                        );
                    }
                })
            };

            claimer.join().unwrap();
            peer.join().unwrap();
        });
    }

    /// The same, with TWO peers, because `shared_dest_alloc` runs on every
    /// worker at once and a release/acquire pair is per-reader.
    ///
    /// Kept separate rather than folded into the test above: loom's state space
    /// grows fast in the thread count, and the one-peer model is the one worth
    /// running on every change.
    #[test]
    fn two_peers_both_see_a_consistent_publication() {
        loom::model(|| {
            let r = Arc::new(LoomRegionMetadata::new());

            let claimer = {
                let r = r.clone();
                thread::spawn(move || {
                    r.committed.store(true, Ordering::Relaxed);
                    r.claim_as_evacuation_destination(SURVIVOR);
                })
            };

            let peers: Vec<_> = (0..2)
                .map(|_| {
                    let r = r.clone();
                    thread::spawn(move || {
                        if r.region_type() == SURVIVOR {
                            assert_eq!(r.age(), 1);
                            assert!(r.committed.load(Ordering::Relaxed));
                        }
                    })
                })
                .collect();

            claimer.join().unwrap();
            for p in peers {
                p.join().unwrap();
            }
        });
    }
}
