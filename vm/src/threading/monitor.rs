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
//! # Where a monitor lives (the mark word IS the monitor table)
//!
//! CratonVM spawns one real OS thread per Java thread, so a process-wide lock
//! on a synchronization fast path is a hard scalability ceiling, not a
//! theoretical one. This module therefore follows HotSpot's shape:
//!
//! * **Uncontended / re-entrant** locking never leaves the object's own
//!   `mark_word` (`try_thin_lock` / `try_thin_recursive_lock` /
//!   `try_thin_unlock`) — one CAS, no allocation, no table.
//! * **Inflated** locking is reached through the object's own mark word too:
//!   `MARK_INFLATED` stores the `Monitor`'s address in its upper 62 bits, so
//!   `monitorenter` / `monitorexit` / `wait` / `notify` on an inflated monitor
//!   are a mark-word load followed by that monitor's *own* mutex. **No global
//!   map probe, no global lock.**
//! * [`MonitorTable`] is only a **sharded enumeration index**: it exists so
//!   cold operations (thread-death monitor release, GC re-key / prune,
//!   diagnostics) can walk the set of inflated monitors, which the mark words
//!   alone cannot enumerate. It is never consulted on a hot path.
//!
//! ## Mark-word monitor ownership and lifetime (READ BEFORE EDITING)
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
use std::sync::atomic::{AtomicU64, Ordering};
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
// per-VM flag. The wait loop in `Monitor::wait` polls it every 5ms (the
// same cadence as the interrupt flag) and exits the wait when set; the
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
// so it genuinely frees. Extending an exact dead set to the moving collectors
// is the flagged cross-file follow-up recorded on `remap_after_gc`.

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
//   NEUTRAL / THIN_LOCKED --CAS--> INFLATED(&Monitor)
//                                       (MonitorTable::inflate_locked)
//
// The inflation path is the only one that allocates a `Monitor`. It must
// publish the mark word via `compare_exchange` against the observed
// pre-inflation mark, *not* an unconditional store, to avoid clobbering a
// concurrent CAS (e.g. another thread releasing a thin lock back to NEUTRAL).
// `MonitorTable::publish_inflated` is the single place that does it, and it
// also welds the CAS to the strong-reference transfer that keeps the published
// `Monitor` alive (see the module-level lifetime rules). There is deliberately
// no unchecked variant: an unconditional store here corrupts the lock state
// machine, and a publish without the reference transfer creates a dangling
// mark word.

/// Attempt thin-lock acquisition via a single CAS on the mark word.
///
/// Returns `Ok(())` on success (the calling thread is now the thin-lock owner
/// with recursion = 0). Returns `Err(current_mark)` if the CAS failed -- the
/// caller can inspect the current state to decide whether to retry, recurse,
/// or inflate.
#[inline]
pub fn try_thin_lock(header: &ObjectHeader, thread_id: u32) -> Result<(), u64> {
    // This used to compare against the literal `types::MARK_NEUTRAL`. It cannot
    // any more: since the `kind` / `element_type` / `gc_age` / `gc_flags`
    // quartet moved into bits 48..61, an unlocked object's word is only zero
    // when it is a plain, never-aged, unflagged object. An `int[]` carries kind
    // and element_type bits, so a literal compare would fail forever and EVERY
    // array lock would inflate a monitor.
    //
    // Masking the quartet out restores the intended test -- "unlocked, and no
    // identity hash installed" -- and preserves the property the literal was
    // silently providing: a hashed word has non-zero bits OUTSIDE the quartet,
    // so it still loses here and the caller still inflates, which is HotSpot's
    // rule that a hashed object cannot be thin-locked.
    let cur = header.mark_word.load(Ordering::Relaxed);
    if cur & !types::MARK_QUARTET_MASK != types::MARK_NEUTRAL {
        return Err(cur);
    }
    header
        .mark_word
        .compare_exchange(
            cur,
            ObjectHeader::make_thin_locked(cur, thread_id, 0),
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .map(|_| ())
        .map_err(|m| m)
}

/// Re-entrant thin-lock: bump the recursion counter via CAS.
///
/// On success returns `Ok(new_recursion)`. Returns `Err(current_mark)` if:
/// - the object is not in `THIN_LOCKED` state, or
/// - it is thin-locked by a *different* thread, or
/// - the recursion counter is at u8::MAX (caller must inflate to support
///   deeper nesting).
#[inline]
pub fn try_thin_recursive_lock(header: &ObjectHeader, thread_id: u32) -> Result<u8, u64> {
    loop {
        let cur = header.mark_word.load(Ordering::Relaxed);
        if ObjectHeader::mark_state(cur) != types::MARK_THIN_LOCKED {
            return Err(cur);
        }
        if ObjectHeader::thin_lock_owner(cur) != thread_id {
            return Err(cur);
        }
        let recursion = ObjectHeader::thin_lock_recursion(cur);
        if recursion == u8::MAX {
            return Err(cur); // overflow → must inflate
        }
        let new = ObjectHeader::make_thin_locked(cur, thread_id, recursion + 1);
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
/// calling thread (caller must dispatch to the inflated path or raise
/// `IllegalMonitorStateException`).
#[inline]
pub fn try_thin_unlock(header: &ObjectHeader, thread_id: u32) -> Result<Option<u8>, u64> {
    loop {
        let cur = header.mark_word.load(Ordering::Relaxed);
        if ObjectHeader::mark_state(cur) != types::MARK_THIN_LOCKED {
            return Err(cur);
        }
        if ObjectHeader::thin_lock_owner(cur) != thread_id {
            return Err(cur);
        }
        let recursion = ObjectHeader::thin_lock_recursion(cur);
        let (new, ret) = if recursion == 0 {
            // Last release → return to NEUTRAL, CARRYING THE QUARTET.
            //
            // `MARK_NEUTRAL` is a bare state constant (`0b00`). Storing it raw
            // erases bits 48..61 — the `kind` / `element_type` / `gc_age` /
            // `gc_flags` quartet that moved into this word when the header
            // shrank 24 -> 16 (2026-08-07). Every other transition on this word
            // already rides the quartet across: `make_thin_locked`,
            // `make_inflated` and `make_neutral_hashed` all derive from the
            // previous value. This one did not, so the FIRST `synchronized`
            // block on an object reset its GC flags on exit.
            //
            // `GC_FLAG_COMPACT` living in that quartet is what made it fatal: a
            // compact object silently became "legacy" the moment it was
            // unlocked, and every later field read then decoded a
            // compact-packed body as 16-byte cells — reading past the end of
            // the object. `FileChannelImpl.fileLockTable()` is
            // double-checked locking over a volatile field, so the assignment
            // landed inside the lock and the read after `monitorexit` came back
            // null: `NullPointerException ... because "flt" is null`, and no
            // file-backed database could open. See the retired
            // `compact-ref-field-layout-corrupts-filechannel-filelock-20260807`
            // write-up (cited by name: its tree is not published).
            //
            // The second victim, found from the other end the same day: every
            // Spring Boot test class failed at JUnit discovery, because
            // `AbstractTestDescriptor.children` is a `Collections.synchronizedSet`
            // and `EngineDiscoveryResultValidator` walks
            // `getChildren().iterator()` — the first `synchronized` inside that
            // wrapper de-compacted it and the backing collection read back
            // null. `probes/MonitorQuartetProbe.java` is that repro.
            (ObjectHeader::quartet_of(cur) | types::MARK_NEUTRAL, None)
        } else {
            (
                ObjectHeader::make_thin_locked(cur, thread_id, recursion - 1),
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
// Mark-word → Monitor access (the hot inflated path; takes NO global lock)
// ---------------------------------------------------------------------------

/// Decode the `Monitor` address out of a mark-word snapshot, or `None` if the
/// snapshot is not in `MARK_INFLATED` state (or carries a null pointer, which
/// no legitimate publish can produce — `make_inflated` rejects it).
#[inline(always)]
fn monitor_ptr_from_mark(mark: u64) -> Option<*const Monitor> {
    if ObjectHeader::mark_state(mark) != types::MARK_INFLATED {
        return None;
    }
    let p = ObjectHeader::inflated_monitor(mark) as *const Monitor;
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

/// Borrow the `Monitor` an `INFLATED` mark word points at.
///
/// This is *the* inflated fast path: a mark-word load plus a pointer deref,
/// with no registry probe and therefore no global lock and no atomic refcount
/// traffic. Use this whenever the borrow cannot outlive the caller's own
/// reference to the object (i.e. everything except handing a monitor to a
/// thread that is about to block on it — that needs [`monitor_arc_from_mark`]).
///
/// # Safety argument
///
/// See the module-level "Mark-word monitor ownership and lifetime" section:
/// an `INFLATED` mark word owns one strong `Arc<Monitor>` reference, released
/// only for an object the collector has proved dead — and a dead object has
/// no possible mark-word reader. The returned lifetime is unconstrained, so
/// callers must keep it within the scope in which they hold `obj_ref`.
#[inline(always)]
/// Resolve the identity hash of an object whose mark word is no longer
/// NEUTRAL, by reading the hash displaced into its `Monitor` at inflation.
///
/// Installed into the `gc` crate at VM start-up (`set_displaced_hash_resolver`)
/// because `gc` owns the `identity_hash_code` accessors but cannot name
/// `Monitor`. Reaching the hash is a pointer dereference through the mark word,
/// so this costs the same as any other inflated fast path -- no registry probe.
///
/// Answers `0` for a word that is not INFLATED (a THIN_LOCKED object that has
/// never been hashed can reach here; it has no displaced hash because a hashed
/// object cannot thin-lock in the first place).
pub fn displaced_hash_from_mark(mark: u64) -> i32 {
    monitor_from_mark(mark).map_or(0, |m| m.displaced_hash())
}

fn monitor_from_mark<'a>(mark: u64) -> Option<&'a Monitor> {
    // SAFETY: as argued above, the pointee is kept alive by the strong
    // reference the mark word itself owns.
    monitor_ptr_from_mark(mark).map(|p| unsafe { &*p })
}

/// Clone an owned `Arc<Monitor>` out of an `INFLATED` mark word.
///
/// Needed when the monitor must outlive the caller's borrow of the object —
/// notably the contended path, where the caller parks on the monitor after
/// releasing everything else, and the thread-termination path, which keeps a
/// stable handle across a moving GC.
///
/// # Safety argument
///
/// Same as [`monitor_from_mark`]: the mark word's own strong reference keeps
/// the strong count >= 1 across the increment, so reconstituting an `Arc` from
/// the raw pointer is sound.
#[inline]
fn monitor_arc_from_mark(mark: u64) -> Option<Arc<Monitor>> {
    let p = monitor_ptr_from_mark(mark)?;
    // SAFETY: `p` came from `Arc::as_ptr` on a still-live allocation (the
    // mark word owns a strong reference). `increment_strong_count` +
    // `from_raw` is the documented way to clone through a raw pointer.
    unsafe {
        Arc::increment_strong_count(p);
        Some(Arc::from_raw(p))
    }
}

/// A monitor reached either straight from a mark word (the normal case, free)
/// or from the address-keyed index (the `ThreadId > u32::MAX` legacy monitors,
/// which have no mark-word home).
///
/// Exists so the shared code below can be written once without forcing an
/// `Arc` clone on the path that does not need one.
enum MonitorHandle<'a> {
    Borrowed(&'a Monitor),
    Owned(Arc<Monitor>),
}

impl std::ops::Deref for MonitorHandle<'_> {
    type Target = Monitor;
    #[inline]
    fn deref(&self) -> &Monitor {
        match self {
            MonitorHandle::Borrowed(m) => m,
            MonitorHandle::Owned(m) => m,
        }
    }
}

/// Look up an `&ObjectHeader` from an `ObjectRef`. Mirrors the pattern used by
/// the GC (`cratonvm_gc::heap::Heap::get_header`) — the first
/// `HEADER_SIZE` bytes of every heap allocation are a `repr(C)` `ObjectHeader`.
#[inline]
fn header_of(obj_ref: ObjectRef) -> &'static ObjectHeader {
    // SAFETY: `ObjectRef` is constructed only from live, properly aligned heap
    // allocations whose first bytes are an `ObjectHeader`. The lifetime is
    // bounded by the GC, which scans monitor state at safepoints.
    unsafe { &*(obj_ref.as_ptr() as *const ObjectHeader) }
}

/// Truncate a `ThreadId(u64)` to the 32-bit field stored in the thin-lock
/// owner slot. Thread ids that exceed `u32::MAX` cannot be represented in the
/// thin lock and force inflation; in practice the JVM never reaches that many
/// live threads.
#[inline]
fn tid_to_u32(tid: ThreadId) -> Option<u32> {
    if tid.0 <= u32::MAX as u64 {
        Some(tid.0 as u32)
    } else {
        None
    }
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
/// `#[repr(align(8))]` is load-bearing, not cosmetic: the mark word packs the
/// monitor's address into its upper 62 bits and tags the low 2 with the lock
/// state, so a `Monitor` whose `Arc` payload were less than 4-byte aligned
/// would corrupt the state field. `ObjectHeader::make_inflated` asserts this,
/// and the alignment attribute makes the guarantee explicit rather than
/// incidental to whatever `parking_lot` happens to contain.
#[repr(align(8))]
pub struct Monitor {
    state: Mutex<MonitorState>,
    /// Wakes threads blocked on `monitorenter` (waiting to acquire the lock).
    entry_condvar: Condvar,
    /// Wakes threads blocked on `Object.wait()`.
    wait_condvar: Condvar,
    /// True once this monitor's address has been published into some object's
    /// mark word, which from that moment **owns one strong `Arc` reference**
    /// (see the module-level lifetime rules).
    ///
    /// Read by the two reclaim sites to know (a) how many strong references
    /// are structural rather than "somebody is using this monitor", and (b)
    /// whether they still owe a `drop` of the mark-word reference. Cleared
    /// with a `swap`, so a monitor that is reached by both `prune_dead` and
    /// `remap_after_gc` releases exactly once.
    mark_ref: std::sync::atomic::AtomicBool,
    /// This object's identity hash, displaced here when the object inflated.
    ///
    /// The hash normally lives in the upper bits of a `MARK_NEUTRAL` mark word
    /// (`ObjectHeader::make_neutral_hashed`). Inflation overwrites the whole
    /// word with `INFLATED | monitor_ptr`, so it is the one transition that
    /// would destroy a hash -- `publish_inflated` moves it here first.
    ///
    /// A monitor is the right home for it rather than a separate address-keyed
    /// table: a displaced hash exists only for an inflated object, every
    /// inflated object has exactly one monitor, and this table is ALREADY
    /// re-keyed on relocation and pruned on death by `MonitorCleanup` -- which
    /// carries the "a new object at a recycled address inherits the dead one's
    /// entry" analysis that a fresh side table would have to repeat. Reaching
    /// it is a pointer dereference through the mark word, not a hash probe.
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
    /// The thread that currently owns this monitor, or `None` if unlocked.
    owner: Option<ThreadId>,
    /// Re-entry count. Incremented on each `monitorenter`, decremented on
    /// `monitorexit`. The monitor is released when this reaches 0.
    entry_count: u32,
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
    /// [`Monitor::take_jfr_enter_recorded`] to decide whether to emit the
    /// matching exit event. Only the outermost reentrant enter records (so a
    /// nested re-acquire of the same monitor by the same thread doesn't
    /// produce a spurious paired event); when `entry_count` returns to 0 the
    /// flag is cleared so the next acquisition starts fresh.
    jfr_enter_recorded: bool,
}

impl Monitor {
    /// Create a new, unlocked monitor.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(MonitorState {
                parked_waiters: 0,
                pending_notifies: 0,
                owner: None,
                entry_count: 0,
                jfr_enter_recorded: false,
            }),
            entry_condvar: Condvar::new(),
            wait_condvar: Condvar::new(),
            mark_ref: std::sync::atomic::AtomicBool::new(false),
            displaced_hash: std::sync::atomic::AtomicI32::new(0),
            notify_calls: std::sync::atomic::AtomicU64::new(0),
            interrupt_wakes: std::sync::atomic::AtomicU64::new(0),
        }
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
    #[allow(dead_code)]
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
        let mut state = self.state.lock();
        debug_assert!(state.owner.is_none(), "Monitor must be fresh");
        state.owner = Some(thread_id);
        state.entry_count = entry_count;
    }

    /// Returns true if this monitor is currently owned by the given
    /// thread. Non-blocking inspection — used by `Thread.holdsLock`.
    fn is_held_by(&self, thread_id: ThreadId) -> bool {
        let state = self.state.lock();
        state.owner == Some(thread_id)
    }

    /// Non-blocking inspection of the current owner, if any. Used at the
    /// contention point (`vm_exec::monitor_enter_blocking`) to detect a
    /// monitor whose owner has since died — see
    /// `MonitorTable::release_monitors_held_by` for why a one-time sweep at
    /// thread-death time isn't sufficient by itself: a monitor that was
    /// still a thin lock (never contended) when its owning thread died gets
    /// inflated *later*, by whichever thread next contends it, and that
    /// inflation pre-seeds the new `Monitor`'s owner from the stale mark
    /// word — the death-time sweep can't have found an object that wasn't
    /// inflated yet.
    pub(crate) fn current_owner(&self) -> Option<ThreadId> {
        self.state.lock().owner
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
    /// It is **not** used by [`MonitorTable::prune_dead`], the only reclaim
    /// site today: an exact swept-address set already proves the object is
    /// unreachable, which is strictly stronger. It is retained (and pinned by
    /// tests) because the flagged follow-up — plumbing an exact dead set
    /// through the moving collectors — needs exactly this predicate, and
    /// because idleness alone must never be mistaken for a licence to free.
    #[allow(dead_code)]
    #[inline]
    fn is_idle(&self) -> bool {
        let state = self.state.lock();
        state.owner.is_none() && state.entry_count == 0
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
        let mut state = self.state.lock();
        // Wait until the monitor is either unowned or owned by us.
        if mon_enter_dump_enabled() {
            // Gated diagnostic only (CRATONVM_DBG_MONENTER): poll on a 5ms
            // cadence so a watchdog stack-dump request can surface a thread
            // deadlocked here. Emits the blocked thread's frames once (the
            // snapshot was deposited by `monitor_enter` before this call).
            let mut dumped = false;
            let mut spins: u64 = 0;
            while state.owner.is_some() && state.owner != Some(thread_id) {
                self.entry_condvar
                    .wait_for(&mut state, std::time::Duration::from_millis(5));
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
                        state.owner.map(|o| o.0),
                        state.entry_count,
                        spins * 5,
                        dbg_label.unwrap_or("?")
                    );
                }
            }
        } else {
            // Normal path — unchanged: block on the condvar until released.
            while state.owner.is_some() && state.owner != Some(thread_id) {
                self.entry_condvar.wait(&mut state);
            }
        }
        match state.owner {
            None => {
                // Unowned — acquire
                state.owner = Some(thread_id);
                state.entry_count = 1;
            }
            Some(_) => {
                // Reentrant — already owned by this thread
                state.entry_count += 1;
            }
        }
    }

    /// Non-blocking acquire: `true` ⇒ acquired (fresh or re-entrant),
    /// `false` ⇒ owned by another thread (the caller must take the
    /// GC-blocked contended path — see `MonitorTable::enter_or_contend`).
    fn try_enter(&self, thread_id: ThreadId) -> bool {
        let mut state = self.state.lock();
        match state.owner {
            None => {
                state.owner = Some(thread_id);
                state.entry_count = 1;
                true
            }
            Some(owner) if owner == thread_id => {
                state.entry_count += 1;
                true
            }
            Some(_) => false,
        }
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

    /// Release this monitor for the given thread.
    ///
    /// Decrements the entry count. When it reaches 0, the monitor is released
    /// and becomes unowned.
    ///
    /// Returns `Err` if the calling thread does not own the monitor
    /// (`IllegalMonitorStateException`).
    pub(crate) fn exit(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let mut state = self.state.lock();
        match state.owner {
            Some(owner) if owner == thread_id => {
                // B6: JVMS §6.5 monitorexit contract — if the entry count is
                // already zero at the moment of the exit attempt, the calling
                // thread does NOT logically own the monitor and must observe
                // `IllegalMonitorStateException`. Check BEFORE the decrement
                // so the failure mode is the spec-defined IMSE rather than a
                // silent saturating wrap. The saturating-sub below becomes
                // defense-in-depth against the same race (frame-unwind release
                // racing a manual `monitorexit`) but no longer hides the bug.
                if state.entry_count == 0 {
                    return Err(MonitorError::NotOwner);
                }
                state.entry_count = state.entry_count.saturating_sub(1);
                if state.entry_count == 0 {
                    state.owner = None;
                    // Round-7 HIGH (vm #5): clear the JFR enter-event flag at
                    // the same instant the monitor becomes unowned. The next
                    // acquirer (potentially a different thread) starts with
                    // a fresh `jfr_enter_recorded = false` and the
                    // interpreter's enter-time snapshot governs whether
                    // the next enter/exit pair is JFR-tracked.
                    state.jfr_enter_recorded = false;
                    // Wake one thread waiting to enter this monitor
                    self.entry_condvar.notify_one();
                }
                Ok(())
            }
            _ => Err(MonitorError::NotOwner),
        }
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
    pub(crate) fn force_release_if_owned_by(&self, thread_id: ThreadId) -> bool {
        let mut state = self.state.lock();
        if state.owner == Some(thread_id) {
            state.owner = None;
            state.entry_count = 0;
            state.jfr_enter_recorded = false;
            self.entry_condvar.notify_all();
            true
        } else {
            false
        }
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
        let mut state = self.state.lock();
        if state.owner == Some(thread_id) {
            state.jfr_enter_recorded = true;
        }
    }

    /// Round-7 HIGH (vm #5): peek the JFR enter-recorded flag for the current
    /// owner. Returns `true` only if the interpreter actually emitted a
    /// matching `monitor_enter` event for the live acquisition — the natural
    /// gate for emitting a paired `monitor_exit` event without producing an
    /// orphan one when JFR turned on between the two opcodes.
    pub(crate) fn jfr_enter_recorded(&self) -> bool {
        let state = self.state.lock();
        state.jfr_enter_recorded
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
    ) -> Result<bool, MonitorError> {
        let mut state = self.state.lock();
        if state.owner != Some(thread_id) {
            return Err(MonitorError::NotOwner);
        }

        // Save and release
        let saved_count = state.entry_count;
        state.owner = None;
        state.entry_count = 0;
        self.entry_condvar.notify_one();

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
        let poll_interval = std::time::Duration::from_millis(5);
        let mut was_interrupted = false;
        let mut frames_dumped = false;
        let mut orphan_reported = false;
        match timeout_ms {
            Some(ms) if ms > 0 => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let wait_time = remaining.min(poll_interval);
                    let result = self.wait_condvar.wait_for(&mut state, wait_time);
                    // The CONDITION, re-tested under the mutex on every wakeup
                    // — the thing a condvar-only wait was missing.
                    if state.pending_notifies > 0 {
                        state.pending_notifies -= 1;
                        NOTIFY_CREDITS_CONSUMED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    if let Some(flag) = interrupted {
                        if flag.load(std::sync::atomic::Ordering::Acquire) {
                            was_interrupted = true;
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
                            if !result.timed_out() {
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
                    // slept the whole duration after being signalled.
                    if !result.timed_out() {
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
                    loop {
                        let result = self.wait_condvar.wait_for(&mut state, poll_interval);
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
                        if state.pending_notifies > 0 {
                            state.pending_notifies -= 1;
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
                            if !result.timed_out() {
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
                                state.owner,
                                state.entry_count,
                                state.parked_waiters,
                                state.pending_notifies,
                                state.owner == Some(thread_id),
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
                                    let still_ours = monitor_ptr_from_mark(cur)
                                        .is_some_and(|p| std::ptr::eq(p, self as *const Monitor));
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
                        // to allow the caller to re-check its condition.
                        if !result.timed_out() {
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
                        if state.pending_notifies > 0 {
                            state.pending_notifies -= 1;
                            NOTIFY_CREDITS_CONSUMED
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                        self.wait_condvar.wait(&mut state);
                    }
                }
            }
        }

        // LEAVE the waiter set. After this point a `notifyAll()` must not
        // count this thread, and a `notify()` must not leave a credit for it.
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
        let mut reacquire_reported = false;
        while state.owner.is_some() && state.owner != Some(thread_id) {
            let _ = self.entry_condvar.wait_for(&mut state, poll_interval);
            if !reacquire_reported && stack_dump_wait_flag().load(Ordering::Acquire) {
                reacquire_reported = true;
                let (notifies_now, interrupt_now) = self.notify_totals();
                eprintln!(
                    "[WAIT-REACQUIRE] thread {thread_id:?} was NOTIFIED and is now stuck \
                     RE-ACQUIRING the monitor, not waiting on it — owner={:?} entry_count={} \
                     saved_count={saved_count} notifies_since_wait={} interrupt_wakes_since_wait={}",
                    state.owner,
                    state.entry_count,
                    notifies_now.wrapping_sub(notifies_at_entry),
                    interrupt_now.wrapping_sub(interrupt_wakes_at_entry),
                );
            }
        }
        state.owner = Some(thread_id);
        state.entry_count = saved_count;

        Ok(was_interrupted)
    }

    /// Object.notify() — wake one thread waiting on this monitor.
    ///
    /// The calling thread must own this monitor.
    fn notify(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let state = self.state.lock();
        if state.owner != Some(thread_id) {
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
        if monitor_pending_notify() && state.pending_notifies < state.parked_waiters {
            state.pending_notifies += 1;
            NOTIFY_CREDITS_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.wait_condvar.notify_one();
        Ok(())
    }

    /// Object.notifyAll() — wake all threads waiting on this monitor.
    ///
    /// The calling thread must own this monitor.
    pub(crate) fn notify_all(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let state = self.state.lock();
        if state.owner != Some(thread_id) {
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
            NOTIFY_CREDITS_CREATED.fetch_add(
                u64::from(state.parked_waiters.saturating_sub(state.pending_notifies)),
                std::sync::atomic::Ordering::Relaxed,
            );
            state.pending_notifies = state.parked_waiters;
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
        let _state = self.state.lock();
        self.interrupt_wakes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.wait_condvar.notify_all();
    }
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
    monitors: Box<[OrderedPlMutex<FxHashMap<usize, Arc<Monitor>>>]>,
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
}

impl MonitorTable {
    /// Create an empty monitor table.
    pub fn new() -> Self {
        // Registered here rather than at a VM init site because this is the
        // earliest point that provably precedes any inflation: an object cannot
        // inflate without a monitor table, so no displaced hash can exist
        // before this runs. Idempotent -- the `OnceLock` keeps the first.
        cratonvm_gc::collector::set_displaced_hash_resolver(displaced_hash_from_mark);
        Self {
            // Every shard of both registries lives at L6 (`monitors`).
            monitors: (0..MONITOR_SHARDS)
                .map(|_| OrderedPlMutex::new(FxHashMap::default(), LockLevel::Monitors))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            cas_lock_cache_table_id: NEXT_CAS_LOCK_CACHE_TABLE_ID.fetch_add(1, Ordering::Relaxed),
            cas_lock_epoch: AtomicU64::new(1),
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
        self.monitor_shard(key).lock().insert(key, monitor.clone());
    }

    /// Look up the inflated `Monitor` for `obj_ref` **in the index**.
    ///
    /// Prefer the mark word ([`monitor_from_mark`] / [`monitor_arc_from_mark`])
    /// wherever the object is in hand: this takes an L6 shard lock and exists
    /// only for the address-keyed paths that have no mark word to consult
    /// (the `> u32::MAX` thread-id legacy path and diagnostics).
    fn lookup_indexed(&self, obj_ref: ObjectRef) -> Option<Arc<Monitor>> {
        let key = obj_ref.as_ptr() as usize;
        self.monitor_shard(key).lock().get(&key).cloned()
    }

    /// Publish `monitor` into `header`'s mark word, transferring one strong
    /// reference to the mark word on success.
    ///
    /// Returns `true` if the CAS from `expected` to `INFLATED(monitor)` won.
    /// On failure nothing is published and no reference is leaked, so the
    /// caller may simply drop its freshly built monitor and retry.
    ///
    /// This is the ONLY place that establishes the module's central invariant
    /// — "an `INFLATED` mark word owns a strong reference to the monitor it
    /// names" — so the increment and the CAS must stay welded together here.
    fn publish_inflated(header: &ObjectHeader, expected: u64, monitor: &Arc<Monitor>) -> bool {
        // Take the reference the mark word will own *before* publishing, so
        // the count is already correct the instant another thread can observe
        // the pointer.
        let mark_owned = Arc::clone(monitor);
        // Carry any identity hash out of the word being overwritten, BEFORE the
        // CAS publishes the monitor. Any thread that can observe the INFLATED
        // pointer can already reach the hash through it; doing this after the
        // CAS would leave a window where the object's hash is simply gone, and
        // a reader in that window would mint a second, different one.
        let displaced = ObjectHeader::neutral_hash(expected);
        if displaced != 0 {
            monitor.displace_hash(displaced);
        }
        // `expected` is the word being replaced, so the quartet rides across
        // inflation the same way the displaced hash does.
        let new_mark = ObjectHeader::make_inflated(expected, Arc::as_ptr(monitor) as usize);
        if header
            .mark_word
            .compare_exchange(expected, new_mark, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            monitor.mark_ref.store(true, Ordering::Release);
            // Transfer ownership of `mark_owned` to the mark word. Balanced by
            // `Monitor::release_mark_ref` at the reclaim site (`prune_dead`).
            std::mem::forget(mark_owned);
            true
        } else {
            drop(mark_owned);
            false
        }
    }

    /// Force inflation of the lock for `obj_ref` and return the heavyweight
    /// `Monitor`.
    ///
    /// If the object is already `INFLATED`, the monitor is read straight out
    /// of the mark word — no allocation, no publish, and the index is repaired
    /// if it happens to be missing the entry. Otherwise a new `Monitor` is
    /// allocated, pre-acquired with the *currently observed* thin-lock owner
    /// (if any), published into the mark word by CAS, and indexed.
    ///
    /// Unlike the previous implementation this cannot fail: the mark word is
    /// the source of truth, so there is no "inflated but unfindable" state to
    /// report. The `Result` is retained because every caller already threads
    /// it and several of them (`wait`/`notify`/`notifyAll`) are on paths that
    /// legitimately return `MethodCallFailed` for other reasons.
    ///
    /// Note the CAS-retry loop takes **no lock at all** in the common case;
    /// the L6 shard lock is touched once, after the publish wins, purely to
    /// record the new monitor for later enumeration.
    fn inflate_locked(
        &self,
        obj_ref: ObjectRef,
        header: &ObjectHeader,
    ) -> Result<Arc<Monitor>, MethodCallFailed> {
        let key = obj_ref.as_ptr() as usize;
        loop {
            let cur = header.mark_word.load(Ordering::Acquire);
            match ObjectHeader::mark_state(cur) {
                s if s == types::MARK_INFLATED => {
                    // Already inflated: the mark word names the one true
                    // monitor for this object. Repairing a missing index entry
                    // here is what makes the index non-authoritative — and is
                    // why a second `Monitor` can no longer be synthesised for
                    // an object that already has one (the old audit finding
                    // 1(b) shape, which orphaned every waiter on the first).
                    let Some(m) = monitor_arc_from_mark(cur) else {
                        // `make_inflated` rejects null, so an INFLATED mark
                        // with a null pointer is memory corruption, not a
                        // race. Surface it rather than dereferencing it.
                        return Err(MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::IllegalStateException {
                                message: format!(
                                    "monitor mark word is INFLATED with a null monitor \
                                     pointer (memory corruption) at obj={key:#x}"
                                ),
                            },
                        )));
                    };
                    self.index_repair_if_absent(key, &m);
                    return Ok(m);
                }
                s if s == types::MARK_THIN_LOCKED => {
                    let owner = ObjectHeader::thin_lock_owner(cur);
                    let recursion = ObjectHeader::thin_lock_recursion(cur);
                    let monitor = Arc::new(Monitor::new());
                    monitor.enter_with_recursion(ThreadId(owner as u64), (recursion as u32) + 1);
                    // Publish atomically — if the CAS loses, the original
                    // owner mutated the word (either recursive bump or
                    // release). Drop the local monitor and retry.
                    if Self::publish_inflated(header, cur, &monitor) {
                        self.index_insert(key, &monitor);
                        return Ok(monitor);
                    }
                    // CAS lost; loop to re-snapshot. The pre-acquired Monitor
                    // is dropped (no other reference exists yet).
                }
                _ => {
                    // NEUTRAL (or reserved). Create an unowned monitor and
                    // publish it; the caller will `enter` it normally.
                    let monitor = Arc::new(Monitor::new());
                    if Self::publish_inflated(header, cur, &monitor) {
                        self.index_insert(key, &monitor);
                        return Ok(monitor);
                    }
                    // CAS lost — retry.
                }
            }
        }
    }

    /// Re-add `monitor` to the enumeration index if the index lost track of it
    /// (e.g. it was pruned as dead while the object was in fact a live in-place
    /// survivor of a partial collection, or a shard was drained mid-remap).
    ///
    /// Idempotent and cheap; keeps `release_monitors_held_by` able to see every
    /// live inflated monitor even though the mark word, not the index, is what
    /// locking consults.
    fn index_repair_if_absent(&self, key: usize, monitor: &Arc<Monitor>) {
        let mut shard = self.monitor_shard(key).lock();
        // Resolve the read to a bool before mutating — holding the `get`
        // borrow across the `insert` would not borrow-check.
        let already_indexed = shard.get(&key).is_some_and(|e| Arc::ptr_eq(e, monitor));
        if !already_indexed {
            shard.insert(key, Arc::clone(monitor));
        }
    }

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
    /// * THIN_LOCKED(other) → inflate transferring ownership, then try-enter.
    /// * THIN_LOCKED(self) at recursion = 255 → inflate, then try-enter.
    /// * INFLATED         → dispatch to `Monitor::try_enter` **via the mark
    ///   word**: a pointer load and that monitor's own mutex. No table lock.
    ///
    /// Every arm above is per-object. The only path that touches a shared L6
    /// lock is inflation itself (one shard insert, once per object, ever).
    ///
    /// `None` ⇒ acquired. `Some(m)` ⇒ contended; caller must
    /// `m.block_enter(thread_id)` (the monitor may have been released in
    /// the interim — `block_enter` then acquires immediately).
    pub(crate) fn enter_or_contend(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Option<Arc<Monitor>> {
        // Fall back to the legacy heavyweight path if the ThreadId doesn't fit
        // in the 32-bit thin-lock owner field.
        let tid32 = match tid_to_u32(thread_id) {
            Some(t) => t,
            None => {
                let m = self.inflate_for_legacy(obj_ref);
                if m.try_enter(thread_id) {
                    return None;
                }
                return Some(m);
            }
        };

        let header = header_of(obj_ref);

        // ── Fast path 1: NEUTRAL → THIN_LOCKED via single CAS. ─────────────
        match try_thin_lock(header, tid32) {
            Ok(()) => return None,
            Err(_) => { /* fall through with up-to-date classification below */ }
        }

        loop {
            let cur = header.mark_word.load(Ordering::Acquire);
            match ObjectHeader::mark_state(cur) {
                s if s == types::MARK_NEUTRAL => {
                    // Raced with another exit() — retry the fast path once.
                    if try_thin_lock(header, tid32).is_ok() {
                        return None;
                    }
                    // Lost the race again → inflate to avoid livelock.
                    // `inflate_locked` now only Errs on an INFLATED mark word
                    // carrying a null monitor pointer, which `make_inflated`
                    // cannot produce — i.e. memory corruption. Panic surfaces
                    // it instead of dereferencing null.
                    let m = self
                        .inflate_locked(obj_ref, header)
                        .expect("monitor inflation invariant: corrupt INFLATED mark word");
                    if m.try_enter(thread_id) {
                        return None;
                    }
                    return Some(m);
                }
                s if s == types::MARK_THIN_LOCKED => {
                    let owner = ObjectHeader::thin_lock_owner(cur);
                    if owner == tid32 {
                        // ── Fast path 2: re-entrant thin lock. ──────────
                        match try_thin_recursive_lock(header, tid32) {
                            Ok(_) => return None,
                            Err(err_mark) => {
                                if ObjectHeader::mark_state(err_mark) == types::MARK_THIN_LOCKED
                                    && ObjectHeader::thin_lock_owner(err_mark) == tid32
                                    && ObjectHeader::thin_lock_recursion(err_mark) == u8::MAX
                                {
                                    // Recursion overflow — inflate. Inflation
                                    // pre-acquires with entry_count =
                                    // recursion+1 = 256, capturing our prior
                                    // re-entrant acquisitions. Now bump once
                                    // more (re-entrant try_enter always
                                    // succeeds) to record the current
                                    // attempted acquisition.
                                    let m = self.inflate_locked(obj_ref, header).expect(
                                        "monitor inflation invariant: corrupt INFLATED mark word",
                                    );
                                    if m.try_enter(thread_id) {
                                        return None;
                                    }
                                    return Some(m);
                                }
                                // Otherwise the state changed under us; reclassify.
                                continue;
                            }
                        }
                    } else {
                        // ── Slow path: contended thin lock → inflate. ───
                        // `inflate_locked` re-snapshots under its mutex and
                        // pre-acquires for whatever owner the mark word
                        // currently shows (or none if it has since gone
                        // NEUTRAL). try_enter succeeds if it has since been
                        // released; otherwise the caller blocks GC-marked.
                        let m = self
                            .inflate_locked(obj_ref, header)
                            .expect("monitor inflation invariant: corrupt INFLATED mark word");
                        if m.try_enter(thread_id) {
                            return None;
                        }
                        return Some(m);
                    }
                }
                s if s == types::MARK_INFLATED => {
                    // ── Inflated: dispatch through the object's own mark
                    // word. THIS IS THE PATH THAT USED TO TAKE A PROCESS-WIDE
                    // LOCK. It is now a pointer load plus this monitor's own
                    // mutex, so two threads locking two different inflated
                    // objects never touch a shared cache line here.
                    //
                    // The uncontended case borrows (`monitor_from_mark`) and
                    // never touches the refcount at all; only the contended
                    // case pays an `Arc` clone, because the caller is about to
                    // park on the monitor and must own a reference across the
                    // park (see the module lifetime rules, point 4).
                    let Some(m) = monitor_from_mark(cur) else {
                        // Null pointer under an INFLATED tag is corruption,
                        // not a race — `make_inflated` rejects null. Fall into
                        // `inflate_locked`, which reports it as an error.
                        let m = self
                            .inflate_locked(obj_ref, header)
                            .expect("monitor inflation invariant: corrupt INFLATED mark word");
                        if m.try_enter(thread_id) {
                            return None;
                        }
                        return Some(m);
                    };
                    if m.try_enter(thread_id) {
                        return None;
                    }
                    return monitor_arc_from_mark(cur);
                }
                _ => {
                    // Reserved state 0b11 — should never occur. Fall back to
                    // inflation as the safest recovery.
                    let m = self
                        .inflate_locked(obj_ref, header)
                        .expect("monitor inflation invariant: corrupt INFLATED mark word");
                    if m.try_enter(thread_id) {
                        return None;
                    }
                    return Some(m);
                }
            }
        }
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

    /// Get or create a monitor without touching the mark word — used only
    /// for the legacy fallback when a `ThreadId` exceeds `u32::MAX` and so
    /// cannot be represented in the thin lock's 32-bit owner field.
    ///
    /// These monitors are the one kind that is *not* reachable from a mark
    /// word, so the index is genuinely authoritative for them and their
    /// `Monitor::mark_ref` stays `false` (see [`Monitor::structural_refs`]).
    /// The address-keyed shard lookup is the only correct home for them.
    fn inflate_for_legacy(&self, obj_ref: ObjectRef) -> Arc<Monitor> {
        let key = obj_ref.as_ptr() as usize;
        self.monitor_shard(key)
            .lock()
            .entry(key)
            .or_insert_with(|| Arc::new(Monitor::new()))
            .clone()
    }

    /// Release the monitor for the given object on behalf of the given thread.
    ///
    /// Fast paths (no allocation):
    /// * THIN_LOCKED(self) at recursion>0 → CAS recursion-1.
    /// * THIN_LOCKED(self) at recursion=0 → CAS back to NEUTRAL.
    ///
    /// Slow path:
    /// * INFLATED → dispatch to `Monitor::exit` **via the mark word**; no
    ///   table lock, no allocation, no refcount traffic.
    ///
    /// Returns `Err(MethodCallFailed)` with `IllegalMonitorStateException` if
    /// the calling thread does not own the monitor.
    pub fn exit(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> Result<(), MethodCallFailed> {
        let key = obj_ref.as_ptr() as usize;
        let header = header_of(obj_ref);

        if let Some(tid32) = tid_to_u32(thread_id) {
            // ── Fast path: thin-lock release. ──────────────────────────────
            match try_thin_unlock(header, tid32) {
                Ok(_) => return Ok(()),
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

        // ── Slow path: inflated monitor dispatch. ──────────────────────────
        //
        // Read the monitor out of the object's own mark word. Falls back to
        // the index only for the legacy (`ThreadId > u32::MAX`) monitors,
        // which have no mark-word representation at all.
        let mark = header.mark_word.load(Ordering::Acquire);
        let monitor: Option<MonitorHandle<'_>> = match monitor_from_mark(mark) {
            Some(m) => Some(MonitorHandle::Borrowed(m)),
            None => self.lookup_indexed(obj_ref).map(MonitorHandle::Owned),
        };
        match monitor {
            Some(m) => m.exit(thread_id).map_err(|MonitorError::NotOwner| {
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
    fn dbg_monexit_forensics(&self, obj_ref: ObjectRef, tid: ThreadId, mark: u64, arm: &str) {
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
                std::ptr::read(p.add(cratonvm_types::NUM_SLOTS_OFFSET) as *const u32),
            )
        };
        let (reg_hit, reg_owner, reg_count) = {
            let shard = self.monitor_shard(key).lock();
            match shard.get(&key) {
                Some(m) => {
                    let st = m.state.lock();
                    (true, st.owner, st.entry_count)
                }
                None => (false, None, 0),
            }
        };
        // The mark word is authoritative now, so print what IT says alongside
        // the index's opinion — a disagreement is the interesting signal.
        let (mark_owner, mark_count) = match monitor_from_mark(mark) {
            Some(m) => {
                let st = m.state.lock();
                (Some(st.owner), st.entry_count)
            }
            None => (None, 0),
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
        match monitor_from_mark(mark) {
            Some(m) => m.jfr_enter_recorded(),
            None => self
                .lookup_indexed(obj_ref)
                .is_some_and(|m| m.jfr_enter_recorded()),
        }
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
        let cur = header.mark_word.load(Ordering::Acquire);
        if let Some(m) = monitor_arc_from_mark(cur) {
            return Ok(m);
        }
        // Not inflated (or the legacy no-mark-word case). `inflate_locked`
        // re-snapshots the mark word so its pre-acquire reflects the current
        // thin-lock owner.
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
        let monitor = self.ensure_inflated(obj_ref, thread_id)?;
        monitor
            .wait(thread_id, timeout_ms, interrupted, Some(obj_ref))
            .map_err(|MonitorError::NotOwner| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!("current thread is not owner"),
                    },
                ))
            })
    }

    /// Perform `Object.notify()` on the monitor for the given object.
    ///
    /// Wakes one thread waiting on this monitor. The calling thread must own it.
    pub fn notify(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> Result<(), MethodCallFailed> {
        let monitor = self.ensure_inflated(obj_ref, thread_id)?;
        monitor.notify(thread_id).map_err(|MonitorError::NotOwner| {
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
        let monitor = self.ensure_inflated(obj_ref, thread_id)?;
        monitor
            .notify_all(thread_id)
            .map_err(|MonitorError::NotOwner| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "thread {thread_id} called notifyAll() without owning the monitor"
                        ),
                    },
                ))
            })
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
    /// and why it cannot swallow a pending `notify`.
    pub fn wake_waiters_for_interrupt(&self, obj_ref: ObjectRef) -> bool {
        let header = header_of(obj_ref);
        let mark = header.mark_word.load(Ordering::Acquire);
        let monitor = match monitor_arc_from_mark(mark) {
            Some(m) => m,
            // Legacy / mark-word-less objects keep their monitor only in the
            // side index; `wait()` reaches them through `inflate_for_legacy`.
            None => match self.lookup_indexed(obj_ref) {
                Some(m) => m,
                None => return false,
            },
        };
        monitor.wake_all_for_interrupt();
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
            s if s == types::MARK_THIN_LOCKED => {
                if let Some(tid32) = tid_to_u32(thread_id) {
                    ObjectHeader::thin_lock_owner(mark) == tid32
                } else {
                    false
                }
            }
            s if s == types::MARK_INFLATED => {
                // Straight from the mark word — no table lock.
                monitor_from_mark(mark).is_some_and(|m| m.is_held_by(thread_id))
            }
            _ => false,
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
            s if s == types::MARK_THIN_LOCKED => {
                Some(ThreadId(u64::from(ObjectHeader::thin_lock_owner(mark))))
            }
            s if s == types::MARK_INFLATED => monitor_from_mark(mark).and_then(|m| {
                let st = m.state.lock();
                st.owner
            }),
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
            s if s == types::MARK_THIN_LOCKED => {
                u32::from(ObjectHeader::thin_lock_recursion(mark)) + 1
            }
            s if s == types::MARK_INFLATED => {
                monitor_from_mark(mark).map_or(0, |m| m.state.lock().entry_count)
            }
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
    /// this point) — `state.owner` goes back to `None` — so the subsequent
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
        // Cold: runs once per thread death. Walk one shard at a time — two L6
        // guards must never be live simultaneously (equal-level nesting is a
        // lock-order violation).
        for shard in self.monitors.iter() {
            let guard = shard.lock();
            for monitor in guard.values() {
                if let Some(exc) = except {
                    if Arc::ptr_eq(monitor, exc) {
                        continue;
                    }
                }
                monitor.force_release_if_owned_by(thread_id);
            }
        }
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
    ///   CROSS-FILE FOLLOW-UP (flagged, not done here — out of edit scope): to
    ///   reclaim dead monitors under the *moving* collectors as well, they need
    ///   to supply an exact dead-address set (or call `prune_dead` alongside
    ///   `remap_after_gc`). That means editing `gc/src/collector.rs` and the
    ///   `remap_after_gc` call sites in `gc/src/heap.rs`, `gc/src/g1.rs` and
    ///   `gc/src/gen_heap.rs` — left to the owner of those files. Until then a
    ///   dead object's monitor is retained by the moving collectors, which is a
    ///   bounded leak and strictly preferable to a dangling mark word.
    pub fn remap_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        // Mutators are stopped for this whole operation. Invalidate cached raw
        // CAS-lock pointers before an idle entry can be dropped or a moved one
        // re-keyed below.
        self.cas_lock_epoch.fetch_add(1, Ordering::Release);
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
        // observe the intermediate state. Even if one did, the mark word — not
        // this index — is what locking consults, so the worst case is a
        // transiently missing index entry, which `index_repair_if_absent`
        // restores.
        {
            let mut survivors: Vec<(usize, Arc<Monitor>)> = Vec::new();
            for shard in self.monitors.iter() {
                let entries: Vec<(usize, Arc<Monitor>)> = shard.lock().drain().collect();
                for (old_key, monitor) in entries {
                    match pointer_map.get(&old_key).copied() {
                        Some(new_key) => {
                            // Object survived AND moved — re-key so enumeration
                            // stays addressable. (Locking already followed the
                            // object automatically: the mark word was copied
                            // with the header.)
                            survivors.push((new_key, monitor));
                        }
                        None => {
                            // Absent from the forwarding map. That is NOT proof
                            // of death under a partial collector — a live
                            // in-place survivor is legitimately absent — and its
                            // mark word still names this monitor. Retain it
                            // under its (unchanged) address. See the safety note
                            // on this method for why reclaiming here would now
                            // be a use-after-free rather than merely wasteful.
                            survivors.push((old_key, monitor));
                        }
                    }
                }
            }
            for (key, monitor) in survivors {
                self.monitor_shard(key).lock().insert(key, monitor);
            }
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
        // `dead` is processed under the collector's stop-the-world token, so
        // no mutator can retain a cache hit while its registry owner is
        // removed. The next mutator observation must use a fresh lookup.
        //
        // Bumped unconditionally, BEFORE the survey below decides whether any
        // removal is possible. One relaxed atomic add is not worth reasoning
        // about whether a mutator can hold a cached handle for an address the
        // sweep has just recycled.
        self.cas_lock_epoch.fetch_add(1, Ordering::Release);
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
        let mut any_monitor = false;
        let mut any_cas = false;
        for i in 0..MONITOR_SHARDS {
            monitors_nonempty[i] = !self.monitors[i].lock().is_empty();
            cas_nonempty[i] = !self.cas_locks[i].lock().is_empty();
            any_monitor |= monitors_nonempty[i];
            any_cas |= cas_nonempty[i];
        }
        if !any_monitor && !any_cas {
            return;
        }
        if any_monitor {
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
        if any_cas {
            for d in dead {
                if !cas_nonempty[shard_of(*d)] {
                    continue;
                }
                self.cas_locks[shard_of(*d)].lock().remove(d);
            }
        }
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
            "the fixture must actually inflate, or the assertion below passes             vacuously against an index that was empty all along"
        );

        // A realistic `dead` slab: the one real address buried in a crowd of
        // addresses that hash all over the 64 shards.
        let mut dead: Vec<usize> = (1..=4096).map(|i| i * 4096).collect();
        dead.push(obj.as_ptr() as usize);
        table.prune_dead(&dead);

        assert_eq!(
            table.indexed_monitor_count(),
            before - 1,
            "the shard survey must not let a real entry through: an address             whose shard is NON-empty has to be looked up"
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
        let obj = test_object();
        let header = header_of(obj);

        let hash = header
            .mark_word_identity_hash(|| 0x0051_1EEF)
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
            ObjectHeader::neutral_hash(mark),
            0,
            "the word no longer carries the hash -- that is what makes the              displacement necessary, not optional"
        );
        let monitor = monitor_arc_from_mark(mark).expect("inflated => monitor");
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
        let monitor = monitor_arc_from_mark(header.mark_word.load(Ordering::Acquire))
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
        let obj = heap.alloc_object(cratonvm_types::ClassId::new(0), 1);
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
        let obj = heap.alloc_object(cratonvm_types::ClassId::new(0), 1);
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
        let first = heap.alloc_object(cratonvm_types::ClassId::new(0), 1);
        let second = heap.alloc_object(cratonvm_types::ClassId::new(0), 1);
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
        let monitor = monitor_arc_from_mark(mark).expect("inflated => monitor");
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

        for expected_rec in 0..5u8 {
            table.enter(obj, tid);
            let mark = header_of(obj).mark_word.load(Ordering::Acquire);
            assert_eq!(ObjectHeader::mark_state(mark), types::MARK_THIN_LOCKED);
            assert_eq!(ObjectHeader::thin_lock_owner(mark), tid.0 as u32);
            assert_eq!(
                ObjectHeader::thin_lock_recursion(mark),
                expected_rec,
                "recursion field must reflect the depth"
            );
        }
        assert_eq!(monitor_registry_len(&table), 0, "no inflation allowed");

        for expected_rec_after in (0..5u8).rev() {
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
    /// the stale mark word. `current_owner`/`force_release_if_owned_by` are
    /// what `vm_exec::monitor_enter_blocking` uses to detect and clear that
    /// dead-owner seed at the contention point, right after inflation,
    /// before blocking — this test exercises that exact mechanism directly.
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

    /// THE headline property: once inflated, `enter` / `exit` / `holds` /
    /// `current_owner` / `entry_count` reach the monitor through the object's
    /// own mark word and never probe the shared index.
    ///
    /// Proven directly rather than by inspection: the test holds the L6 shard
    /// guard that owns this object's key for the whole window, and a second
    /// thread must still complete a full lock/unlock cycle. If any of those
    /// operations still went through the table, the worker would block and the
    /// `recv_timeout` would expire.
    ///
    /// Exactly one shard guard is held, so this does not violate the
    /// equal-level nesting rule the L6 hierarchy enforces.
    #[test]
    fn inflated_lock_unlock_never_touches_the_shared_index() {
        let table = Arc::new(MonitorTable::new());
        let obj = leaked_heap().alloc_object(ClassId::new(0), 0);
        force_inflated(&table, obj, ThreadId(1));

        let key = obj.as_ptr() as usize;
        let shard_guard = table.monitor_shard(key).lock();

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let worker_table = table.clone();
        let h = std::thread::spawn(move || {
            let tid = ThreadId(2);
            worker_table.enter(obj, tid);
            assert!(worker_table.holds(obj, tid));
            assert_eq!(worker_table.current_owner(obj), Some(tid));
            assert_eq!(worker_table.entry_count(obj), 1);
            // Re-entrant acquire on an inflated monitor.
            worker_table.enter(obj, tid);
            assert_eq!(worker_table.entry_count(obj), 2);
            worker_table.exit(obj, tid).unwrap();
            worker_table.exit(obj, tid).unwrap();
            assert!(!worker_table.holds(obj, tid));
            let _ = tx.send(());
        });

        let finished = rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok();
        // Release before asserting so the worker can finish (and `join` cannot
        // hang) even if the property regressed.
        drop(shard_guard);
        h.join().unwrap();
        assert!(
            finished,
            "inflated monitorenter/monitorexit blocked on the monitor index — \
             the inflated fast path must go through the mark word only"
        );
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

        // 256 acquisitions fit in the thin lock (recursion 0..=255).
        for _ in 0..256 {
            table.enter(obj, tid);
        }
        assert!(is_thin_locked(obj), "256 nested acquires must stay thin");
        assert_eq!(table.entry_count(obj), 256);

        // The 257th overflows the recursion field and forces inflation.
        table.enter(obj, tid);
        assert!(is_inflated(obj), "recursion overflow must inflate");
        assert_eq!(
            table.entry_count(obj),
            257,
            "inflation must carry the accumulated recursion count over"
        );
        assert_eq!(table.current_owner(obj), Some(tid));

        for _ in 0..257 {
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
    #[test]
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

        // Pretend three threads are parked, without actually parking any.
        {
            let mut s = m.state.lock();
            s.owner = Some(owner);
            s.entry_count = 1;
            s.parked_waiters = 3;
        }

        // `notify()` leaves ONE credit, however many times a notifier could
        // race — and never more than there are waiters.
        m.notify(owner).unwrap();
        assert_eq!(m.state.lock().pending_notifies, 1);
        m.notify(owner).unwrap();
        m.notify(owner).unwrap();
        assert_eq!(m.state.lock().pending_notifies, 3);
        m.notify(owner).unwrap();
        assert_eq!(
            m.state.lock().pending_notifies,
            3,
            "a notify with no waiter left to take it must not stockpile a credit \
             that a FUTURE wait would consume as a spurious wakeup"
        );

        // `notifyAll()` is exactly the waiters parked right now.
        {
            let mut s = m.state.lock();
            s.pending_notifies = 0;
            s.parked_waiters = 2;
        }
        m.notify_all(owner).unwrap();
        assert_eq!(m.state.lock().pending_notifies, 2);

        // A waiter that arrives AFTER the notifyAll must not be able to take
        // one of those credits and count itself notified.
        {
            let mut s = m.state.lock();
            s.parked_waiters += 1;
        }
        assert_eq!(
            m.state.lock().pending_notifies,
            2,
            "notifyAll() promises the set parked when it ran, not a standing offer"
        );

        // Consuming is a DECREMENT: two waiters take one each and the third
        // finds nothing, which is what stops `notify()` from waking everyone.
        for expected in [1u32, 0] {
            let mut s = m.state.lock();
            assert!(s.pending_notifies > 0);
            s.pending_notifies -= 1;
            assert_eq!(s.pending_notifies, expected);
        }
        assert_eq!(m.state.lock().pending_notifies, 0);
    }

    /// `notify` / `notifyAll` on a monitor this thread does not own must still
    /// be an `IllegalMonitorStateException` and must leave NO credit behind —
    /// the credit is only reachable past the ownership check, and a refused
    /// notification that still armed a waiter would be a spurious wakeup with
    /// no notifier.
    #[test]
    fn a_refused_notify_leaves_no_credit() {
        let m = Monitor::new();
        {
            let mut s = m.state.lock();
            s.owner = Some(ThreadId(1));
            s.entry_count = 1;
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
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        let (m, contended) = table.enter_inflated_or_contend(obj, tid).expect("inflate");
        assert!(!contended);
        assert!(
            m.mark_ref.load(Ordering::Acquire),
            "publishing INFLATED must record that the mark word owns a ref"
        );
        // index entry + mark word + the handle we are holding.
        assert_eq!(Arc::strong_count(&m), 3);
        assert_eq!(m.structural_refs(), 2);

        // SAFETY: test-local object; nothing else can read its mark word.
        unsafe { Monitor::release_mark_ref(&m) };
        assert!(!m.mark_ref.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&m), 2);
        assert_eq!(m.structural_refs(), 1);

        // Second release: no-op, NOT a second decrement.
        unsafe { Monitor::release_mark_ref(&m) };
        assert_eq!(
            Arc::strong_count(&m),
            2,
            "release_mark_ref must be idempotent — a double decrement here is \
             a double free once the remaining holders drop"
        );

        // The monitor is still perfectly usable through the surviving handle.
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
        let m = monitor_arc_from_mark(mark).expect("INFLATED mark must name a monitor");
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
        let m = monitor_arc_from_mark(header_of(obj).mark_word.load(Ordering::Acquire)).unwrap();
        assert!(m.is_idle());
        assert_eq!(Arc::strong_count(&m), m.structural_refs() + 1);
    }

    /// A monitor whose index entry is lost (pruned as dead while its object was
    /// actually a live in-place survivor of a partial collection) must remain
    /// fully usable through the mark word, and must be re-indexed rather than
    /// replaced by a second `Monitor`.
    ///
    /// This is the regression guard for the old "INFLATED mark but registry
    /// entry missing" failure: it used to re-inflate a SECOND monitor, orphaning
    /// every waiter parked on the first.
    #[test]
    fn a_lost_index_entry_is_repaired_not_re_inflated() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);
        force_inflated(&table, obj, ThreadId(1));

        let mark_before = header_of(obj).mark_word.load(Ordering::Acquire);
        let original = monitor_arc_from_mark(mark_before).unwrap();
        assert_eq!(monitor_registry_len(&table), 1);

        // Simulate the lost entry.
        let key = obj.as_ptr() as usize;
        table.monitor_shard(key).lock().remove(&key);
        assert_eq!(monitor_registry_len(&table), 0);

        // Locking still works — the mark word is authoritative.
        table.enter(obj, tid);
        assert!(table.holds(obj, tid));
        assert_eq!(
            header_of(obj).mark_word.load(Ordering::Acquire),
            mark_before,
            "a missing index entry must NOT cause a second inflation"
        );
        table.exit(obj, tid).unwrap();

        // `inflate_locked` — the path a racing inflation attempt takes when the
        // mark word turned INFLATED under it — repairs the index and returns
        // the SAME monitor, never a replacement.
        let m = table
            .inflate_locked(obj, header_of(obj))
            .expect("already-inflated object must resolve from its mark word");
        assert!(
            Arc::ptr_eq(&m, &original),
            "index repair must return the object's existing monitor, \
             not a freshly synthesised one"
        );
        assert_eq!(monitor_registry_len(&table), 1, "index must be repaired");

        // And enumeration (thread-death monitor release) sees it again.
        table.enter(obj, tid);
        table.release_monitors_held_by(tid);
        assert_eq!(table.current_owner(obj), None);
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
}
