// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! t1 wave 4 lane G2 (2026-10-26) — the work-sharing drain behind the
//! stop-the-world old-gen mark's parallel arm (`CRATONVM_GC_OLD_PAR_MARK`,
//! opt-in; step 4 of
//! `docs/internal/retired/gcj-j1og-proposal-parallel-full-gc-adjust-and-mark-20261019-RETIRED-20261027.md`).
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
pub(crate) const PAR_MARK_FLAG: &str = "CRATONVM_GC_OLD_PAR_MARK";

/// The most workers `CRATONVM_GC_OLD_PAR_MARK=<n>` may ask for.
pub(crate) const PAR_MARK_MAX_WORKERS: usize = 16;

/// The serial BFS keeps popping until its worklist holds this many entries;
/// only then are workers started. A chain-shaped graph (a linked list) never
/// gets there and pays nothing; a wide one gets there within a few pops of a
/// large array. A scoped spawn costs tens of microseconds, so a drain must
/// have thousands of objects behind it to pay.
pub(crate) const PAR_MARK_MIN_WORKLIST: usize = 256;

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
pub(crate) const PAR_ARRAY_CHUNK: usize = 8192;

/// t1 w7 G2 -- slots of a split array the chunk scanner prefetches ahead
/// (each target header is one likely miss; this many are in flight).
pub(crate) const CHUNK_PREFETCH_AHEAD: usize = 16;

/// t1 w7 G2 -- prefetch the cache line at `addr` for reading. A hint: never
/// faults (an unmapped or garbage address is a no-op), a no-op off x86-64.
#[inline(always)]
pub(crate) fn prefetch_read(addr: usize) {
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
pub(crate) const PAR_MARK_MIN_OBJECTS: usize = 65_536;

/// t1 w7 G2 -- walked objects per worker: a mark gets one worker per this many
/// (at least two once it is parallel at all), up to the switch's count.
pub(crate) const PAR_MARK_OBJECTS_PER_WORKER: usize = 262_144;

/// t1 w7 G2 -- the workers one mark actually starts: `0` (serial) below
/// `min_objects` walked objects, else `requested` capped at one worker per
/// [`PAR_MARK_OBJECTS_PER_WORKER`] (never below 2).
pub(crate) fn effective_workers(requested: usize, walked: usize, min_objects: usize) -> usize {
    if requested < 2 || walked < min_objects {
        return 0;
    }
    requested.min((walked / PAR_MARK_OBJECTS_PER_WORKER).max(2))
}

/// The worker count a `CRATONVM_GC_OLD_PAR_MARK` value asks for on a host with
/// `cpus` CPUs: `0` (serial) when off, `min(cpus, 4)` when unset.
pub(crate) fn par_mark_workers_from(value: Option<&str>, cpus: usize) -> usize {
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
pub(crate) fn par_mark_workers_from_env() -> usize {
    let raw = cratonvm_types::flags::runtime_var_os(PAR_MARK_FLAG)
        .map(|v| v.to_string_lossy().into_owned());
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    par_mark_workers_from(raw.as_deref(), cpus)
}

/// t1 w8 J1OG -- `CRATONVM_GC_OLD_PAR_CLOSE` (opt-in; same values as
/// [`PAR_MARK_FLAG`]): workers for the old-gen live-set closure's parallel
/// read-only verification (`OldGen::close_live_set_par_verify`; the in-mark
/// closures and the compactions' Phase 0). Read once per generation.
pub(crate) const PAR_CLOSE_FLAG: &str = "CRATONVM_GC_OLD_PAR_CLOSE";

/// [`PAR_CLOSE_FLAG`]'s worker count over the environment (once per
/// generation): `0` (serial) when unset or off.
pub(crate) fn par_close_workers_from_env() -> usize {
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
pub(crate) struct AssertSync<T>(T);

impl<T> AssertSync<T> {
    /// # Safety
    /// See the type's doc: only reads of pause-frozen state and atomics may go
    /// through [`Self::get`] while more than one thread holds it.
    pub(crate) unsafe fn new(t: T) -> Self {
        Self(t)
    }

    #[inline(always)]
    pub(crate) fn get(&self) -> &T {
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
pub(crate) struct DrainCounts {
    pub(crate) scanned: usize,
    pub(crate) max_worker: usize,
    /// t1 w7 G2 -- the CPU accounting below, filled only when [`drain`] is
    /// asked for diagnostics (`CRATONVM_DBG_GCPHASE`): the workers' summed
    /// thread CPU, the time they spent parked on the condvar, shared-state
    /// lock acquisitions, publishes (spills + array splits), and wake-ups.
    pub(crate) cpu_ns: u64,
    pub(crate) wait_ns: u64,
    pub(crate) locks: u64,
    pub(crate) publishes: u64,
    pub(crate) wakes: u64,
}

/// Initial chunk tasks over `n` unmarked candidates: `(0, start, end)` ranges
/// (a null "array"), which `scan_chunk` reads as indices into the caller's
/// candidate list.
pub(crate) fn candidate_ranges(n: usize) -> Vec<(usize, usize, usize)> {
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
pub(crate) fn drain<S, M, F, C>(
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
            crate::thread_cpu::current_thread_cpu_ns()
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
            if let (Some(a), Some(b)) = (cpu0, crate::thread_cpu::current_thread_cpu_ns()) {
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
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU8;

    #[test]
    fn t1_w7_g2_effective_workers_scale_with_the_walked_old_gen() {
        assert_eq!(effective_workers(0, 10_000_000, 0), 0);
        assert_eq!(effective_workers(4, 1000, PAR_MARK_MIN_OBJECTS), 0);
        assert_eq!(effective_workers(4, PAR_MARK_MIN_OBJECTS, PAR_MARK_MIN_OBJECTS), 2);
        assert_eq!(effective_workers(4, 600_000, PAR_MARK_MIN_OBJECTS), 2);
        assert_eq!(effective_workers(4, 800_000, PAR_MARK_MIN_OBJECTS), 3);
        assert_eq!(effective_workers(4, 4_000_000, PAR_MARK_MIN_OBJECTS), 4);
        assert_eq!(effective_workers(7, 10, 0), 2);
    }

    #[test]
    fn workers_from_value() {
        assert_eq!(par_mark_workers_from(None, 8), 4);
        assert_eq!(par_mark_workers_from(Some("0"), 8), 0);
        assert_eq!(par_mark_workers_from(Some("off"), 8), 0);
        assert_eq!(par_mark_workers_from(Some("1"), 8), 4);
        assert_eq!(par_mark_workers_from(Some("on"), 2), 2);
        assert_eq!(par_mark_workers_from(Some("7"), 2), 7);
        assert_eq!(par_mark_workers_from(Some("99"), 2), PAR_MARK_MAX_WORKERS);
        assert_eq!(par_mark_workers_from(Some("junk"), 8), 0);
    }

    /// A random graph over `n` nodes (index-encoded as fake addresses
    /// `(i + 1) * 8`): the drain on 1, 2, 3 and 7 workers marks exactly the
    /// nodes reachable from the seed, scans each exactly once, and the
    /// per-worker states hold every scanned node exactly once.
    #[test]
    fn t1_g2_the_parallel_drain_reaches_exactly_the_serial_closure_once_each() {
        let n = 20_000usize;
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let edges: Vec<Vec<usize>> = (0..n)
            .map(|i| {
                let k = (next() % 4) as usize;
                // Node 0 is a wide "array"; others have 0-3 random edges, and
                // every 7th node is unreachable garbage (no one points at it
                // except other garbage).
                if i == 0 {
                    (1..(PAR_ARRAY_CHUNK * 2 + 77).min(n)).collect()
                } else {
                    (0..k)
                        .map(|_| (next() as usize) % n)
                        .filter(|&t| t % 7 != 3 || i % 7 == 3)
                        .collect()
                }
            })
            .collect();
        // Serial reference closure.
        let mut want = vec![false; n];
        let mut st = vec![0usize];
        want[0] = true;
        while let Some(i) = st.pop() {
            for &t in &edges[i] {
                if !want[t] {
                    want[t] = true;
                    st.push(t);
                }
            }
        }
        for threads in [1usize, 2, 3, 7] {
            let marks: Vec<AtomicU8> = (0..n).map(|_| AtomicU8::new(0)).collect();
            let scans: Vec<AtomicU8> = (0..n).map(|_| AtomicU8::new(0)).collect();
            let chunk_slots = AtomicUsize::new(0);
            marks[0].store(1, Ordering::Relaxed);
            let addr = |i: usize| ((i + 1) * 8) as *mut u8;
            let idx = |p: *mut u8| p as usize / 8 - 1;
            let (states, counts) = drain(
                vec![addr(0)],
                Vec::new(),
                threads,
                true,
                Vec::<usize>::new,
                |p, stack, mine: &mut Vec<usize>| {
                    let i = idx(p);
                    scans[i].fetch_add(1, Ordering::Relaxed);
                    mine.push(i);
                    // Node 0 (the wide one) is split into slot ranges.
                    if i == 0 {
                        return edges[0].len();
                    }
                    for &t in &edges[i] {
                        if marks[t]
                            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            stack.push(addr(t));
                        }
                    }
                    0
                },
                |p, s, e, stack, _mine: &mut Vec<usize>| {
                    assert_eq!(idx(p), 0, "only node 0 is split");
                    chunk_slots.fetch_add(e - s, Ordering::Relaxed);
                    for &t in &edges[0][s..e] {
                        if marks[t]
                            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            stack.push(addr(t));
                        }
                    }
                },
            );
            assert_eq!(
                chunk_slots.load(Ordering::Relaxed),
                edges[0].len(),
                "threads={threads}: every slot range of the split array ran once"
            );
            assert_eq!(states.len(), threads.max(1));
            let got: Vec<bool> = marks
                .iter()
                .map(|m| m.load(Ordering::Relaxed) == 1)
                .collect();
            assert_eq!(got, want, "threads={threads}: marked set differs");
            for i in 0..n {
                let s = scans[i].load(Ordering::Relaxed);
                assert_eq!(
                    s, want[i] as u8,
                    "threads={threads}: node {i} scanned {s} times"
                );
            }
            let mut all: Vec<usize> = states.into_iter().flatten().collect();
            all.sort_unstable();
            let reach: Vec<usize> = (0..n).filter(|&i| want[i]).collect();
            assert_eq!(all, reach, "threads={threads}: per-worker outputs");
            assert_eq!(counts.scanned, reach.len());
        }
    }

    /// An empty seed terminates at once on every width.
    #[test]
    fn t1_g2_an_empty_drain_terminates() {
        for threads in [1usize, 2, 5] {
            let (states, counts) = drain(
                Vec::new(),
                Vec::new(),
                threads,
                false,
                || 0u8,
                |_, _, _| 0,
                |_, _, _, _, _| {},
            );
            assert_eq!(states.len(), threads.max(1));
            assert_eq!(counts.scanned, 0);
        }
    }
}
