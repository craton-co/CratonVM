//! Persistent worker pool for G1's stop-the-world evacuation.
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

use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::{Condvar, Mutex};

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
    shutdown: bool,
}

struct Inner {
    state: Mutex<State>,
    /// Signalled when a job is published, and on shutdown.
    wake: Condvar,
    /// Signalled when `pending` reaches zero.
    done: Condvar,
}

/// A fixed set of evacuation worker threads, reused across collections.
pub struct EvacPool {
    inner: Arc<Inner>,
    threads: Vec<JoinHandle<()>>,
    /// Serialises `scope`. See the module note on why nesting is not supported.
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
                shutdown: false,
            }),
            wake: Condvar::new(),
            done: Condvar::new(),
        });

        let mut threads = Vec::with_capacity(helpers);
        for id in 0..helpers {
            let inner = Arc::clone(&inner);
            let handle = std::thread::Builder::new()
                // Named so a stuck pause is identifiable in a native stack dump
                // without cross-referencing thread ids.
                .name(format!("g1-evac-{id}"))
                .spawn(move || worker_loop(&inner, id))
                // A pool thread that cannot be created is not a condition this
                // collector can degrade around mid-construction; the caller
                // sizes the pool once, at collector setup.
                .expect("g1: failed to spawn evacuation worker thread");
            threads.push(handle);
        }

        Self {
            inner,
            threads,
            dispatch: Mutex::new(()),
        }
    }

    /// Worker threads available besides the driver.
    pub fn helpers(&self) -> usize {
        self.threads.len()
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
    /// it. If any `body` panics, this panics after the barrier too. A panic on
    /// both sides reports the driver's, since it is the one carrying the
    /// collection context.
    pub fn scope<F>(&self, helpers: usize, body: &F, driver: impl FnOnce())
    where
        F: Fn(usize) + Sync,
    {
        let helpers = helpers.min(self.threads.len());
        if helpers == 0 {
            driver();
            return;
        }

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
            debug_assert!(st.job.is_none(), "evac pool dispatched while busy");
            st.job = Some(job);
            st.active = helpers;
            st.pending = helpers;
            st.panicked = false;
            st.generation = st.generation.wrapping_add(1);
            self.inner.wake.notify_all();
        }

        // Catch rather than propagate: helpers still hold `&F`, so the barrier
        // below is not optional even on the unwind path.
        let driver_result = catch_unwind(AssertUnwindSafe(driver));

        let worker_panicked = {
            let mut st = self.inner.state.lock();
            while st.pending > 0 {
                self.inner.done.wait(&mut st);
            }
            st.job = None;
            st.panicked
        };

        if let Err(payload) = driver_result {
            resume_unwind(payload);
        }
        if worker_panicked {
            panic!("g1 parallel-evac worker panicked");
        }
    }
}

impl Drop for EvacPool {
    fn drop(&mut self) {
        {
            let mut st = self.inner.state.lock();
            st.shutdown = true;
            self.inner.wake.notify_all();
        }
        for handle in self.threads.drain(..) {
            // A worker only exits its loop through the shutdown flag, and any
            // panic inside a job was caught and reported at the barrier, so a
            // join error here would mean the loop itself unwound — nothing is
            // recoverable at that point and there is no caller to tell.
            let _ = handle.join();
        }
    }
}

fn worker_loop(inner: &Inner, id: usize) {
    let mut last_generation = 0u64;
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
                    // Sized out of this round. `last_generation` was still
                    // advanced, so this worker does not later mistake the round
                    // for new work — and it must NOT touch `pending`, which
                    // counts only the workers the job asked for.
                    continue;
                }
                inner.wake.wait(&mut st);
            }
        };

        // SAFETY: the driver holds `&F` alive across the whole dispatch, and
        // does not return from `scope` until this worker's decrement below has
        // landed. See the module-level safety argument.
        let result = catch_unwind(AssertUnwindSafe(|| unsafe { (job.run)(job.ptr, id) }));

        let mut st = inner.state.lock();
        if result.is_err() {
            st.panicked = true;
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
