use super::*;


fn test_key() -> MethodKey {
    MethodKey::new("java/lang/String", "hashCode", "()I")
}

/// A key used ONLY by the deny-list test.
///
/// OSR denials used to live in a PROCESS-GLOBAL set, and marking the
/// shared `test_key()` denied OSR for every other test in the binary —
/// which is how `stats_osr_compilations` once intermittently observed 0
/// OSR compilations instead of 1. They are per manager now; the key stays
/// distinct so a test that reads as "denied" names a method nothing else
/// uses.
fn osr_deny_only_key() -> MethodKey {
    MethodKey::new("craton/test/OsrDenyOnly", "denied", "()V")
}

fn test_key2() -> MethodKey {
    MethodKey::new(
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    )
}

#[test]
#[ignore = "BigInteger JIT deny removed 2026-07-31 with the last static ban mirrors (docs/feature-designs/jit-ir-optimizations.md); the assertion is kept as the record of what the ban covered"]
fn hibernate_biginteger_divide_cluster_is_never_background_enqueued() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = MethodKey::new(
        "java/math/MutableBigInteger",
        "divideMagnitude",
        "(Ljava/math/MutableBigInteger;Ljava/math/MutableBigInteger;)Ljava/math/MutableBigInteger;",
    );

    assert_eq!(mgr.on_method_invocation(&key), None);
    assert!(
        mgr.dequeue_compilation().is_none(),
        "quarantined MutableBigInteger must not enter the background queue"
    );
    assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
}

// ── wire-tiered-manager Step 6: CRATONVM_TIER_* policy overrides ─────


#[test]
fn step6_policy_overrides_empty_env_is_default() {
    let p = CompilationPolicy::with_overrides(|_| None);
    let d = CompilationPolicy::default();
    assert_eq!(p.c1_threshold, d.c1_threshold);
    assert_eq!(p.c2_threshold, d.c2_threshold);
    assert_eq!(p.osr_threshold, d.osr_threshold);
    assert_eq!(p.c2_min_invocations, d.c2_min_invocations);
    assert_eq!(p.tiered_enabled, d.tiered_enabled);
}

// ── observed-count fast-forward (stride-boundary deflation fix) ──────

#[test]
fn observed_count_fast_forwards_hotness() {
    let mgr = TieredCompilationManager::with_default_policy();

    // Plain counting: a single visit is far below c1_threshold → None.
    let cold = test_key();
    assert_eq!(mgr.on_method_invocation(&cold), None);

    // Observed-count fast-forward: the interpreter has REALLY seen 5,000
    // invocations of this method but only consults the manager at stride
    // boundaries — the recommendation must fire on this single visit
    // instead of after another c1_threshold visits (64x deflation).
    let hot = test_key2();
    let rec = mgr.on_method_invocation_observed(&hot, 5_000);
    assert!(
        rec.is_some(),
        "observed=5000 must produce a tier recommendation on the first visit"
    );

    // A stale/smaller observed value never rewinds the counter.
    let state_count = mgr
        .method_states()
        .into_iter()
        .find(|(k, _, _)| *k == hot)
        .map(|(_, _, c)| c)
        .expect("state for hot key");
    assert!(state_count >= 5_000);
    let _ = mgr.on_method_invocation_observed(&hot, 3);
    let state_count_after = mgr
        .method_states()
        .into_iter()
        .find(|(k, _, _)| *k == hot)
        .map(|(_, _, c)| c)
        .expect("state for hot key");
    assert!(
        state_count_after > state_count.saturating_sub(1),
        "smaller observed count must not rewind the counter"
    );
}

// ── wire-tiered-manager Step 5: request_osr ──────────────────────────

#[test]
fn step5_request_osr_enqueues_osr_task_immediately() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    // The first call enqueues immediately: the interpreter's per-frame
    // back-edge schedule is the throttle, not a count kept here.
    let task = mgr
        .request_osr(&key, 42)
        .expect("first request should enqueue");
    assert_eq!(task.osr_bci, Some(42));
    assert_eq!(task.priority, CompilationPriority::High);
    assert_eq!(task.target_tier, CompilationTier::C2);
    // Idempotent while queued: a second request is a no-op (no double compile).
    assert!(mgr.request_osr(&key, 42).is_none());
    // The task really is on the queue. The OSR stat counts PUBLISHED
    // artifacts, so a queued request is not one yet.
    let dq = mgr
        .dequeue_compilation()
        .expect("an OSR task should be queued");
    assert_eq!(dq.osr_bci, Some(42));
    assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 0);
    mgr.core
        .complete_task(&key, CompilationTier::C2, 1, true, true, false);
    assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 1);
}

#[test]
fn step5_request_osr_honors_osr_deny_list() {
    // Pinned epoch: a denial expires when the install epoch moves, and the
    // process-wide one moves whenever any test flushes a cache.
    let mgr = worker_manager(CompilationPolicy::default());
    let key = osr_deny_only_key();
    mgr.mark_osr_denied(key.clone());
    assert!(mgr.is_osr_denied(&key));
    assert!(
        mgr.request_osr(&key, 42).is_none(),
        "OSR-denied method must not enqueue an OSR task"
    );
    assert!(mgr.dequeue_compilation().is_none());
    // The denial belongs to this manager, not to the process.
    let other = worker_manager(CompilationPolicy::default());
    assert!(
        other.request_osr(&key, 42).is_some(),
        "another VM's manager must not inherit this one's OSR denials"
    );
}

#[test]
fn step5_request_osr_is_independent_of_method_entry_c2_but_honors_bailout() {
    // A method-entry C2 body does not provide an OSR entry and therefore
    // must not suppress the separately cached OSR artifact.
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C2, 1);
    let task = mgr
        .request_osr(&key, 7)
        .expect("method-entry C2 must still allow an OSR artifact");
    assert_eq!(task.osr_bci, Some(7));

    // C2-bailed method → no OSR enqueue.
    let mgr2 = TieredCompilationManager::with_default_policy();
    let key2 = test_key2();
    mgr2.on_method_invocation(&key2);
    mgr2.on_c2_bailout(&key2);
    assert!(
        mgr2.request_osr(&key2, 7).is_none(),
        "bailed method: no OSR enqueue"
    );
}

/// A denial is a verdict about the code that was loaded when it was made,
/// so it expires when the install epoch moves, and a redefinition of the
/// class forgets it outright. It used to be a process-global set that one
/// failed background compile populated for good.
#[test]
fn an_osr_denial_expires_when_the_install_epoch_moves() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let key = MethodKey::new("craton/test/OsrExpiry", "loop", "()V");
    mgr.mark_osr_denied(key.clone());
    assert!(mgr.request_osr(&key, 3).is_none());
    epoch.store(2, Ordering::Release);
    assert!(
        !mgr.is_osr_denied(&key),
        "a denial stamped at epoch 1 has expired at epoch 2"
    );
    assert!(mgr.request_osr(&key, 3).is_some());

    let redefined = MethodKey::new("craton/test/OsrRedefined", "loop", "()V");
    mgr.mark_osr_denied(redefined.clone());
    mgr.on_class_redefined(ClassId::new(0), "craton/test/OsrRedefined");
    assert!(
        !mgr.is_osr_denied(&redefined),
        "a redefinition forgets the class's denials"
    );
}

/// A failed OSR compile is a failed compile, not a denial: it spends one
/// retry and the next hot back-edge asks again, up to the fail limit.
#[test]
fn a_failed_osr_compile_is_retried_up_to_the_fail_limit() {
    let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
    let key = MethodKey::new("craton/test/OsrRetry", "loop", "()V");
    for attempt in 0..MAX_TIER_FAIL_RETRIES {
        let task = mgr.request_osr(&key, 9).unwrap_or_else(|| {
            panic!("attempt {attempt}: a failed OSR compile must be retried")
        });
        assert_task_eq(mgr.next_fresh_task(), Some(task));
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, false, true, false);
        assert!(
            !mgr.is_osr_denied(&key),
            "a failed compile must not deny OSR"
        );
    }
    assert!(
        mgr.request_osr(&key, 9).is_none(),
        "the retry budget still bounds an OSR compile that keeps failing"
    );
}

/// The background OSR door reacts to the two `None`s of `request_osr` in
/// opposite ways (wait vs retire the loop header), so the classified twin
/// must tell a held slot from a give-up -- and a spent retry budget, which
/// is NOT an OSR denial, must still read as a give-up.
#[test]
fn a_classified_osr_request_tells_waiting_from_giving_up() {
    let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
    let key = MethodKey::new("craton/test/OsrClassified", "loop", "()V");
    let task = match mgr.request_osr_classified(&key, 9) {
        OsrRequest::Queued(task) => task,
        other => panic!("a fresh method must queue, got {other:?}"),
    };
    assert_eq!(
        mgr.request_osr_classified(&key, 9),
        OsrRequest::Pending,
        "a held OSR slot is a wait, not a refusal"
    );
    assert_task_eq(mgr.next_fresh_task(), Some(task));
    for _ in 0..MAX_TIER_FAIL_RETRIES {
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, false, true, false);
        if let OsrRequest::Queued(next) = mgr.request_osr_classified(&key, 9) {
            assert_task_eq(mgr.next_fresh_task(), Some(next));
        }
    }
    assert!(!mgr.is_osr_denied(&key), "a spent budget is not a denial");
    assert_eq!(
        mgr.request_osr_classified(&key, 9),
        OsrRequest::Refused,
        "a spent OSR retry budget refuses every re-offer"
    );
    assert!(mgr.request_osr(&key, 9).is_none());

    let denied = MethodKey::new("craton/test/OsrClassifiedDenied", "loop", "()V");
    mgr.mark_osr_denied(denied.clone());
    assert_eq!(mgr.request_osr_classified(&denied, 3), OsrRequest::Refused);

    let bailed = MethodKey::new("craton/test/OsrClassifiedBailed", "loop", "()V");
    mgr.on_method_invocation(&bailed);
    mgr.on_c2_bailout(&bailed);
    assert_eq!(mgr.request_osr_classified(&bailed, 3), OsrRequest::Refused);
}

/// r11-tier: an OSR decline (the VM's OSR-denial / bail-listed arms, both
/// of which expire with the install epoch) must not set the lifetime,
/// whole-method `ineligible` flag, which also closed method entry.
#[test]
fn an_osr_decline_spends_an_osr_retry_and_leaves_method_entry_open() {
    let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
    let key = MethodKey::new("craton/test/OsrDecline", "loop", "()V");
    let task = mgr.request_osr(&key, 9);
    assert!(task.is_some(), "the first OSR request must be queued");
    assert_task_eq(mgr.next_fresh_task(), task);
    mgr.core
        .complete_task(&key, CompilationTier::C2, 1, false, true, true);
    {
        let methods = mgr.core.methods.lock();
        let state = &methods[&key];
        assert!(
            !state.ineligible,
            "an OSR decline must not close method entry"
        );
        assert_eq!(state.osr_fail_count, 1);
        assert_eq!(state.tier_fail_count, 0);
    }
    let entry = MethodKey::new("craton/test/OsrDecline", "loop", "()V");
    let mut recommended = None;
    for _ in 0..10_000 {
        recommended = recommended.or(mgr.on_method_invocation(&entry));
    }
    assert!(
        recommended.is_some(),
        "method entry must still tier after an OSR decline"
    );
}

// ── Policy defaults ──────────────────────────────────────────────────

#[test]
fn default_policy_values() {
    let p = CompilationPolicy::default();
    assert_eq!(p.c1_threshold, 200);
    assert_eq!(p.c2_threshold, 1_000_000);
    assert_eq!(p.osr_threshold, 10_000);
    assert!(p.tiered_enabled);
    assert_eq!(p.c2_min_invocations, 1_000);
    assert!(p.c1_profiling);
}

// ── Initial state ────────────────────────────────────────────────────

#[test]
fn method_starts_at_interpreter() {
    let mgr = TieredCompilationManager::with_default_policy();
    assert_eq!(mgr.current_tier(&test_key()), CompilationTier::Interpreter);
}

// ── Invocation counting ──────────────────────────────────────────────

#[test]
fn invocation_count_tracking() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    for _ in 0..50 {
        mgr.on_method_invocation(&key);
    }
    let states = mgr.method_states();
    let (_, _, count) = states.iter().find(|(k, _, _)| k == &key).unwrap();
    assert_eq!(*count, 50);
}

// ── C1 trigger ───────────────────────────────────────────────────────

#[test]
fn c1_triggered_after_threshold() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    // Drive exactly `c1_threshold` invocations rather than a hardcoded
    // literal, so this test stays correct regardless of the default
    // policy's threshold value (currently 500; see
    // the rationale comment on `CompilationPolicy::default`).
    let c1_threshold = mgr.policy().c1_threshold;
    let mut triggered = None;
    for _ in 0..c1_threshold {
        if let Some(tier) = mgr.on_method_invocation(&key) {
            triggered = Some(tier);
        }
    }
    assert_eq!(triggered, Some(CompilationTier::C1));
}

// ── C2 trigger ───────────────────────────────────────────────────────

#[test]
fn c2_triggered_after_threshold_with_profile() {
    let policy = CompilationPolicy {
        c1_threshold: 10,
        c2_threshold: 100,
        c2_min_invocations: 50,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();

    // Run up to C1 compilation.
    for _ in 0..10 {
        mgr.on_method_invocation(&key);
    }
    // Drain the C1 task.
    let task = mgr.dequeue_compilation().unwrap();
    assert_eq!(task.target_tier, CompilationTier::C1);
    mgr.compilation_complete(&key, CompilationTier::C1, 5);

    // Keep invoking until C2 threshold.
    let mut c2_triggered = false;
    for _ in 10..100 {
        if let Some(tier) = mgr.on_method_invocation(&key) {
            if tier == CompilationTier::C2 {
                c2_triggered = true;
            }
        }
    }
    assert!(c2_triggered);
}

// ── C1 → C2 on the publish, because nothing asks afterwards ──────────
//
// `c2_triggered_after_threshold_with_profile`, directly above, is the test
// that let this bug live: it keeps calling `on_method_invocation` after the
// C1 publish, and the interpreter does not. Every VM call site that reaches
// the tiered manager is guarded by "this method has no compiled body"
// (`dispatch_static.rs`'s `jit_cache` probe rewrites the site to
// `CachedInvokeTarget::Jit` and returns before the counting block;
// `dispatch_virtual.rs` counts only in the probe's `None` arm;
// `invoke_fast.rs`'s door declines when `callee_has_compiled_body`), and
// `execute_jit_call` counts nothing. `current_tier == C1` means a body WAS
// published, so the hook that would evaluate `should_compile`'s C1→C2 arm
// is never called again and the arm was unreachable in steady state.
//
// These cover the door that replaces it: the same predicate, asked once, on
// the publish itself. See `CompilerCore::promote_hot_c1_to_c2`.

/// The policy these tests share: a C1 bar low enough to cross by hand, and
/// a C2 bar (`max(c2_threshold, c2_min_invocations)` = 50) that a method
/// can reach WHILE its C1 compile is in flight — which is exactly the shape
/// the 100 000-invocation probe produced, where the manager's count froze
/// at ~5 000 with `c2_threshold` lowered to 1 000 and nothing happened.
fn hot_c1_policy() -> CompilationPolicy {
    CompilationPolicy {
        c1_threshold: 5,
        c2_threshold: 50,
        c2_min_invocations: 20,
        ..CompilationPolicy::default()
    }
}

/// Drive `key` to `count` invocations, crossing the C1 bar on the way, and
/// drain the C1 task the crossing enqueues.
///
/// The invocations after the crossing return `None` (the request holds the
/// method's in-flight slot, so `on_method_invocation_settling` returns
/// early) but STILL advance `invocation_count` — which is what makes the
/// count at publish time the interesting number.
fn warm_to_c1_in_flight(mgr: &TieredCompilationManager, key: &MethodKey, count: u32) {
    let mut saw_c1 = false;
    for _ in 0..count {
        if mgr.on_method_invocation(key) == Some(CompilationTier::C1) {
            saw_c1 = true;
        }
    }
    assert!(saw_c1, "crossing c1_threshold must enqueue a C1 task");
    let task = mgr
        .dequeue_compilation()
        .expect("the C1 task should be queued");
    assert_eq!(task.target_tier, CompilationTier::C1);
}

#[test]
fn a_c1_publish_promotes_a_hot_method_without_another_invocation() {
    let mgr = TieredCompilationManager::new(hot_c1_policy());
    let key = test_key();
    // 60 > the C2 bar of 50, all of it accumulated before the publish.
    warm_to_c1_in_flight(&mgr, &key, 60);

    // The publish. NOTHING is invoked after this point — that is the whole
    // claim: in the VM there is no "after this point" for a compiled
    // method's tiering.
    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);

    let task = mgr
        .dequeue_compilation()
        .expect("a method over the C2 bar must be promoted on its C1 publish");
    assert_eq!(task.target_tier, CompilationTier::C2);
    assert_eq!(task.method_key, key);
    assert_eq!(
        task.osr_bci, None,
        "the promotion is a method-entry compile, not an OSR one"
    );
    assert_eq!(
        task.priority,
        CompilationPriority::Low,
        "same priority the C1→C2 supersede uses, so a method both doors \
         would take sees an identical task either way"
    );
}


#[test]
fn a_c1_publish_below_the_c2_bar_is_not_promoted() {
    let mgr = TieredCompilationManager::new(hot_c1_policy());
    let key = test_key();
    // 30 is past `c2_min_invocations` (20) but short of `c2_threshold`
    // (50); the arm needs BOTH, and so does the bar that folds them.
    warm_to_c1_in_flight(&mgr, &key, 30);

    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(
        mgr.dequeue_compilation(),
        None,
        "a method that never crossed the C2 bar must not be promoted"
    );
}


#[test]
fn the_c1_publish_promotion_honors_the_c2_bailout() {
    let mgr = TieredCompilationManager::new(hot_c1_policy());
    let key = test_key();
    warm_to_c1_in_flight(&mgr, &key, 60);
    // A method that has already proved it does not survive C2 stays at C1
    // however hot it is — the same refusal `should_compile`'s C1 arm and
    // `request_c2_upgrade` both apply.
    mgr.core.methods.lock().get_mut(&key).unwrap().c2_bailout = true;

    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(mgr.dequeue_compilation(), None);
}

#[test]
fn the_c1_publish_promotion_honors_ineligibility() {
    let mgr = TieredCompilationManager::new(hot_c1_policy());
    let key = test_key();
    warm_to_c1_in_flight(&mgr, &key, 60);
    mgr.core.methods.lock().get_mut(&key).unwrap().ineligible = true;

    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(mgr.dequeue_compilation(), None);
}

#[test]
fn a_failed_c1_attempt_does_not_promote() {
    let mgr = TieredCompilationManager::new(hot_c1_policy());
    let key = test_key();
    warm_to_c1_in_flight(&mgr, &key, 60);

    // No body was published, so nothing stops the interpreter asking again
    // and `current_tier` is still `Interpreter`. Promoting here would queue
    // an optimizing compile for a method whose cheap tier never succeeded.
    mgr.core
        .complete_task(&key, CompilationTier::C1, 1, false, false, false);
    assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
    assert_eq!(mgr.dequeue_compilation(), None);
}

#[test]
fn an_osr_completion_does_not_take_the_c1_publish_door() {
    let mgr = TieredCompilationManager::new(hot_c1_policy());
    let key = test_key();
    warm_to_c1_in_flight(&mgr, &key, 60);
    // An OSR artifact is a loop-entry body in a separate cache; it neither
    // advances `current_tier` nor says anything about method-entry tiering.
    mgr.core
        .complete_task(&key, CompilationTier::C1, 1, true, true, false);
    assert_eq!(mgr.dequeue_compilation(), None);
}

#[test]
fn tiering_disabled_closes_the_c1_publish_door() {
    let policy = CompilationPolicy {
        tiered_enabled: false,
        ..hot_c1_policy()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();
    // The invocation hook returns before `should_compile` when tiering is
    // off, so warm the state by hand rather than through the C1 crossing.
    for _ in 0..60 {
        mgr.on_method_invocation(&key);
    }
    assert_eq!(mgr.dequeue_compilation(), None, "tiering is off");
    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(
        mgr.dequeue_compilation(),
        None,
        "a disabled tiering must not reach C2 through the publish door \
         either"
    );
}

#[test]
fn the_c1_publish_promotion_opens_and_closes_exactly_one_branch_window() {
    let mgr = TieredCompilationManager::new(hot_c1_policy());
    let key = test_key();
    warm_to_c1_in_flight(&mgr, &key, 60);
    assert_eq!(mgr.branch_window_balance(), 0);

    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(
        mgr.branch_window_balance(),
        1,
        "the C2 nomination arms branch profiling"
    );
    let task = mgr.dequeue_compilation().expect("the promotion dispatches");
    mgr.core
        .complete_task(&key, task.target_tier, 1, true, false, false);
    assert_eq!(
        mgr.branch_window_balance(),
        0,
        "and the completion disarms it exactly once"
    );
}

/// The C1→C2 supersede finds the slot already taken and is a no-op, rather
/// than queueing a second optimizing compile of the same method — the
/// duplicate-install shape `admit`'s one-slot-per-method rule exists to
/// prevent. This is the ordering the worker loop actually runs: `finish`
/// (which now carries the promotion) and then `request_c2_upgrade`.
#[test]
fn the_supersede_does_not_double_queue_behind_the_publish_promotion() {
    let mgr = TieredCompilationManager::new(hot_c1_policy());
    let key = test_key();
    warm_to_c1_in_flight(&mgr, &key, 60);

    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    mgr.core.request_c2_upgrade(&key);

    let first = mgr.dequeue_compilation().expect("one C2 task");
    assert_eq!(first.target_tier, CompilationTier::C2);
    assert_eq!(
        mgr.dequeue_compilation(),
        None,
        "and only one — the method holds a single method-entry slot"
    );
    assert_eq!(mgr.branch_window_balance(), 1, "one window, not two");
}

/// A method the other doors have given up on cannot be re-enqueued through
/// the VM's eager deopt re-queue: `admit` asks the eligibility gates
/// itself, so there is no door left that skips them.
#[test]
fn an_ineligible_method_is_refused_by_the_eager_requeue_door() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.core
        .methods
        .lock()
        .entry(key.clone())
        .or_insert_with(|| MethodState::new(key.clone()))
        .ineligible = true;

    assert!(
        !mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        }),
        "an ineligible method must not be queued by any door"
    );
    assert_eq!(mgr.dequeue_compilation(), None);
    assert_eq!(mgr.ineligible_refusals(), 1);
}

/// The same for a method whose compiles have failed their whole retry
/// budget — the shape that let the deopt path re-queue forever.
#[test]
fn a_retry_exhausted_method_is_refused_by_the_eager_requeue_door() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.core
        .methods
        .lock()
        .entry(key.clone())
        .or_insert_with(|| MethodState::new(key.clone()))
        .tier_fail_count = MAX_TIER_FAIL_RETRIES;

    assert!(!mgr.enqueue_compilation(CompilationTask {
        method_key: key.clone(),
        target_tier: CompilationTier::C2,
        priority: CompilationPriority::Low,
        enqueue_time_ms: 0,
        osr_bci: None,
    }));
    assert_eq!(mgr.dequeue_compilation(), None);
    assert_eq!(mgr.ineligible_refusals(), 1);
}

/// An ineligible refusal reaches the process-wide scheduling counters, not
/// only the per-manager accessor.
///
/// `ineligible_refusals()` has no reader outside this module's own tests,
/// so before the producer landed the only way to see a refusal was to hold
/// the manager that made it. `metrics::summary()` and its JSON are what a
/// sink reads, and a row nobody increments cannot distinguish "never
/// happened" from "never wired" — which is the exact claim the row's doc
/// comment makes about a healthy run staying at zero.
///
/// Per thread (`SchedulingCapture`), for the reason
/// `drops_reach_the_process_wide_scheduling_counters` gives: `admit` runs
/// on the caller's thread, and the global table is everyone's.
#[test]
fn an_ineligible_refusal_reaches_the_process_wide_scheduling_counters() {
    let counts = crate::metrics::SchedulingCapture::start();

    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.core
        .methods
        .lock()
        .entry(key.clone())
        .or_insert_with(|| MethodState::new(key.clone()))
        .ineligible = true;

    assert!(!mgr.enqueue_compilation(CompilationTask {
        method_key: key.clone(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::Normal,
        enqueue_time_ms: 0,
        osr_bci: None,
    }));

    assert_eq!(
        counts.count("queue_refused_ineligible"),
        1,
        "the refusal must be visible in the metrics idiom, not only on the manager"
    );
    assert_eq!(mgr.ineligible_refusals(), 1, "and on the manager too");
    // A refusal is NOT a drop: nothing was queued, so the drop total must
    // not have moved. This is the property that keeps index 7 out of
    // `scheduling_dropped_total`, exactly like `queue_deduplicated`.
    assert_eq!(
        counts.count("queue_dropped_stale_install_epoch")
            + counts.count("queue_dropped_class_invalidated")
            + counts.count("queue_shutdown_abandoned"),
        0,
        "an ineligible refusal must not read as a dropped request"
    );
}

/// Bands still come out strictly by priority when every request is the same
/// age, which is every burst that arrives inside one aging window.
#[test]
fn same_age_requests_still_leave_in_strict_priority_order() {
    let mut q = CompilationQueue::new();
    let key = test_key();
    let at = |priority| CompilationTask {
        method_key: key.clone(),
        target_tier: CompilationTier::C2,
        priority,
        enqueue_time_ms: 1_000,
        osr_bci: None,
    };
    q.enqueue(at(CompilationPriority::Low), 0);
    q.enqueue(at(CompilationPriority::Normal), 0);
    q.enqueue(at(CompilationPriority::High), 0);

    assert_eq!(
        q.dequeue().unwrap().task.priority,
        CompilationPriority::High
    );
    assert_eq!(
        q.dequeue().unwrap().task.priority,
        CompilationPriority::Normal
    );
    assert_eq!(q.dequeue().unwrap().task.priority, CompilationPriority::Low);
}

/// A `Low` C1→C2 upgrade that has waited two bands' worth of time overtakes
/// a `High` OSR request that has just arrived. Without this, a warm-up
/// phase that keeps the High band non-empty means no C1→C2 upgrade ever
/// runs — the two kinds of request share the C2 lane.
#[test]
fn a_long_waiting_low_request_overtakes_a_fresh_high_one() {
    let mut q = CompilationQueue::new();
    let key = test_key();
    // The wait is SYNTHESISED, not slept for: `process_uptime_ms` is
    // monotonic, so a stamp far enough in the past is indistinguishable
    // from having really waited, and the test does not pay the time. The
    // sleep runs only in the rare case where the process is younger than
    // the wait being synthesised (this test running first in a fresh
    // binary).
    let wait = 2 * QUEUE_AGING_MS + 20;
    let now = process_uptime_ms();
    if now <= wait {
        std::thread::sleep(std::time::Duration::from_millis(wait - now + 1));
    }
    q.enqueue(
        CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::Low,
            // Never 0: `0` is the unset sentinel `enqueue` re-stamps.
            enqueue_time_ms: (process_uptime_ms() - wait).max(1),
            osr_bci: None,
        },
        0,
    );
    q.enqueue(
        CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: Some(7),
        },
        0,
    );

    assert_eq!(
        q.dequeue().unwrap().task.priority,
        CompilationPriority::Low,
        "a starving Low request must eventually outrank a fresh High one"
    );
}

/// `enqueue` stamps the arrival time a door left unset, so the aging term
/// has something to read and the queue-latency field stops being dead.
#[test]
fn enqueue_stamps_an_unset_arrival_time_and_keeps_a_supplied_one() {
    let mut q = CompilationQueue::new();
    let key = test_key();
    q.enqueue(
        CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        },
        0,
    );
    assert_ne!(
        q.dequeue().unwrap().task.enqueue_time_ms,
        0,
        "an unset arrival time is stamped on the way into the queue"
    );
    q.enqueue(
        CompilationTask {
            method_key: key,
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 42,
            osr_bci: None,
        },
        0,
    );
    assert_eq!(
        q.dequeue().unwrap().task.enqueue_time_ms,
        42,
        "a door that supplied one keeps it"
    );
    // r11-tier: a wall-clock stamp (the deopt re-queue's) is restamped.
    q.enqueue(
        CompilationTask {
            method_key: test_key(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::High,
            enqueue_time_ms: 1_758_000_000_000,
            osr_bci: None,
        },
        0,
    );
    let stamped = q.dequeue().map_or(0, |r| r.task.enqueue_time_ms);
    assert!(
        stamped != 0 && stamped < 1_758_000_000_000,
        "a stamp on another clock must be restamped, got {stamped}"
    );
}

/// `set_policy` has to re-mirror the bar onto the core, or an operator who
/// lowers `c2_threshold` at runtime gets the old bar on the publish door —
/// which is the same class of inert-knob bug this whole door exists to fix.
#[test]
fn lowering_the_c2_threshold_at_runtime_reaches_the_publish_door() {
    let policy = CompilationPolicy {
        c1_threshold: 5,
        c2_threshold: 1_000_000,
        c2_min_invocations: 1_000_000,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();
    warm_to_c1_in_flight(&mgr, &key, 60);

    mgr.set_policy(hot_c1_policy());
    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    let task = mgr
        .dequeue_compilation()
        .expect("the new bar must be the one the publish door asks");
    assert_eq!(task.target_tier, CompilationTier::C2);
}

// ── Queue priority ordering ──────────────────────────────────────────

#[test]
fn queue_priority_ordering() {
    let mgr = TieredCompilationManager::with_default_policy();
    let k1 = MethodKey::new("A", "a", "()V");
    let k2 = MethodKey::new("B", "b", "()V");
    let k3 = MethodKey::new("C", "c", "()V");

    mgr.enqueue_compilation(CompilationTask {
        method_key: k2.clone(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::Normal,
        enqueue_time_ms: 0,
        osr_bci: None,
    });
    mgr.enqueue_compilation(CompilationTask {
        method_key: k3.clone(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::Low,
        enqueue_time_ms: 0,
        osr_bci: None,
    });
    mgr.enqueue_compilation(CompilationTask {
        method_key: k1.clone(),
        target_tier: CompilationTier::C2,
        priority: CompilationPriority::High,
        enqueue_time_ms: 0,
        osr_bci: None,
    });

    // The three sit on two lanes (C1 and C2); the manual drain still
    // hands them out in priority order across both.
    let t1 = mgr.dequeue_compilation().unwrap();
    assert_eq!(t1.priority, CompilationPriority::High);
    let t2 = mgr.dequeue_compilation().unwrap();
    assert_eq!(t2.priority, CompilationPriority::Normal);
    let t3 = mgr.dequeue_compilation().unwrap();
    assert_eq!(t3.priority, CompilationPriority::Low);
}

// ── Enqueue / dequeue roundtrip ──────────────────────────────────────

#[test]
fn enqueue_dequeue_roundtrip() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.enqueue_compilation(CompilationTask {
        method_key: key.clone(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::Normal,
        enqueue_time_ms: 42,
        osr_bci: None,
    });
    let task = mgr.dequeue_compilation().unwrap();
    assert_eq!(task.method_key, key);
    assert_eq!(task.enqueue_time_ms, 42);
}

// ── Compilation complete updates tier ────────────────────────────────

#[test]
fn compilation_complete_updates_tier() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key); // create state
    mgr.compilation_complete(&key, CompilationTier::C1, 10);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
}

// ── Failed compile attempts do not fake "compiled" and DO retry ──────
//
// Regression coverage for the bg-compile-no-publish bug: a
// `compile_fn` that runs but bails (skip-listed method, resolver miss,
// code-cache cap, ...) must not be recorded as having reached `tier` —
// `current_tier` has to stay put so a later invocation gets another
// shot, and the per-tier compilation stat must not count a body that
// was never published.

#[test]
fn failed_compile_does_not_advance_tier_or_stats() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key); // create state
    mgr.core
        .complete_task(&key, CompilationTier::C1, 10, false, false, false);
    assert_eq!(
        mgr.current_tier(&key),
        CompilationTier::Interpreter,
        "a failed attempt must not advance current_tier"
    );
    assert_eq!(
        mgr.stats().c1_compilations.load(Ordering::Relaxed),
        0,
        "a failed attempt must not count as a C1 compilation"
    );
}

// ── Policy declines are recorded once, not charged to the retry budget ──
//
// A method the VM refuses on policy grounds (skip list, OSR denial) used
// to report `success=false` exactly like a failed compile, so it was
// enqueued and declined three times before `tier_fail_count` saturated.
// Two of those round-trips were waste, and the resulting
// `tier_fail_count=3` was indistinguishable from genuinely broken codegen
// in `hot_but_stuck_in_interpreter`.

#[test]
fn permanent_decline_bans_on_first_attempt_without_spending_retries() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();

    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    mgr.core
        .complete_task(&key, CompilationTier::C1, 0, false, false, true);

    {
        let methods = mgr.core.methods.lock();
        let state = &methods[&key];
        assert!(state.ineligible, "a policy decline must be recorded");
        assert_eq!(
            state.tier_fail_count, 0,
            "a policy decline must NOT spend the compile-failure retry budget"
        );
    }

    // ONE decline is enough: no further invocation may re-enqueue it.
    for _ in 0..10 {
        assert_eq!(
            mgr.on_method_invocation(&key),
            None,
            "an ineligible method must never be re-enqueued"
        );
    }
}

/// Interpreter round i6 wave 116, lane L2 (page `i107-L2`): the
/// waiting-on-class park, driven as the worker drives it (a completion
/// carrying the epoch read before the compile). Target independent: the
/// only producer today is the aarch64 block, the policy is here.
fn i116_park(mgr: &TieredCompilationManager, key: &MethodKey, epoch: u64) {
    mgr.core.finish(
        key,
        Completion {
            tier: CompilationTier::C1,
            osr_bci: None,
            osr: false,
            compile_time_ms: 0,
            compile_time_ns: 0,
            budget_ms: 0,
            success: false,
            declined_permanently: false,
            epoch_moved: false,
            waiting_on_class: Some(epoch),
            cap_refused: false,
            ticket: 0,
        },
    );
}

fn i116_manager(epoch: &Arc<AtomicU64>) -> TieredCompilationManager {
    let mgr = TieredCompilationManager::new(CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    });
    let source = Arc::clone(epoch);
    assert!(mgr.set_class_epoch_source(Box::new(move || source.load(Ordering::Acquire))));
    mgr
}

#[test]
fn i116_l2_waiting_on_class_is_not_charged_and_waits_for_a_definition() {
    let epoch = Arc::new(AtomicU64::new(7));
    let mgr = i116_manager(&epoch);
    let key = test_key();
    // More waits than the failure budget would ever allow.
    for round in 0..(MAX_TIER_FAIL_RETRIES + 3) {
        assert_eq!(
            mgr.on_method_invocation(&key),
            Some(CompilationTier::C1),
            "round {round}: a parked method is offered again once a class is defined"
        );
        i116_park(&mgr, &key, epoch.load(Ordering::Acquire));
        assert_eq!(
            mgr.on_method_invocation(&key),
            None,
            "round {round}: no class defined, no new compile"
        );
        epoch.fetch_add(1, Ordering::AcqRel);
    }
    let methods = mgr.core.methods.lock();
    assert_eq!(methods[&key].tier_fail_count, 0);
    assert_eq!(methods[&key].waiting_on_class_parks, MAX_TIER_FAIL_RETRIES + 3);
    drop(methods);
    assert_eq!(mgr.waiting_on_class_parks(), u64::from(MAX_TIER_FAIL_RETRIES + 3));
}

#[test]
fn i116_l2_a_parked_method_is_let_through_on_the_release_stride() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = i116_manager(&epoch);
    let key = test_key();
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    i116_park(&mgr, &key, 1);
    for i in 1..WAITING_ON_CLASS_RELEASE_OFFERS {
        assert_eq!(mgr.on_method_invocation(&key), None, "offer {i}");
    }
    // The epoch never moved (a loaded owner whose `<clinit>` ran meanwhile).
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
}

#[test]
fn i116_l2_waits_past_the_bound_are_charged_and_a_publish_ends_the_wait() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = i116_manager(&epoch);
    let key = test_key();
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    mgr.core.methods.lock().get_mut(&key).unwrap().waiting_on_class_parks =
        MAX_WAITING_ON_CLASS_PARKS;
    i116_park(&mgr, &key, 1);
    {
        let methods = mgr.core.methods.lock();
        assert_eq!(methods[&key].tier_fail_count, 1, "past the bound: charged");
        assert_eq!(methods[&key].waiting_on_class_epoch, None);
    }
    // Without an epoch source nothing parks: the worker sends `None`.
    let other = MethodKey::new("Other", "m", "()V");
    let plain = TieredCompilationManager::new(CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    });
    assert_eq!(plain.on_method_invocation(&other), Some(CompilationTier::C1));
    i116_park(&plain, &other, 5);
    // A park recorded with no source is released at the next offer.
    assert_eq!(plain.on_method_invocation(&other), Some(CompilationTier::C1));
    // A publish ends a wait.
    let key2 = test_key2();
    assert_eq!(mgr.on_method_invocation(&key2), Some(CompilationTier::C1));
    i116_park(&mgr, &key2, 1);
    mgr.core
        .complete_task(&key2, CompilationTier::C1, 0, true, false, false);
    assert_eq!(mgr.core.methods.lock()[&key2].waiting_on_class_epoch, None);
}

/// Interpreter round i6 wave 117, lane L9 (review; page
/// `i117-L9-a-one-shot-c2-retry-is-eaten-by-the-waiting-on-class-park`):
/// a late-bind retry (a deferred site linked, e.g. a static owner whose
/// `<clinit>` ran, which moves no class-definition epoch) is not eaten by
/// the park: it ends the park and is queued.
#[test]
fn i117_l9_a_late_bind_retry_ends_the_waiting_on_class_park() {
    let epoch = Arc::new(AtomicU64::new(3));
    let mgr = i116_manager(&epoch);
    let key = test_key();
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    i116_park(&mgr, &key, 3);
    assert_eq!(mgr.on_method_invocation(&key), None, "parked");
    assert_eq!(
        mgr.request_late_bind_recompile(&key),
        C2RetryAnswer::Enqueued,
        "the one-shot retry is queued, not refused as a parked offer"
    );
    assert_eq!(mgr.core.methods.lock()[&key].waiting_on_class_epoch, None);
}


#[test]
fn permanent_decline_does_not_ban_unrelated_methods() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let declined = test_key();
    let healthy = MethodKey::new("Other", "m", "()V");

    assert_eq!(
        mgr.on_method_invocation(&declined),
        Some(CompilationTier::C1)
    );
    mgr.core
        .complete_task(&declined, CompilationTier::C1, 0, false, false, true);

    // The other method is untouched and still compiles normally.
    assert_eq!(
        mgr.on_method_invocation(&healthy),
        Some(CompilationTier::C1)
    );
    mgr.core
        .complete_task(&healthy, CompilationTier::C1, 1, true, false, false);
    assert_eq!(mgr.current_tier(&healthy), CompilationTier::C1);
}

#[test]
fn failed_compile_is_retried_up_to_the_fail_limit() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();

    // 1st invocation crosses c1_threshold=1 and enqueues C1.
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    // Fail it MAX_TIER_FAIL_RETRIES - 1 times; each failure must still
    // leave the method eligible for another attempt (slot released,
    // current_tier untouched).
    for i in 0..(MAX_TIER_FAIL_RETRIES - 1) {
        mgr.core
            .complete_task(&key, CompilationTier::C1, 1, false, false, false);
        assert_eq!(
            mgr.on_method_invocation(&key),
            Some(CompilationTier::C1),
            "attempt {i}: should still be retried below the fail limit"
        );
    }
    // One more failure reaches MAX_TIER_FAIL_RETRIES — should_compile
    // must now give up permanently.
    mgr.core
        .complete_task(&key, CompilationTier::C1, 1, false, false, false);
    assert_eq!(
        mgr.on_method_invocation(&key),
        None,
        "should stop recommending compilation after the fail limit"
    );
    assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
}

#[test]
fn successful_compile_after_a_failure_resets_the_fail_count() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.core
        .complete_task(&key, CompilationTier::C1, 1, false, false, false);
    mgr.core
        .complete_task(&key, CompilationTier::C1, 5, true, false, false);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    let methods = mgr.core.methods.lock();
    assert_eq!(
        methods[&key].tier_fail_count, 0,
        "a later success should reset the fail streak"
    );
}

// Regression coverage for the OSR-starves-method-entry bug: a successful
// OSR compile publishes into the SEPARATE OSR artifact cache, so it must
// not stamp `current_tier = C2` — that made `should_compile` refuse every
// later method-entry recommendation while the method-entry cache was
// still empty, so each fresh invocation of a loop-heavy method (e.g.
// QuickBench sieve) re-entered the interpreter and re-OSR'd forever.
#[test]
fn osr_completion_does_not_suppress_method_entry_tiering() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();

    // A hot back-edge enqueues an OSR task (target tier C2) before the
    // invocation counter has recommended anything.
    let task = mgr.request_osr(&key, 42).expect("OSR task should enqueue");
    assert_eq!(task.osr_bci, Some(42));
    // The worker completes it successfully — artifact goes to the OSR
    // cache, `osr = true`.
    mgr.core
        .complete_task(&key, task.target_tier, 3, true, true, false);
    assert_eq!(
        mgr.current_tier(&key),
        CompilationTier::Interpreter,
        "an OSR publish must not advance the method-entry tier"
    );
    // The invocation counter must still be able to recommend the
    // method-entry compile.
    assert_eq!(
        mgr.on_method_invocation(&key),
        Some(CompilationTier::C1),
        "method-entry compilation must still be recommended after an OSR publish"
    );
    // And a successful method-entry completion advances the tier as usual.
    mgr.core
        .complete_task(&key, CompilationTier::C1, 2, true, false, false);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
}

/// RT-8. A swept body demotes the method so it can be compiled again.
///
/// The failure this catches is invisible rather than loud: without
/// `note_body_withdrawn` the method sits at C1 with no body, so
/// `on_method_invocation` — the only door the interpreter ever knocks on —
/// answers `None` forever, and the method is interpreted for the life of
/// the process. Nothing counts a method that is simply never offered, which
/// is why this is asserted here rather than left to a benchmark.
#[test]
fn a_withdrawn_body_demotes_the_method_so_it_can_be_compiled_again() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C1, 50);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    // At C1, the method-entry door is shut: `should_compile`'s C1 arm is
    // the C2 one, and it wants ~20 000 invocations from a counter that has
    // no writer once a C1 body exists.
    assert_eq!(mgr.on_method_invocation(&key), None);

    mgr.note_body_withdrawn(&key);
    assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);

    // The invocation count is deliberately KEPT, so the method re-admits at
    // its next stride instead of re-warming from zero — a method cold
    // enough to sweep would take a very long time to rebuild a count.
    let count = {
        let methods = mgr.core.methods.lock();
        methods[&key].invocation_count
    };
    assert!(
        count > 0,
        "note_body_withdrawn must not reset the invocation count"
    );
}

/// Round 11 wave 18, P1 (`r11-orch-callee-compiled-bodies-are-never-promoted`):
/// a body published by a door the manager did not schedule raises the tier,
/// so the drained count is judged by the C1 arm, and C2 is queued at the bar.
#[test]
fn a_body_published_outside_the_manager_is_promoted_by_its_drained_count() {
    if !crate::entry_counter::callee_promotion_enabled() {
        return; // the kill switch is set in this environment
    }
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.note_published_outside_manager(&key, CompilationTier::C1);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    assert_eq!(
        mgr.on_observed_invocations(&key, 600).recommended,
        None,
        "below the C2 bar a C1 body is left alone"
    );
    assert_eq!(
        mgr.on_observed_invocations(&key, 20_000).recommended,
        Some(CompilationTier::C2),
        "the drained count crosses the C2 bar through the C1 arm"
    );
    // Never lowers: a second single-pass publish leaves C1 (the queued C2
    // task is still in flight) and an `Interpreter` note is ignored.
    mgr.note_published_outside_manager(&key, CompilationTier::Interpreter);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
}


/// HANDLER8-1 census (JIT round 19 wave 9, lane handler9): an outside C1
/// publish that passes the supersede predicate is a candidate ONCE, and
/// its count's later crossing of the C2 bar is counted; a non-candidate
/// publish counts only as a publish. No switch is read on this path.
#[test]
fn the_outside_manager_census_counts_candidates_and_their_bar_crossing() {
    if !crate::entry_counter::callee_promotion_enabled() {
        return;
    }
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    let other = test_key2();
    mgr.note_published_outside_manager_with_c2_candidate(&key, CompilationTier::C1, true, true);
    mgr.note_published_outside_manager_with_c2_candidate(&key, CompilationTier::C1, true, true);
    mgr.note_published_outside_manager_with_c2_candidate(
        &other,
        CompilationTier::C1,
        false,
        true,
    );
    let census = &mgr.core.outside_c2;
    assert_eq!(census.c1_publishes.load(Ordering::Relaxed), 3);
    assert_eq!(
        census.candidates.load(Ordering::Relaxed),
        1,
        "once per method"
    );
    let _ = mgr.on_observed_invocations(&key, 600);
    assert_eq!(census.crossed_bar.load(Ordering::Relaxed), 0);
    let c2_bar = u64::from(mgr.policy().c2_threshold);
    let _ = mgr.on_observed_invocations(&key, c2_bar);
    assert_eq!(census.crossed_bar.load(Ordering::Relaxed), 1);
    let _ = mgr.on_observed_invocations(&key, c2_bar.saturating_mul(2));
    assert_eq!(
        census.crossed_bar.load(Ordering::Relaxed),
        1,
        "counted once"
    );
    let _ = mgr.on_observed_invocations(&other, c2_bar);
    assert_eq!(
        census.crossed_bar.load(Ordering::Relaxed),
        1,
        "not a candidate"
    );
    let line = mgr.outside_manager_c2_census_line();
    assert!(line.contains("c1-publishes=3 candidates=1"), "{line}");
    assert!(line.contains("crossed-bar=1 never-crossed=0"), "{line}");    }


/// HANDLER8-1 supersede door: admits through `request_c2_upgrade` under a
/// per-VM budget; a refusal by the door's own gates does not spend it.
#[test]
fn the_outside_manager_supersede_is_bounded_by_its_budget() {
    if !crate::entry_counter::callee_promotion_enabled() {
        return;
    }
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    let other = test_key2();
    mgr.note_published_outside_manager(&key, CompilationTier::C1);
    mgr.note_published_outside_manager(&other, CompilationTier::C1);
    let census = &mgr.core.outside_c2;
    census.budget_left.store(1, Ordering::Relaxed);
    assert!(mgr.core.request_outside_manager_c2_upgrade(&key));
    assert!(mgr.core.methods.lock()[&key].queued_for_compilation);
    assert!(
        !mgr.core.request_outside_manager_c2_upgrade(&other),
        "the budget is spent"
    );
    assert_eq!(census.budget_refused.load(Ordering::Relaxed), 1);
    assert!(!mgr.core.methods.lock()[&other].queued_for_compilation);
    // A gate refusal (already queued) refunds its unit.
    census.budget_left.store(1, Ordering::Relaxed);
    assert!(!mgr.core.request_outside_manager_c2_upgrade(&key));
    assert_eq!(census.upgrades_refused.load(Ordering::Relaxed), 1);
    assert_eq!(census.budget_left.load(Ordering::Relaxed), 1);
    assert_eq!(census.upgrades_admitted.load(Ordering::Relaxed), 1);
}

/// Wave-9 follow-up: a candidate whose profile is thinner than the manager
/// path's is still a census candidate, and with the supersede off nothing
/// is counted as refused (the gate is asked only of an armed supersede).
#[test]
fn a_cold_profile_candidate_is_counted_but_not_refused_while_unarmed() {
    if !crate::entry_counter::callee_promotion_enabled() || outside_manager_c2_upgrade_enabled()
    {
        return;
    }
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.note_published_outside_manager_with_c2_candidate(
        &key,
        CompilationTier::C1,
        true,
        false,
    );
    let census = &mgr.core.outside_c2;
    assert_eq!(census.candidates.load(Ordering::Relaxed), 1);
    assert_eq!(census.profile_refused.load(Ordering::Relaxed), 0);
    assert!(!mgr.core.methods.lock()[&key].queued_for_compilation);
}

/// MON11-4: HotSpot's Tier4 numbers, with the drain's 8:1 share taken
/// back off the count.
#[test]
fn the_tier4_i_plus_b_predicate_is_hotspots() {
    // 715 calls of a 20-iteration loop: 715 + 14 300 >= 15 000.
    let b = 715 * 20;
    assert!(tier4_i_plus_b_due(715 + (b >> 3), b));
    assert!(!tier4_i_plus_b_due(700 + ((700 * 20) >> 3), 700 * 20));
    // Never below 600 entries, however many back edges.
    assert!(!tier4_i_plus_b_due(599 + (1_000_000 >> 3), 1_000_000));
    assert!(tier4_i_plus_b_due(600 + (1_000_000 >> 3), 1_000_000));
    assert!(tier4_i_plus_b_due(u64::MAX, u64::MAX), "saturates");
}


/// HANDLER8-2: the drain's raw back-edge word is kept for the enqueue
/// line, also on a report that says nothing new about the count.
#[test]
fn the_drained_back_edge_word_is_recorded_for_the_enqueue_line() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    let _ = mgr.on_observed_invocations_with_back_edges(&key, 5, 77);
    assert_eq!(
        mgr.core.methods.lock()[&key].last_drained_back_edges,
        Some(77)
    );
    let _ = mgr.on_observed_invocations_with_back_edges(&key, 5, 90);
    assert_eq!(
        mgr.core.methods.lock()[&key].last_drained_back_edges,
        Some(90)
    );
    let _ = mgr.on_observed_invocations(&key, 6);
    assert_eq!(
        mgr.core.methods.lock()[&key].last_drained_back_edges,
        Some(90),
        "a report without the word keeps the last one"
    );
}

/// P1: an OPTIMIZING body published outside the manager settles the method
/// instead of earning a C2 recompile of itself, and a later single-pass
/// note cannot lower it.
#[test]
fn an_optimizing_body_published_outside_the_manager_settles_it() {
    if !crate::entry_counter::callee_promotion_enabled() {
        return;
    }
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key2();
    mgr.note_published_outside_manager(&key, CompilationTier::C2);
    mgr.note_published_outside_manager(&key, CompilationTier::C1);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
    let verdict = mgr.on_observed_invocations(&key, 50_000);
    assert_eq!(verdict.recommended, None);
    assert_ne!(verdict.settled_generation, 0, "a C2 method is settled");
    // A withdrawal still demotes it.
    mgr.note_body_withdrawn(&key);
    assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
}

/// **RT-8 soak, defect 3.** A method at C2 is SETTLED, and its call sites
/// hold that stamp and skip this manager until the generation moves. A
/// withdrawal that demoted the state without moving the generation was
/// therefore never read: the method stayed interpreted for good. Remove
/// the bump from `note_body_withdrawn` and the second assertion fails.
#[test]
fn a_withdrawal_expires_the_settled_stamp_so_the_demotion_is_read() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C2, 50);
    let stamp = mgr
        .on_method_invocation_settling(&key, 0)
        .settled_generation;
    assert!(
        mgr.tiering_settled(stamp),
        "a method at C2 is settled; the precondition of this test"
    );
    mgr.note_body_withdrawn(&key);
    assert!(
        !mgr.tiering_settled(stamp),
        "after a withdrawal the call site must ask again, or it never \
         learns the method was demoted"
    );
}

/// r11 wave 3 (lane tier): a trap expires the settled stamps only when it
/// unsettles its own method. Every trap used to bump the generation, so a
/// trap storm sent every settled method in the process back to the
/// `methods` mutex once per trap.
#[test]
fn a_trap_expires_the_settled_stamps_only_when_it_unsettles_its_method() {
    let mgr = TieredCompilationManager::with_default_policy();
    let settled = test_key();
    mgr.on_method_invocation(&settled);
    mgr.compilation_complete(&settled, CompilationTier::C2, 50);
    let stamp = mgr
        .on_method_invocation_settling(&settled, 0)
        .settled_generation;
    assert!(
        mgr.tiering_settled(stamp),
        "a method at C2 is settled; the precondition of this test"
    );

    // Traps on another, interpreted method change nothing this stamp
    // depends on.
    let other = MethodKey::new("com/example/Storm", "run", "()V");
    mgr.on_method_invocation(&other);
    for _ in 0..3 {
        mgr.on_deoptimization(
            &other,
            DeoptReason::ClassCheck,
            7,
            DeoptAction::RecompileAndReinterpret,
        );
    }
    assert!(
        mgr.tiering_settled(stamp),
        "a trap that unsettles no method expires no stamp"
    );

    // Evicting the settled method's own body demotes it, so its call
    // sites must ask again.
    mgr.on_deoptimization(
        &settled,
        DeoptReason::ClassCheck,
        7,
        DeoptAction::RecompileAndReinterpret,
    );
    assert!(!mgr.tiering_settled(stamp), "the demotion is read");
}

/// The pressure note expires stamps on the falling edge only: every drain
/// calls it, so a bump on every call at the cap would unsettle the whole
/// process each stride, and a bump on nothing would leave the methods
/// deferred at the cap waiting for good.
#[test]
fn only_the_cap_clearing_expires_the_settled_stamps() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C2, 50);
    let stamp = mgr
        .on_method_invocation_settling(&key, 0)
        .settled_generation;

    assert!(
        !mgr.note_code_cache_pressure(false),
        "no edge: never at the cap"
    );
    assert!(
        !mgr.note_code_cache_pressure(true),
        "rising edge: nothing to release"
    );
    assert!(!mgr.note_code_cache_pressure(true), "still at the cap");
    assert!(
        mgr.tiering_settled(stamp),
        "none of those may expire a stamp"
    );

    assert!(mgr.note_code_cache_pressure(false), "the falling edge");
    assert!(!mgr.tiering_settled(stamp), "and it expires the stamps");
    assert!(!mgr.note_code_cache_pressure(false), "once");
}

/// A key the manager has no state for — an eagerly compiled first call
/// never reaches `on_method_invocation` — must be ignored rather than
/// create an entry. A sweeper runs over the whole cache, so it will hand
/// this function such keys routinely.
#[test]
fn withdrawing_a_body_for_an_unknown_method_does_nothing() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.note_body_withdrawn(&key);
    assert!(
        mgr.core.methods.lock().get(&key).is_none(),
        "an unknown key must not be inserted; the map is walked elsewhere"
    );
}

// ── Deoptimization and trap accounting ───────────────────────────────

#[test]
fn deoptimization_drops_to_interpreter() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C2, 50);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C2);

    mgr.on_deoptimization(
        &key,
        DeoptReason::ClassCheck,
        7,
        DeoptAction::RecompileAndReinterpret,
    );
    assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
}

/// The same speculation failing again and again is a property of the code:
/// `PER_BCI_TRAP_LIMIT` counted traps at one site end the method's C2 career.
#[test]
fn repeated_traps_at_one_bci_trigger_c2_bailout() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    for i in 0..PER_METHOD_TRAP_CUTOFF {
        assert!(
            !mgr.core.methods.lock()[&key].c2_bailout,
            "trap {i} is still below the per-method cutoff"
        );
        mgr.compilation_complete(&key, CompilationTier::C2, 10);
        mgr.on_deoptimization(
            &key,
            DeoptReason::NullCheck,
            12,
            DeoptAction::RecompileAndReinterpret,
        );
    }
    assert!(mgr.core.methods.lock()[&key].c2_bailout);
}

/// Traps spread over different sites are charged to the method, which has
/// a larger allowance than any one site.
#[test]
fn traps_across_sites_use_the_per_method_cutoff() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    for bci in 0..PER_METHOD_TRAP_CUTOFF - 1 {
        mgr.on_deoptimization(
            &key,
            DeoptReason::BoundsCheck,
            bci,
            DeoptAction::RecompileAndReinterpret,
        );
    }
    assert!(
        !mgr.core.methods.lock()[&key].c2_bailout,
        "one trap per site, and still below the per-method cutoff"
    );
    mgr.on_deoptimization(
        &key,
        DeoptReason::BoundsCheck,
        9_999,
        DeoptAction::RecompileAndReinterpret,
    );
    assert!(mgr.core.methods.lock()[&key].c2_bailout);
}

/// Soft deopts are not charged, and — since nothing charges them — nothing
/// throws their body away either. A soft OSR exit, a debugger transfer and
/// a pending exception's handler hand-off all keep the compiled body and
/// leave the tier where it was.
#[test]
fn soft_deopts_do_not_set_c2_bailout() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C2, 10);
    for _ in 0..50 {
        mgr.on_deoptimization(&key, DeoptReason::OsrExit, 30, DeoptAction::Reinterpret);
    }
    {
        let methods = mgr.core.methods.lock();
        let state = &methods[&key];
        assert!(!state.c2_bailout, "soft OSR exits must not ban C2");
        assert_eq!(state.deopt_count, 0);
        assert_eq!(state.current_tier, CompilationTier::C2, "the body was kept");
    }
    for _ in 0..50 {
        mgr.on_deoptimization(
            &key,
            DeoptReason::TransferToInterpreter,
            30,
            DeoptAction::RecompileAndReinterpret,
        );
        mgr.on_deoptimization(
            &key,
            DeoptReason::PendingException,
            30,
            DeoptAction::MakeNotEntrant,
        );
    }
    {
        let methods = mgr.core.methods.lock();
        let state = &methods[&key];
        assert!(!state.c2_bailout);
        assert_eq!(state.deopt_count, 0);
        assert_eq!(
            state.current_tier,
            CompilationTier::C2,
            "a reason nothing charges must not evict the body either: \
             evicting without charging is the unbounded recompile thrash"
        );
    }
    assert_eq!(
        mgr.stats().soft_deoptimizations.load(Ordering::Relaxed),
        150
    );
    assert!(
        mgr.request_osr(&key, 30).is_some(),
        "OSR stays available: nothing here was a counted trap"
    );
}

/// The two trap predicates agree by construction: a reason nothing charges
/// is a reason nothing evicts. The reverse used to hold for exactly one
/// pair (a soft `OsrExit`), which is what let a hot `try`/`catch` evict and
/// recompile forever with nothing charged and nothing backing off.
#[test]
fn an_uncharged_reason_never_evicts_the_body() {
    for action in [
        DeoptAction::Reinterpret,
        DeoptAction::RecompileAndReinterpret,
        DeoptAction::MakeNotEntrant,
        DeoptAction::MakeNotCompilable,
    ] {
        for reason in [
            DeoptReason::PendingException,
            DeoptReason::TransferToInterpreter,
        ] {
            assert!(!deopt_reason_is_charged(reason));
            assert!(!deopt_is_counted_trap(reason, action));
            assert!(
                !deopt_evicts_method_body(reason, action),
                "{reason:?}/{action:?} may not throw a body away that nothing charges for"
            );
        }
    }
    // The soft-OSR-exit exemption is unchanged, and a charged reason still
    // evicts so the next call recompiles against a profile that saw it.
    assert!(!deopt_evicts_method_body(
        DeoptReason::OsrExit,
        DeoptAction::Reinterpret
    ));
    assert!(deopt_evicts_method_body(
        DeoptReason::OsrExit,
        DeoptAction::MakeNotCompilable
    ));
    assert!(deopt_evicts_method_body(
        DeoptReason::UncommonTrap,
        DeoptAction::Reinterpret
    ));
}

/// Trap counts decay, so a burst early in a long run does not ban a method
/// for the rest of it.
#[test]
fn trap_counts_decay_with_invocations() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    for _ in 0..PER_BCI_TRAP_LIMIT - 1 {
        mgr.on_deoptimization(
            &key,
            DeoptReason::ClassCheck,
            5,
            DeoptAction::RecompileAndReinterpret,
        );
    }
    // The method keeps running long enough for its counts to decay...
    let seen = mgr
        .method_states()
        .into_iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, _, count)| count)
        .expect("state for key");
    let _ = mgr.on_method_invocation_observed(&key, seen + TRAP_DECAY_INVOCATIONS);
    // ...so one more trap at the same site is not the one that reaches the
    // limit.
    mgr.on_deoptimization(
        &key,
        DeoptReason::ClassCheck,
        5,
        DeoptAction::RecompileAndReinterpret,
    );
    let methods = mgr.core.methods.lock();
    let state = &methods[&key];
    assert!(
        !state.c2_bailout,
        "decayed counts must not reach the per-bci limit"
    );
    assert_eq!(
        state.trap_counts[&(DeoptReason::ClassCheck, 5)],
        (PER_BCI_TRAP_LIMIT - 1) / 2 + 1
    );
}

#[test]
fn deopt_resets_queued_state() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.enqueue_compilation(CompilationTask {
        method_key: key.clone(),
        target_tier: CompilationTier::C2,
        priority: CompilationPriority::High,
        enqueue_time_ms: 0,
        osr_bci: None,
    });
    mgr.on_deoptimization(
        &key,
        DeoptReason::ReceiverTypeChanged,
        0,
        DeoptAction::RecompileAndReinterpret,
    );

    let methods = mgr.core.methods.lock();
    let state = &methods[&key];
    assert!(!state.queued_for_compilation);
    assert!(state.queued_tier.is_none());
    drop(methods);
    assert!(
        mgr.queue_empty(),
        "the queued request is dropped, not left behind"
    );
}

// ── Tier-4 compile-time guard (jit-inlining-and-ir-calls) ────────────

/// A C2 compile that stays inside `MAX_C2_COMPILE_TIME_MS` must leave the
/// method eligible for C2. Repeated compiles that exceed it demote the
/// method to C1 for the rest of the process, through the SAME `c2_bailout`
/// flag the trap limits use (so every existing degradation path honours it
/// with no additional wiring). One overrun is not enough.
#[test]
fn slow_c2_compile_demotes_to_c1() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);

    // Inside the budget → still a C2 method.
    mgr.compilation_complete(&key, CompilationTier::C2, MAX_C2_COMPILE_TIME_MS);
    assert!(
        !mgr.core.methods.lock()[&key].c2_bailout,
        "a C2 compile at exactly the budget must not demote"
    );

    // One overrun is not evidence.
    mgr.compilation_complete(&key, CompilationTier::C2, MAX_C2_COMPILE_TIME_MS + 1);
    assert!(
        !mgr.core.methods.lock()[&key].c2_bailout,
        "a single over-budget compile must not demote"
    );

    // Repeated overruns are, and the statistic counts the demotion
    // alongside the trap-driven ones.
    let before = mgr.stats().c2_bailouts.load(Ordering::Relaxed);
    for _ in 1..C2_BUDGET_OVERRUNS_BEFORE_DEMOTION {
        mgr.compilation_complete(&key, CompilationTier::C2, MAX_C2_COMPILE_TIME_MS + 1);
    }
    assert!(
        mgr.core.methods.lock()[&key].c2_bailout,
        "repeated C2 compiles over MAX_C2_COMPILE_TIME_MS must demote the method to C1"
    );
    assert_eq!(
        mgr.stats().c2_bailouts.load(Ordering::Relaxed),
        before + 1,
        "the compile-time demotion must be counted as a c2 bailout"
    );
}

/// zt t18/pl8: the perf-data `sun.ci.*` statistics follow HotSpot's
/// `CompileBroker::collect_statistics`: every attempt's time in the total,
/// the per-kind times of the published compiles only, a bailout for a
/// compile that ran and published nothing (an epoch-moved install refusal
/// included), none for a permanent decline.
#[test]
fn zt18pl8_bailouts_and_per_kind_times_follow_collect_statistics() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::new("craton/test/Pl8CompileStats", "m", "()V");
    mgr.core
        .complete_task(&key, CompilationTier::C1, 2, true, false, false);
    mgr.core
        .complete_task(&key, CompilationTier::C2, 3, true, true, false);
    mgr.core
        .complete_task(&key, CompilationTier::C2, 5, false, false, false);
    mgr.core
        .complete_task(&key, CompilationTier::C1, 7, false, false, true);
    let stats = mgr.stats();
    let load = |cell: &AtomicU64| cell.load(Ordering::Relaxed);
    assert_eq!(load(&stats.standard_compile_time_ns), 2_000_000);
    assert_eq!(load(&stats.osr_compile_time_ns), 3_000_000);
    assert_eq!(load(&stats.total_compile_time_ns), 17_000_000);
    assert_eq!(load(&stats.total_bailouts), 1, "the decline is no bailout");
    let failed = stats.last_failed.lock().clone().expect("a bailout");
    assert_eq!(
        (&*failed.class_name, &*failed.method_name, failed.osr),
        ("craton/test/Pl8CompileStats", "m", false)
    );
    mgr.core.finish(
        &key,
        Completion {
            tier: CompilationTier::C2,
            osr_bci: Some(4),
            osr: true,
            compile_time_ms: 0,
            compile_time_ns: 400,
            budget_ms: 0,
            success: false,
            declined_permanently: false,
            epoch_moved: true,
            waiting_on_class: None,
            cap_refused: false,
            ticket: 0,
        },
    );
    assert_eq!(load(&stats.total_bailouts), 2, "a refused install bails");
    assert!(stats.last_failed.lock().as_ref().is_some_and(|f| f.osr));
    assert_eq!(load(&stats.osr_compile_time_ns), 3_000_000);
    assert_eq!(load(&stats.total_compile_time_ns), 17_000_400);
}

/// Only a successful method-entry compile is charged. A slow failure is
/// already bounded by `tier_fail_count`, and an OSR compile builds a
/// different artifact.
#[test]
fn failed_and_osr_c2_compiles_are_not_charged_to_the_budget() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    for _ in 0..(C2_BUDGET_OVERRUNS_BEFORE_DEMOTION * 3) {
        mgr.core.complete_task(
            &key,
            CompilationTier::C2,
            MAX_C2_COMPILE_TIME_MS * 10,
            false,
            false,
            false,
        );
        mgr.core.complete_task(
            &key,
            CompilationTier::C2,
            MAX_C2_COMPILE_TIME_MS * 10,
            true,
            true,
            false,
        );
    }
    let methods = mgr.core.methods.lock();
    assert_eq!(methods[&key].c2_budget_overruns, 0);
    assert!(!methods[&key].c2_bailout);
}

/// The budget is charged `Completion::budget_ms` — compile-thread CPU time
/// from the worker — not the wall-clock time the statistics record. A
/// descheduled compile thread is not a slow compile.
#[test]
fn the_c2_budget_is_charged_cpu_time_not_wall_time() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    for _ in 0..(C2_BUDGET_OVERRUNS_BEFORE_DEMOTION * 2) {
        mgr.core.finish(
            &key,
            Completion {
                tier: CompilationTier::C2,
                osr_bci: None,
                osr: false,
                compile_time_ms: MAX_C2_COMPILE_TIME_MS * 20,
                compile_time_ns: 0,
                budget_ms: 1,
                success: true,
                declined_permanently: false,
                epoch_moved: false,
                waiting_on_class: None,
                cap_refused: false,
                ticket: 0,
            },
        );
    }
    assert!(!mgr.core.methods.lock()[&key].c2_bailout);
}


/// Queue latency is charged per lane, from the request's own stamp, and a
/// stamp from another clock (or no stamp) counts the dispatch but no wait.
#[test]
fn queue_wait_is_charged_per_lane_from_the_enqueue_stamp() {
    let mgr = TieredCompilationManager::with_default_policy();
    let task = |stamp: u64| CompilationTask {
        method_key: test_key(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::Normal,
        enqueue_time_ms: stamp,
        osr_bci: None,
    };
    mgr.core.note_queue_wait(C1_LANE, &task(100), 130);
    mgr.core.note_queue_wait(C1_LANE, &task(100), 110);
    // Another clock's stamp (ahead of "now") and the unset sentinel.
    mgr.core.note_queue_wait(C2_LANE, &task(5_000), 130);
    mgr.core.note_queue_wait(C2_LANE, &task(0), 130);
    let s = mgr.stats();
    assert_eq!(s.dispatched[C1_LANE].load(Ordering::Relaxed), 2);
    assert_eq!(s.queue_wait_ms_total[C1_LANE].load(Ordering::Relaxed), 40);
    assert_eq!(s.queue_wait_ms_max[C1_LANE].load(Ordering::Relaxed), 30);
    assert_eq!(s.dispatched[C2_LANE].load(Ordering::Relaxed), 2);
    assert_eq!(s.queue_wait_ms_total[C2_LANE].load(Ordering::Relaxed), 0);
    assert_eq!(s.queue_wait_ms_max[C2_LANE].load(Ordering::Relaxed), 0);
}

/// An unpublished compile during which the install epoch moved is the
/// flush barrier refusing a stale body, not a compiler failure, and must
/// not spend the method's retry budget -- `retire_stale`'s rule for a
/// request dropped before dispatch, applied to one caught mid-compile.
/// Charged, it took `MAX_TIER_FAIL_RETRIES` epoch moves during warmup to
/// settle a healthy method for the life of the process.
#[test]
fn a_compile_overtaken_by_an_epoch_move_spends_no_retry() {
    let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
    let key = MethodKey::new("craton/test/EpochMovedMidCompile", "m", "()V");
    for _ in 0..(MAX_TIER_FAIL_RETRIES * 2) {
        let task = mgr
            .request_osr(&key, 3)
            .expect("an uncharged outcome leaves the method admissible");
        assert_task_eq(mgr.next_fresh_task(), Some(task));
        mgr.core.finish(
            &key,
            Completion {
                tier: CompilationTier::C2,
                osr_bci: Some(3),
                osr: true,
                compile_time_ms: 1,
                compile_time_ns: 0,
                budget_ms: 1,
                success: false,
                declined_permanently: false,
                epoch_moved: true,
                waiting_on_class: None,
                cap_refused: false,
                ticket: 0,
            },
        );
        let methods = mgr.core.methods.lock();
        assert_eq!(methods[&key].tier_fail_count, 0);
        assert!(!methods[&key].osr_in_flight, "OSR slot released");
    }
    // The same outcome WITHOUT the epoch move is still a failure.
    let _ = mgr.request_osr(&key, 3).expect("admissible");
    let _ = mgr.next_fresh_task();
    mgr.core.finish(
        &key,
        Completion {
            tier: CompilationTier::C2,
            osr_bci: Some(3),
            osr: true,
            compile_time_ms: 1,
            compile_time_ns: 0,
            budget_ms: 1,
            success: false,
            declined_permanently: false,
            epoch_moved: false,
            waiting_on_class: None,
            cap_refused: false,
            ticket: 0,
        },
    );
    // r10: this charge lands on `osr_fail_count`, not `tier_fail_count`. The
    // completion being charged is an OSR one (`request_osr`, `osr: true`), and
    // OSR failures no longer spend the method's ordinary C1->C2 retries -- an
    // OSR compile that cannot be built is not evidence that a whole-method
    // recompile cannot be either. Asserting BOTH counters here is the point:
    // it pins the split rather than merely following it, and it still holds
    // this test's own claim, that the epoch-moved outcomes above spent
    // nothing while this genuine failure spends exactly one.
    let methods = mgr.core.methods.lock();
    assert_eq!(
        methods[&key].osr_fail_count, 1,
        "the OSR failure is charged"
    );
    assert_eq!(
        methods[&key].tier_fail_count, 0,
        "and it does not spend a whole-method retry"
    );
}

/// JIT round 22 wave 11, lane runtime (`Completion::cap_refused`): a
/// method-entry compile the cap gate refused is deferred at the cap and
/// never charged, even when the cap no longer reads full by the time
/// `finish` asks (a drain released room in between, the race that settled
/// `code_cache_sweep_phases`'s phase-3 methods). No cap is configured in
/// this test process, so `jit_code_cache_at_capacity` answers `false`
/// throughout: only the witness can defer.
#[test]
fn r22w11_a_witnessed_cap_refusal_is_deferred_not_charged() {
    let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
    let key = MethodKey::new("craton/test/CapRefusedMidCompile", "m", "()V");
    for _ in 0..(MAX_TIER_FAIL_RETRIES * 2) {
        {
            let mut methods = mgr.core.methods.lock();
            let state = methods
                .entry(key.clone())
                .or_insert_with(|| MethodState::new(key.clone()));
            state.queued_for_compilation = true;
            state.queued_tier = Some(CompilationTier::C1);
        }
        mgr.core.finish(
            &key,
            Completion {
                tier: CompilationTier::C1,
                osr_bci: None,
                osr: false,
                compile_time_ms: 1,
                budget_ms: 1,
                success: false,
                declined_permanently: false,
                epoch_moved: false,
                waiting_on_class: None,
                cap_refused: true,
                ticket: 0,
                compile_time_ns: 0,
            },
        );
        let methods = mgr.core.methods.lock();
        assert_eq!(methods[&key].tier_fail_count, 0, "a cap refusal is never charged");
        assert!(methods[&key].deferred_at_cap, "it is deferred at the cap");
    }
}


/// The compile-time guard is C2-only: a slow C1 compile is not a reason to
/// refuse the optimizing tier (C1 and C2 use different backends, and the
/// single-pass backend is the fallback the bailout demotes *to*).
#[test]
fn slow_c1_compile_does_not_demote() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C1, MAX_C2_COMPILE_TIME_MS * 10);
    assert!(!mgr.core.methods.lock()[&key].c2_bailout);
}

#[test]
fn the_thread_cpu_clock_is_monotonic_where_it_is_wired() {
    // Platforms without a per-thread clock return `None`, and the budget
    // falls back to wall time; nothing to assert there.
    let Some(before) = current_thread_cpu_time() else {
        return;
    };
    let mut x = 0u64;
    for i in 0..2_000_000u64 {
        x = x.wrapping_mul(31).wrapping_add(i);
    }
    std::hint::black_box(x);
    let after = current_thread_cpu_time().expect("a clock that answered once answers again");
    assert!(after >= before);
}


/// zs s9/rs2: a standby worker stays parked below the depth, is promoted
/// by a backlog, and a parked one leaves on shutdown.
#[test]
fn zs9rs2_standby_worker_parks_until_a_backlog_or_shutdown() {
    let lane = Arc::new(CompileLane::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    lane.standby_depth.store(2, Ordering::Relaxed);
    let spawn = |k: usize| {
        let (lane, shutdown) = (Arc::clone(&lane), Arc::clone(&shutdown));
        lane.standby_parked.fetch_add(1, Ordering::AcqRel);
        std::thread::spawn(move || lane.park_standby(&shutdown, k, 0))
    };
    let w1 = spawn(1);
    let w2 = spawn(2);
    // Below the depth: nobody is promoted.
    lane.maybe_promote_standby(1);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!w1.is_finished() && !w2.is_finished());
    assert_eq!(*lane.standby_promoted.lock(), 0);
    // A backlog promotes exactly one (worker 1).
    lane.maybe_promote_standby(2);
    assert!(w1.join().unwrap(), "worker 1 promoted");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!w2.is_finished(), "worker 2 still parked");
    assert_eq!(lane.standby_parked.load(Ordering::Acquire), 1);
    // Shutdown releases the parked one with false.
    shutdown.store(true, Ordering::Release);
    drop(lane.standby_promoted.lock());
    lane.standby_wake.notify_all();
    assert!(!w2.join().unwrap(), "worker 2 left on shutdown");
    // With no parked worker a backlog promotes nothing.
    lane.standby_parked.store(0, Ordering::Release);
    lane.maybe_promote_standby(100);
    assert_eq!(*lane.standby_promoted.lock(), 1);
}

/// JIT r23 wave 11 (page `c2-standby-counters-are-process-wide`): the
/// promotion count lives on the lane that promoted, so one VM's
/// promotions never show in another VM's line; without an owner the line
/// reports zeros.
#[test]
fn r23w11_standby_promotions_are_counted_per_lane_not_per_process() {
    let a = CompileLane::new();
    let b = CompileLane::new();
    a.standby_depth.store(1, Ordering::Relaxed);
    a.standby_parked.store(2, Ordering::Release);
    a.maybe_promote_standby(1);
    a.maybe_promote_standby(1);
    assert_eq!(a.standby_promoted_total.load(Ordering::Relaxed), 2);
    assert_eq!(b.standby_promoted_total.load(Ordering::Relaxed), 0, "lane b saw none");
    assert_eq!(b.standby_started_total.load(Ordering::Relaxed), 0);
    let line = c2_standby_line(None);
    assert!(line.ends_with("started=0 promoted=0"), "{line}");
}


/// zt t2/fp: under the scale switch the k-th promotion waits for a
/// backlog of `depth x live workers`; without it every promotion waits
/// for `depth` alone (the positive control).
#[test]
fn zt2fp_scaled_standby_promotion_waits_for_depth_times_live() {
    for scale in [false, true] {
        let lane = CompileLane::new();
        lane.standby_depth.store(2, Ordering::Relaxed);
        lane.standby_scale.store(scale, Ordering::Relaxed);
        lane.standby_parked.store(3, Ordering::Release);
        lane.maybe_promote_standby(2);
        assert_eq!(*lane.standby_promoted.lock(), 1, "first at depth");
        lane.maybe_promote_standby(3);
        let second = *lane.standby_promoted.lock();
        assert_eq!(second, if scale { 1 } else { 2 }, "scale={scale}");
        lane.maybe_promote_standby(4);
        let third = *lane.standby_promoted.lock();
        assert_eq!(third, if scale { 2 } else { 3 }, "scale={scale}");
        lane.maybe_promote_standby(6);
        assert_eq!(*lane.standby_promoted.lock(), 3, "scale={scale}");
        assert_eq!(lane.standby_parked.load(Ordering::Acquire), 0);
    }
}

/// zt t2/fp: a worker parked by an earlier compiler start leaves when a
/// later start bumps the lane's epoch, even with the shutdown flag
/// cleared again by that start; a worker of the current epoch stays.
#[test]
fn zt2fp_a_new_start_releases_an_earlier_starts_parked_worker() {
    let lane = Arc::new(CompileLane::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let park = |epoch: usize| {
        let (lane, shutdown) = (Arc::clone(&lane), Arc::clone(&shutdown));
        std::thread::spawn(move || lane.park_standby(&shutdown, 1, epoch))
    };
    let old = park(0);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!old.is_finished(), "parked in its own epoch");
    {
        let _promoted = lane.standby_promoted.lock();
        lane.standby_epoch.fetch_add(1, Ordering::AcqRel);
    }
    lane.standby_wake.notify_all();
    assert!(!old.join().unwrap(), "the old epoch's worker left");
    let current = park(1);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!current.is_finished(), "the current epoch's worker stays");
    lane.standby_parked.store(1, Ordering::Release);
    lane.standby_depth.store(1, Ordering::Relaxed);
    lane.maybe_promote_standby(1);
    assert!(current.join().unwrap(), "promoted");
}


// ── C2 bailout stays at C1 ───────────────────────────────────────────

#[test]
fn c2_bailout_stays_at_c1() {
    let policy = CompilationPolicy {
        c1_threshold: 5,
        c2_threshold: 20,
        c2_min_invocations: 10,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();

    // Trigger C1.
    for _ in 0..5 {
        mgr.on_method_invocation(&key);
    }
    mgr.dequeue_compilation();
    mgr.compilation_complete(&key, CompilationTier::C1, 5);

    // Force bailout.
    mgr.on_c2_bailout(&key);

    // Keep invoking past C2 threshold — should NOT trigger C2.
    for _ in 5..30 {
        let tier = mgr.on_method_invocation(&key);
        assert_ne!(tier, Some(CompilationTier::C2));
    }
}

// ── Queue size ───────────────────────────────────────────────────────

#[test]
fn queue_size_tracking() {
    let mgr = TieredCompilationManager::with_default_policy();
    assert_eq!(mgr.queue_size(), 0);

    let key = test_key();
    mgr.enqueue_compilation(CompilationTask {
        method_key: key.clone(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::Normal,
        enqueue_time_ms: 0,
        osr_bci: None,
    });
    assert_eq!(mgr.queue_size(), 1);

    mgr.dequeue_compilation();
    assert_eq!(mgr.queue_size(), 0);
}

// ── Stats counting ───────────────────────────────────────────────────

#[test]
fn stats_c1_compilations() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C1, 10);
    assert_eq!(mgr.stats().c1_compilations.load(Ordering::Relaxed), 1);
}

#[test]
fn stats_c2_compilations() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C2, 50);
    assert_eq!(mgr.stats().c2_compilations.load(Ordering::Relaxed), 1);
}

#[test]
fn stats_osr_compilations() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    assert!(mgr.request_osr(&key, 0).is_some());
    assert!(mgr.request_osr(&key, 0).is_none(), "already queued");
    // Queued is not compiled: the stat sits beside c1/c2, which count
    // published bodies, and must mean the same thing.
    assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 0);
    // A failed OSR compile is not an OSR compilation either.
    mgr.core
        .complete_task(&key, CompilationTier::C2, 1, false, true, false);
    assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 0);
    assert!(
        mgr.request_osr(&key, 0).is_some(),
        "retried after the failure"
    );
    mgr.core
        .complete_task(&key, CompilationTier::C2, 1, true, true, false);
    assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 1);
    // And it is not ALSO a method-entry C2 compile.
    assert_eq!(mgr.stats().c2_compilations.load(Ordering::Relaxed), 0);
}

#[test]
fn stats_deoptimizations() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C2, 10);
    mgr.on_deoptimization(&key, DeoptReason::UncommonTrap, 1, DeoptAction::Reinterpret);
    assert_eq!(mgr.stats().deoptimizations.load(Ordering::Relaxed), 1);
    assert_eq!(mgr.stats().soft_deoptimizations.load(Ordering::Relaxed), 1);
}

#[test]
fn stats_total_compile_time() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C1, 15);
    mgr.compilation_complete(&key, CompilationTier::C2, 85);
    assert_eq!(
        mgr.stats().total_compile_time_ms.load(Ordering::Relaxed),
        100
    );
}

// ── Method states listing ────────────────────────────────────────────

#[test]
fn method_states_listing() {
    let mgr = TieredCompilationManager::with_default_policy();
    let k1 = test_key();
    let k2 = test_key2();
    mgr.on_method_invocation(&k1);
    mgr.on_method_invocation(&k2);
    mgr.on_method_invocation(&k2);

    let states = mgr.method_states();
    assert_eq!(states.len(), 2);
}

// ── Policy update ────────────────────────────────────────────────────

#[test]
fn policy_update() {
    let mgr = TieredCompilationManager::with_default_policy();
    let mut p = mgr.policy();
    assert_eq!(p.c1_threshold, 200);
    p.c1_threshold = 200;
    mgr.set_policy(p);
    assert_eq!(mgr.policy().c1_threshold, 200);
}

// ── Empty queue returns None ─────────────────────────────────────────

#[test]
fn empty_queue_returns_none() {
    let mgr = TieredCompilationManager::with_default_policy();
    assert!(mgr.dequeue_compilation().is_none());
    assert!(mgr.queue_empty());
}

// ── High priority first ──────────────────────────────────────────────

#[test]
fn queue_processes_high_priority_first() {
    let mgr = TieredCompilationManager::with_default_policy();
    mgr.enqueue_compilation(CompilationTask {
        method_key: test_key(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::Low,
        enqueue_time_ms: 0,
        osr_bci: None,
    });
    mgr.enqueue_compilation(CompilationTask {
        method_key: test_key2(),
        target_tier: CompilationTier::C2,
        priority: CompilationPriority::High,
        enqueue_time_ms: 0,
        osr_bci: None,
    });

    let first = mgr.dequeue_compilation().unwrap();
    assert_eq!(first.method_key, test_key2());
    assert_eq!(first.priority, CompilationPriority::High);
}

// ── OSR compilation has bci set ──────────────────────────────────────

#[test]
fn osr_compilation_has_bci() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    let task = mgr.request_osr(&key, 77).unwrap();
    assert_eq!(task.osr_bci, Some(77));
}

// ── Tiered disabled ──────────────────────────────────────────────────

#[test]
fn no_compilation_when_tiered_disabled() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        tiered_enabled: false,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();
    for _ in 0..100 {
        assert!(mgr.on_method_invocation(&key).is_none());
    }
}

// ── Current tier query ───────────────────────────────────────────────

#[test]
fn current_tier_returns_correct_value() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C1WithProfiling, 5);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1WithProfiling);
}

// ── Compilation task priorities ──────────────────────────────────────

#[test]
fn compilation_task_priorities() {
    assert_eq!(CompilationPriority::High, CompilationPriority::High);
    assert_ne!(CompilationPriority::High, CompilationPriority::Low);
}

// ── Queue total_processed ────────────────────────────────────────────

#[test]
fn queue_total_processed_counter() {
    let mgr = TieredCompilationManager::with_default_policy();
    // Two methods: one method holds one in-flight slot, so a second request
    // for the same key would be refused rather than queued.
    mgr.enqueue_compilation(CompilationTask {
        method_key: test_key(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::Normal,
        enqueue_time_ms: 0,
        osr_bci: None,
    });
    mgr.enqueue_compilation(CompilationTask {
        method_key: test_key2(),
        target_tier: CompilationTier::C2,
        priority: CompilationPriority::High,
        enqueue_time_ms: 0,
        osr_bci: None,
    });
    mgr.dequeue_compilation();
    mgr.dequeue_compilation();

    assert_eq!(mgr.core.total_processed(), 2);
}

// ── Custom policy thresholds ─────────────────────────────────────────

#[test]
fn custom_policy_thresholds() {
    let policy = CompilationPolicy {
        c1_threshold: 10,
        c2_threshold: 50,
        osr_threshold: 100,
        tiered_enabled: true,
        c2_min_invocations: 25,
        c1_profiling: false,
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();

    // Should trigger C1 at invocation 10.
    let mut triggered_at = None;
    for i in 1..=15 {
        if let Some(CompilationTier::C1) = mgr.on_method_invocation(&key) {
            triggered_at = Some(i);
            break;
        }
    }
    assert_eq!(triggered_at, Some(10));
}

// ── Compiler active flag ─────────────────────────────────────────────

#[test]
fn compiler_active_flag() {
    let mgr = TieredCompilationManager::with_default_policy();
    assert!(!mgr.compiler_active());
    mgr.set_compiler_active(true);
    assert!(mgr.compiler_active());
}

// ── Tier ordering ────────────────────────────────────────────────────

#[test]
fn tier_ordering() {
    assert!(CompilationTier::Interpreter < CompilationTier::C1);
    assert!(CompilationTier::C1 < CompilationTier::C1WithProfiling);
    assert!(CompilationTier::C1WithProfiling < CompilationTier::FullProfile);
    assert!(CompilationTier::FullProfile < CompilationTier::C2);
}

// ── C2 bailout stat incremented ──────────────────────────────────────

#[test]
fn c2_bailout_stat() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = test_key();
    mgr.on_method_invocation(&key);
    mgr.on_c2_bailout(&key);
    assert_eq!(mgr.stats().c2_bailouts.load(Ordering::Relaxed), 1);
}

// ── Split in-flight slots: method entry and OSR ──────────────────────


/// The branch-profile window belongs to the method-entry slot: an OSR
/// request beside a C2 supersede neither closes it nor opens another.
#[test]
fn an_osr_request_beside_a_supersede_leaves_its_window_alone() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::new("craton/test/SplitSlotWindow", "loop", "()V");
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    mgr.core.request_c2_upgrade(&key);
    assert_eq!(mgr.branch_window_balance(), 1);

    assert!(mgr.request_osr(&key, 4).is_some());
    mgr.core
        .complete_task(&key, CompilationTier::C2, 1, true, true, false);
    assert_eq!(
        mgr.branch_window_balance(),
        1,
        "the OSR completion closes nothing it did not open"
    );
    mgr.core
        .complete_task(&key, CompilationTier::C2, 1, true, false, false);
    assert_eq!(mgr.branch_window_balance(), 0);
}

/// Time-to-tier: the first method-entry body is timed from the first
/// admission, the first C2 body from the first body, each once per method;
/// an OSR publish is neither.
#[test]
fn time_to_tier_is_charged_once_per_method_and_not_by_osr() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = MethodKey::new("craton/test/TimeToTier", "hot", "()V");
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    {
        let mut methods = mgr.core.methods.lock();
        let state = methods.get_mut(&key).expect("tracked");
        assert_ne!(state.first_nominated_ms, 0, "stamped at admission");
        // Backdate so the charged wait is visibly non-zero.
        state.first_nominated_ms = 1;
    }
    let s = mgr.stats();
    assert!(mgr.request_osr(&key, 2).is_some());
    mgr.core
        .complete_task(&key, CompilationTier::C2, 1, true, true, false);
    assert_eq!(s.nominate_to_first_body.load(Ordering::Relaxed), 0);
    assert_eq!(s.first_body_to_c2.load(Ordering::Relaxed), 0);

    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(s.nominate_to_first_body.load(Ordering::Relaxed), 1);
    let charged = s.nominate_to_first_body_ms_total.load(Ordering::Relaxed);
    assert_eq!(
        s.nominate_to_first_body_ms_max.load(Ordering::Relaxed),
        charged
    );
    assert_eq!(s.first_body_to_c2.load(Ordering::Relaxed), 0);

    mgr.compilation_complete(&key, CompilationTier::C2, 1);
    mgr.compilation_complete(&key, CompilationTier::C2, 1);
    assert_eq!(s.nominate_to_first_body.load(Ordering::Relaxed), 1);
    assert_eq!(s.first_body_to_c2.load(Ordering::Relaxed), 1, "once");
}

/// A counted trap drops every queued request for the method and releases
/// both slots.
#[test]
fn a_counted_trap_releases_both_slots() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = MethodKey::new("craton/test/SplitSlotTrap", "loop", "()V");
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    assert!(mgr.request_osr(&key, 3).is_some());
    let removed = mgr.core.remove_queued_for(&key);
    assert_eq!(removed.len(), 2);
    {
        let mut methods = mgr.core.methods.lock();
        let state = methods.get_mut(&key).expect("tracked");
        for (task, ticket) in &removed {
            mgr.core
                .release_slot_for(state, task.target_tier, task.osr_bci, *ticket);
        }
        assert!(!state.queued_for_compilation);
        assert!(!state.osr_in_flight);
    }
    assert!(mgr.request_osr(&key, 3).is_some());
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
}

// ── OSR requests disabled when tiered off ────────────────────────────

#[test]
fn osr_request_disabled_when_tiered_off() {
    let policy = CompilationPolicy {
        tiered_enabled: false,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();
    assert!(mgr.request_osr(&key, 0).is_none());
}

// ── Background worker drains an enqueued task off-thread ──────────────

/// How long a test waits for the background compile thread to reach a
/// rendezvous before giving up.
///
/// A **liveness** bound, not a latency assertion. None of these tests
/// claims anything about how fast a compile is; the budget exists so a
/// worker that never runs fails the suite instead of hanging it.
///
/// Worth knowing what it is NOT for. All four worker tests below failed on
/// this timeout about 1 run in 13 at `--test-threads=32`, which looks
/// exactly like an oversubscribed box starving a spawned thread — each test
/// starts its own `cratonvm-jit-compiler` (16 MiB stack), so a 32-way run
/// has dozens of them. Raising the budget from 5 seconds to 60 changed
/// nothing: still 6 failures in 60 runs. The task was not late, it was
/// GONE — see [`worker_manager`]. Tuning a timeout is what you do after a
/// measurement says it is a timing problem, not instead of measuring.
const WORKER_RENDEZVOUS: std::time::Duration = std::time::Duration::from_secs(30);

/// A manager for a test that starts a background worker: the same thing
/// `TieredCompilationManager::new` builds, with the install epoch PINNED.
///
/// A queued task is stamped with the install epoch it was enqueued at, and
/// `take_fresh` drops it if the epoch has moved since. That is the
/// redefinition rule and it is correct. But `TieredCompilationManager::new`
/// reads the epoch from `crate::jit_install_epoch()`, which is
/// **process-global**, and every `JitCache::clear_all` anywhere in this test
/// binary advances it. So a sibling test clearing its own cache silently
/// invalidated this test's queued task; the worker dropped it instead of
/// compiling it, and the rendezvous channel never received. The symptom is
/// a timeout, which reads as "the worker never ran".
///
/// The section below already knew the hazard — "bumping the real
/// `crate::JIT_INSTALL_EPOCH` would be non-deterministic (every
/// `JitCache::clear_all` anywhere in this test binary advances it)" — and
/// injects an epoch source for the tests that are ABOUT epochs. These four
/// are not about epochs, which is exactly why nobody pinned theirs.
fn worker_manager(policy: CompilationPolicy) -> TieredCompilationManager {
    TieredCompilationManager::with_install_epoch_source(
        policy,
        Some(Arc::new(AtomicU64::new(1))),
    )
}

/// wire-tiered-manager increment 1: crossing the C1 threshold via
/// `on_method_invocation` enqueues a task, and the background compile thread
/// dequeues + "compiles" it on a *different* thread, then publishes the tier.
///
/// Deterministic: the test blocks on an `mpsc` recv (a synchronization
/// handle) rather than sleeping, so it never races on timing.
/// gcj pcp: while a GC pause scope lives, a queued compile does not
/// start; it runs once the scope is dropped, and the wait is counted.
#[test]
fn gcj_pcp_a_compile_queued_during_a_gc_pause_starts_after_it() {
    use std::sync::mpsc;
    let policy = CompilationPolicy {
        c1_threshold: 2,
        c2_threshold: u32::MAX,
        c2_min_invocations: u32::MAX,
        osr_threshold: u32::MAX,
        tiered_enabled: true,
        c1_profiling: true,
    };
    let mgr = worker_manager(policy);
    let key = test_key();
    let (tx, rx) = mpsc::channel::<()>();
    let _bg = mgr
        .start_background_compiler(Box::new(move |_: &CompilationTask| -> CompileOutcome {
            tx.send(()).unwrap();
            CompileOutcome::declined(0)
        }))
        .expect("worker should start");
    let pause = mgr.gc_pause_scope();
    let _ = mgr.on_method_invocation(&key);
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
    while mgr.gc_pause_waits() == 0 && std::time::Instant::now() < deadline {
        assert!(rx.try_recv().is_err(), "no compile starts inside the pause");
        std::thread::yield_now();
    }
    assert_eq!(mgr.gc_pause_waits(), 1, "the worker reached the pause gate");
    assert!(rx.try_recv().is_err(), "the compile is still held");
    drop(pause);
    rx.recv_timeout(WORKER_RENDEZVOUS)
        .expect("the compile runs once the pause ends");
    assert_eq!(mgr.gc_pause_waits(), 1);
}

#[test]
fn background_worker_drains_enqueued_task_off_thread() {
    use std::sync::mpsc;

    // Low C1 threshold so a couple of invocations cross it. Disable the
    // straight-to-C2 path by keeping c2 thresholds high.
    let policy = CompilationPolicy {
        c1_threshold: 2,
        c2_threshold: u32::MAX,
        c2_min_invocations: u32::MAX,
        osr_threshold: u32::MAX,
        tiered_enabled: true,
        c1_profiling: true,
    };
    let mgr = worker_manager(policy);
    let key = test_key();

    // The compile closure reports (task tier, the thread it ran on) back to
    // the test thread, proving the work happened off the "mutator".
    let (tx, rx) = mpsc::channel::<(CompilationTier, std::thread::ThreadId)>();
    let bg = mgr
        .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
            tx.send((task.target_tier, std::thread::current().id()))
                .unwrap();
            // pretend the compile took 7ms and published
            CompileOutcome {
                compile_time_ms: 7,
                published: true,
                c2_upgrade_candidate: false,
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: false,
            }
        }))
        .expect("worker should start");

    let mutator_thread = std::thread::current().id();

    // Drive invocations on *this* (mutator) thread until the threshold
    // crossing enqueues a C1 task.
    assert!(
        mgr.on_method_invocation(&key).is_none(),
        "1st invocation: below threshold"
    );
    let rec = mgr.on_method_invocation(&key);
    assert_eq!(
        rec,
        Some(CompilationTier::C1),
        "threshold crossing enqueues C1"
    );

    // The worker should pick it up off-thread. Block on the channel (no sleep).
    let (compiled_tier, worker_thread) = rx
        .recv_timeout(WORKER_RENDEZVOUS)
        .expect("worker must drain the task");
    assert_eq!(compiled_tier, CompilationTier::C1);
    assert_ne!(
        worker_thread, mutator_thread,
        "compilation must run OFF the mutator thread"
    );

    // After completion the worker must publish the tier and clear the queue.
    // Spin briefly on the completion counter (bounded, no fixed sleep).
    let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
    while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(mgr.completed_compilations(), 1, "one task completed");
    assert!(mgr.queue_empty(), "queue drained");
    assert_eq!(
        mgr.current_tier(&key),
        CompilationTier::C1,
        "tier published by worker"
    );
    {
        let methods = mgr.core.methods.lock();
        assert!(
            !methods[&key].queued_for_compilation,
            "queued flag cleared on completion"
        );
        assert_eq!(methods[&key].last_compile_time_ms, 7);
    }

    // Clean shutdown joins the worker thread.
    drop(bg);
    assert!(!mgr.compiler_active(), "worker stopped after shutdown");
}

/// Round 11 wave 4: an installed idle tick runs on an idle compile worker
/// with no task ever queued, off the installing thread, and keeps running
/// (it is a clock, not a one-shot). Shutdown still joins the worker.
#[test]
fn an_idle_worker_runs_the_installed_idle_tick() {
    use std::sync::mpsc;
    let mgr = worker_manager(CompilationPolicy::default());
    let (tx, rx) = mpsc::channel::<std::thread::ThreadId>();
    let tx = Mutex::new(tx);
    mgr.set_idle_tick(Box::new(move || {
        let _ = tx.lock().send(std::thread::current().id());
    }));
    let bg = mgr
        .start_background_compiler(Box::new(|_: &CompilationTask| CompileOutcome::declined(0)))
        .expect("worker should start");
    let first = rx
        .recv_timeout(WORKER_RENDEZVOUS)
        .expect("an idle worker must run the tick");
    assert_ne!(
        first,
        std::thread::current().id(),
        "the tick runs on a worker"
    );
    rx.recv_timeout(WORKER_RENDEZVOUS)
        .expect("and must keep running it");
    drop(bg);
    assert!(!mgr.compiler_active(), "worker stopped after shutdown");
}

/// j27 TIER (`CRATONVM_JIT_ENTRY_TRIP`): an armed box's trip reaches this
/// manager's sink and raises the idle-tick request; the one sink is
/// shared; an idle C1 worker consumes the request by running the tick.
#[test]
fn j27_an_entry_trip_requests_the_idle_tick() {
    use std::sync::mpsc;
    let mgr = worker_manager(CompilationPolicy::default());
    let sink = mgr.entry_trip_sink();
    assert!(Arc::ptr_eq(&sink, &mgr.entry_trip_sink()), "one sink per manager");
    assert!(!mgr.core.idle_tick_requested.load(Ordering::Acquire));
    let counter = crate::entry_counter::JitEntryCounter::new();
    counter.arm_trip(0, &sink);
    crate::entry_counter::jit_entry_counter_tripped(&counter);
    assert_eq!(sink.trips(), 1);
    assert!(
        mgr.core.idle_tick_requested.load(Ordering::Acquire),
        "the trip raised the request"
    );
    let (tx, rx) = mpsc::channel::<()>();
    let tx = Mutex::new(tx);
    mgr.set_idle_tick(Box::new(move || {
        let _ = tx.lock().send(());
    }));
    let bg = mgr
        .start_background_compiler(Box::new(|_: &CompilationTask| CompileOutcome::declined(0)))
        .expect("worker should start");
    rx.recv_timeout(WORKER_RENDEZVOUS)
        .expect("an idle worker must run the requested tick");
    drop(bg);
    assert!(
        !mgr.core.idle_tick_requested.load(Ordering::Acquire),
        "the worker consumed the request"
    );
}

// ── Tier → backend routing (Step 3) ──────────────────────────────────

#[test]
fn tier_routing_selects_optimized_backend_for_c2() {
    // C1 tiers route to the fast single-pass (no-opt) backend; C2 (and the
    // FullProfile collection tier, which only reaches codegen as a C2
    // promotion) route to the optimizing pipeline.
    assert!(!tier_uses_optimized_backend(CompilationTier::Interpreter));
    assert!(!tier_uses_optimized_backend(CompilationTier::C1));
    assert!(!tier_uses_optimized_backend(
        CompilationTier::C1WithProfiling
    ));
    assert!(tier_uses_optimized_backend(CompilationTier::FullProfile));
    assert!(tier_uses_optimized_backend(CompilationTier::C2));
}

// ── C1→C2 supersede: candidate C1 publish auto-enqueues a C2 recompile ──

/// The policy of the supersede tests: C1 at the first call, C2 only past
/// 1 000 counted invocations, no OSR.
fn supersede_policy() -> CompilationPolicy {
    CompilationPolicy {
        c1_threshold: 1,
        c2_threshold: 1_000,
        c2_min_invocations: 1_000,
        osr_threshold: u32::MAX,
        tiered_enabled: true,
        c1_profiling: true,
    }
}

/// The interpreter keeps counting while a C1 request is queued: put the
/// method past [`supersede_policy`]'s C2 bar, then let the (blocked) C1
/// compile publish, so the supersede finds it hot.
fn make_hot_then_release(
    mgr: &TieredCompilationManager,
    key: &MethodKey,
    go: &std::sync::mpsc::Sender<()>,
) {
    mgr.core
        .methods
        .lock()
        .get_mut(key)
        .expect("the method is tracked")
        .invocation_count = 5_000;
    go.send(()).unwrap();
}

/// The C1→C2 supersede asks the C2 hotness bar like every other door: a C1
/// body published for a method that has not been invoked that often
/// queues no C2 recompile (80 % of a 500-test Netty class's C1 bodies were
/// replaced by a C2 body before this).
#[test]
fn a_c1_publish_below_the_hotness_bar_queues_no_c2_supersede() {
    use std::sync::mpsc;
    let mgr = worker_manager(supersede_policy());
    let key = test_key();
    let (tx, rx) = mpsc::channel::<CompilationTier>();
    let bg = mgr
        .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
            tx.send(task.target_tier).unwrap();
            CompileOutcome {
                compile_time_ms: 1,
                published: true,
                c2_upgrade_candidate: !tier_uses_optimized_backend(task.target_tier),
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: false,
            }
        }))
        .expect("worker should start");

    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    assert_eq!(
        rx.recv_timeout(WORKER_RENDEZVOUS).expect("C1 compile must run"),
        CompilationTier::C1
    );
    // The supersede is asked right after the publish, on the same worker:
    // a C2 compile would follow within the rendezvous, so a short wait that
    // sees none is the refusal.
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
        "a method below the C2 bar must stay at C1"
    );
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    assert!(mgr.queue_empty());
    drop(bg);
}

#[test]
fn c1_publish_with_upgrade_candidate_enqueues_c2_supersede() {
    use std::sync::mpsc;
    let mgr = worker_manager(supersede_policy());
    let key = test_key();

    let (tx, rx) = mpsc::channel::<CompilationTier>();
    // The C1 compile waits until the method has crossed the C2 bar, so the
    // publish finds it hot (the interpreter keeps counting while the
    // request is queued).
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let bg = mgr
        .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
            if !tier_uses_optimized_backend(task.target_tier) {
                go_rx.lock().unwrap().recv().unwrap();
            }
            tx.send(task.target_tier).unwrap();
            CompileOutcome {
                compile_time_ms: 1,
                published: true,
                // Models the VM-side predicate: judged IR-eligible on the
                // C1 pass; a C2 task never re-seeds an upgrade.
                c2_upgrade_candidate: !tier_uses_optimized_backend(task.target_tier),
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: false,
            }
        }))
        .expect("worker should start");

    // Cross c1_threshold=1 → C1 task enqueued.
    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    make_hot_then_release(&mgr, &key, &go_tx);

    // Worker compiles C1, then the loop auto-enqueues + compiles the C2
    // supersede (Low priority). Deterministic via the channel.
    let first = rx
        .recv_timeout(WORKER_RENDEZVOUS)
        .expect("C1 compile must run");
    assert_eq!(first, CompilationTier::C1);
    let second = rx
        .recv_timeout(WORKER_RENDEZVOUS)
        .expect("C2 supersede compile must follow a candidate C1 publish");
    assert_eq!(second, CompilationTier::C2);

    // Both completions recorded; tier settles at C2; nothing re-queued
    // (request_c2_upgrade is idempotent and gated on current_tier < C2).
    let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
    while mgr.completed_compilations() < 2 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(mgr.completed_compilations(), 2);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
    assert!(mgr.queue_empty(), "no repeat upgrade churn");
    drop(bg);
}

/// str17: a C2 body whose publication the cache refused (`put` dropped it,
/// the C1 body stayed live) leaves the method at C1 and is asked again; the
/// second C2 attempt publishes. Before, the refused attempt was reported as a
/// publish, the tier read C2, and the method ran its C1 body for good.
#[test]
fn a_refused_c2_publication_is_retried_and_does_not_record_c2() {
    use std::sync::mpsc;
    let mgr = worker_manager(supersede_policy());
    let key = test_key();
    let c2_attempts = Arc::new(AtomicU32::new(0));
    let attempts = Arc::clone(&c2_attempts);

    let (tx, rx) = mpsc::channel::<CompilationTier>();
    // The C1 compile waits until the method is past the C2 bar.
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let bg = mgr
        .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
            let optimized = tier_uses_optimized_backend(task.target_tier);
            if !optimized {
                go_rx.lock().unwrap().recv().unwrap();
            }
            // The first C2 attempt is refused at publication.
            let refused = optimized && attempts.fetch_add(1, Ordering::SeqCst) == 0;
            tx.send(task.target_tier).unwrap();
            CompileOutcome {
                compile_time_ms: 1,
                published: !refused,
                c2_upgrade_candidate: !optimized,
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: refused,
            }
        }))
        .expect("worker should start");

    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    make_hot_then_release(&mgr, &key, &go_tx);
    let tiers: Vec<CompilationTier> = (0..3)
        .map(|_| {
            rx.recv_timeout(WORKER_RENDEZVOUS)
                .expect("C1, the refused C2 and its retry all run")
        })
        .collect();
    assert_eq!(
        tiers,
        [
            CompilationTier::C1,
            CompilationTier::C2,
            CompilationTier::C2
        ]
    );
    let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
    while mgr.completed_compilations() < 3 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
    assert!(
        mgr.queue_empty(),
        "the published retry asks for nothing more"
    );
    drop(bg);
}


fn an_always_refused_c2_publication_stops_at_the_failure_budget_body() {
    let mgr = worker_manager(supersede_policy());
    let key = test_key();
    let c2_attempts = Arc::new(AtomicU32::new(0));
    let attempts = Arc::clone(&c2_attempts);
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);

    let bg = mgr
        .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
            let optimized = tier_uses_optimized_backend(task.target_tier);
            if optimized {
                attempts.fetch_add(1, Ordering::SeqCst);
            } else {
                go_rx.lock().unwrap().recv().unwrap();
            }
            CompileOutcome {
                compile_time_ms: 1,
                published: !optimized,
                c2_upgrade_candidate: !optimized,
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: optimized,
            }
        }))
        .expect("worker should start");

    assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    make_hot_then_release(&mgr, &key, &go_tx);
    let want = 1 + u64::from(MAX_TIER_FAIL_RETRIES);
    let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
    while (mgr.completed_compilations() < want || !mgr.queue_empty())
        && std::time::Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    assert_eq!(c2_attempts.load(Ordering::SeqCst), MAX_TIER_FAIL_RETRIES);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    assert!(mgr.queue_empty(), "no C2 request after the budget is spent");
    drop(bg);
}

// ── Increment 2: flag-gated off-thread compile publishes the Jit target ──

/// wire-tiered-manager increment 2: with background compilation enabled, a
/// crossed threshold enqueues a task that the worker compiles OFF the
/// mutator thread; the worker then "publishes" the compiled entry (here a
/// shared `Jit`-target map standing in for `SharedVm::jit_cache`, which is
/// VM-crate-only) and the manager's tier is flipped to the compiled tier —
/// the jit-crate analogue of the invoke cache being updated to the `Jit`
/// target. Deterministic: blocks on an `mpsc` recv, never sleeps.
#[test]
fn flag_on_threshold_compiles_off_thread_and_publishes_jit_target() {
    use std::sync::mpsc;

    // A stand-in for the VM's `jit_cache`: the compile closure inserts the
    // method key here to model "the Jit target is now installed/published".
    let published: Arc<Mutex<Vec<MethodKey>>> = Arc::new(Mutex::new(Vec::new()));

    // Low C2 threshold so a couple of invocations route STRAIGHT to C2
    // (the optimized backend), exercising the Step-3 routing decision.
    let policy = CompilationPolicy {
        c1_threshold: u32::MAX, // skip the C1 step
        c2_threshold: 2,
        c2_min_invocations: 2,
        osr_threshold: u32::MAX,
        tiered_enabled: true,
        c1_profiling: true,
    };
    let mgr = worker_manager(policy);
    let key = test_key();

    let mutator_thread = std::thread::current().id();
    let (tx, rx) = mpsc::channel::<(CompilationTier, bool, std::thread::ThreadId)>();
    let published_w = Arc::clone(&published);
    let bg = mgr
        .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
            // Real compile_fn shape: pick the backend by tier (Step 3),
            // "publish" the Jit target, and report back off-thread.
            let optimized = tier_uses_optimized_backend(task.target_tier);
            published_w.lock().push(task.method_key.clone());
            tx.send((task.target_tier, optimized, std::thread::current().id()))
                .unwrap();
            CompileOutcome {
                compile_time_ms: 3,
                published: true,
                c2_upgrade_candidate: false,
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: false,
            }
        }))
        .expect("worker should start");

    // Drive invocations on the mutator thread until C2 is recommended.
    assert!(
        mgr.on_method_invocation(&key).is_none(),
        "1st invocation: below threshold"
    );
    let rec = mgr.on_method_invocation(&key);
    assert_eq!(
        rec,
        Some(CompilationTier::C2),
        "threshold crossing enqueues a C2 task (straight-to-C2 path)"
    );

    // Worker drains + compiles off-thread; block on the channel (no sleep).
    let (compiled_tier, optimized, worker_thread) = rx
        .recv_timeout(WORKER_RENDEZVOUS)
        .expect("worker must drain the task");
    assert_eq!(compiled_tier, CompilationTier::C2);
    assert!(
        optimized,
        "C2 must route to the optimized backend (Step 3 routing)"
    );
    assert_ne!(
        worker_thread, mutator_thread,
        "compilation must run OFF the mutator thread"
    );

    // After completion the worker publishes the tier (invoke-cache analogue)
    // and clears the queue. Bounded spin on the completion counter.
    let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
    while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(mgr.completed_compilations(), 1, "one task completed");
    assert!(mgr.queue_empty(), "queue drained");
    assert_eq!(
        mgr.current_tier(&key),
        CompilationTier::C2,
        "Jit target (tier) published by the worker"
    );
    assert_eq!(
        *published.lock(),
        vec![key.clone()],
        "the compiled method's Jit target was published off-thread"
    );
    assert_eq!(mgr.stats().c2_compilations.load(Ordering::Relaxed), 1);

    drop(bg);
    assert!(!mgr.compiler_active(), "worker stopped after shutdown");
}

// ── Increment 3: GC-STW-safety — no VM-equivalent lock held across waits ──

/// wire-tiered-manager increment 3 (GC-STW-safety): the background worker
/// must hold NO VM lock across (a) its queue wait and (b) an in-flight
/// compile's own blocking. This is the load-bearing invariant that keeps a
/// STW prompt: a mutator wanting an exclusive VM lock (modelled here by
/// `vm_lock`, standing in for `SharedVm::class_manager.write()`) must be
/// able to acquire it WHILE a compile task is in-flight, because the worker
/// only ever takes that lock for a bounded scope and drops it before doing
/// anything blocking.
///
/// The test drives a compile task whose `compile_fn` mirrors the real
/// `background_compile_task` lock shape: briefly take `vm_lock` (read out
/// what it needs), DROP it, then block (here on a barrier standing in for
/// the long codegen / a `load_class_concurrent` condvar wait). While the
/// worker is blocked mid-compile, a competing "STW initiator" thread must
/// acquire `vm_lock` PROMPTLY. If the worker wrongly held a VM lock across
/// its blocking wait, this acquisition would deadlock and the bounded
/// `recv_timeout` would fire. Deterministic: every rendezvous is a channel
/// recv or a barrier, never a sleep.
#[test]
fn worker_holds_no_vm_lock_across_blocking_compile() {
    use std::sync::mpsc;
    use std::sync::{Arc as StdArc, Barrier};

    // Stand-in for `SharedVm::class_manager` (the lock a class-defining
    // mutator / STW path contends for). The worker takes it only briefly.
    let vm_lock: StdArc<Mutex<u64>> = StdArc::new(Mutex::new(0));

    let policy = CompilationPolicy {
        c1_threshold: 1,
        c2_threshold: u32::MAX,
        c2_min_invocations: u32::MAX,
        osr_threshold: u32::MAX,
        tiered_enabled: true,
        c1_profiling: true,
    };
    let mgr = worker_manager(policy);
    let key = test_key();

    // Rendezvous: worker -> test when it has ENTERED the compile and is
    // about to block; a two-party barrier the worker waits on to model the
    // long in-flight compile; and a channel the worker uses to report the
    // VM-lock value it read during its bounded critical section.
    let (entered_tx, entered_rx) = mpsc::channel::<u64>();
    let release = StdArc::new(Barrier::new(2));

    let vm_lock_w = StdArc::clone(&vm_lock);
    let release_w = StdArc::clone(&release);
    let bg = mgr
        .start_background_compiler(Box::new(move |_task: &CompilationTask| -> CompileOutcome {
            // (1) Bounded VM-lock scope: acquire, read, DROP — exactly the
            // shape `try_jit_compile_callee_slow` uses for class_manager /
            // jit_cache. The guard must NOT survive into the blocking wait.
            let seen = {
                let g = vm_lock_w.lock();
                *g
            }; // <-- guard dropped here, BEFORE blocking below.
            entered_tx.send(seen).unwrap();
            // (2) Blocking wait with NO VM lock held — models long codegen
            // or a `load_class_concurrent` condvar wait. If a VM lock were
            // still held here, the STW thread below would deadlock.
            release_w.wait();
            CompileOutcome {
                compile_time_ms: 4,
                published: true,
                c2_upgrade_candidate: false,
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: false,
            }
        }))
        .expect("worker should start");

    // Enqueue one task (crossing c1_threshold=1 on the 1st invocation) ->
    // worker picks it up.
    assert_eq!(
        mgr.on_method_invocation(&key),
        Some(CompilationTier::C1),
        "1st invocation crosses c1_threshold=1 and enqueues a C1 task"
    );
    // The worker has entered the compile and finished its bounded VM-lock
    // critical section; block on the channel (no sleep).
    let seen = entered_rx
        .recv_timeout(WORKER_RENDEZVOUS)
        .expect("worker must enter compile and release the VM lock");
    assert_eq!(seen, 0, "worker read the VM-lock-protected state");

    // The worker is now blocked mid-compile (on `release`). A competing STW
    // initiator MUST be able to grab the VM lock promptly — proving the
    // worker holds no VM lock across its blocking wait. Do it on a separate
    // thread with a bounded join so a regression deadlocks the test thread's
    // timeout rather than hanging forever.
    let vm_lock_stw = StdArc::clone(&vm_lock);
    let (stw_tx, stw_rx) = mpsc::channel::<()>();
    let stw = std::thread::spawn(move || {
        let mut g = vm_lock_stw.lock();
        *g += 1; // mutate while the worker is mid-compile
        stw_tx.send(()).unwrap();
    });
    stw_rx
        .recv_timeout(WORKER_RENDEZVOUS)
        .expect("STW initiator must acquire the VM lock while a compile is in-flight");
    stw.join().unwrap();
    assert_eq!(*vm_lock.lock(), 1, "STW path mutated the VM-locked state");

    // The worker is also not holding its OWN queue lock while mid-compile:
    // `queue_size()` takes `core.queue.lock()` and returns without blocking,
    // confirming the worker dropped the queue lock before running compile_fn
    // (the queue was drained when the task was dequeued).
    assert_eq!(mgr.queue_size(), 0, "queue drained while compile in-flight");

    // Let the in-flight compile finish.
    release.wait();
    let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
    while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(mgr.completed_compilations(), 1, "compile completed");

    drop(bg);
    assert!(!mgr.compiler_active(), "worker stopped after shutdown");
}

// ── Install-epoch stamping and the stale-request drop ────────────────
//
// Every test here drives a manager whose install epoch comes from an
// INJECTED `AtomicU64`, so a "redefinition" is one `store` and the
// scheduling rule is exercised with no thread, no backend, no clock and no
// sleep. Bumping the real `crate::JIT_INSTALL_EPOCH` would be both
// non-deterministic (every `JitCache::clear_all` anywhere in this test
// binary advances it) and untestable in the other direction — there is no
// way to hold it still.
//
// The scheduling counters are process-wide, so the tests that assert exact
// counts use `crate::metrics::SchedulingCapture` — a per-thread view of the
// same recorder. They used to take `METRICS_TEST_LOCK` and reset the table
// instead, which does not work: the lock serialises the tests that ASSERT
// and the other tests here are the PRODUCERS. See the macro's own doc.

/// A manager reading its install epoch from `epoch` instead of the global.
fn epoch_driven_manager(epoch: &Arc<AtomicU64>) -> TieredCompilationManager {
    TieredCompilationManager::with_install_epoch_source(
        CompilationPolicy::default(),
        Some(Arc::clone(epoch)),
    )
}

fn epoch_key(name: &str) -> MethodKey {
    MethodKey::new("craton/test/EpochSubject", name, "()V")
}

fn c1_task(key: &MethodKey) -> CompilationTask {
    CompilationTask {
        method_key: key.clone(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::Normal,
        enqueue_time_ms: 0,
        osr_bci: None,
    }
}

/// Compare a dequeued task against an expected one while ignoring
/// `enqueue_time_ms`.
///
/// `TaskQueue::enqueue` stamps that field from the process clock whenever a
/// door left it at the `0` "unset" sentinel, which is what makes request
/// aging possible at all. Every test below that builds an expected
/// `CompilationTask` by hand leaves it `0`, so full struct equality would
/// compare a constant against a wall-clock reading and fail whenever the
/// process happened to be more than a millisecond old. These tests are
/// about epochs, deduplication, retry limits and window accounting — not
/// about queue latency — so they compare everything else. The two tests
/// that ARE about the stamp assert on `enqueue_time_ms` directly.
#[track_caller]
fn assert_task_eq(got: Option<CompilationTask>, want: Option<CompilationTask>) {
    let strip = |t: Option<CompilationTask>| {
        t.map(|mut t| {
            t.enqueue_time_ms = 0;
            t
        })
    };
    assert_eq!(strip(got), strip(want));
}

#[test]
fn a_request_queued_at_the_current_epoch_dispatches() {
    let epoch = Arc::new(AtomicU64::new(7));
    let mgr = epoch_driven_manager(&epoch);
    let key = epoch_key("stillCurrent");
    mgr.enqueue_compilation(c1_task(&key));

    assert_eq!(mgr.install_epoch(), 7);
    assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&key)));
    assert_eq!(mgr.dropped_requests(), 0);
    assert_eq!(mgr.queue_size(), 0);
}

/// Tier proposal R12-4: a C1 request dequeued after an optimizing body was
/// published for its method outside the manager is dropped at dispatch,
/// with its slot released and the tier left at C2; a request its method
/// has not outgrown is kept.
#[test]
fn a_single_pass_request_overtaken_by_an_optimizing_publish_is_not_compiled() {
    if !crate::entry_counter::callee_promotion_enabled() {
        return; // the outside-publish note is switched off here
    }
    let epoch = Arc::new(AtomicU64::new(3));
    let mgr = epoch_driven_manager(&epoch);
    let key = epoch_key("overtaken");
    assert!(mgr.enqueue_compilation(c1_task(&key)));
    mgr.note_published_outside_manager(&key, CompilationTier::C2);
    let task = mgr
        .next_fresh_task()
        .expect("the C1 request is still current");
    assert!(mgr.core.retire_overtaken(&task, 0));
    assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
    assert_eq!(mgr.dropped_requests(), 1);
    {
        let methods = mgr.core.methods.lock();
        let state = methods.get(&key).expect("tracked");
        assert!(!state.queued_for_compilation, "the slot is released");
    }

    let kept = epoch_key("notOvertaken");
    assert!(mgr.enqueue_compilation(c1_task(&kept)));
    let task = mgr.next_fresh_task().expect("queued");
    assert!(!mgr.core.retire_overtaken(&task, 0));
    assert_eq!(mgr.dropped_requests(), 1);
}

/// Tier proposal W4-1 (round 12 wave 5): with the VM's cache probe
/// installed, a successful single-pass completion takes its tier from the
/// body live under the key. C1 over a single-pass body although the tier
/// read C2 (inferred, it stayed C2 and the method was never promoted
/// again); C2 over a live optimizing body the tier no longer named.
#[test]
fn a_completion_takes_its_tier_from_the_live_body() {
    let mgr = TieredCompilationManager::new(CompilationPolicy::default());
    let live = Arc::new(Mutex::new(LiveEntryBody::SinglePass));
    let answer = Arc::clone(&live);
    if !mgr.set_live_entry_probe(Box::new(move |_: &MethodKey| Some(*answer.lock()))) {
        return; // `CRATONVM_JIT_TIER_FROM_LIVE_BODY=0` in this process
    }
    let key = MethodKey::new("craton/test/LiveBody", "run", "()V");
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C2, 1);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C2);

    // The optimizing body went and a single-pass one was published.
    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);

    // A demotion raced an optimizing republication: the single-pass
    // publish kept that body, and the tier names it again.
    mgr.note_body_withdrawn(&key);
    *live.lock() = LiveEntryBody::Optimizing;
    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
}

#[test]
fn a_request_queued_before_an_epoch_bump_is_dropped_before_it_is_compiled() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let key = epoch_key("queuedThenRedefined");
    mgr.enqueue_compilation(c1_task(&key));
    assert!(
        mgr.method_states().iter().any(|(k, _, _)| *k == key),
        "enqueue tracks the method"
    );

    // A JVMTI redefinition / code-cache flush lands while the request sits
    // in the queue.
    epoch.store(2, Ordering::Release);

    assert_eq!(
        mgr.next_fresh_task(),
        None,
        "the stale request must not reach the backend"
    );
    assert_eq!(mgr.dropped_requests(), 1, "and must be counted, not lost");
    assert_eq!(mgr.queue_size(), 0);
}

#[test]
fn a_dropped_request_releases_the_slot_without_spending_a_retry() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let key = epoch_key("reAdmitted");
    mgr.enqueue_compilation(c1_task(&key));
    epoch.store(2, Ordering::Release);
    assert_task_eq(mgr.next_fresh_task(), None);

    // THE property. A stale drop is not a compile failure: the method's
    // in-flight flag is cleared so the next invocation can re-admit it,
    // and none of `current_tier` / `tier_fail_count` / `ineligible` moved.
    // Routing the drop through `complete_task(success = false)` would burn
    // one of MAX_TIER_FAIL_RETRIES, so three redefinitions during warmup
    // would leave a hot method permanently interpreted.
    {
        let methods = mgr.core.methods.lock();
        let state = methods.get(&key).expect("state survives a stale drop");
        assert!(
            !state.queued_for_compilation,
            "the in-flight slot must be released so the method can re-admit"
        );
        assert_eq!(state.queued_tier, None);
        assert_eq!(state.tier_fail_count, 0, "a stale drop is not a failure");
        assert!(!state.ineligible, "a stale drop is not a policy decline");
        assert_eq!(state.current_tier, CompilationTier::Interpreter);
    }

    // Re-admission works, at the new epoch, and now dispatches.
    mgr.enqueue_compilation(c1_task(&key));
    assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&key)));
    assert_eq!(
        mgr.dropped_requests(),
        1,
        "the re-admitted one was not dropped"
    );
}

#[test]
fn stale_requests_do_not_starve_a_fresh_one_behind_them() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let stale_a = epoch_key("staleA");
    let stale_b = epoch_key("staleB");
    let fresh = epoch_key("fresh");
    // Same band, so the order in the queue is exactly the enqueue order and
    // the fresh request really is behind both stale ones.
    mgr.enqueue_compilation(c1_task(&stale_a));
    mgr.enqueue_compilation(c1_task(&stale_b));
    epoch.store(2, Ordering::Release);
    mgr.enqueue_compilation(c1_task(&fresh));

    assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&fresh)));
    assert_eq!(mgr.dropped_requests(), 2);
    assert_eq!(mgr.queue_size(), 0);
}

#[test]
fn priority_order_survives_the_epoch_gate() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let low = epoch_key("low");
    let high = epoch_key("high");
    mgr.enqueue_compilation(CompilationTask {
        priority: CompilationPriority::Low,
        ..c1_task(&low)
    });
    mgr.enqueue_compilation(CompilationTask {
        target_tier: CompilationTier::C2,
        priority: CompilationPriority::High,
        ..c1_task(&high)
    });

    // The gate filters; it does not reorder.
    let first = mgr.next_fresh_task().expect("a fresh task");
    assert_eq!(first.method_key, high);
    let second = mgr.next_fresh_task().expect("a fresh task");
    assert_eq!(second.method_key, low);
    assert_eq!(mgr.dropped_requests(), 0);
}

#[test]
fn an_empty_queue_is_not_a_drop() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    assert_task_eq(mgr.next_fresh_task(), None);
    epoch.store(9, Ordering::Release);
    assert_task_eq(mgr.next_fresh_task(), None);
    assert_eq!(mgr.dropped_requests(), 0);
}

#[test]
fn every_enqueue_door_stamps_the_epoch() {
    // The stamp is applied by the queue, not by the caller, so a request
    // that entered through the policy path (`on_method_invocation`) or the
    // OSR path is gated identically to one the VM pushed directly. A
    // per-caller stamp is what would eventually be forgotten on one door.
    let policy = CompilationPolicy {
        c1_threshold: 1,
        c2_threshold: 1_000_000,
        osr_threshold: 1,
        tiered_enabled: true,
        c2_min_invocations: 1_000_000,
        c1_profiling: false,
    };
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr =
        TieredCompilationManager::with_install_epoch_source(policy, Some(Arc::clone(&epoch)));

    let invoked = epoch_key("viaInvocationHook");
    assert_eq!(
        mgr.on_method_invocation(&invoked),
        Some(CompilationTier::C1),
        "crossing c1_threshold=1 enqueues"
    );
    epoch.store(2, Ordering::Release);
    assert_eq!(mgr.next_fresh_task(), None, "policy-path request is gated");
    assert_eq!(mgr.dropped_requests(), 1);

    let osr = epoch_key("viaOsrRequest");
    assert!(mgr.request_osr(&osr, 12).is_some(), "OSR request enqueues");
    epoch.store(3, Ordering::Release);
    assert_eq!(mgr.next_fresh_task(), None, "OSR request is gated too");
    assert_eq!(mgr.dropped_requests(), 2);
}

#[test]
fn drops_reach_the_process_wide_scheduling_counters() {
    // Counted PER THREAD. `METRICS_TEST_LOCK` + a reset used to stand here
    // and did not work: the lock serialises the tests that ASSERT, while
    // every other `tiered` test that drops a request is a PRODUCER holding
    // nothing. It failed about 1 run in 20 at `--test-threads=32`,
    // reporting `Some(5)` for a count of 1. `next_fresh_task` records on
    // its caller's thread, which is this one.
    let counts = crate::metrics::SchedulingCapture::start();

    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let key = epoch_key("counted");
    mgr.enqueue_compilation(c1_task(&key));
    epoch.store(2, Ordering::Release);
    assert_task_eq(mgr.next_fresh_task(), None);

    assert_eq!(
        counts.count("queue_dropped_stale_install_epoch"),
        1,
        "the drop must be visible in the metrics idiom, not only on the manager"
    );
    // …and it reaches the GLOBAL table too, which is what a sink reads.
    // Asserted as a floor, because that table is everyone's.
    assert!(crate::metrics::scheduling_dropped_total() >= 1);
}

#[test]
fn invalidate_class_counts_the_requests_it_discards() {
    // Per thread; see the test above.
    let counts = crate::metrics::SchedulingCapture::start();

    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let a = epoch_key("unloadedA");
    let b = epoch_key("unloadedB");
    let survivor = MethodKey::new("craton/test/OtherClass", "kept", "()V");
    mgr.enqueue_compilation(c1_task(&a));
    mgr.enqueue_compilation(CompilationTask {
        priority: CompilationPriority::High,
        ..c1_task(&b)
    });
    mgr.enqueue_compilation(c1_task(&survivor));

    mgr.invalidate_class(ClassId::new(0), "craton/test/EpochSubject");

    assert_eq!(mgr.queue_size(), 1, "only the unrelated class survives");
    assert_eq!(counts.count("queue_dropped_class_invalidated"), 2);
    assert_eq!(mgr.dropped_requests(), 2);
    // Still dispatchable: invalidating one class must not gate another.
    assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&survivor)));
}

/// gc-common w36-c: the batch form leaves exactly what a loop of the
/// single form leaves -- tiered states, queued requests, OSR denials,
/// bail-list verdicts and the drop count -- for the empty set, an absent
/// class, one class, and a mixed set with an id-less member. Covers the
/// `belongs_to` rule both ways: another loader's same-named class keeps its
/// rows against an identified member, and an id-less key (or member)
/// matches by name.
#[test]
fn invalidate_classes_matches_a_loop_of_invalidate_class() {
    let a = ClassId::new(0x7A36_C001);
    let a2 = ClassId::new(0x7A36_C002);
    let b = ClassId::new(0x7A36_C003);
    let c = ClassId::new(0x7A36_C004);
    let keys = [
        MethodKey::with_class_id(a, "w36c/A", "m", "()V"),
        MethodKey::with_class_id(a2, "w36c/A", "m", "()V"),
        MethodKey::with_class_id(b, "w36c/B", "m", "()V"),
        MethodKey::new("w36c/B", "idless", "()V"),
        MethodKey::with_class_id(c, "w36c/C", "m", "()V"),
        MethodKey::new("w36c/C", "idless", "()V"),
    ];
    let fill = || {
        let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
        for key in &keys {
            mgr.core
                .methods
                .lock()
                .insert(key.clone(), MethodState::new(key.clone()));
            mgr.core.mark_osr_denied(key.clone());
            mgr.verdicts().mark_bail_listed(
                key.class_id,
                &key.class_name,
                &key.method_name,
                &key.descriptor,
            );
        }
        for key in &keys {
            assert!(mgr.enqueue_compilation(c1_task(key)));
        }
        mgr
    };
    type Row = (u32, String, String);
    let row = |key: &MethodKey| -> Row {
        (
            key.class_id.as_u32(),
            key.class_name.to_string(),
            key.method_name.to_string(),
        )
    };
    let state = |mgr: &TieredCompilationManager| {
        let mut methods: Vec<Row> = mgr.core.methods.lock().keys().map(row).collect();
        methods.sort();
        let mut denied: Vec<Row> = mgr.core.osr_denied.read().keys().map(row).collect();
        denied.sort();
        let mut queued: Vec<Row> = Vec::new();
        while let Some(task) = mgr.dequeue_compilation() {
            queued.push(row(&task.method_key));
        }
        queued.sort();
        let bail_listed: Vec<bool> = keys
            .iter()
            .map(|key| {
                mgr.verdicts().is_bail_listed(
                    key.class_id,
                    &key.class_name,
                    &key.method_name,
                    &key.descriptor,
                )
            })
            .collect();
        (methods, denied, queued, bail_listed, mgr.dropped_requests())
    };
    let sets: [&[(ClassId, &str)]; 4] = [
        &[],
        &[(ClassId::new(0x7A36_C0F8), "w36c/Absent")],
        &[(a, "w36c/A")],
        &[(a, "w36c/A"), (b, "w36c/B"), (ClassId::new(0), "w36c/C")],
    ];
    for set in sets {
        let looped = fill();
        for &(id, name) in set {
            looped.invalidate_class(id, name);
        }
        let batched = fill();
        batched.invalidate_classes(&crate::UnloadedClassSet::new(set.iter().copied()));
        let (l, bt) = (state(&looped), state(&batched));
        assert_eq!(l, bt, "set {set:?}");
    }
    // The mixed set leaves exactly the other loader's `w36c/A`.
    let batched = fill();
    batched.invalidate_classes(&crate::UnloadedClassSet::new(sets[3].iter().copied()));
    let (methods, denied, queued, bail_listed, dropped) = state(&batched);
    let survivor = vec![row(&keys[1])];
    assert_eq!(methods, survivor);
    assert_eq!(denied, survivor);
    assert_eq!(queued, survivor);
    assert_eq!(bail_listed, [false, true, false, false, false, false]);
    assert_eq!(dropped, 5, "five queued requests were discarded");
}

#[test]
fn shutdown_counts_the_requests_it_abandons() {
    // Per thread; see `drops_reach_the_process_wide_scheduling_counters`.
    // `shutdown` drains on the caller's thread — there is no worker here.
    let counts = crate::metrics::SchedulingCapture::start();

    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    // No worker is started, so nothing drains: the queue is exactly what
    // teardown finds. Deterministic, and no thread to race.
    mgr.enqueue_compilation(c1_task(&epoch_key("abandonedA")));
    mgr.enqueue_compilation(c1_task(&epoch_key("abandonedB")));

    let mut bg = BackgroundCompiler {
        core: Arc::clone(&mgr.core),
        handles: Vec::new(),
    };
    bg.shutdown();

    assert_eq!(mgr.queue_size(), 0, "shutdown drains rather than leaves");
    assert_eq!(
        counts.count("queue_shutdown_abandoned"),
        2,
        "abandoning work at teardown is correct, but it is still countable"
    );
    assert_eq!(mgr.dropped_requests(), 2);
}

// ── Deduplication: one method-entry slot per method ──────────────────

#[test]
fn an_identical_request_is_deduplicated_not_queued_twice() {
    let counts = crate::metrics::SchedulingCapture::start();
    let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
    let key = epoch_key("dedup");
    let c2 = CompilationTask {
        target_tier: CompilationTier::C2,
        priority: CompilationPriority::High,
        ..c1_task(&key)
    };

    assert!(mgr.enqueue_compilation(c1_task(&key)));
    assert!(
        !mgr.enqueue_compilation(c1_task(&key)),
        "an identical request is refused"
    );
    assert!(
        !mgr.enqueue_compilation(c2.clone()),
        "so is a different request while the slot is held: one compile per method at a time"
    );
    assert_eq!(mgr.queue_size(), 1);
    assert_eq!(mgr.deduplicated_requests(), 2);
    assert_eq!(counts.count("queue_deduplicated"), 2);

    // A dispatched request still holds the slot until it completes.
    assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&key)));
    assert!(!mgr.enqueue_compilation(c1_task(&key)));
    mgr.core
        .complete_task(&key, CompilationTier::C1, 1, true, false, false);
    assert!(mgr.enqueue_compilation(c2), "the slot frees on completion");
}

/// r11-tier: a runtime `MakeNotCompilable` closes METHOD ENTRY only. The
/// OSR door deliberately does not ask the runtime de-spec verdict (one cold
/// `assert` trap must not take a method's hot loops their OSR entry), and
/// recording the verdict as `ineligible` refused `request_osr` anyway.
#[test]
fn make_not_compilable_leaves_background_osr_open() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();
    let _ = mgr.on_method_invocation(&key);
    mgr.on_deoptimization(
        &key,
        DeoptReason::UnreachedCode,
        12,
        DeoptAction::MakeNotCompilable,
    );
    {
        let methods = mgr.core.methods.lock();
        let state = &methods[&key];
        assert!(state.entry_ineligible);
        assert!(!state.ineligible, "the OSR pipeline keeps its eligibility");
    }
    for _ in 0..5 {
        assert_eq!(
            mgr.on_method_invocation(&key),
            None,
            "method entry stays closed"
        );
    }
    assert!(
        mgr.request_osr(&key, 40).is_some(),
        "a hot loop in the method may still be OSR-compiled"
    );
}

/// A counted trap drops the request still queued for its method, so the
/// VM's eager re-queue gets the slot instead of a second task beside it.
#[test]
fn a_counted_trap_drops_the_queued_request() {
    let counts = crate::metrics::SchedulingCapture::start();
    let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
    let key = epoch_key("trappedWhileQueued");
    assert!(mgr.enqueue_compilation(CompilationTask {
        target_tier: CompilationTier::C2,
        priority: CompilationPriority::High,
        ..c1_task(&key)
    }));

    mgr.on_deoptimization(
        &key,
        DeoptReason::ClassCheck,
        3,
        DeoptAction::RecompileAndReinterpret,
    );
    assert_eq!(mgr.queue_size(), 0, "the stale C2 request is gone");
    assert_eq!(mgr.dropped_requests(), 1);
    assert_eq!(counts.count("queue_dropped_deoptimized"), 1);

    assert!(mgr.enqueue_compilation(CompilationTask {
        priority: CompilationPriority::High,
        ..c1_task(&key)
    }));
    assert_eq!(mgr.queue_size(), 1, "exactly one request, not two");    }

// ── Loader-aware method keys ─────────────────────────────────────────

#[test]
fn same_named_classes_in_different_loaders_tier_independently() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::with_install_epoch_source(
        policy,
        Some(Arc::new(AtomicU64::new(1))),
    );
    let app = MethodKey::with_class_id(ClassId::new(7), "com/example/Foo", "run", "()V");
    let plugin = MethodKey::with_class_id(ClassId::new(9), "com/example/Foo", "run", "()V");
    assert_ne!(app, plugin);

    assert_eq!(mgr.on_method_invocation(&app), Some(CompilationTier::C1));
    mgr.core
        .complete_task(&app, CompilationTier::C1, 0, false, false, true);
    mgr.mark_osr_denied(app.clone());

    assert_eq!(
        mgr.on_method_invocation(&plugin),
        Some(CompilationTier::C1),
        "one loader's ineligible verdict must not apply to the other's class"
    );
    assert!(!mgr.is_osr_denied(&plugin));

    // Unloading one loader's class leaves the other's state and queue alone.
    mgr.invalidate_class(ClassId::new(7), "com/example/Foo");
    let states = mgr.method_states();
    assert!(states.iter().all(|(k, _, _)| *k != app));
    assert!(states.iter().any(|(k, _, _)| *k == plugin));
    assert!(
        !mgr.is_osr_denied(&app),
        "unload forgets the class's denials"
    );
    // The app's C1 request (never dispatched here) went with its class.
    assert_eq!(mgr.queue_size(), 1, "the plugin's queued request survives");
}

/// A redefinition resets the redefined class's state by identity: a
/// same-named class in another loader keeps its OSR denials.
#[test]
fn redefining_one_loaders_class_leaves_a_same_named_class_alone() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let app = MethodKey::with_class_id(ClassId::new(7), "com/example/Bar", "run", "()V");
    let plugin = MethodKey::with_class_id(ClassId::new(9), "com/example/Bar", "run", "()V");
    mgr.mark_osr_denied(app.clone());
    mgr.mark_osr_denied(plugin.clone());
    assert!(mgr.is_osr_denied(&app));
    assert!(mgr.is_osr_denied(&plugin));

    mgr.on_class_redefined(ClassId::new(7), "com/example/Bar");
    assert!(
        !mgr.is_osr_denied(&app),
        "the redefined class forgets its denials"
    );
    assert!(
        mgr.is_osr_denied(&plugin),
        "the other loader's same-named class keeps its denials"
    );

    // With no identity to go on, a redefinition falls back to the name.
    mgr.on_class_redefined(ClassId::new(0), "com/example/Bar");
    assert!(!mgr.is_osr_denied(&plugin));
}

// ── Branch-profile windows: armed once, closed once ──────────────────

#[test]
fn every_branch_window_is_closed_exactly_once_whichever_way_its_request_leaves() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);

    // 1. A C1→C2 supersede dropped as stale.
    let stale = epoch_key("windowStale");
    mgr.on_method_invocation(&stale);
    mgr.compilation_complete(&stale, CompilationTier::C1, 1);
    mgr.core.request_c2_upgrade(&stale);
    assert_eq!(mgr.branch_window_balance(), 1);
    epoch.store(2, Ordering::Release);
    assert_task_eq(mgr.next_fresh_task(), None);
    // r11 wave 2: the method runs its C1 body, so nothing else would ever
    // re-form the supersede; the drop closes the dropped request's window
    // and the re-admission opens exactly one of its own.
    assert_eq!(
        mgr.branch_window_balance(),
        1,
        "a stale drop closes its window; the re-admitted request holds one"
    );
    let readmitted = mgr
        .next_fresh_task()
        .expect("the dropped supersede is re-admitted at the new epoch");
    assert_eq!(readmitted.method_key, stale);
    assert_eq!(readmitted.target_tier, CompilationTier::C2);
    mgr.core
        .complete_task(&stale, CompilationTier::C2, 1, true, false, false);
    assert_eq!(mgr.branch_window_balance(), 0);

    // 2. An OSR C2 completion never opened one and must not close one.
    let osr = epoch_key("windowOsr");
    let task = mgr.request_osr(&osr, 4).expect("OSR request");
    assert_task_eq(mgr.next_fresh_task(), Some(task));
    mgr.core
        .complete_task(&osr, CompilationTier::C2, 1, true, true, false);
    assert_eq!(
        mgr.branch_window_balance(),
        0,
        "an OSR completion closes nothing it did not open"
    );

    // 3. A deferred-new retry still queued when its class is invalidated.
    let unloaded = epoch_key("windowUnloaded");
    mgr.request_deferred_new_retry(&unloaded);
    assert_eq!(mgr.branch_window_balance(), 1);
    mgr.invalidate_class(ClassId::new(0), "craton/test/EpochSubject");
    assert_eq!(
        mgr.branch_window_balance(),
        0,
        "invalidate_class closes the window of the request it drops"
    );

    // 4. The ordinary exit: a supersede that compiles.
    let compiled = epoch_key("windowCompiled");
    mgr.on_method_invocation(&compiled);
    mgr.compilation_complete(&compiled, CompilationTier::C1, 1);
    mgr.core.request_c2_upgrade(&compiled);
    let task = mgr.next_fresh_task().expect("the supersede dispatches");
    mgr.core
        .complete_task(&compiled, task.target_tier, 1, true, false, false);
    assert_eq!(mgr.branch_window_balance(), 0);
}

/// Round 15 wave 5 (lane sync4): the static self-lock recompile door
/// (`r15w4-sync3-tiered-self-lock-recompile-door-patch-FIXED-20260930.md`) is the
/// deferred-`new` retry's C2 task under its own name: one C2 task, refused
/// while it is queued.
#[test]
fn the_self_lock_recompile_door_enqueues_one_c2_retry() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let key = epoch_key("selfLockRecompile");
    mgr.request_self_lock_recompile(&key);
    assert_eq!(mgr.queue_size(), 1);
    mgr.request_self_lock_recompile(&key);
    assert_eq!(mgr.queue_size(), 1, "already queued");
    let task = mgr.next_fresh_task().expect("the retry dispatches");
    assert_eq!(task.method_key, key);
    assert_eq!(task.target_tier, CompilationTier::C2);
}

/// Round 15 wave 5 (lane osrtier, OT-1): the unenterable-OSR promotion is
/// a METHOD-ENTRY C2 task (no `osr_bci`), refused while it is queued.
#[test]
fn the_osr_unenterable_promotion_enqueues_one_method_entry_c2_task() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let key = epoch_key("osrUnenterablePromotion");
    mgr.request_osr_unenterable_promotion(&key);
    assert_eq!(mgr.queue_size(), 1);
    mgr.request_osr_unenterable_promotion(&key);
    assert_eq!(mgr.queue_size(), 1, "already queued");
    let task = mgr.next_fresh_task().expect("the promotion dispatches");
    assert_eq!(task.method_key, key);
    assert_eq!(task.target_tier, CompilationTier::C2);
    assert_eq!(task.osr_bci, None);
}

/// r11-tier-stale-epoch-drop-loses-c2-upgrade-requests: a C1->C2 supersede
/// dropped as stale is re-admitted at the new epoch, because the method
/// runs its C1 body and no invocation hook will ever form it again. A C1
/// request (formed by the hook) is still only dropped, and an OSR one too.
#[test]
fn a_stale_c2_supersede_is_readmitted_at_the_new_epoch() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = epoch_driven_manager(&epoch);
    let key = epoch_key("supersedeOvertaken");
    mgr.on_method_invocation(&key);
    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    mgr.core.request_c2_upgrade(&key);
    assert_eq!(mgr.queue_size(), 1);

    epoch.store(2, Ordering::Release);
    assert_task_eq(mgr.next_fresh_task(), None);
    assert_eq!(
        mgr.dropped_requests(),
        1,
        "the stale request is still dropped"
    );
    assert_eq!(mgr.queue_size(), 1, "and a fresh one replaces it");
    let fresh = mgr
        .next_fresh_task()
        .expect("dispatchable at the new epoch");
    assert_eq!(fresh.method_key, key);
    assert_eq!(fresh.target_tier, CompilationTier::C2);
    assert_eq!(fresh.osr_bci, None);
    {
        let methods = mgr.core.methods.lock();
        assert_eq!(methods[&key].tier_fail_count, 0, "a drop is not a failure");
    }

    // A compile the epoch overtakes is re-admitted too, uncharged.
    epoch.store(3, Ordering::Release);
    mgr.core.finish(
        &key,
        Completion {
            tier: CompilationTier::C2,
            osr_bci: None,
            osr: false,
            compile_time_ms: 1,
            compile_time_ns: 0,
            budget_ms: 1,
            success: false,
            declined_permanently: false,
            epoch_moved: true,
            waiting_on_class: None,
            cap_refused: false,
            ticket: 0,
        },
    );
    let again = mgr
        .next_fresh_task()
        .expect("re-admitted after the overtaken compile");
    assert_eq!(again.target_tier, CompilationTier::C2);
    mgr.core
        .complete_task(&key, CompilationTier::C2, 1, true, false, false);
    assert_eq!(mgr.branch_window_balance(), 0, "every window closed once");

    // A hook-formed C1 request of an interpreted method is only dropped.
    let interp = epoch_key("interpretedC1");
    mgr.enqueue_compilation(c1_task(&interp));
    epoch.store(4, Ordering::Release);
    assert_task_eq(mgr.next_fresh_task(), None);
    assert_eq!(mgr.queue_size(), 0);
}

/// r11-tier-deopt-tiering-key-drops-the-class-id (tiered half): a deopt
/// reported under an identity-less key reaches the ONE state tracked under
/// that name, and a by-name re-queue does not mint a second, id-0 state.
/// With two states of that name nothing is charged (owner unknowable).
#[test]
fn an_identity_less_deopt_key_resolves_to_the_unique_named_state() {
    let mgr = TieredCompilationManager::new(CompilationPolicy::default());
    let real = MethodKey::with_class_id(ClassId::new(7), "com/example/Only", "run", "()V");
    let by_name = MethodKey::new("com/example/Only", "run", "()V");
    mgr.on_method_invocation(&real);
    mgr.compilation_complete(&real, CompilationTier::C1, 1);
    assert_eq!(mgr.current_tier(&real), CompilationTier::C1);

    mgr.on_deoptimization(
        &by_name,
        DeoptReason::ClassCheck,
        3,
        DeoptAction::RecompileAndReinterpret,
    );
    assert_eq!(
        mgr.current_tier(&real),
        CompilationTier::Interpreter,
        "the evicted body's real state drops to the interpreter"
    );
    assert!(mgr.enqueue_compilation(CompilationTask {
        method_key: by_name.clone(),
        target_tier: CompilationTier::C1,
        priority: CompilationPriority::High,
        enqueue_time_ms: 0,
        osr_bci: None,
    }));
    assert!(
        !mgr.method_states().iter().any(|(k, _, _)| *k == by_name),
        "no id-0 twin state"
    );
    let queued = mgr
        .dequeue_compilation()
        .expect("queued under the real key");
    assert_eq!(queued.method_key.class_id, ClassId::new(7));

    // Two loaders: ambiguous, so the by-name deopt charges neither.
    let a = MethodKey::with_class_id(ClassId::new(8), "com/example/Twin", "run", "()V");
    let b = MethodKey::with_class_id(ClassId::new(9), "com/example/Twin", "run", "()V");
    for k in [&a, &b] {
        mgr.on_method_invocation(k);
        mgr.compilation_complete(k, CompilationTier::C1, 1);
    }
    mgr.on_deoptimization(
        &MethodKey::new("com/example/Twin", "run", "()V"),
        DeoptReason::ClassCheck,
        3,
        DeoptAction::RecompileAndReinterpret,
    );
    assert_eq!(mgr.current_tier(&a), CompilationTier::C1);
    assert_eq!(mgr.current_tier(&b), CompilationTier::C1);
}

// ── Settled tiering: the interpreter stops asking ────────────────────

#[test]
fn a_method_that_can_never_compile_reports_settled_until_something_changes() {
    let policy = CompilationPolicy {
        c1_threshold: 1,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();

    let first = mgr.on_method_invocation_settling(&key, 0);
    assert_eq!(first.recommended, Some(CompilationTier::C1));
    assert_eq!(
        first.settled_generation, 0,
        "a method with a request in flight is not settled"
    );

    mgr.core
        .complete_task(&key, CompilationTier::C1, 0, false, false, true);
    let banned = mgr.on_method_invocation_settling(&key, 0);
    assert_eq!(banned.recommended, None);
    assert!(
        mgr.tiering_settled(banned.settled_generation),
        "an ineligible method is settled"
    );

    // Anything that could unsettle a method expires every stamp.
    mgr.on_class_redefined(ClassId::new(0), "java/lang/String");
    assert!(!mgr.tiering_settled(banned.settled_generation));
    assert_eq!(
        mgr.on_method_invocation_settling(&key, 0).recommended,
        Some(CompilationTier::C1),
        "the redefinition cleared the verdict"
    );
    assert!(!mgr.tiering_settled(0), "a zero stamp is never settled");
}

#[test]
fn observed_invocations_fast_forward_without_counting_the_report() {
    // Thresholds far above anything here, so no request takes the slot.
    let mgr = TieredCompilationManager::new(CompilationPolicy::default());
    let key = test_key();
    let count = |mgr: &TieredCompilationManager| {
        mgr.method_states()
            .into_iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, _, n)| n)
    };

    let _ = mgr.on_observed_invocations(&key, 0);
    assert_eq!(count(&mgr), Some(0), "a report of nothing counts nothing");
    let _ = mgr.on_observed_invocations(&key, 7);
    assert_eq!(count(&mgr), Some(7), "fast-forwarded, no +1");
    let _ = mgr.on_observed_invocations(&key, 7);
    let _ = mgr.on_observed_invocations(&key, 3);
    assert_eq!(
        count(&mgr),
        Some(7),
        "a repeated or stale report is a no-op"
    );
    // The invocation hook still charges the call itself.
    let _ = mgr.on_method_invocation_settling(&key, 7);
    assert_eq!(count(&mgr), Some(8));
}

/// The drain door (round 9 wave 3): a drained method has a published,
/// counted body by construction, so a method the manager still has at
/// `Interpreter` (its body came through a door that bypasses the manager)
/// is NOT recommended for C1 -- that would recompile the body it runs --
/// but IS taken straight to C2 once it crosses the C2 bar. The invocation
/// hook, which reports a genuinely interpreted call, still recommends C1.
#[test]
fn observed_invocations_never_recompile_a_counted_body_at_c1() {
    let policy = CompilationPolicy {
        c1_threshold: 10,
        c2_threshold: 100,
        c2_min_invocations: 50,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();
    assert_eq!(mgr.on_observed_invocations(&key, 9).recommended, None);
    let v = mgr.on_observed_invocations(&key, 10);
    assert_eq!(v.recommended, None, "no C1 recompile of a counted body");
    assert_eq!(v.settled_generation, 0, "not settled: C2 is still ahead");
    assert_eq!(mgr.queue_size(), 0);
    assert_eq!(mgr.on_observed_invocations(&key, 99).recommended, None);
    let v = mgr.on_observed_invocations(&key, 100);
    assert_eq!(
        v.recommended,
        Some(CompilationTier::C2),
        "past the C2 bar the drain takes it straight to the optimizing tier"
    );
    assert_eq!(mgr.queue_size(), 1);

    // The invocation hook is unchanged.
    let hooked = test_key2();
    let v = mgr.on_method_invocation_settling(&hooked, 10);
    assert_eq!(v.recommended, Some(CompilationTier::C1));
}

/// The drain door's reason to exist: a method whose C1 body has landed
/// (so the invocation hook never asks again) is promoted to C2 by the
/// drained compiled-entry count alone.
#[test]
fn observed_invocations_promote_a_c1_method_past_the_c2_bar() {
    let policy = CompilationPolicy {
        c1_threshold: 10,
        c2_threshold: 100,
        c2_min_invocations: 50,
        ..CompilationPolicy::default()
    };
    let mgr = TieredCompilationManager::new(policy);
    let key = test_key();
    for _ in 0..10 {
        mgr.on_method_invocation(&key);
    }
    let task = mgr.dequeue_compilation().expect("the C1 request");
    assert_eq!(task.target_tier, CompilationTier::C1);
    mgr.compilation_complete(&key, CompilationTier::C1, 1);
    assert_eq!(mgr.current_tier(&key), CompilationTier::C1);

    assert_eq!(mgr.on_observed_invocations(&key, 99).recommended, None);
    assert_eq!(
        mgr.on_observed_invocations(&key, 100).recommended,
        Some(CompilationTier::C2)
    );
    let task = mgr.dequeue_compilation().expect("the C2 request");
    assert_eq!(task.target_tier, CompilationTier::C2);
    assert_eq!(task.osr_bci, None);
}

// ── Panic containment ────────────────────────────────────────────────

#[test]
fn a_contained_panic_is_visible_to_the_panic_hook_as_contained() {
    assert!(!compile_panic_is_contained());
    let inside = contain_compile_panic(compile_panic_is_contained).expect("no panic");
    assert!(
        inside,
        "the crash handler must see the scope while the hook runs"
    );
    let caught =
        contain_compile_panic(|| -> () { panic!("deliberate contained panic (test)") });
    assert!(caught.is_err());
    assert!(
        !compile_panic_is_contained(),
        "the scope closes again after the unwind"
    );
}

#[test]
fn the_worker_survives_a_panicking_compile_fn() {
    use std::sync::mpsc;
    let policy = CompilationPolicy {
        c1_threshold: 1,
        c2_threshold: u32::MAX,
        c2_min_invocations: u32::MAX,
        osr_threshold: u32::MAX,
        tiered_enabled: true,
        c1_profiling: true,
    };
    let mgr = worker_manager(policy);
    let doomed = MethodKey::new("craton/test/Panics", "boom", "()V");
    let healthy = MethodKey::new("craton/test/Panics", "fine", "()V");

    let (tx, rx) = mpsc::channel::<MethodKey>();
    let bg = mgr
        .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
            if &*task.method_key.method_name == "boom" {
                panic!("deliberate compile panic (test)");
            }
            tx.send(task.method_key.clone()).unwrap();
            CompileOutcome {
                compile_time_ms: 1,
                published: true,
                c2_upgrade_candidate: false,
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: false,
            }
        }))
        .expect("worker should start");

    assert_eq!(mgr.on_method_invocation(&doomed), Some(CompilationTier::C1));
    let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
    while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(
        mgr.worker_panics(),
        1,
        "the panic was contained and counted"
    );

    // The same lane keeps serving: the next C1 compile still runs.
    assert_eq!(
        mgr.on_method_invocation(&healthy),
        Some(CompilationTier::C1)
    );
    assert_eq!(
        rx.recv_timeout(WORKER_RENDEZVOUS)
            .expect("the worker must survive the panic"),
        healthy
    );
    let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
    while mgr.completed_compilations() < 2 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    {
        let methods = mgr.core.methods.lock();
        assert!(
            methods[&doomed].ineligible,
            "a panicking compile is a permanent decline"
        );
        assert!(
            !methods[&doomed].queued_for_compilation,
            "and its slot is released"
        );
    }
    assert_eq!(
        mgr.inflight_install_epoch(),
        0,
        "the in-flight epoch is reset"
    );
    assert_eq!(
        mgr.inflight_compiles(),
        0,
        "and the count agrees — `0` is a legal epoch, so the epoch alone \
         cannot distinguish idle from 'running at epoch 0'"
    );
    assert_eq!(
        mgr.inflight_slot_exhausted(),
        0,
        "every dispatch found a slot"
    );
    assert!(mgr.compiler_active());
    drop(bg);
}


/// Concurrent dispatches each keep their OWN epoch, and the reader reports
/// the oldest.
///
/// The single-word record this replaced could not do either: the last
/// store won whatever order the dispatches happened in, and a worker
/// finishing out of order left a stale value standing. Both halves are
/// checked here with dispatches interleaved deliberately.
#[test]
fn concurrent_dispatches_keep_their_own_epochs() {
    let core = CompilerCore::with_install_epoch_source(None);

    let a = core.note_dispatch(7);
    let b = core.note_dispatch(9);
    assert_eq!(
        core.inflight_oldest_epoch(),
        7,
        "the OLDEST in-flight dispatch is the answer, not the most recent \
         store — with one word, `b` would have overwritten `a`"
    );
    assert_eq!(core.inflight_count.load(Ordering::Acquire), 2);

    // Finish out of order: the newer compile completes first.
    core.note_dispatch_done(b);
    assert_eq!(
        core.inflight_oldest_epoch(),
        7,
        "`a` is still running at epoch 7"
    );

    core.note_dispatch_done(a);
    assert_eq!(
        core.inflight_oldest_epoch(),
        0,
        "idle once every dispatch is done"
    );
    assert_eq!(core.inflight_count.load(Ordering::Acquire), 0);
    assert_eq!(core.inflight_slot_exhausted.load(Ordering::Acquire), 0);
}

/// Install epoch `0` is a legal state, and the record does not confuse it
/// with "idle".
///
/// `0` is where `with_install_epoch_source`'s counter starts, so a
/// sentinel that collides with it would report every compile dispatched at
/// the first epoch as "no compile running". The slots hold `epoch + 1` for
/// exactly this reason.
#[test]
fn epoch_zero_is_a_dispatch_not_an_idle_worker() {
    let core = CompilerCore::with_install_epoch_source(None);
    assert_eq!(core.inflight_count.load(Ordering::Acquire), 0);

    let slot = core.note_dispatch(0);
    assert_eq!(core.inflight_oldest_epoch(), 0, "the epoch really is 0");
    assert_eq!(
        core.inflight_count.load(Ordering::Acquire),
        1,
        "and the COUNT is what distinguishes this from idle"
    );

    core.note_dispatch_done(slot);
    assert_eq!(core.inflight_count.load(Ordering::Acquire), 0);
}

/// Running out of slots is counted, not silently ignored, and the count
/// still balances.
#[test]
fn a_dispatch_with_no_free_slot_is_counted_and_still_balances() {
    let core = CompilerCore::with_install_epoch_source(None);
    let claimed: Vec<InflightSlot> = (0..INFLIGHT_EPOCH_SLOTS)
        .map(|i| core.note_dispatch(i as u64))
        .collect();
    assert_eq!(core.inflight_slot_exhausted.load(Ordering::Acquire), 0);

    let overflow = core.note_dispatch(9999);
    assert_eq!(overflow, InflightSlot::NONE);
    assert_eq!(
        core.inflight_slot_exhausted.load(Ordering::Acquire),
        1,
        "a dispatch that could not record its epoch is reported"
    );
    assert_eq!(
        core.inflight_count.load(Ordering::Acquire),
        INFLIGHT_EPOCH_SLOTS as u64 + 1,
        "and it is still counted as running"
    );

    core.note_dispatch_done(overflow);
    for slot in claimed {
        core.note_dispatch_done(slot);
    }
    assert_eq!(
        core.inflight_count.load(Ordering::Acquire),
        0,
        "handing back InflightSlot::NONE clears nothing but still balances \
         the count"
    );
    assert_eq!(core.inflight_oldest_epoch(), 0);
}

// ── Per-manager workers ──────────────────────────────────────────────

#[test]
fn a_second_manager_in_one_process_drains_its_own_queue() {
    use std::sync::mpsc;
    let policy = || CompilationPolicy {
        c1_threshold: 1,
        c2_threshold: u32::MAX,
        c2_min_invocations: u32::MAX,
        osr_threshold: u32::MAX,
        tiered_enabled: true,
        c1_profiling: true,
    };
    let compile_into = |tx: mpsc::Sender<MethodKey>| -> CompileFn {
        Box::new(move |task: &CompilationTask| -> CompileOutcome {
            tx.send(task.method_key.clone()).unwrap();
            CompileOutcome {
                compile_time_ms: 1,
                published: true,
                c2_upgrade_candidate: false,
                deferred_new_retry: false,
                declined_permanently: false,
                optimizing_publication_refused: false,
            }
        })
    };
    let first = worker_manager(policy());
    let second = worker_manager(policy());
    let (tx1, rx1) = mpsc::channel::<MethodKey>();
    let (tx2, rx2) = mpsc::channel::<MethodKey>();
    // The production door, for both: it used to be a process-wide `Once`,
    // so the second call started nothing.
    ensure_background_compiler(&first, || compile_into(tx1));
    ensure_background_compiler(&second, || compile_into(tx2));
    assert!(first.compiler_active() && second.compiler_active());

    let k1 = MethodKey::new("craton/test/VmOne", "m", "()V");
    let k2 = MethodKey::new("craton/test/VmTwo", "m", "()V");
    assert_eq!(second.on_method_invocation(&k2), Some(CompilationTier::C1));
    assert_eq!(
        rx2.recv_timeout(WORKER_RENDEZVOUS)
            .expect("the second VM's queue is drained"),
        k2
    );
    assert_eq!(first.on_method_invocation(&k1), Some(CompilationTier::C1));
    assert_eq!(
        rx1.recv_timeout(WORKER_RENDEZVOUS)
            .expect("the first VM's queue is drained"),
        k1
    );
    assert!(
        rx1.try_recv().is_err() && rx2.try_recv().is_err(),
        "neither manager compiled the other's request"
    );

    second.shutdown_background_compiler();
    assert!(!second.compiler_active());
    assert!(
        first.compiler_active(),
        "stopping one VM's workers leaves the other's running"
    );
}

/// r9 wave 7 (review7a): a trap ledger idle for several decay periods owes
/// one halving PER period, as the deopt log's ledger applies them -- not
/// the single halving the next trap used to apply however long it had been.
#[test]
fn trap_decay_applies_every_period_it_owes() {
    let mut state = MethodState::new(test_key());
    state.trap_decay_ms = 1_000;
    state.trap_decay_invocations = 0;
    state.invocation_count = 0;
    state.deopt_count = 8;
    state.trap_counts.insert((DeoptReason::NullCheck, 3), 8);
    state.trap_counts.insert((DeoptReason::ClassCheck, 9), 1);

    // Less than one period: nothing moves.
    decay_traps(&mut state, 1_000 + TRAP_DECAY_MS - 1);
    assert_eq!(state.deopt_count, 8);
    assert_eq!(state.trap_decay_ms, 1_000);

    // Three whole periods and a bit: three halvings, and the stamp moves
    // by the whole periods only, keeping the remainder owed.
    decay_traps(&mut state, 1_000 + 3 * TRAP_DECAY_MS + 7);
    assert_eq!(state.deopt_count, 1, "8 >> 3");
    assert_eq!(
        state.trap_counts.get(&(DeoptReason::NullCheck, 3)),
        Some(&1)
    );
    assert!(
        !state
            .trap_counts
            .contains_key(&(DeoptReason::ClassCheck, 9)),
        "a site halved to zero is dropped"
    );
    assert_eq!(state.trap_decay_ms, 1_000 + 3 * TRAP_DECAY_MS);

    // Invocation periods count too, and the larger debt wins.
    state.deopt_count = 16;
    state.invocation_count = 2 * TRAP_DECAY_INVOCATIONS;
    decay_traps(&mut state, 1_000 + 3 * TRAP_DECAY_MS + 8);
    assert_eq!(state.deopt_count, 4, "16 >> 2 invocation periods");
    assert_eq!(state.trap_decay_invocations, 2 * TRAP_DECAY_INVOCATIONS);

    // An enormous idle gap saturates at zero without an overflowing shift.
    state.deopt_count = u32::MAX;
    decay_traps(&mut state, u64::MAX);
    assert_eq!(state.deopt_count, 0);
}
