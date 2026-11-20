use super::*;
use crate::zgc::mark::TestMarkContext;
use rustc_hash::FxHashMap;
use std::sync::atomic::AtomicUsize;

/// Build a graph from `(node, children)` pairs, auto-inserting any child
/// that was not declared as a leaf. Mirrors `zgc::mark`'s test helper so
/// the two read the same way.
fn graph(edges: &[(u64, &[u64])]) -> FxHashMap<u64, Vec<u64>> {
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    for (node, children) in edges {
        g.insert(*node, children.to_vec());
        for c in children.iter() {
            g.entry(*c).or_default();
        }
    }
    g
}

/// A safepoint that hands the load barrier one scripted address per
/// mark-end flush, newest entry last.
///
/// This is how these tests make a mutator mark arrive *exactly* at the
/// mark-end pause, deterministically: the driver calls
/// `flush_mutator_buffers` inside the safepoint, so an address published
/// from here lands in the ingress immediately before `try_end_mark`
/// probes it. No sleeps, no races.
struct ScriptedSafepoint {
    /// Popped from the back, so the script reads bottom-up. Empty means
    /// "flush nothing", i.e. a quiet safepoint.
    script: Mutex<Vec<u64>>,
    entered: AtomicUsize,
    left: AtomicUsize,
    flushes: AtomicUsize,
}

impl ScriptedSafepoint {
    fn new(script: Vec<u64>) -> Self {
        ScriptedSafepoint {
            script: Mutex::new(script),
            entered: AtomicUsize::new(0),
            left: AtomicUsize::new(0),
            flushes: AtomicUsize::new(0),
        }
    }

    fn balanced(&self) -> bool {
        self.entered.load(Ordering::Relaxed) == self.left.load(Ordering::Relaxed)
    }
}

impl ZgcMarkSafepoint for ScriptedSafepoint {
    fn begin_mark_end_safepoint(&self) {
        self.entered.fetch_add(1, Ordering::Relaxed);
    }

    fn flush_mutator_buffers(&self, handle: &ZMarkHandle) -> usize {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        let next = self.script.lock().pop();
        match next {
            // `mark_live` is the load-barrier slow path: it marks and
            // hands the address to the ingress, which is exactly what a
            // per-thread buffer flush would have produced.
            Some(addr) => usize::from(handle.mark_live(0, addr)),
            None => 0,
        }
    }

    fn end_mark_end_safepoint(&self) {
        self.left.fetch_add(1, Ordering::Relaxed);
    }
}

/// Reference hook that resurrects everything in `candidates` that is not
/// already strongly marked, and records what it observed as live.
struct KeepAliveHook {
    candidates: Vec<u64>,
    observed_live: Mutex<Vec<u64>>,
    calls: AtomicUsize,
}

impl ZNonStrongRefHook for KeepAliveHook {
    fn process(&self, is_marked: &dyn Fn(u64) -> bool, keep_alive: &mut dyn FnMut(u64)) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut live = self.observed_live.lock();
        for &c in &self.candidates {
            if is_marked(c) {
                live.push(c);
            } else {
                keep_alive(c);
            }
        }
    }
}

/// Open a cycle over `g` rooted at `roots`, returning the context and the
/// pool. The caller drives the rest.
fn open_cycle(
    g: FxHashMap<u64, Vec<u64>>,
    roots: &[u64],
    workers: usize,
) -> (Arc<TestMarkContext>, Arc<ZMarkCoordinator>) {
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = Arc::new(ZMarkCoordinator::new(ctx.clone(), workers));
    pool.begin_cycle();
    pool.push_roots(roots);
    (ctx, pool)
}

// -- lifecycle ----------------------------------------------------------

/// The driver spawns, marks the reachable set, and joins cleanly.
///
/// The replacement for the simulation-era
/// `zgc_concurrent_mark_thread_spawns_and_joins`, which could only assert
/// that a thread started and stopped — there was no object graph for it
/// to mark. This one asserts the mark set.
#[test]
fn driver_marks_the_reachable_set_and_joins_cleanly() {
    let g = graph(&[(1, &[2, 3]), (2, &[4]), (3, &[4, 5]), (4, &[]), (5, &[])]);
    let (ctx, pool) = open_cycle(g, &[1], 2);

    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint.clone(),
    ));

    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert!(outcome.mark_set_complete, "{outcome:?}");
    assert_eq!(outcome.passes, 1, "a quiet cycle needs exactly one pass");
    assert_eq!(outcome.restarts, 0);
    assert_eq!(outcome.redrains, 0);
    assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4, 5]);
    assert!(
        safepoint.balanced(),
        "every begin_mark_end_safepoint must be paired with an end"
    );
    assert_eq!(safepoint.flushes.load(Ordering::Relaxed), 1);
}

/// `run_inline` is the same cycle as `spawn` + `join_cycle`, on this thread.
#[test]
fn run_inline_marks_the_reachable_set_on_the_calling_thread() {
    let g = graph(&[
        (1, &[2, 3]),
        (2, &[4]),
        (3, &[4, 5]),
        (4, &[]),
        (5, &[]),
        (6, &[]),
    ]);
    let (ctx, pool) = open_cycle(g, &[1], 2);
    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let outcome = ZgcConcurrentMarkController::run_inline(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint.clone(),
    ))
    .expect("an inline cycle that does not panic returns Ok");
    pool.end_cycle();
    assert!(outcome.mark_set_complete, "{outcome:?}");
    assert_eq!(outcome.passes, 1);
    assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4, 5]);
    assert!(safepoint.balanced());
    assert_eq!(safepoint.flushes.load(Ordering::Relaxed), 1);
}

/// zs s5/mc3: the spawn-or-inline driver marks the same set through a
/// spawned driver thread (the arm a working spawn takes). The inline
/// fallback is `run_inline`, tested above; no seam injects a spawn failure.
#[test]
fn zs5mc3_spawn_and_join_or_inline_marks_the_reachable_set() {
    let g = graph(&[(1, &[2, 3]), (2, &[4]), (3, &[]), (4, &[]), (5, &[])]);
    let (ctx, pool) = open_cycle(g, &[1], 2);
    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let mut spawned = false;
    let outcome = ZgcConcurrentMarkController::spawn_and_join_or_inline(
        ZgcConcurrentMarkParams::new(Arc::clone(&pool), safepoint.clone()),
        &mut spawned,
        None,
    )
    .expect("a cycle that does not panic returns Ok");
    pool.end_cycle();
    assert!(spawned, "the driver thread spawned");
    assert!(outcome.mark_set_complete, "{outcome:?}");
    assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4]);
    assert!(safepoint.balanced());
    // zt t8/rl2: unpriced, the driver reads no clock.
    assert_eq!(outcome.driver_thread_cpu_ns, 0, "None prices nothing");
}

/// A safepoint whose begin burns `ms` of the calling (driver) thread's
/// CPU, then panics: a driver that fails after real work.
struct SpinThenPanicSafepoint {
    ms: u64,
}

impl ZgcMarkSafepoint for SpinThenPanicSafepoint {
    fn begin_mark_end_safepoint(&self) {
        if let Some(t0) = crate::zgc::mark::thread_cpu_nanos() {
            let mut x = 0u64;
            while crate::zgc::mark::thread_cpu_nanos().unwrap_or(u64::MAX)
                < t0 + self.ms * 1_000_000
            {
                x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(1));
            }
        }
        panic!("zt8rl2 test: injected driver panic after real work");
    }

    fn flush_mutator_buffers(&self, _handle: &ZMarkHandle) -> usize {
        0
    }

    fn end_mark_end_safepoint(&self) {}
}

/// zt t8/rl2 (`zgc-zs3ac-collection-cpu-residual-shares` items 2-3): a
/// PRICED spawned driver hands its CPU to the caller's cell, and its
/// outcome carries the same span (the cell, read after the thread
/// exited, is never less); a driver that PANICS after burning 30 ms still
/// hands those 30 ms over -- the caller's panic arm used to book nothing
/// of it.
#[test]
fn zt8rl2_a_priced_driver_hands_its_cpu_over_even_when_it_panics() {
    let g = graph(&[(1, &[2, 3]), (2, &[]), (3, &[])]);
    let (_ctx, pool) = open_cycle(g, &[1], 2);
    let mut spawned = false;
    let mut cell = u64::MAX;
    let outcome = ZgcConcurrentMarkController::spawn_and_join_or_inline(
        ZgcConcurrentMarkParams::new(Arc::clone(&pool), Arc::new(ZgcNoMutatorSafepoint)),
        &mut spawned,
        Some(&mut cell),
    )
    .expect("a quiet cycle returns Ok");
    pool.end_cycle();
    assert!(spawned, "the driver thread spawned");
    assert!(outcome.mark_set_complete, "{outcome:?}");
    assert_ne!(cell, u64::MAX, "the caller's cell was written");
    assert!(
        cell >= outcome.driver_thread_cpu_ns,
        "the exit span covers the outcome's ({cell} < {})",
        outcome.driver_thread_cpu_ns
    );
    drop(pool);

    // The panic arm: Err, and the cell still carries the work.
    let g = graph(&[(1, &[2]), (2, &[])]);
    let (_ctx, pool) = open_cycle(g, &[1], 2);
    let mut cell = 0u64;
    let got = ZgcConcurrentMarkController::spawn_and_join_or_inline(
        ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            Arc::new(SpinThenPanicSafepoint { ms: 30 }),
        ),
        &mut spawned,
        Some(&mut cell),
    );
    assert!(got.is_err(), "the driver's panic reaches the caller");
    assert!(spawned);
    pool.end_cycle();
    drop(pool);
    if crate::zgc::mark::thread_cpu_nanos().is_some() {
        // Spun to the same clock's +30 ms, so even a quantum clock reads
        // at least that.
        assert!(
            cell >= 30_000_000,
            "a panicking driver handed over {cell} ns of its 30 ms"
        );
    }
}

/// **The driver's fixed-point wait is NOTIFIED, not polled.**
///
/// # Why this is a count and not a timing
///
/// The failure it guards is invisible by construction. Every wait in the
/// marker is a `wait_for` and never a bare `wait`, so a missing notification
/// does not hang — it costs a poll interval and behaves in every other way
/// exactly like a working one. That is what was true here until 2026-08-17:
/// the driver waited on `ZgcConcurrentMarkState`'s own condvar, which no ZGC
/// path ever notified for the completion case, so every pass of every cycle
/// ran the full 5 ms out before noticing a fixed point the workers had
/// already reached. A wall-clock assertion could catch that and would be the
/// wrong instrument: flaky on a loaded machine, and silent about why.
///
/// # Why the mark is deliberately made SLOW
///
/// `park_timeouts == 0` would be satisfied by a wait that was never entered,
/// and that is exactly what a fast mark produces — the driver arrives after
/// the workers have converged and takes the `is_terminated` fast path. So the
/// hook holds the first visit for 37 ms, which guarantees the driver is
/// inside the wait, and the assertion is on
/// `park_termination_wakes`: the wait was released by the termination edge
/// itself. Timeouts *during* those 37 ms are expected and are not asserted
/// on.
///
/// 37 and not 35 or 40: the interval is 5 ms, so a sleep at a multiple of it
/// puts termination and a timeout in the same microseconds, and the test
/// would flake on which won. 37 leaves the driver 3 ms into a fresh wait when
/// the mark converges.
#[test]
fn the_drivers_fixed_point_wait_is_woken_by_the_termination_edge() {
    let g = graph(&[(1, &[2, 3]), (2, &[4]), (3, &[4, 5]), (4, &[]), (5, &[])]);
    let slept = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&slept);
    let ctx = Arc::new(TestMarkContext::new(g).with_visit_hook(Box::new(move |_| {
        // FIRST VISIT ONLY, so the mark takes ~37 ms rather than 37 ms per
        // object -- the point is to be slower than one poll interval, not to
        // be slow.
        if !s2.swap(true, Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(37));
        }
    })));
    let pool = Arc::new(ZMarkCoordinator::new(ctx.clone(), 2));
    pool.begin_cycle();
    pool.push_roots(&[1]);

    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint,
    ));
    let outcome = controller.join_cycle().expect("driver joined");

    assert!(outcome.mark_set_complete, "{outcome:?}");
    assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4, 5]);
    assert!(
        slept.load(Ordering::Relaxed),
        "the hook must have run, or the mark was not slowed and the driver \
         may never have entered its wait"
    );
    assert!(
        pool.shared().terminator().park_termination_wakes() >= 1,
        "the driver's wait must be released by the TERMINATION EDGE, not by a \
         timeout. Zero here is the 5 ms-per-pass floor coming back, and it \
         would otherwise show up only as a slower pause that somebody has to \
         attribute. timeouts={}",
        pool.shared().terminator().park_timeouts()
    );
    pool.end_cycle();
}

/// A cycle with no roots converges without marking anything, and the
/// driver still publishes a well-formed outcome.
#[test]
fn an_empty_root_set_converges_in_one_pass() {
    let (ctx, pool) = open_cycle(graph(&[(1, &[2]), (2, &[])]), &[], 3);
    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint,
    ));

    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert!(outcome.mark_set_complete);
    assert_eq!(outcome.passes, 1);
    assert_eq!(ctx.marked_count(), 0);
}

// -- the restart loop ---------------------------------------------------

/// **The reason this module exists.** A mutator mark that arrives at the
/// mark-end pause must force the concurrent phase to run again, and the
/// object's whole closure must end up marked.
///
/// Under SATB a restart means the mark-end flush found buffered work
/// (corrected zs s2/mc2: this said "cannot happen", against the module
/// header -- this heap IS SATB); the mutators are producers until they
/// are stopped, so the pause is a decision point.
/// The scripted safepoint publishes `50` during the first flush, which is
/// exactly the interleaving the restart loop exists for.
#[test]
fn a_mark_arriving_at_the_mark_end_pause_forces_a_restart() {
    let g = graph(&[
        (1, &[2]),
        (2, &[]),
        // Reachable only through the simulated load barrier.
        (50, &[51, 52]),
        (51, &[53]),
        (52, &[]),
        (53, &[]),
    ]);
    let (ctx, pool) = open_cycle(g, &[1], 2);

    let safepoint = Arc::new(ScriptedSafepoint::new(vec![50]));
    let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint.clone(),
    ));

    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert!(outcome.mark_set_complete, "{outcome:?}");
    assert_eq!(
        outcome.restarts, 1,
        "the mark-end probe must have seen the mutator's mark exactly once"
    );
    assert_eq!(
        outcome.passes, 2,
        "one restart means two concurrent phases ran"
    );
    assert_eq!(
        ctx.marked_sorted(),
        vec![1, 2, 50, 51, 52, 53],
        "the mutator's object AND its transitive closure must be marked"
    );
    assert_eq!(
        safepoint.flushes.load(Ordering::Relaxed),
        2,
        "one flush per mark-end pause"
    );
    assert!(safepoint.balanced());
    assert!(
        pool.stats().mark_end_restarts.load(Ordering::Relaxed) >= 1,
        "the engine must have counted the restart too"
    );
}

/// Several late marks in a row are each folded in; the loop iterates as
/// many times as it needs to and still terminates.
#[test]
fn successive_late_marks_each_force_another_pass() {
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    g.insert(1, Vec::new());
    let mut expected = vec![1u64];
    let mut script = Vec::new();
    for i in 0..5u64 {
        let a = 200 + i * 2;
        let b = 201 + i * 2;
        g.insert(a, vec![b]);
        g.insert(b, Vec::new());
        expected.push(a);
        expected.push(b);
        script.push(a);
    }
    // Popped from the back: the order does not matter, only the count.
    let (ctx, pool) = open_cycle(g, &[1], 3);

    let safepoint = Arc::new(ScriptedSafepoint::new(script));
    let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint,
    ));

    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert!(outcome.mark_set_complete, "{outcome:?}");
    assert_eq!(outcome.restarts, 5);
    assert_eq!(outcome.passes, 6);
    expected.sort_unstable();
    assert_eq!(ctx.marked_sorted(), expected);
}

/// The ceiling trips instead of looping forever.
///
/// The script publishes a fresh object at **every** mark-end pause, so
/// the fixed point is never final and an unbounded loop would never
/// return — which is how a GC livelock presents to a user. With
/// `max_mark_end_restarts = 2` the driver gives up on the third restart,
/// logs a `tracing::warn!`, and — the part that actually matters —
/// reports `mark_set_complete == false` so the caller cannot mistake the
/// partial mark set for a sweepable one.
#[test]
fn the_restart_ceiling_trips_instead_of_looping_forever() {
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    g.insert(1, Vec::new());
    // Far more injections than the budget allows, so the loop can only
    // end by hitting the ceiling.
    let mut script = Vec::new();
    for i in 0..64u64 {
        let a = 300 + i;
        g.insert(a, Vec::new());
        script.push(a);
    }
    let (_ctx, pool) = open_cycle(g, &[1], 2);

    let safepoint = Arc::new(ScriptedSafepoint::new(script));
    let params = ZgcConcurrentMarkParams::new(Arc::clone(&pool), safepoint.clone())
        .with_max_mark_end_restarts(2);
    let controller = ZgcConcurrentMarkController::spawn(params);

    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert!(
        outcome.restart_budget_exhausted,
        "the ceiling must trip: {outcome:?}"
    );
    assert!(
        !outcome.mark_set_complete,
        "an exhausted budget leaves the mark set INCOMPLETE; reporting it as \
         complete would be a use-after-free waiting to happen"
    );
    assert_eq!(
        outcome.restarts, 3,
        "the budget is exceeded on the restart AFTER the budget'th one"
    );
    assert_eq!(outcome.passes, 3);
    assert!(!outcome.stopped_early);
    assert!(safepoint.balanced());
}

// -- reference processing ----------------------------------------------

/// A resurrected referent drags an unscanned subgraph behind it, so the
/// driver must re-drain after the hook — and must stop once the hook has
/// nothing left to resurrect.
#[test]
fn a_resurrected_referent_triggers_a_redrain_of_its_closure() {
    let g = graph(&[
        (1, &[2]),
        (2, &[]),
        // Softly-reachable island: strongly unreachable, so the strong
        // mark cannot find it.
        (60, &[61]),
        (61, &[62]),
        (62, &[]),
    ]);
    let (ctx, pool) = open_cycle(g, &[1], 2);

    let hook = Arc::new(KeepAliveHook {
        candidates: vec![2, 60],
        observed_live: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let params = ZgcConcurrentMarkParams::new(Arc::clone(&pool), safepoint)
        .with_refs(hook.clone() as Arc<dyn ZNonStrongRefHook + Send + Sync>);
    let controller = ZgcConcurrentMarkController::spawn(params);

    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert!(outcome.mark_set_complete, "{outcome:?}");
    assert_eq!(
        outcome.resurrected, 1,
        "only 60 was dead; 2 was already live"
    );
    assert_eq!(
        outcome.redrains, 1,
        "the resurrection must force a re-drain"
    );
    assert_eq!(
        outcome.passes, 2,
        "one strong pass, then one more to trace the resurrection"
    );
    assert_eq!(
        ctx.marked_sorted(),
        vec![1, 2, 60, 61, 62],
        "a resurrected referent drags its whole subgraph with it"
    );
    assert_eq!(
        hook.calls.load(Ordering::Relaxed),
        2,
        "the hook runs again after the re-drain, and only then reports nothing \
         left to resurrect"
    );
    assert_eq!(
        hook.observed_live.lock().clone(),
        vec![2, 2, 60],
        "round 1 saw only the strongly-live 2; round 2 saw 2 and the now-live 60 \
         — the hook must never be told an object is live merely because it \
         itself just marked it"
    );
}

/// With no hook the phase is skipped entirely and the cycle is a single
/// pass. Guards against a future refactor that runs an empty reference
/// phase and pays a safepoint for it.
#[test]
fn no_reference_hook_means_no_reference_safepoint() {
    let (_ctx, pool) = open_cycle(graph(&[(1, &[2]), (2, &[])]), &[1], 2);
    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint.clone(),
    ));

    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert!(outcome.mark_set_complete);
    assert_eq!(outcome.redrains, 0);
    assert_eq!(
        safepoint.entered.load(Ordering::Relaxed),
        1,
        "exactly one safepoint: the mark-end pause"
    );
}

/// A hook that resurrects without ever converging must hit the round
/// ceiling rather than spinning forever.
#[test]
fn the_resurrection_ceiling_trips_instead_of_looping_forever() {
    /// Resurrects a *fresh* object every round, so the fixed point of
    /// "nothing left to resurrect" is never reached. A real hook cannot
    /// do this (marking is monotone), which is exactly why the bound is a
    /// guard against a broken hook rather than a policy knob.
    struct NeverConvergingHook {
        next: AtomicUsize,
    }
    impl ZNonStrongRefHook for NeverConvergingHook {
        fn process(&self, _is_marked: &dyn Fn(u64) -> bool, keep_alive: &mut dyn FnMut(u64)) {
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            keep_alive(400 + i as u64);
        }
    }

    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    g.insert(1, Vec::new());
    for i in 0..64u64 {
        g.insert(400 + i, Vec::new());
    }
    let (_ctx, pool) = open_cycle(g, &[1], 2);

    let hook = Arc::new(NeverConvergingHook {
        next: AtomicUsize::new(0),
    });
    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let params = ZgcConcurrentMarkParams::new(Arc::clone(&pool), safepoint.clone())
        .with_refs(hook as Arc<dyn ZNonStrongRefHook + Send + Sync>)
        .with_max_resurrection_rounds(3);
    let controller = ZgcConcurrentMarkController::spawn(params);

    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert!(
        outcome.resurrection_budget_exhausted,
        "the round ceiling must trip: {outcome:?}"
    );
    assert!(!outcome.mark_set_complete);
    assert_eq!(outcome.redrains, 4, "budget 3 means the 4th round gives up");
    assert!(safepoint.balanced());
}

// -- shutdown -----------------------------------------------------------

/// Stopping the controller while marking is in flight must not hang.
///
/// The pool is deliberately wedged: a visit hook holds a worker inside
/// `visit_refs`, so the fixed point cannot be reached and the driver is
/// parked in `await_fixed_point`. `request_stop_and_join` must still
/// return — the driver's wait is bounded and it re-reads the stop flag every
/// interval.
///
/// **This doc used to end "if it waited on the pool's own condvar instead,
/// this would deadlock, which is the whole reason `await_fixed_point`
/// polls". That was wrong, and since 2026-08-17 the driver DOES wait on the
/// terminator's condvar** — so this test is now the proof of the change
/// rather than the reason against it. It cannot deadlock, because
/// `ZMarkTerminator::wait_for_fixed_point` takes the *caller's* stop flag as
/// a parameter and re-checks it under a bounded `wait_for`. See
/// `_DRIVER_POLL_MS_RETIRED` for the API confusion that produced the
/// original claim.
///
/// Nothing here asserts on elapsed time; the test simply cannot pass
/// without the join returning.
#[test]
fn stopping_the_driver_mid_cycle_does_not_hang() {
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    let mut children = Vec::new();
    for i in 0..64u64 {
        let c = 500 + i;
        g.insert(c, Vec::new());
        children.push(c);
    }
    g.insert(1, children);

    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));
    let hook_release = Arc::clone(&release);
    let hook_entered = Arc::clone(&entered);
    let ctx = Arc::new(
        TestMarkContext::new(g).with_visit_hook(Box::new(move |addr: u64| {
            if addr != 1 {
                return;
            }
            hook_entered.fetch_add(1, Ordering::SeqCst);
            // Bounded spin, exactly as `zgc::mark`'s own interleaving
            // tests do it: if the wedge does not materialise on this run
            // the test still checks that the join returns, it just does
            // not exercise the mid-cycle case that time.
            let mut spins: u64 = 0;
            while !hook_release.load(Ordering::Acquire) && spins < 20_000_000 {
                spins += 1;
                std::thread::yield_now();
            }
        })),
    );

    let pool = Arc::new(ZMarkCoordinator::new(ctx.clone(), 2));
    pool.begin_cycle();
    pool.push_roots(&[1]);

    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint,
    ));

    // Wait (bounded, unasserted) until a worker is actually wedged inside
    // `visit_refs`, so the stop really does land mid-cycle.
    let mut spins: u64 = 0;
    while entered.load(Ordering::SeqCst) == 0 && spins < 20_000_000 {
        spins += 1;
        std::thread::yield_now();
    }

    controller
        .request_stop_and_join()
        .expect("driver must join after a stop request");

    // Let the wedged worker out so the pool can be torn down.
    release.store(true, Ordering::Release);
    pool.end_cycle();
    // The pool's own `Drop` stops and joins the workers when this last
    // clone goes; doing it explicitly keeps the teardown in the test.
    drop(pool);
}

/// Dropping the controller without joining must not hang either, and must
/// leave the detached driver able to finish on its own.
#[test]
fn dropping_the_controller_requests_stop_without_blocking() {
    let (_ctx, pool) = open_cycle(graph(&[(1, &[2]), (2, &[3]), (3, &[])]), &[1], 2);
    let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
    let state = {
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint,
        ));
        let state = Arc::clone(&controller.state);
        // Dropped here: flips the stop flag and detaches.
        state
    };
    assert!(
        state.should_stop.load(Ordering::Acquire),
        "Drop must request a stop"
    );
    pool.end_cycle();
}

// -- seam sanity --------------------------------------------------------

/// The no-mutator safepoint is a legitimate `ZgcMarkSafepoint`: it stops
/// nothing and flushes nothing, which is correct precisely when there is
/// no other mutator. Pins the default so an accidental change to it is a
/// test failure rather than a silent unsoundness in single-threaded
/// embeddings.
#[test]
fn the_no_mutator_safepoint_drives_a_cycle() {
    let (ctx, pool) = open_cycle(graph(&[(1, &[2]), (2, &[3]), (3, &[])]), &[1], 1);
    let safepoint: Arc<dyn ZgcMarkSafepoint> = Arc::new(ZgcNoMutatorSafepoint);
    let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint,
    ));

    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert!(outcome.mark_set_complete);
    assert_eq!(outcome.passes, 1);
    assert_eq!(ctx.marked_sorted(), vec![1, 2, 3]);
}

/// Telemetry on the shared state tracks the outcome. Guards the accessor
/// surface a caller polls while the cycle is still running.
#[test]
fn state_telemetry_matches_the_cycle_outcome() {
    let g = graph(&[(1, &[2]), (2, &[]), (70, &[71]), (71, &[])]);
    let (_ctx, pool) = open_cycle(g, &[1], 2);
    let safepoint = Arc::new(ScriptedSafepoint::new(vec![70]));
    let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
        Arc::clone(&pool),
        safepoint,
    ));

    let state = Arc::clone(&controller.state);
    let outcome = controller.join_cycle().expect("driver joined");
    pool.end_cycle();

    assert_eq!(
        state.passes_performed.load(Ordering::Relaxed),
        outcome.passes as u64
    );
    assert_eq!(
        state.restarts_performed.load(Ordering::Relaxed),
        outcome.restarts as u64
    );
    assert_eq!(state.outcome(), Some(outcome));
}
