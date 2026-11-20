    use super::*;

    /// Tests record into the global ProfileStore — gate must be on or
    /// `record_*` is a no-op.  Each test calls this guard before driving
    /// the store; the inner `Mutex` serialises gate transitions so a
    /// parallel test that depends on the disabled-default doesn't observe
    /// `true` mid-run.
    pub(super) fn with_profiling_enabled<R>(f: impl FnOnce() -> R) -> R {
        let _g = GATE.lock();
        let prev = is_profiling_enabled();
        enable_profiling(true);
        let r = f();
        enable_profiling(prev);
        r
    }

    /// Serialises every gate transition in this module. Hoisted out of
    /// `with_profiling_enabled` so the disabled-side helper below shares it —
    /// two helpers with private statics would not exclude each other, and a
    /// parallel test would then observe the wrong gate state.
    static GATE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Inverse of [`with_profiling_enabled`], for tests that assert a recorder
    /// is a no-op while the global gate is off (its process default).
    /// `pub(super)`: the receiver-gate tests below read the master gate too
    /// (`receivers_recording_answer`, JIT round 21 wave 7).
    pub(super) fn with_profiling_disabled<R>(f: impl FnOnce() -> R) -> R {
        let _g = GATE.lock();
        let prev = is_profiling_enabled();
        enable_profiling(false);
        let r = f();
        enable_profiling(prev);
        r
    }

    fn make_key(class_id: u32) -> MethodKey {
        MethodKey {
            class_id,
            method_name: Arc::from("test"),
            descriptor: Arc::from("()V"),
        }
    }

    /// nio21 wave 16, lane L4: an inline-cache seed writes one guarded-evidence
    /// row per class, idempotently, and never lowers a live count.
    #[test]
    fn nio21w16_ic_seed_writes_guard_evidence_once_per_class() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(77);
            let n = crate::MIN_GUARDED_RECEIVER_EVIDENCE;
            assert!(store.seed_ic_observed_receiver(&key, 4, 9, n));
            assert!(
                !store.seed_ic_observed_receiver(&key, 4, 9, n),
                "idempotent"
            );
            let p = store.get_profile(&key).expect("the slot");
            assert_eq!(p.receiver_summary(4).map(|s| s.observations), Some(n));
            assert_eq!(
                crate::ir::profile_guarded_receiver_class(Some(&p), 4),
                Some(9)
            );
            // A second class makes the row bimorphic, not a guess on the first.
            assert!(store.seed_ic_observed_receiver(&key, 4, 10, n));
            let p = store.get_profile(&key).unwrap();
            assert_eq!(crate::ir::profile_guarded_receiver_class(Some(&p), 4), None);
            assert!(crate::ir::profile_bimorphic_receiver_classes(Some(&p), 4).is_some());
            // A live count above the seed is kept.
            for _ in 0..(n + 5) {
                store.record_receiver(&key, 8, 3);
            }
            assert!(!store.seed_ic_observed_receiver(&key, 8, 3, n));
            let p = store.get_profile(&key).unwrap();
            assert_eq!(p.receiver_summary(8).map(|s| s.observations), Some(n + 5));
        });
    }

    #[test]
    fn nio21w16_ic_seed_recompile_is_claimed_once_per_method_and_budgeted() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            assert!(store.claim_ic_seed_recompile(&make_key(1)));
            assert!(
                !store.claim_ic_seed_recompile(&make_key(1)),
                "once per method"
            );
            // A redefinition's fresh slot may claim again.
            store.invalidate_class(1);
            assert!(store.claim_ic_seed_recompile(&make_key(1)));
            let mut granted = 2;
            for c in 2..(IC_SEED_RECOMPILE_BUDGET + 10) {
                if store.claim_ic_seed_recompile(&make_key(c)) {
                    granted += 1;
                }
            }
            assert_eq!(granted, IC_SEED_RECOMPILE_BUDGET, "the VM budget");
        });
    }

    /// j27 HANGHUNT (`CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE`): a claim whose
    /// retry was refused is handed back, the method's mark and the VM budget
    /// alike, so a later seed of the same method may claim again.
    #[test]
    fn seed_ic_observed_receiver_reopens_claim_for_distinct_starved_site_within_method_budget() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(1);
            let n = crate::MIN_GUARDED_RECEIVER_EVIDENCE;

            // Site 1 (pc=35, e.g. ByteBuf.setLong) seeds and claims a recompile.
            assert!(store.seed_ic_observed_receiver(&key, 35, 1209, n));
            assert!(store.claim_ic_seed_recompile(&key));
            assert!(!store.claim_ic_seed_recompile(&key));

            // A second receiver class at already-seeded pc=35 records observations
            // but must NOT reopen the recompile claim.
            assert!(store.seed_ic_observed_receiver(&key, 35, 1210, n));
            assert!(!store.claim_ic_seed_recompile(&key));

            // Site 2 (pc=43, e.g. ByteBuf.getLong) was still starved (0 observations)
            // when the compiler thread snapshotted after pc=35. When pc=43 seeds for
            // the first time, it must be able to claim a follow-up recompile without
            // consuming an extra method unit from IC_SEED_RECOMPILE_BUDGET.
            assert!(store.seed_ic_observed_receiver(&key, 43, 1209, n));
            assert!(
                store.claim_ic_seed_recompile(&key),
                "second newly-seeded starved call site in the same method must claim retry"
            );
            assert!(!store.claim_ic_seed_recompile(&key));

            let mut granted = 1;
            for c in 2..(IC_SEED_RECOMPILE_BUDGET + 10) {
                if store.claim_ic_seed_recompile(&make_key(c)) {
                    granted += 1;
                }
            }
            assert_eq!(
                granted, IC_SEED_RECOMPILE_BUDGET,
                "multi-site seed within one method must hold only one global budget unit"
            );
        });
    }

    #[test]
    fn j27_hanghunt_a_released_ic_seed_claim_can_be_claimed_again() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            assert!(store.claim_ic_seed_recompile(&make_key(1)));
            store.release_ic_seed_recompile(&make_key(1));
            assert!(
                store.claim_ic_seed_recompile(&make_key(1)),
                "released: claimable again"
            );
            // The budget came back too: exactly the budget is granted in all.
            store.release_ic_seed_recompile(&make_key(1));
            let mut granted = 0;
            for c in 1..(IC_SEED_RECOMPILE_BUDGET + 10) {
                if store.claim_ic_seed_recompile(&make_key(c)) {
                    granted += 1;
                }
            }
            assert_eq!(granted, IC_SEED_RECOMPILE_BUDGET, "the release refunds the budget");
            // A release of a method that holds no claim changes nothing.
            store.release_ic_seed_recompile(&make_key(IC_SEED_RECOMPILE_BUDGET + 50));
            assert!(!store.claim_ic_seed_recompile(&make_key(IC_SEED_RECOMPILE_BUDGET + 51)));
        });
    }

    #[test]
    fn test_branch_counts_thresholds() {
        // Not enough samples — neither flag set.
        let sparse = BranchCounts {
            taken: 5,
            not_taken: 5,
        };
        assert!(!sparse.is_usually_taken());
        assert!(!sparse.is_usually_not_taken());

        // Usually taken (>90 %).
        let hot = BranchCounts {
            taken: 95,
            not_taken: 5,
        };
        assert!(hot.is_usually_taken());
        assert!(!hot.is_usually_not_taken());

        // Usually not taken (<10 %).
        let cold = BranchCounts {
            taken: 1,
            not_taken: 99,
        };
        assert!(!cold.is_usually_taken());
        assert!(cold.is_usually_not_taken());

        // Borderline — exactly 90 % (not strictly > 90 %).
        let borderline = BranchCounts {
            taken: 18,
            not_taken: 2,
        };
        assert!(!borderline.is_usually_taken(), "90% is not >90%");

        // r11-tier: both directions near saturation. The saturating `total()`
        // pinned at `u32::MAX` and read this 50/50 branch as usually taken.
        let saturated = BranchCounts {
            taken: 3_900_000_000,
            not_taken: 3_900_000_000,
        };
        assert!(
            !saturated.is_usually_taken(),
            "a 50/50 branch is not biased"
        );
        assert!(!saturated.is_usually_not_taken());
    }

    #[test]
    fn test_dominant_receiver_above_threshold() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 85);
        counts.insert(2, 15);
        // 85% ≥ 80% threshold → dominant = class 1
        assert_eq!(dominant_receiver(&counts, 80), Some(1));
    }

    #[test]
    fn test_dominant_receiver_below_threshold() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 70);
        counts.insert(2, 30);
        // 70% < 80% threshold → no dominant receiver
        assert_eq!(dominant_receiver(&counts, 80), None);
    }

    #[test]
    fn test_profile_store_branch_accumulates() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(42);
            for _ in 0..15 {
                store.record_branch(&key, 10, true);
            }
            for _ in 0..5 {
                store.record_branch(&key, 10, false);
            }
            let profile = store.get_profile(&key).unwrap();
            let counts = &profile.branches[&10];
            assert_eq!(counts.taken, 15);
            assert_eq!(counts.not_taken, 5);
            assert!(!counts.is_usually_taken(), "<20 samples");

            // Add enough to make it >90% taken.
            for _ in 0..80 {
                store.record_branch(&key, 10, true);
            }
            let profile = store.get_profile(&key).unwrap();
            let counts = &profile.branches[&10];
            // 95 taken / 5 not-taken = 95% > 90% with ≥20 samples
            assert_eq!(counts.taken, 95);
            assert!(counts.is_usually_taken());
        });
    }

    /// A held [`ReceiverRecorder`] must keep writing into the SAME slot the
    /// store hands out — that is the property the invoke fast door's one-entry
    /// memo rests on, and the one that would fail silently if it broke.
    ///
    /// Silently is the point: a stale handle does not error, it DROPS the
    /// records. Dropped receiver records are exactly the biased profile that
    /// made the door return wrong answers
    /// (`testrandommapops-deterministic-1810-null-FIXED-20260904.md`),
    /// so this is pinned by a test rather than by the comment that says slots
    /// are insert-only.
    #[test]
    fn a_held_receiver_recorder_stays_the_stores_slot() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let class_id = 7u32;
            let name: std::sync::Arc<str> = std::sync::Arc::from("hot");
            let desc: std::sync::Arc<str> = std::sync::Arc::from("()V");

            // Take the handle FIRST, the way the memo does on its first call.
            let rec = store.receiver_recorder_borrowed(class_id, &name, &desc);

            // Insert other methods in between, so the handle has to survive
            // whatever the store does to its shards while it is held.
            for other in 0..64u32 {
                let n: std::sync::Arc<str> = std::sync::Arc::from(format!("m{other}"));
                store.record_receiver_borrowed(class_id + 1 + other, &n, &desc, 1, 1, 16);
            }

            // Now interleave the two routes into the same method.
            for _ in 0..10 {
                rec.record(11, 99, 16);
            }
            for _ in 0..5 {
                store.record_receiver_borrowed(class_id, &name, &desc, 11, 99, 16);
            }

            let key = MethodKey {
                class_id,
                method_name: name.clone(),
                descriptor: desc.clone(),
            };
            let profile = store
                .get_profile(&key)
                .expect("the handle and the store must name the same method");
            let seen = profile.receivers[&11][&99];
            assert_eq!(
                seen, 15,
                "handle-recorded and store-recorded observations must land in \
                 one slot; a stale handle would show only the store's 5"
            );
        });
    }

    #[test]
    fn test_profile_store_receiver_accumulates() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(7);
            for _ in 0..90 {
                store.record_receiver(&key, 5, 100);
            }
            for _ in 0..10 {
                store.record_receiver(&key, 5, 200);
            }
            let profile = store.get_profile(&key).unwrap();
            let rcounts = &profile.receivers[&5];
            assert_eq!(dominant_receiver(rcounts, 80), Some(100));
        });
    }

    /// Disabled-by-default sanity: with PROFILING_ENABLED=false the recorder
    /// must remain a no-op (no MethodKey clone, no HashMap insert).
    ///
    /// Uses the same gate as `with_profiling_enabled` to avoid racing
    /// against tests that flip the flag on.
    #[test]
    fn profiling_disabled_is_noop() {
        with_profiling_enabled(|| {
            // Within the guard, switch the gate back off for this scope so
            // we can assert the no-op behaviour without a parallel test
            // flipping it on between calls.
            enable_profiling(false);
            let store = ProfileStore::new();
            let key = make_key(123);
            for _ in 0..100 {
                store.record_branch(&key, 1, true);
                store.record_backedge(&key, 1);
                store.record_receiver(&key, 1, 0);
                store.record_trip_complete(&key, 1, 4);
            }
            assert!(
                store.get_profile(&key).is_none(),
                "no profile entry should be created while profiling is disabled"
            );
        });
    }

    // ===== M29 snapshot_all / snapshot_invocation_counts =====

    #[test]
    fn m29_snapshot_all_returns_all_methods() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let k1 = make_key(1);
            let k2 = MethodKey {
                class_id: 2,
                method_name: Arc::from("other"),
                descriptor: Arc::from("(I)V"),
            };
            store.record_branch(&k1, 5, true);
            store.record_branch(&k2, 10, false);
            let all = store.snapshot_all();
            assert_eq!(all.len(), 2);
        });
    }

    #[test]
    fn m29_snapshot_all_empty_store() {
        let store = ProfileStore::new();
        let all = store.snapshot_all();
        assert!(all.is_empty());
    }

    #[test]
    fn m29_snapshot_invocation_counts() {
        let store = ProfileStore::new();
        store.increment_invocation(0x0001_0000_ABCD);
        store.increment_invocation(0x0001_0000_ABCD);
        store.increment_invocation(0x0002_0000_1234);
        let counts = store.snapshot_invocation_counts();
        assert_eq!(counts.len(), 2);
        let abcd = counts.iter().find(|(k, _)| *k == 0x0001_0000_ABCD);
        assert_eq!(abcd.unwrap().1, 2);
    }

    // --- Sharded invocation counters (global-Mutex removal) ----------------

    /// Every increment must be observed exactly once under concurrency. The
    /// old implementation held one process-global `Mutex` for this; the
    /// sharded version relies on a per-shard read-lock plus an atomic RMW, so
    /// a lost update here would mean methods warm up slower than their real
    /// call count (or never reach the JIT threshold).
    #[test]
    fn invocation_counter_concurrent_increments_are_exact() {
        const THREADS: usize = 8;
        const PER_THREAD: u32 = 2_000;
        let store = Arc::new(ProfileStore::new());
        // Two keys that differ ONLY in the high (class_id) half: they must
        // share a shard, since `invocation_shard_for` masks the low 64 bits.
        // This is the case most likely to expose a lost update.
        let key_a: u128 = (1u128 << 64) | 0xDEAD_BEEF;
        let key_b: u128 = (2u128 << 64) | 0xDEAD_BEEF;
        assert_eq!(
            invocation_shard_for(key_a),
            invocation_shard_for(key_b),
            "keys differing only in the class_id half must collide on one shard"
        );

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let store = Arc::clone(&store);
            let key = if t % 2 == 0 { key_a } else { key_b };
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    store.increment_invocation(key);
                }
            }));
        }
        for h in handles {
            h.join().expect("increment worker should not panic");
        }

        let counts = store.snapshot_invocation_counts();
        let expected = PER_THREAD * (THREADS as u32 / 2);
        for key in [key_a, key_b] {
            let got = counts
                .iter()
                .find(|(k, _)| *k == key)
                .expect("key should be present")
                .1;
            assert_eq!(got, expected, "lost update on key {key:#x}");
        }
    }

    /// `invocation_count` reads what `increment_invocation` and
    /// `add_invocations` wrote and counts nothing itself: 0 for a method never
    /// called, and unchanged by being read.
    #[test]
    fn invocation_count_reads_without_counting() {
        let store = ProfileStore::new();
        assert_eq!(store.invocation_count(0xC01D), 0);
        assert_eq!(store.invocation_count(0xC01D), 0, "a read must not insert");
        store.increment_invocation(0xC01D);
        store.add_invocations(0xC01D, 16);
        assert_eq!(store.invocation_count(0xC01D), 17);
        assert_eq!(store.invocation_count(0xC01D), 17);
        assert_eq!(store.invocation_count(0xC01E), 0);
    }

    /// The counter returned by `increment_invocation` is what the interpreter
    /// compares against the warmup threshold, so it must be the *new* value
    /// and must advance by exactly one per call.
    #[test]
    fn invocation_counter_returns_monotonic_new_value() {
        let store = ProfileStore::new();
        for expected in 1..=64u32 {
            assert_eq!(store.increment_invocation(0xFEED_FACE), expected);
        }
    }

    /// JIT round 18 wave 9 (lane eagercold): the read-only peek answers `0`
    /// for an uncounted key without inserting one, and the current count
    /// (both counting doors) without changing it.
    #[test]
    fn invocation_count_peeks_without_counting() {
        let store = ProfileStore::new();
        assert_eq!(store.invocation_count(0xC01D), 0);
        assert_eq!(
            store.invocation_count(0xC01D),
            0,
            "the peek inserts nothing"
        );
        assert_eq!(store.increment_invocation(0xC01D), 1, "still a first call");
        assert_eq!(store.invocation_count(0xC01D), 1);
        let _ = store.add_invocations(0xC01D, 16);
        assert_eq!(store.invocation_count(0xC01D), 17);
        assert_eq!(
            store.invocation_count(0xC01D),
            17,
            "the peek counts nothing"
        );
        assert_eq!(store.invocation_count(0xC01E), 0, "another key");
    }

    /// Saturating semantics are preserved from the pre-sharding
    /// `u32::saturating_add(1)` implementation: the counter pins at `u32::MAX`
    /// instead of wrapping to 0 (which would restart JIT warmup).
    #[test]
    fn invocation_counter_saturates_instead_of_wrapping() {
        let store = ProfileStore::new();
        let key: u128 = 0x0BAD_C0DE;
        // Seed the cell directly at MAX-1 rather than calling increment 4
        // billion times.
        {
            let shard = &store.invocation_counts[invocation_shard_for(key)];
            shard
                .counts
                .write()
                .insert(key, AtomicU32::new(u32::MAX - 1));
        }
        assert_eq!(store.increment_invocation(key), u32::MAX);
        // Further increments stay pinned, and never wrap through 0.
        for _ in 0..4 {
            assert_eq!(store.increment_invocation(key), u32::MAX);
        }
    }

    /// A pinned invocation cell is never observed below the cap, and no
    /// concurrent incrementer is ever handed a restarted count. The old
    /// `fetch_add`-then-restore form wrapped the cell to 0 on EVERY call once
    /// pinned, so both a reader and a racing incrementer could see ~0.
    #[test]
    fn a_pinned_invocation_counter_is_never_momentarily_restarted() {
        let store = Arc::new(ProfileStore::new());
        let key: u128 = (9u128 << 64) | 0x5A7_0000;
        {
            let shard = &store.invocation_counts[invocation_shard_for(key)];
            shard.counts.write().insert(key, AtomicU32::new(u32::MAX));
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let (store, stop) = (Arc::clone(&store), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut lowest = u32::MAX;
                while !stop.load(Ordering::Relaxed) {
                    for (k, n) in store.snapshot_invocation_counts() {
                        if k == key {
                            lowest = lowest.min(n);
                        }
                    }
                }
                lowest
            })
        };
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let mut lowest = u32::MAX;
                    for _ in 0..20_000 {
                        lowest = lowest.min(store.increment_invocation(key));
                    }
                    lowest
                })
            })
            .collect();
        for w in writers {
            assert_eq!(
                w.join().expect("writer"),
                u32::MAX,
                "an incrementer was handed a restarted count"
            );
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(
            reader.join().expect("reader"),
            u32::MAX,
            "a saturated invocation counter must never be observed below its cap"
        );
    }

    /// The batched doors must pin the CELL, not only the value they return:
    /// `fetch_add` let the cell wrap while the returned value saturated, so
    /// the next read restarted a hot method's warmup from almost zero.
    #[test]
    fn batched_invocation_credit_saturates_the_cell() {
        let store = ProfileStore::new();
        let key: u128 = (3u128 << 64) | 0x00C0_FFEE;
        {
            let shard = &store.invocation_counts[invocation_shard_for(key)];
            shard
                .counts
                .write()
                .insert(key, AtomicU32::new(u32::MAX - 3));
        }
        assert_eq!(store.add_invocations(key, 16), u32::MAX);
        assert_eq!(
            store.add_invocations(key, 16),
            u32::MAX,
            "the cell itself stayed pinned instead of wrapping"
        );
        assert_eq!(store.add_loop_work(key, u32::MAX), u32::MAX);
        assert_eq!(store.increment_invocation(key), u32::MAX);
    }

    /// `add_loop_work` credits exactly `loop_work_credit(iterations)`, so the
    /// VM's "did THIS credit cross the threshold" test (the total minus that
    /// credit) names the one credit that crossed, whatever the divisor is.
    #[test]
    fn loop_work_credits_exactly_the_published_credit() {
        let store = ProfileStore::new();
        let key: u128 = (5u128 << 64) | 0x10AD_3A7E;
        assert_eq!(loop_work_credit(LOOP_WORK_PER_INVOCATION - 1), 0);
        assert_eq!(store.add_loop_work(key, LOOP_WORK_PER_INVOCATION - 1), 0);
        let mut total = 0u32;
        for iterations in [LOOP_WORK_PER_INVOCATION * 3 + 7, 300, 1_000, 31, 64] {
            let before = total;
            total = store.add_loop_work(key, iterations).max(total);
            assert_eq!(
                total.saturating_sub(loop_work_credit(iterations)),
                before,
                "the credit applied for {iterations} iterations is the published one"
            );
        }
    }

    /// Overloads must not share a counter. The old key folded name and
    /// descriptor into 32 bits with `31 * h`, where short strings collide by
    /// construction (`"Aa"` and `"BB"` hash alike).
    #[test]
    fn invocation_keys_separate_overloads_and_keep_the_class_id() {
        let a = cratonvm_jit_api::invoc_key_parts(7, "m", "(IJ)V");
        let b = cratonvm_jit_api::invoc_key_parts(7, "m", "(JI)V");
        let c = cratonvm_jit_api::invoc_key_parts(7, "mI", "(J)V");
        let d = cratonvm_jit_api::invoc_key_parts(7, "Aa", "()V");
        let e = cratonvm_jit_api::invoc_key_parts(7, "BB", "()V");
        assert_ne!(a, b);
        assert_ne!(
            a, c,
            "the name/descriptor boundary is part of the fingerprint"
        );
        assert_ne!(d, e);
        assert_eq!((a >> 64) as u32, 7, "the class id stays in the high half");
        let store = ProfileStore::new();
        store.increment_invocation(a);
        assert_eq!(
            store.increment_invocation(b),
            1,
            "an overload starts its own count"
        );
    }

    /// `invalidate_class` must sweep *every* shard: because the shard index
    /// comes from the low 64 bits, one class's methods are spread across all
    /// of them. A single-shard sweep would leak counters for an unloaded
    /// class and let a recycled `class_id` inherit stale warmup state.
    #[test]
    fn invalidate_class_sweeps_all_shards() {
        let store = ProfileStore::new();
        // 256 methods of class 7 — with 64 shards this reliably populates
        // many distinct shards.
        for m in 0..256u128 {
            store.increment_invocation((7u128 << 64) | m);
        }
        // A second class that must survive the sweep.
        for m in 0..256u128 {
            store.increment_invocation((9u128 << 64) | m);
        }
        assert_eq!(store.snapshot_invocation_counts().len(), 512);

        store.invalidate_class(7);

        let remaining = store.snapshot_invocation_counts();
        assert_eq!(remaining.len(), 256, "class 7 counters should all be gone");
        assert!(
            remaining.iter().all(|(k, _)| (*k >> 64) as u32 == 9),
            "only class 9 counters should remain"
        );
    }

    /// gc-common w36-c: the batch form leaves exactly what a loop of the
    /// single form leaves -- in the invocation counters, the profile map and
    /// the name index -- for the empty set, a set naming an absent class, and
    /// a mixed set.
    #[test]
    fn invalidate_classes_matches_a_loop_of_invalidate_class() {
        fn fill() -> ProfileStore {
            let store = ProfileStore::new();
            for class in 1..=6u32 {
                for m in 0..40u128 {
                    store.increment_invocation(((class as u128) << 64) | m);
                }
                for m in 0..5 {
                    let name: Arc<str> = Arc::from(format!("m{m}").as_str());
                    let desc: Arc<str> = Arc::from("()V");
                    // Populates both `methods` and `name_index`.
                    let _ = store.get_or_insert_borrowed(class, &name, &desc);
                    let _ = store.get_or_insert(&MethodKey {
                        class_id: class,
                        method_name: Arc::from(format!("o{m}").as_str()),
                        descriptor: Arc::clone(&desc),
                    });
                }
            }
            store
        }
        fn state(store: &ProfileStore) -> (Vec<u128>, Vec<(u32, String)>, Vec<u32>) {
            let mut counts: Vec<u128> = store
                .snapshot_invocation_counts()
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            counts.sort_unstable();
            let mut methods = Vec::new();
            let mut index = Vec::new();
            for shard in &store.shards {
                for key in shard.methods.read().keys() {
                    methods.push((
                        key.class_id,
                        format!("{}{}", key.method_name, key.descriptor),
                    ));
                }
                for (key, _) in shard.name_index.read().values() {
                    index.push(key.class_id);
                }
            }
            methods.sort();
            index.sort_unstable();
            (counts, methods, index)
        }
        let sets: [&[u32]; 4] = [&[], &[99], &[2], &[1, 3, 6, 99]];
        for set in sets {
            let looped = fill();
            for &id in set {
                looped.invalidate_class(id);
            }
            let batched = fill();
            batched.invalidate_classes(&set.iter().copied().collect());
            let (a, b) = (state(&looped), state(&batched));
            assert_eq!(a, b, "set {set:?}");
            assert!(!a.2.is_empty(), "the name index was populated");
            for &id in set {
                assert!(b.0.iter().all(|k| (*k >> 64) as u32 != id));
                assert!(b.1.iter().all(|(c, _)| *c != id));
                assert!(b.2.iter().all(|c| *c != id));
            }
        }
    }

    #[test]
    fn m29_snapshot_all_preserves_branch_data() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(99);
            for _ in 0..10 {
                store.record_branch(&key, 42, true);
            }
            for _ in 0..5 {
                store.record_branch(&key, 42, false);
            }
            let all = store.snapshot_all();
            assert_eq!(all.len(), 1);
            let (_, profile) = &all[0];
            let counts = &profile.branches[&42];
            assert_eq!(counts.taken, 10);
            assert_eq!(counts.not_taken, 5);
        });
    }

    #[test]
    fn m29_snapshot_all_preserves_receiver_data() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(88);
            store.record_receiver(&key, 20, 500);
            store.record_receiver(&key, 20, 500);
            store.record_receiver(&key, 20, 600);
            let all = store.snapshot_all();
            assert_eq!(all.len(), 1);
            let (_, profile) = &all[0];
            let rcounts = &profile.receivers[&20];
            assert_eq!(rcounts[&500], 2);
            assert_eq!(rcounts[&600], 1);
        });
    }

    // ===== S38 loop trip profile tests =====

    #[test]
    fn s38_loop_trip_profile_basic() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..50 {
            ltp.record_backedge();
        }
        assert_eq!(ltp.backedge_count, 50);
        assert_eq!(ltp.entry_count, 0);
        assert_eq!(ltp.avg_trip_count(), 0.0);
    }

    #[test]
    fn s38_loop_trip_suggests_unroll() {
        let mut ltp = LoopTripProfile::default();
        // Simulate a loop with avg trip count 8: 100 entries × 8 trips = 800 back-edges
        for _ in 0..800 {
            ltp.record_backedge();
        }
        for _ in 0..100 {
            ltp.record_trip_complete(8);
        }
        assert!((ltp.avg_trip_count() - 8.0).abs() < 0.01);
        assert_eq!(ltp.suggests_unroll_factor(8), Some(2)); // 8..=32 trips → 2x (R12-2)
    }

    #[test]
    fn s38_loop_trip_no_unroll_cold() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..50 {
            ltp.record_backedge();
        }
        // < 100 back-edges → not hot enough
        assert_eq!(ltp.suggests_unroll_factor(8), None);
    }

    #[test]
    fn s38_loop_trip_no_unroll_large_trip() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..200 {
            ltp.record_backedge();
        }
        for _ in 0..2 {
            ltp.record_trip_complete(100); // avg = 100
        }
        // Round 12 wave 2 (R12-2): a long loop is the one unrolling pays
        // for, so avg 100 > 32 → 4x (it used to be no hint).
        assert_eq!(ltp.suggests_unroll_factor(8), Some(4));
    }

    #[test]
    fn s38_loop_trip_medium_suggests_2x() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..500 {
            ltp.record_backedge();
        }
        for _ in 0..25 {
            ltp.record_trip_complete(20); // avg = 20
        }
        assert_eq!(ltp.suggests_unroll_factor(8), Some(2)); // avg≤32 → 2x
    }

    /// Tier proposal R12-2: short loops get no copies (`Some(1)`, which the
    /// planner reads as "do not unroll"), long ones the most, and a loop with
    /// back edges but no recorded exit gives no hint at all. The legacy
    /// mapping behind `CRATONVM_JIT_PGO_UNROLL_LEGACY_FACTOR` is unchanged.
    #[test]
    fn the_unroll_factor_grows_with_the_trip_average() {
        let new = |avg: f64, entries: u32| unroll_factor_for(1_000, entries, avg, 8, false);
        assert_eq!(new(1.0, 10), Some(1));
        assert_eq!(new(3.0, 10), Some(1));
        assert_eq!(new(7.9, 10), Some(1));
        assert_eq!(new(8.0, 10), Some(2));
        assert_eq!(new(32.0, 10), Some(2));
        assert_eq!(new(33.0, 10), Some(4));
        assert_eq!(new(10_000.0, 10), Some(4));
        assert_eq!(new(0.0, 0), None, "no exit seen: no average");
        assert_eq!(unroll_factor_for(99, 10, 64.0, 8, false), None, "cold");
        assert_eq!(
            unroll_factor_for(1_000, 10, 64.0, 2, false),
            Some(2),
            "capped"
        );
        let legacy = |avg: f64| unroll_factor_for(1_000, 10, avg, 8, true);
        assert_eq!(legacy(1.0), None);
        assert_eq!(legacy(3.0), Some(4));
        assert_eq!(legacy(20.0), Some(2));
        assert_eq!(legacy(64.0), None);
        assert_eq!(legacy(200.0), None);
    }

    /// Round 11 wave 19: an exit charges the back edges since the previous
    /// exit as its trips, so back edges plus exits alone give the average
    /// `suggests_unroll_factor` reads; replayed back edges are not charged to
    /// the first live exit.
    #[test]
    fn a_loop_exit_charges_the_back_edges_since_the_previous_exit() {
        let mut ltp = LoopTripProfile::default();
        for trips in [6u32, 10, 8] {
            for _ in 0..trips {
                ltp.record_backedge();
            }
            ltp.record_exit();
        }
        assert_eq!((ltp.entry_count, ltp.total_trips), (3, 24));
        assert!((ltp.avg_trip_count() - 8.0).abs() < 0.01);
        // Hot enough once there are 100 back edges.
        for _ in 0..12 {
            for _ in 0..8 {
                ltp.record_backedge();
            }
            ltp.record_exit();
        }
        assert_eq!(ltp.backedge_count, 120);
        assert_eq!(ltp.suggests_unroll_factor(8), Some(2));

        let mut seeded = MethodProfile::default();
        seeded.seed_replayed_loop(9, 1_000, 10, 1_000);
        for _ in 0..4 {
            seeded.record_backedge(9);
        }
        seeded.record_loop_exit(9);
        let lp = &seeded.loops[&9];
        assert_eq!(
            (lp.entry_count, lp.total_trips),
            (11, 1_004),
            "the live exit is charged its own 4 back edges, not the seed's 1 000"
        );
    }

    /// `while (i < n) { if (x) { i++; continue; } i += 2; }` as javac lays it
    /// out: the `continue` is a second backward `goto` to the header (pc 12),
    /// in front of the `if`'s join (pc 15).
    const WHILE_CONTINUE: [u8; 22] = [
        0x1b, 0x1c, 0xa2, 0x00, 0x13, // 0: iload_1 iload_2 if_icmpge 21
        0x1d, 0x99, 0x00, 0x09, // 5: iload_3 ifeq 15
        0x84, 0x01, 0x01, 0xa7, 0xff, 0xf4, // 9: iinc; 12: goto 0 (continue)
        0x84, 0x01, 0x02, 0xa7, 0xff, 0xee, // 15: iinc; 18: goto 0 (closing)
        0xb1, // 21: return
    ];

    #[test]
    fn a_continue_is_keyed_by_its_loops_last_back_edge_and_its_if_is_no_exit() {
        let keyed = LoopExtents::build_keyed(&WHILE_CONTINUE);
        assert_eq!(
            keyed.back_edge_key(12),
            18,
            "the continue counts toward the loop"
        );
        assert_eq!(keyed.back_edge_key(18), 18);
        assert_eq!(
            keyed.exits_at(2),
            &[18],
            "the header's test leaves the loop"
        );
        assert!(
            keyed.exits_at(6).is_empty(),
            "the if's branch to its join stays inside"
        );
        // The pre-W2-3 rule, kept behind CRATONVM_JIT_PGO_LOOP_KEYS=0, charged
        // the `if` as an exit of a loop keyed by the continue's goto.
        let legacy = LoopExtents::build_legacy(&WHILE_CONTINUE);
        assert_eq!(legacy.back_edge_key(12), 12);
        assert_eq!(legacy.exits_at(6), &[12]);
        assert_eq!(legacy.exits_at(2), &[18]);
        // Trailing padding (`Frame::code`'s two zero bytes) changes nothing.
        let mut padded = WHILE_CONTINUE.to_vec();
        padded.extend_from_slice(&[0, 0]);
        let keyed_padded = LoopExtents::build_keyed(&padded);
        assert_eq!(keyed_padded.back_edge_key(12), 18);
        assert_eq!(keyed_padded.exits_at(2), &[18]);
    }

    #[test]
    fn a_break_is_an_exit_of_the_loop_it_leaves() {
        // for (;;) { if (x) break; i++; }
        let code: [u8; 14] = [
            0x1d, 0x99, 0x00, 0x06, // 0: iload_3; 1: ifeq 7
            0xa7, 0x00, 0x09, // 4: goto 13 (break)
            0x84, 0x01, 0x01, // 7: iinc
            0xa7, 0xff, 0xf6, // 10: goto 0
            0xb1, // 13: return
        ];
        let keyed = LoopExtents::build_keyed(&code);
        assert_eq!(keyed.exits_at(4), &[10]);
        assert!(keyed.exits_at(1).is_empty());
        assert!(LoopExtents::build_legacy(&code).exits_at(4).is_empty());
    }

    /// r11-tier-profile-replay-residuals item 2, the map-path half: an owned
    /// frame's length is accepted padded or not, and only another version's
    /// length is refused.
    #[test]
    fn a_map_path_record_from_another_versions_frame_is_dropped() {
        let mut p = MethodProfile::default();
        assert!(
            p.admits_frame_code_len(40),
            "an unstamped slot accepts anything"
        );
        assert!(p.note_code_len(40));
        assert!(p.admits_frame_code_len(40), "the unpadded own length");
        assert!(p.admits_frame_code_len(42), "the padded own length");
        assert!(
            !p.admits_frame_code_len(58),
            "another version's padded length"
        );
        assert!(!p.admits_frame_code_len(0));
        assert_eq!(p.recorded_code_len(), 40, "a refusal marks nothing");
    }

    /// Round 13 wave 3 (lane replay2): the receiver recorders apply the
    /// map-path frame-length rule, on both the store route and a held
    /// [`ReceiverRecorder`] (the invoke doors' memo), so an obsolete frame
    /// cannot vote for the new version's guarded-inline receiver.
    #[test]
    fn a_receiver_from_another_versions_frame_is_dropped() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let name: Arc<str> = Arc::from("run");
            let desc: Arc<str> = Arc::from("()V");
            let slot = store.get_or_insert_borrowed(9, &name, &desc);
            assert!(slot.lock().note_code_len(40));
            let rec = store.receiver_recorder_borrowed(9, &name, &desc);
            rec.record(5, 77, 42); // own version, padded
            rec.record(5, 77, 58); // another version's frame: dropped
            store.record_receiver_borrowed(9, &name, &desc, 5, 77, 40); // own, unpadded
            store.record_receiver_borrowed(9, &name, &desc, 5, 77, 58); // dropped
                                                                        // A snapshot: the held recorder's records sit in the lock-free
                                                                        // rows (W15L2-P3) until a snapshot folds them in.
            let seen = slot.lock().snapshot().receivers[&5][&77];
            assert_eq!(seen, 2, "only the slot's own version's frames record");
            assert_eq!(
                slot.lock().recorded_code_len(),
                40,
                "a refusal marks nothing"
            );
        });
    }

    /// Round 13 wave 3 (lane replay2): a held [`ReceiverRecorder`] whose slot
    /// `invalidate_class` removed (class unloaded or redefined, which keeps
    /// the class id and, interned, the name pointers the doors' memo compares)
    /// answers `false` and records nothing; the holder's fresh lookup gets
    /// the store's new slot, which the next compile reads.
    #[test]
    fn a_recorder_of_a_reclaimed_slot_says_so() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let name: Arc<str> = Arc::from("run");
            let desc: Arc<str> = Arc::from("()V");
            let rec = store.receiver_recorder_borrowed(3, &name, &desc);
            assert!(rec.record(4, 50, 16));
            store.invalidate_class(3);
            assert!(!rec.record(4, 51, 16), "the slot was reclaimed");
            let fresh = store.receiver_recorder_borrowed(3, &name, &desc);
            assert!(fresh.record(4, 52, 16));
            let key = MethodKey {
                class_id: 3,
                method_name: name.clone(),
                descriptor: desc.clone(),
            };
            let profile = store.get_profile(&key).expect("the new slot");
            assert_eq!(profile.receivers[&4].get(&52), Some(&1));
            assert!(profile.receivers[&4].get(&51).is_none());
            assert!(
                profile.receivers[&4].get(&50).is_none(),
                "the old version's slot is gone"
            );
        });
    }

    #[test]
    fn loop_extents_are_built_once_per_body_bytes() {
        let store = ProfileStore::new();
        let name: Arc<str> = Arc::from("run");
        let desc: Arc<str> = Arc::from("()V");
        let a: Arc<[u8]> = Arc::from(&WHILE_CONTINUE[..]);
        let b: Arc<[u8]> = Arc::from(&WHILE_CONTINUE[..]);
        let first = store.loop_extents_borrowed(7, &name, &desc, &a);
        let equal_bytes = store.loop_extents_borrowed(7, &name, &desc, &b);
        assert!(
            Arc::ptr_eq(&first, &equal_bytes),
            "equal bytes reuse the table"
        );
        let mut other = WHILE_CONTINUE.to_vec();
        other[20] = 0xf1; // the closing goto now targets pc 3: another version
        let c: Arc<[u8]> = Arc::from(other);
        let rebuilt = store.loop_extents_borrowed(7, &name, &desc, &c);
        assert!(!Arc::ptr_eq(&first, &rebuilt));
    }

    #[test]
    fn s38_profile_store_backedge_accumulates() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(55);
            for _ in 0..200 {
                store.record_backedge(&key, 42);
            }
            for _ in 0..10 {
                store.record_trip_complete(&key, 42, 20);
            }
            let profile = store.get_profile(&key).unwrap();
            let ltp = &profile.loops[&42];
            assert_eq!(ltp.backedge_count, 200);
            assert_eq!(ltp.entry_count, 10);
            assert!((ltp.avg_trip_count() - 20.0).abs() < 0.01);
        });
    }

    #[test]
    fn s38_snapshot_all_preserves_loop_data() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(66);
            for _ in 0..100 {
                store.record_backedge(&key, 10);
            }
            store.record_trip_complete(&key, 10, 50);
            let all = store.snapshot_all();
            assert_eq!(all.len(), 1);
            let (_, profile) = &all[0];
            let ltp = &profile.loops[&10];
            assert_eq!(ltp.backedge_count, 100);
            assert_eq!(ltp.entry_count, 1);
        });
    }

    /// T10.9.B — smoke test: ProfileStore uses FxHashMap for methods/invocation_counts.
    /// Verifies insert/lookup semantics are preserved after the HashMap → FxHashMap swap.
    #[test]
    fn t10_9_b_fx_hashmap_swap_smoke() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            // Populate a few hundred methods and invocation counts.
            for i in 0..300u32 {
                let key = make_key(i);
                store.record_branch(&key, (i as usize) * 4, i % 2 == 0);
                store.increment_invocation((i as u128) << 64);
            }
            // Snapshot should show all 300 methods.
            let snap = store.snapshot_all();
            assert_eq!(snap.len(), 300, "every inserted method should be present");
            let ivc = store.snapshot_invocation_counts();
            assert_eq!(ivc.len(), 300, "every invocation slot should be present");
            // Spot-check a specific key survives the hash migration.
            let k = make_key(42);
            let profile = store.get_profile(&k).unwrap();
            assert!(
                profile.branches.contains_key(&(42usize * 4)),
                "FxHashMap lookup should find the inserted PC"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Per-call-site evidence
    // -----------------------------------------------------------------------

    #[test]
    fn call_site_evidence_distinguishes_absent_from_zero() {
        let p = MethodProfile::default();
        assert_eq!(p.call_site_count(7), CallSiteEvidence::None);
        assert!(!p.call_site_count(7).is_observed());
        assert_eq!(p.call_site_count(7).count_or_zero(), 0);
    }

    #[test]
    fn call_site_evidence_prefers_the_direct_counter() {
        let mut p = MethodProfile::default();
        // A virtual site with receiver observations only.
        p.record_receiver(4, 100);
        p.record_receiver(4, 100);
        p.record_receiver(4, 200);
        assert_eq!(p.call_site_count(4), CallSiteEvidence::Receivers(3));
        // Once the direct counter exists it wins (it is kind-agnostic and
        // counts executions the receiver hook may not see).
        p.record_call_site(4);
        assert_eq!(p.call_site_count(4), CallSiteEvidence::Direct(1));
    }

    #[test]
    fn call_site_counter_covers_static_calls_receivers_cannot() {
        let mut p = MethodProfile::default();
        for _ in 0..5 {
            p.record_call_site(12); // an invokestatic — no receiver to record
        }
        assert_eq!(p.call_site_count(12), CallSiteEvidence::Direct(5));
        assert!(
            p.receivers.is_empty(),
            "a static call site contributes no receiver evidence"
        );
    }

    #[test]
    fn hot_call_sites_is_ordered_and_deterministic() {
        let mut p = MethodProfile::default();
        for _ in 0..10 {
            p.record_call_site(30);
        }
        for _ in 0..10 {
            p.record_call_site(8); // ties with pc 30 — lower bci must win
        }
        for _ in 0..50 {
            p.record_call_site(20);
        }
        p.record_call_site(99); // below the threshold
        p.record_receiver(40, 7); // receiver-derived, also below threshold
        assert_eq!(p.hot_call_sites(10), vec![(20, 50), (8, 10), (30, 10)]);
        assert_eq!(p.hot_call_sites(1).len(), 5);
        // Same profile, same order — an inliner ranking candidates must not
        // produce a different artifact from run to run.
        assert_eq!(p.hot_call_sites(10), p.hot_call_sites(10));
    }

    #[test]
    fn call_site_counts_saturate_rather_than_wrap() {
        let mut p = MethodProfile::default();
        p.call_sites.insert(3, u32::MAX);
        p.record_call_site(3);
        assert_eq!(p.call_site_count(3), CallSiteEvidence::Direct(u32::MAX));
    }

    #[test]
    fn store_records_and_snapshots_call_sites() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(11);
            store.record_call_site(&key, 16);
            store.record_call_site(&key, 16);
            store.record_call_site(&key, 24);
            let p = store.get_profile(&key).expect("profile recorded");
            assert_eq!(p.call_site_count(16), CallSiteEvidence::Direct(2));
            assert_eq!(p.call_site_count(24), CallSiteEvidence::Direct(1));
            // The bulk snapshot must carry the new map too — AOT training and
            // the tiered manager both read it that way.
            let snap = store.snapshot_all();
            let (_, sp) = snap
                .into_iter()
                .find(|(k, _)| k.class_id == key.class_id)
                .expect("method present in snapshot_all");
            assert_eq!(sp.call_site_count(16), CallSiteEvidence::Direct(2));
        });
    }

    #[test]
    fn call_site_recording_respects_the_global_profiling_gate() {
        // Profiling defaults to OFF; the recorder must be a single atomic load.
        with_profiling_disabled(|| {
            let store = ProfileStore::new();
            let key = make_key(77);
            store.record_call_site(&key, 4);
            assert!(
                store.get_profile(&key).is_none(),
                "nothing may be recorded while profiling is disabled"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Counter arithmetic: the regime where these numbers used to lie
    // -----------------------------------------------------------------------

    /// `dominant_receiver` computed `count * 100` in `u32`, which overflows at
    /// 42 949 673 observations — a threshold one hot virtual site passes in
    /// seconds. In a debug build that panicked inside the JIT's profile read;
    /// in a release build it wrapped and answered arbitrarily.
    #[test]
    fn dominant_receiver_survives_counts_past_the_u32_multiply_overflow() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 3_000_000_000);
        counts.insert(2, 100);
        assert_eq!(dominant_receiver(&counts, 80), Some(1));

        // And the negative answer is still correct at that scale: a 50/50 split
        // of four billion observations is not 80 % dominant.
        let mut split = ReceiverCounts::default();
        split.insert(1, 2_000_000_000);
        split.insert(2, 2_000_000_000);
        assert_eq!(dominant_receiver(&split, 80), None);
    }

    /// `max_by_key` over an `FxHashMap` returns whichever tied entry came last
    /// in iteration order, so two compilations of the same profile could seed
    /// different inline caches. Ties now break on ascending class id.
    #[test]
    fn dominant_receiver_tie_break_is_deterministic() {
        let mut counts = ReceiverCounts::default();
        counts.insert(31, 500);
        counts.insert(4, 500);
        counts.insert(12, 5);
        for _ in 0..32 {
            assert_eq!(dominant_receiver(&counts, 40), Some(4));
        }
    }

    /// Round 12 wave 1 (r11-tier-profile-replay-residuals item 5): one
    /// observation is not a dominant receiver; the floor's worth is.
    #[test]
    fn dominant_receiver_needs_a_minimum_sample() {
        if !true {
            return; // the kill switch is set in this environment        
}
        let mut one = ReceiverCounts::default();
        one.insert(7, 1);
        assert_eq!(dominant_receiver(&one, 80), None);
        let mut just_under = ReceiverCounts::default();
        just_under.insert(7, DOMINANT_RECEIVER_MIN_OBSERVATIONS - 1);
        assert_eq!(dominant_receiver(&just_under, 80), None);
        let mut enough = ReceiverCounts::default();
        enough.insert(7, DOMINANT_RECEIVER_MIN_OBSERVATIONS);
        assert_eq!(dominant_receiver(&enough, 80), Some(7));
        // The floor is on the site's total, not the top class's count.
        let mut spread = ReceiverCounts::default();
        spread.insert(7, DOMINANT_RECEIVER_MIN_OBSERVATIONS - 2);
        spread.insert(8, 2);
        assert_eq!(dominant_receiver(&spread, 80), Some(7));
    }

    #[test]
    fn dominant_receiver_of_an_all_zero_profile_is_none() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 0);
        assert_eq!(dominant_receiver(&counts, 1), None);
    }

    /// Tier proposal R12-5: the one evidence type answers exactly what each of
    /// the three policies it replaces answers, on a grid of shapes.
    #[test]
    fn receiver_evidence_agrees_with_the_three_policies_it_replaces() {
        let shapes: [&[(u32, u32)]; 9] = [
            &[(7, 10)],
            &[(7, 31), (9, 1)],
            &[(7, 29), (9, 3)],
            &[(7, 950), (9, 50)],
            &[(9, 350), (7, 600), (3, 50)],
            &[(4, 100), (2, 100), (3, 100)],
            &[(1, 200), (2, 200)],
            &[(5, u32::MAX), (6, 1)],
            &[(8, 0)],
        ];
        for shape in shapes {
            let counts: ReceiverCounts = shape.iter().copied().collect();
            let ev = ReceiverEvidence::of(&counts);
            assert_eq!(ev.seed_a_guarded_cache(80), dominant_receiver(&counts, 80));
            let mut p = MethodProfile::default();
            p.receivers.insert(11, counts.clone());
            // Round 12 wave 4 routed both trap-guard predicates and
            // `classify_receiver_shape` through this type, so the formulas
            // they used before are kept here: without them the comparisons
            // below would compare the type with itself.
            // Round 12 wave 5 (W4-4): a saturated site's `Supports` is now
            // `Silent`; every other answer is the legacy one.
            let fidelity_rule = !ev.summary().is_exact();
            let legacy_guard = |class_id: u32| {
                let total: u32 = counts.values().copied().fold(0u32, u32::saturating_add);
                if total < crate::MIN_GUARDED_RECEIVER_EVIDENCE {
                    return GuardEvidence::Silent;
                }
                let hits = u64::from(counts.get(&class_id).copied().unwrap_or(0));
                let pct = u64::from(crate::MIN_GUARDED_RECEIVER_PCT);
                if hits * 100 >= u64::from(total) * pct {
                    if fidelity_rule {
                        return GuardEvidence::Silent;
                    }
                    GuardEvidence::Supports
                } else {
                    GuardEvidence::Rejects
                }
            };
            for &(class_id, _) in shape {
                let verdict = ev.guard_evidence(
                    class_id,
                    crate::MIN_GUARDED_RECEIVER_EVIDENCE,
                    crate::MIN_GUARDED_RECEIVER_PCT,
                );
                assert_eq!(
                    verdict,
                    legacy_guard(class_id),
                    "{shape:?} guard {class_id}"
                );
                assert_eq!(
                    verdict == GuardEvidence::Supports,
                    crate::receiver_profile_supports_guard(Some(&p), 11, class_id),
                    "{shape:?} supports {class_id}"
                );
                assert_eq!(
                    verdict == GuardEvidence::Rejects,
                    crate::receiver_profile_rejects_guard(Some(&p), 11, class_id),
                    "{shape:?} rejects {class_id}"
                );
            }
            let s = ev.summary();
            // The sort `classify_receiver_shape` used before wave 4.
            let mut ranked: Vec<(u32, u32)> = shape.to_vec();
            ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            assert_eq!(s.top, ranked.first().copied(), "{shape:?} top");
            assert_eq!(s.second, ranked.get(1).copied(), "{shape:?} second");
            match crate::classify_receiver_shape(Some(&counts)) {
                crate::ReceiverShape::Monomorphic {
                    class_id,
                    observations,
                } => {
                    assert_eq!(s.top.map(|t| t.0), Some(class_id));
                    assert_eq!(s.observations, observations);
                }
                crate::ReceiverShape::Bimorphic {
                    class_ids,
                    observations,
                } => {
                    assert_eq!(
                        [s.top.map(|t| t.0), s.second.map(|t| t.0)],
                        class_ids.map(Some)
                    );
                    assert_eq!(s.observations, observations);
                }
                crate::ReceiverShape::Saturated { types, .. } => {
                    assert!(!s.is_exact());
                    assert_eq!(s.types, types);
                }
                crate::ReceiverShape::Megamorphic {
                    types,
                    observations,
                }
                | crate::ReceiverShape::Cold {
                    types,
                    observations,
                } => {
                    assert_eq!((s.types, s.observations), (types, observations));                }
                crate::ReceiverShape::Unprofiled => assert_eq!(s.observations, 0),            }        }    }

    /// Tier proposal W4-4 (round 12 wave 5): a pinned counter no longer
    /// supports a trap-on-miss guard, while a saturated site's rejection
    /// stands (the true total is at least the clamped one).
    #[test]
    fn a_saturated_profile_never_supports_a_trap_on_miss_guard() {
        let min = crate::MIN_GUARDED_RECEIVER_EVIDENCE;
        let pct = crate::MIN_GUARDED_RECEIVER_PCT;
        let counts = |pairs: &[(u32, u32)]| -> ReceiverCounts { pairs.iter().copied().collect() };
        // String pinned beside a billion other receivers: the true share is
        // about 81 %, the clamped reading 100 %.
        let pinned = counts(&[(5, u32::MAX), (6, 1_000_000_000)]);
        let ev = ReceiverEvidence::of(&pinned);
        assert!(!ev.summary().is_exact());
        assert_eq!(ev.guard_evidence(5, min, pct), GuardEvidence::Silent);
        assert_eq!(ev.guard_evidence(6, min, pct), GuardEvidence::Rejects);
        // Saturated through the total alone: 3e9 of a clamped 4.29e9 is
        // under 90 %, and so of the true 6e9.
        let total_only = counts(&[(1, 3_000_000_000), (2, 3_000_000_000)]);
        let ev = ReceiverEvidence::of(&total_only);
        assert_eq!(ev.guard_evidence(1, min, pct), GuardEvidence::Rejects);
        // An exact profile is unchanged.
        let exact = counts(&[(5, 950), (6, 50)]);
        let ev = ReceiverEvidence::of(&exact);
        assert_eq!(ev.guard_evidence(5, min, pct), GuardEvidence::Supports);
        assert_eq!(ev.guard_evidence(6, min, pct), GuardEvidence::Rejects);
    }

    /// Receiver counts were the one counter here that did not saturate.
    #[test]
    fn receiver_counts_saturate_rather_than_wrap() {
        let mut p = MethodProfile::default();
        p.receivers.entry(3).or_default().insert(9, u32::MAX);
        p.record_receiver(3, 9);
        assert_eq!(p.receivers[&3][&9], u32::MAX, "must pin, not wrap to 0");

        let summary = p.receiver_summary(3).expect("site was recorded");
        assert_eq!(summary.fidelity, ProfileFidelity::Saturated);
        assert!(!summary.is_exact());
    }

    /// Saturation is also reachable through the *total* with no single counter
    /// pinned, so the summary checks both.
    #[test]
    fn summary_reports_saturation_from_the_total_alone() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 3_000_000_000);
        counts.insert(2, 3_000_000_000);
        assert!(counts.values().all(|&n| n != u32::MAX));
        let summary = summarize_receivers(&counts);
        assert_eq!(summary.fidelity, ProfileFidelity::Saturated);
        assert_eq!(summary.observations, u32::MAX);
    }

    /// `taken * 10` / `total * 9` overflow `u32` at ~430 M observations of one
    /// branch, which a hot interpreted loop reaches.
    #[test]
    fn branch_bias_predicates_do_not_overflow_u32() {
        let hot = BranchCounts {
            taken: 1_000_000_000,
            not_taken: 10,
        };
        assert!(hot.is_usually_taken());
        assert!(!hot.is_usually_not_taken());
        assert!(!hot.is_saturated());

        let cold = BranchCounts {
            taken: 10,
            not_taken: 1_000_000_000,
        };
        assert!(cold.is_usually_not_taken());
        assert!(!cold.is_usually_taken());

        let pinned = BranchCounts {
            taken: u32::MAX,
            not_taken: 1,
        };
        assert!(
            pinned.is_saturated(),
            "a pinned counter is no longer proportional to execution"
        );
        // The floor is still a floor, and it is named.
        let sparse = BranchCounts {
            taken: BRANCH_BIAS_MIN_SAMPLES - 1,
            not_taken: 0,
        };
        assert!(!sparse.is_usually_taken());
    }

    // -----------------------------------------------------------------------
    // Receiver summary
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_receivers_of_an_empty_profile_is_no_evidence() {
        let summary = summarize_receivers(&ReceiverCounts::default());
        assert_eq!(summary.observations, 0);
        assert_eq!(summary.types, 0);
        assert!(summary.top.is_none());
        assert!(summary.second.is_none());
        assert!(!summary.top_holds_at_least_pct(1));
        assert!(summary.is_exact());

        // A site with no receiver at all reports absence, not a zeroed profile.
        let p = MethodProfile::default();
        assert!(p.receiver_summary(7).is_none());
    }

    #[test]
    fn summarize_receivers_ranks_top_two_deterministically() {
        let mut counts = ReceiverCounts::default();
        counts.insert(9, 350);
        counts.insert(7, 600);
        counts.insert(3, 50);
        let summary = summarize_receivers(&counts);
        assert_eq!(summary.observations, 1000);
        assert_eq!(summary.types, 3);
        assert_eq!(summary.top, Some((7, 600)));
        assert_eq!(summary.second, Some((9, 350)));
        assert!(summary.top_holds_at_least_pct(60));
        assert!(!summary.top_holds_at_least_pct(61));

        // Ties on count fall to the lower class id, in both slots, every time.
        let mut tied = ReceiverCounts::default();
        tied.insert(31, 480);
        tied.insert(4, 480);
        tied.insert(12, 40);
        for _ in 0..32 {
            let summary = summarize_receivers(&tied);
            assert_eq!(summary.top, Some((4, 480)));
            assert_eq!(summary.second, Some((31, 480)));
        }
    }

    /// The type count is exact: the live store has no `TypeProfileWidth` cap,
    /// so a one-type reading is one type and not an overflowed table. This is
    /// the property the deleted `pgo::ReceiverTypeProfile` (cap 8) did *not*
    /// have, and the reason the two were never interchangeable evidence.
    #[test]
    fn live_receiver_profile_records_every_type_it_sees() {
        let mut p = MethodProfile::default();
        for class_id in 0..64u32 {
            p.record_receiver(5, class_id);
        }
        let summary = p.receiver_summary(5).expect("site recorded");
        assert_eq!(summary.types, 64, "no type is dropped on overflow");
        assert_eq!(summary.observations, 64);
        assert!(summary.is_exact());
    }

    // -----------------------------------------------------------------------
    // Concurrency contract
    // -----------------------------------------------------------------------

    /// The consistency claim in the module docs, asserted:
    ///
    /// * no lost updates — every recorded observation survives, because the
    ///   per-method `Mutex` serialises the read-modify-write that an atomic
    ///   counter would not have made safe (these are hash-map entries);
    /// * every snapshot is an image of a real instant — counts never move
    ///   backwards between two reads, and a summary's total always equals the
    ///   sum of its own parts, so a compiler never plans against a half-applied
    ///   increment.
    ///
    /// No timing assumption: the polling loop is allowed to observe nothing at
    /// all (its assertions hold vacuously from zero), so the test cannot flake
    /// on a slow or fast machine.
    #[test]
    fn concurrent_receiver_recording_is_lossless_and_monotone() {
        with_profiling_enabled(|| {
            const THREADS: usize = 4;
            const PER_THREAD: u32 = 2_000;
            const PC: usize = 88;

            let store = Arc::new(ProfileStore::new());
            let key = make_key(4242);
            let mut handles = Vec::new();
            for t in 0..THREADS {
                let store = Arc::clone(&store);
                let key = key.clone();
                handles.push(std::thread::spawn(move || {
                    let class_id = (t as u32 % 2) + 1;
                    for _ in 0..PER_THREAD {
                        store.record_receiver(&key, PC, class_id);
                    }
                }));
            }

            let mut prev = (0u32, 0u32);
            for _ in 0..256 {
                let Some(p) = store.get_profile(&key) else {
                    continue;
                };
                let Some(counts) = p.receivers.get(&PC) else {
                    continue;
                };
                let a = counts.get(&1).copied().unwrap_or(0);
                let b = counts.get(&2).copied().unwrap_or(0);
                assert!(
                    a >= prev.0 && b >= prev.1,
                    "a snapshot moved backwards: {prev:?} -> {:?}",
                    (a, b)
                );
                let summary = summarize_receivers(counts);
                assert_eq!(
                    summary.observations,
                    a + b,
                    "snapshot total disagrees with its own parts"
                );
                prev = (a, b);
            }

            for h in handles {
                h.join().expect("recorder thread should not panic");
            }

            let p = store.get_profile(&key).expect("profile recorded");
            let counts = &p.receivers[&PC];
            let per_class = PER_THREAD * (THREADS as u32 / 2);
            assert_eq!(counts[&1], per_class, "lost update on class 1");
            assert_eq!(counts[&2], per_class, "lost update on class 2");
            let summary = summarize_receivers(counts);
            assert_eq!(summary.observations, PER_THREAD * THREADS as u32);
            assert_eq!(summary.types, 2);
            assert!(summary.is_exact());
        });
    }
