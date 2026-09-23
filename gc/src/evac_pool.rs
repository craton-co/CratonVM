//! Persistent worker pool for stop-the-world evacuation.
//!
//! Two collectors drive it: G1's `parallel_evacuate` (which it was written
//! for, and whose threads are still named `g1-evac-N` because every crash
//! record in `docs/internal/` identifies a worker by that name) and the
//! GENERATIONAL young cycle's parallel evacuator, `gen_evac::ParEvac::drain`.
//! Neither owns it -- each holds its own `OnceLock<EvacPool>` -- so the width
//! of a pool is whatever its first collection asked for.
//!
//! As of gengc-round2-alloc2 (2026-09-20) that is no longer PERMANENT:
//! [`EvacPool::ensure_helpers`] spawns the difference on demand, so an owner
//! whose worker policy widens later in the run can grow the pool to match. The
//! pool still cannot decide to grow on its own -- both callers clamp their
//! request to [`EvacPool::helpers`] *before* dispatching, so `scope` never
//! learns it was asked for more -- which is why the sibling half of
//! `docs/internal/gaps/gengc-alloc-evac-pool-width-frozen-20260920.md` (the
//! `gen_heap.rs` call site that must ask for the POLICY width rather than one
//! cycle's plan width) is still open.
//!
//! # Why this exists
//!
//! `G1Collector::parallel_evacuate` used to open a `std::thread::scope` and
//! spawn its helper threads inside every collection. That is correct — scope's
//! join is a clean synchronisation point — but it puts thread creation on the
//! pause path, where it is pure overhead measured against a pause budget of
//! `max_gc_pause_ms`. A young pause on a small heap can copy a few hundred
//! kilobytes in well under a millisecond, and spawning three OS threads to do
//! it is a significant fraction of that. The threads are also identical from
//! one pause to the next, so recreating them buys nothing.
//!
//! This pool creates them once and parks them on a condvar. Each pause
//! publishes a job, wakes the parked threads, runs the driver's own share on
//! the calling thread, and blocks until every helper has returned.
//!
//! # The safety argument
//!
//! `std::thread::scope` proves "no spawned thread outlives the borrowed data"
//! in the type system. A persistent pool cannot: its threads outlive any one
//! job, so the borrow has to be erased to a raw pointer and the lifetime
//! re-established dynamically. [`EvacPool::scope`] is where that argument
//! lives, and it rests on exactly one property:
//!
//! > `scope` does not return until `pending == 0`, and `pending` is decremented
//! > by a worker only after its call through the erased pointer has returned —
//! > including when that call unwinds.
//!
//! The "including when it unwinds" half is not a detail. If a panicking worker
//! skipped its decrement, the driver would wait on `done` forever while holding
//! the regions lock, with the panic that caused it never reported. That is
//! precisely the failure `run_worker`'s `RetireOnExit` guard exists to prevent
//! one level down (a leaked `outstanding` count once cost 3h08m of CPU in a
//! suite that finishes in 2.5s), so this level takes the same care: the worker
//! body runs under [`catch_unwind`], the decrement happens unconditionally, and
//! the panic is re-raised on the driver thread after the barrier instead of
//! being swallowed.
//!
//! # What this pool deliberately is not
//!
//! It is not a general executor. There is no queue of independent tasks, no
//! work stealing between jobs, and no way to dispatch a second job while one is
//! running — `scope` holds `dispatch` for its whole extent, so an attempt to
//! nest or overlap blocks rather than aliasing two jobs onto one thread.
//! Evacuation is stop-the-world and single-driver, so that is the whole
//! required contract; anything more would be a second concurrency design to
//! audit for no benefit.

use std::any::Any;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::{Condvar, Mutex};

/// Dispatches that found a job already published — see the fail-safe in
/// [`EvacPool::scope`].
///
/// Expected to be ZERO, and structurally unreachable while `scope` holds the
/// `dispatch` mutex for its whole extent. It is a counter rather than a
/// `debug_assert!` because the site is inside a stop-the-world pause: a panic
/// there leaves the driver on a barrier no helper will reach, which is a
/// strictly worse outcome than the overlap it would be reporting.
pub static EVAC_POOL_DISPATCH_WHILE_BUSY: AtomicU64 = AtomicU64::new(0);

/// Snapshot of [`EVAC_POOL_DISPATCH_WHILE_BUSY`].
pub fn evac_pool_dispatch_while_busy() -> u64 {
    EVAC_POOL_DISPATCH_WHILE_BUSY.load(Ordering::Relaxed)
}

/// A job in flight: the caller's closure, type-erased.
///
/// `ptr` is a `&F` for the `F` that `run` was instantiated with. It is only
/// ever read while `EvacPool::scope` is on the stack of the thread that
/// published it, which is what keeps the reference live — see the module note.
#[derive(Clone, Copy)]
struct Job {
    ptr: *const (),
    run: unsafe fn(*const (), usize),
}

// SAFETY: `Job` is handed to worker threads and dereferenced there. The pointee
// is a `&F` where `F: Fn(usize) + Sync`, so concurrent calls from several
// threads are exactly what `Sync` permits, and `scope`'s completion barrier
// guarantees no worker touches it after the reference dies.
unsafe impl Send for Job {}

struct State {
    /// Bumped once per dispatched job. A worker compares it against the last
    /// generation it observed, which makes a wake-up idempotent: a spurious
    /// condvar wake, or a second worker waking on the same notification, can
    /// never cause a job to be run twice or missed.
    generation: u64,
    job: Option<Job>,
    /// How many of the pool's workers this job wants. Workers with an index at
    /// or beyond it sit the round out.
    active: usize,
    /// Workers that have not yet finished the current job.
    pending: usize,
    /// A worker unwound while running the current job. Re-raised by the driver
    /// after the barrier.
    panicked: bool,
    /// The FIRST such worker's panic payload, carried to the driver so the
    /// pause fails with the message the worker actually produced.
    ///
    /// gengc-round1-alloc, 2026-09-20: this used to be dropped on the worker
    /// thread and replaced at the barrier with a fixed
    /// `"g1 parallel-evac worker panicked"` string. Everything that says WHICH
    /// invariant broke lives in the payload -- the assertion text, the
    /// `heap_types.rs` line, the object address -- and the whole reason the
    /// panic is re-raised on the driver at all is that a worker's own
    /// `thread '<name>' panicked at ...` line is printed on a thread with no
    /// collection context. Losing the payload meant the surviving report named
    /// the mechanism (a worker died) and nothing about the cause. Recorded for
    /// the first panicking worker only: later ones are almost always the same
    /// fault re-hit by a peer, and the first is the one the state is closest
    /// to.
    panic_payload: Option<Box<dyn Any + Send>>,
    shutdown: bool,
    /// One condvar per spawned worker, indexed by worker id. Worker `id` waits
    /// on `wake[id]` and nothing else, so a dispatch of width `w` notifies
    /// exactly `0..w` and the workers sized out of the round are never woken.
    ///
    /// gengc-round2-alloc2, 2026-09-20: this replaces a SINGLE `Condvar` that
    /// every dispatch `notify_all`-ed. A worker at or beyond `active` sat the
    /// round out correctly, but it could only DISCOVER that by waking, taking
    /// `state`, reading `generation` and `active`, and parking again -- so a
    /// pool of 8 running a 2-worker plan performed 8 futex wakes and 8
    /// acquisitions of the very mutex the 2 real workers need in order to
    /// decrement `pending`. `ParEvac::plan` narrows the width whenever
    /// to-space headroom cannot buy one buffer per worker, so "the job is
    /// narrower than the pool" is the normal case under memory pressure, which
    /// is also when the pause matters most. See
    /// `docs/internal/gaps/gengc-alloc-evac-pool-wakes-every-worker-20260920.md`.
    ///
    /// `Arc<Condvar>` and not `Condvar`: the vector GROWS
    /// ([`EvacPool::ensure_helpers`]), and a reallocation would MOVE a
    /// `Condvar` a parked worker is waiting on. Each worker captures its own
    /// `Arc` at spawn, so a realloc only moves pointers.
    ///
    /// Every one of them is notified on shutdown -- `Drop` loops rather than
    /// relying on one `notify_all`, which is the failure mode (a join that
    /// never returns) this module exists to avoid.
    wake: Vec<Arc<Condvar>>,
}

struct Inner {
    state: Mutex<State>,
    /// Signalled when `pending` reaches zero.
    done: Condvar,
}

// ---------------------------------------------------------------------------
// Pool-width census (gengc-round3-tlabreport, 2026-09-21)
// ---------------------------------------------------------------------------
//
// `gengc-alloc-evac-pool-width-frozen-20260920.md` step (4): "`EvacPool::
// helpers()` should be published on the `[GC]` shutdown line beside the
// requested width, so 'the pool was narrower than the ask' stops needing a
// temporary `eprintln!` to see."
//
// The REQUESTED width is not reachable from here and cannot be made so: both
// callers clamp their request to `helpers()` before they call `scope`
// (`gen_evac.rs`'s `ParEvac::drain` and `g1.rs:9113`), so a counter of
// "clamped below the request" would be an unfireable instrument -- the same
// shape as `arena.rs`'s `note_region_leak` and the two write-only TLAB
// tripwires, and it is not added here for exactly that reason.
//
// What IS reachable, and is enough to read the defect off a log, is the pool's
// own history. The frozen-width signature is `built_with == max_dispatched`
// with `grew_by=0` over a large `dispatches` -- a pool pinned at the first
// cycle's width for the life of the run. `grew_by` doubles as the ENGAGEMENT
// counter for `ensure_helpers`, which has no production caller yet (that one
// line is part (b), in `gen_heap.rs`), so `grew_by=0` is also how a run says
// part (b) is still open rather than landed-and-inert.
//
// Process-wide, spanning both owners' pools, because neither owner is
// reachable from the printer either. A run uses one collector, so in practice
// these describe that collector's pool. Cost: one relaxed op per pool
// construction, per grow and per DISPATCH (once per parallel evacuation, not
// per region and not per object).
//
// The current width is printed, but it is deliberately NOT a counter of its
// own: it is rendered as the derived `built_with + grew_by`. A second static
// tracking the same quantity is how two numbers in one summary come to
// disagree, which is the defect class this round kept finding.
static POOL_BUILT_WITH: AtomicUsize = AtomicUsize::new(0);
static POOL_GREW_BY: AtomicUsize = AtomicUsize::new(0);
static POOL_SPAWN_REFUSED: AtomicU64 = AtomicU64::new(0);
static POOL_DISPATCHES: AtomicU64 = AtomicU64::new(0);
static POOL_INLINE_RUNS: AtomicU64 = AtomicU64::new(0);
static POOL_MAX_DISPATCHED: AtomicUsize = AtomicUsize::new(0);

/// `(built_with, grew_by, spawn_refused, dispatches, inline_runs,
/// max_dispatched)` — the process-wide evacuation-pool width census. See the
/// note above [`EvacPool`].
///
/// `built_with` is the widest `EvacPool::new` in the process (helpers, i.e.
/// threads besides the driver); `grew_by` is helpers added by
/// [`EvacPool::ensure_helpers`]; the pool's current width is their sum.
/// `inline_runs` counts `scope` calls that found no helpers at all and ran the
/// driver alone.
pub(crate) fn evac_pool_census() -> (usize, usize, u64, u64, u64, usize) {
    (
        POOL_BUILT_WITH.load(Ordering::Relaxed),
        POOL_GREW_BY.load(Ordering::Relaxed),
        POOL_SPAWN_REFUSED.load(Ordering::Relaxed),
        POOL_DISPATCHES.load(Ordering::Relaxed),
        POOL_INLINE_RUNS.load(Ordering::Relaxed),
        POOL_MAX_DISPATCHED.load(Ordering::Relaxed),
    )
}

/// The `[GC]` shutdown line for [`evac_pool_census`], ready to print.
///
/// Formatted here rather than in `vm_heap.rs` for the reason
/// `tlab::tlab_census_lines` is: the fields and their meaning belong with the
/// mechanism, and the decision to print belongs with the summary. `pool_`
/// prefixes every key because `vm_heap.rs`'s
/// `every_gc_summary_key_is_unique_across_the_whole_summary` requires a name
/// no other summary line owns.
pub fn evac_pool_census_line() -> String {
    let (built, grew, refused, dispatches, inline, max_dispatched) = evac_pool_census();
    format!(
        "[GC] evac_pool: pool_built_with={built} pool_grew_by={grew} \
         pool_width={} pool_dispatches={dispatches} pool_inline_runs={inline} \
         pool_max_dispatched={max_dispatched} pool_spawn_refused={refused}",
        built + grew,
    )
}

/// A fixed set of evacuation worker threads, reused across collections.
pub struct EvacPool {
    inner: Arc<Inner>,
    /// Join handles, in spawn order. Behind a `Mutex` only because
    /// [`Self::ensure_helpers`] appends to it through `&self`; `Drop` drains it
    /// through `get_mut` and takes no lock.
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// `state.wake.len()`, readable without taking `state`.
    ///
    /// Only ever INCREASES (the pool never shrinks -- a parked worker costs a
    /// stack and nothing else, and joining one would have to happen outside a
    /// pause), so a stale read is a conservative clamp in [`Self::scope`] and
    /// never an over-dispatch.
    spawned: AtomicUsize,
    /// Serialises `scope`. See the module note on why nesting is not supported.
    /// Also held across a grow, so a pool can never widen mid-dispatch.
    dispatch: Mutex<()>,
}

impl EvacPool {
    /// Create a pool of `helpers` worker threads.
    ///
    /// `helpers` is the number of threads *besides* the driver, so a
    /// four-worker evacuation wants `EvacPool::new(3)`. `new(0)` is legal and
    /// yields a pool that always runs the driver's share inline.
    pub fn new(helpers: usize) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                generation: 0,
                job: None,
                active: 0,
                pending: 0,
                panicked: false,
                panic_payload: None,
                shutdown: false,
                wake: Vec::with_capacity(helpers),
            }),
            done: Condvar::new(),
        });

        let pool = Self {
            inner,
            threads: Mutex::new(Vec::with_capacity(helpers)),
            spawned: AtomicUsize::new(0),
            dispatch: Mutex::new(()),
        };
        for _ in 0..helpers {
            // A pool thread that cannot be created is not a condition this
            // collector can degrade around mid-construction; the caller sizes
            // the pool once, at collector setup. (`ensure_helpers`, which grows
            // a LIVE pool, degrades instead -- see there.)
            assert!(
                pool.spawn_one(),
                "g1: failed to spawn evacuation worker thread"
            );
        }
        // The width this pool is frozen at until something calls
        // `ensure_helpers`. `fetch_max` and not `store`: a process that built
        // two pools (it does not today -- one collector per run) should report
        // the widest rather than whichever was constructed last.
        POOL_BUILT_WITH.fetch_max(helpers, Ordering::Relaxed);
        pool
    }

    /// Spawn one more worker. `false` if the OS refused the thread.
    ///
    /// Takes `threads` then `state`, which is the only order any caller uses.
    /// A worker takes `state` alone, so it cannot participate in a cycle.
    fn spawn_one(&self) -> bool {
        let mut threads = self.threads.lock();
        let mut st = self.inner.state.lock();
        if st.shutdown {
            return false;
        }
        let id = st.wake.len();
        // A worker spawned into a LIVE pool must not mistake the round that
        // has already been dispatched (or the stale `generation` left by the
        // last one) for work addressed to it: `st.job` is `None` between
        // dispatches, so the `expect` in `worker_loop` would fire. Seeding its
        // `last_generation` from the counter as it stands now -- under both
        // locks, and with `ensure_helpers` holding `dispatch` so no dispatch
        // can be in flight -- makes the NEXT dispatch the first round it sees.
        let start_generation = st.generation;
        let cv = Arc::new(Condvar::new());
        let worker_cv = Arc::clone(&cv);
        let inner = Arc::clone(&self.inner);
        match std::thread::Builder::new()
            // Named so a stuck pause is identifiable in a native stack dump
            // without cross-referencing thread ids.
            .name(format!("g1-evac-{id}"))
            .spawn(move || worker_loop(&inner, id, &worker_cv, start_generation))
        {
            Ok(handle) => {
                threads.push(handle);
                st.wake.push(cv);
                // Published AFTER the condvar is in `wake`, so a `scope` that
                // reads the new width finds a slot to notify.
                self.spawned.store(st.wake.len(), Ordering::Release);
                true
            }
            Err(_) => {
                // Not fatal from `ensure_helpers` (see there), and `new`
                // asserts -- but a refusal silently narrows a pool that a
                // policy asked to widen, so it is counted rather than
                // swallowed.
                POOL_SPAWN_REFUSED.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Worker threads available besides the driver.
    pub fn helpers(&self) -> usize {
        self.spawned.load(Ordering::Acquire)
    }

    /// Grow the pool to at least `helpers` workers, spawning the difference.
    ///
    /// # Why a pool has to be able to grow
    ///
    /// Both owners hold their pool in a `OnceLock` and build it with whatever
    /// the FIRST parallel cycle asked for. On the generational path that number
    /// is the narrowest the run can produce, for two independent reasons:
    /// `young_gc_threads` returns `1` until the young generation crosses
    /// `CRATONVM_GC_PAR_MIN_BYTES`, so the first cycle that crosses it crosses
    /// by the smallest margin it ever will; and the call site sizes the pool
    /// from `plan.workers`, which `ParEvac::plan` lowers whenever to-space
    /// headroom cannot buy one buffer per worker. One tight-headroom cycle at
    /// the start of a run therefore pinned the pool at one helper for the life
    /// of the process, and `scope`'s clamp made that silent -- the collector
    /// reported the width it ASKED for while the pool quietly ran narrower.
    /// See `docs/internal/gaps/gengc-alloc-evac-pool-width-frozen-20260920.md`.
    ///
    /// # Never shrinks
    ///
    /// A parked worker costs a stack and, since the per-worker condvar landed,
    /// not even a wake-up. Joining one would have to happen outside a pause and
    /// would need a second synchronisation design; growth is monotone instead,
    /// which is also what makes [`Self::helpers`] safe to read without a lock.
    ///
    /// # Ordering
    ///
    /// Takes `dispatch` first, so this cannot interleave with a `scope`: a pool
    /// never widens while a job is in flight, and a new worker's
    /// `last_generation` is seeded from a counter that cannot move under it.
    /// Calling this from inside a dispatched body (or a driver closure) would
    /// deadlock on `dispatch`, exactly as a nested `scope` already does.
    ///
    /// A thread the OS refuses is not fatal here -- unlike in [`Self::new`],
    /// there is a working pool to fall back to and `scope` clamps to it.
    pub fn ensure_helpers(&self, helpers: usize) {
        if helpers <= self.helpers() {
            return;
        }
        let _dispatch = self.dispatch.lock();
        let before = self.helpers();
        while self.helpers() < helpers {
            if !self.spawn_one() {
                break;
            }
        }
        // Helpers actually ADDED, not requested: a refused thread must not
        // inflate the width this census reports, or `built_with + grew_by`
        // would claim workers that do not exist.
        POOL_GREW_BY.fetch_add(self.helpers().saturating_sub(before), Ordering::Relaxed);
    }

    /// Run `body(i)` for `i` in `0..helpers` on pool threads and `driver()` on
    /// the calling thread, returning only once all of them have finished.
    ///
    /// `helpers` is clamped to [`Self::helpers`]. Passing `0` (or owning an
    /// empty pool) runs `driver()` inline and dispatches nothing, which is the
    /// single-worker determinism mode `CRATONVM_G1_WORKERS=1` selects.
    ///
    /// # Panics
    ///
    /// If `driver` panics, the panic is re-raised here — but only *after* every
    /// dispatched `body` has returned, because they still hold a reference to
    /// it. If any `body` panics, its **own payload** is re-raised here after
    /// the barrier (gengc-round1-alloc, 2026-09-20: it used to be replaced by a
    /// fixed string, which threw away the assertion text that says which
    /// invariant broke). A panic on both sides reports the driver's, since it
    /// is the one carrying the collection context.
    pub fn scope<F>(&self, helpers: usize, body: &F, driver: impl FnOnce())
    where
        F: Fn(usize) + Sync,
    {
        // Read before `dispatch` is taken, as it always was: the width only
        // ever grows, so a stale read can only clamp lower than necessary --
        // never above the number of workers that exist. Keeping the read here
        // also keeps the `helpers == 0` inline arm free of `dispatch`, which a
        // caller could otherwise re-enter.
        let helpers = helpers.min(self.helpers());
        if helpers == 0 {
            // Counted apart from a dispatch: "every evacuation ran inline"
            // and "every evacuation ran on one helper" are different facts
            // about a run and only one of them is a pool of width zero.
            POOL_INLINE_RUNS.fetch_add(1, Ordering::Relaxed);
            driver();
            return;
        }
        // One relaxed pair per parallel evacuation. `max_dispatched` is what
        // the pool ACTUALLY ran at its widest; beside `built_with` and
        // `grew_by` it is how a log says the pool was pinned at the first
        // cycle's width. See the census note above `EvacPool`.
        POOL_DISPATCHES.fetch_add(1, Ordering::Relaxed);
        POOL_MAX_DISPATCHED.fetch_max(helpers, Ordering::Relaxed);

        // SAFETY (call site of the erased pointer): `p` is the `&F` published
        // below, and `F: Fn(usize) + Sync` allows the concurrent calls.
        unsafe fn call<F: Fn(usize) + Sync>(p: *const (), i: usize) {
            (*(p as *const F))(i)
        }

        let _dispatch = self.dispatch.lock();
        let job = Job {
            ptr: body as *const F as *const (),
            run: call::<F>,
        };

        {
            let mut st = self.inner.state.lock();
            // A COUNTED FAIL-SAFE, NOT A `debug_assert!` (lane D wave 2,
            // 2026-09-20).
            //
            // This line is reached INSIDE a stop-the-world pause, with the
            // regions lock held and every mutator parked, and the audit's
            // G1-11 note is about exactly that: a `debug_assert!` here turns a
            // condition the pause could have reported into a panic that
            // unwinds through the dispatch and leaves the driver blocked on a
            // barrier whose helpers were never woken. The assertion
            // manufactures a worse failure than the one it detects.
            //
            // It was left as an assert because "the `dispatch` mutex makes it
            // unreachable by construction" — which is true today and is an
            // assertion ABOUT THE LOCK, not about pause state. The point of
            // writing it down as a counter instead is that the day the lock
            // changes, this stops being a compile-time-only check that nobody
            // runs in release and becomes a number in the shutdown report.
            // Overwriting `st.job` is also the benign arm: the previous job's
            // pointer is dead by the time `scope` returns, and clobbering it
            // costs a dispatch rather than a hang.
            if st.job.is_some() {
                EVAC_POOL_DISPATCH_WHILE_BUSY.fetch_add(1, Ordering::Relaxed);
            }
            st.job = Some(job);
            st.active = helpers;
            st.pending = helpers;
            st.panicked = false;
            // The previous job's payload, if it was never consumed, dies here
            // rather than being re-raised against an unrelated collection.
            st.panic_payload = None;
            st.generation = st.generation.wrapping_add(1);
            // EXACTLY the workers this job asked for. `helpers` was clamped to
            // the width above and the vector only grows, so the slice is always
            // in bounds. See `State::wake` for why the sized-out workers are
            // left alone rather than woken to discover it for themselves.
            debug_assert!(helpers <= st.wake.len());
            for cv in st.wake.iter().take(helpers) {
                cv.notify_one();
            }
        }

        // Catch rather than propagate: helpers still hold `&F`, so the barrier
        // below is not optional even on the unwind path.
        let driver_result = catch_unwind(AssertUnwindSafe(driver));

        let worker_panic = {
            let mut st = self.inner.state.lock();
            while st.pending > 0 {
                self.inner.done.wait(&mut st);
            }
            st.job = None;
            // Disarm the round. Nothing reads `active` between dispatches
            // today, but a worker spawned by `ensure_helpers` reaches its first
            // lock acquisition at an arbitrary moment, and "no job is live" is
            // cheaper to state here than to re-derive at every reader.
            st.active = 0;
            // Take it: a payload left in the state would be re-raised by the
            // NEXT collection's barrier, which is a report against the wrong
            // pause. (`panicked` is reset at dispatch for the same reason.)
            st.panic_payload.take()
        };

        if let Err(payload) = driver_result {
            resume_unwind(payload);
        }
        if let Some(payload) = worker_panic {
            // The worker's own payload, not a summary of it: see
            // `State::panic_payload`. `resume_unwind` does not re-print the
            // `thread ... panicked` line, so the worker's original message is
            // already on stderr and this carries the same value up the
            // collector's stack.
            resume_unwind(payload);
        }
    }
}

impl Drop for EvacPool {
    fn drop(&mut self) {
        {
            let mut st = self.inner.state.lock();
            st.shutdown = true;
            // EVERY worker, not just the last round's. With one shared condvar
            // a single `notify_all` reached them all; with one condvar each,
            // a worker this loop forgets is a `join` that never returns --
            // precisely the hang this module was written to avoid.
            for cv in &st.wake {
                cv.notify_one();
            }
        }
        for handle in self.threads.get_mut().drain(..) {
            // A worker only exits its loop through the shutdown flag, and any
            // panic inside a job was caught and reported at the barrier, so a
            // join error here would mean the loop itself unwound — nothing is
            // recoverable at that point and there is no caller to tell.
            let _ = handle.join();
        }
    }
}

fn worker_loop(inner: &Inner, id: usize, wake: &Condvar, start_generation: u64) {
    // Seeded by `EvacPool::spawn_one` from the counter as it stood when this
    // worker was created, NOT from 0: a worker spawned into a live pool by
    // `ensure_helpers` would otherwise read the last round's `generation` as
    // new work and `expect` a `job` that the barrier has already cleared.
    let mut last_generation = start_generation;
    loop {
        let job = {
            let mut st = inner.state.lock();
            loop {
                if st.shutdown {
                    return;
                }
                if st.generation != last_generation {
                    last_generation = st.generation;
                    if id < st.active {
                        break st.job.expect("job published for the current generation");
                    }
                    // Sized out of this round. Since the per-worker condvar
                    // landed this is reached only via a SPURIOUS wake (a
                    // dispatch notifies exactly `0..active`), but it must
                    // stay: `last_generation` is advanced so the round is not
                    // later mistaken for new work, and `pending` must NOT be
                    // touched -- it counts only the workers the job asked for.
                    continue;
                }
                wake.wait(&mut st);
            }
        };

        // SAFETY: the driver holds `&F` alive across the whole dispatch, and
        // does not return from `scope` until this worker's decrement below has
        // landed. See the module-level safety argument.
        let result = catch_unwind(AssertUnwindSafe(|| unsafe { (job.run)(job.ptr, id) }));

        let mut st = inner.state.lock();
        if let Err(payload) = result {
            st.panicked = true;
            // First panicking worker only -- see `State::panic_payload`.
            if st.panic_payload.is_none() {
                st.panic_payload = Some(payload);
            }
        }
        st.pending -= 1;
        if st.pending == 0 {
            inner.done.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Every dispatched index runs exactly once, and the driver's share runs on
    /// the calling thread.
    #[test]
    fn a_job_reaches_every_worker_exactly_once() {
        let pool = EvacPool::new(3);
        let seen: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();
        let driver_thread = std::thread::current().id();
        let driver_ran = AtomicUsize::new(0);

        for _ in 0..50 {
            for s in &seen {
                s.store(0, Ordering::Relaxed);
            }
            let body = |i: usize| {
                seen[i].fetch_add(1, Ordering::Relaxed);
            };
            pool.scope(3, &body, || {
                assert_eq!(std::thread::current().id(), driver_thread);
                driver_ran.fetch_add(1, Ordering::Relaxed);
            });
            for (i, s) in seen.iter().enumerate() {
                assert_eq!(
                    s.load(Ordering::Relaxed),
                    1,
                    "worker {i} ran the wrong number of times"
                );
            }
        }
        assert_eq!(driver_ran.load(Ordering::Relaxed), 50);
    }

    /// The barrier is real: nothing a worker writes may still be in flight when
    /// `scope` returns, because the driver reads those slots immediately
    /// afterwards to merge the forwarding shards.
    #[test]
    fn scope_does_not_return_before_every_worker_has_finished() {
        let pool = EvacPool::new(3);
        let counter = AtomicUsize::new(0);
        let body = |_: usize| {
            // Long enough that a missing barrier loses the race reliably.
            std::thread::sleep(std::time::Duration::from_millis(5));
            counter.fetch_add(1, Ordering::SeqCst);
        };
        pool.scope(3, &body, || {});
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    /// A pool with fewer threads than the job asks for must still terminate —
    /// `pending` has to be sized from the clamped count, not the request.
    #[test]
    fn a_request_larger_than_the_pool_is_clamped_rather_than_hanging() {
        let pool = EvacPool::new(2);
        let counter = AtomicUsize::new(0);
        let body = |_: usize| {
            counter.fetch_add(1, Ordering::SeqCst);
        };
        pool.scope(8, &body, || {});
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    /// Sizing a job below the pool's width must leave the unused workers parked
    /// AND still let the barrier fall, which is the case a naive
    /// `pending = threads.len()` would hang on.
    #[test]
    fn a_job_narrower_than_the_pool_leaves_the_extra_workers_parked() {
        let pool = EvacPool::new(4);
        let seen: Vec<AtomicUsize> = (0..4).map(|_| AtomicUsize::new(0)).collect();
        let body = |i: usize| {
            seen[i].fetch_add(1, Ordering::Relaxed);
        };
        pool.scope(2, &body, || {});
        assert_eq!(seen[0].load(Ordering::Relaxed), 1);
        assert_eq!(seen[1].load(Ordering::Relaxed), 1);
        assert_eq!(seen[2].load(Ordering::Relaxed), 0);
        assert_eq!(seen[3].load(Ordering::Relaxed), 0);
        // ...and the pool is still usable at full width afterwards.
        pool.scope(4, &body, || {});
        for s in &seen {
            assert!(s.load(Ordering::Relaxed) >= 1);
        }
    }

    /// THE hang case. A worker panic must surface on the driver thread, not
    /// leave `pending` stuck above zero with the regions lock held.
    #[test]
    fn a_panicking_worker_fails_the_pause_instead_of_wedging_it() {
        let pool = EvacPool::new(2);
        let body = |i: usize| {
            if i == 0 {
                panic!("worker exploded");
            }
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| pool.scope(2, &body, || {})));
        assert!(outcome.is_err(), "the driver must observe the worker panic");
        // And the pool survives it: a leaked `pending` would hang here.
        let counter = AtomicUsize::new(0);
        let ok = |_: usize| {
            counter.fetch_add(1, Ordering::SeqCst);
        };
        pool.scope(2, &ok, || {});
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    /// gengc-round1-alloc, 2026-09-20 — the worker's OWN panic payload reaches
    /// the driver.
    ///
    /// The message is the whole diagnostic value of a worker panic: it carries
    /// the assertion text, the source location and (for the evacuator's
    /// header screens) the object address. Replacing it with a fixed summary
    /// string left the surviving report naming the mechanism and nothing about
    /// the cause.
    #[test]
    fn a_worker_panic_arrives_with_its_own_message() {
        let pool = EvacPool::new(2);
        let body = |i: usize| {
            if i == 1 {
                panic!("forwarding pointer at 0x2008684 is not a base");
            }
        };
        let err = catch_unwind(AssertUnwindSafe(|| pool.scope(2, &body, || {})))
            .expect_err("the driver must observe the worker panic");
        let msg = err
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| err.downcast_ref::<&'static str>().copied())
            .unwrap_or("");
        assert!(
            msg.contains("0x2008684"),
            "the worker's own payload must reach the driver, got {msg:?}",
        );

        // ...and it is not re-raised against the NEXT collection.
        pool.scope(2, &|_: usize| {}, || {});
    }

    /// A driver panic must not skip the barrier — the workers still hold a
    /// reference to the driver's closure environment.
    #[test]
    fn a_driver_panic_still_waits_for_the_workers() {
        let pool = EvacPool::new(2);
        let counter = AtomicUsize::new(0);
        let body = |_: usize| {
            std::thread::sleep(std::time::Duration::from_millis(5));
            counter.fetch_add(1, Ordering::SeqCst);
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            pool.scope(2, &body, || panic!("driver exploded"))
        }));
        assert!(outcome.is_err());
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "the barrier must hold even when the driver unwinds"
        );
    }

    /// gengc-round2-alloc2, 2026-09-20 — the pool GROWS, and a worker spawned
    /// into a live pool joins the next round cleanly.
    ///
    /// This is the mechanism half of
    /// `gengc-alloc-evac-pool-width-frozen-20260920.md`: a pool built at the
    /// first cycle's width used to run every later cycle at that width, so a
    /// young generation that grew into a wide evacuation never got one. The
    /// hazard the test is really pinning is the new worker's `last_generation`:
    /// seeded at 0 it would read the round already behind it as new work and
    /// `expect` a `job` the barrier has cleared, so the assertion that matters
    /// is that the grow is followed by a CORRECT full-width dispatch with every
    /// index running exactly once.
    #[test]
    fn a_pool_grows_on_demand_and_the_new_workers_join_the_next_round() {
        let pool = EvacPool::new(1);
        assert_eq!(pool.helpers(), 1);

        let seen: Vec<AtomicUsize> = (0..4).map(|_| AtomicUsize::new(0)).collect();
        let body = |i: usize| {
            seen[i].fetch_add(1, Ordering::Relaxed);
        };

        // Narrow to begin with: the clamp still holds while the pool is small.
        pool.scope(4, &body, || {});
        assert_eq!(seen[0].load(Ordering::Relaxed), 1);
        assert_eq!(seen[3].load(Ordering::Relaxed), 0, "no worker 3 exists yet");

        pool.ensure_helpers(4);
        assert_eq!(pool.helpers(), 4);
        // Idempotent, and never shrinks.
        pool.ensure_helpers(2);
        assert_eq!(pool.helpers(), 4);

        for s in &seen {
            s.store(0, Ordering::Relaxed);
        }
        pool.scope(4, &body, || {});
        for (i, s) in seen.iter().enumerate() {
            assert_eq!(
                s.load(Ordering::Relaxed),
                1,
                "after the grow, worker {i} must run exactly once"
            );
        }

        // ...and the widened pool is still correct across repeated dispatches,
        // which is where a lost or misdirected notification would show up.
        for _ in 0..20 {
            for s in &seen {
                s.store(0, Ordering::Relaxed);
            }
            pool.scope(4, &body, || {});
            for s in &seen {
                assert_eq!(s.load(Ordering::Relaxed), 1);
            }
        }
    }

    /// A grown pool must still shut down. Every worker has its own condvar
    /// now, so a `Drop` that notified only the last round's workers would hang
    /// in `join` — the exact failure this module exists to avoid.
    #[test]
    fn a_grown_pool_still_drops_cleanly_after_a_narrow_job() {
        let pool = EvacPool::new(1);
        pool.ensure_helpers(4);
        let body = |_: usize| {};
        // Deliberately narrow: workers 1..4 are left parked and were never
        // woken by this dispatch, so only `Drop`'s own loop can release them.
        pool.scope(1, &body, || {});
        drop(pool);
    }

    /// The census line must carry the field names a log grep needs, and the
    /// printed width must be the derived `built_with + grew_by` rather than a
    /// counter of its own.
    ///
    /// Shape only, no values: these statics are process-wide and every other
    /// test in this binary builds and dispatches pools concurrently, so an
    /// equality assertion on any of them would be flaky by construction — the
    /// same correction `tlab.rs`'s census tests took.
    #[test]
    fn the_pool_census_line_carries_the_field_names_a_log_grep_needs() {
        let line = super::evac_pool_census_line();
        for key in [
            "[GC] evac_pool:",
            "pool_built_with=",
            "pool_grew_by=",
            "pool_width=",
            "pool_dispatches=",
            "pool_inline_runs=",
            "pool_max_dispatched=",
            "pool_spawn_refused=",
        ] {
            assert!(line.contains(key), "census line lost `{key}`: {line}");
        }
        // Parsed back out of the SAME line rather than compared against a
        // second `evac_pool_census()` call: a peer test growing a pool between
        // the two calls would make that comparison fail for no reason.
        let field = |key: &str| -> usize {
            let rest = &line[line.find(key).expect("field present") + key.len()..];
            let end = rest.find(' ').unwrap_or(rest.len());
            rest[..end].parse().expect("a decimal field")
        };
        assert_eq!(
            field("pool_width="),
            field("pool_built_with=") + field("pool_grew_by="),
            "the printed width must be the derived sum, not a second counter: {line}"
        );
    }

    /// Growing a pool must move `grew_by`, and by the number of helpers that
    /// actually appeared. This is the ENGAGEMENT check for the census: a
    /// counter that cannot move is the failure the sibling gap pages were
    /// filed over.
    ///
    /// One-way assertions only, for the reason the test above gives.
    #[test]
    fn the_census_sees_a_pool_grow() {
        let (_, grew_before, _, _, _, _) = super::evac_pool_census();
        let pool = EvacPool::new(1);
        pool.ensure_helpers(3);
        assert_eq!(pool.helpers(), 3, "the pool itself must have grown");
        let (built_after, grew_after, _, _, _, _) = super::evac_pool_census();
        assert!(
            grew_after >= grew_before + 2,
            "ensure_helpers added two workers but the census did not see them: \
             {grew_before} -> {grew_after}"
        );
        assert!(
            built_after >= 1,
            "a constructed pool must be visible in `built_with`"
        );
    }

    /// An empty pool is the `CRATONVM_G1_WORKERS=1` determinism mode.
    #[test]
    fn an_empty_pool_runs_the_driver_inline() {
        let pool = EvacPool::new(0);
        let ran = AtomicUsize::new(0);
        let body = |_: usize| unreachable!("no helpers to dispatch to");
        pool.scope(4, &body, || {
            ran.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(ran.load(Ordering::Relaxed), 1);
    }
}
