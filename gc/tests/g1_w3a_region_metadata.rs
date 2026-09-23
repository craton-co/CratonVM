// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W3-A, wave 3 (2026-09-20) — `G1Region`'s classification metadata is no
//! longer raced between evacuation workers.
//!
//! `docs/internal/g1-2026-09-20/lane-d-region-type-is-not-atomic.md` filed the
//! defect: `region_type` and `age` were plain fields, written by ONE evacuation
//! worker inside `SharedEvac::tlab_alloc` (through a `&mut G1Region` formed
//! from the raw regions base) while every OTHER worker read them with no
//! synchronisation — `shared_dest_alloc`'s destination scan,
//! `claim_resume_region`'s `!= Old` test, the pool-exhaustion census, and every
//! screen reached through `SharedEvac::view()`. The parallel evacuator has been
//! the default young path since 2026-08-13, so that is a data race on the path
//! every production pause takes.
//!
//! # What each test here can and cannot prove
//!
//! There are three separable claims, and only two of them are testable at all:
//!
//! 1. **The aliasing half is gone.** `tlab_alloc`, `claim_resume_region` and
//!    `retire_tlab` no longer form `&mut G1Region` from the raw base. Nothing
//!    here proves that, because the COMPILER proves it: `region_type` and `age`
//!    are private, their accessors take `&self`, and there is no longer any
//!    `&mut *regions_base.0.add(_)` in the file. A regression is a build
//!    failure, not a test failure, which is the point of making the fields
//!    private rather than merely atomic.
//! 2. **The publication ORDER is right** — a peer that sees `Survivor` sees
//!    `age == 1`. That is what [`survivor_age_is_never_observed_stale`] below
//!    tests, on real `G1Region`s, through the real accessors, driving the real
//!    [`G1Region::claim_as_evacuation_destination`]. It fails if that
//!    function's two stores are swapped back into their pre-W3-A order.
//! 3. **The memory ORDERINGS are right** — that the `Release`/`Acquire` pair is
//!    what carries the publication, rather than x86-TSO doing it by accident.
//!    No test on an x86 host can falsify that, because TSO makes a `Relaxed`
//!    pair behave like an acquire/release pair for this shape. It is checked by
//!    the loom model in `gc/tests/loom_w3a_region_metadata.rs`, which is the
//!    only tool here that can permute the orderings.
//!
//! # The falsification recipe for (2)
//!
//! In `gc/src/g1.rs`, swap the two statements inside
//! `G1Region::claim_as_evacuation_destination` so the type is published before
//! the age is stamped — which is exactly what `tlab_alloc` did before W3-A:
//!
//! ```text
//!     self.set_region_type(dest_type);
//!     if dest_type == RegionType::Survivor {
//!         self.set_age(1);
//!     }
//! ```
//!
//! `survivor_age_is_never_observed_stale` then reports a non-zero count of
//! peers that saw a Survivor region carrying `age == 0`. Restore the order and
//! it returns to zero. Measured both ways; see
//! `docs/internal/g1-2026-09-20/w3a-region-metadata-ub.md`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Barrier;

use cratonvm_gc::region::{region_type_decode_failsafes, RegionType};
use cratonvm_gc::{G1Collector, G1CollectorConfig};

/// 512 regions of 64 KiB. Small regions because this test never allocates into
/// them — it only exercises their metadata — and a large COUNT because the
/// window under test is one store wide, so the number of publications is what
/// buys the confidence.
const REGION_SIZE: usize = 64 * 1024;
const REGION_COUNT: usize = 512;

/// How many times the whole table is re-published. Chosen so the test takes
/// well under a second on the reference host while still performing
/// ~100k publications; see the hit-rate note on `PEERS`.
const ROUNDS: usize = 200;

/// Peers reading while the claimer publishes.
///
/// Four, not one. The pre-fix window is a single store wide, so a peer that
/// scanned the whole 512-region table would pass over the region being
/// published roughly once every half-microsecond and would almost never land
/// inside it. The peers here scan a SLIDING WINDOW around the claimer's
/// cursor instead, which is not a contrivance to make the test pass: it is
/// precisely what `shared_dest_alloc` does, which starts its destination scan
/// from `G1Collector::evac_dest_hint` — the region the last object of this type
/// landed in — rather than from index 0.
const PEERS: usize = 4;

/// How many regions ahead of the hint a peer scans, mirroring
/// `shared_dest_alloc`'s wrapping pass from the hint.
const PEER_WINDOW: usize = 8;

fn heap() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: REGION_COUNT * REGION_SIZE,
        initial_heap_size: REGION_COUNT * REGION_SIZE,
        region_size: REGION_SIZE,
        ..Default::default()
    })
}

#[test]
fn survivor_age_is_never_observed_stale() {
    let gc = heap();

    // Counted, not asserted, and deliberately so: an `assert!` inside a peer
    // thread would abort the run on the first sighting and tell us nothing
    // about the RATE, which is the number that separates "the window is one
    // store wide" from "the window is the whole publication". The same
    // reasoning the collector itself applies to a `debug_assert!` inside a GC
    // pause.
    let stale = AtomicUsize::new(0);
    let observed_survivors = AtomicUsize::new(0);
    // The claimer's cursor, published `Relaxed` exactly as
    // `G1Collector::evac_dest_hint` is.
    let hint = AtomicUsize::new(0);
    // Round structure — see `WHY THE ROUNDS ARE FENCED` below.
    let round_over = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    let gate = Barrier::new(PEERS + 1);

    gc.with_regions_mut(|regions| {
        // Reborrow SHARED. From here on nothing in this test holds `&mut` to a
        // region — which is the shape the fix makes possible and the pre-W3-A
        // code could not express, because writing `region_type` needed `&mut`.
        let regions: &[_] = regions;
        let n = regions.len().min(REGION_COUNT);
        assert!(
            n >= 64,
            "fixture produced only {n} regions; the window this test samples              would be most of the table and the measurement would be              meaningless"
        );

        // WHY THE ROUNDS ARE FENCED.
        //
        // To publish a region a second time the test has to un-publish it
        // first, and un-publishing carries the OPPOSITE obligation: a peer that
        // still sees a stale `Survivor` while the age has already gone back to
        // 0 reports a violation that the claim protocol never promised
        // anything about. (Measured, before the barrier went in: 427 false
        // positives in 378,005 observations, all of them from the retire half.)
        // There is no store order that makes both halves safe — it is an ABA —
        // so the retire half runs with the peers parked instead. What remains
        // sampled is exactly the window under test and nothing else.
        std::thread::scope(|s| {
            for _ in 0..PEERS {
                s.spawn(|| loop {
                    gate.wait();
                    if done.load(Ordering::Acquire) {
                        return;
                    }
                    while !round_over.load(Ordering::Acquire) {
                        let start = hint.load(Ordering::Relaxed);
                        for step in 0..PEER_WINDOW {
                            let idx = (start + step) % n;
                            let r = &regions[idx];
                            // THE READ UNDER TEST, in the order every screen
                            // performs it: decide from the TYPE that the region
                            // is a survivor, and only then believe its age.
                            if r.region_type() == RegionType::Survivor {
                                observed_survivors.fetch_add(1, Ordering::Relaxed);
                                if r.age() == 0 {
                                    stale.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    gate.wait();
                });
            }

            s.spawn(|| {
                for _ in 0..ROUNDS {
                    // Retire the whole table, with the peers parked at the
                    // gate below.
                    for r in regions.iter().take(n) {
                        r.set_region_type(RegionType::Free);
                        r.set_age(0);
                    }
                    round_over.store(false, Ordering::Release);
                    gate.wait();
                    for (idx, r) in regions.iter().take(n).enumerate() {
                        hint.store(idx, Ordering::Relaxed);
                        // THE PRODUCTION PUBLICATION, called rather than
                        // copied. Swapping its two stores is the falsification
                        // recipe in this file's header.
                        r.claim_as_evacuation_destination(RegionType::Survivor);
                    }
                    round_over.store(true, Ordering::Release);
                    gate.wait();
                }
                done.store(true, Ordering::Release);
                gate.wait();
            });
        });

        for r in regions.iter() {
            r.set_region_type(RegionType::Free);
            r.set_age(0);
        }
    });

    let observed = observed_survivors.load(Ordering::Relaxed);
    let staleness = stale.load(Ordering::Relaxed);

    // A test that observed nothing proves nothing. This is the control: if the
    // peers never caught the claimer mid-table, a zero `stale` count would be
    // vacuous and the test would keep passing through a reintroduced defect.
    assert!(
        observed > 1_000,
        "peers observed only {observed} Survivor regions across {ROUNDS} rounds          of {REGION_COUNT} publications — the threads did not overlap, so a          zero staleness count would be vacuous. Raise ROUNDS or check that the          peers are actually running."
    );
    assert_eq!(
        staleness, 0,
        "{staleness} of {observed} peer observations saw a region already typed          Survivor whose age was still 0. The publication in          `G1Region::claim_as_evacuation_destination` stamps the age BEFORE the          Release store of the type precisely so this cannot happen; a non-zero          count means the two stores are back in their pre-W3-A order and every          reader of a freshly-claimed Survivor region is reading the age of the          Free region it used to be."
    );
}

#[test]
fn concurrent_region_type_reads_always_decode() {
    let gc = heap();
    let before = region_type_decode_failsafes();
    let stop = AtomicBool::new(false);
    let reads = AtomicUsize::new(0);

    // Every one of the six classifications, cycled through by one writer while
    // peers read. `AtomicU8` cannot tear, so this can only fail if the
    // `to_u8`/`from_u8` pair in `region.rs` is not a bijection over the
    // discriminants — which is the one way a future variant added without a
    // `from_u8` arm would show up, and it would show up as the collector
    // silently reclassifying regions as `Old`.
    const ALL: [RegionType; 6] = [
        RegionType::Eden,
        RegionType::Survivor,
        RegionType::Old,
        RegionType::HumongousStart,
        RegionType::HumongousContinuation,
        RegionType::Free,
    ];

    gc.with_regions_mut(|regions| {
        let regions: &[_] = regions;
        let n = regions.len().min(REGION_COUNT);
        std::thread::scope(|s| {
            for _ in 0..PEERS {
                s.spawn(|| {
                    let mut local = 0usize;
                    while !stop.load(Ordering::Relaxed) {
                        for r in regions.iter().take(n) {
                            let t = r.region_type();
                            // The decode is total by construction; this is the
                            // statement that a value OUTSIDE the six would have
                            // to come back as something, and `from_u8` counts
                            // it rather than returning it.
                            assert!(ALL.contains(&t));
                            local += 1;
                        }
                    }
                    reads.fetch_add(local, Ordering::Relaxed);
                });
            }
            s.spawn(|| {
                for round in 0..ROUNDS {
                    for (idx, r) in regions.iter().take(n).enumerate() {
                        r.set_region_type(ALL[(idx + round) % ALL.len()]);
                    }
                }
                stop.store(true, Ordering::Relaxed);
            });
        });
        // Leave the table as the fixture found it, so nothing downstream in
        // this process sees a heap whose regions claim to be humongous.
        for r in regions.iter() {
            r.set_region_type(RegionType::Free);
        }
    });

    assert!(
        reads.load(Ordering::Relaxed) > 10_000,
        "the peers barely ran; this test's zero-failsafe result would be vacuous"
    );
    assert_eq!(
        region_type_decode_failsafes(),
        before,
        "`RegionType::from_u8` hit its fail-safe arm during a concurrent \
         reclassification. The only writer of the byte is `set_region_type`, \
         which encodes a real variant, and `AtomicU8` cannot tear — so this \
         means a variant was added to `RegionType` without an arm in \
         `from_u8`, and every region of that type now reads back as `Old`."
    );
}

#[test]
fn a_claimed_destination_publishes_its_type_and_age_together() {
    // The single-threaded statement of the same contract, so a reader of this
    // file can see what the stress test is sampling for.
    let gc = heap();
    gc.with_regions_mut(|regions| {
        let r = &regions[0];
        r.set_age(0);
        r.set_region_type(RegionType::Free);

        r.claim_as_evacuation_destination(RegionType::Survivor);
        assert_eq!(r.region_type(), RegionType::Survivor);
        assert_eq!(
            r.age(),
            1,
            "a region claimed as a Survivor destination must carry age 1"
        );

        r.set_age(0);
        r.set_region_type(RegionType::Free);
        r.claim_as_evacuation_destination(RegionType::Old);
        assert_eq!(r.region_type(), RegionType::Old);
        assert_eq!(
            r.age(),
            0,
            "an Old destination is not aged; only the Survivor arm stamps"
        );

        r.set_region_type(RegionType::Free);
    });
}
