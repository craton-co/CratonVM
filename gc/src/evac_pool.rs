//! Persistent worker pool for stop-the-world evacuation.
//!
//! Two collectors drive it: G1's `parallel_evacuate` (which it was written
//! for, and whose threads are still named `g1-evac-N` because every crash
//! record in `docs/feature-designs/generative-gc-architecture.md` identifies a worker by that name) and the
//! GENERATIONAL young cycle's parallel evacuator, `gen_evac::ParEvac::drain`,
//! whose pool is built with [`EvacPool::with_name`] and names its threads
//! `gen-evac-N` (since gen r4w2, 2026-09-23; they were `g1-evac-N` too).
//! Neither owns it -- each holds its own `OnceLock<EvacPool>` -- so the width
//! of a pool is whatever its first collection asked for.
//!
//! (gci i2/st, 2026-10-12: "two collectors" undercounts the generational
//! heap's users of ITS pool. Besides `ParEvac::drain` it runs every young mark
//! drain and the span zeroing (`young_mark::run_phase_workers`), the chunked
//! young walks and the parallel card seed (`gen_heap::run_chunk_workers`), and
//! the moving cycle's pointer-map merge. All of them dispatch from the
//! collecting thread between dispatches, which is the contract below; none
//! dispatches from inside a body.)
//!
//! As of gengc-round2-alloc2 (2026-09-20) that is no longer PERMANENT:
//! [`EvacPool::ensure_helpers`] spawns the difference on demand, so an owner
//! whose worker policy widens later in the run can grow the pool to match. The
//! pool still cannot decide to grow on its own -- both callers clamp their
//! request to [`EvacPool::helpers`] *before* dispatching, so `scope` never
//! learns it was asked for more. The generational owner now asks
//! (gengc-round4-move, 2026-09-23): `collect_garbage_inner` calls
//! `ensure_helpers` with the POLICY width before every parallel copy phase, so
//! a pool first built narrower -- by another phase that borrowed it, or by a
//! future plan that narrows -- widens on the next copy. See
//! `docs/feature-designs/generative-gc-architecture.md`.
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

use cratonvm_types::latched_thread::{spawn_latched, LatchedJoinHandle};
use std::any::Any;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

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
    /// gcj pcp: the job was dispatched by [`EvacPool::scope_elastic`]: a
    /// helper that has not STARTED it when the driver closes the round
    /// (`closed`) sits it out, and the barrier does not wait for it.
    elastic: bool,
    /// gcj pcp: set by the driver of an elastic round once its own share
    /// returned; from then on no helper starts the job.
    closed: bool,
    /// gcj pcp: helpers that started the current elastic job before it closed.
    started: usize,
    // gen r4w6/young6: `pending` moved out of this struct to
    // `Inner::pending`, an atomic, so a finishing worker no longer takes this
    // lock. See that field.
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
    /// decrement `pending`. When this was written `ParEvac::plan` narrowed the
    /// width whenever to-space headroom could not buy one buffer per worker;
    /// it no longer does (a thin-slack cycle runs bufferless at full width),
    /// but a job narrower than the pool is still normal: the young-GC worker
    /// policy sizes each cycle, and the pool keeps its widest width. (gen
    /// r5w1/young5: this used to state the old `plan` behaviour as current.)
    /// See
    /// `docs/feature-designs/generative-gc-architecture.md`.
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
    /// Workers that have not yet finished the current job.
    ///
    /// gen r4w6/young6 (2026-09-24): an atomic OUTSIDE `state`, decremented by
    /// each worker with `fetch_sub(AcqRel)`; only the worker that takes it to
    /// zero locks `state`, and only to notify `done`. It used to be a field of
    /// `State`, so EVERY worker locked `state` to decrement it, and the
    /// evacuator's termination protocol ends a job with all workers idle at
    /// the same instant — an N-way convoy on one mutex at the tail of every
    /// parallel copy, with the driver waiting behind it. Item §5 of
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// The barrier argument is unchanged in substance: the driver waits under
    /// `state` until it reads `0` with `Acquire`, and every decrement is a
    /// `Release` RMW, so each worker's writes (its shard) happen-before the
    /// driver's return from `scope`. The last worker notifies UNDER `state`,
    /// and the driver's check-then-wait is atomic under the same lock, so the
    /// wake-up cannot be lost.
    pending: AtomicUsize,
    /// zt pl4 item 3: the CPU the helper threads spent in jobs, nanoseconds
    /// (`crate::thread_cpu::CpuSpan`; HotSpot's G1
    /// `sun.threads.cpu_time.gc_parallel_workers`). The driver's own share
    /// runs on the pause's thread and is not here.
    worker_cpu_ns: std::sync::atomic::AtomicU64,
}

// ---------------------------------------------------------------------------
// Pool-width census (gengc-round3-tlabreport, 2026-09-21)
// ---------------------------------------------------------------------------
//
// `gengc-alloc-evac-pool-width-frozen-FIXED-20260923.md` step (4): "`EvacPool::
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
// counter for `ensure_helpers`, whose production caller is the parallel copy
// driver in `gen_heap::collect_garbage_inner` (part (b), landed), so
// `grew_by=0` with a policy width above `built_with` would say that call is
// inert. (gen r5w1/young5: this used to say `ensure_helpers` had no
// production caller.)
//
// Per pool (g1gen/c6, the G1 / Generational half of `zgc-zt10rl3`): each
// pool keeps its own [`PoolCensus`], and each heap prints its own pool's
// ([`evac_pool_census_line`]). This was process-wide statics spanning every
// heap's pool, because neither owner was reachable from the printer; an
// embedder hosting two VMs (or the `vm` test binary) then read one line summed
// over all of them. Cost: one relaxed op per pool construction, per grow and
// per DISPATCH (once per parallel evacuation, not per region and not per
// object).
//
// The current width is printed, but it is deliberately NOT a counter of its
// own: it is rendered as the derived `built_with + grew_by`. A second counter
// tracking the same quantity is how two numbers in one summary come to
// disagree, which is the defect class this round kept finding.

/// The `[GC]` shutdown line for ONE heap's pool census (`None`: the heap never
/// built its pool -- both owners build it lazily, inside the first collection
/// that wants one), ready to print.
///
/// Formatted here rather than in `vm_heap.rs` for the reason
/// `tlab::tlab_census_lines` is: the fields and their meaning belong with the
/// mechanism, and the decision to print belongs with the summary. `pool_`
/// prefixes every key because `vm_heap.rs`'s
/// `every_gc_summary_key_is_unique_across_the_whole_summary` requires a name
/// no other summary line owns.
///
/// `built_with` is the helpers (threads besides the driver) the pool got at
/// construction; `grew_by` is helpers added by [`EvacPool::ensure_helpers`];
/// the pool's current width is their sum. `inline_runs` counts `scope` calls
/// that found no helpers at all and ran the driver alone. `pool_count` is 1
/// once the heap built its pool (gc-common w8-f added it when the line summed
/// every heap of the process; one heap owns one pool). g1gen/c6: byte for byte
/// the line the process statics printed for a process with one such heap.
pub fn evac_pool_census_line(census: Option<EvacPoolCensus>) -> String {
    let c = census.unwrap_or_default();
    format!(
        "[GC] evac_pool: pool_built_with={} pool_grew_by={} \
         pool_width={} pool_dispatches={} pool_inline_runs={} \
         pool_max_dispatched={} pool_spawn_refused={} \
         pool_count={} pool_elastic_skipped={}",
        c.built_with,
        c.grew_by,
        c.width(),
        c.dispatches,
        c.inline_runs,
        c.max_dispatched,
        c.spawn_refused,
        usize::from(census.is_some()),
        c.elastic_skipped,
    )
}

/// zt t1/rl (`zgc-zs3cr-relocation-review-residuals` item 4): ONE pool's own
/// census. Every pool keeps one, and since g1gen/c6 it is the only census:
/// the process-wide statics G1's and the generational heap's pools also fed
/// are gone, so no heap's pool folds into another heap's `[GC] evac_pool:`
/// line, and each owner prints what IT dispatched. Per pool, never
/// process-wide (AGENTS.md: no process globals).
#[derive(Default)]
struct PoolCensus {
    built_with: AtomicUsize,
    grew_by: AtomicUsize,
    spawn_refused: AtomicU64,
    dispatches: AtomicU64,
    inline_runs: AtomicU64,
    max_dispatched: AtomicUsize,
    dispatch_while_busy: AtomicU64,
    elastic_skipped: AtomicU64,
}

/// A snapshot of one pool's census ([`EvacPool::census`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EvacPoolCensus {
    /// Helpers the pool got at construction.
    pub built_with: usize,
    /// Helpers [`EvacPool::ensure_helpers`] added since.
    pub grew_by: usize,
    /// Worker threads the OS refused.
    pub spawn_refused: u64,
    /// `scope` calls that dispatched to at least one helper.
    pub dispatches: u64,
    /// `scope` calls that ran the driver alone.
    pub inline_runs: u64,
    /// The widest dispatch.
    pub max_dispatched: usize,
    /// Dispatches that found a job already published -- the fail-safe in
    /// [`EvacPool::scope`]. Expected to be ZERO, and structurally unreachable
    /// while `scope` holds the `dispatch` mutex for its whole extent. It is a
    /// counter rather than a `debug_assert!` because the site is inside a
    /// stop-the-world pause: a panic there leaves the driver on a barrier no
    /// helper will reach, which is a strictly worse outcome than the overlap
    /// it would be reporting.
    pub dispatch_while_busy: u64,
    /// gcj pcp: helpers an elastic dispatch ([`EvacPool::scope_elastic`])
    /// released unstarted -- on an oversubscribed host, the wake-ups the
    /// pause did not wait for.
    pub elastic_skipped: u64,
}

impl EvacPoolCensus {
    /// The pool's current width, derived (never a counter of its own).
    pub fn width(&self) -> usize {
        self.built_with + self.grew_by
    }
}

/// A fixed set of evacuation worker threads, reused across collections.
pub struct EvacPool {
    inner: Arc<Inner>,
    /// Join handles, in spawn order. Behind a `Mutex` only because
    /// [`Self::ensure_helpers`] appends to it through `&self`; `Drop` drains it
    /// through `get_mut` and takes no lock.
    threads: Mutex<Vec<LatchedJoinHandle<()>>>,
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
    /// Worker thread-name prefix: `<name>-<id>`. See [`Self::with_name`].
    name: &'static str,
    /// zt t1/rl: this pool's own census (see [`PoolCensus`]).
    census: PoolCensus,
}

impl EvacPool {
    /// [`Self::new`] with the worker threads named `<name>-<id>` instead of
    /// `g1-evac-<id>`.
    ///
    /// gen r4w2/youngpolicy (2026-09-23): the generational heap owns its own
    /// pool but its workers were named `g1-evac-N`, which sends anyone reading
    /// a stuck young pause's native stacks to the wrong collector. Behaviour
    /// is otherwise identical.
    pub fn with_name(helpers: usize, name: &'static str) -> Self {
        Self::build(helpers, name)
    }

    /// [`Self::with_name`] (zt t1/rl built ZGC's relocation pool this way when
    /// it was the one pool whose census was its own; since g1gen/c6 every
    /// pool's is, so the two constructors are the same).
    pub fn per_heap(helpers: usize, name: &'static str) -> Self {
        Self::build(helpers, name)
    }

    /// zt pl4 item 3: the CPU, nanoseconds, this pool's helper threads spent
    /// in jobs so far (monotone; 0 without a thread clock or under
    /// `CRATONVM_PERF_GC_THREAD_CPU=0`).
    pub fn worker_cpu_ns(&self) -> u64 {
        self.inner
            .worker_cpu_ns
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// This pool's own census. Exact for this pool whatever else the process
    /// runs.
    pub fn census(&self) -> EvacPoolCensus {
        let c = &self.census;
        EvacPoolCensus {
            built_with: c.built_with.load(Ordering::Relaxed),
            grew_by: c.grew_by.load(Ordering::Relaxed),
            spawn_refused: c.spawn_refused.load(Ordering::Relaxed),
            dispatches: c.dispatches.load(Ordering::Relaxed),
            inline_runs: c.inline_runs.load(Ordering::Relaxed),
            max_dispatched: c.max_dispatched.load(Ordering::Relaxed),
            dispatch_while_busy: c.dispatch_while_busy.load(Ordering::Relaxed),
            elastic_skipped: c.elastic_skipped.load(Ordering::Relaxed),
        }
    }

    /// Create a pool of `helpers` worker threads.
    ///
    /// `helpers` is the number of threads *besides* the driver, so a
    /// four-worker evacuation wants `EvacPool::new(3)`. `new(0)` is legal and
    /// yields a pool that always runs the driver's share inline.
    ///
    /// A thread the OS refuses NARROWS the pool rather than panicking
    /// (gengc-round4-move, 2026-09-23). Both owners build their pool lazily,
    /// from inside the first collection that wants one -- i.e. inside a
    /// stop-the-world pause with every mutator parked -- so the `assert!` this
    /// replaces turned a refused thread into a panic mid-pause. `scope` already
    /// clamps to [`Self::helpers`], so a narrower pool is a slower pause, not
    /// an incorrect one; the refusal is counted in `pool_spawn_refused`.
    pub fn new(helpers: usize) -> Self {
        Self::build(helpers, "g1-evac")
    }

    fn build(helpers: usize, name: &'static str) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                generation: 0,
                job: None,
                active: 0,
                elastic: false,
                closed: false,
                started: 0,
                panicked: false,
                panic_payload: None,
                shutdown: false,
                wake: Vec::with_capacity(helpers),
            }),
            done: Condvar::new(),
            pending: AtomicUsize::new(0),
            worker_cpu_ns: std::sync::atomic::AtomicU64::new(0),
        });

        let pool = Self {
            inner,
            threads: Mutex::new(Vec::with_capacity(helpers)),
            spawned: AtomicUsize::new(0),
            dispatch: Mutex::new(()),
            name,
            census: PoolCensus::default(),
        };
        for _ in 0..helpers {
            // Degrade, exactly as `ensure_helpers` does: see the doc above for
            // why a panic here would be raised inside a pause. `spawn_one`
            // counts the refusal.
            if !pool.spawn_one() {
                tracing::warn!(
                    requested = helpers,
                    spawned = pool.helpers(),
                    "evac_pool: the OS refused an evacuation worker thread; \
                     running with a narrower pool",
                );
                break;
            }
        }
        // The width this pool is frozen at until something calls
        // `ensure_helpers` -- the width it actually GOT, so a refusal cannot
        // make `built_with + grew_by` claim workers that do not exist.
        pool.census
            .built_with
            .store(pool.helpers(), Ordering::Relaxed);
        pool
    }

    /// Spawn one more worker. `false` if the OS refused the thread.
    ///
    /// Takes `threads` then `state`, which is the only order any caller uses.
    /// A worker takes `state` alone, so it cannot participate in a cycle.
    ///
    /// Phase 5 fast boot (WP3): also `false` in a boot image's capture run,
    /// which must reach its snapshot point with no OS thread of its own. The
    /// pool then stays as narrow as it is (the driver alone if nothing had
    /// spawned), the same fallback as an OS refusal; the first
    /// `ensure_helpers` after the restore widens it.
    fn spawn_one(&self) -> bool {
        if cratonvm_types::image_layout::capturing() {
            return false;
        }
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
        // Latched (`cratonvm_types::latched_thread`): `Drop` can run in a
        // thread-local destructor, where an OS join deadlocks on Windows.
        match spawn_latched(
            // Named so a stuck pause is identifiable in a native stack dump
            // without cross-referencing thread ids.
            std::thread::Builder::new().name(format!("{}-{id}", self.name)),
            move || worker_loop(&inner, id, &worker_cv, start_generation),
        ) {
            Ok(handle) => {
                threads.push(handle);
                st.wake.push(cv);
                // Published AFTER the condvar is in `wake`, so a `scope` that
                // reads the new width finds a slot to notify.
                self.spawned.store(st.wake.len(), Ordering::Release);
                true
            }
            Err(_) => {
                // Not fatal from `ensure_helpers` or from `new` (both narrow;
                // this comment used to say `new` asserts, which it stopped
                // doing in round 4) -- but a refusal silently narrows a pool
                // that a policy asked to widen, so it is counted rather than
                // swallowed.
                self.census.spawn_refused.fetch_add(1, Ordering::Relaxed);
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
    /// See `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// (gengc-round4-move, 2026-09-23: the second reason no longer holds --
    /// `ParEvac::plan` stopped narrowing when thin-slack cycles learned to run
    /// bufferless, and the policy is binary, `1` or the auto width -- so the
    /// generational pool was in practice built at its final width. The
    /// generational call site now calls this with the policy width on every
    /// parallel copy anyway, which is what keeps that true when another phase
    /// borrows the pool first.)
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
        let added = self.helpers().saturating_sub(before);
        self.census.grew_by.fetch_add(added, Ordering::Relaxed);
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
        self.scope_with(helpers, body, driver, false);
    }

    /// gcj pcp: [`Self::scope`] for a job whose work is CLAIMED, not assigned:
    /// `body(i)` runs for the helpers that start before `driver()` returns,
    /// and for no other `i`; the barrier waits only for those. So the job's
    /// work must be shared out by claiming (an atomic cursor, a shared stack),
    /// such that `driver()` alone completes whatever no helper took, and
    /// nothing may rely on any particular helper index running.
    ///
    /// Why: on a host with more runnable threads than cores a woken helper can
    /// wait a scheduler slice or more before it first runs, and a plain
    /// [`Self::scope`] waits for the last of them even when the driver and the
    /// others have already done all the work. A young pause dispatches several
    /// times; on JIT-warm 4-thread javac at load 30-50 its GC averaged ~1.6
    /// cores over a pause with eight workers. A helper that is skipped finds
    /// the round closed when it wakes and goes back to sleep.
    pub fn scope_elastic<F>(&self, helpers: usize, body: &F, driver: impl FnOnce())
    where
        F: Fn(usize) + Sync,
    {
        self.scope_with(helpers, body, driver, true);
    }

    fn scope_with<F>(&self, helpers: usize, body: &F, driver: impl FnOnce(), elastic: bool)
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
            self.census.inline_runs.fetch_add(1, Ordering::Relaxed);
            driver();
            return;
        }
        // One relaxed pair per parallel evacuation. `max_dispatched` is what
        // the pool ACTUALLY ran at its widest; beside `built_with` and
        // `grew_by` it is how a log says the pool was pinned at the first
        // cycle's width. See the census note above `EvacPool`.
        self.census.dispatches.fetch_add(1, Ordering::Relaxed);
        self.census
            .max_dispatched
            .fetch_max(helpers, Ordering::Relaxed);

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

        // gen r4w6/young6: the condvars to notify, collected under `state` and
        // notified AFTER it is released. See the note at the notify loop.
        let to_wake: Vec<Arc<Condvar>>;
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
            // It is NOT benign if it ever happens (gen r5w1/young5: this
            // comment used to call it "the benign arm"): a job still in flight
            // has workers that have not yet decremented `pending`, so the
            // `pending.store` below would race their `fetch_sub`s — a wrap (a
            // hang) or an early barrier (a use of the old `&F` after its
            // `scope` returned). What makes it unreachable is the `dispatch`
            // mutex held across the whole `scope`; the counter is how a change
            // to that would show up.
            if st.job.is_some() {
                self.census
                    .dispatch_while_busy
                    .fetch_add(1, Ordering::Relaxed);
            }
            st.job = Some(job);
            st.active = helpers;
            st.elastic = elastic;
            st.closed = false;
            st.started = 0;
            // Stored under `state`: a worker reads the job only after locking
            // `state`, so it cannot decrement before this store is visible.
            self.inner.pending.store(helpers, Ordering::Relaxed);
            st.panicked = false;
            // The previous job's payload, if it was never consumed, dies here
            // rather than being re-raised against an unrelated collection.
            st.panic_payload = None;
            st.generation = st.generation.wrapping_add(1);
            // EXACTLY the workers this job asked for. `helpers` was clamped to
            // the width above and the vector only grows, so the slice is always
            // in bounds. See `State::wake` for why the sized-out workers are
            // left alone rather than woken to discover it for themselves.
            //
            // gen r5w1/young5: no `debug_assert!` here any more. It ran AFTER
            // the job and generation were published, which is the pattern the
            // counter note above argues against (a panic would unwind with a
            // published job whose `&F` dies); `take(helpers)` is in bounds by
            // the clamp, and never panics even if it were not.
            to_wake = st.wake.iter().take(helpers).cloned().collect();
        }
        // NOTIFY OUTSIDE THE LOCK (gen r4w6/young6, 2026-09-24). This loop used
        // to run while holding `state`. `parking_lot`'s `Condvar::notify_one`
        // does not wake a thread whose mutex is held — it REQUEUES it onto that
        // mutex — so every worker was moved onto `state`'s wait queue, and a
        // mutex unlock wakes exactly one waiter. The pool therefore started as
        // a SERIAL CHAIN: worker k began only after workers 0..k-1 had each been
        // woken, taken `state`, read the job and released it — N sequential
        // futex round trips before the last worker ran, on every dispatch.
        // Unlocked, each notify unparks its worker directly and they start in
        // parallel (then take `state` briefly, contended but spinning).
        //
        // Sound: the generation bump happened under `state`, and a worker only
        // parks after checking the generation under `state` (the check and
        // `wait` are atomic w.r.t. that lock), so a worker either saw the new
        // generation before parking or is parked when notified. `dispatch` is
        // held, so `wake` cannot grow under the snapshot.
        for cv in &to_wake {
            cv.notify_one();
        }
        drop(to_wake);

        // Catch rather than propagate: helpers still hold `&F`, so the barrier
        // below is not optional even on the unwind path.
        let driver_result = catch_unwind(AssertUnwindSafe(driver));

        let worker_panic = {
            let mut st = self.inner.state.lock();
            if st.elastic {
                // gcj pcp: close the round. A helper starts only under `state`
                // and only while the round is open, so `started` is final now,
                // and the helpers that never started will never decrement
                // `pending`: take them off it here.
                st.closed = true;
                let unstarted = st.active.saturating_sub(st.started);
                if unstarted > 0 {
                    self.census
                        .elastic_skipped
                        .fetch_add(unstarted as u64, Ordering::Relaxed);
                    self.inner.pending.fetch_sub(unstarted, Ordering::AcqRel);
                }
            }
            // `Acquire`: pairs with every worker's `Release` decrement, so the
            // shards they wrote are visible once this reads 0. Checked under
            // `state`, which the last worker takes to notify — no lost wake-up.
            while self.inner.pending.load(Ordering::Acquire) > 0 {
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
                        // gcj pcp: an elastic round its driver already closed
                        // is not this worker's to start (and `pending` no
                        // longer counts it); otherwise it is now a starter.
                        if st.elastic {
                            if st.closed {
                                continue;
                            }
                            st.started += 1;
                        }
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
        let cpu = crate::thread_cpu::CpuSpan::begin();
        let result = catch_unwind(AssertUnwindSafe(|| unsafe { (job.run)(job.ptr, id) }));
        cpu.end(&inner.worker_cpu_ns);

        // The payload is recorded under `state` BEFORE the decrement, so the
        // driver, which reads it only after seeing `pending == 0`, finds it.
        if let Err(payload) = result {
            let mut st = inner.state.lock();
            st.panicked = true;
            // First panicking worker only -- see `State::panic_payload`.
            if st.panic_payload.is_none() {
                st.panic_payload = Some(payload);
            }
        }
        // gen r4w6/young6: lock-free unless this is the LAST worker. See
        // `Inner::pending` for the ordering argument. `AcqRel` rather than
        // `Release` so the last decrement also acquires its peers' releases
        // (it is the one that wakes the driver).
        if inner.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _st = inner.state.lock();
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

    /// gcj pcp: an elastic round returns once the driver's share and every
    /// helper that STARTED have finished; a helper that has not started by
    /// then is released (counted) and never runs that round's body, and the
    /// pool keeps serving later rounds, plain and elastic, exactly.
    #[test]
    fn gcj_pcp_an_elastic_round_does_not_wait_for_a_helper_that_never_started() {
        let pool = EvacPool::new(3);
        // A driver that returns at once: each helper either started before
        // the close (and ran once) or was released (and never runs it).
        let ran = AtomicUsize::new(0);
        let body = |_: usize| {
            ran.fetch_add(1, Ordering::Relaxed);
        };
        for round in 1..=200usize {
            pool.scope_elastic(3, &body, || {});
            let total = ran.load(Ordering::Relaxed) + pool.census().elastic_skipped as usize;
            assert_eq!(total, 3 * round, "round {round}: a helper ran twice or was lost");
        }
        // The pool still serves a plain round on every helper, and an elastic
        // one whose driver waits until all three have started.
        let seen: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();
        let body = |i: usize| {
            seen[i].fetch_add(1, Ordering::Relaxed);
        };
        pool.scope(3, &body, || {});
        assert!(seen.iter().all(|s| s.load(Ordering::Relaxed) == 1));
        let before = pool.census().elastic_skipped;
        let arrived = AtomicUsize::new(0);
        let release = std::sync::atomic::AtomicBool::new(false);
        let body = |_: usize| {
            arrived.fetch_add(1, Ordering::Relaxed);
            while !release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        };
        // The driver returns only once all three helpers started; they are
        // still running then, so the close must wait for them, not skip them.
        pool.scope_elastic(3, &body, || {
            while arrived.load(Ordering::Relaxed) < 3 {
                std::thread::yield_now();
            }
            release.store(true, Ordering::Release);
        });
        assert_eq!(arrived.load(Ordering::Relaxed), 3);
        assert_eq!(pool.census().elastic_skipped, before, "nobody skipped: all started");
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
    /// `gengc-alloc-evac-pool-width-frozen-FIXED-20260923.md`: a pool built at the
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
    /// g1gen/c6: the census is the pool's own, so the line is pinned exactly:
    /// byte for byte the line the process statics printed for a process with
    /// one heap (field order, spelling, `pool_count`), and the `None` form a
    /// heap that never built its pool prints.
    #[test]
    fn the_pool_census_line_carries_the_field_names_a_log_grep_needs() {
        let pool = EvacPool::with_name(2, "gen-evac");
        pool.ensure_helpers(3);
        let body = |_: usize| {};
        pool.scope(3, &body, || {});
        pool.scope(1, &body, || {});
        let line = super::evac_pool_census_line(Some(pool.census()));
        assert_eq!(
            line,
            "[GC] evac_pool: pool_built_with=2 pool_grew_by=1 pool_width=3 \
             pool_dispatches=2 pool_inline_runs=0 pool_max_dispatched=3 \
             pool_spawn_refused=0 pool_count=1 pool_elastic_skipped=0"
        );
        assert_eq!(
            super::evac_pool_census_line(None),
            "[GC] evac_pool: pool_built_with=0 pool_grew_by=0 pool_width=0 \
             pool_dispatches=0 pool_inline_runs=0 pool_max_dispatched=0 \
             pool_spawn_refused=0 pool_count=0 pool_elastic_skipped=0"
        );
    }

    /// Growing a pool must move `grew_by`, and by the number of helpers that
    /// actually appeared. This is the ENGAGEMENT check for the census: a
    /// counter that cannot move is the failure the sibling gap pages were
    /// filed over. Exact, since the census is this pool's alone.
    #[test]
    fn the_census_sees_a_pool_grow() {
        let pool = EvacPool::new(1);
        assert_eq!(pool.census().grew_by, 0);
        pool.ensure_helpers(3);
        assert_eq!(pool.helpers(), 3, "the pool itself must have grown");
        let c = pool.census();
        assert_eq!((c.built_with, c.grew_by, c.width()), (1, 2, 3));
    }

    /// g1gen/c6 (`zgc-zt10rl3`, carried to G1 / Generational): two pools of
    /// one process -- G1's `g1-evac` and the generational heap's `gen-evac`,
    /// as two VMs of an embedding would own -- each count only their own
    /// jobs. With the process statics, each line was the sum (and the widest
    /// `built_with`) of both.
    #[test]
    fn g1gen_c6_two_heaps_pools_keep_separate_censuses() {
        let g1 = EvacPool::new(3);
        let gen = EvacPool::with_name(1, "gen-evac");
        let body = |_: usize| {};
        for _ in 0..5 {
            g1.scope(3, &body, || {});
        }
        gen.scope(1, &body, || {});
        gen.ensure_helpers(2);
        let (a, b) = (g1.census(), gen.census());
        assert_eq!(
            (a.built_with, a.grew_by, a.dispatches, a.max_dispatched),
            (3, 0, 5, 3),
            "G1's pool"
        );
        assert_eq!(
            (b.built_with, b.grew_by, b.dispatches, b.max_dispatched),
            (1, 1, 1, 1),
            "the generational pool"
        );
        let la = super::evac_pool_census_line(Some(a));
        let lb = super::evac_pool_census_line(Some(b));
        assert!(la.contains("pool_dispatches=5 ") && la.contains("pool_count=1"), "{la}");
        assert!(lb.contains("pool_dispatches=1 ") && lb.contains("pool_count=1"), "{lb}");
    }

    /// gen r4w6/young6: the lock-free `pending` decrement and the notify
    /// outside `state`, under load. Thousands of back-to-back dispatches at
    /// varying widths, each worker writing only its own slot with `Relaxed`,
    /// and the driver reading every slot the instant `scope` returns: a
    /// missing happens-before edge (the `Release` decrement / `Acquire` wait),
    /// a lost wake-up (the unlocked notify) or a worker that ran twice or not
    /// at all fails an assertion or hangs here.
    #[test]
    fn back_to_back_dispatches_keep_the_barrier_with_the_atomic_pending_count() {
        const W: usize = 8;
        let pool = EvacPool::new(W);
        let slots: Vec<AtomicUsize> = (0..W).map(|_| AtomicUsize::new(0)).collect();
        let runs: Vec<AtomicUsize> = (0..W).map(|_| AtomicUsize::new(0)).collect();
        let mut expected_runs = [0usize; W];
        for round in 1..=3000usize {
            // Widths 1..=W, so narrow rounds (sized-out workers left parked)
            // interleave with full ones.
            let width = 1 + round % W;
            let body = |i: usize| {
                slots[i].store(round, Ordering::Relaxed);
                runs[i].fetch_add(1, Ordering::Relaxed);
            };
            pool.scope(width, &body, || {});
            for (i, s) in slots.iter().enumerate().take(width) {
                assert_eq!(
                    s.load(Ordering::Relaxed),
                    round,
                    "worker {i}'s write of round {round} was not visible after the barrier"
                );
                expected_runs[i] += 1;
            }
        }
        for (i, r) in runs.iter().enumerate() {
            assert_eq!(
                r.load(Ordering::Relaxed),
                expected_runs[i],
                "worker {i} ran the wrong number of times"
            );
        }
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

    /// zt t1/rl: a `per_heap` pool's census is exact for that pool (built,
    /// grown, dispatched, inline), and the pool still names its workers.
    /// Exact equalities are safe here: nothing else in the binary touches
    /// THIS pool's census, unlike the process statics.
    #[test]
    fn zt1_rl_a_per_heap_pool_counts_its_own_census() {
        let pool = EvacPool::per_heap(1, "zgc-reloc");
        assert_eq!(pool.census().built_with, 1);
        pool.ensure_helpers(3);
        let ran = AtomicUsize::new(0);
        let body = |_: usize| {
            ran.fetch_add(1, Ordering::Relaxed);
        };
        pool.scope(2, &body, || {});
        pool.scope(3, &body, || {});
        assert_eq!(ran.load(Ordering::Relaxed), 5);
        let c = pool.census();
        assert_eq!(
            c,
            EvacPoolCensus {
                built_with: 1,
                grew_by: 2,
                spawn_refused: 0,
                dispatches: 2,
                inline_runs: 0,
                max_dispatched: 3,
                dispatch_while_busy: 0,
                elastic_skipped: 0,
            }
        );
        assert_eq!(c.width(), 3);
        let empty = EvacPool::per_heap(0, "zgc-reloc");
        empty.scope(4, &body, || {});
        assert_eq!(empty.census().inline_runs, 1);
        assert_eq!(empty.census().dispatches, 0);
    }

    /// zt t1/rl: the classic constructors keep their own census too (since
    /// g1gen/c6 the only one).
    #[test]
    fn zt1_rl_a_classic_pool_also_keeps_its_own_census() {
        let pool = EvacPool::with_name(2, "gen-evac");
        let body = |_: usize| {};
        pool.scope(2, &body, || {});
        let c = pool.census();
        assert_eq!((c.built_with, c.dispatches, c.max_dispatched), (2, 1, 2));
    }

    /// gen r4w2/youngpolicy: `with_name` names the workers, and `new` keeps
    /// G1's historical `g1-evac-N` names that crash records are keyed on.
    #[test]
    fn a_named_pool_names_its_workers_and_new_keeps_the_g1_name() {
        let names = |pool: &EvacPool| {
            let seen: Mutex<Vec<String>> = Mutex::new(Vec::new());
            let body = |_: usize| {
                seen.lock()
                    .push(std::thread::current().name().unwrap_or("").to_string());
            };
            pool.scope(2, &body, || {});
            let mut v = seen.into_inner();
            v.sort();
            v
        };
        assert_eq!(
            names(&EvacPool::with_name(2, "gen-evac")),
            vec!["gen-evac-0".to_string(), "gen-evac-1".to_string()],
        );
        assert_eq!(
            names(&EvacPool::new(2)),
            vec!["g1-evac-0".to_string(), "g1-evac-1".to_string()],
        );
    }

    /// gci i2/st: run `f` on its own thread and FAIL when it has not returned
    /// within `secs`, so a lost wake-up is a test failure, not a hung binary.
    fn st2_within<F: FnOnce() + Send + 'static>(secs: u64, what: &str, f: F) {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
            Ok(()) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("{what}: not done within {secs} s (lost wake-up or unreachable barrier)")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("{what}: panicked")
            }
        }
    }

    /// gci i2/st -- a helper that JOINS LATE: `ensure_helpers` spawns the
    /// threads and the very next call dispatches to them, usually before a new
    /// thread has run its first instruction, let alone parked on its condvar.
    /// The dispatch's `notify_one` then reaches nobody; the worker must find
    /// the round through the generation it reads on its first lock, or the
    /// barrier never falls. Fresh pools, so the race is re-run every time.
    #[test]
    fn st2_a_worker_spawned_just_before_a_dispatch_still_runs_it() {
        st2_within(120, "spawn-then-dispatch", || {
            for round in 0..100usize {
                let pool = EvacPool::new(0);
                let width = 1 + round % 4;
                pool.ensure_helpers(width);
                let seen: Vec<AtomicUsize> = (0..width).map(|_| AtomicUsize::new(0)).collect();
                let body = |i: usize| {
                    seen[i].fetch_add(1, Ordering::Relaxed);
                };
                pool.scope(width, &body, || {});
                for (i, s) in seen.iter().enumerate() {
                    assert_eq!(s.load(Ordering::Relaxed), 1, "round {round} worker {i}");
                }
            }
        });
    }

    /// gci i2/st -- helpers that reach the job AFTER the driver's own share
    /// returned (the driver's share is empty, each helper yields or sleeps a
    /// varying few tens of microseconds first), across widths that shrink and
    /// grow from one round to the next so a worker sized out of one round is
    /// woken for the next. Every dispatched helper must run exactly once per
    /// round, and `scope` must not return before the last of them.
    #[test]
    fn st2_helpers_that_reach_the_job_after_the_driver_finished_hold_the_barrier() {
        st2_within(120, "late-helper barrier", || {
            let pool = EvacPool::new(3);
            let done = AtomicUsize::new(0);
            for round in 0..600usize {
                let width = 1 + round % 3;
                let body = |i: usize| {
                    match (round + i) % 3 {
                        0 => std::thread::yield_now(),
                        1 => std::thread::sleep(std::time::Duration::from_micros(
                            ((round * 7 + i) % 5) as u64 * 20,
                        )),
                        _ => {}
                    }
                    done.fetch_add(1, Ordering::SeqCst);
                };
                let before = done.load(Ordering::SeqCst);
                pool.scope(width, &body, || {});
                assert_eq!(
                    done.load(Ordering::SeqCst),
                    before + width,
                    "round {round}: scope returned before every helper finished"
                );
            }
        });
    }
}
