// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! t1 wave 4 lane G2 (2026-10-26) — the work-sharing drain behind the
//! stop-the-world old-gen mark's parallel arm (`CRATONVM_GC_OLD_PAR_MARK`,
//! opt-in; step 4 of
//! `docs/feature-designs/generative-gc-architecture.md`).
//!
//! `GenerationalHeap::old_gen_gc_inner`'s BFS hands this module its worklist
//! once the worklist has grown past [`PAR_MARK_MIN_WORKLIST`] entries (and
//! only outside the finalizer resurrection round, whose depth-2 set records
//! pops in order). [`drain`] runs the caller's per-object `scan` on `threads`
//! scoped workers until every worker is idle and the shared stack is empty:
//! the transitive closure of the seed under `scan`'s pushes.
//!
//! Shape (the young mark's `drain_parallel_on_with`, simplified): each worker
//! pops from a private LIFO stack and publishes surplus to one shared stack
//! under a mutex when its stack passes [`SPILL_HIGH`], or [`SPILL_WHEN_IDLE`]
//! while a peer is parked; an empty worker takes a fair share of the shared
//! stack or parks on the condvar. Termination: a worker that finds the shared
//! stack empty counts itself idle; when every LIVE worker is idle the closure
//! is complete (no worker holds private work, and only workers push).
//!
//! **Determinism.** The mark bit is claimed with one CAS
//! (`ObjectHeader::try_add_gc_flags` at the push site), so each object is
//! scanned exactly once whatever the interleaving, and the closure is
//! confluent: the final mark set is the set reachable from the seed, the
//! same set the serial BFS reaches. Only the ORDER of the per-worker outputs
//! (the young records, `pending_old`) varies, and every consumer of those is
//! order-independent (seeding is idempotent). Compaction destinations are
//! computed by address afterwards, so heap layout and Java-visible results do
//! not depend on the worker count.
//!
//! Per-worker state `S` is built by `make_state` on the worker's own thread
//! and returned in worker order (worker 0 is the calling thread).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// t1 G2 — `CRATONVM_GC_OLD_PAR_MARK` (**default ON** since t1 2026-10-27,
/// WARNING t1-user-flip-20261027; unset / `1` / `on` = `min(available_parallelism, 4)`
/// workers, `0` / `off` is the serial BFS, `n >= 2` =
/// `n`, at most [`PAR_MARK_MAX_WORKERS`]). Read once per generation
/// (`OldGen::new_on`), so it is per-VM.
pub const PAR_MARK_FLAG: &str = "CRATONVM_GC_OLD_PAR_MARK";

/// The most workers `CRATONVM_GC_OLD_PAR_MARK=<n>` may ask for.
pub const PAR_MARK_MAX_WORKERS: usize = 16;

/// The serial BFS keeps popping until its worklist holds this many entries;
/// only then are workers started. A chain-shaped graph (a linked list) never
/// gets there and pays nothing; a wide one gets there within a few pops of a
/// large array. A scoped spawn costs tens of microseconds, so a drain must
/// have thousands of objects behind it to pay.
pub const PAR_MARK_MIN_WORKLIST: usize = 256;

/// A worker publishes surplus once its private stack is this deep.
const SPILL_HIGH: usize = 1024;
/// ... or this deep while some peer is parked.
const SPILL_WHEN_IDLE: usize = 32;
/// Entries a spilling worker keeps (at most half its stack).
const SPILL_KEEP: usize = 64;
/// The most entries a worker takes from the shared stack at once.
const ACQUIRE_CHUNK: usize = 256;

/// A reference array longer than this is not scanned as one task: `scan`
/// reports its length and the drain publishes `[s, s + PAR_ARRAY_CHUNK)` slot
/// ranges as separate tasks (HotSpot's `ObjArrayChunkedTask`), so one huge
/// array (the probe's `Node[1000000]`) is marked by every worker, not one.
pub const PAR_ARRAY_CHUNK: usize = 8192;

/// t1 w7 G2 -- slots of a split array the chunk scanner prefetches ahead
/// (each target header is one likely miss; this many are in flight).
pub const CHUNK_PREFETCH_AHEAD: usize = 16;

/// t1 w7 G2 -- prefetch the cache line at `addr` for reading. A hint: never
/// faults (an unmapped or garbage address is a no-op), a no-op off x86-64.
#[inline(always)]
pub fn prefetch_read(addr: usize) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `_mm_prefetch` is a hint and never faults, whatever the address.
    unsafe {
        std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(addr as *const i8);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = addr;
}

/// t1 w7 G2 -- below this many walked old-gen objects the mark stays serial
/// whatever the switch asks (`OldGen::par_mark_min_objects`; tests lower it):
/// a small old generation's mark is over before workers pay for themselves.
pub const PAR_MARK_MIN_OBJECTS: usize = 65_536;

/// t1 w7 G2 -- walked objects per worker: a mark gets one worker per this many
/// (at least two once it is parallel at all), up to the switch's count.
pub const PAR_MARK_OBJECTS_PER_WORKER: usize = 262_144;

/// t1 w7 G2 -- the workers one mark actually starts: `0` (serial) below
/// `min_objects` walked objects, else `requested` capped at one worker per
/// [`PAR_MARK_OBJECTS_PER_WORKER`] (never below 2).
pub fn effective_workers(requested: usize, walked: usize, min_objects: usize) -> usize {
    if requested < 2 || walked < min_objects {
        return 0;
    }
    requested.min((walked / PAR_MARK_OBJECTS_PER_WORKER).max(2))
}

/// The worker count a `CRATONVM_GC_OLD_PAR_MARK` value asks for on a host with
/// `cpus` CPUs: `0` (serial) when off, `min(cpus, 4)` when unset.
pub fn par_mark_workers_from(value: Option<&str>, cpus: usize) -> usize {
    let Some(v) = value else {
        // WARNING t1-user-flip-20261027: default ON (unset = `min(cpus, 4)` workers), `=0` kills.
        return cpus.clamp(1, 4);
    };
    let v = v.trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "0" | "off" | "false" | "no" => 0,
        "1" | "on" | "true" | "yes" => cpus.clamp(1, 4),
        n => n
            .parse::<usize>()
            .map_or(0, |n| n.min(PAR_MARK_MAX_WORKERS)),
    }
}

/// [`par_mark_workers_from`] over the environment (once per generation).
pub fn par_mark_workers_from_env() -> usize {
    let raw = cratonvm_types::flags::runtime_var_os(PAR_MARK_FLAG)
        .map(|v| v.to_string_lossy().into_owned());
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    par_mark_workers_from(raw.as_deref(), cpus)
}

/// t1 w8 J1OG -- `CRATONVM_GC_OLD_PAR_CLOSE` (opt-in; same values as
/// [`PAR_MARK_FLAG`]): workers for the old-gen live-set closure's parallel
/// read-only verification (`OldGen::close_live_set_par_verify`; the in-mark
/// closures and the compactions' Phase 0). Read once per generation.
pub const PAR_CLOSE_FLAG: &str = "CRATONVM_GC_OLD_PAR_CLOSE";

/// [`PAR_CLOSE_FLAG`]'s worker count over the environment (once per
/// generation): `0` (serial) when unset or off.
pub fn par_close_workers_from_env() -> usize {
    let raw = cratonvm_types::flags::runtime_var_os(PAR_CLOSE_FLAG)
        .map(|v| v.to_string_lossy().into_owned());
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    par_mark_workers_from(raw.as_deref(), cpus)
}

/// Hands a `!Sync` borrow to the drain's workers.
///
/// # Safety (of the `Sync` impl)
/// The constructor's caller promises that every access the workers make
/// through [`AssertSync::get`] is a read of state nothing mutates for the
/// drain's duration, or an atomic. For the old mark: `OldGen::contains`,
/// `contains_range`, `in_extent` (plain fields and the commit bitmap, fixed
/// inside the pause), `note_mark_gap_fact` (an atomic), the walked grid and
/// start set (built before the mark, read-only), and the pause's side-table
/// snapshots. Workers never call an `OldGen` method that touches a `Cell` /
/// `RefCell` (the free-list caches, the block-offset table).
///
/// Use it through `get()` inside a closure: edition-2021 closures capture
/// disjoint FIELDS, so `x.0.field` would capture the `!Sync` field itself.
pub struct AssertSync<T>(T);

impl<T> AssertSync<T> {
    /// # Safety
    /// See the type's doc: only reads of pause-frozen state and atomics may go
    /// through [`Self::get`] while more than one thread holds it.
    pub unsafe fn new(t: T) -> Self {
        Self(t)
    }

    #[inline(always)]
    pub fn get(&self) -> &T {
        &self.0
    }
}

// SAFETY: see the type's doc; the constructor is `unsafe` and states it.
unsafe impl<T> Sync for AssertSync<T> {}

struct Shared {
    stack: Vec<usize>,
    /// Slot-range tasks `(array, start, end)` of split reference arrays.
    chunks: Vec<(usize, usize, usize)>,
    idle: usize,
    live: usize,
    done: bool,
}

/// Decrements `live` and wakes the peers when its worker leaves by ANY path
/// (an unwind out of `scan` included), so the survivors' `idle >= live` test
/// still terminates.
struct WorkerExit<'a> {
    shared: &'a Mutex<Shared>,
    cv: &'a Condvar,
}

impl Drop for WorkerExit<'_> {
    fn drop(&mut self) {
        let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        g.live = g.live.saturating_sub(1);
        if std::thread::panicking() {
            // Another worker's closure is now incomplete; stop everyone so the
            // scope joins and re-raises the panic.
            g.done = true;
        }
        drop(g);
        self.cv.notify_all();
    }
}

/// What [`drain`] did: `(objects scanned, scanned by the busiest worker)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainCounts {
    pub scanned: usize,
    pub max_worker: usize,
    /// t1 w7 G2 -- the CPU accounting below, filled only when [`drain`] is
    /// asked for diagnostics (`CRATONVM_DBG_GCPHASE`): the workers' summed
    /// thread CPU, the time they spent parked on the condvar, shared-state
    /// lock acquisitions, publishes (spills + array splits), and wake-ups.
    pub cpu_ns: u64,
    pub wait_ns: u64,
    pub locks: u64,
    pub publishes: u64,
    pub wakes: u64,
}

/// Initial chunk tasks over `n` unmarked candidates: `(0, start, end)` ranges
/// (a null "array"), which `scan_chunk` reads as indices into the caller's
/// candidate list.
pub fn candidate_ranges(n: usize) -> Vec<(usize, usize, usize)> {
    chunk_ranges(0, n).collect()
}

/// The slot ranges of a split array of `len` slots.
fn chunk_ranges(p: usize, len: usize) -> impl Iterator<Item = (usize, usize, usize)> {
    (0..len)
        .step_by(PAR_ARRAY_CHUNK)
        .map(move |s| (p, s, (s + PAR_ARRAY_CHUNK).min(len)))
}

/// Run `scan` over `seed` and everything it pushes, on `threads` workers (the
/// calling thread is worker 0). `scan(obj, stack, state)` pushes the objects it
/// newly claimed onto `stack` and returns `0`, or -- for a reference array it
/// chose not to scan whole -- the array's slot count, and the drain then runs
/// `scan_chunk(obj, start, end, stack, state)` over every
/// [`PAR_ARRAY_CHUNK`]-slot range of it (on any worker). Returns each worker's
/// state, in worker order, and the counts. `threads <= 1` drains on the
/// calling thread alone.
pub fn drain<S, M, F, C>(
    seed: Vec<*mut u8>,
    seed_chunks: Vec<(usize, usize, usize)>,
    threads: usize,
    diag: bool,
    make_state: M,
    scan: F,
    scan_chunk: C,
) -> (Vec<S>, DrainCounts)
where
    S: Send,
    M: Fn() -> S + Sync,
    F: Fn(*mut u8, &mut Vec<*mut u8>, &mut S) -> usize + Sync,
    C: Fn(*mut u8, usize, usize, &mut Vec<*mut u8>, &mut S) + Sync,
{
    if threads <= 1 {
        let mut st = make_state();
        let mut local = seed;
        let mut n = 0usize;
        for (a, s, e) in seed_chunks {
            scan_chunk(a as *mut u8, s, e, &mut local, &mut st);
        }
        while let Some(p) = local.pop() {
            let len = scan(p, &mut local, &mut st);
            if len > 0 {
                for (a, s, e) in chunk_ranges(p as usize, len) {
                    scan_chunk(a as *mut u8, s, e, &mut local, &mut st);
                }
            }
            n += 1;
        }
        return (
            vec![st],
            DrainCounts {
                scanned: n,
                max_worker: n,
                ..DrainCounts::default()
            },
        );
    }
    let shared = Mutex::new(Shared {
        stack: seed.into_iter().map(|p| p as usize).collect(),
        chunks: seed_chunks,
        idle: 0,
        live: threads,
        done: false,
    });
    let cv = Condvar::new();
    let idle_hint = AtomicUsize::new(0);
    let scanned_total = AtomicUsize::new(0);
    let scanned_max = AtomicUsize::new(0);
    let cpu_total = AtomicU64::new(0);
    let wait_total = AtomicU64::new(0);
    let locks_total = AtomicU64::new(0);
    let publishes_total = AtomicU64::new(0);
    let wakes_total = AtomicU64::new(0);

    let worker = || -> S {
        let cpu0 = if diag {
            cratonvm_gc_core::thread_cpu::current_thread_cpu_ns()
        } else {
            None
        };
        let (mut wait_ns, mut locks, mut publishes, mut wakes) = (0u64, 0u64, 0u64, 0u64);
        let _exit = WorkerExit {
            shared: &shared,
            cv: &cv,
        };
        let mut st = make_state();
        let mut local: Vec<*mut u8> = Vec::new();
        let mut chunk: Option<(usize, usize, usize)> = None;
        let mut scanned = 0usize;
        'outer: loop {
            {
                let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
                locks += 1;
                loop {
                    if g.done {
                        break 'outer;
                    }
                    if let Some(c) = g.chunks.pop() {
                        chunk = Some(c);
                        break;
                    }
                    let len = g.stack.len();
                    if len > 0 {
                        let share = len.div_ceil(g.live.max(1));
                        let take = share.clamp(1, ACQUIRE_CHUNK).min(len);
                        local.extend(g.stack.drain(len - take..).map(|a| a as *mut u8));
                        break;
                    }
                    g.idle += 1;
                    idle_hint.store(g.idle, Ordering::Relaxed);
                    if g.idle >= g.live {
                        g.done = true;
                        drop(g);
                        cv.notify_all();
                        break 'outer;
                    }
                    let t = diag.then(std::time::Instant::now);
                    g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
                    if let Some(t) = t {
                        wait_ns += t.elapsed().as_nanos() as u64;
                    }
                    wakes += 1;
                    g.idle -= 1;
                    idle_hint.store(g.idle, Ordering::Relaxed);
                }
            }
            if let Some((a, s, e)) = chunk.take() {
                scan_chunk(a as *mut u8, s, e, &mut local, &mut st);
            }
            while let Some(p) = local.pop() {
                let len = scan(p, &mut local, &mut st);
                scanned += 1;
                if len > 0 {
                    let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
                    g.chunks.extend(chunk_ranges(p as usize, len));
                    drop(g);
                    locks += 1;
                    publishes += 1;
                    cv.notify_all();
                }
                let n = local.len();
                if n >= SPILL_HIGH
                    || (n >= SPILL_WHEN_IDLE && idle_hint.load(Ordering::Relaxed) > 0)
                {
                    let keep = SPILL_KEEP.min(n / 2);
                    let surplus = n - keep;
                    let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
                    g.stack.extend(local.drain(..surplus).map(|a| a as usize));
                    drop(g);
                    locks += 1;
                    publishes += 1;
                    cv.notify_all();
                }
            }
        }
        scanned_total.fetch_add(scanned, Ordering::Relaxed);
        scanned_max.fetch_max(scanned, Ordering::Relaxed);
        if diag {
            if let (Some(a), Some(b)) = (cpu0, cratonvm_gc_core::thread_cpu::current_thread_cpu_ns()) {
                cpu_total.fetch_add(b.saturating_sub(a), Ordering::Relaxed);
            }
            wait_total.fetch_add(wait_ns, Ordering::Relaxed);
            locks_total.fetch_add(locks, Ordering::Relaxed);
            publishes_total.fetch_add(publishes, Ordering::Relaxed);
            wakes_total.fetch_add(wakes, Ordering::Relaxed);
        }
        st
    };

    let states = std::thread::scope(|s| {
        let handles: Vec<_> = (1..threads).map(|_| s.spawn(&worker)).collect();
        let mut out = Vec::with_capacity(threads);
        out.push(worker());
        for h in handles {
            match h.join() {
                Ok(st) => out.push(st),
                Err(e) => std::panic::resume_unwind(e),
            }
        }
        out
    });
    (
        states,
        DrainCounts {
            scanned: scanned_total.load(Ordering::Relaxed),
            max_worker: scanned_max.load(Ordering::Relaxed),
            cpu_ns: cpu_total.load(Ordering::Relaxed),
            wait_ns: wait_total.load(Ordering::Relaxed),
            locks: locks_total.load(Ordering::Relaxed),
            publishes: publishes_total.load(Ordering::Relaxed),
            wakes: wakes_total.load(Ordering::Relaxed),
        },
    )
}

#[cfg(test)]
mod tests;
