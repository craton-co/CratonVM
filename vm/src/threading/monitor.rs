// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVM monitor (intrinsic lock) implementation.
//!
//! Every Java object can be used as a monitor. The JVM's `monitorenter` and
//! `monitorexit` instructions acquire and release these monitors. Monitors
//! are **reentrant**: a thread that already owns a monitor can enter it again,
//! incrementing an entry count.
//!
//! When multiple OS threads contend for the same monitor, `parking_lot::Condvar`
//! is used to block and wake threads. Two separate condvars are used:
//! - `entry_condvar`: wakes threads blocked on `monitorenter`
//! - `wait_condvar`: wakes threads blocked on `Object.wait()`
//!
//! # Where a monitor lives
//!
//! CratonVM spawns one real OS thread per Java thread, so a process-wide lock
//! on a synchronization fast path is a hard scalability ceiling, not a
//! theoretical one. This module therefore follows HotSpot's shape:
//!
//! * **Uncontended / re-entrant** locking never leaves the object's own 32-bit
//!   `mark_word` (`try_thin_lock` / `try_thin_recursive_lock` /
//!   `try_thin_unlock`) — one CAS, no allocation, no table. The owner field is
//!   an 11-bit per-VM lock-slot LEASE ([`LockSlots`]), and compiled code takes
//!   the same CAS inline (`jit/src/runtime_lowering.rs::emit_inline_thin_lock`,
//!   armed per thread through `JitMonitorBlock`), keeping the same lessee-owned
//!   `LeaseBlock::acquired` count the helpers keep.
//! * Compiled code also takes an INFLATED monitor inline -- one CAS on
//!   [`Monitor`]'s owner word to enter, one `XCHG` to leave -- when the
//!   monitor is the one its lease's cache names (`LeaseBlock`, filled by the
//!   helpers, validated by the index epoch). Round 11 wave 17. Since round 12
//!   wave 2 it also spins for such a monitor held by another thread, up to
//!   the monitor's adaptive budget, before it calls the helper
//!   (`CRATONVM_JIT_INLINE_INFLATED_SPIN`); that spinner is never counted in
//!   [`Monitor::spinners`].
//! * **Inflated** locking: since the 8-byte object header (2026-09-24) the
//!   `MARK_INFLATED` word carries NO pointer -- only the state and the
//!   quartet. The `Monitor` is found by the object's ADDRESS in
//!   [`MonitorTable`]'s sharded index (`lookup_indexed`, one L6 shard lock),
//!   which is therefore the monitor's only home, not merely an enumeration
//!   aid. Inflation publishes the INFLATED word and inserts the index entry
//!   under that same shard lock ([`MonitorTable::inflate_locked`]), so a
//!   reader that sees INFLATED and then takes the shard lock always finds it.
//!
//! ## Index ownership and lifetime (READ BEFORE EDITING)
//!
//! The index entry's `Arc<Monitor>` is the structural reference; every thread
//! blocked in `block_enter` / `wait` holds its own clone. `remap_after_gc`
//! re-keys moved objects (a moved entry wins over a retained stale one at the
//! same address) and `prune_dead` removes entries for an EXACT dead set (ZGC).
//! The Generational and G1 backends report no dead set: their entries of dead
//! objects are dropped in the stop-the-world epilogue by
//! `MonitorTable::prune_stale_after_gc`, from the in-place liveness verdict
//! (round 12 wave 2). An entry that survives both is replaced when a new
//! object at that address inflates.
//! An INFLATED word with no entry can only come from a relocation that skipped
//! the re-key; `inflate_locked` reports it and installs a fresh monitor.
//!
//! The numbered rules below describe the PRE-8-byte-header design, in which
//! the mark word held the monitor's address and owned a strong reference
//! (`Monitor::mark_ref`). They are kept for the reclaim reasoning, which still
//! applies to the index entry; `mark_ref` is never set any more, so
//! `release_mark_ref` is a no-op.
//!
//! ## Mark-word monitor ownership and lifetime (pre-8-byte-header)
//!
//! A `Monitor` reachable from a mark word must outlive every thread that can
//! load that mark word; freeing one while a thread is parked on it is a
//! use-after-free, not a perf bug. The rule that makes the raw pointer sound:
//!
//! 1. **The mark word owns a strong reference.** On the inflation CAS that
//!    publishes `MARK_INFLATED`, the publisher leaks one `Arc<Monitor>` clone
//!    into the mark word (see [`MonitorTable::publish_inflated`]) and records
//!    that fact in [`Monitor::mark_ref`]. So an `INFLATED` mark word is, by
//!    construction, accompanied by a live strong reference that nothing but
//!    the reclaim path can drop.
//! 2. **A reader can therefore always upgrade.** `&Monitor` borrowed from the
//!    mark word ([`monitor_from_mark`]) and `Arc<Monitor>` cloned from it
//!    ([`monitor_arc_from_mark`]) are both sound *because* of (1): the strong
//!    count is >= 1 for as long as the mark word says `INFLATED`.
//! 3. **The reference is released only for a provably dead object.** There is
//!    exactly ONE release site: `MonitorTable::prune_dead`, which the collector
//!    calls with an **exact** set of just-swept addresses. A thread can only
//!    load an object's mark word while holding a live reference to that
//!    object, so a swept object's mark word has no possible reader — the
//!    release cannot race a load.
//!
//!    `remap_after_gc` deliberately does **not** release. Its "absent from the
//!    forwarding map" signal means *dead* only for a whole-heap collector; for
//!    a partial one (G1 young/mixed, generational minor GC) an absent key is
//!    routinely a live in-place survivor whose mark word still names the
//!    monitor. Releasing there would be a use-after-free. The old opt-in that
//!    did so (`CRATONVM_RECLAIM_DEAD_MONITORS`) was default-off, had therefore
//!    never run, and is gone.
//! 4. **Release is never the last drop under a waiter.** Any thread parked in
//!    `block_enter` / `wait` reached there through `Arc<Monitor>`, i.e. holds
//!    its own strong reference for the whole park. Dropping the mark-word
//!    reference (and the registry's) therefore cannot free the monitor out
//!    from under it, and `Monitor::mark_ref` is cleared with a `swap` so a
//!    double release is a no-op rather than a double free.
//! 5. **Relocation is free.** A moving collector byte-copies the header, so
//!    the monitor pointer travels with the object and stays valid (monitors
//!    live in the Rust heap, never the Java heap). The GC never interprets
//!    the mark word as an object reference — see the `mark_word` handling in
//!    `gc/src/gc.rs`, `gc/src/g1.rs`, `gc/src/gen_heap.rs`, `gc/src/region.rs`,
//!    all of which copy it verbatim. Re-keying the enumeration index in
//!    `remap_after_gc` is a bookkeeping detail; correctness of locking no
//!    longer depends on it.
//!
//! Consequence: the historical "mark word says INFLATED but the registry has
//! no entry" invariant violation (the audit finding 1(b) tripwire, which used
//! to re-inflate a *second* `Monitor` and orphan every waiter on the first) is
//! structurally impossible now. The mark word is the single source of truth;
//! a missing index entry is repaired from it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use cratonvm_types::{self as types, ObjectHeader};
use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;

use crate::error::{MethodCallFailed, RuntimeError, VmError};
// SECURITY FIX (V11): wire the L6 `monitors` registry through the lock-order
// enforcement framework so the documented hierarchy is actually checked at
// runtime in debug builds.
//
// ARCH-2026-07-26: the registry is now SHARDED, and every shard sits at the
// same L6 `Monitors` level. The descending rule forbids acquiring two locks at
// the same level, so no code path may ever hold two shard guards at once —
// every multi-shard walk below locks exactly one shard at a time. The wrapper
// family moved from the std-backed `OrderedMutex` to the `parking_lot`-backed
// `OrderedPlMutex` (same level, same enforcement) because these maps are never
// held across a panic, so poison plumbing bought nothing but `.expect()` noise
// at every call site.
use crate::runtime::lock_order::{LockLevel, OrderedPlMutex};
use crate::threading::jvm_thread::ThreadId;
use crate::types::ObjectRef;

// ---------------------------------------------------------------------------
// CAS-lock hot-path cache
// ---------------------------------------------------------------------------
//
// `Unsafe.compareAndSet*` is implemented with a per-object `Mutex` because a
// `Value` slot is not itself atomic.  The registry owns each mutex in an Arc;
// the old hot path acquired the registry shard and cloned that Arc for every
// CAS, even when one AQS state object was being retried by the same Java
// thread.  Keep a small, direct-mapped, *non-owning* cache of those mutex
// pointers instead.  The registry remains the owner and linearization point.
//
// A collection may re-key or discard an idle registry entry, so entries are
// valid only for one `cas_lock_epoch`.  `remap_after_gc` and `prune_dead` run
// under the collector's stop-the-world token and bump that epoch before a
// mutator can resume.  Consequently a raw pointer is never dereferenced after
// its owning Arc can have been dropped.  This avoids a per-operation Arc
// retain/release without retaining Java objects or locks across a collection.
const CAS_LOCK_CACHE_SLOTS: usize = 8;

#[derive(Clone, Copy)]
struct CasLockCacheEntry {
    table_id: u64,
    epoch: u64,
    object_key: usize,
    lock: *const Mutex<()>,
}

impl CasLockCacheEntry {
    const EMPTY: Self = Self {
        table_id: 0,
        epoch: 0,
        object_key: 0,
        lock: std::ptr::null(),
    };
}

thread_local! {
    static CAS_LOCK_CACHE: std::cell::RefCell<[CasLockCacheEntry; CAS_LOCK_CACHE_SLOTS]> =
        const { std::cell::RefCell::new([CasLockCacheEntry::EMPTY; CAS_LOCK_CACHE_SLOTS]) };
}

static NEXT_CAS_LOCK_CACHE_TABLE_ID: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// KC16-watchdog: global stack-dump-request flag for parked Object.wait()ers
// ---------------------------------------------------------------------------
//
// The interpreter top-of-loop polls `SharedVm::stack_dump_pending()` so any
// thread executing bytecode acks the watchdog within a single dispatch.
// Threads parked in `Object.wait()` are blocked OFF the interpreter loop
// (inside `parking_lot::Condvar::wait_for`) and therefore never reach the
// top-of-loop check. This static flag lets the watchdog wake them.
//
// `SharedVm::request_stack_dump()` sets this flag in addition to its own
// per-VM flag. The wait loop in `Monitor::wait` polls it every poll slice
// (the same cadence as the interrupt flag: 5 ms, backing off to
// `WAIT_POLL_SLICE_MAX` since round 14 wave 3) and exits the wait when set; the
// monitor_wait caller then sees the `SharedVm` flag and dumps frames
// through the normal path.
static STACK_DUMP_WAIT_FLAG: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[inline]
pub fn stack_dump_wait_flag() -> &'static std::sync::atomic::AtomicBool {
    &STACK_DUMP_WAIT_FLAG
}

/// KC16-watchdog: signal all threads parked in `Object.wait()` to exit the
/// condvar wait and re-check their state (where the interpreter loop will
/// observe `SharedVm::stack_dump_pending()` and emit a frame snapshot).
///
/// Called by `SharedVm::request_stack_dump()`.
pub fn signal_stack_dump_to_waiters() {
    STACK_DUMP_WAIT_FLAG.store(true, std::sync::atomic::Ordering::Release);
}

/// KC16-watchdog: gated diagnostic. When `CRATONVM_DBG_MONENTER` is set, the
/// contended `Monitor::enter` loop polls the stack-dump flag and emits the
/// blocked thread's frame snapshot (deposited by `monitor_enter` in
/// `vm_exec`). This surfaces a thread deadlocked while acquiring a
/// `synchronized` monitor — otherwise invisible to the watchdog, which only
/// sees `Object.wait` / `LockSupport.park` waiters. **Default OFF**: the
/// hot contended-enter path stays byte-for-byte unchanged in normal runs
/// (plain `entry_condvar.wait`); the poll variant runs only under the flag.
/// Read once and cached so the per-enter check is a single relaxed load.
/// `CRATONVM_WAIT_SPURIOUS_MS=<n>` — see the call site in `Monitor::wait`.
/// Diagnostic only; `None` (unset) leaves the wait loop unchanged.
/// `CRATONVM_MONITOR_PENDING_NOTIFY=0` — restore the condvar-only
/// `Object.wait()`, i.e. the behaviour that lost a delivered `notifyAll()`.
///
/// Default ON. It exists so the fix can be A/B'd INSIDE ONE BINARY: this
/// stall's rate is load-sensitive (4/20 at load 12-84, 1/23 at load 6-10), so
/// a before-binary/after-binary comparison across two sessions measures the
/// host, not the change. With this switch the two arms can be interleaved
/// run-by-run and see the same load distribution.
///
/// OFF does NOT stop `parked_waiters` being maintained — that costs two
/// saturating adds under a lock already held and keeps the two arms differing
/// in exactly one thing: whether the condition is consulted.
fn monitor_pending_notify() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_MONITOR_PENDING_NOTIFY")
                .ok()
                .as_deref(),
            Some("0")
        )
    })
}

/// Process-wide totals for the notification CREDIT, so the fast path can be
/// shown to RUN rather than merely to exist.
///
/// The stall dump reports `consumed=` per waiter, but it only prints at a
/// stall — so a run that never stalls says nothing about whether the credit
/// path was exercised at all, and "no stalls" would then be
/// indistinguishable from "the switch was off". These are the denominator.
///
/// `condvar_signalled` counts `wait_for` returns that were NOT timeouts, i.e.
/// the condvar delivering under its own steam. Read beside `consumed`, the two
/// say how much of the waking this VM does actually depends on the signal:
/// `consumed` far above `condvar_signalled` would mean the condvar was
/// carrying almost none of it.
static NOTIFY_CREDITS_CREATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static NOTIFY_CREDITS_CONSUMED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CONDVAR_SIGNALLED_RETURNS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Credits taken on a wakeup the condvar did NOT signal — the waiter's
/// `wait_for` TIMED OUT and the notification was found in state instead.
///
/// **This is the money number.** Every one of these is a notification the
/// condvar-only wait would have had to pick up on some later poll, and a stall
/// is what happens when there is no later poll that ever sees it.
/// `credits_consumed - this` is the ordinary case, where the signal and the
/// credit arrived together.
static NOTIFY_CREDITS_TAKEN_UNSIGNALLED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Print the notification-credit census on `CRATONVM_DBG=monitor-notify`.
///
/// Prints even when every counter is zero: a zero line is the answer "nothing
/// reached this path", and it is worth nothing unless it can be told apart
/// from the switch having been off — which the absence of a line cannot.
pub fn report_monitor_notify_census_at_exit() {
    // The contention census (`CRATONVM_DBG_MONITOR_CONTENTION`, round 12
    // wave 1) shares this exit hook; it prints zeros too, for the same reason.
    if contention_census_on() {
        let mut line = String::from("[MONITOR-CONTENTION] EXIT");
        for (name, count) in CONTENTION_EVENT_NAMES.iter().zip(CONTENTION_CENSUS.iter()) {
            line.push_str(&format!(" {name}={}", count.load(Ordering::Relaxed)));
        }
        eprintln!("{line}");
    }
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MONITOR_NOTIFY").is_none() {
        return;
    }
    use std::sync::atomic::Ordering::Relaxed;
    eprintln!(
        "[MONITOR-NOTIFY] EXIT credits_created={} credits_consumed={} \
         taken_unsignalled={} condvar_signalled={} switch={}",
        NOTIFY_CREDITS_CREATED.load(Relaxed),
        NOTIFY_CREDITS_CONSUMED.load(Relaxed),
        NOTIFY_CREDITS_TAKEN_UNSIGNALLED.load(Relaxed),
        CONDVAR_SIGNALLED_RETURNS.load(Relaxed),
        if monitor_pending_notify() {
            "ON"
        } else {
            "OFF"
        },
    );
}

fn wait_spurious_ms() -> Option<u64> {
    static V: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_WAIT_SPURIOUS_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
    })
}

pub fn mon_enter_dump_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_DBG_MONENTER").is_ok())
}

/// `CRATONVM_MONITOR_ENTER_SPIN=0` — switch off the bounded spin a contended
/// `monitorenter` takes before it parks ([`Monitor::spin_try_enter`]), and
/// restore the park-at-once path in the same binary. Default ON.
/// gen r4w5/thrash5 (2026-09-24).
///
/// gen r4w6/review6: the typed snapshot (`GcFlags::monitor_enter_spin`), not
/// a process-wide `OnceLock` that latched the first reader's value (a test
/// override included) for the rest of the process. One relaxed load plus the
/// snapshot pointer, on a path that is about to spin or park anyway.
pub(crate) fn monitor_enter_spin_enabled() -> bool {
    cratonvm_types::flags().gc.monitor_enter_spin
}

/// `CRATONVM_MONITOR_LAZY_PARK_US=<n>` (`n` in 1..=100 000 microseconds):
/// before a contended `monitorenter` takes the GC-blocked park, it parks for
/// up to `n` us as a still-running mutator ([`Monitor::park_enter_while`]), and
/// only a contender still waiting after that pays the TLAB retire, the root
/// deposit (a conservative scan of its compiled frames) and the wake-side
/// re-deposit. Unset, `0` or unparsable: off, and the path is byte for byte
/// the wave-18 one. Also off under `CRATONVM_MONITOR_FASTPATH=0`.
///
/// Opt-in because the saving has a price the default must not pay unmeasured:
/// a stop-the-world pause requested while a contender is parked here waits
/// up to `n` us (plus the host's timer slack) for it, `Thread.getState()`
/// reads RUNNABLE rather than BLOCKED meanwhile, and the JMX contention
/// publish is made before the park, as on the full path. Round 11 wave 19,
/// lane lock (proposal W17-1, first step). Latched once per process: it is a
/// tuning knob, not compatibility state.
pub(crate) fn monitor_lazy_park_budget() -> Option<std::time::Duration> {
    static BUDGET: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        if !crate::threading::thread_registry::monitor_fastpath_enabled() {
            return None;
        }
        cratonvm_types::flags::runtime_var("CRATONVM_MONITOR_LAZY_PARK_US")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|us| *us > 0)
            .map(|us| std::time::Duration::from_micros(us.min(100_000)))
    })
}

/// `CRATONVM_MONITOR_SPIN_AFTER_WAKE=0` (or `false`/`off`): a parked entrant
/// that is woken re-tries its CAS once and parks again at once, as through
/// round 11. Default ON (round 12 wave 1, lane lock, proposals W16-3 / W19-3):
/// a woken entrant first spins for the monitor with the state lock released
/// and `Monitor::succ_pending` still set, as HotSpot's `EnterI` does after
/// its `park` -- a woken thread that loses to the running owner's next
/// `monitorenter` then usually wins one of the owner's next releases instead
/// of costing a second park and a second OS wake-up. Off where spinning is
/// ([`contended_spin_enabled`]: one CPU, `CRATONVM_MONITOR_FASTPATH=0`).
/// Latched once per process: a tuning knob, not compatibility state.
fn spin_after_wake_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        contended_spin_enabled()
            && !matches!(
                cratonvm_types::flags::runtime_var("CRATONVM_MONITOR_SPIN_AFTER_WAKE").as_deref(),
                Ok("0") | Ok("false") | Ok("off")
            )
    })
}

/// `CRATONVM_MONITOR_MAX_SPINNERS=<n>` (`n >= 1`): a contender that finds `n`
/// threads already spinning on a monitor ([`Monitor::spinners`]) skips both
/// contended spins ([`Monitor::spin_try_enter_adaptive`],
/// [`Monitor::spin_try_enter`]) and goes straight to the park. Unset, `0` or
/// unparsable: no cap, the round-11 behaviour.
///
/// Opt-in, for measurement (round 12 wave 1, lane lock): with 48 threads on
/// one lock (`ThreadChurn 192 20000 1`) every contender spins, up to
/// ~`SPIN_MAX` hints and then ~2 300 hints and eight yields, all polling the
/// owner word, so each release invalidates 47 copies of the owner's line and
/// the spinners compete with the owner for CPUs. HotSpot's `ObjectMonitor`
/// keeps most such threads parked. Whether capping pays for the parks it adds
/// is what the A/B has to say. Latched once per process.
fn max_monitor_spinners() -> Option<u32> {
    static CAP: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_MONITOR_MAX_SPINNERS")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|n| *n > 0)
    })
}

/// `CRATONVM_MONITOR_LAZY_SPINNERS=0` (or `false`/`off`/`no`): every contended
/// spin counts itself in [`Monitor::spinners`] for its whole length, as
/// through round 12 wave 1. Default ON (round 12 wave 2, lane lock2): a
/// spinner registers only once it sees a parked entrant
/// ([`SpinnerGuard::register_if_parked`]).
///
/// Why: the census of `ThreadChurn 192 20000 1` (48 threads on one lock)
/// counted 3 392 443 adaptive-spin wins against 189 parks. Each of those spins
/// did a `SeqCst` `fetch_add` and `fetch_sub` on the hand-off line, the winner's
/// `fetch_sub` inside the critical section it had just won, and the owner's
/// compiled exit reads that same line (twice) before and after its release --
/// so the line ping-ponged between 48 cores several times per hand-over,
/// every trip on the critical path. The count exists only to spare a parked
/// entrant a wake-up a spinner would make useless; with nobody parked it
/// buys nothing. An unregistered spinner is invisible to the release
/// protocol, like a thread that simply arrives and CASes (barging): a
/// release never skips a wake because of it, so it owes no wake on its way
/// out, and the lost-wake-up argument on [`Monitor::wake_successor`] is
/// untouched. Forced on (eager) when `CRATONVM_MONITOR_MAX_SPINNERS` is set:
/// the cap counts registered spinners. Latched once per process: a tuning
/// knob, not compatibility state.
fn lazy_spinner_registration() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        max_monitor_spinners().is_none()
            && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MONITOR_LAZY_SPINNERS")
    })
}

/// The per-VM monitor switches of round 12 wave 8 (lane monitor), read ONCE
/// when the VM's [`MonitorTable`] is built ([`MonitorTuning::from_env`]) and copied
/// into every [`Monitor`] that table inflates, where they sit on the owner
/// line next to the words they govern (written once, before the monitor is
/// published; only ever read after). Per VM rather than a process `OnceLock`
/// (`vm/tests/per_vm_state_statics_ratchet.rs`: no new statics), and so a
/// test can build a monitor with either arm ([`Monitor::with_tuning`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MonitorTuning {
    /// `CRATONVM_MONITOR_QUIET_RELEASE=0` (or `false`/`off`/`no`) restores the
    /// unconditional stores. Default ON: a release writes `thin_seed` and
    /// `jfr_enter_recorded` only when they are not already clear
    /// ([`Monitor::release_owner_word`], [`Monitor::exit_reporting_release`]).
    /// Both sit on the OWNER line every spinner polls, so each redundant
    /// store was one more read-for-ownership that invalidated every
    /// spinner's copy, inside the critical section: a helper or interpreter
    /// release wrote four owner-line words where the compiled inline exit
    /// writes two. Same values, fewer writes.
    quiet_release: bool,
    /// `CRATONVM_MONITOR_SPIN_BACKOFF=0` (or `false`/`off`/`no`) restores the
    /// round-12-wave-2 Rust spins. Default ON: a Rust-side spinner that sees
    /// the monitor free and LOSES it to another thread
    /// ([`Monitor::spin_try_enter_adaptive`], [`Monitor::spin_after_wake`])
    /// stops polling for an exponentially growing number of `spin_loop`
    /// hints ([`LostCasBackoff`], counted against its budget, so no bound
    /// grows), and a spin that ran out having lost a race keeps the monitor's
    /// budget instead of halving it -- as HotSpot's `TrySpin` does, which
    /// shortens `_SpinDuration` only for a spin that never saw the monitor
    /// change hands. Test-and-test-and-set with no back-off sends every
    /// spinner that reads the release at a `LOCK CMPXCHG`: one wins, the rest
    /// take the owner line exclusive in turn, just when the winner needs it
    /// (lock proposal W2L2-2).
    lost_cas_backoff: bool,
    /// `CRATONVM_MONITOR_WAIT_REACQUIRE_SPIN=0` (or `false`/`off`/`no`)
    /// restores the round-13-wave-9 re-acquisition at the end of
    /// `Object.wait()`. Default ON (round 13 wave 10, lane monitor2): a waiter
    /// that leaves the wait set (notified, timed out, interrupted) and finds
    /// the monitor held -- nearly always by its notifier, still inside the
    /// `synchronized` block that called `notify` -- first spins for it with
    /// the state lock released ([`Monitor::spin_for_free`], bounded by the
    /// adaptive budget) before it registers to park on `entry_condvar`, and a
    /// woken re-acquirer spins before it re-parks, as `enter_labeled`'s
    /// entrants do. Until then every notified waiter whose notifier had not
    /// yet left the block registered in `entry_waiters` and slept again: a
    /// SECOND OS wake-up per hand-over (the first is `wait_condvar`'s), and
    /// the notifier's compiled exit, seeing the registered entrant, took the
    /// helper to wake it. HotSpot's notified waiter costs one wake-up (the
    /// notify moves it to the entry list asleep). Unobservable: a spinner
    /// that wins is an interleaving in which the notifier released just
    /// before the waiter's first CAS, and an unregistered spinner owes and
    /// suppresses no wake-up (the lost-wake-up argument on
    /// [`Monitor::wake_successor`] is untouched).
    wait_reacquire_spin: bool,
    /// `CRATONVM_MONITOR_CACHED_NOTIFY=0` (or `false`/`off`/`no`): `holds`,
    /// `notify` and `notifyAll` of an INFLATED object take the index shard
    /// lock and clone the monitor's `Arc` every call, as through round 13
    /// wave 9. Default ON (round 13 wave 10, lane monitor2): they first ask
    /// this thread's inflated-monitor cache ([`MonitorTable::cached_inflated_monitor`]),
    /// the lookup `exit` has used since round 11 wave 16 under the same
    /// conditions -- the caller holds a live reference, the word reads
    /// INFLATED, and nothing between the lookup and the use reaches a
    /// safepoint or blocks in a GC-blocked region. A wait/notify hand-over
    /// paid two shard-lock acquisitions and two `Arc` count RMWs per
    /// `notify` + `holdsLock`, on lines the waiter's own `wait` also takes.
    cached_notify: bool,
    /// `CRATONVM_MONITOR_NOTIFY_ONE_WAITER=0` (or `false`/`off`/`no`): every
    /// `Object.wait()` parks on the monitor's shared `wait_condvar` and
    /// `notify()` signals ALL of them (`notify_all` on the condvar), as through
    /// round 14 wave 2. Default ON (round 14 wave 3, lane monitor, proposal
    /// M2-3): with the notification credit on, each waiter parks on a condvar
    /// of its own that its wait-set entry carries ([`WaitEntry::signal`]), so
    /// `notify()` wakes exactly the waiter it marked -- the N-1 other waiters
    /// no longer wake, take the state mutex in turn, find no credit and
    /// re-park. The credit stays the condition; the per-waiter condvar is only
    /// the signal. No effect under `CRATONVM_MONITOR_PENDING_NOTIFY=0`.
    notify_one_waiter: bool,
    /// `CRATONVM_MONITOR_WAIT_POLL_BACKOFF=0` (or `false`/`off`/`no`): an
    /// `Object.wait()` re-tests its interrupt flag and the watchdog's
    /// stack-dump flag every 5 ms for as long as it waits, as through round 14
    /// wave 2. Default ON (round 14 wave 3, lane monitor, proposal M2-4): the
    /// poll slice starts at 5 ms and doubles after every poll that ended
    /// nothing, up to [`WAIT_POLL_SLICE_MAX`]. The poll is a safety net only:
    /// a notification wakes the waiter through its condvar and is found as a
    /// credit, and `Thread.interrupt()` wakes it through
    /// `MonitorTable::wake_waiters_for_interrupt`. The first slices stay
    /// short, so an interrupt that lands between `wait`'s entry check and its
    /// enrolment (the one window the interrupt wake can miss) is still seen
    /// within 5 ms; later, an unwoken flag costs at most one capped slice.
    wait_poll_backoff: bool,
    /// `CRATONVM_MONITOR_INTERRUPT_WAKES_TARGET=0` (or `false`/`off`/`no`):
    /// `Thread.interrupt()` of a thread in `Object.wait()` wakes EVERY waiter
    /// on that monitor ([`Monitor::wake_all_for_interrupt`]), as through round
    /// 14 wave 3. Default ON (round 14 wave 4, lane monitor2, proposal MON14-1):
    /// each wait-set entry names its waiter ([`WaitEntry::waiter`]), and
    /// [`MonitorTable::wake_waiter_for_interrupt`] signals only the target's
    /// entry ([`Monitor::wake_for_interrupt_of`]); the N-1 other waiters no
    /// longer wake, take the state mutex, find no credit and re-park.
    interrupt_wakes_target: bool,
    /// `CRATONVM_MONITOR_WAIT_ENROL_INTERRUPT_CHECK=0` (or `false`/`off`/`no`):
    /// `Object.wait()` first reads its interrupt flag after its first park, as
    /// through round 14 wave 3. Default ON (round 14 wave 4, lane monitor2,
    /// proposal MON14-2): it reads it once more right after enrolling, under
    /// the state lock the interrupt wake also takes. An interrupter sets the
    /// flag before it takes that lock (and before it reads the registry's
    /// waiting-monitor slot, which the waiter writes before it enrols), so an
    /// interrupt either finds the waiter in the wait set and signals it, or
    /// is seen here: the window the 5 ms first slices of
    /// [`Self::wait_poll_backoff`] covered is closed, and the poll can start
    /// at [`WAIT_POLL_SLICE_RECHECKED`].
    wait_enrol_interrupt_check: bool,
    /// `CRATONVM_MONITOR_WAIT_SINGLE_PARK=0` (or `false`/`off`/`no`): an
    /// `Object.wait()` polls in backed-off slices ([`Self::wait_poll_backoff`]).
    /// Default ON (round 14 wave 4, lane monitor2, proposal MON14-4): with
    /// the notification credit, the enrolment re-check and the back-off all
    /// on, every event a waiter tests has a wake of its own -- a notification
    /// signals the waiter's condvar, an interrupt the target's entry (after
    /// the re-check above), the watchdog's stack dump every waiting monitor
    /// (`MonitorTable::wake_all_waiters_for_stack_dump`) -- so a waiter parks
    /// for its whole remaining timeout, capped by the safety slice
    /// [`WAIT_SAFETY_SLICE`] (which also bounds an untimed wait's park): a
    /// `wait(ms)` with `ms` up to 250 ms wakes once, at its deadline, and an
    /// idle untimed waiter 4 times a second instead of ten. The safety slice is
    /// the whole cost of a wake-less path, should one appear.
    wait_single_park: bool,
    /// `CRATONVM_MONITOR_SPIN_HANDOVER_ABORT=1` (opt-in; default OFF, and
    /// inert unless `CRATONVM_MONITOR_LAZY_PARK_US` is set too): the two Rust
    /// contended spins ([`Monitor::spin_try_enter_adaptive`],
    /// [`Monitor::spin_try_enter`]) give up as soon as they watch the monitor
    /// change hands -- a lost CAS, or the owner word moving from one thread to
    /// another between two polls -- as HotSpot's `TrySpin` does, and the
    /// contender goes to the running park instead of spinning on among the
    /// crowd. Round 14 wave 6 (lane monitor3): the second half of proposal
    /// M2-2 (`jit-r13-monitor2-proposals-RETIRED-20260929.md`), which only pays where the park
    /// is cheap, hence the coupling: without the lazy park an abort would
    /// send the crowd to the GC-blocked park (a TLAB retire and a root deposit
    /// per contender, the `GenR4W4HeapFullThrashProbe` hazard the fixed spin
    /// exists for). An aborted adaptive spin does not halve the budget (it saw
    /// a hand-over, as a lost race does). Measurement arm only until the
    /// census (`spin_wins_after_handover` against `spin_fails_after_handover`)
    /// says the wins an abort gives up are few.
    spin_handover_abort: bool,
}

impl MonitorTuning {
    /// The defaults (every switch on but the opt-in
    /// [`Self::spin_handover_abort`]), and what [`Monitor::new`] uses.
    const DEFAULT: Self = Self {
        quiet_release: true,
        lost_cas_backoff: true,
        wait_reacquire_spin: true,
        cached_notify: true,
        notify_one_waiter: true,
        wait_poll_backoff: true,
        interrupt_wakes_target: true,
        wait_enrol_interrupt_check: true,
        wait_single_park: true,
        spin_handover_abort: false,
    };

    /// This process's switches, for a new [`MonitorTable`].
    fn from_env() -> Self {
        Self {
            quiet_release: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOR_QUIET_RELEASE",
            ),
            lost_cas_backoff: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOR_SPIN_BACKOFF",
            ),
            wait_reacquire_spin: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOR_WAIT_REACQUIRE_SPIN",
            ),
            cached_notify: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOR_CACHED_NOTIFY",
            ),
            notify_one_waiter: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOR_NOTIFY_ONE_WAITER",
            ),
            wait_poll_backoff: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOR_WAIT_POLL_BACKOFF",
            ),
            interrupt_wakes_target: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOR_INTERRUPT_WAKES_TARGET",
            ),
            wait_enrol_interrupt_check: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOR_WAIT_ENROL_INTERRUPT_CHECK",
            ),
            wait_single_park: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOR_WAIT_SINGLE_PARK",
            ),
            spin_handover_abort: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_MONITOR_SPIN_HANDOVER_ABORT",
            ),
        }
    }

    /// Whether an `Object.wait()` parks once per [`WAIT_SAFETY_SLICE`]
    /// ([`Self::wait_single_park`]) rather than in poll slices. Only with
    /// every switch whose wake or re-check makes the poll redundant:
    /// `credits` (the notification credit, `monitor_pending_notify`), the
    /// enrolment re-check, and the back-off -- so
    /// `CRATONVM_MONITOR_WAIT_POLL_BACKOFF=0` still restores the fixed 5 ms
    /// cadence of round 14 wave 2 whatever this switch says.
    fn wait_parks_once(self, credits: bool) -> bool {
        self.wait_single_park && self.wait_enrol_interrupt_check && self.wait_poll_backoff && credits
    }

    /// The first park slice of an `Object.wait()`. 5 ms when the watchdog's
    /// stack dump is already requested (its wake may have run before this
    /// waiter enrolled) or without the enrolment re-check (the interrupt
    /// window, see [`Self::wait_poll_backoff`]); otherwise the safety slice
    /// ([`Self::wait_parks_once`]) or [`WAIT_POLL_SLICE_RECHECKED`].
    fn first_wait_slice(self, credits: bool, dump_requested: bool) -> std::time::Duration {
        if dump_requested || !self.wait_poll_backoff || !self.wait_enrol_interrupt_check {
            WAIT_POLL_SLICE
        } else if self.wait_parks_once(credits) {
            WAIT_SAFETY_SLICE
        } else {
            WAIT_POLL_SLICE_RECHECKED
        }
    }
}

/// The first poll slice of an `Object.wait()` (and every slice with
/// [`MonitorTuning::wait_poll_backoff`] off).
const WAIT_POLL_SLICE: std::time::Duration = std::time::Duration::from_millis(5);
/// The longest poll slice [`MonitorTuning::wait_poll_backoff`] grows to:
/// bounds how late an interrupt flag no wake announced, or the watchdog's
/// stack-dump flag, is noticed by a thread that has waited a while.
const WAIT_POLL_SLICE_MAX: std::time::Duration = std::time::Duration::from_millis(100);
/// The first poll slice once [`MonitorTuning::wait_enrol_interrupt_check`]
/// has closed the enrolment window (round 14 wave 4, MON14-2).
const WAIT_POLL_SLICE_RECHECKED: std::time::Duration = std::time::Duration::from_millis(20);
/// The longest single park of an `Object.wait()` under
/// [`MonitorTuning::wait_single_park`] (round 14 wave 4, MON14-4): what a
/// wake-less path would cost, and how often an idle untimed waiter re-tests.
/// 250 ms since round 14 wave 5 (1 s in wave 4): every interrupt path found
/// by reading wakes its target, but an unknown one would stall a waiter this
/// long, and 4 idle wake-ups a second are still 25x fewer than wave 2's 200.
const WAIT_SAFETY_SLICE: std::time::Duration = std::time::Duration::from_millis(250);

/// The poll slice after one that ended nothing: doubled, capped, with
/// [`MonitorTuning::wait_poll_backoff`]; unchanged without it.
#[inline]
fn next_wait_poll_slice(slice: std::time::Duration, backoff: bool) -> std::time::Duration {
    if backoff {
        (slice * 2).min(WAIT_POLL_SLICE_MAX)
    } else {
        slice
    }
}

/// The park slice after one that ended nothing: the safety slice for a
/// waiter that parks once ([`MonitorTuning::wait_parks_once`]), else
/// [`next_wait_poll_slice`].
#[inline]
fn next_wait_slice(
    slice: std::time::Duration,
    backoff: bool,
    parks_once: bool,
) -> std::time::Duration {
    if parks_once {
        WAIT_SAFETY_SLICE
    } else {
        next_wait_poll_slice(slice, backoff)
    }
}

/// The owner line's padding once [`MonitorTuning`] has taken its bytes (see
/// `Monitor::_owner_line_pad`; the layout assertions below the struct pin
/// the result).
const OWNER_LINE_PAD: usize = 43 - std::mem::size_of::<MonitorTuning>();

/// `spin_loop` hints a Rust-side spinner skips after its first lost race;
/// doubled per further loss up to [`LOST_CAS_BACKOFF_MAX`].
const LOST_CAS_BACKOFF_MIN: u32 = 8;
/// Ceiling of [`LostCasBackoff`]'s window.
const LOST_CAS_BACKOFF_MAX: u32 = 128;

/// The back-off a Rust-side spinner owes after losing a free monitor to
/// another thread ([`MonitorTuning::lost_cas_backoff`]): the winner now holds
/// the monitor for at least its critical section, so re-polling its owner
/// line at once only keeps that line shared by one more core when the winner
/// writes it. Exponential, capped, and always bounded by the room the
/// caller's budget has left, so the spin's own bound (`SPIN_MAX` hints) is
/// unchanged. Unobservable: it changes only WHEN a spinner looks again.
struct LostCasBackoff {
    /// The next window, in `spin_loop` hints.
    next: u32,
    /// Races lost so far in this spin.
    losses: u32,
    /// [`MonitorTuning::lost_cas_backoff`].
    on: bool,
}

impl LostCasBackoff {
    #[inline]
    fn new(on: bool) -> Self {
        Self {
            next: LOST_CAS_BACKOFF_MIN,
            losses: 0,
            on,
        }
    }

    /// Record one more lost race and spin out its window, at most `room`
    /// hints; answers the hints spent (`0` when the switch is off).
    #[inline]
    fn after_loss(&mut self, room: u32) -> u32 {
        self.losses = self.losses.saturating_add(1);
        note_contention(ContentionEvent::LostCas);
        if !self.on {
            return 0;
        }
        let window = self.next.min(room);
        for _ in 0..window {
            std::hint::spin_loop();
        }
        self.next = self.next.saturating_mul(2).min(LOST_CAS_BACKOFF_MAX);
        window
    }

    /// Whether this spin saw the monitor released and lost it at least once
    /// (with the switch on): the spin's failure then says "crowded", not
    /// "held long", and must not shrink the monitor's budget.
    #[inline]
    fn saw_a_handover(&self) -> bool {
        self.on && self.losses != 0
    }
}

/// What one Rust contended spin has watched of the owner word: whether the
/// monitor changed hands while it spun (round 14 wave 6, lane monitor3; the
/// signal HotSpot's `TrySpin` aborts on, and the census M2-2 is decided by).
/// Only fed while [`MonitorTuning::spin_handover_abort`] or the contention
/// census is on, so the default spin does no extra work.
struct HandoverWatch {
    /// The last non-free owner word this spin polled (`0`: none yet).
    last: u64,
    /// The spin has seen a lost race or an owner change.
    seen: bool,
}

impl HandoverWatch {
    #[inline]
    fn new() -> Self {
        Self { last: 0, seen: false }
    }

    /// Record one poll that read `owner` held (not `0`): `true` when it names
    /// a different thread than the previous held poll did -- the monitor was
    /// released and re-taken by someone else between the two. A forced
    /// release's [`OWNER_RELEASING`] is not a thread and counts as neither.
    #[inline]
    fn owner_polled(&mut self, owner: u64) -> bool {
        if owner == 0 || owner == OWNER_RELEASING {
            return false;
        }
        let changed = self.last != 0 && owner != self.last;
        self.last = owner;
        if changed {
            self.seen = true;
        }
        changed
    }

    /// The spin saw the monitor free and lost it to another thread.
    #[inline]
    fn lost_race(&mut self) {
        self.seen = true;
    }

    /// Count the spin's outcome in the census, if it had watched a hand-over.
    #[inline]
    fn finish(&self, won: bool) {
        if self.seen {
            note_contention(if won {
                ContentionEvent::SpinWinAfterHandover
            } else {
                ContentionEvent::SpinFailAfterHandover
            });
        }
    }
}

/// Count one registered entrant's wait in [`Monitor::enter_labeled`] (census
/// only): its microseconds, and its bucket.
fn note_entrant_wait(waited: std::time::Duration) {
    let us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
    note_contention_n(ContentionEvent::EntrantWaitMicros, us);
    note_contention(entrant_wait_bucket(us));
}

/// The census bucket of an entrant wait of `us` microseconds.
#[inline]
fn entrant_wait_bucket(us: u64) -> ContentionEvent {
    match us {
        0..=99 => ContentionEvent::EntrantWaitUnder100us,
        100..=999 => ContentionEvent::EntrantWaitUnder1ms,
        1_000..=15_999 => ContentionEvent::EntrantWaitUnder16ms,
        _ => ContentionEvent::EntrantWaitLonger,
    }
}

/// `CRATONVM_MONITOR_INDEX_PRUNE=0` (or `false`/`off`): the stop-the-world
/// epilogue no longer drops the inflated-monitor index entries of objects the
/// collection proved dead ([`MonitorTable::prune_stale_after_gc`]), and the
/// Generational and G1 backends keep every inflated object's monitor until
/// the VM exits, as through round 12 wave 1. Default ON (round 12 wave 2, lane
/// lock). Latched once per process: a kill switch, not compatibility state.
fn monitor_index_prune_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_MONITOR_INDEX_PRUNE").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// At most this many re-keyed index addresses wait in
/// [`MonitorTable::remapped_keys`] for the next
/// [`MonitorTable::prune_stale_after_gc`]. Past it the list is dropped and the
/// prune judges those keys by the liveness verdict alone, which is the same
/// answer for a relocated survivor (it is a live object whose word reads
/// INFLATED); the list only spares that verdict. Bounds a backend whose pauses
/// re-key without ever reaching the epilogue that consumes the list.
const REMAPPED_KEYS_CAP: usize = 1 << 16;

/// `CRATONVM_DBG_MONITOR_CONTENTION` (set): count, process-wide, what each
/// contended `monitorenter` ended in -- spin wins and failures, lazy-park
/// wins and give-ups, GC-blocked parks, condvar waits, successor wake-ups and
/// spin-after-wake wins -- and print the totals at exit
/// ([`report_monitor_notify_census_at_exit`]). Diagnostic only: the counters
/// are shared words, so a census run perturbs the contention it counts; with
/// the flag unset every site is one latched load and a branch. Round 12 wave
/// 1 (lane lock, proposal W16-4): the 80x page was diagnosed by reading,
/// never by counting, and the park-path proposals W17-1 / W19-1 need the
/// park and wake counts to be sized. Since wave 2 it also counts the index's
/// lifetime: thread-death sweeps, the entries they visit and the monitors they
/// had to release, and the entries the post-collection prune drops.
fn contention_census_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MONITOR_CONTENTION").is_some()
    })
}

/// What a contended `monitorenter` did, for [`note_contention`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentionEvent {
    /// [`Monitor::spin_try_enter_adaptive`] took the monitor.
    AdaptiveSpinWin,
    /// ... ran its budget out.
    AdaptiveSpinFail,
    /// ... or [`Monitor::spin_try_enter`] was skipped: the spinner cap.
    SpinSkipped,
    /// [`Monitor::spin_try_enter`] took the monitor.
    EnterSpinWin,
    /// ... gave up (budget out, or a pause was requested).
    EnterSpinFail,
    /// The running park ([`Monitor::park_enter_while`]) took the monitor.
    LazyParkWin,
    /// ... gave up: budget out, or a pause was requested.
    LazyParkGiveUp,
    /// A contender took the full GC-blocked park (`vm_exec`).
    BlockedPark,
    /// A registered entrant went to sleep on `entry_condvar` (each wait).
    CondvarWait,
    /// A release (or a leaving spinner) woke a parked entrant.
    SuccessorWake,
    /// A woken entrant took the monitor while spinning after its wake.
    SpinAfterWakeWin,
    /// A thread-death sweep ([`MonitorTable::release_monitors_held_by_except`]),
    /// each call. Round 12 wave 2: the index-lifetime half of the census, the
    /// measurement lock proposal W1-7 (skip the sweep) is waiting for.
    DeathSweep,
    /// ... an index entry one of its walks visited (added per walk).
    DeathSweepVisit,
    /// ... a monitor it force-released: the dying thread still owned it.
    DeathSweepRelease,
    /// An index entry [`MonitorTable::prune_stale_after_gc`] dropped.
    IndexPrune,
    /// A Rust-side spinner ([`Monitor::spin_try_enter_adaptive`],
    /// [`Monitor::spin_after_wake`]) saw the monitor free and lost it to
    /// another thread (round 12 wave 8, lane monitor: the size of the
    /// herd each release sets off, lock proposal W2L2-2's first step).
    LostCas,
    /// A waiter leaving `Object.wait()` found the monitor held and took it
    /// by spinning, without registering to park
    /// ([`MonitorTuning::wait_reacquire_spin`], round 13 wave 10).
    WaitReacquireSpinWin,
    /// Compiled code's inline spin took the monitor (round 14 wave 1, lane
    /// sync, M2-1; counted on the monitor by sites built under
    /// `CRATONVM_DBG_JITC`, drained by [`Monitor::drain_inline_spin_census`]).
    InlineSpinWin,
    /// ... ran its budget out and went to the helper.
    InlineSpinBudgetOut,
    /// ... saw a parked entrant and went to the helper.
    InlineSpinWaiterExit,
    /// A Rust contended spin ([`Monitor::spin_try_enter_adaptive`],
    /// [`Monitor::spin_try_enter`]) took the monitor AFTER it had watched it
    /// change hands (a lost race, or the owner word moving from one thread to
    /// another): the wins `CRATONVM_MONITOR_SPIN_HANDOVER_ABORT` would turn
    /// into parks. Round 14 wave 6 (lane monitor3), the census M2-2 needs.
    SpinWinAfterHandover,
    /// ... gave up (budget out, pause, or the abort) having watched a
    /// hand-over: the spins the abort would shorten.
    SpinFailAfterHandover,
    /// ... gave up at a hand-over because
    /// [`MonitorTuning::spin_handover_abort`] is on.
    SpinHandoverAbort,
    /// Microseconds a registered entrant spent in [`Monitor::enter_labeled`]
    /// between registering and acquiring (added per entry; with
    /// [`Self::CondvarWait`] it sizes the park a lazy-park budget must cover).
    EntrantWaitMicros,
    /// ... one such entry under 100 us.
    EntrantWaitUnder100us,
    /// ... 100 us up to 1 ms.
    EntrantWaitUnder1ms,
    /// ... 1 ms up to 16 ms (one Windows clock tick, the shortest timed park
    /// the lazy park gets there).
    EntrantWaitUnder16ms,
    /// ... 16 ms or longer.
    EntrantWaitLonger,
}

const CONTENTION_EVENTS: usize = ContentionEvent::EntrantWaitLonger as usize + 1;

/// The census's names, in [`ContentionEvent`] order.
const CONTENTION_EVENT_NAMES: [&str; CONTENTION_EVENTS] = [
    "adaptive_spin_wins",
    "adaptive_spin_fails",
    "spins_skipped",
    "enter_spin_wins",
    "enter_spin_fails",
    "lazy_park_wins",
    "lazy_park_give_ups",
    "blocked_parks",
    "condvar_waits",
    "successor_wakes",
    "spin_after_wake_wins",
    "death_sweeps",
    "death_sweep_visits",
    "death_sweep_releases",
    "index_prunes",
    "lost_cas",
    "wait_reacquire_spin_wins",
    "inline_spin_wins",
    "inline_spin_budget_outs",
    "inline_spin_waiter_exits",
    "spin_wins_after_handover",
    "spin_fails_after_handover",
    "spin_handover_aborts",
    "entrant_wait_us",
    "entrant_waits_under_100us",
    "entrant_waits_under_1ms",
    "entrant_waits_under_16ms",
    "entrant_waits_longer",
];

static CONTENTION_CENSUS: [AtomicU64; CONTENTION_EVENTS] =
    [const { AtomicU64::new(0) }; CONTENTION_EVENTS];

/// Count one [`ContentionEvent`] under `CRATONVM_DBG_MONITOR_CONTENTION`.
#[inline]
pub(crate) fn note_contention(event: ContentionEvent) {
    note_contention_n(event, 1);
}

/// Count `n` [`ContentionEvent`]s under `CRATONVM_DBG_MONITOR_CONTENTION`.
#[inline]
fn note_contention_n(event: ContentionEvent, n: u64) {
    if n != 0 && contention_census_on() {
        if let Some(c) = CONTENTION_CENSUS.get(event as usize) {
            c.fetch_add(n, Ordering::Relaxed);
        }
    }
}

/// Rounds of [`Monitor::spin_try_enter`]: the first
/// [`ENTER_SPIN_BUSY_ROUNDS`] back off with `spin_loop` hints (1, 2, 4 …
/// capped at 256 per round, about 2 300 hints in all), the rest yield the CPU
/// once each so an owner that shares this core can run and release.
const ENTER_SPIN_ROUNDS: u32 = 24;
/// See [`ENTER_SPIN_ROUNDS`].
const ENTER_SPIN_BUSY_ROUNDS: u32 = 16;

/// The busy-wait length of spin round `round`: `2^round` hints, capped at 256.
#[inline]
fn enter_spin_hints(round: u32) -> u32 {
    1u32 << round.min(8)
}

// REMOVED (ARCH-2026-07-26): `reclaim_dead_monitors_enabled()` /
// `CRATONVM_RECLAIM_DEAD_MONITORS`.
//
// That flag gated reclaiming inflated-monitor entries in `remap_after_gc` on
// the "absent from the GC forwarding map ⇒ dead" signal. That signal is only
// valid for a *whole-heap* collector; under a partial collector (G1
// young/mixed, the generational minor GC) an absent key is frequently a live
// in-place survivor. So the branch could drop the registry's `Arc` for a LIVE
// object — and now that the mark word is what locking dereferences, doing so
// would be a use-after-free rather than the old (already wrong) "registry
// miss, re-inflate a second monitor and orphan the waiters".
//
// The flag was default-OFF and therefore had never run, so removing it loses
// no behaviour. Sound monitor reclamation needs an EXACT dead set, which is
// precisely what `MonitorCleanup::prune_dead` receives — that path is
// unconditional, on by default, and now releases the mark-word reference too,
// so it genuinely frees. The moving collectors without a dead set are covered
// since round 12 wave 2 by `MonitorTable::prune_stale_after_gc`, which runs
// in the VM's stop-the-world epilogue with the heap's liveness verdict.

/// KC16-watchdog: callback installed by the VM that emits the current
/// thread's frame chain from a wait-site context. Set by `SharedVm`
/// during construction so `Monitor::wait` can reach the per-thread
/// frame table held in the `JvmThread` registry.
type WaitSiteDumpFn = Arc<dyn Fn(ThreadId) + Send + Sync>;
static WAIT_SITE_DUMP: std::sync::OnceLock<WaitSiteDumpFn> = std::sync::OnceLock::new();

pub fn install_wait_site_dump<F>(f: F)
where
    F: Fn(ThreadId) + Send + Sync + 'static,
{
    let _ = WAIT_SITE_DUMP.set(Arc::new(f));
}

/// Callback installed by the VM to print the state of the object a thread is
/// parked on in `Object.wait()`. Separate from [`WAIT_SITE_DUMP`] because it
/// needs heap + class metadata, which `monitor.rs` has no handle on.
type WaitObjectDumpFn = Arc<dyn Fn(ObjectRef) + Send + Sync>;
static WAIT_OBJECT_DUMP: std::sync::OnceLock<WaitObjectDumpFn> = std::sync::OnceLock::new();

pub fn install_wait_object_dump<F>(f: F)
where
    F: Fn(ObjectRef) + Send + Sync + 'static,
{
    let _ = WAIT_OBJECT_DUMP.set(Arc::new(f));
}

fn emit_wait_object_state(obj: ObjectRef) {
    if let Some(f) = WAIT_OBJECT_DUMP.get() {
        f(obj);
    }
}

/// Resolve the object a thread is parked on, through a handle the collector
/// FORWARDS.
///
/// `Monitor::wait`'s own `obj` parameter is a plain Rust local that survives
/// the whole wait and is never remapped, so under a moving collector it goes
/// stale the first time the object is evacuated. The thread registry's
/// `jmx_waiting_monitor` is scanned as a root and rewritten by
/// `update_thread_objs_after_gc`, so it is the address that is still correct
/// at dump time. Returns `None` when no resolver is installed (unit tests),
/// leaving the caller to fall back to its own local.
type WaitObjectResolveFn = Arc<dyn Fn(ThreadId) -> Option<ObjectRef> + Send + Sync>;
static WAIT_OBJECT_RESOLVE: std::sync::OnceLock<WaitObjectResolveFn> = std::sync::OnceLock::new();

pub fn install_wait_object_resolve<F>(f: F)
where
    F: Fn(ThreadId) -> Option<ObjectRef> + Send + Sync + 'static,
{
    let _ = WAIT_OBJECT_RESOLVE.set(Arc::new(f));
}

fn resolve_wait_object(thread_id: ThreadId) -> Option<ObjectRef> {
    WAIT_OBJECT_RESOLVE.get().and_then(|f| f(thread_id))
}

fn emit_wait_site_frames(thread_id: ThreadId) {
    if let Some(f) = WAIT_SITE_DUMP.get() {
        f(thread_id);
    } else {
        tracing::warn!(
            target: "kc16_watchdog",
            thread_id = ?thread_id,
            "Object.wait() observed stack_dump request but no dump callback installed"
        );
    }
}

// ---------------------------------------------------------------------------
// Thin-lock fast-path helpers
// ---------------------------------------------------------------------------
//
// These functions implement the lock-word state machine on the object header
// directly, avoiding any allocation in the uncontended case.
//
// State transitions (see `types::heap_types` for the mark word layout):
//
//   NEUTRAL --CAS--> THIN_LOCKED       (try_thin_lock)
//   THIN_LOCKED(self) --CAS--> THIN_LOCKED(self, recursion+1)
//                                       (try_thin_recursive_lock)
//   THIN_LOCKED(self) --CAS--> THIN_LOCKED(self, recursion-1) or NEUTRAL
//                                       (try_thin_unlock)
//   NEUTRAL / THIN_LOCKED --CAS--> INFLATED (monitor found by address)
//                                       (MonitorTable::inflate_locked)
//
// Compiled code performs the first and the recursion-0 half of the third
// transition inline (`runtime_lowering::emit_inline_thin_lock`), with the
// same `LeaseBlock::acquired` bookkeeping as `note_acquired` /
// `note_owner_released`.
//
// The inflation path is the only one that allocates a `Monitor`. It must
// publish the mark word via `compare_exchange` against the observed
// pre-inflation mark, *not* an unconditional store, to avoid clobbering a
// concurrent CAS (e.g. another thread releasing a thin lock back to NEUTRAL).
// `MonitorTable::inflate_locked` is the single place that does it, under the
// object's index shard lock, and it inserts the index entry in the same
// critical section: an INFLATED word whose entry a reader cannot find would
// hand that reader a fresh, unowned monitor. There is deliberately no
// unchecked variant: an unconditional store here corrupts the lock state
// machine.

/// Attempt thin-lock acquisition via a single CAS on the mark word.
///
/// `slot` is the calling thread's LOCK SLOT (see [`LockSlots`]), not its
/// `ThreadId`: the 8-byte object header leaves an 11-bit owner field.
///
/// Returns `Ok(())` on success (the calling thread is now the thin-lock owner
/// with recursion = 0). Returns `Err(current_mark)` if the CAS failed -- the
/// caller can inspect the current state to decide whether to retry, recurse,
/// or inflate.
#[inline]
pub fn try_thin_lock(header: &ObjectHeader, slot: u32) -> Result<(), u32> {
    // `thin_lockable` is "NEUTRAL, empty payload, and (compact instance) no
    // identity-hash bits": a hashed compact instance keeps its hash in this
    // word, so it cannot thin-lock and the caller inflates, displacing the
    // hash into the monitor -- HotSpot's rule. The quartet (kind / flags /
    // age) is ignored, as it must be: an aged or flagged object is still
    // unlocked.
    let cur = header.mark_word.load(Ordering::Relaxed);
    if !ObjectHeader::thin_lockable(cur) {
        return Err(cur);
    }
    header
        .mark_word
        .compare_exchange(
            cur,
            ObjectHeader::make_thin_locked(cur, slot, 0),
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .map(|_| ())
}

/// Re-entrant thin-lock: bump the recursion counter via CAS.
///
/// On success returns `Ok(new_recursion)`. Returns `Err(current_mark)` if:
/// - the object is not in `THIN_LOCKED` state, or
/// - it is thin-locked by a *different* lock slot, or
/// - the recursion counter is at [`types::MAX_THIN_LOCK_RECURSION`] (caller
///   must inflate to support deeper nesting).
#[inline]
pub fn try_thin_recursive_lock(header: &ObjectHeader, slot: u32) -> Result<u32, u32> {
    loop {
        let cur = header.mark_word.load(Ordering::Relaxed);
        if ObjectHeader::mark_state(cur) != types::MARK_THIN_LOCKED {
            return Err(cur);
        }
        if ObjectHeader::thin_lock_owner(cur) != slot {
            return Err(cur);
        }
        let recursion = ObjectHeader::thin_lock_recursion(cur);
        if recursion >= types::MAX_THIN_LOCK_RECURSION {
            return Err(cur); // overflow → must inflate
        }
        let new = ObjectHeader::make_thin_locked(cur, slot, recursion + 1);
        if header
            .mark_word
            .compare_exchange(cur, new, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(recursion + 1);
        }
        // Another thread mutated the mark word (almost certainly because
        // it inflated the lock) — restart and re-classify.
    }
}

/// Symmetric counterpart to `try_thin_lock` / `try_thin_recursive_lock`:
/// drop one level of thin-lock ownership.
///
/// On success returns `Ok(Some(new_recursion))` if recursion remains > 0,
/// or `Ok(None)` if the lock was fully released (mark word now NEUTRAL).
///
/// Returns `Err(current_mark)` if the mark word is not `THIN_LOCKED` by the
/// calling slot (caller must dispatch to the inflated path or raise
/// `IllegalMonitorStateException`).
#[inline]
pub fn try_thin_unlock(header: &ObjectHeader, slot: u32) -> Result<Option<u32>, u32> {
    loop {
        let cur = header.mark_word.load(Ordering::Relaxed);
        if ObjectHeader::mark_state(cur) != types::MARK_THIN_LOCKED {
            return Err(cur);
        }
        if ObjectHeader::thin_lock_owner(cur) != slot {
            return Err(cur);
        }
        let recursion = ObjectHeader::thin_lock_recursion(cur);
        let (new, ret) = if recursion == 0 {
            // Last release → NEUTRAL, CARRYING THE QUARTET. A bare
            // `MARK_NEUTRAL` would erase `kind` / `element_type` / `gc_age` /
            // `gc_flags`, and losing `GC_FLAG_COMPACT` turns a compact object
            // into a "legacy" one whose every later field read strides the
            // wrong layout (the 2026-08-07 `FileChannelImpl.fileLockTable`
            // NPE; `probes/MonitorQuartetProbe.java`).
            (ObjectHeader::make_neutral(cur), None)
        } else {
            (
                ObjectHeader::make_thin_locked(cur, slot, recursion - 1),
                Some(recursion - 1),
            )
        };
        if header
            .mark_word
            .compare_exchange(cur, new, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(ret);
        }
        // Lost the CAS — re-evaluate (an inflation by a concurrent thread
        // is the only realistic cause since we, the owner, are the only one
        // who can legally change a THIN_LOCKED mark word otherwise).
    }
}

// ---------------------------------------------------------------------------
// Lock slots — the thin-lock owner field
// ---------------------------------------------------------------------------

/// Number of thin-lock owner slots a table can lease (the 11-bit field).
const LOCK_SLOT_COUNT: usize = types::MAX_THIN_LOCK_SLOT as usize + 1;

/// One lock slot's LESSEE-owned words: its thin-lock count and the one-entry
/// inflated-monitor cache its compiled code reads. One cache line per slot, so
/// two lessees never share a line (the wave-15 `held_index` spread, which this
/// replaces, bought the same for the count alone).
///
/// Compiled code reaches the block through `JitMonitorBlock::held`
/// (`JitThinLease::held` is this block's address), so its layout is pinned
/// against `cratonvm_jit::runtime_lowering::LEASE_BLOCK_*_OFFSET` below; the
/// emitter is `runtime_lowering::emit_inline_thin_lock`.
///
/// # Single writer (round 11 wave 17, lane lock, proposal W15-1)
///
/// Every field but `owner_word` is written by the LESSEE ONLY: the helpers
/// acting for a `ThreadId` run on that thread (the interpreter, the JIT
/// helpers, deopt relocking and JNI all pass the calling thread's own id --
/// JNI's unnamed callers get an id of their own per OS thread,
/// `jni::unnamed_caller_monitor_owner`), and the compiled code that reads and
/// bumps it runs on that thread too. A virtual thread moving between carriers
/// hands its state over through the scheduler's own synchronization. That is
/// what lets compiled code keep `acquired` with a plain `ADD` / `SUB` (no
/// `LOCK`: one locked RMW fewer on each side of every compiled `synchronized`
/// block) and read the inflated cache's four words without a seqlock. A
/// contender that inflates a thin lock uncounts it in [`LockSlots::stolen`]
/// instead, never here.
///
/// `owner_word` (and the cache's reset to empty) is written when the slot is
/// leased, under `LockSlots::by_tid`, while no lessee exists (a slot is given
/// back only by its dead lessee's `release`, under the same lock), before the
/// new lessee can arm compiled code with it.
#[repr(C, align(64))]
struct LeaseBlock {
    /// Outermost thin acquisitions (NEUTRAL -> THIN_LOCKED) the lessee has
    /// made under this slot, and minus the final releases it made: +1 per
    /// acquisition, -1 per outermost release, from the helpers and from
    /// compiled code alike. Wrapping; the live count is
    /// [`LockSlots::live_thin_locks`].
    acquired: AtomicU32,
    _pad: u32,
    /// The lessee's [`owner_word`]: what an inflated monitor it owns holds in
    /// [`Monitor::owner`]. `0` while the slot is free.
    owner_word: AtomicU64,
    /// Object address of the cached inflated monitor; `0` = empty. Written
    /// LAST when the cache is filled, so the three words below are whole
    /// whenever compiled code on the lessee finds its receiver here.
    inflated_key: AtomicU64,
    /// `*const Monitor` of `index[inflated_key]` as of `inflated_epoch`.
    inflated_monitor: AtomicU64,
    /// The `MonitorTable::index_epoch` the entry was valid at. Compiled code
    /// compares it with `*epoch_addr` before trusting `inflated_monitor`,
    /// exactly as [`MonitorTable::cached_inflated_monitor`] does.
    inflated_epoch: AtomicU64,
    /// Address of the table's `index_epoch` (boxed, so it is stable).
    epoch_addr: AtomicU64,
    /// The cache's SECOND way (round 13 wave 10, lane monitor2, lock
    /// proposal W17-3): the entry `inflated_key` named before the latest
    /// fill, kept only when it was valid at the same `inflated_epoch` (the
    /// one epoch covers both ways) and named another object; `0` = empty.
    /// A thread alternating between two inflated monitors (nested
    /// `synchronized` on two hashed or once-contended objects, a
    /// `static synchronized` call on an inflated mirror inside an instance
    /// one) found the other one's entry here on every operation and took the
    /// helper -- a shard lock and an `Arc` round trip -- for each. Compiled
    /// code reads it only under `CRATONVM_JIT_INLINE_INFLATED_TWO_WAY`
    /// (`cratonvm_jit::runtime_lowering::inline_inflated_two_way_enabled`).
    /// Written like `inflated_key`: key cleared first, set last.
    inflated_key2: AtomicU64,
    /// `*const Monitor` of `index[inflated_key2]` as of `inflated_epoch`.
    inflated_monitor2: AtomicU64,
}

const _: () = {
    use cratonvm_jit::runtime_lowering as rl;
    assert!(std::mem::offset_of!(LeaseBlock, acquired) == rl::LEASE_BLOCK_ACQUIRED_OFFSET);
    assert!(std::mem::offset_of!(LeaseBlock, owner_word) == rl::LEASE_BLOCK_OWNER_WORD_OFFSET);
    assert!(std::mem::offset_of!(LeaseBlock, inflated_key) == rl::LEASE_BLOCK_KEY_OFFSET);
    assert!(std::mem::offset_of!(LeaseBlock, inflated_monitor) == rl::LEASE_BLOCK_MONITOR_OFFSET);
    assert!(std::mem::offset_of!(LeaseBlock, inflated_epoch) == rl::LEASE_BLOCK_EPOCH_OFFSET);
    assert!(std::mem::offset_of!(LeaseBlock, epoch_addr) == rl::LEASE_BLOCK_EPOCH_ADDR_OFFSET);
    assert!(std::mem::offset_of!(LeaseBlock, inflated_key2) == rl::LEASE_BLOCK_KEY2_OFFSET);
    assert!(
        std::mem::offset_of!(LeaseBlock, inflated_monitor2) == rl::LEASE_BLOCK_MONITOR2_OFFSET
    );
    assert!(std::mem::size_of::<LeaseBlock>() == 64);
    assert!(std::mem::size_of::<usize>() == 8);
};

impl LeaseBlock {
    fn new() -> Self {
        Self {
            acquired: AtomicU32::new(0),
            _pad: 0,
            owner_word: AtomicU64::new(0),
            inflated_key: AtomicU64::new(0),
            inflated_monitor: AtomicU64::new(0),
            inflated_epoch: AtomicU64::new(0),
            epoch_addr: AtomicU64::new(0),
            inflated_key2: AtomicU64::new(0),
            inflated_monitor2: AtomicU64::new(0),
        }
    }

    /// Lessee only: `acquired += delta` as a load and a store. The lessee is
    /// the only writer (see the type), and compiled code on the same thread
    /// does the same with a plain `ADD` / `SUB`.
    #[inline]
    fn bump_acquired(&self, delta: u32) {
        let now = self.acquired.load(Ordering::Relaxed);
        self.acquired.store(now.wrapping_add(delta), Ordering::Relaxed);
    }
}

/// What compiled code needs to thin-lock inline on behalf of one thread
/// (`MonitorTable::jit_thin_lease`, armed into `JitMonitorBlock`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JitThinLease {
    /// The low mark-word bits of a thin lock this thread holds at recursion
    /// 0: `MARK_THIN_LOCKED | slot << THIN_LOCK_OWNER_SHIFT` -- what
    /// `ObjectHeader::make_thin_locked(0, slot, 0)` answers. The quartet is
    /// ORed in from the observed word by the emitted sequence.
    pub(crate) owner_bits: u32,
    /// Address of the slot's [`LeaseBlock`], whose first word is the
    /// lessee's `acquired` count (`AtomicU32`) compiled code adds to and
    /// subtracts from.
    pub(crate) held: usize,
}

/// The thin-lock owner leases.
///
/// Since the 8-byte object header the thin-lock owner field is 11 bits, so it
/// cannot hold a `ThreadId` (a never-recycled `u64` counter; virtual threads
/// take one each). Instead a thread LEASES a small slot the first time it
/// thin-locks, and gives it back when it dies -- provided it holds no thin
/// lock at that moment. A thread that dies holding one (a leaked JNI
/// `MonitorEnter`, or a thread torn down inside a native call made from a
/// `synchronized` region) keeps its slot for as long as any such lock is
/// still counted under it, so a later thread can never be mistaken for the
/// owner of a stale thin lock -- the guarantee the never-recycled `ThreadId`
/// used to give. The contender that inflates the last of them gives the slot
/// back ([`MonitorTable::reclaim_dead_thread_lease`]).
///
/// The live count ([`Self::live_thin_locks`]) is exact because every
/// transition keeps it: +1 for each NEUTRAL -> THIN_LOCKED (the helpers'
/// `note_acquired`, compiled code's inline `ADD` through
/// `JitMonitorBlock::held`) and -1 for the owner's final release (from either
/// side), both in the lessee's [`LeaseBlock::acquired`]; and -1 for an
/// inflation, counted by the inflating thread (anyone) as +1 in
/// [`Self::stolen`]. Recursion changes no count. Until round 11 wave 17 it was
/// one counter every side RMWed atomically, which cost compiled code a `LOCK
/// ADD` and a `LOCK SUB` per `synchronized` block.
///
/// A thread that cannot get a slot (all of them leased) simply never
/// thin-locks: every `monitorenter` it performs inflates.
pub(crate) struct LockSlots {
    /// `ThreadId + 1` of the slot's holder; 0 = free.
    owner: Box<[AtomicU64]>,
    /// The lessee-owned words of each slot, one cache line apiece.
    leases: Box<[LeaseBlock]>,
    /// Thin locks under each slot that a `Monitor` took over by inflation
    /// ([`MonitorTable::inflate_locked`], on any thread). Wrapping, like
    /// [`LeaseBlock::acquired`]; cold.
    stolen: Box<[AtomicU32]>,
    /// `ThreadId -> slot` for leased slots. Cold: consulted only on a
    /// thread-local cache miss.
    by_tid: Mutex<FxHashMap<u64, u32>>,
    /// Identity for the thread-local cache (never an address).
    table_id: u64,
    /// Bumped by every lease granted and every lease given back, under
    /// `by_tid`. A cached REFUSAL ([`NO_SLOT`] in [`LOCK_SLOT_CACHE`]) is
    /// valid only while this is unchanged: a slot freed since may now be
    /// leased, and a lease granted since -- to this very `ThreadId` on another
    /// carrier, for a virtual thread -- would make "no slot" a lie that sends
    /// its `monitorexit` of a thin lock to IMSE. Round 11 wave 16
    /// (`r11w15-lock-lease-exhaustion-and-carrier-cache-cliff`).
    lease_epoch: AtomicU64,
}

/// A cached [`LockSlots`] REFUSAL: every slot was leased when this thread
/// asked (never a slot number: slots are `0..LOCK_SLOT_COUNT`).
const NO_SLOT: u32 = u32::MAX;

/// Ways of [`LOCK_SLOT_CACHE`]. A carrier running a handful of virtual threads
/// (consecutive `ThreadId`s, direct-mapped by id) keeps one entry per thread.
const LOCK_SLOT_CACHE_WAYS: usize = 4;

/// An empty [`LOCK_SLOT_CACHE`] way: table id 0 is never issued.
const LOCK_SLOT_CACHE_EMPTY: (u64, u64, u32, u64) = (0, u64::MAX, 0, 0);

thread_local! {
    /// `(table_id, thread_id, slot, lease_epoch)` answers this OS thread
    /// resolved, direct-mapped by `ThreadId` (see [`lock_slot_way`]). `slot`
    /// is a lease (checked against `LockSlots::owner` on every hit, so a lease
    /// given back at thread death is never reused from a stale entry) or
    /// [`NO_SLOT`], a refusal valid while `lease_epoch` is unchanged. A
    /// `ThreadId` is never reused, so a stale entry for a dead thread is never
    /// consulted again.
    ///
    /// Round 11 wave 16: four ways instead of one (a carrier alternating
    /// between virtual threads, each with its own `ThreadId`, missed on every
    /// mount switch and took `by_tid`), and refusals are cached (a lease-less
    /// thread repeated the locked 2048-slot scan on every `monitorenter`).
    static LOCK_SLOT_CACHE: [std::cell::Cell<(u64, u64, u32, u64)>; LOCK_SLOT_CACHE_WAYS] =
        const {
            [
                std::cell::Cell::new(LOCK_SLOT_CACHE_EMPTY),
                std::cell::Cell::new(LOCK_SLOT_CACHE_EMPTY),
                std::cell::Cell::new(LOCK_SLOT_CACHE_EMPTY),
                std::cell::Cell::new(LOCK_SLOT_CACHE_EMPTY),
            ]
        };
}

/// The [`LOCK_SLOT_CACHE`] way `tid` maps to.
#[inline(always)]
fn lock_slot_way(tid: ThreadId) -> usize {
    (tid.0 % LOCK_SLOT_CACHE_WAYS as u64) as usize
}

static NEXT_LOCK_SLOTS_TABLE_ID: AtomicU64 = AtomicU64::new(1);

impl LockSlots {
    fn new() -> Self {
        Self {
            owner: (0..LOCK_SLOT_COUNT).map(|_| AtomicU64::new(0)).collect(),
            leases: (0..LOCK_SLOT_COUNT).map(|_| LeaseBlock::new()).collect(),
            stolen: (0..LOCK_SLOT_COUNT).map(|_| AtomicU32::new(0)).collect(),
            by_tid: Mutex::new(FxHashMap::default()),
            table_id: NEXT_LOCK_SLOTS_TABLE_ID.fetch_add(1, Ordering::Relaxed),
            lease_epoch: AtomicU64::new(1),
        }
    }

    /// This OS thread's cached answer for `tid`: `Some(Some(slot))` for a
    /// lease `tid` still holds, `Some(None)` for a refusal that no lease or
    /// release has overtaken, `None` for a miss.
    #[inline]
    fn cached_slot(&self, tid: ThreadId) -> Option<Option<u32>> {
        let (table, cached_tid, slot, epoch) =
            LOCK_SLOT_CACHE.with(|c| c[lock_slot_way(tid)].get());
        if table != self.table_id || cached_tid != tid.0 {
            return None;
        }
        if slot == NO_SLOT {
            return (epoch == self.lease_epoch.load(Ordering::Acquire)).then_some(None);
        }
        self.leased_to(slot, tid).then_some(Some(slot))
    }

    /// Record `tid`'s answer (`NO_SLOT` for a refusal decided at `epoch`).
    #[inline]
    fn cache_slot(&self, tid: ThreadId, slot: u32, epoch: u64) {
        LOCK_SLOT_CACHE.with(|c| c[lock_slot_way(tid)].set((self.table_id, tid.0, slot, epoch)));
    }

    /// The lock slot `tid` thin-locks under, leasing one on first use.
    /// `None` when every slot is leased.
    #[inline]
    fn slot_for(&self, tid: ThreadId) -> Option<u32> {
        if let Some(answer) = self.cached_slot(tid) {
            return answer;
        }
        let (answer, epoch) = self.lease(tid);
        self.cache_slot(tid, answer.unwrap_or(NO_SLOT), epoch);
        answer
    }

    /// The slot `tid` already holds, without leasing one.
    fn existing_slot(&self, tid: ThreadId) -> Option<u32> {
        if let Some(answer) = self.cached_slot(tid) {
            return answer;
        }
        // A miss that finds no lease is NOT cached: "has not leased yet" is
        // not a refusal, and caching it would stop `slot_for` from leasing.
        let slot = self.by_tid.lock().get(&tid.0).copied()?;
        self.cache_slot(tid, slot, 0);
        Some(slot)
    }

    /// Whether `slot` is currently leased to `tid` (the cache check: a lease
    /// given back at thread death must not be reused from a stale entry).
    #[inline]
    fn leased_to(&self, slot: u32, tid: ThreadId) -> bool {
        self.owner
            .get(slot as usize)
            .is_some_and(|o| o.load(Ordering::Relaxed) == tid.0.wrapping_add(1))
    }

    /// Lease `tid` a slot (or find the one it has), answering the lease
    /// epoch the decision was made at -- what a cached refusal is valid for.
    #[cold]
    fn lease(&self, tid: ThreadId) -> (Option<u32>, u64) {
        let mut by_tid = self.by_tid.lock();
        if let Some(&slot) = by_tid.get(&tid.0) {
            return (Some(slot), self.lease_epoch.load(Ordering::Acquire));
        }
        let Some(tag) = tid.0.checked_add(1) else {
            return (None, self.lease_epoch.load(Ordering::Acquire));
        };
        for (slot, owner) in self.owner.iter().enumerate() {
            // Every `owner` write happens under `by_tid`, which we hold, so a
            // plain load is exact: a full table is scanned with loads, not
            // 2048 failing CASes.
            if owner.load(Ordering::Relaxed) == 0
                && owner
                    .compare_exchange(0, tag, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                // The new lessee's owner word, and an empty inflated cache:
                // the previous lessee (if any) is dead and gave the slot back
                // under this same lock, so nothing else writes the block now,
                // and the lessee reads it only after this lease reached it.
                let block = &self.leases[slot];
                block.owner_word.store(tag, Ordering::Relaxed);
                block.inflated_key.store(0, Ordering::Relaxed);
                block.inflated_key2.store(0, Ordering::Relaxed);
                let slot = slot as u32;
                by_tid.insert(tid.0, slot);
                let epoch = self.lease_epoch.fetch_add(1, Ordering::AcqRel) + 1;
                return (Some(slot), epoch);
            }
        }
        (None, self.lease_epoch.load(Ordering::Acquire))
    }

    /// The thread a thin lock under `slot` belongs to.
    #[inline]
    fn owner_of(&self, slot: u32) -> Option<ThreadId> {
        match self.owner.get(slot as usize)?.load(Ordering::Acquire) {
            0 => None,
            tag => Some(ThreadId(tag - 1)),
        }
    }

    /// `tid`'s lease as compiled code needs it, leasing a slot on first use;
    /// `None` when every slot is leased (such a thread never thin-locks).
    fn jit_lease(&self, tid: ThreadId) -> Option<JitThinLease> {
        let slot = self.slot_for(tid)?;
        Some(JitThinLease {
            owner_bits: ObjectHeader::make_thin_locked(0, slot, 0),
            held: self.lease_block(slot) as *const LeaseBlock as usize,
        })
    }

    /// Slot `slot`'s lessee-owned words (see [`LeaseBlock`]).
    #[inline(always)]
    fn lease_block(&self, slot: u32) -> &LeaseBlock {
        &self.leases[slot as usize % LOCK_SLOT_COUNT]
    }

    /// Every outermost thin acquisition (NEUTRAL -> THIN_LOCKED) adds one --
    /// here, and in compiled code's inline `monitorenter`, which does a plain
    /// `ADD` on the same word through `JitMonitorBlock::held`. LESSEE ONLY:
    /// `slot` is the calling thread's own lease (see [`LeaseBlock`]).
    #[inline]
    fn note_acquired(&self, slot: u32) {
        self.lease_block(slot).bump_acquired(1);
    }

    /// The owner's final release of a thin lock under `slot` takes one away --
    /// here, and in compiled code's inline `monitorexit`, a plain `SUB`.
    /// LESSEE ONLY, like [`Self::note_acquired`].
    #[inline]
    fn note_owner_released(&self, slot: u32) {
        self.lease_block(slot).bump_acquired(u32::MAX);
    }

    /// An inflation moved a thin lock under `slot` into a `Monitor`: any
    /// thread, so an atomic RMW on the cold [`Self::stolen`] word, never on
    /// the lessee's own. The live count can dip below zero (wrapping) for the
    /// instant between an owner's acquiring CAS and its increment when a
    /// contender inflates in between; it is exact again once both have run,
    /// and it is only consulted by [`Self::release`], once the owner is dead.
    #[inline]
    fn note_stolen(&self, slot: u32) {
        if let Some(stolen) = self.stolen.get(slot as usize) {
            stolen.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Thin locks currently counted under `slot`: the lessee's acquisitions
    /// net of its final releases, minus those inflation took over. Exact once
    /// the lessee has stopped running (`release` is reached only then: the
    /// dying thread itself, or a contender after `is_registered_dead`, whose
    /// Acquire read of the registry's liveness orders the lessee's last plain
    /// write before it).
    #[inline]
    fn live_thin_locks(&self, slot: u32) -> u32 {
        let acquired = self.lease_block(slot).acquired.load(Ordering::Acquire);
        let stolen = self
            .stolen
            .get(slot as usize)
            .map_or(0, |s| s.load(Ordering::Acquire));
        acquired.wrapping_sub(stolen)
    }

    /// Offer compiled code on `slot`'s lessee the inflated monitor
    /// `index[key]` as of `epoch` (see [`LeaseBlock::inflated_key`]).
    /// LESSEE ONLY. `key` is written last and cleared first, so a reader on
    /// the lessee that finds its receiver there sees the entry whole (it is
    /// the same thread; the order is for the argument, x86-TSO keeps it).
    #[inline]
    fn cache_inflated(&self, slot: u32, key: usize, monitor: usize, epoch: u64, epoch_addr: usize) {
        let block = self.lease_block(slot);
        let (key, monitor, epoch_addr) = (key as u64, monitor as u64, epoch_addr as u64);
        let prev_key = block.inflated_key.load(Ordering::Relaxed);
        let prev_monitor = block.inflated_monitor.load(Ordering::Relaxed);
        let prev_epoch = block.inflated_epoch.load(Ordering::Relaxed);
        if prev_key == key && prev_monitor == monitor && prev_epoch == epoch {
            return;
        }
        // Round 13 wave 10 (lane monitor2): the entry being replaced moves to
        // the second way when it is valid at the SAME epoch as the new one
        // (the block keeps one epoch for both) and names another object;
        // otherwise the second way is emptied. An entry keeps exactly the
        // `(key, monitor, epoch)` triple it was recorded with, so
        // `cached_inflated_monitor`'s argument covers both ways unchanged.
        block.inflated_key2.store(0, Ordering::Relaxed);
        if prev_key != 0
            && prev_key != key
            && prev_epoch == epoch
            && block.epoch_addr.load(Ordering::Relaxed) == epoch_addr
        {
            block.inflated_monitor2.store(prev_monitor, Ordering::Relaxed);
            block.inflated_key2.store(prev_key, Ordering::Release);
        }
        block.inflated_key.store(0, Ordering::Relaxed);
        block.epoch_addr.store(epoch_addr, Ordering::Relaxed);
        block.inflated_monitor.store(monitor, Ordering::Relaxed);
        block.inflated_epoch.store(epoch, Ordering::Relaxed);
        block.inflated_key.store(key, Ordering::Release);
    }

    /// Give `tid`'s slot back when it dies, unless it still holds a thin lock.
    fn release(&self, tid: ThreadId) {
        let mut by_tid = self.by_tid.lock();
        let Some(&slot) = by_tid.get(&tid.0) else {
            return;
        };
        if self.live_thin_locks(slot) != 0 {
            // Died holding a thin lock: keep the lease, so no later thread can
            // be taken for that lock's owner.
            return;
        }
        by_tid.remove(&tid.0);
        self.owner[slot as usize].store(0, Ordering::Release);
        // A slot is free again: every cached refusal is stale.
        self.lease_epoch.fetch_add(1, Ordering::AcqRel);
    }
}

// ---------------------------------------------------------------------------
// The address-keyed monitor index
// ---------------------------------------------------------------------------

/// One VM's inflated-monitor index: address -> `Monitor`, sharded.
type MonitorShards = [OrderedPlMutex<FxHashMap<usize, Arc<Monitor>>>];

/// Every live monitor index in the process, for the collector's
/// displaced-hash hook (a plain `fn(usize) -> i32`, so it cannot capture a
/// table). Object addresses are unique across the process's heaps, so asking
/// each index in turn is exact.
static LIVE_MONITOR_INDEXES: parking_lot::RwLock<Vec<std::sync::Weak<MonitorShards>>> =
    parking_lot::RwLock::new(Vec::new());

/// Resolve the identity hash of a compact instance whose mark word is not
/// NEUTRAL, by reading the hash displaced into its `Monitor` at inflation.
///
/// Installed into the `gc` crate at VM start-up (`set_displaced_hash_resolver`)
/// because `gc` owns the `identity_hash_code` accessors but cannot name
/// `Monitor`.
///
/// Answers `0` for an object that is not INFLATED: a THIN_LOCKED compact
/// instance has never been hashed (a hashed one cannot thin-lock), and an
/// index entry at a thin-locked object's address would be a dead object's.
pub fn displaced_hash_at(addr: usize) -> i32 {
    if addr == 0 || addr & 7 != 0 {
        return 0;
    }
    // SAFETY: the collector hands this the base of a live object.
    let mark = unsafe {
        (*(addr as *const ObjectHeader))
            .mark_word
            .load(Ordering::Acquire)
    };
    if ObjectHeader::mark_state(mark) != types::MARK_INFLATED {
        return 0;
    }
    let indexes = LIVE_MONITOR_INDEXES.read();
    for weak in indexes.iter() {
        if let Some(shards) = weak.upgrade() {
            if let Some(m) = shards[shard_of(addr)].lock().get(&addr) {
                return m.displaced_hash();
            }
        }
    }
    0
}

/// Look up an `&ObjectHeader` from an `ObjectRef`. Mirrors the pattern used by
/// the GC (`cratonvm_gc::heap::Heap::get_header`) — every heap allocation
/// begins with the 8-byte `ObjectHeader` word.
#[inline]
fn header_of(obj_ref: ObjectRef) -> &'static ObjectHeader {
    // SAFETY: `ObjectRef` is constructed only from live, properly aligned heap
    // allocations whose first bytes are an `ObjectHeader`. The lifetime is
    // bounded by the GC, which scans monitor state at safepoints.
    unsafe { &*(obj_ref.as_ptr() as *const ObjectHeader) }
}

// ---------------------------------------------------------------------------
// Monitor — per-object lock
// ---------------------------------------------------------------------------

/// A single JVM monitor (intrinsic lock).
///
/// Each monitor tracks its owner thread and re-entry count. Two condvars
/// separate the two kinds of blocking: monitor entry contention and
/// `Object.wait()`/`notify()`.
///
/// `#[repr(align(8))]` was load-bearing while the mark word packed the
/// monitor's address next to the state bits. Since the 8-byte header the
/// INFLATED word carries no address (the monitor is found through
/// [`MonitorTable`]'s address-keyed index); `ObjectHeader::make_inflated` no
/// longer takes a pointer. Round 11 wave 16 raised it to a cache line: the
/// monitor sits inside an `Arc`, and a 64-byte alignment keeps the `Arc`'s
/// reference counts (touched whenever a contender clones a handle to park)
/// off the line the owner word is CASed on.
#[repr(C, align(64))]
pub struct Monitor {
    /// The monitor's OWNER, and the one word that decides it: `0` means
    /// unowned, [`OWNER_RELEASING`] marks a dead owner's forced release in
    /// progress, anything else is the owner's `ThreadId + 1` ([`owner_word`]).
    ///
    /// Round 11 wave 16 (lane lock): acquiring a free monitor is one CAS on
    /// this word and releasing it is one store, as in HotSpot's
    /// `ObjectMonitor::_owner`. Until then the owner lived in `MonitorState`
    /// under the state mutex (next to a lock-free "advisory" copy for
    /// spinners), so every contended enter, every spinner's probe and every
    /// exit took that `parking_lot` mutex: with many contenders the mutex
    /// itself was the convoy -- a spinner's `try_enter` held it exactly when
    /// the owner needed it to exit, and a thread that lost the mutex race
    /// parked on it behind an OS wake-up
    /// (`r11w15-orch-contended-monitor-throughput-80x-hotspot`).
    ///
    /// Written only by (a) the CAS from `0` that acquires
    /// ([`Self::try_acquire_free`]), (b) the owner's own release
    /// ([`Self::release_owner_word`]), (c) [`Self::enter_with_recursion`]
    /// before the monitor is published, and (d)
    /// [`Self::force_release_if_owned_by`], which takes it from a DEAD owner
    /// through [`OWNER_RELEASING`]. A thread therefore reads its own
    /// `ThreadId + 1` here exactly while it owns the monitor, whatever the
    /// load's ordering. The acquiring CAS and the releasing store are `SeqCst`:
    /// that is Java's exit -> enter happens-before edge, and one half of the
    /// parking handshake with [`Self::entry_waiters`] (see
    /// [`Self::wake_successor`]).
    ///
    /// Since round 11 wave 17 compiled code performs (a) and (b) inline, at
    /// this field's pinned offset 0 (`LOCK CMPXCHG` from `0`, and an `XCHG`
    /// to `0` followed by `wake_successor`'s decision, both full barriers on
    /// x86): `cratonvm_jit::runtime_lowering::emit_inline_thin_lock`'s
    /// inflated arm. Every rule above holds for it; a release that owes a wake
    /// takes the monitor back and leaves the release to the helper.
    owner: AtomicU64,
    /// The owner this monitor INHERITED from a thin lock when it was inflated
    /// ([`Self::enter_with_recursion`]), in the [`owner_word`] encoding, for
    /// as long as that owner has held it continuously since; cleared the first
    /// time the monitor becomes unowned.
    ///
    /// It marks the one ownership nobody else can vouch for: a thin lock is
    /// invisible to `MonitorTable::release_monitors_held_by` (it walks inflated
    /// monitors only), so a thread that died holding one leaves an owner that
    /// only a later contender can discover — by inflating it.
    /// `vm_exec::release_dead_thin_owner` reads it at the contention point. A
    /// monitor that was inflated first and acquired afterwards never carries
    /// one, which is what keeps the thread-termination handshake (the dying
    /// thread holds its own, already-inflated `Thread` mirror monitor across
    /// `mark_dead`) out of that recovery.
    thin_seed: AtomicU64,
    /// Where `entry_count` lived until round 12 wave 8; kept as a hole so
    /// `jfr_enter_recorded` keeps its pinned offset (20).
    _entry_count_hole: [u8; 4],
    /// Round-7 HIGH (vm #5): JFR enter/exit event-pair consistency.
    ///
    /// JFR can be enabled or disabled at any moment (`cratonvm_jfr::is_enabled()`
    /// flips via a global AtomicBool). The interpreter's `Monitorenter` path
    /// snapshots `is_enabled()` *before* acquiring the lock and only emits the
    /// JFR monitor-enter event if that snapshot was true. The corresponding
    /// `Monitorexit` path would naturally re-check `is_enabled()` — but if JFR
    /// *enabled* between the enter snapshot and the exit, the exit-side check
    /// would emit an exit event for which no matching enter event was ever
    /// recorded (orphan exit), and downstream JFR consumers correlate enter
    /// and exit events by `(thread_id, monitor_addr)` so an orphan exit
    /// corrupts their per-monitor wait-time aggregation.
    ///
    /// To keep the enter/exit pair atomic with respect to JFR state, we stash
    /// the enter-time decision on the monitor itself: the interpreter sets
    /// this flag via [`Monitor::set_jfr_enter_recorded`] right after it emits
    /// the enter event, and the exit path consults
    /// [`Monitor::jfr_enter_recorded`] to decide whether to emit the
    /// matching exit event. Only the outermost reentrant enter records (so a
    /// nested re-acquire of the same monitor by the same thread doesn't
    /// produce a spurious paired event); when the entry count returns to 0 the
    /// flag is cleared so the next acquisition starts fresh. Owner-written,
    /// like [`Self::entry_count`].
    jfr_enter_recorded: std::sync::atomic::AtomicBool,
    /// The VM's monitor switches ([`MonitorTuning`]), copied in by the table
    /// that inflated this monitor before anyone could see it and never
    /// written again: read-only, so the owner line keeps no contender-written
    /// word (round 12 wave 8, lane monitor).
    tuning: MonitorTuning,
    /// Padding: the OWNER LINE (cache line 0) holds the owner word and the
    /// words only the owner writes and reads (`thin_seed`,
    /// `jfr_enter_recorded`; since round 12 wave 8 `entry_count` has a line
    /// of its own), which the owner reads and writes while it
    /// already holds the line from its acquiring CAS. Nothing a contender
    /// WRITES lives here: every spinner polls `owner`, so a contender's RMW on
    /// this line (wave 17 put `spinners` here: a `SeqCst` add and subtract per
    /// spin, two spins per contended enter) invalidated every other spinner's
    /// copy and took the line away from the owner's releasing `XCHG`. That
    /// layout is what regressed `ThreadChurn 192 20000 1` (48 contenders)
    /// ~25% against wave 16 with no kill switch able to remove it (round 11
    /// wave 19, lane lock; HotSpot's `ObjectMonitor` keeps `_owner` on a padded
    /// line of its own for the same reason).
    _owner_line_pad: [u8; OWNER_LINE_PAD],
    /// Threads registered to park on `entry_condvar`: a blocked
    /// `monitorenter` ([`Self::block_enter`]) and `Object.wait()` re-acquiring.
    /// Changed under the state lock (`SeqCst`), read lock-free by a releasing
    /// owner, which skips the state lock entirely when it reads `0`.
    ///
    /// Line 1, the HAND-OFF line (with `spinners`, `succ_pending` and
    /// `spin_limit`): the words contenders write, which a releasing owner
    /// reads once, after its release, for [`Self::wake_successor`]'s decision.
    entry_waiters: AtomicU32,
    /// Threads currently spinning for this monitor ([`SpinnerGuard`]). A
    /// spinner takes a released monitor without being woken, so a release
    /// while one spins wakes nobody -- HotSpot's spinner-as-successor rule.
    /// Since round 12 wave 2 only spinners that saw a parked entrant count
    /// ([`lazy_spinner_registration`]); compiled code's inline spin (the
    /// inflated arm of `emit_inline_thin_lock`) never counts.
    spinners: AtomicU32,
    /// A parked entrant has been woken and has not re-tried yet (HotSpot's
    /// `ObjectMonitor::_succ`). A release while one is on its way wakes
    /// nobody else. Waking one parked thread on EVERY release -- which then
    /// mostly lost the race to the running owner's next enter and parked
    /// again -- was one OS wake-up per contended release.
    succ_pending: std::sync::atomic::AtomicBool,
    /// Adaptive spin budget, in `spin_loop` iterations, for a contended entry
    /// on this monitor (HotSpot's `ObjectMonitor::_SpinDuration`). Grows when a
    /// spin wins the lock and halves when it runs out (since round 12 wave 8,
    /// by default, only when it runs out without having seen the monitor
    /// change hands: [`MonitorTuning::lost_cas_backoff`]), bounded by
    /// [`SPIN_MIN`]..=[`SPIN_MAX`]. Relaxed and racy on purpose: a lost update
    /// only mis-sizes the next spin.
    spin_limit: std::sync::atomic::AtomicU32,
    /// Padding: the state mutex and the condvars start on line 3 (line 2 is
    /// the entry count's), so a parker
    /// or a waker taking `state` disturbs neither the owner line nor the
    /// hand-off line (the layout assertions below the struct pin all three).
    _handoff_line_pad: [u8; 48],
    /// Re-entry count of the current owner. Written only by the owner (and by
    /// the forced release of a dead owner while the word reads
    /// [`OWNER_RELEASING`]), so relaxed accesses suffice: the ownership
    /// hand-over through [`Self::owner`] orders it. Exact while owned; while
    /// unowned it reads `0` (a Rust release, `wait`, a forced release) or,
    /// since round 13 wave 12 (lane monitor3), `1` -- compiled code's final
    /// release leaves it and its acquisition skips storing a `1` that is
    /// already there (`cratonvm_jit::runtime_lowering::
    /// inflated_sticky_count_enabled`), so a compiled hand-over does not move
    /// this line at all. A reader that does not own the monitor asks the
    /// owner word first ([`Self::is_idle`], [`Self::held_entry_count`]).
    ///
    /// On a line of its own, the OWNER-PRIVATE line (round 12 wave 8, lane
    /// monitor): the owner writes it twice per critical section (compiled
    /// code's inflated arm: `= 1` after the acquiring CAS, `= 0` before the
    /// releasing `XCHG`), and on the owner line, which every spinner polls,
    /// each of those stores was a read-for-ownership invalidating every
    /// spinner's copy inside the critical section. Here it moves with the
    /// ownership, one uncontended transfer per hand-over, off the critical
    /// path of the pollers.
    entry_count: AtomicU32,
    /// Alignment of the census words below.
    _count_pad: [u8; 4],
    /// Round 14 wave 1 (lane sync, proposal M2-1 of
    /// `jit-r13-monitor2-proposals-RETIRED-20260929.md`): what compiled code's INLINE spin
    /// (`cratonvm_jit::runtime_lowering` `emit_inline_inflated_spin_acquire`)
    /// ended in -- took the monitor, ran its budget out, gave way to a parked
    /// entrant. Bumped (`LOCK ADD`) only by sites compiled under
    /// `CRATONVM_DBG_JITC` (their census flag), so zero otherwise; drained into
    /// the process census by [`Self::drain_inline_spin_census`]. Here, on the
    /// entry count's line, because a census run perturbs what it counts
    /// anyway and this line's offsets are known to the JIT crate.
    census_inline_spin_wins: AtomicU64,
    /// See [`Self::census_inline_spin_wins`].
    census_inline_spin_budget_outs: AtomicU64,
    /// See [`Self::census_inline_spin_wins`].
    census_inline_spin_waiter_exits: AtomicU64,
    /// Padding: the state mutex starts on line 3.
    _count_line_pad: [u8; 32],
    state: Mutex<MonitorState>,
    /// Wakes threads blocked on `monitorenter` (waiting to acquire the lock).
    entry_condvar: Condvar,
    /// Wakes threads blocked on `Object.wait()`.
    wait_condvar: Condvar,
    /// VESTIGIAL since the 8-byte header (never set: an INFLATED mark word
    /// carries no monitor address, so it owns no reference; the index entry
    /// is the structural one). Formerly: true once this monitor's address had
    /// been published into some object's mark word, which from that moment
    /// **owned one strong `Arc` reference** (see the module-level rules).
    ///
    /// Read by the two reclaim sites to know (a) how many strong references
    /// are structural rather than "somebody is using this monitor", and (b)
    /// whether they still owe a `drop` of the mark-word reference. Cleared
    /// with a `swap`, so a monitor that is reached by both `prune_dead` and
    /// `remap_after_gc` releases exactly once.
    mark_ref: std::sync::atomic::AtomicBool,
    /// This object's identity hash, displaced here when the object inflated.
    ///
    /// A compact instance's hash lives in its `MARK_NEUTRAL` mark word (the
    /// payload plus the high bits, `ObjectHeader::short_hash_bits`). Inflation
    /// replaces the word with `make_inflated` (state + quartet, no hash), so
    /// it is the one transition that would destroy a hash --
    /// `MonitorTable::inflate_locked` moves it here first, and
    /// [`displaced_hash_at`] reads it back through the index. (A long-header
    /// object keeps its hash in the aux word and never displaces one.)
    ///
    /// A monitor is the right home for it rather than a separate address-keyed
    /// table: a displaced hash exists only for an inflated object, every
    /// inflated object has exactly one monitor, and this table is ALREADY
    /// re-keyed on relocation and pruned on death by `MonitorCleanup` -- which
    /// carries the "a new object at a recycled address inherits the dead one's
    /// entry" analysis that a fresh side table would have to repeat. Reaching
    /// it is one index probe (the monitor's own home since the 8-byte header).
    ///
    /// `0` means "none displaced": either the object was never hashed before it
    /// inflated, or it has not been hashed at all yet.
    displaced_hash: std::sync::atomic::AtomicI32,
    /// How many `Object.notify()` / `Object.notifyAll()` calls this monitor has
    /// SERVED, ever. Diagnostic only; carries no semantics.
    ///
    /// This is the counter that partitions the netty
    /// `ParameterizedSslHandlerTest` stall (see
    /// the netty `ParameterizedSslHandlerTest` stall, closed 2026-08-24 by
    /// `MonitorState::pending_notifies`).
    /// At the stall the promise is complete and the waiter is registered
    /// (`result != null`, `waiters == 1`), and the two remaining explanations
    /// need opposite fixes:
    ///
    /// * **zero notifies since the wait began** — the completer never called
    ///   `notifyAll()`. The defect is then above the monitor: either
    ///   `checkNotifyWaiters` read a stale `waiters == 0`, or `setValue0`'s
    ///   `compareAndSet` never reported the success it performed, so the branch
    ///   containing the call was not taken at all.
    /// * **one or more** — the notification WAS delivered to this monitor and
    ///   the waiter did not observe it. The defect is then the condvar
    ///   handshake in this file.
    ///
    /// One relaxed add under a lock the notifier already holds, and it answers
    /// that in ONE stall instead of a rate. `wake_all_for_interrupt` is counted
    /// separately (`interrupt_wakes`) because it is the VM answering
    /// `Thread.interrupt()`, not Java code signalling a condition — folding the
    /// two together would let an unrelated interrupt masquerade as the missing
    /// `notifyAll`.
    notify_calls: std::sync::atomic::AtomicU64,
    /// `wake_all_for_interrupt` calls — see [`Self::notify_calls`].
    interrupt_wakes: std::sync::atomic::AtomicU64,
}

// The words compiled code reads and writes on an INFLATED monitor
// (`cratonvm_jit::runtime_lowering::emit_inline_thin_lock`'s inflated arm,
// round 11 wave 17), at the offsets it bakes. `Arc::as_ptr` points at the
// `Monitor` itself, which `LeaseBlock::inflated_monitor` caches.
const _: () = {
    use cratonvm_jit::runtime_lowering as rl;
    assert!(std::mem::offset_of!(Monitor, owner) == rl::INFLATED_MONITOR_OWNER_OFFSET);
    assert!(std::mem::offset_of!(Monitor, thin_seed) == rl::INFLATED_MONITOR_THIN_SEED_OFFSET);
    assert!(std::mem::offset_of!(Monitor, entry_count) == rl::INFLATED_MONITOR_ENTRY_COUNT_OFFSET);
    assert!(
        std::mem::offset_of!(Monitor, entry_waiters) == rl::INFLATED_MONITOR_ENTRY_WAITERS_OFFSET
    );
    assert!(std::mem::offset_of!(Monitor, spinners) == rl::INFLATED_MONITOR_SPINNERS_OFFSET);
    assert!(
        std::mem::offset_of!(Monitor, jfr_enter_recorded) == rl::INFLATED_MONITOR_JFR_OFFSET
    );
    assert!(
        std::mem::offset_of!(Monitor, succ_pending) == rl::INFLATED_MONITOR_SUCC_PENDING_OFFSET
    );
    // Three lines (round 11 wave 19): the owner line (owner-written words
    // only), the hand-off line (contender-written words), then the state
    // mutex, the condvars and the cold diagnostics.
    assert!(std::mem::offset_of!(Monitor, jfr_enter_recorded) < 64);
    // Round 12 wave 8: the read-only switches ride in the owner line's pad
    // (four since round 13 wave 10).
    // (six since round 14 wave 3: M2-3's and M2-4's switches; nine since
    // round 14 wave 4: MON14-1's, MON14-2's and MON14-4's; ten since round 14
    // wave 6: M2-2's opt-in hand-over abort).
    assert!(std::mem::size_of::<MonitorTuning>() == 10);
    assert!(std::mem::offset_of!(Monitor, tuning) + std::mem::size_of::<MonitorTuning>() <= 64);
    assert!(std::mem::offset_of!(Monitor, entry_waiters) == 64);
    assert!(std::mem::offset_of!(Monitor, spinners) == 68);
    assert!(std::mem::offset_of!(Monitor, succ_pending) == 72);
    assert!(std::mem::offset_of!(Monitor, spin_limit) == 76);
    // Round 12 wave 8: the owner-private line (the entry count), then the
    // state mutex, the condvars and the cold diagnostics.
    assert!(std::mem::offset_of!(Monitor, entry_count) == 128);
    // Round 14 wave 1 (lane sync, M2-1): the inline spin's census words.
    assert!(
        std::mem::offset_of!(Monitor, census_inline_spin_wins)
            == rl::INFLATED_MONITOR_CENSUS_SPIN_WINS_OFFSET
    );
    assert!(
        std::mem::offset_of!(Monitor, census_inline_spin_budget_outs)
            == rl::INFLATED_MONITOR_CENSUS_SPIN_BUDGET_OUTS_OFFSET
    );
    assert!(
        std::mem::offset_of!(Monitor, census_inline_spin_waiter_exits)
            == rl::INFLATED_MONITOR_CENSUS_SPIN_WAITER_EXITS_OFFSET
    );
    assert!(std::mem::offset_of!(Monitor, state) == 192);
    // Round 12 wave 2 (lane lock2): compiled code's inline spin reads the
    // adaptive budget (a `u32`) and refuses one above the ceiling.
    assert!(std::mem::offset_of!(Monitor, spin_limit) == rl::INFLATED_MONITOR_SPIN_LIMIT_OFFSET);
    assert!(std::mem::size_of::<std::sync::atomic::AtomicU32>() == 4);
    assert!(SPIN_MAX == rl::INFLATED_MONITOR_SPIN_MAX);
    assert!(std::mem::size_of::<std::sync::atomic::AtomicBool>() == 1);
};

/// Contended-entry spin tuning. See [`Monitor::spin_limit`].
const SPIN_INITIAL: u32 = 128;
/// Floor of the adaptive budget: a monitor whose spins keep failing still
/// probes briefly, so it can learn that its critical sections got short again.
const SPIN_MIN: u32 = 16;
/// Ceiling of the adaptive budget. `spin_loop` is `PAUSE` on x86 (roughly 10 to
/// 140 cycles depending on the core), so this bounds one contended attempt to
/// tens of microseconds before the thread takes the GC-blocked park — the
/// thread is still COUNTED by the stop-the-world barrier while it spins, so the
/// bound is also a bound on how much it can delay a pause.
const SPIN_MAX: u32 = 1024;
/// Added to the budget after a spin that won the lock.
const SPIN_BONUS: u32 = 64;
/// How long a contender watches a THIN lock owned by another thread before it
/// inflates. Fixed (there is no `Monitor` yet to remember a history on), and
/// small: an inflation is permanent, but so is the cost of never inflating a
/// lock whose owner is parked.
const THIN_CONTENDED_SPINS: u32 = 256;

/// The [`Monitor::owner`] encoding of an owner: `0` for none, otherwise the
/// `ThreadId + 1` (so `ThreadId(0)`, the JNI placeholder, is not "free").
/// `ThreadId`s are a never-recycled counter far below `u64::MAX - 1`, so no
/// thread encodes as `0` or as [`OWNER_RELEASING`].
#[inline]
fn owner_word(owner: Option<ThreadId>) -> u64 {
    owner.map_or(0, |t| t.0.wrapping_add(1))
}

/// [`Monitor::owner`] while [`Monitor::force_release_if_owned_by`] takes a
/// monitor away from a dead owner: not free (no CAS from `0` can win), not a
/// thread. Only one forced release can move the word off the dead owner, so
/// only one resets its entry count.
const OWNER_RELEASING: u64 = u64::MAX;

/// Counts a thread in [`Monitor::spinners`] for as long as it spins on the
/// monitor.
///
/// A release that sees a spinner wakes no parked entrant, trusting the spinner
/// to take the monitor. If the spinner leaves with the monitor FREE (budget
/// out, or told to stop), that trust was misplaced, so the drop re-runs the
/// release's wake decision ([`Monitor::wake_successor`]). The decrement and the
/// owner load are `SeqCst`, after a releaser's `SeqCst` store and loads: either
/// the releaser saw no spinner (and woke), or the leaving spinner sees the
/// monitor free (and wakes), or it sees a newer owner, whose own release will
/// see this spinner gone.
///
/// Since round 12 wave 2 the count is taken lazily
/// ([`lazy_spinner_registration`]): a spinner registers only when it sees a
/// parked entrant ([`Self::register_if_parked`], at the start and once per
/// probe), and only a REGISTERED spinner decrements and re-runs the wake
/// decision on its way out. An unregistered one made no release skip a wake,
/// so it has none to hand on. Registering late is sound for the same reason
/// registering at all is: the count only ever suppresses wakes that a
/// registered spinner's drop then re-checks.
struct SpinnerGuard<'a> {
    monitor: &'a Monitor,
    registered: bool,
}

impl<'a> SpinnerGuard<'a> {
    #[inline]
    fn new(monitor: &'a Monitor) -> Self {
        let mut guard = Self {
            monitor,
            registered: false,
        };
        if lazy_spinner_registration() {
            guard.register_if_parked();
        } else {
            guard.register();
        }
        guard
    }

    /// Count this spinner in [`Monitor::spinners`] (once).
    #[inline]
    fn register(&mut self) {
        if !self.registered {
            self.monitor.spinners.fetch_add(1, Ordering::SeqCst);
            self.registered = true;
        }
    }

    /// [`Self::register`] once an entrant is parked on the monitor: only then
    /// does the count spare anyone a wake-up. The load is a hint (a parker
    /// that registers right after it is woken by the next release, exactly
    /// as if no spinner existed), and while nobody parks it reads a line that
    /// only a change of `spin_limit` writes.
    #[inline]
    fn register_if_parked(&mut self) {
        if !self.registered && self.monitor.entry_waiters.load(Ordering::Relaxed) != 0 {
            self.register();
        }
    }
}

impl Drop for SpinnerGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let monitor = self.monitor;
        monitor.spinners.fetch_sub(1, Ordering::SeqCst);
        if monitor.owner.load(Ordering::SeqCst) == 0 {
            monitor.wake_successor();
        }
    }
}

/// Whether a contended `monitorenter` may spin before inflating or parking.
///
/// Off when the machine has one CPU (the owner cannot run while we spin, so
/// every iteration is wasted) and when `CRATONVM_MONITOR_FASTPATH=0` restores
/// the pre-2026-09-08 monitor path — that flag is the A/B arm for the whole
/// monitor fast path, and spinning is part of it. Latched once: neither input
/// can change during a run.
///
/// Spinning is not observable: any interleaving in which a spinner wins the
/// lock is also an interleaving the park path allows (the owner released just
/// before the contender arrived), and Java monitors promise no fairness.
fn contended_spin_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        crate::threading::thread_registry::monitor_fastpath_enabled()
            && std::thread::available_parallelism().is_ok_and(|n| n.get() > 1)
    })
}

/// Watch a mark word that is THIN-locked by another thread for at most
/// [`THIN_CONTENDED_SPINS`] iterations. `true` once it is in any other state
/// (released, inflated, hashed), so the caller re-classifies it; `false` if the
/// owner kept it for the whole budget.
#[inline]
fn spin_while_thin_locked(header: &ObjectHeader) -> bool {
    for _ in 0..THIN_CONTENDED_SPINS {
        std::hint::spin_loop();
        let cur = header.mark_word.load(Ordering::Relaxed);
        if ObjectHeader::mark_state(cur) != types::MARK_THIN_LOCKED {
            return true;
        }
    }
    false
}

/// The mutable state protected by a monitor's mutex.
struct MonitorState {
    /// Threads currently parked in `Object.wait()` on this monitor.
    ///
    /// Incremented under the state lock immediately before a waiter parks and
    /// decremented immediately after it leaves the wait loop, so it is exact
    /// for anyone holding that lock. `notify_all` uses it to decide how many
    /// notifications to make available; `notify` uses it to avoid stockpiling
    /// notifications nobody is waiting for.
    parked_waiters: u32,
    /// Notifications delivered to this monitor that no waiter has consumed yet.
    ///
    /// **This is the fix for the netty `ParameterizedSslHandlerTest` stall, and
    /// the reason it existed.** `Object.wait()` here used to depend on the
    /// CONDVAR ALONE: park on `wait_condvar`, and treat a signalled return as
    /// the notification. A condvar is a signalling primitive, not a state one —
    /// a notification that is delivered while the waiter is between a
    /// `wait_for` timeout and its next park, or that is consumed by
    /// `parking_lot`'s requeue-to-mutex and then reported as a timeout because
    /// the 5 ms poll deadline passed before the waiter could re-acquire, is
    /// simply gone. The standard discipline is to pair the condvar with a
    /// CONDITION protected by the same mutex and re-test it on every wakeup,
    /// and that is what this counter is.
    ///
    /// MEASURED, one stall, on the instrumented binary:
    ///
    /// ```text
    /// [WAIT-OBJECT] class=…AbstractBootstrap$PendingRegistrationPromise
    ///               result_is=SUCCESS  waiters=Some(Int(1))
    /// [WAIT-OBJECT] notifies_since_wait=1 interrupt_wakes_since_wait=0
    ///               polls=78025 signalled=0 waited_ms=397123
    ///               (monitor totals: notify=1 interrupt=0)
    /// ```
    ///
    /// 397 123 ms over 78 025 polls is 5.09 ms each — the loop was spinning
    /// perfectly healthily — and **not one of those 78 025 `wait_for` returns
    /// was a signalled return**, while the monitor served a `notifyAll()`
    /// during that window. No `[WAIT-REACQUIRE]`, so the thread never left the
    /// wait loop; no `[MONITOR-ORPHAN]`, so the object still pointed at this
    /// monitor. The notification reached the exact condvar the thread was
    /// parked in and the thread never observed it.
    ///
    /// Consuming a notification is a decrement, not a flag, so `notify()` still
    /// releases exactly one waiter and `notifyAll()` exactly the ones parked
    /// when it ran — the JLS semantics, rather than the "wake everybody and let
    /// them re-check" approximation a bare generation counter would give.
    pending_notifies: u32,
    /// The wait set, in arrival order (interpreter round i1 wave 29,
    /// `i29-L4-notify-does-not-wake-the-longest-waiter`): each waiter's ticket
    /// from enrolment until a `notify` / `notifyAll` marks it or the waiter
    /// leaves on its own (timeout, interrupt). HotSpot's `ObjectMonitor`
    /// `WaitSet`: `notify` moves the thread that has waited longest.
    /// Since round 14 wave 3 each entry also carries the waiter's own condvar
    /// ([`WaitEntry::signal`]).
    wait_queue: std::collections::VecDeque<WaitEntry>,
    /// Tickets a `notify` / `notifyAll` marked and their waiters have not
    /// consumed yet; `pending_notifies` is their count. A credit is a
    /// waiter's own, so a thread that enrolls AFTER a `notify` can no longer
    /// take the notification meant for one that was in the wait set when it
    /// ran (JLS §17.2.2), which left that waiter parked: a lost wakeup.
    notified_tickets: Vec<u64>,
    /// The next wait ticket.
    next_wait_ticket: u64,
    // The owner, the re-entry count and the JFR flag were fields here until
    // round 11 wave 16; they are `Monitor::owner` / `entry_count` /
    // `jfr_enter_recorded` now, so a monitor is acquired and released without
    // this mutex (see `Monitor::owner`).
}

/// One member of a monitor's wait set ([`MonitorState::wait_queue`]).
struct WaitEntry {
    /// The waiter's ticket: only a notification that marks it ends the wait.
    ticket: u64,
    /// The condvar this waiter alone parks on
    /// ([`MonitorTuning::notify_one_waiter`], round 14 wave 3, M2-3), or
    /// `None` when it parks on the shared `Monitor::wait_condvar`. Signalled
    /// only under the state lock, and only while the entry is in the wait set
    /// (a notifier signals the entry it removes; an interrupt wake walks the
    /// set), so the waiter is either parked on it, or about to re-take the
    /// state lock and re-test its credit -- a signal that finds nobody parked
    /// is harmless, because the credit, not the signal, is the notification.
    signal: Option<Arc<Condvar>>,
    /// The waiting thread (round 14 wave 4, lane monitor2, MON14-1): what an
    /// interrupt wake aimed at one thread looks for
    /// ([`Monitor::wake_for_interrupt_of`]).
    waiter: ThreadId,
}

impl MonitorState {
    /// Enrol `waiter` at the tail of the wait set; its ticket. The waiter
    /// parks on `signal` if it has one (see [`WaitEntry::signal`]).
    fn enroll_waiter(&mut self, signal: Option<Arc<Condvar>>, waiter: ThreadId) -> u64 {
        let ticket = self.next_wait_ticket;
        self.next_wait_ticket = ticket.wrapping_add(1);
        self.wait_queue.push_back(WaitEntry {
            ticket,
            signal,
            waiter,
        });
        ticket
    }

    /// [`Self::enroll_waiter`] for a waiter on the shared condvar.
    #[cfg(test)]
    fn enroll_wait_ticket(&mut self) -> u64 {
        self.enroll_waiter(None, ThreadId(0))
    }

    /// `notify`: mark the waiter that has waited longest and signal its own
    /// condvar, if it parks on one. `None` when the wait set is empty (no
    /// credit is stockpiled for a future waiter); otherwise whether the marked
    /// waiter parks on the SHARED condvar, which the caller must then signal.
    fn notify_longest_waiter(&mut self) -> Option<bool> {
        let entry = self.wait_queue.pop_front()?;
        self.notified_tickets.push(entry.ticket);
        self.pending_notifies = self.pending_notifies.saturating_add(1);
        Some(match entry.signal {
            Some(own) => {
                own.notify_one();
                false
            }
            None => true,
        })
    }

    /// `notifyAll`: mark every waiter in the wait set now, signalling each
    /// one's own condvar (the caller signals the shared one); how many.
    fn notify_every_waiter(&mut self) -> u32 {
        let n = self.wait_queue.len();
        for entry in self.wait_queue.drain(..) {
            if let Some(own) = &entry.signal {
                own.notify_one();
            }
            self.notified_tickets.push(entry.ticket);
        }
        self.pending_notifies = self.pending_notifies.saturating_add(n as u32); // Cast: waiters on one monitor fit u32
        n as u32 // Cast: as above
    }

    /// Signal every waiter still in the wait set that parks on a condvar of
    /// its own (the interrupt wake; the caller signals the shared condvar).
    /// Marks nothing.
    fn signal_every_own_condvar(&self) {
        for entry in &self.wait_queue {
            if let Some(own) = &entry.signal {
                own.notify_one();
            }
        }
    }

    /// The interrupt wake aimed at `target` (MON14-1): signal the own condvar
    /// of each of `target`'s entries still in the wait set (one, for a live
    /// thread). Marks nothing. `(entries signalled, whether one of them parks
    /// on the SHARED condvar, which the caller must then signal)`; `(0,
    /// false)` when `target` is not in the wait set -- not yet enrolled (its
    /// enrolment re-check sees the flag, `MonitorTuning::wait_enrol_interrupt_check`),
    /// already marked by a notification (signalled then), or gone.
    fn signal_waiter_for_interrupt(&self, target: ThreadId) -> (u32, bool) {
        let mut signalled = 0u32;
        let mut shared = false;
        for entry in self.wait_queue.iter().filter(|e| e.waiter == target) {
            signalled = signalled.saturating_add(1);
            match &entry.signal {
                Some(own) => {
                    own.notify_one();
                }
                None => shared = true,
            }
        }
        (signalled, shared)
    }

    /// Consume `ticket`'s notification, if one marked it.
    fn take_notification(&mut self, ticket: u64) -> bool {
        match self.notified_tickets.iter().position(|&t| t == ticket) {
            Some(i) => {
                self.notified_tickets.swap_remove(i);
                self.pending_notifies = self.pending_notifies.saturating_sub(1);
                true
            }
            None => false,
        }
    }

    /// A waiter leaving unmarked (timeout, interrupt): out of the wait set.
    fn leave_wait_set(&mut self, ticket: u64) {
        if let Some(i) = self.wait_queue.iter().position(|e| e.ticket == ticket) {
            self.wait_queue.remove(i);
        }
    }
}

impl Monitor {
    /// Create a new, unlocked monitor with the default switches.
    pub(crate) fn new() -> Self {
        Self::with_tuning(MonitorTuning::DEFAULT)
    }

    /// Create a new, unlocked monitor governed by `tuning` (the inflating
    /// table's [`MonitorTable::tuning`]).
    fn with_tuning(tuning: MonitorTuning) -> Self {
        Self {
            tuning,
            state: Mutex::new(MonitorState {
                parked_waiters: 0,
                pending_notifies: 0,
                wait_queue: std::collections::VecDeque::new(),
                notified_tickets: Vec::new(),
                next_wait_ticket: 0,
            }),
            entry_condvar: Condvar::new(),
            wait_condvar: Condvar::new(),
            mark_ref: std::sync::atomic::AtomicBool::new(false),
            displaced_hash: std::sync::atomic::AtomicI32::new(0),
            notify_calls: std::sync::atomic::AtomicU64::new(0),
            interrupt_wakes: std::sync::atomic::AtomicU64::new(0),
            owner: AtomicU64::new(0),
            entry_count: AtomicU32::new(0),
            _entry_count_hole: [0; 4],
            _count_pad: [0; 4],
            census_inline_spin_wins: AtomicU64::new(0),
            census_inline_spin_budget_outs: AtomicU64::new(0),
            census_inline_spin_waiter_exits: AtomicU64::new(0),
            _count_line_pad: [0; 32],
            jfr_enter_recorded: std::sync::atomic::AtomicBool::new(false),
            entry_waiters: AtomicU32::new(0),
            spinners: AtomicU32::new(0),
            succ_pending: std::sync::atomic::AtomicBool::new(false),
            _owner_line_pad: [0; OWNER_LINE_PAD],
            spin_limit: std::sync::atomic::AtomicU32::new(SPIN_INITIAL),
            _handoff_line_pad: [0; 48],
            thin_seed: AtomicU64::new(0),
        }
    }

    /// The thin-lock owner this monitor was inflated from, if that thread has
    /// held it without a break ever since — see [`Self::thin_seed`].
    pub(crate) fn thin_seed_owner(&self) -> Option<ThreadId> {
        match self.thin_seed.load(Ordering::Acquire) {
            0 => None,
            encoded => Some(ThreadId(encoded - 1)),
        }
    }

    /// Give the monitor up: end any thin seed (see [`Self::thin_seed`]) and
    /// store `0` into [`Self::owner`]. The caller has already zeroed
    /// `entry_count` (and, for an outermost exit, the JFR flag); the `SeqCst`
    /// store publishes both to the next owner. Every caller then owes a
    /// successor wake ([`Self::wake_successor`] /
    /// [`Self::wake_successor_locked`]) or, for a forced release, a
    /// `notify_all`.
    #[inline]
    fn release_owner_word(&self) {
        // Round 12 wave 8 (`MonitorTuning::quiet_release`): a seed is only
        // ever set before the monitor is published (`enter_with_recursion`)
        // and only ever cleared here, so once this owner reads it clear it
        // stays clear, and the store would rewrite a zero on the line every
        // spinner is polling.
        if !self.tuning.quiet_release || self.thin_seed.load(Ordering::Relaxed) != 0 {
            self.thin_seed.store(0, Ordering::Release);
        }
        self.owner.store(0, Ordering::SeqCst);
    }

    /// Take a FREE monitor: CAS [`Self::owner`] from `0` to `me` (an
    /// [`owner_word`]) and start the entry count at 1. `false` if anyone owns
    /// it, including `me` (the caller has ruled re-entry out).
    #[inline]
    fn try_acquire_free(&self, me: u64) -> bool {
        if self
            .owner
            .compare_exchange(0, me, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            // Round 13 wave 12 (lane monitor3): a compiled release leaves the
            // count at 1 (`cratonvm_jit::runtime_lowering::
            // inflated_sticky_count_enabled`), so the store is skipped when it
            // would write the value already there -- as `store_spin_limit`
            // skips its: same value, no RFO of the count's line from the
            // previous owner. Owner-only word, read after the acquiring CAS.
            if self.entry_count.load(Ordering::Relaxed) != 1 {
                self.entry_count.store(1, Ordering::Relaxed);
            }
            true
        } else {
            false
        }
    }

    /// Whether a release (the owner word already `0`) must wake a parked
    /// entrant: somebody is parked, no woken entrant is still on its way, and
    /// no spinner will take the monitor instead.
    #[inline]
    fn successor_wake_needed(&self) -> bool {
        self.entry_waiters.load(Ordering::SeqCst) != 0
            && !self.succ_pending.load(Ordering::SeqCst)
            && self.spinners.load(Ordering::SeqCst) == 0
    }

    /// After a release: wake ONE parked entrant if [`Self::successor_wake_needed`].
    ///
    /// No lost wake-up. A parker registers in `entry_waiters` and only then
    /// CASes the owner word, both under the state lock and both `SeqCst`; a
    /// releaser stores `0` and only then reads `entry_waiters`, `succ_pending`
    /// and `spinners`, all `SeqCst`. So either the parker's CAS sees the
    /// release, or the releaser sees the parker -- and then takes the state
    /// lock, which the parker holds until it is inside `entry_condvar.wait`.
    /// A release that skips the wake hands the duty on rather than dropping
    /// it: a woken entrant clears `succ_pending` BEFORE it re-tries its CAS
    /// (under the state lock), so an owner it loses to reads the flag clear on
    /// its own release; and a spinner that gives up with the monitor free
    /// wakes on its way out ([`SpinnerGuard`]).
    ///
    /// The common contended release -- nobody parked, or a successor already
    /// on its way -- takes no lock at all.
    #[inline]
    fn wake_successor(&self) {
        if !self.successor_wake_needed() {
            return;
        }
        let state = self.state.lock();
        self.wake_successor_locked(&state);
    }

    /// [`Self::wake_successor`] for a caller that already holds the state
    /// lock (`Object.wait()`'s release).
    #[inline]
    fn wake_successor_locked(&self, _state: &parking_lot::MutexGuard<'_, MonitorState>) {
        if self.successor_wake_needed() && self.entry_condvar.notify_one() {
            self.succ_pending.store(true, Ordering::SeqCst);
            note_contention(ContentionEvent::SuccessorWake);
        }
    }

    /// Bounded adaptive spin for a contended entry: watch [`Self::owner`] and
    /// try to take the monitor the moment it reads unowned. `true` means the
    /// monitor is now held by `thread_id` (exactly as a successful
    /// [`Self::try_enter`]); `false` means the budget ran out and the caller must
    /// take the GC-blocked park path, as it did before spinning existed.
    ///
    /// The caller must NOT be GC-blocked: a spinner stays counted by the
    /// stop-the-world barrier, which is why the budget is bounded by
    /// [`SPIN_MAX`]. While it spins with an entrant parked it counts in
    /// [`Self::spinners`] ([`SpinnerGuard`]), so a release does not wake a
    /// parked thread it would only race.
    pub(crate) fn spin_try_enter_adaptive(&self, thread_id: ThreadId) -> bool {
        self.adaptive_spin(thread_id, self.spin_aborts_on_handover())
    }

    /// Whether this monitor's Rust contended spins give up at a hand-over
    /// ([`MonitorTuning::spin_handover_abort`], only with the lazy park on).
    #[inline]
    fn spin_aborts_on_handover(&self) -> bool {
        self.tuning.spin_handover_abort && monitor_lazy_park_budget().is_some()
    }

    /// The body of [`Self::spin_try_enter_adaptive`]; `abort_on_handover`
    /// is [`Self::spin_aborts_on_handover`] (a parameter so a unit test can
    /// run the abort without the process-wide lazy-park budget).
    fn adaptive_spin(&self, thread_id: ThreadId, abort_on_handover: bool) -> bool {
        // Round 14 wave 1 (lane sync, M2-1): a compiled spin that gave up
        // lands here (through the helper), so this is where its census words
        // reach the process census.
        self.drain_inline_spin_census();
        if self.spinner_cap_reached() {
            return false;
        }
        let raw = self.spin_limit.load(Ordering::Relaxed);
        let limit = raw.clamp(SPIN_MIN, SPIN_MAX);
        let mut spinning = SpinnerGuard::new(self);
        let mut backoff = LostCasBackoff::new(self.tuning.lost_cas_backoff);
        // Round 14 wave 6 (lane monitor3): fed only for the abort or the
        // census, so the default spin polls exactly as before.
        let watching = abort_on_handover || contention_census_on();
        let mut watch = HandoverWatch::new();
        // `spent` counts `spin_loop` hints, the back-off's included, so the
        // spin stays within `limit` hints however many races it loses.
        let mut spent: u32 = 0;
        while spent < limit {
            std::hint::spin_loop();
            spent += 1;
            let owner = self.owner.load(Ordering::Relaxed);
            if owner == 0 {
                if self.try_enter(thread_id) {
                    self.store_spin_limit(raw, limit.saturating_add(SPIN_BONUS).min(SPIN_MAX));
                    note_contention(ContentionEvent::AdaptiveSpinWin);
                    watch.finish(true);
                    return true;
                }
                // Seen free, taken by someone else first.
                watch.lost_race();
                if abort_on_handover {
                    // Counted as a loss (census, and no budget halving
                    // below), with no back-off window: the spin ends here.
                    backoff.after_loss(0);
                    note_contention(ContentionEvent::SpinHandoverAbort);
                    break;
                }
                spent = spent.saturating_add(backoff.after_loss(limit - spent));
            } else if watching && watch.owner_polled(owner) && abort_on_handover {
                note_contention(ContentionEvent::SpinHandoverAbort);
                break;
            }
            spinning.register_if_parked();
        }
        // A spin that lost races watched the monitor change hands: its
        // critical sections are short, the crowd is the problem, and a halved
        // budget would only send more of the crowd to the park. Only a spin
        // that never saw a release shrinks it (as HotSpot's `TrySpin`). With
        // the abort on, an owner change it watched counts as such a release.
        let crowded = backoff.saw_a_handover() || (abort_on_handover && watch.seen);
        if !crowded {
            self.store_spin_limit(raw, (limit / 2).max(SPIN_MIN));
        }
        note_contention(ContentionEvent::AdaptiveSpinFail);
        watch.finish(false);
        false
    }

    /// Round 14 wave 1 (lane sync, proposal M2-1): move the inline spin's
    /// census words ([`Self::census_inline_spin_wins`] and its two siblings,
    /// bumped by compiled code) into the process census, so
    /// `CRATONVM_DBG_MONITOR_CONTENTION`'s exit line reports
    /// `inline_spin_wins` / `inline_spin_budget_outs` /
    /// `inline_spin_waiter_exits` beside the helper's `adaptive_spin_*`.
    /// Called from the helper's spin, where compiled budget-outs and waiter
    /// exits arrive while the monitor is still owned; a word that no later
    /// helper spin on this monitor drains is not reported (an undercount,
    /// mostly of the wins of a monitor that never saw the helper again,
    /// which is the uncontended end of the census anyway). One latched
    /// load and a branch with the census off; the words are then zero anyway.
    fn drain_inline_spin_census(&self) {
        if !contention_census_on() {
            return;
        }
        for (word, event) in [
            (&self.census_inline_spin_wins, ContentionEvent::InlineSpinWin),
            (&self.census_inline_spin_budget_outs, ContentionEvent::InlineSpinBudgetOut),
            (&self.census_inline_spin_waiter_exits, ContentionEvent::InlineSpinWaiterExit),
        ] {
            if word.load(Ordering::Relaxed) != 0 {
                note_contention_n(event, word.swap(0, Ordering::Relaxed));
            }
        }
    }

    /// `spin_limit = new` unless it already reads `new` (it last read `raw`).
    /// The budget saturates at [`SPIN_MAX`] on a monitor whose spins keep
    /// winning, and an unconditional store after every win dirtied the
    /// hand-off line inside the critical section just won, where the owner's
    /// release reads it next (round 12 wave 2, lane lock2). Same values as
    /// before; only the redundant stores are gone.
    #[inline]
    fn store_spin_limit(&self, raw: u32, new: u32) {
        if new != raw {
            self.spin_limit.store(new, Ordering::Relaxed);
        }
    }

    /// Whether `CRATONVM_MONITOR_MAX_SPINNERS` ([`max_monitor_spinners`]) says
    /// a new contender must not spin on this monitor: that many threads
    /// already do. Skipping a spin is always sound (the caller parks, as it
    /// would after a failed spin).
    #[inline]
    fn spinner_cap_reached(&self) -> bool {
        let reached = max_monitor_spinners()
            .is_some_and(|cap| self.spinners.load(Ordering::Relaxed) >= cap);
        if reached {
            note_contention(ContentionEvent::SpinSkipped);
        }
        reached
    }

    /// Spin for the monitor right after a wake-up: `true` once `me` (an
    /// [`owner_word`]) owns it. Called with the state lock RELEASED and
    /// `succ_pending` still set by the release that woke this thread, so the
    /// releases meanwhile wake nobody else; the caller clears the flag after,
    /// whatever the outcome, before any CAS that could send it back to sleep
    /// -- the order [`Self::wake_successor`]'s lost-wake-up argument needs.
    /// Bounded by the monitor's adaptive budget, which it only reads.
    /// Round 12 wave 1 (lane lock, W16-3 / W19-3).
    fn spin_after_wake(&self, me: u64) -> bool {
        let won = self.spin_for_free(me);
        if won {
            note_contention(ContentionEvent::SpinAfterWakeWin);
        }
        won
    }

    /// The spin of [`Self::spin_after_wake`], uncounted: watch the owner
    /// word for up to the adaptive budget (which it only reads) and take the
    /// monitor for `me` the moment it reads free, backing off after a lost
    /// race. The caller must not hold the state lock, and must not count in
    /// [`Self::spinners`] unless it re-runs the wake decision on leaving.
    /// Round 13 wave 10 (lane monitor2) split it out for `Object.wait()`'s
    /// re-acquisition ([`MonitorTuning::wait_reacquire_spin`]).
    fn spin_for_free(&self, me: u64) -> bool {
        let limit = self
            .spin_limit
            .load(Ordering::Relaxed)
            .clamp(SPIN_MIN, SPIN_MAX);
        // Round 12 wave 8: the same lost-race back-off as
        // `spin_try_enter_adaptive`, inside the same bound.
        let mut backoff = LostCasBackoff::new(self.tuning.lost_cas_backoff);
        let mut spent: u32 = 0;
        while spent < limit {
            std::hint::spin_loop();
            spent += 1;
            if self.owner.load(Ordering::Relaxed) == 0 {
                if self.try_acquire_free(me) {
                    return true;
                }
                spent = spent.saturating_add(backoff.after_loss(limit - spent));
            }
        }
        false
    }

    /// The `(notify + notifyAll, interrupt-wake)` totals this monitor has
    /// served. Diagnostic only — see [`Self::notify_calls`].
    #[inline]
    pub(crate) fn notify_totals(&self) -> (u64, u64) {
        (
            self.notify_calls.load(Ordering::Relaxed),
            self.interrupt_wakes.load(Ordering::Relaxed),
        )
    }

    /// The identity hash displaced into this monitor, or `0` if none.
    #[inline]
    pub fn displaced_hash(&self) -> i32 {
        self.displaced_hash.load(Ordering::Acquire)
    }

    /// Install `hash` as this object's identity hash if none is recorded yet,
    /// and return the hash that is now in force.
    ///
    /// Idempotent and racy-safe: the first writer wins and every caller --
    /// including the losers -- converges on that one value. An object's
    /// identity hash may never change once observed, so a plain store would be
    /// wrong even though it looks equivalent.
    #[inline]
    pub fn displace_hash(&self, hash: i32) -> i32 {
        match self
            .displaced_hash
            .compare_exchange(0, hash, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => hash,
            Err(existing) => existing,
        }
    }

    /// How many strong references to this monitor are *structural* rather than
    /// "somebody is actively using it": always 1 for the enumeration index
    /// entry, plus 1 more once the mark word owns a reference.
    ///
    /// The reclaim predicate compares `Arc::strong_count` against this to
    /// decide whether any thread is currently parked on / owns the monitor
    /// through a clone of its own.
    #[inline]
    fn structural_refs(&self) -> usize {
        1 + usize::from(self.mark_ref.load(Ordering::Acquire))
    }

    /// Release the strong reference an object's `MARK_INFLATED` mark word owns
    /// on `monitor`, if it still owns one.
    ///
    /// # Safety
    ///
    /// The caller must have proved that the object whose mark word points at
    /// `monitor` is **dead** — no thread can load that mark word again. See the
    /// module-level lifetime rules; the two legitimate callers are
    /// [`MonitorTable::prune_dead`] (exact swept-address set) and the reclaim
    /// branch of [`MonitorTable::remap_after_gc`].
    ///
    /// This is *not* the last drop under a waiter: any thread parked on the
    /// monitor holds its own `Arc` clone. The `swap` makes a second call a
    /// no-op, so overlapping reclaim paths cannot double-free.
    unsafe fn release_mark_ref(monitor: &Arc<Monitor>) {
        if monitor.mark_ref.swap(false, Ordering::AcqRel) {
            // SAFETY: pairs with the `mem::forget` in
            // `MonitorTable::publish_inflated`, which leaked exactly one
            // strong reference through this same pointer.
            drop(unsafe { Arc::from_raw(Arc::as_ptr(monitor)) });
        }
    }

    /// Pre-acquire this monitor for `thread_id` at the given entry count.
    ///
    /// Used exclusively by the inflation handoff
    /// (`MonitorTable::inflate_locked`) to atomically transfer ownership
    /// from the thin-lock representation to a freshly created `Monitor`.
    /// The monitor must be brand new (no other thread can observe it yet
    /// because the mark word still points at the thin state), so this is
    /// a pure local initialization — no condvar signalling needed.
    pub(crate) fn enter_with_recursion(&self, thread_id: ThreadId, entry_count: u32) {
        debug_assert!(entry_count >= 1, "entry_count must be at least 1");
        debug_assert_eq!(
            self.owner.load(Ordering::Relaxed),
            0,
            "Monitor must be fresh"
        );
        let word = owner_word(Some(thread_id));
        self.entry_count.store(entry_count, Ordering::Relaxed);
        self.owner.store(word, Ordering::Relaxed);
        // The only production caller is the thin-lock inflation handoff, so
        // this owner is one the monitor inherited rather than one that entered
        // it — see `thin_seed`. Published before the object's word turns
        // INFLATED and the index names the monitor (`inflate_locked`'s AcqRel
        // CAS and shard insert), like the rest of it.
        self.thin_seed.store(word, Ordering::Release);
    }

    /// Returns true if this monitor is currently owned by the given
    /// thread. Non-blocking inspection — used by `Thread.holdsLock`.
    fn is_held_by(&self, thread_id: ThreadId) -> bool {
        self.owner.load(Ordering::Acquire) == owner_word(Some(thread_id))
    }

    /// Non-blocking inspection of the current owner, if any.
    ///
    /// The dead-owner check at the contention point
    /// (`vm_exec::release_dead_thin_owner`) deliberately does NOT use this: any
    /// owner that is dead qualifies here, including a thread in its own
    /// termination handshake, while only an owner inherited from a thin lock
    /// ([`Self::thin_seed_owner`]) is one the death-time sweep
    /// (`MonitorTable::release_monitors_held_by`, which walks inflated
    /// monitors only) cannot have released.
    pub(crate) fn current_owner(&self) -> Option<ThreadId> {
        match self.owner.load(Ordering::Acquire) {
            0 | OWNER_RELEASING => None,
            word => Some(ThreadId(word - 1)),
        }
    }

    /// Returns true iff this monitor is fully idle — unowned and with a zero
    /// entry count. A monitor that is held, re-entered, or in the middle of a
    /// `wait()` (which temporarily clears `owner` but keeps a non-zero
    /// `saved_count` *only on the stack*, and is represented to the index by
    /// a live extra `Arc` clone held by the blocked thread) reports `false`.
    ///
    /// This is one half of the *reclaim predicate*:
    /// `Arc::strong_count(m) == m.structural_refs() && m.is_idle()` — "nobody
    /// owns it, nobody is parked on it, and nothing but its structural holders
    /// references it".
    ///
    /// It is **not** used by [`MonitorTable::prune_dead`]: an exact
    /// swept-address set already proves the object is unreachable, which is
    /// strictly stronger. [`MonitorTable::prune_stale_after_gc`] (the
    /// Generational and G1 backends, whose collections hand over no dead set)
    /// requires it on top of its liveness verdict
    /// ([`Monitor::reclaimable_index_entry`]). Idleness alone is never a
    /// licence to free.
    ///
    /// Round 13 wave 12 (lane monitor3): the owner word alone decides. The
    /// count is exact only while the monitor is owned -- a compiled release
    /// leaves it at 1 ([`Self::entry_count`]) -- and every Rust path that
    /// clears the owner word stores the count 0 before it (release, `wait`,
    /// forced release), so on those paths `owner == 0` already implied a
    /// zero count and the old conjunction answered exactly this.
    #[inline]
    fn is_idle(&self) -> bool {
        self.owner.load(Ordering::Acquire) == 0
    }

    /// The owner's entry count, or 0 when the monitor is unowned (the count
    /// of an unowned monitor may read 1, see [`Self::entry_count`]). For
    /// readers that do not own the monitor.
    fn held_entry_count(&self) -> u32 {
        match self.owner.load(Ordering::Acquire) {
            0 | OWNER_RELEASING => 0,
            _ => self.entry_count.load(Ordering::Acquire),
        }
    }

    /// The reclaim predicate above, plus no registered entrant and no
    /// spinner: an index entry for which this holds is referenced by nothing
    /// but the index. [`MonitorTable::prune_stale_after_gc`] drops only such
    /// entries, so a verdict that were ever wrong about a live object could
    /// cost it at most an idle monitor (and a displaced hash), never a held
    /// lock or a parked thread. Round 12 wave 2 (lane lock).
    fn reclaimable_index_entry(monitor: &Arc<Monitor>) -> bool {
        Arc::strong_count(monitor) == monitor.structural_refs()
            && monitor.is_idle()
            && monitor.entry_waiters.load(Ordering::SeqCst) == 0
            && monitor.spinners.load(Ordering::SeqCst) == 0
    }

    /// Acquire this monitor for the given thread.
    ///
    /// If the monitor is unowned, the thread becomes the owner.
    /// If the monitor is already owned by this thread, the entry count is
    /// incremented (reentrant lock).
    /// If the monitor is owned by another thread, this blocks until the
    /// monitor is released.
    fn enter(&self, thread_id: ThreadId) {
        self.enter_labeled(thread_id, None)
    }

    fn enter_labeled(&self, thread_id: ThreadId, dbg_label: Option<&str>) {
        // Free, or already ours: no lock, no registration.
        if self.try_enter(thread_id) {
            return;
        }
        let me = owner_word(Some(thread_id));
        // Round 14 wave 6 (lane monitor3): how long a contended entry waits
        // here, census only (no clock read otherwise).
        let census_since = contention_census_on().then(std::time::Instant::now);
        let mut state = self.state.lock();
        // Register as a parked entrant BEFORE the CAS that decides whether to
        // park: that order, against a releaser's store-then-read, is what makes
        // a release either visible to the CAS or aware of this thread (see
        // `wake_successor`). The state lock is held from the CAS until
        // `entry_condvar.wait` releases it, so a releaser that saw us cannot
        // notify before we are waiting.
        self.entry_waiters.fetch_add(1, Ordering::SeqCst);
        if mon_enter_dump_enabled() {
            // Gated diagnostic only (CRATONVM_DBG_MONENTER): poll on a 5ms
            // cadence so a watchdog stack-dump request can surface a thread
            // deadlocked here. Emits the blocked thread's frames once (the
            // snapshot was deposited by `monitor_enter` before this call).
            let mut dumped = false;
            let mut spins: u64 = 0;
            while !self.try_acquire_free(me) {
                self.entry_condvar
                    .wait_for(&mut state, std::time::Duration::from_millis(5));
                // Whoever woke us (or the timeout), this entrant re-tries now,
                // so it is no longer a successor on its way.
                self.succ_pending.store(false, Ordering::SeqCst);
                if !dumped && stack_dump_wait_flag().load(std::sync::atomic::Ordering::Acquire) {
                    emit_wait_site_frames(thread_id);
                    dumped = true;
                }
                // CRATONVM_DBG_MONENTER stall attribution (2026-07-21): a
                // waiter stuck for whole seconds names the current owner so
                // wedge captures identify the holder directly.
                spins += 1;
                if spins % 2000 == 0 {
                    eprintln!(
                        "[monenter-stall] waiter_tid={} monitor={:p} owner={:?} entry_count={} waited_ms={} obj={}",
                        thread_id.0,
                        self as *const _,
                        self.current_owner().map(|o| o.0),
                        self.entry_count.load(Ordering::Relaxed),
                        spins * 5,
                        dbg_label.unwrap_or("?")
                    );
                }
            }
        } else {
            // Block on the condvar until a release hands us the wake-up (or a
            // CAS wins without one).
            while !self.try_acquire_free(me) {
                note_contention(ContentionEvent::CondvarWait);
                self.entry_condvar.wait(&mut state);
                // Round 12 wave 1: spin for it first, the state lock released
                // and `succ_pending` still set, so the owner's releases in the
                // meantime wake nobody else (see `spin_after_wake`).
                let won = spin_after_wake_enabled()
                    && parking_lot::MutexGuard::unlocked(&mut state, || self.spin_after_wake(me));
                self.succ_pending.store(false, Ordering::SeqCst);
                if won {
                    break;
                }
            }
        }
        self.entry_waiters.fetch_sub(1, Ordering::SeqCst);
        if let Some(since) = census_since {
            note_entrant_wait(since.elapsed());
        }
    }

    /// Non-blocking acquire: `true` ⇒ acquired (fresh or re-entrant),
    /// `false` ⇒ owned by another thread (the caller must take the
    /// GC-blocked contended path — see `MonitorTable::enter_or_contend`).
    ///
    /// Lock-free: one load, then a CAS only when the word reads free
    /// (test-and-test-and-set, so a crowd of spinners does not hammer the
    /// line with failing CASes).
    #[inline]
    fn try_enter(&self, thread_id: ThreadId) -> bool {
        let me = owner_word(Some(thread_id));
        match self.owner.load(Ordering::Relaxed) {
            // Re-entry: only the owner writes the count.
            cur if cur == me => {
                let count = self.entry_count.load(Ordering::Relaxed);
                self.entry_count.store(count + 1, Ordering::Relaxed);
                true
            }
            0 => self.try_acquire_free(me),
            _ => false,
        }
    }

    /// Bounded spin on a CONTENDED monitor before the caller parks: `true` ⇒
    /// acquired, `false` ⇒ still owned by another thread, or `keep_spinning`
    /// said stop. Never blocks; at most [`ENTER_SPIN_ROUNDS`] attempts.
    ///
    /// # Why (gen r4w5/thrash5, 2026-09-24)
    ///
    /// The contended path (`vm_exec::monitor_enter_blocking`) retires the
    /// thread's TLAB before it parks, because a parked thread must not hold a
    /// buffer into a young arena a collection may reset, nor write a filler
    /// after its blocked flag is published. That is right for a real park, and
    /// ruinous for a short critical section: every hand-off of a hot lock
    /// buried the rest of a 256 KiB TLAB under a filler, which the young
    /// trigger counts as occupancy. Two threads alternating on one
    /// `synchronized` block made about one hand-off per carve, and a young
    /// cycle every 13 hand-offs: `GenR4W4HeapFullThrashProbe` step `threads`
    /// ran 1 183 `moving+major` cycles in 120 s with young live flat, each
    /// cycle freeing exactly 13 carves of filler
    /// (`docs/internal/gc/gengc-r4w4-final-two-thread-heap-full-thrash-FIXED-20260924.md`).
    /// HotSpot spins adaptively before it parks for the same reason, and keeps
    /// its TLAB either way.
    ///
    /// `keep_spinning` is polled before every attempt. The caller passes "no
    /// stop-the-world request is pending": a spinning thread is a RUNNING
    /// mutator that the pause must wait for, so it has to leave the spin and
    /// take the GC-blocked path as soon as a pause is requested (the owner may
    /// be the initiator, waiting for everyone else).
    pub(crate) fn spin_try_enter(
        &self,
        thread_id: ThreadId,
        mut keep_spinning: impl FnMut() -> bool,
    ) -> bool {
        if self.spinner_cap_reached() {
            return false;
        }
        // Counted as a spinner once anyone is parked (a release wakes no
        // parked thread meanwhile); the guard wakes one on the way out if it
        // leaves the monitor free.
        let mut spinning = SpinnerGuard::new(self);
        // Round 14 wave 6 (lane monitor3): the hand-over watch of
        // `adaptive_spin`, fed only for the abort or the census.
        let abort_on_handover = self.spin_aborts_on_handover();
        let watching = abort_on_handover || contention_census_on();
        let mut watch = HandoverWatch::new();
        for round in 0..ENTER_SPIN_ROUNDS {
            spinning.register_if_parked();
            if !keep_spinning() {
                note_contention(ContentionEvent::EnterSpinFail);
                watch.finish(false);
                return false;
            }
            if round < ENTER_SPIN_BUSY_ROUNDS {
                for _ in 0..enter_spin_hints(round) {
                    std::hint::spin_loop();
                }
            } else {
                std::thread::yield_now();
            }
            let before = if watching {
                self.owner.load(Ordering::Relaxed)
            } else {
                0
            };
            if self.try_enter(thread_id) {
                note_contention(ContentionEvent::EnterSpinWin);
                watch.finish(true);
                return true;
            }
            if watching {
                // Read free just before a failed attempt: lost to another
                // thread. Read held: compare with the previous held read.
                let handed_over = if before == 0 {
                    watch.lost_race();
                    true
                } else {
                    watch.owner_polled(before)
                };
                if handed_over && abort_on_handover {
                    note_contention(ContentionEvent::SpinHandoverAbort);
                    note_contention(ContentionEvent::EnterSpinFail);
                    watch.finish(false);
                    return false;
                }
            }
        }
        note_contention(ContentionEvent::EnterSpinFail);
        watch.finish(false);
        false
    }

    /// Blocking acquire of a CONTENDED monitor handed out by
    /// `MonitorTable::enter_or_contend`. The caller MUST have marked itself
    /// GC-blocked first (deposit roots + `GcBarrier::enter_blocked`): the
    /// current owner may be parked at a GC safepoint waiting for
    /// `gc_complete`, so a contender that still counts in the barrier's
    /// `expected` wedges the whole VM (the H2 TestScript three-way STW
    /// deadlock: owner waits GC, contender waits owner, GC waits contender).
    pub(crate) fn block_enter(&self, thread_id: ThreadId) {
        self.enter_labeled(thread_id, None);
    }

    /// `block_enter` with a debug label naming the contested object
    /// (class name / identity), shown in the CRATONVM_DBG_MONENTER stall
    /// print so wedge captures identify WHAT is being fought over, not
    /// just which Monitor struct.
    pub(crate) fn block_enter_labeled(&self, thread_id: ThreadId, label: Option<&str>) {
        self.enter_labeled(thread_id, label);
    }

    /// Park for a CONTENDED monitor for at most `budget` WITHOUT the
    /// GC-blocked protocol: `true` ⇒ acquired, `false` ⇒ the budget ran out
    /// (the caller then takes the full GC-blocked park, as before).
    ///
    /// Round 11 wave 19 (lane lock, proposal W17-1, opt-in first step; see
    /// [`monitor_lazy_park_budget`]). The full park
    /// (`vm_exec::monitor_enter_blocking`) retires the TLAB, deposits a fresh
    /// root snapshot -- a conservative scan of the thread's compiled frames,
    /// the frame-slot origins, a frame trace -- raises the blocked flag, and on
    /// wake runs a second (no-flag) deposit; per park that is far more than
    /// the hand-over it waits for. A thread parked HERE is registered in
    /// [`Self::entry_waiters`] exactly like a full parker (same queue, same
    /// successor wake-up from a release), but it still counts as a RUNNING
    /// mutator: a stop-the-world pause cannot complete while it is parked, so
    /// nothing it holds can move -- the same argument that makes the spins in
    /// front of this sound -- and a pause requested meanwhile waits at most
    /// `budget` (plus the host's timer slack) for it to give up and arrive
    /// through the full path. That delay is the price, and why the budget is
    /// bounded and the arm opt-in.
    ///
    /// No lost wake-up: the registration, the CAS and the wait are
    /// [`Self::enter_labeled`]'s, under the state lock. A timed-out wait was
    /// NOT notified (`parking_lot` reports a notify that raced the timeout,
    /// including a requeue onto the state mutex, as a wake), so leaving
    /// consumes no other entrant's wake-up; and the deregistration happens
    /// under the state lock, so a release that counted this thread either
    /// notified it before it timed out or takes the lock after it left and
    /// wakes someone else (or nobody, when nobody is parked). The full path
    /// that follows re-registers and re-tries its CAS before it waits.
    ///
    /// The budget is a DEADLINE for a timed condvar wait, so on Windows it is
    /// rounded up to the system clock tick (15.625 ms by default: see
    /// `jvm_thread::win_park`): `=200` and `=1000` both park up to ~15.6 ms,
    /// and a pause requested meanwhile waits that long. That is why the arm
    /// cannot default on until a pause wakes running parkers (proposal W19-1,
    /// `r12w1-lock-stw-initiator-wakes-lazy-monitor-parkers-patch`).
    ///
    /// `keep_parking` is re-asked (under the state lock) before every wait,
    /// the first included: `false` gives up at once without taking the
    /// monitor. The VM passes "no stop-the-world pause is requested", so an
    /// entrant woken by a release it then loses does not go back to sleep in
    /// front of a pending pause (round 12 wave 1; until then this was
    /// `park_enter_for`, which re-parked unconditionally). A woken entrant
    /// spins before it re-tries ([`Self::spin_after_wake`]).
    pub(crate) fn park_enter_while(
        &self,
        thread_id: ThreadId,
        budget: std::time::Duration,
        keep_parking: impl Fn() -> bool,
    ) -> bool {
        if self.try_enter(thread_id) {
            return true;
        }
        let me = owner_word(Some(thread_id));
        let deadline = std::time::Instant::now() + budget;
        let mut state = self.state.lock();
        self.entry_waiters.fetch_add(1, Ordering::SeqCst);
        let mut acquired = self.try_acquire_free(me);
        while !acquired && keep_parking() {
            note_contention(ContentionEvent::CondvarWait);
            if self
                .entry_condvar
                .wait_until(&mut state, deadline)
                .timed_out()
            {
                break;
            }
            // Woken (or requeued) by a release: spin for it first, as in
            // `enter_labeled` -- unless the wake was a pause's
            // (`MonitorTable::wake_lazy_parkers`) -- then no longer a
            // successor on its way.
            acquired = spin_after_wake_enabled()
                && keep_parking()
                && parking_lot::MutexGuard::unlocked(&mut state, || self.spin_after_wake(me));
            self.succ_pending.store(false, Ordering::SeqCst);
            if !acquired {
                acquired = self.try_acquire_free(me);
            }
        }
        self.entry_waiters.fetch_sub(1, Ordering::SeqCst);
        note_contention(if acquired {
            ContentionEvent::LazyParkWin
        } else {
            ContentionEvent::LazyParkGiveUp
        });
        acquired
    }

    /// Release this monitor for the given thread.
    ///
    /// Decrements the entry count. When it reaches 0, the monitor is released
    /// and becomes unowned.
    ///
    /// Returns `Err` if the calling thread does not own the monitor
    /// (`IllegalMonitorStateException`).
    pub(crate) fn exit(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        self.exit_reporting_release(thread_id).map(|_| ())
    }

    /// [`Self::exit`], also answering whether this release was the LAST one:
    /// `Ok(true)` when the entry count reached zero and `thread_id` no longer
    /// owns the monitor, `Ok(false)` when a re-entrant acquisition is still
    /// held. That is the same answer a `holds` re-check would give, taken from
    /// the transition itself.
    ///
    /// Lock-free unless a parked entrant has to be woken
    /// ([`Self::wake_successor`]).
    pub(crate) fn exit_reporting_release(&self, thread_id: ThreadId) -> Result<bool, MonitorError> {
        if self.owner.load(Ordering::Relaxed) != owner_word(Some(thread_id)) {
            return Err(MonitorError::NotOwner);
        }
        // B6: JVMS §6.5 monitorexit contract — if the entry count is already
        // zero at the moment of the exit attempt, the calling thread does NOT
        // logically own the monitor and must observe
        // `IllegalMonitorStateException`. Checked BEFORE the decrement so the
        // failure mode is the spec-defined IMSE rather than a silent wrap.
        let count = self.entry_count.load(Ordering::Relaxed);
        if count == 0 {
            return Err(MonitorError::NotOwner);
        }
        if count > 1 {
            self.entry_count.store(count - 1, Ordering::Relaxed);
            return Ok(false);
        }
        self.entry_count.store(0, Ordering::Relaxed);
        // Round-7 HIGH (vm #5): clear the JFR enter-event flag at the same
        // instant the monitor becomes unowned. The next acquirer (potentially
        // a different thread) starts with a fresh `jfr_enter_recorded = false`
        // and the interpreter's enter-time snapshot governs whether the next
        // enter/exit pair is JFR-tracked. Only the owner writes the flag, so
        // a clear read is final (round 12 wave 8, `quiet_release`: no store
        // of a value already there onto the polled owner line).
        if !self.tuning.quiet_release || self.jfr_enter_recorded.load(Ordering::Relaxed) {
            self.jfr_enter_recorded.store(false, Ordering::Relaxed);
        }
        self.release_owner_word();
        // Wake one thread waiting to enter this monitor, unless one is already
        // on its way or a spinner will take it.
        self.wake_successor();
        Ok(true)
    }

    /// Forcibly release this monitor if it is (still) owned by `thread_id`,
    /// regardless of entry count. Used only when `thread_id` has already
    /// terminated (`ThreadRegistry::mark_dead`) — a dead thread can never
    /// call `monitorexit` for a monitor it happened to be holding when it
    /// exited (e.g. interrupted out of a blocking native call while inside
    /// a `synchronized` block), so without this every future `monitorenter`
    /// on that object blocks forever on `entry_condvar`. Wakes ALL entry
    /// waiters (not just one, unlike a normal `exit`) since we don't know
    /// how many threads are parked and each must re-check for itself.
    /// Returns `true` if a release actually happened (diagnostic only).
    ///
    /// The owner word moves off `thread_id` through [`OWNER_RELEASING`] first,
    /// so of two racing forced releases only one resets the entry count, and
    /// no acquirer can slip in between the reset and the release (it would
    /// otherwise have its fresh count of 1 overwritten with 0).
    pub(crate) fn force_release_if_owned_by(&self, thread_id: ThreadId) -> bool {
        // Test before the CAS (round 11 wave 19): every thread death walks
        // EVERY inflated monitor through here (`release_monitors_held_by`),
        // and a `LOCK CMPXCHG` takes the owner line exclusive even when it
        // fails -- including a hot monitor's, from under the spinners polling
        // it. The dead owner's word got here by a write ordered before this
        // call (the dying thread's own, or an inflation published through the
        // index this monitor was found in), so a load that reads anything else
        // is a CAS that would have failed.
        if self.owner.load(Ordering::Acquire) != owner_word(Some(thread_id)) {
            return false;
        }
        if self
            .owner
            .compare_exchange(
                owner_word(Some(thread_id)),
                OWNER_RELEASING,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }
        self.entry_count.store(0, Ordering::Relaxed);
        self.jfr_enter_recorded.store(false, Ordering::Relaxed);
        self.release_owner_word();
        // Under the state lock, like every entry wake: a parker holds it from
        // its failed CAS until it waits, so none can miss this.
        let _state = self.state.lock();
        self.entry_condvar.notify_all();
        true
    }

    /// Round-7 HIGH (vm #5): record that the interpreter emitted a JFR
    /// `monitor_enter` event for the current outermost acquisition of this
    /// monitor. Called from the `Monitorenter` opcode handler right after the
    /// event is pushed to the flight recorder, so the matching `Monitorexit`
    /// can decide whether to emit an exit event without re-checking the
    /// (potentially-flipped-since) global JFR enable flag.
    ///
    /// No-op unless the caller currently owns the monitor — defensive against
    /// a stale snapshot in a racing interpreter thread.
    pub(crate) fn set_jfr_enter_recorded(&self, thread_id: ThreadId) {
        if self.owner.load(Ordering::Relaxed) == owner_word(Some(thread_id)) {
            self.jfr_enter_recorded.store(true, Ordering::Relaxed);
        }
    }

    /// Round-7 HIGH (vm #5): peek the JFR enter-recorded flag for the current
    /// owner. Returns `true` only if the interpreter actually emitted a
    /// matching `monitor_enter` event for the live acquisition — the natural
    /// gate for emitting a paired `monitor_exit` event without producing an
    /// orphan one when JFR turned on between the two opcodes.
    pub(crate) fn jfr_enter_recorded(&self) -> bool {
        self.jfr_enter_recorded.load(Ordering::Relaxed)
    }

    /// Object.wait() — release the monitor and block until notified.
    ///
    /// The calling thread must own this monitor. The entry count is saved,
    /// ownership is released, and the thread blocks on `wait_condvar`.
    /// When woken (by `notify`/`notifyAll`), it re-acquires the monitor
    /// with the original entry count restored.
    ///
    /// If `timeout_ms` is Some(millis) with millis > 0, the wait is bounded.
    ///
    /// If `interrupted` is provided, the wait periodically checks the flag
    /// and returns early if the thread has been interrupted (needed because
    /// Thread.interrupt() sets a flag but cannot directly wake a condvar).
    fn wait(
        &self,
        thread_id: ThreadId,
        timeout_ms: Option<u64>,
        interrupted: Option<&std::sync::atomic::AtomicBool>,
        // DIAGNOSTIC ONLY (netty promise stall): the object this monitor was
        // reached through, so the poll loop can re-read its mark word and prove
        // whether the waiter has been ORPHANED — parked on a monitor the object
        // no longer points at, so any later `notifyAll()` inflates a different
        // one and never reaches here. Carries no semantics.
        waited_on: Option<ObjectRef>,
        // The waiter's `Thread.getState()` word (`GcBlockState::java_state`),
        // set to BLOCKED (2) when the re-acquire after the wait has to queue
        // behind another owner, as HotSpot reports a notified waiter that is
        // re-entering a held monitor. `None` in unit tests.
        reacquire_state: Option<&std::sync::atomic::AtomicU8>,
    ) -> Result<WaitOutcome, MonitorError> {
        let me = owner_word(Some(thread_id));
        let mut state = self.state.lock();
        if self.owner.load(Ordering::Relaxed) != me {
            return Err(MonitorError::NotOwner);
        }

        // Save and release. The state lock stays held until this thread is
        // parked on `wait_condvar`, so a thread that takes the monitor the
        // moment it is released still cannot `notify` before this waiter has
        // enrolled below (`notify` needs the same lock).
        let saved_count = self.entry_count.load(Ordering::Relaxed);
        // The JFR enter flag is this OWNER's (round 12 wave 8,
        // `r12w8-monitor-wait-leaves-the-jfr-enter-flag-to-the-next-owner`):
        // saved and cleared with the count, so the next owner neither inherits
        // an enter it did not record nor clears the waiter's, and restored on
        // re-acquisition below. Owner-only writes, made while owning.
        let saved_jfr = self.jfr_enter_recorded.load(Ordering::Relaxed);
        self.entry_count.store(0, Ordering::Relaxed);
        if saved_jfr {
            self.jfr_enter_recorded.store(false, Ordering::Relaxed);
        }
        self.release_owner_word();
        self.wake_successor_locked(&state);

        // The notify/interrupt-wake totals AS OF THE MOMENT THIS WAIT BEGAN.
        // Taken under the state lock that a notifier must also hold, so the
        // snapshot cannot straddle one. The watchdog dump below reports the
        // DELTA, which is the whole question at a stall: a delta of 0 means no
        // `notifyAll()` ever reached this monitor after the waiter registered,
        // and a non-zero delta means one did and was not observed. See
        // `Monitor::notify_calls`.
        let (notifies_at_entry, interrupt_wakes_at_entry) = self.notify_totals();

        // ENROL as a waiter, under the same lock a notifier must take. From
        // here until the decrement below, a `notify` / `notifyAll` on this
        // monitor will leave a `pending_notifies` credit this thread can
        // consume, whether or not the condvar signal itself is observed. See
        // `MonitorState::pending_notifies` for the stall that made this
        // necessary.
        state.parked_waiters = state.parked_waiters.saturating_add(1);
        // This waiter's place in the wait set (interpreter round i1 wave 29):
        // only a notification that marks THIS ticket ends the wait.
        // Round 14 wave 3 (lane monitor, M2-3, `MonitorTuning::notify_one_waiter`):
        // with the credit on, park on a condvar of this waiter's own, which
        // its wait-set entry carries, so a `notify()` aimed at another waiter
        // does not wake this one. Allocated before the park, dropped after
        // the waiter has left the wait set (below), when no notifier can
        // reach it any more.
        let own_signal = (self.tuning.notify_one_waiter && monitor_pending_notify())
            .then(|| Arc::new(Condvar::new()));
        let signal: &Condvar = own_signal.as_deref().unwrap_or(&self.wait_condvar);
        let my_ticket = state.enroll_waiter(own_signal.clone(), thread_id);

        // Block on wait_condvar with periodic interrupt checks.
        // We use short timed waits so that Thread.interrupt() (which only sets
        // a flag) can wake us within a bounded interval.
        //
        // KC16-watchdog: also poll the global stack-dump flag so a thread
        // parked in Object.wait() (e.g. AsyncFutureTask.await ->
        // EnhancedQueueExecutor handoff) observes the watchdog's request
        // and dumps its frame chain from the wait site. Without this
        // poll, a thread that entered wait() before the watchdog fired
        // sits forever in `wait_condvar.wait()` and ack_count stays at
        // 0, leaving the watchdog's only signal as the misleading
        // "main thread is in native (Rust) code" banner.
        //
        // Important: observing the dump flag does NOT consume it and
        // does NOT cause `wait()` to return spuriously. After emitting
        // one frame snapshot per wait call we set a local "already
        // dumped" guard and re-park; this preserves Java semantics
        // (a notify is still required to return) while letting the
        // watchdog see at least one snapshot before it aborts.
        let poll_interval = WAIT_POLL_SLICE;
        let mut was_interrupted = false;
        // A `notify` / `notifyAll` credit was consumed: the wait ended by
        // notification (HotSpot's `WasNotified`), whatever the interrupt flag
        // says by now.
        let mut notified = false;
        // With the notification credit on (the default), a condvar signal that
        // leaves no credit behind is NOT a notification: it is another thread's
        // interrupt wake (`wake_all_for_interrupt`), or a credit another waiter
        // took first. Returning on it was a spurious wakeup HotSpot never
        // produces -- interrupting one waiter returned every other waiter on the
        // monitor from `wait()` (interpreter round i1 wave 29, lane L4, probe
        // `tools/probes/interp/L7/L7W29TWaitNotify.java`, row `interrupt w1`).
        // Only the `CRATONVM_MONITOR_PENDING_NOTIFY=0` A/B arm, which has no
        // credit to test, still treats a bare signal as the notification.
        let credits = monitor_pending_notify();
        // Round 14 wave 4 (lane monitor2, MON14-4, `MonitorTuning::wait_single_park`):
        // with every wake in place, park for the whole remaining timeout (at
        // most the safety slice) instead of polling.
        let parks_once = self.tuning.wait_parks_once(credits);
        // The wait loops' current park slice: 5 ms doubling to 100 ms after
        // each poll that ended nothing with `MonitorTuning::wait_poll_backoff`
        // (round 14 wave 3, lane monitor, M2-4); from 20 ms once the
        // enrolment re-check below closes the interrupt window, and the
        // safety slice throughout when the waiter parks once (round 14 wave
        // 4, MON14-2 / MON14-4).
        let mut slice = self
            .tuning
            .first_wait_slice(credits, stack_dump_wait_flag().load(Ordering::Acquire));
        // Round 14 wave 4 (lane monitor2, MON14-2,
        // `MonitorTuning::wait_enrol_interrupt_check`): an interrupt that
        // landed before this waiter was in the wait set found nobody to wake
        // (or no waiting monitor in the registry); its flag is visible here,
        // under the state lock the interrupt wake takes after setting it. The
        // wait ends at once, `Interrupted`, as its first poll would have
        // reported; nothing can have marked this ticket yet (this thread has
        // held the lock since it enrolled).
        // Round 14 wave 5 (RV5-1): the fence and the SeqCst load pair with the
        // interrupter's SeqCst flag store and its fence before it reads the
        // mark word (`MonitorTable::wake_waiter_for_interrupt`). An interrupter
        // that read this object's mark THIN (before `ensure_inflated`'s CAS)
        // wakes nobody, and only this pairing -- not Release/Acquire --
        // guarantees that this read then sees its flag.
        let interrupted_at_enrol = self.tuning.wait_enrol_interrupt_check
            && interrupted.is_some_and(|flag| {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                flag.load(std::sync::atomic::Ordering::SeqCst)
            });
        let mut frames_dumped = false;
        let mut orphan_reported = false;
        match timeout_ms {
            _ if interrupted_at_enrol => was_interrupted = true,
            Some(ms) if ms > 0 => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let wait_time = remaining.min(slice);
                    let result = signal.wait_for(&mut state, wait_time);
                    slice = next_wait_slice(slice, self.tuning.wait_poll_backoff, parks_once);
                    // The CONDITION, re-tested under the mutex on every wakeup
                    // — the thing a condvar-only wait was missing.
                    if state.take_notification(my_ticket) {
                        NOTIFY_CREDITS_CONSUMED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        notified = true;
                        break;
                    }
                    if let Some(flag) = interrupted {
                        if flag.load(std::sync::atomic::Ordering::Acquire) {
                            was_interrupted = true;
                            // With the credit on, an unconsumed notification is
                            // still a credit in `state` for the next waiter to
                            // wake: nothing to forward (the forwarded signal
                            // was a spurious wake for whoever received it).
                            // LOST-WAKEUP FIX: a notify_one() wakes exactly ONE
                            // waiter. If this thread was both notified and
                            // interrupted, breaking out here to throw
                            // InterruptedException would CONSUME that single
                            // notification — another waiter the notify was meant
                            // for would never wake (lost wakeup). `wait_for`
                            // reports `!timed_out()` when a signal (notify /
                            // notifyAll / spurious) woke us within the slice; in
                            // that case forward the notification to one other
                            // waiter so the JLS/HotSpot guarantee that a notify
                            // wakes *some* waiter still holds. A spurious wakeup
                            // can trigger an extra notify_one() with no waiter to
                            // receive it, which is harmless (condvars do not
                            // accumulate permits). Done while still holding the
                            // monitor state lock, exactly like notify().
                            if !credits && !result.timed_out() {
                                self.wait_condvar.notify_one();
                            }
                            break;
                        }
                    }
                    // Woken by notify()/notifyAll() (or a spurious wakeup) before
                    // the poll slice elapsed — return so the caller can re-check
                    // its condition, exactly like the untimed branch below.
                    // Without this, timed Object.wait(timeout) ignored notify and
                    // always slept the FULL timeout: Thread.join(millis) waited
                    // the entire timeout after the joined thread already died, and
                    // ExecutorService.awaitTermination / timed Condition.await
                    // slept the whole duration after being signalled. With the
                    // credit on, a real notification was answered above; a
                    // signal with no credit is not one (see `credits`).
                    if !credits && !result.timed_out() {
                        break;
                    }
                    if !frames_dumped
                        && stack_dump_wait_flag().load(std::sync::atomic::Ordering::Acquire)
                    {
                        emit_wait_site_frames(thread_id);
                        frames_dumped = true;
                    }
                }
            }
            _ => {
                // Untimed wait. Loop with periodic interrupt checks until
                // notify() wakes us or the interrupt flag is set.
                if let Some(flag) = interrupted {
                    // DIAGNOSTIC A/B (netty ParameterizedSslHandlerTest promise
                    // stall, 2026-08-21). `CRATONVM_WAIT_SPURIOUS_MS=<n>` makes
                    // this untimed wait return to Java after `n` ms even with no
                    // notify. A spurious wakeup is explicitly permitted by
                    // JLS 17.2.1, and every correct caller re-checks its
                    // condition in a `while` loop — netty's
                    // `DefaultPromise.awaitUninterruptibly` does exactly that.
                    //
                    // It exists to PARTITION the stall, on ONE binary, without a
                    // cross-binary comparison: if the stall disappears under it,
                    // the promise had already completed and the notification was
                    // lost, so the defect is in this monitor. If the stall
                    // survives, the promise was never completed and the defect is
                    // upstream, in whatever should have run the task.
                    //
                    // Default OFF: unset leaves the loop byte-for-byte as it was.
                    let spurious_after = wait_spurious_ms();
                    let started = std::time::Instant::now();
                    // IS THIS THREAD EVEN POLLING?
                    //
                    // The dump below reports `notifies_since_wait`, and two
                    // stalls have now shown it as 1 — a `notifyAll()` reached
                    // this monitor while this thread was inside the loop, and
                    // the thread is still here. Two very different things
                    // produce that, and nothing so far tells them apart:
                    //
                    //   * the loop IS spinning (one 5 ms `wait_for` after
                    //     another, `polls` in the tens of thousands) and simply
                    //     never observed a signalled return — the notification
                    //     was lost between `Condvar::notify_all` and this
                    //     parked thread;
                    //   * the loop is NOT spinning (`polls` small and frozen) —
                    //     the thread is stuck INSIDE one `wait_for`, i.e. below
                    //     `parking_lot`, and the 5 ms timeout is not firing at
                    //     all. A GC-blocked or safepoint-parked thread looks
                    //     like this.
                    //
                    // `signalled` separates a notification this loop SAW from
                    // one the monitor merely served: a `wait_for` that returns
                    // `!timed_out()` breaks out one line below, so any value
                    // above zero here means the loop re-entered after a
                    // signalled return — which it can only do by NOT breaking,
                    // and that would be a bug in this loop rather than in the
                    // condvar.
                    let mut polls: u64 = 0;
                    let mut signalled: u64 = 0;
                    let mut consumed: u64 = 0;
                    // The spurious-wakeup diagnostic keeps the fixed slice, so
                    // its `n` ms stays as exact as it was.
                    let backoff = self.tuning.wait_poll_backoff && spurious_after.is_none();
                    // ... and neither a longer first slice nor the single park.
                    let parks_once = parks_once && spurious_after.is_none();
                    if spurious_after.is_some() {
                        slice = poll_interval;
                    }
                    loop {
                        let result = signal.wait_for(&mut state, slice);
                        slice = next_wait_slice(slice, backoff, parks_once);
                        polls = polls.wrapping_add(1);
                        if !result.timed_out() {
                            signalled = signalled.wrapping_add(1);
                            CONDVAR_SIGNALLED_RETURNS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        // The CONDITION, re-tested under the mutex on every
                        // wakeup. `signalled` deliberately counts only the
                        // condvar's own signal, so the two stay separable in
                        // the dump: a run with `consumed=1 signalled=0` is a
                        // notification this loop would have MISSED before.
                        if state.take_notification(my_ticket) {
                            consumed = consumed.wrapping_add(1);
                            NOTIFY_CREDITS_CONSUMED
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if result.timed_out() {
                                // The condvar did NOT signal this wakeup; the
                                // notification was found in state. See
                                // `NOTIFY_CREDITS_TAKEN_UNSIGNALLED`.
                                NOTIFY_CREDITS_TAKEN_UNSIGNALLED
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            notified = true;
                            break;
                        }
                        if let Some(ms) = spurious_after {
                            if started.elapsed() >= std::time::Duration::from_millis(ms) {
                                break;
                            }
                        }
                        if flag.load(std::sync::atomic::Ordering::Acquire) {
                            was_interrupted = true;
                            // LOST-WAKEUP FIX (see the timed branch above): if a
                            // notify woke us in the same slice we observed the
                            // interrupt, forward the single notification to one
                            // other waiter so it is not swallowed by the
                            // InterruptedException throw. `!timed_out()` means a
                            // signal (notify/notifyAll/spurious) arrived within
                            // the poll slice. Re-notify under the held state lock.
                            // Only without the credit: with it, an unconsumed
                            // notification is still in `state` (see the timed
                            // branch).
                            if !credits && !result.timed_out() {
                                self.wait_condvar.notify_one();
                            }
                            break;
                        }
                        if !frames_dumped
                            && stack_dump_wait_flag().load(std::sync::atomic::Ordering::Acquire)
                        {
                            emit_wait_site_frames(thread_id);
                            // WAITED-ON OBJECT STATE (diagnostic). The orphan check
                            // below came back CLEAN on a reproduced stall, so the
                            // waiter IS parked on the monitor its object points at
                            // and a notifier would resolve the same one. What is
                            // left to distinguish is netty's own bookkeeping:
                            //
                            //   result != null && waiters >= 1
                            //       the promise completed AND this waiter had
                            //       registered — so `checkNotifyWaiters` either
                            //       never ran or read a stale `waiters`, i.e. the
                            //       monitor is not establishing happens-before.
                            //   result != null && waiters == 0
                            //       the increment is not visible here at all.
                            //   result == null
                            //       the promise never completed — the 30 s A/B
                            //       result would then need re-examining.
                            //
                            // Only the VM can read those fields, hence the
                            // callback; `emit_wait_site_frames` uses the same
                            // pattern for exactly this reason.
                            // GC-SAFE HANDLE. `waited_on` is the ObjectRef this
                            // call was ENTERED with, held as a plain Rust local
                            // for the whole wait — and a moving collector
                            // relocates the object out from under it. G1 leaves
                            // the from-copy intact until the region is reused,
                            // so a stale read still parses as a plausible object
                            // and quietly reports pre-relocation field values.
                            // That is how the first version of this instrument
                            // produced an unfalsifiable `result`/`waiters` line.
                            //
                            // The registry's `jmx_waiting_monitor` IS a scanned
                            // root AND is forwarded by
                            // `update_thread_objs_after_gc` (gc.rs step 21), so
                            // resolving through it yields the CURRENT address.
                            // Fall back to `waited_on` only when the resolver is
                            // not installed (unit tests).
                            // Report WHICH handle was used, and whether the two
                            // disagree. A disagreement is the direct measurement
                            // that the object was relocated during the wait —
                            // i.e. proof that the earlier stale-local instrument
                            // was reading a dead address, rather than a
                            // suspicion about it. `handle=stale-local` means the
                            // resolver was never installed and this line is back
                            // to being untrustworthy; say so rather than let a
                            // silent fallback look like a sound reading.
                            let resolved = resolve_wait_object(thread_id);
                            let (live, src) = match resolved {
                                Some(o) => (Some(o), "registry-remapped"),
                                None => (waited_on, "stale-local(UNSOUND-FALLBACK)"),
                            };
                            if let (Some(r), Some(w)) = (resolved, waited_on) {
                                if !std::ptr::eq(r.as_ptr(), w.as_ptr()) {
                                    eprintln!(
                                        "[WAIT-OBJECT] RELOCATED during the wait: \
                                         entered_with={:p} now={:p} — any field read through \
                                         the entry pointer is a stale-copy read.",
                                        w.as_ptr(),
                                        r.as_ptr(),
                                    );
                                }
                            }
                            if let Some(obj) = live {
                                eprintln!("[WAIT-OBJECT] handle={src} obj={:p}", obj.as_ptr());
                                emit_wait_object_state(obj);
                            }
                            // THE PARTITIONING LINE. `notifies_since_wait=0`
                            // with a completed promise and `waiters == 1` says
                            // the completer never called `notifyAll()` on this
                            // monitor — so the defect is in the Java-level
                            // bookkeeping above it (a stale `waiters` read, or
                            // a `compareAndSet` that wrote without reporting
                            // success), NOT in the condvar handshake. Any
                            // non-zero value says the reverse. It needs one
                            // stall, not a rate.
                            let (notifies_now, interrupt_wakes_now) = self.notify_totals();
                            eprintln!(
                                "[WAIT-OBJECT] notifies_since_wait={} interrupt_wakes_since_wait={} \
                                 polls={polls} signalled={signalled} consumed={consumed} waited_ms={} \
                                 (monitor totals: notify={notifies_now} interrupt={interrupt_wakes_now})",
                                notifies_now.wrapping_sub(notifies_at_entry),
                                interrupt_wakes_now.wrapping_sub(interrupt_wakes_at_entry),
                                started.elapsed().as_millis(),
                            );
                            // THE MONITOR'S OWN STATE, read under the state
                            // lock this loop is already holding.
                            //
                            // `Object.wait()` must have RELEASED the monitor.
                            // An `owner` that is still this thread would mean
                            // no completer's `synchronized` block can ever run,
                            // which presents as `notify=0` on a promise nobody
                            // ever completed — indistinguishable, from the
                            // lines above alone, from a completer that simply
                            // never ran. Those need opposite investigations,
                            // and the netty `ParameterizedSslHandlerTest`
                            // residual stalls were stuck on exactly that fork.
                            // `parked_waiters` beside it distinguishes "this is
                            // the only waiter" from "several are queued here".
                            eprintln!(
                                "[WAIT-OBJECT] monitor owner={:?} entry_count={} \
                                 parked_waiters={} pending_notifies={} self_is_owner={}",
                                self.current_owner(),
                                self.entry_count.load(Ordering::Relaxed),
                                state.parked_waiters,
                                state.pending_notifies,
                                self.owner.load(Ordering::Relaxed) == me,
                            );
                            // ORPHAN CHECK. If the object's mark word stops
                            // pointing at `self`, a later `notifyAll()` inflates
                            // a DIFFERENT monitor and can never reach this
                            // waiter. Runs here — once, when the watchdog has
                            // already fired — rather than on every 5 ms poll:
                            // per-poll it cost a registry lock under the monitor
                            // state lock for a question only asked at a stall.
                            if !orphan_reported {
                                if let Some(obj) = live {
                                    let cur = header_of(obj).mark_word.load(Ordering::Acquire);
                                    // The monitor is found by address; an object
                                    // that is no longer INFLATED cannot reach it.
                                    let still_ours =
                                        ObjectHeader::mark_state(cur) == types::MARK_INFLATED;
                                    if !still_ours {
                                        orphan_reported = true;
                                        eprintln!(
                                            "[MONITOR-ORPHAN] thread {thread_id:?} is parked in \
                                             Object.wait() on a monitor the object no longer \
                                             points at — obj={:p} mark_state={} monitor={:p}. Any \
                                             later notify()/notifyAll() inflates a DIFFERENT \
                                             monitor and cannot reach this waiter.",
                                            obj.as_ptr(),
                                            ObjectHeader::mark_state(cur),
                                            self as *const Monitor,
                                        );
                                    }
                                }
                            }
                            frames_dumped = true;
                        }
                        // If the condvar was signalled (not timed out), break
                        // to allow the caller to re-check its condition -- only
                        // without the credit, which answered every real
                        // notification above (see `credits`).
                        if !credits && !result.timed_out() {
                            break;
                        }
                    }
                } else {
                    // No interrupt flag — a real untimed wait (unit tests etc.).
                    // Still condition-driven: a notification that arrived
                    // between the enrol above and this park is already a
                    // `pending_notifies` credit, and taking it without parking
                    // is the whole point of pairing the condvar with state.
                    loop {
                        if state.take_notification(my_ticket) {
                            NOTIFY_CREDITS_CONSUMED
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            notified = true;
                            break;
                        }
                        signal.wait(&mut state);
                    }
                }
            }
        }

        // LEAVE the waiter set. After this point a `notifyAll()` must not
        // count this thread, and a `notify()` must not leave a credit for it.
        // Leaving the wait set (interpreter round i1 wave 29). A waiter a
        // `notify` marked while it was timing out or being interrupted was
        // notified (HotSpot's `WasNotified`: it returns normally, its interrupt
        // left pending); one no notification marked takes its ticket out, so
        // a later `notify` goes to a waiter still waiting.
        if !notified {
            if state.take_notification(my_ticket) {
                NOTIFY_CREDITS_CONSUMED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                notified = true;
            } else {
                state.leave_wait_set(my_ticket);
            }
        }
        state.parked_waiters = state.parked_waiters.saturating_sub(1);

        // Re-acquire: wait until monitor is unowned or owned by us.
        //
        // POLLED, and reported. This used to be a bare
        // `entry_condvar.wait(&mut state)` — untimed, with no poll and no
        // diagnostic — which made it the one place in this function a thread
        // could be stuck WITHOUT the watchdog being able to say so. The
        // `[WAIT-OBJECT]` dump lives in the `wait_condvar` loop above, so a
        // thread that was notified, broke out, and then blocked HERE produced
        // exactly the signature the netty page could not explain: a delivered
        // `notifyAll()` (`notifies_since_wait=1`) and a thread still parked.
        //
        // The poll is not only an instrument. `Monitor::exit` releases with
        // `entry_condvar.notify_one()`, so a release wakes exactly one of the
        // threads queued here and in `enter_labeled`; any path that releases
        // this monitor WITHOUT going through `Monitor::exit` — a deflation back
        // to a thin lock, a `force_release_if_owned_by` race — leaves a waiter
        // here with nothing left to wake it. Re-testing the condition on a
        // timer is sound for a lock acquire in a way it would NOT be for
        // `Object.wait` (which owes Java a notification), so this costs one
        // wakeup per 5 ms per contended re-acquire and removes a whole class of
        // permanent stall.
        //
        // Since round 11 wave 16 the owner is a CAS word (`Monitor::owner`): a
        // free monitor is taken at once, and otherwise this thread registers as
        // a parked entrant exactly as `enter_labeled` does, so a release knows
        // to wake it (`wake_successor`).
        if !self.try_acquire_free(me) {
            // A waiter re-entering a monitor someone else holds is BLOCKED, not
            // WAITING (HotSpot's `wait_reenter_begin`): `Thread.getState()` of
            // a notified thread whose notifier still holds the lock.
            if let Some(java_state) = reacquire_state {
                java_state.store(2, std::sync::atomic::Ordering::Release);
            }
            // Round 13 wave 10 (lane monitor2, `MonitorTuning::wait_reacquire_spin`):
            // the holder is almost always the notifier, about to leave its
            // `synchronized` block, so spin for the release before paying a
            // second park and wake-up. Unregistered (a barging arrival): it
            // suppresses no wake-up and owes none. This thread is GC-blocked;
            // the spin touches only this monitor, which its `Arc` keeps alive.
            let reacquire_spin = self.tuning.wait_reacquire_spin && contended_spin_enabled();
            let spun = reacquire_spin
                && parking_lot::MutexGuard::unlocked(&mut state, || self.spin_for_free(me));
            if spun {
                note_contention(ContentionEvent::WaitReacquireSpinWin);
            } else {
                self.entry_waiters.fetch_add(1, Ordering::SeqCst);
                let mut reacquire_reported = false;
                while !self.try_acquire_free(me) {
                    let woken = !self
                        .entry_condvar
                        .wait_for(&mut state, poll_interval)
                        .timed_out();
                    // Woken by a release: spin for it first, the state lock
                    // released and `succ_pending` still set, then clear the
                    // flag before any CAS that could send this thread back to
                    // sleep -- `enter_labeled`'s order (see `spin_after_wake`).
                    let won = woken
                        && reacquire_spin
                        && parking_lot::MutexGuard::unlocked(&mut state, || {
                            self.spin_after_wake(me)
                        });
                    self.succ_pending.store(false, Ordering::SeqCst);
                    if won {
                        break;
                    }
                    if !reacquire_reported && stack_dump_wait_flag().load(Ordering::Acquire) {
                        reacquire_reported = true;
                        let (notifies_now, interrupt_now) = self.notify_totals();
                        eprintln!(
                            "[WAIT-REACQUIRE] thread {thread_id:?} was NOTIFIED and is now stuck \
                             RE-ACQUIRING the monitor, not waiting on it — owner={:?} entry_count={} \
                             saved_count={saved_count} notifies_since_wait={} interrupt_wakes_since_wait={}",
                            self.current_owner(),
                            self.entry_count.load(Ordering::Relaxed),
                            notifies_now.wrapping_sub(notifies_at_entry),
                            interrupt_now.wrapping_sub(interrupt_wakes_at_entry),
                        );
                    }
                }
                self.entry_waiters.fetch_sub(1, Ordering::SeqCst);
            }
        }
        // `try_acquire_free` started the count at 1; restore the depth this
        // thread held when it called `wait()`.
        self.entry_count.store(saved_count, Ordering::Relaxed);
        if saved_jfr {
            self.jfr_enter_recorded.store(true, Ordering::Relaxed);
        }

        Ok(if notified {
            WaitOutcome::Notified
        } else if was_interrupted {
            WaitOutcome::Interrupted
        } else {
            WaitOutcome::TimedOutOrSpurious
        })
    }

    /// Object.notify() — wake one thread waiting on this monitor.
    ///
    /// The calling thread must own this monitor.
    fn notify(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let state = self.state.lock();
        if self.owner.load(Ordering::Relaxed) != owner_word(Some(thread_id)) {
            return Err(MonitorError::NotOwner);
        }
        self.notify_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Make ONE notification available, but never more than there are
        // waiters to consume it: a notification stockpiled for a waiter that
        // does not exist would be consumed by a FUTURE `wait()` that no
        // `notify` was ever aimed at, which is a spurious return the JLS
        // permits but which would also mask a real lost wakeup from this
        // counter. See `MonitorState::pending_notifies`.
        let mut state = state;
        if monitor_pending_notify() {
            // The waiter that has waited longest (interpreter round i1 wave
            // 29): its ticket is marked, so it and no later arrival takes
            // the notification. A waiter on the SHARED condvar is woken with
            // every other waiter there, because the condvar picks its own
            // thread to wake and only the marked one leaves; the others
            // re-test their ticket and re-park. A waiter with a condvar of its
            // own (round 14 wave 3, M2-3) was signalled alone.
            if let Some(shared) = state.notify_longest_waiter() {
                NOTIFY_CREDITS_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if shared {
                    self.wait_condvar.notify_all();
                }
            }
        } else {
            self.wait_condvar.notify_one();
        }
        Ok(())
    }

    /// Object.notifyAll() — wake all threads waiting on this monitor.
    ///
    /// The calling thread must own this monitor.
    pub(crate) fn notify_all(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let state = self.state.lock();
        if self.owner.load(Ordering::Relaxed) != owner_word(Some(thread_id)) {
            return Err(MonitorError::NotOwner);
        }
        self.notify_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Exactly the waiters parked RIGHT NOW, which is what
        // `Object.notifyAll()` promises — a thread that starts waiting after
        // this point is not one of them and must not consume a notification
        // meant for the current set.
        let mut state = state;
        if monitor_pending_notify() {
            // Every waiter in the wait set NOW (interpreter round i1 wave 29);
            // those on condvars of their own are signalled there (round 14
            // wave 3, M2-3), the rest by the shared `notify_all` below.
            let marked = state.notify_every_waiter();
            NOTIFY_CREDITS_CREATED.fetch_add(u64::from(marked), std::sync::atomic::Ordering::Relaxed);
        }
        self.wait_condvar.notify_all();
        Ok(())
    }

    /// Wake every waiter so each re-evaluates its interrupt flag NOW.
    ///
    /// This is not `Object.notify` and takes no ownership check: it is the VM
    /// answering `Thread.interrupt()`, not Java code signalling a condition.
    /// `Thread.interrupt()` only sets a flag, and `wait()` can therefore
    /// observe it no sooner than its next 5 ms poll slice — so an interrupt
    /// aimed at a thread in `Object.wait()` took up to 5 ms to land while the
    /// `LockSupport.park` path next to it was already woken promptly by
    /// `park_state.unpark()`. That asymmetry is what this closes.
    ///
    /// `notify_all`, not `notify_one`: the interrupted thread is not
    /// identifiable from here, and a `notify_one` that reached the wrong
    /// waiter would leave the interrupt un-serviced for another slice while
    /// also consuming a slot. The other waiters get a spurious wakeup, which
    /// `Object.wait()` is explicitly specified to permit and which the
    /// surrounding `while (!condition) wait();` loop absorbs — and unlike a
    /// real `notify`, this consumes no pending notification, so no waiter can
    /// lose one.
    ///
    /// Taken under the monitor state lock, exactly like `notify`/`notify_all`.
    pub(crate) fn wake_all_for_interrupt(&self) {
        let state = self.state.lock();
        self.interrupt_wakes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Waiters on condvars of their own (round 14 wave 3, M2-3) are in the
        // wait set; a notified one not yet out of `wait` was signalled already.
        state.signal_every_own_condvar();
        self.wait_condvar.notify_all();
    }

    /// [`Self::wake_all_for_interrupt`] aimed at one thread (round 14 wave 4,
    /// lane monitor2, MON14-1): signal only `target`'s wait-set entry, so the
    /// other waiters sleep on. A target on the SHARED condvar (the M2-3 switch
    /// off) can only be reached with every waiter there, as before. Counted
    /// in `interrupt_wakes` like the broadcast. A target not in the wait set
    /// is signalled nowhere: before its enrolment its re-check reads the flag
    /// (`MonitorTuning::wait_enrol_interrupt_check`; without that switch its
    /// first 5 ms poll does), after it the wait is already ending.
    /// How many entries were signalled (tests; 0 or 1 for a live thread).
    pub(crate) fn wake_for_interrupt_of(&self, target: ThreadId) -> u32 {
        let state = self.state.lock();
        self.interrupt_wakes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (signalled, shared) = state.signal_waiter_for_interrupt(target);
        if shared {
            self.wait_condvar.notify_all();
        }
        signalled
    }
}

/// How an `Object.wait()` ended (HotSpot's `WasNotified` plus the reason).
///
/// The distinction is what JLS §17.2.4 turns on: a thread that is both
/// notified and interrupted must either return normally with the interrupt
/// pending, or throw and let the notification reach another waiter. HotSpot
/// takes the first branch for a thread whose notification arrived, and so
/// does [`Monitor::wait`]'s credit: it is consumed here, so the waiter must
/// return normally or the notification is lost (interpreter round i1 wave
/// 29, lane L4, probe `tools/probes/interp/L7/L7W29TWaitNotify.java`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// A `notify` / `notifyAll` notification was consumed.
    Notified,
    /// The interrupt flag ended the wait (no notification consumed).
    Interrupted,
    /// The timeout elapsed, or -- only with the notification credit switched
    /// off -- a bare condvar signal was taken as the notification.
    TimedOutOrSpurious,
}

/// Monitor operation error.
#[derive(Debug)]
pub(crate) enum MonitorError {
    /// The current thread does not own the monitor.
    NotOwner,
}

// ---------------------------------------------------------------------------
// MonitorTable — sharded enumeration index for inflated monitors
// ---------------------------------------------------------------------------

/// Number of shards in each of the two registries. Enumeration walks all of
/// them, so this trades a longer cold walk for a shorter inflation critical
/// section; 64 keeps the walk trivial (64 uncontended lock/unlock pairs) while
/// making inflation collisions rare even at high thread counts.
const MONITOR_SHARD_BITS: u32 = 6;
const MONITOR_SHARDS: usize = 1 << MONITOR_SHARD_BITS;

/// Map an object address to its registry shard.
///
/// Object addresses are at least 8-byte aligned and frequently allocated in
/// near-consecutive runs, so the low bits alone cluster badly; multiply by the
/// 64-bit golden ratio and take the *high* bits (fibonacci hashing) to spread
/// consecutive allocations across shards.
#[inline(always)]
fn shard_of(key: usize) -> usize {
    (((key as u64) >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - MONITOR_SHARD_BITS)) as usize
}

/// Sharded index of inflated JVM monitors, keyed by object pointer address.
///
/// CORRECTION (round 11 wave 16): since the 8-byte header this index IS how an
/// inflated monitor is found -- the mark word carries no pointer (see the
/// module docs) -- and a contended `monitorenter` / inflated `monitorexit`
/// reaches it on every operation, served from a thread-local cache
/// (`INFLATED_MONITOR_CACHE`, validated by `index_epoch`) so the hot path takes
/// no shard lock. The paragraphs below predate the 8-byte header.
///
/// **This is not on any hot path and is not how a monitor is found.** Locking
/// an object reaches its monitor through the object's own `mark_word` (see the
/// module-level docs): thin-locked and neutral objects never allocate a
/// `Monitor` at all, and an `INFLATED` mark word carries the monitor's address
/// directly. `enter` / `exit` / `wait` / `notify` / `holds` therefore take
/// **no lock in this table**.
///
/// What the table is still for — all of it cold, all of it enumeration:
///   * releasing every monitor a just-died thread still owned
///     ([`Self::release_monitors_held_by`]);
///   * re-keying / pruning across a collection ([`Self::remap_after_gc`],
///     [`Self::prune_dead`]);
///   * the per-object CAS locks used to emulate non-atomic compare-and-swap
///     ([`Self::with_cas_lock`]), which have no mark-word home;
///   * the legacy path for `ThreadId`s that exceed `u32::MAX` and so cannot be
///     represented in a thin lock;
///   * diagnostics (`Debug`, `CRATONVM_DBG_MONEXIT` forensics).
///
/// Because the mark word is authoritative, a missing index entry is a
/// *bookkeeping* miss, never a correctness failure: the entry is silently
/// re-derived from the mark word. The historical "INFLATED mark but no
/// registry entry" panic path is gone with it.
pub struct MonitorTable {
    /// Inflated-monitor index — keys are object pointer addresses, sharded by
    /// [`shard_of`]. Populated on inflation; read only by enumeration.
    ///
    // SECURITY FIX (V11) / ARCH-2026-07-26: every shard is the `monitors` lock
    // at hierarchy level L6 (see `cratonvm_types::lock_order`). Because all
    // shards share one level and the hierarchy forbids equal-level nesting, no
    // code path may hold two shard guards simultaneously. Every multi-shard
    // walk in this file (`release_monitors_held_by_except`, `remap_after_gc`,
    // `prune_dead`, `indexed_monitor_count`) locks exactly one shard at a
    // time — in debug builds the lock-order checker turns a violation of that
    // rule into an immediate panic rather than a latent deadlock.
    monitors: Arc<MonitorShards>,
    /// Thin-lock owner leases; see [`LockSlots`].
    lock_slots: LockSlots,
    /// Per-object CAS locks for compareAndSwap operations, sharded the same
    /// way. Provides mutual exclusion for non-atomic CAS emulation on Value
    /// slots. The inner per-object `Arc<Mutex<()>>` stays a plain
    /// `parking_lot::Mutex`: it is an L6-internal sub-lock with no global
    /// ordering constraints and is never held while acquiring another tracked
    /// lock.
    cas_locks: Box<[OrderedPlMutex<FxHashMap<usize, Arc<Mutex<()>>>>]>,
    /// Stable identity for this table's thread-local CAS cache entries.  This
    /// deliberately is not an address: a later VM may reuse an old table's
    /// allocation address after its Java threads have exited.
    cas_lock_cache_table_id: u64,
    /// Incremented while all mutators are stopped immediately before a CAS
    /// registry re-key/prune.  A matching cache epoch therefore proves the
    /// cached raw mutex pointer is still owned by the registry.
    cas_lock_epoch: AtomicU64,
    /// Bumped by EVERY change to the inflated-monitor index -- an insert
    /// (inflation, repair) under the key's shard lock, a removal or re-key
    /// with mutators stopped. A thread-local [`INFLATED_MONITOR_CACHE`] entry
    /// filled at epoch `e` therefore still names `index[key]` while the epoch
    /// reads `e`; see [`MonitorTable::cached_inflated_monitor`].
    ///
    /// Boxed (round 11 wave 17): compiled code on a lessee compares it with
    /// its [`LeaseBlock::inflated_epoch`] through the address the block
    /// records, which must not move with the table. On a cache line of its
    /// own since wave 19 ([`IndexEpochCell`]).
    index_epoch: Box<IndexEpochCell>,
    /// Odd while [`Self::remap_after_gc`] has the index drained (it empties
    /// every shard, then refills them re-keyed), even otherwise: a seqlock
    /// for walkers that run while a pause may be in progress. Round 12 wave 1
    /// (lane lock): the dying thread's sweep
    /// ([`Self::release_monitors_held_by_except`]) no longer runs under the
    /// GC barrier's transition lock, so it must not trust a walk that
    /// overlapped a drain.
    remap_seq: AtomicU64,
    /// Threads inside the running park (`vm_exec::lazy_monitor_park`,
    /// [`Monitor::park_enter_while`]) right now. Read by
    /// [`Self::wake_lazy_parkers`], which a stop-the-world initiator calls so
    /// a running parker never holds a pause up for its whole budget (round 12
    /// wave 1, lane lock, proposal W19-1).
    lazy_parkers: AtomicU64,
    /// The addresses [`Self::remap_after_gc`] re-keyed entries TO that the
    /// prune has not judged since ([`Self::prune_stale_after_gc`] takes those
    /// of the shards it judges). Each names a survivor a collector relocated,
    /// so the prune keeps its entry without asking the heap. Capped at
    /// [`REMAPPED_KEYS_CAP`]. Round 12 wave 2 (lane lock).
    remapped_keys: Mutex<Vec<usize>>,
    /// The first shard the next [`Self::prune_stale_after_gc`] judges; it
    /// advances by [`PRUNE_SHARDS_PER_PAUSE`] per call.
    prune_cursor: AtomicU64,
    /// This VM's monitor switches, read once here and copied into every
    /// monitor this table inflates ([`MonitorTuning`], round 12 wave 8).
    tuning: MonitorTuning,
}

/// Index shards [`MonitorTable::prune_stale_after_gc`] judges per collection:
/// an eighth of the index, so every entry is judged once every eight pauses.
/// Judging an entry costs a few cache misses (the monitor's lines, the
/// object's header) plus the heap's verdict, and a program with 100 000 live
/// inflated objects would otherwise pay all of them in every young pause.
/// Must divide [`MONITOR_SHARDS`].
const PRUNE_SHARDS_PER_PAUSE: usize = 8;

/// [`MonitorTable::index_epoch`]'s box: the epoch alone on a cache line.
///
/// Every inflated `monitorenter` / `monitorexit` reads it -- the helpers
/// through [`MonitorTable::cached_inflated_monitor`], compiled code through
/// [`LeaseBlock::epoch_addr`] -- on every thread, while it changes only at an
/// index mutation. A bare 8-byte `Box<AtomicU64>` (wave 17) shares its
/// allocator line with whatever the allocator placed next to it, so any hot
/// neighbour's write would have turned that read into a miss on every
/// thread (round 11 wave 19, lane lock).
#[repr(C, align(64))]
struct IndexEpochCell {
    value: AtomicU64,
}

impl std::ops::Deref for IndexEpochCell {
    type Target = AtomicU64;

    #[inline(always)]
    fn deref(&self) -> &AtomicU64 {
        &self.value
    }
}

thread_local! {
    /// `(table id, index epoch, object address, monitor)` of the last inflated
    /// monitor this OS thread resolved through the index, NON-owning (the
    /// index entry is the owner). Round 11 wave 16 (lane lock): a contended
    /// `monitorenter` and every inflated `monitorexit` used to take the
    /// object's L6 shard mutex and clone / drop the `Arc` -- two more
    /// contended cache lines, and a second mutex convoy, per operation.
    static INFLATED_MONITOR_CACHE: std::cell::Cell<(u64, u64, usize, *const Monitor)> =
        const { std::cell::Cell::new((0, 0, 0, std::ptr::null())) };
}

impl MonitorTable {
    /// Create an empty monitor table.
    pub fn new() -> Self {
        // Registered here rather than at a VM init site because this is the
        // earliest point that provably precedes any inflation: an object cannot
        // inflate without a monitor table, so no displaced hash can exist
        // before this runs. Idempotent -- the `OnceLock` keeps the first.
        cratonvm_gc::collector::set_displaced_hash_resolver(displaced_hash_at);
        // Every shard of both registries lives at L6 (`monitors`).
        let monitors: Arc<MonitorShards> = (0..MONITOR_SHARDS)
            .map(|_| OrderedPlMutex::new(FxHashMap::default(), LockLevel::Monitors))
            .collect::<Vec<_>>()
            .into();
        {
            let mut live = LIVE_MONITOR_INDEXES.write();
            live.retain(|w| w.strong_count() > 0);
            live.push(Arc::downgrade(&monitors));
        }
        Self {
            monitors,
            lock_slots: LockSlots::new(),
            cas_lock_cache_table_id: NEXT_CAS_LOCK_CACHE_TABLE_ID.fetch_add(1, Ordering::Relaxed),
            cas_lock_epoch: AtomicU64::new(1),
            // Starts above the cache's empty value (epoch 0).
            index_epoch: Box::new(IndexEpochCell {
                value: AtomicU64::new(1),
            }),
            remap_seq: AtomicU64::new(0),
            lazy_parkers: AtomicU64::new(0),
            remapped_keys: Mutex::new(Vec::new()),
            prune_cursor: AtomicU64::new(0),
            tuning: MonitorTuning::from_env(),
            cas_locks: (0..MONITOR_SHARDS)
                .map(|_| OrderedPlMutex::new(FxHashMap::default(), LockLevel::Monitors))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }

    /// The monitor-index shard that owns `key`.
    #[inline]
    fn monitor_shard(&self, key: usize) -> &OrderedPlMutex<FxHashMap<usize, Arc<Monitor>>> {
        &self.monitors[shard_of(key)]
    }

    /// Record `monitor` in the enumeration index under `key`, replacing any
    /// stale entry. Cold — called once per inflation (and on index repair).
    fn index_insert(&self, key: usize, monitor: &Arc<Monitor>) {
        let mut shard = self.monitor_shard(key).lock();
        shard.insert(key, monitor.clone());
        self.note_index_mutation();
    }

    /// Invalidate every thread's [`INFLATED_MONITOR_CACHE`] entry for this
    /// table. Called at each index mutation, under the mutated key's shard
    /// lock (inserts) or with mutators stopped (prune / re-key).
    #[inline]
    fn note_index_mutation(&self) {
        self.index_epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// Record `monitor` as `index[key]` in this thread's
    /// [`INFLATED_MONITOR_CACHE`]. The caller holds `key`'s shard lock and has
    /// just read (or inserted) that entry, so the epoch read here is the one
    /// the entry is valid for.
    #[inline]
    fn remember_inflated(&self, key: usize, monitor: &Arc<Monitor>) {
        let epoch = self.index_epoch.load(Ordering::Acquire);
        let entry = (self.cas_lock_cache_table_id, epoch, key, Arc::as_ptr(monitor));
        INFLATED_MONITOR_CACHE.with(|c| c.set(entry));
    }

    /// The monitor at `index[key]`, from this thread's cache, without the
    /// shard lock or an `Arc` clone. `None` on a miss.
    ///
    /// Only for an object the caller holds a live reference to and has just
    /// seen `INFLATED`, and only to be used before the caller next reaches a
    /// safepoint or blocks. Under those conditions the pointer is live:
    ///
    /// * an unchanged `index_epoch` means the index still maps `key` to this
    ///   monitor (every insert, removal and re-key bumps it), so the index's
    ///   `Arc` keeps it alive at the moment of the check;
    /// * after the check, the entry could be dropped only by `prune_dead` /
    ///   `remap_after_gc` (stop-the-world, which cannot complete while this
    ///   thread runs, and which drops only a DEAD object's monitor) or by an
    ///   insert at `key` replacing it -- and an insert at `key` happens only
    ///   for an object at `key` that is NOT inflated yet (or whose entry is
    ///   already gone, which bumped the epoch), never for the live, inflated
    ///   object the caller holds.
    #[inline]
    fn cached_inflated_monitor(&self, key: usize) -> Option<*const Monitor> {
        let (table, epoch, cached_key, monitor) = INFLATED_MONITOR_CACHE.with(|c| c.get());
        (table == self.cas_lock_cache_table_id
            && cached_key == key
            && !monitor.is_null()
            && epoch == self.index_epoch.load(Ordering::Acquire))
        .then_some(monitor)
    }

    /// Hand this OS thread's [`INFLATED_MONITOR_CACHE`] entry for `key`, if
    /// it has one, to compiled code on `slot`'s lessee ([`LeaseBlock`]'s
    /// one-entry cache), so the lessee's next compiled `monitorenter` /
    /// `monitorexit` on that object CASes the monitor's owner word inline
    /// instead of calling the helper (round 11 wave 17, lock proposal W16-1).
    ///
    /// The entry carries the epoch it was recorded at, and compiled code
    /// trusts it only while [`Self::index_epoch`] still reads that epoch and
    /// its receiver's word is INFLATED -- [`Self::cached_inflated_monitor`]'s
    /// conditions, which also hold across the emitted sequence (it has no
    /// safepoint and does not block). An entry that is already stale here is
    /// copied anyway: the emitted epoch check refuses it.
    ///
    /// LESSEE ONLY: `slot` must be the calling thread's own lease.
    #[inline]
    fn share_inflated_with_compiled_code(&self, slot: u32, key: usize) {
        let (table, epoch, cached_key, monitor) = INFLATED_MONITOR_CACHE.with(|c| c.get());
        if table != self.cas_lock_cache_table_id || cached_key != key || monitor.is_null() {
            return;
        }
        let epoch_addr = &self.index_epoch.value as *const AtomicU64 as usize;
        self.lock_slots
            .cache_inflated(slot, key, monitor as usize, epoch, epoch_addr);
    }

    /// Look up the inflated `Monitor` for `obj_ref` in the index -- the ONLY
    /// home an inflated monitor has since the 8-byte header left no room for a
    /// `Monitor` pointer in the mark word. Takes the object's L6 shard lock,
    /// and refills this thread's [`INFLATED_MONITOR_CACHE`] on a hit.
    ///
    /// Only meaningful while the object's mark word reads `INFLATED`: an entry
    /// at the address of an object that is not inflated belongs to a dead
    /// object that used to live there.
    fn lookup_indexed(&self, obj_ref: ObjectRef) -> Option<Arc<Monitor>> {
        let key = obj_ref.as_ptr() as usize;
        let shard = self.monitor_shard(key).lock();
        let monitor = shard.get(&key).cloned();
        if let Some(m) = &monitor {
            self.remember_inflated(key, m);
        }
        monitor
    }

    /// Force inflation of the lock for `obj_ref` and return the heavyweight
    /// `Monitor`.
    ///
    /// The whole inflation runs under the object's index shard lock: the
    /// monitor is found by address (the 8-byte header has no room for a
    /// pointer), so a reader that sees `INFLATED` and then takes the same
    /// shard lock must always find the entry. The mark-word CAS and the index
    /// insert are therefore one step as far as any reader can tell.
    ///
    /// * `INFLATED` -- return the indexed monitor.
    /// * `THIN_LOCKED` -- a new monitor pre-acquired for the thin lock's owner
    ///   (resolved from its lock slot) at `recursion + 1`, published by CAS.
    /// * `NEUTRAL` -- a new unowned monitor; a compact instance's identity hash
    ///   is displaced into it first, since the inflated word drops it.
    fn inflate_locked(
        &self,
        obj_ref: ObjectRef,
        header: &ObjectHeader,
    ) -> Result<Arc<Monitor>, MethodCallFailed> {
        let key = obj_ref.as_ptr() as usize;
        let mut shard = self.monitor_shard(key).lock();
        loop {
            let cur = header.mark_word.load(Ordering::Acquire);
            match ObjectHeader::mark_state(cur) {
                s if s == types::MARK_INFLATED => {
                    if let Some(m) = shard.get(&key) {
                        self.remember_inflated(key, m);
                        return Ok(Arc::clone(m));
                    }
                    // INFLATED with no index entry: a relocation that did not
                    // remap the index. Nothing else can produce it. Report it
                    // and give the object a fresh, unowned monitor so the VM
                    // keeps running; an owner of the lost monitor will see an
                    // `IllegalMonitorStateException` on exit.
                    static LOST: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let n = LOST.fetch_add(1, Ordering::Relaxed) + 1;
                    if n <= 8 || n.is_power_of_two() {
                        eprintln!(
                            "[cratonvm] monitor index has no entry for an INFLATED object at \
                             {key:#x} (#{n}) -- a moving collection did not remap it; \
                             re-inflating with a fresh monitor"
                        );
                    }
                    let m = Arc::new(Monitor::with_tuning(self.tuning));
                    shard.insert(key, Arc::clone(&m));
                    self.note_index_mutation();
                    self.remember_inflated(key, &m);
                    return Ok(m);
                }
                s if s == types::MARK_THIN_LOCKED => {
                    let slot = ObjectHeader::thin_lock_owner(cur);
                    let recursion = ObjectHeader::thin_lock_recursion(cur);
                    let monitor = Arc::new(Monitor::with_tuning(self.tuning));
                    if let Some(owner) = self.lock_slots.owner_of(slot) {
                        monitor.enter_with_recursion(owner, recursion + 1);
                    }
                    if header
                        .mark_word
                        .compare_exchange(
                            cur,
                            ObjectHeader::make_inflated(cur),
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        // The thin lock's ownership moved into the monitor.
                        // Any thread inflates, so it is uncounted in the
                        // slot's `stolen`, never in the lessee's own word.
                        self.lock_slots.note_stolen(slot);
                        shard.insert(key, Arc::clone(&monitor));
                        self.note_index_mutation();
                        self.remember_inflated(key, &monitor);
                        return Ok(monitor);
                    }
                    // The owner mutated the word (recursive bump or release);
                    // re-snapshot.
                }
                _ => {
                    let monitor = Arc::new(Monitor::with_tuning(self.tuning));
                    // A compact instance's identity hash lives in its NEUTRAL
                    // mark word and the inflated word has no room for it: move
                    // it into the monitor BEFORE the CAS, so no reader can find
                    // the object inflated and its hash gone.
                    let displaced = ObjectHeader::neutral_hash_value(header.class_id.as_u32(), cur);
                    if displaced != 0 {
                        monitor.displace_hash(displaced);
                    }
                    if header
                        .mark_word
                        .compare_exchange(
                            cur,
                            ObjectHeader::make_inflated(cur),
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        shard.insert(key, Arc::clone(&monitor));
                        self.note_index_mutation();
                        self.remember_inflated(key, &monitor);
                        return Ok(monitor);
                    }
                }
            }
        }
    }

    // `index_repair_if_absent` (re-insert a monitor at its key if the index
    // lost it) was removed in round 11 wave 17: it had no caller since the
    // 8-byte header made the index the monitor's only home, and replacing the
    // entry of a LIVE inflated object is the one insert the cached-monitor
    // liveness argument (`cached_inflated_monitor`, and compiled code's
    // `LeaseBlock` cache) rules out. An INFLATED word with no entry is
    // `inflate_locked`'s lost-entry arm.

    /// Acquire the monitor for the given object on behalf of the given thread.
    ///
    /// Blocking variant — prefer `enter_or_contend` from interpreter/native
    /// call sites so the contended wait can be wrapped in the GC-blocked
    /// protocol (an unmarked contended wait is counted in the STW barrier's
    /// `expected` and deadlocks the collector against a safepoint-parked
    /// owner — the H2 TestScript three-way wedge).
    pub fn enter(&self, obj_ref: ObjectRef, thread_id: ThreadId) {
        if let Some(m) = self.enter_or_contend(obj_ref, thread_id) {
            m.block_enter(thread_id);
        }
    }

    /// Acquire the monitor if possible WITHOUT blocking; on contention,
    /// return the inflated `Monitor` for the caller to block on (after
    /// marking itself GC-blocked — see `Monitor::block_enter`).
    ///
    /// Fast paths (no allocation):
    /// * NEUTRAL          → CAS to THIN_LOCKED (uncontended uncrossed lock).
    /// * THIN_LOCKED(self) → bump recursion (re-entrant single-thread lock).
    ///
    /// Slow paths (inflate to a real `Monitor`):
    /// * THIN_LOCKED(other) → spin briefly for the owner to release
    ///   ([`THIN_CONTENDED_SPINS`]); if it does not, inflate transferring
    ///   ownership, then try-enter.
    /// * THIN_LOCKED(self) at recursion = `MAX_THIN_LOCK_RECURSION` → inflate,
    ///   then try-enter.
    /// * INFLATED         → dispatch to `Monitor::try_enter`, the monitor found
    ///   by address.
    ///
    /// Before any arm reports contention it spins adaptively on the monitor
    /// ([`Monitor::spin_try_enter`]), as HotSpot does before it parks: the
    /// GC-blocked park the caller takes next (root deposit, TLAB retire, barrier
    /// transition, condvar park and an OS wake-up) costs microseconds, which is
    /// far longer than most critical sections. `CRATONVM_MONITOR_FASTPATH=0`
    /// turns the spinning off ([`contended_spin_enabled`]).
    ///
    /// Every arm above is per-object. The only path that touches a shared L6
    /// lock is inflation itself (one shard insert, once per object, ever).
    ///
    /// `None` ⇒ acquired. `Some(m)` ⇒ contended; caller must
    /// `m.block_enter(thread_id)` (the monitor may have been released in
    /// the interim — `block_enter` then acquires immediately).
    #[inline]
    pub(crate) fn enter_or_contend(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Option<Arc<Monitor>> {
        self.enter_or_contend_spinning(obj_ref, thread_id, true)
    }

    /// [`Self::enter_or_contend`] with no adaptive spin at all: every arm
    /// reports contention on its first failed attempt, exactly as before wave
    /// 4's spinning. For a caller that holds the GC barrier's transition lock
    /// across the call (`GcBarrier::with_no_pause_in_progress`: JNI monitor
    /// ops on a census-excluded thread), where a spin of tens of microseconds
    /// would stall every blocked-region transition in the VM by as much.
    pub(crate) fn enter_or_contend_without_spin(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Option<Arc<Monitor>> {
        self.enter_or_contend_spinning(obj_ref, thread_id, false)
    }

    /// The body of [`Self::enter_or_contend`]; `may_spin == false` skips both
    /// spins (the thin-owner watch and the monitor spin), and
    /// `contended_spin_enabled` is then never read.
    fn enter_or_contend_spinning(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
        may_spin: bool,
    ) -> Option<Arc<Monitor>> {
        let slot = self.lock_slots.slot_for(thread_id);
        let outcome = self.enter_or_contend_with_slot(obj_ref, thread_id, slot, may_spin);
        // An inflated receiver: let this thread's compiled code take its next
        // enter / exit of the object inline (`share_inflated_with_compiled_code`).
        // The mark-word load is of the line this call just CASed or read.
        if let Some(slot) = slot {
            let mark = header_of(obj_ref).mark_word.load(Ordering::Relaxed);
            if ObjectHeader::mark_state(mark) == types::MARK_INFLATED {
                self.share_inflated_with_compiled_code(slot, obj_ref.as_ptr() as usize);
            }
        }
        outcome
    }

    /// [`Self::enter_or_contend_spinning`] once `thread_id`'s lock slot is
    /// known (`None`: every slot is leased).
    fn enter_or_contend_with_slot(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
        slot: Option<u32>,
        may_spin: bool,
    ) -> Option<Arc<Monitor>> {
        let header = header_of(obj_ref);

        // A thread without a lock slot (every slot leased) never thin-locks:
        // it inflates, and its monitor is a real one the mark word agrees with.
        let Some(slot) = slot else {
            let cur = header.mark_word.load(Ordering::Acquire);
            return self.acquire_inflated_or_report_contention(
                obj_ref, header, cur, thread_id, may_spin,
            );
        };

        // ── Fast path 1: NEUTRAL → THIN_LOCKED via single CAS. ─────────────
        match try_thin_lock(header, slot) {
            Ok(()) => {
                self.lock_slots.note_acquired(slot);
                return None;
            }
            // Already inflated (a contended lock stays inflated): straight to
            // the monitor, through the thread-local index cache.
            Err(cur) if ObjectHeader::mark_state(cur) == types::MARK_INFLATED => {
                return self.acquire_inflated_or_report_contention(
                    obj_ref, header, cur, thread_id, may_spin,
                );
            }
            Err(_) => {}
        }

        // One bounded watch of a thin lock owned by someone else, per call:
        // after it the arm inflates, as it always did.
        let mut thin_spun = false;
        loop {
            let cur = header.mark_word.load(Ordering::Acquire);
            match ObjectHeader::mark_state(cur) {
                s if s == types::MARK_NEUTRAL => {
                    // Raced with another exit() — retry the fast path once.
                    if try_thin_lock(header, slot).is_ok() {
                        self.lock_slots.note_acquired(slot);
                        return None;
                    }
                    // Lost again, or the word carries an identity hash (a
                    // hashed compact instance cannot thin-lock) → inflate.
                    let m = self
                        .inflate_locked(obj_ref, header)
                        .expect("monitor inflation cannot fail");
                    return Self::acquire_or_report_contention(m, thread_id, may_spin);
                }
                s if s == types::MARK_THIN_LOCKED => {
                    let owner = ObjectHeader::thin_lock_owner(cur);
                    if owner == slot {
                        // ── Fast path 2: re-entrant thin lock. ──────────
                        match try_thin_recursive_lock(header, slot) {
                            Ok(_) => return None,
                            Err(err_mark) => {
                                if ObjectHeader::mark_state(err_mark) == types::MARK_THIN_LOCKED
                                    && ObjectHeader::thin_lock_owner(err_mark) == slot
                                    && ObjectHeader::thin_lock_recursion(err_mark)
                                        >= types::MAX_THIN_LOCK_RECURSION
                                {
                                    // Recursion overflow — inflate. Inflation
                                    // pre-acquires with entry_count =
                                    // recursion+1, capturing our prior
                                    // re-entrant acquisitions; the re-entrant
                                    // try_enter below records this one.
                                    let m = self
                                        .inflate_locked(obj_ref, header)
                                        .expect("monitor inflation cannot fail");
                                    return Self::acquire_or_report_contention(
                                        m, thread_id, may_spin,
                                    );
                                }
                                // Otherwise the state changed under us; reclassify.
                                continue;
                            }
                        }
                    } else {
                        // ── Contended thin lock: spin, then inflate. ────
                        // A short critical section releases within the watch,
                        // and the word is re-classified: NEUTRAL takes the thin
                        // CAS (no inflation at all), INFLATED goes to the
                        // monitor. Only one watch per call, so a word that is
                        // still thin-locked by another thread afterwards
                        // inflates exactly as before.
                        if !thin_spun && may_spin && contended_spin_enabled() {
                            thin_spun = true;
                            if spin_while_thin_locked(header) {
                                continue;
                            }
                        }
                        let m = self
                            .inflate_locked(obj_ref, header)
                            .expect("monitor inflation cannot fail");
                        return Self::acquire_or_report_contention(m, thread_id, may_spin);
                    }
                }
                _ => {
                    // INFLATED (or a state no live object carries): the monitor
                    // is found by address. `inflate_locked` answers the indexed
                    // monitor for an inflated word.
                    return self.acquire_inflated_or_report_contention(
                        obj_ref, header, cur, thread_id, may_spin,
                    );
                }
            }
        }
    }

    /// [`Self::acquire_or_report_contention`] for an object whose mark word
    /// read `mark`: an `INFLATED` word is served from this thread's
    /// [`INFLATED_MONITOR_CACHE`] when it hits -- no shard lock, no `Arc`
    /// traffic unless the thread has to park -- and everything else (a miss,
    /// or a word that is not inflated) goes through `inflate_locked`, as
    /// before round 11 wave 16.
    #[inline]
    fn acquire_inflated_or_report_contention(
        &self,
        obj_ref: ObjectRef,
        header: &ObjectHeader,
        mark: u32,
        thread_id: ThreadId,
        may_spin: bool,
    ) -> Option<Arc<Monitor>> {
        if ObjectHeader::mark_state(mark) == types::MARK_INFLATED {
            if let Some(ptr) = self.cached_inflated_monitor(obj_ref.as_ptr() as usize) {
                // SAFETY: `obj_ref` is the live object this thread is locking,
                // its word read INFLATED, and nothing below reaches a safepoint
                // or blocks -- the conditions under which
                // `cached_inflated_monitor`'s pointer is live.
                let m = unsafe { &*ptr };
                if m.try_enter(thread_id) {
                    return None;
                }
                if may_spin && contended_spin_enabled() && m.spin_try_enter_adaptive(thread_id) {
                    return None;
                }
                // Contended: the caller parks on it, possibly across a
                // collection, so it gets its own strong reference.
                // SAFETY: as above, the index's `Arc` is still alive, so
                // adding a strong count through the pointer it owns is sound.
                return Some(unsafe {
                    Arc::increment_strong_count(ptr);
                    Arc::from_raw(ptr)
                });
            }
        }
        let m = self
            .inflate_locked(obj_ref, header)
            .expect("monitor inflation cannot fail");
        Self::acquire_or_report_contention(m, thread_id, may_spin)
    }

    /// The common tail of every inflating arm of [`Self::enter_or_contend`]:
    /// take `m` now, or after a bounded adaptive spin, or report it contended.
    #[inline]
    fn acquire_or_report_contention(
        m: Arc<Monitor>,
        thread_id: ThreadId,
        may_spin: bool,
    ) -> Option<Arc<Monitor>> {
        if m.try_enter(thread_id) {
            return None;
        }
        if may_spin && contended_spin_enabled() && m.spin_try_enter_adaptive(thread_id) {
            return None;
        }
        Some(m)
    }

    /// Force the object's monitor into inflated form and acquire it if
    /// possible without blocking. Returns the stable monitor handle and whether
    /// the caller must block on it.
    ///
    /// This is used by thread termination: once it has the `Arc<Monitor>`, the
    /// final `mark_dead`/`notifyAll`/`exit` sequence no longer needs to
    /// re-lookup the monitor through the Java `Thread` object's raw address,
    /// which may be remapped by a concurrent moving GC.
    pub(crate) fn enter_inflated_or_contend(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Result<(Arc<Monitor>, bool), MethodCallFailed> {
        let monitor = self.ensure_inflated(obj_ref, thread_id)?;
        let contended = !monitor.try_enter(thread_id);
        Ok((monitor, contended))
    }

    /// Release the monitor for the given object on behalf of the given thread.
    ///
    /// Fast paths (no allocation):
    /// * THIN_LOCKED(self) at recursion>0 → CAS recursion-1.
    /// * THIN_LOCKED(self) at recursion=0 → CAS back to NEUTRAL.
    ///
    /// Slow path:
    /// * INFLATED → dispatch to `Monitor::exit`, the monitor found by address
    ///   through this thread's `INFLATED_MONITOR_CACHE` (the index's shard
    ///   lock only on a miss; the INFLATED word carries no pointer since the
    ///   8-byte header); no allocation, no refcount traffic.
    ///
    /// Returns `Err(MethodCallFailed)` with `IllegalMonitorStateException` if
    /// the calling thread does not own the monitor.
    pub fn exit(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> Result<(), MethodCallFailed> {
        self.exit_reporting_release(obj_ref, thread_id).map(|_| ())
    }

    /// [`Self::exit`], also answering whether `thread_id` still holds the
    /// monitor afterwards: `Ok(true)` when this was the outermost release,
    /// `Ok(false)` when a re-entrant acquisition remains. It is exactly
    /// `!self.holds(obj_ref, thread_id)` evaluated after a successful exit —
    /// nothing but the owner can re-acquire for the owner, and an inflation by
    /// another thread carries the recursion over — read from the transition
    /// itself instead of a second mark-word load (and, for an inflated monitor,
    /// a second state-lock acquisition).
    ///
    /// On `Err` it answers nothing; a caller that needs ownership after a
    /// failed exit must ask [`Self::holds`].
    pub(crate) fn exit_reporting_release(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Result<bool, MethodCallFailed> {
        let key = obj_ref.as_ptr() as usize;
        let header = header_of(obj_ref);

        let slot = self.lock_slots.existing_slot(thread_id);
        let thin_owner_slot = match slot {
            Some(slot) => Some(slot),
            // No lease, so this thread owns no thin lock. A thin-locked word
            // is someone else's; only an inflated monitor can be ours.
            None => {
                let cur = header.mark_word.load(Ordering::Acquire);
                if ObjectHeader::mark_state(cur) != types::MARK_INFLATED {
                    self.dbg_monexit_forensics(obj_ref, thread_id, cur, "no-slot");
                    return Err(MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::IllegalMonitorStateException {
                            message: format!(
                                "thread {thread_id} does not own the monitor for object at {key:#x}"
                            ),
                        },
                    )));
                }
                None
            }
        };
        if let Some(slot) = thin_owner_slot {
            // ── Fast path: thin-lock release. ──────────────────────────────
            // `None` = the word went back to NEUTRAL; `Some(r)` = still
            // thin-locked by us at recursion `r`.
            match try_thin_unlock(header, slot) {
                Ok(None) => {
                    // The owner's own final release: `slot` is its lease.
                    self.lock_slots.note_owner_released(slot);
                    return Ok(true);
                }
                Ok(Some(_)) => return Ok(false),
                Err(cur) => {
                    if ObjectHeader::mark_state(cur) == types::MARK_INFLATED {
                        // Fall through to inflated dispatch below.
                    } else {
                        // Not held by us (thin, but different owner; or
                        // neutral) — raise IMSE.
                        self.dbg_monexit_forensics(obj_ref, thread_id, cur, "thin-arm");
                        return Err(MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::IllegalMonitorStateException {
                                message: format!(
                                    "thread {thread_id} does not own the monitor for object at {key:#x}"
                                ),
                            },
                        )));
                    }
                }
            }
        }

        // ── Slow path: inflated monitor dispatch, found by address. ─────────
        let mark = header.mark_word.load(Ordering::Acquire);
        let released: Option<Result<bool, MonitorError>> =
            if ObjectHeader::mark_state(mark) == types::MARK_INFLATED {
                match self.cached_inflated_monitor(key) {
                    // SAFETY: `obj_ref` is live and its word read INFLATED;
                    // `exit_reporting_release` never blocks or reaches a
                    // safepoint (see `cached_inflated_monitor`).
                    Some(ptr) => Some(unsafe { &*ptr }.exit_reporting_release(thread_id)),
                    None => self
                        .lookup_indexed(obj_ref)
                        .map(|m| m.exit_reporting_release(thread_id)),
                }
            } else {
                None
            };
        match released {
            Some(result) => result
                .map_err(|MonitorError::NotOwner| {
                    self.dbg_monexit_forensics(
                        obj_ref,
                        thread_id,
                        header.mark_word.load(Ordering::Acquire),
                        "inflated-notowner",
                    );
                    MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::IllegalMonitorStateException {
                            message: format!(
                            "thread {thread_id} does not own the monitor for object at {key:#x}"
                        ),
                        },
                    ))
                }),
            None => {
                // Neither the mark word nor the index names a monitor for this
                // object, so the thread never entered it.
                self.dbg_monexit_forensics(obj_ref, thread_id, mark, "no-monitor");
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "monitorexit on object at {key:#x} that was never entered"
                        ),
                    },
                )))
            }
        }
    }

    /// GC-audit finding 1(b) forensics (gated: `CRATONVM_DBG_MONEXIT`, default
    /// OFF, zero cost when unset). On a monitorexit IMSE, dump everything
    /// needed to classify the failure shape post-hoc: the raw mark word and
    /// its decode (NEUTRAL = zeroed/stale header, THIN by another tid =
    /// identity confusion, INFLATED = owner mismatch), the object header's
    /// class_id/num_slots words (a zeroed pair = the "zeroed live object"
    /// recycled-slot shape), and the registry's view of the inflated monitor.
    #[cold]
    fn dbg_monexit_forensics(&self, obj_ref: ObjectRef, tid: ThreadId, mark: u32, arm: &str) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON
            .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MONEXIT").is_some())
        {
            return;
        }
        let key = obj_ref.as_ptr() as usize;
        let state = ObjectHeader::mark_state(mark);
        let (class_id, num_slots) = unsafe {
            let p = obj_ref.as_ptr() as *const u8;
            (
                std::ptr::read(p as *const u32),
                (*(p as *const ObjectHeader)).num_slots(),
            )
        };
        let (reg_hit, reg_owner, reg_count) = {
            let shard = self.monitor_shard(key).lock();
            match shard.get(&key) {
                Some(m) => (
                    true,
                    m.current_owner(),
                    m.entry_count.load(Ordering::Relaxed),
                ),
                None => (false, None, 0),
            }
        };
        // The mark word is authoritative now, so print what IT says alongside
        // the index's opinion — a disagreement is the interesting signal.
        let (mark_owner, mark_count) = match ObjectHeader::mark_state(mark) {
            s if s == types::MARK_THIN_LOCKED => (
                Some(
                    self.lock_slots
                        .owner_of(ObjectHeader::thin_lock_owner(mark)),
                ),
                ObjectHeader::thin_lock_recursion(mark) + 1,
            ),
            _ => (None, 0),
        };
        eprintln!(
            "[MONEXIT-IMSE] arm={arm} tid={} obj={key:#x} mark={mark:#x} state={} thin_owner={} thin_rec={} class_id={class_id} num_slots={num_slots} index_hit={reg_hit} index_owner={reg_owner:?} index_entry_count={reg_count} mark_owner={mark_owner:?} mark_entry_count={mark_count}",
            tid.0,
            match state {
                s if s == types::MARK_NEUTRAL => "NEUTRAL",
                s if s == types::MARK_THIN_LOCKED => "THIN",
                s if s == types::MARK_INFLATED => "INFLATED",
                _ => "RESERVED",
            },
            ObjectHeader::thin_lock_owner(mark),
            ObjectHeader::thin_lock_recursion(mark),
        );
    }

    /// Round-7 HIGH (vm #5): record on the monitor for `obj_ref` that the
    /// interpreter has emitted a JFR `monitor_enter` event for the current
    /// outermost acquisition by `thread_id`. The exit-side companion is
    /// [`jfr_enter_recorded`].
    ///
    /// If the monitor has not been inflated yet (the uncontended fast path
    /// served the enter via the mark-word thin lock), this forces inflation
    /// so the flag has a stable home. Inflation is the expected price for a
    /// monitor that the JFR consumer is interested in — the JFR-enabled gate
    /// means we're already on the cold path.
    pub fn set_jfr_enter_recorded(&self, obj_ref: ObjectRef, thread_id: ThreadId) {
        // `ensure_inflated` can now only fail on an INFLATED mark word holding
        // a null monitor pointer, which `make_inflated` cannot produce — i.e.
        // memory corruption. This API is `()`-returning (the JFR consumer has
        // no error channel here), so panic to surface it.
        let monitor = self
            .ensure_inflated(obj_ref, thread_id)
            .expect("monitor inflation invariant: corrupt INFLATED mark word");
        monitor.set_jfr_enter_recorded(thread_id);
    }

    /// Round-7 HIGH (vm #5): query whether a JFR `monitor_enter` event was
    /// emitted for the current outermost acquisition of the monitor for
    /// `obj_ref`. Returns `false` if the monitor has not been inflated (in
    /// which case the enter event could not have been recorded — the flag
    /// lives on the heavyweight `Monitor`, never on the thin lock).
    pub fn jfr_enter_recorded(&self, obj_ref: ObjectRef) -> bool {
        let mark = header_of(obj_ref).mark_word.load(Ordering::Acquire);
        ObjectHeader::mark_state(mark) == types::MARK_INFLATED
            && self
                .lookup_indexed(obj_ref)
                .is_some_and(|m| m.jfr_enter_recorded())
    }

    /// The identity hash of an object whose mark word has no room for one,
    /// installing `mint()`'s value the first time and answering with it
    /// forever after.
    ///
    /// A `NEUTRAL` word carries the hash in its upper bits, and that is where
    /// [`ObjectHeader::mark_word_identity_hash`] puts it. Every other state has
    /// the space spoken for: a `THIN_LOCKED` payload is an owner plus a
    /// recursion count, an `INFLATED` payload is a monitor pointer. Before this
    /// method existed the heap answered such an object with `0`, which the VM's
    /// "never hand back 0" guard turned into `i32::MAX` — the SAME value for
    /// every locked-then-hashed object, and a value that changed the moment the
    /// thin lock was released and a real hash was minted into the freed word.
    ///
    /// A changing identity hash is not a cosmetic defect. `HashMap` files an
    /// entry under the hash it read at `put` and looks it up under the hash it
    /// reads at `get`; when those differ the map cannot find its own key, and
    /// re-putting that key grows a SECOND entry for the same object. Spring
    /// Boot's `TomcatWebServer` hits it exactly: it parks a service's
    /// connectors in a `Map<Service, Connector[]>` from inside
    /// `LifecycleBase.start()`, which is `synchronized` on that very
    /// `StandardService`. The lookup after the lock was released missed, the
    /// service came back with no connectors, `Tomcat.getConnector()` fabricated
    /// a fresh port-8080 connector on the already-running service, and every
    /// embedded-Tomcat test failed with `Connector configured to listen on port
    /// 8080 failed to start`.
    ///
    /// Inflating is what HotSpot does here — `ObjectSynchronizer::FastHashCode`
    /// inflates a stack-locked object and stores the hash in the monitor's
    /// displaced header — and it is stable for the object's life: nothing in
    /// this VM deflates a LIVE object's monitor (the mark word's reference is
    /// released only for an object the collector has proved dead), so once
    /// displaced the hash stays reachable through the same pointer.
    ///
    /// `mint` runs at most once per call, and its value is adopted only if this
    /// caller wins the install race; [`Monitor::displace_hash`] makes the losers
    /// converge on the winner's value.
    /// The Java-visible identity hash of `obj_ref`, given `heap_answer` — what
    /// the collector's own `identity_hash_code` accessor said.
    ///
    /// The two-branch shape lives here, in one place, so the VM's
    /// `NativeContext::identity_hash_code` and the regression test below
    /// exercise the same composition rather than each restating it.
    ///
    /// `heap_answer == 0` is not "no hash yet": the heap mints one for a
    /// NEUTRAL word. It means the word cannot hold a hash at all and nothing is
    /// displaced — see [`Self::identity_hash_via_monitor`].
    pub fn java_identity_hash(
        &self,
        obj_ref: ObjectRef,
        heap_answer: i32,
        mint: impl FnOnce() -> i32,
    ) -> i32 {
        let hash = if heap_answer != 0 {
            heap_answer
        } else {
            self.identity_hash_via_monitor(obj_ref, mint)
        };
        // `identityHashCode` must never be 0: the JDK's
        // `InvokerBytecodeGenerator` uses it as a HashMap key and asserts
        // non-zero. Only a corrupt INFLATED word reaches this arm now.
        match hash {
            0 => i32::MAX,
            h => h,
        }
    }

    pub fn identity_hash_via_monitor(&self, obj_ref: ObjectRef, mint: impl FnOnce() -> i32) -> i32 {
        let Ok(monitor) = self.ensure_inflated(obj_ref, ThreadId(0)) else {
            // Only a corrupt INFLATED word carrying a null monitor pointer
            // reaches here. Report "no answer" and let the caller's guard speak.
            return 0;
        };
        let existing = monitor.displaced_hash();
        if existing != 0 {
            return existing;
        }
        // `displace_hash` reads 0 as "none recorded", so a mint that produced 0
        // would install nothing and be re-minted on the next call — the one
        // thing an identity hash must never do.
        let candidate = match mint() {
            0 => i32::MAX,
            h => h,
        };
        monitor.displace_hash(candidate)
    }

    /// Ensure the object's lock is inflated and return the heavyweight
    /// `Monitor`. If the calling thread holds the thin lock, ownership is
    /// transferred atomically.
    ///
    /// Used by `wait`/`notify`/`notifyAll`, which require a heavyweight
    /// monitor (thin locks have no condvars).
    ///
    /// The already-inflated case — by far the common one for a `wait`/`notify`
    /// loop — is served entirely from the mark word: **no L6 lock, no map
    /// probe**. Only a first-time inflation reaches `inflate_locked`.
    ///
    /// Errs only if the mark word reads INFLATED while carrying a null monitor
    /// pointer, which `ObjectHeader::make_inflated` refuses to construct; that
    /// is memory corruption, surfaced as a runtime error rather than a null
    /// dereference.
    fn ensure_inflated(
        &self,
        obj_ref: ObjectRef,
        _thread_id: ThreadId,
    ) -> Result<Arc<Monitor>, MethodCallFailed> {
        let header = header_of(obj_ref);
        // `inflate_locked` answers the indexed monitor for an inflated word,
        // and otherwise re-snapshots the mark word so its pre-acquire reflects
        // the current thin-lock owner.
        self.inflate_locked(obj_ref, header)
    }

    /// Perform `Object.wait()` on the monitor for the given object.
    ///
    /// The calling thread must own the monitor. It releases ownership, blocks
    /// until notified (or timed out), then re-acquires the monitor.
    ///
    /// If `interrupted` is provided, the wait will periodically check the flag
    /// and return `Ok(true)` if the thread was interrupted during the wait.
    pub fn wait(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
        timeout_ms: Option<u64>,
        interrupted: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<bool, MethodCallFailed> {
        self.wait_outcome(obj_ref, thread_id, timeout_ms, interrupted, None)
            .map(|outcome| outcome == WaitOutcome::Interrupted)
    }

    /// [`Self::wait`], reporting HOW the wait ended ([`WaitOutcome`]) so the
    /// caller can apply HotSpot's rule: a notified waiter returns normally even
    /// if it was interrupted meanwhile (the interrupt stays pending), and only
    /// a wait that ended otherwise throws `InterruptedException`.
    /// `reacquire_state` is the waiter's `Thread.getState()` word, set to
    /// BLOCKED while it queues to re-enter a monitor another thread holds.
    pub fn wait_outcome(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
        timeout_ms: Option<u64>,
        interrupted: Option<&std::sync::atomic::AtomicBool>,
        reacquire_state: Option<&std::sync::atomic::AtomicU8>,
    ) -> Result<WaitOutcome, MethodCallFailed> {
        let monitor = self.ensure_inflated(obj_ref, thread_id)?;
        monitor
            .wait(thread_id, timeout_ms, interrupted, Some(obj_ref), reacquire_state)
            .map_err(|MonitorError::NotOwner| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!("current thread is not owner"),
                    },
                ))
            })
    }

    /// What `notify` / `notifyAll` can decide from the mark word alone,
    /// without inflating: `Some(Ok(()))` for a THIN lock `thread_id` holds --
    /// nothing can be waiting, because `wait` inflates and an inflated word
    /// never goes back to thin -- `Some(Err(..))` (`IllegalMonitorStateException`)
    /// for a word `thread_id` provably does not own (NEUTRAL, or thin-locked
    /// under another lease), and `None` (ask the monitor) otherwise.
    ///
    /// Round 12 wave 1 (lane lock): both calls used to inflate first
    /// (`ensure_inflated`), so every object ever `notify()`'d -- the common
    /// `synchronized (lock) { ..; lock.notifyAll(); }` with nobody waiting
    /// included -- took the inflated path on every later lock for the rest
    /// of the run, and kept a `Monitor` in the index that the moving
    /// collectors never prune. HotSpot's `ObjectSynchronizer::notify` returns
    /// at once for a fast-locked object for the same reason.
    /// `CRATONVM_MONITOR_THIN_NOTIFY=0` restores the inflating calls.
    fn notify_without_inflating(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Option<Result<(), MethodCallFailed>> {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let on = *ON.get_or_init(|| {
            !matches!(
                cratonvm_types::flags::runtime_var("CRATONVM_MONITOR_THIN_NOTIFY").as_deref(),
                Ok("0") | Ok("false") | Ok("off")
            )
        });
        if !on {
            return None;
        }
        let mark = header_of(obj_ref).mark_word.load(Ordering::Acquire);
        let owned = match ObjectHeader::mark_state(mark) {
            s if s == types::MARK_THIN_LOCKED => self
                .lock_slots
                .existing_slot(thread_id)
                .is_some_and(|slot| ObjectHeader::thin_lock_owner(mark) == slot),
            s if s == types::MARK_NEUTRAL => false,
            _ => return None,
        };
        Some(if owned {
            Ok(())
        } else {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IllegalMonitorStateException {
                    message: "current thread is not owner".to_string(),
                },
            )))
        })
    }

    /// Perform `Object.notify()` on the monitor for the given object.
    ///
    /// Wakes one thread waiting on this monitor. The calling thread must own it.
    pub fn notify(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> Result<(), MethodCallFailed> {
        if let Some(decided) = self.notify_without_inflating(obj_ref, thread_id) {
            return decided;
        }
        let notified = match self.cached_monitor_of_inflated(obj_ref) {
            // SAFETY: `cached_monitor_of_inflated`'s contract -- `obj_ref` is
            // the live object this running thread notifies, its word read
            // INFLATED, and `Monitor::notify` neither blocks in a GC-blocked
            // region nor reaches a safepoint.
            Some(ptr) => unsafe { &*ptr }.notify(thread_id),
            None => self.ensure_inflated(obj_ref, thread_id)?.notify(thread_id),
        };
        notified.map_err(|MonitorError::NotOwner| {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IllegalMonitorStateException {
                    message: format!("current thread is not owner"),
                },
            ))
        })
    }

    /// Perform `Object.notifyAll()` on the monitor for the given object.
    ///
    /// Wakes all threads waiting on this monitor. The calling thread must own it.
    pub fn notify_all(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Result<(), MethodCallFailed> {
        if let Some(decided) = self.notify_without_inflating(obj_ref, thread_id) {
            return decided;
        }
        let notified = match self.cached_monitor_of_inflated(obj_ref) {
            // SAFETY: as in `notify` just above.
            Some(ptr) => unsafe { &*ptr }.notify_all(thread_id),
            None => self.ensure_inflated(obj_ref, thread_id)?.notify_all(thread_id),
        };
        notified
            .map_err(|MonitorError::NotOwner| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            // The SAME text `wait` and `notify` use twenty lines
                            // up. HotSpot's `IllegalMonitorStateException` for
                            // all three is `current thread is not owner`; this
                            // arm alone spelled it differently, and named a
                            // thread id no other arm names. MEASURED by
                            // `apps/probes/SystemRuntimeObjectSweep.java`.
                            "current thread is not owner"
                        ),
                    },
                ))
            })
    }

    /// Wake every thread parked in `Object.wait()` on any indexed monitor, so
    /// each re-reads the watchdog's stack-dump flag now rather than at its
    /// next poll slice (up to `WAIT_POLL_SLICE_MAX` with
    /// `MonitorTuning::wait_poll_backoff`). Marks nothing: to a waiter this
    /// is the interrupt wake's spurious signal, which re-parks it. Cold: once
    /// per watchdog request (`SharedVm::request_stack_dump`). The monitors
    /// are collected first, so no state lock is taken under a shard lock.
    pub fn wake_all_waiters_for_stack_dump(&self) {
        for shard in self.monitors.iter() {
            let monitors: Vec<Arc<Monitor>> = shard.lock().values().cloned().collect();
            for monitor in monitors {
                monitor.wake_all_for_interrupt();
            }
        }
    }

    /// Wake anything parked in `Object.wait()` on `obj_ref` so it re-reads its
    /// interrupt flag immediately. Called by `Thread.interrupt()`.
    ///
    /// Deliberately does **not** inflate: a monitor with no heavyweight
    /// `Monitor` behind it has never had a `wait()` on it (`wait` goes through
    /// `ensure_inflated` first), so there is nothing to wake and inflating on
    /// an interrupt would allocate a monitor for an object that never needed
    /// one. Returns `true` if a wake was actually delivered — diagnostics and
    /// tests only.
    ///
    /// See [`Monitor::wake_all_for_interrupt`] for why this is a `notify_all`
    /// and why it cannot swallow a pending `notify`. Production callers use
    /// the targeted [`Self::wake_waiter_for_interrupt`]; this broadcast form
    /// is kept for the tests that pin the all-waiters contract.
    #[cfg(test)]
    pub fn wake_waiters_for_interrupt(&self, obj_ref: ObjectRef) -> bool {
        let header = header_of(obj_ref);
        let mark = header.mark_word.load(Ordering::Acquire);
        if ObjectHeader::mark_state(mark) != types::MARK_INFLATED {
            return false;
        }
        let Some(monitor) = self.lookup_indexed(obj_ref) else {
            return false;
        };
        monitor.wake_all_for_interrupt();
        true
    }

    /// [`Self::wake_waiters_for_interrupt`] for the one thread `target` that
    /// `Thread.interrupt()` names (round 14 wave 4, lane monitor2, MON14-1,
    /// `CRATONVM_MONITOR_INTERRUPT_WAKES_TARGET`): only `target`'s wait-set
    /// entry is signalled ([`Monitor::wake_for_interrupt_of`]); every other
    /// waiter on the monitor sleeps on. With the switch off, every waiter is
    /// woken, as [`Self::wake_waiters_for_interrupt`] does. Never inflates;
    /// `true` if the object's monitor was found and the wake delivered.
    pub fn wake_waiter_for_interrupt(&self, obj_ref: ObjectRef, target: ThreadId) -> bool {
        let header = header_of(obj_ref);
        // Round 14 wave 5 (RV5-1): orders the caller's SeqCst flag store
        // (`ThreadRegistry::set_interrupted`) before this mark read. Pairs with
        // the fence at `Monitor::wait`'s enrolment re-check: a waiter that
        // inflates after this read sees the flag there, so a thin mark here
        // (nothing to wake) cannot lose the interrupt.
        std::sync::atomic::fence(Ordering::SeqCst);
        let mark = header.mark_word.load(Ordering::Acquire);
        if ObjectHeader::mark_state(mark) != types::MARK_INFLATED {
            return false;
        }
        let Some(monitor) = self.lookup_indexed(obj_ref) else {
            return false;
        };
        if monitor.tuning.interrupt_wakes_target {
            monitor.wake_for_interrupt_of(target);
        } else {
            monitor.wake_all_for_interrupt();
        }
        true
    }

    /// T1.6.7 — Implements `Thread.holdsLock(Object)`.
    ///
    /// Returns `true` if the given thread currently owns the monitor for
    /// `obj_ref`. Returns `false` if the object has never been entered or
    /// is currently owned by a different thread.
    ///
    /// This is a non-blocking inspection — it briefly takes the monitor
    /// state lock to read the owner field but never waits for entry.
    pub fn holds(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> bool {
        let header = header_of(obj_ref);
        let mark = header.mark_word.load(Ordering::Acquire);
        match ObjectHeader::mark_state(mark) {
            s if s == types::MARK_THIN_LOCKED => self
                .lock_slots
                .existing_slot(thread_id)
                .is_some_and(|slot| ObjectHeader::thin_lock_owner(mark) == slot),
            s if s == types::MARK_INFLATED => match self.cached_monitor_of_inflated(obj_ref) {
                // SAFETY: `cached_monitor_of_inflated`'s contract; `is_held_by`
                // is one load.
                Some(ptr) => unsafe { &*ptr }.is_held_by(thread_id),
                None => self
                    .lookup_indexed(obj_ref)
                    .is_some_and(|m| m.is_held_by(thread_id)),
            },
            _ => false,
        }
    }

    /// `holds` / `notify` / `notifyAll`'s monitor for an object whose word
    /// reads INFLATED, from this thread's [`INFLATED_MONITOR_CACHE`] -- no
    /// shard lock, no `Arc` traffic -- or `None` (a word that is not
    /// INFLATED, a cache miss, or `CRATONVM_MONITOR_CACHED_NOTIFY=0`,
    /// [`MonitorTuning::cached_notify`]), and the caller takes the index
    /// route as before round 13 wave 10.
    ///
    /// The pointer is [`Self::cached_inflated_monitor`]'s, under its
    /// conditions, which the caller must meet: `obj_ref` is a live object the
    /// caller holds, and the caller uses the pointer only before it next
    /// reaches a safepoint or enters a GC-blocked region -- the conditions
    /// `exit_reporting_release` already relies on in the same callers.
    #[inline]
    fn cached_monitor_of_inflated(&self, obj_ref: ObjectRef) -> Option<*const Monitor> {
        if !self.tuning.cached_notify {
            return None;
        }
        let mark = header_of(obj_ref).mark_word.load(Ordering::Acquire);
        if ObjectHeader::mark_state(mark) != types::MARK_INFLATED {
            return None;
        }
        self.cached_inflated_monitor(obj_ref.as_ptr() as usize)
    }

    /// The thin-lock lease compiled code on `thread_id` locks under -- its
    /// owner bits and its `LeaseBlock`'s address (its `acquired` count first,
    /// then the inflated-monitor cache) -- leasing a slot if the thread has
    /// none yet. `None` when every slot is leased.
    ///
    /// For `ThreadRegistry::arm_jit_monitor_block` only, on the thread's OWN
    /// behalf: the lease lasts until [`Self::release_monitors_held_by_except`]
    /// gives it back at the thread's death, and the counter address is stable
    /// for this table's lifetime, which is the VM's.
    pub(crate) fn jit_thin_lease(&self, thread_id: ThreadId) -> Option<JitThinLease> {
        self.lock_slots.jit_lease(thread_id)
    }

    /// Give a DEAD thread's lock-slot lease back, if no thin lock is counted
    /// under it any more.
    ///
    /// A thread that dies holding a thin lock keeps its lease
    /// ([`LockSlots::release`] at [`Self::release_monitors_held_by_except`]),
    /// and until wave 15 kept it for the VM's life: the contender that later
    /// inflated the lock (seeding the monitor from the lease, then
    /// uncounting it) and force-released the dead owner
    /// (`vm_exec::release_dead_thin_owner`) never gave the slot back, so each
    /// such death leaked one of the 2048 slots. The count is exact, so once it
    /// reads 0 no mark word names the slot and the lease can go; while any
    /// other thin lock of the dead thread is still counted it stays.
    ///
    /// Only for a thread that will never run again (`is_registered_dead`):
    /// a live thread must keep its lease, which its compiled code and its
    /// thread-local slot cache both assume.
    pub(crate) fn reclaim_dead_thread_lease(&self, thread_id: ThreadId) {
        self.lock_slots.release(thread_id);
    }

    /// Run `park` -- a running park, [`Monitor::park_enter_while`] with "no
    /// pause is requested" as its `keep_parking` -- counted in
    /// [`Self::lazy_parkers`], so [`Self::wake_lazy_parkers`] knows to look.
    ///
    /// The count is raised, then a `SeqCst` fence, before `park` first reads
    /// `stw_requested` (under the monitor's state lock, before each wait); a
    /// pause initiator raises `stw_requested`, fences, then reads the count.
    /// So either the parker sees the request and does not wait, or the
    /// initiator sees the parker and notifies its monitor's `entry_condvar`
    /// under the same state lock -- which the parker holds from its check
    /// until it is inside the wait. Round 12 wave 1 (lane lock, W19-1).
    pub(crate) fn lazily_parked<R>(&self, park: impl FnOnce() -> R) -> R {
        struct Leave<'a>(&'a AtomicU64);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.lazy_parkers.fetch_add(1, Ordering::SeqCst);
        let _leave = Leave(&self.lazy_parkers);
        std::sync::atomic::fence(Ordering::SeqCst);
        park()
    }

    /// Wake every thread in the running park, so a stop-the-world pause does
    /// not wait out its budget (`CRATONVM_MONITOR_LAZY_PARK_US`; on Windows a
    /// timed wait is rounded up to the 15.6 ms clock tick). Each parker, woken,
    /// re-reads `stw_requested`, leaves the running park and takes the
    /// GC-blocked one, whose root deposit the pause then uses (round 12 wave
    /// 1, lane lock, proposal W19-1).
    ///
    /// For the pause INITIATOR, right after it has raised `stw_requested`
    /// (see [`Self::lazily_parked`] for the handshake). One fence and one
    /// load when nobody parks lazily -- always, while the arm is off. A
    /// full (GC-blocked) parker on the same monitor gets a spurious wake-up,
    /// which it absorbs by re-trying its CAS and waiting again.
    ///
    /// Wiring: `r12w1-lock-stw-initiator-wakes-lazy-monitor-parkers-patch`
    /// (the two `request_stw_opening_cycle` winners in
    /// `runtime/interpreter/gc_and_alloc.rs`).
    pub fn wake_lazy_parkers(&self) {
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.lazy_parkers.load(Ordering::SeqCst) == 0 {
            return;
        }
        for shard in self.monitors.iter() {
            // Collected first: a monitor's state lock is never taken under
            // an index shard lock.
            let parked: Vec<Arc<Monitor>> = shard
                .lock()
                .values()
                .filter(|m| m.entry_waiters.load(Ordering::SeqCst) != 0)
                .cloned()
                .collect();
            for m in parked {
                let _state = m.state.lock();
                m.entry_condvar.notify_all();
            }
        }
    }

    /// Execute a closure while holding a per-object CAS lock.
    ///
    /// Provides mutual exclusion for non-atomic compare-and-swap emulation.
    /// Each object gets its own lock (lazily created), so CAS operations on
    /// different objects do not contend.
    ///
    /// Unlike monitors, CAS locks have no mark-word home, so the lookup is
    /// necessarily address-keyed — but it is sharded, so two CAS operations on
    /// objects in different shards no longer serialize on one process-wide
    /// mutex. The shard guard is dropped before the per-object lock is taken
    /// (both are L6-adjacent; the inner `Mutex<()>` is untracked and must never
    /// be held while re-entering the shard).
    pub fn with_cas_lock<F, R>(&self, obj_ref: ObjectRef, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let key = obj_ref.as_ptr() as usize;
        let epoch = self.cas_lock_epoch.load(Ordering::Acquire);
        let slot = (key >> 3) & (CAS_LOCK_CACHE_SLOTS - 1);
        let cached = CAS_LOCK_CACHE.with(|cache| {
            let entry = cache.borrow()[slot];
            (entry.table_id == self.cas_lock_cache_table_id
                && entry.epoch == epoch
                && entry.object_key == key
                && !entry.lock.is_null())
            .then_some(entry.lock)
        });
        if let Some(lock) = cached {
            // SAFETY: the registry owns this mutex for the whole matching
            // epoch. Every path that can drop/re-key an entry bumps
            // `cas_lock_epoch` while mutators are stopped before they may
            // observe the changed registry.
            let _guard = unsafe { &*lock }.lock();
            return f();
        }
        let lock = {
            let mut cas = self.cas_locks[shard_of(key)].lock();
            cas.entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let lock_ptr = Arc::as_ptr(&lock);
        CAS_LOCK_CACHE.with(|cache| {
            cache.borrow_mut()[slot] = CasLockCacheEntry {
                table_id: self.cas_lock_cache_table_id,
                epoch,
                object_key: key,
                lock: lock_ptr,
            };
        });
        let _guard = lock.lock();
        f()
    }

    /// Test-only inspection helper: return the current owner `ThreadId` of
    /// the monitor for `obj_ref`, or `None` if the monitor is currently
    /// unowned.
    ///
    /// For the thin-lock fast path the answer is derived from the mark word
    /// directly (no allocation, no inflation). For the inflated path we look
    /// up the registered `Arc<Monitor>` and read its owner field under the
    /// monitor's state mutex. Returns `None` if the object has never been
    /// entered.
    ///
    /// Used by `vm/tests/monitor_stress.rs` to assert the final monitor
    /// state after a multi-threaded stress run completes. Not on the hot
    /// path of the interpreter / native dispatch.
    pub fn current_owner(&self, obj_ref: ObjectRef) -> Option<ThreadId> {
        let header = header_of(obj_ref);
        let mark = header.mark_word.load(Ordering::Acquire);
        match ObjectHeader::mark_state(mark) {
            s if s == types::MARK_THIN_LOCKED => self
                .lock_slots
                .owner_of(ObjectHeader::thin_lock_owner(mark)),
            s if s == types::MARK_INFLATED => {
                self.lookup_indexed(obj_ref).and_then(|m| m.current_owner())
            }
            _ => None,
        }
    }

    /// Test-only inspection helper: return the current re-entry count of
    /// the monitor for `obj_ref`. Returns `0` if the monitor is unowned,
    /// has never been entered, or is currently in the NEUTRAL mark state.
    ///
    /// Thin-locked monitors report `recursion + 1` (the mark word encodes
    /// the *additional* recursive acquires beyond the initial one, so a
    /// freshly thin-locked object has recursion=0 / entry_count=1).
    /// Inflated monitors report the raw `entry_count` field.
    ///
    /// Used by `vm/tests/monitor_stress.rs` to assert balanced
    /// enter/exit pairs after multi-threaded contention.
    pub fn entry_count(&self, obj_ref: ObjectRef) -> u32 {
        let header = header_of(obj_ref);
        let mark = header.mark_word.load(Ordering::Acquire);
        match ObjectHeader::mark_state(mark) {
            s if s == types::MARK_THIN_LOCKED => ObjectHeader::thin_lock_recursion(mark) + 1,
            s if s == types::MARK_INFLATED => self
                .lookup_indexed(obj_ref)
                .map_or(0, |m| m.held_entry_count()),
            _ => 0,
        }
    }

    /// Release every inflated monitor still owned by `thread_id`. Called
    /// once from `ThreadRegistry::mark_dead` when a Java thread terminates.
    ///
    /// A thread normally releases every monitor it holds via ordinary
    /// `monitorexit` bytecode (including on the exceptional path, via the
    /// method's exception table) before it can ever finish running — but a
    /// thread that is interrupted or otherwise torn down while blocked
    /// inside a *native* call made from within a `synchronized` region never
    /// executes that bytecode. Without this sweep, such a monitor stays
    /// "held" by a thread ID that will never call `exit`/`notify` again,
    /// and every future `monitorenter` on that same object blocks forever.
    /// Only inflated monitors are covered (this is a registry walk, not a
    /// heap scan) — an uncontended thin lock still held by a dead thread is
    /// a separate, rarer gap (nothing else was contending it, so nothing
    /// else is blocked on it either).
    pub fn release_monitors_held_by(&self, thread_id: ThreadId) {
        self.release_monitors_held_by_except(thread_id, None);
    }

    /// Same as [`Self::release_monitors_held_by`], but leaves `except` alone
    /// even if `thread_id` currently owns it.
    ///
    /// Used by the `Thread.join()` termination-notify sequence (WP4.1,
    /// `vm_exec.rs`'s `thread_start` spawn closure): the terminating thread
    /// deliberately acquires and holds its own Java `Thread` mirror's monitor
    /// across `mark_dead`/this sweep so it can safely `notify_all()` any
    /// `Thread.join()` waiters afterward. Without this exclusion, the blanket
    /// sweep force-releases that monitor too (it IS owned by `thread_id` at
    /// this point) — the owner word goes back to free — so the subsequent
    /// `Monitor::notify_all`/`exit` calls both fail `NotOwner` (silently
    /// discarded by the `let _ =` caller) and `wait_condvar.notify_all()` is
    /// never invoked. Every joiner parked in `Object.wait()` then hangs
    /// forever even though the joined thread's `alive` flag is already false
    /// (the exact "worker alive=false, joiner stuck in `Thread.join()`"
    /// lost-wakeup signature).
    pub fn release_monitors_held_by_except(
        &self,
        thread_id: ThreadId,
        except: Option<&Arc<Monitor>>,
    ) {
        // Round 12 wave 1 (lane lock): the thread-termination caller runs this
        // AFTER leaving the GC barrier's transition lock (it used to run inside
        // `BlockedGuard::finish_after`, holding the lock every blocked-region
        // enter and leave in the VM takes, for a walk of every inflated
        // monitor -- per thread death), and a dead thread is not counted by a
        // pause, so a `remap_after_gc` can drain the index under this walk. A
        // walk that overlapped a drain (`remap_seq` odd, or changed) may have
        // missed a monitor this thread owns, so it waits the drain out and
        // walks again. Bounded: a remap is a stop-the-world pass, and every
        // attempt -- the last included -- releases everything it saw.
        const SWEEP_ATTEMPTS: u32 = 64;
        const DRAIN_WAITS: u32 = 1 << 20;
        note_contention(ContentionEvent::DeathSweep);
        let mut attempts = 0;
        loop {
            let mut seq = self.remap_seq.load(Ordering::Acquire);
            let mut waits = 0;
            while seq & 1 != 0 && waits < DRAIN_WAITS {
                std::thread::yield_now();
                waits += 1;
                seq = self.remap_seq.load(Ordering::Acquire);
            }
            self.force_release_all_held_by(thread_id, except);
            attempts += 1;
            let clean = seq & 1 == 0 && self.remap_seq.load(Ordering::Acquire) == seq;
            if clean || attempts >= SWEEP_ATTEMPTS {
                break;
            }
        }
        // The thread is dying: give its thin-lock slot back (kept if it still
        // holds a thin lock -- see `LockSlots::release`).
        self.lock_slots.release(thread_id);
    }

    /// One walk of [`Self::release_monitors_held_by_except`]: force-release
    /// every indexed monitor `thread_id` owns, `except` aside. Cold: once per
    /// thread death. One shard at a time -- two L6 guards must never be live
    /// simultaneously (equal-level nesting is a lock-order violation).
    fn force_release_all_held_by(&self, thread_id: ThreadId, except: Option<&Arc<Monitor>>) {
        let (mut visits, mut releases) = (0u64, 0u64);
        for shard in self.monitors.iter() {
            let guard = shard.lock();
            visits += guard.len() as u64;
            for monitor in guard.values() {
                if let Some(exc) = except {
                    if Arc::ptr_eq(monitor, exc) {
                        continue;
                    }
                }
                if monitor.force_release_if_owned_by(thread_id) {
                    releases += 1;
                }
            }
        }
        note_contention_n(ContentionEvent::DeathSweepVisit, visits);
        note_contention_n(ContentionEvent::DeathSweepRelease, releases);
    }

    /// Remap monitor keys after GC has moved objects.
    ///
    /// Takes a mapping from old pointer addresses to new pointer addresses.
    /// Re-keys the internal HashMap so monitors remain associated with the
    /// correct (now-relocated) objects.
    ///
    /// PERF (leak bounding): historically every inflated `Monitor` and every
    /// per-object `cas_lock` was inserted on first use and **never removed**, so
    /// the registries grew monotonically for the VM lifetime — each moving GC's
    /// remap then walked an ever-larger table even though most keyed objects
    /// were long dead. This pass drops the `cas_locks` entries that can be
    /// dropped *without ever losing live lock state*. Inflated monitors are only
    /// re-keyed here, never dropped — see below.
    ///
    /// SAFETY of reclamation (why this never drops a live monitor):
    ///
    /// * `cas_locks` are **always** safe to prune when idle. A CAS lock has no
    ///   back-reference from the object header (unlike an inflated monitor,
    ///   whose address is published into the mark word), so removing it is
    ///   invisible to the rest of the VM — `with_cas_lock` simply re-creates an
    ///   equivalent fresh `Mutex` on the next access. We drop an entry only when
    ///   `Arc::strong_count == 1`, i.e. the registry holds the *sole* reference
    ///   and no thread is currently inside `with_cas_lock` holding a clone. An
    ///   entry that is in use (`strong_count > 1`) is always retained/re-keyed.
    ///
    /// * Inflated `monitors` are **never reclaimed here** — only re-keyed.
    ///
    ///   ARCH-2026-07-26, and this is the load-bearing part: reclaiming on the
    ///   "absent from `pointer_map` ⇒ dead" signal was already wrong, and is
    ///   now unsafe. `pointer_map` lists only *moved* objects, so for a
    ///   **partial** collector (G1 young/mixed, the generational minor GC) a
    ///   live old-gen / non-CSet survivor is legitimately absent. Dropping its
    ///   entry used to leave the survivor's `INFLATED` mark word pointing at a
    ///   freed `Monitor`; the old code got away with never dereferencing that
    ///   pointer only because every lookup went through this map (and instead
    ///   re-inflated a SECOND monitor, orphaning every waiter on the first).
    ///   Now that the mark word IS what locking dereferences, the same drop
    ///   would be a use-after-free.
    ///
    ///   So the branch is gone, together with its `CRATONVM_RECLAIM_DEAD_MONITORS`
    ///   opt-in — which was default-OFF and had therefore never run, so nothing
    ///   is lost. Sound reclamation needs an EXACT dead set, which is exactly
    ///   what [`cratonvm_gc::MonitorCleanup::prune_dead`] is given; that path is
    ///   unconditional, on by default, and releases the mark-word reference too,
    ///   so it genuinely frees.
    ///
    ///   The moving collectors that hand over no dead set (Generational, G1)
    ///   get theirs reclaimed after this call instead, in the VM's
    ///   stop-the-world epilogue, where the heap's in-place liveness verdict
    ///   is available: [`Self::prune_stale_after_gc`] (round 12 wave 2). This
    ///   method records the addresses it re-keys entries to
    ///   ([`Self::remapped_keys`]) so that pass keeps them without asking.
    pub fn remap_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        // Mutators are stopped for this whole operation. Invalidate cached raw
        // CAS-lock pointers before an idle entry can be dropped or a moved one
        // re-keyed below.
        self.cas_lock_epoch.fetch_add(1, Ordering::Release);
        // The same for the inflated-monitor index cache (`INFLATED_MONITOR_CACHE`):
        // entries are about to be re-keyed or removed.
        self.note_index_mutation();
        // SHARDING + LOCK ORDER: every shard of both registries is L6, and the
        // hierarchy forbids equal-level nesting, so we may never hold two shard
        // guards at once. Re-keying can also move an entry to a DIFFERENT shard
        // (the key changes), which rules out a simple in-place per-shard
        // drain/refill. Both walks below therefore run in two phases:
        //   phase 1 — lock one shard, drain it, release, classify;
        //   phase 2 — lock the destination shard for each surviving entry and
        //             insert.
        // Safe to do non-atomically because `remap_after_gc` is only ever
        // called by the collector under stop-the-world (see
        // `cratonvm_gc::collector::StopTheWorldToken`), so no mutator can
        // observe the intermediate state. (CORRECTION, round 11 wave 17: since
        // the 8-byte header the index IS how an inflated monitor is found, so
        // an intermediate state a mutator could see would not be benign; the
        // stop-the-world guarantee is what makes this sound, and the
        // `index_repair_if_absent` this comment used to lean on had no caller
        // and is gone.)
        {
            // Two lists, inserted in this order, so a MOVED entry wins over a
            // retained one at the same address. Since the 8-byte header the
            // index is the only home of an inflated monitor, and a live object
            // can be moved onto the address of a dead one whose stale entry is
            // retained below; letting the stale entry win would hand the live
            // object a dead object's monitor.
            let mut retained: Vec<(usize, Arc<Monitor>)> = Vec::new();
            let mut moved: Vec<(usize, Arc<Monitor>)> = Vec::new();
            // Odd from before the first drain until after the last refill:
            // a concurrent `release_monitors_held_by_except` walk that saw a
            // drained shard sees this change and walks again.
            self.remap_seq.fetch_add(1, Ordering::AcqRel);
            for shard in self.monitors.iter() {
                let entries: Vec<(usize, Arc<Monitor>)> = shard.lock().drain().collect();
                for (old_key, monitor) in entries {
                    match pointer_map.get(&old_key).copied() {
                        Some(new_key) => {
                            // Object survived AND moved — re-key: the index is
                            // how locking finds the monitor.
                            moved.push((new_key, monitor));
                        }
                        None => {
                            // Absent from the forwarding map. That is NOT proof
                            // of death under a partial collector — a live
                            // in-place survivor is legitimately absent — and its
                            // mark word still names this monitor. Retain it
                            // under its (unchanged) address. See the safety note
                            // on this method for why reclaiming here would lose
                            // a live object's monitor.
                            retained.push((old_key, monitor));
                        }
                    }
                }
            }
            let moved_to: Vec<usize> = moved.iter().map(|(key, _)| *key).collect();
            for (key, monitor) in retained.into_iter().chain(moved) {
                self.monitor_shard(key).lock().insert(key, monitor);
            }
            self.remap_seq.fetch_add(1, Ordering::AcqRel);
            // For `prune_stale_after_gc` (round 12 wave 2): these entries name
            // relocated survivors. No shard lock is held here.
            if !moved_to.is_empty() && monitor_index_prune_enabled() {
                let mut remapped = self.remapped_keys.lock();
                if remapped.len() + moved_to.len() > REMAPPED_KEYS_CAP {
                    remapped.clear();
                } else {
                    remapped.extend_from_slice(&moved_to);
                }
            }
            // And once more after the refill, so no cache entry taken while the
            // index was half-drained survives it.
            self.note_index_mutation();
        }

        // Also remap CAS locks — same two-phase, one-shard-at-a-time shape.
        //
        // PERF: CAS locks carry no object-header back-reference, so an idle one
        // is always safe to drop and is transparently re-created by the next
        // `with_cas_lock`. Prune every entry that survived-as-dead (absent from
        // `pointer_map`) AND is unreferenced (`strong_count == 1`), independent
        // of the monitor-reclaim flag — this is unconditionally correct for all
        // collectors. A surviving-and-moved lock is re-keyed; an in-use lock
        // (`strong_count > 1`, i.e. a thread is inside `with_cas_lock`) is kept.
        {
            let mut survivors: Vec<(usize, Arc<Mutex<()>>)> = Vec::new();
            for shard in self.cas_locks.iter() {
                let entries: Vec<(usize, Arc<Mutex<()>>)> = shard.lock().drain().collect();
                for (old_key, mut lock) in entries {
                    match pointer_map.get(&old_key).copied() {
                        Some(new_key) => {
                            // Object moved — re-key so a future CAS on the same
                            // (relocated) object reuses the same lock.
                            survivors.push((new_key, lock));
                        }
                        None => {
                            // Absent. For cas_locks this is *always* safe to
                            // treat as reclaimable when unreferenced, even under
                            // a partial collector: dropping a live-but-idle
                            // object's cas_lock only forces a cheap lazy
                            // re-create on the next CAS, it never desyncs any
                            // header state. Keep it only if a thread is
                            // currently using it.
                            if Arc::get_mut(&mut lock).is_none() {
                                survivors.push((old_key, lock));
                            }
                            // else: drop the sole `Arc` — reclaimed.
                        }
                    }
                }
            }
            for (key, lock) in survivors {
                self.cas_locks[shard_of(key)].lock().insert(key, lock);
            }
        }
    }
}

impl Default for MonitorTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Which of `keys` (a registry snapshot, a handful of entries) are elements of
/// `dead` (every address a collection swept, often millions)?
///
/// `dead_sorted` must be true only if `dead` is ascending; then each key is a
/// binary search of `dead`. Otherwise `dead` is scanned once against the sorted
/// key snapshot. Either way no lock is taken and no hash is computed per dead
/// address. The result may be in any order; each dead key appears at most once
/// per occurrence in `dead` (duplicates are harmless -- the second `remove`
/// finds nothing).
fn dead_keys_among(dead: &[usize], dead_sorted: bool, mut keys: Vec<usize>) -> Vec<usize> {
    if dead_sorted {
        keys.retain(|k| dead.binary_search(k).is_ok());
        keys
    } else {
        keys.sort_unstable();
        keys.dedup();
        let mut hits = Vec::new();
        if keys.is_empty() {
            return hits;
        }
        for d in dead {
            if keys.binary_search(d).is_ok() {
                hits.push(*d);
            }
        }
        hits
    }
}

impl cratonvm_gc::MonitorCleanup for MonitorTable {
    fn remap_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        self.remap_after_gc(pointer_map);
    }

    /// Whole-heap dead-address prune (ZGC backend — see the trait doc).
    /// `dead` is EXACT (every element's object was just swept), so removal
    /// is unconditional: a thread still blocked on a dead object's monitor
    /// holds its own `Arc<Monitor>` clone (dropping the index entry and the
    /// mark-word reference cannot free it under that thread), and no future
    /// locker can exist for a dead object — while a NEW object reusing the
    /// address MUST get a fresh monitor, not the dead object's.
    ///
    /// ARCH-2026-07-26: this now also releases the mark-word-owned strong
    /// reference. That is exactly the case the exactness of `dead` licenses —
    /// the object's memory is already freed, so nothing can load its mark word
    /// again — and it is what turns this from "drop one of two references"
    /// (a leak) into a real reclaim.
    fn prune_dead(&self, dead: &[usize]) {
        if dead.is_empty() {
            return;
        }
        // The exact-dead-set backend (ZGC) never runs `prune_stale_after_gc`,
        // which consumes this list; keep it to one pause's worth.
        self.remapped_keys.lock().clear();
        // `dead` is processed under the collector's stop-the-world token, so
        // no mutator can retain a cache hit while its registry owner is
        // removed. The next mutator observation must use a fresh lookup.
        //
        // Bumped unconditionally, BEFORE the survey below decides whether any
        // removal is possible. One relaxed atomic add is not worth reasoning
        // about whether a mutator can hold a cached handle for an address the
        // sweep has just recycled.
        self.cas_lock_epoch.fetch_add(1, Ordering::Release);
        // The same for the inflated-monitor index cache (`INFLATED_MONITOR_CACHE`):
        // entries are about to be re-keyed or removed.
        self.note_index_mutation();
        // Which shards hold anything at all, asked ONCE.
        //
        // `dead` is every address the sweep freed, so on an allocation-heavy
        // workload it is millions of entries per cycle, while the number of
        // INFLATED monitors is usually zero and never more than a handful --
        // inflation needs real contention. The loops below used to lock a
        // shard and hash a key for every one of those addresses, in both
        // registries, to remove nothing: `MonitorTable::prune_dead` measured
        // 5.36% of `LegendreHighPrecisionTest` and 4.98% of
        // `PSquarePercentileTest`, two workloads with no contended monitor in
        // them at all.
        //
        // 64 shards, so this costs at most 128 uncontended lock/unlock pairs
        // and answers the only question that matters: an empty shard cannot
        // contain any dead address, so every key hashing to it can be skipped
        // without taking its lock. When nothing is inflated -- the common case
        // -- the whole prune becomes those 128 pairs instead of `2 * dead.len()`.
        //
        // Sound because this runs under the collector's stop-the-world token:
        // no mutator can inflate a monitor between the survey and the loops,
        // so a shard observed empty stays empty for the duration.
        let mut monitors_nonempty = [false; MONITOR_SHARDS];
        let mut cas_nonempty = [false; MONITOR_SHARDS];
        let mut monitor_entries = 0usize;
        let mut cas_entries = 0usize;
        for i in 0..MONITOR_SHARDS {
            let m = self.monitors[i].lock().len();
            let c = self.cas_locks[i].lock().len();
            monitors_nonempty[i] = m != 0;
            cas_nonempty[i] = c != 0;
            monitor_entries += m;
            cas_entries += c;
        }
        if monitor_entries == 0 && cas_entries == 0 {
            return;
        }
        // INVERTED ARM (round 9 wave 10, zgc10). One inflated monitor or one
        // `cas_lock` entry anywhere used to turn this back into `dead.len()`
        // locked hash probes: `prune_dead` measured 12-15 % of all busy
        // samples on `IrAllocLoop` (a single-threaded list build), more than
        // the compiled loop itself. The registries hold a handful of entries
        // against millions of dead addresses, so walk the ENTRIES instead:
        // snapshot each non-empty shard's keys (one lock per shard), find which
        // of them are dead, and lock only the shards that hold a hit.
        //
        // Membership: the ZGC sweep emits `dead` in ascending address order
        // (bitmap scan order within a shard, `ZSweepShard::absorb` preserves it
        // across shards), so a binary search of `dead` per key is O(k log n).
        // That order is CHECKED here, not assumed: one sequential pass over
        // `dead` costs far less than a single locked hash probe per element,
        // and an unordered slice falls back to a lock-free scan of `dead`
        // against the sorted key snapshot -- still no lock per address.
        //
        // Sound for the same reason as the survey: under the collector's
        // stop-the-world token no mutator can add, remove or re-key an entry
        // between the snapshot and the removal, and removal of a hit is the
        // same `remove` + `release_mark_ref` as the per-address arm.
        let dead_sorted =
            monitor_entries.max(cas_entries) < dead.len() && dead.windows(2).all(|w| w[0] <= w[1]);
        if monitor_entries != 0 {
            if monitor_entries < dead.len() {
                let mut keys: Vec<usize> = Vec::with_capacity(monitor_entries);
                for (i, shard) in self.monitors.iter().enumerate() {
                    if monitors_nonempty[i] {
                        keys.extend(shard.lock().keys().copied());
                    }
                }
                for d in dead_keys_among(dead, dead_sorted, keys) {
                    let removed = self.monitor_shard(d).lock().remove(&d);
                    if let Some(monitor) = removed {
                        // SAFETY: as in the per-address arm below -- `d` is an
                        // element of the EXACT dead set, so its memory is freed
                        // and no thread can read its mark word again.
                        unsafe { Monitor::release_mark_ref(&monitor) };
                    }
                }
            } else {
                // Group by shard so each shard is locked once; never two at a time.
                for d in dead {
                    if !monitors_nonempty[shard_of(*d)] {
                        continue;
                    }
                    let removed = self.monitor_shard(*d).lock().remove(d);
                    if let Some(monitor) = removed {
                        // SAFETY: `dead` is documented EXACT — this address was a
                        // live allocation base before this collection and its
                        // memory is now freed, so no thread can read its mark word.
                        // Any thread still parked on the monitor holds its own
                        // `Arc` clone, so this is not the last drop.
                        unsafe { Monitor::release_mark_ref(&monitor) };
                    }
                }
            }
            // After the removals too: no `INFLATED_MONITOR_CACHE` entry filled
            // while they ran may outlive the monitor it names.
            self.note_index_mutation();
        }
        if cas_entries != 0 {
            if cas_entries < dead.len() {
                let mut keys: Vec<usize> = Vec::with_capacity(cas_entries);
                for (i, shard) in self.cas_locks.iter().enumerate() {
                    if cas_nonempty[i] {
                        keys.extend(shard.lock().keys().copied());
                    }
                }
                for d in dead_keys_among(dead, dead_sorted, keys) {
                    self.cas_locks[shard_of(d)].lock().remove(&d);
                }
            } else {
                for d in dead {
                    if !cas_nonempty[shard_of(*d)] {
                        continue;
                    }
                    self.cas_locks[shard_of(*d)].lock().remove(d);
                }
            }
        }
    }

    /// The shard survey above, asked BEFORE the collector builds the slice.
    ///
    /// Same question, the same 128 uncontended lock/unlock pairs, and the same
    /// soundness argument (under a stop-the-world token a shard observed empty
    /// stays empty until the prune) — only now the collector can skip filling
    /// a `Vec<usize>` with millions of addresses that would have removed
    /// nothing. See [`cratonvm_gc::MonitorCleanup::wants_dead_addresses`] for
    /// the size of what that saves.
    ///
    /// Deliberately not memoised: it is 128 lock pairs once per collection,
    /// and a cached answer would be one more thing that can go stale across a
    /// safepoint.
    fn wants_dead_addresses(&self) -> bool {
        for i in 0..MONITOR_SHARDS {
            if !self.monitors[i].lock().is_empty() || !self.cas_locks[i].lock().is_empty() {
                return true;
            }
        }
        false
    }
}

impl MonitorTable {
    /// Number of inflated monitors currently in the enumeration index, summed
    /// across shards (one shard locked at a time — never two).
    ///
    /// Diagnostic only. It is a lower bound on "monitors that exist": a monitor
    /// whose index entry was pruned while its object was in fact alive is still
    /// perfectly usable through its mark word, and is re-indexed on next use.
    pub fn indexed_monitor_count(&self) -> usize {
        self.monitors.iter().map(|s| s.lock().len()).sum()
    }

    /// Drop the index entries a collection proved stale: the monitors of
    /// objects that died, which the Generational and G1 backends never report
    /// (their `MonitorCleanup` hands over a relocation map, not a dead set, and
    /// [`Self::remap_after_gc`] must retain every key absent from it). Without
    /// this every inflated object's `Monitor` -- every contended lock, every
    /// `wait`, every dead thread's `Thread` mirror -- stayed in the index until
    /// the VM exited, and every thread death walked all of them
    /// ([`Self::release_monitors_held_by_except`]).
    /// Page `r12w1-lock-monitor-index-never-pruned-under-moving-collectors`.
    ///
    /// Call only inside the stop-the-world epilogue of a collection, after the
    /// collector's `remap_after_gc`, before anything allocates. `survived(a)`
    /// is the collection's in-place verdict for an address it did NOT
    /// relocate (`addr_keyed::InPlaceVerdict::survived`), and must answer true
    /// only where the heap parses an object header. An entry is dropped when
    /// all of these hold:
    ///
    /// * its key is not one `remap_after_gc` re-keyed an entry to since its
    ///   shard was last judged ([`Self::remapped_keys`]: a relocated survivor);
    /// * the object at the key did not survive in place, OR it did and its
    ///   mark word reads NEUTRAL or THIN_LOCKED -- a live object's own monitor
    ///   leaves its word INFLATED (there is no deflation), so the entry is a
    ///   dead predecessor's at a reused address;
    /// * nothing but the index references the monitor, and it is unowned with
    ///   no entrant or spinner ([`Monitor::reclaimable_index_entry`]). A thread
    ///   parked on or waiting in a monitor holds its own `Arc`, and an owned
    ///   monitor stays for the death sweep.
    ///
    /// Each call judges [`PRUNE_SHARDS_PER_PAUSE`] of the index's shards, in
    /// rotation, so a dead object's entry leaves within eight collections and
    /// no pause pays for the whole index. Three passes per shard, never two
    /// shard guards at once and no shard guard while the verdict runs (it
    /// takes the old-gen lock): collect the unreferenced entries, judge them
    /// unlocked, then re-check and remove. Only unowned entries leave, so a
    /// dying thread's concurrent sweep (which runs outside the barrier) cannot
    /// miss a monitor it owns. Returns how many entries were dropped. Round 12
    /// wave 2 (lane lock).
    pub(crate) fn prune_stale_after_gc(&self, survived: &dyn Fn(usize) -> bool) -> usize {
        let first = (self
            .prune_cursor
            .fetch_add(PRUNE_SHARDS_PER_PAUSE as u64, Ordering::Relaxed)
            % MONITOR_SHARDS as u64) as usize;
        self.prune_stale_in_shards(first, PRUNE_SHARDS_PER_PAUSE, survived)
    }

    /// [`Self::prune_stale_after_gc`] over the `count` shards from `first`
    /// (wrapping). The re-keyed addresses of those shards are consumed; the
    /// others' wait in [`Self::remapped_keys`] for their turn.
    fn prune_stale_in_shards(
        &self,
        first: usize,
        count: usize,
        survived: &dyn Fn(usize) -> bool,
    ) -> usize {
        let (first, count) = (first % MONITOR_SHARDS, count.min(MONITOR_SHARDS));
        let judged = |shard: usize| (shard + MONITOR_SHARDS - first) % MONITOR_SHARDS < count;
        let mut relocated: Vec<usize> = {
            let mut keys = self.remapped_keys.lock();
            let (mine, rest): (Vec<usize>, Vec<usize>) =
                keys.drain(..).partition(|key| judged(shard_of(*key)));
            *keys = rest;
            mine
        };
        if !monitor_index_prune_enabled() {
            return 0;
        }
        relocated.sort_unstable();
        relocated.dedup();
        let mut removed = 0usize;
        for i in 0..count {
            let shard = &self.monitors[(first + i) % MONITOR_SHARDS];
            let candidates: Vec<(usize, usize)> = shard
                .lock()
                .iter()
                .filter(|(_, m)| Monitor::reclaimable_index_entry(m))
                .map(|(key, m)| (*key, Arc::as_ptr(m) as usize))
                .collect();
            let stale: Vec<(usize, usize)> = candidates
                .into_iter()
                .filter(|(key, _)| {
                    relocated.binary_search(key).is_err() && index_key_is_stale(*key, survived)
                })
                .collect();
            if stale.is_empty() {
                continue;
            }
            let mut dropped: Vec<Arc<Monitor>> = Vec::with_capacity(stale.len());
            {
                let mut guard = shard.lock();
                for (key, monitor) in stale {
                    let unchanged = guard.get(&key).is_some_and(|m| {
                        Arc::as_ptr(m) as usize == monitor && Monitor::reclaimable_index_entry(m)
                    });
                    if unchanged {
                        if let Some(m) = guard.remove(&key) {
                            dropped.push(m);
                        }
                    }
                }
            }
            removed += dropped.len();
            // The last references: freed here, after the shard guard.
            drop(dropped);
        }
        if removed != 0 {
            // No thread-local or compiled-code cache entry may name a dropped
            // monitor (`cached_inflated_monitor`, `LeaseBlock`).
            self.note_index_mutation();
        }
        note_contention_n(ContentionEvent::IndexPrune, removed as u64);
        removed
    }
}

/// Is the index entry at `key` provably not the monitor of a live object at
/// `key`? See [`MonitorTable::prune_stale_after_gc`] for `survived`'s contract.
fn index_key_is_stale(key: usize, survived: &dyn Fn(usize) -> bool) -> bool {
    if key == 0 || key & 7 != 0 {
        return false;
    }
    if !survived(key) {
        return true;
    }
    // SAFETY: `survived` answered true, so the heap parses an object header at
    // `key` (its contract), and the world is stopped: the load reads a live
    // object's mark word, or a word inside one.
    let mark = unsafe {
        (*(key as *const ObjectHeader))
            .mark_word
            .load(Ordering::Acquire)
    };
    let state = ObjectHeader::mark_state(mark);
    state == types::MARK_NEUTRAL || state == types::MARK_THIN_LOCKED
}

impl std::fmt::Debug for MonitorTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorTable")
            .field("active_monitors", &self.indexed_monitor_count())
            .field("shards", &MONITOR_SHARDS)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::ClassId;
    use crate::memory::heap::Heap;

    /// Helper to create a test object on the heap.
    ///
    /// Leaks the backing `Heap`: dropping it here would free its two
    /// (multi-MB) arenas out from under the returned `ObjectRef`, which
    /// every caller dereferences well after this function returns. That was
    /// a real, reproducible use-after-free — SIGSEGV inside
    /// `MonitorTable::enter_inflated_or_contend` -> `ensure_inflated` ->
    /// `header_of` reading the (freed) mark word — that crashed the test
    /// binary partway through this module's suite once the freed arena
    /// mapping got unmapped/reused. A tiny capacity keeps the per-call leak
    /// negligible (this helper is called from ~20 tests).
    fn test_object() -> ObjectRef {
        let heap: &'static Heap = Box::leak(Box::new(Heap::with_capacity(4096)));
        heap.alloc_object(ClassId::new(0), 0)
    }

    /// The class id [`compact_object`] allocates: one reference field.
    const COMPACT_TEST_CLASS: u32 = 900_777;

    /// A COMPACT instance (8-byte header): the shape whose identity hash lives
    /// in its NEUTRAL mark word, so hashing it and thin-locking it compete for
    /// the same word. A legacy (long-header) object keeps its hash in the aux
    /// word and can be hashed and thin-locked at once.
    fn compact_object(heap: &Heap) -> ObjectRef {
        static REGISTER: std::sync::Once = std::sync::Once::new();
        REGISTER.call_once(|| {
            cratonvm_types::register_class_layout(
                cratonvm_types::FIRST_LAYOUT_DOMAIN,
                COMPACT_TEST_CLASS,
                Arc::new(cratonvm_types::CompactLayout {
                    field_disps: vec![8],
                    is_ref: vec![true],
                    field_kinds: vec![cratonvm_types::FieldStorageKind::Reference],
                    ref_disps: vec![8],
                    total_size: 16,
                }),
            );
        });
        let obj = heap.alloc_object(ClassId::new(COMPACT_TEST_CLASS), 1);
        assert!(
            header_of(obj).is_short(),
            "precondition: the test class allocates compact"
        );
        obj
    }

    /// A thin lock/unlock round trip must leave the mark word's quartet
    /// (`kind` / `element_type` / `gc_age` / `gc_flags`) exactly as it found
    /// it.
    ///
    /// Those bits live in the mark word since the header shrank 24 -> 16, and
    /// the last release used to store the bare `MARK_NEUTRAL` constant over
    /// them. `GC_FLAG_COMPACT` is one of them, so the first `synchronized`
    /// block on a compact object converted it to "legacy" and every later field
    /// read decoded a compact-packed body with 16-byte cells.
    #[test]
    fn a_thin_lock_round_trip_preserves_the_mark_word_quartet() {
        use cratonvm_types::{GC_FLAG_COMPACT, GC_FLAG_OLD_GEN};

        let obj = test_object();
        let header = header_of(obj);
        header.set_gc_flags(GC_FLAG_COMPACT | GC_FLAG_OLD_GEN);
        let before = header.mark_word.load(Ordering::Relaxed);
        assert_eq!(
            header.gc_flags(),
            GC_FLAG_COMPACT | GC_FLAG_OLD_GEN,
            "precondition: the flags are in the mark word"
        );

        try_thin_lock(header, 7).expect("an unlocked, unhashed object thin-locks");
        assert_eq!(
            ObjectHeader::quartet_of(header.mark_word.load(Ordering::Relaxed)),
            ObjectHeader::quartet_of(before),
            "locking must carry the quartet"
        );

        // Recursive acquire/release must carry it too — that arm derives from
        // `cur`, but pin it so a future rewrite cannot regress silently.
        try_thin_recursive_lock(header, 7).expect("recursive acquire");
        try_thin_unlock(header, 7).expect("recursive release");
        assert_eq!(
            ObjectHeader::quartet_of(header.mark_word.load(Ordering::Relaxed)),
            ObjectHeader::quartet_of(before),
            "recursive release must carry the quartet"
        );

        assert_eq!(
            try_thin_unlock(header, 7),
            Ok(None),
            "the last release returns the lock to NEUTRAL"
        );
        let after = header.mark_word.load(Ordering::Relaxed);
        assert_eq!(
            ObjectHeader::mark_state(after),
            types::MARK_NEUTRAL,
            "and the state really is NEUTRAL"
        );
        assert_eq!(
            header.gc_flags(),
            GC_FLAG_COMPACT | GC_FLAG_OLD_GEN,
            "the LAST release is the one that used to erase the flags"
        );
        assert_eq!(
            ObjectHeader::quartet_of(after),
            ObjectHeader::quartet_of(before),
            "kind / element_type / gc_age must survive the unlock too"
        );
    }

    #[test]
    fn monitor_enter_exit_basic() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter and exit should succeed
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
    }

    /// Regression guard for the Jetty `Deflater.end()`/`DeflaterPool.end()`
    /// hang: a thread that dies while holding an inflated monitor (e.g.
    /// interrupted/torn down while blocked in a native call made from
    /// inside a `synchronized` region) must not permanently starve every
    /// future `monitorenter` on that object. Without
    /// `release_monitors_held_by`, thread B here would block forever.
    #[test]
    fn dead_thread_owned_monitor_is_released_and_future_enters_succeed() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid_a = ThreadId(1);
        let tid_b = ThreadId(2);

        let (monitor, contended) = table
            .enter_inflated_or_contend(obj, tid_a)
            .expect("inflate");
        assert!(!contended, "fresh monitor should be acquired immediately");
        assert!(monitor.is_held_by(tid_a));

        // Thread A "dies" without ever calling monitorexit.
        table.release_monitors_held_by(tid_a);
        assert!(!monitor.is_held_by(tid_a));

        // A different thread must now be able to acquire the same object's
        // monitor without blocking.
        table.enter(obj, tid_b);
        assert!(monitor.is_held_by(tid_b));
        assert!(table.exit(obj, tid_b).is_ok());
    }

    #[test]
    fn release_monitors_held_by_is_a_no_op_for_monitors_owned_by_other_threads() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid_a = ThreadId(1);
        let tid_b = ThreadId(2);

        let (monitor, _) = table
            .enter_inflated_or_contend(obj, tid_a)
            .expect("inflate");
        // Releasing a thread that owns nothing here must not disturb A's hold.
        table.release_monitors_held_by(tid_b);
        assert!(monitor.is_held_by(tid_a));
        assert!(table.exit(obj, tid_a).is_ok());
    }

    #[test]
    fn monitor_reentrant() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter twice, exit twice
        table.enter(obj, tid);
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_reentrant_deep() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter 10 times, exit 10 times
        for _ in 0..10 {
            table.enter(obj, tid);
        }
        for _ in 0..10 {
            assert!(table.exit(obj, tid).is_ok());
        }
    }

    #[test]
    fn monitor_exit_without_enter_fails() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Exit without entering should fail
        let result = table.exit(obj, tid);
        assert!(result.is_err());
    }

    #[test]
    fn monitor_exit_wrong_thread_fails() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid1 = ThreadId(1);
        let tid2 = ThreadId(2);

        // Thread 1 enters
        table.enter(obj, tid1);

        // Thread 2 tries to exit — should fail
        let result = table.exit(obj, tid2);
        assert!(result.is_err());

        // Thread 1 can still exit
        assert!(table.exit(obj, tid1).is_ok());
    }

    #[test]
    fn monitor_different_objects_independent() {
        let heap = Heap::new();
        let obj1 = heap.alloc_object(ClassId::new(0), 0);
        let obj2 = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();
        let tid = ThreadId(1);

        // Enter both objects
        table.enter(obj1, tid);
        table.enter(obj2, tid);

        // Exit them independently
        assert!(table.exit(obj2, tid).is_ok());
        assert!(table.exit(obj1, tid).is_ok());
    }

    #[test]
    fn monitor_reenter_after_release() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter and exit
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());

        // Enter again — should work
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_remap_after_gc() {
        // With thin locks, the mark word travels with the object bytes during
        // a real GC compaction (the GC must copy the entire ObjectHeader). The
        // `remap_after_gc` path only matters for the INFLATED-monitor registry,
        // so we first force inflation via wait(), then verify the registry
        // entry follows the object to its new address.
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter and force inflation by calling wait() (which requires a
        // heavyweight monitor with condvars). Use a tiny timeout so the test
        // is not slow.
        table.enter(obj, tid);
        table.wait(obj, tid, Some(1), None).unwrap();
        // Now the object's mark word is INFLATED and the registry holds the
        // Arc<Monitor>.

        // Simulate GC moving the object to a new address, AND copy the mark
        // word bytes (this is what a real semi-space copy does).
        let old_addr = obj.as_ptr() as usize;
        let heap2 = Heap::new();
        let new_obj = heap2.alloc_object(ClassId::new(0), 0);
        let new_addr = new_obj.as_ptr() as usize;

        // Copy mark word from old to new (mimicking GC byte copy of the header).
        let old_mark = header_of(obj).mark_word.load(Ordering::Acquire);
        header_of(new_obj)
            .mark_word
            .store(old_mark, Ordering::Release);

        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(old_addr, new_addr);
        table.remap_after_gc(&pointer_map);

        // Exit using the NEW address should succeed — the registry was
        // re-keyed so the inflated Monitor is found.
        assert!(table.exit(new_obj, tid).is_ok());
    }

    #[test]
    fn inflated_handle_survives_object_remap_for_thread_exit_notify() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        let (monitor, contended) = table.enter_inflated_or_contend(obj, tid).expect("inflate");
        assert!(!contended, "fresh monitor should be acquired immediately");

        let old_addr = obj.as_ptr() as usize;
        let heap2 = Heap::new();
        let new_obj = heap2.alloc_object(ClassId::new(0), 0);
        let new_addr = new_obj.as_ptr() as usize;
        let old_mark = header_of(obj).mark_word.load(Ordering::Acquire);
        header_of(new_obj)
            .mark_word
            .store(old_mark, Ordering::Release);

        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(old_addr, new_addr);
        table.remap_after_gc(&pointer_map);

        monitor.notify_all(tid).expect("notify via stable handle");
        monitor.exit(tid).expect("exit via stable handle");
        table.enter(new_obj, tid);
        assert!(table.exit(new_obj, tid).is_ok());
    }

    /// PERF (monitor-leak reclaim): an idle, unreferenced CAS lock whose object
    /// is absent from the GC forwarding map is dropped from the registry
    /// (unconditional — no env flag), bounding the table. The next CAS on the
    /// same object transparently re-creates the lock, so behaviour is preserved.
    #[test]
    fn cas_lock_idle_dead_entry_is_reclaimed() {
        let table = MonitorTable::new();
        let obj = test_object();

        // Create a CAS lock for `obj`, then let the guard drop so the registry
        // holds the sole `Arc` (strong_count == 1).
        table.with_cas_lock(obj, || {});

        // A non-empty pointer_map that does NOT mention `obj` simulates a
        // whole-heap collection in which `obj` was not forwarded (= dead).
        // (An empty map early-returns; use a dummy unrelated remap so the body
        // actually runs.)
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(0xdead_0000usize, 0xbeef_0000usize);
        table.remap_after_gc(&pointer_map);

        // The lock for `obj` was reclaimed: a fresh CAS still works (it lazily
        // re-creates the lock), proving no state was lost.
        let mut ran = false;
        table.with_cas_lock(obj, || ran = true);
        assert!(
            ran,
            "CAS lock must be transparently re-created after reclaim"
        );
    }

    /// An inflated monitor whose object is absent from a *partial*-collection
    /// forwarding map (i.e. a live in-place survivor) is RETAINED and still
    /// usable.
    ///
    /// This is now an unconditional guarantee rather than a default-off one:
    /// `remap_after_gc` no longer reclaims monitors at all, because absence
    /// from `pointer_map` is not proof of death and the survivor's mark word
    /// still points at the monitor (dropping it would be a use-after-free, not
    /// just the BUG-V registry desync it used to be).
    #[test]
    fn inflated_monitor_absent_from_map_is_retained_by_default() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Force inflation via wait() (heavyweight monitor required).
        table.enter(obj, tid);
        table.wait(obj, tid, Some(1), None).unwrap();

        // Partial GC: pointer_map mentions some *other* object, not `obj`
        // (which survived in place). Default flag is off → must NOT reclaim.
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(0xfeed_0000usize, 0xface_0000usize);
        table.remap_after_gc(&pointer_map);

        // The monitor is still registered under the unchanged address: the
        // owning thread can still exit it (a dropped entry would yield IMSE
        // "never entered").
        assert!(
            table.exit(obj, tid).is_ok(),
            "live in-place monitor must survive a partial-GC remap by default"
        );
    }

    /// The shard survey must not lose a removal.
    ///
    /// `prune_dead` skips a dead address whose shard is empty, which is what
    /// makes it O(shards) instead of O(dead) on an allocation-heavy workload.
    /// The risk of that shortcut is precisely that it skips a shard that is
    /// NOT empty, so this inflates a real monitor, prunes it alongside a large
    /// slab of unrelated dead addresses, and asserts the entry is gone.
    ///
    /// Verified by BREAKING it: making the survey answer `false` for every
    /// shard (`monitors_nonempty = [false; MONITOR_SHARDS]`) leaves the entry
    /// in the index and fails here.
    #[test]
    fn prune_dead_still_removes_an_inflated_monitor_among_many_dead() {
        use cratonvm_gc::MonitorCleanup;
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Force inflation, then release it so the entry is prunable.
        table.enter(obj, tid);
        table.wait(obj, tid, Some(1), None).unwrap();
        table.exit(obj, tid).ok();
        let before = table.indexed_monitor_count();
        assert!(
            before > 0,
            "the fixture must actually inflate, or the assertion below passes \
            vacuously against an index that was empty all along"
        );

        // A realistic `dead` slab: the one real address buried in a crowd of
        // addresses that hash all over the 64 shards.
        let mut dead: Vec<usize> = (1..=4096).map(|i| i * 4096).collect();
        dead.push(obj.as_ptr() as usize);
        table.prune_dead(&dead);

        assert_eq!(
            table.indexed_monitor_count(),
            before - 1,
            "the shard survey must not let a real entry through: an address \
            whose shard is NON-empty has to be looked up"
        );
    }

    /// The other half of the same shortcut: with nothing inflated anywhere,
    /// pruning must be a no-op that touches no key.
    ///
    /// This is the case that dominates in practice -- `LegendreHighPrecision`
    /// and `PSquarePercentile` inflate no monitor at all -- and it is the one
    /// the old code spent 5% of the run on.
    #[test]
    fn prune_dead_on_an_empty_table_removes_nothing() {
        use cratonvm_gc::MonitorCleanup;
        let table = MonitorTable::new();
        assert_eq!(table.indexed_monitor_count(), 0);
        let dead: Vec<usize> = (1..=4096).map(|i| i * 4096).collect();
        table.prune_dead(&dead);
        assert_eq!(
            table.indexed_monitor_count(),
            0,
            "an empty index must stay empty"
        );
    }

    /// zgc10: the inverted (entry-walking) arm of `prune_dead`, both
    /// membership shapes. A realistic `dead` is far longer than the registry,
    /// so these exercise the key-snapshot path; the per-address arm is still
    /// reached when the registry outnumbers `dead`.
    #[test]
    fn prune_dead_inverted_arm_removes_monitor_and_cas_lock_sorted_and_unsorted() {
        use cratonvm_gc::MonitorCleanup;
        for sorted in [true, false] {
            let table = MonitorTable::new();
            let obj = test_object();
            let survivor = test_object();
            let tid = ThreadId(1);
            table.enter(obj, tid);
            table.wait(obj, tid, Some(1), None).unwrap();
            table.exit(obj, tid).ok();
            table.with_cas_lock(obj, || {});
            table.with_cas_lock(survivor, || {});
            let monitors_before = table.indexed_monitor_count();
            assert!(monitors_before > 0, "fixture must inflate");
            let cas_count =
                |t: &MonitorTable| -> usize { t.cas_locks.iter().map(|s| s.lock().len()).sum() };
            assert_eq!(cas_count(&table), 2);

            let mut dead: Vec<usize> = (1..=4096).map(|i| i * 4096).collect();
            dead.push(obj.as_ptr() as usize);
            if sorted {
                dead.sort_unstable();
            } else {
                dead.reverse();
                assert!(dead.windows(2).any(|w| w[0] > w[1]));
            }
            table.prune_dead(&dead);

            assert_eq!(
                table.indexed_monitor_count(),
                monitors_before - 1,
                "sorted={sorted}"
            );
            assert_eq!(
                cas_count(&table),
                1,
                "only the survivor's cas lock stays (sorted={sorted})"
            );
            let survivor_key = survivor.as_ptr() as usize;
            assert!(
                table.cas_locks[shard_of(survivor_key)]
                    .lock()
                    .contains_key(&survivor_key),
                "a live object's cas lock must not be pruned (sorted={sorted})"
            );
        }
    }

    /// zgc10: `dead_keys_among` agrees with a naive membership test on both
    /// arms, including duplicates and keys outside `dead`.
    #[test]
    fn dead_keys_among_matches_naive_membership() {
        let dead_sorted: Vec<usize> = (0..1000).map(|i| i * 16 + 8).collect();
        let keys = vec![8usize, 24, 7, 15_992, 16_000, 9_000, 24];
        let mut want: Vec<usize> = keys
            .iter()
            .copied()
            .filter(|k| dead_sorted.contains(k))
            .collect();
        want.sort_unstable();
        want.dedup();

        let mut got = dead_keys_among(&dead_sorted, true, keys.clone());
        got.sort_unstable();
        got.dedup();
        assert_eq!(got, want);

        let mut dead_unsorted = dead_sorted.clone();
        dead_unsorted.reverse();
        let mut got = dead_keys_among(&dead_unsorted, false, keys);
        got.sort_unstable();
        got.dedup();
        assert_eq!(got, want);

        assert!(dead_keys_among(&dead_unsorted, false, Vec::new()).is_empty());
    }

    #[test]
    fn monitor_contention_two_threads() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let counter = Arc::new(AtomicU32::new(0));

        // Thread 1 grabs the monitor, increments counter, releases
        let table1 = table.clone();
        let counter1 = counter.clone();
        let h1 = std::thread::spawn(move || {
            for _ in 0..100 {
                table1.enter(obj, ThreadId(1));
                let val = counter1.load(Ordering::SeqCst);
                counter1.store(val + 1, Ordering::SeqCst);
                table1.exit(obj, ThreadId(1)).unwrap();
            }
        });

        // Thread 2 does the same
        let table2 = table.clone();
        let counter2 = counter.clone();
        let h2 = std::thread::spawn(move || {
            for _ in 0..100 {
                table2.enter(obj, ThreadId(2));
                let val = counter2.load(Ordering::SeqCst);
                counter2.store(val + 1, Ordering::SeqCst);
                table2.exit(obj, ThreadId(2)).unwrap();
            }
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // Without proper locking, a non-atomic read-modify-write would lose increments
        assert_eq!(counter.load(Ordering::SeqCst), 200);
    }

    #[test]
    fn monitor_contention_reentrant() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());

        let table1 = table.clone();
        let h1 = std::thread::spawn(move || {
            table1.enter(obj, ThreadId(1));
            table1.enter(obj, ThreadId(1)); // reentrant
            std::thread::sleep(std::time::Duration::from_millis(10));
            table1.exit(obj, ThreadId(1)).unwrap();
            table1.exit(obj, ThreadId(1)).unwrap();
        });

        let table2 = table.clone();
        let h2 = std::thread::spawn(move || {
            // Give thread 1 a head start
            std::thread::sleep(std::time::Duration::from_millis(2));
            table2.enter(obj, ThreadId(2)); // should block until thread 1 exits
            table2.exit(obj, ThreadId(2)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();
    }

    #[test]
    fn monitor_wait_notify() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let flag = Arc::new(AtomicBool::new(false));

        // Consumer: waits for notify
        let table1 = table.clone();
        let flag1 = flag.clone();
        let h1 = std::thread::spawn(move || {
            table1.enter(obj, ThreadId(1));
            // Wait until producer notifies
            while !flag1.load(Ordering::Acquire) {
                table1.wait(obj, ThreadId(1), Some(50), None).unwrap();
            }
            table1.exit(obj, ThreadId(1)).unwrap();
        });

        // Producer: sets flag and notifies
        let table2 = table.clone();
        let flag2 = flag.clone();
        let h2 = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            table2.enter(obj, ThreadId(2));
            flag2.store(true, Ordering::Release);
            table2.notify(obj, ThreadId(2)).unwrap();
            table2.exit(obj, ThreadId(2)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn monitor_wait_timeout_expires() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();
        let tid = ThreadId(1);

        table.enter(obj, tid);
        // Wait with a short timeout — nobody notifies, so it should return after timeout
        let start = std::time::Instant::now();
        table.wait(obj, tid, Some(50), None).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 40,
            "Wait should have blocked for ~50ms"
        );
        table.exit(obj, tid).unwrap();
    }

    /// The general-bugs TODO's explicit ask for the monitor item: "thread A
    /// waits with a timeout and thread B notifies — verify no deadlock".
    ///
    /// The failure this guards is not a hang but a *silent* one: for a long
    /// time the timed branch of `Monitor::wait` ignored the condvar's
    /// "signalled" verdict and always slept out the full timeout, so
    /// `Thread.join(millis)` and `awaitTermination` looked like they worked
    /// while burning the entire duration after the event they waited for had
    /// already happened. Assert on the elapsed time, not just on returning.
    #[test]
    fn a_timed_waiter_returns_as_soon_as_it_is_notified() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());

        // Long enough that "slept the whole timeout" is unmistakable, short
        // enough that a genuine deadlock still fails the test in bounded time.
        const TIMEOUT_MS: u64 = 5_000;

        let waiter_table = table.clone();
        let waiter = std::thread::spawn(move || {
            waiter_table.enter(obj, ThreadId(1));
            let start = std::time::Instant::now();
            waiter_table
                .wait(obj, ThreadId(1), Some(TIMEOUT_MS), None)
                .unwrap();
            let elapsed = start.elapsed();
            waiter_table.exit(obj, ThreadId(1)).unwrap();
            elapsed
        });

        let notifier_table = table.clone();
        let notifier = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            notifier_table.enter(obj, ThreadId(2));
            notifier_table.notify(obj, ThreadId(2)).unwrap();
            notifier_table.exit(obj, ThreadId(2)).unwrap();
        });

        let elapsed = waiter.join().expect("timed waiter deadlocked");
        notifier.join().unwrap();
        assert!(
            elapsed < std::time::Duration::from_millis(TIMEOUT_MS / 2),
            "timed wait ignored notify() and slept out its timeout ({elapsed:?} \
             of {TIMEOUT_MS}ms)"
        );
    }

    /// `Thread.interrupt()` must be able to end an UNTIMED `Object.wait()`.
    ///
    /// `interrupt()` only sets a flag; the wake has to come from somewhere.
    /// Before `wake_waiters_for_interrupt` the only mechanism was `wait`'s own
    /// 5 ms poll — the `LockSupport.park` path next door was already unparked
    /// promptly by `Thread.interrupt0`, and `Object.wait()` was the odd one
    /// out. This test drives the interrupt exactly as `thread_interrupt` does:
    /// set the flag, then wake the monitor the target is parked on.
    #[test]
    fn an_untimed_waiter_is_released_by_an_interrupt_wake() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let interrupted = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));

        let waiter_table = table.clone();
        let waiter_flag = interrupted.clone();
        let waiter_parked = parked.clone();
        let waiter = std::thread::spawn(move || {
            waiter_table.enter(obj, ThreadId(1));
            waiter_parked.store(true, Ordering::Release);
            // `None` timeout — nothing but the interrupt can end this.
            let was_interrupted = waiter_table
                .wait(obj, ThreadId(1), None, Some(&waiter_flag))
                .unwrap();
            waiter_table.exit(obj, ThreadId(1)).unwrap();
            was_interrupted
        });

        while !parked.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        // Let the waiter actually reach the condvar before interrupting, so
        // the wake exercises the parked path rather than racing the entry.
        std::thread::sleep(std::time::Duration::from_millis(20));
        interrupted.store(true, Ordering::Release);
        assert!(
            table.wake_waiters_for_interrupt(obj),
            "the monitor is inflated (wait() inflates it), so the wake must \
             have been delivered"
        );

        assert!(
            waiter.join().expect("interrupted waiter deadlocked"),
            "wait() must report that an interrupt is what ended it"
        );
    }

    /// The interrupt wake must not steal a pending `notify()`.
    ///
    /// It is a `notify_all` on the same condvar `Object.notify` uses, so the
    /// question is real: a mechanism that consumed notifications would turn
    /// every interrupt of an unrelated thread into a lost wakeup for a waiter
    /// that a later `notify()` was meant for. Condvars accumulate no permits,
    /// so an extra wake cannot be banked — this pins that.
    #[test]
    fn an_interrupt_wake_does_not_swallow_a_later_notify() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let condition = Arc::new(AtomicBool::new(false));

        let waiter_table = table.clone();
        let waiter_cond = condition.clone();
        let waiter = std::thread::spawn(move || {
            waiter_table.enter(obj, ThreadId(1));
            // The Java idiom: re-check the predicate on every wakeup, so a
            // spurious wake (which is all the interrupt wake is, to this
            // thread) simply re-parks.
            while !waiter_cond.load(Ordering::Acquire) {
                waiter_table
                    .wait(obj, ThreadId(1), Some(2_000), None)
                    .unwrap();
            }
            waiter_table.exit(obj, ThreadId(1)).unwrap();
        });

        std::thread::sleep(std::time::Duration::from_millis(20));
        for _ in 0..5 {
            table.wake_waiters_for_interrupt(obj);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        table.enter(obj, ThreadId(2));
        condition.store(true, Ordering::Release);
        table.notify(obj, ThreadId(2)).unwrap();
        table.exit(obj, ThreadId(2)).unwrap();

        waiter
            .join()
            .expect("notify was lost after an interrupt wake");
    }

    /// Interpreter round i1 wave 29, lane L4: a waiter that is notified and
    /// then interrupted before it runs has consumed the notification, so it
    /// must end `Notified` -- the caller returns normally and leaves the
    /// interrupt pending, as HotSpot does -- or the notification is lost.
    #[test]
    fn i29_l4_a_notified_then_interrupted_waiter_reports_notified() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let flag = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));

        let (waiter_table, waiter_flag, waiter_parked) =
            (table.clone(), flag.clone(), parked.clone());
        let waiter = std::thread::spawn(move || {
            waiter_table.enter(obj, ThreadId(1));
            waiter_parked.store(true, Ordering::Release);
            let outcome = waiter_table
                .wait_outcome(obj, ThreadId(1), None, Some(&waiter_flag), None)
                .unwrap();
            waiter_table.exit(obj, ThreadId(1)).unwrap();
            outcome
        });
        while !parked.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        // `notify()` then `interrupt()`, both while holding the monitor.
        table.enter(obj, ThreadId(2));
        table.notify(obj, ThreadId(2)).unwrap();
        flag.store(true, Ordering::Release);
        table.wake_waiters_for_interrupt(obj);
        table.exit(obj, ThreadId(2)).unwrap();

        assert_eq!(
            waiter.join().expect("notified waiter deadlocked"),
            WaitOutcome::Notified,
            "the notification was consumed, so the wait ended by it"
        );
    }

    /// Interpreter round i1 wave 29, lane L4: interrupting one waiter must not
    /// return another waiter on the same monitor from `wait()`. The interrupt
    /// wake is a `notify_all` on the shared condvar; with the notification
    /// credit on, a signal that leaves no credit is not a notification, so the
    /// uninterrupted waiter re-parks until a real `notify()` arrives.
    #[test]
    fn i29_l4_interrupting_one_waiter_does_not_return_another() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let flag1 = Arc::new(AtomicBool::new(false));
        let flag2 = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicU32::new(0));
        let second_returned = Arc::new(AtomicBool::new(false));

        let spawn_waiter = |tid: u64, flag: Arc<AtomicBool>, returned: Option<Arc<AtomicBool>>| {
            let (t, p) = (table.clone(), parked.clone());
            std::thread::spawn(move || {
                t.enter(obj, ThreadId(tid));
                p.fetch_add(1, Ordering::AcqRel);
                let outcome = t.wait_outcome(obj, ThreadId(tid), None, Some(&flag), None).unwrap();
                if let Some(r) = returned {
                    r.store(true, Ordering::Release);
                }
                t.exit(obj, ThreadId(tid)).unwrap();
                outcome
            })
        };
        let first = spawn_waiter(1, flag1.clone(), None);
        let second = spawn_waiter(2, flag2.clone(), Some(second_returned.clone()));
        while parked.load(Ordering::Acquire) < 2 {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));

        flag1.store(true, Ordering::Release);
        table.wake_waiters_for_interrupt(obj);
        assert_eq!(first.join().expect("interrupted waiter"), WaitOutcome::Interrupted);
        // Several poll slices: the second waiter must still be parked.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !second_returned.load(Ordering::Acquire),
            "the other waiter returned from wait() with no notification"
        );

        table.enter(obj, ThreadId(3));
        table.notify(obj, ThreadId(3)).unwrap();
        table.exit(obj, ThreadId(3)).unwrap();
        assert_eq!(second.join().expect("notified waiter"), WaitOutcome::Notified);
    }

    /// An interrupt aimed at an object that has never been waited on must not
    /// allocate a monitor for it. `wait()` inflates, so "no monitor" implies
    /// "no waiter", and inflating here would put a heavyweight monitor on
    /// every object any interrupted thread happened to be holding.
    #[test]
    fn an_interrupt_wake_never_inflates_an_untouched_object() {
        let table = MonitorTable::new();
        let obj = test_object();

        assert!(!is_inflated(obj));
        assert!(
            !table.wake_waiters_for_interrupt(obj),
            "there is no monitor, so nothing can have been woken"
        );
        assert!(
            !is_inflated(obj),
            "an interrupt must not inflate an object that was never waited on"
        );
        assert_eq!(monitor_registry_len(&table), 0);
    }

    #[test]
    fn monitor_notify_without_ownership_fails() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();

        // Try to notify without owning the monitor
        let result = table.notify(obj, ThreadId(1));
        assert!(result.is_err());
    }

    #[test]
    fn monitor_wait_without_ownership_fails() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();

        // Try to wait without owning the monitor
        let result = table.wait(obj, ThreadId(1), None, None);
        assert!(result.is_err());
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn monitor_enter_exit_same_thread_repeated() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Multiple enter/exit cycles on the same monitor
        for _ in 0..50 {
            table.enter(obj, tid);
            assert!(table.exit(obj, tid).is_ok());
        }
    }

    #[test]
    fn monitor_reentrant_count_two() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter twice
        table.enter(obj, tid);
        table.enter(obj, tid);

        // First exit decrements count but monitor is still owned
        assert!(table.exit(obj, tid).is_ok());

        // Exiting again fully releases
        assert!(table.exit(obj, tid).is_ok());

        // Third exit should fail (no longer owned)
        assert!(table.exit(obj, tid).is_err());
    }

    #[test]
    fn monitor_state_after_full_exit() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid1 = ThreadId(1);
        let tid2 = ThreadId(2);

        // Thread 1 enters and exits
        table.enter(obj, tid1);
        assert!(table.exit(obj, tid1).is_ok());

        // Monitor is now unowned; thread 2 can enter
        table.enter(obj, tid2);
        assert!(table.exit(obj, tid2).is_ok());
    }

    #[test]
    fn monitor_table_creation_defaults() {
        let table = MonitorTable::new();
        // Default table should have no active monitors
        let debug_str = format!("{:?}", table);
        assert!(debug_str.contains("active_monitors"));
        assert!(debug_str.contains("0"));
    }

    #[test]
    fn monitor_table_default_trait() {
        let table = MonitorTable::default();
        let obj = test_object();
        let tid = ThreadId(1);

        // Should work the same as MonitorTable::new()
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_notify_all_without_ownership_fails() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();

        let result = table.notify_all(obj, ThreadId(1));
        assert!(result.is_err());
    }

    #[test]
    fn monitor_cas_lock_basic() {
        let table = MonitorTable::new();
        let obj = test_object();

        // CAS lock should execute the closure and return its result
        let result = table.with_cas_lock(obj, || 42);
        assert_eq!(result, 42);
    }

    #[test]
    fn monitor_cas_lock_cache_is_invalidated_on_remap() {
        let table = MonitorTable::new();
        let old_heap = Heap::new();
        let old_obj = old_heap.alloc_object(ClassId::new(0), 0);
        let new_heap = Heap::new();
        let new_obj = new_heap.alloc_object(ClassId::new(0), 0);

        // Populate this OS thread's raw-pointer cache, then simulate the
        // stop-the-world re-key that a moving collection performs.
        table.with_cas_lock(old_obj, || {});
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(old_obj.as_ptr() as usize, new_obj.as_ptr() as usize);
        table.remap_after_gc(&pointer_map);

        let mut ran = false;
        table.with_cas_lock(new_obj, || ran = true);
        assert!(ran, "CAS lock must be re-looked-up after a remap");
    }

    #[test]
    fn monitor_remap_empty_map_is_noop() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        table.enter(obj, tid);
        let empty_map = cratonvm_types::PointerMap::default();
        table.remap_after_gc(&empty_map);
        // Monitor should still be accessible with original address
        assert!(table.exit(obj, tid).is_ok());
    }

    // ── Thin-lock fast-path tests ──────────────────────────────────────────

    /// Returns true if `obj` is currently THIN_LOCKED in its mark word.
    fn is_thin_locked(obj: ObjectRef) -> bool {
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        ObjectHeader::mark_state(mark) == types::MARK_THIN_LOCKED
    }

    /// Returns true if `obj` is currently INFLATED.
    fn is_inflated(obj: ObjectRef) -> bool {
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        ObjectHeader::mark_state(mark) == types::MARK_INFLATED
    }

    /// Returns the active monitor count in the table's enumeration index
    /// (summed across shards).
    fn monitor_registry_len(table: &MonitorTable) -> usize {
        table.indexed_monitor_count()
    }

    /// The whole point of the displacement: a hash installed while the object
    /// was NEUTRAL must still be its hash after inflation destroys the word.
    ///
    /// This is also the test that shows the free half of the design working
    /// end to end -- nothing here asks for inflation. `enter` takes the thin
    /// lock fast path, whose CAS is against the literal `MARK_NEUTRAL`; the
    /// hashed word is non-zero, so that CAS loses and the object inflates on
    /// its own.
    #[test]
    fn an_identity_hash_survives_the_inflation_that_overwrites_its_word() {
        let table = MonitorTable::new();
        let heap = leaked_heap();
        let obj = compact_object(heap);
        let header = header_of(obj);

        let hash = header
            .identity_hash(|| 0x0051_1EEF)
            .expect("a fresh object is NEUTRAL");
        assert_ne!(hash, 0);
        assert!(!is_inflated(obj));

        // No explicit inflation: a hashed word cannot win the thin-lock CAS.
        table.enter(obj, ThreadId(3));
        assert!(
            is_inflated(obj),
            "a hashed object must inflate rather than thin-lock"
        );

        let mark = header.mark_word.load(Ordering::Acquire);
        assert_eq!(
            ObjectHeader::short_hash_bits(mark),
            0,
            "the word no longer carries the hash -- that is what makes the \
             displacement necessary, not optional"
        );
        let monitor = table.lookup_indexed(obj).expect("inflated => monitor");
        assert_eq!(
            monitor.displaced_hash(),
            hash,
            "the hash must have moved into the monitor, not vanished"
        );

        assert!(table.exit(obj, ThreadId(3)).is_ok());
        // Releasing an inflated monitor leaves it inflated, so the hash stays
        // reachable. This is why there is no path back to a bare NEUTRAL word
        // that would let a second, different hash be minted.
        assert!(is_inflated(obj));
        let monitor = table
            .lookup_indexed(obj)
            .expect("still inflated after exit");
        assert_eq!(monitor.displaced_hash(), hash);
    }

    /// End to end, through the accessor the VM actually calls: an object's
    /// identity hash must not change when it inflates.
    ///
    /// This is the property the whole two-sided design exists for, and the one
    /// a single call can never catch. The hash is read BEFORE inflation (from
    /// the mark word) and AFTER (resolved from the Monitor via the hook), and
    /// the two must agree.
    #[test]
    fn the_identity_hash_does_not_change_when_the_object_inflates() {
        let heap = leaked_heap();
        let table = MonitorTable::new();
        let obj = compact_object(heap);
        let tid = ThreadId(11);

        let before = heap.identity_hash_code(obj);
        assert_ne!(before, 0, "a fresh object must get a hash");
        assert!(!is_inflated(obj));

        // Not an explicit inflation: the hashed word loses the thin-lock CAS.
        table.enter(obj, tid);
        assert!(is_inflated(obj));
        table.exit(obj, tid).unwrap();

        let after = heap.identity_hash_code(obj);
        assert_eq!(
            before, after,
            "identity hash changed across inflation: {before} -> {after}"
        );
        // ...and it is stable on repeat, i.e. the displaced path reads rather
        // than mints.
        assert_eq!(heap.identity_hash_code(obj), before);
        assert_eq!(heap.identity_hash_code(obj), before);
    }

    /// An object first hashed while it is THIN_LOCKED must answer the same
    /// value once the lock is released.
    ///
    /// A thin-locked mark word's payload is an owner plus a recursion count, so
    /// the heap cannot answer at all and returns `0`. Before
    /// [`MonitorTable::java_identity_hash`] the VM turned that `0` into
    /// `i32::MAX` and returned it, so the object answered `i32::MAX` while
    /// locked and a freshly minted value after the unlock — a *changing*
    /// identity hash. `probes/IdentityHashWhileLockedProbe.java` measured
    /// exactly that against this VM (`inside=2147483647 afterUnlock=16`) and a
    /// HotSpot control that reports one value throughout; this test is the
    /// in-tree fence for it.
    #[test]
    fn the_identity_hash_of_a_thin_locked_object_survives_the_unlock() {
        let heap = leaked_heap();
        let table = MonitorTable::new();
        let obj = compact_object(heap);
        let tid = ThreadId(29);

        // Lock FIRST, so the object reaches the hash path with a mark word that
        // has no room for one. (Hashing first would keep it in the mark word
        // and take the already-covered path.)
        table.enter(obj, tid);
        assert_eq!(
            ObjectHeader::mark_state(header_of(obj).mark_word.load(Ordering::Relaxed)),
            types::MARK_THIN_LOCKED,
            "precondition: an unhashed object thin-locks"
        );

        // A mint that answers a DIFFERENT value on every call. A constant one
        // would let a re-minting implementation pass this test.
        let next = std::cell::Cell::new(0x5EED_0001_i32);
        let mint = || {
            let v = next.get();
            next.set(v + 1);
            v
        };

        let while_locked = table.java_identity_hash(obj, heap.identity_hash_code(obj), mint);
        assert_ne!(while_locked, 0, "identityHashCode must never be 0");
        assert_ne!(
            while_locked,
            i32::MAX,
            "i32::MAX is the 'no answer' sentinel: returning it hands every \
             locked-then-hashed object in the process the SAME hash"
        );

        table.exit(obj, tid).unwrap();

        let after_unlock = table.java_identity_hash(obj, heap.identity_hash_code(obj), mint);
        assert_eq!(
            while_locked, after_unlock,
            "identity hash changed across the unlock: {while_locked} -> {after_unlock}"
        );
        // Stable on repeat, i.e. the displaced path reads rather than mints.
        assert_eq!(
            table.java_identity_hash(obj, heap.identity_hash_code(obj), mint),
            while_locked
        );
    }

    /// Two different objects hashed while locked must not collide.
    ///
    /// The pre-fix answer was the constant `i32::MAX` for every one of them,
    /// which is a legal-looking hash and a catastrophic one: every such object
    /// lands in the same `HashMap` bucket and compares unequal, so the map
    /// degrades to a linear scan that also cannot find its own keys.
    #[test]
    fn locked_objects_do_not_all_share_one_identity_hash() {
        let heap = leaked_heap();
        let table = MonitorTable::new();
        let first = compact_object(heap);
        let second = compact_object(heap);
        let tid = ThreadId(31);

        let next = std::cell::Cell::new(0x5EED_1001_i32);
        let mint = || {
            let v = next.get();
            next.set(v + 1);
            v
        };

        table.enter(first, tid);
        table.enter(second, tid);
        let a = table.java_identity_hash(first, heap.identity_hash_code(first), mint);
        let b = table.java_identity_hash(second, heap.identity_hash_code(second), mint);
        table.exit(second, tid).unwrap();
        table.exit(first, tid).unwrap();

        assert_ne!(a, b, "two distinct locked objects were given the same hash");
    }

    /// An object that inflates *without* ever having been hashed displaces
    /// nothing -- `0` has to stay "none", or the first hash request after
    /// inflation would read a phantom.
    #[test]
    fn an_unhashed_object_displaces_nothing_on_inflation() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(4);
        table.enter(obj, tid);
        table.wait(obj, tid, Some(1), None).unwrap();
        table.exit(obj, tid).unwrap();
        assert!(is_inflated(obj));
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        let monitor = table.lookup_indexed(obj).expect("inflated => monitor");
        assert_eq!(monitor.displaced_hash(), 0);
    }

    /// A displaced hash is write-once. Two racers must converge, because an
    /// identity hash may never change once observed.
    #[test]
    fn displacing_a_hash_twice_keeps_the_first() {
        let m = Monitor::new();
        assert_eq!(m.displace_hash(111), 111);
        assert_eq!(m.displace_hash(222), 111);
        assert_eq!(m.displaced_hash(), 111);
    }

    #[test]
    fn thin_lock_uncontended() {
        // Single thread enter+exit must take the thin-lock fast path: no
        // Monitor allocation, no registry entry, and the mark word returns
        // to NEUTRAL on release.
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(7);

        // Pre-enter: NEUTRAL.
        assert!(!is_thin_locked(obj));
        assert!(!is_inflated(obj));
        assert_eq!(monitor_registry_len(&table), 0);

        // Enter: thin-locked, no allocation.
        table.enter(obj, tid);
        assert!(is_thin_locked(obj), "fast path must use thin lock");
        assert!(!is_inflated(obj), "uncontended path must not inflate");
        assert_eq!(
            monitor_registry_len(&table),
            0,
            "fast path must not touch the Monitor registry"
        );

        // Exit: back to NEUTRAL, still no allocation.
        table.exit(obj, tid).unwrap();
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        assert_eq!(
            ObjectHeader::mark_state(mark),
            types::MARK_NEUTRAL,
            "release must restore NEUTRAL state"
        );
        assert_eq!(monitor_registry_len(&table), 0);
    }

    #[test]
    fn thin_lock_recursive() {
        // 5 nested acquisitions on the same thread should all use the thin
        // recursive fast path. After all matching exits the mark returns to
        // NEUTRAL with no Monitor allocated.
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(42);

        for expected_rec in 0..5u32 {
            table.enter(obj, tid);
            let mark = header_of(obj).mark_word.load(Ordering::Acquire);
            assert_eq!(ObjectHeader::mark_state(mark), types::MARK_THIN_LOCKED);
            assert_eq!(
                table
                    .lock_slots
                    .owner_of(ObjectHeader::thin_lock_owner(mark)),
                Some(tid),
                "the thin-lock owner field names the thread's lock slot"
            );
            assert_eq!(
                ObjectHeader::thin_lock_recursion(mark),
                expected_rec,
                "recursion field must reflect the depth"
            );
        }
        assert_eq!(monitor_registry_len(&table), 0, "no inflation allowed");

        for expected_rec_after in (0..5u32).rev() {
            table.exit(obj, tid).unwrap();
            let mark = header_of(obj).mark_word.load(Ordering::Acquire);
            if expected_rec_after == 0 {
                assert_eq!(ObjectHeader::mark_state(mark), types::MARK_NEUTRAL);
            } else {
                assert_eq!(ObjectHeader::mark_state(mark), types::MARK_THIN_LOCKED);
                assert_eq!(
                    ObjectHeader::thin_lock_recursion(mark),
                    expected_rec_after - 1
                );
            }
        }

        assert_eq!(monitor_registry_len(&table), 0);
    }

    #[test]
    fn thin_lock_inflates_on_contention() {
        // Two threads contend on the same object. The first arrival takes
        // the thin lock; the second arrival must inflate to a heavyweight
        // Monitor. After both finish, the registry must contain exactly
        // one inflated monitor.
        //
        // CONTENTION IS ARRANGED, NOT TIMED. This test used to hold the lock
        // for 20ms and have the second thread sleep 2ms before entering,
        // trusting the 10x margin to keep the two windows overlapping. That is
        // an assumption about the SCHEDULER, and it does not hold on a busy
        // box: if thread 1's whole hold completes before thread 2 wakes,
        // thread 2 takes an UNCONTENDED thin lock, nothing ever inflates, and
        // the assertions below fail with `left: 0, right: 1`. It failed exactly
        // that way twice on an 8-core CI host with a second cargo job running,
        // and resisted 24 deliberate reproduction attempts afterwards — the
        // signature of a timing assumption, not of a defect in `MonitorTable`.
        // (`fixed-bugs/monitor-inflation-test-timed-its-contention-instead-of-`
        // `arranging-it-FIXED-20260818`, in the internal tree.)
        //
        // The handshake below removes the assumption in both directions:
        //
        //   * thread 2 does not attempt entry until thread 1 has published
        //     that it HOLDS the thin lock, so it can never arrive early;
        //   * thread 1 does not release until it observes the object INFLATED,
        //     so it can never leave early.
        //
        // That is deadlock-free by the documented shape of the contended path:
        // `enter_or_contend`'s `THIN_LOCKED(other)` arm inflates FIRST and only
        // then blocks, so thread 1's wait is satisfied by thread 2 reaching the
        // block, not by thread 1 releasing. The deadline turns a regression
        // that breaks that ordering into a failure with a message instead of a
        // hung suite.
        use std::sync::{Condvar, Mutex};
        use std::time::{Duration, Instant};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());

        // `false` until thread 1 owns the thin lock.
        let held = Arc::new((Mutex::new(false), Condvar::new()));

        let table1 = table.clone();
        let held1 = held.clone();
        let h1 = std::thread::spawn(move || {
            table1.enter(obj, ThreadId(1));
            {
                let (lock, cv) = &*held1;
                *lock.lock().unwrap() = true;
                cv.notify_all();
            }
            // Hold until thread 2's contended entry has actually inflated the
            // object. This is the half that makes the test measure inflation
            // rather than measure the scheduler.
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let mark = header_of(obj).mark_word.load(Ordering::Acquire);
                if ObjectHeader::mark_state(mark) == types::MARK_INFLATED {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "thread 2 never inflated the object while thread 1 held the \
                     thin lock: the contended `enter` path must inflate BEFORE \
                     it blocks, or this handshake (and the fast path it \
                     documents) is wrong"
                );
                std::thread::yield_now();
            }
            table1.exit(obj, ThreadId(1)).unwrap();
        });

        let table2 = table.clone();
        let held2 = held.clone();
        let h2 = std::thread::spawn(move || {
            {
                let (lock, cv) = &*held2;
                let mut owned = lock.lock().unwrap();
                while !*owned {
                    owned = cv.wait(owned).unwrap();
                }
            }
            // Thread 1 provably holds the thin lock right now, so this entry
            // is contended by construction: it inflates, then blocks until
            // thread 1 observes the inflation and releases.
            table2.enter(obj, ThreadId(2));
            table2.exit(obj, ThreadId(2)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // After contention, the object must have been inflated and the
        // monitor registered.
        assert_eq!(
            monitor_registry_len(&table),
            1,
            "contention must produce exactly one inflated monitor"
        );
        // The final state should be INFLATED (mark word permanently points
        // at the Monitor — thin-lock inflation is one-way per object).
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        assert_eq!(
            ObjectHeader::mark_state(mark),
            types::MARK_INFLATED,
            "inflated mark word should persist after contention"
        );
    }

    /// Regression guard for the Jetty `Deflater.end()`/`DeflaterPool.end()`
    /// hang: a monitor that was still an uncontended THIN lock when its
    /// owning thread died (so `release_monitors_held_by` never saw it — it
    /// wasn't inflated yet) gets inflated *later* by whichever thread next
    /// contends it, and inflation pre-seeds the new `Monitor`'s owner from
    /// the stale mark word. `thin_seed_owner`/`force_release_if_owned_by` are
    /// what `vm_exec::release_dead_thin_owner` (called from
    /// `monitor_enter_blocking` and `monitor_enter_synchronized_method`) uses
    /// to detect and clear that dead-owner seed at the contention point, before
    /// blocking — this test exercises that exact mechanism directly. (The call
    /// site was lost in the 2026-07-20 merge `27aa91068` while this test kept
    /// passing, because it never went through `vm_exec`; restored in round 11
    /// wave 4.)
    #[test]
    fn contended_inflation_of_a_dead_threads_thin_lock_is_recoverable() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();
        let tid_dead = ThreadId(1);
        let tid_b = ThreadId(2);

        // Thread "dead" takes the (uncontended) thin lock and never releases
        // it — simulating termination while blocked in a native call made
        // from inside the synchronized region.
        table.enter(obj, tid_dead);
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        assert_eq!(
            ObjectHeader::mark_state(mark),
            types::MARK_THIN_LOCKED,
            "uncontended enter should stay a thin lock"
        );

        // A live thread contends the same object. `enter_or_contend` must
        // inflate (nothing has released the thin lock) and pre-seed the new
        // Monitor's owner from the dead thread's ID — the exact zombie state
        // this fix targets.
        let m = table
            .enter_or_contend(obj, tid_b)
            .expect("contended thin lock must inflate, not silently succeed");
        assert_eq!(m.current_owner(), Some(tid_dead));
        assert_eq!(m.thin_seed_owner(), Some(tid_dead));

        // The dead-owner check + force-release (mirroring
        // `monitor_enter_blocking`'s contention-point check).
        assert!(m.force_release_if_owned_by(tid_dead));
        assert_eq!(m.current_owner(), None);

        // Thread B can now acquire immediately instead of blocking forever.
        m.block_enter(tid_b);
        assert_eq!(m.current_owner(), Some(tid_b));
        assert!(m.exit(tid_b).is_ok());
    }

    // ── ARCH-2026-07-26: mark-word-reachable monitors ─────────────────────
    //
    // The properties these guard are the ones that turn a global-lock probe
    // into a per-object pointer chase without introducing a use-after-free.

    /// Leak a heap so `ObjectRef`s stay valid for the whole test (same reason
    /// as `test_object`, but shared by several objects).
    fn leaked_heap() -> &'static Heap {
        Box::leak(Box::new(Heap::new()))
    }

    /// Drive `obj` all the way to `MARK_INFLATED` and leave it unowned.
    fn force_inflated(table: &MonitorTable, obj: ObjectRef, tid: ThreadId) {
        table.enter(obj, tid);
        // `wait` requires a heavyweight monitor, so it inflates. A 1ms timeout
        // keeps it fast; the monitor is re-acquired on return.
        table.wait(obj, tid, Some(1), None).unwrap();
        table.exit(obj, tid).unwrap();
        assert!(is_inflated(obj), "setup must leave the object INFLATED");
    }

    /// Since the 8-byte header an inflated monitor is found BY ADDRESS: the
    /// mark word says INFLATED and the index names the monitor. Every inflated
    /// operation reaches the same monitor through it.
    #[test]
    fn an_inflated_monitor_is_found_through_the_index() {
        let table = MonitorTable::new();
        let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
        force_inflated(&table, obj, ThreadId(1));
        let m = table
            .lookup_indexed(obj)
            .expect("an inflated object is indexed");

        let tid = ThreadId(2);
        table.enter(obj, tid);
        assert!(table.holds(obj, tid));
        assert!(m.is_held_by(tid), "enter reached the indexed monitor");
        assert_eq!(table.current_owner(obj), Some(tid));
        table.enter(obj, tid);
        assert_eq!(table.entry_count(obj), 2);
        table.exit(obj, tid).unwrap();
        table.exit(obj, tid).unwrap();
        assert!(!table.holds(obj, tid));
        assert!(Arc::ptr_eq(&m, &table.lookup_indexed(obj).unwrap()));
    }

    /// The uncontended fast path allocates nothing and indexes nothing, even
    /// across many lock/unlock cycles: the whole state machine stays in the
    /// object's mark word.
    #[test]
    fn uncontended_fast_path_never_allocates_or_indexes() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(9);

        for _ in 0..1000 {
            table.enter(obj, tid);
            assert!(is_thin_locked(obj));
            table.exit(obj, tid).unwrap();
        }
        assert_eq!(
            monitor_registry_len(&table),
            0,
            "uncontended locking must never create an index entry"
        );
        assert!(!is_inflated(obj));
    }

    /// Recursive entry by the owning thread works past the thin lock's 256-deep
    /// ceiling: the overflow inflates, carrying the accumulated count over, and
    /// the matching exits unwind it exactly.
    #[test]
    fn recursive_entry_survives_thin_lock_overflow_into_inflation() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(3);

        // MAX+1 acquisitions fit in the thin lock (recursion 0..=MAX).
        let fit = types::MAX_THIN_LOCK_RECURSION + 1;
        for _ in 0..fit {
            table.enter(obj, tid);
        }
        assert!(is_thin_locked(obj), "MAX+1 nested acquires must stay thin");
        assert_eq!(table.entry_count(obj), fit);

        // The next one overflows the recursion field and forces inflation.
        table.enter(obj, tid);
        assert!(is_inflated(obj), "recursion overflow must inflate");
        assert_eq!(
            table.entry_count(obj),
            fit + 1,
            "inflation must carry the accumulated recursion count over"
        );
        assert_eq!(table.current_owner(obj), Some(tid));

        for _ in 0..fit + 1 {
            table.exit(obj, tid).unwrap();
        }
        assert_eq!(table.current_owner(obj), None);
        // One exit too many is an IMSE, not a wrap.
        assert!(table.exit(obj, tid).is_err());
    }

    /// Contended handoff on an already-inflated monitor: the second thread
    /// blocks in `block_enter` until the first releases, then acquires.
    #[test]
    fn contended_handoff_between_two_threads_on_an_inflated_monitor() {
        let table = Arc::new(MonitorTable::new());
        let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
        force_inflated(&table, obj, ThreadId(1));

        let holder_has_it = Arc::new(std::sync::Barrier::new(2));
        let contender_observed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let t1 = table.clone();
        let b1 = holder_has_it.clone();
        let h1 = std::thread::spawn(move || {
            t1.enter(obj, ThreadId(1));
            b1.wait();
            // Hold long enough that thread 2 is definitely parked on entry.
            std::thread::sleep(std::time::Duration::from_millis(50));
            t1.exit(obj, ThreadId(1)).unwrap();
        });

        let t2 = table.clone();
        let b2 = holder_has_it.clone();
        let observed = contender_observed.clone();
        let h2 = std::thread::spawn(move || {
            b2.wait();
            // Thread 1 owns it: `enter_or_contend` must report contention
            // rather than silently succeeding.
            let contended = t2.enter_or_contend(obj, ThreadId(2));
            if let Some(m) = contended {
                observed.store(true, std::sync::atomic::Ordering::Release);
                m.block_enter(ThreadId(2));
            }
            assert!(t2.holds(obj, ThreadId(2)));
            t2.exit(obj, ThreadId(2)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();
        assert!(
            contender_observed.load(std::sync::atomic::Ordering::Acquire),
            "the second thread must have seen the monitor as contended"
        );
        assert_eq!(table.current_owner(obj), None);
        assert_eq!(table.entry_count(obj), 0);
        // Contention on an already-inflated monitor must not create a SECOND
        // monitor for the same object (the old registry-miss failure shape).
        assert_eq!(monitor_registry_len(&table), 1);
    }

    /// `wait`/`notify` round-trip across two threads, driven entirely through
    /// the mark word (the object is already inflated before either thread
    /// starts, so no inflation happens on either side).
    /// A NOTIFICATION SURVIVES A CONDVAR SIGNAL THAT IS NEVER OBSERVED.
    ///
    /// This is the invariant the netty `ParameterizedSslHandlerTest` stall cost
    /// 420 s a run to find, and it is white-box on purpose: the black-box
    /// version ("notify, then assert the waiter woke") passes on the BROKEN
    /// code too, because there the condvar signal normally does arrive. What
    /// broke was the case where it does not, and only the state can be asked
    /// about that.
    ///
    /// The measured failure was `polls=78025 signalled=0` with
    /// `notifies_since_wait=1` — 78 025 five-millisecond waits, not one of them
    /// a signalled return, against a `notifyAll()` the monitor really did
    /// serve. The fix is that the notification is now a CREDIT in
    /// `MonitorState`, taken under the same mutex a notifier must hold, so the
    /// condvar signal is an optimisation rather than the mechanism.
    ///
    /// What this test pins, with no condvar involved at all:
    ///
    /// * `notifyAll()` leaves exactly one credit per waiter parked AT THAT
    ///   MOMENT — not more (a later waiter must not consume one) and not fewer;
    /// * `notify()` leaves exactly one, and never stockpiles credits for
    ///   waiters that do not exist;
    /// * a credit is CONSUMED by a decrement, which is what keeps `notify()`
    ///   from behaving like `notifyAll()`.
    #[test]
    fn a_notification_is_a_credit_in_monitor_state_not_only_a_condvar_signal() {
        let m = Monitor::new();
        let owner = ThreadId(7);

        // Pretend three threads are parked, without actually parking any:
        // three tickets in the wait set, in arrival order.
        assert!(m.try_enter(owner));
        let (t1, t2, t3) = {
            let mut s = m.state.lock();
            s.parked_waiters = 3;
            (s.enroll_wait_ticket(), s.enroll_wait_ticket(), s.enroll_wait_ticket())
        };

        // `notify()` leaves ONE credit, for the longest waiter, and never more
        // than there are waiters.
        m.notify(owner).unwrap();
        assert_eq!(m.state.lock().pending_notifies, 1);
        {
            let mut s = m.state.lock();
            assert!(!s.take_notification(t2), "not the second waiter's");
            assert!(s.take_notification(t1), "the longest waiter's (wave 29, FIFO)");
        }
        m.notify(owner).unwrap();
        m.notify(owner).unwrap();
        assert_eq!(m.state.lock().pending_notifies, 2);
        m.notify(owner).unwrap();
        assert_eq!(
            m.state.lock().pending_notifies,
            2,
            "a notify with no waiter left to take it must not stockpile a credit \
             that a FUTURE wait would consume as a spurious wakeup"
        );
        {
            let mut s = m.state.lock();
            assert!(s.take_notification(t3) && s.take_notification(t2));
            assert_eq!(s.pending_notifies, 0);
        }

        // `notifyAll()` is exactly the waiters in the wait set now.
        let (a, b) = {
            let mut s = m.state.lock();
            (s.enroll_wait_ticket(), s.enroll_wait_ticket())
        };
        m.notify_all(owner).unwrap();
        assert_eq!(m.state.lock().pending_notifies, 2);

        // A waiter that arrives AFTER the notifyAll cannot take one of those
        // credits and count itself notified -- nor, since wave 29, a credit
        // a `notify` left for an earlier waiter (the lost wakeup: the earlier
        // waiter would have stayed parked).
        let late = m.state.lock().enroll_wait_ticket();
        assert!(!m.state.lock().take_notification(late));
        assert_eq!(
            m.state.lock().pending_notifies,
            2,
            "notifyAll() promises the set parked when it ran, not a standing offer"
        );

        // Consuming is per waiter: the two marked waiters take theirs, and a
        // waiter leaving unmarked is out of the wait set.
        {
            let mut s = m.state.lock();
            assert!(s.take_notification(a) && s.take_notification(b));
            assert_eq!(s.pending_notifies, 0);
            s.leave_wait_set(late);
            assert!(s.wait_queue.is_empty());
        }
    }

    /// `notify` / `notifyAll` on a monitor this thread does not own must still
    /// be an `IllegalMonitorStateException` and must leave NO credit behind —
    /// the credit is only reachable past the ownership check, and a refused
    /// notification that still armed a waiter would be a spurious wakeup with
    /// no notifier.
    #[test]
    fn a_refused_notify_leaves_no_credit() {
        let m = Monitor::new();
        assert!(m.try_enter(ThreadId(1)));
        {
            let mut s = m.state.lock();
            s.parked_waiters = 2;
        }
        assert!(m.notify(ThreadId(2)).is_err());
        assert!(m.notify_all(ThreadId(2)).is_err());
        assert_eq!(m.state.lock().pending_notifies, 0);
    }

    #[test]
    fn wait_notify_round_trip_on_a_mark_word_reachable_monitor() {
        let table = Arc::new(MonitorTable::new());
        let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
        force_inflated(&table, obj, ThreadId(1));
        let monitor_before = header_of(obj).mark_word.load(Ordering::Acquire);

        let delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let woke = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let t1 = table.clone();
        let d1 = delivered.clone();
        let w1 = woke.clone();
        let consumer = std::thread::spawn(move || {
            let tid = ThreadId(1);
            t1.enter(obj, tid);
            while !d1.load(std::sync::atomic::Ordering::Acquire) {
                t1.wait(obj, tid, Some(1000), None).unwrap();
            }
            w1.store(true, std::sync::atomic::Ordering::Release);
            t1.exit(obj, tid).unwrap();
        });

        let t2 = table.clone();
        let d2 = delivered.clone();
        let producer = std::thread::spawn(move || {
            let tid = ThreadId(2);
            std::thread::sleep(std::time::Duration::from_millis(20));
            t2.enter(obj, tid);
            d2.store(true, std::sync::atomic::Ordering::Release);
            t2.notify_all(obj, tid).unwrap();
            t2.exit(obj, tid).unwrap();
        });

        consumer.join().unwrap();
        producer.join().unwrap();
        assert!(woke.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            header_of(obj).mark_word.load(Ordering::Acquire),
            monitor_before,
            "wait/notify must reuse the monitor the mark word already named, \
             never publish a new one"
        );
        assert_eq!(table.current_owner(obj), None);
    }

    /// Publishing `MARK_INFLATED` transfers exactly one strong reference to the
    /// mark word, and releasing it is idempotent — a second release must not
    /// decrement again (that would be a double free once the real holders drop).
    #[test]
    fn mark_word_owns_one_strong_ref_and_release_is_idempotent() {
        // Since the 8-byte header the mark word holds no `Monitor` pointer, so
        // it owns no reference: the index entry is the monitor's one
        // structural holder, and `release_mark_ref` has nothing to release.
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        let (m, contended) = table.enter_inflated_or_contend(obj, tid).expect("inflate");
        assert!(!contended);
        assert!(!m.mark_ref.load(Ordering::Acquire));
        // index entry + the handle we are holding.
        assert_eq!(Arc::strong_count(&m), 2);
        assert_eq!(m.structural_refs(), 1);

        // SAFETY: test-local object; nothing else can read its mark word.
        unsafe { Monitor::release_mark_ref(&m) };
        assert_eq!(
            Arc::strong_count(&m),
            2,
            "releasing a mark-word reference that does not exist must not \
             decrement anything"
        );

        assert!(m.is_held_by(tid));
        assert!(m.exit(tid).is_ok());
    }

    /// Reclamation must never race a waiter. A thread parked in `Object.wait()`
    /// holds its own `Arc<Monitor>` for the whole park, which pushes the strong
    /// count above `structural_refs()` — the exact predicate the reclaim path
    /// uses to decide a monitor is unreferenced. So while anyone is parked, the
    /// monitor is not reclaimable, and even if it were, the release would not
    /// be the last drop.
    #[test]
    fn a_parked_waiter_keeps_its_monitor_unreclaimable_and_alive() {
        let table = Arc::new(MonitorTable::new());
        let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let t = table.clone();
        let flag = interrupt.clone();
        let waiter = std::thread::spawn(move || {
            let tid = ThreadId(2);
            t.enter(obj, tid);
            // Untimed wait with an interrupt flag: parks until we set it.
            t.wait(obj, tid, None, Some(&*flag)).unwrap();
            t.exit(obj, tid).unwrap();
        });

        // Spin until the waiter has inflated the monitor AND released it into
        // the wait (owner goes back to None while it is parked).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if is_inflated(obj) && table.current_owner(obj).is_none() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "waiter never reached Object.wait()"
            );
            std::thread::yield_now();
        }

        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        let m = table
            .lookup_indexed(obj)
            .expect("INFLATED mark must name a monitor");
        // index + mark word = structural; plus the parked waiter, plus `m`.
        assert!(
            Arc::strong_count(&m) > m.structural_refs() + 1,
            "a thread parked in wait() must hold its own strong reference \
             (strong={}, structural={})",
            Arc::strong_count(&m),
            m.structural_refs()
        );
        // The reclaim predicate shared by `remap_after_gc` and the idle check
        // must therefore refuse this monitor.
        assert!(
            !(Arc::strong_count(&m) == m.structural_refs() && m.is_idle()),
            "a monitor with a parked waiter must never look reclaimable"
        );

        drop(m);
        interrupt.store(true, std::sync::atomic::Ordering::Release);
        waiter.join().unwrap();

        // After the waiter is gone the monitor is idle and referenced only by
        // its structural holders — now it WOULD be reclaimable.
        let m = table.lookup_indexed(obj).unwrap();
        assert!(m.is_idle());
        assert_eq!(Arc::strong_count(&m), m.structural_refs() + 1);
    }

    /// A monitor whose index entry is lost -- which only a relocation that
    /// failed to remap the index can do -- leaves an INFLATED object with no
    /// monitor. The VM must keep running: the next operation gives the object a
    /// fresh, unowned monitor (and reports it), rather than panicking.
    #[test]
    fn a_lost_index_entry_is_replaced_by_a_fresh_monitor() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);
        force_inflated(&table, obj, ThreadId(1));
        assert_eq!(monitor_registry_len(&table), 1);

        // Simulate the lost entry.
        let key = obj.as_ptr() as usize;
        table.monitor_shard(key).lock().remove(&key);
        // Every real index mutation bumps the cache epoch (round 11 wave 16);
        // this simulated one must too, or this thread's cache would still name
        // the monitor just dropped.
        table.note_index_mutation();
        assert_eq!(monitor_registry_len(&table), 0);

        table.enter(obj, tid);
        assert!(table.holds(obj, tid));
        assert_eq!(monitor_registry_len(&table), 1, "the index is repaired");
        table.exit(obj, tid).unwrap();

        // And enumeration (thread-death monitor release) sees it again.
        table.enter(obj, tid);
        table.release_monitors_held_by(tid);
        assert_eq!(table.current_owner(obj), None);
    }

    /// `prune_stale_after_gc` over every shard at once.
    fn prune_all(table: &MonitorTable, survived: &dyn Fn(usize) -> bool) -> usize {
        table.prune_stale_in_shards(0, MONITOR_SHARDS, survived)
    }

    /// The per-pause prune judges an eighth of the shards, in rotation: a
    /// dead entry leaves within eight calls, and one call never judges more.
    #[test]
    fn prune_stale_after_gc_rotates_through_every_shard() {
        let table = MonitorTable::new();
        let heap = leaked_heap();
        for _ in 0..256 {
            let obj = heap.alloc_object(ClassId::new(0), 0);
            table.index_insert(obj.as_ptr() as usize, &Arc::new(Monitor::new()));
        }
        let total = monitor_registry_len(&table);
        assert_eq!(total, 256);
        let mut pruned = 0;
        for _ in 0..MONITOR_SHARDS / PRUNE_SHARDS_PER_PAUSE {
            let n = table.prune_stale_after_gc(&|_| false);
            assert!(n < total, "one call judged every shard");
            pruned += n;
        }
        assert_eq!(pruned, total, "one rotation judged every shard");
        assert_eq!(monitor_registry_len(&table), 0);
    }

    /// Round 12 wave 2 (lane lock): `prune_stale_after_gc` drops the entry of
    /// an object the verdict calls dead, and the entry at an address whose
    /// live object's word is not INFLATED (a dead predecessor's entry), and
    /// keeps a live inflated object's and an owned monitor's.
    #[test]
    fn prune_stale_after_gc_drops_only_provably_stale_idle_entries() {
        let table = MonitorTable::new();
        let heap = leaked_heap();
        let tid = ThreadId(1);
        let live = heap.alloc_object(ClassId::new(0), 0);
        let dead = heap.alloc_object(ClassId::new(0), 0);
        let reused = heap.alloc_object(ClassId::new(0), 0);
        let held = heap.alloc_object(ClassId::new(0), 0);
        force_inflated(&table, live, tid);
        force_inflated(&table, dead, tid);
        // A dead object's entry left at an address a NEUTRAL object now holds.
        assert!(!is_inflated(reused));
        table.index_insert(reused.as_ptr() as usize, &Arc::new(Monitor::new()));
        // Owned, and referenced by nothing but the index.
        let (m, contended) = table.enter_inflated_or_contend(held, tid).expect("inflate");
        assert!(!contended);
        drop(m);
        assert_eq!(monitor_registry_len(&table), 4);

        let dead_key = dead.as_ptr() as usize;
        let held_key = held.as_ptr() as usize;
        let removed = prune_all(&table, &|addr| addr != dead_key && addr != held_key);
        assert_eq!(removed, 2, "the dead object's and the reused address's entries");
        assert!(table.lookup_indexed(live).is_some(), "a live inflated object keeps its monitor");
        assert!(table.lookup_indexed(dead).is_none());
        assert!(table.lookup_indexed(reused).is_none());
        assert!(table.lookup_indexed(held).is_some(), "an owned monitor is never pruned");
        assert!(table.exit(held, tid).is_ok());
        // Unowned now, and the verdict still calls it dead.
        assert_eq!(prune_all(&table, &|addr| addr != held_key), 1);
        assert_eq!(monitor_registry_len(&table), 1);
    }

    /// A key `remap_after_gc` just re-keyed an entry to is a relocated
    /// survivor: kept whatever the verdict says, once.
    #[test]
    fn prune_stale_after_gc_keeps_a_relocated_survivor_without_asking_the_heap() {
        let table = MonitorTable::new();
        let heap = leaked_heap();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        force_inflated(&table, obj, ThreadId(1));
        let to = heap.alloc_object(ClassId::new(0), 0);
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        header_of(to).mark_word.store(mark, Ordering::Release);
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(obj.as_ptr() as usize, to.as_ptr() as usize);
        table.remap_after_gc(&pointer_map);

        assert_eq!(prune_all(&table, &|_| false), 0);
        assert!(table.lookup_indexed(to).is_some());
        // The list is consumed: the next prune asks the verdict.
        assert_eq!(prune_all(&table, &|_| false), 1);
        assert_eq!(monitor_registry_len(&table), 0);
    }

    /// Monitors for different objects land in different shards often enough
    /// that the index is not a single serialization point, and `shard_of` stays
    /// inside bounds for every input it can be handed.
    #[test]
    fn shard_selection_is_in_bounds_and_spreads_consecutive_addresses() {
        for raw in [0usize, 8, 16, 0x1000, usize::MAX & !7] {
            assert!(shard_of(raw) < MONITOR_SHARDS);
        }
        // Consecutive 8-byte-aligned allocations must not all collide: the
        // whole point of hashing the address is that neighbouring objects get
        // independent shards.
        let base = 0x7f00_0000_0000usize;
        let distinct: std::collections::HashSet<usize> = (0..MONITOR_SHARDS)
            .map(|i| shard_of(base + i * 32))
            .collect();
        assert!(
            distinct.len() > MONITOR_SHARDS / 4,
            "shard hash clusters badly: {} distinct shards for {} consecutive \
             allocations",
            distinct.len(),
            MONITOR_SHARDS
        );
    }

    // ── Round 11 wave 4 (lane sync): contended spin, release reporting, and
    // the thin-lock seed that dead-owner recovery needs. ──────────────────

    /// `exit_reporting_release` must give exactly the answer the `holds`
    /// re-check it replaces in `vm_exec::monitor_exit_and_retract_jmx` gave, on
    /// both representations: a re-entrant release still holds, the outermost
    /// one does not, and an unowned exit is still an IMSE.
    #[test]
    fn exit_reporting_release_matches_holds_on_thin_and_inflated_monitors() {
        let table = MonitorTable::new();
        let tid = ThreadId(5);

        let thin = test_object();
        table.enter(thin, tid);
        table.enter(thin, tid);
        assert!(is_thin_locked(thin));
        assert!(!table.exit_reporting_release(thin, tid).unwrap());
        assert!(table.holds(thin, tid));
        assert!(table.exit_reporting_release(thin, tid).unwrap());
        assert!(!table.holds(thin, tid));
        assert!(table.exit_reporting_release(thin, tid).is_err());

        let fat = leaked_heap().alloc_object(ClassId::new(0), 0);
        force_inflated(&table, fat, tid);
        table.enter(fat, tid);
        table.enter(fat, tid);
        assert!(is_inflated(fat));
        assert!(!table.exit_reporting_release(fat, tid).unwrap());
        assert!(table.holds(fat, tid));
        assert!(table.exit_reporting_release(fat, tid).unwrap());
        assert!(!table.holds(fat, tid));
        assert!(table.exit_reporting_release(fat, tid).is_err());
    }

    /// The owner word follows every production ownership transition: acquire,
    /// re-entry, release, `wait`'s release-and-reacquire (with its depth
    /// restored) and the dead-thread force release. Since round 11 wave 16 it
    /// is THE owner, not an advisory copy, so the entry count is pinned too.
    #[test]
    fn the_owner_word_tracks_every_ownership_transition() {
        let m = Monitor::new();
        let a = ThreadId(3);
        let word = |m: &Monitor| m.owner.load(Ordering::Relaxed);
        let count = |m: &Monitor| m.entry_count.load(Ordering::Relaxed);
        assert_eq!(word(&m), 0);
        assert!(m.try_enter(a));
        assert_eq!(word(&m), owner_word(Some(a)));
        assert!(m.try_enter(a));
        assert_eq!(count(&m), 2);
        assert!(!m.exit_reporting_release(a).unwrap());
        assert_eq!(
            word(&m),
            owner_word(Some(a)),
            "re-entrant release keeps the owner"
        );
        assert!(m.exit_reporting_release(a).unwrap());
        assert_eq!(word(&m), 0);
        assert_eq!(count(&m), 0);

        m.block_enter(a);
        m.block_enter(a);
        assert_eq!(word(&m), owner_word(Some(a)));
        // A 1 ms timed wait releases and re-acquires at the same depth.
        assert_eq!(
            m.wait(a, Some(1), None, None, None).unwrap(),
            WaitOutcome::TimedOutOrSpurious
        );
        assert_eq!(word(&m), owner_word(Some(a)));
        assert_eq!(count(&m), 2, "wait() restores the re-entry depth");
        assert!(m.force_release_if_owned_by(a));
        assert_eq!(word(&m), 0);
        assert_eq!(count(&m), 0);
        assert!(
            !m.force_release_if_owned_by(a),
            "a second forced release finds nothing to release"
        );

        let fresh = Monitor::new();
        fresh.enter_with_recursion(a, 2);
        assert_eq!(word(&fresh), owner_word(Some(a)));
        assert_eq!(fresh.thin_seed_owner(), Some(a));
        assert!(!fresh.exit_reporting_release(a).unwrap());
        assert_eq!(
            fresh.thin_seed_owner(),
            Some(a),
            "still held without a break"
        );
        assert!(fresh.exit_reporting_release(a).unwrap());
        assert_eq!(fresh.thin_seed_owner(), None);
        assert_eq!(word(&fresh), 0);
        // `ThreadId(0)` (the JNI placeholder owner) must not encode as "free",
        // and no real thread encodes as the forced-release sentinel.
        assert_ne!(owner_word(Some(ThreadId(0))), 0);
        assert_ne!(owner_word(Some(ThreadId(0))), OWNER_RELEASING);
    }

    /// Round 11 wave 16 (lock): the parking handshake. Many threads fighting
    /// over one inflated monitor, each parking for real (`block_enter`), must
    /// all get through with the protected counter exact -- no lost wake-up
    /// (a hang) and no double ownership (a lost update). The counter is a
    /// plain read-modify-write done only under the monitor.
    #[test]
    fn a_contended_monitor_loses_no_wakeup_and_no_update() {
        const THREADS: u64 = 8;
        const ROUNDS: u64 = 2_000;
        let m = Arc::new(Monitor::new());
        let counter = Arc::new(AtomicU64::new(0));
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let m = Arc::clone(&m);
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    let me = ThreadId(100 + t);
                    for i in 0..ROUNDS {
                        // Alternate the two contended entries: straight to the
                        // park, and spin-then-park, as the VM's callers do.
                        if i % 2 == 0 || !m.spin_try_enter_adaptive(me) {
                            m.block_enter(me);
                        }
                        // Re-entry inside the critical section.
                        assert!(m.try_enter(me));
                        let v = counter.load(Ordering::Relaxed);
                        std::hint::spin_loop();
                        counter.store(v + 1, Ordering::Relaxed);
                        assert!(!m.exit_reporting_release(me).unwrap());
                        assert!(m.exit_reporting_release(me).unwrap());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::Relaxed), THREADS * ROUNDS);
        assert_eq!(m.current_owner(), None);
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(m.spinners.load(Ordering::SeqCst), 0);
    }

    /// Round 11 wave 16 (lock): a release with a woken successor already on
    /// its way wakes nobody else, and a woken entrant clears the flag when it
    /// re-tries -- whether it wins or goes back to sleep.
    #[test]
    fn a_pending_successor_suppresses_further_wakeups_until_it_retries() {
        let m = Arc::new(Monitor::new());
        let (owner, parker) = (ThreadId(201), ThreadId(202));
        assert!(m.try_enter(owner));
        let m2 = Arc::clone(&m);
        let h = std::thread::spawn(move || {
            m2.block_enter(parker);
            assert!(m2.exit_reporting_release(parker).unwrap());
        });
        // Wait until the parker is registered.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while m.entry_waiters.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "parker never parked");
            std::thread::yield_now();
        }
        // Pretend a successor is already on its way: a release then owes no
        // wake-up. (Cleared again before the real release: no such successor
        // exists to clear it.)
        m.succ_pending.store(true, Ordering::SeqCst);
        assert!(!m.successor_wake_needed());
        m.succ_pending.store(false, Ordering::SeqCst);
        assert!(m.successor_wake_needed());
        assert!(m.exit_reporting_release(owner).unwrap());
        h.join().unwrap();
        assert!(!m.succ_pending.load(Ordering::SeqCst), "the woken entrant cleared it");
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(m.current_owner(), None);
    }

    /// Round 11 wave 19 (lock, proposal W17-1): the bounded running park
    /// gives up on a monitor held for its whole budget having taken nothing
    /// and left no registration behind, and is woken -- as a registered
    /// entrant, by the release's own successor wake-up -- when the owner
    /// releases inside it.
    #[test]
    fn park_enter_for_is_bounded_and_woken_by_a_release() {
        let m = Arc::new(Monitor::new());
        let (owner, parker) = (ThreadId(211), ThreadId(212));
        assert!(m.park_enter_while(parker, std::time::Duration::from_millis(1), || true));
        assert_eq!(m.current_owner(), Some(parker), "a free monitor is taken at once");
        assert!(m.exit_reporting_release(parker).unwrap());

        assert!(m.try_enter(owner));
        assert!(!m.park_enter_while(parker, std::time::Duration::from_millis(2), || true));
        assert_eq!(m.current_owner(), Some(owner), "a timed-out park takes nothing");
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0, "and deregisters");
        assert!(!m.succ_pending.load(Ordering::SeqCst));

        let m2 = Arc::clone(&m);
        let h = std::thread::spawn(move || {
            let got = m2.park_enter_while(parker, std::time::Duration::from_secs(30), || true);
            if got {
                assert!(m2.exit_reporting_release(parker).unwrap());
            }
            got
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while m.entry_waiters.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "parker never parked");
            std::thread::yield_now();
        }
        assert!(m.successor_wake_needed(), "a running parker is owed a wake-up");
        assert!(m.exit_reporting_release(owner).unwrap());
        assert!(h.join().unwrap(), "the release woke the parker, which took the monitor");
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert!(!m.succ_pending.load(Ordering::SeqCst), "the woken entrant cleared it");
        assert_eq!(m.current_owner(), None);

        // Round 12 wave 1: a running park asked to stop (a pause requested)
        // gives up before it waits, taking nothing and leaving no
        // registration.
        assert!(m.try_enter(owner));
        let started = std::time::Instant::now();
        assert!(!m.park_enter_while(parker, std::time::Duration::from_secs(30), || false));
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "it did not wait");
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(m.current_owner(), Some(owner));
        assert!(m.exit_reporting_release(owner).unwrap());
    }

    /// Round 12 wave 1 (lane lock): `notify` / `notifyAll` on a thin lock the
    /// caller holds succeed WITHOUT inflating (a thin word was never waited
    /// on: `wait` inflates, for good), and on a word the caller does not own
    /// raise IMSE, also without inflating.
    #[test]
    fn notify_on_a_thin_lock_neither_inflates_nor_loses_ownership() {
        let switched_off = matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_MONITOR_THIN_NOTIFY").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        );
        let table = MonitorTable::new();
        let obj = test_object();
        let (owner, other) = (ThreadId(241), ThreadId(242));
        assert!(table.notify(obj, owner).is_err(), "unowned: IMSE");
        assert!(table.notify_all(obj, owner).is_err(), "unowned: IMSE");
        table.enter(obj, owner);
        table.notify(obj, owner).unwrap();
        table.notify_all(obj, owner).unwrap();
        assert!(table.notify(obj, other).is_err(), "another thread's thin lock");
        assert!(table.notify_all(obj, other).is_err(), "another thread's thin lock");
        assert!(table.holds(obj, owner));
        if !switched_off {
            assert!(is_thin_locked(obj), "never inflated");
            assert_eq!(monitor_registry_len(&table), 0);
        }
        table.exit(obj, owner).unwrap();
        assert_eq!(table.current_owner(obj), None);
    }

    /// Round 12 wave 1 (lane lock, W16-3 / W19-3): the spin a woken entrant
    /// takes before re-parking wins a free monitor at once, as a plain
    /// acquisition (entry count 1), and gives up on a held one within its
    /// budget having taken nothing.
    #[test]
    fn spin_after_wake_takes_a_free_monitor_and_gives_up_on_a_held_one() {
        let m = Monitor::new();
        let (owner, woken) = (ThreadId(221), ThreadId(222));
        let me = owner_word(Some(woken));
        assert!(m.spin_after_wake(me));
        assert_eq!(m.current_owner(), Some(woken));
        assert_eq!(m.entry_count.load(Ordering::Relaxed), 1);
        assert!(m.exit_reporting_release(woken).unwrap());
        assert!(m.try_enter(owner));
        assert!(!m.spin_after_wake(me));
        assert_eq!(m.current_owner(), Some(owner), "a held monitor is never taken");
        assert_eq!(m.spinners.load(Ordering::SeqCst), 0, "not counted as a spinner");
        assert!(m.exit_reporting_release(owner).unwrap());
    }

    /// Round 12 wave 1 (lane lock): a dying thread's sweep that meets the
    /// index drained by a concurrent `remap_after_gc` (`remap_seq` odd) waits
    /// the drain out and still releases what the thread owns -- the sweep no
    /// longer runs under the GC barrier's lock, which used to rule the
    /// overlap out.
    #[test]
    fn the_death_sweep_waits_out_an_index_drain_and_still_releases() {
        let table = Arc::new(MonitorTable::new());
        let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
        let (dead, other) = (ThreadId(231), ThreadId(232));
        force_inflated(&table, obj, dead);
        table.enter(obj, dead);
        assert_eq!(table.current_owner(obj), Some(dead));
        // A remap "in progress": odd.
        table.remap_seq.fetch_add(1, Ordering::AcqRel);
        let t2 = Arc::clone(&table);
        let finisher = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            t2.remap_seq.fetch_add(1, Ordering::AcqRel);
        });
        table.release_monitors_held_by(dead);
        finisher.join().unwrap();
        assert_eq!(table.current_owner(obj), None, "released after the drain");
        assert_eq!(table.remap_seq.load(Ordering::Acquire) & 1, 0);
        assert!(table.enter_or_contend(obj, other).is_none());
        table.exit(obj, other).unwrap();
    }

    /// Round 11 wave 19 (lock): the monitor's three cache lines -- nothing a
    /// contender writes shares the owner word's line, and the state mutex
    /// shares neither -- and the index epoch alone on its line.
    /// `r12w8-monitor-wait-leaves-the-jfr-enter-flag-to-the-next-owner`: the
    /// owner that takes the monitor while a JFR-recorded owner waits does not
    /// inherit its enter flag, and the waiter gets it back.
    #[test]
    fn wait_hands_the_next_owner_no_jfr_enter_flag_and_restores_it() {
        let m = Arc::new(Monitor::new());
        let a = ThreadId(411);
        let b = ThreadId(412);
        m.block_enter(a);
        m.set_jfr_enter_recorded(a);
        assert!(m.jfr_enter_recorded());
        let other = Arc::clone(&m);
        let seen = std::thread::spawn(move || {
            other.block_enter(b);
            let inherited = other.jfr_enter_recorded();
            other.notify(b).unwrap();
            assert!(other.exit_reporting_release(b).unwrap());
            inherited
        });
        let _ = m.wait(a, Some(5_000), None, None, None).unwrap();
        assert!(!seen.join().unwrap(), "the next owner inherited the waiter's JFR flag");
        assert_eq!(m.current_owner(), Some(a));
        assert!(m.jfr_enter_recorded(), "wait() restores the owner's JFR flag");
        assert!(m.exit_reporting_release(a).unwrap());
    }

    #[test]
    fn the_owner_word_shares_its_line_with_no_contender_written_word() {
        let line = |off: usize| off / 64;
        let owner = line(std::mem::offset_of!(Monitor, owner));
        for (name, off) in [
            ("entry_waiters", std::mem::offset_of!(Monitor, entry_waiters)),
            ("spinners", std::mem::offset_of!(Monitor, spinners)),
            ("succ_pending", std::mem::offset_of!(Monitor, succ_pending)),
            ("spin_limit", std::mem::offset_of!(Monitor, spin_limit)),
            ("state", std::mem::offset_of!(Monitor, state)),
        ] {
            assert_ne!(line(off), owner, "{name} is on the owner line");
        }
        assert_ne!(
            line(std::mem::offset_of!(Monitor, state)),
            line(std::mem::offset_of!(Monitor, spinners)),
            "the state mutex is on the hand-off line"
        );
        let count = line(std::mem::offset_of!(Monitor, entry_count));
        assert_ne!(count, owner, "the entry count is on the owner line");
        assert_ne!(
            count,
            line(std::mem::offset_of!(Monitor, spinners)),
            "the entry count is on the hand-off line"
        );
        assert_ne!(
            count,
            line(std::mem::offset_of!(Monitor, state)),
            "the entry count shares the state mutex's line"
        );
        let m = Arc::new(Monitor::new());
        assert_eq!(Arc::as_ptr(&m) as usize % 64, 0);
        let table = MonitorTable::new();
        assert_eq!(&*table.index_epoch as *const IndexEpochCell as usize % 64, 0);
        assert_eq!(std::mem::size_of::<IndexEpochCell>(), 64);
    }

    /// The spin wins a monitor that is free, grows its budget when it does,
    /// gives up on a monitor held for the whole budget, and halves the budget
    /// when it does — never leaving `SPIN_MIN..=SPIN_MAX`.
    #[test]
    fn spin_try_enter_is_bounded_and_adapts() {
        let m = Monitor::new();
        let owner = ThreadId(1);
        let spinner = ThreadId(2);
        let budget = |m: &Monitor| m.spin_limit.load(Ordering::Relaxed);
        assert_eq!(budget(&m), SPIN_INITIAL);

        assert!(
            m.spin_try_enter_adaptive(spinner),
            "a free monitor is won on the first probe"
        );
        assert_eq!(budget(&m), SPIN_INITIAL + SPIN_BONUS);
        assert!(m.exit(spinner).is_ok());

        assert!(m.try_enter(owner));
        for _ in 0..32 {
            assert!(
                !m.spin_try_enter_adaptive(spinner),
                "an owned monitor is never won by spinning"
            );
            assert!((SPIN_MIN..=SPIN_MAX).contains(&budget(&m)));
        }
        assert_eq!(budget(&m), SPIN_MIN, "repeated failures decay to the floor");
        assert_eq!(
            m.current_owner(),
            Some(owner),
            "a failed spin takes nothing"
        );

        assert!(m.exit(owner).is_ok());
        for _ in 0..64 {
            assert!(m.spin_try_enter_adaptive(spinner));
            assert!(m.exit(spinner).is_ok());
        }
        assert_eq!(
            budget(&m),
            SPIN_MAX,
            "repeated successes grow to the ceiling"
        );
    }

    /// A monitor inflated out of another thread's thin lock remembers that
    /// owner for exactly as long as the owner keeps holding it, and a monitor
    /// that was inflated FIRST and acquired afterwards never carries one. The
    /// second half is what keeps the thread-termination handshake (an
    /// already-inflated `Thread` mirror monitor, held across `mark_dead`) out
    /// of `vm_exec::release_dead_thin_owner`'s reach.
    #[test]
    fn a_thin_seed_names_the_inherited_owner_until_it_first_releases() {
        let table = MonitorTable::new();
        let obj = test_object();
        let owner = ThreadId(11);
        let contender = ThreadId(12);

        table.enter(obj, owner);
        assert!(is_thin_locked(obj));
        let m = table
            .enter_or_contend(obj, contender)
            .expect("a thin lock held by another thread is contended");
        assert!(is_inflated(obj));
        assert_eq!(m.current_owner(), Some(owner));
        assert_eq!(m.thin_seed_owner(), Some(owner));
        // A second contender finds it already inflated: the seed is unchanged.
        let again = table
            .enter_or_contend(obj, contender)
            .expect("still held by the owner");
        assert_eq!(again.thin_seed_owner(), Some(owner));

        // The owner's release ends the inheritance, and nothing re-arms it.
        assert!(table.exit_reporting_release(obj, owner).unwrap());
        assert_eq!(m.thin_seed_owner(), None);
        assert!(table.enter_or_contend(obj, contender).is_none());
        assert_eq!(m.thin_seed_owner(), None);
        assert!(table.exit_reporting_release(obj, contender).unwrap());

        // Inflated first, then acquired: no seed, ever.
        let fat = leaked_heap().alloc_object(ClassId::new(0), 0);
        force_inflated(&table, fat, owner);
        table.enter(fat, owner);
        let fm = table
            .enter_or_contend(fat, contender)
            .expect("held by the owner");
        assert_eq!(fm.thin_seed_owner(), None);
        table.exit(fat, owner).unwrap();

        // `force_release_if_owned_by` (the dead-owner recovery) clears it too.
        let dead = test_object();
        table.enter(dead, owner);
        let dm = table
            .enter_or_contend(dead, contender)
            .expect("held by the owner");
        assert_eq!(dm.thin_seed_owner(), Some(owner));
        assert!(dm.force_release_if_owned_by(owner));
        assert_eq!(dm.thin_seed_owner(), None);
        assert!(table.enter_or_contend(dead, contender).is_none());
        table.exit(dead, contender).unwrap();
    }

    /// Spinning must not change what the contended thin arm ends in: a thin
    /// lock its owner keeps for the whole watch still inflates, still carries
    /// the owner's recursion over, and still reports contention.
    #[test]
    fn a_thin_lock_held_through_the_spin_still_inflates_with_its_recursion() {
        let table = MonitorTable::new();
        let obj = test_object();
        let owner = ThreadId(21);
        table.enter(obj, owner);
        table.enter(obj, owner);
        table.enter(obj, owner);
        assert!(table.enter_or_contend(obj, ThreadId(22)).is_some());
        assert!(is_inflated(obj));
        assert_eq!(table.entry_count(obj), 3);
        for _ in 0..3 {
            table.exit(obj, owner).unwrap();
        }
        assert_eq!(table.current_owner(obj), None);
    }

    /// `enter_or_contend_without_spin` (round 11 wave 8, the JNI excluded arm,
    /// which holds the GC barrier lock across the call) ends in the same states
    /// as the spinning entry: a neutral word thin-locks, a re-entry bumps the
    /// recursion, a word another thread holds inflates with that recursion
    /// carried over and reports contention, and a released inflated monitor is
    /// taken at once.
    #[test]
    fn enter_or_contend_without_spin_has_the_spinning_outcomes() {
        let table = MonitorTable::new();
        let obj = test_object();
        let owner = ThreadId(31);
        let other = ThreadId(32);
        assert!(table.enter_or_contend_without_spin(obj, owner).is_none());
        assert!(table.enter_or_contend_without_spin(obj, owner).is_none());
        assert!(!is_inflated(obj));
        assert!(table.enter_or_contend_without_spin(obj, other).is_some());
        assert!(is_inflated(obj));
        assert_eq!(table.entry_count(obj), 2);
        for _ in 0..2 {
            table.exit(obj, owner).unwrap();
        }
        assert!(table.enter_or_contend_without_spin(obj, other).is_none());
        assert_eq!(table.current_owner(obj), Some(other));
        table.exit(obj, other).unwrap();
    }

    /// gen r4w5/thrash5: the contended-enter spin acquires a free monitor at
    /// once, re-enters one it owns, and never takes one another thread owns.
    #[test]
    fn spin_try_enter_takes_free_and_owned_but_not_foreign_monitors() {
        let m = Monitor::new();
        let (me, other) = (ThreadId(11), ThreadId(12));
        assert!(m.spin_try_enter(me, || true), "a free monitor is acquired");
        assert_eq!(m.current_owner(), Some(me));
        assert!(m.spin_try_enter(me, || true), "an owned monitor is re-entered");
        let mut polls = 0u32;
        assert!(
            !m.spin_try_enter(other, || {
                polls += 1;
                true
            }),
            "a monitor another thread holds is never acquired by the spin"
        );
        assert_eq!(polls, ENTER_SPIN_ROUNDS, "the spin is bounded");
        assert_eq!(m.current_owner(), Some(me));
        assert!(m.exit(me).is_ok());
        assert!(m.exit(me).is_ok());
        assert_eq!(m.current_owner(), None);
    }

    /// gen r4w5/thrash5: `keep_spinning` is polled before every attempt, so a
    /// pending stop-the-world request ends the spin before the next try.
    #[test]
    fn spin_try_enter_stops_when_told_to() {
        let m = Monitor::new();
        assert!(!m.spin_try_enter(ThreadId(21), || false));
        assert_eq!(m.current_owner(), None, "no attempt after a stop");
    }

    /// gen r4w5/thrash5: the owner releasing during the spin is what the
    /// spin is for; the spinner then owns the monitor without parking.
    #[test]
    fn spin_try_enter_acquires_once_the_owner_releases() {
        let m = Monitor::new();
        let (owner, spinner) = (ThreadId(31), ThreadId(32));
        assert!(m.spin_try_enter(owner, || true));
        let mut polls = 0u32;
        let got = m.spin_try_enter(spinner, || {
            polls += 1;
            if polls == 3 {
                // The owner leaves its critical section mid-spin.
                assert!(m.exit(owner).is_ok());
            }
            true
        });
        assert!(got, "the release is observed within the bound");
        assert_eq!(polls, 3);
        assert_eq!(m.current_owner(), Some(spinner));
    }

    /// Round 12 wave 2 (lane lock2, `CRATONVM_MONITOR_LAZY_SPINNERS`): a
    /// spinner counts itself in `spinners` only while an entrant is parked
    /// (the only case in which the count spares anyone a wake-up), and never
    /// leaves the count behind; the parked entrant is still woken by the
    /// owner's release.
    #[test]
    fn a_spinner_counts_itself_only_while_an_entrant_is_parked() {
        let m = Arc::new(Monitor::new());
        let (owner, spinner, parker) = (ThreadId(251), ThreadId(252), ThreadId(253));
        assert!(m.try_enter(owner));
        let mut seen: Vec<u32> = Vec::new();
        let got = m.spin_try_enter(spinner, || {
            seen.push(m.spinners.load(Ordering::SeqCst));
            seen.len() < 4
        });
        assert!(!got, "an owned monitor is never won");
        let expected = u32::from(!lazy_spinner_registration());
        assert!(
            seen.iter().all(|&n| n == expected),
            "nobody parked: {seen:?}"
        );
        assert_eq!(m.spinners.load(Ordering::SeqCst), 0);

        let m2 = Arc::clone(&m);
        let h = std::thread::spawn(move || {
            m2.block_enter(parker);
            assert!(m2.exit_reporting_release(parker).unwrap());
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while m.entry_waiters.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "parker never parked");
            std::thread::yield_now();
        }
        seen.clear();
        let got = m.spin_try_enter(spinner, || {
            seen.push(m.spinners.load(Ordering::SeqCst));
            seen.len() < 4
        });
        assert!(!got);
        assert!(seen.iter().all(|&n| n == 1), "an entrant parked: counted ({seen:?})");
        assert_eq!(m.spinners.load(Ordering::SeqCst), 0, "and uncounted on the way out");
        assert!(m.exit_reporting_release(owner).unwrap());
        h.join().unwrap();
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(m.current_owner(), None);
    }

    /// gen r4w5/thrash5: the busy phase backs off exponentially, capped.
    #[test]
    fn enter_spin_hints_back_off_and_cap() {
        assert_eq!(enter_spin_hints(0), 1);
        assert_eq!(enter_spin_hints(3), 8);
        assert_eq!(enter_spin_hints(8), 256);
        assert_eq!(enter_spin_hints(15), 256);
        assert!(ENTER_SPIN_BUSY_ROUNDS < ENTER_SPIN_ROUNDS);
    }

    /// Round 11 wave 17 (lock): every slot's lessee words sit on a cache line
    /// of their own (the wave-15 `held_index` spread, now by construction),
    /// and a lease hands compiled code exactly its own block, with the owner
    /// word the lessee's monitor ownership is encoded as.
    #[test]
    fn each_lease_block_is_its_own_cache_line_and_names_its_lessee() {
        let slots = LockSlots::new();
        for slot in 0..LOCK_SLOT_COUNT as u32 {
            let at = slots.lease_block(slot) as *const LeaseBlock as usize;
            assert_eq!(at % 64, 0, "slot {slot}");
            if slot > 0 {
                let prev = slots.lease_block(slot - 1) as *const LeaseBlock as usize;
                assert!(at - prev >= 64, "slots {} and {slot} share a line", slot - 1);
            }
        }
        let tid = ThreadId(77);
        let lease = slots.jit_lease(tid).expect("a fresh table has free slots");
        let slot = ObjectHeader::thin_lock_owner(lease.owner_bits);
        assert_eq!(lease.held, slots.lease_block(slot) as *const LeaseBlock as usize);
        assert_eq!(
            slots.lease_block(slot).owner_word.load(Ordering::Relaxed),
            owner_word(Some(tid))
        );
        assert_eq!(slots.lease_block(slot).inflated_key.load(Ordering::Relaxed), 0);
    }

    /// Round 11 wave 17 (lock, proposal W16-1): the helpers hand the inflated
    /// monitor an enter resolved to the lessee's compiled code, keyed by the
    /// object's address and the index epoch it was valid at -- the monitor
    /// the index names, its owner word at the offset compiled code CASes --
    /// and an index mutation leaves the handed-out epoch behind, which is
    /// what makes compiled code refuse the entry.
    #[test]
    fn an_inflated_enter_hands_its_monitor_to_the_lessees_compiled_code() {
        let table = MonitorTable::new();
        let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
        let tid = ThreadId(62);
        let lease = table.jit_thin_lease(tid).expect("free slots");
        let slot = ObjectHeader::thin_lock_owner(lease.owner_bits);
        let block = table.lock_slots.lease_block(slot);
        // A thin enter shares nothing.
        assert!(table.enter_or_contend(obj, tid).is_none());
        assert_eq!(block.inflated_key.load(Ordering::Relaxed), 0);
        assert!(table.exit_reporting_release(obj, tid).unwrap());
        force_inflated(&table, obj, tid);
        assert!(table.enter_or_contend(obj, tid).is_none());
        let key = obj.as_ptr() as u64;
        let m = table.lookup_indexed(obj).expect("inflated => monitor");
        assert_eq!(block.inflated_key.load(Ordering::Relaxed), key);
        assert_eq!(
            block.inflated_monitor.load(Ordering::Relaxed),
            Arc::as_ptr(&m) as u64
        );
        assert_eq!(
            block.epoch_addr.load(Ordering::Relaxed),
            &table.index_epoch.value as *const AtomicU64 as u64
        );
        let epoch = block.inflated_epoch.load(Ordering::Relaxed);
        assert_eq!(epoch, table.index_epoch.load(Ordering::Acquire));
        // What compiled code reads at the owner-word offset is the lessee.
        // SAFETY: `m` is alive (the index holds it) and the offset is pinned
        // against the field by the layout assertions under `Monitor`.
        let owner_at_offset = unsafe {
            (*((Arc::as_ptr(&m) as *const u8)
                .add(cratonvm_jit::runtime_lowering::INFLATED_MONITOR_OWNER_OFFSET)
                as *const AtomicU64))
                .load(Ordering::Relaxed)
        };
        assert_eq!(owner_at_offset, block.owner_word.load(Ordering::Relaxed));
        assert!(table.exit_reporting_release(obj, tid).unwrap());
        // Any index mutation moves the epoch past the handed-out one.
        table.note_index_mutation();
        assert_ne!(table.index_epoch.load(Ordering::Acquire), epoch);
    }

    /// Round 11 wave 15 (lock): compiled code's inline thin lock
    /// (`jit/src/runtime_lowering.rs::emit_inline_thin_lock`) CASes the owner
    /// bits [`MonitorTable::jit_thin_lease`] publishes into the mark word and
    /// adds / subtracts one at its `held` address (since wave 17 a plain `ADD`
    /// / `SUB` on the lessee's `LeaseBlock::acquired`; an inflation uncounts
    /// in `LockSlots::stolen`). This drives those words exactly as the emitted
    /// code does, interleaved with the helpers, and checks the live count
    /// stays exact: a compiled acquisition is one the helpers re-enter and
    /// release, a helper acquisition is one compiled code releases, an
    /// inflation of a compiled lock uncounts it, and a thread that dies
    /// holding a compiled lock keeps its lease.
    #[test]
    fn a_jit_lease_counts_compiled_thin_locks_exactly() {
        let heap = leaked_heap();
        let table = MonitorTable::new();
        let (a, b, c) = (ThreadId(41), ThreadId(42), ThreadId(43));
        let lease = table.jit_thin_lease(a).expect("a fresh table has free slots");
        assert_eq!(table.jit_thin_lease(a), Some(lease), "one lease per thread");
        let slot = ObjectHeader::thin_lock_owner(lease.owner_bits);
        assert_eq!(lease.owner_bits, ObjectHeader::make_thin_locked(0, slot, 0));
        // SAFETY: `held` is the address of a `LeaseBlock` inside `table`'s
        // `LockSlots`, which outlives every use below, and its first word is
        // the lessee's `acquired` count, an `AtomicU32`.
        let held = unsafe { &*(lease.held as *const AtomicU32) };
        let count = || table.lock_slots.live_thin_locks(slot);
        let compiled_enter = |obj: ObjectRef| -> bool {
            let h = header_of(obj);
            let cur = h.mark_word.load(Ordering::Relaxed);
            if !ObjectHeader::thin_lockable(cur) {
                return false;
            }
            let won = h
                .mark_word
                .compare_exchange(cur, cur | lease.owner_bits, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok();
            if won {
                held.fetch_add(1, Ordering::AcqRel);
            }
            won
        };
        let compiled_exit = |obj: ObjectRef| -> bool {
            let h = header_of(obj);
            let cur = h.mark_word.load(Ordering::Relaxed);
            if cur & 0xFFFF != lease.owner_bits {
                return false;
            }
            let won = h
                .mark_word
                .compare_exchange(cur, cur ^ lease.owner_bits, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok();
            if won {
                held.fetch_sub(1, Ordering::AcqRel);
            }
            won
        };
        let x = heap.alloc_object(ClassId::new(0), 0);
        let y = heap.alloc_object(ClassId::new(0), 0);

        // A compiled acquisition is the helpers' thin lock under A's lease...
        assert!(compiled_enter(x));
        assert!(table.holds(x, a));
        assert_eq!(count(), 1);
        // ...which the helper re-enters (no count) and releases (uncounts).
        table.enter(x, a);
        assert_eq!(count(), 1, "a recursive acquisition is not counted");
        assert_eq!(table.exit_reporting_release(x, a).ok(), Some(false));
        assert_eq!(table.exit_reporting_release(x, a).ok(), Some(true));
        assert_eq!(count(), 0);
        // A helper acquisition is released by compiled code.
        table.enter(x, a);
        assert_eq!(count(), 1);
        assert!(compiled_exit(x));
        assert_eq!(count(), 0);
        assert!(!table.holds(x, a));
        // Recursion is the helper's on both sides.
        assert!(compiled_enter(x));
        table.enter(x, a);
        assert!(!compiled_exit(x), "recursion 1 is the helper's");
        assert_eq!(table.exit_reporting_release(x, a).ok(), Some(false));
        assert!(compiled_exit(x));
        assert_eq!(count(), 0);

        // A contender inflates a compiled lock: the count moves with it.
        assert!(compiled_enter(y));
        let m = table
            .enter_or_contend_without_spin(y, b)
            .expect("thin-locked by A: contended");
        assert_eq!(m.current_owner(), Some(a), "inflation seeds A from its lease");
        assert_eq!(count(), 0, "inflation uncounts the thin lock it moved");
        assert!(!compiled_exit(y), "an inflated word is the helper's");
        assert!(!compiled_enter(y), "and so is entering it");
        assert_eq!(table.exit_reporting_release(y, a).ok(), Some(true));

        // A thread that dies holding a compiled thin lock keeps its lease, so
        // no later thread can be taken for that lock's owner.
        assert!(compiled_enter(x));
        table.release_monitors_held_by(a);
        let lease_c = table.jit_thin_lease(c).expect("free slots remain");
        assert_ne!(lease_c.owner_bits, lease.owner_bits, "A's slot is still A's");
        assert_eq!(table.current_owner(x), Some(a));
        // Reclaiming while that lock is still counted keeps the lease too.
        table.reclaim_dead_thread_lease(a);
        assert_eq!(count(), 1);
        assert_eq!(table.current_owner(x), Some(a));
        // A contender inflates it (seeding the monitor with A) and uncounts
        // it; now the lease can go back, and the next thread to lease gets
        // A's slot (leases are handed out lowest-first).
        let m = table
            .enter_or_contend_without_spin(x, b)
            .expect("thin-locked by dead A: contended");
        assert_eq!(m.current_owner(), Some(a));
        assert!(m.force_release_if_owned_by(a));
        assert_eq!(count(), 0);
        table.reclaim_dead_thread_lease(a);
        let d = ThreadId(44);
        assert_eq!(table.jit_thin_lease(d), Some(lease), "A's slot and counter, reused");
    }

    /// Round 11 wave 16 (lock), `r11w15-lock-lease-exhaustion-and-carrier-cache-cliff`:
    /// a thread refused a lock slot caches the refusal (no second locked
    /// 2048-slot scan), the refusal dies the moment a slot frees, and a lease
    /// taken on ANOTHER OS thread -- a virtual thread that moved carriers --
    /// is found by a thread whose cache still holds the old refusal.
    #[test]
    fn a_refused_lease_is_cached_until_a_slot_frees_and_never_hides_a_later_lease() {
        let slots = LockSlots::new();
        for t in 0..LOCK_SLOT_COUNT as u64 {
            assert!(slots.slot_for(ThreadId(1_000 + t)).is_some());
        }
        let late = ThreadId(900_001);
        assert_eq!(slots.slot_for(late), None, "a full table refuses");
        assert_eq!(slots.cached_slot(late), Some(None), "the refusal is cached");
        assert_eq!(slots.slot_for(late), None, "and answered from the cache");
        assert_eq!(slots.existing_slot(late), None);

        slots.release(ThreadId(1_000));
        assert_eq!(
            slots.cached_slot(late),
            None,
            "a freed slot invalidates every cached refusal"
        );
        let leased = std::thread::scope(|s| s.spawn(|| slots.slot_for(late)).join().unwrap());
        let slot = leased.expect("the freed slot is leased on the other carrier");
        assert_eq!(slots.existing_slot(late), Some(slot));
        assert_eq!(slots.slot_for(late), Some(slot));
    }

    /// Round 11 wave 16 (lock): a carrier alternating between a handful of
    /// virtual threads (consecutive `ThreadId`s) keeps every one's lease in
    /// its thread-local cache instead of evicting on each mount switch.
    #[test]
    fn the_lease_cache_keeps_one_entry_per_alternating_thread() {
        let slots = LockSlots::new();
        let tids: Vec<ThreadId> = (0..LOCK_SLOT_CACHE_WAYS as u64)
            .map(|i| ThreadId(7_000 + i))
            .collect();
        let leased: Vec<u32> = tids
            .iter()
            .map(|&t| slots.slot_for(t).expect("free slots"))
            .collect();
        for _ in 0..3 {
            for (t, slot) in tids.iter().zip(&leased) {
                assert_eq!(slots.cached_slot(*t), Some(Some(*slot)));
            }
        }
    }

    /// Round 11 wave 16 (lock): an inflated monitor is resolved through the
    /// thread-local index cache, and every index mutation -- here a forced
    /// re-key by a relocation -- invalidates it, so a moved object never
    /// reaches its monitor through the old address's entry.
    #[test]
    fn the_inflated_monitor_cache_is_filled_by_a_lookup_and_dropped_by_a_rekey() {
        let table = MonitorTable::new();
        let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
        let tid = ThreadId(61);
        force_inflated(&table, obj, tid);
        let key = obj.as_ptr() as usize;
        let m = table.lookup_indexed(obj).expect("inflated => monitor");
        assert_eq!(table.cached_inflated_monitor(key), Some(Arc::as_ptr(&m)));
        // Contended-path round trip through the cache.
        assert!(table.enter_or_contend(obj, tid).is_none());
        assert!(m.is_held_by(tid));
        assert!(table.exit_reporting_release(obj, tid).unwrap());
        assert_eq!(m.current_owner(), None);

        let mut moved = cratonvm_types::PointerMap::default();
        moved.insert(key, key + 0x1000);
        table.remap_after_gc(&moved);
        assert_eq!(
            table.cached_inflated_monitor(key),
            None,
            "a re-key invalidates the cache"
        );
    }

    /// Round 12 wave 8 (lane monitor): the lost-race back-off grows from its
    /// floor, stops at its ceiling, never spends more than the room the
    /// spin's budget has left, and does nothing (keeping the old budget
    /// adaptation) when switched off.
    #[test]
    fn lost_cas_backoff_windows_grow_cap_and_fit_the_room() {
        let mut b = LostCasBackoff::new(true);
        assert!(!b.saw_a_handover());
        assert_eq!(b.after_loss(10_000), LOST_CAS_BACKOFF_MIN);
        assert_eq!(b.after_loss(10_000), LOST_CAS_BACKOFF_MIN * 2);
        for _ in 0..16 {
            assert!(b.after_loss(10_000) <= LOST_CAS_BACKOFF_MAX);
        }
        assert_eq!(b.after_loss(10_000), LOST_CAS_BACKOFF_MAX);
        assert_eq!(b.after_loss(3), 3, "never past the budget's room");
        assert_eq!(b.after_loss(0), 0);
        assert!(b.saw_a_handover());

        let mut off = LostCasBackoff::new(false);
        assert_eq!(off.after_loss(10_000), 0);
        assert!(
            !off.saw_a_handover(),
            "switched off, a failed spin halves the budget as before"
        );
    }

    /// Round 12 wave 8 (lane monitor, `CRATONVM_MONITOR_QUIET_RELEASE`): a
    /// release that skips re-writing an already-clear `thin_seed` /
    /// `jfr_enter_recorded` ends in exactly the state the unconditional
    /// stores leave -- through a seeded inflation, re-entry, a JFR-recorded
    /// outermost enter, `wait`'s release and a dead owner's forced release.
    #[test]
    fn a_quiet_release_ends_in_the_state_a_plain_one_does() {
        for quiet in [true, false] {
            let tuning = MonitorTuning {
                quiet_release: quiet,
                ..MonitorTuning::DEFAULT
            };
            let a = ThreadId(301);
            let m = Monitor::with_tuning(tuning);
            m.enter_with_recursion(a, 2);
            assert!(!m.exit_reporting_release(a).unwrap());
            assert_eq!(m.thin_seed_owner(), Some(a), "quiet={quiet}");
            assert!(m.exit_reporting_release(a).unwrap());
            assert_eq!(m.thin_seed_owner(), None, "quiet={quiet}");
            assert!(m.try_enter(a));
            assert!(m.exit_reporting_release(a).unwrap());
            assert_eq!(m.thin_seed_owner(), None, "quiet={quiet}");

            assert!(m.try_enter(a));
            m.set_jfr_enter_recorded(a);
            assert!(m.try_enter(a));
            assert!(!m.exit_reporting_release(a).unwrap());
            assert!(m.jfr_enter_recorded(), "an inner release keeps it, quiet={quiet}");
            assert!(m.exit_reporting_release(a).unwrap());
            assert!(!m.jfr_enter_recorded(), "quiet={quiet}");
            assert!(m.try_enter(a));
            assert!(m.exit_reporting_release(a).unwrap());
            assert!(!m.jfr_enter_recorded(), "quiet={quiet}");
            assert_eq!(m.current_owner(), None);
            assert_eq!(m.entry_count.load(Ordering::Relaxed), 0);

            // `wait` releases through the same word and restores the depth.
            assert!(m.try_enter(a));
            assert!(m.try_enter(a));
            assert_eq!(
                m.wait(a, Some(1), None, None, None).unwrap(),
                WaitOutcome::TimedOutOrSpurious
            );
            assert_eq!(m.current_owner(), Some(a));
            assert_eq!(m.entry_count.load(Ordering::Relaxed), 2);
            assert!(!m.exit_reporting_release(a).unwrap());
            assert!(m.exit_reporting_release(a).unwrap());

            let f = Monitor::with_tuning(tuning);
            f.enter_with_recursion(a, 1);
            assert!(f.force_release_if_owned_by(a));
            assert_eq!(f.thin_seed_owner(), None, "quiet={quiet}");
            assert_eq!(f.current_owner(), None);
            assert!(f.try_enter(ThreadId(302)), "free for the next owner");
        }
    }

    /// Round 12 wave 8 (lane monitor): a crowd of Rust-side spinners that
    /// back off after every lost race -- falling back to the real park when a
    /// spin runs out -- still gets every thread through with the protected
    /// counter exact (no double ownership, no lost wake-up), in both arms of
    /// `CRATONVM_MONITOR_SPIN_BACKOFF`, and leaves the budget in range.
    #[test]
    fn backing_off_spinners_lose_no_update_and_no_wakeup() {
        const THREADS: u64 = 8;
        const ROUNDS: u64 = 3_000;
        for backoff in [true, false] {
            let m = Arc::new(Monitor::with_tuning(MonitorTuning {
                lost_cas_backoff: backoff,
                ..MonitorTuning::DEFAULT
            }));
            let counter = Arc::new(AtomicU64::new(0));
            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let m = Arc::clone(&m);
                    let counter = Arc::clone(&counter);
                    std::thread::spawn(move || {
                        let me = ThreadId(400 + t);
                        for _ in 0..ROUNDS {
                            if !m.spin_try_enter_adaptive(me) {
                                m.block_enter(me);
                            }
                            // A torn read-modify-write would lose an update.
                            let v = counter.load(Ordering::Relaxed);
                            std::hint::spin_loop();
                            counter.store(v + 1, Ordering::Relaxed);
                            assert!(m.try_enter(me), "re-entry inside the section");
                            assert!(!m.exit_reporting_release(me).unwrap());
                            assert!(m.exit_reporting_release(me).unwrap());
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(counter.load(Ordering::Relaxed), THREADS * ROUNDS, "backoff={backoff}");
            assert_eq!(m.current_owner(), None);
            assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
            assert_eq!(m.spinners.load(Ordering::SeqCst), 0);
            assert!((SPIN_MIN..=SPIN_MAX).contains(&m.spin_limit.load(Ordering::Relaxed)));
        }
    }

    /// Round 12 wave 8 (lane monitor): every monitor a table inflates carries
    /// that table's switches, whichever arm of `inflate_locked` made it.
    #[test]
    fn an_inflated_monitor_carries_its_tables_switches() {
        let table = MonitorTable::new();
        let tid = ThreadId(311);
        let fresh = leaked_heap().alloc_object(ClassId::new(0), 0);
        force_inflated(&table, fresh, tid);
        let m = table.lookup_indexed(fresh).expect("inflated => monitor");
        assert_eq!(m.tuning, table.tuning);

        let thin = test_object();
        table.enter(thin, tid);
        let seeded = table
            .enter_or_contend(thin, ThreadId(312))
            .expect("a thin lock held by another thread is contended");
        assert_eq!(seeded.tuning, table.tuning);
        table.exit(thin, tid).unwrap();
    }

    /// Round 13 wave 10 (lane monitor2, lock proposal W17-3): a fill of the
    /// lease cache keeps the entry it displaces as the second way while both
    /// are valid at the block's one epoch, swaps the two when the second way
    /// is filled again, empties the second way at a new epoch (or when the
    /// same object gets another monitor), and a new lessee starts with both
    /// ways empty.
    #[test]
    fn the_lease_cache_keeps_the_displaced_entry_as_its_second_way_at_one_epoch() {
        let slots = LockSlots::new();
        let tid = ThreadId(531);
        let slot = slots.slot_for(tid).expect("a fresh table has free slots");
        // An empty second way (key 0) reads as `(0, 0)`: its monitor word is
        // left as it was, and nothing reads it behind a zero key.
        let ways = || {
            let b = slots.lease_block(slot);
            let key2 = b.inflated_key2.load(Ordering::Relaxed);
            (
                b.inflated_key.load(Ordering::Relaxed),
                b.inflated_monitor.load(Ordering::Relaxed),
                key2,
                if key2 == 0 {
                    0
                } else {
                    b.inflated_monitor2.load(Ordering::Relaxed)
                },
                b.inflated_epoch.load(Ordering::Relaxed),
            )
        };
        let ea = 0xE000;
        slots.cache_inflated(slot, 0xA0, 0x1A0, 7, ea);
        assert_eq!(ways(), (0xA0, 0x1A0, 0, 0, 7));
        slots.cache_inflated(slot, 0xB0, 0x1B0, 7, ea);
        assert_eq!(ways(), (0xB0, 0x1B0, 0xA0, 0x1A0, 7), "A moved to the second way");
        slots.cache_inflated(slot, 0xA0, 0x1A0, 7, ea);
        assert_eq!(ways(), (0xA0, 0x1A0, 0xB0, 0x1B0, 7), "swapped back");
        slots.cache_inflated(slot, 0xA0, 0x1A0, 7, ea);
        assert_eq!(ways(), (0xA0, 0x1A0, 0xB0, 0x1B0, 7), "a repeat fill changes nothing");
        slots.cache_inflated(slot, 0xC0, 0x1C0, 8, ea);
        assert_eq!(ways(), (0xC0, 0x1C0, 0, 0, 8), "a new epoch empties the second way");
        slots.cache_inflated(slot, 0xD0, 0x1D0, 8, ea);
        slots.cache_inflated(slot, 0xD0, 0x1D1, 8, ea);
        assert_eq!(
            ways(),
            (0xD0, 0x1D1, 0, 0, 8),
            "the same object with another monitor keeps no second way"
        );
        slots.cache_inflated(slot, 0xE0, 0x1E0, 8, ea);
        slots.cache_inflated(slot, 0xF0, 0x1F0, 8, ea + 64);
        assert_eq!(
            ways(),
            (0xF0, 0x1F0, 0, 0, 8),
            "another table's epoch word keeps no second way"
        );

        slots.release(tid);
        let next = slots.slot_for(ThreadId(532)).expect("the slot is free again");
        assert_eq!(next, slot);
        let b = slots.lease_block(next);
        assert_eq!(b.inflated_key.load(Ordering::Relaxed), 0);
        assert_eq!(b.inflated_key2.load(Ordering::Relaxed), 0, "a new lessee's second way");
    }

    /// Round 13 wave 10 (lane monitor2, `CRATONVM_MONITOR_WAIT_REACQUIRE_SPIN`):
    /// a wait/notify ping-pong -- every hand-over is a notified waiter
    /// re-acquiring a monitor its notifier still holds -- alternates exactly,
    /// returns from every `wait` owning the monitor at its depth, and leaves
    /// no registered entrant behind, with the re-acquisition spin on and off.
    #[test]
    fn a_wait_notify_ping_pong_hands_over_exactly_in_both_reacquire_arms() {
        const ROUNDS: u64 = 2_000;
        for spin in [true, false] {
            let m = Arc::new(Monitor::with_tuning(MonitorTuning {
                wait_reacquire_spin: spin,
                ..MonitorTuning::DEFAULT
            }));
            // Whose turn it is (0 or 1); read and written only while owning `m`.
            let turn = Arc::new(AtomicU64::new(0));
            let handoffs = Arc::new(AtomicU64::new(0));
            let players: Vec<_> = (0..2u64)
                .map(|side| {
                    let m = Arc::clone(&m);
                    let turn = Arc::clone(&turn);
                    let handoffs = Arc::clone(&handoffs);
                    std::thread::spawn(move || {
                        let me = ThreadId(500 + side);
                        let never = std::sync::atomic::AtomicBool::new(false);
                        for _ in 0..ROUNDS {
                            m.enter(me);
                            assert!(m.try_enter(me), "re-entry: `wait` must restore depth 2");
                            while turn.load(Ordering::Relaxed) != side {
                                let outcome = m.wait(me, None, Some(&never), None, None).unwrap();
                                assert_ne!(outcome, WaitOutcome::Interrupted);
                                assert_eq!(m.current_owner(), Some(me), "spin={spin}");
                                assert_eq!(m.entry_count.load(Ordering::Relaxed), 2);
                            }
                            handoffs.fetch_add(1, Ordering::Relaxed);
                            turn.store(1 - side, Ordering::Relaxed);
                            m.notify(me).unwrap();
                            assert!(!m.exit_reporting_release(me).unwrap());
                            assert!(m.exit_reporting_release(me).unwrap());
                        }
                    })
                })
                .collect();
            for p in players {
                p.join().unwrap();
            }
            assert_eq!(handoffs.load(Ordering::Relaxed), 2 * ROUNDS, "spin={spin}");
            assert_eq!(m.current_owner(), None);
            assert_eq!(m.entry_count.load(Ordering::Relaxed), 0);
            assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0, "spin={spin}");
        }
    }

    /// Round 13 wave 10 (lane monitor2): a notified waiter whose notifier
    /// keeps the monitor far longer than any spin still re-acquires it (the
    /// spin gives up and the waiter parks as before) and is woken by the
    /// notifier's release.
    #[test]
    fn a_reacquire_spin_that_runs_out_still_parks_and_is_woken() {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
        let (w, n) = (ThreadId(511), ThreadId(512));
        let waiting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let m = Arc::clone(&m);
            let waiting = Arc::clone(&waiting);
            std::thread::spawn(move || {
                let never = std::sync::atomic::AtomicBool::new(false);
                m.enter(w);
                waiting.store(true, Ordering::Release);
                let outcome = m.wait(w, None, Some(&never), None, None).unwrap();
                assert_eq!(outcome, WaitOutcome::Notified);
                assert_eq!(m.current_owner(), Some(w));
                assert!(m.exit_reporting_release(w).unwrap());
            })
        };
        // The waiter releases the monitor inside `wait`; take it and notify.
        while !waiting.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        m.enter(n);
        m.notify(n).unwrap();
        // Hold it well past any spin budget: the waiter must park, not spin.
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(m.exit_reporting_release(n).unwrap());
        waiter.join().unwrap();
        assert_eq!(m.current_owner(), None);
        assert_eq!(m.entry_waiters.load(Ordering::SeqCst), 0);
    }

    /// Round 13 wave 10 (lane monitor2, `CRATONVM_MONITOR_CACHED_NOTIFY`):
    /// `holds`, `notify` and `notifyAll` of an inflated object answer the
    /// same through the thread's monitor cache as through the index --
    /// including for a thread that does not own it, and after a re-key
    /// invalidated the cache.
    #[test]
    fn holds_and_notify_answer_alike_through_the_cache_and_the_index() {
        for cached in [true, false] {
            let mut table = MonitorTable::new();
            table.tuning.cached_notify = cached;
            let (a, b) = (ThreadId(521), ThreadId(522));
            let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
            force_inflated(&table, obj, a);
            let key = obj.as_ptr() as usize;
            assert_eq!(
                table.cached_monitor_of_inflated(obj).is_some(),
                cached,
                "the setup's lookups filled the cache"
            );
            assert!(!table.holds(obj, a));
            table.enter(obj, a);
            assert!(table.holds(obj, a), "cached={cached}");
            assert!(!table.holds(obj, b), "cached={cached}");
            table.notify(obj, a).unwrap();
            table.notify_all(obj, a).unwrap();
            assert!(table.notify(obj, b).is_err(), "not the owner: IMSE");
            assert!(table.notify_all(obj, b).is_err(), "not the owner: IMSE");

            // A re-key (the object moved) drops the cache entry; the index
            // route still answers for the object at its new address.
            let mut moved = cratonvm_types::PointerMap::default();
            let new_obj = leaked_heap().alloc_object(ClassId::new(0), 0);
            // Both are live test objects of the same shape; carrying the mark
            // word over is what a moving collector's copy does.
            let old_mark = header_of(obj).mark_word.load(Ordering::Relaxed);
            header_of(new_obj).mark_word.store(old_mark, Ordering::Relaxed);
            moved.insert(key, new_obj.as_ptr() as usize);
            table.remap_after_gc(&moved);
            assert_eq!(table.cached_monitor_of_inflated(new_obj), None);
            assert!(table.holds(new_obj, a), "cached={cached}");
            table.notify(new_obj, a).unwrap();
            assert!(table.exit_reporting_release(new_obj, a).unwrap());
            assert!(!table.holds(new_obj, a));
        }
    }

    /// Round 13 wave 12 (lane monitor3, `CRATONVM_JIT_INFLATED_STICKY_COUNT`):
    /// compiled code's final release of an inflated monitor clears the owner
    /// word and leaves `entry_count` at 1. Every reader that does not own the
    /// monitor must read that as unowned (idle, count 0), and the next
    /// acquisition -- Rust or compiled -- must count exactly from 1 whatever
    /// the unowned count read.
    #[test]
    fn a_count_left_at_one_by_a_compiled_release_reads_as_unowned() {
        let table = MonitorTable::new();
        let (a, b) = (ThreadId(611), ThreadId(612));
        let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
        force_inflated(&table, obj, a);
        let m = table
            .lookup_indexed(obj)
            .expect("an inflated object is indexed");
        assert!(m.is_idle());
        // The Rust acquisition; then the compiled release's shape: the owner
        // word alone (`XCHG [owner], 0`).
        table.enter(obj, a);
        assert_eq!(table.entry_count(obj), 1);
        m.owner.store(0, Ordering::SeqCst);
        assert_eq!(m.entry_count.load(Ordering::Relaxed), 1, "left at 1");
        assert!(m.is_idle(), "unowned is idle whatever the count reads");
        assert_eq!(m.held_entry_count(), 0);
        assert_eq!(table.entry_count(obj), 0, "the inspection reads unowned");
        assert!(!table.holds(obj, a));
        // The next owner counts from 1: re-entry, the non-final and the final
        // Rust release, which still clears the count.
        table.enter(obj, b);
        table.enter(obj, b);
        assert_eq!(table.entry_count(obj), 2);
        assert!(!table.exit_reporting_release(obj, b).unwrap());
        assert!(table.exit_reporting_release(obj, b).unwrap());
        assert_eq!(m.entry_count.load(Ordering::Relaxed), 0);
        assert!(m.is_idle());
        // Any stale unowned count is overwritten by the acquisition.
        m.entry_count.store(9, Ordering::Relaxed);
        table.enter(obj, a);
        assert_eq!(table.entry_count(obj), 1);
        assert!(table.exit_reporting_release(obj, a).unwrap());
        assert_eq!(table.entry_count(obj), 0);
    }

    /// Round 14 wave 1 (lane sync, M2-1): the inline spin's census words are
    /// drained into the process census only under
    /// `CRATONVM_DBG_MONITOR_CONTENTION`, and a drain touches nothing else of
    /// the monitor. (The words' offsets are pinned against the JIT's
    /// `INFLATED_MONITOR_CENSUS_SPIN_*_OFFSET` at compile time, above.)
    #[test]
    fn the_inline_spin_census_words_drain_only_under_the_census() {
        let m = Monitor::new();
        m.census_inline_spin_wins.store(3, Ordering::Relaxed);
        m.census_inline_spin_budget_outs.store(2, Ordering::Relaxed);
        m.census_inline_spin_waiter_exits.store(1, Ordering::Relaxed);
        let before = CONTENTION_CENSUS[ContentionEvent::InlineSpinWin as usize].load(Ordering::Relaxed);
        m.drain_inline_spin_census();
        let on = contention_census_on();
        let left = |w: &AtomicU64| w.load(Ordering::Relaxed);
        assert_eq!(left(&m.census_inline_spin_wins), if on { 0 } else { 3 });
        assert_eq!(left(&m.census_inline_spin_budget_outs), if on { 0 } else { 2 });
        assert_eq!(left(&m.census_inline_spin_waiter_exits), if on { 0 } else { 1 });
        if on {
            // Other tests may count concurrently: at least ours arrived.
            assert!(
                CONTENTION_CENSUS[ContentionEvent::InlineSpinWin as usize].load(Ordering::Relaxed)
                    >= before + 3
            );
        }
        assert!(m.is_idle());
        assert_eq!(m.entry_count.load(Ordering::Relaxed), 0);
        assert_eq!(
            CONTENTION_EVENT_NAMES[ContentionEvent::InlineSpinWaiterExit as usize],
            "inline_spin_waiter_exits"
        );
    }

    /// Spin (yielding) until `done` holds, failing after 10 s.
    fn await_condition(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::yield_now();
        }
    }

    /// Round 14 wave 3 (lane monitor, M2-3, `CRATONVM_MONITOR_NOTIFY_ONE_WAITER`):
    /// each `notify()` returns exactly one waiter -- the longest -- and leaves
    /// every other one in the wait set, in both arms; with the switch on, no
    /// waiter parks on the shared condvar (so no `notify` can wake the others)
    /// and `notify` signals only the entry it pops.
    #[test]
    fn m2_3_notify_returns_exactly_the_marked_waiter_in_both_arms() {
        const N: u64 = 16;
        for one_waiter in [true, false] {
            let m = Arc::new(Monitor::with_tuning(MonitorTuning {
                notify_one_waiter: one_waiter,
                ..MonitorTuning::DEFAULT
            }));
            let returned = Arc::new(AtomicU64::new(0));
            let order = Arc::new(Mutex::new(Vec::new()));
            let mut waiters = Vec::new();
            for i in 0..N {
                let (wm, wreturned, worder) =
                    (Arc::clone(&m), Arc::clone(&returned), Arc::clone(&order));
                let tid = ThreadId(600 + i);
                // Enrol one at a time, so arrival order is the ticket order.
                let before = m.state.lock().wait_queue.len();
                waiters.push(std::thread::spawn(move || {
                    let never = std::sync::atomic::AtomicBool::new(false);
                    wm.enter(tid);
                    let outcome = wm.wait(tid, None, Some(&never), None, None).unwrap();
                    assert_eq!(outcome, WaitOutcome::Notified);
                    assert_eq!(wm.current_owner(), Some(tid));
                    worder.lock().push(i);
                    wreturned.fetch_add(1, Ordering::SeqCst);
                    wm.exit(tid).unwrap();
                }));
                await_condition("a waiter to enrol", || m.state.lock().wait_queue.len() == before + 1);
            }
            let notifier = ThreadId(699);
            for k in 1..=N {
                if one_waiter && monitor_pending_notify() {
                    // Measured on THIS monitor only (the process-wide
                    // `CONDVAR_SIGNALLED_RETURNS` also counts every other
                    // test's waits): every waiter carries a condvar of its
                    // own, and nobody parks on the shared one -- a
                    // `notify_all` there finds no thread to wake. (With the
                    // switch off the waiters park there, so this probe would
                    // be a spurious wake-up; it runs in the ON arm only.)
                    assert!(m.state.lock().wait_queue.iter().all(|e| e.signal.is_some()));
                    assert_eq!(m.wait_condvar.notify_all(), 0, "a waiter parked on the shared condvar");
                }
                m.enter(notifier);
                m.notify(notifier).unwrap();
                m.exit(notifier).unwrap();
                await_condition("the notified waiter to return", || {
                    returned.load(Ordering::SeqCst) == k
                });
                // Several poll slices: nobody else may leave.
                std::thread::sleep(std::time::Duration::from_millis(15));
                assert_eq!(returned.load(Ordering::SeqCst), k, "one_waiter={one_waiter}");
                assert_eq!(
                    m.state.lock().wait_queue.len() as u64,
                    N - k,
                    "one_waiter={one_waiter}"
                );
            }
            for w in waiters {
                w.join().unwrap();
            }
            assert_eq!(*order.lock(), (0..N).collect::<Vec<_>>(), "FIFO, one_waiter={one_waiter}");
            let s = m.state.lock();
            assert_eq!(s.pending_notifies, 0);
            assert!(s.notified_tickets.is_empty() && s.wait_queue.is_empty());
            assert_eq!(s.parked_waiters, 0);
            drop(s);
        }
        // `notify` signals only the entry it pops: an entry with a condvar of
        // its own asks for no shared signal, one without does.
        let mut s = MonitorState {
            parked_waiters: 0,
            pending_notifies: 0,
            wait_queue: std::collections::VecDeque::new(),
            notified_tickets: Vec::new(),
            next_wait_ticket: 0,
        };
        let own = Arc::new(Condvar::new());
        let t0 = s.enroll_waiter(Some(Arc::clone(&own)), ThreadId(0));
        let t1 = s.enroll_wait_ticket();
        assert_eq!(s.notify_longest_waiter(), Some(false));
        assert_eq!(Arc::strong_count(&own), 1, "the popped entry's clone is gone");
        assert_eq!(s.notify_longest_waiter(), Some(true));
        assert_eq!(s.notify_longest_waiter(), None);
        assert!(s.take_notification(t0) && s.take_notification(t1));
    }

    /// Round 14 wave 3 (lane monitor, M2-3): an interrupt wake reaches a
    /// waiter parked on its own condvar, and `notifyAll` returns every
    /// waiter parked on one.
    #[test]
    fn m2_3_interrupt_wake_and_notify_all_reach_waiters_on_their_own_condvars() {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
        let flags: Vec<_> =
            (0..3).map(|_| Arc::new(std::sync::atomic::AtomicBool::new(false))).collect();
        let waiters: Vec<_> = (0..3u64)
            .map(|i| {
                let (m, flag) = (Arc::clone(&m), Arc::clone(&flags[i as usize]));
                std::thread::spawn(move || {
                    let tid = ThreadId(700 + i);
                    m.enter(tid);
                    let outcome = m.wait(tid, None, Some(&flag), None, None).unwrap();
                    m.exit(tid).unwrap();
                    outcome
                })
            })
            .collect();
        await_condition("three waiters to enrol", || m.state.lock().wait_queue.len() == 3);
        let mut waiters = waiters.into_iter();
        let first = waiters.next().unwrap();
        // Let the slices back off, then interrupt the first-spawned waiter,
        // whatever its place in the wait set.
        std::thread::sleep(std::time::Duration::from_millis(300));
        flags[0].store(true, Ordering::Release);
        m.wake_all_for_interrupt();
        assert_eq!(first.join().unwrap(), WaitOutcome::Interrupted);
        assert_eq!(m.state.lock().wait_queue.len(), 2);
        let notifier = ThreadId(799);
        m.enter(notifier);
        m.notify_all(notifier).unwrap();
        m.exit(notifier).unwrap();
        for w in waiters {
            assert_eq!(w.join().unwrap(), WaitOutcome::Notified);
        }
        assert!(m.state.lock().wait_queue.is_empty());
        assert_eq!(m.current_owner(), None);
    }

    /// Round 14 wave 3 (lane monitor, the stack-dump wake): the watchdog's
    /// wake reaches every waiting monitor in the index, marks nothing (the
    /// waiter stays in `wait` until a real `notify`), and inflates nothing.
    #[test]
    fn the_stack_dump_wake_signals_every_waiter_and_marks_nothing() {
        let table = Arc::new(MonitorTable::new());
        let (obj, idle) = (test_object(), test_object());
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let (t, flag) = (Arc::clone(&table), Arc::clone(&flag));
            std::thread::spawn(move || {
                t.enter(obj, ThreadId(721));
                let outcome = t.wait_outcome(obj, ThreadId(721), None, Some(&flag), None).unwrap();
                t.exit(obj, ThreadId(721)).unwrap();
                outcome
            })
        };
        await_condition("the waiter to enrol", || {
            table
                .lookup_indexed(obj)
                .is_some_and(|m| m.state.lock().wait_queue.len() == 1)
        });
        let monitor = table.lookup_indexed(obj).unwrap();
        let (_, wakes_before) = monitor.notify_totals();
        table.wake_all_waiters_for_stack_dump();
        assert_eq!(monitor.notify_totals().1, wakes_before + 1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        {
            let s = monitor.state.lock();
            assert_eq!(s.wait_queue.len(), 1, "a wake is not a notification");
            assert_eq!(s.pending_notifies, 0);
        }
        assert!(!is_inflated(idle));
        table.enter(obj, ThreadId(722));
        table.notify(obj, ThreadId(722)).unwrap();
        table.exit(obj, ThreadId(722)).unwrap();
        assert_eq!(waiter.join().unwrap(), WaitOutcome::Notified);
    }

    /// Round 14 wave 3 (lane monitor, M2-4, `CRATONVM_MONITOR_WAIT_POLL_BACKOFF`):
    /// the slice doubles from 5 ms to the 100 ms cap with the switch, and
    /// stays at 5 ms without it; an interrupt flag that no wake announces is
    /// still noticed within a capped slice by a thread that has waited long
    /// enough to back off.
    #[test]
    fn m2_4_the_wait_poll_slice_backs_off_to_its_cap_and_still_sees_a_silent_interrupt() {
        let ms = std::time::Duration::from_millis;
        let mut slice = WAIT_POLL_SLICE;
        let mut seen = Vec::new();
        for _ in 0..7 {
            seen.push(slice);
            slice = next_wait_poll_slice(slice, true);
        }
        assert_eq!(seen, [ms(5), ms(10), ms(20), ms(40), ms(80), ms(100), ms(100)]);
        assert_eq!(next_wait_poll_slice(WAIT_POLL_SLICE, false), WAIT_POLL_SLICE);

        let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let (m, flag) = (Arc::clone(&m), Arc::clone(&flag));
            std::thread::spawn(move || {
                let tid = ThreadId(711);
                m.enter(tid);
                let outcome = m.wait(tid, None, Some(&flag), None, None).unwrap();
                m.exit(tid).unwrap();
                outcome
            })
        };
        await_condition("the waiter to enrol", || m.state.lock().wait_queue.len() == 1);
        std::thread::sleep(ms(400));
        // No `wake_all_for_interrupt`: only the poll can see this.
        let set_at = std::time::Instant::now();
        flag.store(true, Ordering::Release);
        assert_eq!(waiter.join().unwrap(), WaitOutcome::Interrupted);
        assert!(set_at.elapsed() < std::time::Duration::from_secs(5));
        assert!(m.state.lock().wait_queue.is_empty());
    }

    /// Round 14 wave 4 (lane monitor2, MON14-1): the interrupt wake aimed at
    /// one thread signals that thread's wait-set entry and no other; a target
    /// on the shared condvar asks the caller for the shared signal; a thread
    /// not in the wait set is signalled nowhere.
    #[test]
    fn mon14_1_the_interrupt_wake_signals_only_its_target_entry() {
        let mut s = MonitorState {
            parked_waiters: 0,
            pending_notifies: 0,
            wait_queue: std::collections::VecDeque::new(),
            notified_tickets: Vec::new(),
            next_wait_ticket: 0,
        };
        for i in 0..8u64 {
            s.enroll_waiter(Some(Arc::new(Condvar::new())), ThreadId(810 + i));
        }
        let shared_ticket = s.enroll_waiter(None, ThreadId(830));
        assert_eq!(s.signal_waiter_for_interrupt(ThreadId(813)), (1, false));
        assert_eq!(s.signal_waiter_for_interrupt(ThreadId(830)), (1, true));
        assert_eq!(s.signal_waiter_for_interrupt(ThreadId(899)), (0, false));
        assert_eq!(s.wait_queue.len(), 9, "an interrupt wake marks nothing");
        assert!(s.notified_tickets.is_empty() && s.pending_notifies == 0);
        s.leave_wait_set(shared_ticket);
        assert_eq!(s.signal_waiter_for_interrupt(ThreadId(830)), (0, false));
    }

    /// Round 14 wave 4 (lane monitor2, MON14-1): through real waiters -- the
    /// targeted wake returns the interrupted waiter `Interrupted` and leaves
    /// the three others in the wait set until a `notifyAll`.
    #[test]
    fn mon14_1_a_targeted_interrupt_wake_returns_only_the_target() {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
        let flags: Vec<_> =
            (0..4).map(|_| Arc::new(std::sync::atomic::AtomicBool::new(false))).collect();
        let waiters: Vec<_> = (0..4u64)
            .map(|i| {
                let (m, flag) = (Arc::clone(&m), Arc::clone(&flags[i as usize]));
                std::thread::spawn(move || {
                    let tid = ThreadId(840 + i);
                    m.enter(tid);
                    let outcome = m.wait(tid, None, Some(&flag), None, None).unwrap();
                    m.exit(tid).unwrap();
                    outcome
                })
            })
            .collect();
        await_condition("four waiters to enrol", || m.state.lock().wait_queue.len() == 4);
        flags[2].store(true, Ordering::Release);
        // 1, or 0 when the target's own safety slice ran out between the
        // flag and the wake and it left the wait set by itself.
        assert!(m.wake_for_interrupt_of(ThreadId(842)) <= 1);
        let mut waiters: Vec<_> = waiters.into_iter().map(Some).collect();
        let target = waiters[2].take().unwrap();
        assert_eq!(target.join().unwrap(), WaitOutcome::Interrupted);
        {
            let s = m.state.lock();
            assert_eq!(s.wait_queue.len(), 3);
            assert!(s.wait_queue.iter().all(|e| e.waiter != ThreadId(842)));
        }
        let notifier = ThreadId(849);
        m.enter(notifier);
        m.notify_all(notifier).unwrap();
        m.exit(notifier).unwrap();
        for w in waiters.into_iter().flatten() {
            assert_eq!(w.join().unwrap(), WaitOutcome::Notified);
        }
        assert!(m.state.lock().wait_queue.is_empty());
        assert_eq!(m.current_owner(), None);
    }

    /// Round 14 wave 4 (lane monitor2, MON14-1): the table's targeted wake
    /// never inflates, like the broadcast one.
    #[test]
    fn mon14_1_the_targeted_wake_never_inflates_an_untouched_object() {
        let table = MonitorTable::new();
        let obj = test_object();
        assert!(!table.wake_waiter_for_interrupt(obj, ThreadId(850)));
        assert!(!is_inflated(obj));
        assert_eq!(monitor_registry_len(&table), 0);
    }

    /// Round 14 wave 5 (lane monitor2, RV5-1): the interrupter looks while the
    /// object is still THIN (the waiter has published itself but not yet
    /// inflated): the targeted wake finds no monitor and wakes nobody, and the
    /// waiter's enrolment re-check -- after its own inflation -- ends the
    /// wait `Interrupted` without a park, timed or not.
    #[test]
    fn rv5_1_an_interrupt_that_found_the_mark_thin_is_seen_at_enrolment() {
        for timeout in [None, Some(60_000u64)] {
            let table = MonitorTable::new();
            let obj = test_object();
            let tid = ThreadId(880);
            let flag = std::sync::atomic::AtomicBool::new(false);
            // The interrupter's half, in `thread_interrupt`'s order.
            flag.store(true, Ordering::SeqCst);
            assert!(!table.wake_waiter_for_interrupt(obj, tid), "thin: nothing to wake");
            assert!(!is_inflated(obj));
            // The waiter's half: enter, then wait (which inflates and enrols).
            table.enter(obj, tid);
            let outcome = table.wait_outcome(obj, tid, timeout, Some(&flag), None).unwrap();
            assert_eq!(outcome, WaitOutcome::Interrupted, "timeout={timeout:?}");
            assert!(table.holds(obj, tid));
            table.exit(obj, tid).unwrap();
        }
    }

    /// Round 14 wave 4 (lane monitor2, MON14-2): a flag already set when the
    /// waiter enrols ends the wait under the state lock it enrolled under --
    /// the waiter never parks, so no observer can see it in the wait set --
    /// and the monitor is re-acquired at the saved depth, timed or not.
    #[test]
    fn mon14_2_an_interrupt_before_enrolment_ends_the_wait_without_a_park() {
        for timeout in [None, Some(60_000u64)] {
            let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
            let seen_enrolled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let observer = {
                let (m, seen) = (Arc::clone(&m), Arc::clone(&seen_enrolled));
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    while !done.load(Ordering::Acquire) {
                        if !m.state.lock().wait_queue.is_empty() {
                            seen.store(true, Ordering::Release);
                        }
                        std::thread::yield_now();
                    }
                })
            };
            let tid = ThreadId(860);
            let flag = std::sync::atomic::AtomicBool::new(true);
            m.enter(tid);
            m.enter(tid);
            let outcome = m.wait(tid, timeout, Some(&flag), None, None).unwrap();
            done.store(true, Ordering::Release);
            observer.join().unwrap();
            assert_eq!(outcome, WaitOutcome::Interrupted, "timeout={timeout:?}");
            let parked = seen_enrolled.load(Ordering::Acquire);
            assert!(!parked, "the waiter parked, timeout={timeout:?}");
            assert!(flag.load(Ordering::Acquire), "wait reads the flag, never clears it");
            assert_eq!(m.current_owner(), Some(tid));
            assert_eq!(m.entry_count.load(Ordering::Relaxed), 2);
            {
                let s = m.state.lock();
                assert!(s.wait_queue.is_empty() && s.notified_tickets.is_empty());
                assert_eq!(s.parked_waiters, 0);
            }
            m.exit(tid).unwrap();
            m.exit(tid).unwrap();
            assert_eq!(m.current_owner(), None);
        }
    }

    /// Round 14 wave 4 (lane monitor2, MON14-2 / MON14-4): the slice plan.
    /// A waiter parks once per safety slice only with the credit, the
    /// enrolment re-check and the back-off all on; the back-off's kill
    /// switch still restores the fixed 5 ms cadence; a requested stack dump
    /// starts at 5 ms; the re-check alone starts the back-off at 20 ms.
    #[test]
    fn mon14_4_a_waiter_parks_once_only_with_every_wake_in_place() {
        let ms = std::time::Duration::from_millis;
        let on = MonitorTuning::DEFAULT;
        assert!(on.wait_parks_once(true));
        assert!(!on.wait_parks_once(false), "no credit: a bare signal is the notification");
        assert_eq!(on.first_wait_slice(true, false), WAIT_SAFETY_SLICE);
        assert_eq!(on.first_wait_slice(true, true), WAIT_POLL_SLICE);
        assert_eq!(next_wait_slice(WAIT_POLL_SLICE, true, true), WAIT_SAFETY_SLICE);
        assert_eq!(next_wait_slice(WAIT_SAFETY_SLICE, true, true), WAIT_SAFETY_SLICE);
        assert_eq!(next_wait_slice(ms(20), true, false), ms(40));

        let no_backoff = MonitorTuning {
            wait_poll_backoff: false,
            ..MonitorTuning::DEFAULT
        };
        assert!(!no_backoff.wait_parks_once(true));
        assert_eq!(no_backoff.first_wait_slice(true, false), WAIT_POLL_SLICE);
        assert_eq!(next_wait_slice(WAIT_POLL_SLICE, false, false), WAIT_POLL_SLICE);

        let no_recheck = MonitorTuning {
            wait_enrol_interrupt_check: false,
            ..MonitorTuning::DEFAULT
        };
        assert!(!no_recheck.wait_parks_once(true));
        assert_eq!(no_recheck.first_wait_slice(true, false), WAIT_POLL_SLICE);

        let polling = MonitorTuning {
            wait_single_park: false,
            ..MonitorTuning::DEFAULT
        };
        assert!(!polling.wait_parks_once(true));
        assert_eq!(polling.first_wait_slice(true, false), WAIT_POLL_SLICE_RECHECKED);
    }

    /// Round 14 wave 4 (lane monitor2, MON14-4): a timed wait that parks
    /// once still ends at its deadline, not a safety slice later, and a
    /// notification still ends an untimed one at once.
    #[test]
    fn mon14_4_a_single_park_wait_keeps_its_deadline_and_its_notification() {
        let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
        let tid = ThreadId(870);
        let never = std::sync::atomic::AtomicBool::new(false);
        m.enter(tid);
        let started = std::time::Instant::now();
        let outcome = m.wait(tid, Some(150), Some(&never), None, None).unwrap();
        let waited = started.elapsed();
        assert_eq!(outcome, WaitOutcome::TimedOutOrSpurious);
        assert!(waited >= std::time::Duration::from_millis(150), "{waited:?}");
        assert!(waited < std::time::Duration::from_secs(10), "{waited:?}");
        assert_eq!(m.current_owner(), Some(tid));
        m.exit(tid).unwrap();

        let waiter = {
            let m = Arc::clone(&m);
            std::thread::spawn(move || {
                let tid = ThreadId(871);
                let never = std::sync::atomic::AtomicBool::new(false);
                m.enter(tid);
                let outcome = m.wait(tid, None, Some(&never), None, None).unwrap();
                m.exit(tid).unwrap();
                outcome
            })
        };
        await_condition("the waiter to enrol", || m.state.lock().wait_queue.len() == 1);
        let notifier = ThreadId(872);
        m.enter(notifier);
        m.notify(notifier).unwrap();
        m.exit(notifier).unwrap();
        assert_eq!(waiter.join().unwrap(), WaitOutcome::Notified);
    }

    /// Round 14 wave 6 (lane monitor3, M2-2): the hand-over watch reports a
    /// held owner word naming another thread, and nothing else -- not a free
    /// read, not a forced release's marker, not the same owner re-taking it.
    #[test]
    fn monitor3_the_handover_watch_sees_only_an_owner_changing_threads() {
        let a = owner_word(Some(ThreadId(1)));
        let b = owner_word(Some(ThreadId(2)));
        let mut w = HandoverWatch::new();
        assert!(!w.owner_polled(a), "the first held poll has nothing to compare with");
        assert!(!w.owner_polled(a));
        assert!(!w.owner_polled(0));
        assert!(!w.owner_polled(OWNER_RELEASING));
        assert!(!w.owner_polled(a), "released and re-taken by the same owner");
        assert!(!w.seen);
        assert!(w.owner_polled(b));
        assert!(w.seen);
        assert!(!w.owner_polled(b));
        assert!(w.owner_polled(a));
        let mut lost = HandoverWatch::new();
        assert!(!lost.seen);
        lost.lost_race();
        assert!(lost.seen);
    }

    /// Round 14 wave 6 (lane monitor3, M2-2): a spin watching the owner word
    /// flip between two OTHER threads (never free, so no spin can win) --
    /// with the abort off runs its budget out and halves it, exactly the
    /// round-12 rule (it never saw the monitor free); with the abort on leaves
    /// at a flip and keeps its budget. The abort arm is sampled many times
    /// because a spin the flipper's thread happens not to overlap runs out
    /// like the control; every sample must fail and take nothing.
    #[test]
    fn monitor3_an_aborting_spin_leaves_at_an_owner_change_and_keeps_its_budget() {
        use std::sync::atomic::AtomicBool;
        let m = Arc::new(Monitor::with_tuning(MonitorTuning::DEFAULT));
        let a = owner_word(Some(ThreadId(880)));
        let b = owner_word(Some(ThreadId(881)));
        let spinner = ThreadId(882);
        m.owner.store(a, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let flips = Arc::new(AtomicU64::new(0));
        let flipper = {
            let (m, stop, flips) = (Arc::clone(&m), Arc::clone(&stop), Arc::clone(&flips));
            std::thread::spawn(move || {
                let mut n: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    m.owner.store(if n % 2 == 0 { b } else { a }, Ordering::SeqCst);
                    n += 1;
                    flips.store(n, Ordering::Relaxed);
                    for _ in 0..4 {
                        std::hint::spin_loop();
                    }
                }
            })
        };
        await_condition("the flipper to run", || flips.load(Ordering::Relaxed) > 1000);

        m.spin_limit.store(SPIN_INITIAL, Ordering::Relaxed);
        assert!(!m.adaptive_spin(spinner, false));
        assert_eq!(
            m.spin_limit.load(Ordering::Relaxed),
            SPIN_INITIAL / 2,
            "abort off: owner changes alone never keep the budget"
        );

        let mut kept = 0;
        for _ in 0..200 {
            m.spin_limit.store(SPIN_INITIAL, Ordering::Relaxed);
            assert!(!m.adaptive_spin(spinner, true), "a never-free monitor is not won");
            if m.spin_limit.load(Ordering::Relaxed) == SPIN_INITIAL {
                kept += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        flipper.join().unwrap();
        assert!(kept > 0, "no aborting spin ever left at an owner change");
        assert_ne!(m.current_owner(), Some(spinner));
        m.owner.store(0, Ordering::SeqCst);
    }

    /// Round 14 wave 6 (lane monitor3): the entrant-wait census buckets a
    /// wait by the bounds its names state, and the census names line up with
    /// the events (the exit line prints them zipped).
    #[test]
    fn monitor3_entrant_waits_fall_in_their_named_buckets() {
        use super::ContentionEvent as E;
        for (us, want) in [
            (0u64, E::EntrantWaitUnder100us),
            (99, E::EntrantWaitUnder100us),
            (100, E::EntrantWaitUnder1ms),
            (999, E::EntrantWaitUnder1ms),
            (1_000, E::EntrantWaitUnder16ms),
            (15_999, E::EntrantWaitUnder16ms),
            (16_000, E::EntrantWaitLonger),
            (u64::MAX, E::EntrantWaitLonger),
        ] {
            assert_eq!(entrant_wait_bucket(us), want, "{us} us");
        }
        for (event, name) in [
            (E::SpinWinAfterHandover, "spin_wins_after_handover"),
            (E::SpinFailAfterHandover, "spin_fails_after_handover"),
            (E::SpinHandoverAbort, "spin_handover_aborts"),
            (E::EntrantWaitMicros, "entrant_wait_us"),
            (E::EntrantWaitUnder100us, "entrant_waits_under_100us"),
            (E::EntrantWaitUnder1ms, "entrant_waits_under_1ms"),
            (E::EntrantWaitUnder16ms, "entrant_waits_under_16ms"),
            (E::EntrantWaitLonger, "entrant_waits_longer"),
        ] {
            assert_eq!(CONTENTION_EVENT_NAMES[event as usize], name);
        }
    }
}
