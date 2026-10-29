// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Stop-the-world GC barrier for multi-threaded execution.
//!
//! When garbage collection is needed, the triggering thread requests a
//! stop-the-world (STW) pause. All other threads, at their next safepoint
//! (allocation site or backward branch), deposit their root ObjectRefs and
//! wait for GC to complete. After collection, each thread applies the
//! pointer map to update its own frame references.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::threading::jvm_thread::ThreadId;
use crate::threading::thread_state::{self, ThreadExecState};

// ---------------------------------------------------------------------------
// Barrier-local helpers (gc-common round 2026-09-23, lane A)
// ---------------------------------------------------------------------------

/// Read-once memo of a declared debug flag, override-aware.
///
/// The two `CRATONVM_DBG_*` probes below used to be uncached
/// `runtime_var_os` reads, taken INSIDE the barrier's `inner` critical
/// section on every request and every arrival: a declared-name hash lookup
/// plus a flag-snapshot read per arriving thread, serialised under the one
/// mutex every waking mutator also needs. Same `MemoSlot` machinery
/// `runtime::env_cache` uses, so a test's `with_thread_overrides` still
/// re-derives the value instead of reading a latched one.
#[inline]
fn memo_is_set(slot: &'static cratonvm_types::flags::MemoSlot, name: &str) -> bool {
    match slot.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_var_os(name).is_some();
            slot.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// zt t30/cv (perf-counter page `zgc-zt12pl4-*` item 3):
/// `CRATONVM_PERF_VM_THREAD_CPU` (default ON, `=0` kill) -- whether a pause
/// books its initiator's thread CPU to [`GcBarrier::vm_thread_cpu_ns`]
/// (`sun.threads.cpu_time.vm`). Memoised like the debug flags above.
#[inline]
fn perf_vm_thread_cpu_on() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PERF_VM_THREAD_CPU");
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// zt t30/cv: the CALLING thread's CPU time in nanoseconds, what HotSpot's
/// `os::thread_cpu_time` reads for `CPUTimeCounters`
/// (`cratonvm_gc::thread_cpu::current_thread_cpu_ns`: Linux
/// `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, and since zt pl4 item 3 Windows
/// `GetThreadTimes`, whose scheduler-quantum resolution HotSpot's Windows
/// figure has too: HotSpot 25.0.3 on Windows answers non-zero once its VM
/// thread's operations add up to a quantum, measured 2026-10-23, against the
/// t30 reading that it is always 0). `None` elsewhere or under
/// `CRATONVM_PERF_VM_THREAD_CPU=0`.
fn vm_op_thread_cpu_ns() -> Option<u64> {
    if !cfg!(any(target_os = "linux", windows)) || !perf_vm_thread_cpu_on() {
        return None;
    }
    cratonvm_gc::thread_cpu::current_thread_cpu_ns()
}

/// zt t30/cv: one pause's share of the VM thread's CPU. This VM has no VM
/// thread: a stop-the-world operation runs on the thread that requested it,
/// so its "VM thread" CPU is that thread's CPU from the accepted request
/// (`start`: its [`current_thread_token`] and CPU then) to `complete_gc`
/// (`end_token`, `end_cpu`). A pause completed by another thread (a takeover)
/// books nothing: one thread's clock cannot be read from another here. Never
/// negative.
fn vm_op_cpu_share(start: Option<(u64, u64)>, end_token: Option<u64>, end_cpu: Option<u64>) -> u64 {
    match (start, end_token, end_cpu) {
        (Some((token, cpu0)), Some(token1), Some(cpu1)) if token == token1 => {
            cpu1.saturating_sub(cpu0)
        }
        _ => 0,
    }
}

/// `CRATONVM_DBG_STW_CENSUS` — per-request / per-arrival census lines.
#[inline]
fn stw_census_dbg() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    memo_is_set(&SLOT, "CRATONVM_DBG_STW_CENSUS")
}

/// `CRATONVM_DBG_MAPGEN` — map/generation disagreement report.
#[inline]
fn mapgen_dbg() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    memo_is_set(&SLOT, "CRATONVM_DBG_MAPGEN")
}

/// Process-unique token for the calling OS thread, or `None` once the
/// thread's TLS has been torn down.
///
/// # Why the barrier needs an identity that is not a `ThreadId`
///
/// "Am I the initiator?" was answered only by comparing the CALLER-SUPPLIED
/// `ThreadId` with the one the request recorded, and the blocked-region LEAVE
/// paths (`BlockedGuard::drop`, `mark_blocked_region_leave_after`) are given
/// no id at all — so they could not ask, and an initiator closing a blocked
/// region inside its own pause waited for a generation only it can advance.
///
/// The request is always made on the initiator's own OS thread and
/// `complete_gc` always runs there too, so "same OS thread" is the exact
/// meaning of "initiator". A plain monotonic token rather than
/// `std::thread::current().id()` because the latter clones an `Arc` and is
/// not guaranteed usable during TLS destruction; `try_with` failing simply
/// means "no answer" and the callers fall back to what they did before.
#[inline]
fn current_thread_token() -> Option<u64> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static TOKEN: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    TOKEN.try_with(|t| *t).ok()
}

/// Time-to-safepoint statistics for one [`GcBarrier`] — see
/// [`GcBarrier::ttsp_stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TtspStats {
    /// Pauses whose quota was observed satisfied by the initiator.
    pub pauses: u64,
    /// Sum of request-to-quota-satisfied times, nanoseconds.
    pub total_ns: u64,
    /// Longest single request-to-quota-satisfied time, nanoseconds.
    pub max_ns: u64,
    /// The most recent pause's time, nanoseconds.
    pub last_ns: u64,
    /// The thread the SLOWEST pause waited for last (`ThreadId.0` of its final
    /// participating arrival) — the time-to-safepoint straggler HotSpot's
    /// `-Xlog:safepoint` names. `None` when that pause's quota was met without
    /// a participating arrival (no peer, or the last peers were frozen in
    /// compiled code and excused by the take-over). gc-common w3-a.
    pub max_straggler: Option<u64>,
    /// [`Self::max_straggler`] for the most recent pause.
    pub last_straggler: Option<u64>,
}

/// gci i44/sp: one [`GcBarrier`]'s safepoint accounting, the four figures
/// HotSpot's `RuntimeService` keeps for `sun.management.VMManagementImpl`
/// (`getSafepointCount`, `getTotalSafepointTime`, `getSafepointSyncTime`,
/// `getTotalApplicationNonStoppedTime`, read by `sun.management.HotspotRuntime`).
/// Those natives answered a constant 0 ("no safepoint accounting",
/// `native-builtins/src/jmx.rs`) although every pause of this barrier IS a
/// safepoint and the barrier already timed its time-to-safepoint. See
/// [`GcBarrier::safepoint_stats`] and
/// `docs/feature-designs/jdk-only-mode.md`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SafepointStats {
    /// Pauses completed (`complete_gc` of a pause in flight): HotSpot's
    /// `sun.rt.safepoints`.
    pub count: u64,
    /// Sum, over completed pauses, of request accepted -> `complete_gc`
    /// (time-to-safepoint included, as HotSpot's `sun.rt.safepointTime`
    /// includes its sync), nanoseconds.
    pub stopped_ns: u64,
    /// Sum of request accepted -> quota met ([`TtspStats::total_ns`]),
    /// nanoseconds, never above `stopped_ns`: `sun.rt.safepointSyncTime`.
    pub sync_ns: u64,
    /// Sum of the intervals between pauses (barrier creation or the last
    /// `complete_gc` -> the next accepted request), nanoseconds: HotSpot's
    /// `sun.rt.applicationTime`, which likewise grows only when a safepoint
    /// begins.
    pub application_ns: u64,
}

/// `ThreadId.0 + 1`, `0` for "none" — the encoding of the straggler atomics.
#[inline]
fn encode_straggler(tid: Option<u64>) -> u64 {
    tid.map_or(0, |t| t.saturating_add(1))
}

#[inline]
fn decode_straggler(v: u64) -> Option<u64> {
    v.checked_sub(1)
}

/// gci i20/st: the most arrived ids the stall line prints.
const STALL_ARRIVED_IDS_SHOWN: usize = 256;

/// gci i20/st: the parked-waiter stall line's `arrived_ids=` field: the ids
/// sorted, at most [`STALL_ARRIVED_IDS_SHOWN`] of them, then `+N` for the
/// rest (`[3, 7, 9]`, `[]`, `[1, 2, +40]`). Pure.
fn stall_arrived_ids_text(ids: &[u64]) -> String {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    let mut parts: Vec<String> = sorted
        .iter()
        .take(STALL_ARRIVED_IDS_SHOWN)
        .map(u64::to_string)
        .collect();
    if sorted.len() > STALL_ARRIVED_IDS_SHOWN {
        parts.push(format!("+{}", sorted.len() - STALL_ARRIVED_IDS_SHOWN));
    }
    format!("[{}]", parts.join(", "))
}

// ---------------------------------------------------------------------------
// gci i6/jr: resume fairness (`CRATONVM_STW_RESUME_FAIRNESS`, default OFF)
// ---------------------------------------------------------------------------
//
// `docs/known-issues/gc/gc-jni-linux-generational-jna-upcalls-are-10x-slower-under-churn-20261010.md`.
// A thread that waited out a pause leaves its wait loop only once it has
// re-taken `inner` after `complete_gc`'s `notify_all` (the release -> resume
// latency). The initiator never parked: it returns from `complete_gc` running,
// and a churning allocator can raise the NEXT pause before a parked peer has
// even resumed. The peer then runs to its very next poll and parks again, so a
// thread with many polls per unit of work (the JNA up-call's Java) advances one
// poll per pause while the allocator advances a whole young window. That is the
// starvation the page's numbers describe (Generational: 6.8 young pauses
// crossed per up-call, G1: 0.09, at a similar pause rate; Generational's
// window between pauses is about half G1's, so the same resume latency eats
// all of it).
//
// The census below is always on (a few relaxed atomics per waiter per pause,
// under the lock the waiter already holds): `starved_requests` counts requests
// that found a waiter released by the previous pause but not yet resumed, and
// `resume_*` the release -> resume latency. The switch adds the rule: a request
// that finds such a waiter, or a waiter that resumed less than
// [`RESUME_FAIRNESS_GRANT`] ago, first lets it in -- the lock released, the
// requester yielding its CPU -- for at most [`RESUME_FAIRNESS_MAX_HOLD`]. No
// timed condvar wait (it would round to the 15.625 ms tick on Windows).

/// gci i6/jr: how long a thread that waited out a pause is guaranteed to run
/// before the next pause of the same barrier raises its flag (switch on).
const RESUME_FAIRNESS_GRANT: Duration = Duration::from_micros(250);

/// gci i6/jr: the longest one request holds for the rule (switch on). A waiter
/// still not resumed then is counted in `hold_timeouts` and the pause goes on.
/// gci i15/hx: also the bound of the in-native leave rule below.
const RESUME_FAIRNESS_MAX_HOLD: Duration = Duration::from_millis(2);

// ---------------------------------------------------------------------------
// gci i15/hx: in-native leave fairness (`CRATONVM_STW_IN_NATIVE_LEAVE_FAIRNESS`,
// default ON, `=0` kills)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jvmti-delivery-threading.md`.
// With the JNI native transitions (`CRATONVM_JNI_NATIVE_TRANSITIONS`, default
// ON since 2026-10-13) a Java thread inside a JNI native is EXCLUDED from every
// pause while its C code runs, and EVERY JNIEnv / C JVMTI function it calls
// leaves native through [`GcBarrier::leave_in_native_if`], which waits out a
// pause that is merely REQUESTED (HotSpot's native -> VM transition blocks the
// same way). A thread that requests pauses back to back -- `System.gc()` in a
// loop, an allocator at the young trigger -- returns from `complete_gc`
// running and raises the next flag microseconds later, before the woken
// leaver has re-taken `inner`; the pause excludes the leaver (its flag is
// still up), so the leaver waits the whole next pause too, and so on. Each
// JNIEnv call then costs about one pause instead of a few microseconds: a
// native making thousands of calls per invocation (the vi9-hj T probe: ~8 000
// per round, 100 rounds) ran past 600 s where the same run with the package
// off (the native counted, the pause waiting for it to return to Java) took 5 s.
// The i6/jr census names this exactly (`starved_requests`, `leave_repeats`);
// its general rule (`CRATONVM_STW_RESUME_FAIRNESS`) stays opt-in.
//
// The rule here is the same hold, narrowed to the package's leavers: a request
// that finds an in-native leaver released by the previous pause and not yet
// out of its leave, or one that finished such a leave less than
// [`IN_NATIVE_LEAVE_GRANT`] ago, first lets it run -- the lock released, the
// requester yielding -- for at most [`RESUME_FAIRNESS_MAX_HOLD`]. Sound for the
// same reason as the general hold: it only DELAYS a request, nothing of the
// coming pause exists yet, and the bound turns a leaver the OS does not
// schedule into one bounded delay. Inert unless a thread left native through
// [`GcBarrier::leave_in_native_if`] while a pause was requested.

/// gci i15/hx: how long a thread that waited out a pause on its way out of
/// native is guaranteed to run before the next pause of the same barrier
/// raises its flag (`CRATONVM_STW_IN_NATIVE_LEAVE_FAIRNESS`). Long enough for
/// a few dozen full JNIEnv round trips; the pause it delays is bounded by
/// [`RESUME_FAIRNESS_MAX_HOLD`].
const IN_NATIVE_LEAVE_GRANT: Duration = Duration::from_millis(1);

// ---------------------------------------------------------------------------
// gci i18/fh: the in-native leave rule holds only a request that RACED the
// leaver (`CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY`, default ON, `=0` restores
// the i15/hx trigger exactly)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jdk-only-mode.md`.
// i15/hx held EVERY request that arrived inside a leaver's whole
// [`IN_NATIVE_LEAVE_GRANT`], whatever the request's timing. The starvation it
// fixes is a race: the next request arrives before the woken leaver is back
// (`in_native_leavers > 0`), or microseconds after it (a `System.gc()` loop's
// initiator returns from `complete_gc` running and requests again at once).
// A thread whose pauses come at their own pace -- `GcfF2jJniRepeatProbe
// full`'s churn: a young collection every few ms and a frame-trace sample
// every 64 arrays, about every millisecond -- lands a request at a random
// point of the grant, so most of its requests were held (up to 1 ms each,
// the holder a counted mutator spinning without a poll) although the leaver
// had already run long enough to be in no danger of starving.
//
// Here a request STARTS a hold only when it raced the leaver: the leaver has
// not finished its leave, or finished it less than
// [`IN_NATIVE_LEAVE_RACE_WINDOW`] ago. A hold so started runs exactly as
// i15/hx's (until the leaver is out and its whole grant has passed, bounded by
// [`RESUME_FAIRNESS_MAX_HOLD`]), so the back-to-back initiator of the hx
// probes still gives the leaver its millisecond. A request arriving later in
// the grant is not held (census `in_native_skips`).
//
// Also (same switch): a hold ends as soon as ANOTHER thread's pause is
// requested. The holder is a counted mutator that does not poll while it
// holds, so that pause's barrier wait would wait out the rest of the hold for
// nothing -- the hold's caller loses the request anyway (census
// `hold_yielded`).

/// gci i18/fh: a request arriving this soon after an in-native leaver
/// finished its leave (or before it did) raced it, and starts the i15/hx hold
/// (`CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY`). The i6/jr general grant: a
/// woken thread re-takes the lock, runs its leave and its next JNIEnv call
/// within it on an unloaded host.
const IN_NATIVE_LEAVE_RACE_WINDOW: Duration = RESUME_FAIRNESS_GRANT;

// ---------------------------------------------------------------------------
// gci i47w/ex47: back-to-back initiator fairness
// (`CRATONVM_STW_BACK_TO_BACK_FAIRNESS`, default ON, `=0` kills)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jdk-only-mode.md`.
// A daemon looping `System.gc()` is the initiator of every pause: it never
// parks, it returns from `complete_gc` running and requests the next pause
// microseconds later. A thread leaving a blocked region
// (`leave_blocked_region_flagged_if`) may leave only when NO pause is
// requested, so after each `complete_gc` it must re-take `inner` before the
// initiator does, and nothing bounds how many times in a row it loses (the
// i6/jr note on that function). The `System.exit` paths of both modes leave
// blocked regions (class loading and file reads under `Shutdown.exit`'s Java,
// the pre-exit hook's region, console flushes), so the exiting thread printed
// its line and then took 25 s to more than 120 s to end the process
// (`vi11-ja` X: `--compatible`, whose exit native runs no Java, 46 s after the
// print). HotSpot's VM thread runs the collection and releases every stopped
// thread at once, the requester included, so no waiter starves.
//
// The rule is i6/jr's general rule (`CRATONVM_STW_RESUME_FAIRNESS`, opt-in),
// started only by the request that is exactly this shape: made by the OS
// thread that ran the previous `complete_gc`, less than
// [`BACK_TO_BACK_WINDOW`] after it, while a waiter that pause released has not
// re-taken `inner` yet (or another thread resumed less than
// [`RESUME_FAIRNESS_GRANT`] ago). The hold is the general one: the lock
// released, the requester yielding, bounded by [`RESUME_FAIRNESS_MAX_HOLD`],
// and it ends when another thread's pause is requested. Sound for the same
// reason: it only DELAYS a request, nothing of the coming pause exists yet.
// A request at its own pace (an allocator at the young trigger) is not held.

/// gci i47w/ex47: a request made this soon after its own thread's
/// `complete_gc` released the previous pause is back to back.
const BACK_TO_BACK_WINDOW: Duration = Duration::from_millis(5);

/// gci i15/rb: ends an in-native leave counted by
/// `GcBarrier::note_in_native_leave_wait` -- on drop, so on every way out of
/// `GcBarrier::leave_in_native_if`'s waiting path, an unwind included (see
/// there).
struct InNativeLeaveDone<'a>(&'a GcBarrier);

impl Drop for InNativeLeaveDone<'_> {
    fn drop(&mut self) {
        self.0.note_in_native_leave_done();
    }
}

/// gci i6/jr: slots of [`GcBarrier::resume_census`].
const RC_RESUMES: usize = 0;
const RC_RESUME_NS: usize = 1;
const RC_RESUME_MAX_NS: usize = 2;
const RC_RESUME_MAX_TID: usize = 3;
const RC_STRADDLES: usize = 4;
const RC_LEAVE_REPEATS: usize = 5;
const RC_STARVED_REQUESTS: usize = 6;
const RC_HOLDS: usize = 7;
const RC_HOLD_NS: usize = 8;
const RC_HOLD_MAX_NS: usize = 9;
const RC_HOLD_TIMEOUTS: usize = 10;
/// gci i18/fh: holds the in-native leave rule started (a subset of `RC_HOLDS`).
const RC_IN_NATIVE_HOLDS: usize = 11;
/// gci i18/fh: requests inside an in-native leaver's grant that did not race
/// it and so were not held (i15/hx held them).
const RC_IN_NATIVE_SKIPS: usize = 12;
/// gci i18/fh: holds that ended because another thread's pause was requested.
const RC_HOLD_YIELDED: usize = 13;
/// gci i47w/ex47: holds the back-to-back initiator rule started (a subset of
/// `RC_HOLDS`).
const RC_BACK_TO_BACK_HOLDS: usize = 14;
const RC_LEN: usize = 15;

/// gci i6/jr: the fairness rule, pure. A request holds while some thread that
/// waited out the previous pause has not left its wait loop
/// (`waiters_in_wait > 0`), or the last one to leave did so less than
/// `grant_ns` ago -- unless that last one is the requester itself
/// (`requester_resumed_last`): it has had its window. `last_resume_ns == 0`:
/// nobody ever resumed.
#[inline]
pub(crate) fn resume_fairness_should_hold(
    waiters_in_wait: u32,
    now_ns: u64,
    last_resume_ns: u64,
    grant_ns: u64,
    requester_resumed_last: bool,
) -> bool {
    if waiters_in_wait > 0 {
        return true;
    }
    !requester_resumed_last
        && last_resume_ns != 0
        && now_ns < last_resume_ns.saturating_add(grant_ns)
}

/// gci i47w/ex47: the back-to-back trigger, pure. A request starts the hold
/// only when its thread ran the previous `complete_gc`
/// (`requester_completed_last`) and that pause released its waiters
/// (`released_ns`, `0` = never) less than `window_ns` ago.
#[inline]
pub(crate) fn back_to_back_should_start(
    released_ns: u64,
    now_ns: u64,
    window_ns: u64,
    requester_completed_last: bool,
) -> bool {
    requester_completed_last && released_ns != 0 && now_ns < released_ns.saturating_add(window_ns)
}

/// `Duration` -> nanoseconds, saturating.
#[inline]
fn duration_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The identity census a collection door hands the barrier when it requests a
/// pause — see [`GcBarrier::request_stw_opening_cycle`].
///
/// A struct rather than the `(u32, u32, Vec<u64>)` tuple of
/// the test-only `GcBarrier::request_stw_counted_with_live_blocked` because it carries one
/// more fact that tuple cannot: whether the INITIATOR is itself one of the
/// `alive` threads. `expected = alive - 1 - blocked` assumes it is; an
/// initiator the census did not count would otherwise remove a real mutator
/// from the quota (`docs/feature-designs/jdk-only-mode.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StwCensus {
    /// STW-countable alive threads (`alive && stw_ready`), initiator included
    /// when it is one of them.
    pub alive: u32,
    /// How many of `alive` are published `in_blocked_region` (excluded).
    pub blocked: u32,
    /// The `ThreadId.0` of each of those blocked threads.
    pub blocked_tids: Vec<u64>,
    /// Whether the initiator is one of the `alive` threads.
    ///
    /// `Some(true)` — take its slot off (the usual case). `Some(false)` — the
    /// census provably did not count it (an unregistered host thread, a
    /// carrier before `mark_stw_ready`, a thread after `mark_dead`), so no slot
    /// is removed. `None` — the caller cannot tell; the legacy `- 1` applies.
    pub initiator_counted: Option<bool>,
    /// The `ThreadId.0` of each counted thread (one of `alive`, not one of
    /// `blocked`, not the initiator) the census EXCUSED in a running native
    /// (interpreter round i2 wave 58, lane L6; `jvm_thread::NativeExcuse`):
    /// taken out of `expected`, and no arrival of it counts in this pause.
    /// Empty for every collection.
    pub excused_running: Vec<u64>,
}

/// The `stw_requested` byte, in a cache line of its own, in a page of its own,
/// taken from the same OS allocator the JIT code cache comes from.
///
/// `stw_requested` is the single hottest load in the VM: every interpreter
/// thread reads it **once per bytecode** at the top of the dispatch loop, and
/// compiled code polls the very same byte through
/// [`GcBarrier::stw_requested_flag_addr`]. Its three neighbours in `GcBarrier`
/// are all written by other threads — `gc_generation` every collection,
/// `threads_blocked` on every `enter_blocked`/`leave_blocked` (so every
/// blocking native op: `Object.wait`, `Thread.sleep`, `LockSupport.park`,
/// selector `select`, …), and `inner`, whose `parking_lot::Mutex` word is
/// written on every lock and unlock. All four sat inside the first 64 bytes:
/// `AtomicBool` at offset 0, 7 bytes of alignment padding, then the two
/// `AtomicU64`s at 8 and 16 and the mutex at 24.
///
/// So a write to any of them invalidated, on every interpreting core, the line
/// carrying the flag those cores read every bytecode.
///
/// # What it is worth, bounded rather than guessed
///
/// This cannot be A/B'd the way everything else on its branch was: a struct
/// layout is not a runtime toggle, so there is no kill switch to write. It was
/// therefore bounded arithmetically, from a measured write rate.
///
/// `probes/SharedLine.java` runs compute threads in tight interpreted loops
/// (reading this flag once per bytecode) alongside an **untimed** wait/notify
/// ping-pong, whose every hop crosses `enter_blocked`/`leave_blocked`. Untimed
/// deliberately — a timed wait on Windows is tick-quantized to ~15 ms, which
/// would cap the churn at ~66/s and make the probe vacuous. Measured
/// 2026-09-05: **116,025 handoffs in 4 s ≈ 29,000/s**, so ~58,000 writes/s to
/// this line, from a synthetic ping-pong doing nothing else. Real code does
/// not exceed that by orders of magnitude.
///
/// At 8 interpreting threads and a ~70 ns single-socket coherence miss, that
/// is `58_000 * 8 * 70ns` ≈ **32 ms of aggregate CPU per second across 8
/// cores — about 0.4%**, which is below what the harness resolves.
///
/// The cost scales as `writers x readers x miss_latency`, so it grows with
/// core count and again across sockets — but not dramatically: 64 readers at a
/// ~200 ns cross-socket miss is still only ~1%. **This is a small effect, and
/// the honest claim is a bound, not a win.**
///
/// It is kept because it costs one cache line once per process and cannot
/// regress anything, and because the tree already owns the idiom for exactly
/// this reason (`gc/src/zgc/census.rs` and `gc/src/collector.rs` both carry
/// `#[repr(align(64))]` so unrelated counters cannot share a line). Anyone
/// with a many-core box can put a number on it with the probe; that is the
/// only way this one gets measured rather than bounded.
///
/// 64 rather than 128: the two in-tree precedents use 64 and 128 respectively,
/// and the destructive-interference size on x86-64 is 64.
///
/// # Why it is not just `#[repr(align(64))] AtomicBool`
///
/// Isolation was the whole story until 2026-09-10, and `#[repr(align(64))]` on
/// an inline field buys it. What an inline field cannot buy is an ADDRESS, and
/// this byte is one of the few in the VM whose address is compiled INTO
/// machine code.
///
/// `jit/src/x64/safepoint.rs::emit_safepoint_poll` wants
/// `TEST BYTE [rip+disp32], 0xFF` — the entire poll in **one 7-byte
/// instruction and no register** — on the back edge of every compiled loop and
/// at every method entry. `disp32` reaches ±2 GB, so it needs the flag within
/// 2 GB of the code polling it. When it is not, the emitter falls back to
/// `MOV R11, imm64 ; TEST BYTE [R11], 0xFF`: 15 bytes and a clobbered
/// register, at every one of those sites.
///
/// A `GcBarrier` field is reachable only through `Arc<SharedVm>`, i.e. from
/// the Rust global allocator, which in a shipping build is mimalloc
/// (`vm-cli/src/main.rs`). Mimalloc reserves its arenas nowhere near where an
/// anonymous `mmap` / `VirtualAlloc(NULL, ...)` — the primitive
/// `cratonvm_jit::platform` hands the code cache — lands. Measured on Linux
/// x86-64 with `CRATONVM_DBG_JIT_DISASM=*`: code buffer at `0x7DE4D7F9E000`,
/// flag at `0x2000CD6E2C0`, **123.9 TB apart**, so every compiled poll in the
/// process took the long form. Nothing failed and no test noticed, because the
/// fallback is CORRECT — it reads the same byte and branches the same way, it
/// is just longer.
///
/// So the byte moves out of the struct and into a cell from
/// [`cratonvm_jit::platform::alloc_code_adjacent_cell`], which allocates from
/// the code cache's own primitive precisely so the two land in the same region
/// of the address space. The field becomes a `&'static AtomicBool` into that
/// never-unmapped cell; every reader and writer goes through the same four
/// methods and is unaffected.
///
/// Placement stays a HINT — the OS picks the address and may pick badly, and
/// `emit_safepoint_poll` keeps its fallback for exactly that. Verify on a real
/// run with `CRATONVM_DBG_JIT_DISASM=*` and read any loop body:
/// `test byte [rel ...]` means in reach, `mov r11, <imm64>` means it is not.
///
/// # Under `CRATONVM_JIT_CODE_NEAR_GLOBALS`, the `Box::leak` arm is correct
///
/// That strategy solves the same problem from the other end — it hints `mmap`
/// so each code buffer lands near `layout_replace_epoch_guard()`, and it works
/// on this flag only because the flag is a few hundred megabytes from that
/// anchor in the same mimalloc band. A cell would take the flag OUT of that
/// band and leave the poll ~130 TB behind the code the anchor just pulled in.
///
/// So `alloc_code_adjacent_cell` declines while that flag is set, and the
/// `Box::leak` arm below is then the RIGHT answer rather than a fallback: the
/// heap is where the flag belongs when something else owns placement. The two
/// strategies do not compose and are not meant to; what composes is that
/// exactly one of them is ever engaged.
///
/// The 64-byte cell is never freed, so a process that constructs `GcBarrier`
/// N times keeps 64N bytes forever. That is a real leak and it is bounded by
/// how many VMs a process creates; see `alloc_code_adjacent_cell`'s lifetime
/// note.
pub struct CacheLineFlag(&'static AtomicBool);

impl CacheLineFlag {
    /// Take a cell for this flag.
    ///
    /// Not `const` any more (it allocates), which is why `GcBarrier::new` is
    /// the only construction site — there is no `static CacheLineFlag`.
    ///
    /// The `Box::leak` arm is the correctness floor: if the OS refuses the
    /// mapping the flag still exists, still works, and merely gives up the
    /// short encoding — the state every build was in before 2026-09-10.
    fn new(v: bool) -> Self {
        let cell: &'static AtomicBool = match cratonvm_jit::platform::alloc_code_adjacent_cell() {
            // SAFETY: `alloc_code_adjacent_cell` hands back a freshly
            // carved, zero-filled, 64-byte-aligned cell that no other
            // reference names and that is never unmapped or recycled, so
            // the `'static` and the exclusivity of this initializing write
            // both hold. `AtomicBool` is one byte with alignment 1.
            Some(p) => unsafe {
                let p = p.cast::<AtomicBool>();
                p.write(AtomicBool::new(v));
                &*p
            },
            None => Box::leak(Box::new(AtomicBool::new(v))),
        };
        Self(cell)
    }
    /// The flag itself, for the ordinary atomic API -- for LOADS (the
    /// interpreter hoists it; gcj j2/cm2). Every change of `stw_requested`
    /// happens under `loop_poll_words` together with the per-loop pause count
    /// (`GcBarrier::raise_stw_requested` / `lower_stw_requested`); a store
    /// anywhere else would desync those counts.
    #[inline(always)]
    pub fn flag(&self) -> &AtomicBool {
        self.0
    }
    #[inline(always)]
    pub fn load(&self, order: Ordering) -> bool {
        self.0.load(order)
    }
    /// gcj j2/cm2 (j1-cm registry residuals item 10): private -- only the
    /// barrier's raise / lower pair stores the flag. The unused `swap` is gone
    /// (item 11).
    #[inline(always)]
    fn store(&self, v: bool, order: Ordering) {
        self.0.store(v, order)
    }
}

/// One OS thread's dispatch-loop poll word (interpreter round i1 wave 26, lane
/// L7; the proposal
/// `docs/feature-designs/jdk-only-mode.md`).
///
/// The interpreter's dispatch loop asks one question before every bytecode:
/// "has anything asked this thread to do VM work first?" It used to ask it
/// twice -- a load of the process-shared [`GcBarrier::stw_requested`] byte, and
/// a compare of the frame stack's redefinition move count
/// (`FrameStack::code_moves`) -- and now asks it once, of this word:
///
/// * the low byte ([`Self::PAUSES`]) counts the stop-the-world pauses in
///   progress among the barriers the word is registered with
///   ([`GcBarrier::register_loop_poll_word`]): a barrier adds one to every
///   registered word in the same critical section that raises
///   `stw_requested`, and takes it off in the one that lowers it
///   (`complete_gc`). A count rather than a bit, so no side ever clears
///   another's request; in practice it is 0 or 1;
/// * the bits above are a TRIGGER for frame moves: every move of a frame onto
///   another code allocation (a class redefinition,
///   `interpreter::obsolete_frames`) adds [`Self::MOVE_STEP`] to the word of
///   the OS thread it happens on ([`note_code_moved_on_this_thread`]). The
///   authoritative count stays per stack (`FrameStack::code_moves`); the
///   loop's slow path compares that. This is sound because a stack's frames
///   are only ever moved by the OS thread running its dispatch loops (the
///   move takes `&mut` of its `JvmThread`, which a running loop holds and
///   lends only to its own synchronous callees), or while no loop runs it --
///   and every loop entry starts its gate one move behind.
///
/// So the loop compares the whole word against its snapshot (pause count
/// zero) -- one load of a line only this thread and a pausing collector
/// write, one compare against a register -- and takes a cold path on any
/// difference. While a pause is up the word differs at every bytecode, so the
/// slow path runs at every bytecode, as the flag test did. The flag stays
/// authoritative: compiled code and `safepoint_check` still read
/// `stw_requested`, and the slow path re-reads it before it parks.
///
/// Per OS thread ([`LOOP_POLL`]), not per `JvmThread`: a virtual thread's
/// loops run on its carrier and poll the carrier's word, so a pause touches
/// one word per OS thread that ran a loop, never one per parked virtual
/// thread. Shared (`Arc`) between that thread-local and every barrier it is
/// registered with; [`LoopPollHandle`] marks it dead when the OS thread ends,
/// so the barriers forget it.
pub(crate) struct LoopPollWord {
    word: AtomicU32,
    /// [`GcBarrier`]'s `slot_id` this word was last registered with (`0`:
    /// none), so a dispatch loop entry registers it only when it changes.
    /// Written only by the owning OS thread.
    registered_with: AtomicUsize,
    /// The owning OS thread ended (see [`LoopPollHandle`]).
    owner_dropped: AtomicBool,
    /// t1 lane E (wave 3; `t1-V2-handshake-poll-bit-stale-carrier-arm-skips-the-kick-fallback`):
    /// the owning OS thread's id, in `ThreadEntry::os_tid`'s encoding
    /// (`thread_registry::current_os_tid`), fixed at creation on that thread;
    /// `0` when unknown (the barrier's always-raised word, or a platform with
    /// no tid). A handshake requester arms the word only when its target's
    /// CURRENT mounted tid equals it ([`Self::owner_os_tid`]), so a virtual
    /// thread remounted on another carrier since it published this word is
    /// kicked instead of armed on the old carrier.
    owner_os_tid: u32,
    /// t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`, default ON on Linux and Windows x86-64 since j27): the
    /// address of the owning OS thread's page-poll cell
    /// (`cratonvm_jit::poll_page::this_threads_cell_addr`, taken at creation
    /// on that thread; `0` with the switch off or for the barrier's
    /// always-raised word). The cell mirrors [`Self::HANDSHAKE`]
    /// ([`Self::sync_page_cell`]), so a page-form compiled poll faults into its
    /// slow path while a handshake is armed. Under a lock because a requester
    /// writes another thread's TLS memory: [`LoopPollHandle`]'s drop zeroes it
    /// under the same lock, on the owning thread, before that memory can go.
    page_cell: Mutex<usize>,
}

impl LoopPollWord {
    /// The word's low byte: every reason for a loop -- and, under
    /// `CRATONVM_JIT_POLL_THREAD_WORD`, a compiled poll (`TEST byte [R11],
    /// 0xFF`) -- to take its slow path without changing its gate: the pause
    /// count ([`Self::PAUSE_COUNT`]) and the handshake arm
    /// ([`Self::HANDSHAKE`]). Masked off the loop's gate.
    pub(crate) const PAUSES: u32 = 0xff;
    /// The pauses in progress among the barriers this word is registered with
    /// (t1 lane E: seven bits; the count is in practice 0 or 1 -- one per
    /// barrier pausing at once -- so it never carries into [`Self::HANDSHAKE`]).
    pub(crate) const PAUSE_COUNT: u32 = 0x7f;
    /// t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`, opt-in; step 3 of
    /// `common-a-proposal-per-thread-handshakes`): a handshake was queued for
    /// a thread running on this word's OS thread ([`Self::arm_handshake`]).
    /// In the byte compiled polls test, so a JIT-compiled loop with no helper
    /// call and no allocation reaches its poll's slow path
    /// (`jit_safepoint_slow_path` -> `safepoint_check` ->
    /// `run_pending_handshakes`) at its next back edge. Cleared by the owning
    /// thread BEFORE it drains its queue ([`Self::take_handshake_arm`]), so an
    /// arm that lands after the clear stays set for the next poll: no lost
    /// wakeup. Never set with the switch off.
    pub(crate) const HANDSHAKE: u32 = 0x80;
    /// One frame move on this OS thread (the trigger wraps at 2^24; only its
    /// changes matter).
    pub(crate) const MOVE_STEP: u32 = 0x100;

    /// A word owned by the CALLING OS thread (its [`LOOP_POLL`] initializer).
    fn new() -> Self {
        Self {
            owner_os_tid: crate::threading::thread_registry::current_os_tid(),
            page_cell: parking_lot::const_mutex(cratonvm_jit::poll_page::this_threads_cell_addr()),
            ..Self::with_word(0)
        }
    }

    const fn with_word(word: u32) -> Self {
        Self {
            word: AtomicU32::new(word),
            registered_with: AtomicUsize::new(0),
            owner_dropped: AtomicBool::new(false),
            owner_os_tid: 0,
            page_cell: parking_lot::const_mutex(0),
        }
    }

    /// t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`): make the owning
    /// thread's page-poll cell say what [`Self::HANDSHAKE`] says now -- armed
    /// ([`cratonvm_jit::poll_page::ARMED_OFFSET`]) while the bit is up, `0`
    /// otherwise. Called after EVERY change of the bit, by the requester
    /// ([`Self::arm_handshake`]) and the owner ([`Self::take_handshake_arm`]).
    /// The bit is re-read under the cell lock, so whichever sync runs last
    /// reads every bit change made before it, and every bit change is followed
    /// by a sync: the cell can neither stay armed with the bit down nor miss
    /// an arm. Nothing when the word has no cell.
    fn sync_page_cell(&self) {
        let cell = self.page_cell.lock();
        if *cell == 0 {
            return;
        }
        let armed = self.word.load(Ordering::SeqCst) & Self::HANDSHAKE != 0;
        let value = if armed {
            cratonvm_jit::poll_page::ARMED_OFFSET
        } else {
            0
        };
        // SAFETY: a non-zero cell is the owning thread's live TLS cell: the
        // owner's `LoopPollHandle::drop` zeroes it under this lock before its
        // thread's TLS can be released.
        unsafe { cratonvm_jit::poll_page::store_cell(*cell, value) };
    }

    /// t1 lane E (wave 3): the owning OS thread's id (`0`: unknown, never
    /// matches). See the field.
    #[inline]
    pub(crate) fn owner_os_tid(&self) -> u32 {
        self.owner_os_tid
    }

    /// The whole word, relaxed: the dispatch loop's one load per bytecode.
    /// Nothing is read under its protection: the slow path re-reads the word
    /// with [`Self::load_acquire`] and `stw_requested` with `Acquire` before
    /// it acts on a pause, and compares the stack's own move count.
    #[inline(always)]
    pub(crate) fn load(&self) -> u32 {
        self.word.load(Ordering::Relaxed)
    }

    /// The slow path's read: synchronizes with the barrier's `Release` add,
    /// so a pause count it sees is followed by a flag load that sees the
    /// raised flag (or that pause's own later lowering).
    #[inline]
    pub(crate) fn load_acquire(&self) -> u32 {
        self.word.load(Ordering::Acquire)
    }

    /// A frame of a stack this OS thread runs now runs another code
    /// allocation. An atomic add, not a store: a barrier may be counting a
    /// pause concurrently, and a whole `MOVE_STEP` never carries into the
    /// pause byte. Wraps.
    #[inline]
    fn note_code_moved(&self) {
        self.word.fetch_add(Self::MOVE_STEP, Ordering::Relaxed);
    }

    /// A pause of a barrier this word is registered with began: after the
    /// barrier raised `stw_requested`, under its `loop_poll_words` lock.
    #[inline]
    fn note_pause_begin(&self) {
        let before = self.word.fetch_add(1, Ordering::Release);
        debug_assert!(
            before & Self::PAUSE_COUNT != Self::PAUSE_COUNT,
            "a loop poll word's pause count would carry into its handshake bit"
        );
    }

    /// That pause ended: under the same lock, with the flag lowered. Paired
    /// one-to-one with [`Self::note_pause_begin`] by that lock (see
    /// [`GcBarrier::register_loop_poll_word`]).
    #[inline]
    fn note_pause_end(&self) {
        self.word.fetch_sub(1, Ordering::Release);
    }

    /// The address of the word itself, for compiled code (interpreter round
    /// i6 wave 119, lane L2; `CRATONVM_JIT_POLL_THREAD_WORD`): its first byte
    /// in memory is the [`Self::PAUSES`] byte (x86-64 is little-endian), which
    /// a compiled poll tests with `TEST byte [R11], 0xFF`. The struct's own
    /// address would not do: Rust may place another field first.
    #[inline]
    pub(crate) fn word_addr(&self) -> usize {
        &self.word as *const AtomicU32 as usize
    }

    /// The barrier this word was last registered with (its `slot_id`).
    #[inline]
    pub(crate) fn registered_with(&self) -> usize {
        self.registered_with.load(Ordering::Relaxed)
    }

    fn owner_dropped(&self) -> bool {
        self.owner_dropped.load(Ordering::Acquire)
    }

    /// gcj j2/cm2 (`CRATONVM_GC_HANDSHAKE_TARGET_KICK`, opt-in): send the
    /// dispatch loops on this word's OS thread to their slow path at their
    /// next top, as [`GcBarrier::note_code_moved_on_every_loop`] does for every
    /// word -- one `Release` add of a whole [`Self::MOVE_STEP`], so a loop
    /// whose slow path reads the word with `Acquire` also sees what the kicker
    /// wrote before (a queued handshake). `false`, and nothing done, when the
    /// owning OS thread has ended.
    pub(crate) fn kick(&self) -> bool {
        if self.owner_dropped() {
            return false;
        }
        self.word.fetch_add(Self::MOVE_STEP, Ordering::Release);
        true
    }

    /// t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`, opt-in): raise
    /// [`Self::HANDSHAKE`], for a handshake REQUESTER after it queued its op.
    /// The `Release` RMW orders the queue push before it, so the owner's
    /// `Acquire` clear ([`Self::take_handshake_arm`]) that sees the bit also
    /// sees the op queued. Sends the owner's dispatch loops to their slow path
    /// at their next top (the low byte is masked off their gate) and, under
    /// `CRATONVM_JIT_POLL_THREAD_WORD`, its compiled polls to theirs. `false`,
    /// and nothing done, when the owning OS thread has ended.
    pub(crate) fn arm_handshake(&self) -> bool {
        if self.owner_dropped() {
            return false;
        }
        self.word.fetch_or(Self::HANDSHAKE, Ordering::Release);
        // t1 wave 8, lane SAFEPOLL: and the owner's page-form polls.
        self.sync_page_cell();
        true
    }

    /// t1 lane E: lower [`Self::HANDSHAKE`] if it is up, answering whether it
    /// was. Called by the OWNING OS thread at a poll's slow path, BEFORE it
    /// drains its handshake queue: an arm ordered before this clear has its op
    /// visible to the drain (`AcqRel`), and one after it leaves the bit up for
    /// the next poll. One relaxed load when the bit is down.
    #[inline]
    pub(crate) fn take_handshake_arm(&self) -> bool {
        if self.word.load(Ordering::Relaxed) & Self::HANDSHAKE == 0 {
            return false;
        }
        let was_up = self.word.fetch_and(!Self::HANDSHAKE, Ordering::AcqRel) & Self::HANDSHAKE != 0;
        // t1 wave 8, lane SAFEPOLL: disarm the page-form polls too (re-arms
        // if a requester raised the bit again meanwhile).
        self.sync_page_cell();
        was_up
    }
}

/// t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`): lower the handshake arm of the
/// calling OS thread's own poll word ([`LoopPollWord::take_handshake_arm`]),
/// for the compiled poll's slow path before it drains the queue. Nothing once
/// the thread-local is destroyed (no requester can arm it then: it was never
/// published, or its owner is marked dropped).
#[inline]
pub(crate) fn take_this_threads_handshake_arm() -> bool {
    LOOP_POLL
        .try_with(|h| h.word().take_handshake_arm())
        .unwrap_or(false)
}

/// gcj j2/cm2: the calling OS thread's dispatch-loop poll word ([`LOOP_POLL`])
/// as a shared handle, for a thread to publish the word its loops run on
/// (`GcBlockState::publish_loop_poll_word`). `None` once the thread-local is
/// destroyed (a loop entered then polls the barrier's always-raised word).
pub(crate) fn this_threads_loop_poll_word() -> Option<Arc<LoopPollWord>> {
    LOOP_POLL.try_with(|h| Arc::clone(h.word())).ok()
}

/// The owning OS thread's reference to its [`LoopPollWord`] ([`LOOP_POLL`]):
/// dropping it (a thread-local destructor, at thread exit) tells every
/// barrier holding the word to forget it at its next request or registration.
pub(crate) struct LoopPollHandle(Arc<LoopPollWord>);

impl LoopPollHandle {
    pub(crate) fn new() -> Self {
        Self(Arc::new(LoopPollWord::new()))
    }

    #[inline(always)]
    pub(crate) fn word(&self) -> &Arc<LoopPollWord> {
        &self.0
    }
}

impl Drop for LoopPollHandle {
    fn drop(&mut self) {
        self.0.owner_dropped.store(true, Ordering::Release);
        // t1 wave 8, lane SAFEPOLL: forget the page-poll cell under its lock,
        // on the owning thread, so no requester writes this thread's TLS once
        // it may be released; and leave the cell disarmed, so compiled code a
        // later thread-local destructor runs does not fault at every poll
        // for an arm no one can lower any more.
        let mut cell = self.0.page_cell.lock();
        if *cell != 0 {
            // SAFETY: our own thread's cell, still live (we are running on it).
            unsafe { cratonvm_jit::poll_page::store_cell(*cell, 0) };
            *cell = 0;
        }
    }
}

thread_local! {
    /// This OS thread's dispatch-loop poll word ([`LoopPollWord`]). Per OS
    /// thread, not per VM: it holds no VM's facts, only pause counts that
    /// each barrier it is registered with adds and removes itself, and a move
    /// trigger whose meaning the loop's slow path checks against the stack's
    /// own count.
    static LOOP_POLL: LoopPollHandle = LoopPollHandle::new();
}

/// A frame of a stack this OS thread runs moved onto another code allocation
/// (`FrameStack::note_code_moved`): bump this thread's poll word, so every
/// dispatch loop on it takes its slow path at its next top and compares its
/// stack's move count. Nothing when the thread-local is already gone (a
/// thread-local destructor running Java): a loop entered then polls
/// [`GcBarrier::loop_poll_word_for_this_thread`]'s always-raised word, whose
/// slow path compares the count at every bytecode.
#[inline]
pub(crate) fn note_code_moved_on_this_thread() {
    let _ = LOOP_POLL.try_with(|h| h.word().note_code_moved());
}

/// One OS thread's JNI LEAF-WINDOW word (gcd d5/f; the native5 page
/// `gcd-d2i-jni-native-methods-are-counted-mutators-CLOSED-USER-DECISION-20261012.md`).
///
/// A thread inside a JNI native that runs in native
/// (`CRATONVM_JNI_NATIVE_TRANSITIONS`) is EXCLUDED from every pause: its
/// `in_blocked_region` flag is up and its roots are its deposited snapshot. A
/// handful of JNIEnv functions only read or write the primitive contents of an
/// object the native names through one of its own local handles
/// (`GetArrayLength`, `Get/Set<Prim>ArrayRegion`, `GetString[UTF]Length`) or
/// touch only the thread's own local-handle table (`DeleteLocalRef`): they
/// allocate nothing, run no Java, create no reference, block on nothing and
/// take no lock a pausing thread can hold. Such a function does not need the
/// full native -> VM -> native round trip (wait out a pause, apply the
/// window's fixups, re-deposit the whole root snapshot: ~8 us per call,
/// `Gcd1JniCostProbe`); it needs only the guarantee HotSpot's
/// `_thread_in_vm` gives -- no collection runs while it touches the heap.
///
/// The word is that guarantee, as HotSpot's thread state is: the thread raises
/// `open` and then reads `stw_requested` (a sequentially consistent fence
/// between), and backs off to the full transition if a pause is up; a winning
/// pause request raises `stw_requested` and then, after the same kind of
/// fence, waits for every registered word to read closed
/// ([`GcBarrier::drain_leaf_windows`], inside the request, before the
/// initiator publishes a root or the first collector phase runs; a slow
/// window is waited for with the barrier's `inner` released,
/// [`GcBarrier::finish_owed_leaf_drain`]). Whichever
/// side goes second sees the other, so a pause never overlaps an open window
/// and a window never opens during a pause. The thread stays excluded
/// throughout, exactly as while it runs C code; the window only proves that
/// no pause is in flight while it reads.
///
/// Owned by the calling thread's JNI state (`native::jni`'s one thread-local),
/// so it adds no static; registered with each barrier it is used under
/// ([`GcBarrier::register_leaf_window_word`]), and forgotten by every barrier
/// once its owner dropped it ([`Self::mark_owner_dropped`]).
/// gcj j3/rt3 residual 3: [`GcBarrier::drain_leaf_windows`]'s budget under
/// `inner` -- spins, then yields, summed over every open window. A window
/// still open past it is waited for with `inner` released
/// ([`GcBarrier::finish_owed_leaf_drain`]).
const LEAF_DRAIN_SPINS: u32 = 64;
const LEAF_DRAIN_LOCKED_ROUNDS: u32 = 128;

pub(crate) struct LeafWindowWord {
    open: AtomicBool,
    /// [`GcBarrier`]'s `slot_id` this word was last registered with (`0`:
    /// none). Written only by the owning OS thread.
    registered_with: AtomicUsize,
    /// The owning OS thread ended.
    owner_dropped: AtomicBool,
}

impl LeafWindowWord {
    pub(crate) const fn new() -> Self {
        Self {
            open: AtomicBool::new(false),
            registered_with: AtomicUsize::new(0),
            owner_dropped: AtomicBool::new(false),
        }
    }

    /// End the window [`GcBarrier::try_open_leaf_window`] opened. `Release`:
    /// everything the window did happens-before the drain that reads it
    /// closed. A no-op store when it is not open.
    #[inline]
    pub(crate) fn close(&self) {
        self.open.store(false, Ordering::Release);
    }

    /// Is the window open right now? For tests.
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// The owning thread's JNI state is being dropped (its OS thread ends):
    /// every barrier forgets the word at its next registration or drain.
    pub(crate) fn mark_owner_dropped(&self) {
        self.open.store(false, Ordering::Release);
        self.owner_dropped.store(true, Ordering::Release);
    }

    fn owner_dropped(&self) -> bool {
        self.owner_dropped.load(Ordering::Acquire)
    }
}

/// Coordinates stop-the-world pauses for garbage collection.
///
/// The barrier uses a cheap `AtomicBool` flag (`stw_requested`) that threads
/// poll at safepoints. When set, threads deposit their roots and wait for
/// the GC initiator to finish collection.
pub struct GcBarrier {
    /// Cheap flag polled at every safepoint. Only requires an atomic load.
    ///
    /// Not stored here: [`CacheLineFlag`] is a reference into a cell that
    /// lives outside this struct entirely, on its own cache line, in its own
    /// mapping — see its doc for both reasons (false sharing, and putting the
    /// byte within `disp32` reach of the code that polls it). Its position
    /// among these fields therefore no longer means anything; it used to have
    /// to be FIRST.
    pub stw_requested: CacheLineFlag,
    /// GC generation counter — incremented after each collection.
    /// Threads compare their local generation to detect missed GCs.
    ///
    /// On a cache line of its own ([`PaddedU64`], gc-common w5-a,
    /// `handoff-w4b-gc-generation-on-its-own-cache-line`): since w4-b it is a
    /// HOT READ — the per-thread JIT memo caches validate against it on every
    /// `HashMap.get` / case-conversion memo probe
    /// (`memory::roots::validate_jit_memo_caches`) — while `threads_blocked`
    /// and the `inner` mutex word beside it are written on every blocking
    /// transition. `Deref` keeps every `.load` / `.fetch_add` call site as is.
    pub gc_generation: PaddedU64,
    /// T19.H1 — count of threads currently parked in a *blocking* native
    /// operation (`Object.wait`, `Thread.sleep`, `LockSupport.park`,
    /// `Thread.join`, `ReferenceQueue.remove`, selector `select`, …).
    ///
    /// Such a thread is GC-safe: before blocking it deposits its frame
    /// roots into the thread-registry snapshot (`deposit_root_snapshot`),
    /// and on wake it applies the GC pointer map (`check_post_block_gc`).
    /// While parked it executes no code and cannot reach an interpreter
    /// safepoint to call `arrive_and_wait`.
    ///
    /// The stop-the-world barrier must therefore NOT include parked
    /// threads in `expected` — otherwise `wait_for_all` deadlocks waiting
    /// for a thread that is (correctly) blocked indefinitely, e.g. the
    /// Reference Handler parked in `ReferenceQueue.remove`. This is the
    /// JVM `_thread_blocked` state, scoped precisely to genuine blocking
    /// operations (NOT every native call — a *running* native still
    /// holds raw `ObjectRef`s in Rust locals and must be waited for so
    /// the copying collector does not relocate objects under it).
    ///
    /// Maintained by `enter_blocked` / `leave_blocked`, called from
    /// `deposit_root_snapshot` / `check_post_block_gc`.
    threads_blocked: AtomicU64,
    /// Protected coordination state.
    inner: Mutex<GcBarrierInner>,
    /// Signaled when all expected threads have arrived at the barrier.
    all_arrived: Condvar,
    /// Signaled when GC is complete and threads can resume.
    gc_complete: Condvar,
    /// The empty map handed to callers that did not park (no pause active, or
    /// the initiator itself). Shared so that answer allocates nothing.
    empty_map: Arc<cratonvm_types::PointerMap>,
    /// Time-to-safepoint accounting (request -> quota satisfied), written once
    /// per pause by the initiator. See [`Self::ttsp_stats`].
    ttsp_pauses: AtomicU64,
    ttsp_total_ns: AtomicU64,
    ttsp_max_ns: AtomicU64,
    ttsp_last_ns: AtomicU64,
    /// See [`Self::late_waiter_map_counters`].
    mapgen_composed: AtomicU64,
    mapgen_unresolved: AtomicU64,
    /// The straggler of the slowest pause so far (`ThreadId.0 + 1`; `0` = its
    /// quota was met without a participating arrival) and of the most recent
    /// one. See [`TtspStats::max_straggler`].
    ttsp_max_straggler: AtomicU64,
    ttsp_last_straggler: AtomicU64,
    /// gci i44/sp: [`SafepointStats::count`], bumped by `complete_gc` (under
    /// `inner`) for a pause that was in flight.
    sp_count: AtomicU64,
    /// gci i44/sp: [`SafepointStats::stopped_ns`], added by `complete_gc`.
    sp_stopped_ns: AtomicU64,
    /// zt t30/cv: [`Self::vm_thread_cpu_ns`], added by `complete_gc`.
    vm_thread_cpu_ns: AtomicU64,
    /// gci i44/sp: [`SafepointStats::application_ns`], added by a winning
    /// request (`open_and_raise_locked`).
    sp_application_ns: AtomicU64,
    /// Windows only (gc-common w3-a): an auto-reset event signalled beside
    /// every `all_arrived` notification, so [`Self::wait_for_all_timeout`] can
    /// wait on it together with a high-resolution timer instead of on the
    /// condvar, whose timed wait returns at the next 15.625 ms clock tick
    /// (`jvm_thread::win_park`). `0` when unavailable or
    /// `CRATONVM_WIN_HIRES_PARK=0`; the condvar path is then used as before.
    #[cfg(target_os = "windows")]
    arrival_event: isize,
    /// See [`Self::note_ref_work_queued`].
    ref_work: RefWorkHint,
    /// This barrier's process-unique, never-zero identity in
    /// [`COVERAGE_SLOT_OWNER`]. Not its address: a test may move a
    /// `GcBarrier` between a request and its `complete_gc`.
    slot_id: usize,
    /// This VM's per-pause ledger (gc-common w8-d): the peer JIT coverage
    /// ledger, the pinned-peer depth and the take-over times, which were
    /// process statics in `gc_quiescence`. Bound on the initiator's thread from
    /// a winning request to `complete_gc`, and on a parking peer's thread
    /// around its deposit (`safepoint_check`), so no other VM's pause can read
    /// or write it. See `cratonvm_gc::gc_quiescence::PauseLedger`.
    pause_ledger: Arc<cratonvm_gc::gc_quiescence::PauseLedger>,
    /// The dispatch-loop poll words of the threads that ran an interpreter
    /// loop under this barrier ([`Self::register_loop_poll_word`]): each
    /// counts this barrier's pause while `stw_requested` is up, changed with
    /// the flag under this lock. A lock of its own, never held while `inner`
    /// is taken, so a loop entry never waits on `inner` (lock order: `inner`
    /// -> this). Dead words (their OS thread ended) are pruned at each request
    /// and registration.
    loop_poll_words: Mutex<Vec<Arc<LoopPollWord>>>,
    /// The word a dispatch loop polls when its OS thread's [`LOOP_POLL`] is
    /// already destroyed: its pause count is 1 forever and it is never
    /// registered, so every bytecode takes the slow path, which reads the flag
    /// and the stack's move count itself.
    always_polling: LoopPollWord,
    /// t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`, default ON on Linux and Windows x86-64 since j27): this VM's
    /// poll page pair (`cratonvm_jit::poll_page::map_pair`), `0` with the
    /// switch off or no low mapping. Its stop page is protected for exactly
    /// the span `stw_requested` is up ([`Self::raise_stw_requested`] /
    /// [`Self::lower_stw_requested`]), so every page-form poll of this VM's
    /// code faults into its slow path during a pause. Never unmapped.
    page_poll_base: usize,
    /// Class redefinitions in progress that hold dispatch loops at their top
    /// (interpreter round i1 wave 37, lane L3;
    /// `interpreter::obsolete_frames::RedefinitionFence`): the redefining
    /// thread of each raise. Almost always empty; read under its lock only on
    /// a loop's slow path, after [`Self::redefinition_fences_up`] said some is
    /// up.
    redefinition_fences: Mutex<Vec<ThreadId>>,
    /// `redefinition_fences.len()`, so "none" costs one load.
    redefinition_fences_up: AtomicU32,
    /// Loop tops a fence held back so far (the fence's positive control;
    /// [`Self::note_redefinition_fence_wait`]).
    redefinition_fence_waits: AtomicU64,
    /// gcd d5/f: the JNI leaf-window words of the threads that opened one
    /// under this barrier ([`LeafWindowWord`]). A lock of its own, taken
    /// after `inner` by the drain (lock order: `inner` -> this), never while
    /// a window is open. Dead words are pruned at each registration.
    leaf_window_words: Mutex<Vec<Arc<LeafWindowWord>>>,
    /// Some word was ever registered here: until then a pause request skips
    /// the drain's lock (read after the request's fence, see
    /// [`Self::drain_leaf_windows`]).
    leaf_windows_used: AtomicBool,
    /// gcj j3/rt3 residual 3: the winning request's fast leaf-window drain
    /// (under `inner`) found a window still open; the request finishes the
    /// wait with `inner` released ([`Self::finish_owed_leaf_drain`]) before it
    /// returns. Written and consumed by the winning request's own thread.
    leaf_drain_owed: AtomicBool,
    /// gcg g2/k (`common-a-proposal-lock-free-blocked-transitions-DONE-20261015`):
    /// `CRATONVM_STW_LOCKFREE_BLOCKED`, default ON since the zr triage
    /// (2026-10-12; `=0` is the kill switch), read once per barrier
    /// (per VM) at construction. On: the blocked-region ENTRY transitions
    /// take no `inner` lock (a Dekker store/fence/load against the request,
    /// which then raises `stw_requested` BEFORE its census), and the plain
    /// EXIT transitions take it only when they observe a pause. See
    /// [`Self::enter_blocked`] and [`Self::request_stw_opening_cycle`].
    lockfree_blocked: bool,
    /// gcg g2/k (`common-w21e-proposal-coverage-slot-without-the-wait`):
    /// `CRATONVM_STW_COVERAGE_SLOT_NO_WAIT`, default OFF, read once per
    /// barrier. On: a production request takes the coverage slot with ONE
    /// compare-exchange and never waits for another VM's pause
    /// ([`COVERAGE_SLOT_OWNER`]); a pause that went ahead without it still
    /// adopts it once free (`adopt_free_coverage_slot_locked`).
    coverage_slot_no_wait: bool,
    /// gcg g3/k (`gcg-g2k-proposal-take-the-anonymous-blocked-counter-off-the-transitions`):
    /// `false` under `CRATONVM_STW_NO_BLOCKED_COUNTER` (default OFF, so
    /// `true` by default), read once per barrier. `false`: no blocked-region
    /// transition touches [`Self::threads_blocked`] (it stays 0 and
    /// [`Self::blocked_count`] reads 0). No production decision reads the
    /// counter -- every production request passes the identity census, and
    /// the Dekker pair of the lock-free entry is the deposit's store against
    /// `stw_requested`, not this RMW. Only the `#[cfg(test)]` legacy request
    /// entries (`live_blocked_count == None`) and diagnostics read it.
    /// Constant for the barrier's life, so an increment and its decrement
    /// always agree.
    anon_blocked_counted: bool,
    /// Pauses that published a NON-EMPTY pointer map, i.e. collections that
    /// moved something (interpreter round i2 wave 57, lane L6;
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// Bumped by [`Self::complete_gc`] with [`Self::gc_generation`], which
    /// also counts every pause that moved nothing (a loop-exit, frame-trace or
    /// concurrent-mark pause). A cross-thread reader of another thread's
    /// frames compares it with the generation that thread's frames were last
    /// made current at: only a collection that moved objects can leave them
    /// stale. Written once per pause, read only by a debugger command.
    moving_generation: AtomicU64,
    /// Interpreter round i2 wave 58, lane L6:
    /// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE`, read once per barrier (per VM) at
    /// construction. On: the native-call funnel raises the thread's
    /// `GcBlockState::native_excuse` word, and a handshake pause (frame
    /// trace, loop exit) excuses a counted thread found in a running native
    /// instead of waiting for the native to return
    /// (`docs/feature-designs/jdk-only-mode.md`).
    /// On by default since wave 58 (lane L6); `=0` is the kill switch.
    native_excuse_enabled: bool,
    /// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-5,
    /// `CRATONVM_STW_STALL_REPORT`, default ON): how long a mutator parked at
    /// this barrier waits for its pause before ONE stderr line reports the
    /// pause as stalled (`thread_registry::stw_stall_report_after`). Read once
    /// per barrier (per VM) at construction. `None`: the wait is untimed, as
    /// before.
    stall_report_after: Option<Duration>,
    /// The generation (plus one) of the last pause a parked mutator reported
    /// stalled, so a pause is reported once, not once per waiter.
    stall_reported_gen: AtomicU64,
    /// Interpreter round i2 wave 60, lane L6:
    /// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED`, read once per barrier (per
    /// VM). On (with `native_excuse_enabled`): a native entered under
    /// compiled code is excusable by a frame-trace pause too, which lists its
    /// compiled frames by walking a copy of its JIT entry chain
    /// (`docs/feature-designs/jdk-only-mode.md`).
    /// On by default since wave 60 (lane L6); `=0` is the kill switch.
    native_excuse_compiled_enabled: bool,
    /// Interpreter round i2 wave 61, lane L6:
    /// `CRATONVM_HANDSHAKE_STAND_IN_ROWS`, read once per barrier (per VM). On:
    /// a frame-trace pause that excuses a thread in a registered native
    /// standing in for a Java method (a `--compatible` stand-in such as
    /// `Arrays.fill`) lists that method on top, as HotSpot lists the Java it
    /// runs (`gc_and_alloc::entry_invoked_at`); off (`=0`), the trace has no
    /// row for it, as before.
    excused_stand_in_rows: bool,
    /// Interpreter round i2 wave 62, lane L6:
    /// `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`, read once per barrier (per VM).
    /// On (with `native_excuse_enabled`): the native-call funnel records the
    /// callback its raised native-excuse word is for
    /// (`NativeExcuse::replace_callback`), and a frameless JMX `ThreadInfo`
    /// answers "in native" from it, without the frame-trace pause it took to
    /// read the top row
    /// (`docs/feature-designs/jdk-only-mode.md`).
    /// On by default since wave 62 (lane L6); `=0` is the kill switch.
    excused_native_callback: bool,
    /// Interpreter round i2 wave 64, lane L6
    /// (`CRATONVM_DEBUG_STW_UNPARKS_PARKED`;
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// the OS threads of the debuggees parked at their suspend point on their
    /// suspend word ([`Self::enroll_stw_waker`]), one entry per enrollment.
    /// A winning request unparks each once, right after it raised
    /// `stw_requested` ([`Self::unpark_stw_wakers`]), so such a park need not
    /// wake every millisecond to look at the flag. A lock of its own, taken
    /// after `inner` by the request (lock order: `inner` -> this) and alone by
    /// an enrollment. Empty unless the switch is on and a debugger holds a
    /// thread.
    stw_wakers: Mutex<Vec<std::thread::Thread>>,
    /// Some thread ever enrolled in [`Self::stw_wakers`]: until then a
    /// request skips its lock (one fence and one load; the Dekker pair of
    /// [`Self::enroll_stw_waker`]'s fence).
    stw_wakers_used: AtomicBool,
    /// gci i6/jr: `CRATONVM_STW_RESUME_FAIRNESS`, default OFF, read once per
    /// barrier (per VM) at construction. On: a request first lets the threads
    /// the previous pause released back in (see the section above
    /// [`RESUME_FAIRNESS_GRANT`] and [`Self::resume_fairness_hold`]).
    resume_fairness: bool,
    /// gci i6/jr: the origin of [`Self::released_at_ns`] and
    /// [`Self::last_resume_ns`] (stamps are `elapsed + 1`, `0` = never).
    resume_epoch: Instant,
    /// gci i6/jr: threads inside one of this barrier's generation wait loops
    /// (`arrive_and_wait_inner`, `wait_out_pause_locked`,
    /// `leave_blocked_region_flagged_if`). Changed only under `inner`; read
    /// without it by a holding request. With no pause requested, every one of
    /// them was released by `complete_gc` and has not re-taken `inner` yet.
    waiters_in_wait: AtomicU32,
    /// gci i6/jr: when the last `complete_gc` released its waiters.
    released_at_ns: AtomicU64,
    /// gci i6/jr: when a waiter last left its wait loop, and which OS thread
    /// ([`current_thread_token`]; `0` = unknown).
    last_resume_ns: AtomicU64,
    last_resume_token: AtomicU64,
    /// gci i6/jr: the resume census (`RC_*` slots), printed by
    /// [`Self::resume_census_line`].
    resume_census: [AtomicU64; RC_LEN],
    /// gci i15/hx: `CRATONVM_STW_IN_NATIVE_LEAVE_FAIRNESS`, default ON (`=0`
    /// kills), read once per barrier (per VM) at construction: the in-native
    /// leave rule of the section above [`IN_NATIVE_LEAVE_GRANT`].
    in_native_leave_fairness: bool,
    /// gci i15/hx: threads inside [`Self::leave_in_native_if`] that found a
    /// pause requested and have not finished their leave. Raised under
    /// `inner`, lowered after the leave; read without it by a holding request.
    in_native_leavers: AtomicU32,
    /// gci i15/hx: when such a leave last finished ([`Self::resume_stamp_now`]
    /// units, `0` = never), and on which OS thread ([`current_thread_token`]).
    last_in_native_leave_ns: AtomicU64,
    last_in_native_leave_token: AtomicU64,
    /// gci i18/fh: `CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY`, default ON (`=0`
    /// restores the i15/hx trigger and hold exactly), read once per barrier
    /// (per VM) at construction: see the section above
    /// [`IN_NATIVE_LEAVE_RACE_WINDOW`].
    in_native_leave_race_only: bool,
    /// gci i47w/ex47: `CRATONVM_STW_BACK_TO_BACK_FAIRNESS`, default ON (`=0`
    /// kills), read once per barrier (per VM) at construction: see the section
    /// above [`BACK_TO_BACK_WINDOW`].
    back_to_back_fairness: bool,
    /// gcj j3/rt3: `CRATONVM_GEN_STW_HOLD_SEES_SIBLING_PAUSE`, opt-in, read
    /// once per barrier (per VM) at construction; applied only by
    /// [`Self::request_stw_opening_cycle_for_allocation`] callers that pass it
    /// with a Generational heap.
    hold_sees_sibling_pause: bool,
    /// gcj j3/rt3: see [`Self::hold_sibling_pause_losses`].
    hold_sibling_pause_losses: AtomicU64,
    /// gci i47w/ex47: the OS thread ([`current_thread_token`], `0` = unknown)
    /// that ran the last `complete_gc`, i.e. the last pause's initiator.
    /// Written under `inner` with [`Self::released_at_ns`].
    last_complete_token: AtomicU64,
    /// gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual
    /// 6): `CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT`, default ON (`=0` counts
    /// every pause, as gci i44/sp did), read once per barrier (per VM) at
    /// construction. On: a pause its initiator marks as no HotSpot safepoint
    /// ([`Self::note_pause_not_a_safepoint`]) adds nothing to
    /// [`SafepointStats`].
    sp_hotspot_count: bool,
    /// gci i46/lg: the time-to-safepoint of the pauses
    /// [`Self::note_pause_not_a_safepoint`] marked, which
    /// [`Self::safepoint_stats`] takes out of `sync_ns` (the TTSP sum itself,
    /// [`Self::ttsp_stats`], keeps every pause). Added by `complete_gc`.
    sp_sync_uncounted_ns: AtomicU64,
    /// gci i46/lg (residual 2): per initiating OS thread, the HotSpot VM
    /// operation its frame-trace pauses run as while a
    /// [`FrameTraceOpScope`] is held (`None`: a handshake, no safepoint).
    /// Empty unless a caller entered a scope.
    frame_trace_ops: Mutex<Vec<(std::thread::ThreadId, Option<&'static str>)>>,
    /// gci i46/lg: entries of [`Self::frame_trace_ops`]; `0` lets a lookup
    /// skip the lock.
    frame_trace_ops_used: AtomicU32,
}

/// gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual
/// 2): while held, the calling OS thread's frame-trace pauses are the HotSpot
/// VM operation it names (`None`: HotSpot serves the request by a handshake
/// and runs no safepoint, as for a single thread's `Thread.getStackTrace`;
/// `Some("PrintThreads")`: `jcmd Thread.print` / SIGQUIT's `VM_PrintThreads`).
/// Without a scope a frame-trace pause is `ThreadDump` (`VM_ThreadDump`:
/// `Thread.getAllStackTraces`, `ThreadMXBean` dumps). Nested scopes: the
/// innermost wins. See [`GcBarrier::frame_trace_op_scope`].
pub struct FrameTraceOpScope<'a> {
    barrier: &'a GcBarrier,
    thread: std::thread::ThreadId,
}

impl Drop for FrameTraceOpScope<'_> {
    fn drop(&mut self) {
        let mut ops = self.barrier.frame_trace_ops.lock();
        if let Some(at) = ops.iter().rposition(|(t, _)| *t == self.thread) {
            ops.remove(at);
            self.barrier.frame_trace_ops_used.fetch_sub(1, Ordering::Release);
        }
    }
}

/// An enrollment of the calling OS thread in [`GcBarrier::stw_wakers`]
/// (interpreter round i2 wave 64, lane L6): withdrawn when dropped.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) struct StwWaker<'a> {
    barrier: &'a GcBarrier,
    id: std::thread::ThreadId,
}

impl Drop for StwWaker<'_> {
    fn drop(&mut self) {
        let mut wakers = self.barrier.stw_wakers.lock();
        if let Some(at) = wakers.iter().position(|t| t.id() == self.id) {
            wakers.swap_remove(at);
        }
    }
}

/// The ref-work hint ([`GcBarrier::note_ref_work_queued`]), alone on its cache
/// line.
///
/// `queued` is READ on a hot path (every interpreted allocation) and written
/// rarely: `GcBarrier`'s other fields — `inner`'s mutex word, `threads_blocked`
/// — are written on every arrival and every blocking transition, and sharing
/// their line would turn each such write into a coherence miss for every
/// allocating core. (`gc_generation`, written once per pause, has its own line
/// too since gc-common w5-a: [`PaddedU64`].) `raised_gen` shares the line: it
/// is written only when the hint goes from lowered to raised, and read only
/// inside a pause ([`GcBarrier::ref_work_stale`]).
#[repr(align(64))]
struct RefWorkHint {
    queued: AtomicBool,
    /// `gc_generation` when `queued` last went from `false` to `true`.
    raised_gen: AtomicU64,
}

/// An `AtomicU64` alone on its cache line — [`GcBarrier::gc_generation`].
///
/// Same reasoning as [`RefWorkHint`], for a counter that is read on a hot path
/// (every JIT memo probe) and written once per pause. `Deref` to the atomic so
/// callers keep the ordinary atomic API, and so `&barrier.gc_generation`
/// coerces to `&AtomicU64` where a function takes one.
#[repr(align(64))]
pub struct PaddedU64(AtomicU64);

impl PaddedU64 {
    const fn new(v: u64) -> Self {
        Self(AtomicU64::new(v))
    }
}

impl std::ops::Deref for PaddedU64 {
    type Target = AtomicU64;
    #[inline(always)]
    fn deref(&self) -> &AtomicU64 {
        &self.0
    }
}

struct GcBarrierInner {
    /// Thread that initiated the current STW pause.
    initiator: Option<ThreadId>,
    /// [`current_thread_token`] of the OS thread that made the current
    /// request, `None` outside a pause. Lets the id-less blocked-region leave
    /// paths recognise the initiator (see that function's doc).
    initiator_os: Option<u64>,
    /// When the current request was accepted; `None` outside a pause.
    requested_at: Option<Instant>,
    /// zt t30/cv: the requesting thread's [`current_thread_token`] and CPU
    /// time when the request was accepted ([`vm_op_cpu_share`]); `None`
    /// outside a pause, off Linux and Windows or under `CRATONVM_PERF_VM_THREAD_CPU=0`.
    request_cpu: Option<(u64, u64)>,
    /// Whether this pause's time-to-safepoint has been recorded already
    /// (`wait_for_all_timeout` is polled in a loop).
    ttsp_recorded: bool,
    /// gcg g3/k: nanoseconds the winning production request spent taking the
    /// process coverage slot BEFORE `requested_at` (up to
    /// `COVERAGE_SLOT_WAIT_LIMIT` in a multi-VM process), which no `ttsp_us`
    /// includes. Printed as `slot_wait_us=` on the `[stw-ttsp]` line; 0 for
    /// the test-only request entries.
    slot_wait_ns: u64,
    /// Number of active (non-blocked) threads expected to arrive.
    expected: u32,
    /// Number of threads that have arrived at the barrier.
    arrived: u32,
    /// Pointer map from the last GC, shared with threads for frame updates.
    ///
    /// Behind an `Arc` (gc-common 2026-09-23): every parked thread used to
    /// receive a deep `clone()` of this map, taken INSIDE the barrier lock.
    /// A moving young cycle's map is 154k entries in steady state and 1.4M on
    /// a tenure cycle (`types/src/pointer_map.rs`), so N waking threads paid
    /// N full-map copies one after another under the mutex the next arrival
    /// and the next request also need — O(threads x survivors) of serial
    /// work at the end of every relocating pause, most of it thrown away by
    /// the many callers that bind the result to `_`. Now each waiter takes a
    /// reference count, and the previous map is dropped outside the lock.
    pointer_map: Arc<cratonvm_types::PointerMap>,
    /// `ThreadId.0` of this pause's most recent participating arrival — once
    /// the quota is met, the thread the pause waited for last (its
    /// time-to-safepoint straggler). `None` until one arrives.
    last_arrival: Option<u64>,
    /// gci i20/st: `ThreadId.0` of EVERY participating arrival of this pause,
    /// in arrival order (`note_counted_arrival_locked`), cleared by the next
    /// request (`begin_request_locked`). Diagnostic only: the parked-waiter
    /// stall line prints it, so a stalled pause names the counted thread that
    /// never arrived (the unblocked counted entries of the watchdog's thread
    /// table, minus the initiator, minus these) instead of only the last one
    /// that did (`gci-i19st-g1-virtual-thread-churn-stop-the-world-take-over-stalls`,
    /// i19-st report item 7). One push under the lock every arrival already
    /// holds; the capacity is kept across pauses.
    arrived_ids: Vec<u64>,
    /// How many counted threads this pause excused through
    /// [`GcBarrier::reduce_expected`] (frozen in-JIT peers). Diagnostic.
    takeover_excused: u32,
    /// Whether this barrier's current pause is counted in
    /// [`STW_PAUSES_IN_FLIGHT`], so `complete_gc` releases exactly what the
    /// request took.
    pause_in_flight: bool,
    /// Whether this barrier's current pause holds the process's per-pause GC
    /// coverage state ([`COVERAGE_SLOT_OWNER`]), so `complete_gc` releases
    /// exactly what the request took. `false` for a pause that went ahead
    /// without it (the wait limit expired) and for the test-only request
    /// entries, which never take it.
    holds_coverage_slot: bool,
    /// Whether this barrier's current pause was requested through the
    /// production entry ([`GcBarrier::request_stw_opening_cycle`]) and so
    /// WANTS the coverage slot: a pause that went ahead without it adopts it
    /// the moment it is free (`adopt_free_coverage_slot_locked`, gc-common
    /// w8-d). Cleared by `complete_gc`.
    wants_coverage_slot: bool,
    /// The generation `pointer_map` belongs to — the value `gc_generation`
    /// took when `complete_gc` stored it.
    ///
    /// A waiter releases on "the generation moved past the one I arrived for"
    /// and then clones whatever map is stored. Those are two different facts,
    /// and if they ever disagree the thread applies ANOTHER collection's
    /// relocations to its frames: every slot its own pause moved is left
    /// stale, which is a live object reachable only through an address the
    /// collector vacated. `CRATONVM_DBG_MAPGEN=1` reports the disagreement at
    /// the barrier instead of leaving it to surface as a wrong-class receiver
    /// an unbounded number of collections later.
    map_generation: u64,
    /// GCAUDIT-0711-FIX (finding 1a): identities (`ThreadId.0`) that THIS
    /// pause's `request_stw_counted_with_live_blocked` census read as
    /// `in_blocked_region == true` and therefore excluded from `expected`.
    ///
    /// Whether a given arrival should count toward the barrier's quota is
    /// otherwise UNDECIDABLE at the arrival site: `in_blocked_region`
    /// (`ThreadRegistry`) is set by `deposit_root_snapshot` and read by the
    /// census under a *different* lock (`ThreadRegistry::threads`) than the
    /// one guarding `arrived`/`expected` (`GcBarrier::inner`) — two plain
    /// racy atomics with no ordering relationship to each other. A caller
    /// that infers "was I excluded?" from its own last-known
    /// `in_blocked_region` value (or from `stw_requested` alone) can guess
    /// wrong in either direction: guessing "excluded" when the census
    /// actually counted it hangs `wait_for_all` forever; guessing
    /// "participating" when the census excluded it inflates `arrived` and
    /// releases the initiator while a real counted mutator is still
    /// running — a moving collector then evacuates live, still-mutating
    /// frames (the MTChurn lost-increment / BinaryTrees wrong-total / ES
    /// IVF-KNN Lucene-merge-thread family). Recording the census's actual
    /// per-pause decision here, under the SAME lock every arrival reads it
    /// through (`arrive_and_wait_auto`), makes the check race-free by
    /// construction instead of inferred.
    excluded_blocked: HashSet<u64>,
    /// Interpreter round i2 wave 58, lane L6 (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE`):
    /// identities (`ThreadId.0`) of the counted threads THIS pause's census
    /// excused in a running native ([`StwCensus::excused_running`],
    /// `jvm_thread::NativeExcuse`) and took out of `expected`. An arrival of
    /// one of them in this pause never counts, whichever entry it comes
    /// through (it may reach a poll from inside its native, or block there).
    /// Empty except during a handshake pause that excused someone; cleared by
    /// `complete_gc`.
    excused_running: Vec<u64>,
    /// Waiters currently parked in [`GcBarrier::arrive_and_wait_inner`], as
    /// `(arrival_gen, count)` pairs (gc-common w2-a, 2026-09-23).
    ///
    /// A waiter needs the maps of every pause from `arrival_gen + 1` up to the
    /// generation it wakes into. Normally that is exactly one map (the current
    /// one); it is more only when a LATER pause was requested and completed
    /// before the waiter reacquired this lock — possible for a waiter the later
    /// pause EXCLUDED (a blocked-region arrival, a starting thread). This is
    /// what `complete_gc` consults to decide whether the map it is replacing
    /// must be kept for such a waiter; see [`Self::map_history`].
    ///
    /// Tiny in practice (one entry per distinct generation with a parked
    /// waiter), so a `Vec` with linear search.
    parked_waiters: Vec<(u64, u32)>,
    /// Maps of COMPLETED pauses older than `map_generation` that some parked
    /// waiter still needs, oldest first, as `(generation, map)`.
    ///
    /// Empty in the overwhelmingly common case: `complete_gc` pushes the map it
    /// replaces only when a parked waiter arrived for a generation that map
    /// covers, and prunes every entry no parked waiter needs. So retention is
    /// bounded by the waiters that actually straddle two pauses, and is a
    /// reference count on maps that exist anyway — never a copy. See
    /// `docs/feature-designs/jdk-only-mode.md`.
    map_history: Vec<(u64, Arc<cratonvm_types::PointerMap>)>,
    /// gci i46/lg (`gci-i45rv-waves-i41-i44-review-findings-FIXED-20261016` item 3):
    /// the origin of the next pause's application interval
    /// ([`GcBarrier::resume_stamp_now`] units, `0` = the barrier's creation):
    /// the release instant of the last pause that `complete_gc` COUNTED. It
    /// was `released_at_ns`, which every `complete_gc` writes -- a spurious
    /// one (no pause in flight) included, which dropped the interval before
    /// it from `getTotalApplicationNonStoppedTime`.
    sp_app_since_ns: u64,
    /// gci i46/lg: what this pause's winning request added to
    /// `sp_application_ns` (taken back if the pause is marked as no HotSpot
    /// safepoint, [`GcBarrier::note_pause_not_a_safepoint`]).
    sp_app_added_ns: u64,
    /// gci i46/lg: this pause is no HotSpot safepoint (a handshake, a part of
    /// another operation): `complete_gc` counts nothing for it.
    sp_uncounted: bool,
}

impl GcBarrierInner {
    /// The oldest generation a parked waiter arrived for, if any.
    fn min_parked_arrival_gen(&self) -> Option<u64> {
        self.parked_waiters.iter().map(|&(g, _)| g).min()
    }

    fn note_parked(&mut self, arrival_gen: u64) {
        if let Some(slot) = self
            .parked_waiters
            .iter_mut()
            .find(|(g, _)| *g == arrival_gen)
        {
            slot.1 += 1;
        } else {
            self.parked_waiters.push((arrival_gen, 1));
        }
    }

    fn note_unparked(&mut self, arrival_gen: u64) {
        if let Some(i) = self
            .parked_waiters
            .iter()
            .position(|(g, _)| *g == arrival_gen)
        {
            if self.parked_waiters[i].1 <= 1 {
                self.parked_waiters.swap_remove(i);
            } else {
                self.parked_waiters[i].1 -= 1;
            }
        }
        // A waiter that took its maps may have been the last one needing the
        // older history entries; drop them now rather than at the next pause.
        self.prune_map_history();
    }

    /// Drop every history entry no parked waiter needs: entry `g` is needed by
    /// a waiter that arrived for `a` iff `a < g` (it needs `a+1 ..= current`).
    fn prune_map_history(&mut self) {
        if self.map_history.is_empty() {
            return;
        }
        match self.min_parked_arrival_gen() {
            None => self.map_history.clear(),
            Some(min_arrival) => self.map_history.retain(|(g, _)| *g > min_arrival),
        }
    }

    /// The map(s) a waiter that arrived for `arrival_gen` must apply.
    ///
    /// Called under the lock after the waiter woke, i.e. with
    /// `map_generation >= arrival_gen + 1`. The common answer is the current
    /// map alone and costs one reference count, exactly as before.
    fn maps_for_waiter(&self, arrival_gen: u64) -> WaiterMaps {
        let want_first = arrival_gen + 1;
        if self.map_generation == want_first {
            return WaiterMaps::Own(Arc::clone(&self.pointer_map));
        }
        if self.map_generation < want_first {
            // Cannot happen (the waiter woke because the generation moved).
            return WaiterMaps::Missing;
        }
        let mut out = Vec::with_capacity(
            usize::try_from(self.map_generation - arrival_gen)
                .unwrap_or(usize::MAX)
                .min(64),
        );
        for g in want_first..self.map_generation {
            match self.map_history.iter().find(|(hg, _)| *hg == g) {
                Some((_, m)) => out.push(Arc::clone(m)),
                None => return WaiterMaps::Missing,
            }
        }
        out.push(Arc::clone(&self.pointer_map));
        WaiterMaps::Straddled(out)
    }
}

/// What [`GcBarrierInner::maps_for_waiter`] found.
enum WaiterMaps {
    /// The waiter's own pause is the newest: its map, as always.
    Own(Arc<cratonvm_types::PointerMap>),
    /// The waiter slept through later pauses: every map since its own, oldest
    /// first, to be composed outside the lock.
    Straddled(Vec<Arc<cratonvm_types::PointerMap>>),
    /// A needed map is no longer held (a bug — see the counter).
    Missing,
}

/// How many of the census's `alive` slots belong to the initiator itself.
///
/// * the census also listed it as blocked — the blocked subtraction already
///   removes it (lane A, wave 1);
/// * the caller states it was not counted at all — it owns no slot (w2-a);
/// * otherwise (counted, or unknown) — one slot, the historical arithmetic.
#[inline]
fn initiator_slot(initiator_already_excluded: bool, initiator_counted: Option<bool>) -> u32 {
    if initiator_already_excluded || initiator_counted == Some(false) {
        0
    } else {
        1
    }
}

/// Compose the relocation maps of consecutive pauses, oldest first, into the
/// single map a thread that slept through all of them must apply to an address
/// it captured before the first one.
///
/// Every source address of the first map is chased through each later map in
/// turn; a source of a later map that the earlier maps did not move (a
/// pre-first-pause address of an object only a later pause relocated) is
/// added and chased the same way. An address no map mentions stays unmapped,
/// i.e. unmoved — the same contract as a single map.
///
/// Only reached in the rare straddling case (see
/// `GcBarrierInner::parked_waiters`), outside the barrier lock.
fn compose_pointer_maps(maps: &[Arc<cratonvm_types::PointerMap>]) -> cratonvm_types::PointerMap {
    let mut out = cratonvm_types::PointerMap::default();
    for (i, m) in maps.iter().enumerate() {
        for (&from, &to) in m.iter() {
            // A later map's source that an EARLIER map already moved away from
            // is not a pre-first-pause address of a live object (the earlier
            // pause vacated it), so the earlier entry wins.
            if out.contains_key(&from) {
                continue;
            }
            let mut dest = to;
            for later in &maps[i + 1..] {
                if let Some(&next) = later.get(&dest) {
                    dest = next;
                }
            }
            out.insert(from, dest);
        }
    }
    out
}

/// Monotonic count of stop-the-world pauses STARTED, process-wide.
///
/// A pause parks every mutator, so any counter that only a running mutator
/// advances is frozen for its duration. A sampler that reads such a counter
/// twice and concludes "stalled" is therefore measuring the pause, not the
/// thing it meant to measure -- see
/// `virtual_threads::spawn_starvation_watchdog`, whose carrier-pool growth
/// this exists to keep honest.
///
/// Process-global rather than a `GcBarrier` field because the reader is the
/// virtual-thread starvation watchdog, which is started by
/// `VirtualThreadManager::start_carriers` and has no handle to the barrier --
/// and must not be given one, since the unit tests construct a bare manager
/// with no VM around it.
pub static STW_PAUSE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// How many stop-the-world pauses are in progress RIGHT NOW, process-wide.
/// See [`STW_PAUSE_EPOCH`], which is the one to read across an interval.
///
/// A COUNT, not a flag (gc-common w3-a, 2026-09-23;
/// `docs/feature-designs/jdk-only-mode.md`).
/// It used to be a `bool` that every accepted request set and every
/// `complete_gc` cleared, so with two VMs in one process (the `vm` test binary,
/// an embedder using `libcratonvm`) VM B's `complete_gc` reported "no pause"
/// while VM A was still stopped, and the starvation watchdog grew A's carrier
/// pool against the very pause this exists to discount. Each barrier now adds
/// its own pause once (`GcBarrierInner::pause_in_flight`) and removes exactly
/// that, so "in progress" means "some VM in this process is paused", which is
/// the conservative reading for a sampler that cannot tell VMs apart.
static STW_PAUSES_IN_FLIGHT: AtomicU32 = AtomicU32::new(0);

/// Bump [`STW_PAUSE_EPOCH`] and count a pause in flight. Called once per
/// accepted request, paired with [`note_pause_end`] in `complete_gc`.
fn note_pause_begin() {
    STW_PAUSES_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    STW_PAUSE_EPOCH.fetch_add(1, Ordering::AcqRel);
}

/// Release the in-flight count taken by [`note_pause_begin`].
fn note_pause_end() {
    // Paired by construction: `complete_gc` calls this only when its barrier's
    // `pause_in_flight` was set by `note_pause_begin`'s caller.
    STW_PAUSES_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
}

/// `(epoch, in_progress)` -- the pair a sampler needs to decide whether the
/// interval it just measured contained a pause. `in_progress` is true while
/// any VM in the process is stopped (see [`STW_PAUSES_IN_FLIGHT`]).
pub fn stw_pause_state() -> (u64, bool) {
    (
        STW_PAUSE_EPOCH.load(Ordering::Acquire),
        STW_PAUSES_IN_FLIGHT.load(Ordering::Acquire) > 0,
    )
}

// ---------------------------------------------------------------------------
// The coverage slot: one VM's pause at a time owns the per-pause GC state
// (gc-common w6-a, 2026-09-24)
// ---------------------------------------------------------------------------

/// [`GcBarrier::slot_id`] of the barrier whose pause currently owns the
/// process's per-pause GC coverage state; `0` when no pause does.
///
/// # Why a pause needs to own it
///
/// `cratonvm_gc::gc_quiescence` kept the state of ONE pause -- the moving-young
/// coverage verdict and its reasons, the unrewritable-peer flag, the
/// helper-window pins, the blocked-peer stack-slot and register captures -- in
/// process statics until they moved into the barrier's
/// [`GcBarrier::pause_ledger`] (w8-d to w21-e, see below), and every accepted
/// request CLEARS them (`reset_peer_proven_jit_depth` in
/// `request_stw_counted_locked`, `begin_moving_young_coverage_cycle` as a
/// collection's `open_cycle`). With two VMs in one process (`libcratonvm`
/// embedders; the `vm` unit-test binary, one `SharedVm` per test, in parallel)
/// VM B's request used to clear VM A's pause mid-flight: A's "incomplete"
/// verdict, its take-over verdict and its helper-window pins -- the three
/// inputs that stop a relocation nobody proved safe -- were erased between A's
/// root scan and A's collector reading them
/// (`docs/feature-designs/jdk-only-mode.md`).
///
/// Serialising the pauses that USE the rows was the first step, taken in w6-a
/// before any row could move (the collectors read them through free
/// functions): a request takes this slot BEFORE its barrier lock and the
/// reset, and `complete_gc` releases it, so a reset can only ever clear state
/// no other pause is using -- until the 250 ms give-up below.
///
/// The rows no collector reads are no longer statics at all (gc-common w8-d):
/// the cross-thread proof ledger, the pinned-peer depth and the take-over
/// times live in this barrier's own `PauseLedger` ([`GcBarrier::pause_ledger`]),
/// which no other VM's pause can reach, slot or no slot. So do two rows the
/// collectors read, after a thread audit showed each is written and read only
/// on the pause's requesting thread: the take-over counts (w9-f) and the
/// take-over verdict (w10-g, `gc_quiescence::takeover_verdict`). Then the
/// blocked-peer captures (w18-c), and at gc-common w21-e the moving-young
/// coverage rows themselves -- the verdict and its reasons, the un-rewritable
/// flag, the conservative-scan count and the helper-window pins
/// (`gc_quiescence::CoverageCycle`), whose off-requester writers either bind
/// this ledger or land in the process's orphan rows, which every read includes.
/// Since w21-e, therefore, no row the slot was introduced for is a process
/// static of any VM's pause: another VM's opener can no longer reset any of
/// them. What the slot still serialises is the orphan rows (writes no binding
/// claims, shared exactly as before w21-e). The young pin-word ledger, whose
/// process-wide stamp it also serialised, is per VM since gc-common w36-c (the
/// `young_pins` field of this barrier's `PauseLedger`).
///
/// # What it costs, and why it cannot deadlock
///
/// * One VM: the owner is only ever `0` or this barrier, so a request never
///   waits -- one uncontended compare-exchange per pause, and one on release.
/// * A request of the barrier that already owns the slot (a losing sibling
///   initiator of the pause in flight) does not wait: it is about to lose and
///   arrive.
/// * A request whose OWN barrier is already stopping the world (a pause that
///   went ahead without the slot) stops waiting at once, for the same reason.
/// * Otherwise it waits for the other VM's pause, BOUNDED by
///   [`COVERAGE_SLOT_WAIT_LIMIT`], outside every lock, and without holding
///   anything the other VM's pause can need. On expiry it goes ahead without
///   the slot -- exactly the pre-w6 behaviour -- and counts it
///   ([`COVERAGE_SLOT_TIMEOUTS`], printed under `CRATONVM_DBG_STW_CENSUS`).
///   That bound is what makes a cycle impossible: the one way two pauses could
///   wait on each other is an OS thread that is a counted mutator of VM A while
///   it requests a pause of VM B (an embedder calling one VM from inside
///   another's native without a blocked region), and that thread gives up
///   after the limit and lets A's pause finish.
/// * The owner is an atomic, never a lock: a peer OS-suspended by the take-over
///   can therefore never be holding it.
/// * A hold that a request already waited the full limit for is not waited for
///   again until it is released (gc-common w7-a,
///   [`COVERAGE_SLOT_GAVE_UP_RELEASES`]): a collection that unwinds before its
///   `complete_gc` on a never-dropped `SharedVm` would otherwise cost every
///   other VM 250 ms per pause for the rest of the process.
/// * A request that took the slot and then LOST to a sibling of its own
///   barrier hands the slot to that sibling's pause instead of releasing it
///   (gc-common w7-a, `GcBarrier::settle_unused_slot`).
/// * A production pause that went ahead WITHOUT the slot takes it at its next
///   barrier wait once it is free (gc-common w8-d,
///   `GcBarrier::adopt_free_coverage_slot_locked`), instead of staying
///   slot-less -- and resettable by a third VM -- until its `complete_gc`.
///
/// The test-only request entries (`request_stw`, `request_stw_counted`,
/// `request_stw_counted_with_live_blocked`) do not take it.
static COVERAGE_SLOT_OWNER: AtomicUsize = AtomicUsize::new(0);

/// Source of [`GcBarrier::slot_id`]; starts at 1 so `0` can mean "free".
static NEXT_COVERAGE_SLOT_ID: AtomicUsize = AtomicUsize::new(1);

/// Requests that had to wait for another VM's pause before taking the slot.
/// Diagnostic.
static COVERAGE_SLOT_WAITS: AtomicU64 = AtomicU64::new(0);

/// Requests that gave up waiting and paused without the slot. Diagnostic; see
/// [`COVERAGE_SLOT_OWNER`].
static COVERAGE_SLOT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// Successful releases of [`COVERAGE_SLOT_OWNER`], process-wide. Together
/// with the two `COVERAGE_SLOT_GAVE_UP_*` rows it names ONE hold of the slot:
/// `(owner, releases)` is unchanged exactly while that hold lasts.
static COVERAGE_SLOT_RELEASES: AtomicU64 = AtomicU64::new(0);

/// The hold the last timed-out request gave up on: its owner (`0` = none)...
static COVERAGE_SLOT_GAVE_UP_OWNER: AtomicUsize = AtomicUsize::new(0);

/// ...and [`COVERAGE_SLOT_RELEASES`] at that moment.
///
/// gc-common w7-a. A hold that outlives the wait limit once is treated as
/// abandoned by every later request of another VM until it is released: they
/// go ahead without the slot at once instead of each waiting out
/// [`COVERAGE_SLOT_WAIT_LIMIT`] again. The hold this is for is one that NEVER
/// ends: a collection that unwound between its request and `complete_gc`
/// (`run_collection_pause` has no unwind guard) on a `Vm`-owned `SharedVm`,
/// which is never dropped (the `jcmd_processor` reference cycle in
/// `vm_init.rs`), so `Drop for GcBarrier` never releases it either. Without
/// this, every later pause of every other VM in the process paid 250 ms,
/// forever. A long but live hold loses nothing: the first waiter already went
/// ahead without the slot, and the next release re-arms the wait.
///
/// The two rows are read and written without a common lock. A torn read can
/// only make a request wait when it need not (bounded) or go ahead without the
/// slot during the very hold a request already gave up on (the documented
/// fallback). Either is safe.
static COVERAGE_SLOT_GAVE_UP_RELEASES: AtomicU64 = AtomicU64::new(0);

/// Requests that went ahead without the slot at once because its current hold
/// was already given up on ([`COVERAGE_SLOT_GAVE_UP_RELEASES`]). Diagnostic.
static COVERAGE_SLOT_ABANDONED_SKIPS: AtomicU64 = AtomicU64::new(0);

/// Whether `owner`'s current hold of the slot is one a request already waited
/// the full limit for. See [`COVERAGE_SLOT_GAVE_UP_RELEASES`].
#[inline]
fn coverage_slot_hold_abandoned(owner: usize) -> bool {
    owner != 0
        && COVERAGE_SLOT_GAVE_UP_OWNER.load(Ordering::Acquire) == owner
        && COVERAGE_SLOT_GAVE_UP_RELEASES.load(Ordering::Acquire)
            == COVERAGE_SLOT_RELEASES.load(Ordering::Acquire)
}

/// How long a request waits for ANOTHER VM's pause to release the coverage
/// slot before it pauses without it. Long enough for an ordinary pause of a
/// small heap to finish; short enough that the one pathological wait (see
/// [`COVERAGE_SLOT_OWNER`]) is a hiccup, not a hang.
const COVERAGE_SLOT_WAIT_LIMIT: Duration = Duration::from_millis(250);

impl GcBarrier {
    /// Cooperative JIT safepoint polling (`CRATONVM_JIT_SAFEPOINT_POLLS`) —
    /// stable address of the raw byte backing [`Self::stw_requested`], for
    /// baking into JIT-compiled code as an absolute poll target.
    ///
    /// `AtomicBool` has the same in-memory representation as `bool` (a
    /// single byte, `0` = false / nonzero = true), so a poll can read this
    /// address with a plain non-atomic byte load (`TEST byte ptr [addr],
    /// 0xFF`) and branch on nonzero — no atomic instruction is required at
    /// the poll site. A false-negative race (the poll observes `false` a
    /// few cycles before a concurrent `store(true, Release)` becomes
    /// globally visible) merely defers the thread noticing the request
    /// until its NEXT poll, the same latency bound the interpreter's own
    /// `stw_requested` poll (`vm/src/runtime/interpreter.rs`) already
    /// accepts.
    ///
    /// **Stability contract:** the returned address is valid for the rest of
    /// the process. It does not point into this `GcBarrier`, nor into the
    /// `Arc<SharedVm>` that owns it: [`CacheLineFlag`] holds a `&'static`
    /// into a cell that is never unmapped and never recycled, so a caller may
    /// bake it as an immediate into generated code without tracking the VM's
    /// lifetime at all.
    ///
    /// That is a widening of the old contract ("valid as long as this
    /// `GcBarrier` is alive; must not outlive the `Arc<SharedVm>` it came
    /// from"), which held only because `SharedVm` is `Arc`-allocated once by
    /// `vm/src/vm/vm_init.rs::Vm::new` and never moved thereafter. Under that
    /// contract a compiled poll surviving VM teardown read freed heap; it now
    /// reads a mapped byte that simply stops changing.
    ///
    /// It is not a licence to SHARE the address across VMs — each `GcBarrier`
    /// takes its own cell, and polling another VM's flag would be wrong for
    /// the ordinary reason, not an unsafe one.
    pub fn stw_requested_flag_addr(&self) -> *const u8 {
        self.stw_requested.flag() as *const AtomicBool as *const u8
    }

    /// Create a new GC barrier with no active STW.
    pub fn new() -> Self {
        Self {
            stw_requested: CacheLineFlag::new(false),
            gc_generation: PaddedU64::new(0),
            threads_blocked: AtomicU64::new(0),
            inner: Mutex::new(GcBarrierInner {
                initiator: None,
                initiator_os: None,
                requested_at: None,
                request_cpu: None,
                ttsp_recorded: false,
                slot_wait_ns: 0,
                expected: 0,
                arrived: 0,
                pointer_map: Arc::new(cratonvm_types::PointerMap::default()),
                last_arrival: None,
                arrived_ids: Vec::new(),
                takeover_excused: 0,
                pause_in_flight: false,
                holds_coverage_slot: false,
                wants_coverage_slot: false,
                map_generation: 0,
                excluded_blocked: HashSet::new(),
                excused_running: Vec::new(),
                parked_waiters: Vec::new(),
                map_history: Vec::new(),
                sp_app_since_ns: 0,
                sp_app_added_ns: 0,
                sp_uncounted: false,
            }),
            all_arrived: Condvar::new(),
            gc_complete: Condvar::new(),
            empty_map: Arc::new(cratonvm_types::PointerMap::default()),
            ttsp_pauses: AtomicU64::new(0),
            ttsp_total_ns: AtomicU64::new(0),
            ttsp_max_ns: AtomicU64::new(0),
            ttsp_last_ns: AtomicU64::new(0),
            mapgen_composed: AtomicU64::new(0),
            mapgen_unresolved: AtomicU64::new(0),
            ttsp_max_straggler: AtomicU64::new(0),
            ttsp_last_straggler: AtomicU64::new(0),
            sp_count: AtomicU64::new(0),
            sp_stopped_ns: AtomicU64::new(0),
            vm_thread_cpu_ns: AtomicU64::new(0),
            sp_application_ns: AtomicU64::new(0),
            #[cfg(target_os = "windows")]
            arrival_event: crate::threading::jvm_thread::win_park::create_wake_event(),
            ref_work: RefWorkHint {
                queued: AtomicBool::new(false),
                raised_gen: AtomicU64::new(0),
            },
            slot_id: NEXT_COVERAGE_SLOT_ID.fetch_add(1, Ordering::Relaxed),
            pause_ledger: Arc::new(cratonvm_gc::gc_quiescence::PauseLedger::new()),
            loop_poll_words: Mutex::new(Vec::new()),
            always_polling: LoopPollWord::with_word(1),
            page_poll_base: cratonvm_jit::poll_page::map_pair().unwrap_or(0),
            redefinition_fences: Mutex::new(Vec::new()),
            redefinition_fences_up: AtomicU32::new(0),
            redefinition_fence_waits: AtomicU64::new(0),
            leaf_window_words: Mutex::new(Vec::new()),
            leaf_windows_used: AtomicBool::new(false),
            leaf_drain_owed: AtomicBool::new(false),
            lockfree_blocked: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STW_LOCKFREE_BLOCKED",
            ),
            coverage_slot_no_wait: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_STW_COVERAGE_SLOT_NO_WAIT",
            ),
            anon_blocked_counted: !cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_STW_NO_BLOCKED_COUNTER",
            ),
            moving_generation: AtomicU64::new(0),
            native_excuse_enabled: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_HANDSHAKE_NATIVE_EXCUSE",
            ),
            stall_report_after: crate::threading::thread_registry::stw_stall_report_after(),
            stall_reported_gen: AtomicU64::new(0),
            native_excuse_compiled_enabled: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED",
            ),
            excused_stand_in_rows: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_HANDSHAKE_STAND_IN_ROWS",
            ),
            excused_native_callback: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_HANDSHAKE_NATIVE_CALLBACK",
            ),
            stw_wakers: Mutex::new(Vec::new()),
            stw_wakers_used: AtomicBool::new(false),
            resume_fairness: cratonvm_types::flags::runtime_flag_on("CRATONVM_STW_RESUME_FAIRNESS"),
            resume_epoch: Instant::now(),
            waiters_in_wait: AtomicU32::new(0),
            released_at_ns: AtomicU64::new(0),
            last_resume_ns: AtomicU64::new(0),
            last_resume_token: AtomicU64::new(0),
            resume_census: std::array::from_fn(|_| AtomicU64::new(0)),
            in_native_leave_fairness: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STW_IN_NATIVE_LEAVE_FAIRNESS",
            ),
            in_native_leavers: AtomicU32::new(0),
            last_in_native_leave_ns: AtomicU64::new(0),
            last_in_native_leave_token: AtomicU64::new(0),
            in_native_leave_race_only: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY",
            ),
            back_to_back_fairness: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STW_BACK_TO_BACK_FAIRNESS",
            ),
            hold_sees_sibling_pause: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_GEN_STW_HOLD_SEES_SIBLING_PAUSE",
            ),
            hold_sibling_pause_losses: AtomicU64::new(0),
            last_complete_token: AtomicU64::new(0),
            sp_hotspot_count: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT",
            ),
            sp_sync_uncounted_ns: AtomicU64::new(0),
            frame_trace_ops: Mutex::new(Vec::new()),
            frame_trace_ops_used: AtomicU32::new(0),
        }
    }

    /// Enroll the calling OS thread to be unparked by every stop-the-world
    /// request of this barrier until the returned guard drops (interpreter
    /// round i2 wave 64, lane L6; `CRATONVM_DEBUG_STW_UNPARKS_PARKED`). For a
    /// thread that waits on its own parker (`std::thread::park*`) and must
    /// answer a pause: after this returns, a request either finds the entry
    /// and unparks it, or raised its flag before the caller's next
    /// `stw_requested` load, which then sees it. The caller loads the flag
    /// after enrolling, before its first park.
    #[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
    pub(crate) fn enroll_stw_waker(&self) -> StwWaker<'_> {
        self.stw_wakers_used.store(true, Ordering::SeqCst);
        // The other half of `unpark_stw_wakers`' fence: a request whose
        // `stw_wakers_used` load misses the store above raised its flag
        // before this fence, so the caller's later flag load sees it.
        std::sync::atomic::fence(Ordering::SeqCst);
        let current = std::thread::current();
        let id = current.id();
        self.stw_wakers.lock().push(current);
        StwWaker { barrier: self, id }
    }

    /// OS threads enrolled by [`Self::enroll_stw_waker`] now (with
    /// repeats: one per live enrollment).
    #[cfg(test)]
    pub(crate) fn stw_wakers_enrolled(&self) -> usize {
        self.stw_wakers.lock().len()
    }

    /// Unpark every thread of [`Self::stw_wakers`] once (wave 64, lane L6).
    /// Called by a winning request right after it raised `stw_requested`,
    /// under `inner`; one fence and one load while no thread ever enrolled.
    /// An entry seen under the lock was pushed before the lock was taken, or
    /// its enrollment's lock came after this one released and its flag load
    /// sees the raise: either way the enrolled thread answers the pause.
    fn unpark_stw_wakers(&self) {
        std::sync::atomic::fence(Ordering::SeqCst);
        if !self.stw_wakers_used.load(Ordering::Acquire) {
            return;
        }
        for waker in self.stw_wakers.lock().iter() {
            waker.unpark();
        }
    }

    /// `CRATONVM_HANDSHAKE_NATIVE_CALLBACK` for this VM (interpreter round i2
    /// wave 62, lane L6): does the funnel record the callback of the native
    /// its excuse word is raised for? One load of a field fixed at
    /// construction.
    #[inline(always)]
    pub fn excused_native_callback(&self) -> bool {
        self.excused_native_callback
    }

    /// `CRATONVM_HANDSHAKE_STAND_IN_ROWS` for this VM (interpreter round i2
    /// wave 61, lane L6): does an excused thread's trace list the Java method
    /// a registered native stands in for? Asked by the pause's cold reader.
    #[inline]
    pub fn excused_stand_in_rows(&self) -> bool {
        self.excused_stand_in_rows
    }

    /// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED` for this VM (interpreter
    /// round i2 wave 60, lane L6): may a frame-trace pause excuse, and list,
    /// a thread running a native entered under compiled code?
    #[inline]
    pub fn native_excuse_compiled_enabled(&self) -> bool {
        self.native_excuse_compiled_enabled
    }

    /// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE` for this VM (interpreter round i2
    /// wave 58, lane L6): may a handshake pause excuse a thread running a
    /// native? One load of a field fixed at construction.
    #[inline(always)]
    pub fn native_excuse_enabled(&self) -> bool {
        self.native_excuse_enabled
    }

    /// How many pauses so far published a non-empty pointer map (moved
    /// objects): see the field's doc. Acquire: a reader that sees a value
    /// sees the pause's remaps of the frames it rewrote in place.
    #[inline]
    pub fn moving_generation(&self) -> u64 {
        self.moving_generation.load(Ordering::Acquire)
    }

    /// A barrier with the anonymous blocked counter forced on (`true`, the
    /// default) or off, whatever `CRATONVM_STW_NO_BLOCKED_COUNTER` says. Unit
    /// tests only.
    #[cfg(test)]
    pub(crate) fn with_anon_blocked_counted(on: bool) -> Self {
        let mut b = Self::new();
        b.anon_blocked_counted = on;
        b
    }

    /// Count the caller into [`Self::threads_blocked`] (a blocked-region
    /// entry), unless the counter is off ([`Self::anon_blocked_counted`]).
    #[inline]
    fn anon_blocked_in(&self) {
        if self.anon_blocked_counted {
            self.threads_blocked.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Count the caller out of [`Self::threads_blocked`] (a blocked-region
    /// exit), unless the counter is off.
    #[inline]
    fn anon_blocked_out(&self) {
        if self.anon_blocked_counted {
            self.threads_blocked.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// A barrier with the lock-free blocked transitions forced on or off,
    /// whatever `CRATONVM_STW_LOCKFREE_BLOCKED` says. Unit tests only.
    #[cfg(test)]
    pub(crate) fn with_lockfree_blocked(on: bool) -> Self {
        let mut b = Self::new();
        b.lockfree_blocked = on;
        b
    }

    /// A barrier with the no-wait coverage slot forced on or off, whatever
    /// `CRATONVM_STW_COVERAGE_SLOT_NO_WAIT` says. Unit tests only.
    #[cfg(test)]
    pub(crate) fn with_coverage_slot_no_wait(on: bool) -> Self {
        let mut b = Self::new();
        b.coverage_slot_no_wait = on;
        b
    }

    /// A barrier with the resume fairness rule forced on or off, whatever
    /// `CRATONVM_STW_RESUME_FAIRNESS` says (gci i6/jr). Unit tests only.
    #[cfg(test)]
    pub(crate) fn with_resume_fairness(on: bool) -> Self {
        let mut b = Self::new();
        b.resume_fairness = on;
        // gci i47w/ex47: the i6/jr tests pin the general rule alone.
        b.back_to_back_fairness = false;
        b
    }

    /// A barrier with the general and in-native rules OFF and the gci
    /// i47w/ex47 back-to-back initiator rule forced on or off, whatever
    /// `CRATONVM_STW_BACK_TO_BACK_FAIRNESS` says. Unit tests only.
    #[cfg(test)]
    pub(crate) fn with_back_to_back_fairness(on: bool) -> Self {
        let mut b = Self::new();
        b.resume_fairness = false;
        b.in_native_leave_fairness = false;
        b.back_to_back_fairness = on;
        b
    }

    /// A barrier with the general resume fairness rule OFF and the in-native
    /// leave rule forced on or off, whatever
    /// `CRATONVM_STW_IN_NATIVE_LEAVE_FAIRNESS` says (gci i15/hx). Unit tests
    /// only.
    #[cfg(test)]
    pub(crate) fn with_in_native_leave_fairness(on: bool) -> Self {
        let mut b = Self::new();
        b.resume_fairness = false;
        b.in_native_leave_fairness = on;
        // gci i47w/ex47: the i15/hx and i18/fh tests pin the in-native rule alone.
        b.back_to_back_fairness = false;
        b
    }

    /// [`Self::with_in_native_leave_fairness`]`(true)` with the gci i18/fh
    /// race-only trigger forced on or off, whatever
    /// `CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY` says. Unit tests only.
    #[cfg(test)]
    pub(crate) fn with_in_native_leave_race_only(on: bool) -> Self {
        let mut b = Self::with_in_native_leave_fairness(true);
        b.in_native_leave_race_only = on;
        b
    }

    /// gci i15/hx: an in-native leave found a pause requested and is about to
    /// wait it out. Under `inner`.
    #[inline]
    fn note_in_native_leave_wait(&self) {
        self.in_native_leavers.fetch_add(1, Ordering::AcqRel);
    }

    /// gci i15/hx: that leave is done (whichever way it left the flag): the
    /// grant of [`IN_NATIVE_LEAVE_GRANT`] starts now. Stamp first, then the
    /// count, so a holding request never sees neither.
    #[inline]
    fn note_in_native_leave_done(&self) {
        // gcj j2/cm2 (j1-cm registry residuals item 9): the stamp and the token
        // are two stores, not one word, so a holding request that reads between
        // them pairs the new stamp with the previous leaver's token -- at most
        // once per leave, and the hold it starts is bounded
        // (`RESUME_FAIRNESS_MAX_HOLD`). Accepted: it only delays a request.
        self.last_in_native_leave_ns
            .store(self.resume_stamp_now(), Ordering::Release);
        self.last_in_native_leave_token
            .store(current_thread_token().unwrap_or(0), Ordering::Release);
        self.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
    }

    /// gci i15/hx: does the in-native leave rule hold a request made by the
    /// OS thread `me` now? See the section above [`IN_NATIVE_LEAVE_GRANT`].
    #[inline]
    fn in_native_leave_should_hold(&self, me: u64) -> bool {
        self.in_native_leave_rule_within(me, IN_NATIVE_LEAVE_GRANT)
    }

    /// gci i18/fh: does a request made by the OS thread `me` now RACE an
    /// in-native leaver -- the leaver is not out yet, or came out less than
    /// [`IN_NATIVE_LEAVE_RACE_WINDOW`] ago? Only such a request starts the
    /// hold (see the section above that constant). With
    /// `CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY=0`, i15/hx's trigger: anywhere
    /// in the grant.
    #[inline]
    fn in_native_leave_raced(&self, me: u64) -> bool {
        let window = if self.in_native_leave_race_only {
            IN_NATIVE_LEAVE_RACE_WINDOW
        } else {
            IN_NATIVE_LEAVE_GRANT
        };
        self.in_native_leave_rule_within(me, window)
    }

    /// The in-native leave rule with the grant `window`: a leaver not out
    /// yet, or one (not `me`) that came out less than `window` ago.
    #[inline]
    fn in_native_leave_rule_within(&self, me: u64, window: Duration) -> bool {
        self.in_native_leave_fairness
            && resume_fairness_should_hold(
                self.in_native_leavers.load(Ordering::Acquire),
                self.resume_stamp_now(),
                self.last_in_native_leave_ns.load(Ordering::Acquire),
                duration_ns(window),
                self.last_in_native_leave_token.load(Ordering::Acquire) == me,
            )
    }

    /// i6/jr's general rule for a request made by the OS thread `me` now,
    /// whatever `CRATONVM_STW_RESUME_FAIRNESS` says: a waiter the previous
    /// pause released has not re-taken `inner`, or another thread resumed less
    /// than [`RESUME_FAIRNESS_GRANT`] ago.
    #[inline]
    fn general_rule_now(&self, me: u64) -> bool {
        resume_fairness_should_hold(
            self.waiters_in_wait.load(Ordering::Acquire),
            self.resume_stamp_now(),
            self.last_resume_ns.load(Ordering::Acquire),
            duration_ns(RESUME_FAIRNESS_GRANT),
            self.last_resume_token.load(Ordering::Acquire) == me,
        )
    }

    /// gci i47w/ex47: is a request made by the OS thread `me` now a
    /// back-to-back request that the general rule would hold
    /// (`CRATONVM_STW_BACK_TO_BACK_FAIRNESS`)? See the section above
    /// [`BACK_TO_BACK_WINDOW`].
    #[inline]
    fn back_to_back_raced(&self, me: u64) -> bool {
        if !self.back_to_back_fairness {
            return false;
        }
        let released = self.released_at_ns.load(Ordering::Acquire);
        back_to_back_should_start(
            released,
            self.resume_stamp_now(),
            duration_ns(BACK_TO_BACK_WINDOW),
            me != 0 && self.last_complete_token.load(Ordering::Acquire) == me,
        ) && self.general_rule_now(me)
    }

    /// gci i6/jr: nanoseconds since [`Self::resume_epoch`], plus one (so `0`
    /// stays "never").
    #[inline]
    fn resume_stamp_now(&self) -> u64 {
        duration_ns(self.resume_epoch.elapsed()).saturating_add(1)
    }

    /// gci i6/jr: the caller is about to wait in a generation loop. Under
    /// `inner`.
    #[inline]
    fn note_wait_begin(&self) {
        self.waiters_in_wait.fetch_add(1, Ordering::AcqRel);
    }

    /// gci i6/jr: the caller left a generation loop it entered for pause
    /// `arrival_gen + 1` (the generation moved). Under `inner`. Records the
    /// release -> resume latency, the thread that resumed last, and a straddle
    /// (a whole later pause completed before the caller re-took the lock).
    fn note_wait_end(&self, tid: Option<u64>, arrival_gen: u64) {
        self.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
        // gcj j2/cm2 (item 9): stamp and token in two stores, as in
        // `note_in_native_leave_done` -- a reader between them pairs this stamp
        // with the previous waiter's token, once, for one bounded hold.
        let now = self.resume_stamp_now();
        self.last_resume_ns.store(now, Ordering::Release);
        self.last_resume_token.store(current_thread_token().unwrap_or(0), Ordering::Release);
        let released = self.released_at_ns.load(Ordering::Acquire);
        let c = &self.resume_census;
        if released != 0 && now >= released {
            let lat = now - released;
            c[RC_RESUMES].fetch_add(1, Ordering::Relaxed);
            c[RC_RESUME_NS].fetch_add(lat, Ordering::Relaxed);
            // Written only under `inner`, so the max and its thread pair up
            // (`<=`: a tie, and a first latency of 0 ns, name their thread too).
            if c[RC_RESUME_MAX_NS].fetch_max(lat, Ordering::Relaxed) <= lat {
                c[RC_RESUME_MAX_TID].store(encode_straggler(tid), Ordering::Relaxed);
            }
        }
        if self.gc_generation.load(Ordering::Acquire) > arrival_gen.saturating_add(1) {
            c[RC_STRADDLES].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// gci i6/jr: the fairness hold of a request, called under `inner` right
    /// after the request saw no pause requested, and before its census.
    ///
    /// Always counts `starved_requests` (a waiter released by the previous
    /// pause has not resumed yet). With the switch on and
    /// [`resume_fairness_should_hold`] true, releases `inner` and yields until
    /// the rule is satisfied or [`RESUME_FAIRNESS_MAX_HOLD`] passed, and
    /// returns `true`: the lock was released, so the caller must ask again
    /// whether a pause is requested before it requests one.
    ///
    /// gci i15/hx: the in-native leave rule ([`Self::in_native_leave_should_hold`],
    /// default ON) holds the same way, with the same bound and census slots,
    /// whatever the general switch says.
    ///
    /// gci i18/fh (`CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY`, default ON): that
    /// rule starts a hold only for a request that raced the leaver
    /// ([`Self::in_native_leave_raced`]), and any hold ends when another
    /// thread's pause is requested (the caller then loses its request).
    ///
    /// Sound by construction: it only DELAYS a request. The waiters it waits
    /// for need nothing but `inner` to leave their loops (`gc_complete.wait`
    /// re-takes it, and the code after each loop runs under it), and the hold
    /// is bounded, so a waiter the OS does not schedule costs one bounded
    /// delay, never a hang. Nothing of the coming pause exists yet (no flag,
    /// no census, no coverage reset), so a thread that runs meanwhile runs
    /// outside any pause, as it would have before the request.
    fn resume_fairness_hold(
        &self,
        inner: &mut parking_lot::MutexGuard<'_, GcBarrierInner>,
    ) -> bool {
        let waiting = self.waiters_in_wait.load(Ordering::Acquire);
        if waiting > 0 {
            self.resume_census[RC_STARVED_REQUESTS].fetch_add(1, Ordering::Relaxed);
        }
        // gci i15/hx: the in-native leave rule (default ON) holds on its own
        // counters; the general rule only with its opt-in switch.
        // gci i47w/ex47: and the back-to-back initiator rule (default ON).
        if !self.resume_fairness && !self.in_native_leave_fairness && !self.back_to_back_fairness {
            return false;
        }
        let me = current_thread_token().unwrap_or(u64::MAX);
        let general_hold = || self.resume_fairness && self.general_rule_now(me);
        // gci i47w/ex47: a back-to-back request by the previous pause's
        // initiator starts i6/jr's hold whatever its switch says; the hold
        // then runs on the general rule and yields to another thread's pause.
        let back_to_back = self.back_to_back_raced(me);
        let race_only = self.in_native_leave_race_only;
        // gci i18/fh: the in-native leave rule STARTS a hold only for a
        // request that raced the leaver (`in_native_leave_raced`); a hold it
        // started runs through the leaver's whole grant, as i15/hx's. With the
        // switch off, `in_native_leave_raced` is i15/hx's trigger and every
        // hold also asks the in-native rule, exactly as before.
        let in_native = self.in_native_leave_raced(me);
        // gci i19/rv finding 5: ask the general rule first, so a request it
        // holds is not also counted as an in-native skip.
        let general = general_hold();
        if race_only && !in_native && !general && self.in_native_leave_should_hold(me) {
            // Inside a leaver's grant but no race: i15/hx held this request.
            self.resume_census[RC_IN_NATIVE_SKIPS].fetch_add(1, Ordering::Relaxed);
        }
        if !in_native && !general && !back_to_back {
            return false;
        }
        let in_native_continues = in_native || !race_only;
        let should_hold = || {
            general_hold()
                || (back_to_back && self.general_rule_now(me))
                || (in_native_continues && self.in_native_leave_should_hold(me))
        };
        let t0 = Instant::now();
        let deadline = t0 + RESUME_FAIRNESS_MAX_HOLD;
        // `(timed_out, yielded)`.
        let (timed_out, yielded) = parking_lot::MutexGuard::unlocked(inner, || loop {
            // gci i18/fh: another thread's pause was requested meanwhile. This
            // request loses it anyway, and that pause counts the holder, which
            // does not poll here: stop holding it up. gci i47w/ex47: a
            // back-to-back hold yields the same way.
            if (race_only || back_to_back) && self.stw_requested.load(Ordering::Acquire) {
                break (false, true);
            }
            if !should_hold() {
                break (false, false);
            }
            if Instant::now() >= deadline {
                break (true, false);
            }
            std::thread::yield_now();
        });
        let held = duration_ns(t0.elapsed());
        let c = &self.resume_census;
        c[RC_HOLDS].fetch_add(1, Ordering::Relaxed);
        c[RC_HOLD_NS].fetch_add(held, Ordering::Relaxed);
        c[RC_HOLD_MAX_NS].fetch_max(held, Ordering::Relaxed);
        if in_native {
            c[RC_IN_NATIVE_HOLDS].fetch_add(1, Ordering::Relaxed);
        }
        if back_to_back {
            c[RC_BACK_TO_BACK_HOLDS].fetch_add(1, Ordering::Relaxed);
        }
        if timed_out {
            c[RC_HOLD_TIMEOUTS].fetch_add(1, Ordering::Relaxed);
        }
        if yielded {
            c[RC_HOLD_YIELDED].fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// gci i6/jr: the resume census, slot by slot (`RC_*`). Tests and the
    /// exit line.
    pub(crate) fn resume_census(&self) -> [u64; RC_LEN] {
        std::array::from_fn(|i| self.resume_census[i].load(Ordering::Relaxed))
    }

    /// gci i6/jr: `[GC] stw_resume:` for the exit summary (beside `[GC] ttsp:`).
    ///
    /// * `resumes`, `resume_avg_us`, `resume_max_us`, `resume_max_tid`: waiters
    ///   that left a generation wait loop after a `complete_gc`, and the time
    ///   from that release to their leaving (re-taking the barrier lock);
    ///   `resume_max_tid` is the registry id of the slowest (`none`: a
    ///   blocked-region leave given no id);
    /// * `straddles`: waiters that slept through a whole later pause;
    /// * `leave_repeats`: extra pauses a blocked-region leave waited out in one
    ///   leave (`leave_blocked_region_flagged_if`'s loop came round again);
    /// * `starved_requests`: requests that found a waiter released by the
    ///   previous pause still not resumed -- the starvation itself;
    /// * `holds`, `hold_avg_us`, `hold_max_us`, `hold_timeouts`: the switch's
    ///   engagement (0 with it off), and since gci i15/hx the in-native leave
    ///   rule's (`CRATONVM_STW_IN_NATIVE_LEAVE_FAIRNESS`, default ON), which
    ///   holds through the same slots;
    /// * gci i18/fh: `in_native_holds`, the holds the in-native leave rule
    ///   started; `in_native_skips`, requests inside a leaver's grant that did
    ///   not race it and were not held (i15/hx held them; 0 with
    ///   `CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY=0`); `hold_yielded`, holds
    ///   ended by another thread's pause request.
    /// * gci i47w/ex47: `back_to_back_holds`, the holds the back-to-back
    ///   initiator rule started (`CRATONVM_STW_BACK_TO_BACK_FAIRNESS`).
    pub fn resume_census_line(&self) -> String {
        let c = self.resume_census();
        let avg = |sum: u64, n: u64| sum / n.max(1) / 1_000;
        format!(
            "[GC] stw_resume: resume_fairness={} resumes={} resume_avg_us={} resume_max_us={} \
             resume_max_tid={} straddles={} leave_repeats={} starved_requests={} holds={} \
             hold_avg_us={} hold_max_us={} hold_timeouts={} in_native_holds={} \
             in_native_skips={} hold_yielded={} back_to_back_holds={}",
            if self.resume_fairness { "on" } else { "off" },
            c[RC_RESUMES],
            avg(c[RC_RESUME_NS], c[RC_RESUMES]),
            c[RC_RESUME_MAX_NS] / 1_000,
            decode_straggler(c[RC_RESUME_MAX_TID])
                .map_or_else(|| "none".to_string(), |t| t.to_string()),
            c[RC_STRADDLES],
            c[RC_LEAVE_REPEATS],
            c[RC_STARVED_REQUESTS],
            c[RC_HOLDS],
            avg(c[RC_HOLD_NS], c[RC_HOLDS]),
            c[RC_HOLD_MAX_NS] / 1_000,
            c[RC_HOLD_TIMEOUTS],
            c[RC_IN_NATIVE_HOLDS],
            c[RC_IN_NATIVE_SKIPS],
            c[RC_HOLD_YIELDED],
            c[RC_BACK_TO_BACK_HOLDS],
        )
    }

    /// gcd d5/f: open a JNI leaf window ([`LeafWindowWord`]) on the calling
    /// thread, whose `word` it is. `true`: open, and no pause of this barrier
    /// can begin collecting until [`LeafWindowWord::close`]; `false`: a pause
    /// is up (the window is not open) and the caller must take the full
    /// transition instead.
    ///
    /// The caller must be EXCLUDED from this barrier's pauses (its
    /// `in_blocked_region` up) and must, while the window is open, neither
    /// take this barrier's locks nor block, allocate or run Java.
    pub(crate) fn try_open_leaf_window(&self, word: &Arc<LeafWindowWord>) -> bool {
        if word.registered_with.load(Ordering::Relaxed) != self.slot_id {
            self.register_leaf_window_word(word);
        }
        word.open.store(true, Ordering::Relaxed);
        // Store-load (Dekker) against `drain_leaf_windows`: this thread
        // stores `open` then loads `stw_requested`; a request stores
        // `stw_requested` then loads `open`, each with a SeqCst fence in
        // between, so at least one of them sees the other's store.
        std::sync::atomic::fence(Ordering::SeqCst);
        // `Acquire`: a pause that ended just before this read (its
        // `complete_gc` lowered the flag with `Release` after advancing
        // `gc_generation`) is visible to the caller's generation check.
        if self.stw_requested.load(Ordering::Acquire) {
            word.close();
            return false;
        }
        true
    }

    /// Register a leaf-window word with this barrier (once per thread and
    /// barrier; again only after the thread used another barrier).
    fn register_leaf_window_word(&self, word: &Arc<LeafWindowWord>) {
        let mut words = self.leaf_window_words.lock();
        words.retain(|w| !w.owner_dropped());
        word.registered_with.store(self.slot_id, Ordering::Relaxed);
        if !words.iter().any(|w| Arc::ptr_eq(w, word)) {
            words.push(Arc::clone(word));
        }
        self.leaf_windows_used.store(true, Ordering::Release);
    }

    /// Wait, briefly, until no registered leaf window is open. Called by a
    /// WINNING request right after it raised `stw_requested`, under `inner`:
    /// from here no window can open (see [`Self::try_open_leaf_window`]), and
    /// one that is open closes within the few loads and stores of a leaf JNI
    /// function. The waited-for thread takes none of this barrier's locks
    /// while its window is open, so waiting under `inner` cannot deadlock.
    ///
    /// gcj j3/rt3 residual 3: only the bounded spin / yield phase runs under
    /// `inner`. A window still open after it (a debugger-suspended thread
    /// inside a leaf JNIEnv function, a preempted one) is OWED
    /// (`leaf_drain_owed`): the request publishes the rest of its pause under
    /// the same hold (census, `expected`, excused set, coverage slot, pause in
    /// flight), drops `inner`, and only then waits for it
    /// ([`Self::finish_owed_leaf_drain`]) -- still before it returns, so no
    /// collection starts with a window open. In that gap every path that
    /// takes `inner` (an arrival, a blocked-region leave, a losing request)
    /// sees the same, fully published pause it would see after the request
    /// returned; nothing about the pause is half-built, so no "draining"
    /// state is needed. Before, every mutator's park and wake queued on
    /// `inner` behind the wait for as long as the window stayed open.
    ///
    /// Inert (one fence and one load) unless some thread of this VM ever
    /// opened a window, i.e. unless `CRATONVM_JNI_NATIVE_TRANSITIONS` is on
    /// and a native in native called a leaf JNIEnv function.
    fn drain_leaf_windows(&self) {
        // The other half of `try_open_leaf_window`'s fence. Also orders the
        // `leaf_windows_used` read after the flag store: a thread whose
        // registration this read misses has not fenced yet, so its own
        // `stw_requested` load sees the raised flag and it backs off.
        std::sync::atomic::fence(Ordering::SeqCst);
        if !self.leaf_windows_used.load(Ordering::Acquire) {
            return;
        }
        let words = self.leaf_window_words.lock();
        let mut rounds: u32 = 0;
        for word in words.iter() {
            while word.open.load(Ordering::Acquire) {
                rounds = rounds.saturating_add(1);
                if rounds < LEAF_DRAIN_SPINS {
                    std::hint::spin_loop();
                } else if rounds < LEAF_DRAIN_LOCKED_ROUNDS {
                    std::thread::yield_now();
                } else {
                    self.leaf_drain_owed.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
    }

    /// gcj j3/rt3 residual 3: the rest of [`Self::drain_leaf_windows`], by a
    /// winning request with `inner` RELEASED, before it returns. A no-op (one
    /// load) unless that drain left a window open.
    fn finish_owed_leaf_drain(&self) {
        if !self.leaf_drain_owed.swap(false, Ordering::Relaxed) {
            return;
        }
        // The windows still open, waited for without holding the words' lock
        // either (a thread registering its word meanwhile is refused by the
        // raised flag and opens nothing).
        let open: Vec<Arc<LeafWindowWord>> = self
            .leaf_window_words
            .lock()
            .iter()
            .filter(|w| w.open.load(Ordering::Acquire))
            .cloned()
            .collect();
        let started = Instant::now();
        // gcj j3/rt3 (residuals page item 4): the first report at 5 s, then
        // one each time the wait doubles (10, 20, 40 s, ...), with the time
        // waited. Giving up would be unsound (the window's thread may hold raw
        // references), so the wait itself stays unbounded.
        let mut next_report = Duration::from_secs(5);
        let mut warned = false;
        for word in &open {
            let mut rounds: u32 = 0;
            while word.open.load(Ordering::Acquire) {
                rounds = rounds.saturating_add(1);
                if rounds < 64 {
                    std::thread::yield_now();
                    continue;
                }
                std::thread::sleep(Duration::from_micros(50));
                let waited = started.elapsed();
                if waited >= next_report {
                    next_report = next_report.saturating_mul(2);
                    // gci i15/hx: also on stderr. The `[stw-stall]` report is
                    // written by the take-over wait, after this returns, so a
                    // run without a tracing subscriber showed nothing at all
                    // for a hang in this loop.
                    eprintln!(
                        "[stw-leaf-drain] barrier={} a stop-the-world request has waited \
                         {} s for a JNI leaf window to close (a leaf JNIEnv function is \
                         blocked)",
                        self.slot_id,
                        waited.as_secs()
                    );
                }
                if !warned && waited >= Duration::from_secs(5) {
                    warned = true;
                    tracing::warn!(
                        barrier = self.slot_id,
                        "stop-the-world request has waited 5 s for a JNI leaf window to \
                         close; a leaf JNIEnv function is blocked (a lock the pausing \
                         thread holds?)"
                    );
                }
            }
        }
    }

    /// The poll word a dispatch loop on this OS thread reads before every
    /// bytecode: the thread's [`LOOP_POLL`] word, registered with this
    /// barrier first unless it already is (one compare), or -- when the
    /// thread-local is already destroyed -- this barrier's always-raised
    /// word.
    ///
    /// The pointer is valid for as long as the calling dispatch loop runs: the
    /// thread-local lives until its OS thread's thread-local destructors run,
    /// which cannot begin while a loop is running on that thread (a
    /// destructor that runs Java finds it destroyed and gets the barrier's
    /// word), and the barrier outlives every loop of its VM.
    pub(crate) fn loop_poll_word_for_this_thread(&self) -> *const LoopPollWord {
        let this_thread = LOOP_POLL.try_with(|h| {
            let word = h.word();
            if word.registered_with() != self.slot_id {
                self.register_loop_poll_word(word);
            }
            Arc::as_ptr(word)
        });
        match this_thread {
            Ok(word) => word,
            Err(_) => &self.always_polling as *const LoopPollWord,
        }
    }

    /// Register a dispatch loop's poll word, so this barrier's pauses are
    /// counted in it ([`LoopPollWord`]). Called at a loop entry whose word was
    /// last registered elsewhere or never (one compare otherwise), so a
    /// thread's first loop registers it and later ones do not.
    ///
    /// Every change of `stw_requested` happens under `loop_poll_words`' lock
    /// together with the count it implies ([`Self::raise_stw_requested`],
    /// [`Self::lower_stw_requested`]), and so does this: a word joining the
    /// list while a pause is up is counted for it at once, exactly once (a
    /// word already listed was counted when the pause began), and every
    /// listed word is uncounted when it ends. So the count cannot be lost,
    /// doubled, or taken off a word that never had it.
    pub(crate) fn register_loop_poll_word(&self, word: &Arc<LoopPollWord>) {
        let mut words = self.loop_poll_words.lock();
        words.retain(|w| !w.owner_dropped());
        word.registered_with.store(self.slot_id, Ordering::Relaxed);
        if words.iter().any(|w| Arc::ptr_eq(w, word)) {
            return;
        }
        words.push(Arc::clone(word));
        if self.stw_requested.load(Ordering::Acquire) {
            word.note_pause_begin();
        }
    }

    /// Send every dispatch loop registered with this barrier to its slow path
    /// at its next top, as a frame move on its own thread does
    /// ([`LoopPollWord::note_code_moved`]): after a class redefinition, so a
    /// loop whose frames run a replaced body moves them before its next
    /// bytecode -- including a loop whose thread was in compiled code or
    /// between polls while the redefinition's handshake ran (interpreter round
    /// i1 wave 37, lane L3; `interpreter::obsolete_frames`). A `Release` add,
    /// so a loop whose slow path reads the word with `Acquire` also sees what
    /// the caller wrote before (a raised [`Self::redefinition_fences_up`]).
    pub(crate) fn note_code_moved_on_every_loop(&self) {
        let mut words = self.loop_poll_words.lock();
        words.retain(|w| !w.owner_dropped());
        for word in words.iter() {
            word.word
                .fetch_add(LoopPollWord::MOVE_STEP, Ordering::Release);
        }
    }

    /// A redefinition by thread `owner` is about to replace a class's constant
    /// pool: every dispatch loop of every other thread stops at its top until
    /// [`Self::lower_redefinition_fence`]
    /// (`interpreter::obsolete_frames::RedefinitionFence`; interpreter round i1
    /// wave 37, lane L3). Arms every loop word, so a loop already running
    /// reaches the fence at its next top.
    pub(crate) fn raise_redefinition_fence(&self, owner: ThreadId) {
        {
            let mut fences = self.redefinition_fences.lock();
            fences.push(owner);
            self.redefinition_fences_up.store(
                u32::try_from(fences.len()).unwrap_or(u32::MAX),
                Ordering::SeqCst,
            );
        }
        self.note_code_moved_on_every_loop();
    }

    /// Take down one fence [`Self::raise_redefinition_fence`] put up for
    /// `owner`, and arm every loop word again so the loops it held move their
    /// frames now.
    pub(crate) fn lower_redefinition_fence(&self, owner: ThreadId) {
        {
            let mut fences = self.redefinition_fences.lock();
            if let Some(at) = fences.iter().position(|&f| f == owner) {
                fences.swap_remove(at);
            }
            self.redefinition_fences_up.store(
                u32::try_from(fences.len()).unwrap_or(u32::MAX),
                Ordering::SeqCst,
            );
        }
        self.note_code_moved_on_every_loop();
    }

    /// Does a fence another thread than `thread` raised hold `thread`'s loops
    /// back? One load while no fence is up; the fence list's lock otherwise
    /// (a loop's slow path only).
    pub(crate) fn redefinition_fence_holds(&self, thread: ThreadId) -> bool {
        if self.redefinition_fences_up.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.redefinition_fences
            .lock()
            .iter()
            .any(|&owner| owner != thread)
    }

    /// Has `owner` a fence up already (one raised for a whole
    /// `redefineClasses` call, around the fence of each of its classes)?
    pub(crate) fn redefinition_fence_owned_by(&self, owner: ThreadId) -> bool {
        self.redefinition_fences_up.load(Ordering::Acquire) != 0
            && self.redefinition_fences.lock().contains(&owner)
    }

    /// Count a loop top a redefinition fence held back.
    #[inline]
    pub(crate) fn note_redefinition_fence_wait(&self) {
        self.redefinition_fence_waits
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Loop tops redefinition fences held back so far.
    pub(crate) fn redefinition_fence_waits(&self) -> u64 {
        self.redefinition_fence_waits.load(Ordering::Relaxed)
    }

    /// [`LoopPollWord::registered_with`]'s value for this barrier.
    #[cfg(test)]
    fn loop_poll_key(&self) -> usize {
        self.slot_id
    }

    /// Raise `stw_requested` and count the pause in every registered
    /// dispatch-loop poll word, in one `loop_poll_words` critical section
    /// (see [`Self::register_loop_poll_word`]). The flag goes first, so a
    /// loop that sees the count and then reads the flag with `Acquire`
    /// (`LoopPollWord::load_acquire` first) finds it raised. Called by a
    /// winning request, under `inner`.
    fn raise_stw_requested(&self) {
        let mut words = self.loop_poll_words.lock();
        words.retain(|w| !w.owner_dropped());
        self.stw_requested.store(true, Ordering::Release);
        for word in words.iter() {
            word.note_pause_begin();
        }
        // t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`): after the flag,
        // so a page-form poll that faults finds the pause up in its slow path.
        if self.page_poll_base != 0 {
            self.set_poll_page_stop(true);
        }
    }

    /// t1 wave 8, lane SAFEPOLL: protect / unprotect this VM's stop page. A
    /// refused protection change is reported once per barrier: with it, the
    /// page-form polls of this VM miss (or keep faulting after) the pause,
    /// while every byte-form poll is still served by the flag.
    fn set_poll_page_stop(&self, stop: bool) {
        if !cratonvm_jit::poll_page::set_stop(self.page_poll_base, stop) {
            // Through the exit-report policy (HotSpot prints no `[cratonvm]`
            // line on stderr): seen under `CRATONVM_DBG=exit-census`.
            cratonvm_types::exit_report!(
                "[cratonvm] CRATONVM_JIT_PAGE_POLL: the OS refused to {} the poll page at {:#x}",
                if stop { "protect" } else { "unprotect" },
                self.page_poll_base
            );
        }
    }

    /// t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`): this VM's poll
    /// page pair, `0` when none (`DirectHelperTable::page_poll_base`).
    pub(crate) fn page_poll_base(&self) -> usize {
        self.page_poll_base
    }

    /// Lower `stw_requested` and uncount the pause in every registered word,
    /// in one `loop_poll_words` critical section. Uncounts only when the flag
    /// was up: a `complete_gc` with no pause in progress changes nothing, as
    /// its flag store never did. Called by `complete_gc`, under `inner`.
    fn lower_stw_requested(&self) {
        let words = self.loop_poll_words.lock();
        let was_up = self.stw_requested.load(Ordering::Acquire);
        // t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`): before the flag,
        // so a thread resumed by the lowering never faults again for a pause
        // that is over (the reverse order would only cost it a spin).
        if was_up && self.page_poll_base != 0 {
            self.set_poll_page_stop(false);
        }
        self.stw_requested.store(false, Ordering::Release);
        if was_up {
            for word in words.iter() {
                word.note_pause_end();
            }
        }
    }

    /// This VM's per-pause ledger. A parking peer binds it around its
    /// cross-thread coverage deposit (`safepoint_check`, through
    /// `cratonvm_gc::gc_quiescence::with_pause_ledger`); the initiator is
    /// bound to it by its winning request. gc-common w8-d.
    pub fn pause_ledger(&self) -> &Arc<cratonvm_gc::gc_quiescence::PauseLedger> {
        &self.pause_ledger
    }

    /// Wake the initiator waiting for the quota: the condvar every waiter
    /// uses, and on Windows the event [`Self::wait_for_all_timeout`] waits on.
    /// Called under `inner` at every point the quota can become satisfied.
    #[inline]
    fn notify_quota_met(&self) {
        self.all_arrived.notify_all();
        #[cfg(target_os = "windows")]
        crate::threading::jvm_thread::win_park::signal(self.arrival_event);
    }

    /// Record that a completed collection left finalizable objects queued for
    /// `finalize()`, or Cleaner actions queued to run (gc-common w3-a,
    /// 2026-09-23).
    ///
    /// Every collection door enqueues its dead finalizable objects
    /// (`run_collection_pause`), but only some doors can run Java afterwards:
    /// the allocation-FAILURE door is reached from deep inside allocation
    /// helpers that hold unrooted references, so the objects it queued waited
    /// for a later collection on a door that drains — never, in a program
    /// whose collections are all allocation failures
    /// (`docs/feature-designs/jdk-only-mode.md`).
    /// This hint lets a door that CAN run Java but did not collect — the
    /// interpreter's `maybe_gc` — see, with one relaxed load, that there is a
    /// queue to drain.
    ///
    /// Per VM, on the barrier because the barrier is the per-VM object every
    /// door already holds; on its own cache line because it is read on every
    /// interpreted allocation.
    ///
    /// gc-common w5-a: a raise that finds the hint LOWERED also records the
    /// current `gc_generation`, so [`Self::ref_work_stale`] can tell a hint no
    /// door has taken for whole pauses from one raised by the last one.
    #[inline]
    pub fn note_ref_work_queued(&self) {
        if !self.ref_work.queued.load(Ordering::Relaxed)
            && !self.ref_work.queued.swap(true, Ordering::AcqRel)
        {
            self.ref_work.raised_gen.store(
                self.gc_generation.load(Ordering::Acquire),
                Ordering::Release,
            );
        }
    }

    /// Whether [`Self::note_ref_work_queued`] was raised since the last
    /// [`Self::take_ref_work_queued`]. One relaxed load.
    #[inline]
    pub fn ref_work_queued(&self) -> bool {
        self.ref_work.queued.load(Ordering::Relaxed)
    }

    /// Lower the hint, returning whether it was raised. A drainer lowers it
    /// BEFORE draining: a collection that queues more after this store raises
    /// it again, and one that queued before it is drained by this caller.
    #[inline]
    pub fn take_ref_work_queued(&self) -> bool {
        self.ref_work.queued.swap(false, Ordering::AcqRel)
    }

    /// Whether the hint has stayed raised, taken by nobody, while at least
    /// `pauses` pauses completed (gc-common w5-a).
    ///
    /// The one door that takes the hint — `maybe_gc`'s no-collection path,
    /// after an interpreted allocation with no JIT borrow — is never reached by
    /// a program running compiled code end to end, so a hint that outlives
    /// whole pauses is the signature of exactly the case
    /// `common-w2a-finalizers-queued-by-forced-collections-wait-for-another-door`
    /// left open: work queued by collections nobody can drain. Read inside a
    /// pause, by the initiator; a heuristic, so plain loads suffice.
    #[inline]
    pub fn ref_work_stale(&self, pauses: u64) -> bool {
        self.ref_work.queued.load(Ordering::Acquire)
            && self
                .gc_generation
                .load(Ordering::Acquire)
                .saturating_sub(self.ref_work.raised_gen.load(Ordering::Acquire))
                >= pauses
    }

    /// Whether the caller must be treated as the current pause's initiator:
    /// it runs on the OS thread that made the request (see
    /// [`current_thread_token`]), whatever id it passes.
    ///
    /// # Keyed on the OS thread alone (gc-common w3-a, 2026-09-23)
    ///
    /// Until wave 3 a caller passing the initiator's `ThreadId` from ANY OS
    /// thread was also treated as the initiator. That clause existed only for
    /// the JNI placeholder `ThreadId(0)`: `host_thread_enter_native` on an
    /// UNREGISTERED thread arrived as `ThreadId(0)`, and main being the usual
    /// initiator, "do not arrive" happened to be right for it — while a COUNTED
    /// `jni_monitor_enter` caller arriving under the same placeholder was
    /// wrongly short-circuited and later deadlocked in `BlockedGuard::drop`.
    /// Lane W2-C made every JNI call site resolve its real id and skip the
    /// arrival for an unregistered caller (`jni_caller_thread_id`), so the id
    /// clause now only mis-classifies: a thread arriving under an id that
    /// happens to equal the initiator's would take no slot and never wait.
    ///
    /// The id is kept as the answer only when the request could not record an
    /// OS identity (`current_thread_token` returns `None` once the requesting
    /// thread's TLS is being torn down): without it the initiator would arrive
    /// at its own pause and wait for a generation only it can advance.
    /// `docs/feature-designs/jdk-only-mode.md`.
    #[inline]
    fn is_initiator_locked(inner: &GcBarrierInner, tid: ThreadId) -> bool {
        match inner.initiator_os {
            Some(owner) => current_thread_token() == Some(owner),
            None => matches!(inner.initiator, Some(owner) if owner == tid),
        }
    }

    /// Record this pause's time-to-safepoint, once, the first time the
    /// initiator observes its quota satisfied.
    fn note_quota_met_locked(&self, inner: &mut GcBarrierInner) {
        if inner.ttsp_recorded {
            return;
        }
        inner.ttsp_recorded = true;
        let Some(t0) = inner.requested_at else {
            return;
        };
        let ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
        // The straggler: the last participating arrival, unless the quota was
        // met by a take-over excusal after it (then the pause's last wait was
        // for a frozen peer, which has no arrival to name).
        let straggler = encode_straggler(inner.last_arrival);
        self.ttsp_pauses.fetch_add(1, Ordering::Relaxed);
        self.ttsp_total_ns.fetch_add(ns, Ordering::Relaxed);
        // Written only here, under `inner`, so the max and its straggler
        // cannot be paired with another pause's.
        if self.ttsp_max_ns.fetch_max(ns, Ordering::Relaxed) < ns {
            self.ttsp_max_straggler.store(straggler, Ordering::Relaxed);
        }
        self.ttsp_last_ns.store(ns, Ordering::Relaxed);
        self.ttsp_last_straggler.store(straggler, Ordering::Relaxed);
        if stw_census_dbg() {
            // `orphan_coverage_writes` (gc-common w21-e): coverage writes, over
            // the process's life, that no VM's pause ledger claimed. See
            // `gc_quiescence::coverage_orphan_writes`. Since gc-common w36-c it
            // includes the young pin-word writes no ledger claimed, which are
            // also printed alone as `young_pin_orphans`
            // (`gc_quiescence::young_pin_orphan_writes`).
            eprintln!(
                "[stw-ttsp] ttsp_us={} expected={} arrived={} straggler={:?} takeover_excused={} \
                 orphan_coverage_writes={} young_pin_orphans={} slot_wait_us={}",
                ns / 1_000,
                inner.expected,
                inner.arrived,
                inner.last_arrival,
                inner.takeover_excused,
                cratonvm_gc::gc_quiescence::coverage_orphan_writes(),
                cratonvm_gc::gc_quiescence::young_pin_orphan_writes(),
                inner.slot_wait_ns / 1_000,
            );
        }
    }

    /// Time-to-safepoint across every pause this barrier has run: the time
    /// from an accepted `request_stw*` to the initiator observing
    /// `arrived >= expected` (including quota reductions by the cross-thread
    /// JIT takeover). Before 2026-09-23 the VM had no measurement of this at
    /// all — the only signal was the takeover loop's warning after 64 rounds.
    ///
    /// Pure accounting, no behaviour: one `Instant::now()` per request and
    /// four relaxed atomics per pause.
    pub fn ttsp_stats(&self) -> TtspStats {
        TtspStats {
            pauses: self.ttsp_pauses.load(Ordering::Relaxed),
            total_ns: self.ttsp_total_ns.load(Ordering::Relaxed),
            max_ns: self.ttsp_max_ns.load(Ordering::Relaxed),
            last_ns: self.ttsp_last_ns.load(Ordering::Relaxed),
            max_straggler: decode_straggler(self.ttsp_max_straggler.load(Ordering::Relaxed)),
            last_straggler: decode_straggler(self.ttsp_last_straggler.load(Ordering::Relaxed)),
        }
    }

    /// zt t30/cv (perf-counter page `zgc-zt12pl4-*` item 3): the CPU time,
    /// in nanoseconds, the threads that ran this barrier's pauses spent
    /// inside them -- each pause's initiator, from the accepted request to
    /// `complete_gc` ([`vm_op_cpu_share`]). HotSpot's VM thread runs every
    /// stop-the-world operation and `sun.threads.cpu_time.vm` is its CPU; this
    /// VM runs them on the requesting thread, so this is the same work's CPU.
    /// Monotone; 0 off Linux and Windows and under `CRATONVM_PERF_VM_THREAD_CPU=0`.
    pub fn vm_thread_cpu_ns(&self) -> u64 {
        self.vm_thread_cpu_ns.load(Ordering::Relaxed)
    }

    /// gci i44/sp: this barrier's safepoint accounting ([`SafepointStats`]),
    /// what `VMManagementImpl.getSafepointCount` / `getTotalSafepointTime` /
    /// `getSafepointSyncTime` / `getTotalApplicationNonStoppedTime` answer in
    /// HotSpot. Four relaxed loads; the rows are written once per pause, so a
    /// reader racing a pause may pair one pause's count with the previous
    /// pause's times (HotSpot's perf counters are independent words too).
    /// `sync_ns` is clamped to `stopped_ns`: a pause whose quota was met but
    /// which has not completed has its sync time counted already.
    pub fn safepoint_stats(&self) -> SafepointStats {
        let stopped_ns = self.sp_stopped_ns.load(Ordering::Relaxed);
        // gci i46/lg (residual 6): without the pauses that were no HotSpot
        // safepoint (0 under `CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT=0`).
        let sync_ns = self
            .ttsp_total_ns
            .load(Ordering::Relaxed)
            .saturating_sub(self.sp_sync_uncounted_ns.load(Ordering::Relaxed));
        SafepointStats {
            count: self.sp_count.load(Ordering::Relaxed),
            stopped_ns,
            sync_ns: sync_ns.min(stopped_ns),
            application_ns: self.sp_application_ns.load(Ordering::Relaxed),
        }
    }

    /// gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual
    /// 6): the pause in flight is no HotSpot safepoint -- a handshake
    /// (compiled-loop exit, inline-cache grace, a single thread's stack
    /// trace) or a part of another operation (Generational's concurrent-cycle
    /// pauses, G1's initial mark, marking pauses inside a G1 full
    /// collection). HotSpot's `sun.rt.safepoints`, `safepointTime`,
    /// `safepointSyncTime` and `applicationTime` count only safepoints, so
    /// `complete_gc` counts nothing for it, and the application interval its
    /// winning request ended is taken back (the next counted pause's interval
    /// runs from the last counted release, across this pause). Call INSIDE the
    /// pause, on its initiator, before reading [`Self::safepoint_stats`];
    /// idempotent; a no-op outside a pause and under
    /// `CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT=0`. One uncontended lock.
    ///
    /// The take-back happens while the world is stopped, so a Java reader of
    /// `getTotalApplicationNonStoppedTime` (stopped too) never sees the value
    /// fall.
    pub fn note_pause_not_a_safepoint(&self) {
        if !self.sp_hotspot_count {
            return;
        }
        let mut inner = self.inner.lock();
        if !inner.pause_in_flight || inner.sp_uncounted {
            return;
        }
        inner.sp_uncounted = true;
        let added = std::mem::take(&mut inner.sp_app_added_ns);
        let _ = self
            .sp_application_ns
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(added))
            });
    }

    /// gci i46/lg (residual 2): run the calling OS thread's frame-trace pauses
    /// as the HotSpot VM operation `op` until the guard drops
    /// ([`FrameTraceOpScope`]). One small lock to enter and one to leave.
    pub fn frame_trace_op_scope(&self, op: Option<&'static str>) -> FrameTraceOpScope<'_> {
        let thread = std::thread::current().id();
        self.frame_trace_ops.lock().push((thread, op));
        self.frame_trace_ops_used.fetch_add(1, Ordering::Release);
        FrameTraceOpScope {
            barrier: self,
            thread,
        }
    }

    /// gci i46/lg (residual 2): the operation the calling OS thread's
    /// innermost [`FrameTraceOpScope`] names -- `Some(op)` inside a scope
    /// (`op` itself `None` for a handshake), `None` outside every scope. One
    /// load when no scope is held anywhere.
    pub fn frame_trace_op(&self) -> Option<Option<&'static str>> {
        if self.frame_trace_ops_used.load(Ordering::Acquire) == 0 {
            return None;
        }
        let thread = std::thread::current().id();
        self.frame_trace_ops
            .lock()
            .iter()
            .rev()
            .find(|(t, _)| *t == thread)
            .map(|(_, op)| *op)
    }

    /// Request a stop-the-world pause. TEST-ONLY.
    ///
    /// `alive_count` is the total number of alive threads (including the initiator).
    /// Returns `true` if the request was accepted, `false` if another STW is in progress.
    ///
    /// `#[cfg(test)]` since gc-common w2-a (2026-09-23): this and
    /// [`Self::request_stw_counted`] key the quota on the ANONYMOUS
    /// `threads_blocked` counter, which is wrong in both directions (see
    /// `Self::request_stw_counted_with_live_blocked`), and neither had a
    /// production caller — only unit tests that exercise the barrier's
    /// arithmetic. A new STW phase must use
    /// [`Self::request_stw_opening_cycle`] (a collection, or with a no-op
    /// `open_cycle` any other pause) plus the cross-thread
    /// takeover wait (`stw_take_over_and_wait`), never a plain `wait_for_all`.
    /// The three `brief_stw*` wrappers, which paired this with a plain
    /// `wait_for_all` and had no caller at all, were deleted.
    /// `docs/feature-designs/jdk-only-mode.md`.
    #[cfg(test)]
    pub(crate) fn request_stw(&self, initiator: ThreadId, alive_count: u32) -> bool {
        self.request_stw_counted(initiator, || alive_count)
    }

    /// Request a stop-the-world pause, computing `alive_count` while the
    /// barrier transition lock is held. TEST-ONLY — see [`Self::request_stw`].
    ///
    /// Subtracts the barrier's anonymous blocked count as-is, which is why it
    /// is not a production entry point: production requests use the
    /// registry's identity census.
    #[cfg(test)]
    pub(crate) fn request_stw_counted<F>(&self, initiator: ThreadId, alive_count: F) -> bool
    where
        F: FnOnce() -> u32,
    {
        let mut inner = self.inner.lock();
        // gci i6/jr: the production request's hold, so the unit tests reach it
        // through this entry too (`request_stw_opening_cycle` has the same).
        if !self.stw_requested.load(Ordering::Acquire) {
            let _ = self.resume_fairness_hold(&mut inner);
        }
        let alive_count = alive_count();
        let won = self.request_stw_counted_locked(
            &mut inner,
            initiator,
            alive_count,
            None,
            None,
            None,
            None::<fn()>,
        );
        drop(inner);
        if won {
            self.finish_owed_leaf_drain();
        }
        won
    }

    /// Request the stop-the-world pause of a COLLECTION, opening that
    /// collection's per-pause coverage cycle atomically with winning it.
    ///
    /// `census` runs under the barrier lock (as in
    /// the legacy `request_stw_counted_with_live_blocked`), but only when no pause
    /// is already requested — a losing initiator no longer pays a registry
    /// walk under the lock every mutator's park and wake also needs.
    /// `open_cycle` runs under the same lock, ONLY when the request wins, after
    /// the previous pause's state is gone and before `stw_requested` becomes
    /// visible — i.e. strictly before the first peer of THIS pause can deposit.
    /// Production passes `gc_quiescence::begin_moving_young_coverage_cycle`.
    ///
    /// # Why the open is part of the request (gc-common w2-a, 2026-09-23)
    ///
    /// `begin_moving_young_coverage_cycle` resets per-pause state (process
    /// statics then, this VM's `PauseLedger` since w8-d..w21-e): the coverage
    /// verdict, `conservative_jit_scans`, the cross-thread
    /// helper-window pins, the captured peer stack slots and registers. The
    /// GC doors used to run it BEFORE their request, so:
    ///
    /// * a thread that LOST the race reset the WINNER's pause after its peers
    ///   had deposited (`common-a-losing-initiator-resets-coverage-state-FIXED-20260923.md`);
    ///   wave 1 guarded that with `run_if_no_stw_requested`, which left
    /// * two would-be initiators BOTH before any request: A resets and
    ///   publishes its own verdicts, B resets (erasing them), A wins
    ///   (`common-e-coverage-cycle-reset-races-a-sibling-initiator-FIXED-20260923.md`) — on
    ///   Generational that disarms the `unrewritable_conservative_jit_roots`
    ///   divert for A's own conservatively-scanned frame.
    ///
    /// Here only the winner resets, and it resets before anyone can observe
    /// its pause. The initiator publishes its own snapshot AFTER this returns
    /// `true` (the doors call `update_root_snapshot` then), so its marks land
    /// after the reset by construction. A loser resets nothing.
    ///
    /// # One VM's pause at a time (gc-common w6-a, 2026-09-24)
    ///
    /// The reset above cleared PROCESS-global rows, so before taking the lock
    /// this request takes the coverage slot ([`COVERAGE_SLOT_OWNER`]): with
    /// another VM's pause in flight it waits for that pause's `complete_gc`
    /// (bounded; see there) instead of erasing its verdicts. A single-VM
    /// process never waits. Since gc-common w21-e the reset clears this VM's
    /// `PauseLedger` rows and the process's orphan coverage rows only (see
    /// `gc_quiescence::CoverageCycle`).
    pub fn request_stw_opening_cycle<C, O>(
        &self,
        initiator: ThreadId,
        census: C,
        open_cycle: O,
    ) -> bool
    where
        C: FnOnce() -> StwCensus,
        O: FnOnce(),
    {
        self.request_stw_opening_cycle_ruled(initiator, false, census, open_cycle)
    }

    /// [`Self::request_stw_opening_cycle`] for an ALLOCATION-driven collection
    /// (gcj j3/rt3; `gcj-j1cm-safepoint-barrier-and-registry-review-residuals`
    /// item 7). With `sibling_rule` (the caller passes
    /// `CRATONVM_GEN_STW_HOLD_SEES_SIBLING_PAUSE` -- [`Self::hold_sees_sibling_pause`]
    /// -- and a Generational heap) a request whose fairness hold let a
    /// sibling's WHOLE pause run and complete (the barrier's generation moved
    /// while `inner` was released) loses, as HotSpot's
    /// `VM_CollectForAllocation` is skipped when the collection count moved
    /// since it was queued: the caller's allocation retry then sees the
    /// sibling's collection instead of requesting a back-to-back duplicate.
    /// Without the rule only a sibling's pause still REQUESTED makes it lose,
    /// as before.
    pub fn request_stw_opening_cycle_for_allocation<C, O>(
        &self,
        initiator: ThreadId,
        sibling_rule: bool,
        census: C,
        open_cycle: O,
    ) -> bool
    where
        C: FnOnce() -> StwCensus,
        O: FnOnce(),
    {
        self.request_stw_opening_cycle_ruled(initiator, sibling_rule, census, open_cycle)
    }

    /// `CRATONVM_GEN_STW_HOLD_SEES_SIBLING_PAUSE` for this VM, read once (gcj
    /// j3/rt3, opt-in): see [`Self::request_stw_opening_cycle_for_allocation`].
    pub fn hold_sees_sibling_pause(&self) -> bool {
        self.hold_sees_sibling_pause
    }

    /// gcj j3/rt3: allocation requests that lost because a sibling's whole
    /// pause ran inside their fairness hold
    /// ([`Self::request_stw_opening_cycle_for_allocation`]). Tests and rows.
    pub fn hold_sibling_pause_losses(&self) -> u64 {
        self.hold_sibling_pause_losses.load(Ordering::Relaxed)
    }

    fn request_stw_opening_cycle_ruled<C, O>(
        &self,
        initiator: ThreadId,
        sibling_rule: bool,
        census: C,
        open_cycle: O,
    ) -> bool
    where
        C: FnOnce() -> StwCensus,
        O: FnOnce(),
    {
        let won =
            self.request_stw_opening_cycle_ruled_inner(initiator, sibling_rule, census, open_cycle);
        // gcj j3/rt3 residual 3: `inner` is released here; a leaf window the
        // locked drain left open is waited for now, before the pause's
        // initiator goes on to collect.
        if won {
            self.finish_owed_leaf_drain();
        }
        won
    }

    /// [`Self::request_stw_opening_cycle_ruled`] up to the release of `inner`.
    fn request_stw_opening_cycle_ruled_inner<C, O>(
        &self,
        initiator: ThreadId,
        sibling_rule: bool,
        census: C,
        open_cycle: O,
    ) -> bool
    where
        C: FnOnce() -> StwCensus,
        O: FnOnce(),
    {
        // Outside `inner`: this VM's mutators park and wake through it, and
        // the wait is for ANOTHER VM's pause.
        //
        // gcg g2/k: under `CRATONVM_STW_COVERAGE_SLOT_NO_WAIT` one try and no
        // wait (`common-w21e-proposal-coverage-slot-without-the-wait`); a
        // pause that goes ahead without the slot adopts it once it is free.
        let slot_t0 = Instant::now();
        let mut slot = if self.coverage_slot_no_wait {
            self.try_take_coverage_slot()
        } else {
            self.take_coverage_slot(COVERAGE_SLOT_WAIT_LIMIT)
        };
        let slot_wait_ns = u64::try_from(slot_t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let mut inner = self.inner.lock();
        if self.stw_requested.load(Ordering::Acquire) {
            self.settle_unused_slot(&mut inner, slot);
            return false;
        }
        // gci i6/jr: the resume census, and with `CRATONVM_STW_RESUME_FAIRNESS`
        // the hold that lets the previous pause's waiters back in first. The
        // hold released `inner`: a sibling request may have won meanwhile.
        // gcj j3/rt3: under `sibling_rule`, and won AND completed meanwhile
        // (the generation is bumped by `complete_gc` under `inner`).
        let generation_before = self.gc_generation.load(Ordering::Acquire);
        if self.resume_fairness_hold(&mut inner) {
            if self.stw_requested.load(Ordering::Acquire) {
                self.settle_unused_slot(&mut inner, slot);
                return false;
            }
            if sibling_rule && self.gc_generation.load(Ordering::Acquire) != generation_before {
                self.hold_sibling_pause_losses
                    .fetch_add(1, Ordering::Relaxed);
                self.settle_unused_slot(&mut inner, slot);
                return false;
            }
        }
        if !slot {
            // Not waited for: this barrier looked like the owner (its previous
            // pause, released since -- `complete_gc` frees the slot under this
            // lock), or its own pause was requested. One non-blocking try; on
            // failure this pause goes ahead without the slot, as before w6.
            slot = self.try_take_coverage_slot();
        }
        let won = if self.lockfree_blocked {
            // gcg g2/k: flag BEFORE census (see `request_flag_then_census_locked`).
            self.request_flag_then_census_locked(&mut inner, initiator, census, open_cycle)
        } else {
            let StwCensus {
                alive,
                blocked,
                blocked_tids,
                initiator_counted,
                excused_running,
            } = census();
            let won = self.request_stw_counted_locked(
                &mut inner,
                initiator,
                alive,
                Some(blocked),
                Some(blocked_tids),
                initiator_counted,
                Some(open_cycle),
            );
            // Wave 58 (lane L6): no arrival can be counted before this
            // returns, which holds `inner` throughout.
            if won {
                Self::publish_excused_running_locked(&mut inner, excused_running);
            }
            won
        };
        if won {
            inner.holds_coverage_slot = slot;
            inner.wants_coverage_slot = true;
            // After the request core (whose `begin_request_locked` zeroes it).
            inner.slot_wait_ns = slot_wait_ns;
        } else {
            self.settle_unused_slot(&mut inner, slot);
        }
        won
    }

    /// A production pause that went ahead WITHOUT the coverage slot (its
    /// request gave up waiting for another VM's pause, or skipped an abandoned
    /// hold) takes the slot the moment it is free. Under `inner`; called by
    /// the initiator's barrier waits, i.e. before its root scan.
    ///
    /// gc-common w8-d. Until then such a pause ran slot-less to its
    /// `complete_gc` even after the other VM's pause had released the slot, so
    /// a THIRD VM's request could take the free slot and reset -- as its
    /// `open_cycle` -- the verdict, pins and take-over verdict this pause's
    /// collector was about to read: the w6-a hazard, reopened for the whole
    /// of a slot-less pause instead of only its overlap with the pause it gave
    /// up on. One compare-exchange, and only while a production pause holds
    /// no slot; never taken for a slot some sibling request of this barrier
    /// holds (the owner is then this barrier, not free), which
    /// [`Self::settle_unused_slot`] hands over instead.
    #[inline]
    fn adopt_free_coverage_slot_locked(&self, inner: &mut GcBarrierInner) {
        if inner.wants_coverage_slot
            && !inner.holds_coverage_slot
            && inner.pause_in_flight
            && self.try_take_coverage_slot()
        {
            inner.holds_coverage_slot = true;
        }
    }

    /// A request that LOST took the slot (`slot`): give it to this barrier's
    /// pause in flight if that pause went ahead without it, else release it.
    /// Under `inner`.
    ///
    /// gc-common w7-a. Releasing it outright lost the slot in a sibling race
    /// of one VM: T1 takes the slot and queues on `inner`; T2 of the same
    /// barrier sees the barrier as owner (`take_coverage_slot` does not wait
    /// for itself), gets `inner` first, finds the slot taken by T1 and WINS
    /// without it; T1 then loses and released the slot. T2's pause ran with
    /// the slot free, so another VM's request could take it and reset the
    /// per-pause rows T2's collection was about to read -- the very hazard the
    /// slot exists for. The slot is the barrier's (`slot_id`), not a thread's,
    /// so handing it to the pause in flight is exact: that pause's
    /// `complete_gc` releases it.
    fn settle_unused_slot(&self, inner: &mut GcBarrierInner, slot: bool) {
        if !slot {
            return;
        }
        if inner.pause_in_flight
            && !inner.holds_coverage_slot
            && self.stw_requested.load(Ordering::Acquire)
        {
            inner.holds_coverage_slot = true;
        } else {
            self.release_coverage_slot();
        }
    }

    /// One compare-exchange `0 -> self`. `true` if this call took the slot.
    #[inline]
    fn try_take_coverage_slot(&self) -> bool {
        COVERAGE_SLOT_OWNER
            .compare_exchange(0, self.slot_id, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Take the coverage slot for a request of this barrier, waiting at most
    /// `limit` for another VM's pause to release it. `true` if this call took
    /// it; `false` if this barrier already owns it, its own pause is already
    /// requested, or the wait expired -- see [`COVERAGE_SLOT_OWNER`] for why
    /// each of those must not wait.
    fn take_coverage_slot(&self, limit: Duration) -> bool {
        let mut waiting_since: Option<Instant> = None;
        let mut rounds: u32 = 0;
        loop {
            match COVERAGE_SLOT_OWNER.compare_exchange(
                0,
                self.slot_id,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if waiting_since.is_some() {
                        COVERAGE_SLOT_WAITS.fetch_add(1, Ordering::Relaxed);
                    }
                    return true;
                }
                Err(owner) if owner == self.slot_id => return false,
                Err(owner) => {
                    // A hold some request already waited the full limit for
                    // (see `COVERAGE_SLOT_GAVE_UP_RELEASES`): do not wait again.
                    if coverage_slot_hold_abandoned(owner) {
                        COVERAGE_SLOT_ABANDONED_SKIPS.fetch_add(1, Ordering::Relaxed);
                        return false;
                    }
                }
            }
            if self.stw_requested.load(Ordering::Acquire) {
                return false;
            }
            let since = *waiting_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= limit {
                // Name the hold given up on, as `(owner, releases)` read with
                // no release in between: a release between the two reads
                // (re-checked below) would pair this owner with its NEXT
                // hold's count and make that hold look abandoned too.
                let releases = COVERAGE_SLOT_RELEASES.load(Ordering::Acquire);
                let owner_now = COVERAGE_SLOT_OWNER.load(Ordering::Acquire);
                if owner_now != 0
                    && owner_now != self.slot_id
                    && COVERAGE_SLOT_RELEASES.load(Ordering::Acquire) == releases
                {
                    COVERAGE_SLOT_GAVE_UP_RELEASES.store(releases, Ordering::Release);
                    COVERAGE_SLOT_GAVE_UP_OWNER.store(owner_now, Ordering::Release);
                }
                let n = COVERAGE_SLOT_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
                if stw_census_dbg() {
                    eprintln!(
                        "[stw-coverage-slot] barrier={} gave up after {:?} waiting for barrier={}; \
                         pausing without the slot (timeouts={} waits={} abandoned_skips={})",
                        self.slot_id,
                        limit,
                        owner_now,
                        n,
                        COVERAGE_SLOT_WAITS.load(Ordering::Relaxed),
                        COVERAGE_SLOT_ABANDONED_SKIPS.load(Ordering::Relaxed),
                    );
                }
                return false;
            }
            rounds = rounds.saturating_add(1);
            if rounds < 64 {
                std::hint::spin_loop();
            } else if rounds < 128 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(100));
            }
        }
    }

    /// Release the coverage slot if this barrier owns it. Idempotent.
    #[inline]
    fn release_coverage_slot(&self) {
        if COVERAGE_SLOT_OWNER
            .compare_exchange(self.slot_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // Ends this hold: a request that gave up on it waits again for
            // the next one (`coverage_slot_hold_abandoned`).
            COVERAGE_SLOT_RELEASES.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Request a stop-the-world pause with both the alive-thread count and the
    /// subset of those alive threads that are currently published as blocked.
    ///
    /// `live_blocked_count` is the authoritative exclusion count for these
    /// callers: `in_blocked_region` is set only after the thread's root snapshot
    /// has been deposited, and it can become visible before `enter_blocked`
    /// increments the anonymous `threads_blocked` counter. The global counter is
    /// still useful for legacy callers and diagnostics, but production GC must
    /// key the expected mutator quota off the same registry state that owns the
    /// blocked root/fixup publication.
    ///
    /// `counts` also returns the IDENTITIES (`ThreadId.0`) of the alive
    /// threads currently published `in_blocked_region == true` — the exact
    /// set this request excludes from `expected`. Recorded in
    /// `excluded_blocked` so every arrival for this pause can look up its
    /// own exclusion status under the same lock (see
    /// [`Self::arrive_and_wait_auto`]) instead of guessing from a stale
    /// local flag.
    ///
    /// TEST-ONLY since gc-common w4-a (2026-09-23): the tuple cannot say
    /// whether the initiator is one of the counted threads, so it always takes
    /// the legacy `alive - 1 - blocked`. Every production pause — collections
    /// and the non-collection pauses alike (`request_non_collection_pause` in
    /// `gc_and_alloc.rs`) — requests through [`Self::request_stw_opening_cycle`]
    /// with a [`StwCensus`]. Kept for the barrier-arithmetic tests here and the
    /// JNI host-native census test.
    #[cfg(test)]
    pub(crate) fn request_stw_counted_with_live_blocked<F>(
        &self,
        initiator: ThreadId,
        counts: F,
    ) -> bool
    where
        F: FnOnce() -> (u32, u32, Vec<u64>),
    {
        let mut inner = self.inner.lock();
        let (alive_count, live_blocked_count, blocked_tids) = counts();
        let won = self.request_stw_counted_locked(
            &mut inner,
            initiator,
            alive_count,
            Some(live_blocked_count),
            Some(blocked_tids),
            None,
            None::<fn()>,
        );
        drop(inner);
        if won {
            self.finish_owed_leaf_drain();
        }
        won
    }

    /// The shared request core. `initiator_counted` / `open_cycle`: see
    /// [`StwCensus::initiator_counted`] and [`Self::request_stw_opening_cycle`];
    /// `None` for both reproduces the pre-w2 behaviour exactly.
    #[allow(clippy::too_many_arguments)]
    fn request_stw_counted_locked<O: FnOnce()>(
        &self,
        inner: &mut GcBarrierInner,
        initiator: ThreadId,
        alive_count: u32,
        live_blocked_count: Option<u32>,
        excluded_blocked: Option<Vec<u64>>,
        initiator_counted: Option<bool>,
        open_cycle: Option<O>,
    ) -> bool {
        if self.stw_requested.load(Ordering::Acquire) {
            return false;
        }
        Self::begin_request_locked(inner, initiator);
        Self::publish_census_locked(
            inner,
            self.threads_blocked.load(Ordering::Acquire),
            initiator,
            alive_count,
            live_blocked_count,
            excluded_blocked,
            initiator_counted,
        );
        self.open_and_raise_locked(inner, open_cycle);
        true
    }

    /// gcg g2/k: the request core of the lock-free blocked transitions
    /// (`CRATONVM_STW_LOCKFREE_BLOCKED`,
    /// `common-a-proposal-lock-free-blocked-transitions-DONE-20261015`). Same steps as
    /// [`Self::request_stw_counted_locked`], in a different order: the flag
    /// goes up FIRST (after the coverage cycle opened, as before), then a
    /// `SeqCst` fence, then the census, then `expected`.
    ///
    /// # Why that order makes the entry lock unnecessary
    ///
    /// A mutator entering a blocked region stores its `in_blocked_region`
    /// (the deposit), fences `SeqCst`, then loads `stw_requested`
    /// ([`Self::enter_blocked`]). This request stores `stw_requested`, fences
    /// `SeqCst`, then loads every `in_blocked_region` (the census). Store,
    /// fence, load on both sides: at least one sees the other's store. So a
    /// thread whose entry read `pre_stw == false` is seen blocked by this
    /// census (excluded, and it parks without arriving -- correct), and one
    /// that read `true` arrives through `arrive_and_wait_auto`, which takes
    /// `inner` -- held here until `expected` and `excluded_blocked` are
    /// published -- and so classifies itself from THIS census exactly. That
    /// is the same pair of outcomes the entry lock produced.
    ///
    /// Everything else that reads the flag also waits for `inner` before it
    /// acts on a pause (arrivals, the flagged leave, thread start's
    /// `run_if_no_stw_requested`, thread exit's `finish_after`), so raising
    /// the flag before `expected` exists exposes no half-built pause.
    /// Lock order is unchanged (`inner` -> registry read lock for the census,
    /// as before); the census now runs while peers can see the flag, so a
    /// peer that held the registry WRITE lock while waiting for a pause would
    /// deadlock here -- no such path is known, and the old order deadlocked
    /// it at the root scan anyway.
    fn request_flag_then_census_locked<C, O>(
        &self,
        inner: &mut GcBarrierInner,
        initiator: ThreadId,
        census: C,
        open_cycle: O,
    ) -> bool
    where
        C: FnOnce() -> StwCensus,
        O: FnOnce(),
    {
        if self.stw_requested.load(Ordering::Acquire) {
            return false;
        }
        Self::begin_request_locked(inner, initiator);
        // No arrival can be counted before `publish_census_locked`: every
        // arrival takes `inner`, which this request holds throughout.
        inner.arrived = 0;
        inner.excluded_blocked.clear();
        self.open_and_raise_locked(inner, Some(open_cycle));
        // The request's half of the Dekker pair (see the doc above).
        // `open_and_raise_locked`'s leaf-window drain fences too; this one is
        // the census's own, so the argument does not depend on that drain.
        std::sync::atomic::fence(Ordering::SeqCst);
        let StwCensus {
            alive,
            blocked,
            blocked_tids,
            initiator_counted,
            excused_running,
        } = census();
        if stw_census_dbg() {
            // The switch's engagement control: only this order prints it.
            eprintln!("[stw-request] order=flag-first initiator={}", initiator.0);
        }
        Self::publish_census_locked(
            inner,
            self.threads_blocked.load(Ordering::Acquire),
            initiator,
            alive,
            Some(blocked),
            Some(blocked_tids),
            initiator_counted,
        );
        Self::publish_excused_running_locked(inner, excused_running);
        true
    }

    /// Take the threads the census excused in a running native (interpreter
    /// round i2 wave 58, lane L6; [`StwCensus::excused_running`]) out of
    /// `expected` and record them, so none of their arrivals counts in this
    /// pause. Under `inner`, right after `publish_census_locked`, before any
    /// arrival of this pause can be counted. Nothing for an empty set.
    fn publish_excused_running_locked(inner: &mut GcBarrierInner, mut excused: Vec<u64>) {
        inner.excused_running.clear();
        if excused.is_empty() {
            return;
        }
        // gcj j2/cm2 (`gcj-j1cm-safepoint-barrier-and-registry-review-residuals`
        // item 5): a thread the census EXCLUDED as blocked was never in
        // `expected`, so excusing it too would subtract it twice. Not
        // constructible today -- the excusal walk tests the same flag the
        // census does, under the same lock hold, and every legitimate lowering
        // of the flag takes `inner` or loses the Dekker race -- so this changes
        // nothing that runs now; it stops a future bare `store(false)` from
        // releasing a pause one mutator early.
        excused.retain(|tid| !inner.excluded_blocked.contains(tid));
        if excused.is_empty() {
            return;
        }
        let n = u32::try_from(excused.len()).unwrap_or(u32::MAX);
        inner.expected = inner.expected.saturating_sub(n);
        inner.excused_running.extend(excused);
        if stw_census_dbg() {
            eprintln!(
                "[stw-request] excused_running={:?} expected={}",
                inner.excused_running, inner.expected
            );
        }
    }

    /// Does this pause's census excuse `tid` in a running native (wave 58,
    /// lane L6)? Under `inner`; one compare per excused thread (usually none).
    #[inline]
    fn excused_running_locked(inner: &GcBarrierInner, tid: ThreadId) -> bool {
        !inner.excused_running.is_empty() && inner.excused_running.contains(&tid.0)
    }

    /// The per-request fields every request core resets first. Under `inner`.
    fn begin_request_locked(inner: &mut GcBarrierInner, initiator: ThreadId) {
        inner.initiator = Some(initiator);
        inner.initiator_os = current_thread_token();
        inner.requested_at = Some(Instant::now());
        // zt t30/cv: the VM-thread CPU's start (`sun.threads.cpu_time.vm`).
        // zt pl4 item 3: and nothing booked as collector work yet.
        let _ = cratonvm_gc::thread_cpu::take_pause_work_cpu();
        inner.request_cpu = match (inner.initiator_os, vm_op_thread_cpu_ns()) {
            (Some(token), Some(cpu)) => Some((token, cpu)),
            _ => None,
        };
        inner.ttsp_recorded = false;
        inner.slot_wait_ns = 0;
        inner.last_arrival = None;
        inner.arrived_ids.clear();
        inner.takeover_excused = 0;
        inner.excused_running.clear();
    }

    /// gci i20/st: book one PARTICIPATING arrival of `tid` for the current
    /// pause: `last_arrival` (the time-to-safepoint straggler) and
    /// `arrived_ids` (the stall report's arrived set). Under `inner`; the
    /// caller bumps `arrived` and checks the quota as before.
    fn note_counted_arrival_locked(inner: &mut GcBarrierInner, tid: ThreadId) {
        inner.last_arrival = Some(tid.0);
        inner.arrived_ids.push(tid.0);
    }

    /// gci i20/st: the ids of this pause's participating arrivals so far,
    /// sorted (empty between pauses once the next request clears them).
    /// Test-only (the stall line reads the field under its own lock hold).
    #[cfg(test)]
    pub(crate) fn arrived_ids_snapshot(&self) -> Vec<u64> {
        let mut ids = self.inner.lock().arrived_ids.clone();
        ids.sort_unstable();
        ids
    }

    /// Publish this pause's census: `excluded_blocked`, `expected`, and
    /// `arrived = 0`. Under `inner`. `anon_blocked` is the anonymous
    /// `threads_blocked` count, used only when `live_blocked_count` is `None`
    /// (the test-only legacy entries) and for the census log.
    fn publish_census_locked(
        inner: &mut GcBarrierInner,
        anon_blocked: u64,
        initiator: ThreadId,
        alive_count: u32,
        live_blocked_count: Option<u32>,
        excluded_blocked: Option<Vec<u64>>,
        initiator_counted: Option<bool>,
    ) {
        // GCAUDIT-0711-FIX (finding 1a): publish the exact excluded-thread
        // set for THIS pause atomically with `expected`, under the same
        // lock every `arrive_and_wait_auto` call reads it through.
        //
        // gc-common w18-c: refilled in place rather than replaced by a freshly
        // collected set. The census's blocked set is O(blocked threads) and
        // this runs under `inner`, the lock every arrival and blocked
        // transition queues on; replacing the set allocated and grew a new
        // table on every pause and dropped the old one inside the lock.
        // `complete_gc` already `clear()`s it (keeping its capacity), so the
        // steady state now allocates nothing here. Same contents either way.
        inner.excluded_blocked.clear();
        if let Some(v) = excluded_blocked {
            inner.excluded_blocked.extend(v);
        }
        // T19.H1: exclude threads currently parked in a blocking native
        // from the set we wait for. They are GC-safe (roots already
        // deposited via `deposit_root_snapshot`) and execute no code, so
        // they will not reach an interpreter safepoint. Without this, a
        // STW initiated while e.g. the Reference Handler thread is parked
        // in `ReferenceQueue.remove` deadlocks `wait_for_all` forever.
        //
        // Race analysis (the count may change after this read):
        //  * blocked->running after the read: the waking thread runs
        //    `check_post_block_gc`, sees `stw_requested`, and waits the
        //    pause out. B2 fix: it does so via `arrive_and_wait_excluded`
        //    (or, once it has left the blocked region and become a counted
        //    mutator, via the participating `arrive_and_wait`). An excluded
        //    caller never bumps `arrived`, so it can no longer satisfy the
        //    `arrived >= expected` quota early and release `wait_for_all`
        //    while a counted mutator is still running.
        //  * running->blocked after the read: handled by `enter_blocked`,
        //    which, if a STW is already active, makes the thread
        //    arrive at the barrier *before* it parks, so the initiator
        //    is not left waiting for a thread that counted in `expected`
        //    and then vanished into a block.
        let blocked_u32 = u32::try_from(anon_blocked).unwrap_or(u32::MAX);
        let live_blocked_for_log = live_blocked_count.unwrap_or(blocked_u32);
        let effective_blocked = live_blocked_count.unwrap_or(blocked_u32);
        // gc-common 2026-09-23 (lane A): `alive - 1 - blocked` removes the
        // initiator twice when the identity census itself reports the
        // initiator as blocked (its own `in_blocked_region` still raised when
        // it requests — a native carrier marked by `mark_native_thread_blocked`
        // that allocates, or a leaked raise not yet healed). Each double
        // subtraction lowers `expected` by one, i.e. the barrier releases with
        // one genuinely counted mutator still running — a moving collection
        // racing live frames. The identity set says exactly when this happens,
        // so correct for it exactly; the legacy anonymous count cannot say
        // and keeps the old arithmetic.
        let initiator_already_excluded =
            live_blocked_count.is_some() && inner.excluded_blocked.contains(&initiator.0);
        // gc-common w2-a (2026-09-23): the other half of the same page — an
        // initiator the census did not count at all (`initiator_counted ==
        // Some(false)`) owns no slot in `alive`, so taking one off removes a
        // real mutator from the quota. Only a caller that KNOWS says so;
        // `None` keeps the old arithmetic.
        let initiator_slot = initiator_slot(initiator_already_excluded, initiator_counted);
        inner.expected = alive_count
            .saturating_sub(initiator_slot)
            .saturating_sub(effective_blocked);
        inner.arrived = 0;
        if stw_census_dbg() {
            eprintln!(
                "[stw-request] initiator={} alive={} blocked={} live_blocked={} effective_blocked={} initiator_blocked={} initiator_counted={:?} expected={}",
                initiator.0,
                alive_count,
                blocked_u32,
                live_blocked_for_log,
                effective_blocked,
                initiator_already_excluded,
                initiator_counted,
                inner.expected
            );
        }
    }

    /// Bind and reset this VM's pause ledger, open the coverage cycle (a
    /// collection's), raise `stw_requested`, drain the JNI leaf windows and
    /// count the pause in flight. Under `inner`, by a request that has won.
    fn open_and_raise_locked<O: FnOnce()>(
        &self,
        inner: &mut GcBarrierInner,
        open_cycle: Option<O>,
    ) {
        // NOTE: do NOT clear `pointer_map` here. With generation-keyed waiting
        // (see `arrive_and_wait_inner`), a thread that arrived for the previous
        // generation may not read its remap map until after THIS `request_stw`
        // runs; clearing it would hand that thread an empty map and strand its
        // frame pointers at pre-GC (relocated) addresses. The map is overwritten
        // wholesale by the matching `complete_gc`, so a stale map never leaks
        // into the wrong generation.
        //
        // DO clear the cross-thread JIT coverage ledger, and do it here rather
        // than only in `begin_moving_young_coverage_cycle`. Every peer deposit
        // happens inside the window this store opens (a peer parks because it
        // observed `stw_requested`), so this is the one place in the process
        // that is guaranteed to run before the first deposit of a pause and
        // after the last deposit of the previous one — and it runs under the
        // same lock that publishes `expected`, so no peer can be mid-deposit
        // across it. `begin_moving_young_coverage_cycle` also clears it; the
        // duplication is deliberate, because only SOME pauses open a coverage
        // cycle and an over-count is the ledger's only unsound state.
        //
        // The ledger is THIS VM's (gc-common w8-d): bind it on the initiator's
        // thread first, so the reset below -- and `open_cycle`, the root scan,
        // the take-over and the pause line after it -- reach this barrier's
        // `PauseLedger` and no other VM's. `complete_gc` unbinds it.
        cratonvm_gc::gc_quiescence::bind_pause_ledger(&self.pause_ledger);
        cratonvm_gc::gc_quiescence::reset_peer_proven_jit_depth();
        // A COLLECTION's pause also opens its coverage cycle here, and only
        // here — see `request_stw_opening_cycle`. Same lock, same point: after
        // the request is known to win, before any peer can observe it.
        if let Some(open_cycle) = open_cycle {
            open_cycle();
        }
        // Every interpreter dispatch loop polls its own thread's word, not the
        // flag (interpreter round i1 wave 26, lane L7): the flag and the pause
        // count in every registered word change together.
        self.raise_stw_requested();
        // gcd d5/f: no JNI leaf window may still be open once the request
        // returns (see `LeafWindowWord`). One fence and one load unless a
        // thread of this VM ever opened one.
        self.drain_leaf_windows();
        // Interpreter round i2 wave 64, lane L6: wake the debuggees parked on
        // their suspend word (`CRATONVM_DEBUG_STW_UNPARKS_PARKED`), which
        // wait unbounded by the safepoint poll. One fence and one load unless
        // some thread of this VM ever enrolled.
        self.unpark_stw_wakers();
        if !inner.pause_in_flight {
            inner.pause_in_flight = true;
            note_pause_begin();
            // gci i44/sp: the application interval this pause ends -- from the
            // last counted pause's release or the barrier's creation (stamp
            // 1), as HotSpot's `RuntimeService::record_safepoint_begin` adds
            // `_app_timer`. gci i46/lg (rv item 3): the base is
            // `sp_app_since_ns`, written under this lock only by a
            // `complete_gc` that counted a pause; it was `released_at_ns`,
            // which a spurious `complete_gc` moves too.
            let since = inner.sp_app_since_ns.max(1);
            let added = self.resume_stamp_now().saturating_sub(since);
            inner.sp_app_added_ns = added;
            inner.sp_uncounted = false;
            self.sp_application_ns.fetch_add(added, Ordering::Relaxed);
        }
    }

    /// Run `f` only if no STW request is active, serialized with the request
    /// core's census and `expected` (`request_stw_opening_cycle`; both orders,
    /// the default and `CRATONVM_STW_LOCKFREE_BLOCKED`'s, run them under
    /// `inner`).
    ///
    /// Startup threads use this to become STW-countable only when a
    /// concurrent request cannot have just excluded them from `expected`.
    pub fn run_if_no_stw_requested<F>(&self, f: F) -> bool
    where
        F: FnOnce(),
    {
        let _inner = self.inner.lock();
        if self.stw_requested.load(Ordering::Acquire) {
            return false;
        }
        f();
        true
    }

    /// T19.H1 — mark the calling thread as entering a blocking native
    /// operation (about to park in `wait`/`park`/`sleep`/`select`/…) and
    /// return a [`BlockedGuard`] that clears the mark on drop.
    ///
    /// The guard makes the in-blocked accounting **leak-proof**: even if
    /// the blocking call returns `Err` via `?` or unwinds, `Drop` still
    /// decrements the counter, so `request_stw`'s `expected` can never
    /// drift permanently low (which would make GC stop waiting for live
    /// mutators).
    ///
    /// `pre_stw` on the returned guard is `true` if a stop-the-world
    /// pause was already in progress at the transition: the caller
    /// should then `arrive_and_wait_auto` *before* parking, because
    /// `request_stw` may have counted this thread in `expected` before
    /// it became blocked.
    pub fn enter_blocked(&self) -> BlockedGuard<'_> {
        // Serialize the transition against `request_stw` (which computes
        // `expected` under the same lock): either our increment lands
        // BEFORE the count read (we are excluded AND `pre_stw` reads the
        // pre-request value `false`, so we park without arriving) or AFTER
        // (we are counted and `pre_stw=true` makes the caller arrive —
        // exactly once). Without the lock the two could interleave so that
        // an EXCLUDED thread arrives anyway; `arrived` is a plain counter,
        // so that spurious arrival releases `wait_for_all` while a counted
        // mutator is still running — a moving GC racing live frames.
        //
        // gcg g2/k: under `CRATONVM_STW_LOCKFREE_BLOCKED` the same two
        // outcomes come from a Dekker pair instead of the lock
        // (`enter_blocked_count_and_read`).
        let pre_stw = self.enter_blocked_count_and_read();
        // P1 shadow record. Deliberately after the lock drop: the shadow
        // registry's own lock must never be taken inside this critical
        // section's remaining span. See `threading::thread_state`.
        thread_state::record_transition(
            ThreadExecState::NativeBlocked,
            "gc_barrier::enter_blocked",
        );
        // Code reclamation: publish what this thread's stack can return into
        // while it is blocked, so a thread parked inside compiled code no longer
        // holds every retired body in the process. Withdrawn first thing in
        // `BlockedGuard::drop` (or `finish_after`).
        crate::jit::conservative_roots::note_blocking_transition_enter();
        crate::jit::conservative_roots::publish_self_block_stamp(true);
        BlockedGuard {
            barrier: self,
            pre_stw,
        }
    }

    /// The serialized half of every blocked-region ENTRY: count the caller in
    /// `threads_blocked` and read `stw_requested` as one observation against
    /// the request's census. Returns `pre_stw`.
    ///
    /// Default: under `inner`, so the census runs entirely before or entirely
    /// after it. gcg g2/k, `CRATONVM_STW_LOCKFREE_BLOCKED`: no lock -- the
    /// caller's `in_blocked_region` store (its deposit, made before this
    /// call) and this load are separated by a `SeqCst` fence, and the request
    /// raises the flag, fences, and only then runs its census
    /// ([`Self::request_flag_then_census_locked`]), so at least one side sees
    /// the other. The anonymous count is diagnostics-only on every production
    /// request (the identity census decides `expected`).
    #[inline]
    fn enter_blocked_count_and_read(&self) -> bool {
        if self.lockfree_blocked {
            self.anon_blocked_in();
            std::sync::atomic::fence(Ordering::SeqCst);
            return self.stw_requested.load(Ordering::Acquire);
        }
        let inner = self.inner.lock();
        self.anon_blocked_in();
        let pre_stw = self.stw_requested.load(Ordering::Acquire);
        drop(inner);
        pre_stw
    }

    /// The plain blocked-region EXIT (`BlockedGuard::drop`,
    /// `mark_blocked_region_leave`): wait out the pause in progress, then
    /// leave the anonymous count.
    ///
    /// gcg g2/k, `CRATONVM_STW_LOCKFREE_BLOCKED`: when no pause is observed
    /// the count is released without the lock. Sound because nothing about
    /// the caller's census class changes here: its `in_blocked_region` (the
    /// identity the census reads) is dropped later, under `inner`, by the
    /// flagged leave, so a pause whose flag store this load missed still
    /// excludes it and is waited out there -- exactly as a pause requested
    /// just after the locked version's unlock would be.
    #[inline]
    fn leave_blocked_count(&self) {
        if self.lockfree_blocked && !self.stw_requested.load(Ordering::Acquire) {
            self.anon_blocked_out();
            return;
        }
        let mut inner = self.inner.lock();
        self.wait_out_pause_locked(&mut inner);
        self.anon_blocked_out();
    }

    /// T19.H1 — non-guard variant of `enter_blocked` for native methods
    /// that open a blocking region via `NativeContext::begin_blocking_region`
    /// (e.g. `ReferenceQueue.remove`'s poll loop). Must be balanced by
    /// exactly one `mark_blocked_region_leave`.
    ///
    /// Returns `true` if a stop-the-world pause is already in progress
    /// (caller should `arrive_and_wait_auto`).
    pub fn mark_blocked_region_enter(&self) -> bool {
        // Serialized against `request_stw` — see `enter_blocked` for the
        // exact-counting rationale.
        let pre_stw = self.enter_blocked_count_and_read();
        // P1 shadow record. NOTE: unlike every `NativeContextImpl` caller,
        // `jni::host_thread_enter_native` reaches here WITHOUT a preceding
        // `deposit_root_snapshot`; its identity flag (`in_blocked_region`) is
        // raised just before this call by
        // `ThreadRegistry::mark_native_thread_blocked` (with an empty
        // snapshot: a thread in HOST code holds no Java roots). gcj j1/cm
        // corrected "without raising `in_blocked_region`", false since that
        // call was added. The shadow state follows this counter -- see
        // `docs/threading/thread-transition-states.md`, §"Unsound or
        // unmodelled transitions", item 1.
        thread_state::record_transition(
            ThreadExecState::NativeBlocked,
            "gc_barrier::mark_blocked_region_enter",
        );
        // Code reclamation — see `enter_blocked`. Withdrawn by
        // `mark_blocked_region_leave_after`.
        crate::jit::conservative_roots::note_blocking_transition_enter();
        crate::jit::conservative_roots::publish_self_block_stamp(true);
        pre_stw
    }

    /// T19.H1 — end a region opened by `mark_blocked_region_enter`.
    ///
    /// Exact-counting contract (see `enter_blocked`): a thread may NOT
    /// transition blocked→running while a stop-the-world pause is active —
    /// it was EXCLUDED from that pause's `expected`, so the initiator will
    /// not wait for it, and arriving would inflate `arrived` for someone
    /// else's quota. Instead we wait the pause out while still counted as
    /// blocked (the GC maintains our roots via
    /// `fold_pointer_map_into_blocked`), and only then decrement — any
    /// LATER pause counts us in `expected` and we arrive exactly once via
    /// `check_post_block_gc`.
    pub fn mark_blocked_region_leave(&self) {
        // `mark_blocked_region_leave_after(|| {})`, with the lock-free fast
        // path of `leave_blocked_count` (identical when the switch is off).
        crate::jit::conservative_roots::publish_self_block_stamp(false);
        crate::jit::conservative_roots::note_blocking_transition_leave();
        self.leave_blocked_count();
    }

    /// End a region opened by `mark_blocked_region_enter` after running `f`
    /// while holding the barrier transition lock.
    ///
    /// Use this when leaving the blocked population is coupled to another
    /// scheduler-visible state change, such as marking a thread dead. The two
    /// changes then serialize as one observation against `request_stw_counted`.
    pub fn mark_blocked_region_leave_after<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        // Withdraw the blocked-stack summary before anything else: from here on
        // this thread may run compiled code again.
        crate::jit::conservative_roots::publish_self_block_stamp(false);
        crate::jit::conservative_roots::note_blocking_transition_leave();
        let mut inner = self.inner.lock();
        self.wait_out_pause_locked(&mut inner);

        struct LeaveOnDrop<'a>(&'a GcBarrier);

        impl Drop for LeaveOnDrop<'_> {
            fn drop(&mut self) {
                self.0.anon_blocked_out();
            }
        }

        let leave = LeaveOnDrop(self);
        f();
        drop(leave);
    }

    /// [`Self::mark_blocked_region_leave_after`] for a thread being torn down
    /// by its own thread-local destructors: a foreign attachment whose OS
    /// thread exited without `DetachCurrentThread`, reaped by
    /// `jni::ForeignThreadBox`'s drop (gen r5w1/crash5).
    ///
    /// The same serialization -- wait out the pause in progress, then run `f`
    /// and leave the blocked population under the barrier lock -- with every
    /// thread-local access removed. `wait_out_pause_locked` records a shadow
    /// state transition (`thread_state::current_state`, a `LocalKey::with`) and
    /// `note_blocking_transition_leave` reads the JIT quiescence record; in a
    /// destructor either key may already be destroyed, `with` then panics, and
    /// a panic out of a thread-local destructor aborts the process. The caller
    /// is on its exit path, not inside a collection, so it is never the
    /// initiator whose own pause this would wait for; and its JIT quiescence
    /// record dies with the thread.
    ///
    /// Contract as for `mark_blocked_region_leave_after`: the caller is counted
    /// in `threads_blocked` (it is idle in the blocked region), and `f` must
    /// neither take this barrier's lock nor block.
    pub(crate) fn leave_blocked_region_at_thread_teardown<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        let mut inner = self.inner.lock();
        if self.stw_requested.load(Ordering::Acquire) {
            let gen = self.gc_generation.load(Ordering::Acquire);
            while self.gc_generation.load(Ordering::Acquire) == gen {
                self.gc_complete.wait(&mut inner);
            }
        }
        f();
        self.anon_blocked_out();
        drop(inner);
    }

    /// Run `f` with no stop-the-world pause in progress and none able to
    /// begin: wait out the active pause (if any), then run `f` holding the
    /// barrier transition lock, which every pause request takes before it
    /// raises `stw_requested`.
    ///
    /// For a thread the STW census EXCLUDES (its `in_blocked_region` is
    /// raised) that must touch the heap anyway — JNI `MonitorEnter` /
    /// `MonitorExit` from an idle foreign-attached or host-native thread CAS
    /// an object's mark word, and no pause waits for such a thread, so without
    /// this a moving collection could relocate the object under the CAS
    /// (`r11w4-sync-jni-excluded-monitor-ops-touch-a-movable-header`).
    ///
    /// Contract, each clause load-bearing:
    /// * the caller must be excluded from every pause that can be active: a
    ///   COUNTED thread that waits here is its own pause's missing arrival
    ///   (deadlock);
    /// * `f` must not take this barrier's lock (it is not re-entrant) and must
    ///   not block: every blocked-region transition in the VM queues behind
    ///   it. Lock order is barrier -> whatever `f` takes, the same order
    ///   `mark_blocked_region_leave_after`'s closures and the census use.
    pub(crate) fn with_no_pause_in_progress<R>(&self, f: impl FnOnce() -> R) -> R {
        let mut inner = self.inner.lock();
        self.wait_out_pause_locked(&mut inner);
        f()
    }

    /// Is a stop-the-world pause requested or in progress now? A hint: the
    /// answer may be stale by the time the caller acts on it.
    pub(crate) fn stop_requested(&self) -> bool {
        self.stw_requested.load(Ordering::Acquire)
    }

    /// Run `f` holding the barrier transition lock, WITHOUT waiting out the
    /// pause in progress (interpreter round i2, the i53 retirement): while it
    /// runs no pause can be requested, no counted thread can arrive and the
    /// JIT take-over cannot reduce a quota, so a pause already requested
    /// cannot get past its wait for the counted threads -- and one that got
    /// past it before `f` began is collecting with every counted thread
    /// arrived. For the JDWP reader of a thread running a native that such a
    /// pause counts and waits for (`interpreter::publish_frames_of_native_thread_in_stop`).
    ///
    /// Contract, as for [`Self::with_no_pause_in_progress`]: the caller is no
    /// registered mutator a pause counts (it would be its own pause's missing
    /// arrival), and `f` neither takes this barrier's lock nor blocks -- every
    /// blocked-region transition in the VM queues behind it. Lock order is
    /// barrier -> whatever `f` takes.
    pub(crate) fn with_transition_lock<R>(&self, f: impl FnOnce() -> R) -> R {
        let _inner = self.inner.lock();
        f()
    }

    /// Wait out the CURRENTLY-active stop-the-world pause (if any), keyed on the
    /// GC generation rather than the shared `stw_requested` flag.
    ///
    /// CRIT (multi-thread STW deadlock): a plain `while stw_requested` loop here
    /// would stall a thread across DISTINCT pauses — when the pause it entered on
    /// completes and a new one begins before it wakes, it observes `stw_requested`
    /// set again and waits forever, even though its own pause is long over. (At
    /// Churn shutdown this stranded the main thread in `BlockedGuard::drop` while
    /// the last worker's `System.gc` initiator waited on a thread that never
    /// arrived.) We instead capture the active pause's generation and return the
    /// instant it advances. If no pause is active we return immediately (the
    /// generation would otherwise never change → its own deadlock).
    ///
    /// gci i6/jr: a method of the barrier (it was handed the three fields), so
    /// the wait is counted in `waiters_in_wait` and the resume census.
    #[inline]
    fn wait_out_pause_locked(&self, inner: &mut parking_lot::MutexGuard<'_, GcBarrierInner>) {
        let gc_generation = &self.gc_generation;
        if !self.stw_requested.load(Ordering::Acquire) {
            return;
        }
        // The initiator must never wait out its OWN pause: the generation it
        // would wait for is advanced only by its own `complete_gc`, so the
        // loop below could never end. `arrive_and_wait_inner` and
        // `leave_blocked_region_flagged` always had this short-circuit; the
        // two blocked-region LEAVE paths (`BlockedGuard::drop`,
        // `mark_blocked_region_leave_after`) did not, so an initiator that
        // opened and closed a blocked region inside its own pause (any
        // `enter_blocked` reached from code the initiator runs between
        // `request_stw*` and `complete_gc`) would hang the whole VM with every
        // mutator parked, silently. No such path is known to exist today; the
        // guard makes one a no-op instead of a hang. Keyed on the OS thread,
        // as `is_initiator_locked` is, because these paths are given no id.
        if inner.initiator_os.is_some() && inner.initiator_os == current_thread_token() {
            return;
        }
        // P1 shadow record: this is a genuine parked window — the caller is
        // still counted blocked, executes no code, and its frames are
        // maintained by `fold_pointer_map_into_blocked`. Restore whatever the
        // caller was in afterwards rather than guessing, so the recorder never
        // invents a transition the code does not perform.
        let resume_state = thread_state::current_state();
        thread_state::record_transition(
            ThreadExecState::SafepointParked,
            "gc_barrier::wait_out_pause_locked",
        );
        let gen = gc_generation.load(Ordering::Acquire);
        self.note_wait_begin();
        while gc_generation.load(Ordering::Acquire) == gen {
            self.gc_complete.wait(inner);
        }
        // gci i6/jr: no thread id on these paths.
        self.note_wait_end(None, gen);
        thread_state::record_transition(resume_state, "gc_barrier::wait_out_pause_locked:resume");
    }

    /// Number of threads currently parked in a blocking native. Used by
    /// diagnostics and by the watchdog's hang report. Always 0 under
    /// `CRATONVM_STW_NO_BLOCKED_COUNTER` ([`Self::anon_blocked_counted`]); the
    /// identity census (`in_blocked_region` over alive registry entries) is
    /// the authoritative blocked set either way.
    pub fn blocked_count(&self) -> u64 {
        self.threads_blocked.load(Ordering::Acquire)
    }

    /// May any thread be in a blocked region? A GATE for work that is owed
    /// only to blocked threads (the take-over's helper-window pass), never a
    /// count.
    ///
    /// gcg g4/xt: that gate read `blocked_count() > 0`, which is always false
    /// under `CRATONVM_STW_NO_BLOCKED_COUNTER` (the anonymous counter is not
    /// maintained), so with that switch on the helper-window pass never ran
    /// and a blocked peer's compiled frames were in no root set. With the
    /// counter off this answers `true` and the caller's identity census (the
    /// registry's `blocked_os_tids`) decides who is blocked.
    pub fn may_have_blocked_threads(&self) -> bool {
        !self.anon_blocked_counted || self.threads_blocked.load(Ordering::Acquire) > 0
    }

    /// Finding 1(a/c) — atomically leave the blocked-region FLAG state:
    /// drain every active stop-the-world pause, then clear the caller's
    /// `in_blocked_region` flag under the SAME barrier-lock hold that
    /// confirmed no pause is active.
    ///
    /// Why this must be one atomic step: every pause requested while the flag
    /// was up EXCLUDED the caller from `expected` (identity census), so the
    /// caller may not resume bytecode while such a pause is mid-collection —
    /// but with a plain `store(false)` after a drain loop, a pause requested
    /// in the window between the drain's last check and the store both
    /// excludes the thread AND lets it run: the collector relocates objects
    /// under its live frames (finding 1's corruption family). Clearing under
    /// the lock closes the window exactly: a census serialized before the
    /// clear sees the flag up (excluded — we wait it out below), one
    /// serialized after sees it down (counted — the pause waits for our next
    /// safepoint arrival, so the caller's post-clear fixup application can
    /// never race a fold).
    ///
    /// While a pause is active we arrive with the census-aware (`auto`)
    /// participation rule: identity-census pauses excluded us (flag up) — we
    /// only wait; a legacy anonymous-count pause counted us (our
    /// `threads_blocked` slot was already released by the guard drop) — we
    /// arrive exactly once for it. Waiting is generation-keyed per pause
    /// (see `wait_out_pause_locked`'s rationale), and the re-check after each
    /// pause happens under the reacquired lock, so back-to-back pauses are
    /// each classified exactly.
    ///
    /// Starvation note: under continuous GC pressure this can wait out
    /// several consecutive pauses (each finite). gci i6/jr: the mutex's
    /// eventual fairness, which this note used to cite as the bound, does not
    /// bound that: each new request is its own lock acquisition, and a woken
    /// waiter that re-takes the lock after a request raised the next flag
    /// waits the whole next pause; nothing limits how many in a row. The
    /// resume census counts it (`leave_repeats`, `starved_requests`), and
    /// `CRATONVM_STW_RESUME_FAIRNESS` (opt-in) lets the waiter out first
    /// (`resume_fairness_hold`). The parked WIP variant additionally applied the blocked
    /// fixup BEFORE this synchronization (racing the next pause's fold),
    /// which is the corruption this ordering eliminates.
    pub fn leave_blocked_region_flagged(
        &self,
        tid: ThreadId,
        flag: &std::sync::atomic::AtomicBool,
    ) {
        let _ = self.leave_blocked_region_flagged_if(tid, flag, || true);
    }

    /// [`Self::leave_blocked_region_flagged`], but the flag is cleared only if
    /// `clear_now` answers `true` when it is asked -- under the barrier lock,
    /// with no pause in progress and none able to begin -- and is otherwise
    /// left UP (returns `false`), for the caller to leave through its full
    /// wake path (`check_post_block_gc`).
    ///
    /// gcd d5/f: the in-native JNI leave (`native::jni`) asks "did any pause
    /// complete, or any fold target this thread, since it went into native?"
    /// here. The answer cannot change before the flag drops, because pauses
    /// begin only under this lock and folds run only inside pauses; so a `true`
    /// proves the thread's frames, locals and snapshot are exactly as it
    /// deposited them, and the fixup application and snapshot refresh of the
    /// full wake have nothing to do. A pause waited out here makes the next
    /// question answer `false`.
    pub(crate) fn leave_blocked_region_flagged_if(
        &self,
        tid: ThreadId,
        flag: &std::sync::atomic::AtomicBool,
        clear_now: impl FnOnce() -> bool,
    ) -> bool {
        // Asked at most once (the only call is followed by a return).
        let mut clear_now = Some(clear_now);
        // P1 shadow record: this entry is always self-called for the caller's
        // own `tid`, so it is a safe binding point for a thread that has not
        // yet named itself to the recorder.
        thread_state::bind_current_thread(tid.0);
        let mut inner = self.inner.lock();
        // gci i6/jr: pauses this one leave has waited out so far.
        let mut drained = 0u32;
        loop {
            if !self.stw_requested.load(Ordering::Acquire) || Self::is_initiator_locked(&inner, tid)
            {
                if !clear_now.take().is_some_and(|ask| ask()) {
                    return false;
                }
                flag.store(false, Ordering::Release);
                // This is the authoritative blocked -> running edge: the
                // identity flag drops under the same lock hold that proved no
                // pause is active. Recorded as `JavaRunning`; a caller that is
                // really resuming inside a native (`end_blocking_region_refs`)
                // records `NativeRunning` at its own next transition, and
                // `JavaRunning -> NativeRunning` is itself a tabled edge, so
                // the approximation cannot cascade into a false violation.
                thread_state::record_transition(
                    ThreadExecState::JavaRunning,
                    "gc_barrier::leave_blocked_region_flagged",
                );
                return true;
            }
            // A pause is active. Arrive for it exactly once if its census
            // counted us (auto rule); either way wait its generation out.
            // Wave 58 (lane L6): nor when it excused us in a running native.
            let participating = !inner.excluded_blocked.contains(&tid.0)
                && !Self::excused_running_locked(&inner, tid);
            let arrival_gen = self.gc_generation.load(Ordering::Acquire);
            if participating {
                inner.arrived += 1;
                Self::note_counted_arrival_locked(&mut inner, tid);
                if stw_census_dbg() {
                    eprintln!(
                        "[stw-arrive] tid={} gen={} arrived={} expected={} (leave-blocked drain)",
                        tid.0, arrival_gen, inner.arrived, inner.expected
                    );
                }
                if inner.arrived >= inner.expected {
                    self.notify_quota_met();
                }
            }
            // P1 shadow record: still flagged blocked, but genuinely parked
            // for the duration of this pause.
            thread_state::record_transition(
                ThreadExecState::SafepointParked,
                "gc_barrier::leave_blocked_region_flagged:drain",
            );
            // gci i6/jr: a second (third, ...) pause in the same leave is the
            // blocked-region half of the starvation the resume census counts.
            if drained > 0 {
                self.resume_census[RC_LEAVE_REPEATS].fetch_add(1, Ordering::Relaxed);
            }
            drained = drained.saturating_add(1);
            self.note_wait_begin();
            while self.gc_generation.load(Ordering::Acquire) == arrival_gen {
                self.gc_complete.wait(&mut inner);
            }
            self.note_wait_end(Some(tid.0), arrival_gen);
            thread_state::record_transition(
                ThreadExecState::NativeBlocked,
                "gc_barrier::leave_blocked_region_flagged:drained",
            );
            // Lock reacquired by the condvar — reclassify from the top.
        }
    }

    /// gcd d10/j: [`Self::mark_blocked_region_enter`] for a Java thread going
    /// INTO NATIVE for a JNI native call or back after a JNIEnv function
    /// (`native::jni`, `CRATONVM_JNI_NATIVE_TRANSITIONS`). The same
    /// serialized count and the same `pre_stw` answer, WITHOUT the
    /// blocked-stack summary of code reclamation
    /// (`conservative_roots::note_blocking_transition_enter`, a walk of the
    /// stack band down to the outermost compiled entry, paid on every JNI
    /// bracket).
    ///
    /// Without a summary the code-reclamation drain treats the thread as
    /// running compiled code (`classify_thread_quiescence`: no valid summary
    /// means "may return into anything published since its quiescent
    /// generation"), which is how it treats a thread in a JNI native with the
    /// flag off: retired bodies wait for the native to return, nothing is
    /// freed under it. Pair with exactly one [`Self::leave_in_native_if`].
    ///
    /// JIT round 16 wave 9 (lane ffm): also the entry of an FFM downcall's
    /// quiet in-native region (`NativeContextImpl::begin_native_region_quiet`,
    /// `CRATONVM_FFM_DOWNCALL_IN_NATIVE_QUIET`), with the same pairing.
    pub(crate) fn mark_in_native_enter(&self) -> bool {
        let pre_stw = self.enter_blocked_count_and_read();
        thread_state::record_transition(
            ThreadExecState::NativeBlocked,
            "gc_barrier::mark_in_native_enter",
        );
        pre_stw
    }

    /// gcd d10/j: leave what [`Self::mark_in_native_enter`] entered:
    /// `mark_blocked_region_leave` followed by
    /// [`Self::leave_blocked_region_flagged_if`], in ONE lock hold when no
    /// pause is in progress (every JNI bracket paid two). Same answer and same
    /// end states: `true`, the count released and `flag` cleared because
    /// `clear_now` said so under the lock; `false`, the count released and
    /// `flag` left up for the caller's full wake (`check_post_block_gc`).
    ///
    /// Why one hold is the two-step with one interleaving removed: with no
    /// pause in progress the first step only decrements the count and the
    /// second only asks `clear_now` and clears; nothing may happen between
    /// them that the lock does not already exclude (a pause request takes
    /// this lock first). With a pause in progress it IS the two-step: wait the
    /// pause out while still counted blocked, release the count, then the
    /// flagged leave -- and no summary to withdraw (none was published).
    pub(crate) fn leave_in_native_if(
        &self,
        tid: ThreadId,
        flag: &std::sync::atomic::AtomicBool,
        clear_now: impl FnOnce() -> bool,
    ) -> bool {
        thread_state::bind_current_thread(tid.0);
        let inner = self.inner.lock();
        self.leave_in_native_locked(inner, tid, flag, clear_now, false)
    }

    /// gci i20/jt: does this barrier run the flag-first request order (the
    /// lock-free blocked transitions, `CRATONVM_STW_LOCKFREE_BLOCKED`, default
    /// ON)? [`Self::leave_in_native_lockfree_if`] is lock-free only then; the
    /// JNI phase census counts its engagement by it.
    #[inline]
    pub(crate) fn lockfree_blocked_enabled(&self) -> bool {
        self.lockfree_blocked
    }

    /// gci i20/jt (`gci-i19jt-jni-calls-serialise-on-the-barrier-lock-FIXED-20261017`
    /// item 1, `CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE`, default OFF, latched in
    /// `native::jni`): [`Self::leave_in_native_if`] WITHOUT the barrier lock
    /// when no pause is in progress -- HotSpot's `_thread_in_native ->
    /// _thread_in_native_trans` shape: one store, one fence, one load. Same
    /// answers and same end states as the locked leave.
    ///
    /// `leaving` is the caller's `GcBlockState::in_native_leaving`, the word the
    /// production census reads BEFORE `in_blocked_region`
    /// (`GcBlockState::census_blocked`: a thread whose `leaving` is up is
    /// COUNTED, whatever its flag says).
    ///
    /// # Why it is sound (the Dekker pair, and why the flag itself is not the word)
    ///
    /// The leaver stores `leaving = true`, fences `SeqCst`, loads
    /// `stw_requested`. A request (flag-first order,
    /// [`Self::request_flag_then_census_locked`]) stores `stw_requested`,
    /// fences `SeqCst`, then reads each thread's `leaving` and only then its
    /// flag. At least one side sees the other:
    ///
    /// * the leaver saw no request: every pause whose census runs from here on
    ///   reads `leaving == true` (or, after the leaver's Release reset, the
    ///   flag already down) and COUNTS it -- so no pause can collect before the
    ///   thread's next safepoint arrival, and the flag may drop with no lock.
    ///   `clear_now` is asked AFTER the load: a pause that excluded the thread
    ///   and completed before the load advanced `gc_generation` (Release, before
    ///   its `stw_requested` lowering, which the load then read), so
    ///   `NativeSync` answers "changed" and the flag stays up for the full wake;
    /// * the leaver saw a request: its flag is still UP -- it is never dropped on
    ///   this path before the lock -- so a census that read `leaving == false`
    ///   excluded a thread whose snapshot and fold stay valid, exactly as the
    ///   locked leave's pause-in-progress branch has them. Under `inner` (the
    ///   census is complete: the request held it throughout) the census'
    ///   verdict is read back from `excluded_blocked`: EXCLUDED, the locked
    ///   leave's own two-step; COUNTED (the census read `leaving == true`), the
    ///   thread ARRIVES through [`Self::leave_blocked_region_flagged_if`]'s
    ///   participating rule -- waiting the pause out while counted would be its
    ///   own pause's missing arrival -- with its flag up, so the pause scans its
    ///   snapshot and folds into its fixup (the `arrive_in_native` shape), and
    ///   its leave then sees the generation move and takes the full wake.
    ///
    /// Why not Dekker on `in_blocked_region` itself (the shape the i19 page
    /// sketched): a census that read the flag UP excludes the thread, and the
    /// flag would then be DOWN, from the leaver's store until it re-raised it,
    /// while that pause's root scan selects blocked threads by the flag -- a
    /// window in which the thread's snapshot and fold are in no root set.
    ///
    /// Requires the flag-first request order (`CRATONVM_STW_LOCKFREE_BLOCKED`,
    /// default ON since dev's flip): with the census-first order a census can
    /// run, read `leaving == false` and the flag up, before the request's flag
    /// store becomes visible, and the leaver's load would miss it. Without it
    /// this IS [`Self::leave_in_native_if`].
    pub(crate) fn leave_in_native_lockfree_if(
        &self,
        tid: ThreadId,
        flag: &std::sync::atomic::AtomicBool,
        leaving: &std::sync::atomic::AtomicBool,
        clear_now: impl FnOnce() -> bool,
    ) -> bool {
        if !self.lockfree_blocked {
            return self.leave_in_native_if(tid, flag, clear_now);
        }
        thread_state::bind_current_thread(tid.0);
        leaving.store(true, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::SeqCst);
        if !self.stw_requested.load(Ordering::Acquire) {
            // Diagnostics only under the flag-first order (the identity census
            // decides `expected`), so released without the lock as
            // `leave_blocked_count` does.
            self.anon_blocked_out();
            if !clear_now() {
                // The flag stays up for the full wake, which classifies any
                // pause that counted the thread meanwhile (`auto` rule).
                leaving.store(false, Ordering::Release);
                return false;
            }
            // Flag first, then the reset (Release): a census that reads the
            // reset `leaving` reads this flag down too.
            flag.store(false, Ordering::Release);
            leaving.store(false, Ordering::Release);
            thread_state::record_transition(
                ThreadExecState::JavaRunning,
                "gc_barrier::leave_in_native_lockfree_if",
            );
            return true;
        }
        let inner = self.inner.lock();
        // Every census that can still read it runs under `inner`, after this.
        leaving.store(false, Ordering::Release);
        self.leave_in_native_locked(inner, tid, flag, clear_now, true)
    }

    /// The body of [`Self::leave_in_native_if`] (and of
    /// [`Self::leave_in_native_lockfree_if`]'s path that saw a request), under
    /// the barrier lock the caller took. `census_may_count`: the caller's
    /// `in_native_leaving` was up while a census could read it, so the pause
    /// in progress may have COUNTED the caller; it then arrives (flag up)
    /// instead of waiting the pause out. `false` (the locked leave): every
    /// pause in progress excluded it (its flag was up and nothing else made
    /// it countable), today's two-step.
    fn leave_in_native_locked(
        &self,
        mut inner: parking_lot::MutexGuard<'_, GcBarrierInner>,
        tid: ThreadId,
        flag: &std::sync::atomic::AtomicBool,
        clear_now: impl FnOnce() -> bool,
        census_may_count: bool,
    ) -> bool {
        if !self.stw_requested.load(Ordering::Acquire) {
            self.anon_blocked_out();
            if !clear_now() {
                return false;
            }
            flag.store(false, Ordering::Release);
            drop(inner);
            thread_state::record_transition(
                ThreadExecState::JavaRunning,
                "gc_barrier::leave_in_native_if",
            );
            return true;
        }
        // gci i20/jt: the pause in progress counted a lock-free leaver (its
        // census read `in_native_leaving` up). It must arrive -- through the
        // flagged leave's participating rule, which counts it once, waits the
        // pause out and leaves the flag up for the full wake (the generation
        // moved, so `clear_now` answers false there).
        if census_may_count
            && !Self::is_initiator_locked(&inner, tid)
            && !inner.excluded_blocked.contains(&tid.0)
            && !Self::excused_running_locked(&inner, tid)
        {
            self.note_in_native_leave_wait();
            let leave_done = InNativeLeaveDone(self);
            self.anon_blocked_out();
            drop(inner);
            let cleared = self.leave_blocked_region_flagged_if(tid, flag, clear_now);
            drop(leave_done);
            return cleared;
        }
        // A pause is in progress: `mark_blocked_region_leave`'s half, under
        // this same hold, then the flagged leave (which takes the lock again
        // and classifies the next pause, if one began, from the top).
        //
        // gci i15/hx: counted as an in-native leaver from here until the
        // leave is done, so that a request made after this pause completes
        // lets it out first (`in_native_leave_should_hold`); its grant starts
        // when the leave is done and also covers the caller's full wake.
        //
        // gci i15/rb (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
        // finding 2): the "done" half runs from a drop guard, so an unwind out
        // of the wait or the flagged leave (a panicking `clear_now`, the
        // `CRATONVM_STRESS_THREAD_STATES` tripwire in `record_transition`)
        // cannot leave `in_native_leavers` raised for good -- which would hold
        // EVERY later request of this VM for the full bound.
        self.note_in_native_leave_wait();
        let leave_done = InNativeLeaveDone(self);
        self.wait_out_pause_locked(&mut inner);
        self.anon_blocked_out();
        drop(inner);
        let cleared = self.leave_blocked_region_flagged_if(tid, flag, clear_now);
        drop(leave_done);
        cleared
    }

    /// gch h2/jr (`gch-h1jw-proposal-in-native-arrival-without-waiting-20261008`,
    /// `CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT`, default off, latched in
    /// `native::jni`): the arrival of a thread going INTO native for a JNI
    /// native call, for a pause that raced its entry, WITHOUT parking.
    ///
    /// The caller is [`Self::mark_in_native_enter`]'s `pre_stw == true` path:
    /// its deposit is complete and its `in_blocked_region` is already up, so
    /// every pause folds its moves into its fixup
    /// (`ThreadRegistry::fold_pointer_map_into_blocked_audited` selects by the
    /// flag, not by the census) and reads its snapshot; the C code it is about
    /// to run touches the heap only after leaving native, which waits out a
    /// pause in progress ([`Self::leave_in_native_if`]), or through a leaf
    /// window, which no pause lets open ([`Self::try_open_leaf_window`]). So
    /// the park of `arrive_and_wait_auto` buys nothing the excluded case does
    /// not already stand on: HotSpot's `_thread_in_Java -> _thread_in_native`
    /// transition does not block for a safepoint in progress either.
    ///
    /// What it does, under `inner`, exactly `arrive_and_wait_inner`'s arrival
    /// half and nothing of its wait:
    ///
    /// 1. no pause in progress, or the caller is the initiator: nothing
    ///    (`false`);
    /// 2. the census counted the caller (not in `excluded_blocked`, not
    ///    excused in a running native): `arrived += 1`, `last_arrival`, and
    ///    the quota notification when it is met;
    /// 3. the caller joins `excluded_blocked` for the REST of this pause
    ///    (cleared by `complete_gc`), so a second arrival of the same thread
    ///    in the same pause -- a JNIEnv function's escalation, the leave's
    ///    flagged drain -- never counts twice and takes the excluded wait;
    /// 4. returns `true` without `note_parked`: the thread never waits for a
    ///    map (it has none to apply; the fold is its channel, as for an
    ///    excluded thread, and its leave sees the generation move and takes
    ///    the full wake).
    ///
    /// A pause the caller arrived at here may complete while the caller is
    /// still in C; one that begins later finds its flag up and excludes it.
    pub(crate) fn arrive_in_native(&self, tid: ThreadId) -> bool {
        thread_state::bind_current_thread(tid.0);
        let mut inner = self.inner.lock();
        if !self.stw_requested.load(Ordering::Acquire) || Self::is_initiator_locked(&inner, tid) {
            return false;
        }
        let participating =
            !inner.excluded_blocked.contains(&tid.0) && !Self::excused_running_locked(&inner, tid);
        if participating {
            inner.arrived += 1;
            Self::note_counted_arrival_locked(&mut inner, tid);
            if stw_census_dbg() {
                eprintln!(
                    "[stw-arrive] tid={} gen={} arrived={} expected={} (in-native, no wait)",
                    tid.0,
                    self.gc_generation.load(Ordering::Acquire),
                    inner.arrived,
                    inner.expected
                );
            }
            if inner.arrived >= inner.expected {
                self.notify_quota_met();
            }
        }
        inner.excluded_blocked.insert(tid.0);
        true
    }

    /// Wait for all expected threads to arrive at the barrier.
    /// Called by the GC initiator after `request_stw`.
    pub fn wait_for_all(&self) {
        let mut inner = self.inner.lock();
        self.adopt_free_coverage_slot_locked(&mut inner);
        while inner.arrived < inner.expected {
            self.all_arrived.wait(&mut inner);
        }
        self.note_quota_met_locked(&mut inner);
    }

    /// Leave a lazy native region atomically with respect to a pause census.
    /// `clear_native` changes the thread from the unclaimed native state to
    /// Java-running. If a pause already owns the barrier, its census sees this
    /// thread as an ordinary mutator and waits for the poll after the native
    /// call returns. If the pause starts after this transition, its census
    /// also sees a Java mutator and waits for that poll.
    pub(crate) fn leave_lazy_native_if(&self, clear_native: impl FnOnce() -> bool) -> bool {
        // Held, never read: the census runs under this lock, so the transition
        // is one observation against it. (gci i1/pp: was `let mut inner`, an
        // unused, needlessly mutable binding.)
        let _inner = self.inner.lock();
        if !clear_native() {
            return false;
        }
        self.stw_requested.load(Ordering::Acquire)
    }

    /// BUG-03 — wait for all expected threads to arrive, but give up after
    /// `dur`. Returns `true` if the barrier was satisfied (`arrived >=
    /// expected`), `false` on timeout.
    ///
    /// Used by the cross-thread STW JIT root scan: the initiator forcibly
    /// stops in-JIT peers (which never arrive cooperatively) and excludes
    /// them via [`reduce_expected`], then loops on this bounded wait to pick
    /// up any thread that entered JIT *after* a previous take-over pass.
    ///
    /// # Windows: an accurate slice (gc-common w3-a, 2026-09-23)
    ///
    /// A `parking_lot` condvar timed wait returns at the first 15.625 ms clock
    /// tick after its deadline on Windows (`jvm_thread::win_park`, measured),
    /// so the take-over loop's 1 ms slice was a 15.6 ms slice: +15.6 ms of TTSP
    /// per missed in-JIT peer, and its round-counted cadence (20 fast rounds,
    /// warn after 64) stretched to ~312 ms and ~1 s. A short wait therefore
    /// waits on [`Self::notify_quota_met`]'s auto-reset event together with a
    /// high-resolution timer instead — still woken at once by the arrival that
    /// meets the quota, now also accurate when none comes. An arrival between
    /// the unlock and the wait leaves the event SET, so the wait returns at
    /// once; a stale signal from an earlier pause costs one spurious, harmless
    /// round. `CRATONVM_WIN_HIRES_PARK=0` (no event) restores the condvar path.
    /// `docs/feature-designs/jdk-only-mode.md`.
    pub fn wait_for_all_timeout(&self, dur: std::time::Duration) -> bool {
        #[cfg(target_os = "windows")]
        {
            if dur < std::time::Duration::from_millis(16) && self.arrival_event != 0 {
                {
                    let mut inner = self.inner.lock();
                    self.adopt_free_coverage_slot_locked(&mut inner);
                    if inner.arrived >= inner.expected {
                        self.note_quota_met_locked(&mut inner);
                        return true;
                    }
                } // released BEFORE the wait: arrivals need this lock.
                if crate::threading::jvm_thread::win_park::wait_until(self.arrival_event, dur) {
                    let mut inner = self.inner.lock();
                    let met = inner.arrived >= inner.expected;
                    if met {
                        self.note_quota_met_locked(&mut inner);
                    }
                    return met;
                }
                // No high-resolution timer on this thread: the condvar below.
            }
        }
        let mut inner = self.inner.lock();
        self.adopt_free_coverage_slot_locked(&mut inner);
        if inner.arrived >= inner.expected {
            self.note_quota_met_locked(&mut inner);
            return true;
        }
        // parking_lot's `wait_for` may wake spuriously; one bounded wait is
        // enough because the caller loops. Re-check the predicate after waking.
        let _ = self.all_arrived.wait_for(&mut inner, dur);
        let met = inner.arrived >= inner.expected;
        if met {
            self.note_quota_met_locked(&mut inner);
        }
        met
    }

    /// BUG-03 — remove `n` threads from the set the initiator is waiting for.
    ///
    /// Called when the initiator has taken over `n` in-JIT peers via OS
    /// suspension (see [`crate::jit::xt_root_scan`]): those threads are now
    /// frozen and conservatively scanned, so they will never arrive at the
    /// barrier and must not be counted in `expected` — otherwise
    /// `wait_for_all` would block on them forever. Saturates at zero and
    /// re-fires `all_arrived` in case the reduced quota is already met.
    pub fn reduce_expected(&self, n: u32) {
        if n == 0 {
            return;
        }
        let mut inner = self.inner.lock();
        let was_met = inner.arrived >= inner.expected;
        inner.expected = inner.expected.saturating_sub(n);
        inner.takeover_excused = inner.takeover_excused.saturating_add(n);
        if inner.arrived >= inner.expected {
            // The quota was completed by this excusal: the pause's last wait
            // was for a frozen peer, not for the last arrival (TTSP
            // attribution — the straggler has no arrival to name).
            if !was_met {
                inner.last_arrival = None;
            }
            self.notify_quota_met();
        }
    }

    /// Signal that GC is complete and threads can resume.
    /// Called by the GC initiator after running collection.
    ///
    /// Stores the pointer map so threads can update their own frames.
    pub fn complete_gc(&self, pointer_map: cratonvm_types::PointerMap) {
        // Built before the lock: an empty map reuses the shared empty `Arc`
        // (most pauses — concurrent-mark pauses, frame-trace pauses, every
        // non-moving cycle — publish nothing), a real one is moved in once.
        let moved = !pointer_map.is_empty();
        let new_map = if moved {
            Arc::new(pointer_map)
        } else {
            Arc::clone(&self.empty_map)
        };
        let mut previous_map;
        {
            let mut inner = self.inner.lock();
            previous_map = std::mem::replace(&mut inner.pointer_map, new_map);
            // Keep the replaced map iff a still-parked waiter needs it: one
            // that arrived for a generation BEFORE the one that map belongs to
            // (it needs `arrival+1 ..= now`). Otherwise it is dropped below,
            // outside the lock, exactly as before. See `map_history`.
            let previous_gen = inner.map_generation;
            if previous_gen > 0
                && inner
                    .min_parked_arrival_gen()
                    .is_some_and(|min_arrival| min_arrival < previous_gen)
            {
                let kept = std::mem::replace(&mut previous_map, Arc::clone(&self.empty_map));
                inner.map_history.push((previous_gen, kept));
            }
            inner.prune_map_history();
            inner.initiator = None;
            inner.initiator_os = None;
            // gci i44/sp: kept for the pause's stopped time below.
            let requested_at = inner.requested_at.take();
            // zt t30/cv: kept for the pause's VM-thread CPU below.
            let request_cpu = inner.request_cpu.take();
            inner.excluded_blocked.clear();
            inner.excused_running.clear();
            // Stamp the map with the generation it belongs to, under the same lock
            // that publishes it. See `Inner::map_generation`.
            inner.map_generation = self.gc_generation.load(Ordering::Acquire) + 1;
            // Interpreter round i2 wave 57, lane L6: before the generation, so
            // a reader that sees the new generation sees this count too.
            if moved {
                self.moving_generation.fetch_add(1, Ordering::Release);
            }
            self.gc_generation.fetch_add(1, Ordering::Release);
            // With the pause count of every dispatch-loop poll word (wave 26,
            // lane L7; see `raise_stw_requested`).
            self.lower_stw_requested();
            // gci i46/lg (rv item 3): whether this call ends a counted pause,
            // whose release instant is the next application interval's base.
            let mut app_base_moves = false;
            if std::mem::take(&mut inner.pause_in_flight) {
                note_pause_end();
                // zt t30/cv: every pause is a VM operation of HotSpot's VM
                // thread (a counted safepoint or not), so each books its
                // initiator's CPU across it.
                //
                // zt pl4 item 3: less the collector work a ZGC or G1
                // collection did on this thread (HotSpot runs it on GC
                // worker threads; its Serial collects on the VM thread, and
                // the Generational heap books none here).
                let collector_work = cratonvm_gc::thread_cpu::take_pause_work_cpu();
                if request_cpu.is_some() {
                    let share =
                        vm_op_cpu_share(request_cpu, current_thread_token(), vm_op_thread_cpu_ns())
                            .saturating_sub(collector_work);
                    self.vm_thread_cpu_ns.fetch_add(share, Ordering::Relaxed);
                }
                if std::mem::take(&mut inner.sp_uncounted) {
                    // gci i46/lg (residual 6): no HotSpot safepoint -- no
                    // count, no stopped time, its sync time taken out of
                    // `sync_ns`, and the application interval runs on across
                    // it (its begin's share was taken back when it was
                    // marked).
                    if inner.ttsp_recorded {
                        self.sp_sync_uncounted_ns.fetch_add(
                            self.ttsp_last_ns.load(Ordering::Relaxed),
                            Ordering::Relaxed,
                        );
                    }
                } else {
                    // gci i44/sp: one completed safepoint and its stopped time
                    // (request accepted -> now, its time-to-safepoint included).
                    self.sp_count.fetch_add(1, Ordering::Relaxed);
                    if let Some(t0) = requested_at {
                        self.sp_stopped_ns
                            .fetch_add(duration_ns(t0.elapsed()), Ordering::Relaxed);
                    }
                    app_base_moves = true;
                }
            }
            // Last use of the per-pause coverage rows is over (the collection
            // and the root fix-up ran before this call): another VM's pause may
            // reset them now. Under `inner`, so a sibling request of THIS
            // barrier that saw itself as the owner re-tries exactly once
            // (`request_stw_opening_cycle`).
            inner.wants_coverage_slot = false;
            if std::mem::take(&mut inner.holds_coverage_slot) {
                self.release_coverage_slot();
            }
            // gci i6/jr: the release instant every waiter's resume latency is
            // measured from (`note_wait_end`). One clock read per pause.
            let released = self.resume_stamp_now();
            // gci i47w/ex47: the thread whose next request is back to back
            // (before the release stamp, so a reader of the stamp sees it).
            self.last_complete_token.store(current_thread_token().unwrap_or(0), Ordering::Release);
            self.released_at_ns.store(released, Ordering::Release);
            if app_base_moves {
                inner.sp_app_since_ns = released;
            }
            self.gc_complete.notify_all();
        }
        // The pause's ledger stops being this thread's (a no-op when another
        // thread completes the pause, or the thread is bound elsewhere).
        cratonvm_gc::gc_quiescence::unbind_pause_ledger(&self.pause_ledger);
        // The previous pause's map (possibly 1M+ entries) is freed HERE, after
        // the lock is released, rather than inside the critical section every
        // waking mutator is queued on. A waiter that still holds a reference
        // keeps it alive until it drops its own `Arc`.
        drop(previous_map);
    }

    /// Called by non-initiator threads at safepoints when STW is active.
    ///
    /// The thread signals its arrival, then waits for GC to complete.
    /// Returns the pointer map for updating this thread's frame references.
    ///
    /// This is the **participating** (counted) entry: the caller is a
    /// running mutator that `request_stw` included in `expected` (a genuine
    /// interpreter safepoint, or a thread whose `enter_blocked` reported
    /// `pre_stw == true` — it became blocked *after* `expected` was
    /// computed, so it was counted). Use [`arrive_and_wait_excluded`] for a
    /// thread that was NOT in `expected` (parked in a blocking native and
    /// excluded by `request_stw`); counting such a thread can satisfy the
    /// `arrived >= expected` quota early and release `wait_for_all` while a
    /// real mutator is still running.
    ///
    /// NOTE: Production callers should use [`Self::arrive_and_wait_auto`], which
    /// resolves participation status automatically against the current pause's
    /// `excluded_blocked` snapshot. `arrive_and_wait` is primarily used in tests
    /// or when participation is known a priori.
    pub fn arrive_and_wait(&self, tid: ThreadId) -> Arc<cratonvm_types::PointerMap> {
        self.arrive_and_wait_inner(tid, Some(true))
    }

    /// Like [`arrive_and_wait`], but for a thread that is **excluded** from
    /// the current STW's `expected` count (it was parked in a blocking
    /// native when `request_stw` ran, so it must not be counted toward
    /// `arrived`).
    ///
    /// B2 fix — the excluded thread still has to *wait out* the pause (so
    /// the moving collector never relocates objects under its live Rust
    /// `ObjectRef`s once it resumes), but it must NOT bump `arrived` or fire
    /// `all_arrived`: doing so can push `arrived` to `expected` while a
    /// counted mutator has not yet reached its safepoint, releasing the
    /// initiator's `wait_for_all` early and letting GC run under a live
    /// mutator.
    pub fn arrive_and_wait_excluded(&self, tid: ThreadId) -> Arc<cratonvm_types::PointerMap> {
        self.arrive_and_wait_inner(tid, Some(false))
    }

    /// GCAUDIT-0711-FIX (finding 1a) — arrival for a caller whose
    /// participation status is NOT locally decidable: any site that may run
    /// either before OR after this thread's own `in_blocked_region` flag
    /// went up (the ambiguity described on [`GcBarrierInner::excluded_blocked`])
    /// must use this instead of guessing via `arrive_and_wait`/
    /// `arrive_and_wait_excluded`.
    ///
    /// Looks up `tid` in the CURRENT pause's `excluded_blocked` snapshot —
    /// populated by `request_stw_counted_locked` atomically with `expected`,
    /// under the same `inner` lock this read takes — so the decision exactly
    /// matches what the census actually did for this pause, race-free. Safe
    /// to use unconditionally in place of `arrive_and_wait`: a caller never
    /// in `excluded_blocked` gets identical (participating) behavior.
    pub fn arrive_and_wait_auto(&self, tid: ThreadId) -> Arc<cratonvm_types::PointerMap> {
        self.arrive_and_wait_inner(tid, None)
    }

    /// Shared core for [`arrive_and_wait`] / [`arrive_and_wait_excluded`] /
    /// [`arrive_and_wait_auto`].
    ///
    /// `mode = Some(true)`/`Some(false)` forces participating/excluded
    /// unconditionally (used only by callers that can PROVE their status
    /// without consulting the pause's exclusion snapshot — see each
    /// wrapper's doc). `mode = None` decides from `excluded_blocked`,
    /// read under the same lock `request_stw_counted_locked` populated it
    /// under, which is race-free by construction.
    fn arrive_and_wait_inner(
        &self,
        tid: ThreadId,
        mode: Option<bool>,
    ) -> Arc<cratonvm_types::PointerMap> {
        // P1 shadow record: always self-called for the caller's own `tid`
        // (see the initiator short-circuit below), so this is the primary
        // binding point for the recorder.
        thread_state::bind_current_thread(tid.0);
        let mut inner = self.inner.lock();
        // If this is the initiator or STW is not active, return immediately
        if !self.stw_requested.load(Ordering::Acquire) || Self::is_initiator_locked(&inner, tid) {
            return Arc::clone(&self.empty_map);
        }
        // The state to restore when this pause ends. The barrier cannot know
        // whether the caller reached here from the interpreter poll, a
        // blocking-region entry, a startup retry loop or compiled code, so it
        // records the parked window and puts the caller back where it was
        // instead of inventing an edge.
        let resume_state = thread_state::current_state();
        // Interpreter round i2 wave 58, lane L6: a thread this pause excused
        // in a running native is not in `expected`, whatever entry it arrives
        // through (a poll reached from inside its native, a block there).
        let participating = mode.unwrap_or_else(|| !inner.excluded_blocked.contains(&tid.0))
            && !Self::excused_running_locked(&inner, tid);
        // Capture the generation of the STW we are arriving for (under the lock,
        // so `complete_gc` — which bumps the generation under the same lock —
        // cannot race between this and the `stw_requested` check above). We wait
        // until THIS generation completes, NOT until `stw_requested` clears.
        //
        // CRIT (multi-thread STW deadlock): `stw_requested` is a single shared
        // flag reused across STW cycles. If a thread arrives for generation G,
        // parks in the wait loop, and generation G completes AND a new
        // generation G+1 starts before this thread wakes, a `while stw_requested`
        // loop observes the flag set again (for G+1) and keeps waiting — yet this
        // thread never `arrive`d for G+1, so G+1's initiator's `wait_for_all`
        // blocks on it forever. Keying on the generation makes the thread return
        // the instant ITS pause ends; it then re-arrives for G+1 at its next
        // safepoint. (Reproduces with 6 threads each looping `System.gc()` —
        // scratch_churn/Churn.java.)
        let arrival_gen = self.gc_generation.load(Ordering::Acquire);
        // Signal arrival — but only for threads the initiator is actually
        // waiting for. An excluded (blocked) thread that wakes mid-STW must
        // not inflate `arrived`: it was never in `expected`, so counting it
        // could satisfy `arrived >= expected` before a counted mutator has
        // arrived and prematurely release `wait_for_all`.
        if participating {
            inner.arrived += 1;
            Self::note_counted_arrival_locked(&mut inner, tid);
            if stw_census_dbg() {
                eprintln!(
                    "[stw-arrive] tid={} gen={} arrived={} expected={}",
                    tid.0, arrival_gen, inner.arrived, inner.expected
                );
            }
            if inner.arrived >= inner.expected {
                self.notify_quota_met();
            }
        }
        thread_state::record_transition(
            ThreadExecState::SafepointParked,
            if participating {
                "gc_barrier::arrive_and_wait_inner:participating"
            } else {
                "gc_barrier::arrive_and_wait_inner:excluded"
            },
        );
        // Wait until THIS pause completes (its generation is published by
        // `complete_gc`), not merely until `stw_requested` clears — see above.
        //
        // Registered as parked for `arrival_gen` for the whole wait, so a
        // `complete_gc` of a LATER pause that runs before this thread wakes
        // keeps the maps this thread still needs (see `map_history`).
        inner.note_parked(arrival_gen);
        // JIT round 19 wave 1 (lane eintr19, FFMEINTR18-5): a timed first
        // wait, then the untimed one; one stderr line per stalled pause.
        let mut report_at = self.stall_report_after.map(|d| Instant::now() + d);
        // gci i6/jr: counted in `waiters_in_wait` from here to the loop's end.
        self.note_wait_begin();
        while self.gc_generation.load(Ordering::Acquire) == arrival_gen {
            match report_at {
                None => self.gc_complete.wait(&mut inner),
                Some(at) => {
                    let timed_out = self.gc_complete.wait_until(&mut inner, at).timed_out();
                    if timed_out && self.gc_generation.load(Ordering::Acquire) == arrival_gen {
                        report_at = None;
                        self.report_parked_waiter_stall(
                            &mut inner,
                            tid,
                            arrival_gen,
                            participating,
                        );
                    }
                }
            }
        }
        // gci i6/jr: resumed (the lock is re-taken; the generation moved).
        self.note_wait_end(Some(tid.0), arrival_gen);
        thread_state::record_transition(resume_state, "gc_barrier::arrive_and_wait_inner:resume");
        // The map this thread is about to apply must describe EVERY pause
        // since it arrived — its own, plus any later one it slept through.
        // The release condition ("the generation moved") and the map stored
        // now are two different facts — see `Inner::map_generation`.
        //
        // Until gc-common w2-a (2026-09-23) this returned whatever map was
        // stored, i.e. the NEWEST pause's: a waiter excluded from pause G+1
        // (a blocked-region arrival — `monitor_wait` remaps the Rust local it
        // is about to wait on with this map; a starting thread applies it to
        // its frames) received G+1's map for pre-G addresses, so everything
        // its own pause G moved stayed stale.
        let maps = inner.maps_for_waiter(arrival_gen);
        inner.note_unparked(arrival_gen);
        match maps {
            WaiterMaps::Own(map) => map,
            WaiterMaps::Straddled(maps) => {
                let generations = maps.len();
                drop(inner);
                self.mapgen_composed.fetch_add(1, Ordering::Relaxed);
                if mapgen_dbg() {
                    eprintln!(
                        "[mapgen] tid={} arrived_for_gen={} woke at gen={} — composed {} pauses' maps",
                        tid.0,
                        arrival_gen + 1,
                        arrival_gen + u64::try_from(generations).unwrap_or(u64::MAX),
                        generations,
                    );
                }
                let composed = compose_pointer_maps(&maps);
                if composed.is_empty() {
                    Arc::clone(&self.empty_map)
                } else {
                    Arc::new(composed)
                }
            }
            WaiterMaps::Missing => {
                // A needed map is no longer held. Cannot happen while every
                // waiter registers in `parked_waiters`; kept as the old
                // behaviour (the newest map) and counted, not asserted, because
                // the old behaviour is what every build before this one did.
                self.mapgen_unresolved.fetch_add(1, Ordering::Relaxed);
                if mapgen_dbg() {
                    eprintln!(
                        "[mapgen] tid={} arrived_for_gen={} but the stored map is gen={} \
                         (map_len={}) and the intermediate maps are gone — this thread's \
                         own pause's relocations are NOT in it",
                        tid.0,
                        arrival_gen + 1,
                        inner.map_generation,
                        inner.pointer_map.len(),
                    );
                }
                // A reference count, not a copy — see `GcBarrierInner::pointer_map`.
                Arc::clone(&inner.pointer_map)
            }
        }
    }

    /// `(composed, unresolved)`: how many barrier waiters woke into a
    /// generation past their own pause and received a composition of every map
    /// since, and how many could not be given one (always 0 by construction;
    /// nonzero is a bug). Diagnostic.
    pub fn late_waiter_map_counters(&self) -> (u64, u64) {
        (
            self.mapgen_composed.load(Ordering::Relaxed),
            self.mapgen_unresolved.load(Ordering::Relaxed),
        )
    }

    /// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-5,
    /// `CRATONVM_STW_STALL_REPORT`): a mutator has been parked at this barrier
    /// for [`Self::stall_report_after`] waiting for pause `arrival_gen` to
    /// end. The first such waiter of the pause prints one line: the barrier's
    /// count, the initiator, and how long ago the pause was requested.
    /// `pending=0` says the quota was met and the INITIATOR is stuck inside
    /// the pause (its own `[stw-stall]` dump covers a wait still short of the
    /// quota). The line is written with the barrier lock RELEASED (a
    /// helper-parked peer may hold stderr's lock), then retaken.
    fn report_parked_waiter_stall(
        &self,
        inner: &mut parking_lot::MutexGuard<'_, GcBarrierInner>,
        tid: ThreadId,
        arrival_gen: u64,
        participating: bool,
    ) {
        let key = arrival_gen.wrapping_add(1);
        if self.stall_reported_gen.swap(key, Ordering::AcqRel) == key {
            return;
        }
        // gci i20/st: the arrived set, sorted (bounded: a stall waits for a
        // few dozen counted threads, never thousands), so the straggler is
        // the watchdog table's unblocked counted entries minus the initiator
        // minus these.
        let arrived_ids = stall_arrived_ids_text(&inner.arrived_ids);
        let line = format!(
            "[stw-stall] thread {} has been parked {:?} at the stop-the-world barrier for pause \
             gen={} (participating={participating}): initiator={:?} requested {:?} ago, \
             expected={} arrived={} pending={} takeover_excused={} excluded_blocked={} \
             excused_running={} last_arrival={:?} arrived_ids={} \
             (CRATONVM_STW_STALL_REPORT=0 silences; \
             CRATONVM_DBG_STW_WATCHDOG_SECS=N dumps every thread)",
            tid.0,
            self.stall_report_after.unwrap_or_default(),
            arrival_gen + 1,
            inner.initiator.map(|t| t.0),
            inner.requested_at.map(|t| t.elapsed()),
            inner.expected,
            inner.arrived,
            inner.expected.saturating_sub(inner.arrived),
            inner.takeover_excused,
            inner.excluded_blocked.len(),
            inner.excused_running.len(),
            inner.last_arrival,
            arrived_ids,
        );
        parking_lot::MutexGuard::unlocked(inner, || eprintln!("{line}"));
    }

    /// Get the number of expected threads still outstanding.
    /// Used for diagnostics/testing.
    pub fn pending_count(&self) -> u32 {
        let inner = self.inner.lock();
        inner.expected.saturating_sub(inner.arrived)
    }
}

impl Default for GcBarrier {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for GcBarrier {
    fn drop(&mut self) {
        // gcj j1/cm: uncount a pause in progress from every registered
        // dispatch-loop poll word. The words are per OS thread
        // (`LOOP_POLL`) and shared with every other VM that thread runs
        // loops for; left counted, every loop on those threads -- in any VM
        // -- took its slow path at every bytecode for the rest of the
        // thread's life. A no-op unless the flag is up (`complete_gc` never
        // ran).
        self.lower_stw_requested();
        // A barrier torn down mid-pause (VM teardown, a test that requested and
        // never completed) must not leave the process-wide in-flight count
        // raised forever — see `STW_PAUSES_IN_FLIGHT`.
        if std::mem::take(&mut self.inner.get_mut().pause_in_flight) {
            note_pause_end();
        }
        // Same for the coverage slot: every other VM's next request would
        // otherwise wait out `COVERAGE_SLOT_WAIT_LIMIT` on every pause, forever.
        self.inner.get_mut().holds_coverage_slot = false;
        self.release_coverage_slot();
        // And for the requester's pause-ledger binding (gc-common w21-e), when
        // the barrier is dropped on the thread that requested the pause:
        // `complete_gc` never ran to unbind it, so that thread would go on
        // writing its moving-young coverage rows (`gc_quiescence::CoverageCycle`)
        // into a dead VM's ledger -- which no collector reads -- instead of the
        // orphan rows every VM's read includes, and would keep the ledger alive.
        // A no-op on any other thread and on a thread bound elsewhere.
        cratonvm_gc::gc_quiescence::unbind_pause_ledger(&self.pause_ledger);
        #[cfg(target_os = "windows")]
        crate::threading::jvm_thread::win_park::close(self.arrival_event);
    }
}

/// T19.H1 — RAII guard returned by [`GcBarrier::enter_blocked`].
///
/// Holding the guard means "this thread is parked in a blocking native
/// and is GC-safe". Dropping it (normal return, `?` early-return, or
/// unwind) decrements the barrier's blocked-thread count, so the
/// accounting can never leak.
#[must_use = "dropping the guard immediately ends the blocked state"]
pub struct BlockedGuard<'a> {
    barrier: &'a GcBarrier,
    /// `true` if a stop-the-world pause was already active when the
    /// thread entered the blocked state. The caller should arrive at the
    /// barrier before parking — see `GcBarrier::enter_blocked`.
    pub pre_stw: bool,
}

impl<'a> BlockedGuard<'a> {
    /// Finish this blocked region after running `f` while holding the same
    /// barrier transition lock used by `request_stw_counted`.
    ///
    /// This is for transitions that must be observed atomically with leaving
    /// the blocked population. Thread termination uses it to flip
    /// `alive=false` and decrement `threads_blocked` as one state change; a GC
    /// initiator can no longer see a dead thread still included in the blocked
    /// count and under-estimate `expected`.
    pub fn finish_after<F>(self, f: F)
    where
        F: FnOnce(),
    {
        let this = std::mem::ManuallyDrop::new(self);
        this.barrier.mark_blocked_region_leave_after(f);
    }
}

impl Drop for BlockedGuard<'_> {
    fn drop(&mut self) {
        // Withdraw the blocked-stack summary `enter_blocked` published before
        // anything else: from here on this thread may run compiled code again.
        crate::jit::conservative_roots::publish_self_block_stamp(false);
        crate::jit::conservative_roots::note_blocking_transition_leave();
        // Checked leave — identical contract to `mark_blocked_region_leave`:
        // wait out the active stop-the-world pause we were excluded from
        // (arriving or running would both be wrong) before re-entering the
        // mutator population. Keyed on the GC generation, NOT `stw_requested`,
        // so a back-to-back new pause cannot strand this thread forever — see
        // `GcBarrier::wait_out_pause_locked`. gcg g2/k: lock-free when no
        // pause is observed under `CRATONVM_STW_LOCKFREE_BLOCKED`.
        self.barrier.leave_blocked_count();
    }
}

impl std::fmt::Debug for GcBarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcBarrier")
            .field("stw_active", &self.stw_requested.load(Ordering::Relaxed))
            .field("generation", &self.gc_generation.load(Ordering::Relaxed))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A pause must be VISIBLE to a sampler that is not the initiator and holds
    /// no barrier handle.
    ///
    /// This is the contract `virtual_threads::spawn_starvation_watchdog` rests
    /// on. That watchdog grows the carrier pool when `dispatch_count` has not
    /// moved since its last sample — and a stop-the-world pause parks every
    /// carrier, so `dispatch_count` CANNOT move across one. Without a way to
    /// tell "stopped" from "starved" it grew the pool once per pause, and each
    /// added carrier is one more OS thread for the next pause to stop (and, on
    /// Windows, one more for `xt_root_scan::take_over_pass` to suspend and
    /// resume): slower pause -> more stalled samples -> more carriers. Measured
    /// on `VthreadGcStress`, the pool ran from its base 32 to 233 and
    /// `dispatch_count` froze permanently.
    ///
    /// # What is and is not assertable here
    ///
    /// [`STW_PAUSE_EPOCH`] and [`STW_PAUSES_IN_FLIGHT`] describe THE VM, of
    /// which production has exactly one — but this test binary builds many
    /// `GcBarrier`s and runs their tests in parallel, so a sibling pause can
    /// advance the epoch between any two lines below. Only MONOTONICITY of the
    /// epoch survives that, and monotonicity is what the watchdog needs:
    /// it compares the epoch against its own previous sample and asks whether
    /// it changed, never what it changed by. The per-instance `stw_requested`
    /// is asserted alongside it because that one IS deterministic here.
    #[test]
    fn a_pause_is_visible_to_a_sampler_with_no_barrier_handle() {
        let barrier = GcBarrier::new();
        let (epoch_before, _) = stw_pause_state();

        assert!(
            barrier.request_stw(ThreadId(1), 1),
            "the first request on a fresh barrier must be accepted",
        );
        let (epoch_during, in_progress) = stw_pause_state();
        // Deterministic since w3-a: an in-flight COUNT cannot be cleared by a
        // sibling barrier's `complete_gc` while this pause is outstanding.
        assert!(
            in_progress,
            "a sibling VM's pause ending must not report this VM's pause as over"
        );
        assert!(
            epoch_during > epoch_before,
            "requesting a pause must advance the global epoch, or a sampler holding no handle to this barrier cannot tell a stopped VM from a starved one (epoch {epoch_before} -> {epoch_during})",
        );
        assert!(
            barrier.stw_requested.load(Ordering::Acquire),
            "the per-instance flag must be set while the pause is outstanding",
        );

        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(
            !barrier.stw_requested.load(Ordering::Acquire),
            "completing the collection must clear the per-instance flag",
        );
        let (epoch_after, _) = stw_pause_state();
        assert!(
            epoch_after >= epoch_during,
            "the epoch counts pauses STARTED and must never go backwards",
        );
    }

    #[test]
    fn barrier_no_stw_by_default() {
        let barrier = GcBarrier::new();
        assert!(!barrier.stw_requested.load(Ordering::Relaxed));
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 0);
    }

    /// Interpreter round i1 wave 26, lane L7: the dispatch loop polls its
    /// thread's `LoopPollWord`, not `stw_requested`. A request counts its
    /// pause in every registered word together with the flag, `complete_gc`
    /// uncounts it together with the flag, a word registered mid-pause is
    /// counted once, a re-registration never counts twice, and a move count
    /// shares the word and survives all of it.
    #[test]
    fn a_pause_is_counted_in_every_registered_loop_poll_word() {
        let pauses = |h: &LoopPollHandle| h.word().load() & LoopPollWord::PAUSES;
        let barrier = GcBarrier::new();
        let stack_a = LoopPollHandle::new();
        let stack_b = LoopPollHandle::new();
        barrier.register_loop_poll_word(stack_a.word());
        barrier.register_loop_poll_word(stack_b.word());
        // Registering twice keeps one entry.
        barrier.register_loop_poll_word(stack_a.word());
        assert_eq!(barrier.loop_poll_words.lock().len(), 2);
        assert_eq!(stack_a.word().registered_with(), barrier.loop_poll_key());
        stack_a.word().note_code_moved();
        assert_eq!(
            stack_a.word().load(),
            LoopPollWord::MOVE_STEP,
            "a move, no pause"
        );

        assert!(barrier.request_stw(ThreadId(1), 1));
        assert_eq!(pauses(&stack_a), 1, "counted with the flag");
        assert_eq!(pauses(&stack_b), 1, "counted with the flag");
        // A re-registration during the pause does not count it again.
        barrier.register_loop_poll_word(stack_a.word());
        assert_eq!(pauses(&stack_a), 1);
        // A move during the pause does not disturb the count.
        stack_a.word().note_code_moved();
        assert_eq!(stack_a.word().load(), (2 * LoopPollWord::MOVE_STEP) + 1);
        // A word registered while the pause is up is counted once, now.
        let late = LoopPollHandle::new();
        barrier.register_loop_poll_word(late.word());
        assert_eq!(pauses(&late), 1, "registered mid-pause");

        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(
            stack_a.word().load(),
            2 * LoopPollWord::MOVE_STEP,
            "uncounted, moves kept"
        );
        assert_eq!(pauses(&stack_b), 0);
        assert_eq!(pauses(&late), 0);
        // A completion with no pause in progress uncounts nothing.
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(pauses(&stack_b), 0);

        // A dropped stack's word is forgotten at the next registration.
        drop(stack_b);
        barrier.register_loop_poll_word(stack_a.word());
        assert_eq!(
            barrier.loop_poll_words.lock().len(),
            2,
            "stack_a and late remain"
        );

        // The dispatch loop's entry point registers this OS thread's word, once.
        let this_thread = barrier.loop_poll_word_for_this_thread();
        assert_eq!(barrier.loop_poll_word_for_this_thread(), this_thread);
        assert_eq!(barrier.loop_poll_words.lock().len(), 3);
        assert!(barrier.request_stw(ThreadId(1), 1));
        // SAFETY: this test thread's thread-local word, alive for the whole test.
        assert_eq!(unsafe { &*this_thread }.load() & LoopPollWord::PAUSES, 1);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(unsafe { &*this_thread }.load() & LoopPollWord::PAUSES, 0);
    }

    /// gcj j1/cm: a barrier dropped mid-pause uncounts the pause from the
    /// loop words registered with it, so another VM's loops on the same OS
    /// thread do not see a pause that will never end.
    #[test]
    fn gcj1cm_a_barrier_dropped_mid_pause_uncounts_its_loop_words() {
        let word = LoopPollHandle::new();
        {
            let barrier = GcBarrier::new();
            barrier.register_loop_poll_word(word.word());
            assert!(barrier.request_stw(ThreadId(1), 1));
            assert_eq!(word.word().load() & LoopPollWord::PAUSES, 1);
        }
        assert_eq!(word.word().load() & LoopPollWord::PAUSES, 0);
    }

    /// gcj j2/cm2 (`CRATONVM_GC_HANDSHAKE_TARGET_KICK`): a thread publishes
    /// the poll word its loops run on, and a kick of that publication moves
    /// that word alone by one whole trigger step (no pause counted). Nothing
    /// published, or a word that is not the publishing thread's own, kicks
    /// nothing (the caller then kicks every loop).
    #[test]
    fn gcj2cm2_a_published_loop_word_is_kicked_alone() {
        use crate::threading::jvm_thread::GcBlockState;
        let barrier = GcBarrier::new();
        let own = barrier.loop_poll_word_for_this_thread();
        let state = GcBlockState::new();
        assert!(!state.kick_published_loop_poll_word(), "nothing published");
        state.publish_loop_poll_word(own);
        // SAFETY: this test thread's own word, alive for the test.
        let before = unsafe { &*own }.load();
        assert!(state.kick_published_loop_poll_word());
        let after = unsafe { &*own }.load();
        assert_eq!(after.wrapping_sub(before), LoopPollWord::MOVE_STEP);
        assert_eq!(after & LoopPollWord::PAUSES, 0, "no pause counted");
        // Another OS thread's word, published from here, is not this thread's:
        // published as none.
        let other = std::thread::spawn(this_threads_loop_poll_word)
            .join()
            .expect("the other thread")
            .expect("its word");
        let elsewhere = GcBlockState::new();
        elsewhere.publish_loop_poll_word(Arc::as_ptr(&other));
        assert!(!elsewhere.kick_published_loop_poll_word());
        // A word whose OS thread has ended is not kicked either.
        let ended = LoopPollHandle::new();
        let word = Arc::clone(ended.word());
        drop(ended);
        assert!(!word.kick());
    }

    /// t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`): the handshake arm lives in
    /// the byte compiled polls test, beside the pause count, and neither
    /// disturbs the other; the loop's gate (`& !PAUSES`) ignores it; the
    /// owner's clear answers whether it was up exactly once; an arm after the
    /// clear stays up; a published word is armed through its `GcBlockState`;
    /// an ended owner's word is not armed.
    #[test]
    fn t1e_the_handshake_arm_shares_the_polled_byte_with_the_pause_count() {
        use crate::threading::jvm_thread::GcBlockState;
        let word = LoopPollHandle::new();
        let w = word.word();
        let gate = w.load() & !LoopPollWord::PAUSES;
        assert!(w.arm_handshake());
        assert_eq!(w.load() & LoopPollWord::PAUSES, LoopPollWord::HANDSHAKE);
        assert_eq!(w.load() & !LoopPollWord::PAUSES, gate, "the gate is unchanged");
        // A pause counted while armed: both visible, neither lost.
        w.note_pause_begin();
        assert_eq!(w.load() & LoopPollWord::PAUSE_COUNT, 1);
        assert!(w.take_handshake_arm());
        assert!(!w.take_handshake_arm(), "cleared once");
        assert_eq!(w.load() & LoopPollWord::PAUSE_COUNT, 1, "the count survives the clear");
        w.note_pause_end();
        assert_eq!(w.load() & LoopPollWord::PAUSES, 0);
        // Re-armed after the clear: up for the next poll.
        assert!(w.arm_handshake());
        assert_eq!(w.load() & LoopPollWord::HANDSHAKE, LoopPollWord::HANDSHAKE);
        assert!(w.take_handshake_arm());
        // Through a publication of this thread's own word.
        let barrier = GcBarrier::new();
        let own = barrier.loop_poll_word_for_this_thread();
        let state = GcBlockState::new();
        let me = crate::threading::thread_registry::current_os_tid();
        assert!(!state.arm_published_loop_poll_word_handshake(Some(me)), "nothing published");
        state.publish_loop_poll_word(own);
        // SAFETY: this test thread's own word, alive for the test.
        let own_ref = unsafe { &*own };
        assert_eq!(own_ref.owner_os_tid(), me, "owned by the creating OS thread");
        // t1 lane E wave 3: not provably current -> not armed (the caller kicks).
        assert!(!state.arm_published_loop_poll_word_handshake(None), "unmounted / unknown");
        assert!(!state.arm_published_loop_poll_word_handshake(Some(0)), "no tid");
        assert!(
            !state.arm_published_loop_poll_word_handshake(Some(me.wrapping_add(1))),
            "remounted on another carrier"
        );
        assert_eq!(own_ref.load() & LoopPollWord::HANDSHAKE, 0, "nothing armed so far");
        if me != 0 {
            assert!(state.arm_published_loop_poll_word_handshake(Some(me)));
            assert!(take_this_threads_handshake_arm());
        }
        assert!(!take_this_threads_handshake_arm());
        assert_eq!(own_ref.load() & LoopPollWord::PAUSES, 0);
        // The barrier's always-raised fallback word has no owner tid.
        assert_eq!(barrier.always_polling.owner_os_tid(), 0);
        let ended = LoopPollHandle::new();
        let gone = Arc::clone(ended.word());
        drop(ended);
        assert!(!gone.arm_handshake());
        assert_eq!(gone.load() & LoopPollWord::HANDSHAKE, 0);
    }

    /// Interpreter round i6 wave 119, lane L2 (`CRATONVM_JIT_POLL_THREAD_WORD`;
    /// the multi-VM gate of stage 3 of `i24-L3`): the invariant a compiled
    /// poll reading the per-thread word rests on. A thread whose word is
    /// registered with VM A's barrier and then runs VM B's compiled code went
    /// through B's compiled-entry door (`helpers::PollWordMirrorScope::enter`),
    /// which asks B's barrier for the word, so B's pause is counted in it and
    /// B's pause cannot wait on a thread whose poll misses it. A's pauses stay
    /// counted too (a word stays listed with every barrier it joined), so A's
    /// code on the same thread is equally covered; the cross-counting costs
    /// only slow-path visits that find no pause of their own VM.
    #[test]
    fn a_word_registered_with_another_vm_counts_this_vms_pause_once_its_door_asked() {
        let vm_a = GcBarrier::new();
        let vm_b = GcBarrier::new();
        let pauses = |w: *const LoopPollWord| {
            // SAFETY: this test thread's thread-local word, alive for the whole test.
            unsafe { &*w }.load() & LoopPollWord::PAUSES
        };
        // The thread last ran VM A's dispatch loop.
        let word = vm_a.loop_poll_word_for_this_thread();
        assert_eq!(unsafe { &*word }.registered_with(), vm_a.loop_poll_key());
        // VM B's compiled-entry door asks B's barrier: the same per-OS-thread
        // word, now in B's list as well.
        let from_b = vm_b.loop_poll_word_for_this_thread();
        assert_eq!(from_b, word, "one word per OS thread");
        assert_eq!(unsafe { &*word }.registered_with(), vm_b.loop_poll_key());
        // B's pause is seen by a poll reading the word ...
        assert!(vm_b.request_stw(ThreadId(1), 1));
        assert_eq!(pauses(word), 1, "VM B's pause reaches the word");
        vm_b.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(pauses(word), 0);
        // ... and so is A's, though the word now names B as its last barrier.
        assert!(vm_a.request_stw(ThreadId(1), 1));
        assert_eq!(pauses(word), 1, "VM A still counts its pause in the word");
        vm_a.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(pauses(word), 0);
        // Both pauses at once: counted twice, uncounted one by one.
        assert!(vm_a.request_stw(ThreadId(1), 1));
        assert!(vm_b.request_stw(ThreadId(1), 1));
        assert_eq!(pauses(word), 2);
        vm_a.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(pauses(word), 1, "B's pause is still up");
        vm_b.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(pauses(word), 0);
    }

    /// The other half of the gate: a word that never joined VM B's list (the
    /// thread reached B's compiled code by a path that is not a door) does
    /// NOT see B's pause, which is why such a path must find the mirror null
    /// and fall back to B's baked flag (`PollWordMirrorScope` publishes only
    /// inside a door, after asking that door's barrier).
    #[test]
    fn a_word_never_registered_with_a_vm_misses_its_pause() {
        let vm_a = GcBarrier::new();
        let vm_b = GcBarrier::new();
        let word = LoopPollHandle::new();
        vm_a.register_loop_poll_word(word.word());
        assert!(vm_b.request_stw(ThreadId(1), 1));
        assert_eq!(
            word.word().load() & LoopPollWord::PAUSES,
            0,
            "B never counted a word it was never given"
        );
        assert!(vm_b.stw_requested.load(Ordering::Acquire), "B's own flag is up");
        vm_b.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// Cooperative JIT safepoint polling — the flag must sit within `disp32`
    /// reach of the JIT code cache, so `emit_safepoint_poll` can use its
    /// one-instruction `TEST BYTE [rip+disp32], 0xFF` form.
    ///
    /// The sibling of `platform::tests::a_cell_is_within_disp32_of_a_code_buffer`,
    /// which asserts the same thing about a bare cell. This one asserts it
    /// about the byte that is actually polled, reached the way the JIT reaches
    /// it (`stw_requested_flag_addr`), so it also fails if `CacheLineFlag` ever
    /// goes back to being an inline field of this `Arc`-allocated struct — the
    /// state that made every compiled poll in the process 15 bytes and a
    /// clobbered R11 instead of 7 bytes and none.
    ///
    /// x86-64 only; `disp32` reach is what makes the distance matter.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn stw_requested_flag_is_within_disp32_of_the_code_cache() {
        use cratonvm_jit::platform::{alloc_executable, free_executable};

        if cratonvm_jit::platform::alloc_code_adjacent_cell().is_none() {
            // Either the OS refused the mapping or `CRATONVM_JIT_CODE_NEAR_GLOBALS`
            // owns placement and the cell allocator declines by design — in
            // which case the flag is SUPPOSED to stay on the VM heap, beside
            // the anchor that strategy pulls the code towards, and its distance
            // from an unhinted buffer means nothing.
            return;
        }
        let barrier = GcBarrier::new();
        let flag = barrier.stw_requested_flag_addr() as usize;
        let code = alloc_executable(4096).expect("alloc_executable failed");
        // Widening: usize -> i128, so the subtraction cannot wrap.
        let delta = (flag as i128) - (code as usize as i128);
        let in_reach = delta >= i32::MIN as i128 && delta <= i32::MAX as i128;
        free_executable(code, 4096);
        assert!(
            in_reach,
            "stw_requested flag {flag:#x} is {:.1} GB from a code buffer at \
             {:#x}; every compiled safepoint poll falls back to \
             MOV R11, imm64 ; TEST BYTE [R11], 0xFF",
            (delta.unsigned_abs() as f64) / (1024.0 * 1024.0 * 1024.0),
            code as usize,
        );
    }

    /// Cooperative JIT safepoint polling — the raw byte address must alias
    /// `stw_requested` exactly: a plain byte read through the pointer must
    /// track every transition the atomic makes.
    #[test]
    fn stw_requested_flag_addr_aliases_the_atomic_byte() {
        let barrier = GcBarrier::new();
        let addr = barrier.stw_requested_flag_addr();
        assert!(!addr.is_null());
        // SAFETY: `addr` points at `barrier.stw_requested`, which is alive
        // for the whole scope of this test.
        assert_eq!(unsafe { *addr }, 0, "expected false (0) initially");

        assert!(barrier.request_stw(ThreadId(0), 1));
        // SAFETY: same as above.
        assert_ne!(unsafe { *addr }, 0, "expected nonzero after request_stw");

        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        // SAFETY: same as above.
        assert_eq!(unsafe { *addr }, 0, "expected false (0) after complete_gc");
    }

    #[test]
    fn barrier_request_and_complete_single_thread() {
        let barrier = GcBarrier::new();
        // Single alive thread (the initiator) → expected = 0
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert!(barrier.stw_requested.load(Ordering::Relaxed));

        // No other threads to wait for
        barrier.wait_for_all();

        // Complete with empty pointer map
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(!barrier.stw_requested.load(Ordering::Relaxed));
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn barrier_two_threads() {
        let barrier = Arc::new(GcBarrier::new());

        // Request STW with 2 alive threads
        assert!(barrier.request_stw(ThreadId(0), 2));

        let barrier2 = barrier.clone();
        let handle = std::thread::spawn(move || {
            // Non-initiator thread arrives at safepoint
            barrier2.arrive_and_wait(ThreadId(1))
        });

        // Initiator waits for the other thread
        barrier.wait_for_all();

        // Complete GC with a pointer remap
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x1000, 0x2000);
        barrier.complete_gc(pm);

        // Non-initiator thread should receive the pointer map
        let result_map = handle.join().unwrap();
        assert_eq!(result_map.get(&0x1000), Some(&0x2000));
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    /// Interpreter round i2 wave 57, lane L6: `moving_generation` counts only
    /// the pauses that published a non-empty pointer map; `gc_generation`
    /// counts every pause.
    #[test]
    fn i57_l6_moving_generation_counts_only_pauses_that_moved() {
        let barrier = GcBarrier::new();
        assert_eq!(barrier.moving_generation(), 0);
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
        assert_eq!(barrier.moving_generation(), 0, "nothing moved");
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.wait_for_all();
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x1000, 0x2000);
        barrier.complete_gc(pm);
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 2);
        assert_eq!(barrier.moving_generation(), 1);
    }

    #[test]
    fn barrier_duplicate_request_rejected() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 1));
        // Second request while first is active should fail
        assert!(!barrier.request_stw(ThreadId(1), 2));
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    #[test]
    fn barrier_initiator_not_blocked() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 1));
        // Initiator calling arrive_and_wait should return immediately
        let map = barrier.arrive_and_wait(ThreadId(0));
        assert!(map.is_empty());
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// B2 — an EXCLUDED (blocked) thread that wakes mid-STW and calls
    /// `arrive_and_wait_excluded` must NOT count toward `arrived`, so it
    /// cannot satisfy the quota and release the initiator's `wait_for_all`
    /// while the genuine counted mutator has not yet arrived.
    ///
    /// Scenario: 3 alive threads — initiator I, counted mutator M, blocked
    /// (excluded) thread B. `request_stw` is told 1 thread is blocked, so
    /// `expected = 3 - 1 (initiator) - 1 (blocked) = 1` (just M).
    #[test]
    fn barrier_excluded_thread_does_not_release_early() {
        let barrier = Arc::new(GcBarrier::new());
        // Pretend B is already parked in a blocking native.
        barrier.threads_blocked.store(1, Ordering::Release);
        assert!(barrier.request_stw(ThreadId(0), 3));
        // expected counts only the running mutator M.
        {
            let inner = barrier.inner.lock();
            assert_eq!(inner.expected, 1);
        }

        // B (excluded) wakes mid-STW and drains via the excluded entry.
        let b = barrier.clone();
        let hb = std::thread::spawn(move || b.arrive_and_wait_excluded(ThreadId(2)));

        // Give B time to run its (non-counting) arrival.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // The excluded arrival must NOT have satisfied the quota: M (the
        // only counted thread) has not arrived, so `arrived` stays 0 and
        // `wait_for_all` would still block. `pending_count` proves it.
        assert_eq!(
            barrier.pending_count(),
            1,
            "excluded thread must not be counted toward arrived",
        );

        // Now the counted mutator M arrives — quota satisfied.
        let m = barrier.clone();
        let hm = std::thread::spawn(move || m.arrive_and_wait(ThreadId(1)));

        // The initiator can now proceed.
        barrier.wait_for_all();
        assert_eq!(barrier.pending_count(), 0);

        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = hm.join();
        let _ = hb.join();
    }

    /// B2 — sanity: with no excluded threads, the participating entry still
    /// counts and releases exactly as before.
    #[test]
    fn barrier_participating_entry_counts() {
        let barrier = Arc::new(GcBarrier::new());
        assert!(barrier.request_stw(ThreadId(0), 2));
        let b2 = barrier.clone();
        let h = std::thread::spawn(move || b2.arrive_and_wait(ThreadId(1)));
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = h.join();
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    /// BUG-03 — cross-thread JIT takeover may discover an in-JIT mutator only
    /// after a bounded wait has already timed out. Reducing `expected` after
    /// such a timeout must release the initiator once all non-cooperative peers
    /// have been removed from the quota.
    #[test]
    fn barrier_late_reduce_expected_can_satisfy_bounded_wait() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 3));

        assert!(
            !barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)),
            "two non-initiator mutators are still pending"
        );

        barrier.reduce_expected(1);
        assert!(
            !barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)),
            "one mutator is still pending after the first takeover"
        );

        barrier.reduce_expected(1);
        assert!(
            barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)),
            "late takeover should satisfy the reduced barrier quota"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    #[test]
    fn blocked_dead_transition_is_observed_atomically() {
        let barrier = GcBarrier::new();
        let guard = barrier.enter_blocked();
        let mut alive = 3u32; // initiator + live mutator + terminating blocked thread

        guard.finish_after(|| {
            alive = 2; // the terminating thread is now dead
        });

        assert!(barrier.request_stw_counted(ThreadId(0), || alive));
        {
            let inner = barrier.inner.lock();
            assert_eq!(
                inner.expected, 1,
                "dead blocked thread must not be subtracted from the live mutator quota",
            );
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    #[test]
    fn manual_blocked_dead_transition_is_observed_atomically() {
        let barrier = GcBarrier::new();
        let _pre_stw = barrier.mark_blocked_region_enter();
        let mut alive = 3u32; // initiator + live mutator + terminating blocked thread

        barrier.mark_blocked_region_leave_after(|| {
            alive = 2; // the terminating blocked thread is now dead
        });

        assert!(barrier.request_stw_counted(ThreadId(0), || alive));
        {
            let inner = barrier.inner.lock();
            assert_eq!(
                inner.expected, 1,
                "manual dead blocked thread must not be subtracted from the live mutator quota",
            );
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }
    #[test]
    fn counted_request_ignores_stale_global_blocked_slots() {
        let barrier = GcBarrier::new();
        // Simulate a leaked/stale blocked slot not represented by any live
        // blocked registry entry. A plain global subtraction would set
        // expected=0 for two alive threads and let GC proceed without waiting
        // for the one live peer.
        barrier.threads_blocked.store(1, Ordering::Release);

        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
        {
            let inner = barrier.inner.lock();
            assert_eq!(inner.expected, 1);
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    #[test]
    fn counted_request_uses_live_blocked_before_global_counter_increment() {
        let barrier = GcBarrier::new();
        // A blocking thread deposits roots and sets its registry blocked flag
        // before `enter_blocked` increments the anonymous global count. Once the
        // root snapshot is published, STW must exclude it even if the global
        // counter still reads zero.
        barrier.threads_blocked.store(0, Ordering::Release);

        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (4, 2, vec![2, 3])));
        {
            let inner = barrier.inner.lock();
            assert_eq!(inner.expected, 1);
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// GCAUDIT-0711-FIX (finding 1a) — `arrive_and_wait_auto` must resolve
    /// EXACTLY like `arrive_and_wait_excluded` for a thread the pause's
    /// census actually excluded, even though the caller itself cannot prove
    /// that locally (see `GcBarrierInner::excluded_blocked`'s doc for why
    /// `pre_stw`/`in_blocked_region` alone are ambiguous at the call site).
    #[test]
    fn auto_arrival_excluded_thread_does_not_release_early() {
        let barrier = Arc::new(GcBarrier::new());
        // 3 alive: initiator I(0), counted mutator M(1), blocked B(2).
        // The census reports B's identity as excluded.
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (3, 1, vec![2])));
        {
            let inner = barrier.inner.lock();
            assert_eq!(inner.expected, 1, "only M should be counted");
        }

        let b = barrier.clone();
        let hb = std::thread::spawn(move || b.arrive_and_wait_auto(ThreadId(2)));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            barrier.pending_count(),
            1,
            "auto arrival for an excluded tid must not satisfy the quota",
        );

        let m = barrier.clone();
        let hm = std::thread::spawn(move || m.arrive_and_wait_auto(ThreadId(1)));
        barrier.wait_for_all();
        assert_eq!(barrier.pending_count(), 0);

        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = hm.join();
        let _ = hb.join();
    }

    /// GCAUDIT-0711-FIX (finding 1a) — sanity: with no excluded threads,
    /// `arrive_and_wait_auto` counts exactly like `arrive_and_wait`.
    #[test]
    fn auto_arrival_participating_by_default() {
        let barrier = Arc::new(GcBarrier::new());
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
        let b2 = barrier.clone();
        let h = std::thread::spawn(move || b2.arrive_and_wait_auto(ThreadId(1)));
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = h.join();
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    /// GCAUDIT-0711-FIX (finding 1a) — `excluded_blocked` must not leak
    /// across generations: a tid excluded by pause G must be treated as
    /// participating by default in pause G+1 unless that pause's own
    /// census excludes it again.
    #[test]
    fn excluded_blocked_does_not_leak_across_generations() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![1])));
        barrier.complete_gc(cratonvm_types::PointerMap::default());

        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
        {
            let inner = barrier.inner.lock();
            assert!(
                !inner.excluded_blocked.contains(&1),
                "stale exclusion from a completed pause must not survive complete_gc",
            );
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// Finding 1(c) — `leave_blocked_region_flagged` must not clear the
    /// caller's `in_blocked_region` flag while a pause that excluded the
    /// caller is still active; the clear lands only once no pause is active,
    /// with no start-and-complete window in between.
    #[test]
    fn leave_blocked_region_flagged_waits_out_active_pause() {
        use std::sync::atomic::AtomicBool;

        let barrier = Arc::new(GcBarrier::new());
        let flag = Arc::new(AtomicBool::new(true));

        // Pause active; thread 2 is excluded by the identity census.
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![2])));

        let b = barrier.clone();
        let f = flag.clone();
        let h = std::thread::spawn(move || b.leave_blocked_region_flagged(ThreadId(2), &f));

        // While the pause is active the flag must stay up (the thread is
        // waiting the pause out, not resuming).
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            flag.load(Ordering::Acquire),
            "flag must not clear while an excluding pause is active",
        );
        // The excluded leave must not have satisfied any quota either.
        assert_eq!(
            barrier.pending_count(),
            0,
            "expected was 0 (only initiator + excluded)"
        );

        barrier.complete_gc(cratonvm_types::PointerMap::default());
        h.join().unwrap();
        assert!(
            !flag.load(Ordering::Acquire),
            "flag clears once no pause is active",
        );
    }

    /// `leave_blocked_region_flagged` with no active pause clears the flag
    /// immediately and never blocks.
    #[test]
    fn leave_blocked_region_flagged_immediate_when_idle() {
        use std::sync::atomic::AtomicBool;
        let barrier = GcBarrier::new();
        let flag = AtomicBool::new(true);
        barrier.leave_blocked_region_flagged(ThreadId(7), &flag);
        assert!(!flag.load(Ordering::Acquire));
    }

    /// gcd d5/f: `leave_blocked_region_flagged_if` clears the flag only when
    /// its question answers yes, and otherwise leaves it up for the caller's
    /// full wake.
    #[test]
    fn leave_blocked_region_flagged_if_keeps_the_flag_up_when_told_to() {
        use std::sync::atomic::AtomicBool;
        let barrier = GcBarrier::new();
        let flag = AtomicBool::new(true);
        assert!(!barrier.leave_blocked_region_flagged_if(ThreadId(7), &flag, || false));
        assert!(flag.load(Ordering::Acquire), "left up for the full wake");
        assert!(barrier.leave_blocked_region_flagged_if(ThreadId(7), &flag, || true));
        assert!(!flag.load(Ordering::Acquire));
    }

    /// gcd d10/j: the in-native pair counts the thread blocked and releases
    /// it exactly once whichever way the leave goes, clears the flag only
    /// when told to, and with a pause in progress waits it out before it
    /// leaves (the two-step path).
    #[test]
    fn the_in_native_pair_counts_once_and_waits_out_a_pause_on_the_way_out() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;
        let barrier = Arc::new(GcBarrier::new());
        let flag = Arc::new(AtomicBool::new(true));

        assert!(!barrier.mark_in_native_enter(), "no pause in progress");
        assert_eq!(barrier.blocked_count(), 1);
        assert!(!barrier.leave_in_native_if(ThreadId(7), &flag, || false));
        assert_eq!(
            barrier.blocked_count(),
            0,
            "released even when the flag stays up"
        );
        assert!(flag.load(Ordering::Acquire), "left up for the full wake");

        assert!(!barrier.mark_in_native_enter());
        assert!(barrier.leave_in_native_if(ThreadId(7), &flag, || true));
        assert_eq!(barrier.blocked_count(), 0);
        assert!(!flag.load(Ordering::Acquire), "cleared when told to");

        // A pause that excluded the thread (identity census) holds its leave.
        flag.store(true, Ordering::Release);
        assert!(!barrier.mark_in_native_enter());
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![7])));
        let left = Arc::new(AtomicBool::new(false));
        let leaver = {
            let barrier = Arc::clone(&barrier);
            let flag = Arc::clone(&flag);
            let left = Arc::clone(&left);
            std::thread::spawn(move || {
                let quiet = barrier.leave_in_native_if(ThreadId(7), &flag, || true);
                left.store(true, Ordering::SeqCst);
                quiet
            })
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !left.load(Ordering::SeqCst),
            "an excluded thread does not leave native while the pause holds"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(leaver.join().expect("the leaver does not panic"));
        assert!(left.load(Ordering::SeqCst));
        assert_eq!(barrier.blocked_count(), 0, "released exactly once");
        assert!(!flag.load(Ordering::Acquire));
    }

    /// Poll `cond` for up to `limit`.
    fn gch_jr_within(limit: std::time::Duration, cond: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        cond()
    }

    /// gch h2/jr (`CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT`): a counted thread
    /// going into native during a pause arrives ONCE and runs on (it reaches
    /// "in C" while the pause is still waiting for another mutator); a second
    /// arrival of the same thread in that pause -- the no-wait entry again, or
    /// a parked `auto` arrival (a JNIEnv function's escalation) -- counts
    /// nothing; the quota is met only when the other counted mutator arrives;
    /// and the thread's leave still waits for `complete_gc`.
    #[test]
    fn gch_jr_in_native_arrival_meets_the_quota_without_parking() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;
        let barrier = Arc::new(GcBarrier::new());
        let flag = Arc::new(AtomicBool::new(true));
        // No pause: nothing to arrive for.
        assert!(!barrier.arrive_in_native(ThreadId(7)));

        // 3 alive: the initiator 0, the in-native thread 7, a mutator 1; the
        // census counted both (the flag of 7 went up after it).
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (3, 0, vec![])));
        assert_eq!(barrier.pending_count(), 2);

        let in_c = Arc::new(AtomicBool::new(false));
        let left = Arc::new(AtomicBool::new(false));
        let native = {
            let barrier = Arc::clone(&barrier);
            let flag = Arc::clone(&flag);
            let in_c = Arc::clone(&in_c);
            let left = Arc::clone(&left);
            std::thread::spawn(move || {
                assert!(barrier.mark_in_native_enter(), "the pause raced the entry");
                assert!(
                    barrier.arrive_in_native(ThreadId(7)),
                    "arrived, did not park"
                );
                assert!(
                    barrier.arrive_in_native(ThreadId(7)),
                    "again: still no park"
                );
                in_c.store(true, Ordering::SeqCst);
                let quiet = barrier.leave_in_native_if(ThreadId(7), &flag, || true);
                left.store(true, Ordering::SeqCst);
                quiet
            })
        };
        assert!(
            gch_jr_within(Duration::from_secs(5), || in_c.load(Ordering::SeqCst)),
            "the in-native arrival must return while the pause is still in progress"
        );
        assert_eq!(barrier.pending_count(), 1, "counted once, not twice");
        // A parked arrival of the same thread in the same pause counts nothing.
        let escalated = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let _ = barrier.arrive_and_wait_auto(ThreadId(7));
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(barrier.pending_count(), 1, "the escalation is excluded");
        assert!(
            !quota_met_within(&barrier, Duration::from_millis(20)),
            "only the other mutator meets the quota"
        );

        let mutator = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let _ = barrier.arrive_and_wait_auto(ThreadId(1));
            })
        };
        barrier.wait_for_all();
        assert_eq!(barrier.pending_count(), 0);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !left.load(Ordering::SeqCst),
            "the thread does not leave native while the pause holds"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(native.join().expect("the native thread does not panic"));
        escalated.join().expect("the escalation does not panic");
        mutator.join().expect("the mutator does not panic");
        assert!(left.load(Ordering::SeqCst));
        assert_eq!(
            barrier.blocked_count(),
            0,
            "the in-native count released once"
        );
        assert!(!flag.load(Ordering::Acquire));
    }

    /// gch h2/jr: an in-native arrival of a thread the census EXCLUDED (its
    /// flag was up first) counts nothing, and the exclusion it records lasts
    /// only for its pause: the next pause's census counts it again and its
    /// arrival there counts once.
    #[test]
    fn gch_jr_in_native_arrival_is_excluded_for_the_rest_of_its_pause_only() {
        let barrier = Arc::new(GcBarrier::new());
        // 3 alive: initiator 0, mutator 1, thread 7 excluded by the census.
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (3, 1, vec![7])));
        assert_eq!(barrier.pending_count(), 1);
        let b = Arc::clone(&barrier);
        assert!(std::thread::spawn(move || b.arrive_in_native(ThreadId(7)))
            .join()
            .expect("no panic"));
        assert_eq!(
            barrier.pending_count(),
            1,
            "an excluded arrival counts nothing"
        );
        let b = Arc::clone(&barrier);
        let mutator = std::thread::spawn(move || {
            let _ = b.arrive_and_wait_auto(ThreadId(1));
        });
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        mutator.join().expect("no panic");

        // Pause 2 counts 7 (alive 2: initiator and 7).
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
        assert_eq!(barrier.pending_count(), 1);
        let b = Arc::clone(&barrier);
        assert!(std::thread::spawn(move || b.arrive_in_native(ThreadId(7)))
            .join()
            .expect("no panic"));
        assert_eq!(
            barrier.pending_count(),
            0,
            "counted once in the pause that counted it"
        );
        {
            let inner = barrier.inner.lock();
            assert!(
                inner.excluded_blocked.contains(&7),
                "and excluded for the rest of it"
            );
        }
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(
            !barrier.inner.lock().excluded_blocked.contains(&7),
            "cleared with its pause"
        );
    }

    /// gcd d5/f: a JNI leaf window opens only while no pause is up, and a
    /// winning pause request does not return while one is open (it drains
    /// them after raising `stw_requested`).
    #[test]
    fn a_leaf_window_opens_outside_pauses_and_a_request_waits_for_it() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;
        let barrier = Arc::new(GcBarrier::new());
        let word = Arc::new(LeafWindowWord::new());
        assert!(barrier.try_open_leaf_window(&word));
        assert!(word.is_open());

        // A request from another thread waits for the open window.
        let requested = Arc::new(AtomicBool::new(false));
        let requester = {
            let barrier = Arc::clone(&barrier);
            let requested = Arc::clone(&requested);
            std::thread::spawn(move || {
                let won = barrier.request_stw(ThreadId(1), 1);
                requested.store(true, Ordering::SeqCst);
                won
            })
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !requested.load(Ordering::SeqCst),
            "the request must not return while a leaf window is open"
        );
        word.close();
        assert!(requester.join().expect("the requester does not panic"));
        assert!(requested.load(Ordering::SeqCst));

        // With the pause up, no window opens.
        assert!(!barrier.try_open_leaf_window(&word));
        assert!(!word.is_open(), "a refused window is left closed");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(
            barrier.try_open_leaf_window(&word),
            "open again after the pause"
        );
        word.close();

        // A dead owner's word is forgotten, and never waited for.
        word.mark_owner_dropped();
        let other = Arc::new(LeafWindowWord::new());
        assert!(barrier.try_open_leaf_window(&other));
        other.close();
        assert_eq!(
            barrier.leaf_window_words.lock().len(),
            1,
            "the dead word was pruned"
        );
        assert!(barrier.request_stw(ThreadId(1), 1));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// gcj j3/rt3 residual 3, fail-first: a window that stays open past the
    /// locked drain's budget is waited for with `inner` RELEASED -- another
    /// thread takes `inner` (an arrival's, a leave's, a losing request's
    /// lock) while the request still has not returned, and sees the pause
    /// fully published. Before, the lock was held for the whole wait.
    #[test]
    fn gcj3rt3_a_slow_leaf_window_is_drained_with_inner_released() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;
        let barrier = Arc::new(GcBarrier::new());
        let word = Arc::new(LeafWindowWord::new());
        assert!(barrier.try_open_leaf_window(&word));
        let returned = Arc::new(AtomicBool::new(false));
        let requester = {
            let barrier = Arc::clone(&barrier);
            let returned = Arc::clone(&returned);
            std::thread::spawn(move || {
                let won =
                    barrier.request_stw_counted_with_live_blocked(ThreadId(1), || (2, 1, vec![5]));
                returned.store(true, Ordering::SeqCst);
                won
            })
        };
        // Until the flag is up, the request has not reached its drain.
        while !barrier.stw_requested.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(!returned.load(Ordering::SeqCst), "still draining");
        {
            let inner = barrier
                .inner
                .try_lock_for(Duration::from_secs(5))
                .expect("inner is not held across the slow drain");
            assert!(inner.pause_in_flight, "the pause is fully published");
            assert!(inner.excluded_blocked.contains(&5));
            assert_eq!(inner.expected, 0);
        }
        assert!(
            !returned.load(Ordering::SeqCst),
            "the request waits for the window"
        );
        word.close();
        assert!(requester.join().expect("no panic"));
        assert!(!barrier.leaf_drain_owed.load(Ordering::Relaxed), "consumed");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// `with_no_pause_in_progress` (round 11 wave 8, JNI monitor ops from an
    /// excluded thread): `f` does not run while a pause is active, and runs as
    /// soon as that pause completes.
    #[test]
    fn with_no_pause_in_progress_waits_out_an_active_pause() {
        use std::sync::atomic::AtomicBool;

        let barrier = Arc::new(GcBarrier::new());
        let ran = Arc::new(AtomicBool::new(false));
        // Pause active; thread 2 is excluded by the identity census.
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![2])));

        let b = barrier.clone();
        let r = ran.clone();
        let h = std::thread::spawn(move || {
            b.with_no_pause_in_progress(|| {
                // No pause may be visible to `f`.
                let stw = b.stw_requested.load(Ordering::Acquire);
                r.store(true, Ordering::Release);
                stw
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !ran.load(Ordering::Acquire),
            "f must not run while a pause is active"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let saw_stw = h.join().unwrap();
        assert!(ran.load(Ordering::Acquire));
        assert!(!saw_stw, "f ran with the pause still requested");
    }

    /// The other half: while `f` runs no pause can BEGIN — a request made from
    /// inside the scope waits until `f` has returned.
    #[test]
    fn with_no_pause_in_progress_holds_off_a_new_request() {
        use std::sync::atomic::AtomicBool;

        let barrier = Arc::new(GcBarrier::new());
        let requested = Arc::new(AtomicBool::new(false));
        let requester = barrier.with_no_pause_in_progress(|| {
            let b = barrier.clone();
            let q = requested.clone();
            let h = std::thread::spawn(move || {
                let won = b.request_stw(ThreadId(9), 1);
                q.store(true, Ordering::Release);
                won
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(
                !requested.load(Ordering::Acquire),
                "a pause request must not complete inside the scope"
            );
            assert!(!barrier.stw_requested.load(Ordering::Acquire));
            h
        });
        assert!(requester.join().unwrap(), "the request wins once f returns");
        assert!(barrier.stw_requested.load(Ordering::Acquire));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// Poll `wait_for_all_timeout` until it reports the quota met or `limit`
    /// passes. One call is a single bounded wait that may wake spuriously.
    fn quota_met_within(barrier: &GcBarrier, limit: std::time::Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if barrier.wait_for_all_timeout(std::time::Duration::from_millis(20)) {
                return true;
            }
        }
        false
    }

    /// gc-common 2026-09-23 (lane A) — a non-initiator OS thread arriving
    /// under its own id is counted and receives the pause's map (the plain
    /// contract, restated here beside the initiator tests below so a change
    /// to `is_initiator_locked` has to keep it).
    #[test]
    fn a_distinct_os_thread_with_its_own_id_is_counted() {
        let barrier = Arc::new(GcBarrier::new());
        assert!(barrier.request_stw(ThreadId(0), 2));
        let b = barrier.clone();
        let h = std::thread::spawn(move || b.arrive_and_wait_auto(ThreadId(8)));
        assert!(
            quota_met_within(&barrier, std::time::Duration::from_secs(10)),
            "a counted non-initiator must satisfy its quota slot"
        );
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x1000, 0x2000);
        barrier.complete_gc(pm);
        let got = h.join().unwrap();
        assert_eq!(
            got.get(&0x1000),
            Some(&0x2000),
            "it must have waited the pause out and received that pause's map"
        );
    }

    /// The initiator's OS thread short-circuits whatever id it passes.
    #[test]
    fn the_initiator_short_circuits_whatever_id_it_passes() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(3), 1));
        let map = barrier.arrive_and_wait_auto(ThreadId(99));
        assert!(map.is_empty());
        assert_eq!(barrier.pending_count(), 0);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// gc-common w3-a — "initiator" is the requesting OS THREAD, not an id:
    /// another OS thread arriving under the initiator's `ThreadId` (the old JNI
    /// placeholder shape) is an ordinary counted arrival and waits the pause
    /// out. Before w3-a it was short-circuited as the initiator, took no slot
    /// and returned at once.
    #[test]
    fn another_os_thread_passing_the_initiators_id_is_not_the_initiator() {
        let barrier = Arc::new(GcBarrier::new());
        assert!(barrier.request_stw(ThreadId(0), 2));
        let b = barrier.clone();
        let h = std::thread::spawn(move || b.arrive_and_wait_auto(ThreadId(0)));
        assert!(
            quota_met_within(&barrier, std::time::Duration::from_secs(10)),
            "the arrival must fill the one counted slot"
        );
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x40, 0x80);
        barrier.complete_gc(pm);
        let got = h.join().unwrap();
        assert_eq!(
            got.get(&0x40),
            Some(&0x80),
            "it must have parked for the pause and received its map"
        );
    }

    /// gc-common w3-a — the slowest pause names the thread it waited for last,
    /// and a quota completed by a take-over excusal names nobody.
    #[test]
    fn ttsp_names_the_straggler_of_the_slowest_pause() {
        let barrier = Arc::new(GcBarrier::new());
        // Pause 1: one counted peer, tid 7, arrives late.
        assert!(barrier.request_stw(ThreadId(0), 2));
        let b = barrier.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            b.arrive_and_wait(ThreadId(7))
        });
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = h.join();
        let s = barrier.ttsp_stats();
        assert_eq!(s.last_straggler, Some(7));
        assert_eq!(s.max_straggler, Some(7));

        // Pause 2: the only counted peer is excused (frozen in compiled code).
        assert!(barrier.request_stw(ThreadId(0), 2));
        barrier.reduce_expected(1);
        assert!(barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let s = barrier.ttsp_stats();
        assert_eq!(s.last_straggler, None, "an excused peer has no arrival");
        assert_eq!(
            s.max_straggler,
            Some(7),
            "the slowest pause (30 ms) is still pause 1"
        );
    }

    /// gci i20/st: every PARTICIPATING arrival of a pause is booked in
    /// `arrived_ids` (the stall line's arrived set), an excluded one never,
    /// and the next request starts an empty set.
    #[test]
    fn gci_i20st_arrived_ids_name_every_counted_arrival_and_reset_per_pause() {
        let barrier = Arc::new(GcBarrier::new());
        // alive 3 = the initiator + two counted peers.
        assert!(barrier.request_stw(ThreadId(0), 3));
        let bx = barrier.clone();
        let excluded = std::thread::spawn(move || bx.arrive_and_wait_excluded(ThreadId(5)));
        let b1 = barrier.clone();
        let h1 = std::thread::spawn(move || b1.arrive_and_wait(ThreadId(9)));
        let b2 = barrier.clone();
        let h2 = std::thread::spawn(move || b2.arrive_and_wait(ThreadId(7)));
        assert!(
            quota_met_within(&barrier, std::time::Duration::from_secs(10)),
            "the two counted peers fill the quota"
        );
        assert_eq!(
            barrier.arrived_ids_snapshot(),
            vec![7, 9],
            "the arrived set names both counted peers, sorted, and never the excluded one"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = h1.join();
        let _ = h2.join();
        let _ = excluded.join();
        assert!(barrier.request_stw(ThreadId(0), 2));
        assert!(
            barrier.arrived_ids_snapshot().is_empty(),
            "a new request starts with nobody arrived"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// gci i20/st: the stall line's arrived-set text is sorted and bounded.
    #[test]
    fn gci_i20st_stall_arrived_ids_text_sorts_and_bounds() {
        assert_eq!(stall_arrived_ids_text(&[]), "[]");
        assert_eq!(stall_arrived_ids_text(&[9, 3, 7]), "[3, 7, 9]");
        let many: Vec<u64> = (0..(STALL_ARRIVED_IDS_SHOWN as u64 + 3)).rev().collect();
        let text = stall_arrived_ids_text(&many);
        assert!(text.starts_with("[0, 1, 2, "), "{text}");
        assert!(text.ends_with(", +3]"), "{text}");
    }

    /// gc-common w3-a — the finalizer hint is raised by a queueing collection
    /// and lowered exactly once by a drainer.
    #[test]
    fn the_finalizer_hint_is_taken_once() {
        let barrier = GcBarrier::new();
        assert!(!barrier.ref_work_queued());
        assert!(!barrier.take_ref_work_queued());
        barrier.note_ref_work_queued();
        assert!(barrier.ref_work_queued());
        assert!(barrier.take_ref_work_queued());
        assert!(!barrier.ref_work_queued());
        assert!(!barrier.take_ref_work_queued());
    }

    /// gc-common w5-a — the hint remembers the generation it was raised at, so
    /// a hint nobody took across whole pauses reads stale; a re-raise of a
    /// raised hint does not refresh it, and a take resets it.
    #[test]
    fn a_hint_nobody_takes_goes_stale_after_whole_pauses() {
        let barrier = GcBarrier::new();
        assert!(!barrier.ref_work_stale(0), "a lowered hint is never stale");
        barrier.note_ref_work_queued(); // raised at generation 0
        assert!(!barrier.ref_work_stale(1));
        barrier.gc_generation.fetch_add(1, Ordering::Release);
        barrier.note_ref_work_queued(); // already raised: keeps generation 0
        assert!(barrier.ref_work_stale(1));
        assert!(!barrier.ref_work_stale(2));
        barrier.gc_generation.fetch_add(1, Ordering::Release);
        assert!(barrier.ref_work_stale(2));
        assert!(barrier.take_ref_work_queued());
        assert!(!barrier.ref_work_stale(0), "a taken hint is not stale");
        barrier.note_ref_work_queued(); // raised afresh at generation 2
        assert!(!barrier.ref_work_stale(1));
    }

    /// gc-common w5-a (`handoff-w4b-gc-generation-on-its-own-cache-line`):
    /// the generation counter the JIT memo caches probe on every lookup shares
    /// no cache line with the fields every blocking transition writes.
    #[test]
    fn the_gc_generation_has_a_cache_line_of_its_own() {
        let barrier = GcBarrier::new();
        let line = |p: usize| p / 64;
        let gen = &*barrier.gc_generation as *const AtomicU64 as usize;
        let blocked = &barrier.threads_blocked as *const AtomicU64 as usize;
        let inner = &barrier.inner as *const Mutex<GcBarrierInner> as usize;
        assert_eq!(gen % 64, 0, "PaddedU64 starts a line");
        assert_ne!(line(gen), line(blocked));
        assert_ne!(line(gen), line(inner));
        assert!(std::mem::size_of::<PaddedU64>() >= 64);
        // Deref keeps the atomic API.
        barrier.gc_generation.fetch_add(2, Ordering::Release);
        assert_eq!(barrier.gc_generation.load(Ordering::Acquire), 2);
    }

    /// gc-common w3-a — a barrier dropped mid-pause releases its in-flight
    /// count, and one that completed releases it exactly once.
    #[test]
    fn a_barrier_releases_its_in_flight_pause_exactly_once() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert!(barrier.inner.lock().pause_in_flight);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(!barrier.inner.lock().pause_in_flight);
        // A second `complete_gc` without a request releases nothing.
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(!barrier.inner.lock().pause_in_flight);
        let dropped_mid_pause = GcBarrier::new();
        assert!(dropped_mid_pause.request_stw(ThreadId(0), 1));
        let ledger = Arc::clone(dropped_mid_pause.pause_ledger());
        assert!(cratonvm_gc::gc_quiescence::pause_ledger_bound(&ledger));
        drop(dropped_mid_pause);
        // gc-common w21-e: the requester is no longer bound to the dropped
        // VM's ledger, so its coverage writes reach the orphan rows again.
        assert!(!cratonvm_gc::gc_quiescence::pause_ledger_bound(&ledger));
    }

    /// gc-common w3-a — on Windows a 1 ms bounded wait with no arrival must
    /// take about 1 ms, not the 15.625 ms clock tick. Best of five, so a
    /// descheduled attempt on a loaded host is not read as the tick.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_short_bounded_wait_is_not_tick_quantized_on_windows() {
        if !crate::threading::jvm_thread::win_park::enabled() {
            return;
        }
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 2));
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let t0 = Instant::now();
            assert!(!barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
            best = best.min(t0.elapsed());
        }
        barrier.reduce_expected(1);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(
            best < std::time::Duration::from_millis(8),
            "best 1 ms wait took {best:?}: the condvar's tick-quantized path"
        );
    }

    /// gc-common 2026-09-23 (lane A) — the initiator closing a blocked region
    /// inside its own pause must not wait for a generation only it can
    /// advance. Both leave paths (`BlockedGuard::drop`,
    /// `mark_blocked_region_leave`) used to self-deadlock here.
    #[test]
    fn the_initiator_leaving_a_blocked_region_in_its_own_pause_does_not_hang() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let barrier = GcBarrier::new();
            assert!(barrier.request_stw(ThreadId(5), 1));
            let guard = barrier.enter_blocked();
            assert!(guard.pre_stw);
            drop(guard);
            let _ = barrier.mark_blocked_region_enter();
            barrier.mark_blocked_region_leave();
            assert_eq!(barrier.blocked_count(), 0);
            barrier.complete_gc(cratonvm_types::PointerMap::default());
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
            "the initiator hung waiting out its own pause"
        );
    }

    /// gc-common 2026-09-23 (lane A) — every waiter of one pause receives the
    /// SAME map allocation (a reference count), not a per-thread deep copy
    /// taken under the barrier lock.
    #[test]
    fn waiters_share_one_pointer_map_allocation() {
        let barrier = Arc::new(GcBarrier::new());
        assert!(barrier.request_stw(ThreadId(0), 3));
        let b1 = barrier.clone();
        let b2 = barrier.clone();
        let h1 = std::thread::spawn(move || b1.arrive_and_wait(ThreadId(1)));
        let h2 = std::thread::spawn(move || b2.arrive_and_wait(ThreadId(2)));
        barrier.wait_for_all();
        let mut pm = cratonvm_types::PointerMap::default();
        for i in 0..1024usize {
            pm.insert(0x10_0000 + i * 16, 0x20_0000 + i * 16);
        }
        barrier.complete_gc(pm);
        let m1 = h1.join().unwrap();
        let m2 = h2.join().unwrap();
        assert!(Arc::ptr_eq(&m1, &m2), "both waiters must share one map");
        assert_eq!(m1.len(), 1024);
        assert_eq!(m2.get(&0x10_0010), Some(&0x20_0010));
    }

    /// gc-common 2026-09-23 (lane A) — an initiator the identity census
    /// reports as blocked must not be subtracted from `expected` twice.
    ///
    /// 4 alive: initiator I(0) (flag still raised), running mutators M1(1)
    /// and M2(2), blocked B(3). The census says blocked = {0, 3}. The old
    /// `alive - 1 - blocked` gave 1 and released after ONE of the two running
    /// mutators arrived.
    #[test]
    fn a_blocked_flagged_initiator_is_not_subtracted_twice() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (4, 2, vec![0, 3])));
        assert_eq!(
            barrier.pending_count(),
            2,
            "both running mutators must be waited for"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());

        // The ordinary case is unchanged: initiator not in the blocked set.
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (4, 1, vec![3])));
        assert_eq!(barrier.pending_count(), 2);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// Time-to-safepoint is recorded exactly once per pause, however many
    /// times the initiator polls the quota.
    #[test]
    fn ttsp_is_recorded_once_per_pause() {
        let barrier = GcBarrier::new();
        assert_eq!(barrier.ttsp_stats().pauses, 0);
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.wait_for_all();
        assert!(barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
        assert!(barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let s = barrier.ttsp_stats();
        assert_eq!(s.pauses, 1);
        assert!(s.max_ns >= s.last_ns && s.total_ns >= s.last_ns);

        assert!(barrier.request_stw(ThreadId(0), 1));
        assert!(barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.ttsp_stats().pauses, 2);
    }

    /// gci i46/orch: the coverage slot ([`COVERAGE_SLOT_OWNER`]) and its
    /// gave-up rows are process-wide, so the tests below that take it (through
    /// `request_stw_opening_cycle` or `take_coverage_slot`) run one at a time:
    /// under the parallel runner one test's hold made another's timed wait or
    /// owner assert read a foreign pause (1 run in 4).
    fn coverage_slot_tests_serialised() -> std::sync::MutexGuard<'static, ()> {
        static SLOT_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SLOT_TESTS.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn census(alive: u32, blocked_tids: Vec<u64>, initiator_counted: Option<bool>) -> StwCensus {
        StwCensus {
            alive,
            blocked: u32::try_from(blocked_tids.len()).unwrap_or(u32::MAX),
            blocked_tids,
            initiator_counted,
            excused_running: Vec::new(),
        }
    }

    /// Interpreter round i2 wave 58, lane L6: a thread the census excused in
    /// a running native is out of `expected`, and its arrival -- through any
    /// entry -- does not count, so it cannot meet the quota for a counted
    /// mutator still running. Both request orders.
    #[test]
    fn an_excused_running_native_is_out_of_the_quota_and_never_counts() {
        let _slot = coverage_slot_tests_serialised();
        for lockfree in [false, true] {
            let mut barrier = GcBarrier::new();
            barrier.lockfree_blocked = lockfree;
            let barrier = Arc::new(barrier);
            // Three counted mutators (7, 8, 9); 9 is excused in a native.
            let mut c = census(3, Vec::new(), Some(false));
            c.excused_running = vec![9];
            assert!(barrier.request_stw_opening_cycle(ThreadId(1), || c, || {}));
            assert_eq!(barrier.pending_count(), 2, "lockfree={lockfree}");
            // The excused thread reaches a poll from inside its native: it
            // waits the pause out without filling a slot.
            let b = Arc::clone(&barrier);
            let excused = std::thread::spawn(move || {
                let _ = b.arrive_and_wait(ThreadId(9));
            });
            let t0 = std::time::Instant::now();
            while t0.elapsed() < std::time::Duration::from_millis(50) {
                std::thread::yield_now();
            }
            assert_eq!(barrier.pending_count(), 2, "the excused arrival counted");
            let b7 = Arc::clone(&barrier);
            let b8 = Arc::clone(&barrier);
            let a = std::thread::spawn(move || {
                let _ = b7.arrive_and_wait(ThreadId(7));
            });
            let b = std::thread::spawn(move || {
                let _ = b8.arrive_and_wait(ThreadId(8));
            });
            barrier.wait_for_all();
            barrier.complete_gc(cratonvm_types::PointerMap::default());
            for h in [excused, a, b] {
                h.join().unwrap();
            }
            // The next pause excused nobody: 9 counts again.
            assert!(barrier.request_stw_opening_cycle(
                ThreadId(1),
                || census(3, Vec::new(), Some(false)),
                || {}
            ));
            assert_eq!(barrier.pending_count(), 3);
            barrier.complete_gc(cratonvm_types::PointerMap::default());
        }
    }

    /// gcj j2/cm2 (j1-cm registry residuals item 5): a thread the census
    /// excluded as blocked is not subtracted from `expected` a second time when
    /// it is also named excused -- the state a bare lowering of the blocked
    /// flag between the census and the excusal walk would produce. Both orders.
    #[test]
    fn gcj2cm2_an_excused_thread_already_blocked_is_not_subtracted_twice() {
        let _slot = coverage_slot_tests_serialised();
        for lockfree in [false, true] {
            let mut barrier = GcBarrier::new();
            barrier.lockfree_blocked = lockfree;
            // Three alive: 7 counted, 8 counted, 9 blocked AND named excused.
            let mut c = census(3, vec![9], Some(false));
            c.excused_running = vec![9];
            assert!(barrier.request_stw_opening_cycle(ThreadId(1), || c, || {}));
            assert_eq!(barrier.pending_count(), 2, "lockfree={lockfree}");
            barrier.complete_gc(cratonvm_types::PointerMap::default());
        }
    }

    /// gc-common w2-a — the coverage cycle is opened by the WINNING request
    /// only, under the barrier lock and before `stw_requested` is visible; a
    /// losing request neither opens it nor runs its census.
    ///
    /// This is the ordering that closes both coverage-reset races
    /// (`common-a-losing-initiator-resets-coverage-state`,
    /// `common-e-coverage-cycle-reset-races-a-sibling-initiator`): a reset that
    /// only a winner performs, before any peer can observe its pause, can erase
    /// neither another pause's deposits nor a sibling's pre-request marks.
    #[test]
    fn only_the_winning_request_opens_the_coverage_cycle() {
        let _slot = coverage_slot_tests_serialised();
        let barrier = GcBarrier::new();
        let opened = std::cell::Cell::new(0u32);
        let censused = std::cell::Cell::new(0u32);
        let won = barrier.request_stw_opening_cycle(
            ThreadId(1),
            || {
                censused.set(censused.get() + 1);
                census(1, vec![], Some(true))
            },
            || {
                assert!(
                    !barrier.stw_requested.load(Ordering::Acquire),
                    "the cycle must open BEFORE the pause is visible to any peer"
                );
                opened.set(opened.get() + 1);
            },
        );
        assert!(won);
        assert_eq!((opened.get(), censused.get()), (1, 1));

        // A second would-be initiator while the pause is in flight: loses,
        // resets nothing, and pays no census under the lock.
        let lost = barrier.request_stw_opening_cycle(
            ThreadId(2),
            || {
                censused.set(censused.get() + 1);
                census(2, vec![], Some(true))
            },
            || opened.set(opened.get() + 1),
        );
        assert!(!lost);
        assert_eq!(
            (opened.get(), censused.get()),
            (1, 1),
            "a losing request must not reset the winner's per-pause state"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());

        // The next pause opens its own cycle.
        assert!(barrier.request_stw_opening_cycle(
            ThreadId(2),
            || census(1, vec![], Some(true)),
            || opened.set(opened.get() + 1),
        ));
        assert_eq!(opened.get(), 2);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// gc-common w6-a — a second VM's pause does not open (and so cannot reset
    /// the process-global per-pause coverage rows) while the first VM's pause
    /// owns them; it opens as soon as the first completes. A sibling request of
    /// the OWNING barrier does not wait at all, and a dropped barrier releases
    /// the slot. `common-a-process-global-gc-coordination-state-FIXED-20260926.md`.
    ///
    /// Other tests in this binary run real VMs whose pauses take the same
    /// process slot, so the ordering half is asserted only when barrier A
    /// actually got it (it can time out behind a sibling test's long pause).
    #[test]
    fn a_second_vms_pause_waits_for_the_first_vms_coverage_cycle() {
        let _slot = coverage_slot_tests_serialised();
        let a = Arc::new(GcBarrier::new());
        let b = Arc::new(GcBarrier::new());
        assert_ne!(a.slot_id, b.slot_id);
        assert_ne!(a.slot_id, 0);
        assert!(a.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
        let a_owns = a.inner.lock().holds_coverage_slot;
        if a_owns {
            assert_eq!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), a.slot_id);
            // A losing sibling initiator of A's own pause: no wait.
            let t0 = Instant::now();
            assert!(!a.request_stw_opening_cycle(
                ThreadId(2),
                || census(2, vec![], Some(true)),
                || panic!("a losing request must not open a cycle")
            ));
            assert!(
                t0.elapsed() < COVERAGE_SLOT_WAIT_LIMIT,
                "the owner's own sibling request must not wait for itself"
            );
            assert!(
                a.inner.lock().holds_coverage_slot,
                "and must not release it"
            );
        }

        let opened = Arc::new(AtomicBool::new(false));
        let b_thread = {
            let b = Arc::clone(&b);
            let opened = Arc::clone(&opened);
            std::thread::spawn(move || {
                let won = b.request_stw_opening_cycle(
                    ThreadId(3),
                    || census(1, vec![], Some(true)),
                    || opened.store(true, Ordering::SeqCst),
                );
                b.complete_gc(cratonvm_types::PointerMap::default());
                won
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        if a_owns {
            assert!(
                !opened.load(Ordering::SeqCst),
                "VM B must not open (reset) its coverage cycle while VM A's pause owns it"
            );
        }
        a.complete_gc(cratonvm_types::PointerMap::default());
        assert!(!a.inner.lock().holds_coverage_slot);
        assert!(
            b_thread.join().expect("B's request thread"),
            "B's pause runs after A's"
        );
        assert!(opened.load(Ordering::SeqCst));
        assert_ne!(
            COVERAGE_SLOT_OWNER.load(Ordering::Acquire),
            b.slot_id,
            "B released it"
        );

        // A barrier dropped mid-pause frees the slot for everybody else.
        let c = GcBarrier::new();
        let c_id = c.slot_id;
        assert!(c.request_stw_opening_cycle(ThreadId(4), || census(1, vec![], Some(true)), || {}));
        drop(c);
        assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), c_id);
    }

    /// gc-common w7-a — a request that took the slot and then LOST to a
    /// sibling of its own barrier (which won without it) hands the slot to that
    /// pause, whose `complete_gc` releases it; with no pause of its own in
    /// flight it releases it. The race itself (T2 wins between T1's
    /// compare-exchange and T1's `inner.lock()`) is not schedulable from a
    /// test, so this drives `settle_unused_slot` from the state it leaves.
    /// Guarded like the test above: another test's VM may own the slot.
    #[test]
    fn a_losing_sibling_hands_the_slot_to_its_barriers_pause() {
        let _slot = coverage_slot_tests_serialised();
        let a = GcBarrier::new();
        // A pause of `a` that runs WITHOUT the slot (T2's win).
        assert!(a.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
        let had = std::mem::take(&mut a.inner.lock().holds_coverage_slot);
        if had {
            a.release_coverage_slot();
        }
        // T1 took the slot before queuing on `inner`, then lost.
        if a.try_take_coverage_slot() {
            {
                let mut inner = a.inner.lock();
                a.settle_unused_slot(&mut inner, true);
                assert!(
                    inner.holds_coverage_slot,
                    "the pause in flight must hold the slot the loser took"
                );
            }
            assert_eq!(
                COVERAGE_SLOT_OWNER.load(Ordering::Acquire),
                a.slot_id,
                "the loser must not release the slot under its sibling's pause"
            );
            a.complete_gc(cratonvm_types::PointerMap::default());
            assert_ne!(
                COVERAGE_SLOT_OWNER.load(Ordering::Acquire),
                a.slot_id,
                "the pause's complete_gc releases it"
            );
        } else {
            a.complete_gc(cratonvm_types::PointerMap::default());
        }
        // No pause in flight: a lost request's slot is simply released.
        if a.try_take_coverage_slot() {
            {
                let mut inner = a.inner.lock();
                a.settle_unused_slot(&mut inner, true);
                assert!(!inner.holds_coverage_slot);
            }
            assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), a.slot_id);
        }
    }

    /// gc-common w7-a — a hold that one request waited the full limit for is
    /// not waited for again until it is released
    /// (`COVERAGE_SLOT_GAVE_UP_RELEASES`), and its release re-arms the wait.
    /// Guarded on `x` actually owning the slot.
    #[test]
    fn a_hold_given_up_on_once_is_not_waited_for_again() {
        let _slot = coverage_slot_tests_serialised();
        let x = GcBarrier::new();
        let y = GcBarrier::new();
        assert!(x.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
        if x.inner.lock().holds_coverage_slot {
            let limit = Duration::from_millis(20);
            let t0 = Instant::now();
            assert!(!y.take_coverage_slot(limit), "x's pause holds the slot");
            assert!(
                t0.elapsed() >= limit,
                "the first request waits the limit out"
            );
            assert!(coverage_slot_hold_abandoned(x.slot_id));
            let t1 = Instant::now();
            assert!(!y.take_coverage_slot(Duration::from_secs(10)));
            assert!(
                t1.elapsed() < Duration::from_secs(5),
                "the same hold must not be waited for twice"
            );
        }
        x.complete_gc(cratonvm_types::PointerMap::default());
        assert!(
            !coverage_slot_hold_abandoned(x.slot_id),
            "a release ends the hold that was given up on"
        );
    }

    /// gc-common w2-a — an initiator the census states it did NOT count owns
    /// no slot in `alive`; `None` keeps the historical `- 1`.
    #[test]
    fn an_initiator_outside_the_census_takes_no_slot() {
        let _slot = coverage_slot_tests_serialised();
        let barrier = GcBarrier::new();
        // Two counted mutators, neither of them the initiator.
        assert!(barrier.request_stw_opening_cycle(
            ThreadId(9),
            || census(2, vec![], Some(false)),
            || {}
        ));
        assert_eq!(
            barrier.pending_count(),
            2,
            "both real mutators must be waited for"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());

        assert!(barrier.request_stw_opening_cycle(
            ThreadId(1),
            || census(2, vec![], Some(true)),
            || {}
        ));
        assert_eq!(barrier.pending_count(), 1);
        barrier.complete_gc(cratonvm_types::PointerMap::default());

        assert!(barrier.request_stw_opening_cycle(ThreadId(1), || census(2, vec![], None), || {}));
        assert_eq!(
            barrier.pending_count(),
            1,
            "unknown keeps the legacy arithmetic"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());

        // Blocked-and-counted initiator: the blocked subtraction already
        // removes it, whatever `initiator_counted` says.
        assert!(barrier.request_stw_opening_cycle(
            ThreadId(1),
            || census(3, vec![1], Some(true)),
            || {}
        ));
        assert_eq!(barrier.pending_count(), 2);
        barrier.complete_gc(cratonvm_types::PointerMap::default());

        assert_eq!(initiator_slot(false, Some(true)), 1);
        assert_eq!(initiator_slot(false, None), 1);
        assert_eq!(initiator_slot(false, Some(false)), 0);
        assert_eq!(initiator_slot(true, Some(true)), 0);
    }

    /// gc-common w2-a — a waiter that slept through a LATER pause is handed the
    /// composition of its own pause's map and every later one, not the newest
    /// map alone. Driven deterministically through the barrier's own
    /// bookkeeping: a parked waiter for generation 0 is registered, two pauses
    /// complete, and the maps it would be given are inspected.
    #[test]
    fn a_waiter_straddling_two_pauses_gets_the_composed_map() {
        let barrier = GcBarrier::new();
        barrier.inner.lock().note_parked(0);

        assert!(barrier.request_stw(ThreadId(0), 1));
        let mut m1 = cratonvm_types::PointerMap::default();
        m1.insert(0x100, 0x200); // moved in G, and again in G+1
        m1.insert(0x300, 0x400); // moved in G only
        barrier.complete_gc(m1);

        assert!(barrier.request_stw(ThreadId(0), 1));
        let mut m2 = cratonvm_types::PointerMap::default();
        m2.insert(0x200, 0x500);
        m2.insert(0x600, 0x700); // moved in G+1 only
        barrier.complete_gc(m2);

        let maps = match barrier.inner.lock().maps_for_waiter(0) {
            WaiterMaps::Straddled(maps) => maps,
            WaiterMaps::Own(_) => panic!("a waiter for gen 1 woke at gen 2: not its own map"),
            WaiterMaps::Missing => panic!("the straddled pause's map must still be held"),
        };
        assert_eq!(maps.len(), 2);
        let c = compose_pointer_maps(&maps);
        assert_eq!(c.get(&0x100), Some(&0x500), "chased through both pauses");
        assert_eq!(c.get(&0x300), Some(&0x400), "moved by its own pause only");
        assert_eq!(c.get(&0x600), Some(&0x700), "moved by the later pause only");
        assert_eq!(c.get(&0x200), Some(&0x500));

        barrier.inner.lock().note_unparked(0);
        assert!(
            barrier.inner.lock().map_history.is_empty(),
            "the history must be released once no waiter needs it"
        );
    }

    /// Without a straddling waiter nothing is retained: the common path keeps
    /// exactly one map alive, as before.
    #[test]
    fn no_map_history_without_a_straddling_waiter() {
        let barrier = Arc::new(GcBarrier::new());
        for round in 0..3usize {
            assert!(barrier.request_stw(ThreadId(0), 2));
            let b = barrier.clone();
            let h = std::thread::spawn(move || b.arrive_and_wait(ThreadId(1)));
            barrier.wait_for_all();
            let mut pm = cratonvm_types::PointerMap::default();
            pm.insert(0x1000 + round * 16, 0x2000 + round * 16);
            barrier.complete_gc(pm);
            let got = h.join().unwrap();
            assert_eq!(
                got.get(&(0x1000 + round * 16)),
                Some(&(0x2000 + round * 16))
            );
            let inner = barrier.inner.lock();
            assert!(inner.map_history.is_empty());
            assert!(inner.parked_waiters.is_empty());
        }
    }

    /// The real-thread version of the straddle, asserted in the form that
    /// cannot flake: whether or not the waiter wakes between the two pauses,
    /// the map it receives must carry ITS OWN pause's relocation of `0x100`.
    /// The pre-w2 barrier handed a late waiter the newest map alone, which has
    /// no entry for `0x100` at all.
    #[test]
    fn a_late_waiter_never_receives_only_a_later_pauses_map() {
        let barrier = Arc::new(GcBarrier::new());
        // W (tid 2) is excluded as blocked from both pauses, like a
        // blocked-region arrival.
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![2])));
        let b = barrier.clone();
        let h = std::thread::spawn(move || b.arrive_and_wait_auto(ThreadId(2)));
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while barrier.inner.lock().parked_waiters.is_empty() {
            assert!(Instant::now() < deadline, "waiter never parked");
            std::thread::yield_now();
        }
        let mut m1 = cratonvm_types::PointerMap::default();
        m1.insert(0x100, 0x200);
        barrier.complete_gc(m1);
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![2])));
        let mut m2 = cratonvm_types::PointerMap::default();
        m2.insert(0x200, 0x500);
        barrier.complete_gc(m2);
        let got = h.join().unwrap();
        let v = got.get(&0x100).copied();
        assert!(
            v == Some(0x200) || v == Some(0x500),
            "a waiter for pause G must get G's relocation (alone, or composed with G+1), got {v:?}"
        );
        assert!(barrier.inner.lock().map_history.is_empty());
    }

    /// gc-common w8-d — a winning request binds its OWN VM's pause ledger on
    /// the initiator's thread and `complete_gc` unbinds it; a peer's deposit
    /// bound to another barrier never reaches this initiator's reading, and
    /// another VM's request never resets this VM's ledger. Deterministic even
    /// beside other tests' VMs: each ledger is one barrier's, and the binding
    /// is per thread.
    #[test]
    fn a_pause_reads_and_resets_only_its_own_vms_ledger() {
        use cratonvm_gc::gc_quiescence as q;
        let a = Arc::new(GcBarrier::new());
        let b = Arc::new(GcBarrier::new());
        assert!(a.request_stw(ThreadId(0), 1));
        assert!(q::pause_ledger_bound(a.pause_ledger()));
        // gc-common w10-g: A's take-over froze a peer; the verdict is A's.
        let frozen = q::TakeoverVerdict {
            frozen: 1,
            pins_complete: false,
            ..q::TakeoverVerdict::NONE
        };
        q::publish_takeover_verdict(frozen);
        // A peer of VM B, parking for B's pause, deposits into B's ledger.
        let bb = Arc::clone(&b);
        std::thread::spawn(move || {
            q::with_pause_ledger(bb.pause_ledger(), || q::add_peer_proven_jit_depth(6));
        })
        .join()
        .expect("B's peer");
        assert_eq!(
            q::peer_proven_jit_depth(),
            0,
            "A's initiator must not be credited with B's peer's proof"
        );
        // A peer of VM A deposits into A's.
        let aa = Arc::clone(&a);
        std::thread::spawn(move || {
            q::with_pause_ledger(aa.pause_ledger(), || q::add_peer_proven_jit_depth(2));
        })
        .join()
        .expect("A's peer");
        assert_eq!(q::peer_proven_jit_depth(), 2);
        // VM B opens a pause on its own thread: B's ledger resets, A's stands.
        let bb = Arc::clone(&b);
        std::thread::spawn(move || {
            assert!(bb.request_stw(ThreadId(0), 1));
            assert_eq!(bb.pause_ledger().proven_jit_depth(), 0);
            assert_eq!(
                q::takeover_verdict(),
                q::TakeoverVerdict::NONE,
                "B's pause opens clear"
            );
            bb.complete_gc(cratonvm_types::PointerMap::default());
            assert!(!q::pause_ledger_bound(bb.pause_ledger()));
        })
        .join()
        .expect("B's initiator");
        assert_eq!(
            q::peer_proven_jit_depth(),
            2,
            "B's pause must not reset A's ledger"
        );
        assert_eq!(
            q::takeover_verdict(),
            frozen,
            "B's pause must not reset A's take-over verdict to `Move`"
        );
        a.complete_gc(cratonvm_types::PointerMap::default());
        assert!(!q::pause_ledger_bound(a.pause_ledger()));
        // A's next pause opens A's ledger afresh.
        assert!(a.request_stw(ThreadId(0), 1));
        assert_eq!(q::peer_proven_jit_depth(), 0);
        assert_eq!(q::takeover_verdict(), q::TakeoverVerdict::NONE);
        a.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// gc-common w18-c — the blocked/frozen-peer native-stack captures (the
    /// `PEER_STACK_SLOTS` process static until then) are the pausing VM's.
    /// Two VMs pause CONCURRENTLY on two threads: A's initiator records, then
    /// B's request opens B's pause and B's opener discards B's captures, then
    /// B records. Each initiator then reads and drains exactly its own words:
    /// B's opening no longer discards A's, and A's fold no longer adopts B's
    /// (as `UNROUTED`, a false repair outage) or folds them against A's map.
    /// Uses the test-only request entry, which takes no coverage slot, so the
    /// two pauses really overlap.
    #[test]
    fn concurrent_pauses_capture_and_drain_only_their_own_peer_stack_words() {
        use cratonvm_gc::gc_quiescence as q;
        if !q::blocked_peer_stack_remap_enabled() {
            // `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP` is set for this
            // process: nothing is captured, so there is nothing to isolate.
            return;
        }
        let a = Arc::new(GcBarrier::new());
        let b = Arc::new(GcBarrier::new());
        let a_recorded = Arc::new(std::sync::Barrier::new(2));
        let b_recorded = Arc::new(std::sync::Barrier::new(2));
        type Seen = (Vec<usize>, Vec<(u32, usize, usize)>, bool);
        let a_side = {
            let (a, a_recorded, b_recorded) = (a.clone(), a_recorded.clone(), b_recorded.clone());
            std::thread::spawn(move || -> Seen {
                assert!(a.request_stw(ThreadId(0), 1));
                q::record_peer_stack_slot(0xA18, 0x7a18_0000, 0x7a18_1000);
                q::record_takeover_stack_slot(0xA18, 0x7a18_0008, 0x7a18_2000);
                a_recorded.wait();
                // B opens, discards and records while A's pause is in flight.
                b_recorded.wait();
                let mut values = Vec::new();
                q::peer_stack_slot_values_into(&mut values, |_| true);
                let saturated = q::peer_stack_slots_saturated();
                let drained = q::take_peer_stack_slots();
                a.complete_gc(cratonvm_types::PointerMap::default());
                (values, drained, saturated)
            })
        };
        let b_side = {
            let b = b.clone();
            std::thread::spawn(move || -> Seen {
                a_recorded.wait();
                assert!(b.request_stw(ThreadId(0), 1));
                // What a collection's opener does (`begin_moving_young_coverage_cycle`).
                q::clear_peer_stack_slots();
                q::record_peer_stack_slot(0xB18, 0x7b18_0000, 0x7b18_1000);
                b_recorded.wait();
                let mut values = Vec::new();
                q::peer_stack_slot_values_into(&mut values, |_| true);
                let saturated = q::peer_stack_slots_saturated();
                let drained = q::take_peer_stack_slots();
                b.complete_gc(cratonvm_types::PointerMap::default());
                (values, drained, saturated)
            })
        };
        let (a_values, a_drained, a_saturated) = a_side.join().expect("A's initiator");
        let (b_values, b_drained, b_saturated) = b_side.join().expect("B's initiator");
        assert_eq!(
            a_drained,
            vec![
                (0xA18, 0x7a18_0000, 0x7a18_1000),
                (0xA18, 0x7a18_0008, 0x7a18_2000)
            ],
            "A's fold drains A's captures, all of them, and none of B's"
        );
        assert_eq!(a_values, vec![0x7a18_1000, 0x7a18_2000]);
        assert_eq!(
            b_drained,
            vec![(0xB18, 0x7b18_0000, 0x7b18_1000)],
            "B's are B's only"
        );
        assert_eq!(b_values, vec![0x7b18_1000]);
        assert!(!a_saturated && !b_saturated);
        // Both pauses are over; their ledgers are empty, and an unbound thread
        // drains only its own (empty) fallback.
        assert!(std::thread::spawn(q::take_peer_stack_slots)
            .join()
            .expect("unbound drain")
            .is_empty());
        assert!(
            !q::pause_ledger_bound(a.pause_ledger()) && !q::pause_ledger_bound(b.pause_ledger())
        );
    }

    /// gc-common w21-e — the moving-young coverage rows (the verdict, its
    /// reasons, the un-rewritable flag, the conservative-scan count and the
    /// helper-window pins; process statics until then) are the pausing VM's.
    /// Two VMs pause CONCURRENTLY on two threads: A's initiator opens its
    /// coverage cycle and records a frozen peer, then B's request opens B's
    /// cycle (which used to erase A's verdict and pins: licence `Move` behind a
    /// frozen peer) and records its own. A's collector then reads exactly A's
    /// rows. Afterwards a blocking deposit of VM B made OUTSIDE every pause (a
    /// scoped binding on a third thread, as `deposit_root_snapshot_inner` does)
    /// lands in B's rows, survives A's next coverage open, and is cleared by
    /// B's. Uses the test-only request entry, which takes no coverage slot, so
    /// the pauses really overlap. Negative checks read the ledgers directly:
    /// the free functions also OR in the process's orphan rows, which other
    /// tests of this binary may write.
    #[test]
    fn concurrent_pauses_keep_their_own_coverage_verdicts_and_pins() {
        use cratonvm_gc::gc_quiescence as q;
        const PIN_A: usize = 0x7a21_e000;
        const PIN_B: usize = 0x7b21_e000;
        let a = Arc::new(GcBarrier::new());
        let b = Arc::new(GcBarrier::new());
        let a_marked = Arc::new(std::sync::Barrier::new(2));
        let b_marked = Arc::new(std::sync::Barrier::new(2));
        let a_side = {
            let (a, a_marked, b_marked) = (a.clone(), a_marked.clone(), b_marked.clone());
            std::thread::spawn(move || -> (bool, usize, bool, usize, Vec<usize>) {
                assert!(a.request_stw(ThreadId(0), 1));
                // What a collection's `open_cycle` does, then A's take-over.
                q::begin_moving_young_coverage_cycle();
                q::mark_moving_young_coverage_incomplete_because(q::incomplete_reason::XT_TAKEOVER);
                q::note_conservative_jit_scan();
                q::add_xt_cycle_pinned_jit_roots(&[PIN_A]);
                a_marked.wait();
                // B opens and records while A's pause is in flight.
                b_marked.wait();
                let seen = (
                    q::moving_young_coverage_incomplete(),
                    q::moving_young_incomplete_reason(),
                    q::unrewritable_peer_state(),
                    q::conservative_jit_scans(),
                    q::pinned_jit_roots_snapshot(),
                );
                a.complete_gc(cratonvm_types::PointerMap::default());
                seen
            })
        };
        let b_side = {
            let b = b.clone();
            std::thread::spawn(move || -> q::CoverageSnapshot {
                a_marked.wait();
                assert!(b.request_stw(ThreadId(0), 1));
                q::begin_moving_young_coverage_cycle();
                let opened = b.pause_ledger().coverage_snapshot();
                q::mark_moving_young_coverage_incomplete_because(
                    q::incomplete_reason::UNPUBLISHED_FRAME_OOP,
                );
                q::add_xt_cycle_pinned_jit_roots(&[PIN_B]);
                b_marked.wait();
                b.complete_gc(cratonvm_types::PointerMap::default());
                opened
            })
        };
        let (incomplete, reason, unrewritable, scans, pins) = a_side.join().expect("A's initiator");
        let b_opened = b_side.join().expect("B's initiator");
        assert_eq!(
            b_opened,
            q::CoverageSnapshot::default(),
            "B's cycle opens without A's rows"
        );
        assert!(incomplete, "B's coverage open must not erase A's verdict");
        assert_eq!(
            reason,
            q::incomplete_reason::XT_TAKEOVER,
            "A's collector reads A's reason"
        );
        assert!(unrewritable, "A's frozen peer still forbids promotion");
        assert!(scans >= 1, "A's conservative scan is still counted");
        assert!(
            pins.contains(&PIN_A),
            "B's open must not drop A's helper-window pin"
        );
        assert!(!pins.contains(&PIN_B), "B's pin is not A's");
        assert_eq!(
            a.pause_ledger().coverage_snapshot(),
            q::CoverageSnapshot {
                incomplete: true,
                reason: q::incomplete_reason::XT_TAKEOVER,
                reason_mask: 1usize << q::incomplete_reason::XT_TAKEOVER,
                unrewritable: true,
                scans: 1,
                xt_pins: vec![PIN_A],
            }
        );
        assert_eq!(b.pause_ledger().coverage_snapshot().xt_pins, vec![PIN_B]);

        // A blocking deposit of VM B, outside every pause.
        let bb = b.clone();
        std::thread::spawn(move || {
            let _scope = q::scoped_pause_ledger(bb.pause_ledger());
            q::mark_moving_young_coverage_incomplete_because(
                q::incomplete_reason::UNREGISTERED_JIT_FRAME,
            );
            q::note_conservative_jit_scan();
        })
        .join()
        .expect("B's blocking thread");
        let b_rows = b.pause_ledger().coverage_snapshot();
        assert!(
            b_rows.incomplete && b_rows.scans == 1,
            "a writer outside every pause lands in its VM's rows"
        );
        // A's next collection opens A's rows only.
        assert!(a.request_stw(ThreadId(0), 1));
        q::begin_moving_young_coverage_cycle();
        assert_eq!(
            a.pause_ledger().coverage_snapshot(),
            q::CoverageSnapshot::default()
        );
        a.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(
            b.pause_ledger().coverage_snapshot(),
            b_rows,
            "A's coverage open must not clear B's rows"
        );
        // B's own next collection does.
        assert!(b.request_stw(ThreadId(0), 1));
        q::begin_moving_young_coverage_cycle();
        assert_eq!(
            b.pause_ledger().coverage_snapshot(),
            q::CoverageSnapshot::default()
        );
        b.complete_gc(cratonvm_types::PointerMap::default());
        assert!(
            !q::pause_ledger_bound(a.pause_ledger()) && !q::pause_ledger_bound(b.pause_ledger())
        );
    }

    /// gc-common w8-d — a production pause that went ahead WITHOUT the
    /// coverage slot takes it at its next barrier wait once it is free,
    /// instead of running slot-less to `complete_gc`; `complete_gc` releases
    /// it. A test-only request (which never wants the slot) does not. Guarded
    /// like the tests above: another test's VM may own the slot.
    #[test]
    fn a_pause_without_the_slot_adopts_it_once_free() {
        let _slot = coverage_slot_tests_serialised();
        let x = GcBarrier::new();
        assert!(x.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
        // Make it the pause that went ahead without the slot.
        let had = std::mem::take(&mut x.inner.lock().holds_coverage_slot);
        if had {
            x.release_coverage_slot();
        }
        assert!(x.wait_for_all_timeout(Duration::from_millis(1)));
        if x.inner.lock().holds_coverage_slot {
            assert_eq!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), x.slot_id);
            x.complete_gc(cratonvm_types::PointerMap::default());
            assert_ne!(
                COVERAGE_SLOT_OWNER.load(Ordering::Acquire),
                x.slot_id,
                "complete_gc releases the adopted slot"
            );
        } else {
            // Another test's VM holds it: nothing to adopt yet.
            assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), x.slot_id);
            x.complete_gc(cratonvm_types::PointerMap::default());
        }
        assert!(!x.inner.lock().wants_coverage_slot);

        // The test-only entries never want it.
        let y = GcBarrier::new();
        assert!(y.request_stw(ThreadId(0), 1));
        assert!(y.wait_for_all_timeout(Duration::from_millis(1)));
        assert!(!y.inner.lock().holds_coverage_slot);
        assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), y.slot_id);
        y.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// gcg g2/k (`common-a-proposal-lock-free-blocked-transitions-DONE-20261015`): with
    /// the lock-free transitions on, a request raises `stw_requested` BEFORE
    /// its census (with them off, after), an entry with no pause up reads
    /// `pre_stw == false` and is seen blocked by the next census, an entry
    /// during the pause reads `true`, and the plain leave releases the
    /// anonymous count without the lock once the pause is over.
    #[test]
    fn gcg_k2_lockfree_request_raises_the_flag_before_its_census() {
        let _slot = coverage_slot_tests_serialised();
        for lockfree in [false, true] {
            let b = GcBarrier::with_lockfree_blocked(lockfree);
            let flag = AtomicBool::new(true); // the entering thread's deposit
            assert!(!b.mark_blocked_region_enter(), "no pause is up yet");
            let flag_seen_by_census = std::cell::Cell::new(None);
            assert!(b.request_stw_opening_cycle(
                ThreadId(1),
                || {
                    flag_seen_by_census.set(Some(b.stw_requested.load(Ordering::SeqCst)));
                    let blocked = if flag.load(Ordering::SeqCst) {
                        vec![42]
                    } else {
                        vec![]
                    };
                    census(2, blocked, Some(true))
                },
                || {},
            ));
            assert_eq!(
                flag_seen_by_census.get(),
                Some(lockfree),
                "the census runs after the flag store exactly when lock-free"
            );
            {
                let inner = b.inner.lock();
                assert_eq!(inner.expected, 0, "the blocked thread is excluded");
                assert!(inner.excluded_blocked.contains(&42));
            }
            // An entry while the pause is up (from the initiator's own thread,
            // so the locked leave below does not wait for this pause).
            assert!(b.mark_blocked_region_enter(), "a pause is up");
            assert_eq!(b.blocked_count(), 2);
            b.mark_blocked_region_leave();
            assert!(b.wait_for_all_timeout(Duration::from_millis(1)));
            b.complete_gc(cratonvm_types::PointerMap::default());
            b.mark_blocked_region_leave();
            assert_eq!(b.blocked_count(), 0);
            drop(b.enter_blocked());
            assert_eq!(b.blocked_count(), 0, "the guard releases what it took");
        }
    }

    /// One mutator entering and leaving a blocked region in a loop against a
    /// requester running pauses back to back. The invariant the entry lock
    /// (default) and the Dekker pair (lock-free) must both give: an entry
    /// that read `pre_stw == false` is EXCLUDED by every pause in progress
    /// while its `in_blocked_region` is still up. Returns the violations.
    fn race_blocked_entry_against_census(lockfree: bool, pauses: u32) -> u64 {
        race_blocked_entry_against_census_counted(lockfree, true, pauses)
    }

    /// [`race_blocked_entry_against_census`] with the anonymous blocked
    /// counter on (`counted`, the default) or off
    /// (`CRATONVM_STW_NO_BLOCKED_COUNTER`, gcg g3/k). Also checks, after the
    /// race, that the counter is back at 0 (on) or never moved (off).
    fn race_blocked_entry_against_census_counted(
        lockfree: bool,
        counted: bool,
        pauses: u32,
    ) -> u64 {
        const MUTATOR: ThreadId = ThreadId(42);
        let mut barrier = GcBarrier::with_lockfree_blocked(lockfree);
        barrier.anon_blocked_counted = counted;
        let b = Arc::new(barrier);
        let max_seen = Arc::new(AtomicU64::new(0));
        let flag = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let violations = Arc::new(AtomicU64::new(0));
        let mutator = {
            let (b, flag, stop, violations, max_seen) = (
                Arc::clone(&b),
                Arc::clone(&flag),
                Arc::clone(&stop),
                Arc::clone(&violations),
                Arc::clone(&max_seen),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    // A running mutator's safepoint.
                    let _ = b.arrive_and_wait_auto(MUTATOR);
                    // The deposit raises the identity flag, then the entry.
                    flag.store(true, Ordering::Release);
                    let entered = b.mark_blocked_region_enter();
                    max_seen.fetch_max(b.blocked_count(), Ordering::Relaxed);
                    if entered {
                        let _ = b.arrive_and_wait_auto(MUTATOR);
                    } else {
                        let inner = b.inner.lock();
                        if b.stw_requested.load(Ordering::Acquire)
                            && !inner.excluded_blocked.contains(&MUTATOR.0)
                        {
                            violations.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    b.mark_blocked_region_leave();
                    b.leave_blocked_region_flagged(MUTATOR, &flag);
                }
            })
        };
        for _ in 0..pauses {
            let won = b.request_stw_opening_cycle(
                ThreadId(1),
                || {
                    let blocked = if flag.load(Ordering::Acquire) {
                        vec![MUTATOR.0]
                    } else {
                        vec![]
                    };
                    census(2, blocked, Some(true))
                },
                || {},
            );
            assert!(won, "the only requester wins");
            let t0 = Instant::now();
            while !b.wait_for_all_timeout(Duration::from_millis(1)) {
                assert!(
                    t0.elapsed() < Duration::from_secs(20),
                    "a counted mutator never arrived (lockfree={lockfree})"
                );
            }
            b.complete_gc(cratonvm_types::PointerMap::default());
        }
        stop.store(true, Ordering::Release);
        mutator.join().expect("mutator thread");
        assert_eq!(b.blocked_count(), 0, "every entry was left again");
        let max_seen = max_seen.load(Ordering::Relaxed);
        if counted {
            assert_eq!(
                max_seen, 1,
                "positive control: the counted entry is visible"
            );
        } else {
            assert_eq!(max_seen, 0, "the counter is off: no transition touches it");
        }
        violations.load(Ordering::Relaxed)
    }

    /// gcg g2/k: the race above, with the entry lock (default) and with the
    /// lock-free transitions. Zero violations and no hang in both.
    #[test]
    fn gcg_k2_blocked_entry_and_census_agree_under_a_race() {
        assert_eq!(race_blocked_entry_against_census(false, 1500), 0);
        assert_eq!(race_blocked_entry_against_census(true, 1500), 0);
    }

    /// gcg g3/k: a production request that waited for another VM's pause to
    /// free the coverage slot records that wait (`slot_wait_us=` on the
    /// `[stw-ttsp]` line), which its `ttsp_us` does not include.
    #[test]
    fn gcg_k3_a_won_request_records_its_coverage_slot_wait() {
        let _slot = coverage_slot_tests_serialised();
        // VM A's whole pause (request .. complete) on one thread of its own,
        // holding the slot for ~40 ms.
        //
        // gci i44/sp: the 20 ms floor is asserted only when b's request began
        // with at least 30 ms of a's hold still to run. The hold is a's 40 ms
        // sleep from the instant it reported owning the slot; on a loaded
        // test binary b's thread was seen to resume from `recv` late enough
        // that most of the hold had passed (1 run in 4 under
        // `-- gci_i44 gc_barrier`), and the wait it then recorded was
        // correctly short. The 10 ms slack covers b's own lag between `t0`
        // and the request's slot clock.
        let (tx, rx) = std::sync::mpsc::channel::<(bool, Instant)>();
        let releaser = std::thread::spawn(move || {
            let a = GcBarrier::with_coverage_slot_no_wait(false);
            assert!(a.request_stw_opening_cycle(
                ThreadId(1),
                || census(1, vec![], Some(true)),
                || {}
            ));
            let owns = a.inner.lock().holds_coverage_slot;
            // Taken before the sleep: the slot is released no earlier than
            // 40 ms after this instant.
            tx.send((owns, Instant::now())).unwrap();
            std::thread::sleep(Duration::from_millis(40));
            a.complete_gc(cratonvm_types::PointerMap::default());
        });
        let (a_owns, hold_from) = rx.recv().unwrap();
        let b = GcBarrier::with_coverage_slot_no_wait(false);
        let t0 = Instant::now();
        let hold_left = Duration::from_millis(40).saturating_sub(t0.duration_since(hold_from));
        assert!(b.request_stw_opening_cycle(ThreadId(2), || census(1, vec![], Some(true)), || {}));
        let waited = t0.elapsed();
        let recorded = Duration::from_nanos(b.inner.lock().slot_wait_ns);
        assert!(
            recorded <= waited,
            "recorded {recorded:?} > the request's own {waited:?}"
        );
        if a_owns && hold_left >= Duration::from_millis(30) {
            assert!(
                recorded >= Duration::from_millis(20),
                "b waited for a's pause ({waited:?}) but recorded {recorded:?}"
            );
        }
        releaser.join().unwrap();
        b.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// gcg g3/k (`CRATONVM_STW_NO_BLOCKED_COUNTER`): the same race with the
    /// anonymous counter off, in both entry orders. The census decides
    /// `expected` from identities alone, so nothing may change: zero
    /// violations, no hang, and the counter never moves.
    #[test]
    fn gcg_k3_no_blocked_counter_keeps_the_census_exact_under_a_race() {
        assert_eq!(
            race_blocked_entry_against_census_counted(false, false, 1500),
            0
        );
        assert_eq!(
            race_blocked_entry_against_census_counted(true, false, 1500),
            0
        );
    }

    /// gcg g3/k: every blocked-region transition pair leaves the counter
    /// where it found it, and with the counter off never moves it (the
    /// guard form, the non-guard form, `leave_after`, and the native bracket).
    #[test]
    fn gcg_k3_every_blocked_transition_pair_is_balanced_with_the_counter_on_or_off() {
        for counted in [true, false] {
            let b = GcBarrier::with_anon_blocked_counted(counted);
            let inside = u64::from(counted);
            let g = b.enter_blocked();
            assert_eq!(b.blocked_count(), inside);
            drop(g);
            assert_eq!(b.blocked_count(), 0);
            assert!(!b.mark_blocked_region_enter());
            assert_eq!(b.blocked_count(), inside);
            b.mark_blocked_region_leave();
            assert_eq!(b.blocked_count(), 0);
            assert!(!b.mark_blocked_region_enter());
            b.mark_blocked_region_leave_after(|| {});
            assert_eq!(b.blocked_count(), 0);
            assert!(!b.mark_in_native_enter());
            assert_eq!(b.blocked_count(), inside);
            let flag = AtomicBool::new(true);
            assert!(b.leave_in_native_if(ThreadId(7), &flag, || true));
            assert!(!flag.load(Ordering::Acquire));
            assert_eq!(b.blocked_count(), 0, "counted={counted}");
        }
    }

    /// gcg g4/xt: the helper-window gate. With the anonymous counter it
    /// follows the count; without it (`CRATONVM_STW_NO_BLOCKED_COUNTER`) it
    /// can never answer "nobody is blocked", because `blocked_count` is 0
    /// there whoever is blocked -- the gate that skipped the pass.
    #[test]
    fn gcg_g4xt_blocked_gate_never_reads_an_unmaintained_counter() {
        let b = GcBarrier::with_anon_blocked_counted(true);
        assert!(!b.may_have_blocked_threads());
        assert!(!b.mark_blocked_region_enter());
        assert!(b.may_have_blocked_threads());
        b.mark_blocked_region_leave();
        assert!(!b.may_have_blocked_threads());

        let b = GcBarrier::with_anon_blocked_counted(false);
        assert!(b.may_have_blocked_threads());
        assert!(!b.mark_blocked_region_enter());
        // Positive control: the old gate's input is blind here.
        assert_eq!(b.blocked_count(), 0);
        assert!(b.may_have_blocked_threads());
        b.mark_blocked_region_leave();
        assert!(b.may_have_blocked_threads());
    }

    /// gcg g2/k (`common-w21e-proposal-coverage-slot-without-the-wait`): a
    /// request of a barrier with `CRATONVM_STW_COVERAGE_SLOT_NO_WAIT` does not
    /// wait for another VM's pause holding the coverage slot; it goes ahead
    /// without it and adopts it at its next barrier wait once free.
    #[test]
    fn gcg_k2_a_no_wait_request_does_not_wait_for_another_vms_pause() {
        let _slot = coverage_slot_tests_serialised();
        let a = GcBarrier::new();
        assert!(a.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
        let a_owns = a.inner.lock().holds_coverage_slot;
        let b = GcBarrier::with_coverage_slot_no_wait(true);
        let t0 = Instant::now();
        assert!(b.request_stw_opening_cycle(ThreadId(2), || census(1, vec![], Some(true)), || {}));
        assert!(
            t0.elapsed() < COVERAGE_SLOT_WAIT_LIMIT / 2,
            "a no-wait request must not wait out another VM's pause"
        );
        if a_owns {
            assert!(!b.inner.lock().holds_coverage_slot, "A still owns the slot");
        }
        a.complete_gc(cratonvm_types::PointerMap::default());
        assert!(b.wait_for_all_timeout(Duration::from_millis(1)));
        if b.inner.lock().holds_coverage_slot {
            assert_eq!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), b.slot_id);
        }
        b.complete_gc(cratonvm_types::PointerMap::default());
        assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), b.slot_id);
    }

    /// Interpreter round i2 wave 64, lane L6
    /// (`CRATONVM_DEBUG_STW_UNPARKS_PARKED`): a thread enrolled with
    /// `enroll_stw_waker` and parked on its own parker with a long bound is
    /// woken by a winning request at once, not at its bound; the guard's drop
    /// withdraws the enrollment, and nested enrollments of one thread count
    /// once each.
    #[test]
    fn w64_l6_a_stop_the_world_request_unparks_an_enrolled_waker() {
        let _slot = coverage_slot_tests_serialised();
        let barrier = Arc::new(GcBarrier::new());
        assert_eq!(barrier.stw_wakers_enrolled(), 0);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let parked = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let outer = barrier.enroll_stw_waker();
                let inner = barrier.enroll_stw_waker();
                ready_tx.send(()).unwrap();
                let started = Instant::now();
                let mut parks = 0u32;
                // The park loop's shape: flag first, then a park bounded far
                // beyond the test's patience.
                while !barrier.stw_requested.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_secs(60));
                    parks += 1;
                    assert!(
                        started.elapsed() < Duration::from_secs(30),
                        "never unparked"
                    );
                }
                drop(inner);
                assert_eq!(barrier.stw_wakers_enrolled(), 1, "the outer level stays");
                drop(outer);
                parks
            })
        };
        ready_rx.recv().unwrap();
        assert_eq!(barrier.stw_wakers_enrolled(), 2, "one entry per enrollment");
        let t0 = Instant::now();
        assert!(barrier.request_stw_opening_cycle(
            ThreadId(1),
            || census(1, vec![], Some(true)),
            || {}
        ));
        let parks = parked.join().unwrap();
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "woken by the request"
        );
        assert!(parks <= 2, "no poll: {parks} parks");
        assert_eq!(barrier.stw_wakers_enrolled(), 0);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// Wave 64, lane L6: a barrier no thread ever enrolled with skips the
    /// wakers' lock (`stw_wakers_used` stays false through a pause).
    #[test]
    fn w64_l6_a_request_with_no_enrolled_waker_touches_no_list() {
        let _slot = coverage_slot_tests_serialised();
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw_opening_cycle(
            ThreadId(1),
            || census(1, vec![], Some(true)),
            || {}
        ));
        assert!(!barrier.stw_wakers_used.load(Ordering::Acquire));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        drop(barrier.enroll_stw_waker());
        assert!(barrier.stw_wakers_used.load(Ordering::Acquire));
        assert_eq!(barrier.stw_wakers_enrolled(), 0);
    }

    // ── gci i6/jr: the resume census and `CRATONVM_STW_RESUME_FAIRNESS` ──

    /// The rule, pure: an unresumed waiter always holds; otherwise only inside
    /// the grant of the last resume, and never for the thread that resumed
    /// last itself.
    #[test]
    fn gci_i6jr_the_fairness_rule_holds_for_an_unresumed_waiter_or_inside_the_grant() {
        assert!(resume_fairness_should_hold(1, 10, 0, 250, false));
        assert!(resume_fairness_should_hold(1, 10, 5, 250, true));
        assert!(!resume_fairness_should_hold(0, 10, 0, 250, false), "nobody resumed yet");
        assert!(resume_fairness_should_hold(0, 100, 1, 250, false), "inside the grant");
        assert!(!resume_fairness_should_hold(0, 251, 1, 250, false), "the grant is over");
        assert!(!resume_fairness_should_hold(0, 100, 1, 250, true), "the requester had it");
    }

    /// A counted waiter released by `complete_gc` is counted once in the
    /// census, under its own id, and leaves `waiters_in_wait` at zero; with
    /// the switch off nothing ever holds.
    #[test]
    fn gci_i6jr_a_released_waiter_is_counted_with_its_resume_latency() {
        let barrier = Arc::new(GcBarrier::with_resume_fairness(false));
        assert!(barrier.request_stw(ThreadId(0), 2));
        let b = Arc::clone(&barrier);
        let h = std::thread::spawn(move || b.arrive_and_wait(ThreadId(5)));
        assert!(quota_met_within(&barrier, Duration::from_secs(20)));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = h.join().unwrap();
        let c = barrier.resume_census();
        assert_eq!(c[RC_RESUMES], 1);
        assert_eq!(decode_straggler(c[RC_RESUME_MAX_TID]), Some(5));
        assert_eq!(c[RC_STRADDLES], 0);
        assert_eq!(c[RC_HOLDS], 0);
        assert_eq!(barrier.waiters_in_wait.load(Ordering::Acquire), 0);
        let line = barrier.resume_census_line();
        assert!(line.starts_with("[GC] stw_resume: resume_fairness=off resumes=1 "), "{line}");
        assert!(line.contains(" resume_max_tid=5 "), "{line}");
        assert!(line.contains(" holds=0 hold_avg_us=0 hold_max_us=0 hold_timeouts=0 "), "{line}");
        // gci i18/fh: the in-native rule's slots, then gci i47w/ex47's, close the line.
        let tail = " in_native_holds=0 in_native_skips=0 hold_yielded=0 back_to_back_holds=0";
        assert!(line.ends_with(tail), "{line}");
    }

    /// A request that finds a released waiter not yet back is a starved
    /// request in the census; with the switch off it is not held.
    #[test]
    fn gci_i6jr_a_starved_request_is_counted_and_not_held_with_the_switch_off() {
        let barrier = GcBarrier::with_resume_fairness(false);
        // A waiter released by the previous pause that has not re-taken the lock.
        barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);
        assert!(barrier.request_stw(ThreadId(0), 1));
        let c = barrier.resume_census();
        assert_eq!(c[RC_STARVED_REQUESTS], 1);
        assert_eq!(c[RC_HOLDS], 0);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
    }

    /// With the switch on, a waiter that never comes back holds the request
    /// for the bound and no longer, and the pause still goes ahead.
    #[test]
    fn gci_i6jr_the_hold_is_bounded_for_a_waiter_that_never_resumes() {
        let barrier = GcBarrier::with_resume_fairness(true);
        barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);
        let t0 = Instant::now();
        assert!(barrier.request_stw(ThreadId(0), 1));
        let took = t0.elapsed();
        assert!(took >= RESUME_FAIRNESS_MAX_HOLD, "held for the bound: {took:?}");
        assert!(took < Duration::from_secs(20), "and not forever: {took:?}");
        let c = barrier.resume_census();
        assert_eq!((c[RC_STARVED_REQUESTS], c[RC_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 1, 1));
        assert!(barrier.stw_requested.load(Ordering::Acquire), "the pause went ahead");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
        assert!(barrier.resume_census_line().contains("resume_fairness=on "));
    }

    /// gcj j3/rt3 (residuals page item 7): an allocation request whose
    /// fairness hold saw a sibling's whole pause complete (the generation
    /// moved while `inner` was released) loses under the sibling rule, and
    /// wins without it (the old back-to-back duplicate request). The sibling's
    /// pauses are simulated by a thread bumping the generation for as long as
    /// the request runs; a starved host may let a hold pass without a bump,
    /// so the rule gets a few tries.
    #[test]
    fn gcj3rt3_a_hold_that_saw_a_sibling_pause_complete_loses_under_the_rule() {
        let _slot = coverage_slot_tests_serialised();
        assert!(!GcBarrier::new().hold_sees_sibling_pause(), "opt-in");
        let barrier = Arc::new(GcBarrier::with_resume_fairness(true));
        barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);
        let run = Arc::new(AtomicBool::new(true));
        let (b, r) = (Arc::clone(&barrier), Arc::clone(&run));
        let bumper = std::thread::spawn(move || {
            while r.load(Ordering::Acquire) {
                b.gc_generation.fetch_add(1, Ordering::Release);
                std::thread::yield_now();
            }
        });
        let mut lost = false;
        for _ in 0..20 {
            let won = barrier.request_stw_opening_cycle_for_allocation(
                ThreadId(0),
                true,
                || census(1, Vec::new(), Some(true)),
                || {},
            );
            if !won {
                lost = true;
                break;
            }
            barrier.complete_gc(cratonvm_types::PointerMap::default());
        }
        assert!(lost, "a sibling pause inside the hold makes the request lose");
        assert!(barrier.hold_sibling_pause_losses() >= 1);
        assert!(!barrier.stw_requested.load(Ordering::Acquire), "nothing requested");
        // Without the rule the same hold goes ahead.
        let won = barrier.request_stw_opening_cycle_for_allocation(
            ThreadId(0),
            false,
            || census(1, Vec::new(), Some(true)),
            || {},
        );
        assert!(won, "the old answer: a duplicate request");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        run.store(false, Ordering::Release);
        bumper.join().unwrap();
        barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
    }

    /// With the switch on, a request made right after `complete_gc` lets the
    /// released waiter leave its wait loop before the next flag goes up
    /// (unless the bound ran out on a starved host, which is then counted).
    #[test]
    fn gci_i6jr_a_released_waiter_resumes_before_the_next_pause_with_the_switch_on() {
        let barrier = Arc::new(GcBarrier::with_resume_fairness(true));
        assert!(barrier.request_stw(ThreadId(0), 2));
        let b = Arc::clone(&barrier);
        let h = std::thread::spawn(move || b.arrive_and_wait(ThreadId(6)));
        assert!(quota_met_within(&barrier, Duration::from_secs(20)));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        // The very next request, from the initiator's thread.
        assert!(barrier.request_stw(ThreadId(0), 2));
        let c = barrier.resume_census();
        assert!(
            barrier.waiters_in_wait.load(Ordering::Acquire) == 0 || c[RC_HOLD_TIMEOUTS] == 1,
            "the waiter was back before the flag, or the bound was counted: {c:?}"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = h.join().unwrap();
        assert_eq!(barrier.resume_census()[RC_RESUMES], 1);
        assert_eq!(barrier.waiters_in_wait.load(Ordering::Acquire), 0);
    }

    /// A blocked-region leave that waits out two pauses in a row counts one
    /// `leave_repeats`, and each wait is a resume.
    #[test]
    fn gci_i6jr_a_blocked_leave_across_two_pauses_counts_a_repeat() {
        let barrier = Arc::new(GcBarrier::with_resume_fairness(false));
        let flag = Arc::new(AtomicBool::new(true));
        // The leaver is excluded from both pauses (identity census: blocked).
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![9])));
        let (b, f) = (Arc::clone(&barrier), Arc::clone(&flag));
        let h = std::thread::spawn(move || b.leave_blocked_region_flagged(ThreadId(9), &f));
        // Wait until the leaver is parked in the first pause.
        let deadline = Instant::now() + Duration::from_secs(20);
        while barrier.waiters_in_wait.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "the leaver never parked");
            std::thread::yield_now();
        }
        // Complete the first pause and request the second under the lock, so
        // the leaver cannot get between them.
        {
            let mut inner = barrier.inner.lock();
            // `complete_gc`'s generation step, without its lock (we hold it).
            inner.initiator = None;
            inner.initiator_os = None;
            barrier.gc_generation.fetch_add(1, Ordering::Release);
            barrier.lower_stw_requested();
            barrier.released_at_ns.store(barrier.resume_stamp_now(), Ordering::Release);
            barrier.gc_complete.notify_all();
            // The second pause, still excluding the leaver.
            let excluded = vec![9u64];
            assert!(barrier.request_stw_counted_locked(
                &mut inner,
                ThreadId(0),
                2,
                Some(1),
                Some(excluded),
                None,
                None::<fn()>,
            ));
        }
        // The leaver wakes into the second pause and waits it out too.
        let deadline = Instant::now() + Duration::from_secs(20);
        while barrier.resume_census()[RC_LEAVE_REPEATS] == 0 {
            assert!(Instant::now() < deadline, "the second wait was never counted");
            std::thread::yield_now();
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        h.join().unwrap();
        assert!(!flag.load(Ordering::Acquire), "the leave finished");
        let c = barrier.resume_census();
        assert_eq!(c[RC_LEAVE_REPEATS], 1);
        assert_eq!(c[RC_RESUMES], 2);
        assert_eq!(decode_straggler(c[RC_RESUME_MAX_TID]), Some(9));
    }

    // ── gci i15/hx: `CRATONVM_STW_IN_NATIVE_LEAVE_FAIRNESS` ──

    /// With no in-native leave ever made, the rule is inert: a request is not
    /// held, whatever the switch says.
    #[test]
    fn gci_i15hx_the_in_native_leave_rule_is_inert_without_a_leaver() {
        let barrier = GcBarrier::with_in_native_leave_fairness(true);
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert_eq!(barrier.resume_census()[RC_HOLDS], 0);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// An in-native leaver released by the previous pause that has not left
    /// yet holds the next request -- for the bound and no longer when it never
    /// comes back -- and the pause still goes ahead; with the switch killed
    /// (`=0`) it is not held.
    #[test]
    fn gci_i15hx_an_unfinished_in_native_leave_holds_the_next_request_bounded() {
        let barrier = GcBarrier::with_in_native_leave_fairness(true);
        barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
        let t0 = Instant::now();
        assert!(barrier.request_stw(ThreadId(0), 1));
        let took = t0.elapsed();
        assert!(took >= RESUME_FAIRNESS_MAX_HOLD, "held for the bound: {took:?}");
        assert!(took < Duration::from_secs(20), "and not forever: {took:?}");
        let c = barrier.resume_census();
        assert_eq!((c[RC_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 1));
        assert!(barrier.stw_requested.load(Ordering::Acquire), "the pause went ahead");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);

        let killed = GcBarrier::with_in_native_leave_fairness(false);
        killed.in_native_leavers.fetch_add(1, Ordering::AcqRel);
        assert!(killed.request_stw(ThreadId(0), 1));
        assert_eq!(killed.resume_census()[RC_HOLDS], 0, "=0 restores the old request");
        killed.complete_gc(cratonvm_types::PointerMap::default());
        killed.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
    }

    /// The grant is never held against the thread that finished the leave
    /// itself (it has had its window), and the rule, pure, holds only inside
    /// the grant of another thread's leave.
    #[test]
    fn gci_i15hx_the_grant_belongs_to_the_leaver_not_to_the_requester() {
        let barrier = GcBarrier::with_in_native_leave_fairness(true);
        let me = current_thread_token().expect("a thread token");
        barrier
            .last_in_native_leave_ns
            .store(barrier.resume_stamp_now(), Ordering::Release);
        barrier.last_in_native_leave_token.store(me, Ordering::Release);
        assert!(!barrier.in_native_leave_should_hold(me), "the leaver itself requests");
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert_eq!(barrier.resume_census()[RC_HOLDS], 0);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        // Another thread finished a leave "now": a third thread's request is
        // inside its grant (the stamp is in the future, so no scheduling delay
        // can end the grant before the question is asked).
        barrier
            .last_in_native_leave_ns
            .store(barrier.resume_stamp_now() + 60_000_000_000, Ordering::Release);
        assert!(barrier.in_native_leave_should_hold(me.wrapping_add(1)));
        // A grant that ended long ago holds nothing.
        barrier.last_in_native_leave_ns.store(1, Ordering::Release);
        std::thread::sleep(IN_NATIVE_LEAVE_GRANT * 2);
        assert!(!barrier.in_native_leave_should_hold(me.wrapping_add(1)));
    }

    /// A real in-native leave that waits out a pause is counted as a leaver
    /// while it waits, and on its way out stamps its grant on its own thread
    /// and leaves the count at zero (whichever way the flag went).
    #[test]
    fn gci_i15hx_an_in_native_leave_across_a_pause_is_counted_then_stamped() {
        use std::sync::atomic::AtomicBool;
        let barrier = Arc::new(GcBarrier::with_in_native_leave_fairness(true));
        let flag = Arc::new(AtomicBool::new(true));
        assert!(!barrier.mark_in_native_enter());
        // The pause excludes thread 7 (identity census: its flag is up).
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![7])));
        let leaver = {
            let barrier = Arc::clone(&barrier);
            let flag = Arc::clone(&flag);
            std::thread::spawn(move || {
                let cleared = barrier.leave_in_native_if(ThreadId(7), &flag, || true);
                (cleared, current_thread_token().expect("a thread token"))
            })
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while barrier.in_native_leavers.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "the leaver never waited");
            std::thread::yield_now();
        }
        assert_eq!(barrier.last_in_native_leave_ns.load(Ordering::Acquire), 0);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let (cleared, leaver_token) = leaver.join().expect("the leaver does not panic");
        assert!(cleared);
        assert!(!flag.load(Ordering::Acquire));
        assert_eq!(barrier.in_native_leavers.load(Ordering::Acquire), 0);
        assert_ne!(barrier.last_in_native_leave_ns.load(Ordering::Acquire), 0);
        assert_eq!(
            barrier.last_in_native_leave_token.load(Ordering::Acquire),
            leaver_token,
            "the grant is the leaver's"
        );
        assert_eq!(barrier.blocked_count(), 0, "released exactly once");
    }

    /// A leave with no pause requested takes the one-hold path and touches
    /// none of the rule's state (the package's common case stays as cheap).
    #[test]
    fn gci_i15hx_an_uncontended_in_native_leave_is_not_a_leaver() {
        use std::sync::atomic::AtomicBool;
        let barrier = GcBarrier::with_in_native_leave_fairness(true);
        let flag = AtomicBool::new(true);
        assert!(!barrier.mark_in_native_enter());
        assert!(barrier.leave_in_native_if(ThreadId(7), &flag, || true));
        assert_eq!(barrier.in_native_leavers.load(Ordering::Acquire), 0);
        assert_eq!(barrier.last_in_native_leave_ns.load(Ordering::Acquire), 0);
    }

    // ── gci i15/rb: review of the i15/hx rule ──

    /// gci i15/rb (finding 2 of
    /// `gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`): an
    /// in-native leave that unwinds out of its waiting path (here its
    /// `clear_now` panics, as the stress tripwire can) still ends its leaver
    /// count and stamps its grant, so a later request is not held for the
    /// bound by a leaver that is gone.
    #[test]
    fn gci_i15rb_an_unwinding_in_native_leave_still_ends_its_count() {
        use std::sync::atomic::AtomicBool;
        let barrier = Arc::new(GcBarrier::with_in_native_leave_fairness(true));
        let flag = Arc::new(AtomicBool::new(true));
        assert!(!barrier.mark_in_native_enter());
        // The pause excludes thread 7 (identity census: its flag is up).
        assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![7])));
        let leaver = {
            let barrier = Arc::clone(&barrier);
            let flag = Arc::clone(&flag);
            std::thread::spawn(move || {
                barrier.leave_in_native_if(ThreadId(7), &flag, || -> bool {
                    panic!("gci_i15rb: clear_now unwinds")
                })
            })
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while barrier.in_native_leavers.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "the leaver never waited");
            std::thread::yield_now();
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(leaver.join().is_err(), "the closure's panic reached the caller");
        assert_eq!(barrier.in_native_leavers.load(Ordering::Acquire), 0, "not leaked");
        assert_ne!(barrier.last_in_native_leave_ns.load(Ordering::Acquire), 0, "stamped");
        // Past the grant, a request is not held at all.
        std::thread::sleep(IN_NATIVE_LEAVE_GRANT * 4);
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert_eq!(barrier.resume_census()[RC_HOLDS], 0, "no leaked leaver holds it");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    // ── gci i17/jh: `gci-i15orch-jni-repeat-probe-full-hangs-silently-on-the-default-path-FIXED-20261017` ──

    /// gci i17/jh: the repeat probe's two threads at the barrier, with the
    /// default in-native leave rule. A JNI loop thread goes into native and
    /// back ROUNDS times (`jni::enter_in_native_region`, then
    /// `jni::native_call_leave_native`: the quiet leave when no pause
    /// completed since the entry, else the full wake's flagged leave) and
    /// polls while it runs; a churn thread requests pause after pause with no
    /// gap, its census excluding the loop thread exactly while its flag is up
    /// (the identity census, read under `inner` as the locked request reads
    /// it). The loop must finish every round: no lost wakeup, no wait that
    /// outlives its pause, no leave starved for good. A regression fails on
    /// the deadline instead of hanging the test binary.
    #[test]
    fn gci_i17jh_an_in_native_loop_progresses_under_back_to_back_pauses() {
        let _slot = coverage_slot_tests_serialised();
        const ROUNDS: u64 = 500;
        let mut b = GcBarrier::with_in_native_leave_fairness(true);
        // The census below reads the flag under `inner` BEFORE the request
        // raises `stw_requested`: the locked entry's order (the lock-free
        // entry raises first and needs `request_flag_then_census_locked`).
        b.lockfree_blocked = false;
        let barrier = Arc::new(b);
        let flag = Arc::new(AtomicBool::new(false));
        let pauses = Arc::new(AtomicU64::new(0));
        let rounds = Arc::new(AtomicU64::new(0));
        let loop_parked = Arc::new(AtomicBool::new(false));
        let churn_stopped = Arc::new(AtomicBool::new(false));

        let churn = {
            let barrier = Arc::clone(&barrier);
            let flag = Arc::clone(&flag);
            let pauses = Arc::clone(&pauses);
            let loop_parked = Arc::clone(&loop_parked);
            let churn_stopped = Arc::clone(&churn_stopped);
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !loop_parked.load(Ordering::Acquire) {
                    {
                        let mut inner = barrier.inner.lock();
                        // `request_stw_opening_cycle`'s hold, then its census.
                        if !barrier.stw_requested.load(Ordering::Acquire) {
                            let _ = barrier.resume_fairness_hold(&mut inner);
                        }
                        let (blocked, tids) = if flag.load(Ordering::Acquire) {
                            (1, vec![7u64])
                        } else {
                            (0, Vec::new())
                        };
                        assert!(barrier.request_stw_counted_locked(
                            &mut inner,
                            ThreadId(0),
                            2,
                            Some(blocked),
                            Some(tids),
                            None,
                            None::<fn()>,
                        ));
                    }
                    let until = Instant::now() + Duration::from_secs(30);
                    while !barrier.wait_for_all_timeout(Duration::from_millis(50)) {
                        assert!(Instant::now() < until, "pause {n}: a counted loop thread never arrived");
                    }
                    barrier.complete_gc(cratonvm_types::PointerMap::default());
                    n += 1;
                    pauses.store(n, Ordering::Release);
                }
                churn_stopped.store(true, Ordering::Release);
                n
            })
        };

        let looper = {
            let barrier = Arc::clone(&barrier);
            let flag = Arc::clone(&flag);
            let pauses = Arc::clone(&pauses);
            let rounds = Arc::clone(&rounds);
            let loop_parked = Arc::clone(&loop_parked);
            let churn_stopped = Arc::clone(&churn_stopped);
            std::thread::spawn(move || {
                let tid = ThreadId(7);
                let started = Instant::now() + Duration::from_secs(60);
                while pauses.load(Ordering::Acquire) == 0 {
                    assert!(Instant::now() < started, "the churn thread never paused");
                    // gci i18/fh (i18a red): running Java with the flag down,
                    // so the first pause's census COUNTS this thread -- it must
                    // poll here as in its loop, or that pause waits for it
                    // forever (the deadlock this wait used to be).
                    if barrier.stop_requested() {
                        let _ = barrier.arrive_and_wait_auto(tid);
                    }
                    std::thread::yield_now();
                }
                for _ in 0..ROUNDS {
                    // Running -> in native: the flag, then the counted entry,
                    // arriving when a pause raced it.
                    flag.store(true, Ordering::Release);
                    if barrier.mark_in_native_enter() {
                        let _ = barrier.arrive_and_wait_auto(tid);
                    }
                    let entered = barrier.gc_generation.load(Ordering::Acquire);
                    // In native -> running.
                    let quiet = barrier.leave_in_native_if(tid, &flag, || {
                        barrier.gc_generation.load(Ordering::Acquire) == entered
                    });
                    if !quiet {
                        barrier.leave_blocked_region_flagged(tid, &flag);
                    }
                    assert!(!flag.load(Ordering::Acquire), "the leave cleared the flag");
                    // Running Java: a poll.
                    if barrier.stop_requested() {
                        let _ = barrier.arrive_and_wait_auto(tid);
                    }
                    rounds.fetch_add(1, Ordering::AcqRel);
                }
                // In native for good, so no later census counts this thread.
                flag.store(true, Ordering::Release);
                if barrier.mark_in_native_enter() {
                    let _ = barrier.arrive_and_wait_auto(tid);
                }
                loop_parked.store(true, Ordering::Release);
                while !churn_stopped.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                if !barrier.leave_in_native_if(tid, &flag, || true) {
                    barrier.leave_blocked_region_flagged(tid, &flag);
                }
            })
        };

        let deadline = Instant::now() + Duration::from_secs(120);
        while !loop_parked.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "the loop thread stalled at round {} of {ROUNDS} after {} pauses",
                rounds.load(Ordering::Acquire),
                pauses.load(Ordering::Acquire)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let n = churn.join().expect("the churn thread does not panic");
        looper.join().expect("the loop thread does not panic");
        assert_eq!(rounds.load(Ordering::Acquire), ROUNDS);
        assert!(n >= 1, "the pauses really interleaved");
        assert!(!flag.load(Ordering::Acquire), "the loop thread left native");
        assert!(!barrier.stop_requested());
        assert_eq!(barrier.in_native_leavers.load(Ordering::Acquire), 0);
        assert_eq!(barrier.blocked_count(), 0, "every entry released exactly once");
    }

    // ── gci i18/fh: `CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY` ──

    /// gci i18/fh: stamp an in-native leave done by ANOTHER thread `age_ns`
    /// ago on `barrier` (whose epoch must be older than that).
    fn gci_i18fh_stamp_leave_done_ago(barrier: &GcBarrier, age_ns: u64) {
        let me = current_thread_token().expect("a thread token");
        let now = barrier.resume_stamp_now();
        assert!(now > age_ns + 1, "the barrier is older than the stamp");
        barrier.last_in_native_leave_token.store(me.wrapping_add(1), Ordering::Release);
        barrier.last_in_native_leave_ns.store(now - age_ns, Ordering::Release);
    }

    /// The trigger, by the barrier's own predicates: a leaver not out yet, or
    /// one out "now" (a stamp in the future: no scheduling delay can age it),
    /// races a request; one out half a grant ago does not race it although it
    /// is still inside its grant; the requester's own leave never races it.
    /// With the switch off the trigger is i15/hx's: the whole grant.
    #[test]
    fn gci_i18fh_only_a_request_that_races_the_leaver_starts_a_hold() {
        let barrier = GcBarrier::with_in_native_leave_race_only(true);
        let me = current_thread_token().expect("a thread token");
        assert!(!barrier.in_native_leave_raced(me), "no leave ever made");
        barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
        assert!(barrier.in_native_leave_raced(me), "a leaver not out yet");
        barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
        barrier.last_in_native_leave_token.store(me.wrapping_add(1), Ordering::Release);
        barrier
            .last_in_native_leave_ns
            .store(barrier.resume_stamp_now() + 60_000_000_000, Ordering::Release);
        assert!(barrier.in_native_leave_raced(me), "a leaver out just now");
        barrier.last_in_native_leave_token.store(me, Ordering::Release);
        assert!(!barrier.in_native_leave_raced(me), "the requester's own leave");

        // Half a grant ago: past the race window (raced is monotone in time,
        // so this half is exact), still inside the grant (retried, since a
        // loaded host can age the stamp past the grant between two lines).
        let half = duration_ns(IN_NATIVE_LEAVE_GRANT) / 2;
        assert!(half >= duration_ns(IN_NATIVE_LEAVE_RACE_WINDOW), "the window is inside the grant");
        let off = GcBarrier::with_in_native_leave_race_only(false);
        std::thread::sleep(IN_NATIVE_LEAVE_GRANT * 4);
        let mut seen_in_grant = false;
        for _ in 0..200 {
            gci_i18fh_stamp_leave_done_ago(&barrier, half);
            gci_i18fh_stamp_leave_done_ago(&off, half);
            let on_raced = barrier.in_native_leave_raced(me);
            let on_grant = barrier.in_native_leave_should_hold(me);
            let off_raced = off.in_native_leave_raced(me);
            assert!(!on_raced, "half a grant ago does not race");
            if on_grant && off_raced {
                seen_in_grant = true;
                break;
            }
        }
        assert!(seen_in_grant, "never inside the grant in 200 tries");
    }

    /// A request late in an in-native leaver's grant is not held with the
    /// switch on (counted `in_native_skips`), and is held with it off (=0
    /// restores i15/hx).
    #[test]
    fn gci_i18fh_a_request_late_in_the_grant_is_held_only_with_the_switch_off() {
        let half = duration_ns(IN_NATIVE_LEAVE_GRANT) / 2;
        let on = GcBarrier::with_in_native_leave_race_only(true);
        let off = GcBarrier::with_in_native_leave_race_only(false);
        std::thread::sleep(IN_NATIVE_LEAVE_GRANT * 4);
        let mut skipped = false;
        for _ in 0..200 {
            gci_i18fh_stamp_leave_done_ago(&on, half);
            assert!(on.request_stw(ThreadId(0), 1));
            on.complete_gc(cratonvm_types::PointerMap::default());
            let c = on.resume_census();
            assert_eq!(c[RC_HOLDS], 0, "a request that did not race is never held");
            if c[RC_IN_NATIVE_SKIPS] > 0 {
                skipped = true;
                break;
            }
        }
        assert!(skipped, "never inside the grant in 200 tries");
        assert!(on.resume_census_line().contains(" in_native_skips=1 "), "{}", on.resume_census_line());

        let mut held = false;
        for _ in 0..200 {
            gci_i18fh_stamp_leave_done_ago(&off, half);
            assert!(off.request_stw(ThreadId(0), 1));
            off.complete_gc(cratonvm_types::PointerMap::default());
            let c = off.resume_census();
            assert_eq!(c[RC_IN_NATIVE_SKIPS], 0, "=0 never skips");
            if c[RC_HOLDS] > 0 {
                assert_eq!(c[RC_IN_NATIVE_HOLDS], c[RC_HOLDS]);
                held = true;
                break;
            }
        }
        assert!(held, "=0: never held inside the grant in 200 tries");
    }

    /// A request that raced a leaver not out yet is held as i15/hx held it
    /// (here to the bound: the leaver never comes out) and is counted an
    /// in-native hold.
    #[test]
    fn gci_i18fh_a_racing_request_is_still_held() {
        let barrier = GcBarrier::with_in_native_leave_race_only(true);
        barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
        let t0 = Instant::now();
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert!(t0.elapsed() >= RESUME_FAIRNESS_MAX_HOLD, "held for the bound");
        let c = barrier.resume_census();
        assert_eq!((c[RC_HOLDS], c[RC_IN_NATIVE_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 1, 1));
        assert_eq!(c[RC_HOLD_YIELDED], 0);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
    }

    /// A hold ends as soon as another thread's pause is requested (the holder
    /// is a counted mutator that does not poll while it holds), and the
    /// holding request then loses. Retried: an attempt where the other
    /// request came before the hold (no hold) or after the bound (a timeout)
    /// decides nothing.
    #[test]
    fn gci_i18fh_a_hold_yields_to_another_threads_pause() {
        let mut yielded = false;
        for _ in 0..50 {
            let barrier = Arc::new(GcBarrier::with_in_native_leave_race_only(true));
            // A leaver that never comes out: the hold would run to the bound.
            barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
            let start = Instant::now();
            let other = {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    // A spin, not a sleep: a Windows sleep rounds to its tick.
                    while start.elapsed() < Duration::from_micros(300) {
                        std::hint::spin_loop();
                    }
                    let mut inner = barrier.inner.lock();
                    barrier.request_stw_counted_locked(
                        &mut inner,
                        ThreadId(1),
                        1,
                        Some(0),
                        Some(Vec::new()),
                        None,
                        None::<fn()>,
                    )
                })
            };
            let won = barrier.request_stw(ThreadId(0), 1);
            let other_won = other.join().expect("the other requester does not panic");
            assert!(won != other_won, "exactly one request wins");
            barrier.complete_gc(cratonvm_types::PointerMap::default());
            barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
            let c = barrier.resume_census();
            if c[RC_HOLD_YIELDED] == 1 {
                assert!(other_won, "the holder lost to the pause it yielded to");
                assert_eq!((c[RC_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 0));
                yielded = true;
                break;
            }
        }
        assert!(yielded, "the other request never landed inside a hold in 50 tries");
    }

    /// With the switch off a hold does not yield (i15/hx's loop exactly): a
    /// pause requested meanwhile waits out the bound.
    #[test]
    fn gci_i18fh_with_the_switch_off_a_hold_never_yields() {
        let barrier = Arc::new(GcBarrier::with_in_native_leave_race_only(false));
        barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
        let other = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let start = Instant::now();
                while start.elapsed() < Duration::from_micros(300) {
                    std::hint::spin_loop();
                }
                let mut inner = barrier.inner.lock();
                barrier.request_stw_counted_locked(
                    &mut inner,
                    ThreadId(1),
                    1,
                    Some(0),
                    Some(Vec::new()),
                    None,
                    None::<fn()>,
                )
            })
        };
        let _ = barrier.request_stw(ThreadId(0), 1);
        let _ = other.join().expect("the other requester does not panic");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
        assert_eq!(barrier.resume_census()[RC_HOLD_YIELDED], 0);
    }

    // ── gci i47w/ex47: `CRATONVM_STW_BACK_TO_BACK_FAIRNESS` ──
    // `gci-i47w-oz47-system-exit-hangs-beside-a-system-gc-daemon-FIXED-20261017`.

    /// The trigger, pure: only the thread that ran the previous
    /// `complete_gc`, only inside the window after that release.
    #[test]
    fn gci_i47w_ex47_the_back_to_back_trigger_is_the_last_completer_inside_the_window() {
        assert!(back_to_back_should_start(100, 150, 100, true));
        assert!(!back_to_back_should_start(100, 150, 100, false), "another thread completed last");
        assert!(!back_to_back_should_start(0, 150, 100, true), "no pause ever released");
        assert!(!back_to_back_should_start(100, 200, 100, true), "the window is over");
        assert!(!back_to_back_should_start(u64::MAX, 1, u64::MAX, false));
    }

    /// The initiator's very next request, made while a waiter its pause
    /// released has not re-taken the lock, is held -- to the bound when the
    /// waiter never comes back -- and the pause still goes ahead; killed
    /// (`=0`), the same request is only counted starved.
    #[test]
    fn gci_i47w_ex47_a_back_to_back_request_holds_for_an_unresumed_waiter_bounded() {
        for on in [true, false] {
            let barrier = GcBarrier::with_back_to_back_fairness(on);
            assert!(barrier.request_stw(ThreadId(0), 1));
            // Completed on THIS thread: its next request is back to back.
            barrier.complete_gc(cratonvm_types::PointerMap::default());
            // The release "now" for good (a stamp in the future): no scheduling
            // delay can end the window before the request asks.
            barrier
                .released_at_ns
                .store(barrier.resume_stamp_now() + 60_000_000_000, Ordering::Release);
            // A waiter that pause released and that never re-takes the lock.
            barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);
            let t0 = Instant::now();
            assert!(barrier.request_stw(ThreadId(0), 1));
            let took = t0.elapsed();
            let c = barrier.resume_census();
            assert_eq!(c[RC_STARVED_REQUESTS], 1);
            if on {
                assert!(took >= RESUME_FAIRNESS_MAX_HOLD, "held for the bound: {took:?}");
                assert!(took < Duration::from_secs(20), "and not forever: {took:?}");
                assert_eq!((c[RC_HOLDS], c[RC_BACK_TO_BACK_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 1, 1));
                assert_eq!(c[RC_IN_NATIVE_HOLDS], 0);
                assert!(barrier.resume_census_line().ends_with(" back_to_back_holds=1"));
            } else {
                assert_eq!((c[RC_HOLDS], c[RC_BACK_TO_BACK_HOLDS]), (0, 0), "=0: the old request");
            }
            assert!(barrier.stw_requested.load(Ordering::Acquire), "the pause went ahead");
            barrier.complete_gc(cratonvm_types::PointerMap::default());
            barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Not back to back: a request by a thread that did not run the previous
    /// `complete_gc`, or one made after the window, is not held although a
    /// released waiter is still out (the general rule stays opt-in).
    #[test]
    fn gci_i47w_ex47_a_request_by_another_thread_or_after_the_window_is_not_held() {
        let barrier = GcBarrier::with_back_to_back_fairness(true);
        let me = current_thread_token().expect("a thread token");
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.last_complete_token.load(Ordering::Acquire), me, "the completer");
        barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);

        // Another thread ran the last `complete_gc`.
        barrier.last_complete_token.store(me.wrapping_add(1), Ordering::Release);
        barrier
            .released_at_ns
            .store(barrier.resume_stamp_now() + 60_000_000_000, Ordering::Release);
        assert!(!barrier.back_to_back_raced(me));
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert_eq!(barrier.resume_census()[RC_HOLDS], 0, "not the initiator's request");
        barrier.complete_gc(cratonvm_types::PointerMap::default());

        // This thread completed last, but long ago.
        std::thread::sleep(BACK_TO_BACK_WINDOW * 2);
        barrier.last_complete_token.store(me, Ordering::Release);
        barrier.released_at_ns.store(1, Ordering::Release);
        assert!(!barrier.back_to_back_raced(me));
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert_eq!(barrier.resume_census()[RC_HOLDS], 0, "a request at its own pace");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
        assert_eq!(barrier.resume_census()[RC_STARVED_REQUESTS], 2);
    }

    /// The defect's shape on two plain threads: an initiator requesting pause
    /// after pause with no gap (`System.gc()` in a loop: hold, census, request,
    /// wait, `complete_gc`, again), and a thread that enters and leaves a
    /// blocked region ROUNDS times and polls between them, excluded from every
    /// pause whose census found its flag up. Every leave has to see NO pause
    /// requested, so without the rule it loses the race to each next request
    /// and its progress is unbounded; with it every round finishes. A
    /// regression fails on the deadline instead of hanging the test binary.
    #[test]
    fn gci_i47w_ex47_a_blocked_leave_progresses_under_a_back_to_back_initiator() {
        let _slot = coverage_slot_tests_serialised();
        const ROUNDS: u64 = 200;
        let mut b = GcBarrier::with_back_to_back_fairness(true);
        // The census below reads the flag under `inner` BEFORE the request
        // raises `stw_requested` (the locked entry's order).
        b.lockfree_blocked = false;
        let barrier = Arc::new(b);
        let flag = Arc::new(AtomicBool::new(false));
        let pauses = Arc::new(AtomicU64::new(0));
        let rounds = Arc::new(AtomicU64::new(0));
        let leaver_done = Arc::new(AtomicBool::new(false));
        let churn_stopped = Arc::new(AtomicBool::new(false));
        let (first_request_tx, first_request_rx) = std::sync::mpsc::channel();

        let churn = {
            let barrier = Arc::clone(&barrier);
            let flag = Arc::clone(&flag);
            let pauses = Arc::clone(&pauses);
            let leaver_done = Arc::clone(&leaver_done);
            let churn_stopped = Arc::clone(&churn_stopped);
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !leaver_done.load(Ordering::Acquire) {
                    {
                        let mut inner = barrier.inner.lock();
                        // `request_stw_opening_cycle`'s hold, then its census.
                        if !barrier.stw_requested.load(Ordering::Acquire) {
                            let _ = barrier.resume_fairness_hold(&mut inner);
                        }
                        let (blocked, tids) = if flag.load(Ordering::Acquire) {
                            (1, vec![9u64])
                        } else {
                            (0, Vec::new())
                        };
                        assert!(barrier.request_stw_counted_locked(
                            &mut inner,
                            ThreadId(0),
                            2,
                            Some(blocked),
                            Some(tids),
                            None,
                            None::<fn()>,
                        ));
                    }
                    if n == 0 {
                        first_request_tx.send(()).expect("the leaver is waiting");
                    }
                    let until = Instant::now() + Duration::from_secs(30);
                    while !barrier.wait_for_all_timeout(Duration::from_millis(50)) {
                        assert!(Instant::now() < until, "pause {n}: the leaver never arrived");
                    }
                    barrier.complete_gc(cratonvm_types::PointerMap::default());
                    n += 1;
                    pauses.store(n, Ordering::Release);
                }
                churn_stopped.store(true, Ordering::Release);
                n
            })
        };

        let leaver = {
            let barrier = Arc::clone(&barrier);
            let flag = Arc::clone(&flag);
            let rounds = Arc::clone(&rounds);
            let leaver_done = Arc::clone(&leaver_done);
            let churn_stopped = Arc::clone(&churn_stopped);
            std::thread::spawn(move || {
                // The old fixture could finish all 200 rounds before the
                // initiator was scheduled, failing only its overlap assertion.
                // Start the leaves after the first pause has really opened.
                first_request_rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the initiator opens the first pause");
                let tid = ThreadId(9);
                for _ in 0..ROUNDS {
                    // Running -> blocked (a class-file read, a console write).
                    // A pause whose census missed the store counts this thread;
                    // the flagged leave then arrives for it exactly once.
                    flag.store(true, Ordering::Release);
                    // Blocked -> running: only when no pause is requested.
                    barrier.leave_blocked_region_flagged(tid, &flag);
                    assert!(!flag.load(Ordering::Acquire), "the leave cleared the flag");
                    // Running Java: a poll.
                    if barrier.stop_requested() {
                        let _ = barrier.arrive_and_wait_auto(tid);
                    }
                    rounds.fetch_add(1, Ordering::AcqRel);
                }
                // Blocked for good, answering any pause a census counted it in
                // until the churn stops.
                flag.store(true, Ordering::Release);
                leaver_done.store(true, Ordering::Release);
                while !churn_stopped.load(Ordering::Acquire) {
                    if barrier.stop_requested() {
                        let _ = barrier.arrive_and_wait_auto(tid);
                    }
                    std::thread::yield_now();
                }
                barrier.leave_blocked_region_flagged(tid, &flag);
            })
        };

        let deadline = Instant::now() + Duration::from_secs(120);
        while !leaver_done.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "the leaver stalled at round {} of {ROUNDS} after {} pauses: {}",
                rounds.load(Ordering::Acquire),
                pauses.load(Ordering::Acquire),
                barrier.resume_census_line()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let n = churn.join().expect("the churn thread does not panic");
        leaver.join().expect("the leaver does not panic");
        assert_eq!(rounds.load(Ordering::Acquire), ROUNDS);
        assert!(n >= 1, "the pauses really interleaved");
        assert!(!flag.load(Ordering::Acquire), "the leaver left its region");
        assert!(!barrier.stop_requested());
        assert_eq!(barrier.waiters_in_wait.load(Ordering::Acquire), 0);
        let c = barrier.resume_census();
        assert!(c[RC_BACK_TO_BACK_HOLDS] <= c[RC_HOLDS], "{c:?}");
        assert_eq!(c[RC_IN_NATIVE_HOLDS], 0, "the in-native rule is off here");
    }

    /// gci i20/jt: the census rule `GcBlockState::census_blocked` applies,
    /// on two plain words: `leaving` first, then the flag.
    fn gci_i20jt_census_blocked(leaving: &AtomicBool, flag: &AtomicBool) -> bool {
        !leaving.load(Ordering::Acquire) && flag.load(Ordering::Acquire)
    }

    /// gci i20/jt (`CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE`): with no pause in
    /// progress the lock-free in-native leave never takes the barrier lock --
    /// it completes while another thread HOLDS `inner` -- and ends exactly as
    /// the locked leave does (count released, flag cleared when told to, left
    /// up otherwise, `leaving` down either way).
    #[test]
    fn gci_i20jt_a_quiet_lockfree_leave_takes_no_lock() {
        let barrier = Arc::new(GcBarrier::with_lockfree_blocked(true));
        let flag = Arc::new(AtomicBool::new(true));
        let leaving = Arc::new(AtomicBool::new(false));
        for clear in [true, false] {
            flag.store(true, Ordering::Release);
            assert!(!barrier.mark_in_native_enter(), "no pause in progress");
            assert_eq!(barrier.blocked_count(), 1);
            let held = barrier.inner.lock();
            let leaver = {
                let (barrier, flag, leaving) =
                    (Arc::clone(&barrier), Arc::clone(&flag), Arc::clone(&leaving));
                std::thread::spawn(move || {
                    barrier.leave_in_native_lockfree_if(ThreadId(7), &flag, &leaving, || clear)
                })
            };
            let t0 = Instant::now();
            while !leaver.is_finished() {
                assert!(
                    t0.elapsed() < Duration::from_secs(10),
                    "the quiet leave waited for the barrier lock"
                );
                std::thread::yield_now();
            }
            drop(held);
            assert_eq!(leaver.join().expect("the leaver does not panic"), clear);
            assert_eq!(barrier.blocked_count(), 0, "released exactly once");
            assert_eq!(flag.load(Ordering::Acquire), !clear, "cleared only when told to");
            assert!(!leaving.load(Ordering::Acquire), "leaving is reset");
        }
    }

    /// gci i20/jt: under the census-first request order (the lock-free
    /// blocked transitions off) the Dekker pair has no partner, so the
    /// lock-free leave IS the locked one: it waits for the lock.
    #[test]
    fn gci_i20jt_without_the_flag_first_order_the_leave_takes_the_lock() {
        let barrier = Arc::new(GcBarrier::with_lockfree_blocked(false));
        let flag = Arc::new(AtomicBool::new(true));
        let leaving = Arc::new(AtomicBool::new(false));
        assert!(!barrier.mark_in_native_enter());
        let held = barrier.inner.lock();
        let leaver = {
            let (barrier, flag, leaving) =
                (Arc::clone(&barrier), Arc::clone(&flag), Arc::clone(&leaving));
            std::thread::spawn(move || {
                barrier.leave_in_native_lockfree_if(ThreadId(7), &flag, &leaving, || true)
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!leaver.is_finished(), "the fallback is the locked leave");
        assert!(!leaving.load(Ordering::Acquire), "the fallback never raises leaving");
        drop(held);
        assert!(leaver.join().expect("the leaver does not panic"));
        assert!(!flag.load(Ordering::Acquire));
        assert_eq!(barrier.blocked_count(), 0);
    }

    /// gci i20/jt: a pause whose census EXCLUDED the thread (flag up,
    /// `leaving` down when it read them) holds the lock-free leave until
    /// `complete_gc`, the flag stays up throughout, and the leave then reports
    /// the generation change (`false`: the full wake).
    #[test]
    fn gci_i20jt_an_excluding_pause_holds_the_lockfree_leave_with_the_flag_up() {
        let _slot = coverage_slot_tests_serialised();
        let barrier = Arc::new(GcBarrier::with_lockfree_blocked(true));
        let flag = Arc::new(AtomicBool::new(true));
        let leaving = Arc::new(AtomicBool::new(false));
        assert!(!barrier.mark_in_native_enter());
        let gen0 = barrier.gc_generation.load(Ordering::Acquire);
        let blocked = gci_i20jt_census_blocked(&leaving, &flag);
        assert!(blocked, "the census reads the in-native thread blocked");
        assert!(barrier.request_stw_opening_cycle(
            ThreadId(1),
            || census(2, vec![7], Some(true)),
            || {}
        ));
        let leaver = {
            let (barrier, flag, leaving) =
                (Arc::clone(&barrier), Arc::clone(&flag), Arc::clone(&leaving));
            std::thread::spawn(move || {
                barrier.leave_in_native_lockfree_if(ThreadId(7), &flag, &leaving, || {
                    barrier.gc_generation.load(Ordering::Acquire) == gen0
                })
            })
        };
        assert!(
            barrier.wait_for_all_timeout(Duration::from_secs(5)),
            "an excluded thread is not waited for"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(!leaver.is_finished(), "an excluded thread does not leave during the pause");
        assert!(flag.load(Ordering::Acquire), "its flag stays up for the root scan");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(
            !leaver.join().expect("the leaver does not panic"),
            "a pause completed in the window: the full wake"
        );
        assert!(flag.load(Ordering::Acquire), "left up for the full wake");
        assert!(!leaving.load(Ordering::Acquire));
        assert_eq!(barrier.blocked_count(), 0);
    }

    /// gci i20/jt: a pause whose census COUNTED the thread (it read
    /// `leaving` up) is met by the leave's arrival -- waiting it out instead
    /// would be the pause's missing arrival -- with the flag still up; the
    /// leave then takes the full wake.
    #[test]
    fn gci_i20jt_a_counting_pause_is_met_by_the_lockfree_leaver() {
        let _slot = coverage_slot_tests_serialised();
        let barrier = Arc::new(GcBarrier::with_lockfree_blocked(true));
        let flag = Arc::new(AtomicBool::new(true));
        let leaving = Arc::new(AtomicBool::new(false));
        assert!(!barrier.mark_in_native_enter());
        let gen0 = barrier.gc_generation.load(Ordering::Acquire);
        // The census ran while the leaver's word was up: counted.
        leaving.store(true, Ordering::Release);
        assert!(!gci_i20jt_census_blocked(&leaving, &flag), "leaving is counted");
        assert!(barrier.request_stw_opening_cycle(
            ThreadId(1),
            || census(2, vec![], Some(true)),
            || {}
        ));
        leaving.store(false, Ordering::Release);
        let leaver = {
            let (barrier, flag, leaving) =
                (Arc::clone(&barrier), Arc::clone(&flag), Arc::clone(&leaving));
            std::thread::spawn(move || {
                barrier.leave_in_native_lockfree_if(ThreadId(7), &flag, &leaving, || {
                    barrier.gc_generation.load(Ordering::Acquire) == gen0
                })
            })
        };
        assert!(
            barrier.wait_for_all_timeout(Duration::from_secs(10)),
            "the counted leaver arrives"
        );
        assert!(flag.load(Ordering::Acquire), "it arrived with its flag up");
        assert!(!leaver.is_finished(), "and waits for the pause to complete");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert!(!leaver.join().expect("the leaver does not panic"));
        assert!(flag.load(Ordering::Acquire), "left up for the full wake");
        assert!(!leaving.load(Ordering::Acquire));
        assert_eq!(barrier.blocked_count(), 0);
    }

    /// gci i20/jt: the lock-free leave against back-to-back pauses whose
    /// census reads `leaving` then the flag. Two invariants, each a violation
    /// when broken: (1) a thread a census EXCLUDED keeps its flag up until
    /// that pause completes (its root scan selects blocked threads by the
    /// flag); (2) a thread whose leave was quiet is not excluded by the pause
    /// in progress. No hang, and the anonymous count balances.
    #[test]
    fn gci_i20jt_lockfree_leave_and_census_agree_under_a_race() {
        let _slot = coverage_slot_tests_serialised();
        const MUTATOR: ThreadId = ThreadId(42);
        let b = Arc::new(GcBarrier::with_lockfree_blocked(true));
        let flag = Arc::new(AtomicBool::new(false));
        let leaving = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let violations = Arc::new(AtomicU64::new(0));
        let quiet_leaves = Arc::new(AtomicU64::new(0));
        let mutator = {
            let (b, flag, leaving, stop, violations, quiet_leaves) = (
                Arc::clone(&b),
                Arc::clone(&flag),
                Arc::clone(&leaving),
                Arc::clone(&stop),
                Arc::clone(&violations),
                Arc::clone(&quiet_leaves),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    // A running mutator's safepoint.
                    let _ = b.arrive_and_wait_auto(MUTATOR);
                    // The deposit raises the identity flag, then the entry.
                    flag.store(true, Ordering::Release);
                    if b.mark_in_native_enter() {
                        let _ = b.arrive_and_wait_auto(MUTATOR);
                    }
                    let gen = b.gc_generation.load(Ordering::Acquire);
                    let quiet = b.leave_in_native_lockfree_if(MUTATOR, &flag, &leaving, || {
                        b.gc_generation.load(Ordering::Acquire) == gen
                    });
                    if quiet {
                        quiet_leaves.fetch_add(1, Ordering::Relaxed);
                        let inner = b.inner.lock();
                        if b.stw_requested.load(Ordering::Acquire)
                            && inner.excluded_blocked.contains(&MUTATOR.0)
                        {
                            violations.fetch_add(1, Ordering::Relaxed);
                        }
                    } else {
                        // The full wake.
                        b.leave_blocked_region_flagged(MUTATOR, &flag);
                    }
                }
            })
        };
        for _ in 0..1500u32 {
            let mut excluded = false;
            let won = b.request_stw_opening_cycle(
                ThreadId(1),
                || {
                    excluded = gci_i20jt_census_blocked(&leaving, &flag);
                    let blocked = if excluded { vec![MUTATOR.0] } else { vec![] };
                    census(2, blocked, Some(true))
                },
                || {},
            );
            assert!(won, "the only requester wins");
            let t0 = Instant::now();
            while !b.wait_for_all_timeout(Duration::from_millis(1)) {
                assert!(
                    t0.elapsed() < Duration::from_secs(20),
                    "a counted mutator never arrived"
                );
            }
            for _ in 0..64 {
                if excluded && !flag.load(Ordering::Acquire) {
                    violations.fetch_add(1, Ordering::Relaxed);
                }
                std::hint::spin_loop();
            }
            b.complete_gc(cratonvm_types::PointerMap::default());
        }
        stop.store(true, Ordering::Release);
        mutator.join().expect("mutator thread");
        assert_eq!(violations.load(Ordering::Relaxed), 0);
        assert!(
            quiet_leaves.load(Ordering::Relaxed) > 0,
            "positive control: some leaves were quiet"
        );
        assert_eq!(b.blocked_count(), 0, "every entry was left again");
        assert!(!leaving.load(Ordering::Acquire));
    }
}

/// gci i44/sp: the safepoint accounting behind `VMManagementImpl`'s
/// `getSafepointCount` / `getTotalSafepointTime` / `getSafepointSyncTime` /
/// `getTotalApplicationNonStoppedTime` (`SafepointStats`).
#[cfg(test)]
mod gci_i44_sp_safepoint_stats_tests {
    use super::*;

    /// A fresh barrier has run no safepoint.
    #[test]
    fn gci_i44_sp_a_fresh_barrier_counts_no_safepoint() {
        let barrier = GcBarrier::new();
        assert_eq!(barrier.safepoint_stats(), SafepointStats::default());
    }

    /// Each completed pause counts once, its stopped time covers the whole
    /// request -> `complete_gc` span, the sync time never exceeds it, and the
    /// application time grows by the interval between pauses (and only when
    /// the next pause begins).
    #[test]
    fn gci_i44_sp_each_pause_counts_once_with_its_stopped_and_application_time() {
        let barrier = GcBarrier::new();
        std::thread::sleep(Duration::from_millis(2));
        assert!(barrier.request_stw(ThreadId(0), 1));
        let first_app = barrier.safepoint_stats().application_ns;
        assert!(
            first_app >= 2_000_000,
            "creation -> first request is application time: {first_app}"
        );
        barrier.wait_for_all();
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(barrier.safepoint_stats().count, 0, "counted at completion");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let s = barrier.safepoint_stats();
        assert_eq!(s.count, 1);
        assert!(s.stopped_ns >= 3_000_000, "the pause lasted 3 ms: {s:?}");
        assert!(s.sync_ns <= s.stopped_ns, "{s:?}");
        assert_eq!(s.application_ns, first_app, "nothing added until the next pause");

        // A completion with no pause in flight is not a safepoint.
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.safepoint_stats().count, 1);

        std::thread::sleep(Duration::from_millis(2));
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let t = barrier.safepoint_stats();
        assert_eq!(t.count, 2);
        assert!(t.stopped_ns >= s.stopped_ns);
        assert!(
            t.application_ns >= first_app + 2_000_000,
            "the 2 ms between the pauses is application time: {t:?}"
        );
        assert!(t.sync_ns <= t.stopped_ns);
    }

    /// A losing request is no safepoint of its own.
    #[test]
    fn gci_i44_sp_a_losing_request_adds_nothing() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert!(!barrier.request_stw(ThreadId(1), 2), "a pause is in flight");
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.safepoint_stats().count, 1);
    }
}

/// gci i46/lg: the safepoint accounting's application-time base
/// (`gci-i45rv-waves-i41-i44-review-findings-FIXED-20261016` item 3, missing test
/// M3), the pauses that are no HotSpot safepoint
/// (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual 6) and the
/// frame-trace operation scopes (residual 2).
#[cfg(test)]
mod gci_i46_lg_safepoint_tests {
    use super::*;

    fn pause(barrier: &GcBarrier) {
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }

    /// M3: a `complete_gc` with no pause in flight neither counts nor moves
    /// the application-time base, so the interval before it stays
    /// application time. (Before i46/lg the base was `released_at_ns`, which
    /// the spurious call rewrote, and the 3 ms below were lost.)
    #[test]
    fn gci_i46_lg_a_spurious_completion_keeps_the_application_interval() {
        let barrier = GcBarrier::new();
        pause(&barrier);
        let first = barrier.safepoint_stats();
        assert_eq!(first.count, 1);
        std::thread::sleep(Duration::from_millis(3));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.safepoint_stats(), first, "a spurious completion adds nothing");
        assert!(barrier.request_stw(ThreadId(0), 1));
        let s = barrier.safepoint_stats();
        assert!(
            s.application_ns >= first.application_ns + 3_000_000,
            "the 3 ms before the spurious call are application time: {first:?} -> {s:?}"
        );
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.safepoint_stats().count, 2);
    }

    /// Residual 6: a pause marked as no HotSpot safepoint adds no count, no
    /// stopped or sync time, and gives its application interval back at
    /// once; the next counted pause's interval spans it.
    #[test]
    fn gci_i46_lg_a_pause_that_is_no_safepoint_is_not_counted() {
        let barrier = GcBarrier::new();
        // Outside a pause: a no-op.
        barrier.note_pause_not_a_safepoint();
        pause(&barrier);
        let first = barrier.safepoint_stats();
        std::thread::sleep(Duration::from_millis(2));
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.wait_for_all();
        assert!(barrier.safepoint_stats().application_ns > first.application_ns);
        barrier.note_pause_not_a_safepoint();
        barrier.note_pause_not_a_safepoint();
        assert_eq!(
            barrier.safepoint_stats().application_ns,
            first.application_ns,
            "taken back inside the pause, once"
        );
        std::thread::sleep(Duration::from_millis(1));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let after = barrier.safepoint_stats();
        assert_eq!(after.count, 1);
        assert_eq!(after.stopped_ns, first.stopped_ns);
        assert_eq!(after.sync_ns, first.sync_ns);
        assert_eq!(after.application_ns, first.application_ns);
        // The TTSP census itself keeps every pause.
        assert_eq!(barrier.ttsp_stats().pauses, 2);
        std::thread::sleep(Duration::from_millis(2));
        assert!(barrier.request_stw(ThreadId(0), 1));
        let s = barrier.safepoint_stats();
        assert!(
            s.application_ns >= first.application_ns + 5_000_000,
            "from the first pause's release, across the uncounted one: {first:?} -> {s:?}"
        );
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let t = barrier.safepoint_stats();
        assert_eq!(t.count, 2);
        assert!(t.sync_ns <= t.stopped_ns);
    }

    /// `CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT=0`: every pause counts, as gci
    /// i44/sp did.
    #[test]
    fn gci_i46_lg_the_kill_switch_counts_every_pause() {
        let barrier = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT", Some("0"))],
            GcBarrier::new,
        );
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.wait_for_all();
        barrier.note_pause_not_a_safepoint();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.safepoint_stats().count, 1);
    }

    /// Residual 2: the innermost scope of the calling thread names its
    /// frame-trace pauses; another thread, and the thread after the last
    /// scope drops, see none.
    #[test]
    fn gci_i46_lg_frame_trace_op_scopes_nest_per_thread() {
        let barrier = GcBarrier::new();
        assert_eq!(barrier.frame_trace_op(), None);
        let outer = barrier.frame_trace_op_scope(None);
        assert_eq!(barrier.frame_trace_op(), Some(None));
        {
            let _inner = barrier.frame_trace_op_scope(Some("PrintThreads"));
            assert_eq!(barrier.frame_trace_op(), Some(Some("PrintThreads")));
            std::thread::scope(|s| {
                s.spawn(|| assert_eq!(barrier.frame_trace_op(), None, "per thread"));
            });
        }
        assert_eq!(barrier.frame_trace_op(), Some(None));
        drop(outer);
        assert_eq!(barrier.frame_trace_op(), None);
        assert_eq!(barrier.frame_trace_ops_used.load(Ordering::Acquire), 0);
    }
}

/// zt t30/cv (perf-counter page `zgc-zt12pl4-*` item 3): the VM-thread CPU
/// accumulator behind `sun.threads.cpu_time.vm`.
#[cfg(test)]
mod zt30_cv_vm_thread_cpu_tests {
    use super::*;

    /// One pause's share: the same thread's CPU delta, nothing for a pause
    /// another thread completed or a clock that could not be read, never
    /// negative.
    #[test]
    fn zt30_cv_a_pause_books_its_initiators_cpu_delta_only() {
        assert_eq!(
            vm_op_cpu_share(Some((7, 1_000)), Some(7), Some(4_500)),
            3_500
        );
        assert_eq!(vm_op_cpu_share(Some((7, 1_000)), Some(8), Some(4_500)), 0);
        assert_eq!(vm_op_cpu_share(Some((7, 1_000)), Some(7), None), 0);
        assert_eq!(vm_op_cpu_share(None, Some(7), Some(4_500)), 0);
        assert_eq!(vm_op_cpu_share(Some((7, 9_000)), Some(7), Some(4_500)), 0);
    }

    /// Spin `ms` milliseconds of this thread's CPU. Counted on the thread's
    /// own CPU clock where there is one: on a loaded host a wall-clock spin
    /// can get a fraction of a core (8 ms of CPU for a 20 ms spin on a host
    /// at load 23 of 16 cores), which failed the 10 ms floor below. The wall
    /// cap keeps a clock that does not advance from hanging the test.
    fn burn(ms: u64) {
        let wall_cap = Instant::now() + Duration::from_millis(ms * 20);
        let start = cratonvm_gc::thread_cpu::current_thread_cpu_ns();
        let until = Instant::now() + Duration::from_millis(ms);
        let mut x = 0u64;
        loop {
            for _ in 0..1024 {
                x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(1));
            }
            let done = match (start, cratonvm_gc::thread_cpu::current_thread_cpu_ns()) {
                (Some(s), Some(now)) => now.saturating_sub(s) >= ms * 1_000_000,
                _ => Instant::now() >= until,
            };
            if done || Instant::now() >= wall_cap {
                break;
            }
        }
    }

    /// A pause that burns CPU on its initiator grows the total on Linux and
    /// Windows (and only across the pause: CPU between pauses is not booked);
    /// on Windows the clock has a scheduler quantum's resolution (~15.6 ms),
    /// as HotSpot's there, so only the 20 ms pause is asserted. 0 elsewhere.
    #[test]
    fn zt30_cv_pauses_accumulate_the_initiators_cpu_monotonically() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_PERF_VM_THREAD_CPU", None)],
            || {
                let barrier = GcBarrier::new();
                assert_eq!(barrier.vm_thread_cpu_ns(), 0);
                assert!(barrier.request_stw(ThreadId(0), 1));
                barrier.wait_for_all();
                burn(20);
                barrier.complete_gc(cratonvm_types::PointerMap::default());
                let first = barrier.vm_thread_cpu_ns();
                // Outside a pause: not booked.
                burn(50);
                assert!(barrier.request_stw(ThreadId(0), 1));
                barrier.wait_for_all();
                burn(5);
                barrier.complete_gc(cratonvm_types::PointerMap::default());
                let second = barrier.vm_thread_cpu_ns();
                if cfg!(target_os = "linux") {
                    assert!(first >= 10_000_000, "20 ms burnt in the pause: {first}");
                    assert!(second > first, "monotone: {first} -> {second}");
                    assert!(
                        second - first < 40_000_000,
                        "the 50 ms between the pauses was booked: {first} -> {second}"
                    );
                } else if cfg!(windows) {
                    assert!(second >= first, "monotone: {first} -> {second}");
                    assert!(
                        second - first < 40_000_000,
                        "the 50 ms between the pauses was booked: {first} -> {second}"
                    );
                } else {
                    assert_eq!((first, second), (0, 0));
                }
                // A completion with no pause in flight books nothing.
                barrier.complete_gc(cratonvm_types::PointerMap::default());
                assert_eq!(barrier.vm_thread_cpu_ns(), second);
            },
        );
    }

    /// `CRATONVM_PERF_VM_THREAD_CPU=0`: nothing is booked.
    #[test]
    fn zt30_cv_the_kill_switch_books_nothing() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_PERF_VM_THREAD_CPU", Some("0"))],
            || {
                let barrier = GcBarrier::new();
                assert!(barrier.request_stw(ThreadId(0), 1));
                barrier.wait_for_all();
                burn(5);
                barrier.complete_gc(cratonvm_types::PointerMap::default());
                assert_eq!(barrier.vm_thread_cpu_ns(), 0);
            },
        );
    }
}

/// t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`): the per-OS-thread
/// page-poll cell follows the handshake bit, and only it.
#[cfg(test)]
mod t1w8_safepoll_tests {
    use super::*;
    use cratonvm_jit::poll_page::ARMED_OFFSET;

    fn leaked_cell() -> &'static AtomicUsize {
        Box::leak(Box::new(AtomicUsize::new(0)))
    }

    fn word_with_cell(cell: &'static AtomicUsize) -> Arc<LoopPollWord> {
        let w = LoopPollWord::with_word(0);
        *w.page_cell.lock() = cell as *const AtomicUsize as usize;
        Arc::new(w)
    }

    #[test]
    fn the_page_cell_mirrors_the_handshake_bit_not_the_pause_count() {
        let cell = leaked_cell();
        let w = word_with_cell(cell);
        assert!(w.arm_handshake());
        assert_eq!(cell.load(Ordering::SeqCst), ARMED_OFFSET);
        assert!(w.arm_handshake(), "a second arm keeps it armed");
        assert_eq!(cell.load(Ordering::SeqCst), ARMED_OFFSET);
        assert!(w.take_handshake_arm());
        assert_eq!(cell.load(Ordering::SeqCst), 0, "the owner's take disarms");
        assert!(!w.take_handshake_arm());
        assert_eq!(cell.load(Ordering::SeqCst), 0);
        // A pause is the barrier's stop page, never the cell.
        w.note_pause_begin();
        assert_eq!(cell.load(Ordering::SeqCst), 0);
        w.note_pause_end();
    }

    #[test]
    fn a_word_without_a_cell_arms_only_its_bit() {
        let w = Arc::new(LoopPollWord::with_word(0));
        assert!(w.arm_handshake());
        assert_eq!(w.load() & LoopPollWord::HANDSHAKE, LoopPollWord::HANDSHAKE);
        assert!(w.take_handshake_arm());
        assert_eq!(*w.page_cell.lock(), 0);
    }

    #[test]
    fn a_dropped_handle_disarms_and_forgets_its_cell() {
        let cell = leaked_cell();
        let handle = LoopPollHandle(word_with_cell(cell));
        assert!(handle.word().arm_handshake());
        assert_eq!(cell.load(Ordering::SeqCst), ARMED_OFFSET);
        let kept = Arc::clone(handle.word());
        drop(handle);
        assert_eq!(cell.load(Ordering::SeqCst), 0, "left disarmed");
        assert_eq!(*kept.page_cell.lock(), 0, "and forgotten");
        assert!(!kept.arm_handshake());
        assert_eq!(cell.load(Ordering::SeqCst), 0);
    }

    /// Requesters and the owner racing: once everyone is done the cell says
    /// what the bit says (the store-under-lock protocol of `sync_page_cell`).
    #[test]
    fn racing_arms_and_takes_leave_the_cell_agreeing_with_the_bit() {
        let cell = leaked_cell();
        let w = word_with_cell(cell);
        let mut joins = Vec::new();
        for _ in 0..3 {
            let w = Arc::clone(&w);
            joins.push(std::thread::spawn(move || {
                for _ in 0..2_000 {
                    w.arm_handshake();
                }
            }));
        }
        {
            let w = Arc::clone(&w);
            joins.push(std::thread::spawn(move || {
                for _ in 0..2_000 {
                    w.take_handshake_arm();
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
        let up = w.load() & LoopPollWord::HANDSHAKE != 0;
        let expect = if up { ARMED_OFFSET } else { 0 };
        assert_eq!(cell.load(Ordering::SeqCst), expect);
        w.take_handshake_arm();
        assert_eq!(cell.load(Ordering::SeqCst), 0);
    }

    /// Off (the default), a barrier maps no pair and its pauses change nothing
    /// a page-form poll could see.
    #[test]
    fn a_barrier_without_the_switch_has_no_poll_page() {
        if cratonvm_jit::poll_page::enabled() {
            return;
        }
        let barrier = GcBarrier::new();
        assert_eq!(barrier.page_poll_base(), 0);
        assert!(barrier.request_stw(ThreadId(1), 1));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.page_poll_base(), 0);
    }
}
