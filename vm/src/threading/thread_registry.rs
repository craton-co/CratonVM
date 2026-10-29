// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Registry for tracking all JVM threads.
//!
//! The `ThreadRegistry` keeps track of spawned threads, their Java Thread
//! objects, join handles, and liveness status. It also hands out unique
//! `ThreadId` values.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::threading::jvm_thread::{GcBlockState, ParkState, ThreadId};
use crate::types::ObjectRef;

/// What [`ThreadRegistry::request_handshake`] did with a handshake op (gcg
/// g3/k, step 1 of `common-a-proposal-per-thread-handshakes`).
pub enum HandshakeRequest {
    /// No such live thread; the op is handed back unrun.
    Gone(crate::threading::jvm_thread::HandshakeOp),
    /// The target is in a blocked region (its frames are quiescent and
    /// deposited); the op is handed back for the requester's blocked path.
    TargetBlocked(crate::threading::jvm_thread::HandshakeOp),
    /// Queued on `state`'s queue; `ticket` settles when the target runs it
    /// (or it is abandoned / withdrawn through `state.handshakes`).
    Queued {
        state: Arc<GcBlockState>,
        ticket: Arc<crate::threading::jvm_thread::HandshakeTicket>,
    },
}

/// How [`ThreadRegistry::handshake_and_wait`] ended (gci i23/hs, step 2 of
/// `common-a-proposal-per-thread-handshakes`). Exactly one of: the op ran
/// once on the target ([`Self::Ran`]), or it never ran and never will (every
/// other variant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// The target ran the op at its poll.
    Ran,
    /// The target was in a blocked region, or entered one before it took the
    /// op: its frames are quiescent and its deposit describes them.
    TargetBlocked,
    /// No such live thread, or it was dropped before it took the op.
    Gone,
    /// Not taken within the budget, or the requester's `abort` asked to stop
    /// waiting (a pause began): the op was withdrawn. The caller falls back to
    /// what it did before handshakes (the frame-trace pause). gci i24/rv: also
    /// an op the LIVE target abandoned because it (or an op before it)
    /// unwound -- it did not run to its end.
    NotServed,
}

/// gci i23/hs: how long [`ThreadRegistry::handshake_and_wait`] waits for an
/// op the target already took (and so is running) when the wait would
/// otherwise stop. An op is a short publish that takes no pause, so this is
/// a backstop against a hang, never the expected wait; past it the outcome
/// is [`HandshakeOutcome::NotServed`] and the op's later effect is harmless.
pub const HANDSHAKE_TAKEN_OP_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

// ---------------------------------------------------------------------------
// CRATONVM_DBG_THREADREG_PERF — cumulative cost instrumentation for the
// full-registry-walk functions called on every GC / STW pause
// (`collect_all_root_snapshots`, `alive_count_and_os_tids`,
// `alive_count_blocked_and_os_tids`). Investigating a CratonVM-specific
// per-alive-thread VM overhead gap (Cluster B,
// http-client-connector-teardown-hang-crash-FIXED.md):
// real HotSpot finishes a test class that briefly accumulates ~750 mostly-
// idle threads in 6s; CratonVM takes 25-300+s for the identical thread
// count. This measures which of these O(N) walkers actually dominates
// wall-clock, rather than guessing from a stack dump. Zero cost when unset
// (one load of a latched `OnceLock` per call to check the gate; gcj j1/cm:
// the registry size is now asked only when the gate is on -- it was a
// `threads.read()` per call, inside the STW census's `inner` hold).
mod threadreg_perf {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::Instant;

    fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_DBG_THREADREG_PERF")
                .map(|v| {
                    let t = v.trim();
                    !t.is_empty() && t != "0" && !t.eq_ignore_ascii_case("false")
                })
                .unwrap_or(false)
        })
    }

    pub struct Counters {
        calls: AtomicU64,
        nanos: AtomicU64,
        entries: AtomicU64,
        name: &'static str,
        started_report: AtomicBool,
    }

    impl Counters {
        pub const fn new(name: &'static str) -> Self {
            Self {
                calls: AtomicU64::new(0),
                nanos: AtomicU64::new(0),
                entries: AtomicU64::new(0),
                name,
                started_report: AtomicBool::new(false),
            }
        }

        /// Time a closure, accumulating (call count, nanos, entries scanned).
        /// `entries` is the registry size AT THIS CALL (not a running total) —
        /// used to report an average N alongside cumulative cost, and asked
        /// (`entries()`) only when the gate is on. Prints a
        /// running summary every 50 calls so a long test run shows progress
        /// without waiting for process exit (which this diagnostic-only VM
        /// mostly doesn't reach cleanly on a hang anyway).
        #[inline]
        pub fn time<R>(&self, entries: impl FnOnce() -> usize, f: impl FnOnce() -> R) -> R {
            if !enabled() {
                return f();
            }
            let entries = entries();
            let start = Instant::now();
            let r = f();
            let elapsed = start.elapsed().as_nanos() as u64;
            let calls = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            let nanos = self.nanos.fetch_add(elapsed, Ordering::Relaxed) + elapsed;
            let total_entries =
                self.entries.fetch_add(entries as u64, Ordering::Relaxed) + entries as u64;
            if calls == 1 || calls % 50 == 0 {
                self.started_report.store(true, Ordering::Relaxed);
                eprintln!(
                    "[threadreg-perf] {} calls={} total_ms={:.1} avg_us={:.1} avg_registry_size={:.0} last_registry_size={}",
                    self.name,
                    calls,
                    nanos as f64 / 1_000_000.0,
                    (nanos as f64 / 1000.0) / calls as f64,
                    total_entries as f64 / calls as f64,
                    entries,
                );
            }
            r
        }
    }
}

/// The calling OS thread's id, in the same encoding `ThreadEntry::os_tid`
/// stores (`GetCurrentThreadId` on Windows, `gettid` on Linux) — see
/// `ThreadRegistry::set_os_tid_current`, whose publication this reads back.
/// `0` means "this platform has no takeover/identity backend", which every
/// caller must read as "unknown": `0` is also the never-published sentinel in
/// the entry.
#[cfg(windows)]
#[inline]
pub(crate) fn current_os_tid() -> u32 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    unsafe { GetCurrentThreadId() }
}

/// Linux variant of [`current_os_tid`].
#[cfg(target_os = "linux")]
#[inline]
pub(crate) fn current_os_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

/// Fallback for platforms with no `os_tid` publication (the takeover machinery
/// is Windows/Linux-only, and so is `set_os_tid_current`).
#[cfg(all(not(windows), not(target_os = "linux")))]
#[inline]
pub(crate) fn current_os_tid() -> u32 {
    0
}

/// gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`): publish
/// the CALLING OS thread's JIT deopt stash cells into `state` under `os_tid`
/// (which must be the calling thread's tid). Read back only by the take-over's
/// frozen-peer walk (`jni::push_frozen_peer_deopt_stash_roots`).
pub(crate) fn publish_deopt_stash_cells_into(state: &GcBlockState, os_tid: u32) {
    if let Some((deopt, exceptional)) = cratonvm_jit::deopt::stash_cell_addrs() {
        state.publish_deopt_stash_cells(os_tid, deopt, exceptional);
    }
}

/// The decision table of `ThreadRegistry::alive_count_blocked_and_os_tids_for`,
/// split out so it is testable without a second OS thread.
///
/// `own_counted` — the initiator's own entry is in the census; `os_counted` — a
/// counted entry is published on the caller's OS thread; `me_known` — the
/// caller's OS tid is known (non-zero); `unknown_os` — some counted entry has
/// not published an `os_tid`.
#[inline]
fn initiator_counted_verdict(
    own_counted: bool,
    os_counted: bool,
    me_known: bool,
    unknown_os: bool,
) -> Option<bool> {
    if own_counted || os_counted {
        Some(true)
    } else if !me_known || unknown_os {
        None
    } else {
        Some(false)
    }
}

/// `CRATONVM_MONITOR_FASTPATH=0` — restore the pre-2026-09-08 uncontended
/// `monitorenter` / `monitorexit` path, so one binary can measure both arms.
///
/// Off, the JMX bookkeeping goes back through the registry map on every
/// acquire and release and always visits the contended slots, and the two
/// opcode handlers go back to pre-building their JEP 358 message context. The
/// two arms are behaviourally identical — this exists because
/// `probes/SyncCost.java` has to be run on a shared host where a sequential
/// pair of builds is not a measurement.
///
/// Since round 11 wave 4 it is also the kill switch for contended-entry
/// spinning (`threading::monitor::contended_spin_enabled`): off, a contended
/// `monitorenter` inflates and parks at once, as it did before. Since wave 5,
/// off also sends every owned-monitor publish to `JmxLockStack`'s mutexed
/// overflow list — the pre-lock-stack cost — instead of its owner-written slots.
pub(crate) fn monitor_fastpath_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_MONITOR_FASTPATH").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// The per-thread JMX monitor slots, in one allocation.
///
/// # Why these three moved out of [`ThreadEntry`]
///
/// `complete_jmx_monitor_enter` runs on EVERY uncontended `monitorenter` and
/// `remove_jmx_locked_monitor` on every `monitorexit`, and both reached their
/// slots through `ThreadRegistry::threads` — an `RwLock` read plus an
/// `FxHashMap` lookup — before touching three `parking_lot` mutexes. `perf` on
/// `probes/SyncCost.java`'s `synchronized`-block loop put the pair at 15.0% of
/// the whole loop, against 3.8% for `MonitorTable::{enter_or_contend,exit}`:
/// the bookkeeping cost four times the locking it describes.
///
/// Behind an `Arc`, the owning thread caches a handle once and never consults
/// the map again — the same idiom, for the same reason, as
/// the (since removed) async-exception slot. Cross-thread readers (the JMX snapshot,
/// the GC root walk and its post-move remap) still go through the map and see
/// the very same allocation.
#[derive(Default)]
pub(crate) struct JmxMonitorBook {
    /// The monitor this thread is currently BLOCKED_ON_MONITOR_ENTER for.
    contended_monitor: Mutex<Option<ObjectRef>>,
    /// When that block began, for `getBlockedTime()`.
    contended_started: Mutex<Option<Instant>>,
    /// Monitors this thread currently owns: `getLockedMonitors()`, and a GC
    /// root set that is remapped across a moving collection. Written only by
    /// the owning thread, without an RMW on the common path — see
    /// `JmxLockStack`.
    locked: JmxLockStack,
    /// `true` from `set_jmx_contended_monitor` until the matching
    /// `complete_jmx_monitor_enter`, i.e. exactly while the two slots above may
    /// hold `Some`.
    ///
    /// It exists so the UNCONTENDED arm — which publishes ownership through the
    /// same `complete_jmx_monitor_enter` and therefore ran the clear-and-drain
    /// unconditionally — can skip both mutexes on a relaxed load. It is a hint
    /// in one direction only: a stale `true` costs one extra pass over two
    /// slots that are already `None` (which is what `reset_jmx_contention_stats`
    /// can leave behind), and a `false` is only ever written by the thread that
    /// wrote the `true`, after it has cleared both slots.
    contending: AtomicBool,
    /// Which interpreted frame locked each entry of `locked`, published by the
    /// owner with its frame trace ([`ThreadRegistry::publish_jmx_frame_monitors`])
    /// for cross-thread `ThreadInfo.getLockedMonitors()` (interpreter round i1
    /// wave 24, lane L7). Positions, not addresses: a moving collection
    /// rewrites `locked`'s slots in place and would leave addresses stale.
    frame_monitors: Mutex<FrameMonitorAttribution>,
    /// `true` while `frame_monitors` holds a publication, so an owner holding
    /// no monitor skips its mutex on every deposit.
    frame_monitors_published: AtomicBool,
    /// `ThreadMXBean` `getBlockedCount()`: monitor entries that had to block.
    /// Counted when the block BEGINS (`set_jmx_contended_monitor`).
    ///
    /// Here rather than on the `ThreadEntry` since round 11 wave 17 (lane lock,
    /// proposal W17-2): both contended-path writers then reach it through the
    /// thread's cached book, and a parking thread no longer takes the
    /// registry's `threads` read lock -- one RMW on a line every parking thread
    /// in the VM shares -- twice per park.
    blocked_count: AtomicU64,
    /// `getBlockedTime()`, in nanoseconds: added when a contended entry
    /// completes (`complete_jmx_monitor_enter`).
    blocked_nanos: AtomicU64,
    /// JIT round 19 wave 12, lane gc12 (`CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`):
    /// the lock stack as it was when a frame-trace PAUSE made this thread
    /// publish its trace ([`ThreadRegistry::record_trace_locks`]), so a reader
    /// of a RUNNING thread pairs the trace with the monitors held at that same
    /// instant, as HotSpot's one safepoint does. `None` until the first.
    trace_locks: Mutex<Option<TraceLockRecord>>,
    /// JIT round 25 wave 6, lane W6-F fix 4: bumped by every write of this
    /// thread's frame trace that this book is told of -- a pause's live
    /// publish ([`ThreadRegistry::record_trace_locks`], which stores the new
    /// value in its record), a blocking deposit
    /// ([`ThreadRegistry::note_trace_deposit`]), a pause-built excused trace
    /// ([`ThreadRegistry::store_frame_trace`]). A record whose `seq` is the
    /// current value describes the trace published now
    /// ([`ThreadRegistry::live_trace_locks`]).
    trace_seq: AtomicU64,
}

/// JIT round 19 wave 12, lane gc12: one [`JmxMonitorBook::trace_locks`]
/// record. `keys` are object addresses, valid only while no pause has ended
/// since `gen` (the `gc_generation` read inside the publishing pause, which
/// that pause's end bumps by one): the reader accepts the record only at
/// `gen + 1`, so no collection can have moved them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TraceLockRecord {
    pub(crate) gen: u64,
    /// `moving_generation` at the record: a moving collection since makes
    /// the addresses stale whatever `gen` says.
    pub(crate) moving_gen: u64,
    pub(crate) trace_len: usize,
    pub(crate) keys: Vec<usize>,
    /// W6-F fix 4: [`JmxMonitorBook::trace_seq`] after this record's publish.
    pub(crate) seq: u64,
}

impl TraceLockRecord {
    /// May a reader that now sees `gc_generation == gen_now` and
    /// `moving_generation == moving_now` use the record's addresses? No
    /// moving collection since, and at most the one pause end (the
    /// publishing pause's own) since.
    pub(crate) fn current_at(&self, gen_now: u64, moving_now: u64) -> bool {
        moving_now == self.moving_gen && (gen_now == self.gen || gen_now == self.gen + 1)
    }
}

/// JIT round 19 proposal wave 7, lane gc7 (SYNC619-2): the published trace
/// attribution `interp` (`(trace index, lock-stack slot)` per interpreter
/// frame monitor, innermost first) with the compiled activations' holds
/// `compiled` (`(trace index, address)`, innermost first;
/// `held_monitors::published_compiled_holds`) merged in, in report order:
/// trace index descending, each index keeping its own entries' order.
///
/// A compiled hold is kept only as the slot of the lock-stack object `keys`
/// holds at exactly that address, and only at an index inside the trace
/// (`trace_len`); everything else is dropped (it is left to the reader's
/// "held, attributed to no frame"). So no address outlives the publish: the
/// book stores slots, which a moving collection remaps in place.
fn merge_compiled_hold_entries(
    mut interp: Vec<(u32, u32)>,
    compiled: &[(u32, usize)],
    keys: &[ObjectRef],
    trace_len: usize,
) -> Vec<(u32, u32)> {
    if compiled.is_empty() {
        return interp;
    }
    let mut extra: Vec<(u32, u32)> = compiled
        .iter()
        .filter(|&&(at, _)| (at as usize) < trace_len)
        .filter_map(|&(at, word)| {
            let slot = keys.iter().position(|k| k.as_ptr() as usize == word)?;
            Some((at, u32::try_from(slot).ok()?))
        })
        .collect();
    if extra.is_empty() {
        return interp;
    }
    interp.append(&mut extra);
    // Stable: entries at one index keep their order (an interpreter frame and
    // a compiled activation never share an index).
    interp.sort_by(|a, b| b.0.cmp(&a.0));
    interp
}

/// One publication of [`JmxMonitorBook::frame_monitors`]: for the frame trace
/// published at the same deposit (`frame_count` entries) and the lock stack as
/// it was then (`lock_len` entries), `(frame index from the bottom, index into
/// the lock-stack snapshot)` per locked monitor, in report order.
///
/// `trace_count` / `trace_entries`: the same attribution indexed into the
/// published TRACE, when that trace interleaves compiled frames with the
/// interpreter ones (`stackwalker::capture_published_trace`, interpreter
/// round i1 wave 38, lane L7): `trace_count` is the trace's length, `0` when
/// the trace has one entry per interpreter frame and `entries` serves both
/// readers.
#[derive(Default)]
struct FrameMonitorAttribution {
    frame_count: usize,
    lock_len: usize,
    entries: Vec<(u32, u32)>,
    trace_count: usize,
    trace_entries: Vec<(u32, u32)>,
}

/// Owner-written slots in a [`JmxLockStack`] before publishes spill to its
/// mutexed overflow list. Eight distinct monitors held at once covers the
/// nesting of practically every `synchronized` program; a deeper thread pays
/// the pre-lock-stack mutex for its ninth and later monitors only.
const JMX_LOCK_STACK_SLOTS: usize = 8;

/// The monitors one thread owns, as `getLockedMonitors()` reports them and as
/// the GC roots and remaps them — HotSpot's `LM_LIGHTWEIGHT` lock stack, kept
/// for JMX rather than for locking.
///
/// # Why not a `Mutex<Vec<_>>` (what this replaced, round 11 wave 5)
///
/// Every uncontended `monitorenter` publishes here and every outermost
/// `monitorexit` retracts, so the old mutex cost four RMWs per `synchronized`
/// block (lock + unlock, twice) on top of the thin-lock CAS pair it describes.
/// Here the common case is plain stores:
///
/// * **One writer.** Only the owning thread pushes or tombstones, so a write
///   needs no RMW: it is bracketed by a single-writer seqlock (`seq` odd while
///   writing) and the stores themselves are `Relaxed`. Every caller of
///   [`ThreadRegistry::complete_jmx_monitor_enter`] /
///   [`ThreadRegistry::remove_jmx_locked_monitor`] passes its OWN thread id,
///   which is what makes this sound — a cross-thread publish would be a second
///   writer.
/// * **Cross-thread readers** ([`Self::snapshot`], i.e.
///   `jmx_lock_snapshot`) retry while `seq` is odd or moved, so they see a set
///   the thread really held at one instant, as the mutex gave them.
/// * **The collector** ([`Self::extend_roots_stw`], [`Self::remap_stw`]) runs
///   under stop-the-world, when a counted owner is at a safepoint and so not
///   inside a write. It reads ALL slots, never trusting `top`, so even a write
///   interrupted halfway (slot stored, `top` not yet) is scanned. The one owner
///   that can run during a pause is an EXCLUDED one (a JNI `MonitorExit` from a
///   thread in a blocked region); excluded callers only ever retract (JNI's
///   excluded `MonitorEnter` does not publish), a retract is a single
///   tombstone store, and the remap rewrites a slot with a CAS against the
///   value it read — so a racing tombstone is never resurrected.
///
/// Order is the old list's insertion order: slots are pushed at `top`, a
/// retract tombstones (and pops trailing tombstones, so the LIFO release of
/// structured locking leaves no holes), and once anything has spilled
/// (`spilled`) every later publish also goes to the overflow list until it
/// drains, so the overflow always follows every slot entry. At most one copy
/// of a monitor exists (publish dedupes against slots and, when spilled, the
/// overflow), and a retract removes every copy, as `retain` did.
///
/// # Compiled code is a writer too (round 11 wave 6)
///
/// `#[repr(C)]` because the JIT's inline thin-lock (both tiers, through
/// `jit/src/runtime_lowering.rs::emit_inline_thin_lock`,
/// `monitorenter`/`monitorexit`) pushes and pops here directly, at the
/// `cratonvm_jit_api::JIT_LOCK_STACK_*` offsets pinned below. It is still the
/// owner writing (compiled code runs on the owning
/// thread and reaches this stack through its own `JvmThread`), and it follows
/// the same protocol: `seq += 1`, slot store, `top` store, `seq += 1` -- or,
/// since round 13 wave 12 (`CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP`, default
/// on), slot store, `top` store, `seq += 2`: one compiled push or pop is
/// consistent at every prefix (a reader that loads the old `top` never reads
/// the new slot; a tombstone reads as released), and the trailing bump still
/// makes [`Self::snapshot`] retry across any SECOND write, whose stores cannot
/// become visible before the first write's bump on x86-TSO. The argument is
/// on `lock_stack_single_bump_enabled`. It only
/// pushes when the monitor was NEUTRAL (so not already recorded, leaks aside),
/// `top < 8` and `!spilled`; it only pops the `top - 1` slot, after its
/// release CAS, and without popping trailing tombstones below it (a later
/// retract does). Everything else stays on the helper path.
#[derive(Default)]
#[repr(C)]
struct JmxLockStack {
    /// Seqlock sequence: odd while the owner is writing.
    seq: AtomicU64,
    /// One past the highest slot in use; every slot at or above it is 0.
    top: std::sync::atomic::AtomicUsize,
    /// Object addresses, 0 = empty or retracted (a tombstone).
    slots: [std::sync::atomic::AtomicUsize; JMX_LOCK_STACK_SLOTS],
    /// `true` while `overflow` may be non-empty. Written by the owner only,
    /// under the `overflow` mutex; `false` guarantees `overflow` is empty.
    spilled: AtomicBool,
    /// Monitors published while every slot was taken or while `spilled`, and
    /// every publish when `CRATONVM_MONITOR_FASTPATH=0`.
    overflow: Mutex<Vec<ObjectRef>>,
}

// The layout compiled code bakes (`cratonvm_jit_api::JIT_LOCK_STACK_*`). A
// field reorder or a slot-count change must fail the build here, not become a
// wild store from a `synchronized` block.
const _: () = {
    use cratonvm_jit_api as api;
    assert!(std::mem::offset_of!(JmxLockStack, seq) == api::JIT_LOCK_STACK_SEQ_OFFSET);
    assert!(std::mem::offset_of!(JmxLockStack, top) == api::JIT_LOCK_STACK_TOP_OFFSET);
    assert!(std::mem::offset_of!(JmxLockStack, slots) == api::JIT_LOCK_STACK_SLOTS_OFFSET);
    assert!(JMX_LOCK_STACK_SLOTS == api::JIT_LOCK_STACK_SLOTS);
    assert!(std::mem::offset_of!(JmxLockStack, spilled) == api::JIT_LOCK_STACK_SPILLED_OFFSET);
    assert!(std::mem::size_of::<AtomicU64>() == 8);
    assert!(std::mem::size_of::<std::sync::atomic::AtomicUsize>() == 8);
    assert!(std::mem::size_of::<AtomicBool>() == 1);
};

impl JmxLockStack {
    /// Owner only: open a write. Readers that load an odd `seq`, or a `seq`
    /// that moved across their read, retry.
    #[inline]
    fn write_begin(&self) -> u64 {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        // Orders the odd `seq` before the slot stores that follow, for a
        // reader whose `fence(Acquire)` follows its slot loads.
        std::sync::atomic::fence(Ordering::Release);
        s
    }

    /// Owner only: close the write [`Self::write_begin`] opened.
    #[inline]
    fn write_end(&self, s: u64) {
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Owner only: record `monitor` as held, unless it already is.
    fn publish(&self, monitor: ObjectRef) {
        let key = monitor.as_ptr() as usize;
        let top = self.top.load(Ordering::Relaxed).min(JMX_LOCK_STACK_SLOTS);
        if self
            .slots
            .iter()
            .take(top)
            .any(|slot| slot.load(Ordering::Relaxed) == key)
        {
            return;
        }
        if !self.spilled.load(Ordering::Relaxed) && monitor_fastpath_enabled() {
            // `None` = every slot taken: spill below.
            if let Some(slot) = self.slots.get(top) {
                let s = self.write_begin();
                slot.store(key, Ordering::Relaxed);
                self.top.store(top + 1, Ordering::Relaxed);
                self.write_end(s);
                return;
            }
        }
        let mut overflow = self.overflow.lock();
        if !overflow.iter().any(|o| o.as_ptr() as usize == key) {
            overflow.push(monitor);
        }
        self.spilled.store(true, Ordering::Relaxed);
    }

    /// Owner only: forget `monitor`, wherever it is recorded.
    fn retract(&self, monitor: ObjectRef) {
        let key = monitor.as_ptr() as usize;
        let top = self.top.load(Ordering::Relaxed).min(JMX_LOCK_STACK_SLOTS);
        if self
            .slots
            .iter()
            .take(top)
            .any(|slot| slot.load(Ordering::Relaxed) == key)
        {
            let s = self.write_begin();
            for slot in self.slots.iter().take(top) {
                if slot.load(Ordering::Relaxed) == key {
                    slot.store(0, Ordering::Relaxed);
                }
            }
            let mut new_top = top;
            while let Some(below) = new_top.checked_sub(1) {
                if self
                    .slots
                    .get(below)
                    .is_some_and(|slot| slot.load(Ordering::Relaxed) != 0)
                {
                    break;
                }
                new_top = below;
            }
            self.top.store(new_top, Ordering::Relaxed);
            self.write_end(s);
        }
        if self.spilled.load(Ordering::Relaxed) {
            let mut overflow = self.overflow.lock();
            overflow.retain(|o| o.as_ptr() as usize != key);
            if overflow.is_empty() {
                self.spilled.store(false, Ordering::Relaxed);
            }
        }
    }

    /// Any thread: the held monitors, in publish order, as of one instant.
    ///
    /// Holding the overflow mutex freezes the overflow half (the owner only
    /// changes it under that mutex); the seqlock validates the slot half.
    fn snapshot(&self) -> Vec<ObjectRef> {
        let overflow = self.overflow.lock();
        let mut keys = [0usize; JMX_LOCK_STACK_SLOTS];
        let mut attempts = 0u32;
        let n = loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 0 {
                let mut n = 0usize;
                // `Acquire` (round 13 wave 12, lane monitor3): compiled code's
                // push stores the slot, then `top`, then bumps `seq` once
                // with no odd bracket (`cratonvm_jit::runtime_lowering::
                // lock_stack_single_bump_enabled`), so this load must order
                // the slot loads after it -- a new `top` then brings its slot.
                // A plain `MOV` on x86, like the `Relaxed` load it replaces.
                let top = self.top.load(Ordering::Acquire).min(JMX_LOCK_STACK_SLOTS);
                for slot in self.slots.iter().take(top) {
                    let v = slot.load(Ordering::Relaxed);
                    if v != 0 {
                        if let Some(k) = keys.get_mut(n) {
                            *k = v;
                            n += 1;
                        }
                    }
                }
                std::sync::atomic::fence(Ordering::Acquire);
                if self.seq.load(Ordering::Relaxed) == s1 {
                    break n;
                }
            }
            // The owner is inside a write of a few stores; it only stays there
            // if it was descheduled, so stop burning the CPU it needs.
            attempts = attempts.saturating_add(1);
            if attempts < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        };
        let mut out = Vec::with_capacity(n + overflow.len());
        // SAFETY: every non-zero slot holds an `ObjectRef::as_ptr()` address
        // `publish` stored (or `remap_stw` forwarded).
        out.extend(
            keys.iter()
                .take(n)
                .map(|&k| unsafe { ObjectRef::from_raw(k as *mut u8) }),
        );
        out.extend(overflow.iter().copied());
        out
    }

    /// Collector, under stop-the-world: every held monitor, as a root.
    fn extend_roots_stw(&self, out: &mut Vec<ObjectRef>) {
        for slot in &self.slots {
            let v = slot.load(Ordering::Acquire);
            if v != 0 {
                // SAFETY: as in `snapshot`.
                out.push(unsafe { ObjectRef::from_raw(v as *mut u8) });
            }
        }
        out.extend(self.overflow.lock().iter().copied());
    }

    /// Collector, under stop-the-world: forward every entry a moving
    /// collection relocated.
    fn remap_stw(&self, pointer_map: &cratonvm_types::PointerMap) {
        for slot in &self.slots {
            let v = slot.load(Ordering::Acquire);
            if v == 0 {
                continue;
            }
            if let Some(&new_addr) = pointer_map.get(&v) {
                // Against the value read: an excluded owner's racing tombstone
                // wins, and the retracted monitor is not brought back.
                let (success, failure) = (Ordering::AcqRel, Ordering::Relaxed);
                let _ = slot.compare_exchange(v, new_addr, success, failure);
            }
        }
        for obj in self.overflow.lock().iter_mut() {
            if let Some(&new_addr) = pointer_map.get(&(obj.as_ptr() as usize)) {
                // SAFETY: produced by the GC pointer map.
                *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
}

/// An entry in the thread registry for one JVM thread.
struct ThreadEntry {
    /// Human-readable name.
    name: String,
    /// OS thread join handle (None for the main thread).
    join_handle: Option<JoinHandle<()>>,
    /// The Java `Thread` object on the heap.
    java_thread_obj: Option<ObjectRef>,
    /// The Java-side `Thread.tid` value of `java_thread_obj` (real-JDK mode).
    /// `Thread.tid` is `final`, assigned once in the constructor from a
    /// process-wide monotonic counter, and NEVER reused across Thread
    /// objects — unlike a heap address, which the collector can recycle for
    /// a brand-new `Thread` after this entry's mirror dies. All identity
    /// lookups (getState/isAlive/interrupt/unpark) therefore prefer this
    /// key over pointer comparison; see `find_thread_id_by_java_tid` and
    /// the aliasing incident writeup in
    /// dohead-residual-http2-midrun-hang-FIXED.md.
    /// 0 = unknown (synthetic-layout mirror, or registered mid-construction
    /// before the ctor assigned `tid` — backfilled lazily on first lookup).
    java_tid: u64,
    /// Whether this thread is still running.
    alive: AtomicBool,
    /// Whether this thread can currently participate in counted STW barriers.
    ///
    /// `Thread.start()` registers the Java mirror before the spawned carrier has
    /// published its GC state or reached a safepoint-capable startup point. Such
    /// entries are alive for Java APIs but must not contribute to STW `expected`
    /// until the child marks itself ready.
    stw_ready: AtomicBool,
    /// T19.K1 — Whether this thread is a daemon. Default `false` matches
    /// the JLS rule that newly created threads inherit the parent's
    /// daemon status, which is `false` for the main thread. The CLI
    /// uses this to decide whether the process must wait for this
    /// thread before exiting (per the JVM spec, the VM lives until
    /// every non-daemon thread terminates). Stored as `AtomicBool`
    /// because `Thread.setDaemon(boolean)` may flip it from another
    /// thread before the target is `start()`-ed.
    daemon: AtomicBool,
    /// Parking permit for LockSupport.park()/unpark().
    park_state: Arc<ParkState>,
    /// Thread interrupt flag (shared with JvmThread for cross-thread access).
    interrupted: Arc<AtomicBool>,
    /// Root snapshot: ObjectRefs from this thread's frames, deposited at safepoints
    /// and before blocking operations. Used by GC to scan all threads' roots.
    root_snapshot: Arc<Mutex<Vec<ObjectRef>>>,
    /// Frame trace snapshot (shared with the JvmThread, like `root_snapshot`):
    /// this thread's Java call stack, published at the same blocking deposit
    /// points. Lets another thread read where this one is parked, backing
    /// cross-thread `Thread.getStackTrace()` / `dumpThreads()`.
    frame_trace: Arc<Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,
    /// Optional VM-side breadcrumb published by the owning `JvmThread` when
    /// `CRATONVM_DBG_VM_STATE=1` is enabled.
    vm_state: Arc<Mutex<String>>,
    /// Blocked-region GC state (shared with the JvmThread, like `root_snapshot`):
    /// lets a GC initiator remap this thread's snapshot and accumulate frame
    /// fixups while the thread is parked in a blocking native. See
    /// `GcBlockState` and `fold_pointer_map_into_blocked`.
    gc_block_state: Arc<GcBlockState>,
    /// JMX diagnostic roots. These are deliberately separate from a blocked
    /// thread's frame snapshot: a lock relationship must remain observable
    /// while the owner is running, and must be remapped across a moving GC.
    /// The three slots every `monitorenter` / `monitorexit` touches, in one
    /// allocation behind an `Arc` so the owning thread can reach them without
    /// the registry map — see [`ThreadRegistry::with_self_jmx_monitors`].
    jmx_monitors: Arc<JmxMonitorBook>,
    jmx_waiting_monitor: Mutex<Option<ObjectRef>>,
    /// Shared, so the owning thread can reach its own list without going
    /// through [`ThreadRegistry::threads`] — see
    /// [`ThreadRegistry::jmx_locked_synchronizers_of`].
    jmx_locked_synchronizers: Arc<Mutex<Vec<ObjectRef>>>,
    jmx_wait_started: Mutex<Option<Instant>>,
    // `jmx_blocked_count` / `jmx_blocked_nanos` moved into `JmxMonitorBook`
    // (`blocked_count` / `blocked_nanos`) in round 11 wave 17.
    jmx_waited_count: AtomicU64,
    jmx_waited_nanos: AtomicU64,
    // (The T1.5.1 `async_exception_slot` that sat here was removed in
    // interpreter round i1 wave 4: nothing posted to it — JDK 20+
    // `Thread.stop` throws, JDK 17's `stop0` keeps its own table, and there is
    // no JVMTI `StopThread` / JDWP `ThreadReference.Stop` — and nothing raised
    // what the safepoint drained from it. See
    // docs/feature-designs/profile-guided-inlining.md.)
    /// BUG-03 — raw address of this thread's [`cratonvm_gc::Tlab`] (a field of
    /// its `JvmThread`), published once the thread starts running. 0 until set
    /// / after teardown. Read by the cross-thread STW JIT root scan to recover
    /// the un-retired reserved tail of a peer it forcibly stopped while it was
    /// in JIT code (such a peer never reached a safepoint to retire its TLAB).
    ///
    /// Safety of the cross-thread read: it happens only after the STW barrier
    /// has been satisfied, at which point every *alive* peer is either parked
    /// at a safepoint or blocked (both having already retired their TLAB →
    /// `cursor`/`end` null) or forcibly OS-suspended by the collector — so the
    /// `Tlab` is never being mutated, and an alive thread's `JvmThread` cannot
    /// be concurrently dropped (teardown flips `alive=false` and clears this
    /// before dropping). `AtomicUsize` so the owning thread sets/clears it
    /// lock-free.
    tlab_addr: std::sync::atomic::AtomicUsize,
    /// XT-FRAME-SCAN: the owning thread's whole `JvmThread` address (0 =
    /// unpublished). Published with the same discipline as `tlab_addr`
    /// (address-stable for the thread's life, published by the owner) and
    /// cleared together with it in [`ThreadRegistry::clear_tlab_addr`]; read
    /// only for OS-frozen takeover peers (see `frozen_peer_thread_addrs`).
    jvm_thread_addr: std::sync::atomic::AtomicUsize,
    /// xt-hardening (2026-07-03): the OS thread id of the thread executing
    /// this entry, published by the thread itself at startup (next to its
    /// TLAB address). `0` = not yet published. The GC initiator snapshots
    /// the alive set's OS tids atomically with the barrier's `expected`
    /// computation so the cross-thread JIT takeover only excuses
    /// (`reduce_expected`) frozen peers that were actually COUNTED —
    /// excusing an uncounted newcomer releases the barrier while a counted
    /// mutator still runs, racing the collection.
    os_tid: std::sync::atomic::AtomicU32,
    /// gc-common w6-g (`common-w3a-jni-unbound-counted-monitor-enter`,
    /// residual 2): the publish ordinal of the last [`ThreadRegistry::set_os_tid_current`]
    /// for this entry (`0` = never published through it). Several alive
    /// entries can carry one OS tid: every virtual thread that ever mounted on
    /// a carrier republished the carrier's tid at its mount, and nothing
    /// clears it at unmount. The MOUNTED one is the entry whose publish on
    /// that OS thread is the newest, because a mount publishes before it runs
    /// any Java (`vm_exec.rs::resume_virtual_continuation`). Written after
    /// `os_tid`, both `Release`; see [`newest_os_tid_claimant`] for the read
    /// side.
    os_tid_seq: AtomicU64,
    /// gen r5w2/roots6: is this entry's Java execution on a machine stack
    /// right now? `true` for life for platform and foreign threads; a virtual
    /// thread lowers it when its continuation yields (just BEFORE the deposit
    /// that raises `in_blocked_region`, so it is never seen blocked and
    /// mounted while its carrier unmounts it) and raises it again at every
    /// mount, next to its `set_os_tid_current`
    /// (`vm_exec::resume_virtual_continuation`).
    ///
    /// `os_tid` of an unmounted continuation is its LAST carrier's, kept on
    /// purpose (the census needs a non-zero tid), so without this bit a parked
    /// continuation looked like a thread blocked on that carrier: the
    /// helper-window pass signalled the carrier for it (`blocked_os_tids`), and
    /// a take-over walked its heap frames as if they were frozen on the
    /// carrier's stack (`frozen_peer_thread_addrs`). See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    mounted: AtomicBool,
    /// gci i12/vp: bumped by every [`ThreadRegistry::set_mounted`] (a
    /// continuation's every mount and unmount) and by every replacement of
    /// the entry's `root_snapshot` / `gc_block_state` `Arc`. The validity
    /// epoch of [`Self::parked_filter`]: an unmounted continuation's
    /// blocked-window words (snapshot, fixup values, `slot_origins`,
    /// `native_slots`) change only through the fold that recomputes the
    /// filter or through the owner, which runs only after a mount -- and the
    /// mount bumps this first (`jdk_continuation::run_slice`,
    /// `vm_exec::resume_virtual_continuation`: `set_mounted(tid, true)`
    /// before the remount's drain).
    mount_epoch: AtomicU64,
    /// gci i12/vp (`CRATONVM_GC_VT_PARKED_FOLD_FILTER`): the address filter
    /// of this entry's blocked-window words as the last fold left them,
    /// valid for one [`Self::mount_epoch`]. Written and read only by a
    /// pause's fold.
    parked_filter: ParkedFoldFilter,
    /// When the entry registered (interpreter round i3 wave 81, lane L6): a
    /// thread dump's `elapsed=`, as HotSpot's thread statistics count from the
    /// thread's creation.
    registered_at: Instant,
    /// Monitors this thread entered OUTSIDE structured locking and has not yet
    /// exited: JNI `MonitorEnter` and a VM native's
    /// `NativeContext::monitor_enter` / `monitor_enter_gc_safe` (HotSpot's
    /// `JavaThread::_jni_monitor_count`). Counted only under
    /// `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT` (`VmConfig::vthread_death_sweep_by_count`,
    /// interpreter round i4 wave 95, lane L6), where a JDK continuation's death
    /// skips the walk of every inflated monitor when it reads 0
    /// ([`ThreadRegistry::note_unstructured_monitor_enter`]).
    unstructured_monitor_enters: std::sync::atomic::AtomicU32,
    /// This entry is counted in the JMM's live platform-thread count
    /// ([`ThreadRegistry::note_jmm_thread_start`]); lowered, and the count
    /// with it, exactly once at its death (interpreter round i5 wave 100,
    /// lane L6).
    jmm_counted: AtomicBool,
    /// zt t15/pl6: the entry is a virtual thread's, fixed at registration
    /// ([`ThreadRegistry::register_starting_virtual`], a JDK continuation's
    /// prepared registration). Keeps it out of the JMM roster
    /// ([`ThreadRegistry::jmm_perf_thread_counts`]), as `ThreadMXBean`'s
    /// mirror walk leaves a `BaseVirtualThread` out.
    virtual_thread: bool,
}

/// What [`ThreadRegistry::retire_continuation_thread`] read off a dying JDK
/// continuation thread's entry, for the second half of its death
/// ([`ThreadRegistry::finish_retired_continuation_thread`]) and its monitor
/// sweep (interpreter round i4 wave 95, lane L6).
pub(crate) struct RetiredThread {
    mirror: Option<ObjectRef>,
    park_state: Arc<ParkState>,
    /// This retire lowered `alive` (wave 96; `drop_dead_park_index_entry`).
    first_death: bool,
    /// `ThreadEntry::unstructured_monitor_enters` at the death.
    pub(crate) unstructured_monitor_enters: u32,
}

/// zt t15/pl6: `ThreadMXBean`'s four thread figures, read off the registry
/// ([`ThreadRegistry::jmm_perf_thread_counts`]) for the `java.threads.*` perf
/// counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JmmThreadCounts {
    /// `java.threads.live`: `getThreadCount`.
    pub live: i32,
    /// `java.threads.daemon`: `getDaemonThreadCount`.
    pub daemon: i32,
    /// `java.threads.livePeak`: `getPeakThreadCount`.
    pub peak: i32,
    /// `java.threads.started`: `getTotalStartedThreadCount`.
    pub started: i64,
}

/// What a thread dump prints of one registered thread beyond its stack
/// (interpreter round i3 wave 81, lane L6; [`ThreadRegistry::thread_dump_entries`]).
#[derive(Debug, Clone)]
pub struct ThreadDumpEntry {
    pub thread_id: ThreadId,
    pub name: String,
    pub alive: bool,
    pub daemon: bool,
    pub java_thread_obj: Option<ObjectRef>,
    /// `Thread.tid` when the registry has it (`0` is not reported).
    pub java_tid: Option<u64>,
    pub os_tid: Option<u32>,
    pub registered_at: Instant,
    /// The thread's `JvmThread` address, `0` when unpublished.
    pub jvm_thread_addr: usize,
    /// The low end of the stack band of the blocked wait the thread is in
    /// (`GcBlockState::blocked_wait_lo`), `0` outside one.
    pub blocked_wait_lo: usize,
}

/// The ordinal source for `ThreadEntry::os_tid_seq`.
///
/// Not compatibility state: only the RELATIVE order of two publishes on the
/// same OS thread is ever compared, and within one registry. Sharing one
/// counter between VMs changes no answer: every value is still unique and
/// increases in program order on each OS thread.
static OS_TID_PUBLISH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Which of several alive entries that claim one OS tid is the thread running
/// there now: the unique one with the newest publish ordinal.
///
/// `claims` is `(id, os_tid_seq)` for every alive entry whose `os_tid` is the
/// caller's. One claim is the answer. Several: the newest ordinal, if exactly
/// one entry holds it. A tie (only possible for ordinal `0`, an entry whose
/// `os_tid` was stored without [`ThreadRegistry::set_os_tid_current`]) is
/// ambiguous, so `None`.
fn newest_os_tid_claimant(claims: &[(ThreadId, u64)]) -> Option<ThreadId> {
    if let [(only, _)] = claims {
        return Some(*only);
    }
    let newest = claims.iter().map(|(_, s)| *s).max()?;
    let mut holders = claims.iter().filter(|(_, s)| *s == newest);
    let (id, _) = holders.next()?;
    if holders.next().is_some() {
        return None;
    }
    Some(*id)
}

/// For every OS tid an ALIVE entry claims, the claimant running there now
/// ([`newest_os_tid_claimant`]); `None` when the claim is ambiguous.
///
/// gen r5w1/crash5: the owner of a blocked-peer native-stack capture, which the
/// fold used to hand to EVERY alive blocked entry claiming the scanned tid. See
/// `ThreadRegistry::fold_pointer_map_into_blocked_audited`.
///
/// gcj j2/pn2 (`gcj-j1pn-raw-root-write-owner-findings-FIXED-20261025` item 1;
/// `skip_unmounted`, see [`claimant_skips_unmounted_enabled`]): an UNMOUNTED
/// entry (a parked virtual thread) claims nothing. A mount publishes a fresh
/// `os_tid_seq` and nothing republishes the carrier's own entry at unmount,
/// so a parked continuation stayed the newest claimant of its last carrier's
/// tid; the fold then refused it (unmounted) AND the carrier (not newest), and
/// a blocked carrier's own captures were never adopted -- its words kept
/// from-space addresses after a relocating cycle.
fn newest_claimants_by_os_tid(
    threads: &FxHashMap<ThreadId, ThreadEntry>,
    skip_unmounted: bool,
) -> FxHashMap<u32, Option<ThreadId>> {
    let mut claims: FxHashMap<u32, Vec<(ThreadId, u64)>> = FxHashMap::default();
    for (tid, e) in threads.iter() {
        if !e.alive.load(Ordering::Acquire) {
            continue;
        }
        let os_tid = e.os_tid.load(Ordering::Acquire);
        if !os_tid_claim_admitted(os_tid, e.mounted.load(Ordering::Acquire), skip_unmounted) {
            continue;
        }
        claims
            .entry(os_tid)
            .or_default()
            .push((*tid, e.os_tid_seq.load(Ordering::Acquire)));
    }
    claims
        .into_iter()
        .map(|(os_tid, c)| (os_tid, newest_os_tid_claimant(&c)))
        .collect()
}

/// gcj j2/pn2: may an alive entry with `os_tid` and `mounted` claim that OS
/// tid in [`newest_claimants_by_os_tid`]? A zero tid never claims; with
/// `skip_unmounted` an unmounted entry does not either. Pure, for the tests.
#[inline]
fn os_tid_claim_admitted(os_tid: u32, mounted: bool, skip_unmounted: bool) -> bool {
    os_tid != 0 && (mounted || !skip_unmounted)
}

/// `CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED` -- gcj j2/pn2, default ON (`=0`
/// kills: an unmounted continuation claims its last carrier's tid again, the
/// gen r5w1/crash5 rule), read once per fold that captured blocked-peer
/// stack words, and applied only when the fold's heap is the GENERATIONAL one
/// (the blocked-peer capture fold is its moving path; G1 / ZGC unchanged).
fn claimant_skips_unmounted_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED")
}

/// The capture fold's `skip_unmounted` for a fold whose heap
/// `is_generational`: the switch applies to the Generational fold only; a G1
/// or ZGC fold keeps the gen r5w1/crash5 rule whatever the switch says (their
/// conservative captures pin, so nothing they capture is written back moved).
/// gcj j3/x2 R9: split out so the "G1 / ZGC unchanged" claim is pinned by a
/// test, not by reading.
#[inline]
fn fold_claimant_skips_unmounted(is_generational: bool) -> bool {
    is_generational && claimant_skips_unmounted_enabled()
}

/// gen r5w1/crash5: blocked-peer native-stack captures the fold did NOT hand to
/// an alive blocked entry because another entry is the newest claimant of the
/// scanned OS tid (a parked virtual thread's stale carrier claim). Before the
/// fix each of these was adopted and later written from another thread.
pub static NATIVE_SLOTS_DECLINED_STALE_CLAIM: AtomicU64 = AtomicU64::new(0);

/// gen r5w2/roots6: blocked-peer native-stack captures NOT adopted because the
/// thread already carries the same `(address, captured value)` from an earlier
/// pass or pause. See `ThreadRegistry::fold_pointer_map_into_blocked_audited`.
pub static NATIVE_SLOTS_DUPLICATE_CAPTURES: AtomicU64 = AtomicU64::new(0);

/// gen r5w1/crash5: blocked-peer native-stack write-backs the waking thread
/// refused because the word is not in its own live stack
/// (`vm_exec::apply_native_slot_fixups`). Non-zero means a capture reached a
/// thread that does not own it -- every one of them used to be a store into
/// another thread's stack or into dead memory.
pub static NATIVE_SLOTS_REFUSED_OFF_STACK: AtomicU64 = AtomicU64::new(0);

/// gen r5w1/crash5: foreign attachments whose OS thread exited without
/// `DetachCurrentThread` and whose `JvmThread` was reaped by the thread-local
/// destructor (`jni::ForeignThreadBox`), and those it had to LEAK because the
/// thread was not idle or no VM could be reached. See
/// [`root_write_repair_census`].
pub static FOREIGN_ATTACHMENTS_REAPED: AtomicU64 = AtomicU64::new(0);
/// See [`FOREIGN_ATTACHMENTS_REAPED`].
pub static FOREIGN_ATTACHMENTS_LEAKED: AtomicU64 = AtomicU64::new(0);

/// `CRATONVM_DBG_ROOT_WRITE_AUDIT`'s line for a capture the fold declined to
/// route to a stale claimant. Rate-limited like the audit's own lines.
#[cold]
fn note_declined_stale_claim(thread_id: u64, os_tid: u32, declined: usize) {
    static LINES: AtomicU64 = AtomicU64::new(0);
    // gcj j2/pn2: ring mode prints nothing on the capture side either
    // (`NATIVE_SLOTS_DECLINED_STALE_CLAIM` keeps the count).
    if cratonvm_gc::root_write_audit::ring_mode() {
        return;
    }
    if LINES.fetch_add(1, Ordering::Relaxed) < cratonvm_gc::root_write_audit::ANOMALY_LINE_BUDGET {
        eprintln!(
            "[root-write-audit] fold declined {declined} native-stack capture(s) of os_tid={os_tid} \
             for thread_id={thread_id}: it claims that OS thread but is not the claimant running there"
        );
    }
}

/// `CRATONVM_DBG_ROOT_WRITE_AUDIT`'s line for a foreign attachment whose OS
/// thread exited attached (`jni::ForeignThreadBox`): reaped, or leaked.
#[cold]
pub(crate) fn note_undetached_attachment(thread_id: u64, freed: bool) {
    static LINES: AtomicU64 = AtomicU64::new(0);
    if LINES.fetch_add(1, Ordering::Relaxed) < cratonvm_gc::root_write_audit::ANOMALY_LINE_BUDGET {
        eprintln!(
            "[root-write-audit] foreign attachment thread_id={thread_id} exited without \
             DetachCurrentThread: {}",
            if freed {
                "registry entry released (or its VM was gone), JvmThread freed"
            } else {
                "LEAKED (not idle, or no VM to reach its registry through)"
            }
        );
    }
}

/// `(declined stale-claim captures, refused off-stack write-backs, reaped
/// undetached attachments, leaked undetached attachments)`, for a shutdown
/// census or a test.
pub fn root_write_repair_census() -> (u64, u64, u64, u64) {
    (
        NATIVE_SLOTS_DECLINED_STALE_CLAIM.load(Ordering::Relaxed),
        NATIVE_SLOTS_REFUSED_OFF_STACK.load(Ordering::Relaxed),
        FOREIGN_ATTACHMENTS_REAPED.load(Ordering::Relaxed),
        FOREIGN_ATTACHMENTS_LEAKED.load(Ordering::Relaxed),
    )
}

/// JIT round 16 wave 9b (lane ffm): may a pause skip SIGNALLING this thread?
/// Yes while it runs foreign code in a GC-safe region
/// (`GcBlockState::foreign_native`, raised by the owning thread BEFORE the
/// region's root deposit) AND that region is up (`in_blocked_region`, raised by
/// the deposit's last step). Both up means: the thread deposited a complete
/// root snapshot (its interpreter frames, its compiled frames scanned
/// conservatively and published pinned, `native_pin_roots`), it is excluded
/// from the barrier, its frames are frozen (C code cannot touch them, and
/// leaving the region waits out an active pause), and its `Rip` is in C, not in
/// compiled code. So the take-over would answer `STATE_NOT_JIT` for it and the
/// helper-window pass would find on its stack only what the deposit already
/// published; the `SIGUSR2` either sends buys nothing, and it makes a blocking
/// syscall in the C code return `EINTR` (`nanosleep` and `epoll_wait` are never
/// restarted, `SA_RESTART` or not). HotSpot never signals a thread in native.
///
/// JIT round 16 wave 11 (lane jnieintr): a JNI native in the in-native package
/// (`CRATONVM_JNI_NATIVE_TRANSITIONS`) raises the same mark
/// (`native::jni::raise_foreign_native`, `CRATONVM_JNI_IN_NATIVE_NO_SIGNAL`),
/// with the same order and the same deposit argument. A COUNTED thread in C
/// (no region) is never exempt: it made no deposit, and nothing proves its
/// `Rip` is outside compiled code (an up-call through a door that does not
/// transition would leave such a mark stale on a compiled peer).
fn ffm_in_native_signal_exempt(block: &GcBlockState) -> bool {
    block.foreign_native.load(Ordering::Acquire) && block.in_blocked_region.load(Ordering::Acquire)
}

/// gci i24/jp: does one blocked row count as "holds a deferred chain record"
/// for [`deferred_chain_peer_tids`]? Only when the collector's walk of the
/// record will run this pause: `resolve_deferred_blocked_jit_chains` skips a
/// state whose owner was dropped, so such a row counts as a row WITHOUT a
/// record (and keeps every band on its tid a window). Pure.
fn deferred_chain_row(owner_dropped: bool, has_record: bool) -> bool {
    has_record && !owner_dropped
}

/// gci i20/vh: the rule of
/// [`ThreadRegistry::blocked_os_tids_with_deferred_jit_chain`] over its rows
/// `(os tid, the entry holds a deferred chain record)`: a tid qualifies when at
/// least one blocked entry on it holds a record and NO blocked entry on it
/// lacks one. Sorted, deduplicated. Pure.
fn deferred_chain_peer_tids(rows: Vec<(u32, bool)>) -> Vec<u32> {
    let mut without: Vec<u32> = rows.iter().filter(|r| !r.1).map(|r| r.0).collect();
    without.sort_unstable();
    without.dedup();
    let mut with: Vec<u32> = rows
        .iter()
        .filter(|r| r.1 && without.binary_search(&r.0).is_err())
        .map(|r| r.0)
        .collect();
    with.sort_unstable();
    with.dedup();
    with
}

/// `CRATONVM_FFM_IN_NATIVE_NO_SIGNAL` (default ON, a correctness fix; JIT
/// round 16 wave 9b, lane ffm): see [`ffm_in_native_signal_exempt`]. `=0`
/// signals a thread in a GC-safe FFM downcall again. Read once per registry
/// (`ThreadRegistry::ffm_in_native_no_signal`, gci i2/jn; it was read per
/// roster read; no new `static`: the vm statics ratchet).
fn ffm_in_native_no_signal_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_IN_NATIVE_NO_SIGNAL")
}

/// `CRATONVM_DBG_STW_WATCHDOG_SECS=N` (JIT round 18 wave 10, lane ffmeintr;
/// debug, off when unset, `0` or unparsable): when a stop-the-world wait
/// (the take-over loop of a collection or of a handshake pause, the plain
/// `wait_for_all` of a pause without a take-over) or a lazy in-native leave
/// waiting on a claim outlives `N` seconds, print every registered thread's
/// state to stderr ([`ThreadRegistry::debug_stw_watchdog_census`]) -- once per
/// `N` seconds of the same wait -- so a silent hang names what it waits for.
/// Read once per wait, never on a fast path.
pub fn stw_watchdog_secs() -> Option<u64> {
    cratonvm_types::flags::runtime_var("CRATONVM_DBG_STW_WATCHDOG_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
}

/// The last-resort stall threshold of [`stw_stall_report_after`].
pub const STW_STALL_REPORT_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// `CRATONVM_STW_STALL_REPORT` (JIT round 19 wave 1, lane eintr19,
/// FFMEINTR18-5; default ON, `=0` off): a last-resort report, to stderr only,
/// of a wait that is already [`STW_STALL_REPORT_AFTER`] stuck -- the
/// initiator's stop-the-world wait (with the [`ThreadRegistry::debug_stw_watchdog_census`]
/// dump), a mutator parked at the barrier for one pause, a lazy in-native
/// leave on a claim, a lazy entry on a closed signal gate. Once per wait, by
/// `eprintln!` (no `tracing` subscriber needed): the R17 battery's silent
/// hangs printed nothing because the take-over loop's 64-round warning is a
/// `tracing::warn!`. HotSpot prints nothing here, and a healthy run never
/// reaches it, so the Java-visible streams are unchanged. `None` when off.
/// Read once per wait, never on a fast path. `CRATONVM_DBG_STW_WATCHDOG_SECS`,
/// when set, takes precedence where both apply.
pub fn stw_stall_report_after() -> Option<std::time::Duration> {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STW_STALL_REPORT")
        .then_some(STW_STALL_REPORT_AFTER)
}

/// JIT round 16 wave 14 (lane ffm3): one thread a pause claimed out of a lazy
/// in-native FFM downcall ([`ThreadRegistry::claim_lazy_native_peers`]): its
/// OS tid, its frozen stack band `[lo, hi)`, and its block state, which the
/// pause hands back with `GcBlockState::release_lazy_native_claim` when it
/// resumes its frozen peers.
pub(crate) struct LazyNativeClaim {
    pub(crate) os_tid: u32,
    pub(crate) lo: usize,
    pub(crate) hi: usize,
    pub(crate) state: Arc<GcBlockState>,
}

/// gch h4/bp: what one [`ThreadRegistry::scan_in_native_bands`] read: the
/// bands, their words and the roots the probe accepted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InNativeBandScan {
    pub(crate) threads: usize,
    pub(crate) words: usize,
    pub(crate) roots: usize,
}

/// gch h4/bp: the compiled band a JNI in-native thread left to the pause
/// (`native::jni::in_native_band_at_pause`), when the pause may read it: the
/// thread is in a blocked region (`in_blocked_region`, Acquire: it publishes
/// the band the thread stored before raising it) and has a band that passes
/// the lazy claim's own bounds. Every other thread answers `None` with one or
/// three loads.
///
/// NOT keyed on `foreign_native` (it was in h4a, and `vh4-bp.list` B failed
/// 3/3 on it): `native_call_leave_native` lowers that mark FIRST, then waits
/// in `leave_in_native_if` for a pause in progress with `in_blocked_region`
/// still up, and a pause in that window excluded the thread and skipped its
/// band -- the compiled frames were then in no root set (`poll-across`, a
/// leave per JNIEnv call). A band is non-zero only from its call's entry to
/// the call's last leave (`clear_in_native_band`, always with the flag down),
/// and every frame above `lo` belongs to that call's suspended callers, so
/// whenever the flag is up -- in C, waiting at a leave, or blocked in a region
/// a JNIEnv function opened -- the band is frozen, mapped and complete.
fn in_native_band_of(block: &GcBlockState) -> Option<(usize, usize)> {
    if !block.in_blocked_region.load(Ordering::Acquire) {
        return None;
    }
    let lo = block.in_native_band_lo.load(Ordering::Relaxed);
    let hi = block.in_native_band_hi.load(Ordering::Relaxed);
    crate::threading::jvm_thread::lazy_native_band_ok(lo, hi).then_some((lo, hi))
}

/// gch h4/bp: push every word of `[lo, hi)` that `probe` accepts onto `out`;
/// how many it pushed. Read volatile word by word: the lowest part of a band
/// may be under the thread's C code (`in_native_band_at_pause`), and a
/// garbage word the probe accepts only over-retains.
///
/// # Safety
///
/// `[lo, hi)` must be mapped, 8-aligned at `lo`, for the whole call:
/// [`ThreadRegistry::scan_in_native_bands`]'s contract.
unsafe fn scan_in_native_band_words(
    lo: usize,
    hi: usize,
    probe: &dyn Fn(usize) -> Option<ObjectRef>,
    out: &mut Vec<ObjectRef>,
) -> usize {
    let mut found = 0usize;
    let mut p = lo;
    while p + 8 <= hi {
        // SAFETY: the caller's contract.
        let w = std::ptr::read_volatile(p as *const usize);
        if let Some(o) = probe(w) {
            out.push(o);
            found += 1;
        }
        p += 8;
    }
    found
}

/// gci i24/bp: what one [`ThreadRegistry::credit_in_native_proven_bands`]
/// did: the bands it credited, their summed depth and young words, and the
/// bands it left uncredited (not exempt from the blocked roster; no proven
/// depth; a depth or OS-tid / band mismatch) or found already credited for
/// the same pause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InNativeCredit {
    pub(crate) credited: usize,
    pub(crate) depth: usize,
    pub(crate) words: usize,
    pub(crate) not_exempt: usize,
    pub(crate) unproven: usize,
    pub(crate) mismatched: usize,
    pub(crate) already: usize,
}

/// gci i24/bp: may a pause credit an in-native band whose deposit proved
/// `proven` JIT entries, on OS thread `os_tid` (0 = unknown), whose published
/// depth is `published`? Only a nonzero proof on a known thread, exactly equal
/// to the published depth (a smaller proof is a shortfall anyway; crediting
/// more than the thread holds would let it make up another peer's shortfall,
/// the one unsound direction of the account), and not a `duplicate` (a second
/// band on an OS tid or address range already credited this call). Pure.
#[inline]
fn in_native_credit_admissible(
    proven: usize,
    os_tid: u32,
    published: Option<usize>,
    duplicate: bool,
) -> bool {
    proven != 0 && os_tid != 0 && !duplicate && published == Some(proven)
}

/// gci i24/bp: push every word of `[lo, hi)` that `keep` accepts onto `out`,
/// raw (the young pin ledger wants words, interior ones included, not
/// objects). Read volatile word by word, as [`scan_in_native_band_words`]
/// reads: a garbage word from the C code's part of the band only over-pins.
///
/// # Safety
///
/// `[lo, hi)` must be mapped, 8-aligned at `lo`, for the whole call:
/// [`ThreadRegistry::scan_in_native_bands`]'s contract.
unsafe fn in_native_band_words_where(
    lo: usize,
    hi: usize,
    keep: &dyn Fn(usize) -> bool,
    out: &mut Vec<usize>,
) {
    let mut p = lo;
    while p + 8 <= hi {
        // SAFETY: the caller's contract.
        let w = std::ptr::read_volatile(p as *const usize);
        if keep(w) {
            out.push(w);
        }
        p += 8;
    }
}

/// Interpreter round i2 wave 58, lane L6: one counted thread a handshake
/// pause's census excused in a running native
/// ([`ThreadRegistry::excuse_running_natives`], `jvm_thread::NativeExcuse`).
/// The pause releases it (`NativeExcuse::release`) after its `complete_gc`.
pub(crate) struct ExcusedNative {
    pub(crate) tid: ThreadId,
    pub(crate) os_tid: u32,
    /// The word the census found raised (without `NX_EXCUSED`).
    pub(crate) word: usize,
    pub(crate) state: Arc<GcBlockState>,
}

/// Global registry of all JVM threads.
///
/// Tracks spawned threads, their Java Thread objects, join handles, and
/// liveness. Thread-safe via `parking_lot::Mutex`.
pub struct ThreadRegistry {
    /// T10.9.B: FxHashMap — ThreadId is internal.
    ///
    /// ARCH-2026-07-26 — `RwLock`, not `Mutex`. CratonVM runs one real OS
    /// thread per Java thread, and of the ~50 accessors on this type only 14
    /// mutate the map (they are all registration / teardown / one-shot
    /// publication paths). The other ~40 — `is_alive`, `get_park_state`,
    /// `is_blocked`, `java_block_state`, `frame_trace_of`, the JMX readers, and
    /// every O(N) safepoint census the collector runs — only need to *find* an
    /// entry and then operate on the atomics and per-entry locks inside it.
    /// Under a `Mutex` all of that traffic serialized on one global lock at L5,
    /// i.e. below the levels that are held across it. As a reader-writer lock
    /// the reads proceed concurrently and only genuine registration churn
    /// excludes.
    ///
    /// This is sound to do here (and was checked site by site) because no
    /// accessor calls another accessor while holding a guard: every method
    /// either scopes the guard to a block and drops it before calling out, or
    /// holds it for a self-contained walk. `parking_lot::RwLock` reads are NOT
    /// recursion-safe, so a future edit that nests two acquisitions on one
    /// thread would deadlock — keep the "acquire, use, drop, then call out"
    /// discipline, or use `read_recursive()` deliberately.
    ///
    /// Level: L5 `thread_registry` (see `cratonvm_types::lock_order`). This
    /// lock is not yet wrapped in `OrderedPlRwLock`, so the checker does not
    /// observe it — the level is documentation, exactly as it was before.
    ///
    /// AUTO-TRAIT NOTE: this raises a bound. `Mutex<T>: Sync` needs only
    /// `T: Send`, but `RwLock<T>: Sync` needs `T: Send + Sync`, so
    /// [`ThreadEntry`] must now be `Sync` for `ThreadRegistry` to be `Sync` —
    /// which `vm::vm::realms::ThreadRealm` requires, since it holds one by
    /// value inside the shared VM. It is, but only because
    /// `ObjectRef` carries an `unsafe impl Sync` (`types/src/value.rs`), which
    /// the bare `java_thread_obj: Option<ObjectRef>` field relies on, and
    /// because `std::thread::JoinHandle<T>` is unconditionally `Sync`. If
    /// either ever changes, put those two fields behind their own locks rather
    /// than reverting this one.
    threads: RwLock<FxHashMap<ThreadId, ThreadEntry>>,
    /// The registry id whose `java.lang.Thread` has ENDED while its entry stays
    /// alive (`u64::MAX`: none): the launcher's main thread once `main` ended
    /// (interpreter round i5 wave 101, lane L6;
    /// `vm_exec::end_main_thread_java_side`). Its `JvmThread` still waits for
    /// the non-daemon threads and runs the shutdown sequence, so the entry is
    /// counted for safepoints and roots as before; only the Java-visible doors
    /// (`isAlive`, `getState`, the enumeration) answer it terminated, as
    /// HotSpot's detached main thread reads.
    java_ended_thread: AtomicU64,
    /// The [`Self::java_ended_thread`] entry's `java_thread_obj` was replaced
    /// by a `DestroyJavaVM` mirror (interpreter round i5 wave 102, lane L6;
    /// `vm_exec::list_destroy_java_vm_thread`): the doors that are handed a
    /// mirror answer that one alive and RUNNABLE, the ended `main` mirror
    /// still terminated, and the enumeration lists the entry again.
    destroy_java_vm_listed: AtomicBool,
    /// How many entries of [`Self::threads`] are dead (`alive == false`).
    /// Written only under the `threads` WRITE lock (the one insert, the one
    /// `alive` true -> false edge, the purge), so it always equals the walk
    /// `mark_dead` used to count with. Interpreter round i4 wave 95, lane L6:
    /// a JDK continuation's death reads it instead of walking every entry
    /// (`CRATONVM_VTHREAD_ONE_PUBLISH`, [`Self::retire_continuation_thread`]).
    dead_entries: std::sync::atomic::AtomicUsize,
    /// The JMM's platform-thread counters (interpreter round i5 wave 100,
    /// lane L6; HotSpot's `ThreadService::_total_threads_count`,
    /// `_atomic_threads_count` and `_peak_threads_count`): threads started
    /// (`Thread.start` of a platform thread, the main thread, a JNI attach;
    /// never a virtual thread, never decremented), live ones of those, and
    /// the high-water mark of the live count, raised at every start so a
    /// spike between two `getPeakThreadCount` reads is seen.
    jmm_started: AtomicU64,
    jmm_live: std::sync::atomic::AtomicI32,
    jmm_peak: std::sync::atomic::AtomicI32,
    /// zt t15/pl6: the JMM roster -- the entries `ThreadMXBean`'s walk lists
    /// (alive, with a `java.lang.Thread` mirror, not a virtual thread's;
    /// [`ThreadRegistry::jmm_roster_lists`]) -- and the daemons among them,
    /// kept at each edge (registration, death, mirror set or withdrawn,
    /// daemon flag), so the perf-data sampler reads the bean's figures
    /// without a walk ([`ThreadRegistry::jmm_perf_thread_counts`]).
    jmm_roster_live: std::sync::atomic::AtomicI32,
    jmm_roster_daemon: std::sync::atomic::AtomicI32,
    next_id: AtomicU64,
    /// Process-unique identity for this registry instance, used to key the
    /// per-thread self-handle cache (see [`ThreadRegistry::with_self_jmx_monitors`]).
    ///
    /// Deliberately NOT the registry's address: a dropped registry's address
    /// can be reused by a later one, and `ThreadId`s restart per registry, so
    /// an address-keyed cache could serve one registry's slot to another's
    /// same-numbered thread. A monotonic id cannot alias.
    registry_id: u64,
    /// WP4.1 — O(1) reverse index from Java `Thread` object pointer to
    /// its `ParkState`. AQS / `LockSupport.unpark(Thread)` calls this on
    /// the hot path of every queued lock release, so a linear walk over
    /// `threads` (the previous behavior) costs Θ(N) per unpark with
    /// 16-thread fair-mode contention causing 160 K unparks → 2.5 M map
    /// scans + mutex acquires. The reverse map keeps the same lock-free
    /// semantics (the entries are `Arc<ParkState>` clones, identical to
    /// the per-`ThreadEntry` copies) but takes O(1).
    ///
    /// Keyed by `ObjectRef::as_ptr() as usize` because `ObjectRef`
    /// already hashes by pointer; we just need a `Hash + Eq` form.
    // addr-keyed: REMAPPED by update_thread_objs_after_gc, removed with the thread.
    thread_obj_to_park: Mutex<FxHashMap<usize, Arc<ParkState>>>,
    /// Reverse index from Java-side `Thread.tid` (unique, never reused —
    /// see `ThreadEntry::java_tid`) to registry `ThreadId`. This is the
    /// aliasing-proof identity map: a dead thread's entry stays here for
    /// TERMINATED `getState()` answers, but a NEW `Thread` allocated at the
    /// dead mirror's recycled heap address has a different `tid` and can
    /// never be confused with it (the address-keyed maps can).
    java_tid_to_id: Mutex<FxHashMap<u64, ThreadId>>,
    /// Vacated heap addresses of relocated `java.lang.Thread` mirrors →
    /// owning thread id, for stale-receiver recovery (see
    /// [`Self::recover_stale_mirror`]). Populated by
    /// `update_thread_objs_after_gc` every time a moving / promoting young
    /// collection relocates a mirror: the address the GC vacated. A running
    /// or blocked frame that resumed holding a not-yet-remapped copy of that
    /// OLD address (the frame/operand remap-coverage gap documented in
    /// `gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md`)
    /// can then recover the live mirror instead of reading a zeroed object's
    /// null `holder` and NPEing in `Thread.getThreadGroup` (Tomcat
    /// `TestDigestAuthenticator` et al.). `.0` is the lookup map; `.1` is the
    /// FIFO eviction order bounding it to `FORMER_MIRROR_CAP` entries.
    former_mirror_addrs: Mutex<(
        // addr-keyed: VACATED addresses on purpose; bounded FIFO (FORMER_MIRROR_CAP).
        FxHashMap<usize, ThreadId>,
        std::collections::VecDeque<usize>,
    )>,
    /// Reverse index from an owned `AbstractOwnableSynchronizer`'s heap address
    /// to the thread its `jmx_locked_synchronizers` list currently records it
    /// on — the same shape, and for the same reason, as `thread_obj_to_park`
    /// above: without it, one AQS ownership transition costs Θ(threads) map
    /// walks and mutex acquisitions, and there are two of them per uncontended
    /// `ReentrantLock.lock()`/`unlock()` pair.
    ///
    /// Strictly derived state. [`Self::set_jmx_owned_synchronizer`] is its only
    /// producer, and it updates this map and the per-thread lists together;
    /// [`Self::update_thread_objs_after_gc`] rekeys it whenever a moving
    /// collection relocates a recorded synchronizer, in the same pass that
    /// remaps the lists themselves. An entry naming a reaped thread is inert —
    /// `threads.get` answers `None` and the removal is skipped, exactly as the
    /// linear scan used to skip a missing entry.
    /// SHARDED, and the shard count is the point. This map is written twice per
    /// uncontended `ReentrantLock` lock/unlock pair by whichever thread owns
    /// the lock, so a single mutex here serialises every producer against every
    /// consumer that share a queue — the exact shape of
    /// `LinkedBlockingQueue.add`/`take`.
    ///
    /// MEASURED, one binary, `CRATONVM_JMX_OWNED_SYNCHRONIZERS=off` as the B
    /// arm. Single-threaded (`probes/AqsAttributionProbe.java`, the
    /// `setExclusiveOwnerThread x2` rung) the whole index costs **nothing**:
    /// 386 ns against 384. Two threads sharing a queue
    /// (`probes/HwtScaleProbe.java` at n=100 000, the producer thread and the
    /// timer worker) it costs **25% of the drain**: 767/718 ms against 529/615.
    /// A cost that appears only when a second thread arrives, and not at all
    /// on the same work done alone, is contention and nothing else.
    // addr-keyed: REMAPPED by update_thread_objs_after_gc (two phases, gcg g2/k).
    synchronizer_owner: [Mutex<FxHashMap<usize, ThreadId>>; SYNCHRONIZER_OWNER_SHARDS],
    /// `CRATONVM_FFM_LAZY_CLAIM_NO_ROOTS_PAUSE` for this VM, read once. The
    /// downcall return path uses it to keep the default lazy-native fast path
    /// free of a barrier lock; `=0` retains the synchronized wait path.
    no_roots_lazy_claims_enabled: bool,
    /// `CRATONVM_FFM_IN_NATIVE_NO_SIGNAL` for this VM, read once (gci i2/jn,
    /// i1 review R5): the three roster reads (`blocked_os_tids`,
    /// `blocked_os_tids_split_by_monitor_proof`, `alive_count_and_os_tids`)
    /// read the environment on every call before. Default on; `=0` off.
    ffm_in_native_no_signal: bool,
    /// `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE` for this VM, read once
    /// (interpreter round i2 wave 59, lane L6): a blocking region opened as
    /// `BlockKind::InNative` publishes `jvm_thread::JAVA_STATE_IN_NATIVE`
    /// (HotSpot's `_thread_in_native`: RUNNABLE, in native) instead of
    /// WAITING. See [`Self::blocked_in_native_runnable`].
    blocked_in_native_runnable: bool,
    /// `CRATONVM_THREAD_INFO_FRAMELESS` for this VM, read once (interpreter
    /// round i2 wave 61, lane L6;
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// a `ThreadInfo` door that reports no frames (`maxDepth` 0, no locked
    /// monitors) builds its snapshot without capturing the target's stack,
    /// so it takes no frame-trace pause. See [`Self::thread_info_frameless`].
    thread_info_frameless: bool,
    /// `CRATONVM_THREAD_DUMP_ONE_PAUSE` for this VM, read once (interpreter
    /// round i2 wave 61, lane L6;
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// a dump of many threads' stacks takes one frame-trace pause, not one
    /// per running target. See [`Self::thread_dump_one_pause`].
    thread_dump_one_pause: bool,
    /// `CRATONVM_THREADS_ATTACH_ORDER` for this VM, read once (interpreter
    /// round i6 wave 133, lane L7; HotSpot parity, default on, `=0` off;
    /// item 5 of
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// See [`Self::threads_attach_order`].
    threads_attach_order: bool,
    /// `CRATONVM_GC_HANDSHAKE_FRAME_TRACE` for this VM, read once (gci i23/hs;
    /// default ON since JIT round 23 wave 6, `=0` kills). See
    /// [`Self::handshake_frame_trace`].
    handshake_frame_trace: bool,
    /// `CRATONVM_GC_HANDSHAKE_LOOP_KICK` for this VM, read once (gcj j1/cm,
    /// default ON). See [`Self::handshake_loop_kick`].
    handshake_loop_kick: bool,
    /// `CRATONVM_GC_HANDSHAKE_SPIN_WAIT` for this VM, read once (gcj j1/cm,
    /// default ON, `=0` kills; reached only by [`Self::handshake_and_wait`]):
    /// the requester polls its ticket between slices instead of a timed
    /// condvar wait, which the OS rounds up to its timer tick.
    handshake_spin_wait: bool,
    /// `CRATONVM_DBG_STW_EXPECTED_IDS` for this VM, latched once (gcj j2/cm2;
    /// `gcj-j1cm-safepoint-barrier-and-registry-review-residuals` item 8):
    /// read by the collection request's census closure under the barrier's
    /// `inner` and by every blocking park, which re-read the environment
    /// snapshot each time. Presence, as before. See
    /// [`Self::dbg_stw_expected_ids`].
    dbg_stw_expected_ids: bool,
    /// `CRATONVM_GC_HANDSHAKE_TARGET_KICK` for this VM, read once (gcj j2/cm2,
    /// opt-in; reached only under the opt-in `handshake_frame_trace` with the
    /// loop kick on, and only on the Generational backend). See
    /// [`Self::handshake_target_kick`].
    handshake_target_kick: bool,
    /// `CRATONVM_GC_HANDSHAKE_POLL_BIT` for this VM, read once (t1 lane E,
    /// default ON since 2026-10-26, `=0` kills; every backend). See [`Self::handshake_poll_bit`].
    handshake_poll_bit: bool,
    /// `CRATONVM_GC_HANDSHAKE_WAIT_SEES_BLOCK` for this VM, read once (t1 lane
    /// E2, opt-in, performance; every backend): the spin slice of
    /// [`Self::handshake_and_wait_kicking_twice`] also ends the moment the
    /// target enters a blocked region or is dropped, instead of at the slice's
    /// end.
    handshake_wait_sees_block: bool,
    /// `CRATONVM_GC_HANDSHAKE_UNMOUNTED_DEFERS_KICK` for this VM, read once
    /// (t1 wave 8 lane HANDSHK, opt-in, performance; every backend): a target
    /// that is UNMOUNTED when its op is queued is not kicked at once; the kick
    /// runs after the first slice, only if the op is still pending. See
    /// [`Self::handshake_and_wait_kicking_twice`].
    handshake_unmounted_defers_kick: bool,
    /// `CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED` for this VM, read once (gcj
    /// j3/rt3, default ON, `=0` kills; every backend): an unmounted entry
    /// never answers [`Self::thread_id_for_current_os_tid`]. See
    /// [`Self::thread_id_for_os_tid_inner`].
    os_tid_identity_skips_unmounted: bool,
    /// `CRATONVM_PUBLISHED_WAIT_KIND` for this VM, read once (interpreter
    /// round i2 wave 63, lane L1; stage 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a `BlockKind::Sleeping` region publishes
    /// `jvm_thread::JAVA_WAIT_KIND_SLEEPING`, and JVMTI's `SLEEPING` and
    /// JDWP's `ThreadStatus.SLEEPING` read it ([`Self::published_sleeping`])
    /// instead of the deposited top frame. Default ON since the wave-64 host run
    /// (`L1W63JvmtiSleepKinds --compatible` matches HotSpot only with it on); `=0` is the kill switch.
    published_wait_kind: bool,
    /// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` for this VM, read once
    /// (interpreter round i2 wave 64, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// every wait publishes its kind -- a park `PARKED`, `Object.wait` and
    /// the served `Thread.join` `IN_OBJECT_WAIT`, a sleep `SLEEPING` (it
    /// implies [`Self::published_wait_kind`]) -- and JVMTI's kind bit
    /// ([`Self::published_wait_kind_of`]) and the lazy-wait reading of
    /// [`Self::java_block_state`] read the word, taking no per-thread mutex.
    /// Default ON since the wave-67 triage: the unrecorded-wait census reads 0
    /// regions in both modes and the JVMTI wait-kind probes (`L1W64JvmtiAllWaitKinds`,
    /// `L1W63JvmtiSleepKinds`, `L1W57JvmtiWaitStates`, `L1W58JvmtiInNative`) match
    /// HotSpot with it on; `=0` is the kill switch.
    published_all_wait_kinds: bool,
    /// `CRATONVM_SERVED_WAIT_KINDS` for this VM, read once (interpreter round
    /// i2 wave 65, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// a served stand-in's `Object.wait` that stands for JDK code that parks
    /// (`NativeContext::monitor_wait_as` with `BlockKind::Parked` /
    /// `TimedParked`; the `--compatible` `CountDownLatch.await`) reports the
    /// JDK body's state (TIMED_WAITING for a timed one) and publishes
    /// `jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED`, which JVMTI reads as
    /// `PARKED` ([`Self::published_monitor_parked`]). Off, such a wait is a
    /// plain native `Object.wait` (WAITING, `IN_OBJECT_WAIT`). Default on;
    /// `=0` is the kill switch.
    served_wait_kinds: bool,
    /// `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` for this VM, read once
    /// (interpreter round i2 wave 66, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// the census of the blocking regions that open with no recorded wait
    /// kind, by the Rust call site of the door. Off (the default), one load of
    /// its mode per kind-less door.
    unrecorded_wait_census: UnrecordedWaitCensus,
    /// `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE` for this VM, read once
    /// (interpreter round i2 wave 66, lane L1): a wait HotSpot performs
    /// inside the VM (a JVMTI function waiting for another thread to stop)
    /// opens `NativeContext::begin_vm_wait_region` and reads RUNNABLE, not a
    /// WAITING region with no wait kind. Default on; `=0` is the kill switch.
    vm_internal_waits_runnable: bool,
    /// `CRATONVM_PARK_LAZY` for this VM, read once (interpreter round i3 wave
    /// 71, lane L1; `docs/feature-designs/jdk-only-mode.md`):
    /// `Some(budget)` -- the monitor lazy-park budget,
    /// `CRATONVM_MONITOR_LAZY_PARK_US`, 2 000 us unless set -- when a
    /// platform thread's `Unsafe.park` first waits for its permit as a
    /// running mutator, and only a park still waiting after it pays the
    /// blocking prologue (TLAB retire, root deposit with its JIT band scan,
    /// GC-blocked entry, wake-side refresh). While it waits, its
    /// `JAVA_WAIT_KIND_PARKED` kind answers [`Self::java_block_state`] as the
    /// lazy `Object.wait` does. Default OFF (`truthy_word`).
    park_lazy_budget: Option<std::time::Duration>,
    /// `CRATONVM_STAND_IN_WAIT_OBJECTS` for this VM, read once (interpreter
    /// round i2 wave 68, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// a stand-in's blocking region publishes the object the JDK code it
    /// stands for would wait or park on (`NativeContext::begin_blocking_region_as_on`):
    /// an `Object.wait` kind as the thread's waiting monitor
    /// ([`Self::publish_region_waiting_monitor`]), a park kind as the
    /// thread's `parkBlocker`; and the `LockSupport.park*(Object, ..)`
    /// bridges set `parkBlocker` as the JDK's own bodies do. So
    /// `ThreadInfo.getLockName()` names what HotSpot names. Default on (a
    /// correctness fix toward HotSpot); `=0` is the kill switch.
    stand_in_wait_objects: bool,
    /// `CRATONVM_PARK_LAZY_STW_WAKE` for this VM, read once (interpreter
    /// round i3 wave 72, lane L6; page
    /// `interpreter-i71-L1-proposal-a-running-phase-before-a-blocking-park-DONE-20261224`,
    /// follow-up (a)): a park's running phase is counted in
    /// [`Self::lazy_park_phases`] and listed in [`Self::lazy_park_states`],
    /// and a stop-the-world initiator wakes it
    /// ([`Self::wake_lazy_park_phases`]) instead of the phase noticing the
    /// pause at its next 100 us slice; the phase then waits in one wait of
    /// its whole budget. Default OFF (`truthy_word`).
    park_lazy_stw_wake: bool,
    /// How many threads are in a park's running phase right now, with
    /// [`Self::park_lazy_stw_wake`] on (0 otherwise). The handshake with
    /// [`Self::wake_lazy_park_phases`] is `MonitorTable::lazily_parked`'s.
    lazy_park_phases: AtomicU64,
    /// The `ParkState`s of the threads counted in [`Self::lazy_park_phases`]
    /// (pushed before the count is raised, removed after it is lowered).
    lazy_park_states: Mutex<Vec<Arc<ParkState>>>,
    /// `CRATONVM_PARK_DEPOSIT_FROZEN_BAND` for this VM, read once
    /// (interpreter round i3 wave 72, lane L6; follow-up (b) of the same
    /// page): a park that blocks deposits its roots under
    /// `conservative_roots::with_frozen_band_memo`, as
    /// `begin_blocking_region_with_state` has since SBR5, so a park from a
    /// compiled caller does not re-scan the frozen native band word by word.
    /// Default OFF (`truthy_word`).
    park_deposit_frozen_band: bool,
    /// gci i47w lane pr47: `CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`
    /// (default ON, `=0` kills) for this VM, read once: on G1 a blocking
    /// park deposits through the lazy blocking-region deposit, so its
    /// compiled chain is deferred to the collector and the helper-window
    /// pass demotes the parked peer (see `vm_exec::park_deposit_defers_jit_chain`).
    park_defers_jit_chain: bool,
    /// `CRATONVM_UNPARK_ONE_LOOKUP` for this VM, read once (interpreter round
    /// i3 wave 72, lane L6; follow-up (c) of the same page): `Unsafe.unpark`
    /// of a platform thread reads the mirror's `tid` once and resolves the
    /// thread id and its `ParkState` in one registry call
    /// ([`Self::unpark_target_by_java_tid`]), and asks the virtual-thread
    /// manager only once a virtual thread exists. Default ON
    /// (`runtime_flag_default_on`; `=0` reverts).
    unpark_one_lookup: bool,
    /// `CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT` for this VM, read once
    /// (interpreter round i4 wave 99, lane L1; page
    /// `interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md`): the leave of a
    /// `begin_blocking_region*` region (`end_blocking_region*`) keeps the
    /// root snapshot its entry deposited, skipping the frame audit and the
    /// full rebuild, when nothing that snapshot is a function of changed in
    /// the window (`vm_exec::blocking_leave_key`). Default ON
    /// (`runtime_flag_default_on`; `=0` restores the rebuild on every leave).
    blocking_leave_keeps_snapshot: bool,
    /// `CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS` for this VM, read once
    /// (interpreter round i5 wave 100, lane L1; proposal B of
    /// `interpreter-i100-L1-proposal-a-blocked-thread-is-walked-not-deposited-CLOSED-20261224.md`):
    /// the quiet leave's key is built into a recycled per-thread list and the
    /// leave compares it streamed, with no allocation; the comparison stays
    /// exact (`vm_exec::blocking_leave_key_matches`). Default OFF
    /// (`runtime_flag_on`) until the A/B medians.
    blocking_leave_key_reuses_buffers: bool,
    /// `CRATONVM_PARK_LAZY_SUSPEND_AWARE` for this VM, read once
    /// (interpreter round i3 wave 72, lane L6; follow-up (d) of the same
    /// page): while a JDWP / JVMTI suspension is in force
    /// (`DebuggerGates::suspension_in_force`, `experimental-debug` builds) a
    /// park skips its running phase, and a phase already running leaves it at
    /// its next slice -- or at once, with [`Self::park_lazy_stw_wake`], when
    /// `debug::publish_debugger_gates` wakes it -- so a suspended parker
    /// reaches its blocked region (deposited, frames published for the
    /// debugger) without waiting out the budget. Default OFF (`truthy_word`).
    park_lazy_suspend_aware: bool,
    /// `CRATONVM_PARK_ATOMIC_PERMIT` for this VM, read once (interpreter
    /// round i3 wave 73, lane L6; page
    /// `i72-L6-proposal-lock-free-park-permit-20261105`): `Unsafe.unpark` of
    /// a platform thread sets the permit with one swap and takes the
    /// `ParkState` mutex (and a wake-up syscall) only when the target waits
    /// (`ParkState::unpark_lock_free`), and a park with the permit set takes
    /// it without the mutex (`ParkState::take_permit_lock_free`). Default
    /// OFF (`truthy_word`).
    park_atomic_permit: bool,
    /// `CRATONVM_PARK_LAZY_SPIN_NS` for this VM, read once (interpreter round
    /// i3 wave 79, lane L6; page
    /// `i73-L6-proposal-spin-on-the-atomic-permit-before-the-lazy-wait`):
    /// `Some(spin)` -- a platform thread's park spins on its permit for up
    /// to `spin` (capped at [`PARK_LAZY_SPIN_CAP`]) before its running
    /// phase waits on the condvar. `None` (default: unset, `0`, unparsable,
    /// or a 1-CPU host, where the waker cannot run while the parker spins).
    park_lazy_spin: Option<std::time::Duration>,
    /// `CRATONVM_PARK_LAZY_ADAPTIVE` for this VM, read once (interpreter round
    /// i3 wave 81, lane L6; page `i77-L6-proposal-adaptive-lazy-park-budget`):
    /// a platform thread whose last [`crate::threading::jvm_thread::LAZY_PARK_MISSES_TO_SKIP`]
    /// running phases all ran out their budget skips the phase (straight to
    /// the blocking park), except every
    /// [`crate::threading::jvm_thread::LAZY_PARK_RECHECK_EVERY`]th park; a park
    /// that ends on its permit within the budget resets it
    /// (`ParkState::lazy_phase_skip`). Default OFF (`truthy_word`).
    park_lazy_adaptive: bool,
    /// `CRATONVM_UNPARK_EETOP` for this VM, read once (interpreter round i3
    /// wave 73, lane L6; page `i72-L6-proposal-unpark-through-eetop-20261105`):
    /// `Unsafe.unpark` of a platform thread reads the registry id from the
    /// mirror's `Thread.eetop` (`vm_exec::live_eetop`) and takes only the
    /// `threads` read lock, not the `java_tid` index mutex. Default OFF
    /// (`truthy_word`).
    unpark_eetop: bool,
    /// `CRATONVM_PARK_UNTIMED_NO_POLL` for this VM, read once (interpreter
    /// round i3 wave 74, lane L6; page
    /// `i73-L6-proposal-untimed-park-without-a-5ms-poll-20261106`): a
    /// blocking park waits without the 5 ms interrupt poll
    /// (`ParkState::park_interruptible_unpolled`), and
    /// [`Self::set_interrupted`] wakes the target's `ParkState` after raising
    /// its flag. Default ON since the wave-81 triage (`runtime_flag_default_on`;
    /// `L6W74ParkNoPollBench` idle-cpu 1.17 ms -> 0 per idle second, the other
    /// rows unchanged; `=0` reverts).
    park_no_poll: bool,
    /// `CRATONVM_SLEEP_NO_POLL` for this VM, read once (interpreter round i3
    /// wave 75, lane L6; page
    /// `i74-L6-proposal-sleep-without-the-10ms-poll-20261107`): a platform
    /// thread's `Thread.sleep` waits on its `ParkState`'s condvar for the
    /// whole remainder (`ParkState::sleep_unpolled`) instead of 10 ms
    /// `std::thread::sleep` slices, and [`Self::set_interrupted`] wakes it
    /// after raising the flag. Default ON (`runtime_flag_default_on`; `=0`
    /// reverts).
    sleep_no_poll: bool,
    /// `CRATONVM_PARK_WAKE_AFTER_UNLOCK` for this VM, read once (interpreter
    /// round i3 wave 76, lane L6; page
    /// `i73-L6-proposal-untimed-park-without-a-5ms-poll-20261106`): an
    /// `Unsafe.unpark` sets the permit under the `ParkState` mutex and wakes
    /// the waiters after releasing it, only when it set the permit
    /// (`ParkState::unpark_after_unlock`, HotSpot's `Parker::unpark`), and
    /// `Thread.interrupt` raises the flag without a wake
    /// ([`Self::raise_interrupt`]) and then wakes the target's state once
    /// (`ParkState::interrupt_wake_after_unlock`). Default OFF
    /// (`truthy_word`).
    park_wake_after_unlock: bool,
    /// `CRATONVM_PARK_UNTIMED_EVENT` for this VM, read once (interpreter round
    /// i5 wave 107, lane L6; pages `i97-L1-process-exit-wake-latency` and
    /// `i91-L6`, "Wave 107"): on Windows an untimed unpolled park waits on
    /// its `ParkState`'s auto-reset wake event with `WaitForSingleObject`
    /// (`ParkState::park_wait_with`, `untimed_event`), as HotSpot 25's
    /// Windows `Parker::park` waits on `_ParkHandle`, instead of the
    /// `parking_lot` condvar (whose wake requeues the waiter onto the mutex
    /// and goes through `WaitOnAddress`). Every unpark / interrupt wake
    /// already signals the event of a waiter on it. No effect elsewhere.
    /// Default OFF (`truthy_word`).
    park_untimed_event: bool,
    /// `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK` for this VM, read once
    /// (interpreter round i5 wave 108, lane L6; page
    /// `i97-L1-process-exit-wake-latency`, "Wave 108"): on Windows a
    /// Java-level timed park's one coarse wait on its wake event
    /// (`ParkState::park_wait_once_with`) asks its waker to raise the event
    /// AFTER releasing the `ParkState` mutex, as HotSpot's `Parker::unpark`
    /// calls `SetEvent` holding no lock. Set under the mutex, the event
    /// readies a waiter (boosted, often onto the waker's own core) whose
    /// first act is to re-take the mutex the preempted waker still holds.
    /// The untimed event wait (`park_untimed_event`) always defers. No effect
    /// elsewhere. Default OFF (`truthy_word`).
    park_event_signal_after_unlock: bool,
    /// `CRATONVM_PARK_EVENT_LOCKLESS_WAKE` for this VM, read once
    /// (interpreter round i5 wave 114, lane L1; page
    /// `i97-L1-process-exit-wake-latency`, "Wave 114"): on Windows a platform
    /// thread's Java-level park (the timed one wait, and the untimed unpolled
    /// wait) waits on its `ParkState`'s wake event and returns WITHOUT
    /// re-taking the state's mutex after the OS wait
    /// (`ParkState::park_wait_once_lockless`, `park_wait_untimed_lockless`),
    /// as HotSpot 25's Windows `Parker::park` returns from
    /// `WaitForSingleObject`. Takes precedence over
    /// `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK` and
    /// `CRATONVM_PARK_UNTIMED_EVENT` for those waits. No effect elsewhere.
    /// Default OFF (`truthy_word`).
    park_event_lockless_wake: bool,
    /// `CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL` for this VM, read once
    /// (interpreter round i5 wave 109, lane L6; page
    /// `i97-L1-process-exit-wake-latency`, "Wave 109"): on Windows a platform
    /// thread's `Thread.sleep` wait (`NativeContextImpl::sleep_wait_unpolled`)
    /// whose length in whole milliseconds is not a multiple of 10 holds
    /// `timeBeginPeriod(1)` for the wait, as HotSpot 25's Windows
    /// `PlatformEvent::park(jlong)` does through `HighResolutionInterval`
    /// (`Thread.sleep` -> `JavaThread::sleep_nanos` -> `_SleepEvent`). No
    /// Java-visible answer changes; the system clock ticks every 1 ms during
    /// the sleep, as on HotSpot. No effect elsewhere. Default OFF.
    sleep_high_resolution_interval: bool,
    /// `CRATONVM_INTERRUPT_ONE_LOOKUP` for this VM, read once (interpreter
    /// round i3 wave 76, lane L6; page
    /// `i75-L6-proposal-one-lookup-thread-interrupt-20261108`):
    /// `Thread.interrupt` resolves its target from `Thread.eetop` and raises
    /// the flag, takes the park state and the JMX waiting monitor in one
    /// `threads` read ([`Self::raise_interrupt`]). Default ON since the
    /// wave-76 triage (`runtime_flag_default_on`; `=0` reverts).
    interrupt_one_lookup: bool,
    /// `CRATONVM_INTERRUPT_ONE_WAKE` for this VM, read once (interpreter
    /// round i4 wave 91, lane L6; page
    /// `interpreter-i77-L6-proposal-one-trip-interrupt-wake-DONE-20261127`): `Thread.interrupt`
    /// skips the flag wake and wakes its target's park state once, at the
    /// end, with the notify still under the mutex
    /// (`ParkState::interrupt_wake_locked`). Ignored under
    /// [`Self::park_wake_after_unlock`], which already folds the two trips.
    /// Default ON since the wave-92 triage (`runtime_flag_default_on`, `=0` off).
    interrupt_one_wake: bool,
    /// `CRATONVM_INTERRUPT_FIELD_MEMO` for this VM, read once (interpreter
    /// round i3 wave 77, lane L6; page
    /// `i76-L6-proposal-interrupt-status-field-through-the-memo-20261109`):
    /// every write of the Java `Thread.interrupted` field -- the set of
    /// `Thread.interrupt` and the clear of every consumed interrupt
    /// (`vm_exec::set_thread_interrupted_field`) -- takes its slot from the
    /// thread-layout memo instead of a by-name lookup under the class-manager
    /// lock, and `Thread.interrupt` reads `CRATONVM_DBG_INTERRUPT` from
    /// [`Self::dbg_interrupt`] instead of the environment on every call.
    /// Default ON (`runtime_flag_default_on`; `=0` reverts).
    interrupt_field_memo: bool,
    /// `CRATONVM_DBG_INTERRUPT` read once at construction (wave 77); asked
    /// only with [`Self::interrupt_field_memo`] on.
    dbg_interrupt: bool,
    /// `CRATONVM_THREAD_START_ONE_PUBLISH` for this VM, read once
    /// (interpreter round i3 wave 82, lane L6; page
    /// `i74-L6-proposal-one-registry-write-per-thread-start-20261107`): a
    /// platform thread's start takes every `Arc` its registry entry holds in
    /// one read ([`Self::starting_thread_shared`]) and the child adopts them
    /// (`JvmThread::with_shared`), instead of the parent's two and the
    /// child's four `set_*` write locks re-installing them. Default OFF
    /// (`truthy_word`).
    thread_start_one_publish: bool,
    /// gci i12/vp: `CRATONVM_GC_VT_PARKED_FOLD_FILTER` for this VM, read once
    /// (default ON since 2026-10-17, `=0` kills). A relocating pause's blocked-thread fold
    /// ([`Self::fold_pointer_map_into_blocked_audited`]) skips an UNMOUNTED
    /// continuation whose cached address filter ([`FoldAddrFilter`], kept in
    /// `ThreadEntry::parked_filter` and valid for one `mount_epoch`) proves
    /// none of its words is a key of the pause's pointer map -- the fold would
    /// have rewritten nothing. Page
    /// `gci-i12vp-parked-continuations-folded-by-every-young-pause-FIXED-20261016`.
    vt_parked_fold_filter: bool,
    /// gci i12/vp: `CRATONVM_DBG_VT_PARKED_ROOTS` for this VM, read once: one
    /// `[vt-parked-roots]` stderr line per root gather
    /// ([`Self::collect_all_root_snapshots`]) and per blocked fold, with the
    /// unmounted continuations' share (entries, words, time). Diagnostic.
    dbg_vt_parked_roots: bool,
    /// gci i13/vg: `CRATONVM_GC_VT_PARKED_GATHER_FILTER` for this VM, read
    /// once (default ON since 2026-10-17, `=0` kills). The gather half of
    /// `gci-i12vp-parked-continuations-folded-by-every-young-pause-FIXED-20261016`:
    /// [`Self::collect_all_root_snapshots_split`] hands an UNMOUNTED
    /// continuation's snapshot to the collector as an OLD-ONLY root section
    /// when its cached [`FoldAddrFilter`] proves none of its words is in the
    /// collector's young set; a young-only collection skips that section and
    /// every phase that marks or moves old space consumes it. Implies that
    /// the blocked fold keeps the filters up to date (without skipping
    /// anything unless `vt_parked_fold_filter` is on too).
    vt_parked_gather_filter: bool,
    /// gci i13/vg: the collection door's split request for the NEXT
    /// [`Self::collect_all_root_snapshots`] and that gather's old-only
    /// section ([`Self::arm_parked_split_gather`],
    /// [`Self::take_parked_old_only`]). Armed and taken by the same pause on
    /// the same thread (`run_collection_pause`), so the door keeps its one
    /// pinned gather call. A leaf lock, never held across the gather.
    parked_split: Mutex<ParkedSplitGather>,
    /// gci i14/ve: `CRATONVM_GC_VT_PARKED_MIRROR_KEYED` for this VM, read
    /// once (default ON since 2026-10-17, `=0` kills). Item 6 of
    /// `gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016`:
    /// on a licensed Generational pause ([`Self::arm_mirror_keyed_gather`])
    /// a deposited JDK continuation's snapshot and its `VirtualThread` mirror
    /// stop being strong roots; each snapshot word is filed as a
    /// `metadata_pin` value OWNED by the mirror (the ephemeron "mirror ->
    /// frames"), so the frames are marked exactly when something else reaches
    /// the mirror, as HotSpot's stack chunk is reachable only from
    /// `VirtualThread.cont`. A mirror the pause did not keep retires its
    /// entry in the same pause ([`Self::sweep_mirror_keyed_parked`]).
    vt_parked_mirror_keyed: bool,
    /// gci i14/ve: the door's mirror-keyed request for the NEXT
    /// [`Self::collect_all_root_snapshots`], the entries that gather left out
    /// (consumed by the same pause's [`Self::sweep_mirror_keyed_parked`]) and
    /// the ids that sweep retired (taken after the pause,
    /// [`Self::take_collected_parked`]). A leaf lock, never held across the
    /// gather's entry walk.
    mirror_keyed: Mutex<MirrorKeyedGather>,
    /// gci i18/hw: `CRATONVM_G1_VT_PRECISE_DEPOSIT` for this VM, read once
    /// (default ON, `=0` kills). On G1 a virtual thread's continuation
    /// deposits its roots precisely, without the conservative-locals probes
    /// ([`continuation_deposit_is_precise`]).
    g1_vt_precise_deposit: bool,
    /// nio21 wave 5 L1 (page
    /// `r21-interrupt-field-and-flag-can-split-on-a-consume-race-FIXED-20261009.md`):
    /// striped per-thread interrupt locks. A thread's interrupt status has two
    /// records, the VM flag (`ThreadEntry::interrupted`, shared with the
    /// `JvmThread`) and the Java `Thread.interrupted` field; an interrupter
    /// writes (field := 1, flag := 1) and a consume (flag := 0, field := 0),
    /// and two plain stores on each side cannot be made to agree without
    /// excluding each other. The stripe is chosen by the flag's `Arc`
    /// address, so the registry entry and the `JvmThread` (which share one
    /// `Arc`) meet on the same lock. Leaf lock: held for the two stores only,
    /// never across a class-manager lock, an allocation or a block. Per VM,
    /// not a process global.
    interrupt_locks: [Mutex<()>; INTERRUPT_LOCK_STRIPES],
    /// `CRATONVM_INTERRUPT_LOCK` for this VM, read once (nio21 wave 5 L1):
    /// default on; `=0` is the kill switch (the two records are written with
    /// no exclusion, as before wave 5).
    interrupt_lock_enabled: bool,
}

/// How many stripes [`ThreadRegistry::interrupt_locks`] has. A power of two
/// so the index is a mask; collisions only serialize two unrelated threads'
/// two-store sections.
const INTERRUPT_LOCK_STRIPES: usize = 64;

/// The stripe of an interrupt flag, from its address. Multiplicative hash:
/// an `ArcInner<AtomicBool>` is 8- or 16-byte aligned, so the low bits are
/// constant.
#[inline]
fn interrupt_lock_stripe(flag: &AtomicBool) -> usize {
    let addr = flag as *const AtomicBool as usize as u64;
    ((addr >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize & (INTERRUPT_LOCK_STRIPES - 1)
}

/// What `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` asks of a blocking region that
/// opens with no recorded wait kind (interpreter round i2 wave 66, lane L1):
/// a plain `NativeContext::begin_blocking_region` /
/// `begin_timed_blocking_region`, or `begin_blocking_region_as` with
/// `BlockKind::Waiting` / `TimedWaiting`. Such a region publishes nothing in
/// `GcBlockState::java_wait_kind`, so JVMTI reads it `PARKED` whatever JDK
/// code it stands for (`PublishedWaitKind::Unrecorded`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnrecordedWaitMode {
    /// Unset, or `0` / `false` / `off` / `no`: nothing is counted.
    Off,
    /// Any other value (`1`, `CRATONVM_DBG=unrecorded-wait-kinds`): count
    /// each region by its door's call site; the table is printed at exit
    /// (`ThreadRegistry::report_unrecorded_wait_regions`).
    Count,
    /// `trace`: count, and print one line the first time each call site
    /// opens a region.
    Trace,
    /// `assert`: count and trace, and fail a `debug_assert!` at the door
    /// (debug builds; a release build traces). For a run that must reach no
    /// unclassified region.
    Assert,
}

impl UnrecordedWaitMode {
    /// The mode a value of `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` names
    /// (`None`: the variable is unset).
    pub fn from_value(value: Option<&str>) -> Self {
        match value {
            None => Self::Off,
            Some(v) if cratonvm_types::flags::value_is_off(v) => Self::Off,
            Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                "trace" => Self::Trace,
                "assert" => Self::Assert,
                _ => Self::Count,
            },
        }
    }
}

/// One call site's row of the unrecorded-wait census: the door it opened
/// and how many regions it opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnrecordedWaitSite {
    /// `file:line:column` of the door's caller (`#[track_caller]` on the
    /// `NativeContext` doors).
    pub site: String,
    /// Which door: `begin_blocking_region`, `begin_timed_blocking_region`,
    /// `begin_blocking_region_as(Waiting)` or `begin_blocking_region_as(TimedWaiting)`.
    pub door: &'static str,
    /// Regions opened there.
    pub count: u64,
}

/// The census behind `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` (interpreter round
/// i2 wave 66, lane L1). Per VM (a field of [`ThreadRegistry`]); the table is
/// taken only when a door is counted, never on the default path.
pub(crate) struct UnrecordedWaitCensus {
    mode: UnrecordedWaitMode,
    /// `(file, line, column, door)` -> regions opened.
    sites: Mutex<FxHashMap<(&'static str, u32, u32, &'static str), u64>>,
}

impl UnrecordedWaitCensus {
    fn from_env() -> Self {
        let value = cratonvm_types::flags::runtime_var("CRATONVM_DBG_UNRECORDED_WAIT_KINDS").ok();
        Self::with_mode(UnrecordedWaitMode::from_value(value.as_deref()))
    }

    fn with_mode(mode: UnrecordedWaitMode) -> Self {
        Self {
            mode,
            sites: Mutex::new(FxHashMap::default()),
        }
    }
}

/// The longest permit spin `CRATONVM_PARK_LAZY_SPIN_NS` may ask for: well
/// under the 100 us lazy-park slice, since a spinning parker is a running
/// mutator a pause has to wait for (interpreter round i3 wave 79, lane L6).
pub(crate) const PARK_LAZY_SPIN_CAP: std::time::Duration = std::time::Duration::from_micros(50);

/// The permit spin when `CRATONVM_PARK_LAZY_SPIN_NS` is unset: on by default
/// since the wave-79 triage (`L6W79ParkSpinBench` park-pingpong 17.9 -> 0.83
/// us, sq-handoff 15.9 -> 10.3 us, lock-condition 40.7 -> 26.1 us, both modes;
/// the battery unchanged).
pub(crate) const PARK_LAZY_SPIN_DEFAULT_NS: u64 = 10_000;

/// [`ThreadRegistry::park_lazy_spin`] from the variable's value and the
/// host's CPU count: a positive number of nanoseconds, capped at
/// [`PARK_LAZY_SPIN_CAP`]; [`PARK_LAZY_SPIN_DEFAULT_NS`] when unset; `None`
/// for `0`, an unparsable value, or a host with one CPU.
pub(crate) fn park_lazy_spin_from(value: Option<&str>, cpus: usize) -> Option<std::time::Duration> {
    if cpus < 2 {
        return None;
    }
    let nanos = match value {
        None => PARK_LAZY_SPIN_DEFAULT_NS,
        Some(v) => v.trim().parse::<u64>().ok().filter(|&n| n > 0)?,
    };
    Some(std::time::Duration::from_nanos(nanos).min(PARK_LAZY_SPIN_CAP))
}

/// The kind of a WAITING / TIMED_WAITING thread's wait, as its own wait
/// published it in `GcBlockState::java_wait_kind`
/// ([`ThreadRegistry::published_wait_kind_of`]; interpreter round i2 wave
/// 64, lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`). HotSpot's
/// `JavaThreadSleepState`, `JavaThreadParkedState` and
/// `JavaThreadInObjectWaitState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishedWaitKind {
    /// No kind published: a blocking region that did not say what it stands
    /// for (a plain `begin_blocking_region`, a stand-in for JDK code that
    /// parks), an unmounted virtual thread, or a thread not waiting at all.
    /// JVMTI reads it `PARKED`, as it read every such wait before.
    Unrecorded,
    /// `Thread.sleep` (`BlockKind::Sleeping`): JVMTI `SLEEPING`, JDWP
    /// `ThreadStatus.SLEEPING`.
    Sleeping,
    /// `LockSupport.park*` (or `BlockKind::Parked` / `TimedParked`): JVMTI
    /// `PARKED`.
    Parked,
    /// `Object.wait`, or a region standing for a JDK `wait` loop (the served
    /// `Thread.join`): JVMTI `IN_OBJECT_WAIT`.
    ObjectWait,
}

/// [`PublishedWaitKind`] of `block`'s published word. A sleep counts only
/// under the flag over TIMED_WAITING, as [`gc_block_state_sleeping`] reads
/// it; the other kinds are read as stored (the callers ask only a thread
/// whose state is WAITING / TIMED_WAITING, and `Object.wait`'s kind spans its
/// lazy phase with the flag down). Interpreter round i2 wave 64, lane L1.
#[inline]
fn gc_block_state_wait_kind(block: &GcBlockState) -> PublishedWaitKind {
    use crate::threading::jvm_thread::{
        JAVA_WAIT_KIND_MONITOR_PARKED, JAVA_WAIT_KIND_OBJECT_WAIT, JAVA_WAIT_KIND_PARKED,
        JAVA_WAIT_KIND_REGION_OBJECT_WAIT, JAVA_WAIT_KIND_SLEEPING,
    };
    match block.java_wait_kind.load(Ordering::Acquire) {
        JAVA_WAIT_KIND_SLEEPING if gc_block_state_sleeping(block) => PublishedWaitKind::Sleeping,
        // Wave 65 (lane L1, `CRATONVM_SERVED_WAIT_KINDS`): a served stand-in's
        // monitor wait for JDK code that parks.
        JAVA_WAIT_KIND_PARKED | JAVA_WAIT_KIND_MONITOR_PARKED => PublishedWaitKind::Parked,
        JAVA_WAIT_KIND_OBJECT_WAIT | JAVA_WAIT_KIND_REGION_OBJECT_WAIT => {
            PublishedWaitKind::ObjectWait
        }
        _ => PublishedWaitKind::Unrecorded,
    }
}

/// A thread in a `BlockKind::InNative` blocking region: the flag up over
/// `jvm_thread::JAVA_STATE_IN_NATIVE` (stored before the flag rises, lowered
/// at the region's end). Interpreter round i2 wave 59, lane L6.
#[inline]
fn gc_block_state_blocked_in_native(block: &GcBlockState) -> bool {
    block.in_blocked_region.load(Ordering::Acquire)
        && block.java_state.load(Ordering::Acquire)
            == crate::threading::jvm_thread::JAVA_STATE_IN_NATIVE
}

/// A thread in a `BlockKind::Sleeping` blocking region: the flag up over a
/// TIMED_WAITING `java_state` and `jvm_thread::JAVA_WAIT_KIND_SLEEPING`
/// (both stored before the flag rises; the kind lowered after it falls).
/// Interpreter round i2 wave 63, lane L1.
#[inline]
fn gc_block_state_sleeping(block: &GcBlockState) -> bool {
    block.in_blocked_region.load(Ordering::Acquire)
        && block.java_state.load(Ordering::Acquire) == 3
        && block.java_wait_kind.load(Ordering::Acquire)
            == crate::threading::jvm_thread::JAVA_WAIT_KIND_SLEEPING
}

/// How many shards [`ThreadRegistry::synchronizer_owner`] is split into.
///
/// A power of two so the index is a mask. 64 is chosen against the thing being
/// spread: threads that share one lock, which is a handful, not hundreds — and
/// the memory is 64 empty `FxHashMap`s per registry, which allocate nothing
/// until first insert.
const SYNCHRONIZER_OWNER_SHARDS: usize = 64;

/// Which shard owns a synchronizer's heap address.
///
/// Shifted by four first: heap objects are at least 8-byte aligned and in
/// practice 16, so the low bits are constant and hashing on them would put
/// everything in one shard — the failure mode that would leave this change
/// measuring as inert while looking correct.
#[inline]
fn synchronizer_owner_shard_index(key: usize) -> usize {
    (key >> 4) & (SYNCHRONIZER_OWNER_SHARDS - 1)
}

/// Move the rows of an address-keyed map from each `(old, new)` pair's old
/// key to its new key: every moving row is REMOVED first, then all are
/// inserted.
///
/// gcg g2/k. One pair at a time (`remove(old)` then `insert(new)`) is wrong
/// under a sliding compactor, where one object's new address is routinely
/// another one's old address: the first insert overwrites the second row
/// (dropping its value), and the second `remove` then moves the FIRST row
/// on. Two phases cannot collide: no two live objects share a new address,
/// and a row that does not move cannot sit at a moving row's new address
/// (that address held no live object before the collection, or held one that
/// moved away and is in the batch). A row left at a dead address is
/// overwritten, as it would have been by a later insert.
// addr-keyed: a parameter; the caller's table carries the disposition.
fn rekey_in_two_phases<V>(map: &mut rustc_hash::FxHashMap<usize, V>, moves: &[(usize, usize)]) {
    let mut moving: Vec<(usize, V)> = Vec::with_capacity(moves.len());
    for &(old_addr, new_addr) in moves {
        if let Some(v) = map.remove(&old_addr) {
            moving.push((new_addr, v));
        }
    }
    for (new_addr, v) in moving {
        map.insert(new_addr, v);
    }
}

/// Upper bound on retained former-mirror addresses (see
/// [`ThreadRegistry::former_mirror_addrs`]). Mirrors relocate rarely, and a
/// stale frame copy is only consultable until its slot is reclaimed/reused,
/// so a few thousand recent vacated addresses is ample; the FIFO bound keeps
/// the table from growing across a long-running process.
const FORMER_MIRROR_CAP: usize = 8192;

// A terminated Java thread's observable state lives in its mirror. Keeping a
// registry entry after its carrier has exited is therefore unnecessary: all
// registry-backed operations on a missing entry already have the terminated
// semantics (not alive, no interrupt target, join returns immediately). More
// importantly, every GC/STW census walks this map. Tomcat's non-blocking test
// repeatedly creates and tears down endpoint pools, and retaining every dead
// carrier turned those censuses into an ever-growing O(total threads ever
// created) cost. Keep a small diagnostic tail while bounding that cost.
const TERMINATED_THREAD_ENTRY_CAP: usize = 256;

/// Whether `entry` is a thread the stop-the-world machinery may count, freeze,
/// or read through: `alive`, AND its OS thread has not already finished.
///
/// gc-common 2026-09-23 (lane A). `alive` is cleared only by `mark_dead`,
/// which a carrier calls at the END of its closure (`finish_after` in
/// `vm_exec.rs`'s thread start). A carrier whose closure UNWINDS instead — a
/// Rust panic outside the per-method `catch_unwind` in `interpreter.rs`
/// (uncaught-exception dispatch, the deposit/teardown code, a panic inside a
/// panic handler) — never gets there. Its entry stays `alive` and
/// `stw_ready` for the rest of the process, so:
///
/// * every later census counts it in `expected`, and nothing can ever arrive
///   for it (it is not in compiled code either, so the takeover cannot excuse
///   it): every subsequent stop-the-world pause waits forever;
/// * `tlab_addr` / `jvm_thread_addr` still point into the `JvmThread` that was
///   a stack local of the unwound closure, and `collect_reserved_tlab_tails`
///   dereferences it — a use-after-free on every pause.
///
/// `JoinHandle::is_finished` is the one fact that is true of such a thread and
/// of no live one (the handle is stored by the parent right after spawn and
/// taken by `mark_dead`, so a normally-exiting thread never reaches this test
/// with a handle). It is consulted only by the GC-facing walks; Java-visible
/// liveness (`is_alive`, `Thread.isAlive`) is unchanged, and a later
/// `Thread.join` still marks the entry dead the ordinary way.
#[inline]
fn entry_is_stw_live(entry: &ThreadEntry) -> bool {
    if !entry.alive.load(Ordering::Acquire) {
        return false;
    }
    match entry.join_handle.as_ref() {
        Some(handle) if handle.is_finished() => {
            note_finished_but_alive(entry);
            false
        }
        _ => true,
    }
}

/// `CRATONVM_THREAD_DEAD_CLEARS_OS_TID` (JIT round 17 wave 2, lane smallfix):
/// does [`ThreadRegistry::mark_dead`] withdraw the entry's published OS tid?
/// A correctness fix, default ON; `=0` keeps the tid on the dead entry. Read
/// once per thread death (not a hot path), through the latched snapshot.
#[inline]
fn mark_dead_clears_os_tid() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THREAD_DEAD_CLEARS_OS_TID")
}

/// Report, once per process, a carrier that finished without `mark_dead`.
#[cold]
fn note_finished_but_alive(entry: &ThreadEntry) {
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if !REPORTED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "cratonvm::gc::guard",
            name = %entry.name,
            java_tid = entry.java_tid,
            "a Java thread's OS thread has finished but its registry entry is still \
             alive (it unwound past `mark_dead`); stop-the-world censuses now treat \
             it as dead instead of waiting for it forever",
        );
    }
}

impl ThreadRegistry {
    /// Create a new, empty registry. The next thread id will be 1
    /// (id 0 is reserved for the main thread).
    pub fn new() -> Self {
        static NEXT_REGISTRY_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            threads: RwLock::new(FxHashMap::default()),
            java_ended_thread: AtomicU64::new(u64::MAX),
            destroy_java_vm_listed: AtomicBool::new(false),
            dead_entries: std::sync::atomic::AtomicUsize::new(0),
            jmm_started: AtomicU64::new(0),
            jmm_live: std::sync::atomic::AtomicI32::new(0),
            jmm_peak: std::sync::atomic::AtomicI32::new(0),
            jmm_roster_live: std::sync::atomic::AtomicI32::new(0),
            jmm_roster_daemon: std::sync::atomic::AtomicI32::new(0),
            next_id: AtomicU64::new(1),
            registry_id: NEXT_REGISTRY_ID.fetch_add(1, Ordering::Relaxed),
            thread_obj_to_park: Mutex::new(FxHashMap::default()),
            java_tid_to_id: Mutex::new(FxHashMap::default()),
            former_mirror_addrs: Mutex::new((
                FxHashMap::default(),
                std::collections::VecDeque::new(),
            )),
            synchronizer_owner: std::array::from_fn(|_| Mutex::new(FxHashMap::default())),
            no_roots_lazy_claims_enabled: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_FFM_LAZY_CLAIM_NO_ROOTS_PAUSE",
            ),
            ffm_in_native_no_signal: ffm_in_native_no_signal_enabled(),
            blocked_in_native_runnable: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE",
            ),
            thread_info_frameless: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THREAD_INFO_FRAMELESS",
            ),
            thread_dump_one_pause: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THREAD_DUMP_ONE_PAUSE",
            ),
            threads_attach_order: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THREADS_ATTACH_ORDER",
            ),
            // JIT round 23 wave 6 (proxy-define page): default ON, `=0` kills;
            // compiled code now serves it at its helper-entry door
            // (`CRATONVM_GC_HANDSHAKE_AT_HELPER_ENTRY`), so it is taken in
            // microseconds on a peer that calls helpers, and the frame-trace
            // pause stays the fallback after the 1 ms budget.
            handshake_frame_trace: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GC_HANDSHAKE_FRAME_TRACE",
            ),
            handshake_loop_kick: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GC_HANDSHAKE_LOOP_KICK",
            ),
            handshake_spin_wait: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GC_HANDSHAKE_SPIN_WAIT",
            ),
            dbg_stw_expected_ids: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_DBG_STW_EXPECTED_IDS",
            )
            .is_some(),
            handshake_target_kick: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_GC_HANDSHAKE_TARGET_KICK",
            ),
            // WARNING t1-user-flip-20261026: default ON, `=0` kills.
            handshake_poll_bit: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GC_HANDSHAKE_POLL_BIT",
            ),
            // t1 lane E2: opt-in, performance.
            handshake_wait_sees_block: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_GC_HANDSHAKE_WAIT_SEES_BLOCK",
            ),
            // t1 wave 8 lane HANDSHK: opt-in, performance.
            handshake_unmounted_defers_kick: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_GC_HANDSHAKE_UNMOUNTED_DEFERS_KICK",
            ),
            os_tid_identity_skips_unmounted: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED",
            ),
            published_wait_kind: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PUBLISHED_WAIT_KIND",
            ),
            published_all_wait_kinds: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PUBLISHED_ALL_WAIT_KINDS",
            ),
            served_wait_kinds: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_SERVED_WAIT_KINDS",
            ),
            unrecorded_wait_census: UnrecordedWaitCensus::from_env(),
            vm_internal_waits_runnable: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VM_INTERNAL_WAITS_RUNNABLE",
            ),
            stand_in_wait_objects: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STAND_IN_WAIT_OBJECTS",
            ),
            park_lazy_budget: if cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_LAZY",
            ) {
                crate::threading::monitor::monitor_lazy_park_budget()
            } else {
                None
            },
            park_lazy_stw_wake: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PARK_LAZY_STW_WAKE",
            ),
            lazy_park_phases: AtomicU64::new(0),
            lazy_park_states: Mutex::new(Vec::new()),
            park_deposit_frozen_band: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PARK_DEPOSIT_FROZEN_BAND",
            ),
            park_defers_jit_chain:
                crate::jit::xt_root_scan::helper_window_deferred_chain_native_enabled(),
            unpark_one_lookup: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_UNPARK_ONE_LOOKUP",
            ),
            blocking_leave_keeps_snapshot: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT",
            ),
            blocking_leave_key_reuses_buffers: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS",
            ),
            park_lazy_suspend_aware: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PARK_LAZY_SUSPEND_AWARE",
            ),
            park_atomic_permit: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PARK_ATOMIC_PERMIT",
            ),
            park_lazy_spin: park_lazy_spin_from(
                cratonvm_types::flags::runtime_var("CRATONVM_PARK_LAZY_SPIN_NS")
                    .ok()
                    .as_deref(),
                std::thread::available_parallelism().map_or(1, |n| n.get()),
            ),
            park_lazy_adaptive: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_LAZY_ADAPTIVE",
            ),
            unpark_eetop: cratonvm_types::flags::runtime_flag_on("CRATONVM_UNPARK_EETOP"),
            interrupt_locks: std::array::from_fn(|_| Mutex::new(())),
            interrupt_lock_enabled: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERRUPT_LOCK",
            ),
            park_no_poll: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PARK_UNTIMED_NO_POLL",
            ),
            sleep_no_poll: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_SLEEP_NO_POLL"),
            park_wake_after_unlock: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PARK_WAKE_AFTER_UNLOCK",
            ),
            park_untimed_event: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PARK_UNTIMED_EVENT",
            ),
            park_event_signal_after_unlock: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK",
            ),
            park_event_lockless_wake: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PARK_EVENT_LOCKLESS_WAKE",
            ),
            sleep_high_resolution_interval: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL",
            ),
            interrupt_one_lookup: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERRUPT_ONE_LOOKUP",
            ),
            interrupt_one_wake: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERRUPT_ONE_WAKE",
            ),
            interrupt_field_memo: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERRUPT_FIELD_MEMO",
            ),
            dbg_interrupt: cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INTERRUPT")
                .is_some(),
            thread_start_one_publish: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_THREAD_START_ONE_PUBLISH",
            ),
            vt_parked_fold_filter: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GC_VT_PARKED_FOLD_FILTER",
            ),
            dbg_vt_parked_roots: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DBG_VT_PARKED_ROOTS",
            ),
            vt_parked_gather_filter: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GC_VT_PARKED_GATHER_FILTER",
            ),
            parked_split: Mutex::new(ParkedSplitGather::default()),
            vt_parked_mirror_keyed: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GC_VT_PARKED_MIRROR_KEYED",
            ),
            mirror_keyed: Mutex::new(MirrorKeyedGather::default()),
            g1_vt_precise_deposit: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_G1_VT_PRECISE_DEPOSIT",
            ),
        }
    }

    /// `CRATONVM_G1_VT_PRECISE_DEPOSIT` for this VM (gci i18/hw): one load of
    /// a field fixed at construction, once per blocking deposit.
    #[inline(always)]
    pub fn g1_vt_precise_deposit(&self) -> bool {
        self.g1_vt_precise_deposit
    }

    /// gci i13/vg: ask the NEXT [`Self::collect_all_root_snapshots`] for a
    /// split gather against `young` (the collector's young set for this
    /// pause, `VmHeap::parked_old_only_young_ranges`). The collection door
    /// arms it right before its gather and takes the old-only section right
    /// after ([`Self::take_parked_old_only`]); no other gather runs between
    /// the two (both are inside the door's pause, on its thread).
    ///
    /// gcj j3/x2 R10: the dedupe arm's owner-thread rule
    /// ([`Self::arm_initiator_snapshot_skip`]) for this arm too. It records the
    /// ARMING thread, and only a gather on that thread takes it; a plain
    /// gather on any other thread between the arm and the door's own is the
    /// whole gather, leaves the arm, and is counted
    /// ([`Self::parked_split_foreign_gathers`]). Before, such a gather took the
    /// split -- its old-only section diverted where nobody would read it --
    /// and the door's gather got the whole set. Serialised by the pause
    /// today, so the counter reads 0 on a healthy run.
    pub fn arm_parked_split_gather(&self, young: &[(usize, usize)]) {
        let mut filter = FoldAddrFilter::default();
        for &(lo, hi) in young {
            filter.insert_range(lo, hi);
        }
        let mut s = self.parked_split.lock();
        s.armed = Some((filter, std::thread::current().id()));
        s.old_only.clear();
    }

    /// gcj j3/x2 R10: plain gathers on a thread other than the arming one that
    /// left an armed parked split alone (see [`Self::arm_parked_split_gather`]).
    pub fn parked_split_foreign_gathers(&self) -> u64 {
        self.parked_split.lock().split_foreign_gathers
    }

    /// gci i13/vg: the old-only section of the last armed gather (empty when
    /// none was armed or nothing qualified), and disarm. Idempotent.
    pub fn take_parked_old_only(&self) -> Vec<ObjectRef> {
        let mut s = self.parked_split.lock();
        s.armed = None;
        std::mem::take(&mut s.old_only)
    }

    /// gci i21/pr (`CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`, opt-in; step 11 of
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// ask the NEXT [`Self::collect_all_root_snapshots`] to leave out the
    /// snapshot words of every alive entry that shares `initiator_snapshot`
    /// (`Arc::ptr_eq`) -- the collection initiator's own `root_snapshot`,
    /// whose words its `collect_roots` step 11 pushes in the same pause.
    ///
    /// Armed by the collection door (`run_collection_pause`) right before its
    /// `collect_roots_registry_appended` and consumed by the one gather that
    /// follows it, exactly like [`Self::arm_parked_split_gather`]; no other
    /// gather runs between the two (both inside the door's pause, on its
    /// thread). Only that door may arm it: every other gather (the concurrent
    /// cycles' marks, HPROF) has no step 11 in front of it.
    ///
    /// gcj j3/x2 (the flips page's item (i)): the arm records the ARMING
    /// thread, and only a gather on that thread consumes it. A gather on any
    /// other thread between the arm and the door's own (HPROF, another door)
    /// is the historical whole gather and leaves the arm for the door; it is
    /// counted ([`Self::initiator_skip_foreign_gathers`]). Before, the first
    /// gather on ANY thread took the arm and left the initiator's words --
    /// not duplicated in that gather's root set -- out of it.
    pub fn arm_initiator_snapshot_skip(&self, initiator_snapshot: &Arc<Mutex<Vec<ObjectRef>>>) {
        let mut s = self.parked_split.lock();
        s.initiator_skip = Some((Arc::clone(initiator_snapshot), std::thread::current().id()));
        s.initiator_skipped = None;
    }

    /// gcj j3/x2: how many plain gathers ran on a thread other than the
    /// arming one while an initiator-snapshot skip was armed (each left the
    /// arm alone). 0 on a healthy run: the door's pause serialises gathers.
    pub fn initiator_skip_foreign_gathers(&self) -> u64 {
        self.parked_split.lock().initiator_foreign_gathers
    }

    /// gci i21/pr: disarm [`Self::arm_initiator_snapshot_skip`] and return
    /// how many snapshot words the gather that consumed it left out (`None`:
    /// no gather consumed it). Idempotent.
    pub fn disarm_initiator_snapshot_skip(&self) -> Option<usize> {
        let mut s = self.parked_split.lock();
        s.initiator_skip = None;
        s.initiator_skipped.take()
    }

    /// `CRATONVM_GC_VT_PARKED_GATHER_FILTER` for this VM (gci i13/vg, the
    /// gather half of i12/vp's page): one load of a field fixed at construction, once per
    /// collection pause.
    #[inline(always)]
    pub fn vt_parked_gather_filter(&self) -> bool {
        self.vt_parked_gather_filter
    }

    /// Test hook: set [`Self::vt_parked_gather_filter`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_vt_parked_gather_filter_for_test(&mut self, on: bool) {
        self.vt_parked_gather_filter = on;
    }

    /// `CRATONVM_GC_VT_PARKED_MIRROR_KEYED` for this VM (gci i14/ve): one
    /// load of a field fixed at construction, once per collection pause.
    #[inline(always)]
    pub fn vt_parked_mirror_keyed(&self) -> bool {
        self.vt_parked_mirror_keyed
    }

    /// Test hook: set [`Self::vt_parked_mirror_keyed`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_vt_parked_mirror_keyed_for_test(&mut self, on: bool) {
        self.vt_parked_mirror_keyed = on;
    }

    /// gci i14/ve: ask the NEXT [`Self::collect_all_root_snapshots`] to key
    /// the eligible parked continuations by their mirrors
    /// ([`mirror_keyed_mirror`]). `vm` is this VM's identity (the
    /// `metadata_pin` rows are per VM), `[old_base, old_base + old_len)` the
    /// Generational old generation, and `candidates` the registry ids of the
    /// JDK continuations whose state is deposited in the virtual-thread
    /// manager right now (`VirtualThreadManager::deposited_jdk_continuation_ids`).
    /// The collection door arms it inside its pause, right before its one
    /// gather, and only when the pause is licensed
    /// (`gc_and_alloc::arm_parked_mirror_keyed_gather`); the guard it returns
    /// disarms on drop. Does not touch the deferred list.
    pub fn arm_mirror_keyed_gather(
        &self,
        vm: usize,
        old_base: usize,
        old_len: usize,
        candidates: Vec<u64>,
    ) {
        let candidates: rustc_hash::FxHashSet<u64> = candidates.into_iter().collect();
        self.mirror_keyed.lock().armed = Some(MirrorKeyedArm {
            vm,
            old_base,
            old_len,
            old_ranges: Vec::new(),
            candidates,
        });
    }

    /// gci i14/ve: drop an armed request no gather consumed (the door's
    /// guard). Idempotent; the deferred list is the sweep's.
    pub fn disarm_mirror_keyed_gather(&self) {
        self.mirror_keyed.lock().armed = None;
    }

    /// gci i14/ve: the door could not read the candidates without blocking
    /// (the virtual-thread manager's lock was held elsewhere), so this pause
    /// keys nothing and gathers as before. Counted for the census line's
    /// ` keyed_skipped_contended=`.
    pub fn note_mirror_keyed_contended(&self) {
        self.mirror_keyed.lock().skipped_contended += 1;
    }

    /// gci i14/ve: how many pauses [`Self::note_mirror_keyed_contended`]
    /// counted. Test-only since JIT round 22 wave 7 (lane review): the census
    /// line reads the field itself, nothing in the VM calls this.
    #[cfg(test)]
    pub fn mirror_keyed_skipped_contended(&self) -> u64 {
        self.mirror_keyed.lock().skipped_contended
    }

    /// gci i14/ve: the verdict half, inside the SAME pause as the gather that
    /// left entries out (`memory::gc::update_all_roots`, with the in-pause
    /// reclaimed-hole verdict). For every `(tid, mirror)` that gather keyed:
    /// a mirror the pause moved (`pointer_map` key) or kept in place
    /// (`survived`, asked of the PRE-remap address, as every address-keyed
    /// sweep there asks it) keeps its continuation; a mirror that did not
    /// survive was unreachable, so its frames -- `metadata_pin` values owned
    /// by it -- were not marked and may be freed already. That entry is
    /// retired HERE, before the blocked fold, the mirror remap and any later
    /// pause can read its words: TLAB / `JvmThread` addresses withdrawn and
    /// the entry marked dead (a dead entry is never gathered, folded or
    /// counted). Its execution state is dropped after the pause
    /// (`jdk_continuation::retire_collected_continuations`, from
    /// [`Self::take_collected_parked`]). An entry that is no longer the same
    /// unmounted thread of that mirror is left alone (cannot happen: a
    /// mount needs a carrier holding the mirror). Returns how many retired.
    pub fn sweep_mirror_keyed_parked(
        &self,
        pointer_map: &cratonvm_types::PointerMap,
        survived: &dyn Fn(usize) -> bool,
    ) -> usize {
        let deferred = std::mem::take(&mut self.mirror_keyed.lock().deferred);
        if deferred.is_empty() {
            return 0;
        }
        let dead = mirror_keyed_dead(&deferred, pointer_map, survived);
        let retired = self.retire_mirror_keyed_dead(&dead);
        if self.dbg_vt_parked_roots {
            eprintln!(
                "[vt-parked-roots] mirror-keyed deferred={} collected={}",
                deferred.len(),
                retired.len()
            );
        }
        let n = retired.len();
        self.mirror_keyed.lock().collected.extend(retired);
        n
    }

    /// gci i14/ve (split out by gci i15/vh, unchanged): retire every
    /// `(tid, mirror)` of `dead` that is still that unmounted, alive thread of
    /// that mirror -- TLAB address withdrawn, entry marked dead -- and return
    /// their ids. Any other entry is left alone with an error line (cannot
    /// happen: a mount needs a carrier holding the mirror).
    fn retire_mirror_keyed_dead(&self, dead: &[(ThreadId, usize)]) -> Vec<ThreadId> {
        let mut retired: Vec<ThreadId> = Vec::new();
        {
            let threads = self.threads.read();
            for &(tid, mirror) in dead {
                let Some(entry) = threads.get(&tid) else {
                    continue;
                };
                let same = entry.alive.load(Ordering::Acquire)
                    && !entry.mounted.load(Ordering::Acquire)
                    && entry.java_thread_obj.map(|m| m.as_ptr() as usize) == Some(mirror);
                if same {
                    retired.push(tid);
                } else {
                    tracing::error!(
                        "gci i14/ve: mirror-keyed thread {} lost its mirror {mirror:#x} but is \
                         no longer that parked thread; left alone",
                        tid.0
                    );
                }
            }
        }
        for &tid in &retired {
            self.clear_tlab_addr(tid);
            self.mark_dead(tid);
        }
        retired
    }

    /// gci i14/ve: the ids [`Self::sweep_mirror_keyed_parked`] retired, for
    /// the door to drop their execution states once the world is released.
    pub fn take_collected_parked(&self) -> Vec<ThreadId> {
        std::mem::take(&mut self.mirror_keyed.lock().collected)
    }

    /// gci i15/vh (G1): [`Self::arm_mirror_keyed_gather`] with the old space
    /// given as G1's `Old`-region ranges (`G1Collector::parked_mirror_keyed_view`)
    /// instead of one Generational span. Sorted here, so a caller's order does
    /// not matter. Armed only by the two G1 marking root scans (the initial
    /// mark and the remark, `gc_and_alloc::g1_key_parked_for_mark`).
    pub fn arm_mirror_keyed_gather_ranges(
        &self,
        vm: usize,
        mut old_ranges: Vec<(usize, usize)>,
        candidates: Vec<u64>,
    ) {
        old_ranges.retain(|&(lo, hi)| lo < hi);
        old_ranges.sort_unstable();
        let candidates: rustc_hash::FxHashSet<u64> = candidates.into_iter().collect();
        self.mirror_keyed.lock().armed = Some(MirrorKeyedArm {
            vm,
            old_base: 0,
            old_len: 0,
            old_ranges,
            candidates,
        });
    }

    /// gci i15/vh (G1): the mirrors of the pairs the last keyed gather left
    /// out and nobody judged yet, one per keyed thread, for the G1 marking
    /// door to take step 10b's copy of each out of its `collect_roots` result
    /// ([`strip_keyed_mirror_registry_roots`]).
    pub fn mirror_keyed_deferred_mirrors(&self) -> Vec<usize> {
        self.mirror_keyed
            .lock()
            .deferred
            .iter()
            .map(|&(_, mirror)| mirror)
            .collect()
    }

    /// gci i15/vh (G1): drop the deferred pairs and any judgement without a
    /// verdict -- the initial mark (its mark is not complete; only the remark
    /// judges), an unwinding or lost remark, and the door's guard. The rows
    /// stay (the cycle's marker follows them; the first scan after the cycle
    /// drops them). Never touches `collected`.
    pub fn forget_mirror_keyed_deferred(&self) {
        let mut keyed = self.mirror_keyed.lock();
        keyed.deferred.clear();
        keyed.judged_dead.clear();
        keyed.judged_deferred = 0;
    }

    /// gci i15/vh (G1, inside the remark callback, after the keep closure is
    /// marked and before cleanup frees anything): judge every pair the
    /// remark's keyed gather left out by the COMPLETED mark (`is_live` is
    /// `G1Collector::is_live_after_mark`: bitmap below TAMS, live above it).
    /// An unmarked mirror's pair is held for
    /// [`Self::settle_mirror_keyed_parked_after_mark`]; nothing is retired
    /// yet, because only cleanup says whether it trusted this mark. Consumes
    /// the deferred list. Returns how many were judged dead.
    ///
    /// First, when a G1 marking scan keyed anything this cycle, the values of
    /// every `metadata_pin` row whose owner `is_live` are handed to `mark`
    /// (the door's `G1Collector::resurrect_after_remark`: mark gray and drain
    /// to a fixed point), repeated until no further owner turns live
    /// ([`mark_live_row_values_to_fixpoint`]). The concurrent marker runs
    /// while a marking pause gathers, so a batch that captured the side
    /// tables before the gather filed a mirror's rows may have scanned that
    /// mirror without them (`MarkSideTables` re-captures only per batch): the
    /// frames of a LIVE keyed thread -- and an object the thread moved from a
    /// frame into the heap after the initial mark, still named by that
    /// mark's rows -- would then stay unmarked and cleanup would free them.
    /// The fixed point closes the rows over the completed mark instead of
    /// trusting the marker's timing; every value it marks is reachable (its
    /// owner is). A mirror reachable only from another live keyed thread's
    /// frames turns live inside it, before the verdict below.
    pub fn judge_mirror_keyed_parked_after_mark(
        &self,
        is_live: &dyn Fn(usize) -> bool,
        mark: &mut dyn FnMut(&[usize]),
    ) -> usize {
        let (deferred, cycle_keyed) = {
            let mut keyed = self.mirror_keyed.lock();
            (
                std::mem::take(&mut keyed.deferred),
                std::mem::take(&mut keyed.g1_cycle_keyed),
            )
        };
        if cycle_keyed {
            if let Some(rows) = cratonvm_types::metadata_pin::snapshot() {
                let _ = mark_live_row_values_to_fixpoint(&rows, is_live, mark);
            }
        }
        if deferred.is_empty() {
            return 0;
        }
        let dead = mirror_keyed_dead(&deferred, &cratonvm_types::PointerMap::default(), is_live);
        let n = dead.len();
        let mut keyed = self.mirror_keyed.lock();
        keyed.judged_deferred += deferred.len();
        keyed.judged_dead.extend(dead);
        n
    }

    /// gci i15/vh (G1, the remark pause, right after
    /// `VmHeap::g1_final_remark_and_cleanup` returned): retire the judged
    /// pairs when `trusted` (the cleanup trusted the mark that left their
    /// mirrors unmarked: the mirror is unreachable and its frames, rows owned
    /// by it, were left unmarked and may be in a freed region now), else
    /// keep them (an untrusted cleanup freed nothing and proved nothing; the
    /// entries are rooted again by every later gather). Either way the
    /// judgement and any unjudged deferred pair are consumed. Retired ids join
    /// `collected` for the door's post-pause release. Returns how many retired.
    pub fn settle_mirror_keyed_parked_after_mark(&self, trusted: bool) -> usize {
        let (dead, judged) = {
            let mut keyed = self.mirror_keyed.lock();
            keyed.deferred.clear();
            let judged = std::mem::take(&mut keyed.judged_deferred);
            (std::mem::take(&mut keyed.judged_dead), judged)
        };
        if judged == 0 && dead.is_empty() {
            return 0;
        }
        let retired = if trusted {
            self.retire_mirror_keyed_dead(&dead)
        } else {
            Vec::new()
        };
        if self.dbg_vt_parked_roots {
            eprintln!(
                "[vt-parked-roots] mirror-keyed deferred={judged} collected={} door=g1-remark \
                 trusted={trusted} unmarked={}",
                retired.len(),
                dead.len()
            );
        }
        let n = retired.len();
        self.mirror_keyed.lock().collected.extend(retired);
        n
    }

    /// `CRATONVM_GC_VT_PARKED_FOLD_FILTER` for this VM (gci i12/vp): one load
    /// of a field fixed at construction, once per relocating pause.
    #[inline(always)]
    pub fn vt_parked_fold_filter(&self) -> bool {
        self.vt_parked_fold_filter
    }

    /// Test hook: set [`Self::vt_parked_fold_filter`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_vt_parked_fold_filter_for_test(&mut self, on: bool) {
        self.vt_parked_fold_filter = on;
    }

    /// `CRATONVM_THREAD_START_ONE_PUBLISH` for this VM (interpreter round i3
    /// wave 82, lane L6): one load of a field fixed at construction, once per
    /// thread start.
    #[inline(always)]
    pub fn thread_start_one_publish(&self) -> bool {
        self.thread_start_one_publish
    }

    /// Test hook: set [`Self::thread_start_one_publish`] without the
    /// environment.
    #[cfg(test)]
    pub(crate) fn set_thread_start_one_publish_for_test(&mut self, on: bool) {
        self.thread_start_one_publish = on;
    }

    /// `CRATONVM_PARK_LAZY_STW_WAKE` for this VM (interpreter round i3 wave
    /// 72, lane L6): is a park's running phase woken by a pause initiator
    /// ([`Self::wake_lazy_park_phases`]) rather than polling in slices? One
    /// load of a field fixed at construction.
    #[inline(always)]
    pub fn park_lazy_stw_wake(&self) -> bool {
        self.park_lazy_stw_wake
    }

    /// `CRATONVM_PARK_LAZY_ADAPTIVE` for this VM (interpreter round i3 wave
    /// 81, lane L6). One load of a field fixed at construction, on the
    /// park's slow path only.
    #[inline(always)]
    pub fn park_lazy_adaptive(&self) -> bool {
        self.park_lazy_adaptive
    }

    /// Test hook: set [`Self::park_lazy_stw_wake`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_lazy_stw_wake_for_test(&mut self, on: bool) {
        self.park_lazy_stw_wake = on;
    }

    /// `CRATONVM_PARK_DEPOSIT_FROZEN_BAND` for this VM (interpreter round i3
    /// wave 72, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_deposit_frozen_band(&self) -> bool {
        self.park_deposit_frozen_band
    }

    /// gci i47w lane pr47: [`Self::park_defers_jit_chain`]'s field, one load
    /// of a value fixed at construction (the G1 test is the caller's).
    #[inline(always)]
    pub fn park_defers_jit_chain(&self) -> bool {
        self.park_defers_jit_chain
    }

    /// `CRATONVM_UNPARK_ONE_LOOKUP` for this VM (interpreter round i3 wave
    /// 72, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn unpark_one_lookup(&self) -> bool {
        self.unpark_one_lookup
    }

    /// `CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT` for this VM (interpreter
    /// round i4 wave 99, lane L1): one load of a field fixed at construction.
    #[inline(always)]
    pub fn blocking_leave_keeps_snapshot(&self) -> bool {
        self.blocking_leave_keeps_snapshot
    }

    /// `CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS` for this VM (interpreter
    /// round i5 wave 100, lane L1): one load of a field fixed at construction.
    #[inline(always)]
    pub fn blocking_leave_key_reuses_buffers(&self) -> bool {
        self.blocking_leave_key_reuses_buffers
    }

    /// `CRATONVM_PARK_LAZY_SUSPEND_AWARE` for this VM (interpreter round i3
    /// wave 72, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_lazy_suspend_aware(&self) -> bool {
        self.park_lazy_suspend_aware
    }

    /// `CRATONVM_PARK_ATOMIC_PERMIT` for this VM (interpreter round i3 wave
    /// 73, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_atomic_permit(&self) -> bool {
        self.park_atomic_permit
    }

    /// `CRATONVM_PARK_LAZY_SPIN_NS` for this VM (interpreter round i3 wave
    /// 79, lane L6): the permit spin before a park's running phase waits,
    /// `None` when off. One load of a field fixed at construction.
    #[inline(always)]
    pub fn park_lazy_spin(&self) -> Option<std::time::Duration> {
        self.park_lazy_spin
    }

    /// Test hook: set [`Self::park_lazy_spin`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_lazy_spin_for_test(&mut self, spin: Option<std::time::Duration>) {
        self.park_lazy_spin = spin;
    }

    /// Test hook: set [`Self::park_atomic_permit`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_atomic_permit_for_test(&mut self, on: bool) {
        self.park_atomic_permit = on;
    }

    /// `CRATONVM_UNPARK_EETOP` for this VM (interpreter round i3 wave 73,
    /// lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn unpark_eetop(&self) -> bool {
        self.unpark_eetop
    }

    /// `CRATONVM_PARK_UNTIMED_NO_POLL` for this VM (interpreter round i3 wave
    /// 74, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_no_poll(&self) -> bool {
        self.park_no_poll
    }

    /// Test hook: set [`Self::park_no_poll`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_no_poll_for_test(&mut self, on: bool) {
        self.park_no_poll = on;
    }

    /// `CRATONVM_SLEEP_NO_POLL` for this VM (interpreter round i3 wave 75,
    /// lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn sleep_no_poll(&self) -> bool {
        self.sleep_no_poll
    }

    /// Test hook: set [`Self::sleep_no_poll`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_sleep_no_poll_for_test(&mut self, on: bool) {
        self.sleep_no_poll = on;
    }

    /// `CRATONVM_PARK_WAKE_AFTER_UNLOCK` for this VM (interpreter round i3
    /// wave 76, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_wake_after_unlock(&self) -> bool {
        self.park_wake_after_unlock
    }

    /// `CRATONVM_PARK_UNTIMED_EVENT` for this VM (interpreter round i5 wave
    /// 107, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_untimed_event(&self) -> bool {
        self.park_untimed_event
    }

    /// `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK` for this VM (interpreter
    /// round i5 wave 108, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_event_signal_after_unlock(&self) -> bool {
        self.park_event_signal_after_unlock
    }

    /// `CRATONVM_PARK_EVENT_LOCKLESS_WAKE` for this VM (interpreter round i5
    /// wave 114, lane L1): one load of a field fixed at construction.
    #[inline(always)]
    pub fn park_event_lockless_wake(&self) -> bool {
        self.park_event_lockless_wake
    }

    /// `CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL` for this VM (interpreter
    /// round i5 wave 109, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn sleep_high_resolution_interval(&self) -> bool {
        self.sleep_high_resolution_interval
    }

    /// How this VM's `LockSupport.unpark` and `Thread.interrupt` set a
    /// permit and wake (interpreter round i3 wave 77, lane L6;
    /// `ParkState::unpark_with`): `CRATONVM_PARK_ATOMIC_PERMIT`,
    /// `CRATONVM_PARK_UNTIMED_NO_POLL` and `CRATONVM_PARK_WAKE_AFTER_UNLOCK`,
    /// three loads of fields fixed at construction (read on each call, so a
    /// test hook's change is seen).
    #[inline(always)]
    pub fn unpark_shape(&self) -> crate::threading::jvm_thread::UnparkShape {
        crate::threading::jvm_thread::UnparkShape {
            lock_free: self.park_atomic_permit,
            wake_all: self.park_no_poll,
            after_unlock: self.park_wake_after_unlock,
        }
    }

    /// Test hook: set [`Self::park_wake_after_unlock`] without the
    /// environment.
    #[cfg(test)]
    pub(crate) fn set_park_wake_after_unlock_for_test(&mut self, on: bool) {
        self.park_wake_after_unlock = on;
    }

    /// `CRATONVM_INTERRUPT_ONE_LOOKUP` for this VM (interpreter round i3 wave
    /// 76, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn interrupt_one_lookup(&self) -> bool {
        self.interrupt_one_lookup
    }

    /// Test hook: set [`Self::interrupt_one_lookup`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_interrupt_one_lookup_for_test(&mut self, on: bool) {
        self.interrupt_one_lookup = on;
    }

    /// `CRATONVM_INTERRUPT_ONE_WAKE` for this VM (interpreter round i4 wave
    /// 91, lane L6): `Thread.interrupt`'s one locked wake of its target's
    /// park state. One load of a field fixed at construction.
    #[inline(always)]
    pub fn interrupt_one_wake(&self) -> bool {
        self.interrupt_one_wake
    }

    /// `CRATONVM_INTERRUPT_FIELD_MEMO` for this VM (interpreter round i3 wave
    /// 77, lane L6): one load of a field fixed at construction.
    #[inline(always)]
    pub fn interrupt_field_memo(&self) -> bool {
        self.interrupt_field_memo
    }

    /// Test hook: set [`Self::interrupt_field_memo`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_interrupt_field_memo_for_test(&mut self, on: bool) {
        self.interrupt_field_memo = on;
    }

    /// `CRATONVM_DBG_INTERRUPT` as read once at construction (interpreter
    /// round i3 wave 77, lane L6); `Thread.interrupt` asks it in place of the
    /// environment only with [`Self::interrupt_field_memo`] on.
    #[inline(always)]
    pub fn dbg_interrupt(&self) -> bool {
        self.dbg_interrupt
    }

    /// Run `phase` -- the running phase of a park on `park_state`
    /// (`ParkState::park_lazily_sliced`, whose `keep_running` reads "no pause
    /// is requested" under the state's mutex) -- counted in
    /// [`Self::lazy_park_phases`] and listed in [`Self::lazy_park_states`],
    /// so [`Self::wake_lazy_park_phases`] can wake it (interpreter round i3
    /// wave 72, lane L6, `CRATONVM_PARK_LAZY_STW_WAKE`). Off, this is
    /// `phase()`.
    ///
    /// The handshake is `MonitorTable::lazily_parked_on`'s: the state is
    /// listed BEFORE the count is raised (a `SeqCst` RMW, then a `SeqCst`
    /// fence) and unlisted AFTER it is lowered, and the phase reads
    /// `stw_requested` only after that fence. An initiator raises
    /// `stw_requested`, fences, then reads the count: either the parker sees
    /// the request and leaves, or the initiator sees the count, takes the
    /// list lock (so it sees this entry) and wakes the state under its mutex,
    /// which the parker holds from its read until it is inside the wait.
    pub(crate) fn lazily_parked<R>(
        &self,
        park_state: &Arc<ParkState>,
        phase: impl FnOnce() -> R,
    ) -> R {
        if !self.park_lazy_stw_wake {
            return phase();
        }
        struct Leave<'a> {
            count: &'a AtomicU64,
            list: &'a Mutex<Vec<Arc<ParkState>>>,
            state: *const ParkState,
        }
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.count.fetch_sub(1, Ordering::SeqCst);
                let mut list = self.list.lock();
                if let Some(at) = list
                    .iter()
                    .position(|s| std::ptr::eq(Arc::as_ptr(s), self.state))
                {
                    list.swap_remove(at);
                }
            }
        }
        self.lazy_park_states.lock().push(Arc::clone(park_state));
        self.lazy_park_phases.fetch_add(1, Ordering::SeqCst);
        let _leave = Leave {
            count: &self.lazy_park_phases,
            list: &self.lazy_park_states,
            state: Arc::as_ptr(park_state),
        };
        std::sync::atomic::fence(Ordering::SeqCst);
        phase()
    }

    /// Wake every thread in a park's running phase, for a stop-the-world
    /// INITIATOR right after it raised `stw_requested` (interpreter round i3
    /// wave 72, lane L6, `CRATONVM_PARK_LAZY_STW_WAKE`; beside
    /// `MonitorTable::wake_lazy_parkers` at both `request_stw_opening_cycle`
    /// winners). Each woken parker re-reads `stw_requested`, leaves the
    /// phase and takes the blocking park, whose GC-blocked entry arrives at
    /// the pause. Off: one load. On with nobody in the phase: a fence and a
    /// load. The list lock is dropped before any state's mutex is taken.
    pub fn wake_lazy_park_phases(&self) {
        if !self.park_lazy_stw_wake {
            return;
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.lazy_park_phases.load(Ordering::SeqCst) == 0 {
            return;
        }
        let parked: Vec<Arc<ParkState>> = self.lazy_park_states.lock().clone();
        for state in parked {
            state.wake_lazy_phase();
        }
    }

    /// How many park phases are listed for [`Self::wake_lazy_park_phases`].
    /// Tests.
    #[cfg(test)]
    fn lazy_park_state_count(&self) -> usize {
        self.lazy_park_states.lock().len()
    }

    /// `CRATONVM_PARK_LAZY` for this VM (interpreter round i3 wave 71, lane
    /// L1): the running phase a platform thread's `Unsafe.park` takes before
    /// its blocking prologue (`NativeContextImpl::park_lazily` in `vm_exec`),
    /// `None` when off. One load of a field fixed at construction.
    #[inline(always)]
    pub fn park_lazy_budget(&self) -> Option<std::time::Duration> {
        self.park_lazy_budget
    }

    /// Test hook: set [`Self::park_lazy_budget`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_park_lazy_budget_for_test(&mut self, budget: Option<std::time::Duration>) {
        self.park_lazy_budget = budget;
    }

    /// nio21 wave 5 L1: the per-thread interrupt lock guarding `flag` (the
    /// `Arc<AtomicBool>` a `JvmThread` and its registry entry share), or
    /// `None` under `CRATONVM_INTERRUPT_LOCK=0`. Hold the guard across the
    /// flag store and the `Thread.interrupted` field store, and nothing else:
    /// resolve the field slot before taking it. See
    /// [`Self::interrupt_locks`].
    #[inline]
    pub fn lock_interrupt(&self, flag: &AtomicBool) -> Option<parking_lot::MutexGuard<'_, ()>> {
        if !self.interrupt_lock_enabled {
            return None;
        }
        Some(self.interrupt_locks[interrupt_lock_stripe(flag)].lock())
    }

    /// Test hook: set `CRATONVM_INTERRUPT_LOCK` for this registry without the
    /// environment.
    #[cfg(test)]
    pub(crate) fn set_interrupt_lock_enabled_for_test(&mut self, on: bool) {
        self.interrupt_lock_enabled = on;
    }

    /// `CRATONVM_STAND_IN_WAIT_OBJECTS` for this VM (interpreter round i2
    /// wave 68, lane L1): does a stand-in's blocking region publish the
    /// object its JDK code waits or parks on? One load of a field fixed at
    /// construction.
    #[inline(always)]
    pub fn stand_in_wait_objects(&self) -> bool {
        self.stand_in_wait_objects
    }

    /// Test hook: set [`Self::stand_in_wait_objects`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_stand_in_wait_objects_for_test(&mut self, on: bool) {
        self.stand_in_wait_objects = on;
    }

    /// `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE` for this VM (interpreter round
    /// i2 wave 66, lane L1): does a wait HotSpot performs inside the VM read
    /// RUNNABLE (`NativeContext::begin_vm_wait_region`)? One load of a field
    /// fixed at construction.
    #[inline(always)]
    pub fn vm_internal_waits_runnable(&self) -> bool {
        self.vm_internal_waits_runnable
    }

    /// Is `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` counting for this VM
    /// (interpreter round i2 wave 66, lane L1)? One load of a field fixed at
    /// construction; the kind-less doors ask it before anything else.
    #[inline(always)]
    pub fn unrecorded_wait_census_on(&self) -> bool {
        self.unrecorded_wait_census.mode != UnrecordedWaitMode::Off
    }

    /// The census mode (`CRATONVM_DBG_UNRECORDED_WAIT_KINDS`).
    pub fn unrecorded_wait_mode(&self) -> UnrecordedWaitMode {
        self.unrecorded_wait_census.mode
    }

    /// Test hook: set the census mode without the environment.
    #[cfg(test)]
    pub(crate) fn set_unrecorded_wait_mode_for_test(&mut self, mode: UnrecordedWaitMode) {
        self.unrecorded_wait_census.mode = mode;
    }

    /// Count one blocking region that opened with no recorded wait kind, at
    /// `site` (the caller of the door), through `door` (interpreter round i2
    /// wave 66, lane L1, `CRATONVM_DBG_UNRECORDED_WAIT_KINDS`;
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// Under `trace` / `assert` the first region of a site prints one
    /// `[cratonvm] unrecorded-wait-kind:` line; under `assert` a debug build
    /// then fails. Nothing when the census is off. Called by the door before
    /// it deposits, while the thread is still a running mutator; takes only
    /// the census's own mutex.
    #[cold]
    #[inline(never)]
    pub fn note_unrecorded_wait_region(
        &self,
        site: &'static std::panic::Location<'static>,
        door: &'static str,
    ) {
        let mode = self.unrecorded_wait_census.mode;
        if mode == UnrecordedWaitMode::Off {
            return;
        }
        let first = {
            let mut sites = self.unrecorded_wait_census.sites.lock();
            let count = sites
                .entry((site.file(), site.line(), site.column(), door))
                .or_insert(0);
            *count = count.saturating_add(1);
            *count == 1
        };
        if first && matches!(mode, UnrecordedWaitMode::Trace | UnrecordedWaitMode::Assert) {
            eprintln!(
                "[cratonvm] unrecorded-wait-kind: {door} at {}:{}:{}",
                site.file(),
                site.line(),
                site.column()
            );
        }
        if mode == UnrecordedWaitMode::Assert {
            debug_assert!(
                false,
                "CRATONVM_DBG_UNRECORDED_WAIT_KINDS=assert: a blocking region opened with no \
                 recorded wait kind ({door} at {}:{}:{}); open it with \
                 begin_blocking_region_as and the BlockKind of the JDK code it stands for",
                site.file(),
                site.line(),
                site.column()
            );
        }
    }

    /// The census table (interpreter round i2 wave 66, lane L1,
    /// `CRATONVM_DBG_UNRECORDED_WAIT_KINDS`): one row per call site and door,
    /// most regions first, then by site. Empty when the census is off or
    /// nothing was counted.
    pub fn unrecorded_wait_sites(&self) -> Vec<UnrecordedWaitSite> {
        let mut rows: Vec<UnrecordedWaitSite> = self
            .unrecorded_wait_census
            .sites
            .lock()
            .iter()
            .map(|(&(file, line, column, door), &count)| UnrecordedWaitSite {
                site: format!("{file}:{line}:{column}"),
                door,
                count,
            })
            .collect();
        rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.site.cmp(&b.site)));
        rows
    }

    /// Print the census table to stderr (interpreter round i2 wave 66, lane
    /// L1): a `[cratonvm] unrecorded-wait-kinds: R regions at S sites` header,
    /// then one `[cratonvm] unrecorded-wait-kinds:   <count> <door> <site>`
    /// line per row. Nothing when the census is off; the header alone (with
    /// zeros) when it counted nothing, so a clean run says so. Called by the
    /// launcher's shutdown reports (`cratonvm_vm::report_unrecorded_wait_regions_at_exit`).
    pub fn report_unrecorded_wait_regions(&self) {
        if !self.unrecorded_wait_census_on() {
            return;
        }
        let rows = self.unrecorded_wait_sites();
        let regions: u64 = rows.iter().map(|r| r.count).sum();
        eprintln!(
            "[cratonvm] unrecorded-wait-kinds: {regions} regions at {} sites",
            rows.len()
        );
        for row in &rows {
            eprintln!(
                "[cratonvm] unrecorded-wait-kinds:   {} {} {}",
                row.count, row.door, row.site
            );
        }
    }

    /// `CRATONVM_SERVED_WAIT_KINDS` for this VM (interpreter round i2 wave 65,
    /// lane L1): does a served stand-in's `Object.wait` that stands for JDK
    /// code that parks (`NativeContext::monitor_wait_as`) report the JDK
    /// body's state and publish `jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED`?
    /// One load of a field fixed at construction.
    #[inline(always)]
    pub fn served_wait_kinds(&self) -> bool {
        self.served_wait_kinds
    }

    /// Test hook: set [`Self::served_wait_kinds`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_served_wait_kinds_for_test(&mut self, on: bool) {
        self.served_wait_kinds = on;
    }

    /// Is live thread `thread_id` inside a served stand-in's `Object.wait`
    /// that stands for JDK code that parks, as the wait published it
    /// (`jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED`; interpreter round i2
    /// wave 65, lane L1, `CRATONVM_SERVED_WAIT_KINDS`)? JVMTI's kind bit
    /// (`jvmti::native_env::wait_kind`) asks it ahead of the JMX waiting
    /// monitor, which such a wait also publishes (an interrupt wakes the
    /// waiter through it) but which would read `IN_OBJECT_WAIT`. `false`
    /// without the switch, for an unknown or dead thread, and for any other
    /// wait.
    pub fn published_monitor_parked(&self, thread_id: ThreadId) -> bool {
        if !self.served_wait_kinds {
            return false;
        }
        self.gc_block_state_of(thread_id).is_some_and(|block| {
            block.java_wait_kind.load(Ordering::Acquire)
                == crate::threading::jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED
        })
    }

    /// `CRATONVM_PUBLISHED_WAIT_KIND` for this VM (interpreter round i2 wave
    /// 63, lane L1): does a `BlockKind::Sleeping` region publish its wait
    /// kind, and do the JVMTI / JDWP readers trust it
    /// ([`Self::published_sleeping`])? One load of a field fixed at
    /// construction. Also on with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`
    /// ([`Self::published_all_wait_kinds`], wave 64), which publishes the
    /// sleep with every other kind.
    #[inline(always)]
    pub fn published_wait_kind(&self) -> bool {
        self.published_wait_kind || self.published_all_wait_kinds
    }

    /// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` for this VM (interpreter round i2
    /// wave 64, lane L1): does every wait publish its kind (a park, an
    /// `Object.wait`, the served `Thread.join`), and do the readers trust the
    /// word ([`Self::published_wait_kind_of`], the lazy-wait reading of
    /// [`Self::java_block_state`])? One load of a field fixed at
    /// construction.
    #[inline(always)]
    pub fn published_all_wait_kinds(&self) -> bool {
        self.published_all_wait_kinds
    }

    /// Test hook: set [`Self::published_all_wait_kinds`] without the
    /// environment.
    #[cfg(test)]
    pub(crate) fn set_published_all_wait_kinds_for_test(&mut self, on: bool) {
        self.published_all_wait_kinds = on;
    }

    /// Test hook: set [`Self::published_wait_kind`] without the environment.
    #[cfg(test)]
    pub(crate) fn set_published_wait_kind_for_test(&mut self, on: bool) {
        self.published_wait_kind = on;
    }

    /// `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE` for this VM (interpreter round i2
    /// wave 59, lane L6): does a `BlockKind::InNative` region report HotSpot's
    /// `_thread_in_native` (RUNNABLE, in native, the native on top of the
    /// stack) rather than WAITING? One load of a field fixed at construction.
    #[inline(always)]
    pub fn blocked_in_native_runnable(&self) -> bool {
        self.blocked_in_native_runnable
    }

    /// Whether no-roots pauses claim lazy FFM downcalls for this VM. Fixed at
    /// registry construction so every downcall return can use one plain load.
    #[inline(always)]
    pub(crate) fn no_roots_lazy_claims_enabled(&self) -> bool {
        self.no_roots_lazy_claims_enabled
    }

    /// `CRATONVM_THREAD_INFO_FRAMELESS` for this VM (interpreter round i2
    /// wave 61, lane L6): does a `ThreadInfo` asked without frames skip the
    /// target's stack capture (and so its frame-trace pause)? One load of a
    /// field fixed at construction.
    #[inline(always)]
    pub fn thread_info_frameless(&self) -> bool {
        self.thread_info_frameless
    }

    /// `CRATONVM_THREAD_DUMP_ONE_PAUSE` for this VM (interpreter round i2
    /// wave 61, lane L6): does a dump of many threads' stacks
    /// (`NativeContext::begin_thread_dump`) take one frame-trace pause for
    /// all its running targets? One load of a field fixed at construction.
    #[inline(always)]
    pub fn thread_dump_one_pause(&self) -> bool {
        self.thread_dump_one_pause
    }

    /// `CRATONVM_THREADS_ATTACH_ORDER` for this VM (interpreter round i6
    /// wave 133, lane L7; HotSpot parity, default on, `=0` off): does
    /// `NativeContext::enumerate_threads` (behind `Thread.getThreads()`,
    /// `ThreadImpl.getThreads()` and `dumpAllThreads`) list the live threads
    /// in attach order ([`Self::alive_thread_objects_in_id_order`]), as
    /// HotSpot's threads list does, instead of the map's hash order? One load
    /// of a field fixed at construction.
    #[inline(always)]
    pub fn threads_attach_order(&self) -> bool {
        self.threads_attach_order
    }

    /// Allocate the next unique `ThreadId`.
    pub fn next_thread_id(&self) -> ThreadId {
        ThreadId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Register a thread (called before or at spawn time).
    pub fn register(&self, thread_id: ThreadId, name: &str, java_thread_obj: Option<ObjectRef>) {
        self.register_with_daemon(thread_id, name, java_thread_obj, false);
    }

    /// T19.K1 — Register a thread with an explicit daemon flag.
    ///
    /// `daemon == true` marks the thread as a JVM "daemon": the CLI's
    /// non-daemon-wait loop ignores it, so the process can exit while
    /// this thread is still running. `daemon == false` (the default)
    /// marks it as a "user" thread that the VM must wait for at
    /// shutdown.
    pub fn register_with_daemon(
        &self,
        thread_id: ThreadId,
        name: &str,
        java_thread_obj: Option<ObjectRef>,
        daemon: bool,
    ) {
        self.register_with_daemon_stw_ready(thread_id, name, java_thread_obj, daemon, true, false);
    }

    /// Register a thread that is alive but not yet able to answer STW barriers.
    pub fn register_starting_with_daemon(
        &self,
        thread_id: ThreadId,
        name: &str,
        java_thread_obj: Option<ObjectRef>,
        daemon: bool,
    ) {
        self.register_with_daemon_stw_ready(thread_id, name, java_thread_obj, daemon, false, false);
    }

    /// zt t15/pl6: [`Self::register_starting_with_daemon`] for a virtual
    /// thread (always a daemon), marked as one for good, so the JMM roster
    /// ([`Self::jmm_perf_thread_counts`]) never lists it, as `ThreadMXBean`
    /// never lists a virtual thread.
    pub fn register_starting_virtual(&self, thread_id: ThreadId, name: &str, vthread: ObjectRef) {
        self.register_with_daemon_stw_ready(thread_id, name, Some(vthread), true, false, true);
    }

    fn register_with_daemon_stw_ready(
        &self,
        thread_id: ThreadId,
        name: &str,
        java_thread_obj: Option<ObjectRef>,
        daemon: bool,
        stw_ready: bool,
        virtual_thread: bool,
    ) {
        let park_state = Arc::new(ParkState::new());
        if self.park_atomic_permit {
            park_state.enable_lock_free();
        }
        let entry = ThreadEntry {
            name: name.to_string(),
            join_handle: None,
            java_thread_obj,
            java_tid: 0,
            alive: AtomicBool::new(true),
            stw_ready: AtomicBool::new(stw_ready),
            daemon: AtomicBool::new(daemon),
            park_state: park_state.clone(),
            interrupted: Arc::new(AtomicBool::new(false)),
            root_snapshot: Arc::new(Mutex::new(Vec::new())),
            frame_trace: Arc::new(Mutex::new(Vec::new())),
            vm_state: Arc::new(Mutex::new(String::new())),
            gc_block_state: Arc::new(GcBlockState::new()),
            jmx_monitors: Arc::new(JmxMonitorBook::default()),
            jmx_waiting_monitor: Mutex::new(None),
            jmx_locked_synchronizers: Arc::new(Mutex::new(Vec::new())),
            jmx_wait_started: Mutex::new(None),
            jmx_waited_count: AtomicU64::new(0),
            jmx_waited_nanos: AtomicU64::new(0),
            tlab_addr: std::sync::atomic::AtomicUsize::new(0),
            jvm_thread_addr: std::sync::atomic::AtomicUsize::new(0),
            os_tid: std::sync::atomic::AtomicU32::new(0),
            os_tid_seq: AtomicU64::new(0),
            mounted: AtomicBool::new(true),
            mount_epoch: AtomicU64::new(0),
            parked_filter: ParkedFoldFilter::new(),
            registered_at: Instant::now(),
            unstructured_monitor_enters: std::sync::atomic::AtomicU32::new(0),
            jmm_counted: AtomicBool::new(false),
            virtual_thread,
        };
        self.insert_entry(thread_id, entry);
        if let Some(obj) = java_thread_obj {
            self.thread_obj_to_park
                .lock()
                .insert(obj.as_ptr() as usize, park_state);
        }
    }

    /// BUG-03 — publish the address of this thread's `JvmThread`'s
    /// [`cratonvm_gc::Tlab`]. Called once by the thread itself just after it
    /// starts running (the TLAB lives in the `JvmThread`, whose address is
    /// stable for the thread's life). No-op for an unknown id.
    pub fn set_tlab_addr(&self, thread_id: ThreadId, tlab_addr: usize) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.tlab_addr.store(tlab_addr, Ordering::Release);
        }
    }

    /// zt t26/zc5b item 1: the `[tlab, tlab + 24)` span -- the `cursor`, `end`
    /// and `start` words (`cratonvm_gc::Tlab`'s JIT-contract offsets 0, 8 and
    /// 16) -- of every thread whose TLAB address is published. These words
    /// live in the thread's `JvmThread`, i.e. on its own stack, and are never
    /// Java references; the take-over and helper-window band scans skip them
    /// (`xt_root_scan::is_tlab_pointer_slot`). Unsorted; one registry read
    /// lock. A thread that publishes after this call simply has no span this
    /// pause (its words are probed, as before).
    pub fn tlab_pointer_slots(&self) -> Vec<(usize, usize)> {
        const TLAB_POINTER_WORDS: usize = 3;
        let threads = self.threads.read();
        threads
            .values()
            .map(|entry| entry.tlab_addr.load(Ordering::Acquire))
            .filter(|&addr| addr != 0)
            .map(|addr| (addr, addr + TLAB_POINTER_WORDS * 8))
            .collect()
    }

    /// BUG-03 — clear this thread's published TLAB address. MUST be called by
    /// the thread (or its teardown) before its `JvmThread` is dropped so the
    /// collector never dereferences a dangling TLAB pointer.
    pub fn clear_tlab_addr(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.tlab_addr.store(0, Ordering::Release);
            // XT-FRAME-SCAN: the JvmThread address shares the TLAB address's
            // exact lifecycle (published once the JvmThread's address is
            // final, must stop being visible before it drops) — clear both
            // under the same lock hold so no reader can observe a cleared
            // TLAB with a still-published thread address or vice versa.
            entry.jvm_thread_addr.store(0, Ordering::Release);
        }
    }

    /// XT-FRAME-SCAN: publish the owning thread's `JvmThread` address so the
    /// cross-thread STW takeover can walk a frozen peer's interpreter frames
    /// (locals + operand stacks) directly. Same publication discipline as
    /// [`Self::set_tlab_addr`]: called once after the `JvmThread`'s address
    /// is final (worker spawn body / primordial boot / JNI attach); cleared
    /// by [`Self::clear_tlab_addr`] before the `JvmThread` drops. No-op for
    /// an unknown id.
    pub fn set_jvm_thread_addr(&self, thread_id: ThreadId, addr: usize) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.jvm_thread_addr.store(addr, Ordering::Release);
        }
    }

    /// gen r5w2/roots6: record whether `thread_id`'s Java execution is on a
    /// machine stack (see `ThreadEntry::mounted`). A virtual thread calls it
    /// with `false` when its continuation yields and `true` at every mount;
    /// nothing else needs to (entries start mounted). No-op for an unknown id.
    pub fn set_mounted(&self, thread_id: ThreadId, mounted: bool) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            // gci i12/vp: retire the parked fold filter's epoch BEFORE the bit
            // moves (and so before a remount's drain or the owner's next
            // deposit can change a word the filter summarises).
            entry.mount_epoch.fetch_add(1, Ordering::AcqRel);
            entry.mounted.store(mounted, Ordering::Release);
        }
    }

    /// Is `thread_id` mounted (see [`Self::set_mounted`])? `false` for an
    /// unknown id.
    pub fn is_mounted(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .is_some_and(|e| e.mounted.load(Ordering::Acquire))
    }

    /// XT-FRAME-SCAN: the published `JvmThread` addresses of every alive
    /// thread whose OS tid is in `os_tids` (the takeover's frozen-peer set).
    ///
    /// SAFETY contract for the caller's deref: entries must only be
    /// dereferenced for peers that are OS-frozen by the takeover (suspended
    /// with `Rip` inside registered compiled code) and stay frozen until
    /// after the deref ends — compiled code never mutates the `JvmThread`'s
    /// interpreter state, and a frozen thread cannot exit (so `alive` cannot
    /// flip nor the `JvmThread` drop while frozen).
    pub fn frozen_peer_thread_addrs(&self, os_tids: &[u32]) -> Vec<usize> {
        if os_tids.is_empty() {
            return Vec::new();
        }
        // gcf f1/r: membership by binary search over a sorted copy, not a
        // linear `contains` per registry entry (`O(threads * frozen)` inside
        // the pause). Same answer.
        let mut os_tids = os_tids.to_vec();
        os_tids.sort_unstable();
        let threads = self.threads.read();
        let mut out = Vec::new();
        for entry in threads.values() {
            // gen r5w2/roots6: an unmounted virtual thread is not frozen on
            // its last carrier (its frames are on the heap and that carrier
            // may be mounting it right now), and a dropped `JvmThread` must
            // not be read at all.
            if !entry_is_stw_live(entry)
                || !entry.mounted.load(Ordering::Acquire)
                || entry.gc_block_state.owner_dropped()
            {
                continue;
            }
            let tid = entry.os_tid.load(Ordering::Acquire);
            if tid == 0 || os_tids.binary_search(&tid).is_err() {
                continue;
            }
            let addr = entry.jvm_thread_addr.load(Ordering::Acquire);
            if addr != 0 {
                out.push(addr);
            }
        }
        out
    }

    /// Peers whose precise deposits this pause holds until rewrite ends.
    pub(crate) fn precise_lazy_claimed_thread_addrs(&self) -> Vec<usize> {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| {
                entry_is_stw_live(e)
                    && e.mounted.load(Ordering::Acquire)
                    && !e.gc_block_state.owner_dropped()
                    && matches!(
                        e.gc_block_state.lazy_native.load(Ordering::Acquire),
                        crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED
                            | crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED_LEAVING
                    )
                    && e.gc_block_state
                        .lazy_native_precise_claim
                        .load(Ordering::Acquire)
            })
            .map(|e| e.jvm_thread_addr.load(Ordering::Acquire))
            .filter(|a| *a != 0)
            .collect()
    }

    /// Claim every live lazy FFM downcall peer whose OS thread is not already
    /// frozen or claimed by this take-over. The claim holds its Java frames
    /// stable so collection roots or frame traces can be published on its behalf.
    /// Number of threads still in a lazy FFM downcall. No-roots pauses that
    /// cannot claim must wait for these peers to return before walking the heap.
    pub(crate) fn lazy_native_population_count(&self) -> usize {
        self.threads
            .read()
            .values()
            .filter(|entry| {
                entry.gc_block_state.lazy_native.load(Ordering::Acquire)
                    == crate::threading::jvm_thread::LAZY_NATIVE_IN
            })
            .count()
    }

    pub(crate) fn claim_lazy_native_peers(&self, already: &[u32]) -> Vec<LazyNativeClaim> {
        let threads = self.threads.read();
        let mut out: Vec<LazyNativeClaim> = Vec::new();
        for entry in threads.values() {
            let block = &entry.gc_block_state;
            if block.lazy_native.load(Ordering::Acquire)
                != crate::threading::jvm_thread::LAZY_NATIVE_IN
            {
                continue;
            }
            if !entry_is_stw_live(entry)
                || !entry.mounted.load(Ordering::Acquire)
                || block.owner_dropped()
                || entry.jvm_thread_addr.load(Ordering::Acquire) == 0
            {
                continue;
            }
            let os_tid = entry.os_tid.load(Ordering::Acquire);
            if os_tid == 0 || already.contains(&os_tid) || out.iter().any(|c| c.os_tid == os_tid) {
                continue;
            }
            if let Some((lo, hi)) = block.try_claim_lazy_native() {
                out.push(LazyNativeClaim {
                    os_tid,
                    lo,
                    hi,
                    state: Arc::clone(block),
                });
            }
        }
        out
    }

    /// gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`,
    /// `gcg-g4jn-proposal-in-native-band-scanned-at-the-pause-DONE-20261015`): the
    /// pause's half of a JNI in-native thread's compiled band. For every alive
    /// entry whose thread is in a blocked region with a band recorded
    /// ([`in_native_band_of`]), push every word of
    /// `[lo, hi)` that `probe` accepts onto `out`. Each band is read once (two
    /// entries on one band -- never expected -- are read once).
    ///
    /// Only inside a pause: there the thread cannot leave native
    /// (`GcBarrier::leave_in_native_if` waits the pause out), so its frames
    /// above the C code are frozen and its stack stays mapped (it cannot exit);
    /// why the band is complete is on `native::jni::in_native_band_at_pause`.
    /// The registry read lock is held for the whole scan, as
    /// [`Self::collect_all_root_snapshots`] holds it.
    pub(crate) fn scan_in_native_bands(
        &self,
        probe: &dyn Fn(usize) -> Option<ObjectRef>,
        out: &mut Vec<ObjectRef>,
    ) -> InNativeBandScan {
        let threads = self.threads.read();
        let mut scan = InNativeBandScan::default();
        // gci i30/gv (`i28-nb-report.md` review item 2): a hash set, not a
        // `Vec` searched per entry (O(threads^2) compares per pause with a
        // few hundred in-native threads). Empty sets do not allocate.
        // addr-keyed: not a heap address -- in-native band stack bounds, local to one pause scan.
        let mut seen: rustc_hash::FxHashSet<(usize, usize)> = rustc_hash::FxHashSet::default();
        for entry in threads.values() {
            let block = &entry.gc_block_state;
            let Some((lo, hi)) = in_native_band_of(block) else {
                continue;
            };
            if !entry.alive.load(Ordering::Acquire) || block.owner_dropped() || !seen.insert((lo, hi)) {
                continue;
            }
            // SAFETY: the band was recorded by its thread for the in-native
            // window it is in now (flag and mark up); inside a pause that
            // window cannot end, so `[lo, hi)` is that thread's mapped stack
            // (the doc above).
            let found = unsafe { scan_in_native_band_words(lo, hi, probe, out) };
            scan.threads += 1;
            scan.words += (hi - lo) / 8;
            scan.roots += found;
        }
        scan
    }

    /// gci i18/dc (`gci-i3hd-heap-walks-and-diagnostics-review-findings` item
    /// 5, the in-native band's half): [`Self::scan_in_native_bands`], the same
    /// entries, bands and contract, but each band's roots are handed to `each`
    /// with the thread that owns the band, so the heap dump names that
    /// thread's serial on them (it named serial 1). `each` runs under the
    /// registry read lock: it must not touch the registry. Cold: dumps only.
    #[cold]
    pub(crate) fn scan_in_native_bands_by_owner(
        &self,
        probe: &dyn Fn(usize) -> Option<ObjectRef>,
        each: &mut dyn FnMut(ThreadId, Vec<ObjectRef>),
    ) {
        let threads = self.threads.read();
        // gci i30/gv: a hash set, as in `scan_in_native_bands`.
        // addr-keyed: not a heap address -- in-native band stack bounds, local to one pause scan.
        let mut seen: rustc_hash::FxHashSet<(usize, usize)> = rustc_hash::FxHashSet::default();
        for (tid, entry) in threads.iter() {
            let block = &entry.gc_block_state;
            let Some((lo, hi)) = in_native_band_of(block) else {
                continue;
            };
            if !entry.alive.load(Ordering::Acquire) || block.owner_dropped() || !seen.insert((lo, hi)) {
                continue;
            }
            let mut found = Vec::new();
            // SAFETY: as in `scan_in_native_bands` (inside a pause the
            // in-native window cannot end, so the band stays mapped).
            let _ = unsafe { scan_in_native_band_words(lo, hi, probe, &mut found) };
            each(*tid, found);
        }
    }

    /// gcj j2/jn2 (cut 2 of
    /// `gcj-j1jn-proposal-jni-transition-cost-ledger-and-three-cuts-20261019`,
    /// `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB=1`): retire, on its owner's behalf,
    /// the TLAB of every JNI in-native thread that went into native keeping it
    /// (`GcBlockState::in_native_tlab_kept`), as HotSpot's
    /// `ensure_parsability` retires every thread's TLAB at the safepoint.
    /// Returns how many it retired.
    ///
    /// Only inside a collecting pause, after its barrier is satisfied
    /// (`gc_and_alloc::retire_in_native_kept_tlabs`). Why the owner cannot
    /// touch its `Tlab` meanwhile: the mark is raised while the owner is
    /// still a counted mutator, before `in_blocked_region` goes up (Release,
    /// read here with Acquire), and the owner leaves native only through
    /// `GcBarrier::leave_in_native_if`, which waits out the pause in progress
    /// before it drops the flag; the C code in between never allocates (every
    /// JNIEnv function leaves native first, and a leaf window does not
    /// allocate). The swap decides the one retirer: a thread whose flag is down
    /// keeps its mark for its own leave to clear. The registry read lock is
    /// held for the walk, as [`Self::collect_reserved_tlab_tails`] holds it.
    pub(crate) fn retire_in_native_kept_tlabs(&self) -> usize {
        let threads = self.threads.read();
        let mut retired = 0usize;
        for entry in threads.values() {
            let block = &entry.gc_block_state;
            if !entry_is_stw_live(entry) || block.owner_dropped() {
                continue;
            }
            if !block.in_blocked_region.load(Ordering::Acquire) {
                continue;
            }
            if !block.in_native_tlab_kept.swap(false, Ordering::AcqRel) {
                continue;
            }
            let addr = entry.tlab_addr.load(Ordering::Acquire);
            if addr == 0 {
                continue;
            }
            // SAFETY: `addr` is the live owner's own `Tlab` (published by it,
            // address-stable for its life, cleared before its `JvmThread` is
            // dropped; `entry_is_stw_live` and `owner_dropped` screened the
            // unwound and the dropped). The owner is in native and cannot
            // leave while this pause holds the barrier (the doc above), so
            // nothing else reads or writes the `Tlab` during this call, and the
            // swap above made this the only retirer.
            unsafe { (*(addr as *mut cratonvm_gc::Tlab)).retire() };
            retired += 1;
        }
        retired
    }

    /// gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in;
    /// `gch-h4bp-review-findings-RETIRED-UNFIXED-20261018` item 1): credit, for the pause
    /// stamped `stamp` (`gc_quiescence::young_pin_pause_stamp`, 0 = none:
    /// nothing is credited), every JNI in-native band whose deposit PROVED its
    /// compiled chain (`GcBlockState::in_native_proven_jit_depth`). For each,
    /// `credit(words, depth)` receives the band's words that `young` accepts
    /// (read raw, volatile: interior and derived words as they are) and the
    /// proven depth, and the band is marked credited for `stamp`
    /// (`GcBlockState::in_native_credit_stamp`).
    ///
    /// A band is credited only when [`in_native_credit_admissible`] holds: a
    /// proven depth equal to the depth its OS thread published (`depth_of`,
    /// `gc_quiescence::jit_depth_of_tid` in production -- the coverage account
    /// subtracts published depth, so crediting more would be the one unsound
    /// direction), and the first band of its OS thread and of its address
    /// range (a second entry on one OS tid or one band would credit the same
    /// frames twice). Every other band stays uncredited, and the pause's band
    /// scan then marks the cycle as before (`gc_and_alloc::append_in_native_band_roots`).
    /// A band already credited for `stamp` (a second root gathering in the same
    /// pause) is skipped: each band is credited once per pause.
    ///
    /// Same entries, contract and lock as [`Self::scan_in_native_bands`]: only
    /// inside a pause, where the band's thread cannot leave native, so the band
    /// is frozen and mapped.
    pub(crate) fn credit_in_native_proven_bands(
        &self,
        stamp: u64,
        depth_of: &dyn Fn(u32) -> Option<usize>,
        young: &dyn Fn(usize) -> bool,
        credit: &mut dyn FnMut(&[usize], usize),
    ) -> InNativeCredit {
        let mut out = InNativeCredit::default();
        if stamp == 0 {
            return out;
        }
        let threads = self.threads.read();
        let mut seen_bands: Vec<(usize, usize)> = Vec::new();
        let mut seen_tids: Vec<u32> = Vec::new();
        let mut words: Vec<usize> = Vec::new();
        for entry in threads.values() {
            let block = &entry.gc_block_state;
            let Some((lo, hi)) = in_native_band_of(block) else {
                continue;
            };
            if !entry.alive.load(Ordering::Acquire) || block.owner_dropped() {
                continue;
            }
            if block.in_native_credit_stamp.load(Ordering::Acquire) == stamp {
                out.already += 1;
                continue;
            }
            // Only a thread the pause's blocked roster EXCLUDES (in C, with
            // its foreign-native mark up and the registry's exemption on):
            // any other blocked thread is read by the helper-window pass,
            // whose pinned credit (`xt_cycle_pinned_jit_depth`) could count
            // the same depth twice. The mark only falls during a pause (a
            // leave waiting it out), so a thread the take-over read as a
            // window, before this runs, is never credited here.
            if !(self.ffm_in_native_no_signal && ffm_in_native_signal_exempt(block)) {
                out.not_exempt += 1;
                continue;
            }
            let depth = block.in_native_proven_jit_depth.load(Ordering::Acquire);
            if depth == 0 {
                out.unproven += 1;
                continue;
            }
            let os_tid = entry.os_tid.load(Ordering::Acquire);
            let duplicate = seen_tids.contains(&os_tid) || seen_bands.contains(&(lo, hi));
            if !in_native_credit_admissible(depth, os_tid, depth_of(os_tid), duplicate) {
                out.mismatched += 1;
                continue;
            }
            seen_tids.push(os_tid);
            seen_bands.push((lo, hi));
            words.clear();
            // SAFETY: as in `scan_in_native_bands` (inside a pause the
            // in-native window cannot end, so the band stays mapped).
            unsafe { in_native_band_words_where(lo, hi, young, &mut words) };
            credit(&words[..], depth);
            block.in_native_credit_stamp.store(stamp, Ordering::Release);
            out.credited += 1;
            out.depth += depth;
            out.words += words.len();
        }
        out
    }

    /// gci i24/bp: how many in-native bands a pause reads
    /// ([`Self::scan_in_native_bands`]' entries) were NOT credited for `stamp`
    /// by [`Self::credit_in_native_proven_bands`]. `stamp == 0` counts every
    /// band. Same contract and lock.
    pub(crate) fn uncredited_in_native_bands(&self, stamp: u64) -> usize {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| {
                let block = &e.gc_block_state;
                in_native_band_of(block).is_some()
                    && e.alive.load(Ordering::Acquire)
                    && !block.owner_dropped()
                    && (stamp == 0 || block.in_native_credit_stamp.load(Ordering::Acquire) != stamp)
            })
            .count()
    }

    /// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-4): which of `os_tids`
    /// (peers a take-over just froze or claimed) belong to an alive entry
    /// that is in a blocked region now. Such a peer was never in the pause's
    /// `expected`, or has already arrived through its region's entry, so the
    /// barrier must not excuse it (`gc_and_alloc::stw_take_over_and_wait`).
    ///
    /// An OS tid is answered only when EVERY live entry published on it is
    /// blocked: a mounted virtual thread shares its carrier's tid, and the
    /// carrier's own entry may sit in a blocked region while the mounted
    /// thread runs (and is the one frozen or claimed); an entry that is not
    /// blocked keeps the tid excusable, as before this rule. One registry
    /// read lock.
    ///
    /// gcj j2/cm2 (`gcj-j1cm-safepoint-barrier-and-registry-review-residuals`
    /// item 6): sorted lookups, not a `contains` per entry and a linear `find`
    /// per match -- that was O(entries x `os_tids`) inside the pause, ~0.5 M
    /// compares a round at 750 frozen threads. Same set, answered in
    /// ascending tid order, each tid once.
    pub(crate) fn blocked_among_os_tids(&self, os_tids: &[u32]) -> Vec<u32> {
        if os_tids.is_empty() {
            return Vec::new();
        }
        let mut wanted = os_tids.to_vec();
        wanted.sort_unstable();
        // (tid, this live entry is blocked), one row per matching entry.
        let mut rows: Vec<(u32, bool)> = Vec::new();
        {
            let threads = self.threads.read();
            for entry in threads.values() {
                if !entry_is_stw_live(entry) {
                    continue;
                }
                let tid = entry.os_tid.load(Ordering::Acquire);
                if tid == 0 || wanted.binary_search(&tid).is_err() {
                    continue;
                }
                let blocked = entry
                    .gc_block_state
                    .in_blocked_region
                    .load(Ordering::Acquire);
                rows.push((tid, blocked));
            }
        }
        rows.sort_unstable();
        let mut out: Vec<u32> = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            let tid = rows[i].0;
            let mut all = true;
            while i < rows.len() && rows[i].0 == tid {
                all &= rows[i].1;
                i += 1;
            }
            if all {
                out.push(tid);
            }
        }
        out
    }

    /// Interpreter round i2 wave 58, lane L6 (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE`):
    /// excuse, for the handshake pause whose census runs now, every COUNTED
    /// thread found running a native through the funnel
    /// (`jvm_thread::NativeExcuse::try_excuse`; with `need_listable`, only one
    /// whose frames below the native another thread may list). Under the
    /// barrier lock, inside the request's census closure, with the same
    /// counted test as the census (alive, `stw_ready`, not
    /// `in_blocked_region`) and never the initiator: not `initiator`'s entry,
    /// nor one published on the calling OS thread (`me`, 0 = unknown).
    ///
    /// An excused thread may keep running its native's Rust code, but it
    /// cannot return from it, run Java or move its frames until the pause
    /// releases it, and none of its arrivals counts in this pause
    /// (`GcBarrier`'s `excused_running`) -- so the pause neither waits for it
    /// nor can be released early by it.
    pub(crate) fn excuse_running_natives(
        &self,
        initiator: ThreadId,
        need_listable: bool,
    ) -> Vec<ExcusedNative> {
        self.excuse_running_natives_on(initiator, current_os_tid(), need_listable)
    }

    /// [`Self::excuse_running_natives`] with the calling OS thread's id
    /// supplied (a test places the "caller" on any OS thread).
    fn excuse_running_natives_on(
        &self,
        initiator: ThreadId,
        me: u32,
        need_listable: bool,
    ) -> Vec<ExcusedNative> {
        let threads = self.threads.read();
        let mut out = Vec::new();
        for (tid, e) in threads.iter() {
            let state = &e.gc_block_state;
            // The common case, one load: not in an excusable native.
            if state.native_excuse.load() & crate::threading::jvm_thread::NX_IN == 0 {
                continue;
            }
            if *tid == initiator
                || !entry_is_stw_live(e)
                || !e.stw_ready.load(Ordering::Acquire)
                || !e.mounted.load(Ordering::Acquire)
                || state.owner_dropped()
                || state.in_blocked_region.load(Ordering::Acquire)
            {
                continue;
            }
            let os_tid = e.os_tid.load(Ordering::Acquire);
            if me != 0 && os_tid == me {
                continue;
            }
            if let Some(word) = state.native_excuse.try_excuse(need_listable) {
                out.push(ExcusedNative {
                    tid: *tid,
                    os_tid,
                    word,
                    state: Arc::clone(state),
                });
            }
        }
        out
    }

    /// The frame trace of an excused thread, built by the pause that excused
    /// it (interpreter round i2 wave 58, lane L6): stored where the thread's
    /// own publish would have put it (`JvmThread::frame_trace`, shared with
    /// the entry).
    pub(crate) fn store_frame_trace(
        &self,
        thread_id: ThreadId,
        trace: Vec<cratonvm_native_api::StackTraceEntry>,
    ) {
        if let Some(e) = self.threads.read().get(&thread_id) {
            *e.frame_trace.lock() = trace;
            // W6-F fix 4: not a live publish's trace (`live_trace_locks`).
            e.jmx_monitors.trace_seq.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// gci i28/tl: every STW-live thread's TLAB cycle counters and running
    /// allocated bytes, keyed by its thread id, for the
    /// `-Xlog:gc+tlab=debug` `TLAB totals` line
    /// (`unified_logging::stash_tlab_totals`). Read-only: the counters are
    /// running totals the collector differences, it never resets another
    /// thread's buffer.
    ///
    /// SAFETY: the call-time contract of [`Self::collect_reserved_tlab_tails`]
    /// (the world is stopped; the liveness screen is the same), whose reads
    /// this mirrors: a handful of plain field loads per thread.
    pub fn tlab_cycle_snapshots(&self) -> Vec<cratonvm_gc::tlab::TlabCycleSnapshot> {
        let threads = self.threads.read();
        let mut out = Vec::with_capacity(threads.len());
        for (id, entry) in threads.iter() {
            if !entry_is_stw_live(entry) || entry.gc_block_state.owner_dropped() {
                continue;
            }
            let addr = entry.tlab_addr.load(Ordering::Acquire);
            if addr == 0 {
                continue;
            }
            // SAFETY: see the method contract.
            let tlab = unsafe { &*(addr as *const cratonvm_gc::Tlab) };
            // gci i43/tj: keyed by the never-reused thread id, not the
            // address (`TlabCycleSnapshot`), so `TlabCycleMark::since` needs
            // no plausibility test.
            let key = usize::try_from(id.0).unwrap_or(usize::MAX);
            out.push((key, tlab.cycle_mark(), tlab.thread_allocated_bytes()));
        }
        out
    }

    /// BUG-03 — collect the reserved (un-retired) TLAB tails of every alive
    /// thread, as absolute `(cursor, end)` pairs.
    ///
    /// Called by the multi-threaded GC initiator AFTER the STW barrier is
    /// satisfied (so no counted peer is running Java): at that point normal
    /// parked or blocked peers should already have retired their TLAB
    /// (`reserved_tail` -> `None`), but collecting every alive thread makes the
    /// collector robust if a blocked or tearing-down path missed that retire.
    /// The non-empty tails are the regions the non-moving young sweep must skip
    /// (see
    /// [`cratonvm_gc::vm_heap::VmHeap::set_jit_tlab_skip_regions`]).
    ///
    /// SAFETY: reads each alive thread's `Tlab` through its published address.
    /// Per the call-time invariant above, no alive thread is mutating its TLAB
    /// (all are parked / blocked / OS-suspended), and an alive thread's
    /// `JvmThread` cannot be concurrently dropped (teardown clears the address
    /// and flips `alive` first). Dead / un-published entries are skipped.
    pub fn collect_reserved_tlab_tails(&self) -> Vec<(usize, usize)> {
        let threads = self.threads.read();
        let mut out = Vec::new();
        // Census counters for the `CRATONVM_G1_DBG_REACH` line below. An empty
        // result has three very different causes and the G1 walk-break reports
        // ("ZERO published skip spans this pause") cannot tell them apart:
        // nobody is registered, everybody is registered but retired, or this
        // function was never reached at all. Counting the population separates
        // the first two, and the absence of the line entirely settles the third.
        let (mut total, mut dead, mut unregistered, mut retired) = (0usize, 0, 0, 0);
        for entry in threads.values() {
            total += 1;
            // `entry_is_stw_live`, not `alive`: a carrier that unwound past
            // `mark_dead` left `tlab_addr` pointing into its dropped stack.
            // gen r5w2/roots6: and one whose `JvmThread` has been DROPPED
            // (`GcBlockState::owner_dropped`) is skipped before its OS thread
            // finishes too -- the unwind window `entry_is_stw_live` cannot see.
            if !entry_is_stw_live(entry) || entry.gc_block_state.owner_dropped() {
                dead += 1;
                continue;
            }
            let addr = entry.tlab_addr.load(Ordering::Acquire);
            if addr == 0 {
                unregistered += 1;
                continue;
            }
            // SAFETY: see method contract — the owning thread is parked /
            // blocked / OS-suspended, so the `Tlab` at `addr` is live and not
            // being mutated.
            let tlab = unsafe { &*(addr as *const cratonvm_gc::Tlab) };
            match tlab.reserved_tail() {
                Some(tail) => {
                    // Paired with `CRATONVM_DBG_WATCH_ALLOC_CID` /
                    // `CRATONVM_DBG_WATCH_HOLDER`: a published "un-allocated
                    // tail" that CONTAINS a live object is not a conservative
                    // degrade -- every linear walk in the sweep skips the span,
                    // and the mark oracle then answers "gap space, not an
                    // object" for any root pointing into it, so the object is
                    // neither scanned nor swept and everything it references is
                    // reclaimed. Name the owner and its cursor when it happens.
                    let watch = cratonvm_gc::heap::young_mark_watch();
                    if watch != 0 && watch >= tail.0 && watch < tail.1 {
                        eprintln!(
                            "[TLAB-SKIP] published tail [{:#x},{:#x}) CONTAINS watch {watch:#x} — owner java_tid={:?} name={} alive={} tlab_addr={addr:#x}",
                            tail.0,
                            tail.1,
                            entry.java_tid,
                            entry.name,
                            entry.alive.load(Ordering::Acquire),
                        );
                    }
                    out.push(tail)
                }
                None => retired += 1,
            }
        }
        if cratonvm_types::flags().gc.g1_dbg_reach {
            // Sequence number, not a pause id: the walk-break reports carry no
            // pause identity either, so the only thing that can be compared is
            // "how many times did the publish path run" against "how many
            // breaks reported ZERO published skip spans". The first run
            // produced exactly ONE census line across 453 s and four
            // zero-span breaks, which is the fact this counter is here to
            // confirm or kill.
            static CENSUS_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = CENSUS_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!(
                "[g1][TLAB-CENSUS] #{seq} entries={total} dead={dead} \
                 alive_no_tlab_addr={unregistered} alive_retired={retired} published={}",
                out.len()
            );
        }
        out
    }

    /// INT-3 — the deposited root-snapshot contents of every alive thread
    /// whose OS tid is in `os_tids`.
    ///
    /// Used by the G1 cross-thread STW JIT takeover: a forcibly-frozen peer
    /// is excused from the barrier, so it never applies the collection's
    /// pointer map to its own frames — its deposit snapshot is the only view
    /// of what those frames reference. Keeping the snapshot alive (it is
    /// already merged into the collection roots) is not enough under an
    /// evacuating collector: the objects must also NOT MOVE, so the caller
    /// pins each returned address's region out of the CSet (see
    /// `cratonvm_gc::gc_quiescence::add_pinned_jit_root`). Not needed on the
    /// Generational backend, whose frozen-peer cycles run the fully
    /// non-moving sweep.
    pub fn root_snapshots_for_os_tids(&self, os_tids: &[u32]) -> Vec<ObjectRef> {
        if os_tids.is_empty() {
            return Vec::new();
        }
        let threads = self.threads.read();
        let mut out = Vec::new();
        for entry in threads.values() {
            if !entry_is_stw_live(entry) {
                continue;
            }
            let tid = entry.os_tid.load(Ordering::Acquire);
            if tid != 0 && os_tids.contains(&tid) {
                out.extend(entry.root_snapshot.lock().iter().copied());
            }
        }
        out
    }

    /// T19.K1 — Update the daemon flag for an already-registered thread.
    ///
    /// `Thread.setDaemon(boolean)` may be called between construction
    /// and `start()`; once `start()` is invoked the JLS forbids further
    /// changes (an `IllegalThreadStateException` is thrown by the JDK
    /// bytecode). We don't enforce that gate here — callers (the
    /// native bridge) are responsible — we just store the new value.
    /// Returns `true` if the thread was found, `false` if the id was
    /// unknown.
    pub fn set_daemon(&self, thread_id: ThreadId, daemon: bool) -> bool {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            let was = entry.daemon.swap(daemon, Ordering::AcqRel);
            // zt t15/pl6: a listed entry's flip moves the roster's daemons.
            // Every edge that changes whether an entry is listed is made
            // under the `threads` write lock, which this read excludes.
            if was != daemon && Self::jmm_roster_lists(entry) {
                let delta = if daemon { 1 } else { -1 };
                self.jmm_roster_daemon.fetch_add(delta, Ordering::AcqRel);
            }
            true
        } else {
            false
        }
    }

    /// T19.K1 — Read the daemon flag for a thread. Returns `false` for
    /// unknown thread ids (matches `is_alive`'s "missing means absent"
    /// convention so callers can treat unknown ids as already-gone
    /// non-daemon threads — the safe default).
    pub fn is_daemon(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.daemon.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// T19.H1 watchdog — unpark every registered thread so threads parked
    /// in `LockSupport.park` (e.g. AQS lock/latch waiters) wake, return
    /// through the interpreter top-of-loop poll, and emit their stack dump.
    /// Diagnostic-only: a spurious unpark is spec-legal (`park` may return
    /// spuriously) and the process aborts immediately after the dump, so the
    /// extra permits never affect correctness.
    pub fn unpark_all_for_stack_dump(&self) {
        // Interpreter round i4 wave 91, lane L6 (page
        // `i77-L6-proposal-every-vm-unpark-through-the-shape`): the VM's
        // unpark shape, as `Unsafe.unpark` gives it.
        let shape = self.unpark_shape();
        let threads = self.threads.read();
        for entry in threads.values() {
            entry.park_state.unpark_with(shape);
        }
    }

    /// Legend printed under the T19.H1 thread-summary header, explaining what
    /// the `deposit=` column means. See [`Self::deposit_freshness`] for why
    /// this exists at all.
    pub(crate) const DEPOSIT_LEGEND: &'static str = concat!(
        "  legend: deposit=live        — parked in a blocking native; blocked=/roots=/top= ARE its current state.\n",
        "          deposit=STALE       — RUNNING right now. top=/roots= are whatever it deposited at its\n",
        "                                LAST blocking call and can be arbitrarily old — read the live\n",
        "                                \"T19.H1 stack dump\" above for where this thread actually is,\n",
        "                                UNLESS the row says no-live-dump (see below).\n",
        "          no-live-dump        — RUNNING, and it produced no live dump: it never reached an\n",
        "                                interpreter dispatch point during the watchdog's grace period.\n",
        "                                That means JIT-COMPILED code or a long native call — NOT a\n",
        "                                deadlock, and NOT evidence about which. Re-run with --nojit:\n",
        "                                if the live dump then appears, it was compiled code.\n",
        "          deposit=post-mortem — dead. alive=false is authoritative; blocked=/roots=/top= are residue from\n",
        "                                its termination sequence (nothing clears them), NOT evidence of a wait."
    );

    /// How current a registry entry's *deposited* state — the `blocked=`,
    /// `roots=` and frame-chain columns of the T19.H1 summary — actually is.
    ///
    /// A thread deposits its root snapshot and frame chain when it is about to
    /// block, and **nothing clears either afterwards**: not on wake, not on
    /// death. So the deposit is a faithful readout of the thread's position in
    /// exactly one case — it is alive AND still inside a blocked region.
    /// Otherwise it is history:
    ///
    /// * `STALE` (alive, not blocked): the thread is executing bytecode. The
    ///   chain names its last *blocking* call, which may be seconds or minutes
    ///   old and in a completely different part of the program.
    /// * `post-mortem` (dead): `blocked=true` here is the terminal
    ///   `deposit_root_snapshot()` in the thread-exit sequence, which raises
    ///   `in_blocked_region` and has no counterpart to lower it. Every STW
    ///   census filters on `alive` before reading that flag
    ///   (`alive_count_blocked_and_os_tids_inner`, `blocked_os_tids`,
    ///   `fold_pointer_map_into_blocked`), so the residue is inert to the
    ///   barrier — it is purely a reporting artifact.
    ///
    /// Labelling this is not cosmetic. Three separate investigations have now
    /// been sent the wrong way by an unmarked stale deposit:
    /// `onclasscondition-join-never-returns-20260801` (read `alive=false` on
    /// dead entries as a missed wakeup), the Elasticsearch
    /// `ES-HANG-20260719-threadjoin` capture (same shape), and
    /// `bug-h2-teststringcache-thread-join-blocked-on-dead-threads-20260807`,
    /// where `main` was `alive=true blocked=false` — running
    /// `TestStringCache.runBenchmark()` — while its unmarked deposit still read
    /// `java/lang/Thread.join@129` from a join that had returned ~40 s earlier.
    /// The live stack dump printed directly above said `runBenchmark`; nothing
    /// flagged the contradiction, and the whole doc was written against the
    /// stale line.
    pub(crate) fn deposit_freshness(alive: bool, blocked: bool) -> &'static str {
        match (alive, blocked) {
            (false, _) => "post-mortem",
            (true, true) => "live",
            (true, false) => "STALE",
        }
    }

    /// T19.H1 watchdog — write [`Self::render_thread_summary_for`] to stderr.
    ///
    /// `live_dumped` is the set of thread ids that answered the watchdog with a
    /// live frame dump. See [`Self::render_thread_summary_for`].
    pub fn dump_thread_summary_to_stderr(&self, live_dumped: &[u64]) {
        use std::io::Write;
        let text = self.render_thread_summary_for(live_dumped);
        let stderr = std::io::stderr();
        let mut h = stderr.lock();
        let _ = h.write_all(text.as_bytes());
        let _ = h.flush();
    }

    /// [`Self::render_thread_summary_for`] with no live-dump information —
    /// every RUNNING thread is then reported as `no-live-dump`, which is the
    /// safe reading when the caller cannot say. Test-only since JIT round 22
    /// wave 7 (lane review): no caller outside the tests was ever added.
    #[cfg(test)]
    pub fn render_thread_summary(&self) -> String {
        self.render_thread_summary_for(&[])
    }

    /// T19.H1 watchdog — a one-line summary of every registered thread (name,
    /// liveness, daemon, deposited-root count), plus the complete deposited
    /// frame chain of every alive thread. Lets the watchdog show threads that
    /// have NO dumpable interpreter frames — e.g. a thread blocked in a
    /// Rust-level native lock, or one that never started — which the frame-dump
    /// path cannot surface. A non-zero `roots` on an otherwise-silent thread
    /// means it deposited roots before blocking (it IS blocked in a native),
    /// distinguishing "blocked in native" from "never ran".
    ///
    /// Every row carries a `deposit=` freshness tag; see
    /// [`Self::deposit_freshness`] for what each value licenses you to
    /// conclude, and why an untagged chain has repeatedly been misread as a
    /// live wait site.
    ///
    /// `live_dumped` lists the thread ids that produced a live frame dump in
    /// response to this watchdog cycle. It is what separates the two very
    /// different states that both render as `blocked=false`:
    ///
    ///  * a RUNNING thread that DID dump — the live dump above is where it is;
    ///  * a RUNNING thread that did NOT — it never reached an interpreter
    ///    dispatch point, i.e. it is in JIT-compiled code or a long native
    ///    call. The summary used to point every such row at a
    ///    `"T19.H1 stack dump: tid=N"` section that was never emitted, and the
    ///    absence of that section then read as "blocked somewhere the watchdog
    ///    cannot reach". It cost
    ///    `known-issues/netty/brotli-integration-test-hangs-outside-the-interpreter`
    ///    a whole investigation: the thread was neither blocked nor in native
    ///    code, it was spinning in a compiled `ByteBuf.writeByte` loop, and
    ///    `--nojit` produced the full 73-frame dump immediately.
    ///
    /// Returned as a `String` (rather than written straight out) so the format
    /// is unit-testable — the tags below are the load-bearing part.
    pub fn render_thread_summary_for(&self, live_dumped: &[u64]) -> String {
        use std::fmt::Write as _;
        let threads = self.threads.read();
        let mut h = String::new();
        let _ = writeln!(
            h,
            "--- T19.H1 thread summary: {} registered thread(s) ---",
            threads.len()
        );
        let _ = writeln!(h, "{}", Self::DEPOSIT_LEGEND);
        struct SummaryRow {
            tid: u64,
            os_tid: u64,
            name: String,
            alive: bool,
            daemon: bool,
            blocked: bool,
            roots: usize,
            state: String,
            top: String,
            full_frames: Vec<String>,
        }
        let mut rows: Vec<SummaryRow> = threads
            .iter()
            .map(|(tid, e)| {
                let state = {
                    let s = e.vm_state.lock();
                    if s.is_empty() {
                        "<unset>".to_string()
                    } else {
                        s.clone()
                    }
                };
                let trace = e.frame_trace.lock();
                let mut top = String::new();
                for (i, frame) in trace.iter().rev().take(3).enumerate() {
                    if i > 0 {
                        top.push_str(" <- ");
                    }
                    top.push_str(&format!(
                        "{}.{}@{}",
                        frame.class_name, frame.method_name, frame.byte_code_index
                    ));
                }
                if top.is_empty() {
                    top.push_str("<no-frame-trace>");
                }
                // 2026-08-03 (onclasscondition-join-never-returns) — the
                // one-line `top` above caps at 3 frames, which is enough to
                // name a wait site but not to tell "this thread is a live
                // participant mid-call-chain" from "this is a stale dead
                // entry that happens to share a wait site with hundreds of
                // others from earlier per-test JVM reboots". Every alive
                // thread's COMPLETE deposited chain, oldest frame first (same
                // order `dump_current_thread_frames` uses), so a hang repro
                // can be told apart from the accumulated dead-thread noise
                // without a second run.
                let full_frames = if e.alive.load(std::sync::atomic::Ordering::Acquire) {
                    trace
                        .iter()
                        .map(|frame| {
                            format!(
                                "{}.{}@{}",
                                frame.class_name, frame.method_name, frame.byte_code_index
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                SummaryRow {
                    tid: tid.0,
                    os_tid: e.os_tid.load(std::sync::atomic::Ordering::Acquire) as u64,
                    name: e.name.clone(),
                    alive: e.alive.load(std::sync::atomic::Ordering::Acquire),
                    daemon: e.daemon.load(std::sync::atomic::Ordering::Acquire),
                    blocked: e
                        .gc_block_state
                        .in_blocked_region
                        .load(std::sync::atomic::Ordering::Acquire),
                    roots: e.root_snapshot.lock().len(),
                    state,
                    top,
                    full_frames,
                }
            })
            .collect();
        rows.sort_by_key(|r| r.tid);
        for row in &rows {
            // `deposit=` sits immediately before `top=` on purpose: the two are
            // read together, and the tag is what stops the chain from being
            // taken for a live wait site.
            let _ = writeln!(
                h,
                "  tid={} os_tid={} name={:?} alive={} daemon={} blocked={} roots={} state={:?} deposit={} top={}",
                row.tid,
                row.os_tid,
                row.name,
                row.alive,
                row.daemon,
                row.blocked,
                row.roots,
                row.state,
                Self::deposit_freshness(row.alive, row.blocked),
                row.top
            );
            if row.alive && !row.blocked && !live_dumped.contains(&row.tid) {
                let _ = writeln!(
                    h,
                    "        ^ no-live-dump: this thread produced NO \"T19.H1 stack dump: tid={}\" \
                     section — it never reached an interpreter dispatch point, so it is in \
                     JIT-compiled code or a long native call. Re-run with --nojit; if the live \
                     dump appears there, it was compiled code (and it was RUNNING, not stuck).",
                    row.tid
                );
            }
        }
        // Full frame chains, split out from the one-line-per-thread summary
        // above so that table stays scannable. Restricted to `alive` threads
        // — the registry retains every thread it has ever seen for the life
        // of the process, so a long-running multi-class suite run can carry
        // hundreds of dead entries whose frame chains are pure noise here.
        let live_with_frames: Vec<&SummaryRow> = rows
            .iter()
            .filter(|r| r.alive && !r.full_frames.is_empty())
            .collect();
        // "alive" is NOT the same as "parked here". A running thread keeps the
        // chain it deposited at its last blocking call, so split the count —
        // an all-STALE section is a section with no wait sites in it at all.
        let at_wait_site = live_with_frames.iter().filter(|r| r.blocked).count();
        let _ = writeln!(
            h,
            "--- T19.H1 full frame chains: {} alive thread(s) with a deposited snapshot ({} at a live wait site, {} STALE) ---",
            live_with_frames.len(),
            at_wait_site,
            live_with_frames.len() - at_wait_site
        );
        for row in live_with_frames {
            let tag = if row.blocked {
                "[deposit=live — this IS the thread's current wait site]".to_string()
            } else if live_dumped.contains(&row.tid) {
                format!(
                    "[deposit=STALE — thread is RUNNING (blocked=false); chain below is from its LAST \
                     blocking call and may be arbitrarily old. Its real position is in the \
                     \"T19.H1 stack dump: tid={}\" section above]",
                    row.tid
                )
            } else {
                // Do NOT send the reader to a section that was never emitted —
                // its absence is the finding, not a missing feature.
                "[deposit=STALE + no-live-dump — thread is RUNNING (blocked=false) and answered no \
                 dump, so it is in JIT-compiled code or a long native call. The chain below is from \
                 its LAST blocking call and may be arbitrarily old; there is NO live section for \
                 this thread. Re-run with --nojit to get one.]"
                    .to_string()
            };
            let _ = writeln!(
                h,
                "  tid={} os_tid={} name={:?} ({} frame(s), oldest first) {}:",
                row.tid,
                row.os_tid,
                row.name,
                row.full_frames.len(),
                tag
            );
            for (depth, frame) in row.full_frames.iter().enumerate() {
                let _ = writeln!(h, "    [{depth}] {frame}");
            }
        }
        let _ = writeln!(h, "--- T19.H1 end thread summary ---");
        h
    }

    /// Store the OS `JoinHandle` for a spawned thread.
    pub fn set_join_handle(&self, thread_id: ThreadId, handle: JoinHandle<()>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.join_handle = Some(handle);
        }
    }

    /// Mark a thread as dead (called when the thread finishes execution).
    pub fn mark_dead(&self, thread_id: ThreadId) {
        self.mark_dead_with_purge_slack(thread_id, 0);
    }

    /// [`Self::mark_dead`], letting up to `slack` terminated entries pile up
    /// past `TERMINATED_THREAD_ENTRY_CAP` before one purge trims the tail back
    /// to the cap. `0` is `mark_dead`: past the cap, every death collects and
    /// sorts every dead entry to remove the oldest one, under the `threads`
    /// write lock. A JDK continuation's death passes a slack under
    /// `CRATONVM_VTHREAD_LEAN_REGISTRY` (interpreter round i4 wave 94, lane
    /// L6), so a virtual thread per task pays that collect-and-sort once per
    /// `slack` deaths. What a reader sees is unchanged except that up to
    /// `slack` more terminated entries (the newest ones; the purge removes the
    /// oldest) answer as TERMINATED rather than as unknown.
    pub fn mark_dead_with_purge_slack(&self, thread_id: ThreadId, slack: usize) {
        let dead_mirror;
        let dead_park_state;
        let first_death;
        let mut retired_java_tids = Vec::new();
        {
            let mut threads = self.threads.write();
            let Some(entry) = threads.get_mut(&thread_id) else {
                return;
            };
            // JIT round 17 wave 2 (lane smallfix;
            // `r17-takeover-roster-tid-outlives-its-thread-FIXED-20261005.md`): withdraw
            // the published OS tid BEFORE the entry reads dead. The OS recycles
            // a tid once its thread ends, and a dead entry is kept (up to
            // `TERMINATED_THREAD_ENTRY_CAP`) with it: `os_tid_of` (no `alive`
            // filter) would then answer another thread's tid for this one, and
            // a dead virtual thread's entry would keep naming its last
            // carrier. Every roster, census and claimant read already skips a
            // `0` tid. The ordinal is left alone, as `clear_os_tid` does.
            // `CRATONVM_THREAD_DEAD_CLEARS_OS_TID=0` keeps the old tid.
            if mark_dead_clears_os_tid() {
                entry.os_tid.store(0, Ordering::Release);
            }
            // Wave 95 (lane L6): the `alive` edge is counted in
            // `dead_entries` (a second `mark_dead` of a dead entry, e.g. a
            // joiner's, is not).
            first_death = entry.alive.swap(false, Ordering::AcqRel);
            if first_death {
                self.dead_entries.fetch_add(1, Ordering::Relaxed);
                // zt t15/pl6: a listed entry leaves the JMM roster, once.
                self.jmm_roster_leave_on_death(entry);
            }
            // Interpreter round i5 wave 100, lane L6: a counted platform
            // thread leaves the JMM live count, once.
            self.jmm_note_death(entry);
            // Reclaim the OS thread handle. Tomcat's poller/acceptor/pool
            // threads are daemon threads that are never `join()`ed, so their
            // `JoinHandle` would otherwise sit in the registry forever, leaking
            // the underlying Windows thread HANDLE. Across heavy short-lived
            // thread churn — e.g. a parameterized test that starts and stops a
            // whole Tomcat instance ~140 times — this leaks hundreds of thread
            // handles (observed: 219 Thread handles for ~21 live threads),
            // adding steady per-iteration overhead. The thread has finished, so
            // dropping the handle simply detaches it; a later `join()` of an
            // already-dead thread still returns immediately (see `join`).
            entry.join_handle.take();
            dead_mirror = entry.java_thread_obj;
            dead_park_state = entry.park_state.clone();

            // gcj j1/cm: the kept count, not a walk. Every platform thread's
            // death runs this under the registry write lock AND the barrier's
            // `inner` (`finish_after` -> `mark_blocked_region_leave_after`),
            // so the O(entries) walk it replaced serialised every park, wake
            // and pause request behind each death (O(n^2) over a churn
            // burst). Wave 95 (lane L6) keeps `dead_entries` exact; a debug
            // build still checks it against the walk at every death.
            let dead_count = self.dead_entries.load(Ordering::Relaxed);
            debug_assert_eq!(
                dead_count,
                threads
                    .values()
                    .filter(|entry| !entry.alive.load(Ordering::Acquire))
                    .count(),
                "ThreadRegistry::dead_entries drifted from the dead entries"
            );
            if dead_count > TERMINATED_THREAD_ENTRY_CAP.saturating_add(slack) {
                self.purge_dead_tail_locked(&mut threads, dead_count, &mut retired_java_tids);
            }
        }
        // Drop the dead thread's mirror-ADDRESS→ParkState reverse-index entry.
        // The entry itself is retained (TERMINATED `getState()` answers), but
        // once the mirror is unreachable the collector can recycle its heap
        // address for a brand-new `Thread`; a lingering address key would then
        // route that NEW thread's `unpark`/`interrupt` wakeups to this dead
        // ParkState — a silently lost wakeup (observed as Tomcat executor
        // workers parked forever across DoHead's 288 Tomcat start/stop cycles).
        // Guarded by Arc identity so a same-address re-registration that
        // already overwrote the slot (new thread registered before this ran)
        // is left untouched. Lock order threads → thread_obj_to_park matches
        // the register/set paths.
        if let Some(obj) = dead_mirror {
            self.drop_dead_park_index_entry(obj, &dead_park_state, first_death);
        }
        if !retired_java_tids.is_empty() {
            let mut by_java_tid = self.java_tid_to_id.lock();
            for java_tid in retired_java_tids {
                by_java_tid.remove(&java_tid);
            }
        }
        // P1 shadow record — the `-> Terminated` edge. `mark_dead` is NOT
        // always self-called: `ThreadRegistry::join` (below) marks the joinee
        // dead from the *joining* thread. `record_transition_for` therefore
        // records only when the calling thread's cell is bound to this id, so
        // a peer's termination can never be attributed to the caller.
        crate::threading::thread_state::record_transition_for(
            thread_id.0,
            crate::threading::thread_state::ThreadExecState::Terminated,
            "thread_registry::mark_dead",
        );
    }

    /// Drop a dead thread's `thread_obj_to_park` entry: the one keyed by its
    /// mirror's address `obj`, if it still maps to the thread's `park_state`
    /// (a newer thread registered at a recycled address keeps its own).
    ///
    /// Interpreter round i4 wave 96, lane L6: `obj` was read off the entry
    /// before this call, and a dead thread is outside every stop-the-world
    /// census, so a moving collection can relocate the mirror in between;
    /// `update_thread_objs_after_gc` then re-keys the index entry to the new
    /// address and the lookup by `obj` misses. Before, that miss left the dead
    /// `ParkState` in the index for the life of the address -- a terminated
    /// thread kept in a map, which a new `Thread` allocated at that address
    /// before its start would reach. On a miss the index is searched for the
    /// state by identity (rare: only a death that raced a collection). Whatever
    /// the order against the collector's re-key, which moves only keys that are
    /// present, the entry goes: removed here first, the re-key finds nothing;
    /// re-keyed first, the identity search finds it. `first_death`: this call
    /// follows the death that lowered `alive`; a later `mark_dead` of the same
    /// thread (a joiner's) misses because the first one removed the entry, and
    /// does not search.
    fn drop_dead_park_index_entry(
        &self,
        obj: ObjectRef,
        park_state: &Arc<ParkState>,
        first_death: bool,
    ) {
        let key = obj.as_ptr() as usize;
        let mut idx = self.thread_obj_to_park.lock();
        match idx.get(&key) {
            Some(mapped) => {
                if Arc::ptr_eq(mapped, park_state) {
                    idx.remove(&key);
                }
            }
            None if first_death => idx.retain(|_, mapped| !Arc::ptr_eq(mapped, park_state)),
            None => {}
        }
    }

    /// Insert `entry` under the `threads` write lock, keeping
    /// [`Self::dead_entries`] exact when it replaces a dead entry of the same
    /// id (ids are not reused; this is for the hand-built ones).
    fn insert_entry(&self, thread_id: ThreadId, entry: ThreadEntry) {
        let mut threads = self.threads.write();
        // zt t15/pl6: the new entry joins the JMM roster when listed.
        if Self::jmm_roster_lists(&entry) {
            self.jmm_roster_add(entry.daemon.load(Ordering::Acquire), 1);
        }
        if let Some(old) = threads.insert(thread_id, entry) {
            if !old.alive.load(Ordering::Acquire) {
                self.dead_entries.fetch_sub(1, Ordering::Relaxed);
            }
            // Wave 100 (lane L6): a replaced live entry leaves the JMM count.
            self.jmm_note_death(&old);
            // zt t15/pl6: and the roster.
            if Self::jmm_roster_lists(&old) {
                self.jmm_roster_add(old.daemon.load(Ordering::Acquire), -1);
            }
        }
    }

    /// Count `thread_id`'s start in the JMM's platform-thread counters
    /// (interpreter round i5 wave 100, lane L6; HotSpot's
    /// `ThreadService::add_thread`): the cumulative started count, the live
    /// count and, raised to it, the peak. Once per entry (a second call is a
    /// no-op); an unknown or dead id is not counted. Callers: a platform
    /// thread's `Thread.start` (`vm_exec::thread_start`, never a virtual
    /// thread), the main thread's registration and a JNI attach.
    pub fn note_jmm_thread_start(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        let Some(entry) = threads.get(&thread_id) else {
            return;
        };
        if !entry.alive.load(Ordering::Acquire) || entry.jmm_counted.swap(true, Ordering::AcqRel) {
            return;
        }
        self.jmm_started.fetch_add(1, Ordering::Relaxed);
        let live = self.jmm_live.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        self.jmm_peak.fetch_max(live, Ordering::AcqRel);
    }

    /// The death half of [`Self::note_jmm_thread_start`]: a counted entry
    /// leaves the live count exactly once (`jmm_counted` lowered).
    fn jmm_note_death(&self, entry: &ThreadEntry) {
        if entry.jmm_counted.swap(false, Ordering::AcqRel) {
            self.jmm_live.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// The JMM's platform-thread counters `(started, live, peak)` (wave 100,
    /// lane L6; `ThreadMXBean.getTotalStartedThreadCount`, `getThreadCount`,
    /// `getPeakThreadCount`).
    pub fn jmm_counters(&self) -> (i64, i32, i32) {
        let started = i64::try_from(self.jmm_started.load(Ordering::Acquire)).unwrap_or(i64::MAX);
        let live = self.jmm_live.load(Ordering::Acquire);
        let peak = self.jmm_peak.load(Ordering::Acquire).max(live);
        (started, live, peak)
    }

    /// `ThreadMXBean.resetPeakThreadCount`: the peak becomes the live count
    /// (wave 100, lane L6).
    pub fn jmm_peak_reset(&self) {
        self.jmm_peak
            .store(self.jmm_live.load(Ordering::Acquire), Ordering::Release);
    }

    /// zt t15/pl6: does `ThreadMXBean`'s walk list `entry`? Alive, with a
    /// `java.lang.Thread` mirror (`alive_thread_objects`), and not a virtual
    /// thread's (`management::is_virtual_thread_mirror`).
    fn jmm_roster_lists(entry: &ThreadEntry) -> bool {
        !entry.virtual_thread
            && entry.java_thread_obj.is_some()
            && entry.alive.load(Ordering::Acquire)
    }

    /// zt t15/pl6: move the JMM roster by `delta` (one entry joining or
    /// leaving), and its daemon count too for a daemon.
    fn jmm_roster_add(&self, daemon: bool, delta: i32) {
        self.jmm_roster_live.fetch_add(delta, Ordering::AcqRel);
        if daemon {
            self.jmm_roster_daemon.fetch_add(delta, Ordering::AcqRel);
        }
    }

    /// zt t15/pl6: `entry`'s `alive` just went true -> false (the caller saw
    /// the edge): it leaves the roster if it was listed then.
    fn jmm_roster_leave_on_death(&self, entry: &ThreadEntry) {
        if !entry.virtual_thread && entry.java_thread_obj.is_some() {
            self.jmm_roster_add(entry.daemon.load(Ordering::Acquire), -1);
        }
    }

    /// zt t15/pl6 (`zgc-zt12pl4-perf-counters-what-is-left-*` item 1): the
    /// figures `ThreadMXBean` answers, from the registry alone, for the
    /// `java.threads.*` perf counters the sampler writes (it cannot read a
    /// mirror's class or holder off a mutator): `live` (`getThreadCount`),
    /// `daemon` (`getDaemonThreadCount`), `peak` (`getPeakThreadCount`) and
    /// `started` (`getTotalStartedThreadCount`). `live` and `daemon` are the
    /// roster ([`Self::jmm_roster_lists`]) less the ended `main` thread the
    /// bean's enumeration leaves out (`NativeContext::enumerate_threads`);
    /// `peak` and `started` are the JMM counters plus the listed threads they
    /// did not count, never below `live`, with the formulas of
    /// `management::peak_thread_count` / `total_started_thread_count`.
    /// HotSpot's `ThreadService` keeps the four perf counters with the bean's
    /// own figures (`_live_threads_count`, `_daemon_threads_count`,
    /// `_peak_threads_count`, `_total_threads_count`).
    pub fn jmm_perf_thread_counts(&self) -> JmmThreadCounts {
        let mut live = self.jmm_roster_live.load(Ordering::Acquire);
        let mut daemon = self.jmm_roster_daemon.load(Ordering::Acquire);
        if self.java_side_ended(ThreadId(0)) && !self.destroy_java_vm_listed() {
            if let Some(main) = self.threads.read().get(&ThreadId(0)) {
                if Self::jmm_roster_lists(main) {
                    live -= 1;
                    if main.daemon.load(Ordering::Acquire) {
                        daemon -= 1;
                    }
                }
            }
        }
        let (live, daemon) = (live.max(0), daemon.max(0));
        let (started, counted_live, peak) = self.jmm_counters();
        let uncounted = live.saturating_sub(counted_live);
        JmmThreadCounts {
            live,
            daemon,
            peak: peak.saturating_add(uncounted).max(live),
            started: started
                .saturating_add(i64::from(uncounted))
                .max(i64::from(live)),
        }
    }

    /// Remove the oldest (lowest-id) dead entries until `TERMINATED_THREAD_ENTRY_CAP`
    /// remain; `dead_count` is the number of dead entries now. The caller holds
    /// the `threads` write lock and drops each retired `java_tid` from the
    /// index after releasing it.
    fn purge_dead_tail_locked(
        &self,
        threads: &mut FxHashMap<ThreadId, ThreadEntry>,
        dead_count: usize,
        retired_java_tids: &mut Vec<u64>,
    ) {
        let mut purge: Vec<ThreadId> = threads
            .iter()
            .filter_map(|(id, entry)| (!entry.alive.load(Ordering::Acquire)).then_some(*id))
            .collect();
        purge.sort_by_key(|id| id.0);
        let mut removed = 0usize;
        for id in purge
            .into_iter()
            .take(dead_count.saturating_sub(TERMINATED_THREAD_ENTRY_CAP))
        {
            if let Some(retired) = threads.remove(&id) {
                removed += 1;
                if retired.java_tid != 0 {
                    retired_java_tids.push(retired.java_tid);
                }
            }
        }
        // Only dead entries were removed.
        self.dead_entries.fetch_sub(removed, Ordering::Relaxed);
    }

    /// Register JDK continuation thread `thread_id` COMPLETE, in one `threads`
    /// write (interpreter round i4 wave 95, lane L6, `CRATONVM_VTHREAD_ONE_PUBLISH`):
    /// its virtual-thread mirror and that mirror's `Thread.tid` (`0`: unknown),
    /// the `Arc`s its `JvmThread` already holds (`shared`), and the addresses
    /// of its `JvmThread` and TLAB, as HotSpot's `Threads::add` publishes a
    /// prepared thread once. Replaces `register_starting_with_daemon`, then
    /// `set_java_thread_obj_with_tid` (two writes), `starting_thread_states`,
    /// `set_park_state`, `set_interrupted_flag`, `set_root_snapshot`,
    /// `set_frame_trace`, `set_vm_state`, `set_gc_block_state` (one write each)
    /// and `set_tlab_addr` / `set_jvm_thread_addr`: what
    /// `jdk_continuation::register_continuation_thread` used to do, with the
    /// same end state (a daemon entry, not yet STW-ready, mounted). The
    /// mirror and `tid` indexes are updated after the write, as
    /// [`Self::register_starting_with_daemon`] and
    /// [`Self::set_java_thread_obj_with_tid`] update them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_prepared_continuation_thread(
        &self,
        thread_id: ThreadId,
        name: &str,
        vthread: ObjectRef,
        java_tid: u64,
        shared: crate::threading::jvm_thread::ThreadSharedArcs,
        tlab_addr: usize,
        jvm_thread_addr: usize,
    ) {
        let crate::threading::jvm_thread::ThreadSharedArcs {
            park_state,
            interrupted,
            root_snapshot,
            frame_trace,
            vm_state,
            gc_block_state,
        } = shared;
        // Wave 73 (`CRATONVM_PARK_ATOMIC_PERMIT`): before the state is
        // reachable by an unpark through this registry (`set_park_state`).
        if self.park_atomic_permit {
            park_state.enable_lock_free();
        }
        let entry = ThreadEntry {
            name: name.to_string(),
            join_handle: None,
            java_thread_obj: Some(vthread),
            java_tid,
            alive: AtomicBool::new(true),
            stw_ready: AtomicBool::new(false),
            daemon: AtomicBool::new(true),
            park_state: park_state.clone(),
            interrupted,
            root_snapshot,
            frame_trace,
            vm_state,
            gc_block_state,
            jmx_monitors: Arc::new(JmxMonitorBook::default()),
            jmx_waiting_monitor: Mutex::new(None),
            jmx_locked_synchronizers: Arc::new(Mutex::new(Vec::new())),
            jmx_wait_started: Mutex::new(None),
            jmx_waited_count: AtomicU64::new(0),
            jmx_waited_nanos: AtomicU64::new(0),
            tlab_addr: std::sync::atomic::AtomicUsize::new(tlab_addr),
            jvm_thread_addr: std::sync::atomic::AtomicUsize::new(jvm_thread_addr),
            os_tid: std::sync::atomic::AtomicU32::new(0),
            os_tid_seq: AtomicU64::new(0),
            mounted: AtomicBool::new(true),
            mount_epoch: AtomicU64::new(0),
            parked_filter: ParkedFoldFilter::new(),
            registered_at: Instant::now(),
            unstructured_monitor_enters: std::sync::atomic::AtomicU32::new(0),
            jmm_counted: AtomicBool::new(false),
            virtual_thread: true,
        };
        self.insert_entry(thread_id, entry);
        self.thread_obj_to_park
            .lock()
            .insert(vthread.as_ptr() as usize, park_state);
        if java_tid != 0 {
            self.java_tid_to_id.lock().insert(java_tid, thread_id);
        }
    }

    /// A JDK continuation's mount, in one `threads` READ (interpreter round i4
    /// wave 95, lane L6, `CRATONVM_VTHREAD_ONE_PUBLISH`): the four publishes
    /// `jdk_continuation::run_slice` made one lock each, in their order --
    /// this OS thread's tid with a fresh ordinal (`set_os_tid_current`), the
    /// `mounted` bit, the `JvmThread` and TLAB addresses -- and the entry's
    /// mirror (`java_thread_obj`), which it read with a fifth. `None` for an
    /// unknown id (nothing published), as each of the five answered then.
    /// With `CRATONVM_DBG_BLOCKED_ACCESS` on, the caller keeps the old calls
    /// (`set_os_tid_current` registers its canary there).
    pub(crate) fn mount_continuation_thread(
        &self,
        thread_id: ThreadId,
        jvm_thread_addr: usize,
        tlab_addr: usize,
    ) -> Option<Option<ObjectRef>> {
        let os_tid = current_os_tid();
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        if os_tid != 0 {
            entry.os_tid.store(os_tid, Ordering::Release);
            let seq = OS_TID_PUBLISH_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            entry.os_tid_seq.store(seq, Ordering::Release);
        }
        // gci merge 2026-10-15: the one-publish mount moves the `mounted` bit
        // too, so it retires the parked fold filter's epoch first, exactly as
        // `set_mounted` does (gci i12/vp).
        entry.mount_epoch.fetch_add(1, Ordering::AcqRel);
        entry.mounted.store(true, Ordering::Release);
        entry
            .jvm_thread_addr
            .store(jvm_thread_addr, Ordering::Release);
        entry.tlab_addr.store(tlab_addr, Ordering::Release);
        Some(entry.java_thread_obj)
    }

    /// The first half of a JDK continuation's death, in ONE `threads` write
    /// with no walk (interpreter round i4 wave 95, lane L6,
    /// `CRATONVM_VTHREAD_ONE_PUBLISH`), for `BlockedGuard::finish_after`
    /// (the GC barrier's transition lock, which every blocked-region enter and
    /// leave in the VM takes, is held across it): `clear_tlab_addr` (the TLAB
    /// and `JvmThread` addresses), then `mark_dead`'s edge -- the OS tid
    /// withdrawn, `alive` lowered and counted in [`Self::dead_entries`], the
    /// join handle dropped. The rest of `mark_dead` -- the mirror index, the
    /// purge of the terminated tail, the `tid` index, the shadow record --
    /// is [`Self::finish_retired_continuation_thread`]'s, after the barrier
    /// lock is released. `None` for an unknown id.
    pub(crate) fn retire_continuation_thread(&self, thread_id: ThreadId) -> Option<RetiredThread> {
        let mut threads = self.threads.write();
        let entry = threads.get_mut(&thread_id)?;
        entry.tlab_addr.store(0, Ordering::Release);
        entry.jvm_thread_addr.store(0, Ordering::Release);
        if mark_dead_clears_os_tid() {
            entry.os_tid.store(0, Ordering::Release);
        }
        let first_death = entry.alive.swap(false, Ordering::AcqRel);
        if first_death {
            self.dead_entries.fetch_add(1, Ordering::Relaxed);
            // zt t15/pl6: a continuation's entry is virtual (never listed);
            // the edge is kept for any other entry retired this way.
            self.jmm_roster_leave_on_death(entry);
        }
        entry.join_handle.take();
        Some(RetiredThread {
            mirror: entry.java_thread_obj,
            park_state: entry.park_state.clone(),
            first_death,
            unstructured_monitor_enters: entry
                .unstructured_monitor_enters
                .load(Ordering::Acquire),
        })
    }

    /// The second half of a JDK continuation's death
    /// ([`Self::retire_continuation_thread`]), outside the GC barrier's lock:
    /// the mirror index entry goes (as in `mark_dead`), and once more than
    /// `TERMINATED_THREAD_ENTRY_CAP + slack` entries are dead -- read from
    /// [`Self::dead_entries`], not counted by a walk -- one purge trims the
    /// tail back to the cap. Answers whether it purged.
    pub(crate) fn finish_retired_continuation_thread(
        &self,
        thread_id: ThreadId,
        retired: &RetiredThread,
        slack: usize,
    ) -> bool {
        if let Some(obj) = retired.mirror {
            self.drop_dead_park_index_entry(obj, &retired.park_state, retired.first_death);
        }
        let limit = TERMINATED_THREAD_ENTRY_CAP.saturating_add(slack);
        let mut retired_java_tids = Vec::new();
        let mut purged = false;
        if self.dead_entries.load(Ordering::Relaxed) > limit {
            let mut threads = self.threads.write();
            // Re-read under the lock: another death may have purged already.
            let dead_count = self.dead_entries.load(Ordering::Relaxed);
            if dead_count > limit {
                self.purge_dead_tail_locked(&mut threads, dead_count, &mut retired_java_tids);
                purged = true;
            }
        }
        if !retired_java_tids.is_empty() {
            let mut by_java_tid = self.java_tid_to_id.lock();
            for java_tid in retired_java_tids {
                by_java_tid.remove(&java_tid);
            }
        }
        crate::threading::thread_state::record_transition_for(
            thread_id.0,
            crate::threading::thread_state::ThreadExecState::Terminated,
            "thread_registry::mark_dead",
        );
        purged
    }

    /// Count one monitor `thread_id` entered outside structured locking
    /// (`ThreadEntry::unstructured_monitor_enters`; interpreter round i4 wave
    /// 95, lane L6, `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`). No-op for an
    /// unknown id.
    pub fn note_unstructured_monitor_enter(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            entry
                .unstructured_monitor_enters
                .fetch_add(1, Ordering::AcqRel);
        }
    }

    /// `thread_id`'s count of [`Self::note_unstructured_monitor_enter`]s not
    /// yet exited. An unknown id answers `u32::MAX` (never "holds nothing").
    pub(crate) fn unstructured_monitor_enters_of(&self, thread_id: ThreadId) -> u32 {
        self.threads.read().get(&thread_id).map_or(u32::MAX, |entry| {
            entry.unstructured_monitor_enters.load(Ordering::Acquire)
        })
    }

    /// The matching successful exit of [`Self::note_unstructured_monitor_enter`]
    /// (never below 0: an exit of a monitor entered by bytecode is not one).
    pub fn note_unstructured_monitor_exit(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            let _ = entry.unstructured_monitor_enters.try_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |n| n.checked_sub(1),
            );
        }
    }

    /// Aliasing-proof identity lookup: registry `ThreadId` by the Java-side
    /// `Thread.tid` value (see `ThreadEntry::java_tid`). Dead threads remain
    /// resolvable (TERMINATED), but a recycled mirror address can never alias
    /// because `tid` values are unique for the life of the process.
    pub fn find_thread_id_by_java_tid(&self, java_tid: u64) -> Option<ThreadId> {
        if java_tid == 0 {
            return None;
        }
        self.java_tid_to_id.lock().get(&java_tid).copied()
    }

    /// ParkState lookup by Java-side `Thread.tid` — the aliasing-proof
    /// counterpart of `find_park_state_by_thread_obj` (see
    /// `find_thread_id_by_java_tid`).
    pub fn find_park_state_by_java_tid(&self, java_tid: u64) -> Option<Arc<ParkState>> {
        let id = self.find_thread_id_by_java_tid(java_tid)?;
        self.get_park_state(id)
    }

    /// `Unsafe.unpark`'s whole registry question in one call (interpreter
    /// round i3 wave 72, lane L6, `CRATONVM_UNPARK_ONE_LOOKUP`): the thread
    /// id and the `ParkState` of the thread whose Java `tid` is `java_tid`.
    /// The two answers `find_thread_id_by_java_tid` and
    /// `find_park_state_by_java_tid` gave separately (the `java_tid` index
    /// taken twice), from one index lookup and one `threads` read.
    pub fn unpark_target_by_java_tid(&self, java_tid: u64) -> Option<(ThreadId, Arc<ParkState>)> {
        let id = self.find_thread_id_by_java_tid(java_tid)?;
        let state = self.get_park_state(id)?;
        Some((id, state))
    }

    /// Pointer-identity walk like `find_thread_id_by_thread_obj`, but guarded
    /// by the caller-supplied Java `tid` of the queried mirror: an entry at
    /// the same address whose recorded `java_tid` differs is a DEAD thread's
    /// stale entry whose mirror address was recycled for the queried (new)
    /// Thread — matching it would misreport a freshly constructed thread as
    /// RUNNABLE/TERMINATED (the DoHead `IllegalThreadStateException`-at-
    /// engine-start flake). An entry with `java_tid == 0` (registered
    /// mid-construction, before the ctor assigned `tid`) is accepted and
    /// lazily backfilled so subsequent lookups take the O(1) tid index.
    pub fn find_thread_id_by_thread_obj_tid_checked(
        &self,
        obj: ObjectRef,
        java_tid: u64,
    ) -> Option<ThreadId> {
        let mut threads = self.threads.write();
        let mut found: Option<ThreadId> = None;
        for (id, entry) in threads.iter() {
            if let Some(thread_obj) = entry.java_thread_obj {
                if std::ptr::eq(thread_obj.as_ptr(), obj.as_ptr())
                    && (entry.java_tid == java_tid || entry.java_tid == 0)
                {
                    found = Some(*id);
                    break;
                }
            }
        }
        if let Some(id) = found {
            if java_tid != 0 {
                let mut backfilled = false;
                if let Some(entry) = threads.get_mut(&id) {
                    if entry.java_tid == 0 {
                        entry.java_tid = java_tid;
                        backfilled = true;
                    }
                }
                drop(threads);
                if backfilled {
                    self.java_tid_to_id.lock().insert(java_tid, id);
                }
            }
        }
        found
    }

    /// Check if a thread is still alive.
    /// Record that `thread_id`'s `java.lang.Thread` ended while its entry
    /// stays alive (see the field `java_ended_thread`).
    pub fn mark_java_side_ended(&self, thread_id: ThreadId) {
        self.java_ended_thread.store(thread_id.0, Ordering::Release);
    }

    /// Did `thread_id`'s `java.lang.Thread` end while its entry stays alive?
    /// One load.
    #[inline]
    pub fn java_side_ended(&self, thread_id: ThreadId) -> bool {
        self.java_ended_thread.load(Ordering::Acquire) == thread_id.0
    }

    /// Record that the ended entry now carries its `DestroyJavaVM` mirror
    /// (see the field `destroy_java_vm_listed`; wave 102, lane L6).
    pub fn mark_destroy_java_vm_listed(&self) {
        self.destroy_java_vm_listed.store(true, Ordering::Release);
    }

    /// Does the ended entry carry a `DestroyJavaVM` mirror? One load.
    #[inline]
    pub fn destroy_java_vm_listed(&self) -> bool {
        self.destroy_java_vm_listed.load(Ordering::Acquire)
    }

    /// Is `thread_obj`, resolved to `thread_id`, a `java.lang.Thread` whose
    /// Java side ended ([`Self::java_side_ended`])? The entry's current
    /// mirror is not, once it is the `DestroyJavaVM` one
    /// ([`Self::destroy_java_vm_listed`]); every other mirror resolved to the
    /// entry (the ended `main`) is.
    pub fn java_side_ended_for(&self, thread_id: ThreadId, thread_obj: ObjectRef) -> bool {
        if !self.java_side_ended(thread_id) {
            return false;
        }
        !(self.destroy_java_vm_listed()
            && self
                .java_thread_obj(thread_id)
                .is_some_and(|m| m.as_ptr() == thread_obj.as_ptr()))
    }

    pub fn is_alive(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.alive.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// `true` only for a thread this registry registered and has since marked
    /// dead — POSITIVE evidence of death, unlike `!is_alive`, which is also true
    /// for an id the registry never saw (the JNI `ThreadId(0)` placeholder
    /// owner, or a unit-test id). Used to release a thin lock whose owner died
    /// before anyone contended it (`vm_exec::release_dead_thin_owner`), where
    /// a false positive would hand a live thread's lock to another thread.
    ///
    /// A dead entry reaped past `TERMINATED_THREAD_ENTRY_CAP` answers `false`:
    /// the recovery then does not happen, which is the behaviour before it
    /// existed, never a wrong release.
    pub(crate) fn is_registered_dead(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .is_some_and(|e| !e.alive.load(Ordering::Acquire))
    }

    /// gcj j3/x2 R7: is `thread_id` registered, alive and able to answer
    /// counted STW barriers ([`Self::mark_stw_ready`])?
    pub fn is_stw_ready(&self, thread_id: ThreadId) -> bool {
        self.threads
            .read()
            .get(&thread_id)
            .is_some_and(|e| e.alive.load(Ordering::Acquire) && e.stw_ready.load(Ordering::Acquire))
    }

    /// Mark a started carrier as able to participate in counted STW barriers.
    pub fn mark_stw_ready(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.stw_ready.store(true, Ordering::Release);
        }
        drop(threads);
        // P1 shadow record — the `Starting -> JavaRunning` edge. Always
        // self-called by the freshly spawned carrier (`vm/src/vm/vm_exec.rs`
        // `thread_start` and the virtual-thread first-mount arm), inside
        // `GcBarrier::run_if_no_stw_requested`, so binding here is safe.
        crate::threading::thread_state::bind_current_thread(thread_id.0);
        crate::threading::thread_state::record_transition(
            crate::threading::thread_state::ThreadExecState::JavaRunning,
            "thread_registry::mark_stw_ready",
        );
    }

    /// Block the calling OS thread until the target thread finishes.
    ///
    /// Takes the `JoinHandle` out of the registry and joins on it.
    /// Returns `true` if the join succeeded, `false` if no handle was found
    /// (already joined, or it's the main thread).
    pub fn join(&self, thread_id: ThreadId) -> bool {
        self.join_giving_up_at(thread_id, None)
    }

    /// [`Self::join`], but a joiner that finds no handle while the thread is
    /// alive gives up at `give_up` (`None`: waits for the death).
    ///
    /// Interpreter round i1 wave 24, lane L7: the no-handle arm used to spin
    /// for about one second and return while the target was still alive. The
    /// arm is not only the spawn/`set_join_handle` race its comment named: a
    /// SECOND concurrent `Thread.join()` of the same thread always lands here,
    /// because the first joiner took the handle. So of two threads joining one
    /// worker that runs longer than a second, the second returned early and
    /// read the worker's results before they existed. `Thread.join()` has no
    /// deadline; only `wait_for_non_daemon_threads`, which re-snapshots its
    /// pending set in a loop and honours its own deadline, bounds the wait.
    fn join_giving_up_at(&self, thread_id: ThreadId, give_up: Option<std::time::Instant>) -> bool {
        let handle = {
            let mut threads = self.threads.write();
            threads
                .get_mut(&thread_id)
                .and_then(|e| e.join_handle.take())
        };
        match handle {
            Some(h) => {
                let _ = h.join();
                // Mark as dead now that join completed
                self.mark_dead(thread_id);
                true
            }
            None => {
                // No handle — either already joined, or it's the main thread.
                // If the thread is dead, return immediately.
                if !self.is_alive(thread_id) {
                    return false;
                }
                // Thread is alive but its handle is elsewhere: another joiner
                // holds it, or the spawn has not stored it yet. Poll for the
                // death (the thread's exit path and the handle holder both
                // `mark_dead`), backing off from 1 ms to 10 ms.
                let mut slice = std::time::Duration::from_millis(1);
                loop {
                    if !self.is_alive(thread_id) {
                        return false;
                    }
                    if give_up.is_some_and(|d| std::time::Instant::now() >= d) {
                        return false;
                    }
                    std::thread::sleep(slice);
                    slice = (slice * 2).min(std::time::Duration::from_millis(10));
                }
            }
        }
    }

    /// Get the Java Thread object for a given ThreadId.
    pub fn java_thread_obj(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        self.threads
            .read()
            .get(&thread_id)
            .and_then(|e| e.java_thread_obj)
    }

    /// Record that thread `tid`'s `java.lang.Thread` mirror just vacated
    /// `old_addr` because a moving / promoting GC relocated it. Enables
    /// [`Self::recover_stale_mirror`] to repair a frame that resumed holding a
    /// stale copy of `old_addr`. Bounded FIFO ([`FORMER_MIRROR_CAP`]). Takes
    /// only the `former_mirror_addrs` lock — callers must NOT hold the
    /// `threads` lock across this (see `update_thread_objs_after_gc`, which
    /// records only after dropping `threads`) so the lock order stays
    /// `threads → former`, opposite to `recover_stale_mirror`'s
    /// `former → threads`; neither nests, so they cannot deadlock.
    fn record_former_mirror_addr(&self, old_addr: usize, tid: ThreadId) {
        let mut g = self.former_mirror_addrs.lock();
        let (map, order) = &mut *g;
        if map.insert(old_addr, tid).is_none() {
            order.push_back(old_addr);
            while order.len() > FORMER_MIRROR_CAP {
                if let Some(evict) = order.pop_front() {
                    map.remove(&evict);
                }
            }
        }
    }

    /// If `stale_addr` is a recorded former address of some thread's
    /// `java.lang.Thread` mirror, return that thread's CURRENT (live,
    /// GC-remapped) mirror so a stale-receiver use site can recover. Identity
    /// preserving — a vacated address uniquely identified one thread's mirror,
    /// so the returned mirror is the right thread's even if a *different*
    /// thread is executing. The caller must still verify the returned object
    /// is live (non-zero header) before use.
    pub fn recover_stale_mirror(&self, stale_addr: usize) -> Option<ObjectRef> {
        let tid = {
            let g = self.former_mirror_addrs.lock();
            *g.0.get(&stale_addr)?
        };
        self.java_thread_obj(tid)
    }

    /// Set the Java Thread object for a given ThreadId, recording the
    /// mirror's Java-side `Thread.tid` (0 = unknown; see
    /// `ThreadEntry::java_tid`) so identity lookups can use the
    /// aliasing-proof tid index instead of the mirror's (recyclable) address.
    pub fn set_java_thread_obj_with_tid(&self, thread_id: ThreadId, obj: ObjectRef, java_tid: u64) {
        self.set_java_thread_obj(thread_id, obj);
        if java_tid != 0 {
            {
                let mut threads = self.threads.write();
                if let Some(entry) = threads.get_mut(&thread_id) {
                    entry.java_tid = java_tid;
                } else {
                    return;
                }
            }
            self.java_tid_to_id.lock().insert(java_tid, thread_id);
        }
    }

    /// Set the Java Thread object for a given ThreadId.
    pub fn set_java_thread_obj(&self, thread_id: ThreadId, obj: ObjectRef) {
        let park_state_clone;
        let prev_obj;
        {
            let mut threads = self.threads.write();
            if let Some(entry) = threads.get_mut(&thread_id) {
                prev_obj = entry.java_thread_obj;
                // zt t15/pl6: a first mirror lists the entry.
                let listed = Self::jmm_roster_lists(entry);
                entry.java_thread_obj = Some(obj);
                if !listed && Self::jmm_roster_lists(entry) {
                    self.jmm_roster_add(entry.daemon.load(Ordering::Acquire), 1);
                }
                park_state_clone = entry.park_state.clone();
            } else {
                return;
            }
        }
        // WP4.1 — keep reverse index in sync. If we replaced an older obj
        // pointer (rare; only happens if Thread is re-bound), drop the
        // stale entry first.
        let mut idx = self.thread_obj_to_park.lock();
        if let Some(prev) = prev_obj {
            if prev.as_ptr() != obj.as_ptr() {
                idx.remove(&(prev.as_ptr() as usize));
            }
        }
        idx.insert(obj.as_ptr() as usize, park_state_clone);
    }

    /// Withdraw a provisional mirror after construction refuses allocation.
    /// A replacement binding is left alone. Java tid aliases name the logical
    /// thread and remain valid if a caller restores its previous mirror.

    pub(crate) fn clear_java_thread_obj_if(&self, thread_id: ThreadId, expected: ObjectRef) {
        let removed = {
            let mut threads = self.threads.write();
            if let Some(entry) = threads.get_mut(&thread_id) {
                if entry.java_thread_obj == Some(expected) {
                    // zt t15/pl6: a withdrawn mirror unlists the entry.
                    if Self::jmm_roster_lists(entry) {
                        self.jmm_roster_add(entry.daemon.load(Ordering::Acquire), -1);
                    }
                    entry.java_thread_obj.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(object) = removed {
            self.thread_obj_to_park
                .lock()
                .remove(&(object.as_ptr() as usize));
        }
    }

    /// Rename a registered thread's entry (the name thread dumps, JVMTI and
    /// hprof read): a JNI-attached thread's placeholder once its mirror takes
    /// the JDK's `Thread-N` (interpreter round i4 wave 84, lane L6). No-op for
    /// an unknown id.
    pub fn set_thread_name(&self, thread_id: ThreadId, name: &str) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.name = name.to_string();
        }
    }

    /// Restart the entry's thread-dump clock (`elapsed=`) now: an entry that
    /// starts carrying a new Java thread, as the launcher's main entry does
    /// at its `DestroyJavaVM` attach (interpreter round i5 wave 104, lane L6;
    /// HotSpot's `elapsed=` counts from the attached `JavaThread`'s creation,
    /// not from `main`'s).
    pub fn restamp_registered_at(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.registered_at = Instant::now();
        }
    }

    /// Get the name of a thread.
    pub fn thread_name(&self, thread_id: ThreadId) -> Option<String> {
        self.threads.read().get(&thread_id).map(|e| e.name.clone())
    }

    /// The Java-side `Thread.tid` for a registry id, or `None` when this entry
    /// has no Java mirror yet (see [`ThreadEntry::java_tid`], 0 = unknown).
    ///
    /// Needed because `ThreadJmxSnapshot::thread_id` carries the JAVA tid
    /// while monitor ownership is keyed by the registry `ThreadId`. Putting a
    /// registry id in `lock_owner_id` would mix two numbering schemes in one
    /// field, and `findDeadlockedThreads` matches `lock_owner_id` against
    /// other threads' `thread_id` to build its wait-for graph — so the
    /// mismatch would silently yield no edges rather than a visible error.
    pub fn java_tid_of(&self, thread_id: ThreadId) -> Option<u64> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.java_tid)
            .filter(|tid| *tid != 0)
    }

    /// The OS-level thread id for a registry id, or `None` if this entry has
    /// not published one yet (0 = unset).
    ///
    /// This is what an ARBITRARY-thread CPU-time read needs — `GetThreadTimes`
    /// after `OpenThread` on Windows, `/proc/self/task/<tid>/stat` on Linux.
    /// `ThreadMXBean.isThreadCpuTimeSupported()` answered false purely because
    /// this was not reachable from a native, not because the VM lacked the
    /// datum: `set_os_tid_current` has been publishing it all along.
    pub fn os_tid_of(&self, thread_id: ThreadId) -> Option<u32> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.os_tid.load(Ordering::Acquire))
            .filter(|tid| *tid != 0)
    }

    /// Publish a monitor acquisition attempt before it can block. The object
    /// is rooted by this registry entry until the acquire completes.
    ///
    /// Through the thread's cached book (round 11 wave 17), not the registry
    /// map: every writer is the contending thread itself, and the closure
    /// touches only the book's mutexes and atomics, never its single-writer
    /// lock stack, so a caller naming another thread (a unit test) is still
    /// sound -- it just misses the cache.
    pub fn set_jmx_contended_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) {
        self.with_self_jmx_monitors(thread_id, |book| {
            *book.contended_monitor.lock() = Some(monitor);
            *book.contended_started.lock() = Some(Instant::now());
            // Arms the two slots above for `complete_jmx_monitor_enter`, whose
            // uncontended callers skip them entirely — see `JmxMonitorBook`.
            book.contending.store(true, Ordering::Release);
            // Count the block HERE, not on release. The JMM counts a thread as
            // having blocked the moment it starts waiting, and a JMX consumer
            // diagnosing a hang reads `getBlockedCount()` while the thread is
            // STILL blocked — counting on the way out leaves exactly that case
            // reporting zero, which is the case that matters. Measured: a
            // thread parked on a held monitor reported `blockedCount == 0` on
            // every thread in the VM until this moved.
            book.blocked_count.fetch_add(1, Ordering::Relaxed);
        });
    }

    /// Finish an acquisition attempt. Successful acquisitions become owned
    /// monitor roots; failed/aborted attempts simply lose the contention root.
    pub fn complete_jmx_monitor_enter(&self, thread_id: ThreadId, monitor: ObjectRef) {
        self.with_self_jmx_monitors(thread_id, |book| {
            // THE CONTENDED SLOTS, ONLY WHEN THERE WAS CONTENTION. This method
            // is the ownership publish for the uncontended arm too, and that arm
            // never called `set_jmx_contended_monitor`, so both slots are
            // already `None` and locking them twice to write `None` over `None`
            // was pure cost on every `monitorenter` in the process.
            if book.contending.load(Ordering::Acquire) || !monitor_fastpath_enabled() {
                *book.contended_monitor.lock() = None;
                // Only the DURATION is added here — the count was taken when
                // the block began. On the book since round 11 wave 17, so the
                // contended arm no longer takes the registry map either.
                if let Some(start) = book.contended_started.lock().take() {
                    let nanos = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                    book.blocked_nanos.fetch_add(nanos, Ordering::Relaxed);
                }
                book.contending.store(false, Ordering::Release);
            }
            book.locked.publish(monitor);
        });
    }

    pub fn remove_jmx_locked_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) {
        self.with_self_jmx_monitors(thread_id, |book| book.locked.retract(monitor));
    }

    /// `(top, spilled)` of the calling thread's own lock stack: the two words
    /// the inline `monitorenter` refuses on (a full stack, a spilled one). For
    /// the `CRATONVM_DBG_JITC` monitor census
    /// (`vm_exec::report_monitor_inline_census`). Self-only, like
    /// [`Self::with_self_jmx_monitors`].
    pub(crate) fn jmx_lock_stack_shape(&self, thread_id: ThreadId) -> Option<(usize, bool)> {
        self.with_self_jmx_monitors(thread_id, |book| {
            (
                book.locked.top.load(Ordering::Relaxed),
                book.locked.spilled.load(Ordering::Relaxed),
            )
        })
    }

    /// Arm `block` (the calling thread's own `JvmThread::jit_monitor_block`)
    /// so compiled code can take an uncontended `monitorenter`/`monitorexit`
    /// inline, recording it in the SAME [`JmxLockStack`] the helpers publish
    /// to. A no-op when already armed from this registry.
    ///
    /// `lease` is asked only when the block is not armed already, and must
    /// answer the calling thread's own thin-lock lease from THIS VM's
    /// `MonitorTable` (`MonitorTable::jit_thin_lease`): the owner bits compiled
    /// code CASes into a mark word and the lease's `LeaseBlock` (the thin-lock
    /// count it keeps, and the inflated-monitor cache it reads).
    ///
    /// Leaves the block unarmed — every compiled site keeps its helper call —
    /// when `CRATONVM_MONITOR_FASTPATH=0`, when this registry does not know the
    /// thread, or when `lease` answers `None` (every lock slot is leased; the
    /// helpers inflate for such a thread). A block armed from ANOTHER registry
    /// is disarmed first, so compiled code can never push into a book, or
    /// count into a lease table, the helpers of this VM do not read.
    ///
    /// Self-only, like [`Self::with_self_jmx_monitors`]: the lock stack has a
    /// single writer and the caller's compiled code is that writer.
    pub(crate) fn arm_jit_monitor_block(
        &self,
        block: &mut crate::threading::jvm_thread::JitMonitorBlock,
        thread_id: ThreadId,
        lease: impl FnOnce() -> Option<crate::threading::monitor::JitThinLease>,
    ) {
        if block.is_armed_for(self.registry_id) {
            return;
        }
        block.disarm();
        if !monitor_fastpath_enabled() {
            return;
        }
        // The lease FIRST (round 11 wave 16,
        // `r11w15-lock-lease-exhaustion-and-carrier-cache-cliff` item 4): a
        // thread refused a lock slot stays unarmed and so comes back here on
        // every helper-path `monitorenter`; its refusal is answered from the
        // thread-local lease cache (`LockSlots::cached_slot`) without the
        // registry read lock, map probe and `Arc` clone below. A thread that
        // gets a lease here would have leased on this same `monitorenter`
        // anyway (`MonitorTable::enter_or_contend`).
        let Some(lease) = lease() else {
            return;
        };
        let Some(book) = self
            .threads
            .read()
            .get(&thread_id)
            .map(|e| Arc::clone(&e.jmx_monitors))
        else {
            return;
        };
        let lock_stack = &book.locked as *const JmxLockStack as usize;
        block.arm(lease, self.registry_id, book, lock_stack);
    }

    /// Run `f` against `thread_id`'s [`JmxMonitorBook`], reaching it through a
    /// per-thread cached handle rather than the registry map.
    ///
    /// Sound because the `Arc` is
    /// created once in `register_with_daemon_stw_ready` and never replaced, so
    /// the cached handle is the same allocation the map holds, forever. The
    /// cache is keyed by `(registry_id, thread_id)` — a monotonic registry id,
    /// not an address, because `ThreadId`s restart per registry and the test
    /// suite builds many registries on one OS thread. Reaping a terminated
    /// entry does not invalidate it: the `Arc` keeps the book alive, and a
    /// thread whose entry is gone is dead.
    ///
    /// Every caller is a SELF-lookup (`monitorenter` / `monitorexit` on the
    /// calling thread), which is what makes the cache hit. It is also a
    /// soundness requirement, not only a performance one: the book's
    /// [`JmxLockStack`] is single-writer, so a publish or retract on ANOTHER
    /// thread's id would race that thread's own writes.
    ///
    /// `f` runs while the thread-local cache is borrowed, so it must not call
    /// back into this method. Every caller is in this file and only touches
    /// the book's own lock stack, mutexes and counters (since round 11 wave 17
    /// the contended-entry publish, `set_jmx_contended_monitor`, and the
    /// blocked-time accounting come through here too; neither touches the
    /// lock stack, so a caller naming another thread stays sound for them).
    fn with_self_jmx_monitors<R>(
        &self,
        thread_id: ThreadId,
        f: impl FnOnce(&JmxMonitorBook) -> R,
    ) -> Option<R> {
        thread_local! {
            static CACHED: std::cell::RefCell<Option<(u64, ThreadId, Arc<JmxMonitorBook>)>> =
                const { std::cell::RefCell::new(None) };
        }
        CACHED.with(|c| {
            if monitor_fastpath_enabled() {
                let cached = c.borrow();
                if let Some((rid, tid, book)) = cached.as_ref() {
                    if *rid == self.registry_id && *tid == thread_id {
                        return Some(f(book));
                    }
                }
            }
            let book = self
                .threads
                .read()
                .get(&thread_id)
                .map(|e| Arc::clone(&e.jmx_monitors))?;
            let r = f(&book);
            if monitor_fastpath_enabled() {
                *c.borrow_mut() = Some((self.registry_id, thread_id, book));
            }
            Some(r)
        })
    }

    pub fn set_jmx_waiting_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            *entry.jmx_waiting_monitor.lock() = Some(monitor);
            *entry.jmx_wait_started.lock() = Some(Instant::now());
        }
    }

    /// The monitor `thread_id` is currently blocked in `Object.wait()` on, if
    /// any — without consuming it or closing the JMX wait-time accounting.
    ///
    /// Used by `Thread.interrupt()` to wake a target parked in `Object.wait()`.
    /// The slot is written just before the park and taken just after it, so a
    /// `Some` here means the target is (or was a moment ago) in the wait, and a
    /// wake sent to a target that has already left is harmless: `wait()` is
    /// specified to permit spurious wakeups, and the surrounding Java `while`
    /// loop re-checks and re-parks.
    pub fn peek_jmx_waiting_monitor(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let threads = self.threads.read();
        let monitor = *threads.get(&thread_id)?.jmx_waiting_monitor.lock();
        monitor
    }

    /// EVERY registered thread that is currently parked in `Object.wait()`,
    /// with the object each one is parked on.
    ///
    /// # Why a census, and not the one thread the watchdog reaches
    ///
    /// The wait-site dump reports the FIRST parked thread it finds, which on
    /// the netty `ParameterizedSslHandlerTest` stalls was always the test thread
    /// — and the test thread is parked on a promise that something *else* was
    /// supposed to complete. A dump that names only that thread cannot tell
    /// "nothing ever completed this promise" from "the thread that would have
    /// completed it is itself parked somewhere", and those two need opposite
    /// investigations. One line per waiter settles it inside the stall that is
    /// already happening, with no second run.
    ///
    /// Rows are `(tid, name, alive, waited_on, waited_ms)`, sorted by tid.
    /// `waited_ms` is `0` when the wait-start stamp is missing, so a caller
    /// never has to unwrap it. The `ObjectRef` comes from the same
    /// `jmx_waiting_monitor` slot [`Self::peek_jmx_waiting_monitor`] reads —
    /// a scanned root that `update_thread_objs_after_gc` forwards — so it is
    /// sound to dereference under a moving collector, unlike `Monitor::wait`'s
    /// own entry-time local.
    pub fn waiting_monitor_census(&self) -> Vec<(u64, String, bool, ObjectRef, u64)> {
        let threads = self.threads.read();
        let mut out: Vec<(u64, String, bool, ObjectRef, u64)> = Vec::new();
        for (tid, entry) in threads.iter() {
            let waiting = *entry.jmx_waiting_monitor.lock();
            let Some(obj) = waiting else {
                continue;
            };
            let started = *entry.jmx_wait_started.lock();
            let waited_ms = started
                .map(|t| t.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0);
            out.push((
                tid.0,
                entry.name.clone(),
                entry.alive.load(Ordering::Acquire),
                obj,
                waited_ms,
            ));
        }
        out.sort_by_key(|r| r.0);
        out
    }

    pub fn take_jmx_waiting_monitor(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        let monitor = entry.jmx_waiting_monitor.lock().take();
        if let Some(start) = entry.jmx_wait_started.lock().take() {
            entry.jmx_waited_count.fetch_add(1, Ordering::Relaxed);
            entry.jmx_waited_nanos.fetch_add(
                start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
        monitor
    }

    /// Publish `monitor` as the object `thread_id` waits on for the span of a
    /// stand-in's blocking region that stands for a JDK `Object.wait` (the
    /// `--compatible` `ReferenceQueue.remove` poll stands for
    /// `lock.wait()`; interpreter round i2 wave 68, lane L1,
    /// `CRATONVM_STAND_IN_WAIT_OBJECTS`). The same slot `monitor_wait`
    /// publishes, so JMX's lock name, JVMTI's contended-monitor and waiter
    /// answers and the wait-site census read it unchanged; it is a scanned
    /// root that a moving collection forwards. Unlike
    /// [`Self::set_jmx_waiting_monitor`] it never overwrites a monitor
    /// already published and starts no wait-time accounting (a poll's
    /// slices are not the JDK's one wait). `true` when it published, which
    /// the caller pairs with exactly one
    /// [`Self::withdraw_region_waiting_monitor`].
    pub fn publish_region_waiting_monitor(&self, thread_id: ThreadId, monitor: ObjectRef) -> bool {
        let threads = self.threads.read();
        let Some(entry) = threads.get(&thread_id) else {
            return false;
        };
        let mut slot = entry.jmx_waiting_monitor.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(monitor);
        true
    }

    /// Take what [`Self::publish_region_waiting_monitor`] published, with no
    /// wait-time accounting (interpreter round i2 wave 68, lane L1).
    pub fn withdraw_region_waiting_monitor(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            *entry.jmx_waiting_monitor.lock() = None;
        }
    }

    /// `AbstractOwnableSynchronizer` has one exclusive owner. Remove a
    /// synchronizer from any former owner before attaching it to the new one.
    ///
    /// # Why this keeps a reverse index instead of scanning
    ///
    /// This runs on **every AQS ownership transition** — `acquire` passes the
    /// owner, `release` passes `null`, so it is twice per uncontended
    /// `ReentrantLock.lock()`/`unlock()` pair (censused in
    /// `probes/LockNativeCensusProbe.java`). The original body walked EVERY
    /// registered thread and took EVERY thread's `jmx_locked_synchronizers`
    /// mutex to `retain` the one it was removing, so the cost of one
    /// uncontended lock grew with the number of threads in the VM — a property
    /// no correct JVM has, and one that a single-threaded microbenchmark like
    /// `probes/AqsBreakdownProbe.java` cannot see at all.
    /// `probes/AqsOwnerScaleProbe.java` is the differential that does.
    ///
    /// `synchronizer_owner` is a derived mirror of the per-thread lists: this
    /// function is their only mutator besides the GC remap in
    /// `update_thread_objs_after_gc` (which rekeys the index in the same pass,
    /// because the key is a heap address and a moving collection relocates it)
    /// and thread reaping. So the previous owner can be looked up instead of
    /// searched for, and the transition touches at most two threads' lists.
    ///
    /// A stale index entry naming a thread that has since exited resolves to
    /// `None` in `threads.get` and is simply dropped — the same no-op the scan
    /// performed for a thread whose entry was already gone.
    /// This thread's own `jmx_locked_synchronizers` list, as a handle it can
    /// hold and reach directly.
    ///
    /// The same shape as [`Self::gc_block_state_of`], and for the same reason:
    /// a thread that needs its own entry on a hot path should not be taking the
    /// registry-wide `RwLock` to find itself in a map. `None` for a thread that
    /// is not registered, whose caller must then use the general
    /// [`Self::set_jmx_owned_synchronizer`].
    ///
    /// Fetch once and keep it: the entry for a given `ThreadId` is inserted
    /// exactly once (`next_id` is monotonic, so ids are never reused), so the
    /// handle cannot go stale while the thread lives. After the thread is
    /// reaped the handle keeps an orphaned list alive and nothing reads it,
    /// which is the correct outcome for a thread that is gone.
    pub fn jmx_locked_synchronizers_of(
        &self,
        thread_id: ThreadId,
    ) -> Option<Arc<Mutex<Vec<ObjectRef>>>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| Arc::clone(&entry.jmx_locked_synchronizers))
    }

    /// [`Self::set_jmx_owned_synchronizer`] for the case AQS actually performs:
    /// the calling thread is the transition's subject, and its own list is the
    /// only one that changes.
    ///
    /// `acquire` passes `Thread.currentThread()` and `release` passes `null`,
    /// both from the owning thread, so `previous` is either `None` or the caller
    /// itself on every transition a `ReentrantLock` generates. The general form
    /// reads the registry `RwLock` and hashes the owner's `ThreadId` to find the
    /// list it already knows — measured at 45.4 ns for one acquire/release pair
    /// in `jit::helpers::jit_native_dispatch_profile`, on a path that runs twice
    /// per uncontended lock/unlock pair.
    ///
    /// A genuine cross-thread steal (`previous` naming a peer) still takes the
    /// registry route for that peer's list, so the recorded state is identical
    /// either way — this only removes a lookup from the case that does not need
    /// one. `own_list` must be the handle [`Self::jmx_locked_synchronizers_of`]
    /// returned for `self_id`; passing any other list would silently split the
    /// record the JMX snapshot reads from the one this writes.
    pub fn set_jmx_owned_synchronizer_own(
        &self,
        self_id: ThreadId,
        own_list: &Mutex<Vec<ObjectRef>>,
        owner: Option<ThreadId>,
        synchronizer: ObjectRef,
    ) {
        debug_assert!(
            owner.is_none() || owner == Some(self_id),
            "set_jmx_owned_synchronizer_own is for a self-owned transition; owner={owner:?} self={self_id:?}"
        );
        let key = synchronizer.as_ptr() as usize;
        let previous = {
            let mut index = self.synchronizer_owner[synchronizer_owner_shard_index(key)].lock();
            match owner {
                Some(owner) => index.insert(key, owner),
                None => index.remove(&key),
            }
        };
        match previous {
            // Ownership was stolen from a peer without it releasing. Not a
            // shape AQS produces; keep it exact rather than fast.
            Some(previous) if previous != self_id => {
                {
                    let threads = self.threads.read();
                    if let Some(entry) = threads.get(&previous) {
                        entry
                            .jmx_locked_synchronizers
                            .lock()
                            .retain(|o| o.as_ptr() != synchronizer.as_ptr());
                    }
                }
                if owner == Some(self_id) {
                    own_list.lock().push(synchronizer);
                }
            }
            // The caller already held it: a release clears it, and a repeated
            // acquire (a reentrant setter re-run) must not double-count.
            Some(_) => {
                if owner.is_none() {
                    own_list
                        .lock()
                        .retain(|o| o.as_ptr() != synchronizer.as_ptr());
                }
            }
            None => {
                if owner == Some(self_id) {
                    own_list.lock().push(synchronizer);
                }
            }
        }
    }

    pub fn set_jmx_owned_synchronizer(&self, owner: Option<ThreadId>, synchronizer: ObjectRef) {
        let key = synchronizer.as_ptr() as usize;
        let previous = {
            let mut index = self.synchronizer_owner[synchronizer_owner_shard_index(key)].lock();
            match owner {
                Some(owner) => index.insert(key, owner),
                None => index.remove(&key),
            }
        };
        let threads = self.threads.read();
        if let Some(previous) = previous {
            if Some(previous) != owner {
                if let Some(entry) = threads.get(&previous) {
                    entry
                        .jmx_locked_synchronizers
                        .lock()
                        .retain(|o| o.as_ptr() != synchronizer.as_ptr());
                }
            }
        }
        if let Some(owner) = owner {
            // `previous == Some(owner)` means this synchronizer was already
            // recorded against this thread (a reentrant acquire re-running the
            // setter); pushing again would double-count it in the snapshot.
            if previous == Some(owner) {
                return;
            }
            if let Some(entry) = threads.get(&owner) {
                entry.jmx_locked_synchronizers.lock().push(synchronizer);
            }
        }
    }

    pub fn jmx_lock_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> Option<(
        Option<ObjectRef>,
        Option<ObjectRef>,
        Vec<ObjectRef>,
        Vec<ObjectRef>,
        i64,
        i64,
        i64,
        i64,
    )> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        let snapshot = (
            *entry.jmx_monitors.contended_monitor.lock(),
            *entry.jmx_waiting_monitor.lock(),
            entry.jmx_monitors.locked.snapshot(),
            entry.jmx_locked_synchronizers.lock().clone(),
            (entry.jmx_monitors.blocked_nanos.load(Ordering::Relaxed) / 1_000_000) as i64,
            entry.jmx_monitors.blocked_count.load(Ordering::Relaxed) as i64,
            (entry.jmx_waited_nanos.load(Ordering::Relaxed) / 1_000_000) as i64,
            entry.jmx_waited_count.load(Ordering::Relaxed) as i64,
        );
        Some(snapshot)
    }

    /// Owner only, at the points that publish its frame trace: record which
    /// of `frames` locked each monitor of this thread's lock stack, for
    /// [`Self::jmx_frame_monitors`] (interpreter round i1 wave 24, lane L7).
    ///
    /// Costs one pass over the frames' two monitor fields when no frame holds
    /// a monitor, and touches neither the registry nor a lock unless a
    /// previous publication must be withdrawn (`published`, the owner's
    /// `JvmThread::jmx_frame_monitors_published`). A monitor a frame names
    /// that the lock stack does not hold (a stale record) is left out.
    pub(crate) fn publish_jmx_frame_monitors(
        &self,
        thread_id: ThreadId,
        frames: &[crate::runtime::frame::Frame],
        published: &AtomicBool,
        trace_positions: Option<(&[u32], usize)>,
        compiled_holds: &[(u32, usize)],
    ) {
        let pairs = crate::runtime::interpreter::held_monitors::frame_locked_monitors(frames);
        // JIT round 19 proposal wave 7, lane gc7 (SYNC619-2): the compiled
        // activations' holds count only with the trace positions they index.
        let compiled_holds = if trace_positions.is_some() {
            compiled_holds
        } else {
            &[]
        };
        if pairs.is_empty() && compiled_holds.is_empty() && !published.load(Ordering::Relaxed) {
            return;
        }
        published.store(
            !pairs.is_empty() || !compiled_holds.is_empty(),
            Ordering::Relaxed,
        );
        let frame_count = frames.len();
        self.with_self_jmx_monitors(thread_id, |book| {
            // SYNC619-2: a publication made only of compiled holds is made
            // only when one of them IS a lock-stack object; otherwise the
            // thread publishes nothing, as before the wave (the reader's
            // "no attribution" answer, not an empty one).
            let keys = if pairs.is_empty() && !compiled_holds.is_empty() {
                let keys = book.locked.snapshot();
                let any = compiled_holds
                    .iter()
                    .any(|&(_, word)| keys.iter().any(|k| k.as_ptr() as usize == word));
                any.then_some(keys)
            } else {
                None
            };
            if pairs.is_empty() && keys.is_none() {
                if book.frame_monitors_published.load(Ordering::Relaxed) {
                    *book.frame_monitors.lock() = FrameMonitorAttribution::default();
                    book.frame_monitors_published
                        .store(false, Ordering::Release);
                }
                return;
            }
            let keys = keys.unwrap_or_else(|| book.locked.snapshot());
            let entries: Vec<(u32, u32)> = pairs
                .iter()
                .filter_map(|&(frame, obj)| {
                    let slot = keys.iter().position(|&k| k == obj)?;
                    Some((u32::try_from(frame).ok()?, u32::try_from(slot).ok()?))
                })
                .collect();
            // The published trace interleaves compiled frames (wave 38, lane
            // L7): a reader of the whole trace needs each frame's index in it.
            let (trace_count, trace_entries) = match trace_positions {
                Some((positions, trace_len)) if positions.len() == frame_count => (
                    trace_len,
                    merge_compiled_hold_entries(
                        entries
                            .iter()
                            .filter_map(|&(frame, slot)| {
                                Some((*positions.get(usize::try_from(frame).ok()?)?, slot))
                            })
                            .collect(),
                        compiled_holds,
                        &keys,
                        trace_len,
                    ),
                ),
                _ => (0, Vec::new()),
            };
            if crate::runtime::env_cache::frame_trace() {
                // JIT round 25: which publication a reader will find.
                eprintln!(
                    "[JMX_PUBLISH] tid={} frames={frame_count} trace={trace_count} \
                     entries={entries:?} trace_entries={trace_entries:?} compiled_holds={}",
                    thread_id.0,
                    compiled_holds.len()
                );
            }
            *book.frame_monitors.lock() = FrameMonitorAttribution {
                frame_count,
                lock_len: keys.len(),
                entries,
                trace_count,
                trace_entries,
            };
            book.frame_monitors_published.store(true, Ordering::Release);
        });
    }

    /// JIT round 19 wave 9, lane gc9 (`CRATONVM_JMX_HELD_NATIVE_FRAME_MONITORS`):
    /// [`Self::publish_jmx_frame_monitors`] made by a frame-trace pause on
    /// behalf of `thread_id`, a thread it holds excused in a native
    /// (`gc_and_alloc::publish_excused_native_traces`), for the trace of
    /// `trace_len` entries it just stored, `positions` naming each of
    /// `frames`' entry index. `compiled_holds` (JIT round 19 wave 11, lane
    /// gc11, GC920-4): the held thread's compiled activations' own holds as
    /// `(trace index, address)` candidates
    /// (`held_monitors::held_thread_compiled_holds`; empty unless
    /// `CRATONVM_JMX_HELD_NATIVE_COMPILED_HOLDS`), kept only when one IS a
    /// lock-stack object, as the owner's own publish keeps them.
    ///
    /// The owner's `JvmThread::jmx_frame_monitors_published` is the flag the
    /// owner's own next publish reads to decide whether a publication must be
    /// withdrawn, so it is set here through the registry's published
    /// `JvmThread` address -- a field-level atomic, never a reference to the
    /// whole thread. Nothing is published when the address is unknown.
    ///
    /// Sound as the pause's trace read is: the excusal holds the thread in its
    /// native, where it neither runs Java nor changes `frames` or its lock
    /// stack's frame records, and cannot exit (so the `JvmThread` stays).
    pub(crate) fn publish_held_thread_frame_monitors(
        &self,
        thread_id: ThreadId,
        frames: &[crate::runtime::frame::Frame],
        positions: &[u32],
        trace_len: usize,
        compiled_holds: &[(u32, usize)],
    ) {
        let addr = self
            .threads
            .read()
            .get(&thread_id)
            .map_or(0, |e| e.jvm_thread_addr.load(Ordering::Acquire));
        if addr == 0 {
            return;
        }
        let thread = addr as *const crate::threading::jvm_thread::JvmThread;
        // SAFETY: see above -- the held thread's `JvmThread` is alive for the
        // pause; only the atomic field is borrowed (`addr_of!` makes no
        // reference to the rest of the thread, which its owner may hold
        // `&mut` across the native).
        let published: &AtomicBool =
            unsafe { &*std::ptr::addr_of!((*thread).jmx_frame_monitors_published) };
        self.publish_jmx_frame_monitors(
            thread_id,
            frames,
            published,
            Some((positions, trace_len)),
            compiled_holds,
        );
    }

    /// Inside a frame-trace pause, by the thread that publishes `thread_id`'s
    /// trace of `trace_len` entries (the thread itself at its poll, or the
    /// pause for a held one): record the lock stack as of now with the pause's
    /// generation `gen` (JIT round 19 wave 12, lane gc12,
    /// `CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`; [`TraceLockRecord`]). A pause
    /// path only -- a blocking deposit records nothing -- so its cost is one
    /// lock-stack snapshot per thread per reader-requested pause.
    pub(crate) fn record_trace_locks(
        &self,
        thread_id: ThreadId,
        trace_len: usize,
        gen: u64,
        moving_gen: u64,
    ) {
        let Some(book) = self
            .threads
            .read()
            .get(&thread_id)
            .map(|e| Arc::clone(&e.jmx_monitors))
        else {
            return;
        };
        let keys = book
            .locked
            .snapshot()
            .iter()
            .map(|o| o.as_ptr() as usize)
            .collect();
        let seq = book.trace_seq.fetch_add(1, Ordering::AcqRel) + 1;
        *book.trace_locks.lock() = Some(TraceLockRecord {
            gen,
            moving_gen,
            trace_len,
            keys,
            seq,
        });
    }

    /// Owner only, at a blocking deposit that replaced its frame trace (JIT
    /// round 25 wave 6, lane W6-F fix 4): the [`TraceLockRecord`] a pause's
    /// LIVE publish left no longer describes the published trace
    /// ([`Self::live_trace_locks`] answers `None`). The record itself is kept
    /// for [`Self::trace_locks`]' readers, as before fix 3 (which cleared it,
    /// and regressed `Object.wait` samples).
    pub(crate) fn note_trace_deposit(&self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            entry.jmx_monitors.trace_seq.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// JIT round 25 wave 9, lane W9-1: [`JmxMonitorBook::trace_seq`] of
    /// `thread_id` now -- bumped by every write of its published frame trace
    /// (a pause's live or excused publish, a blocking deposit) -- or `None`
    /// for an unknown thread. A reader compares two reads to tell whether the
    /// trace was republished in between (`vm_exec::held_trace_is_stale`).
    pub(crate) fn trace_seq(&self, thread_id: ThreadId) -> Option<u64> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| entry.jmx_monitors.trace_seq.load(Ordering::Acquire))
    }

    /// W6-F fix 4: `thread_id`'s [`TraceLockRecord`] when it describes the
    /// trace published NOW -- the live one a frame-trace pause made the
    /// thread publish while it ran, not replaced by a deposit since.
    pub(crate) fn live_trace_locks(&self, thread_id: ThreadId) -> Option<TraceLockRecord> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        let record = book.trace_locks.lock().clone()?;
        (record.seq == book.trace_seq.load(Ordering::Acquire)).then_some(record)
    }

    /// Any thread: `thread_id`'s [`TraceLockRecord`], if any.
    pub(crate) fn trace_locks(&self, thread_id: ThreadId) -> Option<TraceLockRecord> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        let record = book.trace_locks.lock().clone();
        record
    }

    /// [`Self::jmx_frame_monitors`] resolved against `keys` -- the lock stack
    /// a [`TraceLockRecord`] kept with the trace -- instead of the current one
    /// (JIT round 19 wave 12, lane gc12): the publication must describe a
    /// trace of `frame_count` entries and a lock stack of `keys.len()`.
    pub(crate) fn jmx_frame_monitors_against(
        &self,
        thread_id: ThreadId,
        frame_count: usize,
        keys: &[ObjectRef],
    ) -> Option<Vec<(usize, ObjectRef)>> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        if !book.frame_monitors_published.load(Ordering::Acquire) {
            return None;
        }
        let attribution = book.frame_monitors.lock();
        let entries = if attribution.frame_count == frame_count {
            &attribution.entries
        } else if attribution.trace_count != 0 && attribution.trace_count == frame_count {
            &attribution.trace_entries
        } else {
            return None;
        };
        if keys.len() != attribution.lock_len {
            return None;
        }
        entries
            .iter()
            .map(|&(frame, slot)| {
                let obj = *keys.get(usize::try_from(slot).ok()?)?;
                Some((usize::try_from(frame).ok()?, obj))
            })
            .collect()
    }

    /// Any thread: `thread_id`'s published frame attribution as `(frame index
    /// from the bottom, object)` pairs, resolved against its CURRENT lock stack
    /// (whose slots a moving collection remaps in place). `None` -- report
    /// without attribution -- when nothing is published, when the publication
    /// belongs to a trace of a different length than the `frame_count` the
    /// caller is reporting, or when the lock stack changed length since.
    pub fn jmx_frame_monitors(
        &self,
        thread_id: ThreadId,
        frame_count: usize,
    ) -> Option<Vec<(usize, ObjectRef)>> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        if !book.frame_monitors_published.load(Ordering::Acquire) {
            if crate::runtime::env_cache::frame_trace() {
                eprintln!("[JMX_FRAME_MONITORS] tid={} nothing published", thread_id.0);
            }
            return None;
        }
        let attribution = book.frame_monitors.lock();
        // Indexed by interpreter frame (the debugger's reader, and a trace
        // with no compiled frames), or into the whole published trace when it
        // interleaves compiled ones (wave 38, lane L7).
        let entries = if attribution.frame_count == frame_count {
            &attribution.entries
        } else if attribution.trace_count != 0 && attribution.trace_count == frame_count {
            &attribution.trace_entries
        } else {
            if crate::runtime::env_cache::frame_trace() {
                // Round 19 wave 9 (lane gc9): why a published attribution
                // was not used.
                eprintln!(
                    "[JMX_FRAME_MONITORS] tid={} asked={frame_count} published frames={} trace={}",
                    thread_id.0, attribution.frame_count, attribution.trace_count
                );
            }
            return None;
        };
        let keys = book.locked.snapshot();
        if keys.len() != attribution.lock_len {
            if crate::runtime::env_cache::frame_trace() {
                eprintln!(
                    "[JMX_FRAME_MONITORS] tid={} lock stack {} != published {}",
                    thread_id.0,
                    keys.len(),
                    attribution.lock_len
                );
            }
            return None;
        }
        entries
            .iter()
            .map(|&(frame, slot)| {
                let obj = *keys.get(usize::try_from(slot).ok()?)?;
                Some((usize::try_from(frame).ok()?, obj))
            })
            .collect()
    }

    /// JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`):
    /// [`Self::jmx_frame_monitors`] for a reader that holds a TRACE of
    /// `trace_len` entries (another thread's published trace with stand-in
    /// rows stripped from its top): the publication must index a trace of
    /// that length -- its `trace_entries` when it interleaves compiled
    /// frames, else its per-frame `entries` when the trace is one entry per
    /// interpreter frame. Never the per-frame list of an interleaved trace
    /// whose interpreter-frame count merely equals `trace_len` (another index
    /// space).
    pub(crate) fn jmx_trace_monitors(
        &self,
        thread_id: ThreadId,
        trace_len: usize,
    ) -> Option<Vec<(usize, ObjectRef)>> {
        let threads = self.threads.read();
        let book = &threads.get(&thread_id)?.jmx_monitors;
        if !book.frame_monitors_published.load(Ordering::Acquire) {
            return None;
        }
        let attribution = book.frame_monitors.lock();
        let entries = if attribution.trace_count != 0 {
            (attribution.trace_count == trace_len).then_some(&attribution.trace_entries)?
        } else {
            (attribution.frame_count == trace_len).then_some(&attribution.entries)?
        };
        let keys = book.locked.snapshot();
        if keys.len() != attribution.lock_len {
            return None;
        }
        entries
            .iter()
            .map(|&(frame, slot)| {
                let obj = *keys.get(usize::try_from(slot).ok()?)?;
                Some((usize::try_from(frame).ok()?, obj))
            })
            .collect()
    }

    pub fn reset_jmx_contention_stats(&self) {
        for entry in self.threads.read().values() {
            entry.jmx_monitors.blocked_count.store(0, Ordering::Relaxed);
            entry.jmx_monitors.blocked_nanos.store(0, Ordering::Relaxed);
            entry.jmx_waited_count.store(0, Ordering::Relaxed);
            entry.jmx_waited_nanos.store(0, Ordering::Relaxed);
            *entry.jmx_monitors.contended_started.lock() = None;
            *entry.jmx_wait_started.lock() = None;
        }
    }

    /// Every registered thread's thread-dump facts, in registry id order
    /// (the order the threads registered; HotSpot lists its threads in the
    /// order they were added). Interpreter round i3 wave 81, lane L6: one
    /// registry read, cold (`jcmd Thread.print`, `jstack`, `SIGQUIT`).
    pub fn thread_dump_entries(&self) -> Vec<ThreadDumpEntry> {
        let mut out: Vec<ThreadDumpEntry> = self
            .threads
            .read()
            .iter()
            .map(|(tid, e)| ThreadDumpEntry {
                thread_id: *tid,
                name: e.name.clone(),
                alive: e.alive.load(Ordering::Acquire),
                daemon: e.daemon.load(Ordering::Acquire),
                java_thread_obj: e.java_thread_obj,
                java_tid: Some(e.java_tid).filter(|t| *t != 0),
                os_tid: Some(e.os_tid.load(Ordering::Acquire)).filter(|t| *t != 0),
                registered_at: e.registered_at,
                jvm_thread_addr: e.jvm_thread_addr.load(Ordering::Acquire),
                blocked_wait_lo: e.gc_block_state.blocked_wait_lo.load(Ordering::Acquire),
            })
            .collect();
        out.sort_by_key(|e| e.thread_id.0);
        out
    }

    /// Return all (ThreadId, name) pairs for currently registered threads.
    pub fn all_thread_names(&self) -> Vec<(ThreadId, String)> {
        self.threads
            .read()
            .iter()
            .map(|(tid, entry)| (*tid, entry.name.clone()))
            .collect()
    }

    /// Set the park state for a given ThreadId (used when spawning threads
    /// to share the JvmThread's ParkState with the registry).
    pub fn set_park_state(&self, thread_id: ThreadId, state: Arc<ParkState>) {
        // Wave 73 (`CRATONVM_PARK_ATOMIC_PERMIT`): before the state is
        // reachable by an unpark through this registry.
        if self.park_atomic_permit {
            state.enable_lock_free();
        }
        let obj_opt;
        {
            let mut threads = self.threads.write();
            if let Some(entry) = threads.get_mut(&thread_id) {
                entry.park_state = state.clone();
                obj_opt = entry.java_thread_obj;
            } else {
                return;
            }
        }
        // WP4.1 — keep reverse index pointing at the new ParkState so
        // unpark(Thread) hits the same instance the parked thread is
        // blocked on.
        if let Some(obj) = obj_opt {
            self.thread_obj_to_park
                .lock()
                .insert(obj.as_ptr() as usize, state);
        }
    }

    /// Get the park state for a given ThreadId (for unpark from another thread).
    pub fn get_park_state(&self, thread_id: ThreadId) -> Option<Arc<ParkState>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.park_state.clone())
    }

    /// The `ParkState` and interrupt flag a thread being started adopts
    /// (interpreter round i3 wave 74, lane L6; page
    /// `i73-L6-unpark-in-the-thread-start-window-is-lost-20261106`): the ones
    /// its registry entry already has, so an `unpark` (or an interrupt) that
    /// reached the entry between its registration and the start's
    /// `set_park_state` -- the `java_tid` index and `eetop` are live from
    /// early in `start0` -- lands on the state the thread will park on, as
    /// HotSpot's `Unsafe_Unpark` reaches the new thread's `Parker` once
    /// `JavaThread::prepare` has run. Fresh ones only for an unregistered id.
    pub fn starting_thread_states(&self, thread_id: ThreadId) -> (Arc<ParkState>, Arc<AtomicBool>) {
        if let Some(entry) = self.threads.read().get(&thread_id) {
            return (entry.park_state.clone(), entry.interrupted.clone());
        }
        let park_state = Arc::new(ParkState::new());
        if self.park_atomic_permit {
            park_state.enable_lock_free();
        }
        (park_state, Arc::new(AtomicBool::new(false)))
    }

    /// Every `Arc` `thread_id`'s entry shares with its `JvmThread` (park
    /// state, interrupt flag, root snapshot, frame trace, VM-state
    /// breadcrumb, blocked-region GC state), cloned under ONE `threads` read
    /// lock (`CRATONVM_THREAD_START_ONE_PUBLISH`, interpreter round i3 wave
    /// 82, lane L6). The starting thread adopts them
    /// (`JvmThread::with_shared`), so its entry is complete from
    /// registration on -- as HotSpot's `Threads::add` publishes a prepared
    /// thread once -- and no `set_*` write lock re-installs them. `None` for
    /// an unregistered id.
    pub(crate) fn starting_thread_shared(
        &self,
        thread_id: ThreadId,
    ) -> Option<crate::threading::jvm_thread::ThreadSharedArcs> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        Some(crate::threading::jvm_thread::ThreadSharedArcs {
            park_state: entry.park_state.clone(),
            interrupted: entry.interrupted.clone(),
            root_snapshot: entry.root_snapshot.clone(),
            frame_trace: entry.frame_trace.clone(),
            vm_state: entry.vm_state.clone(),
            gc_block_state: entry.gc_block_state.clone(),
        })
    }

    /// Find the park state for a thread identified by its Java Thread object.
    /// Used by `Unsafe.unpark(Thread)` and `LockSupport.unpark(Thread)`.
    ///
    /// WP4.1 — O(1) via the reverse index. Falls back to a linear walk
    /// of `threads` only if the index lookup misses, which should never
    /// happen for a thread that has been properly registered with a
    /// non-null `java_thread_obj`. The fallback exists to keep tests
    /// that construct registries by hand (without the modern register
    /// path) working.
    pub fn find_park_state_by_thread_obj(&self, obj: ObjectRef) -> Option<Arc<ParkState>> {
        if let Some(ps) = self.thread_obj_to_park.lock().get(&(obj.as_ptr() as usize)) {
            return Some(ps.clone());
        }
        // Fallback: legacy registries that bypassed register_with_daemon.
        let threads = self.threads.read();
        for entry in threads.values() {
            if let Some(thread_obj) = entry.java_thread_obj {
                if std::ptr::eq(thread_obj.as_ptr(), obj.as_ptr()) {
                    return Some(entry.park_state.clone());
                }
            }
        }
        None
    }

    /// DBG (`CRATONVM_DBG_UNPARK_MISS`): snapshot of every registered
    /// thread's `(tid, java_thread_obj address, alive)` so an unpark whose
    /// mirror lookup missed can print what the registry believes the live
    /// mirror addresses are (a stale `Node.waiter` shows up as a caller
    /// address absent from this list).
    pub fn debug_thread_obj_addrs(&self) -> Vec<(u64, usize, bool)> {
        let threads = self.threads.read();
        threads
            .iter()
            .map(|(id, e)| {
                (
                    id.0,
                    e.java_thread_obj.map_or(0, |o| o.as_ptr() as usize),
                    e.alive.load(std::sync::atomic::Ordering::Relaxed),
                )
            })
            .collect()
    }

    /// WP4.1 — Find the `ThreadId` for a registered thread by its Java
    /// `Thread` mirror object.  Real-JDK-mode `Thread` objects do not have
    /// a single fixed slot we can write a `ThreadId` into (the class layout
    /// stores `tid` inside an inner `FieldHolder`), so callers that used
    /// to read field 2 directly must instead walk the registry.
    ///
    /// Pointer-identity comparison matches the existing
    /// `find_park_state_by_thread_obj` helper.
    pub fn find_thread_id_by_thread_obj(&self, obj: ObjectRef) -> Option<ThreadId> {
        let threads = self.threads.read();
        for (id, entry) in threads.iter() {
            if let Some(thread_obj) = entry.java_thread_obj {
                if std::ptr::eq(thread_obj.as_ptr(), obj.as_ptr()) {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Set the interrupted flag for a thread (cross-thread interrupt).
    pub fn set_interrupted(&self, thread_id: ThreadId, value: bool) {
        let woken = {
            let threads = self.threads.read();
            let Some(entry) = threads.get(&thread_id) else {
                return;
            };
            // SeqCst (round 14 wave 5, lane monitor2, RV5-1): with the
            // `fence(SeqCst)` in `MonitorTable::wake_waiter_for_interrupt` and
            // the SeqCst enrolment re-check in `Monitor::wait`, a waiter that
            // inflates its monitor after the interrupter read the mark thin
            // still sees this flag (the store-buffering pattern needs SeqCst).
            entry
                .interrupted
                .store(value, std::sync::atomic::Ordering::SeqCst);
            // Interpreter round i3 wave 74, lane L6
            // (`CRATONVM_PARK_UNTIMED_NO_POLL`, default off): a park no
            // longer polls the flag, so raising it wakes the park (after the
            // `threads` lock is dropped).
            // Wave 75 (`CRATONVM_SLEEP_NO_POLL`): a sleep waits on the same
            // state's condvar, so the same wake ends it.
            (value && (self.park_no_poll || self.sleep_no_poll)).then(|| entry.park_state.clone())
        };
        if let Some(park_state) = woken {
            // Wave 76 (`CRATONVM_PARK_WAKE_AFTER_UNLOCK`): the wake after the
            // state's mutex is released (`ParkState::unpark_after_unlock`).
            park_state.wake_for_interrupt_with(self.park_wake_after_unlock);
        }
    }

    /// [`Self::set_interrupted`]'s wake of a raised interrupt
    /// (`CRATONVM_PARK_UNTIMED_NO_POLL` / `CRATONVM_SLEEP_NO_POLL`), WITHOUT
    /// the flag store: for `Thread.interrupt`, whose flag was already raised
    /// with the Java field under the target's interrupt lock
    /// (`vm_exec::raise_thread_interrupt`, nio21 wave 5 L1). Merged at nio21.
    pub fn wake_raised_interrupt(&self, thread_id: ThreadId) {
        if !(self.park_no_poll || self.sleep_no_poll) {
            return;
        }
        let woken = self
            .threads
            .read()
            .get(&thread_id)
            .map(|entry| entry.park_state.clone());
        if let Some(park_state) = woken {
            park_state.wake_for_interrupt_with(self.park_wake_after_unlock);
        }
    }

    /// Raise a thread's interrupt flag WITHOUT waking it, and hand back its
    /// `ParkState` and the monitor it waits on in `Object.wait()` (the JMX
    /// record [`Self::peek_jmx_waiting_monitor`] reads), all under ONE
    /// `threads` read (interpreter round i3 wave 76, lane L6;
    /// `CRATONVM_INTERRUPT_ONE_LOOKUP` and `CRATONVM_PARK_WAKE_AFTER_UNLOCK`).
    /// `Thread.interrupt` wakes the state itself afterwards: with
    /// `CRATONVM_PARK_WAKE_AFTER_UNLOCK` once
    /// (`ParkState::interrupt_wake_after_unlock`: the permit and every
    /// waiter, park or sleep, in one trip through the state's mutex, where
    /// the default takes it twice -- the wake of [`Self::set_interrupted`] and
    /// the unpark after it); otherwise with [`Self::set_interrupted`]'s wake
    /// and the unpark, as before. The flag store is
    /// [`Self::set_interrupted`]'s (`SeqCst`, same reason). `None`: no such
    /// thread (nothing was raised).
    ///
    /// nio21 merge: `raise == false` hands back the same pair WITHOUT the
    /// flag store, for a caller that already raised the flag together with
    /// the Java field under the target's interrupt lock
    /// (`vm_exec::raise_thread_interrupt`, nio21 wave 5 L1); a second store
    /// after the lock is released could re-raise an interrupt a consume took
    /// in between and split the flag from the field again.
    pub fn raise_interrupt(
        &self,
        thread_id: ThreadId,
        raise: bool,
    ) -> Option<(Arc<ParkState>, Option<ObjectRef>)> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        if raise {
            entry
                .interrupted
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let monitor = *entry.jmx_waiting_monitor.lock();
        Some((entry.park_state.clone(), monitor))
    }

    /// Get the interrupted flag Arc for a thread (to share with JvmThread).
    pub fn get_interrupted_flag(&self, thread_id: ThreadId) -> Option<Arc<AtomicBool>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.interrupted.clone())
    }

    /// Set the interrupted flag Arc for a thread (share JvmThread's flag with registry).
    pub fn set_interrupted_flag(&self, thread_id: ThreadId, flag: Arc<AtomicBool>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.interrupted = flag;
        }
    }

    /// Set the root snapshot Arc for a thread (share JvmThread's snapshot with registry).
    /// The objects thread `thread_id` last deposited as its roots (the
    /// C JVMTI table's heap walk reports them as the thread's stack
    /// references; interpreter round i2 wave 63, lane L1). Empty for an
    /// unknown thread.
    pub fn root_snapshot_of(&self, thread_id: ThreadId) -> Vec<ObjectRef> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| entry.root_snapshot.lock().clone())
            .unwrap_or_default()
    }

    pub fn set_root_snapshot(&self, thread_id: ThreadId, snapshot: Arc<Mutex<Vec<ObjectRef>>>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.root_snapshot = snapshot;
            // gci i12/vp: a different snapshot; the parked fold filter is stale.
            entry.mount_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Share the JvmThread's frame-trace Arc with the registry, so other threads
    /// can read this thread's published call stack.
    pub fn set_frame_trace(
        &self,
        thread_id: ThreadId,
        trace: Arc<Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,
    ) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.frame_trace = trace;
        }
    }

    /// Share the JvmThread's VM-state breadcrumb with the registry.
    pub fn set_vm_state(&self, thread_id: ThreadId, state: Arc<Mutex<String>>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            entry.vm_state = state;
        }
    }

    /// Read a copy of `thread_id`'s last-published frame trace (call stack),
    /// innermost frame first. Empty if the thread is unknown or never deposited.
    ///
    /// **Every entry has `line_number == -1`.** The depositor
    /// (`stackwalker::capture_frames_no_lines`, called from every blocking
    /// deposit point) is deliberately lock-free and takes no `ClassStore`
    /// borrow, so it cannot resolve lines. Prefer
    /// [`frame_trace_of_resolved`](Self::frame_trace_of_resolved) at any call
    /// site that has a `ClassStore` in hand — a thread dump then gains source
    /// lines it does not have today, at zero cost to the deposit path.
    pub fn frame_trace_of(&self, thread_id: ThreadId) -> Vec<cratonvm_native_api::StackTraceEntry> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|e| e.frame_trace.lock().clone())
            .unwrap_or_default()
    }

    /// gcg g3/k (`common-a-proposal-per-thread-handshakes`, step 1): ask
    /// `thread_id` to run `op` on itself, without a pause.
    ///
    /// * the thread is unknown or dead: [`HandshakeRequest::Gone`], `op`
    ///   handed back;
    /// * its `in_blocked_region` is up: [`HandshakeRequest::TargetBlocked`],
    ///   `op` handed back -- the target executes no code and its deposit
    ///   (root snapshot, frame trace) describes its frames, so the requester
    ///   works from those (the parked fast path cross-thread
    ///   `getStackTrace()` already takes);
    /// * otherwise the op is queued on the target's [`GcBlockState`] and the
    ///   ticket returned: the target runs it at its next
    ///   `JvmThread::run_pending_handshakes`. A requester that stops waiting
    ///   (the target parked meanwhile, or never polls) takes it back with
    ///   `state.handshakes.withdraw(&ticket)` -- on the SAME `GcBlockState`
    ///   it was queued on, which the variant carries (the entry's state can be
    ///   replaced by `set_gc_block_state` meanwhile) -- and falls back to
    ///   whatever it did before (the blocked path, or a pause), so the op
    ///   runs exactly once or not at all.
    ///
    /// Classified under the registry read lock, so the entry cannot be
    /// removed in between; the blocked flag can still rise right after the
    /// read, which the ticket's timeout and the withdrawal cover.
    pub fn request_handshake(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
    ) -> HandshakeRequest {
        let threads = self.threads.read();
        let Some(entry) = threads.get(&thread_id) else {
            return HandshakeRequest::Gone(op);
        };
        if !entry.alive.load(Ordering::Acquire) || entry.gc_block_state.owner_dropped() {
            return HandshakeRequest::Gone(op);
        }
        if entry
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
        {
            return HandshakeRequest::TargetBlocked(op);
        }
        let ticket = entry.gc_block_state.handshakes.push(op);
        HandshakeRequest::Queued {
            state: Arc::clone(&entry.gc_block_state),
            ticket,
        }
    }

    /// gci i23/hs (step 2 of `common-a-proposal-per-thread-handshakes`): ask
    /// `thread_id` to run `op` on itself ([`Self::request_handshake`]) and
    /// wait for it, up to `budget`, in `slice`s.
    ///
    /// Between slices the wait stops early when the target entered a blocked
    /// region ([`HandshakeOutcome::TargetBlocked`]: its deposit is current),
    /// was dropped ([`HandshakeOutcome::Gone`]), or `abort` answers true
    /// ([`HandshakeOutcome::NotServed`]). The requester is a running mutator,
    /// not GC-safe while it waits, so the caller passes "a pause is
    /// requested" as `abort`: a pause then waits at most one `slice` for it.
    /// Whenever it stops early or the budget runs out, the op is withdrawn
    /// from the SAME queue it was pushed on; a withdrawal that finds it gone
    /// means the target already took it, and the wait goes on for that op's
    /// end (it takes no pause and no lock the requester holds), bounded by
    /// [`HANDSHAKE_TAKEN_OP_WAIT`]. So the op runs exactly once
    /// ([`HandshakeOutcome::Ran`]) or never -- except past that bound: a
    /// taken op that has not ended within [`HANDSHAKE_TAKEN_OP_WAIT`] answers
    /// [`HandshakeOutcome::NotServed`] although it is running (its later
    /// effect is harmless, see the constant), and the caller takes its
    /// fallback pause as well. (gcj j2/cm2: this said "exactly once or never"
    /// without the exception; `gcj-j1cm-safepoint-barrier-and-registry-review-residuals`
    /// item 3.)
    pub fn handshake_and_wait(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
        budget: std::time::Duration,
        slice: std::time::Duration,
        abort: impl FnMut() -> bool,
    ) -> HandshakeOutcome {
        self.handshake_and_wait_kicked(thread_id, op, budget, slice, abort, || {})
    }

    /// gcj j1/cm: [`Self::handshake_and_wait`] plus a kick. `kick` runs once,
    /// right after the op was queued and before the first wait, and never
    /// when nothing was queued (`Gone` / `TargetBlocked` at the request).
    /// The frame-trace client kicks every interpreter dispatch loop's poll
    /// word (`GcBarrier::note_code_moved_on_every_loop`), so an interpreted
    /// target that allocates nothing still reaches its poll
    /// (`interpreter::loop_top_poll` runs `run_pending_handshakes`) at its
    /// next bytecode instead of at its next allocation slow path (step "P4/ph"
    /// of `common-a-proposal-per-thread-handshakes`). The push's `Release`
    /// store of the queue word happens before `kick`'s `Release` add, so a
    /// loop whose `Acquire` read of its poll word sees the kick also sees the
    /// op queued.
    pub fn handshake_and_wait_kicked(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
        budget: std::time::Duration,
        slice: std::time::Duration,
        abort: impl FnMut() -> bool,
        kick: impl FnOnce(),
    ) -> HandshakeOutcome {
        self.handshake_and_wait_kicking_target(thread_id, op, budget, slice, abort, |_| kick())
    }

    /// gcj j2/cm2: [`Self::handshake_and_wait_kicked`] whose `kick` is handed
    /// the TARGET's [`GcBlockState`] (the one the op was queued on), so it can
    /// kick only the poll word the target's loops run on
    /// (`GcBlockState::kick_published_loop_poll_word`,
    /// `CRATONVM_GC_HANDSHAKE_TARGET_KICK`). Same contract otherwise.
    pub fn handshake_and_wait_kicking_target(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
        budget: std::time::Duration,
        slice: std::time::Duration,
        abort: impl FnMut() -> bool,
        kick: impl FnOnce(&GcBlockState),
    ) -> HandshakeOutcome {
        self.handshake_and_wait_kicking_twice(thread_id, op, budget, slice, abort, kick, |_| {})
    }

    /// t1 lane E wave 3 (`t1-V2-handshake-poll-bit-stale-carrier-arm-skips-the-kick-fallback`):
    /// [`Self::handshake_and_wait_kicking_target`] plus a second chance.
    /// `rekick` runs at most once, with the same [`GcBlockState`], when the
    /// FIRST slice ended with the op still pending and the wait goes on (the
    /// target neither blocked nor gone, no abort, budget left). The
    /// frame-trace client uses it to fall back to the loop kicks after a
    /// handshake-bit arm that turned out stale (a remount racing its
    /// owner check, or another VM's poll on the same OS thread lowering the
    /// bit); a prompt target has settled long before. Same contract otherwise.
    ///
    /// t1 wave 8 lane HANDSHK (`CRATONVM_GC_HANDSHAKE_UNMOUNTED_DEFERS_KICK`,
    /// opt-in): when the target is UNMOUNTED right after the push (a virtual
    /// thread inside `run_slice`'s unmount window: `set_mounted(false)` came
    /// before its deposit raises `in_blocked_region`), `kick` is not run then
    /// -- the target polls no loop until it remounts, and a remount's first
    /// dispatch-loop top polls the queue anyway -- but after the first slice,
    /// in `rekick`'s place, and only when the op is still pending and the wait
    /// goes on; `rekick` then gets the next such slice. The common case (the
    /// deposit raises the flag within the slice) ends `TargetBlocked` with no
    /// kick at all, instead of a kick of every dispatch loop of the VM.
    #[allow(clippy::too_many_arguments)]
    pub fn handshake_and_wait_kicking_twice(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
        budget: std::time::Duration,
        slice: std::time::Duration,
        mut abort: impl FnMut() -> bool,
        kick: impl FnOnce(&GcBlockState),
        rekick: impl FnOnce(&GcBlockState),
    ) -> HandshakeOutcome {
        use crate::threading::jvm_thread::HandshakeState;
        let mut rekick = Some(rekick);
        let (state, ticket) = match self.request_handshake(thread_id, op) {
            HandshakeRequest::Gone(_) => return HandshakeOutcome::Gone,
            HandshakeRequest::TargetBlocked(_) => return HandshakeOutcome::TargetBlocked,
            HandshakeRequest::Queued { state, ticket } => (state, ticket),
        };
        // Read after the push: a remount after this read finds the op queued
        // at its first loop top; one before it is seen as mounted (kicked now).
        let mut deferred_kick = if self.handshake_unmounted_defers_kick
            && !self.is_mounted(thread_id)
        {
            Some(kick)
        } else {
            kick(&*state);
            None
        };
        let settled = |s: HandshakeState| match s {
            HandshakeState::Ran => Some(HandshakeOutcome::Ran),
            // gci i24/rv: `Abandoned` is the owner's drop (`abandon_all`,
            // which runs after the drop notice raised `owner_dropped`) -- or
            // an op that UNWOUND on a live owner (`SettleOnUnwind`: that op,
            // or one queued before it, panicked). The second published
            // nothing, so it is not served: `Gone` made the frame-trace
            // client skip its pause and hand back a stale trace.
            HandshakeState::Abandoned if state.owner_dropped() => Some(HandshakeOutcome::Gone),
            HandshakeState::Abandoned => Some(HandshakeOutcome::NotServed),
            // Only this requester withdraws its ticket.
            HandshakeState::Withdrawn => Some(HandshakeOutcome::NotServed),
            HandshakeState::Pending => None,
        };
        let deadline = Instant::now() + budget;
        // gcj j1/cm (`CRATONVM_GC_HANDSHAKE_SPIN_WAIT`, default ON, `=0`
        // kills): wait each slice by polling the ticket with `yield_now`, not
        // by a timed condvar wait. A timed wait of 100 us is rounded up to the
        // OS timer tick where the target never notifies (a JIT-compiled peer
        // that does not poll within the budget): on Windows' default 15.6 ms
        // tick the 1 ms budget became ~13 ms per sample before the
        // frame-trace pause (vi23-hs L3 on the dev base: p50 13363 us on vs
        // 477 us off). Polling ends each slice on time; the requester yields
        // its core meanwhile.
        let spin = self.handshake_spin_wait;
        // t1 lane E2 (`CRATONVM_GC_HANDSHAKE_WAIT_SEES_BLOCK`, opt-in): the spin
        // also watches the target's blocked flag and drop notice, so a target
        // that parks, unmounts (a virtual thread's yield deposit) or enters a
        // GC-safe native AFTER the op was queued is answered from its deposit
        // at once -- HotSpot's requester processes the handshake itself for a
        // target that reached a safe state -- not at the end of the slice
        // (100 us for the frame trace; the deposit never polls, so the op
        // could only be withdrawn there). `saw_block`: the flag was up inside
        // the spin; the target may have left the region again by the check
        // below, and its deposit of that instant is still the answer.
        let sees_block = self.handshake_wait_sees_block;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut saw_block = false;
            let now_state = if spin {
                let until = Instant::now() + slice.min(left);
                loop {
                    let s = ticket.state();
                    if s != HandshakeState::Pending || Instant::now() >= until {
                        break s;
                    }
                    if sees_block
                        && (state.in_blocked_region.load(Ordering::Acquire)
                            || state.owner_dropped())
                    {
                        saw_block = true;
                        break s;
                    }
                    std::thread::yield_now();
                }
            } else {
                ticket.wait_timeout(slice.min(left))
            };
            if let Some(done) = settled(now_state) {
                return done;
            }
            let blocked = saw_block || state.in_blocked_region.load(Ordering::Acquire);
            let gone = state.owner_dropped();
            let aborted = abort();
            if !(blocked || gone || aborted || Instant::now() >= deadline) {
                if let Some(kick) = deferred_kick.take() {
                    kick(&*state);
                } else if let Some(rekick) = rekick.take() {
                    rekick(&*state);
                }
                continue;
            }
            if state.handshakes.withdraw(&ticket).is_some() {
                return if gone {
                    HandshakeOutcome::Gone
                } else if blocked {
                    HandshakeOutcome::TargetBlocked
                } else {
                    HandshakeOutcome::NotServed
                };
            }
            // Taken by the target (running it now) or abandoned by its drop.
            if aborted {
                // When abort() stopped the wait (a pause was requested), the requester is a
                // counted, non-GC-safe mutator holding that pause. Wait in short slices rather
                // than holding up to HANDSHAKE_TAKEN_OP_WAIT in a single block.
                let taken_deadline = Instant::now() + HANDSHAKE_TAKEN_OP_WAIT;
                let poll_slice = slice.min(std::time::Duration::from_millis(5));
                while Instant::now() < taken_deadline {
                    let s = ticket.wait_timeout(poll_slice);
                    if let Some(done) = settled(s) {
                        return done;
                    }
                }
                return HandshakeOutcome::NotServed;
            }
            return settled(ticket.wait_timeout(HANDSHAKE_TAKEN_OP_WAIT))
                .unwrap_or(HandshakeOutcome::NotServed);
        }
    }

    /// `CRATONVM_GC_HANDSHAKE_FRAME_TRACE` for this VM, read once (gci i23/hs;
    /// default ON since JIT round 23 wave 6, `=0` kills): does a cross-thread `getStackTrace()` of a running peer ask
    /// it to publish its trace through a handshake
    /// ([`Self::handshake_and_wait`]) before it falls back to the frame-trace
    /// pause? One load of a field fixed at construction.
    #[inline(always)]
    pub fn handshake_frame_trace(&self) -> bool {
        self.handshake_frame_trace
    }

    /// `CRATONVM_GC_HANDSHAKE_LOOP_KICK` for this VM, read once (gcj j1/cm;
    /// default ON, `=0` kills; reached only under the opt-in
    /// [`Self::handshake_frame_trace`]): does the frame-trace handshake kick
    /// every interpreter dispatch loop's poll word after queueing
    /// ([`Self::handshake_and_wait_kicked`])? `=0`: the gci i23/hs reach (an
    /// interpreted target serves at its next allocation slow path only).
    #[inline(always)]
    pub fn handshake_loop_kick(&self) -> bool {
        self.handshake_loop_kick
    }

    /// `CRATONVM_DBG_STW_EXPECTED_IDS` for this VM, latched at construction
    /// (gcj j2/cm2): print the census's expected identity set per collection
    /// request (`[stw-expected]`) and each blocking park's arrival
    /// (`[stw-park]`). Diagnostic; presence of the variable turns it on.
    #[inline(always)]
    pub fn dbg_stw_expected_ids(&self) -> bool {
        self.dbg_stw_expected_ids
    }

    /// `CRATONVM_GC_HANDSHAKE_TARGET_KICK` for this VM, read once (gcj j2/cm2,
    /// opt-in; `gcj-j1cm-proposal-target-only-handshake-kick`): does a dispatch
    /// loop entry publish the poll word it runs on
    /// (`GcBlockState::publish_loop_poll_word`), and the frame-trace
    /// handshake kick only its target's published word instead of every loop
    /// of the VM? The requester side is Generational only (G1 and ZGC keep the
    /// kick-all); the publication is one compare per loop entry.
    #[inline(always)]
    pub fn handshake_target_kick(&self) -> bool {
        self.handshake_target_kick
    }

    /// `CRATONVM_GC_HANDSHAKE_POLL_BIT` for this VM, read once (t1 lane E,
    /// opt-in; step 3 of `common-a-proposal-per-thread-handshakes`): does a
    /// dispatch loop entry publish the poll word it runs on (as under
    /// [`Self::handshake_target_kick`]), and the frame-trace handshake raise
    /// the HANDSHAKE bit of its target's published word
    /// (`GcBlockState::arm_published_loop_poll_word_handshake`) instead of
    /// kicking loops? The bit sits in the byte compiled polls test under
    /// `CRATONVM_JIT_POLL_THREAD_WORD`, so a JIT-compiled peer that calls no
    /// helper and allocates nothing serves the handshake at its next poll
    /// rather than costing its requester the 1 ms budget plus the pause.
    /// Every backend (the bit is per thread and no collector reads it).
    #[inline(always)]
    pub fn handshake_poll_bit(&self) -> bool {
        self.handshake_poll_bit
    }

    /// t1 lane E wave 3 (`t1-V2-handshake-poll-bit-stale-carrier-arm-skips-the-kick-fallback`):
    /// the OS tid `thread_id` runs on NOW, for a handshake-bit arm's owner
    /// check (`GcBlockState::arm_published_loop_poll_word_handshake`): `None`
    /// for an unknown or dead entry, an UNMOUNTED continuation (its `os_tid` is
    /// its last carrier's, kept for the census), or no published tid. One
    /// registry read lock.
    pub(crate) fn mounted_os_tid(&self, thread_id: ThreadId) -> Option<u32> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        if !entry.alive.load(Ordering::Acquire) || !entry.mounted.load(Ordering::Acquire) {
            return None;
        }
        Some(entry.os_tid.load(Ordering::Acquire)).filter(|&t| t != 0)
    }

    /// Hand every registered thread's published frame snapshot to `f`, each
    /// under its own lock, to be rewritten in place. A self-contained walk
    /// under the registry read guard: `f` must not call back into the
    /// registry. Interpreter round i1 wave 22, lane L3
    /// (`obsolete_frames::resolve_captured_lines_of_class`).
    pub(crate) fn for_each_frame_trace(
        &self,
        mut f: impl FnMut(&mut Vec<cratonvm_native_api::StackTraceEntry>),
    ) {
        let threads = self.threads.read();
        for entry in threads.values() {
            let mut trace = entry.frame_trace.lock();
            f(&mut trace);
        }
    }

    /// Hand every registered thread's `GcBlockState` to `f` (an unmounted
    /// virtual thread's boxed `JvmThread` included: its entry shares that
    /// `Arc` too), under the registry read guard: `f` must not call back into
    /// the registry. The stale-frame census of interpreter round i1 waves
    /// 23-24, lane L3 (`obsolete_frames::prune_histories_by_census`), which
    /// reads `obsolete_frame_floor`, `in_blocked_region` and
    /// `blocked_frame_classes`.
    pub(crate) fn for_each_gc_block_state(&self, mut f: impl FnMut(&GcBlockState)) {
        let threads = self.threads.read();
        for entry in threads.values() {
            f(&entry.gc_block_state);
        }
    }

    /// [`frame_trace_of`](Self::frame_trace_of) with source line numbers filled
    /// in from `class_store`.
    ///
    /// ARCH-2026-07-26 (`cross-owner-closeout`, request CR-SW-2 of
    /// `stackwalk-and-vtable.md`). The published
    /// snapshot is line-less because the *depositor* must stay lock-free; the
    /// *reader* usually does hold a `ClassStore` (cross-thread
    /// `Thread.getStackTrace()`, `dumpThreads()`, the JMX thread dump), so it
    /// can pay for the resolution once, only when a dump is actually taken.
    ///
    /// Strictly additive: `stackwalker::resolve_line_numbers_in_place` is
    /// fail-closed — it yields the line an eager capture would have produced or
    /// leaves `-1` — so this can only replace unknowns with correct lines. It
    /// never produces a wrong line, and it never touches an entry that already
    /// has one (including the `-2` native sentinel).
    ///
    /// Resolution is exact for frames carrying `StackTraceEntry::method_index`;
    /// the deposit path cannot compute one (it has no `ClassStore`), so those
    /// frames fall back to the unambiguous-name rule and overloaded frames stay
    /// at `-1`. That is still strictly more than the all-`-1` snapshot.
    ///
    /// The registry lock is released before the resolution runs: the walk over
    /// the returned `Vec` touches no registry state, and holding L5 across a
    /// `ClassStore` walk would invert the usual acquisition order.
    pub fn frame_trace_of_resolved(
        &self,
        thread_id: ThreadId,
        class_store: &crate::classloading::ClassStore,
    ) -> Vec<cratonvm_native_api::StackTraceEntry> {
        let mut trace = self.frame_trace_of(thread_id);
        crate::runtime::stackwalker::resolve_line_numbers_in_place(class_store, &mut trace);
        trace
    }

    /// xt-hardening follow-up (2026-07-03): OS tids of alive threads
    /// currently `in_blocked_region` (excluded from the barrier, covered by
    /// `deposit_root_snapshot`, whose JIT-chain scan -- eager, or deferred to
    /// `resolve_deferred_blocked_jit_chains` -- leaves the Rust frames around
    /// the chain on the native stack uncovered; gci i25/rv corrected "never
    /// scans the JIT band", false since `24cf0b19b0`). This is the ONLY gap `helper_window_pass` exists
    /// to close (its own doc comment says so); a cooperatively-arrived
    /// mutator already published its JIT roots via `update_root_snapshot`
    /// before parking at the barrier, so re-scanning it is pure redundant
    /// over-retention risk. Threads with `os_tid == 0` (registered but not
    /// yet started) cannot be blocked, so they never contribute a false 0.
    /// JIT round 21 lane refs, REFS21-6 (opt-in
    /// `CRATONVM_XT_HELPER_WINDOW_PARK_CREDIT`): the OS tids of blocked,
    /// mounted threads whose block is an armed Java-level park
    /// (`GcBlockState::park_credit_armed`) AND whose flag-raising deposit ran
    /// its compiled-chain scan (`park_credit_deposited`: eagerly, rooting and
    /// pinning the compiled frames in the deposit itself, or deferred to the
    /// collector's walk this pause, `resolve_deferred_blocked_jit_chains`). A
    /// tid shared by an entry that does not qualify (a mounted virtual thread
    /// and its carrier) is not credited.
    pub fn park_credited_os_tids(&self) -> Vec<u32> {
        let threads = self.threads.read();
        let mut credited: Vec<u32> = Vec::new();
        let mut refused: Vec<u32> = Vec::new();
        for e in threads.values() {
            if !(entry_is_stw_live(e)
                && e.mounted.load(Ordering::Acquire)
                && e.gc_block_state.in_blocked_region.load(Ordering::Acquire))
            {
                continue;
            }
            let tid = e.os_tid.load(Ordering::Acquire);
            if tid == 0 {
                continue;
            }
            let state = &e.gc_block_state;
            if state.park_credit_armed.load(Ordering::Acquire)
                && state.park_credit_deposited.load(Ordering::Acquire)
                && !state.owner_dropped()
            {
                credited.push(tid);
            } else {
                refused.push(tid);
            }
        }
        credited.retain(|t| !refused.contains(t));
        credited.sort_unstable();
        credited.dedup();
        credited
    }

    pub fn blocked_os_tids(&self) -> Vec<u32> {
        // JIT round 16 wave 9b (lane ffm): a thread in foreign code is never
        // signalled (`ffm_in_native_signal_exempt`).
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| {
                // gen r5w2/roots6: `mounted` -- a parked (unmounted) virtual
                // thread has nothing on its last carrier's stack, and that
                // carrier is not blocked at all.
                entry_is_stw_live(e)
                    && e.mounted.load(Ordering::Acquire)
                    && e.gc_block_state.in_blocked_region.load(Ordering::Acquire)
                    && !(exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state))
            })
            .map(|e| e.os_tid.load(Ordering::Acquire))
            .filter(|&t| t != 0)
            .collect()
    }

    /// zwr: the delivery threads' block states (`GcBlockState::delivery_thread`),
    /// for the take-over to read their published Rust band floor once frozen
    /// (`TakenOver::set_rust_band_floors`).
    pub(crate) fn delivery_thread_states(&self) -> Vec<(u32, Arc<GcBlockState>)> {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| e.gc_block_state.delivery_thread.load(Ordering::Acquire))
            .filter_map(|e| {
                let t = e.os_tid.load(Ordering::Acquire);
                (t != 0).then(|| (t, Arc::clone(&e.gc_block_state)))
            })
            .collect()
    }

    /// JIT round 18 wave 10b (lane ffmeintr, `CRATONVM_FFM_LAZY_SIGNAL_GATE`):
    /// `(os_tid, block state)` of every alive, mounted entry with a published
    /// OS tid, for the Linux take-over pass to gate its signal on
    /// (`TakenOver::set_signal_gates`). One registry read and one `Arc` clone
    /// per thread, per pass.
    pub(crate) fn signal_gate_states(&self) -> Vec<(u32, Arc<GcBlockState>)> {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| entry_is_stw_live(e) && e.mounted.load(Ordering::Acquire))
            .filter_map(|e| {
                let t = e.os_tid.load(Ordering::Acquire);
                (t != 0).then(|| (t, Arc::clone(&e.gc_block_state)))
            })
            .collect()
    }

    /// JIT round 18 wave 10 (lane ffmeintr, EINTR2-1,
    /// `CRATONVM_XT_HELPER_UNPARKED_READ`): the blocked, mounted threads that
    /// are inside a blocked-wait trampoline right now
    /// (`GcBlockState::blocked_wait_lo` non-zero), with their block state, for
    /// the helper-window pass to read without parking. A snapshot: the pass
    /// re-checks each one with `try_hold_blocked_wait_band` and parks any
    /// thread that has left its trampoline since.
    pub(crate) fn blocked_wait_band_states(&self) -> Vec<(u32, Arc<GcBlockState>)> {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| {
                entry_is_stw_live(e)
                    && e.mounted.load(Ordering::Acquire)
                    && e.gc_block_state.in_blocked_region.load(Ordering::Acquire)
                    && e.gc_block_state.blocked_wait_lo.load(Ordering::Acquire) != 0
            })
            .filter_map(|e| {
                let t = e.os_tid.load(Ordering::Acquire);
                (t != 0).then(|| (t, Arc::clone(&e.gc_block_state)))
            })
            .collect()
    }

    /// [`Self::blocked_os_tids`], split in one registry read (gcd d2/i, opt-in
    /// `CRATONVM_XT_BLOCKED_MONITOR_PROOF`): the blocked threads that proved
    /// their JIT entry chain rewritable at a block in the compiled
    /// `monitorenter` helper, as `(os_tid, proven depth)`, and every other
    /// blocked thread's OS tid, exactly as `blocked_os_tids` lists it.
    ///
    /// The row is read only for an entry whose `in_blocked_region` is already
    /// seen up (Acquire), and the owner settles it before raising the flag
    /// (Release), so a proven depth read here is the one its current block's
    /// deposit proved. `0` and the armed marker read as "not proven".
    pub fn blocked_os_tids_split_by_monitor_proof(&self) -> (Vec<u32>, Vec<(u32, usize)>) {
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        let mut rest = Vec::new();
        let mut proven = Vec::new();
        for e in threads.values() {
            if !(entry_is_stw_live(e)
                && e.mounted.load(Ordering::Acquire)
                && e.gc_block_state.in_blocked_region.load(Ordering::Acquire))
            {
                continue;
            }
            // JIT round 16 wave 9b (lane ffm): see `blocked_os_tids`.
            if exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state) {
                continue;
            }
            let tid = e.os_tid.load(Ordering::Acquire);
            if tid == 0 {
                continue;
            }
            let depth = e
                .gc_block_state
                .monitor_block_proven_jit_depth
                .load(Ordering::Acquire);
            if depth != 0 && depth != crate::threading::jvm_thread::MONITOR_BLOCK_PROOF_ARMED {
                proven.push((tid, depth));
            } else {
                rest.push(tid);
            }
        }
        (rest, proven)
    }

    /// gci i20/vh (`CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`, default
    /// ON since gci i24/jp; a row whose owner was dropped counts as one
    /// without a record, `deferred_chain_row`):
    /// the OS tids of the blocked peers whose CURRENT blocking deposit deferred
    /// a complete compiled-chain walk to the collector
    /// (`GcBlockState::deferred_jit_chain` is `Some`), i.e. the peers whose
    /// compiled frames this pause's `resolve_deferred_blocked_jit_chains` marks
    /// and pins from their frozen bands. Same roster rule as
    /// [`Self::blocked_os_tids`]. A tid shared with a blocked entry WITHOUT a
    /// deferred record (a mounted virtual thread shares its carrier's tid) is
    /// left out: its band is not covered by a record. Sorted, deduplicated.
    ///
    /// Read during a pause: a blocked thread cannot leave its region (and so
    /// cannot take or replace its record) until the pause ends, so the answer
    /// holds for the helper-window pass that consumes it. Lock order is the
    /// collector's own (`for_each_gc_block_state` then the record), as in
    /// `resolve_deferred_blocked_jit_chains`.
    /// ki3 (`CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY`): the OS tids
    /// of the blocked peers whose current window's deposit covered their whole
    /// compiled chain -- deferred it to this pause's walk (a
    /// `deferred_jit_chain` record) or walked it itself
    /// (`deposit_walked_jit_chain`). The roster rules of
    /// [`Self::blocked_os_tids_with_deferred_jit_chain`]: an owner that is not
    /// dropped, and no blocked entry on the tid without the cover.
    pub(crate) fn blocked_os_tids_with_covered_jit_chain(&self) -> Vec<u32> {
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        let mut rows: Vec<(u32, bool)> = Vec::new();
        for e in threads.values() {
            if !(entry_is_stw_live(e)
                && e.mounted.load(Ordering::Acquire)
                && e.gc_block_state.in_blocked_region.load(Ordering::Acquire))
            {
                continue;
            }
            if exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state) {
                continue;
            }
            let tid = e.os_tid.load(Ordering::Acquire);
            if tid == 0 {
                continue;
            }
            let covered = deferred_chain_row(
                e.gc_block_state.owner_dropped(),
                e.gc_block_state
                    .deposit_walked_jit_chain
                    .load(Ordering::Acquire)
                    || e.gc_block_state.deferred_jit_chain.lock().is_some(),
            );
            rows.push((tid, covered));
        }
        drop(threads);
        deferred_chain_peer_tids(rows)
    }

    pub(crate) fn blocked_os_tids_with_deferred_jit_chain(&self) -> Vec<u32> {
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        let mut rows: Vec<(u32, bool)> = Vec::new();
        for e in threads.values() {
            if !(entry_is_stw_live(e)
                && e.mounted.load(Ordering::Acquire)
                && e.gc_block_state.in_blocked_region.load(Ordering::Acquire))
            {
                continue;
            }
            // JIT round 16 wave 9b (lane ffm): see `blocked_os_tids`.
            if exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state) {
                continue;
            }
            let tid = e.os_tid.load(Ordering::Acquire);
            if tid == 0 {
                continue;
            }
            // gci i24/jp: the resolve's own filter (`owner_dropped`), so a
            // demoted tid is always one `resolve_deferred_blocked_jit_chains`
            // walks this pause (`deferred_chain_row`).
            let deferred = deferred_chain_row(
                e.gc_block_state.owner_dropped(),
                e.gc_block_state.deferred_jit_chain.lock().is_some(),
            );
            rows.push((tid, deferred));
        }
        drop(threads);
        deferred_chain_peer_tids(rows)
    }

    /// Mark a VM-registered native carrier thread as parked in host-native code.
    ///
    /// These threads do not own interpreter frames while parked, so their
    /// authoritative root snapshot is empty. The flag still matters: moving-GC
    /// fixups and STW diagnostics use it to classify the thread as blocked.
    pub fn mark_native_thread_blocked(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.root_snapshot.lock().clear();
            entry.frame_trace.lock().clear();
            // No deposit took the thread's frame classes for this block: the
            // stale-frame census must count it for every class (interpreter
            // round i1 wave 24, lane L3). Before the flag goes up.
            entry.gc_block_state.blocked_frame_classes.lock().exact = false;
            entry
                .gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
    }

    /// Whether a live registered thread is in a GC-safe blocking region.
    ///
    /// The same transition is the authoritative source for Java
    /// `Thread.State.WAITING`: it is published before monitor/AQS/native waits
    /// and cleared only after the thread wakes and applies post-GC fixups.
    pub fn is_blocked(&self, thread_id: ThreadId) -> bool {
        let threads = self.threads.read();
        threads.get(&thread_id).is_some_and(|entry| {
            entry.alive.load(Ordering::Acquire)
                && entry
                    .gc_block_state
                    .in_blocked_region
                    .load(Ordering::Acquire)
        })
    }

    /// The monitor `thread_id` contends for, as JVMTI
    /// `GetCurrentContendedMonitor` and JDWP `CurrentContendedMonitor` answer
    /// it: the monitor a `monitorenter` (or a synchronized method's entry)
    /// contends for, else -- while the thread is BLOCKED -- the monitor its
    /// wait stands on, which a notified waiter re-entering it contends for
    /// (JDK-8256314's rule; interpreter round i2 waves 55 and 56, lane L6).
    /// `None` for an unknown thread or no contention.
    pub fn current_contended_monitor(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let (contended, waiting, ..) = self.jmx_lock_snapshot(thread_id)?;
        contended.or_else(|| waiting.filter(|_| self.java_block_state(thread_id) == 2))
    }

    /// The Java blocking state for a currently blocked thread: 1 is WAITING,
    /// 2 is BLOCKED, 3 is TIMED_WAITING. Returns 0 for running, dead, and
    /// unknown threads.
    ///
    /// A thread that has published a contended monitor entry
    /// (`set_jmx_contended_monitor`) and not completed it is BLOCKED even
    /// while it is still a running mutator: the opt-in running park
    /// (`CRATONVM_MONITOR_LAZY_PARK_US`, `vm_exec::lazy_monitor_park`) waits
    /// for the monitor there without entering the GC-blocked region, and read
    /// RUNNABLE for its whole park (round 12 wave 1, lane lock). HotSpot too
    /// reports BLOCKED from the moment a contended enter starts waiting.
    pub fn java_block_state(&self, thread_id: ThreadId) -> u8 {
        let threads = self.threads.read();
        let Some(entry) = threads
            .get(&thread_id)
            .filter(|entry| entry.alive.load(Ordering::Acquire))
        else {
            return 0;
        };
        self.java_block_state_of_entry(entry)
    }

    /// The C JVMTI table's `GetThreadState` facts of thread `thread_id` from
    /// ONE `threads` read (interpreter round i6 wave 132, lane L1; the first
    /// "next step" of i130-L1b, `CRATONVM_JVMTI_THREAD_STATE_ONE_READ`,
    /// performance, default off): `(java_block_state, published wait kind,
    /// interrupted)`, each exactly what [`Self::java_block_state`],
    /// [`Self::published_wait_kind_of`] and [`Self::get_interrupted_flag`]
    /// answer, where the three took the lock three times and the last cloned
    /// and dropped the flag's `Arc` (two RMWs). The wait kind is `None` when
    /// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` is off, or when the thread is not
    /// waiting (block state other than 1 / 3): the caller asks it only then.
    /// The three are read under one lock hold rather than three, so a state
    /// that changes meanwhile is seen at one instant instead of three; every
    /// answer remains one the three separate reads could have given.
    pub fn jvmti_state_facts(&self, thread_id: ThreadId) -> (u8, Option<PublishedWaitKind>, bool) {
        self.jvmti_state_facts_and_suspend_word(thread_id).0
    }

    /// [`Self::jvmti_state_facts`] and, under the same `threads` read, the
    /// thread's C JVMTI state suspend word (`GcBlockState::jvmti_state_suspended`;
    /// interpreter round i7 wave 135, lane L1, `CRATONVM_JVMTI_STATE_SUSPEND_WORD`).
    /// The word is `JVMTI_STATE_SUSPEND_UNPUBLISHED` for an unknown thread.
    pub fn jvmti_state_facts_and_suspend_word(
        &self,
        thread_id: ThreadId,
    ) -> ((u8, Option<PublishedWaitKind>, bool), u8) {
        let threads = self.threads.read();
        let Some(entry) = threads.get(&thread_id) else {
            return (
                (0, None, false),
                crate::threading::jvm_thread::JVMTI_STATE_SUSPEND_UNPUBLISHED,
            );
        };
        let word = entry.gc_block_state.jvmti_state_suspended.load(Ordering::Acquire);
        let interrupted = entry.interrupted.load(Ordering::Acquire);
        if !entry.alive.load(Ordering::Acquire) {
            // `java_block_state` answers 0 (RUNNABLE): no wait kind is asked.
            return ((0, None, interrupted), word);
        }
        let block = self.java_block_state_of_entry(entry);
        let kind = (self.published_all_wait_kinds && matches!(block, 1 | 3))
            .then(|| gc_block_state_wait_kind(&entry.gc_block_state));
        ((block, kind, interrupted), word)
    }

    /// Thread `thread_id`'s C JVMTI state suspend word alone, one `threads`
    /// read and no `Arc` clone (interpreter round i7 wave 135, lane L1;
    /// [`Self::jvmti_state_facts_and_suspend_word`] without the facts).
    pub fn jvmti_state_suspend_word(&self, thread_id: ThreadId) -> u8 {
        self.threads.read().get(&thread_id).map_or(
            crate::threading::jvm_thread::JVMTI_STATE_SUSPEND_UNPUBLISHED,
            |entry| entry.gc_block_state.jvmti_state_suspended.load(Ordering::Acquire),
        )
    }

    /// [`Self::java_block_state`] of a live entry, under the caller's
    /// `threads` read (wave 132: shared with [`Self::jvmti_state_facts`]).
    fn java_block_state_of_entry(&self, entry: &ThreadEntry) -> u8 {
        if entry
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
        {
            // A native blocked in C (wave 59, lane L6) is RUNNABLE.
            return match entry.gc_block_state.java_state.load(Ordering::Acquire) {
                crate::threading::jvm_thread::JAVA_STATE_IN_NATIVE => 0,
                state => state,
            };
        }
        if entry.jmx_monitors.contending.load(Ordering::Acquire)
            && entry.jmx_monitors.contended_monitor.lock().is_some()
        {
            return 2;
        }
        // A thread inside `Object.wait` that is not (yet, or no longer)
        // GC-blocked: the release-first wait publishes WAITING /
        // TIMED_WAITING to `holder.threadStatus` before it releases the
        // monitor, then parks as a running mutator for up to the lazy-park
        // budget (`CRATONVM_MONITOR_WAIT_LAZY_PARK`) before it deposits. Its
        // waiting monitor is published for that whole span (taken before the
        // wait restores RUNNABLE), and `java_state` was stored before it, so
        // the wait's own state (2 while it re-enters) answers here. Under
        // `--jdk-only` the JDK's `Thread.getState()` reads the mirror's word
        // and said WAITING while JVMTI and JDWP, which read this, said
        // RUNNABLE (interpreter round i2 wave 57, lane L1; probe
        // `L1W57JvmtiWaitStates`).
        //
        // Interpreter round i2 wave 64, lane L1 (`CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
        // the wait's published kind spans exactly the waiting monitor's span
        // (stored before it, lowered after it is taken; `vm_exec::monitor_wait_with_state`),
        // so one atomic load answers where the monitor's mutex was taken for
        // every RUNNABLE thread asked.
        if self.published_all_wait_kinds {
            // Wave 65 (lane L1, `CRATONVM_SERVED_WAIT_KINDS`): a served
            // stand-in's monitor wait for JDK code that parks spans the same
            // lazy phase.
            let kind = entry.gc_block_state.java_wait_kind.load(Ordering::Acquire);
            if matches!(
                kind,
                crate::threading::jvm_thread::JAVA_WAIT_KIND_OBJECT_WAIT
                    | crate::threading::jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED
            )
                // Interpreter round i3 wave 71, lane L1 (`CRATONVM_PARK_LAZY`):
                // a park's running phase publishes its state and kind first.
                || (self.park_lazy_budget.is_some()
                    && kind == crate::threading::jvm_thread::JAVA_WAIT_KIND_PARKED)
            {
                let state = entry.gc_block_state.java_state.load(Ordering::Acquire);
                if matches!(state, 1..=3) {
                    return state;
                }
            }
        } else if entry.jmx_waiting_monitor.lock().is_some() {
            let state = entry.gc_block_state.java_state.load(Ordering::Acquire);
            if matches!(state, 1..=3) {
                return state;
            }
        }
        0
    }

    /// Is live thread `thread_id` blocked in a registered native that stands
    /// in for a JDK `native` method blocking in C -- a `BlockKind::InNative`
    /// region, `jvm_thread::JAVA_STATE_IN_NATIVE` (interpreter round i2 wave
    /// 59, lane L6, `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE`)? Such a thread is
    /// RUNNABLE ([`Self::java_block_state`] answers 0) and in native.
    pub fn blocked_in_native(&self, thread_id: ThreadId) -> bool {
        self.gc_block_state_of(thread_id)
            .is_some_and(|block| gc_block_state_blocked_in_native(&block))
    }

    /// Does live thread `thread_id` run native code with no Java above it, as
    /// HotSpot's `_thread_in_native` (JMX `ThreadInfo.isInNative()`, the C
    /// JVMTI table's `IN_NATIVE`)? A native blocked in C
    /// ([`Self::blocked_in_native`]), a JNI native's C code under the C JVMTI
    /// table's mark (`GcBlockState::jvmti_in_native`), a GC-safe FFM downcall
    /// in its in-native region, or a lazy FFM downcall. The one place the
    /// in-native readers ask (interpreter round i2 wave 59, lane L6).
    pub fn thread_in_native(&self, thread_id: ThreadId) -> bool {
        let Some(block) = self.gc_block_state_of(thread_id) else {
            return false;
        };
        block.jvmti_in_native.load(Ordering::Acquire)
            || (block.foreign_native.load(Ordering::Acquire)
                && block.in_blocked_region.load(Ordering::Acquire))
            || block.lazy_native.load(Ordering::Acquire)
                != crate::threading::jvm_thread::LAZY_NATIVE_OFF
            || gc_block_state_blocked_in_native(&block)
    }

    /// Is live thread `thread_id` inside `Thread.sleep`, as its own blocking
    /// region published it (a `BlockKind::Sleeping` region:
    /// `GcBlockState::java_wait_kind` is `jvm_thread::JAVA_WAIT_KIND_SLEEPING`
    /// under a TIMED_WAITING `java_state` while the flag is up)? Interpreter
    /// round i2 wave 63, lane L1, stage 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`.
    ///
    /// `None` without `CRATONVM_PUBLISHED_WAIT_KIND`: the kind is not
    /// published, and the reader asks the deposited top frame as before
    /// (`debug::is_thread_sleep_frame`, `debug::frame_invokes_thread_sleep`).
    /// With it, the answer is authoritative: the JVMTI `SLEEPING` bit
    /// (`jvmti::native_env::wait_kind`) and JDWP's `SLEEPING`
    /// (`debug::jdwp_thread_status_with`) read it whatever frame the sleep
    /// was entered from (a reflective `Method.invoke`, a method handle,
    /// `TimeUnit.sleep`), where the frame test read `PARKED` / `WAIT`.
    pub fn published_sleeping(&self, thread_id: ThreadId) -> Option<bool> {
        if !self.published_wait_kind() {
            return None;
        }
        let threads = self.threads.read();
        let Some(entry) = threads
            .get(&thread_id)
            .filter(|entry| entry.alive.load(Ordering::Acquire))
        else {
            return Some(false);
        };
        Some(gc_block_state_sleeping(&entry.gc_block_state))
    }

    /// The kind of live thread `thread_id`'s wait, as the wait itself
    /// published it in `GcBlockState::java_wait_kind` (interpreter round i2
    /// wave 64, lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`; the proposal's
    /// item 3 in
    /// `docs/feature-designs/jdk-only-mode.md`:
    /// one function the kind readers ask).
    ///
    /// `None` without the switch: the reader assembles the kind as before
    /// (the JMX waiting monitor, [`Self::published_sleeping`], the deposited
    /// top frame). With it the answer is authoritative and takes no
    /// per-thread mutex: every `Object.wait` publishes
    /// [`PublishedWaitKind::ObjectWait`] before its waiting monitor, so a
    /// [`PublishedWaitKind::Unrecorded`] wait is not one. Asked only of a
    /// thread whose [`Self::java_block_state`] is WAITING / TIMED_WAITING;
    /// an unknown or dead thread is `Unrecorded`.
    pub fn published_wait_kind_of(&self, thread_id: ThreadId) -> Option<PublishedWaitKind> {
        if !self.published_all_wait_kinds {
            return None;
        }
        let threads = self.threads.read();
        let Some(entry) = threads
            .get(&thread_id)
            .filter(|entry| entry.alive.load(Ordering::Acquire))
        else {
            return Some(PublishedWaitKind::Unrecorded);
        };
        Some(gc_block_state_wait_kind(&entry.gc_block_state))
    }

    /// The kind of WAITING / TIMED_WAITING thread `thread_id`'s wait as a
    /// thread dump prints it (interpreter round i3 wave 79, lane L6; page
    /// `i63-L1-thread-dump-has-no-wait-kind-suffix`): the JVMTI reader's
    /// order (`jvmti::native_env::wait_kind`) without its deposited-frame
    /// fallback -- the published word with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`;
    /// otherwise a served stand-in's parked monitor wait, the JMX waiting
    /// monitor (`Object.wait`), the published sleep. Anything else, an
    /// unrecorded wait included, is [`PublishedWaitKind::Parked`], as JVMTI
    /// reads it `PARKED`. Never [`PublishedWaitKind::Unrecorded`].
    pub fn dump_wait_kind(&self, thread_id: ThreadId) -> PublishedWaitKind {
        match self.published_wait_kind_of(thread_id) {
            Some(PublishedWaitKind::Unrecorded) => return PublishedWaitKind::Parked,
            Some(kind) => return kind,
            None => {}
        }
        if self.published_monitor_parked(thread_id) {
            return PublishedWaitKind::Parked;
        }
        if self.peek_jmx_waiting_monitor(thread_id).is_some() {
            return PublishedWaitKind::ObjectWait;
        }
        if self.published_sleeping(thread_id) == Some(true) {
            return PublishedWaitKind::Sleeping;
        }
        PublishedWaitKind::Parked
    }

    /// Is live thread `thread_id` inside a native it entered through the
    /// native-call funnel, outside a blocking region (its
    /// `GcBlockState::native_excuse` word raised; interpreter round i2 wave
    /// 60, lane L6)? Raised for every registered native the funnel runs, so a
    /// reader that means HotSpot's `_thread_in_native` also checks that the
    /// method is a `native` one (`thread_jmx_snapshot`: the trace's top row).
    pub fn running_funnel_native(&self, thread_id: ThreadId) -> bool {
        self.gc_block_state_of(thread_id).is_some_and(|block| {
            block.native_excuse.load() & crate::threading::jvm_thread::NX_IN != 0
                && !block.in_blocked_region.load(Ordering::Acquire)
        })
    }

    /// The callback of the native live thread `thread_id` runs through the
    /// native-call funnel outside a blocking region, as its raised
    /// native-excuse word records it (interpreter round i2 wave 62, lane L6,
    /// `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`): `None` when it runs none
    /// ([`Self::running_funnel_native`] is false) or nothing was recorded (the
    /// switch is off). The callback is that native's or, if the thread left it
    /// meanwhile, a later one's.
    pub fn running_funnel_callback(&self, thread_id: ThreadId) -> Option<usize> {
        let block = self.gc_block_state_of(thread_id)?;
        if block.native_excuse.load() & crate::threading::jvm_thread::NX_IN == 0
            || block.in_blocked_region.load(Ordering::Acquire)
        {
            return None;
        }
        Some(block.native_excuse.callback()).filter(|&cb| cb != 0)
    }

    /// Live thread `thread_id`'s native-excuse word as it stands (0 for no
    /// thread), for diagnostics (interpreter round i2 wave 61, lane L6).
    pub fn native_excuse_word(&self, thread_id: ThreadId) -> usize {
        self.gc_block_state_of(thread_id)
            .map_or(0, |block| block.native_excuse.load())
    }

    /// The callback live thread `thread_id` runs through the native-call
    /// funnel, as it recorded it (`GcBlockState::funnel_native_callback`;
    /// interpreter round i2 wave 61, lane L6): `None` for no thread or no
    /// record (none is kept unless a debugger is attached or a C JVMTI env is
    /// bound with `CRATONVM_JVMTI_C_RUNNING_NATIVE` on).
    pub fn funnel_native_callback(&self, thread_id: ThreadId) -> Option<usize> {
        self.gc_block_state_of(thread_id)
            .map(|block| block.funnel_native_callback.load(Ordering::Acquire))
            .filter(|&cb| cb != 0)
    }

    /// Clear the GC-blocked mark for a VM-registered native carrier thread.
    pub fn mark_native_thread_unblocked(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            {
                let mut f = entry.gc_block_state.fixup.lock();
                if !f.is_empty()
                    && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_BLOCKGC")
                {
                    eprintln!(
                        "[blockgc] native-unblock DISCARDS {} fixups tid={}",
                        f.len(),
                        thread_id.0,
                    );
                }
                f.clear();
            }
            entry.gc_block_state.slot_origins.lock().clear();
            entry.root_snapshot.lock().clear();
            entry
                .gc_block_state
                .in_blocked_region
                .store(false, Ordering::Release);
        }
    }

    /// CENSUS-RECONCILE — the shared [`GcBlockState`] published for
    /// `thread_id`.
    ///
    /// This is the SAME `Arc` the owning `JvmThread` holds (installed by
    /// [`Self::set_gc_block_state`]), so a caller that cannot reach the
    /// `JvmThread` itself can still hand the *authoritative*
    /// `in_blocked_region` flag to
    /// [`crate::threading::gc_barrier::GcBarrier::leave_blocked_region_flagged`]
    /// rather than clearing it with a bare `store(false)` (finding 1(c): a
    /// bare store lets a pause requested between the drain and the store both
    /// EXCLUDE the thread and let it run).
    ///
    /// `jni::host_thread_leave_native` is the motivating caller: it runs on a
    /// host OS thread that has no `JNI_THREAD` binding, so `with_jni_context`
    /// — every other path's route to the flag — resolves to `None` there.
    ///
    /// The `Arc` is CLONED out rather than borrowed on purpose: the registry
    /// lock must be released before the caller can park inside the barrier.
    /// Holding it across a pause drain would queue every later census behind a
    /// pending registry writer.
    pub fn gc_block_state_of(&self, thread_id: ThreadId) -> Option<Arc<GcBlockState>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| entry.gc_block_state.clone())
    }

    /// CENSUS-RECONCILE — the registered, alive `ThreadId` whose published
    /// `os_tid` is the CALLING OS thread's.
    ///
    /// The identity census (`alive_count_blocked_and_os_tids`) keys the STW
    /// exclusion set on `ThreadId`, so a host-facing entry point that wants to
    /// be *excluded* must name itself. A host thread parked outside the VM has
    /// no `JvmThread` borrow and no JNI TLS binding, but its carrier's
    /// `os_tid` was published by [`Self::set_os_tid_current`] at registration
    /// — which makes the OS id the only identity link that survives leaving
    /// the VM.
    ///
    /// More than one alive entry can claim this OS thread: a virtual thread
    /// mounted on a carrier republishes the carrier's `os_tid` under its own
    /// id at every mount (`vm_exec.rs::resume_virtual_continuation`), and no
    /// unmount clears it, so every parked virtual thread that last ran here
    /// still claims it. The answer is then the MOUNTED one: the claimant
    /// whose [`Self::set_os_tid_current`] on this OS thread is the newest
    /// (gc-common w6-g, `common-w3a-jni-unbound-counted-monitor-enter`
    /// residual 2). That is sound because the question is only ever asked by
    /// code running on this OS thread, and a mount publishes before it runs
    /// any Java, so the running identity's publish is the last one made here.
    /// A claimant that is concurrently republishing on ANOTHER carrier is
    /// filtered by re-reading its `os_tid` after its ordinal (the writer
    /// stores `os_tid` first).
    ///
    /// Returns `None` when the answer is unknown:
    ///
    /// * no alive entry claims this OS thread (a genuinely foreign host
    ///   thread — it is not in `alive_count` either, so it occupies no
    ///   `expected` slot and needs no exclusion),
    /// * several claim it and no single newest publish names one (only
    ///   possible for entries whose `os_tid` was stored without
    ///   `set_os_tid_current`), or
    /// * the platform has no `os_tid` backend at all (`current_os_tid() == 0`).
    ///
    /// `None` is always the safe answer: the caller then falls back to the
    /// anonymous-counter-only behaviour, which is what every such call did
    /// before this existed.
    pub fn thread_id_for_current_os_tid(&self) -> Option<ThreadId> {
        self.thread_id_for_os_tid_inner(current_os_tid())
    }

    /// [`Self::thread_id_for_current_os_tid`] for an explicit OS tid, split
    /// out so the claimant choice is testable without extra OS threads.
    ///
    /// gcj j3/rt3 (j2-pn2 finding 11, the twin of
    /// [`newest_claimants_by_os_tid`]'s rule): an UNMOUNTED entry -- a parked
    /// virtual thread -- claims nothing. Its mount published the carrier's tid
    /// under a NEWER ordinal than the carrier's own entry, and nothing
    /// republishes the carrier at unmount, so after the unmount the parked
    /// continuation stayed the newest claimant and a carrier asking its own
    /// identity (an unbound JNI `MonitorEnter`, the barrier identity of
    /// `jni_caller_thread_id`) was answered with the parked thread: a monitor
    /// owned under, or a pause excusing, a thread that runs nowhere. The
    /// mounted bit is lowered before the yield's deposit and raised at every
    /// mount next to its publish, so the identity running here is never the
    /// one skipped. Every backend: thread identity is not a collector's
    /// (`CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED=0` kills, read per registry).
    fn thread_id_for_os_tid_inner(&self, os_tid: u32) -> Option<ThreadId> {
        if os_tid == 0 {
            return None;
        }
        let skip_unmounted = self.os_tid_identity_skips_unmounted;
        let threads = self.threads.read();
        let mut claims: Vec<(ThreadId, u64)> = Vec::new();
        for (tid, e) in threads.iter() {
            if !e.alive.load(Ordering::Acquire) || e.os_tid.load(Ordering::Acquire) != os_tid {
                continue;
            }
            if !os_tid_claim_admitted(os_tid, e.mounted.load(Ordering::Acquire), skip_unmounted) {
                continue;
            }
            let seq = e.os_tid_seq.load(Ordering::Acquire);
            // Re-read after the ordinal: a claimant that moved to another
            // carrier stored its new `os_tid` before its new ordinal, so if
            // the ordinal above is the new one this read is no longer ours.
            if e.os_tid.load(Ordering::Acquire) != os_tid {
                continue;
            }
            claims.push((*tid, seq));
        }
        newest_os_tid_claimant(&claims)
    }

    /// gcf f2/j: does an ALIVE entry claim the calling OS thread (its
    /// published `os_tid`, [`Self::set_os_tid_current`], which every VM thread
    /// runs at startup and every mount republishes)? `None` when this
    /// platform has no OS-tid backend, i.e. the answer is unknown.
    ///
    /// Unlike [`Self::thread_id_for_current_os_tid`] it does not pick ONE
    /// claimant, so several claims (a carrier's parked virtual threads) still
    /// answer `Some(true)`. The JNI invocation interface's "is this OS thread
    /// attached?" (`jni_get_env`'s `JNI_EDETACHED`, `AttachCurrentThread`'s
    /// already-attached test) reads it after the cheaper thread-local tests.
    /// An idle foreign attachment withdraws its tid (`clear_os_tid`); the JNI
    /// side answers for it from its own attachment record first.
    ///
    /// Deliberately NOT [`Self::thread_id_for_os_tid_inner`]'s unmounted rule
    /// (gcj j3/rt3 residual 2): this asks "is this OS thread attached?", not
    /// "which identity runs here?". Every carrier that ever mounted a virtual
    /// thread is itself a registered, alive entry with its own tid, so
    /// counting a parked continuation's stale claim cannot change the answer;
    /// it could only for an unregistered OS thread with a continuation
    /// mounted on it, which the VM never builds. Copy the skip here only with
    /// such a carrier and a test that tells the two answers apart.
    pub fn current_os_tid_claimed(&self) -> Option<bool> {
        let os_tid = current_os_tid();
        if os_tid == 0 {
            return None;
        }
        let threads = self.threads.read();
        Some(
            threads.values().any(|e| {
                e.alive.load(Ordering::Acquire) && e.os_tid.load(Ordering::Acquire) == os_tid
            }),
        )
    }

    /// The published `JvmThread` address of one alive entry (`None` when the
    /// entry is unknown, dead, or has not published it; see
    /// [`Self::set_jvm_thread_addr`]).
    ///
    /// Only the thread that owns the entry may dereference the answer. A
    /// published address stays valid while its own thread runs, and no other
    /// thread's `JvmThread` is safe to touch from here (the take-over has
    /// its own frozen-peer contract, [`Self::frozen_peer_thread_addrs`]).
    pub fn own_jvm_thread_addr(&self, thread_id: ThreadId) -> Option<usize> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        if !entry.alive.load(Ordering::Acquire) {
            return None;
        }
        Some(entry.jvm_thread_addr.load(Ordering::Acquire)).filter(|a| *a != 0)
    }

    /// DBG (CRATONVM_DBG_MTROOTS): per-thread (tid, in_blocked_region,
    /// snapshot_len) for every alive thread. Used at an STW to see whether a
    /// thread that holds a reclaimed live oop was counted BLOCKED (excluded from
    /// the barrier `expected`) while actually running — the multi-thread
    /// root-coverage gap.
    pub fn dump_blocked_states(&self) -> Vec<(u64, bool, usize)> {
        let threads = self.threads.read();
        let mut v = Vec::new();
        for (tid, entry) in threads.iter() {
            if entry.alive.load(Ordering::Acquire) {
                let blk = entry
                    .gc_block_state
                    .in_blocked_region
                    .load(Ordering::Acquire);
                let len = entry.root_snapshot.lock().len();
                v.push((tid.0, blk, len));
            }
        }
        v
    }

    /// Debug-only STW census: one line per alive JVM thread with the state that
    /// matters to stop-the-world accounting. Used when the GC barrier is waiting
    /// for a cooperative mutator but the cross-thread JIT takeover cannot find a
    /// live JIT frame to freeze.
    pub fn debug_thread_census(&self) -> String {
        use std::fmt::Write as _;

        let threads = self.threads.read();
        let mut out = String::new();
        for (tid, entry) in threads.iter() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }

            let blocked = entry
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire);
            let stw_ready = entry.stw_ready.load(Ordering::Acquire);
            let snapshot_len = entry.root_snapshot.lock().len();
            let os_tid = entry.os_tid.load(Ordering::Acquire);
            let vm_state = {
                let s = entry.vm_state.lock();
                if s.is_empty() {
                    "<unset>".to_string()
                } else {
                    s.clone()
                }
            };
            let trace = entry.frame_trace.lock();
            let mut top = String::new();
            for (i, frame) in trace.iter().rev().take(4).enumerate() {
                if i > 0 {
                    top.push_str(" <- ");
                }
                let _ = write!(
                    top,
                    "{}.{}@{}",
                    frame.class_name, frame.method_name, frame.byte_code_index
                );
            }
            if top.is_empty() {
                top.push_str("<no-frame-trace>");
            }
            // Same `deposit=` tag as the T19.H1 summary, for the same reason:
            // this census is alive-only, but "alive" does not mean "parked at
            // the frames below" — a mutator the barrier is still waiting on is
            // by definition RUNNING, and its chain is last-block history.
            // See `ThreadRegistry::deposit_freshness`.
            let _ = write!(
                out,
                "\n  t{} os_tid={} name={:?} blocked={} ready={} snapshot={} state={:?} deposit={} top={}",
                tid.0,
                os_tid,
                entry.name,
                blocked,
                stw_ready,
                snapshot_len,
                vm_state,
                Self::deposit_freshness(true, blocked),
                top
            );
        }
        out
    }

    /// JIT round 18 wave 10 (lane ffmeintr): the `CRATONVM_DBG_STW_WATCHDOG_SECS`
    /// dump, one line per registered thread (alive or not), for a pause whose
    /// wait has outlived the watchdog: every state a stop-the-world wait can
    /// be stuck on, so a silent hang names the thread it waits for.
    ///
    /// Built to be safe from INSIDE a stuck pause: the registry read lock (a
    /// pause already takes it every round), atomics, and `try_lock` on the
    /// two per-thread mutexes (a frozen, claimed or blocked peer may hold
    /// them), never `lock`. `counted` / `taken` are the pause's census and
    /// frozen-or-claimed OS tids; `peer_os` answers the OS side of a tid
    /// (`xt_root_scan::debug_peer_os_state`: kernel state, syscall, take-over
    /// slot).
    pub fn debug_stw_watchdog_census(
        &self,
        counted: &[u32],
        taken: &[u32],
        peer_os: &dyn Fn(u32) -> String,
    ) -> String {
        use std::fmt::Write as _;
        let threads = self.threads.read();
        let mut out = String::new();
        for (tid, entry) in threads.iter() {
            let block = &entry.gc_block_state;
            let os_tid = entry.os_tid.load(Ordering::Acquire);
            let lazy = match block.lazy_native.load(Ordering::Acquire) {
                crate::threading::jvm_thread::LAZY_NATIVE_OFF => "off",
                crate::threading::jvm_thread::LAZY_NATIVE_IN => "IN",
                crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED => "CLAIMED",
                crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED_LEAVING => "CLAIMED_LEAVING",
                crate::threading::jvm_thread::LAZY_NATIVE_SIGNALLING => "SIGNALLING",
                _ => "?",
            };
            let vm_state = entry
                .vm_state
                .try_lock()
                .map(|s| {
                    if s.is_empty() {
                        "<unset>".to_string()
                    } else {
                        s.clone()
                    }
                })
                .unwrap_or_else(|| "<locked>".to_string());
            let top = match entry.frame_trace.try_lock() {
                Some(trace) => trace
                    .iter()
                    .rev()
                    .take(3)
                    .map(|f| format!("{}.{}@{}", f.class_name, f.method_name, f.byte_code_index))
                    .collect::<Vec<_>>()
                    .join(" <- "),
                None => "<locked>".to_string(),
            };
            let _ = write!(
                out,
                "\n  t{} os_tid={} name={:?} alive={} ready={} mounted={} counted={} taken={} \
                 blocked={} java_state={} foreign_native={} lazy_native={} handshake_pending={} \
                 vm_state={:?} last_trace=[{}] os=[{}]",
                tid.0,
                os_tid,
                entry.name,
                entry.alive.load(Ordering::Acquire),
                entry.stw_ready.load(Ordering::Acquire),
                entry.mounted.load(Ordering::Acquire),
                os_tid != 0 && counted.contains(&os_tid),
                os_tid != 0 && taken.contains(&os_tid),
                block.in_blocked_region.load(Ordering::Acquire),
                block.java_state.load(Ordering::Acquire),
                block.foreign_native.load(Ordering::Acquire),
                lazy,
                block.handshakes.is_pending(),
                vm_state,
                top,
                if os_tid != 0 {
                    peer_os(os_tid)
                } else {
                    String::new()
                },
            );
        }
        out
    }

    /// Collect root snapshots from all alive threads.
    /// Returns a combined vector of all ObjectRefs from all threads' snapshots.
    pub fn collect_all_root_snapshots(&self) -> Vec<ObjectRef> {
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("collect_all_root_snapshots");
        let len = || self.threads.read().len();
        PERF.time(len, || {
            // gci i13/vg: a split the collection door armed for this gather
            // (`arm_parked_split_gather`); consumed here, its old-only
            // section left for `take_parked_old_only`.
            //
            // gci i21/pr (`CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`, opt-in): the
            // initiator's own snapshot the door armed for this gather
            // (`arm_initiator_snapshot_skip`); consumed here, with the split.
            //
            // gcj j3/x2: taken only on the arming thread (see the arm).
            let (armed, initiator_skip) = {
                let mut s = self.parked_split.lock();
                let mine = match s.initiator_skip.as_ref() {
                    Some((_, owner)) => *owner == std::thread::current().id(),
                    None => false,
                };
                let skip = if mine {
                    s.initiator_skip.take().map(|(snapshot, _)| snapshot)
                } else {
                    if s.initiator_skip.is_some() {
                        s.initiator_foreign_gathers += 1;
                    }
                    None
                };
                // gcj j3/x2 R10: the split, by the same owner rule.
                let split_mine = s
                    .armed
                    .as_ref()
                    .is_some_and(|(_, owner)| *owner == std::thread::current().id());
                let armed = if split_mine {
                    s.armed.take().map(|(filter, _)| filter)
                } else {
                    if s.armed.is_some() {
                        s.split_foreign_gathers += 1;
                    }
                    None
                };
                (armed, skip)
            };
            let mut initiator_skipped = 0usize;
            let skip = initiator_skip.as_ref();
            // gci i14/ve: a mirror-keyed request the door armed for this
            // gather (`arm_mirror_keyed_gather`); consumed here.
            let keyed_arm = self.mirror_keyed.lock().armed.take();
            let mut keyed_out = MirrorKeyedOut::default();
            let mut keyed: Option<(&MirrorKeyedArm, &mut MirrorKeyedOut)> = None;
            if let Some(arm) = keyed_arm.as_ref() {
                keyed = Some((arm, &mut keyed_out));
            }
            let roots = match armed {
                Some(young) => {
                    let mut old_only: Vec<ObjectRef> = Vec::new();
                    let roots = self.collect_all_root_snapshots_inner(
                        Some((&young, &mut old_only)),
                        keyed,
                        skip,
                        &mut initiator_skipped,
                    );
                    self.parked_split.lock().old_only = old_only;
                    roots
                }
                None => self.collect_all_root_snapshots_inner(
                    None,
                    keyed,
                    skip,
                    &mut initiator_skipped,
                ),
            };
            if initiator_skip.is_some() {
                self.parked_split.lock().initiator_skipped = Some(initiator_skipped);
            }
            if let Some(arm) = keyed_arm.as_ref() {
                self.publish_mirror_keyed(arm, keyed_out);
            }
            roots
        })
    }

    /// gci i14/ve: file the rows of the entries a mirror-keyed gather left
    /// out -- `(mirror, word)` for every snapshot word, this VM's
    /// `metadata_pin` rows, which every Generational marker follows from a
    /// marked owner -- and remember `(tid, mirror)` for the same pause's
    /// verdict. The rows are added AFTER `collect_roots` step 1 replaced this
    /// VM's rows (the door gathers after it), so they reach this pause's
    /// collector, and the next scan's step 1 drops them. Appends to the
    /// deferred list: the sweep of this pause takes it whole.
    fn publish_mirror_keyed(&self, arm: &MirrorKeyedArm, out: MirrorKeyedOut) {
        if out.deferred.is_empty() {
            return;
        }
        cratonvm_types::metadata_pin::add_metadata_pins(arm.vm, &out.rows);
        let mut keyed = self.mirror_keyed.lock();
        keyed.deferred.extend(out.deferred);
        // gci i15/vh: a G1 marking scan keyed something this cycle, so the
        // remark closes the rows over live owners
        // (`judge_mirror_keyed_parked_after_mark`).
        if !arm.old_ranges.is_empty() {
            keyed.g1_cycle_keyed = true;
        }
    }

    /// gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`, default ON since 2026-10-17): the gather
    /// of [`Self::collect_all_root_snapshots`], split in two root sections for
    /// a collector that can tell a young-only phase from one that marks or
    /// moves old space. `young` is the collector's young set for THIS pause
    /// (`VmHeap::parked_old_only_young_ranges`: Generational both semispaces,
    /// G1 every region that is not `Old`); it is asked inside the pause, so it
    /// cannot change before the collection.
    ///
    /// Returns `(roots, old_only)`. `old_only` holds the snapshots of the
    /// UNMOUNTED continuations whose cached [`FoldAddrFilter`] (valid for the
    /// entry's current `mount_epoch`, i.e. nothing but a fold has touched its
    /// words since the filter was taken) is disjoint from `young`: none of
    /// their words is a young address. Everything else -- every mounted
    /// entry, every entry without a valid filter, every thread mirror and JMX
    /// monitor root (a `VirtualThread` mirror can be young) -- is in `roots`,
    /// exactly as [`Self::collect_all_root_snapshots`] gathers it.
    ///
    /// Soundness (the page's argument): a young-only collection follows no
    /// root into old space (old -> young edges come from the card table / the
    /// remembered sets, never from a root), and moves and frees nothing old,
    /// so leaving out a root whose every word is outside the young set changes
    /// nothing it does; the snapshot words stay valid because nothing old
    /// moved, and the blocked fold still remaps them on a pause that does move
    /// old space. The caller hands `old_only` to the heap
    /// (`VmHeap::stage_parked_old_only_roots`), which adds it back to the root
    /// set of every phase that marks or moves old space.
    pub fn collect_all_root_snapshots_split(
        &self,
        young: &[(usize, usize)],
    ) -> (Vec<ObjectRef>, Vec<ObjectRef>) {
        let mut young_filter = FoldAddrFilter::default();
        for &(lo, hi) in young {
            young_filter.insert_range(lo, hi);
        }
        let mut old_only: Vec<ObjectRef> = Vec::new();
        let roots = self.collect_all_root_snapshots_inner(
            Some((&young_filter, &mut old_only)),
            None,
            None,
            &mut 0,
        );
        (roots, old_only)
    }

    /// `CRATONVM_DBG_ZGC_WILD`: one `[ZGC-WILD-SNAP]` line per alive thread's
    /// snapshot root that `is_obj` refuses, naming the thread, the index, and
    /// the frame slot it came from when `slot_origins` recorded one. Diagnostic
    /// only; takes the registry read lock and every snapshot lock in turn.
    pub(crate) fn dbg_wild_snapshot_roots(&self, is_obj: &dyn Fn(usize) -> bool) {
        let threads = self.threads.read();
        for (tid, entry) in threads.iter() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }
            let snapshot = entry.root_snapshot.lock();
            for (i, r) in snapshot.iter().enumerate() {
                let a = r.as_ptr() as usize;
                if is_obj(a) {
                    continue;
                }
                let origin = entry
                    .gc_block_state
                    .slot_origins
                    .lock()
                    .iter()
                    .find(|so| so.orig == a || so.cur == a)
                    .map(|so| {
                        format!(
                            "frame#{} {}[{}] live={}",
                            so.frame,
                            if so.is_stack { "stack" } else { "local" },
                            so.idx,
                            so.live
                        )
                    });
                eprintln!(
                    "[ZGC-WILD-SNAP] thread={:?} tid={} idx={i}/{} addr={a:#x} blocked={} \
                     conservative={} origin={:?}",
                    entry.name,
                    tid.0,
                    snapshot.len(),
                    entry
                        .gc_block_state
                        .in_blocked_region
                        .load(Ordering::Acquire),
                    entry
                        .gc_block_state
                        .deposit_conservative
                        .load(Ordering::Acquire),
                    origin
                );
            }
        }
    }

    /// `split` (gci i13/vg, [`Self::collect_all_root_snapshots_split`]): the
    /// collector's young filter for this pause and the OLD-ONLY section that
    /// receives the snapshot of every entry [`parked_snapshot_is_old_only`]
    /// admits. `None`: one section, the historical gather.
    ///
    /// `keyed` (gci i14/ve, [`Self::arm_mirror_keyed_gather`]): the door's
    /// mirror-keyed request and where the entries it admits go
    /// ([`mirror_keyed_mirror`]). Such an entry contributes NOTHING to either
    /// section -- neither its snapshot nor its mirror; its JMX roots stay --
    /// and its words become `(mirror, word)` rows in `out.rows`. It wins over
    /// the old-only split (a keyed entry is in neither section).
    ///
    /// `skip` (gci i21/pr, [`Self::arm_initiator_snapshot_skip`]): the
    /// collection initiator's own `root_snapshot` `Arc`, whose words its
    /// `collect_roots` step 11 already pushed in this pause; `skipped` counts
    /// the words left out. An entry SHARING that `Arc` (`Arc::ptr_eq`) gives
    /// nothing to `all_roots` from its snapshot; everything else about it is
    /// unchanged (its mirror, its JMX roots, the guard's conservative words,
    /// the census, and a keyed or old-only routing, which already kept the
    /// words out of `all_roots`).
    fn collect_all_root_snapshots_inner(
        &self,
        mut split: Option<(&FoldAddrFilter, &mut Vec<ObjectRef>)>,
        mut keyed: Option<(&MirrorKeyedArm, &mut MirrorKeyedOut)>,
        skip: Option<&Arc<Mutex<Vec<ObjectRef>>>>,
        skipped: &mut usize,
    ) -> Vec<ObjectRef> {
        let threads = self.threads.read();
        let mut all_roots = Vec::new();
        // gci i13/vg: the old-only section's share, for the census line.
        let mut old_only_entries = 0usize;
        let mut old_only_words = 0usize;
        // gci i14/ve: the mirror-keyed share, for the census line.
        let mut keyed_entries = 0usize;
        let mut keyed_words = 0usize;
        // gcg g6/jn (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS`, default ON
        // since gch h2/yg, `=0` off):
        // the words of every snapshot that was deposited with the
        // conservative interpreter-local probe on, for the unrecorded-young-
        // root guard to exclude. See `conservative_deposit_words`.
        let guard_deposits = unrecorded_root_guard_deposits_enabled();
        let mut conservative_words: Vec<usize> = Vec::new();
        // gch h3/yc: the parked threads' precise words kept under the guard
        // (the `[blockgc] conservative-deposit` line's engagement count), and
        // its kill switch, read once per gather.
        let parked_precise = guard_deposits && unrecorded_root_guard_parked_precise_enabled();
        let mut parked_precise_words = 0usize;
        // gci i12/vp (`CRATONVM_DBG_VT_PARKED_ROOTS`): the unmounted
        // continuations' share of this gather. Counted only with the census on.
        let mut vp_census = self
            .dbg_vt_parked_roots
            .then(|| (Instant::now(), ParkedGatherCensus::default()));
        for (tid, entry) in threads.iter() {
            if entry.alive.load(Ordering::Acquire) {
                let snapshot = entry.root_snapshot.lock();
                // gci i14/ve: a deposited JDK continuation keyed by its
                // mirror is no root of this pause at all; its words are rows
                // owned by the mirror (`publish_mirror_keyed`).
                let keyed_mirror = match keyed.as_ref() {
                    Some((arm, _)) => mirror_keyed_mirror(*tid, entry, &snapshot, arm),
                    None => None,
                };
                // gci i13/vg: an unmounted continuation none of whose words
                // is young goes to the old-only section (see
                // `collect_all_root_snapshots_split`); its mirror and JMX
                // roots below stay in `all_roots`.
                let to_old_only = keyed_mirror.is_none()
                    && match split.as_mut() {
                        Some((young, _)) => parked_snapshot_is_old_only(entry, &**young),
                        None => false,
                    };
                // gci i21/pr: the initiator's own snapshot, already in the
                // door's root vector through `collect_roots` step 11.
                let initiator_dup = match skip {
                    Some(initiator) => Arc::ptr_eq(initiator, &entry.root_snapshot),
                    None => false,
                };
                if let Some(mirror) = keyed_mirror {
                    if let Some((_, out)) = keyed.as_mut() {
                        out.rows
                            .extend(snapshot.iter().map(|w| (mirror, w.as_ptr() as usize)));
                        out.deferred.push((*tid, mirror));
                    }
                    keyed_entries += 1;
                    keyed_words += snapshot.len();
                } else if to_old_only {
                    if let Some((_, old_only)) = split.as_mut() {
                        old_only.extend(snapshot.iter().copied());
                    }
                    old_only_entries += 1;
                    old_only_words += snapshot.len();
                } else if initiator_dup {
                    *skipped += snapshot.len();
                } else {
                    all_roots.extend(snapshot.iter().copied());
                }
                if let Some((_, census)) = vp_census.as_mut() {
                    census.note(!entry.mounted.load(Ordering::Acquire), snapshot.len());
                }
                if guard_deposits {
                    let block = &entry.gc_block_state;
                    let conservative = block.deposit_conservative.load(Ordering::Acquire);
                    // gch h1/yp: a BLOCKED thread's flag-raising deposit also
                    // recorded every Object slot it published (`slot_origins`,
                    // advanced by every fold since, like the snapshot). Its
                    // LIVE locals and its operand-stack slots are precise, so
                    // they stay under the guard; only the rest of a
                    // conservative snapshot is excluded. Lock order snapshot
                    // -> slot_origins, as the deposit and the fold take them.
                    // gch h3/yc: a PARKED thread's safepoint deposit records
                    // no `slot_origins`; it publishes its precise words in
                    // `precise_deposit_words` instead (same lock order:
                    // snapshot -> precise_deposit_words, as the deposit
                    // writes it). Empty until that deposit fills it, which
                    // is the h2 behaviour (the whole snapshot excluded).
                    let precise: Vec<usize> = if !conservative {
                        Vec::new()
                    } else if block.in_blocked_region.load(Ordering::Acquire) {
                        precise_slot_origin_words(&block.slot_origins.lock())
                    } else if parked_precise {
                        let words = sorted_precise_words(&block.precise_deposit_words.lock());
                        parked_precise_words += words.len();
                        words
                    } else {
                        Vec::new()
                    };
                    conservative_deposit_words(
                        conservative,
                        &snapshot,
                        &precise,
                        &mut conservative_words,
                    );
                }
                // Root this thread's `java.lang.Thread` mirror. The
                // initiator's own mirror is rooted via `collect_roots`
                // (roots.rs step 10), but a PARKED thread's mirror is only
                // reachable through `ThreadEntry.java_thread_obj` — a raw
                // address copy that no frame root or heap edge necessarily
                // keeps alive (CratonVM's synthetic ThreadGroup does not
                // retain its threads the way the real JDK's does). Without
                // rooting it here, a GC initiated by ANOTHER thread while
                // this one is parked sweeps the mirror; the dangling copy
                // then surfaces as the "all-zero header" invokevirtual
                // fallback on `Thread.currentThread()`, and the real-JDK
                // `Thread.getThreadGroup()` reads a null `holder` and NPEs
                // (Tomcat TestDigestAuthenticator: a worker-thread GC frees
                // the JUnit main thread's mirror). The matching remap is
                // `update_thread_objs_after_gc`.
                // gci i14/ve: not for a mirror-keyed entry -- the mirror
                // lives only as long as something else reaches it.
                if keyed_mirror.is_none() {
                    if let Some(obj) = entry.java_thread_obj {
                        all_roots.push(obj);
                    }
                }
                if let Some(obj) = *entry.jmx_monitors.contended_monitor.lock() {
                    all_roots.push(obj);
                }
                if let Some(obj) = *entry.jmx_waiting_monitor.lock() {
                    all_roots.push(obj);
                }
                entry.jmx_monitors.locked.extend_roots_stw(&mut all_roots);
                all_roots.extend(entry.jmx_locked_synchronizers.lock().iter().copied());
            }
        }
        if guard_deposits {
            if blockgc_dbg() {
                eprintln!(
                    "[blockgc] conservative-deposit words={} parked_precise={parked_precise_words} \
                     (excluded from the unrecorded-young-root guard)",
                    conservative_words.len()
                );
            }
            // Replaces the previous collection's set: this pause's root set is
            // the one the guard will read.
            cratonvm_gc::gc_quiescence::publish_conservative_deposit_words(conservative_words);
        }
        if let Some((started, census)) = vp_census {
            let mut line = census.line(all_roots.len(), started.elapsed());
            // gci i13/vg: only on a split gather, so the i12/vp line is
            // unchanged otherwise. `roots=` above excludes this section.
            if split.is_some() {
                line.push_str(&format!(
                    " old_only={old_only_entries} old_only_words={old_only_words}"
                ));
            }
            // gci i14/ve: only on a mirror-keyed gather, likewise.
            if keyed.is_some() {
                line.push_str(&format!(
                    " mirror_keyed={keyed_entries} mirror_keyed_words={keyed_words}"
                ));
            }
            // gci i14/ve: with the switch on, every gather line carries the
            // contended-arm count (the pauses that fell back to rooting).
            if self.vt_parked_mirror_keyed {
                line.push_str(&format!(
                    " keyed_skipped_contended={}",
                    self.mirror_keyed.lock().skipped_contended
                ));
            }
            eprintln!("{line}");
        }
        all_roots
    }

    /// Set the blocked-region GC state Arc for a thread (share JvmThread's
    /// state with the registry, like `set_root_snapshot`).
    pub fn set_gc_block_state(&self, thread_id: ThreadId, state: Arc<GcBlockState>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            // gci i9/ds: a state installed AFTER the owner published its tid
            // (`publish_os_tid_claim`) would carry no deopt stash cells; when
            // the owner itself installs it, publish them into it too.
            let published = entry.os_tid.load(Ordering::Acquire);
            if published != 0 && published == current_os_tid() {
                publish_deopt_stash_cells_into(&state, published);
            }
            entry.gc_block_state = state;
            // gci i12/vp: a different block state; the parked fold filter is
            // stale.
            entry.mount_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// GC maintenance for the registry's raw `java.lang.Thread` mirrors —
    /// called by the GC initiator (under STW) with the collection's pointer
    /// map.
    ///
    /// Every `ThreadEntry.java_thread_obj` is a raw address copy, and the
    /// `thread_obj_to_park` reverse index is KEYED by such addresses. A
    /// moving collection relocates the mirrors; without this step the
    /// copies dangle: natives that serve them back to Java
    /// (`enumerate_threads` / `Thread.getAllStackTraces`) resurrect stale
    /// refs into bytecode (the all-zero-header invokevirtual WARN flood),
    /// and `LockSupport.unpark(Thread)`'s O(1) lookup misses the live
    /// mirror's new address — a silently lost unpark.
    pub fn update_thread_objs_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        let mut threads = self.threads.write();
        let mut rekeyed: Vec<(usize, usize)> = Vec::new();
        // Vacated mirror addresses + owning tid, recorded into
        // `former_mirror_addrs` AFTER `threads` is dropped (lock order).
        let mut vacated: Vec<(usize, ThreadId)> = Vec::new();
        // `(old_addr, new_addr, owner)` for every recorded owned synchronizer
        // this collection relocated; applied to `synchronizer_owner` after
        // `threads` is dropped, same lock discipline as `vacated`.
        let mut synchronizer_rekeys: Vec<(usize, usize, ThreadId)> = Vec::new();
        for (tid, entry) in threads.iter_mut() {
            if let Some(ref mut obj) = entry.java_thread_obj {
                let old_addr = obj.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    // SAFETY: produced by the GC pointer map.
                    *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                    rekeyed.push((old_addr, new_addr));
                    vacated.push((old_addr, *tid));
                }
            }
            let mut remap_jmx = |obj: &mut ObjectRef| {
                if let Some(&new_addr) = pointer_map.get(&(obj.as_ptr() as usize)) {
                    *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            };
            if let Some(obj) = entry.jmx_monitors.contended_monitor.lock().as_mut() {
                remap_jmx(obj);
            }
            if let Some(obj) = entry.jmx_waiting_monitor.lock().as_mut() {
                remap_jmx(obj);
            }
            entry.jmx_monitors.locked.remap_stw(pointer_map);
            // `synchronizer_owner` is keyed by these very addresses, so the
            // rekeying happens in the same pass that rewrites them: a key left
            // pointing at a vacated address would make the next
            // `set_jmx_owned_synchronizer` for that lock miss its previous
            // owner and record it against two threads at once. The `(old, new,
            // owner)` triples are collected here and applied after `threads` is
            // dropped, keeping this function's "acquire, use, drop, then call
            // out" lock discipline.
            for obj in entry.jmx_locked_synchronizers.lock().iter_mut() {
                let old_addr = obj.as_ptr() as usize;
                remap_jmx(obj);
                let new_addr = obj.as_ptr() as usize;
                if new_addr != old_addr {
                    synchronizer_rekeys.push((old_addr, new_addr, *tid));
                }
            }
        }
        drop(threads);
        if !rekeyed.is_empty() {
            let mut idx = self.thread_obj_to_park.lock();
            rekey_in_two_phases(&mut idx, &rekeyed);
        }
        if !synchronizer_rekeys.is_empty() {
            // gcg g2/k: two phases, as for `thread_obj_to_park` above. Under a
            // sliding compactor one synchronizer's NEW address can be another
            // one's OLD address; moving them one at a time let the first
            // insert overwrite the second's row (the overwritten owner was
            // lost, and the second move then failed its owner check and was
            // skipped). Remove every moving row first, then insert them all.
            let mut moving: Vec<(usize, ThreadId)> = Vec::new();
            for &(old_addr, new_addr, tid) in &synchronizer_rekeys {
                let mut index =
                    self.synchronizer_owner[synchronizer_owner_shard_index(old_addr)].lock();
                if index.get(&old_addr) == Some(&tid) {
                    index.remove(&old_addr);
                    moving.push((new_addr, tid));
                }
            }
            for (new_addr, tid) in moving {
                self.synchronizer_owner[synchronizer_owner_shard_index(new_addr)]
                    .lock()
                    .insert(new_addr, tid);
            }
        }
        // Record vacated mirror addresses for stale-receiver recovery. Done
        // here (threads lock released) to preserve the threads → former lock
        // order; see `record_former_mirror_addr`.
        for (old_addr, tid) in vacated {
            self.record_former_mirror_addr(old_addr, tid);
        }
    }

    /// Blocked-thread root maintenance — called by the GC initiator (under
    /// STW, before `complete_gc`) with the collection's pointer map.
    ///
    /// For every alive thread currently inside a blocked region (parked in
    /// `Object.wait` / `Thread.join` / `LockSupport.park` /
    /// `ReferenceQueue.remove`, hence excluded from the barrier and unable
    /// to apply this map itself):
    ///
    /// 1. compose the map into the thread's pending frame fixup, chaining
    ///    `orig → cur → new` so the entry stays keyed by the address the
    ///    thread's frames still hold, no matter how many GCs it sleeps
    ///    through;
    /// 2. seed first-move entries from the (pre-remap) snapshot — a snapshot
    ///    address with no existing chain IS the frame-held address;
    /// 3. remap the thread's `root_snapshot` in place so the NEXT collection
    ///    scans live addresses instead of a vacated semispace (scanning a
    ///    stale snapshot is itself a heap corruptor: the collector would
    ///    evacuate garbage "objects" through recycled addresses).
    ///
    /// Threads NOT in a blocked region never need this: `request_stw` counts
    /// them in `expected`, so no GC can complete before they refresh their
    /// snapshot and self-apply the map at a safepoint.
    ///
    /// The thread applies + clears its fixup in `check_post_block_gc` on
    /// wake. While it still holds stale frame addresses the only consumers
    /// are this fold (keyed by those addresses) and the wake-side apply,
    /// so the chain stays consistent.
    pub fn fold_pointer_map_into_blocked(&self, pointer_map: &cratonvm_types::PointerMap) {
        self.fold_pointer_map_into_blocked_audited(pointer_map, None)
    }

    /// [`Self::fold_pointer_map_into_blocked`] plus the post-fold invariant
    /// check, for the production call site that can hand over the heap.
    ///
    /// The invariant: once this fold returns, no address a blocked thread will
    /// resume on may lie in the INACTIVE young semispace. That arena is what
    /// the moving cycle just evacuated and zeroed, so a reference into it is a
    /// still-referenced object the collector took — the "all-zero header
    /// `java.lang.Object` receiver" family — and it is detectable HERE, in the
    /// cycle that caused it, with the frame and slot that hold it. Every other
    /// witness of this bug is downstream: a `NoSuchMethodError` against
    /// `java.lang.Object`, a `checkcast` failure, an out-of-bounds field read —
    /// each an unbounded distance from the collection with the gap, and each
    /// naming only the victim's *use* site.
    ///
    /// The check that runs UNCONDITIONALLY is the precise one: a
    /// `slot_origins` entry — the exact `(frame, slot)` tracker, filled by the
    /// blocking deposit and advanced through each cycle's pointer map — whose
    /// `cur` lands in the vacated arena. Its report carries
    /// `was_a_scanned_root`, which forks the fix: `false` means the deposit
    /// never published that slot (a root COVERAGE gap), `true` means the
    /// collector was handed the address and left it behind (an EVACUATION
    /// gap).
    ///
    /// The whole-snapshot version of the same question is behind
    /// `CRATONVM_DBG_BLOCKGC`, because a snapshot legitimately carries
    /// conservative candidates that are not object starts and which
    /// `forward_object` correctly declines to relocate.
    ///
    /// `heap` is `None` only from tests that drive the fold directly with a
    /// synthetic pointer map.
    pub fn fold_pointer_map_into_blocked_audited(
        &self,
        pointer_map: &cratonvm_types::PointerMap,
        heap: Option<&crate::memory::VmHeap>,
    ) {
        if pointer_map.is_empty() {
            return;
        }
        // gc-common w4-a (`handoff-w3g-blocked-fold-costs` edit 3): this fold's
        // share of the root fix-up, under the same `CRATONVM_DBG_ROOTFIXUP`
        // switch as `update_all_roots`' census (which already counts it inside
        // its total, as one undifferentiated part). See `blocked_fold_census`.
        let census_timer = blocked_fold_census_on().then(BlockedFoldTimer::start);
        // Latched (edit 2): this ran an uncached environment probe once per
        // relocating pause.
        let dbg = blockgc_dbg();
        // One arena lock for the whole fold; the per-address test below is two
        // integer compares, so the audit is affordable unconditionally and does
        // not need a flag to have been set before the run that reproduces.
        // gen r5w5/pin9: the inactive semispace plus a pinned in-place
        // cycle's vacated spans (the roots6 page).
        let vacated = heap.and_then(|h| h.young_vacated_probe());
        // This cycle's cross-thread conservative scan of BLOCKED peers' native
        // stacks, as `(os_tid, addr, value)`. Drained once here and bucketed by
        // owner below: these words live in a peer's machine stack, are named by
        // no oop map, and are the population §12 of
        // `internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
        // measured stale on essentially every relocating cycle. Draining
        // unconditionally matters as much as using it -- a capture left behind
        // by a cycle that did not fold would be applied against the WRONG
        // pointer map later.
        let scanned_stack_slots = cratonvm_gc::gc_quiescence::take_peer_stack_slots();
        // Adopted by SOME blocked thread below. The complement is the reading
        // that matters: a capture nobody claimed is a word in a peer that is
        // NOT in a blocked region -- a take-over freeze, whose thread has no
        // wake hook to apply a fixup at -- naming an object this cycle just
        // relocated, with nothing left to rewrite it. Counted rather than
        // assumed absent; see `PEER_STACK_SLOTS_UNROUTED`.
        let mut adopted_here = 0usize;
        let threads = self.threads.read();
        // gen r5w1/crash5: which entry a capture belongs to. A capture is
        // keyed by the OS tid it was read from, and several alive entries can
        // claim one OS tid -- every virtual thread that last mounted on a
        // carrier keeps the carrier's tid while it is parked (nothing clears it
        // at unmount; see `ThreadEntry::os_tid_seq`). Each of them used to adopt
        // every word of the carrier's stack, and later wrote the words back
        // from whichever carrier it was remounted on: into a stack another
        // thread was running on. Only the claimant running on that OS thread
        // now (the newest `set_os_tid_current` there, the rule
        // `thread_id_for_current_os_tid` already uses) adopts; the write-back
        // itself refuses any word outside the writer's own stack
        // (`vm_exec::apply_native_slot_fixups`). Built only when this cycle
        // captured something.
        let routed_claimant: FxHashMap<u32, Option<ThreadId>> = if scanned_stack_slots.is_empty()
            || !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            FxHashMap::default()
        } else {
            newest_claimants_by_os_tid(
                &threads,
                fold_claimant_skips_unmounted(heap.is_some_and(|h| h.is_generational())),
            )
        };
        // t1 wave 5 lane E2 (hazard 2 of stage 2 of
        // `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`,
        // `CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED`): an OS thread running a
        // continuation slice below its blocked carrier holds two threads'
        // words. A capture at or above the slice's published base is the
        // CARRIER's (`shadow_stack::segment_owner_of_capture`) and is adopted by
        // it alone, below the base by the tid's claimant -- so each word is
        // written back once, by the thread whose stack it is (the carrier's
        // precise compiled slots are rewritten by its own wake remap as well,
        // and two writers make a sliding map's `A -> B, B -> C` a double move).
        // Empty (no allocation) unless a slice runs on a carrier.
        let segments = if scanned_stack_slots.is_empty()
            || !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            Vec::new()
        } else {
            cratonvm_gc::shadow_stack::stack_segments_snapshot()
        };
        // gci i12/vp: the parked-continuation fold filter (opt-in, per VM) and
        // its census (`CRATONVM_DBG_VT_PARKED_ROOTS`). `map_filter` -- this
        // map's keys plus the vacated arena -- is built on the first cached
        // candidate only, so a pause with none pays nothing for it.
        let filter_on = self.vt_parked_fold_filter;
        // gci i13/vg: the split gather (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`)
        // reads the same cached filters, so either switch has the fold leave
        // them; only `filter_on` lets a fold be skipped.
        let filter_kept = filter_on || self.vt_parked_gather_filter;
        let remap_on = cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled();
        let mut map_filter: Option<FoldAddrFilter> = None;
        let vp_started = self.dbg_vt_parked_roots.then(Instant::now);
        let mut vp = ParkedFoldCensus::default();
        for (tid, entry) in threads.iter() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }
            if !entry
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire)
            {
                // NOT blocked: the fixup chain and `slot_origins` below are
                // blocked-region machinery and do not apply.
                //
                // A snapshot remap was tried here on 2026-08-17 — on the theory
                // that a thread censused as blocked and then leaving the region
                // is skipped by this fold AND not waited for by the pause, so
                // its stale snapshot is what the next collection marks from.
                // The theory is sound and the change is a no-op:
                // `CRATONVM_DBG_ROOT_REMAP_AUDIT=1`, which re-runs the root scan
                // at the END of `update_all_roots` and looks for an address this
                // collection moved (excluding slide destinations), reports ZERO
                // with the remap and ZERO without it, on the workload that
                // reproduces the H2 MVStore-writer residual. Every stale
                // snapshot entry it did find was on a BLOCKED thread and was
                // already handled below.
                //
                // Left unwritten deliberately: an unmeasured behaviour change
                // in the root set is exactly what this file's history is made
                // of. If the excluded-thread race is ever observed, the audit
                // above is how to see it and this is where the fix goes.
                //
                // gcg g4/jn (2026-10-02), `CRATONVM_DBG_BLOCKGC` only: name an
                // unblocked thread whose deposited snapshot holds addresses
                // this map moves. A thread parked at its safepoint forwards
                // its own snapshot on resume (`remap_root_snapshot`), so a
                // line here is expected for it; the line that matters is the
                // one whose addresses come back at the NEXT cycle as an
                // `[unrecorded-young-root]` (zero header, `root_source=None`,
                // the g1/j 5.5 signal): that thread resumed WITHOUT forwarding
                // its snapshot. `try_lock`: a frozen peer may hold its lock.
                if dbg {
                    if let Some(snapshot) = entry.root_snapshot.try_lock() {
                        let (moved, first) = moved_snapshot_entries(
                            &snapshot,
                            pointer_map,
                            UNBLOCKED_MOVED_REPORT_ADDRS,
                        );
                        if moved > 0 {
                            eprintln!(
                                "[blockgc] unblocked tid={} os_tid={} mounted={} snapshot_len={} \
                                 moved_by_this_map={} first={}",
                                tid.0,
                                entry.os_tid.load(Ordering::Acquire),
                                entry.mounted.load(Ordering::Acquire),
                                snapshot.len(),
                                moved,
                                first
                                    .iter()
                                    .map(|a| format!("0x{a:x}"))
                                    .collect::<Vec<_>>()
                                    .join(","),
                            );
                        }
                    }
                }
                continue;
            }
            if let Some(census) = &census_timer {
                census.note_thread();
            }
            // gcd d5/f: this thread's blocked state is being rewritten by a
            // relocating pause; its in-native JNI fast paths (`native::jni`)
            // read this count to know they must take the full wake instead.
            entry
                .gc_block_state
                .blocked_folds
                .fetch_add(1, Ordering::Release);
            // gci i12/vp (default-ON `CRATONVM_GC_VT_PARKED_FOLD_FILTER`): an
            // UNMOUNTED continuation whose words this map cannot name. Its
            // cached filter summarises every word the fold below looks up
            // (snapshot, fixup values, `native_slots`, `slot_origins`), as the
            // last fold left them, for the mount epoch it was taken at; with
            // the epoch unchanged nothing but a fold has touched them since.
            // Disjoint from this map's keys (and from the arena the audits
            // below read) means every lookup below misses: the fold would
            // rewrite nothing and report nothing, so skipping it is exact.
            // `blocked_folds` was bumped above all the same (the JNI fast
            // paths and the wake's snapshot keep read it as "a relocating
            // pause targeted this thread", conservatively).
            vp.blocked += 1;
            let parked = !entry.mounted.load(Ordering::Acquire);
            let mut parked_epoch: Option<u64> = None;
            if parked {
                vp.parked += 1;
                if filter_kept {
                    let epoch = entry.mount_epoch.load(Ordering::Acquire);
                    // gci i13/vg: the skip only under the fold switch; the
                    // gather switch alone keeps the filters fresh (below).
                    if let Some(cached) = entry.parked_filter.cached(epoch).filter(|_| filter_on) {
                        vp.cached += 1;
                        let this_map = map_filter.get_or_insert_with(|| {
                            let t = Instant::now();
                            let arena: (Option<(usize, usize)>, &[(usize, usize)]) =
                                match vacated.as_ref() {
                                    Some(p) => (Some(p.inactive_semispace()), p.in_place_spans()),
                                    None => (None, <&[(usize, usize)] as Default>::default()),
                                };
                            let f = pointer_map_fold_filter(pointer_map, arena);
                            vp.map_filter_nanos += t.elapsed().as_nanos();
                            f
                        });
                        if !cached.intersects(this_map) {
                            vp.skipped += 1;
                            // What the native-slot block below does for an
                            // unmounted entry (never `routed_here`): count the
                            // captures that named its stale carrier tid.
                            if remap_on {
                                let os_tid =
                                    Some(entry.os_tid.load(Ordering::Acquire)).filter(|t| *t != 0);
                                if let Some(mine) = os_tid {
                                    let declined = scanned_stack_slots
                                        .iter()
                                        .filter(|&&(t, _, _)| t == mine)
                                        .count();
                                    if declined > 0 {
                                        NATIVE_SLOTS_DECLINED_STALE_CLAIM
                                            .fetch_add(declined as u64, Ordering::Relaxed);
                                        if cratonvm_gc::root_write_audit::enabled() {
                                            note_declined_stale_claim(tid.0, mine, declined);
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                    }
                    parked_epoch = Some(epoch);
                }
            }
            // The filter this fold leaves for the next one (filled below as
            // each word set reaches its final value; `None` unless the filter
            // is on and this entry is an unmounted continuation).
            let mut new_filter: Option<FoldAddrFilter> =
                parked_epoch.map(|_| FoldAddrFilter::default());
            let mut native_slots_summarised = false;
            let mut fixup = entry.gc_block_state.fixup.lock();
            let mut snapshot = entry.root_snapshot.lock();
            let (composed, seeded) = fold_one_blocked_chain(&mut fixup, &mut snapshot, pointer_map);
            if parked {
                vp.parked_words += snapshot.len() + fixup.len();
            }
            if let Some(f) = new_filter.as_mut() {
                for r in snapshot.iter() {
                    f.insert(r.as_ptr() as usize);
                }
                for &v in fixup.values() {
                    f.insert(v);
                }
            }
            // THE BLOCKED-PEER NATIVE-STACK REPAIR (2026-09-07).
            //
            // Adopt this cycle's captures for this thread, then advance every
            // entry -- new and inherited -- through this collection's pointer
            // map. Same shape as `slot_origins` below, and for the same reason:
            // an exact per-map lookup cannot strand an entry on a missed chain
            // seed. The difference is only that these are keyed by a RAW
            // ADDRESS in the peer's stack rather than by `(frame, idx)`.
            if cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
                // Read the entry we are already holding, NOT `self.os_tid_of`.
                //
                // gc-common 2026-09-23 (lane A): `os_tid_of` takes
                // `self.threads.read()` again while this loop already holds a
                // read guard on the same lock. `parking_lot::RwLock` is
                // task-fair — a reader BLOCKS while a writer is queued — so the
                // nested read is exactly the recursion this type's own doc
                // forbids: if any thread queued a `threads.write()` between the
                // two acquisitions, the initiator (inside a moving pause,
                // every counted mutator parked) waits for the writer, and the
                // writer waits for the initiator's outer guard. Writers DO run
                // during a pause: a starting carrier's `set_gc_block_state`
                // (not yet `stw_ready`, so not parked), a JNI attach's
                // `register`, a blocked joiner's `mark_dead` after its
                // `JoinHandle::join` returns. Default-ON path
                // (`blocked_peer_stack_remap_enabled`), reached on every
                // relocating cycle with at least one blocked thread.
                let os_tid = Some(entry.os_tid.load(Ordering::Acquire)).filter(|t| *t != 0);
                let mut ns = entry.gc_block_state.native_slots.lock();
                // gen r5w2/roots6: and only a MOUNTED entry. An unmounted
                // continuation that is still the newest claimant of its last
                // carrier's tid (nothing has mounted there since) is not on
                // that stack; the carrier's words are someone else's.
                let routed_here = entry.mounted.load(Ordering::Acquire)
                    && os_tid.is_some_and(|mine| {
                        routed_claimant.get(&mine).copied().flatten() == Some(*tid)
                    });
                if let Some(mine) = os_tid.filter(|_| routed_here) {
                    // gen r5w2/roots6 (`gengc-r5w1-crash5-blocked-native-slot-
                    // captures-grow-every-cycle`, item 1): a word this thread
                    // already carries with the SAME captured value is not
                    // adopted again. The helper-window pass re-reads a
                    // long-parked thread's stack on every pause (and on every
                    // barrier round of one pause), and each re-read used to
                    // append a fresh copy, growing the list and every fold's
                    // walk of it without bound. At wake only the OLDEST entry
                    // for an address can store (the rest fail the `orig`
                    // guard once it has), so dropping an exact duplicate
                    // changes nothing but the cost. A word whose value CHANGED
                    // since the earlier capture is a new capture and is kept.
                    // addr-keyed: local to this fold (one pause).
                    let mut held: rustc_hash::FxHashSet<(usize, usize)> =
                        ns.iter().map(|e| (e.addr, e.orig)).collect();
                    for &(t, addr, value) in &scanned_stack_slots {
                        if t == mine {
                            // t1 wave 5 lane E2: a word above a slice's base
                            // belongs to the segment's owner (the blocked
                            // carrier), adopted below; unless this entry IS it.
                            if cratonvm_gc::shadow_stack::segment_owner_of_capture(
                                &segments, mine, addr,
                            )
                            .is_some_and(|owner| owner != tid.0)
                            {
                                continue;
                            }
                            if !held.insert((addr, value)) {
                                NATIVE_SLOTS_DUPLICATE_CAPTURES.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            ns.push(crate::threading::jvm_thread::NativeSlotFixup {
                                addr,
                                orig: value,
                                cur: value,
                            });
                            cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_ADOPTED
                                .fetch_add(1, Ordering::Relaxed);
                            adopted_here += 1;
                        }
                    }
                } else if let Some(mine) = os_tid {
                    // A stale claimant: these words are the stack of whoever
                    // runs on `mine` now, not this entry's.
                    let declined = scanned_stack_slots
                        .iter()
                        .filter(|&&(t, _, _)| t == mine)
                        .count();
                    if declined > 0 {
                        NATIVE_SLOTS_DECLINED_STALE_CLAIM
                            .fetch_add(declined as u64, Ordering::Relaxed);
                        if cratonvm_gc::root_write_audit::enabled() {
                            note_declined_stale_claim(tid.0, mine, declined);
                        }
                    }
                }
                // t1 wave 5 lane E2: this entry is the blocked carrier of a
                // continuation slice running on `seg_tid` below `base` (its own
                // tid claim is withdrawn for the slice): adopt the captures of
                // its half of that stack, which its own wake writes back. Not
                // when it is the claimant of that tid again (the slice is over;
                // the block above adopted every word).
                for &(seg_tid, base, owner) in &segments {
                    if owner != tid.0 || (routed_here && os_tid == Some(seg_tid)) {
                        continue;
                    }
                    // addr-keyed: local to this fold (one pause).
                    let mut held: rustc_hash::FxHashSet<(usize, usize)> =
                        ns.iter().map(|e| (e.addr, e.orig)).collect();
                    for &(t, addr, value) in &scanned_stack_slots {
                        if t != seg_tid || addr < base {
                            continue;
                        }
                        if !held.insert((addr, value)) {
                            NATIVE_SLOTS_DUPLICATE_CAPTURES.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        ns.push(crate::threading::jvm_thread::NativeSlotFixup {
                            addr,
                            orig: value,
                            cur: value,
                        });
                        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_ADOPTED
                            .fetch_add(1, Ordering::Relaxed);
                        adopted_here += 1;
                    }
                }
                for e in ns.iter_mut() {
                    if let Some(&new) = pointer_map.get(&e.cur) {
                        e.cur = new;
                    }
                }
                // gci i12/vp: the native slots' share of the next filter.
                if parked {
                    vp.parked_words += ns.len();
                }
                if let Some(f) = new_filter.as_mut() {
                    for e in ns.iter() {
                        f.insert(e.cur);
                    }
                    native_slots_summarised = true;
                }
            }
            // cceres3 FIX: advance the exact per-slot tracker through THIS
            // collection's pointer map (see `GcBlockState::slot_origins`).
            // Exact lookups per map — no chain keys to strand.
            {
                let mut origins = entry.gc_block_state.slot_origins.lock();
                for so in origins.iter_mut() {
                    if let Some(&new) = pointer_map.get(&so.cur) {
                        so.cur = new;
                    }
                }
                // gci i12/vp: the slot origins' share of the next filter
                // (dead locals included: the lookup above does not tell them
                // apart, so neither may the skip).
                if parked {
                    vp.parked_words += origins.len();
                }
                if let Some(f) = new_filter.as_mut() {
                    for so in origins.iter() {
                        f.insert(so.cur);
                    }
                }
                if let Some(probe) = vacated.as_ref() {
                    // Built lazily: a frame slot landing in the vacated arena
                    // is the rare case, and this set is only needed to report
                    // one.
                    // addr-keyed: local to this fold (one pause).
                    let mut in_snapshot: Option<rustc_hash::FxHashSet<usize>> = None;
                    for so in origins.iter() {
                        // A DEAD local in the vacated arena is the per-bci
                        // liveness analysis working as designed, not a defect
                        // — see `SlotOrigin::live`.
                        if !so.live || !probe.contains(so.cur) {
                            continue;
                        }
                        let published = in_snapshot
                            .get_or_insert_with(|| {
                                snapshot.iter().map(|r| r.as_ptr() as usize).collect()
                            })
                            .contains(&so.cur);
                        // The remap above rewrote the snapshot in place, so a
                        // hit means the collector had this exact address as a
                        // root and left it behind; a miss means the deposit
                        // never published it.
                        blocked_root_gap_report(
                            tid.0,
                            so.frame,
                            so.idx,
                            so.is_stack,
                            so.orig,
                            so.cur,
                            published,
                        );
                    }
                }
            }
            // The whole-snapshot version of the same question. Gated, unlike
            // the per-slot check above, because the snapshot legitimately
            // carries CONSERVATIVE candidates — register/stack words and
            // JIT-band scans that are not object starts — and `forward_object`
            // correctly declines to relocate those, so they land in the vacated
            // arena on every moving cycle by design. Only a hit that is ALSO a
            // frame slot is unambiguous, and that is what the check above
            // reports unconditionally.
            if dbg {
                if let Some(probe) = vacated.as_ref() {
                    // gcg g5/jn: with the entry's index and the snapshot's
                    // length. `deposit_root_snapshot_inner` appends the
                    // conservative JIT-band candidates (then the shadow stack)
                    // LAST (after frames, off-frame roots and JNI locals), so
                    // an index near the end on a thread with compiled
                    // frames is a candidate the collector declines to
                    // forward by design.
                    let len = snapshot.len();
                    for (idx, r) in snapshot.iter().enumerate() {
                        let a = r.as_ptr() as usize;
                        if probe.contains(a) {
                            blocked_snapshot_gap_report(tid.0, a, idx, len);
                        }
                    }
                }
            }
            if dbg && (composed > 0 || seeded > 0) {
                eprintln!(
                    "[blockgc] fold tid={} composed={} seeded={} fixup_total={} snapshot_len={}",
                    tid.0,
                    composed,
                    seeded,
                    fixup.len(),
                    snapshot.len()
                );
            }
            // gci i12/vp: leave the filter for the next fold, tagged with the
            // epoch read BEFORE any word was read. A mount racing this pause
            // (the remount's drain waits the pause out) has bumped the epoch
            // already or will: either way the tag no longer matches.
            if let (Some(epoch), Some(f)) = (parked_epoch, new_filter) {
                if native_slots_summarised {
                    entry.parked_filter.store(epoch, f);
                    vp.stored += 1;
                } else {
                    entry.parked_filter.clear();
                }
            }
        }
        if let Some(started) = vp_started {
            eprintln!(
                "{}",
                vp.line(pointer_map.len(), filter_on, started.elapsed())
            );
        }
        // The complement of the adoption, taken after the walk so every blocked
        // thread has had its chance to claim a capture.
        if scanned_stack_slots.len() > adopted_here {
            cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_UNROUTED.fetch_add(
                (scanned_stack_slots.len() - adopted_here) as u64,
                Ordering::Relaxed,
            );
        }
    }

    /// Get the number of registered threads.
    pub fn count(&self) -> usize {
        self.threads.read().len()
    }

    /// Get the number of alive threads.
    pub fn alive_count(&self) -> usize {
        self.threads
            .read()
            .values()
            .filter(|e| e.alive.load(Ordering::Acquire))
            .count()
    }

    /// xt-hardening (2026-07-03): publish the calling thread's OS thread id
    /// for `thread_id` (see `ThreadEntry::os_tid`). Called by the thread
    /// itself at startup, before it can execute any Java/JIT code.
    pub fn set_os_tid_current(&self, thread_id: ThreadId) {
        // CRATONVM_DBG_BLOCKED_ACCESS: register THIS thread's authoritative
        // `in_blocked_region` flag with the gc crate's heap-access canary,
        // which cannot reach the `JvmThread` from the `get_header` funnel.
        // Like the os_tid publish itself, this always runs on the owning
        // thread at startup (main / spawned / native-carrier). No-op when the
        // gate is off. The leaked `Arc<GcBlockState>` clone (debug-gated, one
        // per thread) pins the flag's address for the process lifetime, which
        // is the safety contract `register_self_blocked_flag` requires.
        if cratonvm_gc::blocked_access_debug::enabled() {
            let threads = self.threads.read();
            if let Some(entry) = threads.get(&thread_id) {
                let keep = entry.gc_block_state.clone();
                let flag: *const std::sync::atomic::AtomicBool = &keep.in_blocked_region;
                std::mem::forget(keep);
                // SAFETY: `flag` points into the leaked Arc's referent above,
                // so it stays valid for the process lifetime; only the owning
                // thread reads it back through its TLS slot.
                unsafe { cratonvm_gc::blocked_access_debug::register_self_blocked_flag(flag) };
            }
        }
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetCurrentThreadId() -> u32;
            }
            let os_tid = unsafe { GetCurrentThreadId() };
            self.publish_os_tid_claim(thread_id, os_tid);
        }
        #[cfg(target_os = "linux")]
        {
            let os_tid = unsafe { libc::syscall(libc::SYS_gettid) as u32 };
            self.publish_os_tid_claim(thread_id, os_tid);
        }
        #[cfg(all(not(windows), not(target_os = "linux")))]
        {
            let _ = thread_id; // takeover machinery is currently Windows/Linux-only
        }
    }

    /// gcd d3/k: publish the calling OS thread's id for a foreign-attached
    /// thread leaving its idle window (`jni::foreign_leave_idle`, under
    /// `CRATONVM_JNI_FOREIGN_TRANSITIONS`). [`Self::set_os_tid_current`]
    /// without its once-per-thread debug canary registration, which leaks one
    /// `Arc` per call and is not meant to run per transition.
    pub fn republish_os_tid_current(&self, thread_id: ThreadId) {
        let os_tid = current_os_tid();
        if os_tid != 0 {
            self.publish_os_tid_claim(thread_id, os_tid);
        }
    }

    /// gcd d3/k: withdraw `thread_id`'s published OS tid (store 0) as a
    /// foreign-attached thread goes idle in host code
    /// (`jni::foreign_enter_idle`). An idle attachment has no Java or compiled
    /// frame, so no take-over or helper-window pass has anything to read on
    /// it, and with the tid published every pass would signal (Linux) or
    /// suspend (Windows) a host thread parked in `epoll_wait` for nothing. The
    /// ordinal is left alone: a `0` tid matches no claimant query.
    pub fn clear_os_tid(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.os_tid.store(0, Ordering::Release);
        }
    }

    /// Store `os_tid` for `thread_id`, then stamp the entry with a fresh
    /// publish ordinal (see `ThreadEntry::os_tid_seq`). The order matters to
    /// [`Self::thread_id_for_current_os_tid`]: a reader that sees the new
    /// ordinal with `Acquire` also sees the new `os_tid`, so an entry moving
    /// to another carrier can never be read as "newest on the old one".
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    fn publish_os_tid_claim(&self, thread_id: ThreadId, os_tid: u32) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            // gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`):
            // every caller passes the CALLING OS thread's tid (startup, a
            // virtual thread's mount, a foreign thread leaving idle), before it
            // runs Java or compiled code there. Publish this OS thread's JIT
            // deopt stash cells beside it, so a take-over that freezes the
            // thread in compiled code can root its stashed frames
            // (`stw_take_over_and_wait`'s frozen-peer walk). A no-op when the
            // triple is already published.
            publish_deopt_stash_cells_into(&entry.gc_block_state, os_tid);
            entry.os_tid.store(os_tid, Ordering::Release);
            let seq = OS_TID_PUBLISH_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            entry.os_tid_seq.store(seq, Ordering::Release);
        }
    }

    /// xt-hardening (2026-07-03): STW-ready alive-thread count PLUS the
    /// published OS tids of those counted threads, read under one registry lock
    /// so the GC barrier's `expected` and the takeover's counted-set snapshot
    /// cannot disagree about which threads exist.
    ///
    /// A `Thread.start()` child is alive before its carrier can answer a
    /// safepoint. It stays out of this counted set until `mark_stw_ready` flips
    /// the startup gate; if an STW is already active, the child waits it out via
    /// `arrive_and_wait_excluded` first.
    pub fn alive_count_and_os_tids(&self) -> (usize, Vec<u32>) {
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("alive_count_and_os_tids");
        let len = || self.threads.read().len();
        PERF.time(len, || self.alive_count_and_os_tids_inner())
    }

    fn alive_count_and_os_tids_inner(&self) -> (usize, Vec<u32>) {
        // JIT round 16 wave 9b (lane ffm): the take-over's roster leaves out a
        // thread in foreign code (`ffm_in_native_signal_exempt`); the COUNT is
        // unchanged.
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        let mut n = 0usize;
        let mut tids = Vec::with_capacity(threads.len());
        for e in threads.values() {
            if entry_is_stw_live(e) && e.stw_ready.load(Ordering::Acquire) {
                n += 1;
                if exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state) {
                    continue;
                }
                let t = e.os_tid.load(Ordering::Acquire);
                if t != 0 {
                    tids.push(t);
                }
            }
        }
        (n, tids)
    }

    /// Alive-thread count, live blocked-thread count, and alive OS tids read
    /// under one registry lock.
    ///
    /// The blocked count is the production STW exclusion count: a thread sets
    /// `in_blocked_region` only after depositing its root snapshot, and that
    /// publication can race ahead of the barrier's anonymous `threads_blocked`
    /// counter. Reading it with the alive set keeps the expected mutator quota
    /// aligned with the root/fixup state the collector will actually scan.
    ///
    /// STWREADY-GAP-FIX (2026-07-11): also gate on `stw_ready`, matching the
    /// sibling `alive_count_and_os_tids` (added 2026-07-03, "identity-based
    /// barrier excusal for xt-takeover"). Without this, a `Thread.start()`
    /// child is alive (registered in this map) before its carrier can answer
    /// a safepoint -- `vm_exec.rs`'s `thread_start` startup loop keeps such a
    /// thread out of `mark_stw_ready` until it observes NO active STW
    /// (`run_if_no_stw_requested`), and if one IS active when it starts, it
    /// calls the NON-participating `arrive_and_wait_excluded` -- which never
    /// increments the barrier's `arrived` counter. A pause whose `expected`
    /// snapshot (this function) included that same not-yet-ready thread can
    /// then never satisfy its quota: `arrived` structurally cannot reach
    /// `expected`, and `wait_for_all`/the STW requester's own wait loop
    /// blocks forever -- a whole-VM freeze with zero forward progress,
    /// confirmed via gdb (the stuck threads sit in
    /// `GcBarrier::arrive_and_wait_excluded` from `thread_start`'s startup
    /// loop, never having deposited a root snapshot or reached any
    /// safepoint). A not-yet-`stw_ready` thread cannot yet be executing Java
    /// bytecode (that is exactly what the gate protects), so excluding it
    /// from `expected` cannot let it mutate the heap concurrently with an
    /// evacuating collection -- there is nothing for the collector to race
    /// against on that thread until it actually marks itself ready, at which
    /// point it becomes visible to the NEXT pause's snapshot as normal.
    ///
    /// GCAUDIT-0711-FIX (finding 1a): also returns the IDENTITIES
    /// (`ThreadId.0`) of the alive threads counted in `blocked`, so
    /// `GcBarrier::request_stw_counted_with_live_blocked` can publish exactly
    /// which threads THIS pause excluded — see
    /// `GcBarrierInner::excluded_blocked`'s doc for why per-thread identity,
    /// not just a count, is required for a race-free arrival decision.
    pub fn alive_count_blocked_and_os_tids(&self) -> (usize, usize, Vec<u32>, Vec<u64>) {
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("alive_count_blocked_and_os_tids");
        let len = || self.threads.read().len();
        PERF.time(len, || self.alive_count_blocked_and_os_tids_inner())
    }

    fn alive_count_blocked_and_os_tids_inner(&self) -> (usize, usize, Vec<u32>, Vec<u64>) {
        let threads = self.threads.read();
        let mut alive = 0usize;
        let mut blocked = 0usize;
        let mut tids = Vec::with_capacity(threads.len());
        let mut blocked_tids = Vec::new();
        for (tid, e) in threads.iter() {
            if entry_is_stw_live(e) && e.stw_ready.load(Ordering::Acquire) {
                alive += 1;
                // gci i20/jt: a thread in a lock-free in-native leave is counted.
                if e.gc_block_state.census_blocked() {
                    blocked += 1;
                    blocked_tids.push(tid.0);
                }
                let t = e.os_tid.load(Ordering::Acquire);
                if t != 0 {
                    tids.push(t);
                }
            }
        }
        (alive, blocked, tids, blocked_tids)
    }

    /// The STW census of [`Self::alive_count_blocked_and_os_tids`] plus whether
    /// `initiator` owns one of the counted slots — the fact
    /// `GcBarrier`'s `StwCensus::initiator_counted` carries (gc-common w3-a,
    /// 2026-09-23; `handoff-w2a-census-reports-initiator`,
    /// `docs/feature-designs/jdk-only-mode.md`).
    ///
    /// `expected = alive - 1 - blocked` assumes the initiator is one of the
    /// `alive` threads. The door can only tell that its OWN id is counted; it
    /// cannot tell "not counted at all" (an unregistered host thread, a carrier
    /// before `mark_stw_ready`, a thread after `mark_dead`) from "counted under
    /// another id" (a virtual thread mounted on a registered carrier runs on the
    /// carrier's OS thread). Answering "not counted" in the second case would
    /// make the barrier wait for a slot that is the initiator itself — a hang.
    /// Only this registry has both the entries' published `os_tid`s and the
    /// calling OS thread's id, so the answer is computed here, under the same
    /// lock hold as the census itself:
    ///
    /// * `Some(true)` — `initiator`'s own entry is counted, or a counted entry
    ///   is published on the calling OS thread;
    /// * `Some(false)` — neither, and every counted entry has published its
    ///   `os_tid`, so none of them can be this thread;
    /// * `None` — undecidable: the platform has no OS-tid backend, or a counted
    ///   entry has not published its `os_tid` yet and could be this thread.
    ///   The barrier then keeps the historical `- 1`.
    ///
    /// Runs under the barrier lock (inside the request's census closure), so it
    /// is the same single `threads.read()` walk as the plain census.
    pub fn alive_count_blocked_and_os_tids_for(
        &self,
        initiator: ThreadId,
    ) -> (usize, usize, Vec<u32>, Vec<u64>, Option<bool>) {
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("alive_count_blocked_and_os_tids_for");
        let len = || self.threads.read().len();
        PERF.time(len, || {
            self.alive_count_blocked_and_os_tids_for_inner(initiator, current_os_tid())
        })
    }

    /// [`Self::alive_count_blocked_and_os_tids_for`] with the calling OS
    /// thread's id supplied, so a test can place the "caller" on any OS thread
    /// (`me == 0` = no backend).
    fn alive_count_blocked_and_os_tids_for_inner(
        &self,
        initiator: ThreadId,
        me: u32,
    ) -> (usize, usize, Vec<u32>, Vec<u64>, Option<bool>) {
        let threads = self.threads.read();
        let mut alive = 0usize;
        let mut blocked = 0usize;
        let mut tids = Vec::with_capacity(threads.len());
        let mut blocked_tids = Vec::new();
        let (mut own_counted, mut os_counted, mut unknown_os) = (false, false, false);
        for (tid, e) in threads.iter() {
            if entry_is_stw_live(e) && e.stw_ready.load(Ordering::Acquire) {
                alive += 1;
                // gci i20/jt: a thread in a lock-free in-native leave is counted.
                if e.gc_block_state.census_blocked() {
                    blocked += 1;
                    blocked_tids.push(tid.0);
                }
                let t = e.os_tid.load(Ordering::Acquire);
                if t != 0 {
                    tids.push(t);
                    os_counted |= me != 0 && t == me;
                } else {
                    unknown_os = true;
                }
                own_counted |= *tid == initiator;
            }
        }
        let verdict = initiator_counted_verdict(own_counted, os_counted, me != 0, unknown_os);
        (alive, blocked, tids, blocked_tids, verdict)
    }

    /// DIAGNOSTIC (2026-07-13, STW takeover 5-class cluster investigation):
    /// `ThreadId`s of every alive, `stw_ready` thread NOT in `excluded` —
    /// i.e. the exact identity set a `request_stw_counted_with_live_blocked`
    /// caller is about to count as "expected". Not lock-atomic with a prior
    /// `alive_count_blocked_and_os_tids` call, but both are invoked from
    /// inside the SAME `request_stw_counted_with_live_blocked` closure, which
    /// runs under the barrier's transition lock — no mutator can flip
    /// `in_blocked_region` in a way that would matter between the two calls
    /// for diagnostic purposes.
    pub fn alive_thread_ids_excluding(&self, excluded: &[u64]) -> Vec<u64> {
        let threads = self.threads.read();
        threads
            .iter()
            .filter(|(tid, e)| {
                entry_is_stw_live(e)
                    && e.stw_ready.load(Ordering::Acquire)
                    && !excluded.contains(&tid.0)
            })
            .map(|(tid, _)| tid.0)
            .collect()
    }

    /// Get Java Thread objects for all alive threads (up to `max` entries).
    pub fn alive_thread_objects(&self, max: usize) -> Vec<ObjectRef> {
        self.threads
            .read()
            .values()
            .filter(|e| e.alive.load(Ordering::Acquire))
            .filter_map(|e| e.java_thread_obj)
            .take(max)
            .collect()
    }

    /// [`Self::alive_thread_objects`], every one, in `ThreadId` order
    /// (interpreter round i5 wave 102, lane L1): JVMTI `GetAllThreads`,
    /// `GetAllStackTraces` and `GetThreadGroupChildren` list threads in
    /// HotSpot's threads-list order (each thread appended as it attaches),
    /// not in the map's hash order, which differed from run to run.
    pub fn alive_thread_objects_in_id_order(&self) -> Vec<ObjectRef> {
        let mut live: Vec<(ThreadId, ObjectRef)> = self
            .threads
            .read()
            .iter()
            .filter(|(_, e)| e.alive.load(Ordering::Acquire))
            .filter_map(|(tid, e)| e.java_thread_obj.map(|obj| (*tid, obj)))
            .collect();
        live.sort_unstable_by_key(|(tid, _)| tid.0);
        live.into_iter().map(|(_, obj)| obj).collect()
    }

    /// T19.K1 — Enumerate the `ThreadId`s of every currently-alive
    /// non-daemon thread. The returned `Vec` is a snapshot taken under
    /// the registry mutex; concurrent transitions (a non-daemon thread
    /// terminating, or a thread that hasn't been registered yet starting)
    /// are not reflected.
    ///
    /// `ThreadId(0)` (the "main" thread) is always excluded: the CLI
    /// invokes this query *from* the main thread after `main()`
    /// returns, so including it would deadlock. Tests register
    /// non-zero ids to exercise the join path.
    pub fn alive_non_daemon_thread_ids(&self) -> Vec<ThreadId> {
        self.threads
            .read()
            .iter()
            .filter(|(tid, e)| {
                tid.0 != 0 && e.alive.load(Ordering::Acquire) && !e.daemon.load(Ordering::Acquire)
            })
            .map(|(tid, _)| *tid)
            .collect()
    }

    /// T19.K1 — Block the caller until every non-daemon thread that
    /// currently exists in the registry has terminated.
    ///
    /// Per the JVM specification (JLS §12.8 / §17), the VM keeps running
    /// until every thread that *isn't* a daemon completes. Daemon threads
    /// (event loops, GC workers, finalisers, …) may still be running when
    /// this call returns — the caller is expected to terminate the
    /// process at that point and let the OS clean those up.
    ///
    /// # Algorithm
    ///
    /// We snapshot the set of non-daemon thread ids, attempt to take each
    /// one's `JoinHandle` out of the registry, and join on it. If a
    /// thread has already finished and its handle was already taken,
    /// `join()` returns `false` quickly. After joining everything in the
    /// snapshot we re-snapshot and repeat — this covers the case where
    /// a non-daemon thread spawned more non-daemon threads and exited
    /// while we were joining its predecessor.
    ///
    /// # Termination
    ///
    /// The loop terminates when a fresh snapshot is empty. If a hostile
    /// or buggy program keeps spawning non-daemon threads, the loop
    /// will not exit — same as HotSpot. The optional `deadline` lets
    /// the CLI bound this for diagnostic / CI runs (`None` ≡ wait
    /// forever; `Some(_)` ≡ best-effort with a timeout).
    ///
    /// # Returns
    ///
    /// The number of distinct non-daemon threads that were joined (i.e.
    /// the number of `JoinHandle::join` calls that succeeded). `0` means
    /// no non-daemon threads existed when the call was made — the safe
    /// fast-path for `HelloWorld`-style programs.
    pub fn wait_for_non_daemon_threads(&self, deadline: Option<std::time::Instant>) -> usize {
        let mut joined = 0usize;
        loop {
            let pending = self.alive_non_daemon_thread_ids();
            if pending.is_empty() {
                return joined;
            }
            for tid in pending {
                if let Some(d) = deadline {
                    if std::time::Instant::now() >= d {
                        return joined;
                    }
                }
                // T19.K1 — `join()` takes the handle out, joins on it,
                // and marks the thread dead. If the handle was already
                // taken (e.g. a sibling pumped through `Thread.join()`
                // first) we only count successful joins. We still loop
                // back so we re-snapshot for newly-spawned non-daemon
                // threads.
                // At most ~1 s per handle-less thread, as before wave 24
                // (the outer loop re-snapshots), and never past `deadline`.
                let give_up = std::time::Instant::now() + std::time::Duration::from_secs(1);
                let give_up = deadline.map_or(give_up, |d| d.min(give_up));
                if self.join_giving_up_at(tid, Some(give_up)) {
                    joined += 1;
                }
            }
        }
    }
}

/// Rate limit for the two blocked-root-gap reporters below. The defect
/// cascades — one blocked thread typically has many slots pointing at the
/// same lost subgraph — and the first handful carry all the information.
const BLOCKED_ROOT_GAP_REPORTS: u64 = 12;

/// gcg g6/jn (2026-10-02),
/// `gcg-g5jn-unrecorded-root-guard-reads-a-conservative-deposit-as-precise`:
/// `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS` (gch h2/yg: default ON,
/// `runtime_flag_default_on`; `=0` restores the gch h1 default). On, every
/// collection's snapshot gather publishes the words of the snapshots
/// deposited with the conservative interpreter-local probe on
/// (`GcBlockState::deposit_conservative`) to the pause ledger
/// (`gc_quiescence::publish_conservative_deposit_words`), and the
/// Generational unrecorded-young-root guard excludes them on a cycle it
/// otherwise treats as precise -- except a blocked thread's live locals and
/// operand slots, which stay guarded (gch h1/yp). Read once per gather.
///
/// Flipped on gch vh1-yp block A: 8 interleaved pairs, stdout SAME 16/16;
/// the `_def` arm refused 2 young cycles (2 of 8 runs, `root_source=None`,
/// zero headers -- the g5jn signature), the `_dep` arm 0 of 8, with
/// `[blockgc] conservative-deposit words=15` engaged on every `_dep` run.
fn unrecorded_root_guard_deposits_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS")
}

/// `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_PARKED_PRECISE` (gch h3/yc: default ON,
/// `runtime_flag_default_on`; `=0` restores the h2 gather). With
/// [`unrecorded_root_guard_deposits_enabled`] on, a PARKED thread's
/// conservative snapshot keeps the precise words its safepoint deposit
/// recorded (`GcBlockState::precise_deposit_words`) under the guard, as a
/// blocked thread's `slot_origins` words are kept; `=0` excludes the whole
/// parked snapshot again
/// (`gch-h2yg-parked-conservative-snapshot-is-excluded-whole-from-the-young-root-guard`).
/// Inert until the safepoint deposit fills the field. Read once per gather.
fn unrecorded_root_guard_parked_precise_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_GEN_UNRECORDED_ROOT_GUARD_PARKED_PRECISE",
    )
}

/// gcg g6/jn: append `snapshot`'s words to `out` when the snapshot was
/// deposited conservatively. The deposit probed EVERY interpreter local
/// (`scan_local_objects_all_live`, dead ones included) because some thread was
/// in compiled code then; a later cycle with no compiled frame live must not
/// read those words as precise roots. The whole snapshot goes: the deposit
/// does not record which entries the probe added, and excluding a precise
/// word only returns the guard to its pre-gcd-d2/h fail-open for that word.
///
/// gch h1/yp: except the words in `precise` (sorted, from
/// [`precise_slot_origin_words`]): the values of the live locals and operand
/// slots a blocked thread's deposit recorded. Those are precise roots, so the
/// guard keeps checking them, and a dangling live local still refuses its
/// cycle with the switch on. A `precise` that is stale (an earlier window's)
/// only keeps a word under the guard, i.e. at most one refused cycle per
/// address: the switch-off behaviour for that word, never a dangle. Pure.
fn conservative_deposit_words(
    conservative: bool,
    snapshot: &[ObjectRef],
    precise: &[usize],
    out: &mut Vec<usize>,
) {
    if conservative {
        out.extend(
            snapshot
                .iter()
                .map(|r| r.as_ptr() as usize)
                .filter(|a| precise.binary_search(a).is_err()),
        );
    }
}

/// gch h1/yp: the current values (`cur`) of the slots in `origins` that were
/// precise roots at the deposit -- a LIVE local, or an operand-stack slot
/// (always live, `SlotOrigin::live`) -- sorted and deduplicated for
/// [`conservative_deposit_words`]. Pure.
fn precise_slot_origin_words(origins: &[crate::threading::jvm_thread::SlotOrigin]) -> Vec<usize> {
    let mut words: Vec<usize> = origins
        .iter()
        .filter(|so| so.is_stack || so.live)
        .map(|so| so.cur)
        .collect();
    words.sort_unstable();
    words.dedup();
    words
}

/// gch h3/yc: a parked thread's `GcBlockState::precise_deposit_words`, sorted
/// and deduplicated for [`conservative_deposit_words`]. Pure.
fn sorted_precise_words(words: &[usize]) -> Vec<usize> {
    let mut out = words.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

/// gcg g4/jn: how many moved addresses the `CRATONVM_DBG_BLOCKGC` line for an
/// UNBLOCKED thread lists (the count is always complete).
const UNBLOCKED_MOVED_REPORT_ADDRS: usize = 8;

/// gcg g4/jn: how many entries of `snapshot` name an address `pointer_map`
/// moves, and the first `max` of those addresses (snapshot order). Pure; the
/// diagnostic half of [`ThreadRegistry::fold_pointer_map_into_blocked_audited`]
/// for a thread that is NOT in a blocked region.
fn moved_snapshot_entries(
    snapshot: &[ObjectRef],
    pointer_map: &cratonvm_types::PointerMap,
    max: usize,
) -> (usize, Vec<usize>) {
    let mut moved = 0usize;
    let mut first = Vec::new();
    for r in snapshot {
        let a = r.as_ptr() as usize;
        if pointer_map.contains_key(&a) {
            moved += 1;
            if first.len() < max {
                first.push(a);
            }
        }
    }
    (moved, first)
}

/// A blocked thread's frame slot still points into the arena this moving
/// cycle evacuated: the object it names was NOT relocated, so on wake the
/// slot reads the all-zero header the collector left behind.
///
/// `in_snapshot` is the fork that decides where the fix goes. `false` (the
/// common case) means `deposit_root_snapshot`'s frame scan never published
/// this slot, so the collector could not have known to evacuate it — a
/// root-coverage gap in the deposit. `true` means the collector held this
/// exact address as a root and left it behind anyway — an evacuation gap.
fn blocked_root_gap_report(
    tid: u64,
    frame: u32,
    idx: u32,
    is_stack: bool,
    orig: usize,
    cur: usize,
    in_snapshot: bool,
) {
    static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if R.fetch_add(1, Ordering::Relaxed) >= BLOCKED_ROOT_GAP_REPORTS {
        return;
    }
    tracing::error!(
        target: "cratonvm::gc::guard",
        tid = tid,
        frame = frame,
        slot = if is_stack { "stack" } else { "local" },
        idx = idx,
        deposited_addr = format!("{orig:#x}"),
        current_addr = format!("{cur:#x}"),
        was_a_scanned_root = in_snapshot,
        "blocked-thread frame slot points into the semispace this moving cycle just \
         evacuated — the object was not relocated and the slot will read an all-zero \
         `java.lang.Object` header on wake. `was_a_scanned_root=false` means the \
         blocking deposit never published this slot (root-coverage gap); `true` means \
         the collector was handed it and did not evacuate it (evacuation gap).",
    );
}

/// A blocked thread's deposited root snapshot still names the evacuated
/// arena after the fold remapped it.
///
/// `CRATONVM_DBG_BLOCKGC` only. Most hits are benign by construction: the
/// snapshot carries conservative candidates (register/stack words, JIT-band
/// scans) that are not object starts, and `forward_object` declines to
/// relocate anything `young_object_starts` does not vouch for. A hit is only
/// interesting when the SAME address is also a frame slot — which
/// [`blocked_root_gap_report`] reports on its own, unconditionally.
fn blocked_snapshot_gap_report(tid: u64, addr: usize, idx: usize, snapshot_len: usize) {
    static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if R.fetch_add(1, Ordering::Relaxed) >= BLOCKED_ROOT_GAP_REPORTS {
        return;
    }
    tracing::error!(
        target: "cratonvm::gc::guard",
        tid = tid,
        obj = format!("{addr:#x}"),
        idx = idx,
        snapshot_len = snapshot_len,
        "blocked-thread root snapshot still names the semispace this moving cycle just \
         evacuated, after the fold's remap — the collector scanned this root and did not \
         relocate it. The next collection would scan the same stale address.",
    );
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ThreadRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let threads = self.threads.read();
        f.debug_struct("ThreadRegistry")
            .field("total", &threads.len())
            .field(
                "alive",
                &threads
                    .values()
                    .filter(|e| e.alive.load(Ordering::Acquire))
                    .count(),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// gci i12/vp: parked virtual threads as roots of every pause
// (`gci-i12vp-parked-continuations-folded-by-every-young-pause-FIXED-20261016`).
// ---------------------------------------------------------------------------

/// [`FoldAddrFilter`]'s granule: 1 MiB, no larger than the smallest G1
/// region and far below the Generational young semispace, so a young (or
/// collection-set) granule and an old one share a bit only through the
/// modulo-256 fold, i.e. only on a heap span above 256 MiB.
const FOLD_FILTER_GRANULE_SHIFT: u32 = 20;

/// A 256-bit summary of a set of addresses: bit `(addr >> 20) & 255`. Two
/// sets with a common address share a bit, so `!a.intersects(&b)` PROVES
/// them disjoint. The converse does not hold; a false "may intersect" costs
/// only the full fold it would have run anyway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FoldAddrFilter([u64; 4]);

impl FoldAddrFilter {
    #[inline]
    fn bucket(addr: usize) -> usize {
        (addr >> FOLD_FILTER_GRANULE_SHIFT) & 255
    }

    #[inline]
    pub(crate) fn insert(&mut self, addr: usize) {
        let b = Self::bucket(addr);
        self.0[b >> 6] |= 1u64 << (b & 63);
    }

    /// Every granule of `[lo, hi)`.
    pub(crate) fn insert_range(&mut self, lo: usize, hi: usize) {
        if hi <= lo {
            return;
        }
        let first = lo >> FOLD_FILTER_GRANULE_SHIFT;
        let last = (hi - 1) >> FOLD_FILTER_GRANULE_SHIFT;
        if last - first >= 255 {
            self.0 = [u64::MAX; 4];
            return;
        }
        for g in first..=last {
            self.insert(g << FOLD_FILTER_GRANULE_SHIFT);
        }
    }

    #[inline]
    pub(crate) fn intersects(&self, other: &FoldAddrFilter) -> bool {
        self.0.iter().zip(other.0.iter()).any(|(a, b)| a & b != 0)
    }

    fn is_full(&self) -> bool {
        self.0.iter().all(|w| *w == u64::MAX)
    }
}

/// The filter of everything one blocked fold can rewrite or report: every
/// key of `pointer_map` (each lookup the fold makes is a key test) and the
/// arena the cycle vacated (`vacated`: the Generational inactive semispace
/// and a pinned in-place cycle's spans), which the fold's `blocked-root-gap`
/// audits test the same words against. Stops early once every bit is set.
fn pointer_map_fold_filter(
    pointer_map: &cratonvm_types::PointerMap,
    vacated: (Option<(usize, usize)>, &[(usize, usize)]),
) -> FoldAddrFilter {
    let mut f = FoldAddrFilter::default();
    let (inactive, spans) = vacated;
    if let Some((lo, hi)) = inactive {
        f.insert_range(lo, hi);
    }
    for &(a, len) in spans {
        f.insert_range(a, a.saturating_add(len));
    }
    for (i, &k) in pointer_map.keys().enumerate() {
        if i % 4096 == 0 && f.is_full() {
            break;
        }
        f.insert(k);
    }
    f
}

/// One registry entry's cached [`FoldAddrFilter`] (`ThreadEntry::parked_filter`):
/// valid only while `ThreadEntry::mount_epoch` is the value it was stored for.
/// Written and read only by a pause's blocked fold (pauses are serialised by
/// the barrier), so `Relaxed` words under an `Acquire` / `Release` tag.
pub(crate) struct ParkedFoldFilter {
    /// `mount_epoch + 1` at the store; `0` = no filter.
    valid_at: AtomicU64,
    bits: [AtomicU64; 4],
}

impl ParkedFoldFilter {
    pub(crate) fn new() -> Self {
        Self {
            valid_at: AtomicU64::new(0),
            bits: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// The filter stored for `epoch`, if that is the one stored.
    pub(crate) fn cached(&self, epoch: u64) -> Option<FoldAddrFilter> {
        if self.valid_at.load(Ordering::Acquire) != epoch.wrapping_add(1) {
            return None;
        }
        Some(FoldAddrFilter(std::array::from_fn(|i| {
            self.bits[i].load(Ordering::Relaxed)
        })))
    }

    pub(crate) fn store(&self, epoch: u64, f: FoldAddrFilter) {
        self.valid_at.store(0, Ordering::Relaxed);
        for (cell, word) in self.bits.iter().zip(f.0) {
            cell.store(word, Ordering::Relaxed);
        }
        self.valid_at.store(epoch.wrapping_add(1), Ordering::Release);
    }

    pub(crate) fn clear(&self) {
        self.valid_at.store(0, Ordering::Release);
    }
}

/// gci i13/vg: `ThreadRegistry::parked_split` -- the young filter armed for
/// the next gather, and the old-only section that gather left.
///
/// gci i21/pr: also the collection door's initiator-snapshot skip
/// ([`ThreadRegistry::arm_initiator_snapshot_skip`]) -- the same one-gather
/// arm, consumed by the same `collect_all_root_snapshots` -- and the words
/// that gather left out (`None`: no armed gather ran since the arm).
#[derive(Default)]
struct ParkedSplitGather {
    /// gcj j3/x2 R10: with the arming thread; only a gather on it consumes it.
    armed: Option<(FoldAddrFilter, std::thread::ThreadId)>,
    old_only: Vec<ObjectRef>,
    /// gcj j3/x2 R10: plain gathers on another thread that left the split alone.
    split_foreign_gathers: u64,
    /// gcj j3/x2: with the arming thread; only a gather on it consumes the arm.
    initiator_skip: Option<(Arc<Mutex<Vec<ObjectRef>>>, std::thread::ThreadId)>,
    initiator_skipped: Option<usize>,
    /// gcj j3/x2: plain gathers on another thread that left an armed skip alone.
    initiator_foreign_gathers: u64,
}

/// gci i13/vg: may this entry's snapshot go to the OLD-ONLY root section of a
/// split gather ([`ThreadRegistry::collect_all_root_snapshots_split`])?
///
/// Only an UNMOUNTED continuation parked in its blocked region (its words are
/// the yield deposit's, advanced only by the blocked fold since), with a
/// cached [`FoldAddrFilter`] that is valid for the entry's CURRENT
/// `mount_epoch` and disjoint from `young`. The filter summarises every
/// snapshot word as the last fold left it (a superset: it also covers the
/// fixup values, native slots and slot origins), and an unchanged epoch means
/// no mount, unmount, snapshot or block-state replacement happened since
/// (`set_mounted` / `set_root_snapshot` / `set_gc_block_state` bump it), so
/// "disjoint" proves no snapshot word is a young address NOW. A mount racing
/// this pause bumps the epoch before its drain, and the drain waits the pause
/// out, so the words read here are still the parked ones either way.
fn parked_snapshot_is_old_only(entry: &ThreadEntry, young: &FoldAddrFilter) -> bool {
    if entry.mounted.load(Ordering::Acquire)
        || !entry
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
    {
        return false;
    }
    let epoch = entry.mount_epoch.load(Ordering::Acquire);
    entry
        .parked_filter
        .cached(epoch)
        .is_some_and(|f| !f.intersects(young))
}

/// gci i14/ve: `ThreadRegistry::mirror_keyed` -- the door's request for the
/// next gather, the `(tid, mirror)` pairs that gather left out (the same
/// pause's sweep takes them) and the ids that sweep retired (taken after the
/// pause).
#[derive(Default)]
struct MirrorKeyedGather {
    armed: Option<MirrorKeyedArm>,
    deferred: Vec<(ThreadId, usize)>,
    collected: Vec<ThreadId>,
    /// Pauses that keyed nothing because the virtual-thread manager's lock
    /// was contended at the arm (cumulative, per VM; census only).
    skipped_contended: u64,
    /// gci i15/vh (G1): the `(tid, mirror)` pairs the remark's completed mark
    /// left unmarked ([`ThreadRegistry::judge_mirror_keyed_parked_after_mark`]),
    /// retired or dropped by the same pause's
    /// [`ThreadRegistry::settle_mirror_keyed_parked_after_mark`] once cleanup
    /// said whether it trusted that mark.
    judged_dead: Vec<(ThreadId, usize)>,
    /// gci i15/vh: how many pairs that judgement read (census only).
    judged_deferred: usize,
    /// gci i15/vh: a G1 marking scan (initial mark or remark) keyed at least
    /// one thread since the last remark judgement; that judgement then marks
    /// the rows of every live owner to a fixed point first
    /// ([`mark_live_row_values_to_fixpoint`]). Taken by the judgement.
    g1_cycle_keyed: bool,
}

/// gci i14/ve: one mirror-keyed request ([`ThreadRegistry::arm_mirror_keyed_gather`]).
pub(crate) struct MirrorKeyedArm {
    /// This VM's identity: the `metadata_pin` rows are filed under it.
    vm: usize,
    /// The Generational old generation, `[old_base, old_base + old_len)`.
    old_base: usize,
    old_len: usize,
    /// gci i15/vh: G1's `Old` regions, sorted, non-overlapping `[lo, hi)`
    /// ranges ([`ThreadRegistry::arm_mirror_keyed_gather_ranges`]); empty on
    /// the Generational arm.
    old_ranges: Vec<(usize, usize)>,
    /// Registry ids of the JDK continuations deposited in the manager.
    candidates: rustc_hash::FxHashSet<u64>,
}

impl MirrorKeyedArm {
    fn is_old(&self, addr: usize) -> bool {
        (self.old_len != 0 && addr.wrapping_sub(self.old_base) < self.old_len)
            || addr_in_sorted_ranges(&self.old_ranges, addr)
    }
}

/// gci i15/vh: is `addr` inside one of `ranges` (sorted by start,
/// non-overlapping, each `[lo, hi)`)? Binary search; `false` when empty.
fn addr_in_sorted_ranges(ranges: &[(usize, usize)], addr: usize) -> bool {
    let i = ranges.partition_point(|&(_, hi)| hi <= addr);
    ranges.get(i).is_some_and(|&(lo, hi)| lo <= addr && addr < hi)
}

/// gci i15/vh: hand `mark` the values of every row of `rows` (owner ->
/// values, a `metadata_pin::snapshot`) whose owner `is_live`, then again for
/// the owners that turned live through those marks, until none does. Each
/// owner's values are handed over once; the loop ends because every round
/// that marks anything takes at least one owner out of the remaining set.
/// `is_live` must read the marks `mark` makes (the G1 door passes
/// `is_live_after_mark` and `resurrect_after_remark`). Returns how many
/// values were handed over. See
/// [`ThreadRegistry::judge_mirror_keyed_parked_after_mark`].
///
/// gci i16/mr: `pub(crate)` for the G1 remark's default-path closure of every
/// row (`gc_and_alloc.rs` `g1_remark_close_metadata_rows`,
/// `gci-i15vh-g1-marking-scan-rows-filed-while-the-marker-runs-FIXED-20261017`).
pub(crate) fn mark_live_row_values_to_fixpoint(
    // addr-keyed: borrowed for one remark (the caller's snapshot of the rows, consumed under the pause).
    rows: &FxHashMap<usize, Vec<usize>>,
    is_live: &dyn Fn(usize) -> bool,
    mark: &mut dyn FnMut(&[usize]),
) -> usize {
    let mut pending: Vec<usize> = rows.keys().copied().collect();
    let mut handed = 0usize;
    loop {
        let (live, dead): (Vec<usize>, Vec<usize>) =
            pending.into_iter().partition(|&owner| is_live(owner));
        pending = dead;
        let values: Vec<usize> = live
            .iter()
            .filter_map(|owner| rows.get(owner))
            .flat_map(|v| v.iter().copied())
            .collect();
        if live.is_empty() {
            return handed;
        }
        if !values.is_empty() {
            handed += values.len();
            mark(&values);
        }
        if pending.is_empty() {
            return handed;
        }
    }
}

/// gci i15/vh (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED` on G1): remove from a
/// plain `roots::collect_roots` result exactly ONE occurrence of each mirror
/// a keyed gather left out (`mirrors`, one entry per keyed thread). Step 10b
/// of that scan pushes `alive_thread_objects(usize::MAX)` -- every alive
/// entry's `java_thread_obj`, each exactly once -- and a keyed entry is alive
/// with that mirror, so one occurrence is step 10b's; any other occurrence
/// is a real root (a JNI global, another frame, a `ThreadLocal` handle) and
/// stays. The Generational door has no such duplicate
/// (`collect_roots_registry_appended` skips step 10b). Returns how many were
/// removed; a mirror not found removes nothing (never a real root).
pub(crate) fn strip_keyed_mirror_registry_roots(
    roots: &mut Vec<ObjectRef>,
    mirrors: &[usize],
) -> usize {
    if mirrors.is_empty() {
        return 0;
    }
    // addr-keyed: local to this fixed point (one remark).
    let mut owed: rustc_hash::FxHashMap<usize, usize> = rustc_hash::FxHashMap::default();
    for &m in mirrors {
        *owed.entry(m).or_insert(0) += 1;
    }
    let mut removed = 0usize;
    roots.retain(|r| {
        let addr = r.as_ptr() as usize;
        match owed.get_mut(&addr) {
            Some(n) if *n > 0 => {
                *n -= 1;
                removed += 1;
                false
            }
            _ => true,
        }
    });
    removed
}

/// gci i14/ve: what a mirror-keyed gather produced: `(mirror, word)` rows and
/// the `(tid, mirror)` of every entry it left out.
#[derive(Default)]
pub(crate) struct MirrorKeyedOut {
    rows: Vec<(usize, usize)>,
    deferred: Vec<(ThreadId, usize)>,
}

/// gci i14/ve: may this entry be keyed by its mirror on this pause
/// ([`ThreadRegistry::collect_all_root_snapshots`])? `Some(mirror address)`
/// only for:
///
/// * a JDK continuation whose state is deposited in the virtual-thread
///   manager (`arm.candidates`: the yield finished, no mount has taken it,
///   no debugger is reading it), UNMOUNTED and in its blocked region (the
///   words are the yield deposit's, advanced only by the blocked fold);
/// * a PRECISE deposit (`deposit_conservative` down): every word is an exact
///   object reference (a tagged frame slot recorded in `slot_origins`, an
///   off-frame reference, a frame's class owner), so a word a marker reaches
///   only through the row is relocated by the blocked fold exactly as a
///   rooted one; a conservative deposit's extra words may be untagged slots
///   that only a root's pinning protects, so it stays rooted;
/// * a mirror and EVERY word in the old generation. A young-only collection
///   then neither frees nor moves any of them and reads none of them (its
///   old -> young edges come from the cards), so leaving them out changes
///   nothing it does; every phase that marks old space (the majors' old
///   BFS, the true-root trace, the non-moving old sweep) follows a marked
///   owner's `metadata_pin` row, and the old-generation verdict the sweep
///   asks is exact. A young mirror or word keeps the whole entry rooted.
///
/// Every other entry is gathered as before.
fn mirror_keyed_mirror(
    tid: ThreadId,
    entry: &ThreadEntry,
    snapshot: &[ObjectRef],
    arm: &MirrorKeyedArm,
) -> Option<usize> {
    if !arm.candidates.contains(&tid.0)
        || entry.mounted.load(Ordering::Acquire)
        || !entry
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
        || entry
            .gc_block_state
            .deposit_conservative
            .load(Ordering::Acquire)
    {
        return None;
    }
    let mirror = entry.java_thread_obj?.as_ptr() as usize;
    if !arm.is_old(mirror) {
        return None;
    }
    snapshot
        .iter()
        .all(|w| arm.is_old(w.as_ptr() as usize))
        .then_some(mirror)
}

/// gci i14/ve: the `(tid, mirror)` pairs of `deferred` whose mirror the pause
/// did not keep: not moved (no `pointer_map` key) and not `survived` in place.
/// Pure, for its test.
fn mirror_keyed_dead(
    deferred: &[(ThreadId, usize)],
    pointer_map: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) -> Vec<(ThreadId, usize)> {
    deferred
        .iter()
        .copied()
        .filter(|&(_, mirror)| pointer_map.get(&mirror).is_none() && !survived(mirror))
        .collect()
}

/// gci i18/hw (`gci-i15orch-g1-parked-virtual-thread-holder-graph-reads-wrong-on-the-default-path-RETIRED-UNFIXED-20261018`):
/// does a blocking deposit (`NativeContextImpl::deposit_root_snapshot_inner_with`)
/// skip the conservative-locals probes for this thread? Only for a virtual
/// thread's continuation (`ThreadKind::Virtual`) on G1, with
/// `CRATONVM_G1_VT_PRECISE_DEPOSIT` on (the default).
///
/// The probes (`roots::conservative_locals_enabled`: every local of every frame
/// as a root, plus the tag-blind `scan_locals_conservative` /
/// `scan_object_refs_conservative` candidates) run while ANY thread is in
/// compiled code, to catch a lost-tag local a compiled callee left in an
/// interpreted caller. Their contract is "sound ONLY under the non-moving
/// sweep", which is what that condition selects on Generational. On G1 every
/// pause evacuates, and a continuation never runs compiled code
/// (`jit_bridge::assert_no_compiled_continuation`), so it has no such local:
/// the probes add only dead locals' words and tag-blind candidates. A parked
/// continuation's yield deposit is its only root for the whole park, so on a
/// JIT run on G1 those extra words (11 a thread on the holder probe: 34 words
/// against 23 under `--nojit`) were roots of every pause, and the
/// mirror-keyed arm (which needs a precise deposit) keyed none of them.
///
/// Generational keeps the probes (its JIT-active sweep is the non-moving one
/// they were written for, and its unrecorded-root guard reads the flag). Pure,
/// for its test.
pub(crate) fn continuation_deposit_is_precise(
    is_g1: bool,
    is_continuation: bool,
    switch_on: bool,
) -> bool {
    is_g1 && is_continuation && switch_on
}

/// gci i19/vd: the `is_continuation` input of [`continuation_deposit_is_precise`]
/// for `thread`'s own blocking deposit -- a virtual thread's continuation
/// (`ThreadKind::Virtual`) that has never entered compiled code
/// (`GcBlockState::continuation_entered_compiled`, set by
/// `jit_bridge::assert_no_compiled_continuation` in every build).
///
/// Why the second half: the precise deposit drops the tag-blind probes, whose
/// one job is a lost-tag local a compiled callee left in an interpreted frame.
/// A continuation has none only because no compiled code ran on it, and that
/// rule is enforced by per-door `ThreadKind::Virtual` declines and checked
/// only by a `debug_assert!`. With the sticky flag the precise arm covers
/// exactly the continuations that provably ran interpreted-only; one that a
/// door let into compiled code deposits conservatively, as before i18, for
/// the rest of its life. Every other live word of a parked continuation is in
/// the precise set by construction: its frames live in its `JvmThread` (the
/// JDK path's `StackChunk` holds only ints, `jdk_continuation::mark_tail`), the
/// liveness-mapped local scan with its type half is the scan `collect_roots`
/// and `update_root_snapshot` use, the operand stack keeps its tag-ruled scan,
/// monitors / class owners / off-frame roots / JNI locals are unchanged, and a
/// runner OS thread that ran no compiled code has no JIT chain, spill slots or
/// deopt stash to add. See `i19-vd-report.md`.
#[inline]
pub(crate) fn continuation_is_interpreted_only(thread: &crate::threading::JvmThread) -> bool {
    matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && !thread
            .gc_block_state
            .continuation_entered_compiled
            .load(Ordering::Relaxed)
}

/// `CRATONVM_DBG_VT_PARKED_ROOTS`: one root gather's census
/// ([`ThreadRegistry::collect_all_root_snapshots`]).
#[derive(Default, Debug)]
struct ParkedGatherCensus {
    alive: usize,
    parked: usize,
    words: usize,
    parked_words: usize,
}

impl ParkedGatherCensus {
    fn note(&mut self, unmounted: bool, words: usize) {
        self.alive += 1;
        self.words += words;
        if unmounted {
            self.parked += 1;
            self.parked_words += words;
        }
    }

    fn line(&self, roots: usize, elapsed: std::time::Duration) -> String {
        format!(
            "[vt-parked-roots] gather alive={} parked={} words={} parked_words={} roots={} us={}",
            self.alive,
            self.parked,
            self.words,
            self.parked_words,
            roots,
            elapsed.as_micros()
        )
    }
}

/// `CRATONVM_DBG_VT_PARKED_ROOTS`: one blocked fold's census
/// ([`ThreadRegistry::fold_pointer_map_into_blocked_audited`]). `parked` are
/// the blocked, unmounted entries; `parked_words` the words the fold walked
/// for those it did not skip.
#[derive(Default, Debug)]
struct ParkedFoldCensus {
    blocked: usize,
    parked: usize,
    cached: usize,
    skipped: usize,
    stored: usize,
    parked_words: usize,
    map_filter_nanos: u128,
}

impl ParkedFoldCensus {
    fn line(&self, map_len: usize, filter_on: bool, elapsed: std::time::Duration) -> String {
        format!(
            "[vt-parked-roots] fold filter={} map={} blocked={} parked={} cached={} skipped={} \
             stored={} parked_words={} map_filter_us={} us={}",
            if filter_on { "on" } else { "off" },
            map_len,
            self.blocked,
            self.parked,
            self.cached,
            self.skipped,
            self.stored,
            self.parked_words,
            self.map_filter_nanos / 1000,
            elapsed.as_micros()
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// One blocked thread's share of
/// [`ThreadRegistry::fold_pointer_map_into_blocked_audited`]: compose the
/// existing fixup chain through `pointer_map`, seed a chain entry for every
/// snapshot address this map moves for the first time, and remap the snapshot
/// in place. Returns `(composed, seeded)`.
///
/// gc-common w4-a (`handoff-w3g-blocked-fold-costs` edit 1,
/// `common-w2g-per-gc-remap-costs` §1): the set of already-chained addresses
/// (the chain's PRE-compose values) was rebuilt for every blocked thread on
/// every relocating pause -- an allocation and a hash insert per entry of a
/// chain that only grows while the thread stays blocked -- although only a
/// snapshot entry that this map MOVES reads it. It is now built on the first
/// such entry, and the snapshot pass runs BEFORE the compose so the values it
/// is built from are still pre-compose without a copy; the seeds are inserted
/// after the compose, exactly where the old order inserted them, so a seed is
/// never composed through the map that produced it. A thread none of whose
/// snapshot moved pays one probe per snapshot entry and nothing else.
fn fold_one_blocked_chain(
    fixup: &mut cratonvm_types::PointerMap,
    snapshot: &mut [ObjectRef],
    pointer_map: &cratonvm_types::PointerMap,
) -> (usize, usize) {
    // Addresses already chained (frame holds the ORIG key, the snapshot holds
    // the CUR value), PRE-compose. `None` until a snapshot entry moves.
    // addr-keyed: local to this fold (one pause).
    let mut chained: Option<rustc_hash::FxHashSet<usize>> = None;
    let mut seeds: Vec<(usize, usize)> = Vec::new();
    for r in snapshot.iter_mut() {
        let s = r.as_ptr() as usize;
        if let Some(&new) = pointer_map.get(&s) {
            let already = chained
                .get_or_insert_with(|| fixup.values().copied().collect())
                .contains(&s);
            if !already {
                // First move of this object while blocked: the frames hold `s`
                // itself.
                seeds.push((s, new));
            }
            // SAFETY: `new` comes from the GC pointer map and points at the
            // relocated object's header.
            *r = unsafe { ObjectRef::from_raw(new as *mut u8) };
        }
    }
    let mut composed = 0usize;
    for cur in fixup.values_mut() {
        if let Some(&new) = pointer_map.get(cur) {
            *cur = new;
            composed += 1;
        }
    }
    let seeded = seeds.len();
    for (s, new) in seeds {
        // `or_insert` guards the ABA case where `s` was recycled and is also an
        // existing chain key for a DIFFERENT (older) object -- whose value the
        // compose above has already advanced.
        fixup.entry(s).or_insert(new);
    }
    (composed, seeded)
}

/// `CRATONVM_DBG_BLOCKGC`, latched (it was an uncached environment probe once
/// per relocating pause).
fn blockgc_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_BLOCKGC"))
}

/// `CRATONVM_DBG_ROOTFIXUP`, latched: the switch `update_all_roots`' root
/// fix-up census reads.
fn blocked_fold_census_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_ROOTFIXUP"))
}

static BLOCKED_FOLD_CALLS: AtomicU64 = AtomicU64::new(0);
static BLOCKED_FOLD_THREADS: AtomicU64 = AtomicU64::new(0);
static BLOCKED_FOLD_NANOS: AtomicU64 = AtomicU64::new(0);

/// Times one fold into the `blocked_fold` census on drop.
struct BlockedFoldTimer {
    started: std::time::Instant,
}

impl BlockedFoldTimer {
    fn start() -> Self {
        BlockedFoldTimer {
            started: std::time::Instant::now(),
        }
    }

    fn note_thread(&self) {
        BLOCKED_FOLD_THREADS.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for BlockedFoldTimer {
    fn drop(&mut self) {
        BLOCKED_FOLD_CALLS.fetch_add(1, Ordering::Relaxed);
        BLOCKED_FOLD_NANOS.fetch_add(
            u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

/// `(folds, blocked_threads_folded, nanos)` of the in-pause blocked-thread fold
/// under `CRATONVM_DBG_ROOTFIXUP=1` -- the `blocked_fold` share of the root
/// fix-up census (`memory::gc::root_fixup_census`, whose total already
/// contains it). gc-common w4-a; printed at exit by the launcher
/// (`handoff-w4a-print-the-blocked-fold-census`).
pub fn blocked_fold_census() -> (u64, u64, u64) {
    (
        BLOCKED_FOLD_CALLS.load(Ordering::Relaxed),
        BLOCKED_FOLD_THREADS.load(Ordering::Relaxed),
        BLOCKED_FOLD_NANOS.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_thread_id_increments() {
        let registry = ThreadRegistry::new();
        let id1 = registry.next_thread_id();
        let id2 = registry.next_thread_id();
        assert_eq!(id1, ThreadId(1));
        assert_eq!(id2, ThreadId(2));
    }

    /// zt t15/pl6: the JMM roster behind `java.threads.*` lists what
    /// `ThreadMXBean`'s walk lists -- alive entries with a mirror that are not
    /// virtual threads -- at every edge, and the four figures follow the
    /// bean's formulas (an uncounted listed thread adds to `started` and
    /// `peak`; the ended `main` is left out).
    #[test]
    fn zt15pl6_the_jmm_roster_lists_what_the_bean_walk_lists() {
        let registry = ThreadRegistry::new();
        let mut backing = [[0u64; 2]; 5];
        let mut mirror = |i: usize| {
            let ptr = backing[i].as_mut_ptr() as *mut u8;
            // SAFETY: a live, 8-byte-aligned, non-null allocation, never read.
            unsafe { ObjectRef::from_raw(ptr) }
        };
        let (m0, m1, m2, m3, m4) = (mirror(0), mirror(1), mirror(2), mirror(3), mirror(4));
        let counts = |r: &ThreadRegistry| {
            let c = r.jmm_perf_thread_counts();
            (c.live, c.daemon, c.peak, c.started)
        };
        assert_eq!(counts(&registry), (0, 0, 0, 0));
        // main: mirror set after the registration, counted.
        registry.register(ThreadId(0), "main", None);
        registry.note_jmm_thread_start(ThreadId(0));
        // Counted with no mirror yet: the bean's difference is -1, so its
        // `peak` / `started` read the live count until the mirror exists.
        assert_eq!(counts(&registry), (0, 0, 0, 0), "counted, not listed yet");
        registry.set_java_thread_obj(ThreadId(0), m0);
        assert_eq!(counts(&registry), (1, 0, 1, 1));
        // A started daemon platform thread.
        registry.register_starting_with_daemon(ThreadId(1), "d", Some(m1), true);
        registry.note_jmm_thread_start(ThreadId(1));
        assert_eq!(counts(&registry), (2, 1, 2, 2));
        // A virtual thread: never listed.
        registry.register_starting_virtual(ThreadId(2), "v", m2);
        assert_eq!(counts(&registry), (2, 1, 2, 2));
        // A VM-made thread with a mirror the JMM did not count: listed, and
        // added to `started` / `peak` as the bean adds it.
        registry.register_with_daemon(ThreadId(3), "vm-made", None, true);
        registry.set_java_thread_obj(ThreadId(3), m3);
        assert_eq!(counts(&registry), (3, 2, 3, 3));
        // A daemon flag flip of a listed entry.
        assert!(registry.set_daemon(ThreadId(3), false));
        assert_eq!(counts(&registry), (3, 1, 3, 3));
        // A withdrawn mirror unlists; re-set lists again (not twice).
        registry.clear_java_thread_obj_if(ThreadId(3), m3);
        assert_eq!(counts(&registry).0, 2);
        registry.set_java_thread_obj(ThreadId(3), m3);
        registry.set_java_thread_obj(ThreadId(3), m4);
        assert_eq!(counts(&registry).0, 3);
        // Deaths leave once; a second `mark_dead` changes nothing.
        registry.mark_dead(ThreadId(1));
        registry.mark_dead(ThreadId(1));
        registry.mark_dead(ThreadId(2));
        assert_eq!(counts(&registry), (2, 0, 3, 3), "peak and started stay");
        // The ended `main` is left out until it carries `DestroyJavaVM`.
        registry.mark_java_side_ended(ThreadId(0));
        assert_eq!(counts(&registry).0, 1);
        registry.mark_destroy_java_vm_listed();
        assert_eq!(counts(&registry).0, 2);
    }

    /// Fabricated, never-dereferenced heap addresses. Everything the owned-
    /// synchronizer index does with an `ObjectRef` is take `as_ptr()` and use
    /// it as a map key / identity comparison, so these never need to be real
    /// objects — and making them real would mean booting a heap for a test
    /// about bookkeeping.
    fn fake_synchronizer(addr: usize) -> ObjectRef {
        // SAFETY: this ref is only ever compared and hashed by address; no
        // code under test reads through it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// JIT round 19 wave 9, lane gc9 (`CRATONVM_JMX_HELD_NATIVE_FRAME_MONITORS`):
    /// a pause's publish on behalf of a held thread lands at the trace
    /// positions it was given, is read back for that trace length, and sets
    /// the OWNER's published flag (so the owner's next empty publish
    /// withdraws it); a thread with no published `JvmThread` gets nothing.
    #[test]
    fn a_held_threads_frame_monitors_are_published_for_its_stored_trace() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(42);
        registry.register(tid, "gc9-held", None);
        let thread = Box::new(crate::threading::jvm_thread::JvmThread::new(
            tid, "gc9-held",
        ));
        registry.set_jvm_thread_addr(
            tid,
            &*thread as *const crate::threading::jvm_thread::JvmThread as usize,
        );
        let lock_f = fake_synchronizer(0x60_000);
        let lock_g = fake_synchronizer(0x60_040);
        registry.complete_jmx_monitor_enter(tid, lock_f);
        registry.complete_jmx_monitor_enter(tid, lock_g);
        let frame = |method: &str| {
            crate::runtime::frame::Frame::new(
                crate::classloading::ClassId::new(3),
                "probe/T".to_string(),
                method.to_string(),
                "()V".to_string(),
                None,
                vec![0; 16],
                vec![],
                2,
                2,
                &[],
            )
        };
        let mut frames = vec![frame("run"), frame("tCold")];
        frames[1].held_monitors.push(lock_f);
        // Two frames at entries 1 and 2 (a served row below them), the
        // native on top: four entries.
        registry.publish_held_thread_frame_monitors(tid, &frames, &[1, 2], 4, &[]);
        assert!(thread.jmx_frame_monitors_published.load(Ordering::Relaxed));
        assert_eq!(registry.jmx_frame_monitors(tid, 4), Some(vec![(2, lock_f)]));
        // Another length: the publication does not describe it.
        assert_eq!(registry.jmx_frame_monitors(tid, 3), None);
        // No `JvmThread` address: nothing is published.
        let other = ThreadId(43);
        registry.register(other, "gc9-unknown", None);
        registry.complete_jmx_monitor_enter(other, lock_f);
        registry.publish_held_thread_frame_monitors(other, &frames, &[1, 2], 4, &[]);
        assert_eq!(registry.jmx_frame_monitors(other, 4), None);
    }

    /// JIT round 19 wave 11, lane gc11 (GC920-4,
    /// `CRATONVM_JMX_HELD_NATIVE_COMPILED_HOLDS`): a held thread whose block
    /// lock sits in a COMPILED activation (`tCold` at trace index 2, above the
    /// interpreted `run` at 1, the native at 3) gets that hold published at
    /// the activation's index; a candidate word that is no lock-stack object
    /// (index 3) and one past the trace are dropped; the interpreted frame's
    /// own hold still lands at its position.
    #[test]
    fn a_held_threads_compiled_holds_land_at_their_trace_positions() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(44);
        registry.register(tid, "gc11-held", None);
        let thread = Box::new(crate::threading::jvm_thread::JvmThread::new(
            tid,
            "gc11-held",
        ));
        registry.set_jvm_thread_addr(
            tid,
            &*thread as *const crate::threading::jvm_thread::JvmThread as usize,
        );
        let lock_e = fake_synchronizer(0x61_000);
        let lock_f = fake_synchronizer(0x61_040);
        let lock_g = fake_synchronizer(0x61_080);
        registry.complete_jmx_monitor_enter(tid, lock_e);
        registry.complete_jmx_monitor_enter(tid, lock_f);
        registry.complete_jmx_monitor_enter(tid, lock_g);
        let mut frames = vec![crate::runtime::frame::Frame::new(
            crate::classloading::ClassId::new(3),
            "probe/T".to_string(),
            "run".to_string(),
            "()V".to_string(),
            None,
            vec![0; 16],
            vec![],
            2,
            2,
            &[],
        )];
        frames[0].held_monitors.push(lock_e);
        let compiled = [
            (2u32, lock_f.as_ptr() as usize),
            (3, 0x61_0c0),
            (9, lock_g.as_ptr() as usize),
        ];
        registry.publish_held_thread_frame_monitors(tid, &frames, &[1], 4, &compiled);
        assert!(thread.jmx_frame_monitors_published.load(Ordering::Relaxed));
        assert_eq!(
            registry.jmx_frame_monitors(tid, 4),
            Some(vec![(2, lock_f), (1, lock_e)])
        );
        // Off (no candidates): the interpreted frame's hold alone, as in wave 9.
        registry.publish_held_thread_frame_monitors(tid, &frames, &[1], 4, &[]);
        assert_eq!(registry.jmx_frame_monitors(tid, 4), Some(vec![(1, lock_e)]));
    }

    /// JIT round 19 wave 12, lane gc12 (`CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`):
    /// the pause's record keeps the lock stack of the trace's instant; the
    /// attribution resolves against it even after the thread took another
    /// monitor (where the current-stack read refuses), and a record is current
    /// only within one pause end and no moving collection.
    #[test]
    fn a_trace_lock_record_pairs_the_attribution_with_its_own_instant() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(45);
        registry.register(tid, "gc12-running", None);
        let thread = Box::new(crate::threading::jvm_thread::JvmThread::new(
            tid,
            "gc12-running",
        ));
        registry.set_jvm_thread_addr(
            tid,
            &*thread as *const crate::threading::jvm_thread::JvmThread as usize,
        );
        let lock_a = fake_synchronizer(0x62_000);
        let callee = fake_synchronizer(0x62_040);
        registry.complete_jmx_monitor_enter(tid, lock_a);
        let mut frames = vec![crate::runtime::frame::Frame::new(
            crate::classloading::ClassId::new(3),
            "probe/T".to_string(),
            "drive".to_string(),
            "()V".to_string(),
            None,
            vec![0; 16],
            vec![],
            2,
            2,
            &[],
        )];
        frames[0].held_monitors.push(lock_a);
        // The pause: trace of 3 entries, `drive` at entry 1, lock stack [a].
        registry.publish_held_thread_frame_monitors(tid, &frames, &[1], 3, &[]);
        registry.record_trace_locks(tid, 3, 7, 2);
        let record = registry.trace_locks(tid).expect("recorded");
        assert_eq!(record.keys, vec![lock_a.as_ptr() as usize]);
        assert_eq!(record.trace_len, 3);
        // The thread runs on and takes the callee's monitor.
        registry.complete_jmx_monitor_enter(tid, callee);
        assert_eq!(
            registry.jmx_frame_monitors(tid, 3),
            None,
            "the current stack grew"
        );
        let keys = [lock_a];
        assert_eq!(
            registry.jmx_frame_monitors_against(tid, 3, &keys),
            Some(vec![(1, lock_a)])
        );
        assert_eq!(registry.jmx_frame_monitors_against(tid, 4, &keys), None);
        // Current at the publishing pause's own end only, and with no move.
        assert!(record.current_at(7, 2));
        assert!(record.current_at(8, 2));
        assert!(!record.current_at(9, 2));
        assert!(!record.current_at(8, 3));
    }

    /// gcg g6/jn: a conservatively deposited snapshot contributes every word
    /// to the guard's exclusion; a precise one contributes none.
    #[test]
    fn gcg_g6jn_conservative_deposit_words_only_from_conservative_snapshots() {
        let snap: Vec<ObjectRef> = [0x50_000usize, 0x50_040, 0x50_080]
            .iter()
            .map(|&a| fake_synchronizer(a))
            .collect();
        let mut out = vec![0x10];
        conservative_deposit_words(false, &snap, &[], &mut out);
        assert_eq!(out, vec![0x10], "a precise deposit adds nothing");
        conservative_deposit_words(true, &snap, &[], &mut out);
        assert_eq!(out, vec![0x10, 0x50_000, 0x50_040, 0x50_080]);
        conservative_deposit_words(true, &[], &[], &mut out);
        assert_eq!(out.len(), 4, "an empty snapshot adds nothing");
    }

    /// gch h1/yp: a blocked thread's live locals and operand slots stay under
    /// the unrecorded-young-root guard; its dead locals and every word no
    /// slot recorded (the conservative probe's) are excluded.
    #[test]
    fn gch_h1yp_live_slot_words_of_a_conservative_deposit_stay_guarded() {
        use crate::threading::jvm_thread::SlotOrigin;
        let so = |idx: u32, is_stack: bool, live: bool, cur: usize| SlotOrigin {
            frame: 0,
            idx,
            is_stack,
            live,
            orig: cur,
            cur,
        };
        let origins = [
            so(0, false, true, 0x60_080),  // a live local: precise
            so(1, false, false, 0x60_000), // a dead local: conservative
            so(0, true, true, 0x60_040),   // an operand slot: precise
            so(2, false, true, 0x60_080),  // the same value twice
        ];
        let precise = precise_slot_origin_words(&origins);
        assert_eq!(
            precise,
            vec![0x60_040, 0x60_080],
            "sorted, deduplicated, live only"
        );
        let snap: Vec<ObjectRef> = [0x60_000usize, 0x60_040, 0x60_080, 0x60_0c0]
            .iter()
            .map(|&a| fake_synchronizer(a))
            .collect();
        let mut out = Vec::new();
        conservative_deposit_words(true, &snap, &precise, &mut out);
        assert_eq!(
            out,
            vec![0x60_000, 0x60_0c0],
            "the dead local and the unrecorded word are excluded; the live ones are not"
        );
        out.clear();
        conservative_deposit_words(false, &snap, &precise, &mut out);
        assert!(out.is_empty(), "a precise deposit still excludes nothing");
        assert!(precise_slot_origin_words(&[]).is_empty());
    }

    /// gch h3/yc: a PARKED thread's conservative safepoint deposit keeps the
    /// precise words it recorded (`GcBlockState::precise_deposit_words`, in
    /// deposit order, duplicates included) under the guard; only the
    /// conservative probe's extras are excluded. An empty record is the h2
    /// behaviour: the whole snapshot is excluded.
    #[test]
    fn gch_h3yc_a_parked_deposits_precise_words_stay_guarded() {
        let recorded = [0x70_080usize, 0x70_000, 0x70_080];
        let precise = sorted_precise_words(&recorded);
        assert_eq!(precise, vec![0x70_000, 0x70_080], "sorted and deduplicated");
        let snap: Vec<ObjectRef> = [0x70_000usize, 0x70_040, 0x70_080, 0x70_0c0]
            .iter()
            .map(|&a| fake_synchronizer(a))
            .collect();
        let mut out = Vec::new();
        conservative_deposit_words(true, &snap, &precise, &mut out);
        assert_eq!(
            out,
            vec![0x70_040, 0x70_0c0],
            "only the probe's extras are excluded"
        );
        out.clear();
        conservative_deposit_words(true, &snap, &sorted_precise_words(&[]), &mut out);
        assert_eq!(out.len(), 4, "no record: the whole snapshot, as before");
        // The record lives on the thread's block state and starts empty.
        let block = crate::threading::jvm_thread::GcBlockState::new();
        assert!(block.precise_deposit_words.lock().is_empty());
    }

    /// gcg g4/jn: the unblocked-thread `[blockgc]` line counts every snapshot
    /// entry the map moves and lists the first `max` in snapshot order;
    /// unmoved entries and an empty map count nothing.
    #[test]
    fn gcg_g4jn_moved_snapshot_entries_counts_and_bounds() {
        let snapshot: Vec<ObjectRef> = (0..12usize)
            .map(|i| fake_synchronizer(0x40_000 + i * 0x40))
            .collect();
        let mut pm = cratonvm_types::PointerMap::default();
        for i in (0..12usize).filter(|i| i % 3 != 0) {
            pm.insert(0x40_000 + i * 0x40, 0x80_000 + i * 0x40);
        }
        let (moved, first) = moved_snapshot_entries(&snapshot, &pm, 3);
        assert_eq!(moved, 8);
        assert_eq!(first, vec![0x40_040, 0x40_080, 0x40_100]);
        let (none, empty) =
            moved_snapshot_entries(&snapshot, &cratonvm_types::PointerMap::default(), 8);
        assert_eq!((none, empty.len()), (0, 0));
        let (all, capped) = moved_snapshot_entries(&snapshot, &pm, UNBLOCKED_MOVED_REPORT_ADDRS);
        assert_eq!((all, capped.len()), (8, UNBLOCKED_MOVED_REPORT_ADDRS));
    }

    fn owned_synchronizers(registry: &ThreadRegistry, tid: ThreadId) -> Vec<usize> {
        registry
            .jmx_lock_snapshot(tid)
            .map(|snapshot| snapshot.3.iter().map(|o| o.as_ptr() as usize).collect())
            .unwrap_or_default()
    }

    /// [`ThreadRegistry::set_jmx_owned_synchronizer_own`] must record exactly
    /// what the general form records, for **every** (previous owner, new owner)
    /// combination — not only the two AQS actually generates.
    ///
    /// The fast form exists because the general one reads the registry-wide
    /// `RwLock` to find a list the calling thread could have held a handle to.
    /// That is only sound if the two are indistinguishable in their effect, and
    /// the interesting combinations are the ones AQS does *not* produce: a
    /// steal from a peer, and a release of a lock a peer owns. Both take the
    /// fast form's fallback arm, and a divergence there would surface as a
    /// `ThreadInfo.getLockedSynchronizers()` that names two owners for one lock
    /// — silently, and only under JMX inspection.
    ///
    /// Driven as a table so a future arm cannot be added without a case.
    #[test]
    fn the_own_handle_transition_records_what_the_general_one_records() {
        // (label, previous owner, new owner)
        let cases: [(&str, Option<ThreadId>, Option<ThreadId>); 6] = [
            ("first acquire by self", None, Some(ThreadId(1))),
            (
                "reentrant acquire by self",
                Some(ThreadId(1)),
                Some(ThreadId(1)),
            ),
            ("release by self", Some(ThreadId(1)), None),
            ("steal from a peer", Some(ThreadId(2)), Some(ThreadId(1))),
            ("release of a peer's lock", Some(ThreadId(2)), None),
            ("release of an unowned lock", None, None),
        ];
        let (me, peer) = (ThreadId(1), ThreadId(2));
        let lock = fake_synchronizer(0x2000);

        for (label, previous, owner) in cases {
            // Two registries, seeded identically, then driven one call apart.
            let general = ThreadRegistry::new();
            let own = ThreadRegistry::new();
            for registry in [&general, &own] {
                registry.register(me, "me", None);
                registry.register(peer, "peer", None);
                if let Some(previous) = previous {
                    registry.set_jmx_owned_synchronizer(Some(previous), lock);
                }
            }

            general.set_jmx_owned_synchronizer(owner, lock);
            let own_list = own
                .jmx_locked_synchronizers_of(me)
                .expect("the handle must exist for a registered thread");
            own.set_jmx_owned_synchronizer_own(me, &own_list, owner, lock);

            for tid in [me, peer] {
                assert_eq!(
                    owned_synchronizers(&general, tid),
                    owned_synchronizers(&own, tid),
                    "{label}: the two forms disagree about what {tid:?} owns"
                );
            }
        }
    }

    /// The handle must be the *same* list the JMX snapshot reads.
    ///
    /// This is the one way the fast form can be wrong without any of its own
    /// logic being wrong: hand it a list that is not the registry's, and every
    /// write lands somewhere `jmx_lock_snapshot` will never look. The `Arc` is
    /// what makes them one list, and nothing else in the type system says so.
    #[test]
    fn the_handle_and_the_snapshot_are_the_same_list() {
        let registry = ThreadRegistry::new();
        let me = ThreadId(1);
        registry.register(me, "me", None);
        let handle = registry
            .jmx_locked_synchronizers_of(me)
            .expect("registered");
        handle.lock().push(fake_synchronizer(0x3000));
        assert_eq!(
            owned_synchronizers(&registry, me),
            vec![0x3000],
            "a write through the handle was invisible to jmx_lock_snapshot — \
             the handle is not the registry's list"
        );
    }

    /// An unregistered thread has no handle, which is what sends its caller to
    /// the general form rather than to a private list nobody reads.
    #[test]
    fn an_unregistered_thread_has_no_synchronizer_handle() {
        let registry = ThreadRegistry::new();
        assert!(registry.jmx_locked_synchronizers_of(ThreadId(77)).is_none());
    }

    fn addrs(objs: &[ObjectRef]) -> Vec<usize> {
        objs.iter().map(|o| o.as_ptr() as usize).collect()
    }

    /// The lock stack must report exactly what the `Mutex<Vec<_>>` it replaced
    /// reported — same members, same order — through publish (dedupe), retract
    /// (every copy), overflow past the slots, holes, and the drain back.
    ///
    /// Driven against a model of the old list over a pseudo-random sequence
    /// biased toward LIFO releases (what structured locking produces) but with
    /// out-of-order ones too (JNI can release in any order). With twenty
    /// addresses the held set hovers around seven, so the sequence crosses the
    /// eight-slot boundary in both directions many times.
    #[test]
    fn the_lock_stack_reports_exactly_what_the_old_list_reported() {
        let stack = JmxLockStack::default();
        let mut model: Vec<usize> = Vec::new();
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for step in 0..20_000u32 {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let a = 0x10_000usize + ((x >> 33) % 20) as usize * 0x40;
            match (x >> 60) & 3 {
                0 | 1 => {
                    stack.publish(fake_synchronizer(a));
                    if !model.contains(&a) {
                        model.push(a);
                    }
                }
                2 => {
                    if let Some(&last) = model.last() {
                        stack.retract(fake_synchronizer(last));
                        model.retain(|&m| m != last);
                    }
                }
                _ => {
                    stack.retract(fake_synchronizer(a));
                    model.retain(|&m| m != a);
                }
            }
            assert_eq!(
                addrs(&stack.snapshot()),
                model,
                "step {step}: the snapshot diverged from the list it replaced"
            );
            let mut roots = Vec::new();
            stack.extend_roots_stw(&mut roots);
            assert_eq!(addrs(&roots), model, "step {step}: the GC roots diverged");
        }
    }

    /// A moving collection must forward entries in the slots AND the overflow,
    /// and a later retract by the relocated address (which is what the owner's
    /// remapped frames hold) must find them.
    #[test]
    fn the_lock_stack_remaps_slots_and_overflow() {
        let stack = JmxLockStack::default();
        let old: Vec<usize> = (0..JMX_LOCK_STACK_SLOTS + 3)
            .map(|i| 0x20_000 + i * 0x40)
            .collect();
        for &a in &old {
            stack.publish(fake_synchronizer(a));
        }
        let mut pm = cratonvm_types::PointerMap::default();
        let mut expected = Vec::new();
        for (i, &a) in old.iter().enumerate() {
            if i % 2 == 0 {
                let moved = 0x90_000 + i * 0x40;
                pm.insert(a, moved);
                expected.push(moved);
            } else {
                expected.push(a);
            }
        }
        stack.remap_stw(&pm);
        assert_eq!(addrs(&stack.snapshot()), expected);
        for &a in expected.iter().rev() {
            stack.retract(fake_synchronizer(a));
        }
        assert!(stack.snapshot().is_empty());
        let top = stack.top.load(Ordering::Relaxed);
        assert_eq!(top, 0, "LIFO release leaves no holes");
        assert!(
            !stack.spilled.load(Ordering::Relaxed),
            "a drained overflow must send the next publish back to the slots"
        );
    }

    /// A cross-thread `getLockedMonitors()` reads while the owner writes. It
    /// must only ever see a set the owner actually held: here `{}`, `{A}` or
    /// `{A, B}` — never `{B}`, which an unvalidated slot walk can assemble from
    /// a stale `top` and slots rewritten under it.
    #[test]
    fn a_cross_thread_snapshot_never_sees_a_set_the_owner_did_not_hold() {
        let stack = Arc::new(JmxLockStack::default());
        let (a, b) = (0x30_000usize, 0x30_040usize);
        let done = Arc::new(AtomicBool::new(false));
        let owner = {
            let stack = Arc::clone(&stack);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                for _ in 0..50_000 {
                    stack.publish(fake_synchronizer(a));
                    stack.publish(fake_synchronizer(b));
                    stack.retract(fake_synchronizer(b));
                    stack.retract(fake_synchronizer(a));
                }
                done.store(true, Ordering::Release);
            })
        };
        while !done.load(Ordering::Acquire) {
            let got = addrs(&stack.snapshot());
            assert!(
                got.is_empty() || got == [a] || got == [a, b],
                "torn snapshot {got:x?}"
            );
        }
        owner.join().expect("owner thread panicked");
    }

    /// The registry front door: a re-entrant publish is one entry, the entry is
    /// a GC root while held, and the retract removes both.
    #[test]
    fn owned_monitors_publish_once_root_while_held_and_retract() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "owner", None);
        let m = fake_synchronizer(0x40_000);
        registry.complete_jmx_monitor_enter(tid, m);
        registry.complete_jmx_monitor_enter(tid, m);
        let held = registry.jmx_lock_snapshot(tid).expect("registered").2;
        assert_eq!(addrs(&held), vec![0x40_000]);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![0x40_000]);
        registry.remove_jmx_locked_monitor(tid, m);
        let held = registry.jmx_lock_snapshot(tid).expect("registered").2;
        assert!(held.is_empty());
        assert!(registry.collect_all_root_snapshots().is_empty());
    }

    /// An AQS ownership transition must leave the synchronizer recorded against
    /// exactly one thread — the property the old Θ(threads) scan bought by
    /// walking every thread and taking every thread's mutex, twice per
    /// uncontended `ReentrantLock.lock()`/`unlock()` pair.
    ///
    /// The reverse index replaces the scan; this pins that it still answers the
    /// same. `probes/AqsOwnerScaleProbe.java` is the companion measurement —
    /// with the scan in place, an uncontended lock got ~2x more expensive going
    /// from 1 to 256 live threads while a `synchronized` control stayed flat.
    #[test]
    fn an_ownership_transition_records_the_synchronizer_against_exactly_one_thread() {
        let registry = ThreadRegistry::new();
        let (a, b, c) = (ThreadId(1), ThreadId(2), ThreadId(3));
        for (tid, name) in [(a, "a"), (b, "b"), (c, "c")] {
            registry.register(tid, name, None);
        }
        let lock = fake_synchronizer(0x1000);

        // acquire on A
        registry.set_jmx_owned_synchronizer(Some(a), lock);
        assert_eq!(owned_synchronizers(&registry, a), vec![0x1000]);
        assert!(owned_synchronizers(&registry, b).is_empty());

        // A reentrant acquire re-runs the setter; it must not double-record.
        registry.set_jmx_owned_synchronizer(Some(a), lock);
        assert_eq!(owned_synchronizers(&registry, a), vec![0x1000]);

        // Hand-off to B without an intervening release: A must lose it.
        registry.set_jmx_owned_synchronizer(Some(b), lock);
        assert!(
            owned_synchronizers(&registry, a).is_empty(),
            "the former owner must not keep a lock it no longer holds"
        );
        assert_eq!(owned_synchronizers(&registry, b), vec![0x1000]);

        // release
        registry.set_jmx_owned_synchronizer(None, lock);
        for tid in [a, b, c] {
            assert!(
                owned_synchronizers(&registry, tid).is_empty(),
                "a released synchronizer is owned by nobody"
            );
        }

        // A second, distinct lock is independent of the first.
        let other = fake_synchronizer(0x2000);
        registry.set_jmx_owned_synchronizer(Some(c), lock);
        registry.set_jmx_owned_synchronizer(Some(c), other);
        let mut held = owned_synchronizers(&registry, c);
        held.sort_unstable();
        assert_eq!(held, vec![0x1000, 0x2000]);
        registry.set_jmx_owned_synchronizer(None, lock);
        assert_eq!(owned_synchronizers(&registry, c), vec![0x2000]);
    }

    /// The index is keyed by a heap address, so a moving collection that
    /// relocates a held synchronizer has to rekey it in the same pass that
    /// remaps the per-thread lists. Without that, the next transition for the
    /// lock misses its previous owner and records it against two threads.
    #[test]
    fn a_moving_collection_rekeys_the_owned_synchronizer_index() {
        let registry = ThreadRegistry::new();
        let (a, b) = (ThreadId(1), ThreadId(2));
        registry.register(a, "a", None);
        registry.register(b, "b", None);

        registry.set_jmx_owned_synchronizer(Some(a), fake_synchronizer(0x1000));
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(0x1000usize, 0x9000usize);
        registry.update_thread_objs_after_gc(&pointer_map);

        assert_eq!(
            owned_synchronizers(&registry, a),
            vec![0x9000],
            "the list itself is remapped"
        );

        // Now hand the (relocated) lock to B. If the index still held the
        // vacated key, A would keep its stale entry and both threads would
        // report owning it.
        registry.set_jmx_owned_synchronizer(Some(b), fake_synchronizer(0x9000));
        assert!(
            owned_synchronizers(&registry, a).is_empty(),
            "the index followed the object, so the former owner was found"
        );
        assert_eq!(owned_synchronizers(&registry, b), vec![0x9000]);
    }

    /// gcg g2/k: a sliding compaction moves one object onto another's OLD
    /// address. Rekeying pair by pair in that order overwrote the second row
    /// and then carried the first one on; two phases keep both.
    #[test]
    fn gcg_k2_rekey_in_two_phases_survives_a_slide_onto_a_vacated_key() {
        // addr-keyed: a test-local map.
        let mut map: rustc_hash::FxHashMap<usize, &str> = rustc_hash::FxHashMap::default();
        map.insert(0x1000, "a");
        map.insert(0x2000, "b");
        map.insert(0x5000, "unmoved");
        // `a` slides onto `b`'s old address, `b` onto a fresh one -- in the
        // order that broke the one-pair-at-a-time rekey.
        rekey_in_two_phases(&mut map, &[(0x1000, 0x2000), (0x2000, 0x3000)]);
        assert_eq!(map.get(&0x2000), Some(&"a"));
        assert_eq!(map.get(&0x3000), Some(&"b"));
        assert_eq!(map.get(&0x5000), Some(&"unmoved"));
        assert_eq!(map.get(&0x1000), None);
        assert_eq!(map.len(), 3);
        // A pair whose old key has no row moves nothing.
        rekey_in_two_phases(&mut map, &[(0x7000, 0x8000)]);
        assert_eq!(map.len(), 3);
    }

    /// gcg g2/k: the same slide through the owned-synchronizer index: after
    /// the collection each synchronizer is still recorded against its owner,
    /// so a release of each finds it.
    #[test]
    fn gcg_k2_a_sliding_collection_keeps_every_owned_synchronizer_row() {
        let registry = ThreadRegistry::new();
        let (a, b) = (ThreadId(1), ThreadId(2));
        registry.register(a, "a", None);
        registry.register(b, "b", None);
        registry.set_jmx_owned_synchronizer(Some(a), fake_synchronizer(0x1000));
        registry.set_jmx_owned_synchronizer(Some(b), fake_synchronizer(0x2000));
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(0x1000usize, 0x2000usize);
        pointer_map.insert(0x2000usize, 0x3000usize);
        registry.update_thread_objs_after_gc(&pointer_map);
        assert_eq!(owned_synchronizers(&registry, a), vec![0x2000]);
        assert_eq!(owned_synchronizers(&registry, b), vec![0x3000]);
        registry.set_jmx_owned_synchronizer(None, fake_synchronizer(0x2000));
        registry.set_jmx_owned_synchronizer(None, fake_synchronizer(0x3000));
        assert!(
            owned_synchronizers(&registry, a).is_empty(),
            "a's row followed its lock"
        );
        assert!(
            owned_synchronizers(&registry, b).is_empty(),
            "b's row was not overwritten"
        );
    }

    /// gcg g3/k (`common-a-proposal-per-thread-handshakes`, step 1): the
    /// three classifications, the owner-side run, the requester's withdrawal
    /// and the abandonment of a dropped owner's queue.
    #[test]
    fn gcg_k3_a_handshake_is_queued_run_withdrawn_or_handed_back() {
        use crate::threading::jvm_thread::{HandshakeState, JvmThread};
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let registry = ThreadRegistry::new();
        // gci i23/hs: an op gets the VM its target polls for.
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let tid = ThreadId(31);
        let ran = Arc::new(AtomicUsize::new(0));
        let op = |ran: &Arc<AtomicUsize>| -> crate::threading::jvm_thread::HandshakeOp {
            let ran = Arc::clone(ran);
            Box::new(move |_: &crate::vm::SharedVm, t: &mut JvmThread| {
                assert_eq!(
                    t.thread_id,
                    ThreadId(31),
                    "runs on the target's own thread state"
                );
                ran.fetch_add(1, Ordering::SeqCst);
            })
        };

        // Unknown thread: handed back.
        assert!(matches!(
            registry.request_handshake(tid, op(&ran)),
            HandshakeRequest::Gone(_)
        ));

        registry.register(tid, "hs", None);
        let mut thread = JvmThread::new(tid, "hs");
        registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));

        // Running target: queued, run by the owner, exactly once.
        let HandshakeRequest::Queued { state, ticket } = registry.request_handshake(tid, op(&ran))
        else {
            panic!("a running target queues the op");
        };
        assert!(Arc::ptr_eq(&state, &thread.gc_block_state));
        assert!(thread.gc_block_state.handshakes.is_pending());
        assert_eq!(ticket.state(), HandshakeState::Pending);
        thread.run_pending_handshakes(&shared);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(
            ticket.wait_timeout(Duration::from_secs(5)),
            HandshakeState::Ran
        );
        assert!(!thread.gc_block_state.handshakes.is_pending());
        assert!(state.handshakes.withdraw(&ticket).is_none(), "already run");
        thread.run_pending_handshakes(&shared);
        assert_eq!(ran.load(Ordering::SeqCst), 1, "never twice");

        // Withdrawn before the owner polls: never runs.
        let HandshakeRequest::Queued { state, ticket } = registry.request_handshake(tid, op(&ran))
        else {
            panic!("queued");
        };
        assert!(state.handshakes.withdraw(&ticket).is_some());
        assert_eq!(ticket.state(), HandshakeState::Withdrawn);
        assert!(!thread.gc_block_state.handshakes.is_pending());
        thread.run_pending_handshakes(&shared);
        assert_eq!(ran.load(Ordering::SeqCst), 1);

        // Blocked target: handed back for the requester's blocked path.
        thread
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        assert!(matches!(
            registry.request_handshake(tid, op(&ran)),
            HandshakeRequest::TargetBlocked(_)
        ));
        thread
            .gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);

        // Owner dropped with an op queued: abandoned, never run.
        let HandshakeRequest::Queued { ticket, .. } = registry.request_handshake(tid, op(&ran))
        else {
            panic!("queued");
        };
        drop(thread);
        assert_eq!(
            ticket.wait_timeout(Duration::from_secs(5)),
            HandshakeState::Abandoned
        );
        assert!(matches!(
            registry.request_handshake(tid, op(&ran)),
            HandshakeRequest::Gone(_)
        ));
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// gcg g3/k: a requester on another thread waits on the ticket while the
    /// target runs the op at its poll.
    #[test]
    fn gcg_k3_a_queued_handshake_wakes_its_waiting_requester() {
        use crate::threading::jvm_thread::{HandshakeState, JvmThread};
        use std::time::Duration;
        let registry = Arc::new(ThreadRegistry::new());
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let tid = ThreadId(32);
        registry.register(tid, "hs2", None);
        let mut thread = JvmThread::new(tid, "hs2");
        registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
        let HandshakeRequest::Queued { ticket, .. } = registry.request_handshake(
            tid,
            Box::new(|_: &crate::vm::SharedVm, t: &mut JvmThread| t.name.push('!')),
        ) else {
            panic!("queued");
        };
        let waiter = {
            let ticket = Arc::clone(&ticket);
            std::thread::spawn(move || ticket.wait_timeout(Duration::from_secs(30)))
        };
        std::thread::sleep(Duration::from_millis(5));
        thread.run_pending_handshakes(&shared);
        assert_eq!(waiter.join().unwrap(), HandshakeState::Ran);
        assert_eq!(thread.name, "hs2!");
    }

    /// gci i23/hs: an op that counts its runs into `ran`.
    fn i23hs_counting_op(
        ran: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> crate::threading::jvm_thread::HandshakeOp {
        let ran = Arc::clone(ran);
        Box::new(
            move |_: &crate::vm::SharedVm, _: &mut crate::threading::jvm_thread::JvmThread| {
                ran.fetch_add(1, Ordering::SeqCst);
            },
        )
    }

    /// gci i23/hs (step 2): `handshake_and_wait` returns `Ran` once a polling
    /// target runs the op, exactly once, with the requester on another thread.
    #[test]
    fn gci_i23hs_handshake_and_wait_runs_the_op_at_the_targets_poll() {
        use crate::threading::jvm_thread::JvmThread;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let registry = Arc::new(ThreadRegistry::new());
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let tid = ThreadId(33);
        registry.register(tid, "hs3", None);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        // The target owns its `JvmThread` on its own OS thread and polls.
        let target = {
            let registry = Arc::clone(&registry);
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut thread = JvmThread::new(tid, "hs3");
                registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
                ready_tx.send(Arc::clone(&thread.gc_block_state)).unwrap();
                while !stop.load(Ordering::Acquire) {
                    thread.run_pending_handshakes(&shared);
                    std::thread::yield_now();
                }
            })
        };
        let state = ready_rx.recv().unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let outcome = registry.handshake_and_wait(
            tid,
            i23hs_counting_op(&ran),
            Duration::from_secs(30),
            Duration::from_micros(100),
            || false,
        );
        stop.store(true, Ordering::Release);
        target.join().unwrap();
        assert_eq!(outcome, HandshakeOutcome::Ran);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(!state.handshakes.is_pending());
    }

    /// gcj j1/cm (P4/ph): the kicked wait kicks exactly once when it queued
    /// the op (before the target serves it), and never when it queued
    /// nothing (an unknown or a blocked target).
    #[test]
    fn gcj1cm_the_kicked_wait_kicks_once_and_only_when_it_queued() {
        use crate::threading::jvm_thread::JvmThread;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let registry = ThreadRegistry::new();
        let ran = Arc::new(AtomicUsize::new(0));
        let kicks = AtomicUsize::new(0);
        let slice = Duration::from_micros(200);
        let kick = || {
            kicks.fetch_add(1, Ordering::SeqCst);
        };
        assert_eq!(
            registry.handshake_and_wait_kicked(
                ThreadId(39),
                i23hs_counting_op(&ran),
                Duration::from_millis(2),
                slice,
                || false,
                kick
            ),
            HandshakeOutcome::Gone
        );
        assert_eq!(kicks.load(Ordering::SeqCst), 0, "nothing queued, no kick");
        let tid = ThreadId(39);
        registry.register(tid, "cm1", None);
        let thread = JvmThread::new(tid, "cm1");
        registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
        thread
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        assert_eq!(
            registry.handshake_and_wait_kicked(
                tid,
                i23hs_counting_op(&ran),
                Duration::from_millis(2),
                slice,
                || false,
                kick
            ),
            HandshakeOutcome::TargetBlocked
        );
        assert_eq!(kicks.load(Ordering::SeqCst), 0, "blocked, no kick");
        thread
            .gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
        let state = Arc::clone(&thread.gc_block_state);
        // The kick sees the op already queued (it runs after the push).
        let saw_pending = AtomicBool::new(false);
        assert_eq!(
            registry.handshake_and_wait_kicked(
                tid,
                i23hs_counting_op(&ran),
                Duration::from_millis(2),
                slice,
                || false,
                || {
                    kicks.fetch_add(1, Ordering::SeqCst);
                    saw_pending.store(state.handshakes.is_pending(), Ordering::SeqCst);
                }
            ),
            HandshakeOutcome::NotServed
        );
        assert_eq!(kicks.load(Ordering::SeqCst), 1, "queued: one kick");
        assert!(saw_pending.load(Ordering::SeqCst));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "never polled, withdrawn");
    }

    /// gci i23/hs: a target that never polls is withdrawn at the budget
    /// (`NotServed`) and then never runs the op; one that is blocked from the
    /// start, or unknown, is answered without queueing anything.
    #[test]
    fn gci_i23hs_handshake_and_wait_withdraws_an_unserved_op() {
        use crate::threading::jvm_thread::JvmThread;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let registry = ThreadRegistry::new();
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let ran = Arc::new(AtomicUsize::new(0));
        let slice = Duration::from_micros(200);
        assert_eq!(
            registry.handshake_and_wait(
                ThreadId(34),
                i23hs_counting_op(&ran),
                Duration::from_millis(5),
                slice,
                || false
            ),
            HandshakeOutcome::Gone
        );
        let tid = ThreadId(34);
        registry.register(tid, "hs4", None);
        let mut thread = JvmThread::new(tid, "hs4");
        registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));

        let t0 = Instant::now();
        assert_eq!(
            registry.handshake_and_wait(
                tid,
                i23hs_counting_op(&ran),
                Duration::from_millis(5),
                slice,
                || false
            ),
            HandshakeOutcome::NotServed
        );
        assert!(t0.elapsed() >= Duration::from_millis(5), "waited its budget");
        assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn");
        thread.run_pending_handshakes(&shared);
        assert_eq!(ran.load(Ordering::SeqCst), 0, "a withdrawn op never runs");

        thread
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        assert_eq!(
            registry.handshake_and_wait(
                tid,
                i23hs_counting_op(&ran),
                Duration::from_secs(30),
                slice,
                || false
            ),
            HandshakeOutcome::TargetBlocked
        );
        assert!(!thread.gc_block_state.handshakes.is_pending(), "never queued");
        thread
            .gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
        thread.run_pending_handshakes(&shared);
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    /// t1 lane E wave 3 (`t1-V2-handshake-poll-bit-stale-carrier-arm-skips-the-kick-fallback`):
    /// the second-chance kick runs exactly once, after the first unserved
    /// slice, with the state the op was queued on -- and never for a target
    /// that settled before the first slice ended.
    #[test]
    fn t1e_the_second_chance_kick_runs_once_after_an_unserved_first_slice() {
        use crate::threading::jvm_thread::JvmThread;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let registry = ThreadRegistry::new();
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let ran = Arc::new(AtomicUsize::new(0));
        let slice = Duration::from_micros(200);
        let tid = ThreadId(31_901);
        registry.register(tid, "t1e-2nd", None);
        let mut thread = JvmThread::new(tid, "t1e-2nd");
        registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
        let expected = Arc::as_ptr(&thread.gc_block_state) as usize;

        // Unserved: kick once, second chance once, then the budget.
        let kicks = AtomicUsize::new(0);
        let rekicks = AtomicUsize::new(0);
        let rekicked_on = AtomicUsize::new(0);
        assert_eq!(
            registry.handshake_and_wait_kicking_twice(
                tid,
                i23hs_counting_op(&ran),
                // 100 ms, not 5: under a loaded host one yield_now slice can outlast a
                // 5 ms budget, so the deadline passed before the second chance.
                Duration::from_millis(100),
                slice,
                || false,
                |_| {
                    kicks.fetch_add(1, Ordering::SeqCst);
                },
                |state| {
                    rekicks.fetch_add(1, Ordering::SeqCst);
                    rekicked_on.store(state as *const GcBlockState as usize, Ordering::SeqCst);
                },
            ),
            HandshakeOutcome::NotServed
        );
        assert_eq!(kicks.load(Ordering::SeqCst), 1);
        assert_eq!(rekicks.load(Ordering::SeqCst), 1, "exactly one second chance");
        assert_eq!(rekicked_on.load(Ordering::SeqCst), expected, "the queued-on state");
        assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn");

        // Served inside the first kick (the target polled at once): no second chance.
        let rekicks = AtomicUsize::new(0);
        let target = &mut thread;
        assert_eq!(
            registry.handshake_and_wait_kicking_twice(
                tid,
                i23hs_counting_op(&ran),
                Duration::from_secs(30),
                slice,
                || false,
                |_| target.run_pending_handshakes(&shared),
                |_| {
                    rekicks.fetch_add(1, Ordering::SeqCst);
                },
            ),
            HandshakeOutcome::Ran
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(rekicks.load(Ordering::SeqCst), 0, "a prompt target is never re-kicked");
    }

    /// t1 lane E wave 3: [`ThreadRegistry::mounted_os_tid`] answers only a live,
    /// MOUNTED entry's published tid -- never an unmounted continuation's kept
    /// last-carrier tid, a dead entry's, or an unpublished `0`.
    #[test]
    fn t1e_mounted_os_tid_answers_only_a_live_mounted_published_tid() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(31_902);
        assert_eq!(registry.mounted_os_tid(tid), None, "unknown");
        registry.register(tid, "t1e-tid", None);
        let entry_do = |f: &dyn Fn(&ThreadEntry)| {
            let threads = registry.threads.read();
            f(threads.get(&tid).unwrap());
        };
        entry_do(&|e| e.os_tid.store(0, Ordering::Release));
        assert_eq!(registry.mounted_os_tid(tid), None, "unpublished");
        entry_do(&|e| e.os_tid.store(4242, Ordering::Release));
        assert_eq!(registry.mounted_os_tid(tid), Some(4242));
        entry_do(&|e| e.mounted.store(false, Ordering::Release));
        assert_eq!(registry.mounted_os_tid(tid), None, "unmounted keeps its last carrier's tid");
        entry_do(&|e| e.mounted.store(true, Ordering::Release));
        entry_do(&|e| e.alive.store(false, Ordering::Release));
        assert_eq!(registry.mounted_os_tid(tid), None, "dead");
    }

    /// t1 lane E2 (`CRATONVM_GC_HANDSHAKE_WAIT_SEES_BLOCK`): a target that
    /// enters a blocked region after the op was queued ends the spin slice at
    /// once (`TargetBlocked`, the op withdrawn and never run); with the switch
    /// off the same wait lasts out its slice. A drop ends it as `Gone`.
    #[test]
    fn t1e2_the_spin_slice_ends_when_the_target_blocks_after_the_queue() {
        use crate::threading::jvm_thread::JvmThread;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let ran = Arc::new(AtomicUsize::new(0));
        for sees_block in [true, false] {
            let mut registry = ThreadRegistry::new();
            registry.handshake_spin_wait = true;
            registry.handshake_wait_sees_block = sees_block;
            let tid = ThreadId(31_903);
            registry.register(tid, "t1e2-block", None);
            let mut thread = JvmThread::new(tid, "t1e2-block");
            registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
            let state = Arc::clone(&thread.gc_block_state);
            // Long slice with the switch on (it must not be waited out);
            // short with it off (it is).
            let slice = if sees_block {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(60)
            };
            let t0 = Instant::now();
            let outcome = std::thread::scope(|s| {
                s.spawn(|| {
                    std::thread::sleep(Duration::from_millis(2));
                    state.in_blocked_region.store(true, Ordering::Release);
                });
                registry.handshake_and_wait_kicking_twice(
                    tid,
                    i23hs_counting_op(&ran),
                    Duration::from_secs(10),
                    slice,
                    || false,
                    |_| {},
                    |_| {},
                )
            });
            let took = t0.elapsed();
            assert_eq!(outcome, HandshakeOutcome::TargetBlocked, "sees_block={sees_block}");
            if sees_block {
                assert!(took < Duration::from_millis(2500), "ended at the block: {took:?}");
            } else {
                assert!(took >= slice, "lasted out its slice: {took:?}");
            }
            assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn");
            thread
                .gc_block_state
                .in_blocked_region
                .store(false, Ordering::Release);
            thread.run_pending_handshakes(&shared);
            assert_eq!(ran.load(Ordering::SeqCst), 0, "a withdrawn op never runs");
        }
    }

    /// t1 wave 8 lane HANDSHK (`CRATONVM_GC_HANDSHAKE_UNMOUNTED_DEFERS_KICK`):
    /// a target UNMOUNTED at the push is not kicked at once. (a) It deposits
    /// (blocks) within the first slice: `TargetBlocked`, no kick at all. (b) It
    /// stays unmounted and running: the kick runs once after the first slice,
    /// the second chance (`rekick`) after the next. (c) A MOUNTED target, or the
    /// switch off, is kicked at the push as before.
    #[test]
    fn t1w8_handshk_an_unmounted_target_is_kicked_only_after_its_first_slice() {
        use crate::threading::jvm_thread::JvmThread;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let ran = Arc::new(AtomicUsize::new(0));
        let setup = |defers: bool, mounted: bool| {
            let mut registry = ThreadRegistry::new();
            registry.handshake_spin_wait = true;
            registry.handshake_wait_sees_block = false;
            registry.handshake_unmounted_defers_kick = defers;
            let tid = ThreadId(31_904);
            registry.register(tid, "t1w8-handshk", None);
            let thread = JvmThread::new(tid, "t1w8-handshk");
            registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
            registry.set_mounted(tid, mounted);
            (registry, tid, thread)
        };
        // (a) blocks inside the first slice: never kicked.
        {
            let (registry, tid, thread) = setup(true, false);
            let state = Arc::clone(&thread.gc_block_state);
            let kicks = AtomicUsize::new(0);
            let rekicks = AtomicUsize::new(0);
            let outcome = std::thread::scope(|s| {
                s.spawn(|| {
                    std::thread::sleep(Duration::from_millis(2));
                    state.in_blocked_region.store(true, Ordering::Release);
                });
                registry.handshake_and_wait_kicking_twice(
                    tid,
                    i23hs_counting_op(&ran),
                    Duration::from_secs(10),
                    Duration::from_millis(200),
                    || false,
                    |_| {
                        kicks.fetch_add(1, Ordering::SeqCst);
                    },
                    |_| {
                        rekicks.fetch_add(1, Ordering::SeqCst);
                    },
                )
            });
            assert_eq!(outcome, HandshakeOutcome::TargetBlocked);
            assert_eq!(kicks.load(Ordering::SeqCst), 0, "a deposit within the slice: no kick");
            assert_eq!(rekicks.load(Ordering::SeqCst), 0);
            assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn");
        }
        // (b) stays unmounted and running: kick after slice 1, rekick after slice 2.
        {
            let (registry, tid, thread) = setup(true, false);
            let kicks = AtomicUsize::new(0);
            let rekicks = AtomicUsize::new(0);
            let slices = AtomicUsize::new(0);
            let outcome = registry.handshake_and_wait_kicking_twice(
                tid,
                i23hs_counting_op(&ran),
                Duration::from_millis(80),
                Duration::from_millis(20),
                || {
                    slices.fetch_add(1, Ordering::SeqCst);
                    false
                },
                |_| {
                    assert!(
                        slices.load(Ordering::SeqCst) >= 1,
                        "the deferred kick runs after the first slice"
                    );
                    kicks.fetch_add(1, Ordering::SeqCst);
                },
                |_| {
                    assert_eq!(kicks.load(Ordering::SeqCst), 1, "the second chance comes after");
                    rekicks.fetch_add(1, Ordering::SeqCst);
                },
            );
            assert_eq!(outcome, HandshakeOutcome::NotServed);
            assert_eq!(kicks.load(Ordering::SeqCst), 1);
            assert_eq!(rekicks.load(Ordering::SeqCst), 1);
            assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn at the budget");
        }
        // (c) mounted, or the switch off: kicked at the push.
        for (defers, mounted) in [(true, true), (false, false)] {
            let (registry, tid, thread) = setup(defers, mounted);
            let state = Arc::clone(&thread.gc_block_state);
            let kicks = AtomicUsize::new(0);
            let outcome = std::thread::scope(|s| {
                s.spawn(|| {
                    std::thread::sleep(Duration::from_millis(2));
                    state.in_blocked_region.store(true, Ordering::Release);
                });
                registry.handshake_and_wait_kicking_twice(
                    tid,
                    i23hs_counting_op(&ran),
                    Duration::from_secs(10),
                    Duration::from_millis(200),
                    || false,
                    |_| {
                        kicks.fetch_add(1, Ordering::SeqCst);
                    },
                    |_| {},
                )
            });
            assert_eq!(
                outcome,
                HandshakeOutcome::TargetBlocked,
                "defers={defers} mounted={mounted}"
            );
            assert_eq!(kicks.load(Ordering::SeqCst), 1, "defers={defers} mounted={mounted}");
        }
        assert_eq!(ran.load(Ordering::SeqCst), 0, "no op ran: every one was withdrawn");
    }

    /// gci i23/hs: the wait stops early -- withdrawing the op -- when `abort`
    /// says so (a pause was requested: `NotServed` within a slice or two of
    /// it, not at the budget), when the target enters a blocked region
    /// (`TargetBlocked`), and when the target is dropped (`Gone`).
    #[test]
    fn gci_i23hs_handshake_and_wait_stops_early_for_a_pause_a_block_or_a_drop() {
        use crate::threading::jvm_thread::JvmThread;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;
        let registry = ThreadRegistry::new();
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let ran = Arc::new(AtomicUsize::new(0));
        let slice = Duration::from_micros(200);
        let long = Duration::from_secs(30);
        let tid = ThreadId(35);
        registry.register(tid, "hs5", None);
        let mut thread = JvmThread::new(tid, "hs5");
        registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));

        // A pause is requested after a few slices.
        let t0 = Instant::now();
        let mut polls = 0u32;
        let outcome = registry.handshake_and_wait(tid, i23hs_counting_op(&ran), long, slice, || {
            polls += 1;
            polls >= 3
        });
        assert_eq!(outcome, HandshakeOutcome::NotServed);
        assert_eq!(polls, 3);
        assert!(t0.elapsed() < Duration::from_secs(10), "not the budget");
        thread.run_pending_handshakes(&shared);
        assert_eq!(ran.load(Ordering::SeqCst), 0);

        // The target blocks while the requester waits.
        let state = Arc::clone(&thread.gc_block_state);
        let blocker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            state.in_blocked_region.store(true, Ordering::Release);
        });
        let outcome =
            registry.handshake_and_wait(tid, i23hs_counting_op(&ran), long, slice, || false);
        blocker.join().unwrap();
        assert_eq!(outcome, HandshakeOutcome::TargetBlocked);
        thread
            .gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
        thread.run_pending_handshakes(&shared);
        assert_eq!(ran.load(Ordering::SeqCst), 0, "withdrawn, never run");

        // The target is dropped while the requester (on its own thread) waits.
        let registry = Arc::new(registry);
        let requester = {
            let registry = Arc::clone(&registry);
            let ran = Arc::clone(&ran);
            std::thread::spawn(move || {
                registry.handshake_and_wait(tid, i23hs_counting_op(&ran), long, slice, || false)
            })
        };
        std::thread::sleep(Duration::from_millis(5));
        drop(thread);
        assert_eq!(requester.join().unwrap(), HandshakeOutcome::Gone);
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    /// gci i23/hs: an op that unwinds settles its ticket (and any later
    /// one) `Abandoned`, so its requester does not wait for it.
    #[test]
    fn gci_i23hs_an_unwinding_op_abandons_its_ticket() {
        use crate::threading::jvm_thread::{HandshakeState, JvmThread};
        use std::time::Duration;
        let registry = ThreadRegistry::new();
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let tid = ThreadId(36);
        registry.register(tid, "hs6", None);
        let mut thread = JvmThread::new(tid, "hs6");
        registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
        let HandshakeRequest::Queued { ticket: first, .. } = registry.request_handshake(
            tid,
            Box::new(|_: &crate::vm::SharedVm, t: &mut JvmThread| {
                if t.thread_id == ThreadId(36) {
                    panic!("i23hs op unwinds");
                }
            }),
        ) else {
            panic!("queued");
        };
        let HandshakeRequest::Queued { ticket: second, .. } = registry.request_handshake(
            tid,
            Box::new(|_: &crate::vm::SharedVm, t: &mut JvmThread| t.name.push('!')),
        ) else {
            panic!("queued");
        };
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            thread.run_pending_handshakes(&shared);
        }));
        assert!(unwound.is_err());
        assert_eq!(
            first.wait_timeout(Duration::from_secs(5)),
            HandshakeState::Abandoned
        );
        assert_eq!(
            second.wait_timeout(Duration::from_secs(5)),
            HandshakeState::Abandoned
        );
        assert_eq!(thread.name, "hs6", "the op after the unwinding one never ran");
    }

    /// gci i24/rv (`gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017`
    /// item 7): an op that unwinds on a LIVE target is `NotServed` (the
    /// caller takes its frame-trace pause), not `Gone` (which skipped the
    /// pause and served the stale trace). A dropped target stays `Gone`
    /// (`gci_i23hs_handshake_and_wait_stops_early_for_a_pause_a_block_or_a_drop`).
    #[test]
    fn gci_i24rv_an_op_that_unwinds_on_a_live_target_is_not_served() {
        use crate::threading::jvm_thread::JvmThread;
        use std::time::Duration;
        let registry = Arc::new(ThreadRegistry::new());
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let tid = ThreadId(37);
        registry.register(tid, "rv24", None);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        // The target polls on its own OS thread and survives the unwind.
        let target = {
            let registry = Arc::clone(&registry);
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut thread = JvmThread::new(tid, "rv24");
                registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
                ready_tx.send(()).unwrap();
                while !stop.load(Ordering::Acquire) {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        thread.run_pending_handshakes(&shared);
                    }));
                    std::thread::yield_now();
                }
            })
        };
        ready_rx.recv().unwrap();
        let outcome = registry.handshake_and_wait(
            tid,
            Box::new(|_: &crate::vm::SharedVm, t: &mut JvmThread| {
                if t.thread_id == ThreadId(37) {
                    panic!("i24rv op unwinds");
                }
            }),
            Duration::from_secs(30),
            Duration::from_micros(100),
            || false,
        );
        stop.store(true, Ordering::Release);
        target.join().unwrap();
        assert_eq!(outcome, HandshakeOutcome::NotServed);
    }

    #[test]
    fn register_and_is_alive() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker-1", None);
        assert!(registry.is_alive(tid));
        assert_eq!(registry.count(), 1);
        assert_eq!(registry.alive_count(), 1);
        assert_eq!(registry.alive_count_and_os_tids().0, 1);
    }

    #[test]
    fn mark_dead() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker-1", None);
        assert!(registry.is_alive(tid));

        registry.mark_dead(tid);
        assert!(!registry.is_alive(tid));
        assert_eq!(registry.count(), 1);
        assert_eq!(registry.alive_count(), 0);
    }

    /// Dead-owner lock recovery needs POSITIVE evidence of death: a registered
    /// thread marked dead. An id the registry never saw (the JNI `ThreadId(0)`
    /// placeholder) is `!is_alive` too, and releasing its lock would hand a
    /// live thread's monitor to someone else.
    #[test]
    fn is_registered_dead_needs_a_registered_thread_marked_dead() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        assert!(!registry.is_registered_dead(tid), "never registered");
        assert!(!registry.is_registered_dead(ThreadId(0)));
        registry.register(tid, "worker-1", None);
        assert!(!registry.is_registered_dead(tid), "alive");
        registry.mark_dead(tid);
        assert!(registry.is_registered_dead(tid));
    }

    /// T19.H1 summary honesty. The deposited frame chain is only a readout of
    /// where a thread *is* while that thread is inside a blocked region;
    /// otherwise it is history that nothing clears. An unmarked stale chain
    /// has now produced three wrong root causes (see
    /// `ThreadRegistry::deposit_freshness`), the most recent being
    /// `bug-h2-teststringcache-thread-join-blocked-on-dead-threads-20260807`:
    /// `main` was running `TestStringCache.runBenchmark()` while its untagged
    /// deposit still read `java/lang/Thread.join@129`, and the whole doc was
    /// written against that line. So the tags are asserted, not just the
    /// columns.
    #[test]
    fn t19_summary_tags_a_running_threads_deposit_as_stale() {
        assert_eq!(ThreadRegistry::deposit_freshness(true, true), "live");
        assert_eq!(ThreadRegistry::deposit_freshness(true, false), "STALE");
        assert_eq!(
            ThreadRegistry::deposit_freshness(false, true),
            "post-mortem",
            "a dead thread's raised in_blocked_region is termination residue"
        );
        assert_eq!(
            ThreadRegistry::deposit_freshness(false, false),
            "post-mortem"
        );

        let registry = ThreadRegistry::new();
        let running = ThreadId(1);
        let parked = ThreadId(2);
        let dead = ThreadId(3);
        for (tid, name) in [(running, "running"), (parked, "parked"), (dead, "dead")] {
            registry.register(tid, name, None);
            registry.set_frame_trace(
                tid,
                Arc::new(Mutex::new(vec![cratonvm_native_api::StackTraceEntry {
                    class_name: Arc::from("java/lang/Thread"),
                    method_name: Arc::from("join"),
                    method_descriptor: None,
                    source_file: Some(Arc::from("Thread.java")),
                    line_number: crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN,
                    byte_code_index: 129,
                    class_id: None,
                    method_index: None,
                }])),
            );
        }
        // `parked` is genuinely inside a blocked region. `dead` carries the
        // flag its exit sequence's final `deposit_root_snapshot()` raised and
        // that nothing lowers — the exact shape the H2 dump showed for its
        // three finished workers.
        for tid in [parked, dead] {
            registry
                .gc_block_state_of(tid)
                .expect("registered")
                .in_blocked_region
                .store(true, Ordering::Release);
        }
        registry.mark_dead(dead);

        let text = registry.render_thread_summary();
        let line_for = |name: &str| -> String {
            text.lines()
                .find(|l| l.contains(&format!("name={name:?}")))
                .unwrap_or_else(|| panic!("no summary row for {name}:\n{text}"))
                .to_string()
        };

        let running_line = line_for("running");
        assert!(
            running_line.contains("alive=true")
                && running_line.contains("blocked=false")
                && running_line.contains("deposit=STALE"),
            "a running thread's deposit must be tagged STALE: {running_line}"
        );
        assert!(
            line_for("parked").contains("deposit=live"),
            "a thread inside a blocked region IS at its deposited wait site"
        );
        let dead_line = line_for("dead");
        assert!(
            dead_line.contains("alive=false")
                && dead_line.contains("blocked=true")
                && dead_line.contains("deposit=post-mortem"),
            "a dead entry's raised blocked= must be labelled residue: {dead_line}"
        );

        assert!(
            text.contains("(1 at a live wait site, 1 STALE)"),
            "the chain section must split live wait sites from stale history:\n{text}"
        );
        let stale_header = text
            .lines()
            .find(|l| l.contains("name=\"running\"") && l.contains("frame(s), oldest first"))
            .unwrap_or_else(|| panic!("no chain header for the running thread:\n{text}"));
        assert!(
            stale_header.contains("deposit=STALE") && stale_header.contains("RUNNING"),
            "the stale chain's own header must say so: {stale_header}"
        );
        assert!(
            text.contains(ThreadRegistry::DEPOSIT_LEGEND),
            "the legend explaining deposit= must accompany the table:\n{text}"
        );
    }

    /// A RUNNING thread that produced no live dump must be labelled as such,
    /// and must NOT be pointed at a `"T19.H1 stack dump: tid=N"` section that
    /// was never emitted.
    ///
    /// That dangling pointer is what
    /// `known-issues/netty/brotli-integration-test-hangs-outside-the-interpreter`
    /// was written around: the summary said "its real position is in the live
    /// stack dump above", the live dump for that thread did not exist because
    /// the thread was in JIT-compiled code, and the missing section read as
    /// "blocked somewhere the watchdog cannot reach". The thread was neither
    /// blocked nor in native code — it was spinning in a compiled
    /// `ByteBuf.writeByte` loop, and `--nojit` dumped all 73 frames.
    #[test]
    fn t19_summary_names_a_running_thread_that_produced_no_live_dump() {
        let registry = ThreadRegistry::new();
        let dumped = ThreadId(1);
        let silent = ThreadId(2);
        for (tid, name) in [(dumped, "dumped"), (silent, "silent")] {
            registry.register(tid, name, None);
            registry.set_frame_trace(
                tid,
                Arc::new(Mutex::new(vec![cratonvm_native_api::StackTraceEntry {
                    class_name: Arc::from("java/lang/Thread"),
                    method_name: Arc::from("join"),
                    method_descriptor: None,
                    source_file: Some(Arc::from("Thread.java")),
                    line_number: crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN,
                    byte_code_index: 129,
                    class_id: None,
                    method_index: None,
                }])),
            );
        }

        // Only tid=1 answered the watchdog.
        let text = registry.render_thread_summary_for(&[dumped.0]);
        let row_for = |name: &str| -> String {
            let lines: Vec<&str> = text.lines().collect();
            let idx = lines
                .iter()
                .position(|l| l.contains(&format!("name={name:?}")) && l.contains("alive="))
                .unwrap_or_else(|| panic!("no summary row for {name}:\n{text}"));
            lines[idx..(idx + 2).min(lines.len())].join("\n")
        };

        assert!(
            !row_for("dumped").contains("no-live-dump"),
            "a thread that DID dump must not be flagged: {}",
            row_for("dumped")
        );
        assert!(
            row_for("silent").contains("no-live-dump"),
            "a RUNNING thread that produced no dump must say so: {}",
            row_for("silent")
        );
        assert!(
            row_for("silent").contains("--nojit"),
            "and must name the one flag that separates compiled code from \
             native code: {}",
            row_for("silent")
        );

        // The chain header for the silent thread must not send the reader to a
        // section that does not exist.
        let silent_chain = text
            .lines()
            .find(|l| l.contains("name=\"silent\"") && l.contains("frame(s), oldest first"))
            .unwrap_or_else(|| panic!("no chain header for the silent thread:\n{text}"));
        assert!(
            !silent_chain.contains("stack dump: tid=2"),
            "must not point at a live section that was never emitted: {silent_chain}"
        );
        assert!(
            silent_chain.contains("no-live-dump"),
            "the silent thread's chain header must carry the tag: {silent_chain}"
        );
        let dumped_chain = text
            .lines()
            .find(|l| l.contains("name=\"dumped\"") && l.contains("frame(s), oldest first"))
            .unwrap_or_else(|| panic!("no chain header for the dumped thread:\n{text}"));
        assert!(
            dumped_chain.contains("stack dump: tid=1"),
            "a thread that DID dump keeps the pointer to its live section: {dumped_chain}"
        );

        // With no ack information at all, every running thread is reported the
        // safe way rather than the confident-and-wrong way.
        let blind = registry.render_thread_summary();
        assert_eq!(
            blind.matches("no-live-dump").count() >= 2,
            true,
            "an empty ack set must not license the 'read the dump above' \
             phrasing for anyone:\n{blind}"
        );
    }

    #[test]
    fn terminated_entries_are_bounded() {
        let registry = ThreadRegistry::new();
        for id in 1..=(TERMINATED_THREAD_ENTRY_CAP as u64 + 3) {
            let tid = ThreadId(id);
            registry.register(tid, "short-lived", None);
            registry.mark_dead(tid);
        }

        assert_eq!(registry.alive_count(), 0);
        assert_eq!(registry.count(), TERMINATED_THREAD_ENTRY_CAP);
        assert!(!registry.is_alive(ThreadId(1)));
    }

    #[test]
    fn starting_thread_is_not_stw_counted_until_ready() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register_starting_with_daemon(tid, "starting", None, false);

        assert!(registry.is_alive(tid));
        assert_eq!(registry.alive_count(), 1);
        assert_eq!(registry.alive_count_and_os_tids().0, 0);

        registry.mark_stw_ready(tid);
        assert_eq!(registry.alive_count_and_os_tids().0, 1);
    }

    #[test]
    fn unknown_thread_not_alive() {
        let registry = ThreadRegistry::new();
        assert!(!registry.is_alive(ThreadId(99)));
    }

    // -----------------------------------------------------------------------
    // ARCH-2026-07-26 `cross-owner-closeout` (CR-SW-2): thread dumps get line
    // numbers. The depositor publishes a line-less snapshot (it must stay
    // lock-free), so the reader resolves — but only fail-closed.
    // -----------------------------------------------------------------------

    use crate::runtime::stackwalker::test_support::{named_method, store_with};
    use crate::runtime::stackwalker::{LINE_NUMBER_NATIVE, LINE_NUMBER_UNKNOWN};
    use cratonvm_native_api::StackTraceEntry;
    use cratonvm_reader::attribute::LineNumberEntry;

    fn deposited_entry(
        class_id: crate::classloading::ClassId,
        method: &str,
        bci: i32,
    ) -> StackTraceEntry {
        // Exactly the shape `stackwalker::capture_frames_no_lines` deposits.
        StackTraceEntry {
            class_name: Arc::from("probe/Target"),
            method_name: Arc::from(method),
            method_descriptor: None,
            source_file: Some(Arc::from("Target.java")),
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(class_id),
            method_index: None,
        }
    }

    fn registry_with_trace(trace: Vec<StackTraceEntry>) -> (ThreadRegistry, ThreadId) {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker-1", None);
        registry.set_frame_trace(tid, Arc::new(Mutex::new(trace)));
        (registry, tid)
    }

    #[test]
    fn frame_trace_of_is_line_less_and_resolved_reader_fills_it_in() {
        let (store, cid) = store_with(vec![named_method(
            "compute",
            "()I",
            vec![
                LineNumberEntry {
                    start_pc: 0,
                    line_number: 40,
                },
                LineNumberEntry {
                    start_pc: 4,
                    line_number: 41,
                },
            ],
        )]);
        let (registry, tid) = registry_with_trace(vec![
            deposited_entry(cid, "compute", 0),
            deposited_entry(cid, "compute", 6),
        ]);

        // What every consumer sees today.
        let raw = registry.frame_trace_of(tid);
        assert_eq!(raw.len(), 2);
        assert!(raw.iter().all(|e| e.line_number == LINE_NUMBER_UNKNOWN));

        // What a reader holding a ClassStore can see instead.
        let resolved = registry.frame_trace_of_resolved(tid, &store);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].line_number, 40);
        assert_eq!(resolved[1].line_number, 41);

        // Resolution is non-destructive: the published snapshot is untouched,
        // so a second reader still starts from the deposited state.
        assert!(registry
            .frame_trace_of(tid)
            .iter()
            .all(|e| e.line_number == LINE_NUMBER_UNKNOWN));
    }

    #[test]
    fn resolved_reader_leaves_overloaded_and_native_frames_alone() {
        let (store, cid) = store_with(vec![
            named_method(
                "run",
                "(I)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 11,
                }],
            ),
            named_method(
                "run",
                "(J)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 22,
                }],
            ),
        ]);
        let mut native = deposited_entry(cid, "run", -1);
        native.line_number = LINE_NUMBER_NATIVE;
        let (registry, tid) = registry_with_trace(vec![deposited_entry(cid, "run", 0), native]);

        let resolved = registry.frame_trace_of_resolved(tid, &store);
        assert_eq!(
            resolved[0].line_number, LINE_NUMBER_UNKNOWN,
            "a deposited frame has no method_index, so an overload set must \
             stay unknown rather than print a line from the wrong body"
        );
        assert_eq!(resolved[1].line_number, LINE_NUMBER_NATIVE);
        // Frame identity is never lost, which is the property a dump needs.
        assert_eq!(&*resolved[0].class_name, "probe/Target");
        assert_eq!(&*resolved[0].method_name, "run");
    }

    #[test]
    fn resolved_reader_on_an_unknown_thread_is_empty_not_a_panic() {
        let (store, _cid) = store_with(vec![named_method("compute", "()I", Vec::new())]);
        let registry = ThreadRegistry::new();
        assert!(registry
            .frame_trace_of_resolved(ThreadId(99), &store)
            .is_empty());
    }

    #[test]
    fn resolved_reader_after_class_unload_keeps_the_frame_and_drops_only_the_line() {
        let (mut store, cid) = store_with(vec![named_method(
            "compute",
            "()I",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 40,
            }],
        )]);
        let (registry, tid) = registry_with_trace(vec![deposited_entry(cid, "compute", 0)]);
        assert_eq!(
            registry.frame_trace_of_resolved(tid, &store)[0].line_number,
            40
        );

        // ClassIds are monotonic and never reused, so a stale id can only miss.
        let _ = store.remove(cid);
        let after = registry.frame_trace_of_resolved(tid, &store);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].line_number, LINE_NUMBER_UNKNOWN);
        assert_eq!(&*after[0].method_name, "compute");
    }

    #[test]
    fn native_blocked_thread_is_reported_as_blocked_until_woken() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "waiter", None);

        assert!(!registry.is_blocked(tid));
        registry.mark_native_thread_blocked(tid);
        assert!(registry.is_blocked(tid));
        registry.mark_native_thread_unblocked(tid);
        assert!(!registry.is_blocked(tid));

        registry.mark_dead(tid);
        assert!(!registry.is_blocked(tid));
    }

    // -----------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------

    /// Build a non-null, 8-byte-aligned dummy heap address. The registry
    /// only round-trips the address (scan as root / remap) and never
    /// dereferences it, so a live `Box` allocation is a safe stand-in for
    /// a real heap object pointer.
    fn dummy_aligned_objref(backing: &mut [u64; 2]) -> ObjectRef {
        let ptr = backing.as_mut_ptr() as *mut u8;
        // SAFETY: `backing` is a live, 8-byte-aligned, non-null allocation.
        unsafe { ObjectRef::from_raw(ptr) }
    }

    /// An alive thread's `java.lang.Thread` mirror is reported as a GC root
    /// by `collect_all_root_snapshots`, so a collection initiated by ANOTHER
    /// thread (while this one is parked) cannot sweep the mirror out from
    /// under the dangling `ThreadEntry.java_thread_obj` copy. Regression for
    /// the Tomcat TestDigestAuthenticator "this.holder is null" NPE: a worker-
    /// thread GC freed the parked JUnit main thread's mirror, so the cached
    /// `currentThread()` pointer dereferenced an all-zero (freed) header.
    #[test]
    fn alive_thread_mirror_is_a_root() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);

        let mut backing = [0u64; 2];
        let mirror = dummy_aligned_objref(&mut backing);

        // Registered WITHOUT a mirror → not a root yet.
        registry.register(tid, "main", None);
        assert!(registry.collect_all_root_snapshots().is_empty());

        // Once the mirror is published it must be rooted.
        registry.set_java_thread_obj(tid, mirror);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(roots.len(), 1, "alive thread mirror must be a root");
        assert_eq!(roots[0].as_ptr(), mirror.as_ptr());

        // A dead thread's mirror is no longer rooted (it may be reclaimed).
        registry.mark_dead(tid);
        assert!(registry.collect_all_root_snapshots().is_empty());
    }

    #[test]
    fn collect_reserved_tlab_tails_reports_only_alive_unretired_tlabs() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);

        let mut backing = vec![0u64; 128];
        let ptr = backing.as_mut_ptr() as *mut u8;
        let size = backing.len() * std::mem::size_of::<u64>();
        let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
        assert!(tlab.alloc(64, 8).is_some());

        registry.set_tlab_addr(tid, &tlab as *const cratonvm_gc::Tlab as usize);

        let tails = registry.collect_reserved_tlab_tails();
        assert_eq!(tails, vec![(ptr as usize + 64, ptr as usize + size)]);

        tlab.retire();
        assert!(
            registry.collect_reserved_tlab_tails().is_empty(),
            "retired TLABs must not publish skip regions"
        );

        backing.fill(0);
        let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
        assert!(tlab.alloc(64, 8).is_some());
        registry.set_tlab_addr(tid, &tlab as *const cratonvm_gc::Tlab as usize);
        registry.mark_dead(tid);
        assert!(
            registry.collect_reserved_tlab_tails().is_empty(),
            "dead threads must not publish stale TLAB pointers"
        );
    }

    /// gci i28/tl: the `TLAB totals` snapshot reads each live thread's cycle
    /// counters through its published address, and skips a dead one.
    #[test]
    fn gci_i28tl_tlab_cycle_snapshots_read_live_threads_only() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);
        let mut backing = vec![0u64; 128];
        let ptr = backing.as_mut_ptr() as *mut u8;
        let size = backing.len() * std::mem::size_of::<u64>();
        // SAFETY: `backing` outlives `tlab` and is `size` bytes.
        let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
        assert!(tlab.alloc(64, 8).is_some());
        tlab.retire_for_refill();
        let addr = &tlab as *const cratonvm_gc::Tlab as usize;
        registry.set_tlab_addr(tid, addr);
        let snaps = registry.tlab_cycle_snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].0, 1, "keyed by the thread id");
        assert_eq!(snaps[0].1, tlab.cycle_mark());
        assert_eq!(
            snaps[0].1.since_start().refill_waste_bytes,
            (size - 64) as u64
        );
        assert_eq!(snaps[0].2, tlab.thread_allocated_bytes());
        registry.mark_dead(tid);
        assert!(registry.tlab_cycle_snapshots().is_empty());
    }

    /// INT-3 — the G1 takeover path pins the deposited snapshot roots of
    /// FROZEN peers only, selected by OS tid; other threads' snapshots (and
    /// dead threads) must not leak into the pin set.
    #[test]
    fn root_snapshots_for_os_tids_returns_only_matching_alive_threads() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);
        registry.set_os_tid_current(tid);

        let mut backing = [0u64; 2];
        let root = dummy_aligned_objref(&mut backing);
        registry.set_root_snapshot(tid, Arc::new(Mutex::new(vec![root])));

        let (_, _, tids, _) = registry.alive_count_blocked_and_os_tids();
        assert!(!tids.is_empty(), "os tid must be published");
        let roots = registry.root_snapshots_for_os_tids(&tids);
        assert_eq!(roots.len(), 1, "matching alive thread's snapshot returned");
        assert_eq!(roots[0].as_ptr(), root.as_ptr());

        assert!(
            registry.root_snapshots_for_os_tids(&[u32::MAX]).is_empty(),
            "non-matching tid must contribute nothing"
        );
        assert!(
            registry.root_snapshots_for_os_tids(&[]).is_empty(),
            "empty tid set short-circuits"
        );

        registry.mark_dead(tid);
        assert!(
            registry.root_snapshots_for_os_tids(&tids).is_empty(),
            "dead thread's snapshot must not be pinned"
        );
    }

    #[test]
    fn moved_thread_mirror_records_former_address_for_recovery() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);

        let mut old_backing = [0u64; 2];
        let mut new_backing = [0u64; 2];
        let old_ref = dummy_aligned_objref(&mut old_backing);
        let new_ref = dummy_aligned_objref(&mut new_backing);
        let old_addr = old_ref.as_ptr() as usize;
        let new_addr = new_ref.as_ptr() as usize;
        assert_ne!(old_addr, new_addr);

        registry.register(tid, "main", Some(old_ref));

        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(old_addr, new_addr);
        registry.update_thread_objs_after_gc(&pm);

        assert_eq!(
            registry.java_thread_obj(tid).unwrap().as_ptr() as usize,
            new_addr,
            "registry mirror must be remapped to the live address",
        );
        assert_eq!(
            registry.recover_stale_mirror(old_addr).unwrap().as_ptr() as usize,
            new_addr,
            "former mirror address must recover the live mirror",
        );
        assert!(
            registry.recover_stale_mirror(new_addr).is_none(),
            "the current live address is not itself a former address",
        );
    }

    #[test]
    fn stale_mirror_recovery_survives_multiple_moves() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);

        let mut first_backing = [0u64; 2];
        let mut second_backing = [0u64; 2];
        let mut third_backing = [0u64; 2];
        let first = dummy_aligned_objref(&mut first_backing);
        let second = dummy_aligned_objref(&mut second_backing);
        let third = dummy_aligned_objref(&mut third_backing);
        let first_addr = first.as_ptr() as usize;
        let second_addr = second.as_ptr() as usize;
        let third_addr = third.as_ptr() as usize;
        assert_ne!(first_addr, second_addr);
        assert_ne!(second_addr, third_addr);

        registry.register(tid, "main", Some(first));

        let mut pm1 = cratonvm_types::PointerMap::default();
        pm1.insert(first_addr, second_addr);
        registry.update_thread_objs_after_gc(&pm1);

        let mut pm2 = cratonvm_types::PointerMap::default();
        pm2.insert(second_addr, third_addr);
        registry.update_thread_objs_after_gc(&pm2);

        for stale_addr in [first_addr, second_addr] {
            assert_eq!(
                registry.recover_stale_mirror(stale_addr).unwrap().as_ptr() as usize,
                third_addr,
                "any retained former mirror address should recover the current mirror",
            );
        }
    }

    #[test]
    fn thread_name() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "my-thread", None);
        assert_eq!(registry.thread_name(tid), Some("my-thread".to_string()));
        assert_eq!(registry.thread_name(ThreadId(99)), None);
    }

    #[test]
    fn join_with_real_thread() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);

        let handle = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(10));
        });
        registry.set_join_handle(tid, handle);

        let joined = registry.join(tid);
        assert!(joined);

        // Second join returns false (handle already taken)
        let joined2 = registry.join(tid);
        assert!(!joined2);
    }

    #[test]
    fn join_no_handle_dead_thread() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);
        registry.mark_dead(tid);

        // No handle, thread is dead — returns false immediately
        let joined = registry.join(tid);
        assert!(!joined);
    }

    // -----------------------------------------------------------------
    // T19.K1 — non-daemon thread waiting tests
    // -----------------------------------------------------------------

    /// Default `register` paints the thread as non-daemon.
    #[test]
    fn t19_k1_default_register_is_non_daemon() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "user", None);
        assert!(!registry.is_daemon(tid));
        assert_eq!(registry.alive_non_daemon_thread_ids(), vec![tid]);
    }

    /// `register_with_daemon(true)` paints it as daemon and excludes it
    /// from the non-daemon enumeration.
    #[test]
    fn t19_k1_register_with_daemon_excludes_from_wait() {
        let registry = ThreadRegistry::new();
        let user = ThreadId(1);
        let dmn = ThreadId(2);
        registry.register_with_daemon(user, "user", None, false);
        registry.register_with_daemon(dmn, "dmn", None, true);

        assert!(!registry.is_daemon(user));
        assert!(registry.is_daemon(dmn));
        assert_eq!(registry.alive_non_daemon_thread_ids(), vec![user]);
    }

    /// `set_daemon` flips the flag for a registered thread; setting
    /// `daemon = true` removes it from the wait set.
    #[test]
    fn t19_k1_set_daemon_flips_flag() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "worker", None);
        assert!(!registry.is_daemon(tid));
        assert!(registry.set_daemon(tid, true));
        assert!(registry.is_daemon(tid));
        assert!(registry.alive_non_daemon_thread_ids().is_empty());
    }

    /// `set_daemon` on an unknown id returns `false` and is a no-op.
    #[test]
    fn t19_k1_set_daemon_unknown_id_returns_false() {
        let registry = ThreadRegistry::new();
        assert!(!registry.set_daemon(ThreadId(99), true));
    }

    /// HelloWorld parity: an empty registry waits for zero threads
    /// and returns immediately.
    ///
    /// `n == 0` is the property. The elapsed bound only distinguishes
    /// "returned" from "blocked forever", so it is a hang backstop, not a
    /// 50 ms performance budget — a loaded runner can lose 50 ms to
    /// scheduling before the test body even starts (observed failing).
    #[test]
    fn t19_k1_wait_with_empty_registry_returns_zero() {
        let registry = ThreadRegistry::new();
        let start = std::time::Instant::now();
        let n = registry.wait_for_non_daemon_threads(None);
        assert_eq!(n, 0);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "empty wait must not block: {:?}",
            start.elapsed(),
        );
    }

    /// HelloWorld parity: a registry that contains only daemon threads
    /// waits for zero threads and returns immediately.
    ///
    /// As above: `n == 0` plus "neither daemon was joined" (only `join()`
    /// marks a thread dead) are the timing-independent properties; the
    /// elapsed bound is a hang backstop.
    #[test]
    fn t19_k1_wait_with_only_daemons_returns_zero() {
        let registry = ThreadRegistry::new();
        registry.register_with_daemon(ThreadId(1), "gc", None, true);
        registry.register_with_daemon(ThreadId(2), "finalizer", None, true);
        let start = std::time::Instant::now();
        let n = registry.wait_for_non_daemon_threads(None);
        assert_eq!(n, 0);
        assert!(registry.is_alive(ThreadId(1)));
        assert!(registry.is_alive(ThreadId(2)));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "daemon-only wait must not block: {:?}",
            start.elapsed(),
        );
    }

    /// Happy path: a non-daemon thread blocks the wait until it
    /// terminates.
    ///
    /// "Blocked until it terminated" is stated directly by the worker's
    /// completion flag: the flag is set as the worker's last action, so
    /// observing it `true` *after* the wait returned proves the ordering.
    /// The old `elapsed >= 80 ms` was a proxy for that and is not actually
    /// safe under load in the way a lower bound usually is: the clock only
    /// starts after `spawn` + `set_join_handle`, so on a loaded box the
    /// worker's 100 ms sleep can be over before `start` is even sampled
    /// (its sibling `t19_k1_wait_for_multiple_non_daemons` was observed
    /// failing that way at 134 µs).
    #[test]
    fn t19_k1_wait_blocks_until_non_daemon_finishes() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "user", None);
        let finished = Arc::new(AtomicBool::new(false));
        let f = finished.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            f.store(true, Ordering::Release);
        });
        registry.set_join_handle(tid, handle);
        let joined = registry.wait_for_non_daemon_threads(None);
        assert_eq!(joined, 1);
        assert!(
            finished.load(Ordering::Acquire),
            "the wait returned before the non-daemon thread had finished",
        );
    }

    /// A daemon thread does not extend the wait — even if it runs for a
    /// long time, the wait returns without considering it.
    ///
    /// Same flake shape as `t19_k1_mixed_only_joins_non_daemons`: the
    /// original `start.elapsed() < 50 ms` is a fixed wall-clock bound with
    /// essentially no margin, and it was observed failing under load. What
    /// must be true is that the daemon was *not waited for*, which the
    /// counters below state directly and load cannot falsify.
    #[test]
    fn t19_k1_daemon_thread_does_not_keep_main_alive() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register_with_daemon(tid, "dmn", None, true);
        let daemon_finished = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let df = daemon_finished.clone();
        let rel = release.clone();
        let handle = std::thread::spawn(move || {
            // Long-running — the wait must not block on this. The 60 s cap
            // makes a regression fail rather than hang forever.
            let cap = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !rel.load(Ordering::Acquire) && std::time::Instant::now() < cap {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            df.store(true, Ordering::Release);
        });
        registry.set_join_handle(tid, handle);
        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        let elapsed = start.elapsed();
        assert_eq!(joined, 0, "a daemon must never be joined");
        assert!(
            !daemon_finished.load(Ordering::Acquire),
            "the wait returned only after the daemon finished — it was waited for",
        );
        // Only `join()` marks a thread dead, so this is a direct readout
        // that the daemon was skipped.
        assert!(registry.is_alive(tid));
        // Hang backstop only — well under the daemon's 60 s cap so
        // "waited for the daemon anyway" still fails here.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "daemon thread must not delay shutdown: {:?}",
            elapsed,
        );
        release.store(true, Ordering::Release);
        assert!(registry.join(tid), "daemon handle should still be present");
    }

    /// Multiple non-daemon threads — the wait blocks until ALL of them
    /// finish, and the count reflects all joined.
    ///
    /// "Waited for the slowest" is stated as a completion count taken
    /// after the wait returned, not as an elapsed-time lower bound. The
    /// old `elapsed >= 80 ms` looked like a safe-direction assertion but
    /// was not: the clock starts only after all three spawns, so on a
    /// loaded box the sleeps can already be over by then — this test was
    /// observed failing with `must wait for slowest: 134.294µs`.
    #[test]
    fn t19_k1_wait_for_multiple_non_daemons() {
        let registry = ThreadRegistry::new();
        let done = Arc::new(AtomicU64::new(0));
        for (i, sleep_ms) in [50u64, 100, 80].iter().enumerate() {
            let tid = ThreadId(i as u64 + 1);
            registry.register(tid, &format!("user-{i}"), None);
            let ms = *sleep_ms;
            let d = done.clone();
            let handle = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                d.fetch_add(1, Ordering::AcqRel);
            });
            registry.set_join_handle(tid, handle);
        }
        let joined = registry.wait_for_non_daemon_threads(None);
        assert_eq!(joined, 3);
        assert_eq!(
            done.load(Ordering::Acquire),
            3,
            "the wait returned before every non-daemon thread had finished",
        );
    }

    /// A panic in a non-daemon thread doesn't deadlock the joiner —
    /// `JoinHandle::join` returns `Err` but our wrapper swallows it
    /// and continues.
    #[test]
    fn t19_k1_panic_in_non_daemon_does_not_deadlock_joiner() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "panicker", None);
        let handle = std::thread::Builder::new()
            .name("k1-panicker".into())
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(20));
                panic!("intentional panic for K1 test");
            })
            .expect("spawn");
        registry.set_join_handle(tid, handle);
        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        let elapsed = start.elapsed();
        assert_eq!(joined, 1, "panicked thread should still count as joined");
        // `joined == 1` is the property (the `Err` from `JoinHandle::join`
        // was swallowed and counted). The elapsed check only separates
        // "returned" from "deadlocked", so it is a hang backstop — the
        // former 5 s budget was observed failing on a loaded runner.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "join must complete in bounded time even when target panicked: {:?}",
            elapsed,
        );
    }

    /// Mixed daemon + non-daemon: the wait only joins the non-daemon
    /// threads. The daemon thread is left alive (its handle still in
    /// the registry — caller is expected to abandon it on exit).
    ///
    /// The property under test is WHICH threads were joined, not how long
    /// the call took. The earlier version asserted
    /// `start.elapsed() < 1s`, which is a fixed wall-clock bound over three
    /// OS-thread spawns plus a 30 ms sleep; on a loaded CI runner the
    /// spawns alone can blow it, so it flaked. Every assertion below is
    /// timing-independent except one deliberately generous hang backstop:
    ///
    /// * `joined == 1` — exactly one join succeeded.
    /// * `user_finished` — the non-daemon's body ran to completion before
    ///   the wait returned (we really did wait for it).
    /// * `!daemon_finished` — the daemon was still mid-flight when the
    ///   wait returned (we did *not* wait for it). The daemon is gated on
    ///   a flag this test owns, so this cannot be falsified by slowness:
    ///   load can only delay the observation, never open the gate.
    /// * `is_alive(dmn)` / `!is_alive(user)` — only `join()` calls
    ///   `mark_dead`, so the alive flags are a direct readout of who was
    ///   joined, independent of wall clock.
    #[test]
    fn t19_k1_mixed_only_joins_non_daemons() {
        let registry = ThreadRegistry::new();
        let user = ThreadId(1);
        let dmn = ThreadId(2);
        registry.register_with_daemon(user, "user", None, false);
        registry.register_with_daemon(dmn, "dmn", None, true);

        let user_finished = Arc::new(AtomicBool::new(false));
        let daemon_finished = Arc::new(AtomicBool::new(false));
        // Gate that keeps the daemon running until this test releases it.
        let release_daemon = Arc::new(AtomicBool::new(false));

        let uf = user_finished.clone();
        let user_h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            uf.store(true, Ordering::Release);
        });
        let df = daemon_finished.clone();
        let rd = release_daemon.clone();
        let dmn_h = std::thread::spawn(move || {
            // Long-running daemon — must NOT be joined. The 60 s cap is a
            // safety net so a regression fails instead of hanging forever.
            let cap = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !rd.load(Ordering::Acquire) && std::time::Instant::now() < cap {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            df.store(true, Ordering::Release);
        });
        registry.set_join_handle(user, user_h);
        registry.set_join_handle(dmn, dmn_h);

        let start = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(None);
        let elapsed = start.elapsed();

        assert_eq!(joined, 1, "only the non-daemon thread may be joined");
        assert!(
            user_finished.load(Ordering::Acquire),
            "the non-daemon thread must have completed before the wait returned",
        );
        assert!(
            !daemon_finished.load(Ordering::Acquire),
            "the wait returned only after the daemon finished — it was waited for",
        );
        // Daemon is still alive (we didn't join it).
        assert!(registry.is_alive(dmn));
        // User thread is joined → marked dead.
        assert!(!registry.is_alive(user));
        // Hang backstop only. Must stay comfortably below the daemon's 60 s
        // cap so "joined the daemon anyway" still fails here rather than
        // sailing through; it is *not* a performance assertion.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "wait_for_non_daemon_threads appears to have hung: {:?}",
            elapsed,
        );

        // Release and reap the daemon so it doesn't linger for the rest of
        // the test binary's life.
        release_daemon.store(true, Ordering::Release);
        assert!(registry.join(dmn), "daemon handle should still be present");
    }

    /// Deadline-bounded wait: when a non-daemon thread's
    /// `JoinHandle` is missing (registered + alive but no
    /// `set_join_handle` call) the inner `join()` call returns
    /// `false` after a brief spin-wait. The deadline is checked
    /// between iterations; here we use a deadline shorter than
    /// the spin-wait would take to complete a fresh second
    /// iteration. The wait returns within a bounded window even
    /// though the alive flag never flipped.
    #[test]
    fn t19_k1_wait_respects_deadline_when_join_returns_false() {
        let registry = ThreadRegistry::new();
        // Spawn a real OS thread that runs for 50 ms then signals
        // the registry it's dead. We set the handle so the join
        // call has something to take.
        let tid = ThreadId(1);
        registry.register(tid, "dies-eventually", None);
        let handle = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(50));
        });
        registry.set_join_handle(tid, handle);
        // Deadline is generous: we expect the thread to finish in
        // ~50 ms, the join to complete, and the loop to exit
        // before the deadline. This proves the deadline path
        // doesn't pessimize the happy path.
        //
        // `joined == 1` *is* the assertion: had the wait not completed
        // within the deadline, the loop would have bailed out and returned
        // 0. So the deadline itself is the bound under test, and it is set
        // generously (a loaded runner needs the slack — the former 5 s
        // deadline / 2 s assertion pair was observed failing). The elapsed
        // check below is only a hang backstop.
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(60);
        let joined = registry.wait_for_non_daemon_threads(Some(deadline));
        assert_eq!(
            joined, 1,
            "the happy path must complete within a generous deadline"
        );
        assert!(
            !registry.is_alive(tid),
            "the joined thread must be marked dead"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "happy-path deadline should not extend execution: {:?}",
            start.elapsed(),
        );
    }

    /// `alive_non_daemon_thread_ids` reflects the current state at
    /// the moment of the snapshot — once a thread is marked dead
    /// it drops out of the enumeration.
    #[test]
    fn t19_k1_snapshot_excludes_dead_threads() {
        let registry = ThreadRegistry::new();
        let a = ThreadId(1);
        let b = ThreadId(2);
        registry.register(a, "a", None);
        registry.register(b, "b", None);
        assert_eq!(registry.alive_non_daemon_thread_ids().len(), 2);
        registry.mark_dead(a);
        assert_eq!(registry.alive_non_daemon_thread_ids(), vec![b]);
    }

    /// Re-snapshot after a non-daemon thread spawns more non-daemon
    /// threads: the wait loop must catch the second batch even
    /// though the first snapshot was already empty when those
    /// threads were added.
    #[test]
    fn t19_k1_wait_picks_up_late_spawned_threads() {
        let registry = std::sync::Arc::new(ThreadRegistry::new());
        let tid_a = ThreadId(1);
        registry.register(tid_a, "a", None);

        // Thread A sleeps a bit, then registers thread B.
        let b_finished = Arc::new(AtomicBool::new(false));
        let bf = b_finished.clone();
        let reg2 = registry.clone();
        let handle_a = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            // Register a second non-daemon thread that runs for 60 ms.
            let tid_b = ThreadId(2);
            reg2.register(tid_b, "b", None);
            let h_b = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(60));
                bf.store(true, Ordering::Release);
            });
            reg2.set_join_handle(tid_b, h_b);
        });
        registry.set_join_handle(tid_a, handle_a);

        let joined = registry.wait_for_non_daemon_threads(None);
        // Both A and B must be joined. `joined == 2` plus B's completion
        // flag say "the re-snapshot caught B" directly; the former
        // `elapsed >= 80 ms` said it only by proxy and, like its siblings,
        // could be defeated by a loaded box finishing the sleeps before
        // the clock was even sampled.
        assert_eq!(joined, 2, "must catch late-spawned thread B");
        assert!(
            b_finished.load(Ordering::Acquire),
            "the wait returned before late-spawned thread B had finished",
        );
        assert!(!registry.is_alive(ThreadId(2)), "B must be joined + dead");
    }

    /// `register_with_daemon` and the legacy `register` must coexist —
    /// the legacy path defaults to non-daemon, the explicit path
    /// honours its argument.
    #[test]
    fn t19_k1_legacy_register_is_non_daemon_explicit_register_honors_flag() {
        let registry = ThreadRegistry::new();
        registry.register(ThreadId(1), "legacy", None);
        registry.register_with_daemon(ThreadId(2), "explicit-daemon", None, true);
        registry.register_with_daemon(ThreadId(3), "explicit-user", None, false);

        let mut ids = registry.alive_non_daemon_thread_ids();
        ids.sort_by_key(|t| t.0);
        assert_eq!(ids, vec![ThreadId(1), ThreadId(3)]);
    }

    /// Deadline-bounded wait exits before all threads are joined: an
    /// expired deadline stops the loop, leaving live non-daemon threads
    /// unjoined; the same registry then joins all of them once the
    /// deadline is lifted.
    ///
    /// The deadline is checked *between* joins — an in-flight
    /// `JoinHandle::join` is not interruptible — so the property under
    /// test is "the loop stopped handing out joins", not "the call took
    /// less than N milliseconds".
    ///
    /// The earlier version spawned five 30 ms sleepers with a 60 ms
    /// deadline and asserted only `start.elapsed() < 1s`. That measured
    /// nothing: the five sleepers run concurrently, so on an idle box all
    /// five finish at ~30 ms and the deadline never fires at all, while on
    /// a loaded CI runner five OS-thread spawns plus scheduling latency can
    /// exceed 1 s outright — a pure machine-speed assertion, and a flaky
    /// one. Here the workers are gated on a flag this test owns, so they
    /// are guaranteed still alive and unjoinable while the deadline is
    /// tested, and the outcome is decided by counters rather than a clock:
    ///
    /// * `joined == 0` with all five still `is_alive` after the expired
    ///   deadline — only `join()` calls `mark_dead`, so this is a direct
    ///   timing-independent readout that nothing was joined. Had the
    ///   deadline been ignored, the call would have blocked on the gate
    ///   (and, after the workers' own safety cap, reported 5).
    /// * `joined == 5` on the follow-up unbounded wait — proves the zero
    ///   above was the deadline's doing and not an empty snapshot.
    #[test]
    fn t19_k1_deadline_exits_before_all_joined() {
        let registry = ThreadRegistry::new();
        // Gate: the workers stay alive — and therefore unjoinable — until
        // this test releases them.
        let release = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::new();
        for i in 0..5 {
            let tid = ThreadId(i as u64 + 1);
            registry.register(tid, &format!("u-{i}"), None);
            let rel = release.clone();
            let handle = std::thread::spawn(move || {
                // The 30 s cap is a safety net: a regression that ignores
                // the deadline fails loudly instead of hanging forever.
                let cap = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while !rel.load(Ordering::Acquire) && std::time::Instant::now() < cap {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            registry.set_join_handle(tid, handle);
            ids.push(tid);
        }

        // A deadline sampled *now* is already reached by the time the wait
        // checks it (`Instant` is monotonic, and the check is `>=`), so the
        // loop must bail out before its first join. No sleeping, no
        // race: this cannot be perturbed by machine load.
        let start = std::time::Instant::now();
        let deadline = std::time::Instant::now();
        let joined = registry.wait_for_non_daemon_threads(Some(deadline));
        let elapsed = start.elapsed();

        assert_eq!(
            joined, 0,
            "an already-expired deadline must stop the wait before any join",
        );
        for tid in &ids {
            assert!(
                registry.is_alive(*tid),
                "{tid:?} was joined despite the expired deadline",
            );
        }
        // Hang backstop only — must stay well under the workers' 30 s cap
        // so a deadline regression still fails here. Not a perf assertion.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "deadline must bound the wait: {:?}",
            elapsed,
        );

        // Lift the gate: the very same registry now joins all five, which
        // is what makes the `0` above attributable to the deadline.
        release.store(true, Ordering::Release);
        assert_eq!(
            registry.wait_for_non_daemon_threads(None),
            5,
            "the unbounded wait must join every non-daemon thread",
        );
        for tid in &ids {
            assert!(!registry.is_alive(*tid), "{tid:?} should be joined + dead");
        }
    }

    // -----------------------------------------------------------------
    // ARCH-2026-07-26 — registry contention
    // -----------------------------------------------------------------

    /// Registry reads are concurrent: many threads must be able to be inside
    /// `is_alive` / `get_park_state` / `is_blocked` at the same instant.
    ///
    /// Proven by holding a *read* guard on the map for the whole window and
    /// requiring readers on other threads to complete anyway. Under the old
    /// `Mutex` this would deadlock; under an `RwLock` the readers share.
    #[test]
    fn registry_reads_do_not_exclude_each_other() {
        let registry = Arc::new(ThreadRegistry::new());
        for i in 0..8u64 {
            registry.register(ThreadId(i), &format!("t{i}"), None);
        }

        let held = registry.threads.read();

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut workers = Vec::new();
        for i in 0..8u64 {
            let r = registry.clone();
            let tx = tx.clone();
            workers.push(std::thread::spawn(move || {
                assert!(r.is_alive(ThreadId(i)));
                assert!(!r.is_blocked(ThreadId(i)));
                assert_eq!(r.java_block_state(ThreadId(i)), 0);
                assert!(r.get_park_state(ThreadId(i)).is_some());
                assert_eq!(r.thread_name(ThreadId(i)), Some(format!("t{i}")));
                assert_eq!(r.alive_count(), 8);
                let _ = tx.send(());
            }));
        }
        drop(tx);

        let mut done = 0;
        while done < 8 {
            if rx.recv_timeout(std::time::Duration::from_secs(10)).is_err() {
                break;
            }
            done += 1;
        }
        // Release before asserting/joining so a regression fails the assert
        // instead of hanging the suite.
        drop(held);
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(
            done, 8,
            "registry readers blocked each other — `threads` must be an RwLock \
             so concurrent lookups do not serialize"
        );
    }

    /// Registration still excludes readers: a write guard must block a reader,
    /// which is what keeps the map's invariants intact across insert/remove.
    #[test]
    fn registry_writes_still_exclude_readers() {
        let registry = Arc::new(ThreadRegistry::new());
        registry.register(ThreadId(1), "t", None);

        let held = registry.threads.write();
        let r = registry.clone();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let alive = r.is_alive(ThreadId(1));
            let _ = tx.send(());
            alive
        });

        let blocked = rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_err();
        drop(held);
        let alive = worker.join().unwrap();
        assert!(blocked, "a writer must exclude readers");
        assert!(
            alive,
            "the reader must still get the right answer afterwards"
        );
    }

    /// The census reports EVERY thread parked in `Object.wait()`, not the one
    /// the watchdog happened to reach first.
    ///
    /// This is the property
    /// `internal/fixed-suite-bugs/netty/parameterizedsslhandlertest-residual-stalls-FIXED-20261013` was
    /// blocked on: with one waiter reported, "nothing completed this promise"
    /// and "the thread that would have completed it is itself parked" are
    /// indistinguishable. A thread that has LEFT the wait must not appear, or
    /// the census would manufacture the second reading.
    #[test]
    fn waiting_monitor_census_lists_every_waiter_and_only_waiters() {
        let registry = ThreadRegistry::new();
        let mut b1 = [0u64; 2];
        let mut b2 = [0u64; 2];
        let mut b3 = [0u64; 2];
        let (m1, m2, m3) = (
            dummy_aligned_objref(&mut b1),
            dummy_aligned_objref(&mut b2),
            dummy_aligned_objref(&mut b3),
        );
        for (i, name) in ["main", "reactor-1", "reactor-2"].iter().enumerate() {
            registry.register(ThreadId(i as u64), name, None);
        }
        assert!(
            registry.waiting_monitor_census().is_empty(),
            "nobody is waiting yet"
        );

        registry.set_jmx_waiting_monitor(ThreadId(0), m1);
        registry.set_jmx_waiting_monitor(ThreadId(2), m3);
        let rows = registry.waiting_monitor_census();
        assert_eq!(rows.len(), 2, "both waiters, and only them: {rows:?}");
        assert_eq!(rows[0].0, 0);
        assert_eq!(rows[0].1, "main");
        assert!(std::ptr::eq(rows[0].3.as_ptr(), m1.as_ptr()));
        assert_eq!(rows[1].0, 2);
        assert!(std::ptr::eq(rows[1].3.as_ptr(), m3.as_ptr()));

        // A thread that leaves the wait drops out; one that enters appears.
        registry.take_jmx_waiting_monitor(ThreadId(0));
        registry.set_jmx_waiting_monitor(ThreadId(1), m2);
        let rows = registry.waiting_monitor_census();
        assert_eq!(
            rows.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![1, 2],
            "the census must track the slot, not a high-water mark"
        );
    }

    /// gc-common 2026-09-23 (lane A) — a carrier whose OS thread finished
    /// without `mark_dead` (its closure unwound) must drop out of every
    /// stop-the-world census and must not have its TLAB read, while its
    /// Java-visible liveness is left to the ordinary paths.
    #[test]
    fn a_finished_carrier_that_skipped_mark_dead_is_not_stw_counted() {
        let registry = ThreadRegistry::new();
        let main = ThreadId(0);
        let worker = ThreadId(1);
        registry.register(main, "main", None);
        registry.register(worker, "worker", None);
        // A TLAB address that must never be dereferenced: if the census still
        // treated this entry as live, `collect_reserved_tlab_tails` would read
        // through it.
        registry.set_tlab_addr(worker, 0x8);
        let handle = std::thread::spawn(|| {});
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        registry.set_join_handle(worker, handle);

        let (alive, blocked, _os, blocked_tids) = registry.alive_count_blocked_and_os_tids();
        assert_eq!(alive, 1, "only main may be counted");
        assert_eq!(blocked, 0);
        assert!(blocked_tids.is_empty());
        assert_eq!(registry.alive_count_and_os_tids().0, 1);
        assert_eq!(registry.alive_thread_ids_excluding(&[]), vec![0]);
        assert!(
            registry.collect_reserved_tlab_tails().is_empty(),
            "a finished carrier's TLAB address must not be dereferenced"
        );
        // Java-visible liveness is not this census's business.
        assert!(registry.is_alive(worker));
        // And the ordinary join path still retires it.
        assert!(registry.join(worker));
        assert!(!registry.is_alive(worker));
    }

    fn fold_obj(addr: usize) -> ObjectRef {
        // SAFETY: compared and rewritten by address only; never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn fold_addrs(snapshot: &[ObjectRef]) -> Vec<usize> {
        snapshot.iter().map(|r| r.as_ptr() as usize).collect()
    }

    /// gc-common w4-a (`handoff-w3g-blocked-fold-costs` edit 1): the reordered
    /// per-thread fold (snapshot pass first, lazily built chained set, seeds
    /// inserted after the compose) gives exactly the pre-w4 compose-then-seed
    /// result, including the overlap where a chained object moves ONTO the
    /// address a first-moving object vacates in the same map.
    #[test]
    fn the_blocked_fold_matches_compose_then_seed() {
        // Frames hold 0x100; an earlier pause moved it to 0x200 (the snapshot
        // holds the CUR value).
        let mut fixup = cratonvm_types::PointerMap::default();
        fixup.insert(0x100, 0x200);
        let mut snapshot = vec![fold_obj(0x200), fold_obj(0x300), fold_obj(0x400)];
        // This pause: the chained object 0x200 -> 0x300, a first-moving object
        // 0x300 -> 0x500, 0x400 stays.
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x200, 0x300);
        pm.insert(0x300, 0x500);
        let (composed, seeded) = fold_one_blocked_chain(&mut fixup, &mut snapshot, &pm);
        assert_eq!((composed, seeded), (1, 1));
        // Advanced through THIS map once -- not on through 0x300 -> 0x500.
        assert_eq!(fixup.get(&0x100), Some(&0x300));
        // The first move is seeded with its new address and not composed.
        assert_eq!(fixup.get(&0x300), Some(&0x500));
        assert_eq!(fixup.len(), 2);
        assert_eq!(fold_addrs(&snapshot), vec![0x300, 0x500, 0x400]);
    }

    /// The ABA guard survives the reorder: a recycled address that is already
    /// a chain KEY for an older object keeps that object's (composed) value.
    /// And a pause that moves none of the snapshot seeds nothing.
    #[test]
    fn the_blocked_fold_keeps_the_aba_guard_and_skips_unmoved_snapshots() {
        let mut fixup = cratonvm_types::PointerMap::default();
        fixup.insert(0x600, 0x700);
        let mut snapshot = vec![fold_obj(0x600)];
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x600, 0x800);
        pm.insert(0x700, 0x900);
        let (composed, seeded) = fold_one_blocked_chain(&mut fixup, &mut snapshot, &pm);
        assert_eq!((composed, seeded), (1, 1));
        assert_eq!(fixup.get(&0x600), Some(&0x900), "the older chain wins");
        assert_eq!(fold_addrs(&snapshot), vec![0x800]);

        // Nothing in the snapshot moves: only the chain composes.
        let mut fixup = cratonvm_types::PointerMap::default();
        fixup.insert(0x100, 0x200);
        let mut snapshot = vec![fold_obj(0x1000), fold_obj(0x2000)];
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x200, 0x250);
        assert_eq!(
            fold_one_blocked_chain(&mut fixup, &mut snapshot, &pm),
            (1, 0)
        );
        assert_eq!(fixup.get(&0x100), Some(&0x250));
        assert_eq!(fold_addrs(&snapshot), vec![0x1000, 0x2000]);
    }

    /// JIT round 17 wave 2 (lane smallfix): a dead entry no longer names the
    /// OS tid its thread had (the OS recycles it), so `os_tid_of` answers
    /// `None` and no claimant query can match it.
    #[test]
    fn mark_dead_withdraws_the_os_tid() {
        let registry = ThreadRegistry::new();
        let t = ThreadId(1);
        registry.register(t, "dying", None);
        registry.publish_os_tid_claim(t, 4243);
        assert_eq!(registry.os_tid_of(t), Some(4243));
        registry.mark_dead(t);
        if mark_dead_clears_os_tid() {
            assert_eq!(registry.os_tid_of(t), None);
        }
        assert_eq!(registry.thread_id_for_os_tid_inner(4243), None);
    }

    /// gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`):
    /// publishing the calling thread's OS tid also publishes its JIT deopt
    /// stash cells into the entry's `GcBlockState`, whichever order the
    /// owner installs the state and the tid in.
    #[test]
    fn gci_i9ds_the_os_tid_claim_publishes_the_deopt_stash_cells() {
        let own = current_os_tid();
        let cells = cratonvm_jit::deopt::stash_cell_addrs().expect("live TLS");
        // State first, tid second (the usual startup order).
        let registry = ThreadRegistry::new();
        let t = ThreadId(1);
        registry.register(t, "stash-owner", None);
        let state = Arc::new(GcBlockState::new());
        registry.set_gc_block_state(t, state.clone());
        assert_eq!(state.published_deopt_stash_cells(), None, "no tid yet");
        registry.set_os_tid_current(t);
        if own == 0 {
            // No OS-tid backend on this target: nothing is published.
            assert_eq!(state.published_deopt_stash_cells(), None);
            return;
        }
        assert_eq!(state.published_deopt_stash_cells(), Some((own, cells.0, cells.1)));
        // Tid first, state second: the owner's install republishes.
        let later = Arc::new(GcBlockState::new());
        registry.set_gc_block_state(t, later.clone());
        assert_eq!(later.published_deopt_stash_cells(), Some((own, cells.0, cells.1)));
        // Another OS thread's install does not publish ITS cells under `own`.
        let foreign = Arc::new(GcBlockState::new());
        std::thread::scope(|s| {
            s.spawn(|| registry.set_gc_block_state(t, foreign.clone()));
        });
        assert_eq!(foreign.published_deopt_stash_cells(), None);
        // A different triple replaces the old one whole.
        later.publish_deopt_stash_cells(own.wrapping_add(1).max(1), 0x1000, 0x2000);
        assert_eq!(
            later.published_deopt_stash_cells(),
            Some((own.wrapping_add(1).max(1), 0x1000, 0x2000))
        );
        // Zero words are refused (nothing half-published).
        let empty = GcBlockState::new();
        empty.publish_deopt_stash_cells(own, 0, 0x2000);
        empty.publish_deopt_stash_cells(0, 0x1000, 0x2000);
        assert_eq!(empty.published_deopt_stash_cells(), None);
    }

    /// gc-common 2026-09-23 (lane A) — the blocked-thread fold must never
    /// re-acquire `threads` while it holds it.
    ///
    /// `parking_lot::RwLock` is task-fair: a reader blocks while a writer is
    /// queued. The fold used to call `self.os_tid_of` (a second
    /// `threads.read()`) inside its walk, so a writer queued between the two
    /// acquisitions deadlocked the GC initiator against that writer. This
    /// drives exactly that interleaving — a fold loop over a BLOCKED entry on
    /// one thread, registry writes on another — and fails on a hang instead
    /// of hanging the suite. Probabilistic by nature, but the window is hit
    /// within a few thousand iterations on any multi-core machine.
    #[test]
    fn blocked_fold_never_reacquires_the_registry_lock() {
        let registry = Arc::new(ThreadRegistry::new());
        let blocked = ThreadId(1);
        registry.register(blocked, "blocked", None);
        {
            let threads = registry.threads.read();
            let entry = threads.get(&blocked).unwrap();
            entry
                .gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
            entry.os_tid.store(4242, Ordering::Release);
        }
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x1000, 0x2000);

        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let registry = registry.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // Any `threads.write()` will do; an unknown id keeps the
                    // registry contents unchanged.
                    registry.set_gc_block_state(ThreadId(u64::MAX), Arc::new(GcBlockState::new()));
                }
            })
        };
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let registry = registry.clone();
            std::thread::spawn(move || {
                for _ in 0..20_000 {
                    registry.fold_pointer_map_into_blocked(&pm);
                }
                let _ = tx.send(());
            });
        }
        let finished = rx.recv_timeout(std::time::Duration::from_secs(60)).is_ok();
        stop.store(true, Ordering::Relaxed);
        if finished {
            writer.join().unwrap();
        }
        assert!(
            finished,
            "fold_pointer_map_into_blocked deadlocked against a queued registry writer"
        );
    }

    /// gen r5w1/crash5: a blocked-peer native-stack capture is adopted only by
    /// the NEWEST claimant of the OS tid it was read from. Two alive, blocked
    /// entries claim one tid here -- a parked virtual thread's stale carrier
    /// claim and the continuation mounted there since -- and only the mounted
    /// one may later write the word back. Before the fix both adopted it, and
    /// the stale one wrote it from whichever carrier remounted it.
    #[test]
    fn a_native_stack_capture_is_adopted_only_by_the_newest_claimant_of_its_tid() {
        let registry = ThreadRegistry::new();
        let (stale, mounted, elsewhere) = (ThreadId(1), ThreadId(2), ThreadId(3));
        registry.register(stale, "parked-continuation", None);
        registry.register(mounted, "mounted-continuation", None);
        registry.register(elsewhere, "other-thread", None);
        // `stale` mounted on OS thread 4343 first, `mounted` after it.
        registry.publish_os_tid_claim(stale, 4343);
        registry.publish_os_tid_claim(mounted, 4343);
        registry.publish_os_tid_claim(elsewhere, 4344);
        {
            let threads = registry.threads.read();
            for id in [stale, mounted, elsewhere] {
                threads
                    .get(&id)
                    .unwrap()
                    .gc_block_state
                    .in_blocked_region
                    .store(true, Ordering::Release);
            }
        }
        // This test thread's unbound pause ledger stands in for a pause's.
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc::gc_quiescence::record_peer_stack_slot(4343, 0x7a00_1000, 0x1000);
        cratonvm_gc::gc_quiescence::record_peer_stack_slot(4344, 0x7b00_2000, 0x3000);
        let declined_before = NATIVE_SLOTS_DECLINED_STALE_CLAIM.load(Ordering::Relaxed);
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x1000, 0x2000);
        pm.insert(0x3000, 0x4000);
        registry.fold_pointer_map_into_blocked(&pm);

        let slots_of = |id: ThreadId| -> Vec<(usize, usize, usize)> {
            registry
                .threads
                .read()
                .get(&id)
                .unwrap()
                .gc_block_state
                .native_slots
                .lock()
                .iter()
                .map(|n| (n.addr, n.orig, n.cur))
                .collect()
        };
        if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
            // `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP` set for this process:
            // nothing is captured or adopted, so there is nothing to route.
            assert!(slots_of(mounted).is_empty());
            return;
        }
        assert!(
            slots_of(stale).is_empty(),
            "a stale claimant adopts nothing of the stack it no longer runs on"
        );
        assert_eq!(slots_of(mounted), vec![(0x7a00_1000, 0x1000, 0x2000)]);
        assert_eq!(
            slots_of(elsewhere),
            vec![(0x7b00_2000, 0x3000, 0x4000)],
            "a sole claimant adopts its own capture exactly as before"
        );
        assert!(NATIVE_SLOTS_DECLINED_STALE_CLAIM.load(Ordering::Relaxed) > declined_before);
    }

    /// t1 wave 5 lane E2 (hazard 2 of i94 stage 2): while a continuation slice
    /// runs on its carrier's OS thread, a capture at or above the published
    /// slice base is adopted by the carrier (whose tid claim is withdrawn for
    /// the slice), below it by the continuation (the claimant) -- never both.
    #[test]
    fn t1e2_a_slice_stack_capture_is_routed_by_the_segment_base() {
        let registry = ThreadRegistry::new();
        let (carrier, cont) = (ThreadId(11_4646), ThreadId(12_4646));
        let os_tid = 46_4646u32;
        registry.register(carrier, "carrier", None);
        registry.register(cont, "continuation", None);
        registry.publish_os_tid_claim(carrier, os_tid);
        registry.clear_os_tid(carrier);
        registry.publish_os_tid_claim(cont, os_tid);
        {
            let threads = registry.threads.read();
            for id in [carrier, cont] {
                threads
                    .get(&id)
                    .unwrap()
                    .gc_block_state
                    .in_blocked_region
                    .store(true, Ordering::Release);
            }
        }
        let prev = cratonvm_gc::shadow_stack::publish_stack_segment(os_tid, 0x7c00_2000, carrier.0);
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc::gc_quiescence::record_peer_stack_slot(os_tid, 0x7c00_1000, 0x1000);
        cratonvm_gc::gc_quiescence::record_peer_stack_slot(os_tid, 0x7c00_3000, 0x3000);
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x1000, 0x2000);
        pm.insert(0x3000, 0x4000);
        registry.fold_pointer_map_into_blocked(&pm);
        cratonvm_gc::shadow_stack::restore_stack_segment(os_tid, prev);
        let slots_of = |id: ThreadId| -> Vec<(usize, usize, usize)> {
            registry
                .threads
                .read()
                .get(&id)
                .unwrap()
                .gc_block_state
                .native_slots
                .lock()
                .iter()
                .map(|n| (n.addr, n.orig, n.cur))
                .collect()
        };
        if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
            assert!(slots_of(cont).is_empty() && slots_of(carrier).is_empty());
            return;
        }
        assert_eq!(slots_of(cont), vec![(0x7c00_1000, 0x1000, 0x2000)], "the slice's half");
        assert_eq!(
            slots_of(carrier),
            vec![(0x7c00_3000, 0x3000, 0x4000)],
            "the carrier's half, adopted by the carrier alone"
        );
    }

    /// gen r5w2/roots6 (`gengc-r5w1-crash5-blocked-native-slot-captures-grow-
    /// every-cycle`, item 1): a long-parked thread's stack word re-captured
    /// with the SAME value -- by another barrier round of the same pause, or by
    /// the next pause -- is adopted once, and keeps following its object
    /// through every later map. A word whose value changed is a new capture.
    #[test]
    fn a_recaptured_native_stack_word_is_adopted_once() {
        let registry = ThreadRegistry::new();
        let parked = ThreadId(1);
        registry.register(parked, "long-parked", None);
        registry.publish_os_tid_claim(parked, 4545);
        registry
            .threads
            .read()
            .get(&parked)
            .unwrap()
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        let slots = || -> Vec<(usize, usize, usize)> {
            registry
                .threads
                .read()
                .get(&parked)
                .unwrap()
                .gc_block_state
                .native_slots
                .lock()
                .iter()
                .map(|n| (n.addr, n.orig, n.cur))
                .collect()
        };
        let dup_before = native_slot_duplicate_captures();
        // Pause 1, two barrier rounds reading the same word.
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        cratonvm_gc::gc_quiescence::record_peer_stack_slot(4545, 0x7c00_1000, 0x5000);
        cratonvm_gc::gc_quiescence::record_peer_stack_slot(4545, 0x7c00_1000, 0x5000);
        let mut first = cratonvm_types::PointerMap::default();
        first.insert(0x5000, 0x6000);
        registry.fold_pointer_map_into_blocked(&first);
        if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
            assert!(slots().is_empty());
            return;
        }
        assert_eq!(slots(), vec![(0x7c00_1000, 0x5000, 0x6000)]);
        // Pause 2: still parked, so the word still reads 0x5000; beside it a
        // word whose value changed since.
        cratonvm_gc::gc_quiescence::record_peer_stack_slot(4545, 0x7c00_1000, 0x5000);
        cratonvm_gc::gc_quiescence::record_peer_stack_slot(4545, 0x7c00_1008, 0x7000);
        let mut second = cratonvm_types::PointerMap::default();
        second.insert(0x6000, 0x8000);
        second.insert(0x7000, 0x9000);
        registry.fold_pointer_map_into_blocked(&second);
        assert_eq!(
            slots(),
            vec![(0x7c00_1000, 0x5000, 0x8000), (0x7c00_1008, 0x7000, 0x9000)],
            "one entry per (address, value), each composed through both maps"
        );
        assert!(native_slot_duplicate_captures() >= dup_before + 2);
    }

    /// gen r5w2/roots6 (`gengc-r5w1-crash5-parked-virtual-threads-keep-their-
    /// carriers-os-tid`): an UNMOUNTED virtual thread still carries its last
    /// carrier's OS tid, but it is not blocked on that carrier (the
    /// helper-window pass must not signal it for its sake) and not frozen on
    /// it (a take-over must not walk its heap frames as the frozen stack's).
    #[test]
    fn an_unmounted_virtual_thread_is_neither_blocked_nor_frozen_on_its_last_carrier() {
        let registry = ThreadRegistry::new();
        let (parked, mounted) = (ThreadId(1), ThreadId(2));
        registry.register(parked, "vt-parked", None);
        registry.register(mounted, "vt-mounted", None);
        registry.publish_os_tid_claim(parked, 4646);
        registry.publish_os_tid_claim(mounted, 4646);
        {
            let threads = registry.threads.read();
            for id in [parked, mounted] {
                threads
                    .get(&id)
                    .unwrap()
                    .gc_block_state
                    .in_blocked_region
                    .store(true, Ordering::Release);
            }
        }
        registry.set_jvm_thread_addr(parked, 0x1000);
        registry.set_jvm_thread_addr(mounted, 0x2000);
        assert!(registry.is_mounted(parked), "an entry starts mounted");
        registry.set_mounted(parked, false);
        assert!(!registry.is_mounted(parked));
        assert_eq!(
            registry.blocked_os_tids(),
            vec![4646],
            "only the mounted one"
        );
        assert_eq!(registry.frozen_peer_thread_addrs(&[4646]), vec![0x2000]);
        registry.set_mounted(mounted, false);
        assert!(registry.blocked_os_tids().is_empty());
        assert!(registry.frozen_peer_thread_addrs(&[4646]).is_empty());
        registry.set_mounted(parked, true);
        assert_eq!(
            registry.frozen_peer_thread_addrs(&[4646]),
            vec![0x1000],
            "remounted"
        );
    }

    /// JIT round 16 wave 9b (lane ffm,
    /// `r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native`): a
    /// thread blocked in foreign code is left out of the OS tids a pause
    /// signals (the helper-window list, its monitor-proof split, the take-over
    /// roster) but stays in the alive count; foreign code that is not (yet) in
    /// its region, or a region that is not foreign, is signalled as before.
    #[test]
    fn a_thread_in_foreign_code_is_not_signalled_but_still_counted() {
        let registry = ThreadRegistry::new();
        let (ffm, sleeper) = (ThreadId(1), ThreadId(2));
        registry.register(ffm, "ffm", None);
        registry.register(sleeper, "sleeper", None);
        registry.publish_os_tid_claim(ffm, 5151);
        registry.publish_os_tid_claim(sleeper, 5252);
        registry.mark_stw_ready(ffm);
        registry.mark_stw_ready(sleeper);
        let state = |id: ThreadId| {
            registry
                .threads
                .read()
                .get(&id)
                .unwrap()
                .gc_block_state
                .clone()
        };
        for id in [ffm, sleeper] {
            state(id).in_blocked_region.store(true, Ordering::Release);
        }
        let sorted = |mut v: Vec<u32>| {
            v.sort_unstable();
            v
        };
        assert_eq!(sorted(registry.blocked_os_tids()), vec![5151, 5252]);
        state(ffm).foreign_native.store(true, Ordering::Release);
        assert_eq!(
            registry.blocked_os_tids(),
            vec![5252],
            "the foreign one is exempt"
        );
        let (rest, proven) = registry.blocked_os_tids_split_by_monitor_proof();
        assert_eq!((rest, proven), (vec![5252], Vec::new()));
        let (alive, roster) = registry.alive_count_and_os_tids();
        assert_eq!(alive, 2, "still counted alive");
        assert_eq!(roster, vec![5252]);
        // Raised before the deposit, region not up yet: a running mutator.
        state(ffm).in_blocked_region.store(false, Ordering::Release);
        assert_eq!(
            sorted(registry.alive_count_and_os_tids().1),
            vec![5151, 5252]
        );
        state(ffm).foreign_native.store(false, Ordering::Release);
        state(ffm).in_blocked_region.store(true, Ordering::Release);
        assert_eq!(sorted(registry.blocked_os_tids()), vec![5151, 5252]);
    }

    /// gci i2/jn (i1 review R5): `CRATONVM_FFM_IN_NATIVE_NO_SIGNAL` is read
    /// once, into the registry, and all three roster reads use that value:
    /// default on; with it off (`=0`, here the field) a thread in foreign code
    /// is listed and signalled again by every one of them.
    #[test]
    fn gci_i2jn_ffm_no_signal_is_latched_per_registry() {
        assert!(ThreadRegistry::new().ffm_in_native_no_signal, "default on");
        let mut registry = ThreadRegistry::new();
        registry.ffm_in_native_no_signal = false;
        let (ffm, sleeper) = (ThreadId(1), ThreadId(2));
        registry.register(ffm, "ffm", None);
        registry.register(sleeper, "sleeper", None);
        registry.publish_os_tid_claim(ffm, 5151);
        registry.publish_os_tid_claim(sleeper, 5252);
        registry.mark_stw_ready(ffm);
        registry.mark_stw_ready(sleeper);
        for id in [ffm, sleeper] {
            let threads = registry.threads.read();
            let block = &threads.get(&id).unwrap().gc_block_state;
            block.in_blocked_region.store(true, Ordering::Release);
            if id == ffm {
                block.foreign_native.store(true, Ordering::Release);
            }
        }
        let sorted = |mut v: Vec<u32>| {
            v.sort_unstable();
            v
        };
        assert_eq!(sorted(registry.blocked_os_tids()), vec![5151, 5252]);
        assert_eq!(sorted(registry.blocked_os_tids_split_by_monitor_proof().0), vec![5151, 5252]);
        let (alive, roster) = registry.alive_count_and_os_tids();
        assert_eq!((alive, sorted(roster)), (2, vec![5151, 5252]));
    }

    /// gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`): a JNI in-native
    /// thread's band is read only while its region is up (whatever its
    /// foreign-native mark says) and the band passes the bounds; each band
    /// once; every word the probe accepts is pushed, in band order.
    #[test]
    fn gch_h4bp_in_native_bands_are_read_only_inside_the_bracket() {
        let registry = ThreadRegistry::new();
        let (a, b) = (ThreadId(1), ThreadId(2));
        registry.register(a, "jni", None);
        registry.register(b, "other", None);
        let state = |id: ThreadId| {
            registry
                .threads
                .read()
                .get(&id)
                .unwrap()
                .gc_block_state
                .clone()
        };
        let set_band = |s: &GcBlockState, lo: usize, hi: usize| {
            s.in_native_band_lo.store(lo, Ordering::Relaxed);
            s.in_native_band_hi.store(hi, Ordering::Relaxed);
        };
        // The "stack": a local array, words 0x5150 / 0x5158 standing for objects.
        let band: [usize; 8] = [0, 0x5150, 7, 0x5150, 0x5158, 0, 1, 0x5150];
        let lo = std::hint::black_box(&band) as *const [usize; 8] as usize;
        let hi = lo + std::mem::size_of::<[usize; 8]>();
        let probe = |w: usize| (w == 0x5150 || w == 0x5158).then(|| fake_synchronizer(w));
        let mut out = Vec::new();
        let none = InNativeBandScan::default();
        assert_eq!(
            registry.scan_in_native_bands(&probe, &mut out),
            none,
            "nobody in native"
        );
        let (sa, sb) = (state(a), state(b));
        set_band(&*sa, lo, hi);
        assert_eq!(
            registry.scan_in_native_bands(&probe, &mut out),
            none,
            "a band alone"
        );
        sa.foreign_native.store(true, Ordering::Release);
        assert_eq!(
            registry.scan_in_native_bands(&probe, &mut out),
            none,
            "in C, region not up yet"
        );
        sa.in_blocked_region.store(true, Ordering::Release);
        let scan = registry.scan_in_native_bands(&probe, &mut out);
        assert_eq!(
            scan,
            InNativeBandScan {
                threads: 1,
                words: 8,
                roots: 4
            }
        );
        assert_eq!(
            out.iter().map(|o| o.as_ptr() as usize).collect::<Vec<_>>(),
            vec![0x5150, 0x5150, 0x5158, 0x5150]
        );
        // gch h4/bp (vh4-bp B on h4a): the leave lowers `foreign_native` FIRST
        // and then waits out the pause with the region still up; the band must
        // still be read there.
        sa.foreign_native.store(false, Ordering::Release);
        out.clear();
        assert_eq!(
            registry.scan_in_native_bands(&probe, &mut out).threads,
            1,
            "waiting at the leave: the mark is down, the region is up"
        );
        assert_eq!(out.len(), 4);
        // A second entry on the same band (never expected) is not read twice.
        set_band(&*sb, lo, hi);
        sb.in_blocked_region.store(true, Ordering::Release);
        sb.foreign_native.store(true, Ordering::Release);
        out.clear();
        assert_eq!(registry.scan_in_native_bands(&probe, &mut out).threads, 1);
        assert_eq!(out.len(), 4);
        // A band that fails the bounds is never read; a withdrawn one neither.
        set_band(&*sb, lo + 4, hi);
        set_band(&*sa, 0, 0);
        out.clear();
        assert_eq!(registry.scan_in_native_bands(&probe, &mut out), none);
        assert!(out.is_empty());
        std::hint::black_box(&band);
    }

    /// gcj j2/jn2 (`CRATONVM_JNI_IN_NATIVE_KEEP_TLAB`, cut 2 of the JNI
    /// transition ledger): the pause retires a kept TLAB only while its owner
    /// is in native (region up) with the mark up, exactly once (the swap), and
    /// leaves a mark whose region is down to the owner's leave; a retired TLAB
    /// publishes no reserved tail.
    #[test]
    fn gcj2jn2_a_kept_in_native_tlab_is_retired_once_and_only_inside_the_bracket() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(1);
        registry.register(tid, "jni", None);
        let state = registry
            .threads
            .read()
            .get(&tid)
            .unwrap()
            .gc_block_state
            .clone();
        let mut backing = vec![0u64; 128];
        let ptr = backing.as_mut_ptr() as *mut u8;
        let size = backing.len() * std::mem::size_of::<u64>();
        let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
        assert!(tlab.alloc(64, 8).is_some());
        let p = std::ptr::addr_of_mut!(tlab);
        registry.set_tlab_addr(tid, p as usize);
        // SAFETY (every read below): `p` is the live local `Tlab`, read only
        // between the registry's calls.
        let tail = || unsafe { (*p).reserved_tail() };
        assert_eq!(tail(), Some((ptr as usize + 64, ptr as usize + size)));
        // No mark: never retired, region up or not.
        assert_eq!(registry.retire_in_native_kept_tlabs(), 0);
        state.in_blocked_region.store(true, Ordering::Release);
        assert_eq!(registry.retire_in_native_kept_tlabs(), 0);
        assert!(tail().is_some());
        // The mark with the region down (the owner left native): the owner's.
        state.in_blocked_region.store(false, Ordering::Release);
        state.in_native_tlab_kept.store(true, Ordering::Release);
        assert_eq!(registry.retire_in_native_kept_tlabs(), 0);
        assert!(state.in_native_tlab_kept.load(Ordering::Acquire), "left to the leave");
        assert!(tail().is_some());
        // Both up: retired once, the mark taken.
        state.in_blocked_region.store(true, Ordering::Release);
        assert_eq!(registry.retire_in_native_kept_tlabs(), 1);
        assert!(!state.in_native_tlab_kept.load(Ordering::Acquire));
        assert_eq!(tail(), None, "retired");
        assert!(registry.collect_reserved_tlab_tails().is_empty());
        assert_eq!(registry.retire_in_native_kept_tlabs(), 0, "once");
        state.in_blocked_region.store(false, Ordering::Release);
        std::hint::black_box(&backing);
    }

    /// gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`): the pause
    /// credits a proven in-native band once per pause stamp, only inside the
    /// bracket, only with a proven depth equal to the published one, never a
    /// second band on one address range; its young words reach the ledger raw
    /// and in band order; the uncredited count follows every case.
    #[test]
    fn gci_i24bp_proven_in_native_bands_are_credited_once_per_pause() {
        let mut registry = ThreadRegistry::new();
        // The registry's exemption (`CRATONVM_FFM_IN_NATIVE_NO_SIGNAL`, default
        // ON) whatever this process's environment says.
        registry.ffm_in_native_no_signal = true;
        let (a, b) = (ThreadId(1), ThreadId(2));
        registry.register(a, "jni", None);
        registry.register(b, "jni2", None);
        let state = |id: ThreadId| {
            registry
                .threads
                .read()
                .get(&id)
                .unwrap()
                .gc_block_state
                .clone()
        };
        let set_os_tid = |id: ThreadId, t: u32| {
            registry
                .threads
                .read()
                .get(&id)
                .unwrap()
                .os_tid
                .store(t, Ordering::Release);
        };
        set_os_tid(a, 9101);
        set_os_tid(b, 9102);
        let set_band = |s: &GcBlockState, lo: usize, hi: usize| {
            s.in_native_band_lo.store(lo, Ordering::Relaxed);
            s.in_native_band_hi.store(hi, Ordering::Relaxed);
        };
        // The "stack": three words in the young range, three outside it.
        let band: [usize; 6] = [0x7000_0010, 3, 0x7000_0020, 0, 0x6000_0000, 0x7000_0018];
        let lo = std::hint::black_box(&band) as *const [usize; 6] as usize;
        let hi = lo + std::mem::size_of::<[usize; 6]>();
        let young = |w: usize| (0x7000_0000..0x7000_1000).contains(&w);
        let published = |t: u32| -> Option<usize> {
            match t {
                9101 => Some(3),
                9102 => Some(2),
                _ => None,
            }
        };
        let run = |stamp: u64| {
            let mut got: Vec<(Vec<usize>, usize)> = Vec::new();
            let done = registry.credit_in_native_proven_bands(
                stamp,
                &published,
                &young,
                &mut |w: &[usize], d: usize| got.push((w.to_vec(), d)),
            );
            (done, got)
        };
        let none = InNativeCredit::default();
        let (sa, sb) = (state(a), state(b));
        set_band(&*sa, lo, hi);
        assert_eq!(run(5), (none, Vec::new()), "a band with the region down");
        sa.in_blocked_region.store(true, Ordering::Release);
        sa.in_native_proven_jit_depth.store(3, Ordering::Release);
        assert_eq!(
            run(5).0,
            InNativeCredit {
                not_exempt: 1,
                ..none
            },
            "the mark down (a leave waiting out the pause): a helper window's, not ours"
        );
        assert_eq!(registry.uncredited_in_native_bands(5), 1);
        sa.foreign_native.store(true, Ordering::Release);
        sa.in_native_proven_jit_depth.store(0, Ordering::Release);
        assert_eq!(
            run(5).0,
            InNativeCredit {
                unproven: 1,
                ..none
            },
            "no proven depth"
        );
        assert_eq!(registry.uncredited_in_native_bands(5), 1);
        sa.in_native_proven_jit_depth.store(3, Ordering::Release);
        assert_eq!(run(0), (none, Vec::new()), "no pause stamp: nothing");
        let (done, got) = run(5);
        assert_eq!(
            done,
            InNativeCredit {
                credited: 1,
                depth: 3,
                words: 3,
                ..none
            }
        );
        assert_eq!(got, vec![(vec![0x7000_0010, 0x7000_0020, 0x7000_0018], 3)]);
        assert_eq!(registry.uncredited_in_native_bands(5), 0, "credited for 5");
        assert_eq!(registry.uncredited_in_native_bands(6), 1, "not for the next pause");
        assert_eq!(registry.uncredited_in_native_bands(0), 1, "stamp 0 counts every band");
        assert_eq!(
            run(5),
            (
                InNativeCredit {
                    already: 1,
                    ..none
                },
                Vec::new()
            ),
            "a second gathering in the same pause credits nothing again"
        );
        // A proof that does not match the published depth is never credited.
        sa.in_native_proven_jit_depth.store(4, Ordering::Release);
        assert_eq!(
            run(6).0,
            InNativeCredit {
                mismatched: 1,
                ..none
            }
        );
        assert_eq!(registry.uncredited_in_native_bands(6), 1);
        // Two entries on one band: the first is credited, the second is not.
        sa.in_native_proven_jit_depth.store(3, Ordering::Release);
        set_band(&*sb, lo, hi);
        sb.in_native_proven_jit_depth.store(2, Ordering::Release);
        sb.foreign_native.store(true, Ordering::Release);
        sb.in_blocked_region.store(true, Ordering::Release);
        let (done, got) = run(7);
        assert_eq!((done.credited, done.mismatched), (1, 1));
        assert_eq!(got.len(), 1);
        assert_eq!(registry.uncredited_in_native_bands(7), 1);
        std::hint::black_box(&band);
    }

    /// gci i24/bp: the credit rule (pure).
    #[test]
    fn gci_i24bp_in_native_credit_rule() {
        assert!(in_native_credit_admissible(3, 9101, Some(3), false));
        assert!(!in_native_credit_admissible(0, 9101, Some(0), false), "nothing proven");
        assert!(!in_native_credit_admissible(3, 0, Some(3), false), "unknown OS thread");
        assert!(!in_native_credit_admissible(3, 9101, None, false), "no published depth");
        assert!(!in_native_credit_admissible(3, 9101, Some(2), false), "an over-credit");
        assert!(!in_native_credit_admissible(2, 9101, Some(3), false), "a short proof");
        assert!(!in_native_credit_admissible(3, 9101, Some(3), true), "a duplicate");
    }

    /// JIT round 16 wave 14 (lane ffm3): only a thread in a lazy in-native
    /// downcall is claimed, once, with its band; a claimed thread's leave
    /// waits for the release; a tid already frozen by the take-over, an
    /// unpublished `JvmThread` and an unmounted entry are left alone.
    #[test]
    fn only_threads_in_a_lazy_downcall_are_claimed() {
        use crate::threading::jvm_thread::{LAZY_NATIVE_CLAIMED, LAZY_NATIVE_IN};
        let registry = ThreadRegistry::new();
        let (a, b, c, d) = (ThreadId(1), ThreadId(2), ThreadId(3), ThreadId(4));
        for (id, tid) in [(a, 7101u32), (b, 7102), (c, 7103), (d, 7104)] {
            registry.register(id, "t", None);
            registry.publish_os_tid_claim(id, tid);
            registry.mark_stw_ready(id);
            registry.set_jvm_thread_addr(id, 0x1000 * tid as usize);
        }
        let state = |id: ThreadId| {
            registry
                .threads
                .read()
                .get(&id)
                .map(|e| e.gc_block_state.clone())
                .unwrap_or_default()
        };
        assert!(
            registry.claim_lazy_native_peers(&[]).is_empty(),
            "nobody in C"
        );
        state(a).enter_lazy_native(0x10_000, 0x20_000);
        state(b).enter_lazy_native(0x30_000, 0x40_000);
        state(c).enter_lazy_native(0x50_000, 0x60_000);
        registry.set_mounted(c, false);
        // `b` was already frozen by the take-over this pause.
        let claims = registry.claim_lazy_native_peers(&[7102]);
        assert_eq!(claims.len(), 1);
        assert_eq!(
            (claims[0].os_tid, claims[0].lo, claims[0].hi),
            (7101, 0x10_000, 0x20_000)
        );
        assert_eq!(
            state(a).lazy_native.load(Ordering::Acquire),
            LAZY_NATIVE_CLAIMED
        );
        assert_eq!(state(b).lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert_eq!(state(c).lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert!(
            registry
                .claim_lazy_native_peers(&[])
                .iter()
                .all(|c| c.os_tid != 7101),
            "a claimed thread is not claimed twice"
        );
        assert!(claims[0].state.release_lazy_native_claim());
        assert_eq!(state(a).lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert!(!state(a).leave_lazy_native());
    }

    /// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-1): a frame-trace
    /// pause can claim peers with either interpreted or compiled activations;
    /// trace publication reads the held owner's published state.
    #[test]
    fn r19_frame_trace_claim_admits_compiled_callers() {
        use crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED;
        let registry = ThreadRegistry::new();
        let (a, b) = (ThreadId(1), ThreadId(2));
        for (id, tid) in [(a, 7201u32), (b, 7202)] {
            registry.register(id, "t", None);
            registry.publish_os_tid_claim(id, tid);
            registry.mark_stw_ready(id);
            registry.set_jvm_thread_addr(id, 0x1000 * tid as usize);
        }
        let state = |id: ThreadId| {
            registry
                .threads
                .read()
                .get(&id)
                .map(|e| e.gc_block_state.clone())
                .unwrap_or_default()
        };
        state(a).enter_lazy_native(0x10_000, 0x20_000);
        state(b).enter_lazy_native(0x30_000, 0x40_000);
        let claims = registry.claim_lazy_native_peers(&[]);
        assert_eq!(
            claims.iter().map(|c| c.os_tid).collect::<Vec<_>>(),
            vec![7201, 7202]
        );
        assert!(claims
            .iter()
            .all(|c| c.state.lazy_native.load(Ordering::Acquire) == LAZY_NATIVE_CLAIMED));
        for claim in &claims {
            assert!(claim.state.release_lazy_native_claim());
        }
    }

    /// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-4): an OS tid counts as
    /// blocked only when every live entry on it is blocked (a mounted virtual
    /// thread shares its carrier's tid).
    #[test]
    fn r19_eintr19_a_tid_is_blocked_only_when_every_entry_on_it_is() {
        let registry = ThreadRegistry::new();
        let (carrier, mounted, other) = (ThreadId(1), ThreadId(2), ThreadId(3));
        for (id, tid) in [(carrier, 7301u32), (mounted, 7301), (other, 7302)] {
            registry.register(id, "t", None);
            registry.publish_os_tid_claim(id, tid);
            registry.mark_stw_ready(id);
        }
        let state = |id: ThreadId| {
            registry
                .threads
                .read()
                .get(&id)
                .map(|e| e.gc_block_state.clone())
                .unwrap_or_default()
        };
        assert!(registry.blocked_among_os_tids(&[]).is_empty());
        assert!(registry.blocked_among_os_tids(&[7301, 7302]).is_empty());
        state(carrier)
            .in_blocked_region
            .store(true, Ordering::Release);
        state(other)
            .in_blocked_region
            .store(true, Ordering::Release);
        assert_eq!(registry.blocked_among_os_tids(&[7301, 7302]), vec![7302]);
        state(mounted)
            .in_blocked_region
            .store(true, Ordering::Release);
        let mut both = registry.blocked_among_os_tids(&[7301, 7302]);
        both.sort_unstable();
        assert_eq!(both, vec![7301, 7302]);
        assert_eq!(registry.blocked_among_os_tids(&[7302]), vec![7302]);
    }

    /// gcj j2/cm2 (j1-cm registry residuals item 6): the sorted form answers
    /// in ascending order, each tid once, whatever the order and repeats of
    /// the asked tids; an unasked or unpublished tid is never answered.
    #[test]
    fn gcj2cm2_blocked_among_os_tids_is_sorted_and_deduplicated() {
        let registry = ThreadRegistry::new();
        let rows = [
            (ThreadId(1), 7403u32, true),
            (ThreadId(2), 7401, true),
            (ThreadId(3), 7402, false),
            (ThreadId(4), 7401, true),
            (ThreadId(5), 7404, true),
        ];
        for (id, tid, _) in rows {
            registry.register(id, "t", None);
            registry.publish_os_tid_claim(id, tid);
            registry.mark_stw_ready(id);
        }
        for (id, _, blocked) in rows {
            let state = registry
                .threads
                .read()
                .get(&id)
                .map(|e| e.gc_block_state.clone())
                .unwrap_or_default();
            state.in_blocked_region.store(blocked, Ordering::Release);
        }
        assert_eq!(
            registry.blocked_among_os_tids(&[7403, 7402, 7401, 7401, 9999]),
            vec![7401, 7403]
        );
        assert_eq!(registry.blocked_among_os_tids(&[7404]), vec![7404]);
        assert!(registry.blocked_among_os_tids(&[7402]).is_empty());
    }

    /// gcj j2/cm2 (j1-cm registry residuals item 8):
    /// `CRATONVM_DBG_STW_EXPECTED_IDS` is latched per registry (per VM), by
    /// presence, as the two readers tested it before.
    #[test]
    fn gcj2cm2_the_expected_ids_diagnostic_is_latched_per_registry() {
        let latched = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_DBG_STW_EXPECTED_IDS", v)],
                || ThreadRegistry::new().dbg_stw_expected_ids(),
            )
        };
        assert!(!latched(None));
        assert!(latched(Some("1")));
        let registry = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DBG_STW_EXPECTED_IDS", Some("1"))],
            ThreadRegistry::new,
        );
        // The override is gone; the registry keeps what it read.
        assert!(registry.dbg_stw_expected_ids());
    }

    /// Interpreter round i2 wave 58, lane L6: a handshake pause excuses only
    /// counted threads raised in a running native -- never the initiator (by
    /// id or by OS thread), a blocked one or an unmounted one -- once, and a
    /// frame-trace pause only a listable one. An excused thread's lowering
    /// waits for the release.
    #[test]
    fn a_handshake_census_excuses_counted_threads_in_a_running_native() {
        use crate::threading::jvm_thread::{NativeExcuse, NX_EXCUSED};
        let registry = ThreadRegistry::new();
        let ids = [
            ThreadId(1),
            ThreadId(2),
            ThreadId(3),
            ThreadId(4),
            ThreadId(5),
        ];
        for (i, id) in ids.iter().enumerate() {
            registry.register(*id, "t", None);
            registry.publish_os_tid_claim(*id, 8101 + i as u32);
            registry.mark_stw_ready(*id);
        }
        let state = |id: ThreadId| {
            registry
                .threads
                .read()
                .get(&id)
                .map(|e| e.gc_block_state.clone())
                .unwrap_or_default()
        };
        let listable = NativeExcuse::raised(true, 4);
        let opaque = NativeExcuse::raised(false, 4);
        for id in ids {
            state(id).native_excuse.raise(listable);
        }
        state(ThreadId(2)).native_excuse.lower(listable);
        state(ThreadId(2)).native_excuse.raise(opaque);
        state(ThreadId(3))
            .in_blocked_region
            .store(true, Ordering::Release);
        registry.set_mounted(ThreadId(4), false);
        // Initiator 1; 5 runs on the calling OS thread.
        let trace = registry.excuse_running_natives_on(ThreadId(1), 8105, true);
        assert!(
            trace.is_empty(),
            "only the opaque 2 is left, and it is not listable"
        );
        let loop_exit = registry.excuse_running_natives_on(ThreadId(1), 8105, false);
        assert_eq!(loop_exit.len(), 1);
        assert_eq!((loop_exit[0].tid, loop_exit[0].os_tid), (ThreadId(2), 8102));
        assert_eq!(loop_exit[0].word, opaque);
        assert_eq!(state(ThreadId(2)).native_excuse.load(), opaque | NX_EXCUSED);
        assert!(
            registry
                .excuse_running_natives_on(ThreadId(1), 8105, false)
                .is_empty(),
            "excused once"
        );
        // The thread's lowering waits for the release.
        let s2 = state(ThreadId(2));
        let waiter = std::thread::spawn(move || s2.native_excuse.lower(opaque));
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_millis(30) {
            std::thread::yield_now();
        }
        assert_eq!(state(ThreadId(2)).native_excuse.load(), opaque | NX_EXCUSED);
        loop_exit[0].state.native_excuse.release();
        assert!(waiter.join().unwrap(), "it waited");
        assert_eq!(state(ThreadId(2)).native_excuse.load(), 0);
        // A listable thread, with no initiator in the registry.
        let trace = registry.excuse_running_natives_on(ThreadId(u64::MAX), 0, true);
        let mut tids: Vec<u64> = trace.iter().map(|x| x.tid.0).collect();
        tids.sort_unstable();
        assert_eq!(tids, vec![1, 5]);
    }

    /// gen r5w2/roots6 (`gengc-r5w1-crash5-an-unwinding-carrier-publishes-a-
    /// dropped-jvmthread`): once the `JvmThread` whose `Tlab` an entry
    /// publishes is DROPPED -- as an unwinding carrier's closure drops it,
    /// with the entry still alive and the address still published -- no
    /// collector reader dereferences it: `collect_reserved_tlab_tails` reports
    /// no tail and `frozen_peer_thread_addrs` no address.
    #[test]
    fn a_dropped_jvm_thread_publishes_nothing_to_the_collector() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(7);
        registry.register(tid, "unwinding", None);
        registry.publish_os_tid_claim(tid, 4747);
        let mut backing = vec![0u64; 128];
        let ptr = backing.as_mut_ptr() as *mut u8;
        let size = backing.len() * std::mem::size_of::<u64>();
        // The published `Tlab` stands in for the dropped frame's: it stays
        // readable here, so a reader that ignored the flag would report it.
        // SAFETY: `backing` outlives `tlab` and is `size` bytes.
        let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
        assert!(tlab.alloc(64, 8).is_some());
        {
            let thread = crate::threading::jvm_thread::JvmThread::new(tid, "unwinding");
            registry.set_gc_block_state(tid, thread.gc_block_state.clone());
            registry.set_tlab_addr(tid, &tlab as *const cratonvm_gc::Tlab as usize);
            registry.set_jvm_thread_addr(tid, &thread as *const _ as usize);
            assert_eq!(
                registry.collect_reserved_tlab_tails(),
                vec![(ptr as usize + 64, ptr as usize + size)],
                "live: the tail is published"
            );
            assert_eq!(registry.frozen_peer_thread_addrs(&[4747]).len(), 1);
            // `thread` drops here, as the unwind drops it.
        }
        assert!(
            registry.collect_reserved_tlab_tails().is_empty(),
            "dropped: nothing"
        );
        assert!(registry.frozen_peer_thread_addrs(&[4747]).is_empty());
        tlab.retire();
        drop(backing);
    }

    fn publish_os_tid(registry: &ThreadRegistry, tid: ThreadId, os_tid: u32) {
        registry
            .threads
            .read()
            .get(&tid)
            .expect("registered")
            .os_tid
            .store(os_tid, Ordering::Release);
    }

    /// gc-common w3-a — the census says whether the initiator owns a counted
    /// slot, by its own id or by OS thread, and says "unknown" whenever a
    /// counted entry could be the caller.
    #[test]
    fn the_census_reports_whether_the_initiator_is_counted() {
        let registry = ThreadRegistry::new();
        registry.register(ThreadId(0), "main", None);
        registry.register(ThreadId(1), "worker", None);
        publish_os_tid(&registry, ThreadId(0), 100);
        publish_os_tid(&registry, ThreadId(1), 101);

        // An unregistered caller on a third OS thread: provably not counted.
        let (alive, blocked, os, blocked_tids, verdict) =
            registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 555);
        assert_eq!((alive, blocked), (2, 0));
        assert_eq!(os.len(), 2);
        assert!(blocked_tids.is_empty());
        assert_eq!(
            verdict,
            Some(false),
            "two counted entries on other OS threads"
        );

        // Its own entry is counted.
        let (.., verdict) = registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(1), 555);
        assert_eq!(verdict, Some(true));

        // Another id published on the caller's OS thread (a virtual thread on
        // this carrier): counted, never "not counted" — that would hang.
        let (.., verdict) = registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 101);
        assert_eq!(verdict, Some(true));

        // No OS-tid backend: undecidable.
        let (.., verdict) = registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 0);
        assert_eq!(verdict, None);

        // A counted entry that has not published its OS tid could be the caller.
        registry.register(ThreadId(2), "fresh", None);
        let (alive, .., verdict) =
            registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 555);
        assert_eq!(alive, 3);
        assert_eq!(verdict, None);

        // The census half is identical to the plain census.
        let plain = registry.alive_count_blocked_and_os_tids();
        let with = registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 555);
        assert_eq!((plain.0, plain.1), (with.0, with.1));
    }

    /// A dead or not-yet-ready entry is outside the census, so an initiator
    /// whose only entry is one of those owns no slot.
    #[test]
    fn an_initiator_outside_the_stw_census_is_not_counted() {
        let registry = ThreadRegistry::new();
        registry.register(ThreadId(0), "main", None);
        publish_os_tid(&registry, ThreadId(0), 100);
        registry.register_starting_with_daemon(ThreadId(1), "starting", None, false);
        publish_os_tid(&registry, ThreadId(1), 101);
        // The starting thread requests from its own OS thread before
        // `mark_stw_ready`: its entry is not counted, main's is on another OS
        // thread.
        let (alive, .., verdict) =
            registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(1), 101);
        assert_eq!(alive, 1);
        assert_eq!(verdict, Some(false));
        registry.mark_stw_ready(ThreadId(1));
        let (alive, .., verdict) =
            registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(1), 101);
        assert_eq!(alive, 2);
        assert_eq!(verdict, Some(true));
        registry.mark_dead(ThreadId(1));
        let (alive, .., verdict) =
            registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(1), 101);
        assert_eq!(alive, 1);
        assert_eq!(verdict, Some(false));
    }

    /// gc-common w6-g (`common-w3a-jni-unbound-counted-monitor-enter`,
    /// residual 2): the claimant choice itself.
    #[test]
    fn newest_os_tid_claimant_table() {
        let (a, b, c) = (ThreadId(1), ThreadId(2), ThreadId(3));
        assert_eq!(newest_os_tid_claimant(&[]), None);
        assert_eq!(
            newest_os_tid_claimant(&[(a, 0)]),
            Some(a),
            "one claim, even unstamped"
        );
        assert_eq!(newest_os_tid_claimant(&[(a, 4), (b, 9), (c, 7)]), Some(b));
        assert_eq!(
            newest_os_tid_claimant(&[(a, 0), (b, 0)]),
            None,
            "a tie is ambiguous"
        );
        assert_eq!(newest_os_tid_claimant(&[(a, 0), (b, 0), (c, 1)]), Some(c));
    }

    /// gc-common w6-g: several virtual threads that mounted on one carrier all
    /// keep its OS tid; the one that published last there is the mounted one
    /// and is the answer. Moving a claimant to another carrier, and its death,
    /// take it out of the old carrier's answer.
    #[test]
    fn the_newest_publish_on_an_os_thread_names_the_mounted_thread() {
        let registry = ThreadRegistry::new();
        let (v1, v2, v3) = (ThreadId(1), ThreadId(2), ThreadId(3));
        registry.register(v1, "vt-1", None);
        registry.register(v2, "vt-2", None);
        registry.register(v3, "vt-3", None);
        const CARRIER_A: u32 = 7001;
        const CARRIER_B: u32 = 7002;
        assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), None);
        registry.publish_os_tid_claim(v1, CARRIER_A); // v1 mounts on A
        assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), Some(v1));
        registry.publish_os_tid_claim(v2, CARRIER_A); // v1 parks, v2 mounts on A
        assert_eq!(
            registry.thread_id_for_os_tid_inner(CARRIER_A),
            Some(v2),
            "the parked v1 still claims A, but v2 published there last",
        );
        registry.publish_os_tid_claim(v1, CARRIER_B); // v1 resumes on B
        assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), Some(v2));
        assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_B), Some(v1));
        registry.publish_os_tid_claim(v3, CARRIER_A); // v2 parks, v3 mounts on A
        assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), Some(v3));
        registry.mark_dead(v3); // v3 ends; v2 remounts on A
        registry.publish_os_tid_claim(v2, CARRIER_A);
        assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), Some(v2));
        assert_eq!(registry.thread_id_for_os_tid_inner(0), None, "no backend");

        // Entries whose tid was stored without a publish ordinal stay
        // ambiguous when more than one claims the OS thread.
        let registry = ThreadRegistry::new();
        registry.register(v1, "a", None);
        registry.register(v2, "b", None);
        publish_os_tid(&registry, v1, 900);
        publish_os_tid(&registry, v2, 900);
        assert_eq!(registry.thread_id_for_os_tid_inner(900), None);
    }

    /// gcj j2/pn2 (`CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED`): a parked
    /// (unmounted) virtual thread that published last on its carrier's OS tid
    /// no longer outranks the carrier in the capture fold's claimant table;
    /// without the rule (the `=0` kill and every non-Generational fold) it does.
    #[test]
    fn gcj2pn2_an_unmounted_continuation_claims_no_carrier_tid() {
        let registry = ThreadRegistry::new();
        let (carrier, vt) = (ThreadId(1), ThreadId(2));
        registry.register(carrier, "carrier", None);
        registry.register(vt, "vt", None);
        const TID: u32 = 7100;
        registry.publish_os_tid_claim(carrier, TID);
        registry.publish_os_tid_claim(vt, TID); // vt mounts on the carrier
        let table = |skip: bool| newest_claimants_by_os_tid(&registry.threads.read(), skip);
        assert_eq!(table(true).get(&TID), Some(&Some(vt)), "mounted: vt runs there");
        registry.set_mounted(vt, false); // vt parks
        assert_eq!(table(false).get(&TID), Some(&Some(vt)), "the old rule");
        assert_eq!(table(true).get(&TID), Some(&Some(carrier)), "the carrier again");
        assert!(os_tid_claim_admitted(TID, true, true));
        assert!(!os_tid_claim_admitted(TID, false, true));
        assert!(os_tid_claim_admitted(TID, false, false));
        assert!(!os_tid_claim_admitted(0, true, false));
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED", None)],
            claimant_skips_unmounted_enabled,
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED", Some("0"))],
            claimant_skips_unmounted_enabled,
        );
        assert!(on && !off, "default ON, `=0` kills");
        // gcj j3/x2 R9: the fold's own gate. Generational follows the switch;
        // a G1 / ZGC fold never skips, switch on or off.
        let gate = |generational: bool, value: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED", value)],
                || fold_claimant_skips_unmounted(generational),
            )
        };
        assert!(gate(true, None));
        assert!(!gate(true, Some("0")));
        assert!(!gate(false, None), "G1 / ZGC keep the old rule");
        assert!(!gate(false, Some("0")));
    }

    /// gcj j3/rt3 (j2-pn2 finding 11): a carrier that asks its own identity
    /// after its virtual thread unmounted is answered with ITSELF, not with
    /// the parked continuation whose mount published last on its OS tid; a
    /// mounted continuation still answers. `CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED=0`
    /// (latched per registry) restores the newest-publish-only answer.
    #[test]
    fn gcj3rt3_a_carrier_after_an_unmount_is_answered_with_itself() {
        let registry = ThreadRegistry::new();
        assert!(registry.os_tid_identity_skips_unmounted, "default ON");
        let (carrier, vt) = (ThreadId(1), ThreadId(2));
        registry.register(carrier, "carrier", None);
        registry.register(vt, "vt", None);
        const TID: u32 = 7200;
        registry.publish_os_tid_claim(carrier, TID);
        assert_eq!(registry.thread_id_for_os_tid_inner(TID), Some(carrier));
        registry.publish_os_tid_claim(vt, TID); // vt mounts on the carrier
        assert_eq!(registry.thread_id_for_os_tid_inner(TID), Some(vt), "vt runs there");
        registry.set_mounted(vt, false); // vt parks
        assert_eq!(registry.thread_id_for_os_tid_inner(TID), Some(carrier));
        registry.set_mounted(vt, true); // vt remounts (its publish is still the newest)
        assert_eq!(registry.thread_id_for_os_tid_inner(TID), Some(vt));
        // Only parked continuations claim the tid: nobody answers.
        registry.set_mounted(vt, false);
        registry.mark_dead(carrier);
        assert_eq!(registry.thread_id_for_os_tid_inner(TID), None);

        let killed = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED", Some("0"))],
            ThreadRegistry::new,
        );
        assert!(!killed.os_tid_identity_skips_unmounted, "`=0` kills");
        killed.register(carrier, "carrier", None);
        killed.register(vt, "vt", None);
        killed.publish_os_tid_claim(carrier, TID);
        killed.publish_os_tid_claim(vt, TID);
        killed.set_mounted(vt, false);
        assert_eq!(killed.thread_id_for_os_tid_inner(TID), Some(vt), "the old answer");
    }

    /// gc-common w6-g: `own_jvm_thread_addr` answers only a published address
    /// of an alive entry.
    #[test]
    fn own_jvm_thread_addr_answers_only_a_published_alive_entry() {
        let registry = ThreadRegistry::new();
        let t = ThreadId(4);
        assert_eq!(registry.own_jvm_thread_addr(t), None, "unknown id");
        registry.register(t, "t", None);
        assert_eq!(registry.own_jvm_thread_addr(t), None, "not published");
        registry.set_jvm_thread_addr(t, 0x1000);
        assert_eq!(registry.own_jvm_thread_addr(t), Some(0x1000));
        registry.mark_dead(t);
        assert_eq!(registry.own_jvm_thread_addr(t), None, "dead");
    }

    #[test]
    fn initiator_counted_verdict_table() {
        assert_eq!(
            initiator_counted_verdict(true, false, false, true),
            Some(true)
        );
        assert_eq!(
            initiator_counted_verdict(false, true, true, true),
            Some(true)
        );
        assert_eq!(initiator_counted_verdict(false, false, false, false), None);
        assert_eq!(initiator_counted_verdict(false, false, true, true), None);
        assert_eq!(
            initiator_counted_verdict(false, false, true, false),
            Some(false)
        );
    }
}

/// The inline thin-lock's half of the lock-stack contract (round 11 wave 6):
/// compiled code writes [`JmxLockStack`] through the raw
/// `cratonvm_jit_api::JIT_LOCK_STACK_*` offsets, so these tests write it the
/// same way and require every reader to agree.
#[cfg(test)]
mod jit_inline_lock_stack_tests {
    use super::*;
    use cratonvm_jit_api as api;
    use std::sync::atomic::AtomicUsize;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: only compared and hashed by address; never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn word(stack: &JmxLockStack, off: usize) -> &AtomicUsize {
        // SAFETY: `off` is a pinned word offset inside the `#[repr(C)]` stack
        // (the const assertions beside `JmxLockStack` check every one), and
        // each of those words is an 8-byte, 8-aligned atomic.
        unsafe { &*((stack as *const JmxLockStack as *const u8).add(off) as *const AtomicUsize) }
    }

    fn spilled(stack: &JmxLockStack) -> bool {
        let p = (stack as *const JmxLockStack as *const u8)
            .wrapping_add(api::JIT_LOCK_STACK_SPILLED_OFFSET) as *const AtomicBool;
        // SAFETY: as in `word`, for the one-byte `spilled` flag.
        unsafe { (*p).load(Ordering::Relaxed) }
    }

    /// What the JIT's inline `monitorenter` (`runtime_lowering::
    /// emit_inline_thin_lock`, both tiers) does after its CAS won.
    /// `false` = it would have taken the helper instead.
    fn jit_push(stack: &JmxLockStack, a: usize) -> bool {
        jit_push_as(stack, a, false)
    }

    /// What the inline `monitorexit` does after its CAS won. `false` = helper.
    fn jit_pop(stack: &JmxLockStack, a: usize) -> bool {
        jit_pop_as(stack, a, false)
    }

    /// [`jit_push`] in either compiled shape: the odd/even bracket, or
    /// (`single`, round 13 wave 12, `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP`)
    /// slot, `top`, then one `seq += 2`. The single shape's stores are
    /// `Release`, which is what the emitted `MOV`s are on x86-TSO (every
    /// store is visible after the ones before it).
    fn jit_push_as(stack: &JmxLockStack, a: usize, single: bool) -> bool {
        let top = word(stack, api::JIT_LOCK_STACK_TOP_OFFSET).load(Ordering::Relaxed);
        if spilled(stack) || top >= api::JIT_LOCK_STACK_SLOTS {
            return false;
        }
        let seq = word(stack, api::JIT_LOCK_STACK_SEQ_OFFSET);
        let s = seq.load(Ordering::Relaxed);
        let slot = word(stack, api::JIT_LOCK_STACK_SLOTS_OFFSET + 8 * top);
        let top_word = word(stack, api::JIT_LOCK_STACK_TOP_OFFSET);
        if single {
            slot.store(a, Ordering::Release);
            top_word.store(top + 1, Ordering::Release);
            seq.store(s + 2, Ordering::Release);
            return true;
        }
        seq.store(s + 1, Ordering::Relaxed);
        slot.store(a, Ordering::Relaxed);
        top_word.store(top + 1, Ordering::Relaxed);
        seq.store(s + 2, Ordering::Relaxed);
        true
    }

    /// [`jit_pop`] in either compiled shape (see [`jit_push_as`]).
    fn jit_pop_as(stack: &JmxLockStack, a: usize, single: bool) -> bool {
        let top = word(stack, api::JIT_LOCK_STACK_TOP_OFFSET).load(Ordering::Relaxed);
        if top == 0 || top > api::JIT_LOCK_STACK_SLOTS {
            return false;
        }
        let slot = word(stack, api::JIT_LOCK_STACK_SLOTS_OFFSET + 8 * (top - 1));
        if slot.load(Ordering::Relaxed) != a {
            return false;
        }
        let seq = word(stack, api::JIT_LOCK_STACK_SEQ_OFFSET);
        let s = seq.load(Ordering::Relaxed);
        let top_word = word(stack, api::JIT_LOCK_STACK_TOP_OFFSET);
        if single {
            slot.store(0, Ordering::Release);
            top_word.store(top - 1, Ordering::Release);
            seq.store(s + 2, Ordering::Release);
            return true;
        }
        seq.store(s + 1, Ordering::Relaxed);
        slot.store(0, Ordering::Relaxed);
        top_word.store(top - 1, Ordering::Relaxed);
        seq.store(s + 2, Ordering::Relaxed);
        true
    }

    /// Round 13 wave 12 (lane monitor3, proposal S7-4): a single-bump
    /// compiled writer racing a cross-thread [`JmxLockStack::snapshot`]
    /// never shows the reader a set the owner did not hold at one instant.
    /// The writer cycles `push x_i; push y_i; pop y_i; pop x_i` over four
    /// pairs, so the only true states are `[]`, `[x_i]` and `[x_i, y_i]`; a
    /// torn read would mix two pairs (`[x_i, y_j]`) or show a `y` alone.
    #[test]
    fn a_single_bump_writer_never_shows_a_reader_a_torn_set() {
        const X0: usize = 0x70_000;
        const PAIR_STRIDE: usize = 0x100;
        const Y_OFF: usize = 0x40;
        let stack = JmxLockStack::default();
        let done = AtomicBool::new(false);
        let is_x = |v: usize| v >= X0 && v < X0 + 4 * PAIR_STRIDE && (v - X0) % PAIR_STRIDE == 0;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..200_000usize {
                    let x = X0 + (i % 4) * PAIR_STRIDE;
                    let y = x + Y_OFF;
                    assert!(jit_push_as(&stack, x, true));
                    assert!(jit_push_as(&stack, y, true));
                    assert!(jit_pop_as(&stack, y, true));
                    assert!(jit_pop_as(&stack, x, true));
                }
                done.store(true, Ordering::Release);
            });
            let mut reads = 0u64;
            while !done.load(Ordering::Acquire) || reads == 0 {
                let snap: Vec<usize> = stack
                    .snapshot()
                    .iter()
                    .map(|o| o.as_ptr() as usize)
                    .collect();
                let consistent = match snap.as_slice() {
                    [] => true,
                    [x] => is_x(*x),
                    [x, y] => is_x(*x) && *y == *x + Y_OFF,
                    _ => false,
                };
                assert!(consistent, "torn snapshot {snap:x?} after {reads} reads");
                reads += 1;
            }
        });
        let seq = word(&stack, api::JIT_LOCK_STACK_SEQ_OFFSET).load(Ordering::Relaxed);
        assert_eq!(seq, 4 * 2 * 200_000, "every write moved seq by exactly 2");
        assert_eq!(stack.snapshot().len(), 0);
    }

    /// Compiled and helper acquisitions/releases interleaved at random must
    /// leave the stack reporting exactly the held set, in acquisition order,
    /// to both the JMX snapshot and the collector: the model the helper-only
    /// test in `tests` checks, with the inline writer added.
    #[test]
    fn inline_and_helper_writers_agree_on_one_lock_stack() {
        let stack = JmxLockStack::default();
        let mut model: Vec<usize> = Vec::new();
        let (mut pushed, mut popped) = (0u32, 0u32);
        let mut x: u64 = 0xD1B5_4A32_D192_ED03;
        for step in 0..20_000u32 {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let a = 0x30_000usize + ((x >> 33) % 20) as usize * 0x40;
            match (x >> 59) & 7 {
                // A compiled `monitorenter`: inline only for a monitor this
                // thread does not hold (its mark word is NEUTRAL); held means
                // recursion, which is the helper's publish (a dedupe).
                0..=2 => {
                    if model.contains(&a) {
                        stack.publish(fake(a));
                    } else {
                        // Both compiled shapes, alternating (round 13 wave 12).
                        if jit_push_as(&stack, a, step % 2 == 1) {
                            pushed += 1;
                        } else {
                            stack.publish(fake(a));
                        }
                        model.push(a);
                    }
                }
                // A compiled outermost `monitorexit` of the innermost monitor.
                3 | 4 => {
                    if let Some(&last) = model.last() {
                        if jit_pop_as(&stack, last, step % 2 == 1) {
                            popped += 1;
                        } else {
                            stack.retract(fake(last));
                        }
                        model.retain(|&m| m != last);
                    }
                }
                // A helper-path acquisition, and an out-of-order release.
                5 => {
                    stack.publish(fake(a));
                    if !model.contains(&a) {
                        model.push(a);
                    }
                }
                _ => {
                    stack.retract(fake(a));
                    model.retain(|&m| m != a);
                }
            }
            let seq = word(&stack, api::JIT_LOCK_STACK_SEQ_OFFSET).load(Ordering::Relaxed);
            assert_eq!(seq & 1, 0, "step {step}: a write left the seqlock open");
            let snap: Vec<usize> = stack
                .snapshot()
                .iter()
                .map(|o| o.as_ptr() as usize)
                .collect();
            assert_eq!(snap, model, "step {step}: the snapshot diverged");
            let mut roots = Vec::new();
            stack.extend_roots_stw(&mut roots);
            let roots: Vec<usize> = roots.iter().map(|o| o.as_ptr() as usize).collect();
            assert_eq!(roots, model, "step {step}: the GC roots diverged");
        }
        if monitor_fastpath_enabled() {
            assert!(pushed > 0 && popped > 0, "the inline writer never ran");
        }
    }

    fn block_word(block: &crate::threading::jvm_thread::JitMonitorBlock, off: usize) -> u64 {
        let p = (block as *const crate::threading::jvm_thread::JitMonitorBlock as *const u8)
            .wrapping_add(off) as *const u64;
        // SAFETY: `off` is one of the two pinned `JIT_MONITOR_BLOCK_*` offsets
        // of the `#[repr(C)]` block, each an 8-byte, 8-aligned word.
        unsafe { *p }
    }

    /// Arming publishes the thread's thin-lock LEASE (owner bits and `held`
    /// counter, from the VM's `MonitorTable`) and the address of the SAME lock
    /// stack the helpers write, and refuses what the helpers refuse.
    #[test]
    fn arming_points_compiled_code_at_the_helpers_lock_stack() {
        use crate::threading::jvm_thread::JitMonitorBlock;
        use crate::threading::monitor::MonitorTable;
        let registry = ThreadRegistry::new();
        let table = MonitorTable::new();
        let tid = registry.next_thread_id();
        let mut block = JitMonitorBlock::unarmed();
        let owner = |b: &JitMonitorBlock| block_word(b, api::JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET);
        // Unregistered: stays unarmed. Since round 11 wave 16 the lease is
        // asked FIRST (a refused thread skips the registry lookup), so an
        // unregistered thread may lease its slot here: the same slot its
        // helper-path `monitorenter` would lease anyway, and the one the
        // lookup below answers again.
        registry.arm_jit_monitor_block(&mut block, tid, || table.jit_thin_lease(tid));
        assert_eq!(owner(&block), u64::MAX);
        registry.register(tid, "armed", None);
        // No slot to lease (every one taken): stays unarmed.
        registry.arm_jit_monitor_block(&mut block, tid, || None);
        assert_eq!(owner(&block), u64::MAX);
        registry.arm_jit_monitor_block(&mut block, tid, || table.jit_thin_lease(tid));
        if !monitor_fastpath_enabled() {
            // The kill switch leaves every thread on the helper path.
            assert_eq!(owner(&block), u64::MAX);
            return;
        }
        let lease = table
            .jit_thin_lease(tid)
            .expect("a fresh table has free slots");
        assert_eq!(owner(&block), u64::from(lease.owner_bits));
        assert_eq!(
            owner(&block) & u64::from(cratonvm_types::MARK_STATE_MASK),
            u64::from(cratonvm_types::MARK_THIN_LOCKED),
            "the owner bits are a THIN_LOCKED word at recursion 0"
        );
        assert_eq!(
            block_word(&block, api::JIT_MONITOR_BLOCK_HELD_OFFSET) as usize,
            lease.held
        );
        assert_ne!(lease.held, 0);
        let ls = block_word(&block, api::JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET) as usize;
        assert_ne!(ls, 0);
        // SAFETY: the block holds an `Arc` of the book `ls` points into.
        let stack = unsafe { &*(ls as *const JmxLockStack) };
        assert!(jit_push(stack, 0x40_000));
        registry.complete_jmx_monitor_enter(tid, fake(0x40_040));
        let seen = registry
            .with_self_jmx_monitors(tid, |b| b.locked.snapshot())
            .expect("registered");
        let seen: Vec<usize> = seen.iter().map(|o| o.as_ptr() as usize).collect();
        assert_eq!(seen, vec![0x40_000, 0x40_040], "one stack, two writers");
        registry.remove_jmx_locked_monitor(tid, fake(0x40_040));
        assert!(jit_pop(stack, 0x40_000));
        // Re-arming from the same registry is a no-op, and asks for no lease.
        registry.arm_jit_monitor_block(&mut block, tid, || {
            panic!("an armed block must not lease again")
        });
        assert_eq!(
            block_word(&block, api::JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET) as usize,
            ls
        );
        // A block armed from another registry is never left pointing at it.
        let other = ThreadRegistry::new();
        other.arm_jit_monitor_block(&mut block, tid, || table.jit_thin_lease(tid));
        assert_eq!(owner(&block), u64::MAX);
        assert_eq!(block_word(&block, api::JIT_MONITOR_BLOCK_HELD_OFFSET), 0);
    }
}

/// Round 12 wave 1 (lane lock): `Thread.getState()` of a thread inside a
/// contended monitor entry that has not entered the GC-blocked region -- the
/// opt-in running park, `CRATONVM_MONITOR_LAZY_PARK_US` -- is BLOCKED, and it
/// is RUNNABLE again once the entry completes.
#[cfg(test)]
mod r12_lock_block_state_tests {
    use super::*;

    #[test]
    fn a_published_contended_entry_reads_blocked_until_it_completes() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(9101);
        registry.register(tid, "r12-lazy-parker", None);
        // SAFETY: only stored and compared by address; never dereferenced.
        let lock = unsafe { ObjectRef::from_raw(0x51_0000 as *mut u8) };
        assert_eq!(registry.java_block_state(tid), 0, "running: RUNNABLE");
        registry.set_jmx_contended_monitor(tid, lock);
        assert_eq!(registry.java_block_state(tid), 2, "contending: BLOCKED");
        registry.complete_jmx_monitor_enter(tid, lock);
        assert_eq!(registry.java_block_state(tid), 0, "acquired: RUNNABLE");
        registry.remove_jmx_locked_monitor(tid, lock);
        assert_eq!(
            registry.java_block_state(ThreadId(9102)),
            0,
            "unknown thread"
        );
    }

    /// Interpreter round i2 wave 57 (lane L1): a thread inside `Object.wait`
    /// that is not GC-blocked (the release-first wait's lazy running phase)
    /// reads its wait's state while its waiting monitor is published, and
    /// RUNNABLE once the wait takes it back, whatever `java_state` still says.
    #[test]
    fn a_published_wait_reads_its_state_before_it_blocks() {
        let mut registry = ThreadRegistry::new();
        // The wave-57 reading this test pins; `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`
        // (default on since the wave-67 triage) reads the published state
        // without a monitor.
        registry.set_published_all_wait_kinds_for_test(false);
        let tid = ThreadId(9103);
        registry.register(tid, "w57-lazy-waiter", None);
        let block = registry.gc_block_state_of(tid).expect("registered");
        // SAFETY: only stored and compared by address; never dereferenced.
        let lock = unsafe { ObjectRef::from_raw(0x52_0000 as *mut u8) };
        block.java_state.store(1, Ordering::Release);
        assert_eq!(
            registry.java_block_state(tid),
            0,
            "no wait published: RUNNABLE"
        );
        registry.set_jmx_waiting_monitor(tid, lock);
        assert_eq!(registry.java_block_state(tid), 1, "waiting: WAITING");
        block.java_state.store(3, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 3, "timed: TIMED_WAITING");
        block.java_state.store(2, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 2, "re-entering: BLOCKED");
        registry.take_jmx_waiting_monitor(tid);
        assert_eq!(registry.java_block_state(tid), 0, "wait over: RUNNABLE");
    }

    /// Interpreter round i6 wave 132 (lane L1, i130-L1b,
    /// `CRATONVM_JVMTI_THREAD_STATE_ONE_READ`): the one-read facts answer
    /// what the three separate reads answer, in every state a published wait
    /// passes through, with the all-kinds switch on and off, interrupted or
    /// not, and for an unknown thread.
    #[test]
    fn the_one_read_state_facts_equal_the_three_reads() {
        for all_kinds in [false, true] {
            let mut registry = ThreadRegistry::new();
            registry.set_published_all_wait_kinds_for_test(all_kinds);
            let tid = ThreadId(9132);
            registry.register(tid, "w132-one-read", None);
            let block = registry.gc_block_state_of(tid).expect("registered");
            // SAFETY: only stored and compared by address; never dereferenced.
            let lock = unsafe { ObjectRef::from_raw(0x53_0000 as *mut u8) };
            let check = |registry: &ThreadRegistry, what: &str| {
                for id in [tid, ThreadId(9133)] {
                    let (state, kind, interrupted) = registry.jvmti_state_facts(id);
                    assert_eq!(state, registry.java_block_state(id), "{what}: state");
                    let separate = matches!(state, 1 | 3)
                        .then(|| registry.published_wait_kind_of(id))
                        .flatten();
                    assert_eq!(kind, separate, "{what}: kind (all={all_kinds})");
                    let flag = registry
                        .get_interrupted_flag(id)
                        .is_some_and(|f| f.load(Ordering::Acquire));
                    assert_eq!(interrupted, flag, "{what}: interrupted");
                }
            };
            check(&registry, "running");
            block.java_state.store(1, Ordering::Release);
            check(&registry, "state stored, nothing published");
            registry.set_jmx_waiting_monitor(tid, lock);
            check(&registry, "waiting");
            block.java_state.store(3, Ordering::Release);
            check(&registry, "timed");
            if let Some(flag) = registry.get_interrupted_flag(tid) {
                flag.store(true, Ordering::Release);
            }
            check(&registry, "timed, interrupted");
            block.java_state.store(2, Ordering::Release);
            check(&registry, "re-entering");
            registry.take_jmx_waiting_monitor(tid);
            check(&registry, "wait over");
        }
    }

    /// Interpreter round i2 wave 59 (lane L6): a thread blocked in a
    /// registered native standing in for a JDK native (`BlockKind::InNative`,
    /// `jvm_thread::JAVA_STATE_IN_NATIVE`) reads RUNNABLE and in native while
    /// the flag is up, and neither once it is down; a WAITING region is not
    /// in native.
    #[test]
    fn a_thread_blocked_in_a_native_reads_runnable_and_in_native() {
        use crate::threading::jvm_thread::JAVA_STATE_IN_NATIVE;
        let registry = ThreadRegistry::new();
        let tid = ThreadId(9104);
        registry.register(tid, "w59-blocked-in-native", None);
        let block = registry.gc_block_state_of(tid).expect("registered");
        block
            .java_state
            .store(JAVA_STATE_IN_NATIVE, Ordering::Release);
        assert!(!registry.blocked_in_native(tid), "flag down: not blocked");
        block.in_blocked_region.store(true, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 0, "in native: RUNNABLE");
        assert!(registry.blocked_in_native(tid));
        assert!(registry.thread_in_native(tid));
        block.java_state.store(1, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 1, "a wait: WAITING");
        assert!(!registry.blocked_in_native(tid));
        assert!(!registry.thread_in_native(tid));
        block.in_blocked_region.store(false, Ordering::Release);
        assert!(
            !registry.blocked_in_native(ThreadId(9105)),
            "unknown thread"
        );
    }

    /// Interpreter round i2 wave 63 (lane L1, `CRATONVM_PUBLISHED_WAIT_KIND`):
    /// a sleep's published wait kind reads as sleeping only while the flag is
    /// up over a TIMED_WAITING state; switched off, the reader is told to
    /// ask the frames (`None`).
    #[test]
    fn a_published_sleep_reads_sleeping_only_inside_its_region() {
        use crate::threading::jvm_thread::{JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_SLEEPING};
        let mut registry = ThreadRegistry::new();
        registry.set_published_wait_kind_for_test(false);
        // `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` implies the word too, and is
        // default on since the wave-67 triage.
        registry.set_published_all_wait_kinds_for_test(false);
        let tid = ThreadId(9106);
        registry.register(tid, "w63-sleeper", None);
        let block = registry.gc_block_state_of(tid).expect("registered");
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_SLEEPING, Ordering::Release);
        block.java_state.store(3, Ordering::Release);
        block.in_blocked_region.store(true, Ordering::Release);
        assert_eq!(
            registry.published_sleeping(tid),
            None,
            "switch off: ask the frames"
        );

        registry.set_published_wait_kind_for_test(true);
        assert_eq!(registry.published_sleeping(tid), Some(true), "sleeping");
        assert_eq!(registry.java_block_state(tid), 3, "still TIMED_WAITING");
        // A timed park after the sleep: the kind was lowered at the sleep's end.
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
        assert_eq!(registry.published_sleeping(tid), Some(false), "timed park");
        // A stale kind under another state does not read as a sleep.
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_SLEEPING, Ordering::Release);
        block.java_state.store(1, Ordering::Release);
        assert_eq!(
            registry.published_sleeping(tid),
            Some(false),
            "WAITING is no sleep"
        );
        block.java_state.store(3, Ordering::Release);
        block.in_blocked_region.store(false, Ordering::Release);
        assert_eq!(
            registry.published_sleeping(tid),
            Some(false),
            "flag down: running"
        );
        assert_eq!(
            registry.published_sleeping(ThreadId(9107)),
            Some(false),
            "unknown thread"
        );
    }

    /// Interpreter round i2 wave 64 (lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
    /// every published kind reads back through the one function; switched
    /// off, the reader is told to assemble the kind itself (`None`); the
    /// switch implies the wave-63 sleep switch.
    #[test]
    fn every_published_wait_kind_reads_back_through_one_function() {
        use crate::threading::jvm_thread::{
            JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_OBJECT_WAIT, JAVA_WAIT_KIND_PARKED,
            JAVA_WAIT_KIND_REGION_OBJECT_WAIT, JAVA_WAIT_KIND_SLEEPING,
        };
        let mut registry = ThreadRegistry::new();
        registry.set_published_wait_kind_for_test(false);
        registry.set_published_all_wait_kinds_for_test(false);
        let tid = ThreadId(9108);
        registry.register(tid, "w64-waiter", None);
        let block = registry.gc_block_state_of(tid).expect("registered");
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_PARKED, Ordering::Release);
        block.java_state.store(1, Ordering::Release);
        block.in_blocked_region.store(true, Ordering::Release);
        assert_eq!(registry.published_wait_kind_of(tid), None, "switch off");
        assert!(!registry.published_wait_kind(), "sleep switch off too");

        registry.set_published_all_wait_kinds_for_test(true);
        assert!(registry.published_wait_kind(), "implies the sleep switch");
        assert_eq!(
            registry.published_wait_kind_of(tid),
            Some(PublishedWaitKind::Parked)
        );
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_OBJECT_WAIT, Ordering::Release);
        assert_eq!(
            registry.published_wait_kind_of(tid),
            Some(PublishedWaitKind::ObjectWait)
        );
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_REGION_OBJECT_WAIT, Ordering::Release);
        assert_eq!(
            registry.published_wait_kind_of(tid),
            Some(PublishedWaitKind::ObjectWait),
            "the served join"
        );
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
        assert_eq!(
            registry.published_wait_kind_of(tid),
            Some(PublishedWaitKind::Unrecorded)
        );
        // A sleep counts only over TIMED_WAITING with the flag up.
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_SLEEPING, Ordering::Release);
        assert_eq!(
            registry.published_wait_kind_of(tid),
            Some(PublishedWaitKind::Unrecorded),
            "WAITING is no sleep"
        );
        block.java_state.store(3, Ordering::Release);
        assert_eq!(
            registry.published_wait_kind_of(tid),
            Some(PublishedWaitKind::Sleeping)
        );
        assert_eq!(registry.published_sleeping(tid), Some(true));
        block.in_blocked_region.store(false, Ordering::Release);
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
        assert_eq!(
            registry.published_wait_kind_of(ThreadId(9109)),
            Some(PublishedWaitKind::Unrecorded),
            "unknown thread"
        );
    }

    /// Interpreter round i2 wave 64 (lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
    /// the lazy phase of `Object.wait` (flag down) reads its wait's state from
    /// the published `Object.wait` kind, not the JMX waiting monitor; the
    /// served join's kind, which outlives its flag for a moment, does not
    /// count there.
    #[test]
    fn a_lazy_wait_reads_its_state_from_the_published_kind() {
        use crate::threading::jvm_thread::{
            JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_OBJECT_WAIT, JAVA_WAIT_KIND_REGION_OBJECT_WAIT,
        };
        let mut registry = ThreadRegistry::new();
        registry.set_published_all_wait_kinds_for_test(true);
        let tid = ThreadId(9110);
        registry.register(tid, "w64-lazy-waiter", None);
        let block = registry.gc_block_state_of(tid).expect("registered");
        block.java_state.store(3, Ordering::Release);
        assert_eq!(
            registry.java_block_state(tid),
            0,
            "no wait published: RUNNABLE"
        );
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_OBJECT_WAIT, Ordering::Release);
        assert_eq!(
            registry.java_block_state(tid),
            3,
            "lazy timed wait: TIMED_WAITING"
        );
        block.java_state.store(2, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 2, "re-entering: BLOCKED");
        block.java_state.store(1, Ordering::Release);
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_REGION_OBJECT_WAIT, Ordering::Release);
        assert_eq!(
            registry.java_block_state(tid),
            0,
            "a join's kind after its flag: RUNNABLE"
        );
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 0, "wait over: RUNNABLE");
    }

    /// Interpreter round i3 wave 79, lane L6 (`CRATONVM_PARK_LAZY_SPIN_NS`):
    /// the value parse -- off unset, at `0`, unparsable or on one CPU; a
    /// positive value in nanoseconds, capped at `PARK_LAZY_SPIN_CAP`.
    #[test]
    fn park_lazy_spin_parses_nanos_caps_them_and_skips_one_cpu() {
        use std::time::Duration;
        assert_eq!(
            park_lazy_spin_from(None, 8),
            Some(Duration::from_nanos(PARK_LAZY_SPIN_DEFAULT_NS)),
            "unset: the default spin"
        );
        assert_eq!(
            park_lazy_spin_from(None, 1),
            None,
            "one CPU: no spin, even by default"
        );
        assert_eq!(park_lazy_spin_from(Some("0"), 8), None);
        assert_eq!(park_lazy_spin_from(Some("x"), 8), None);
        assert_eq!(park_lazy_spin_from(Some("-5"), 8), None);
        assert_eq!(
            park_lazy_spin_from(Some("2000"), 1),
            None,
            "one CPU: no spin"
        );
        assert_eq!(
            park_lazy_spin_from(Some(" 2000 "), 2),
            Some(Duration::from_nanos(2_000))
        );
        assert_eq!(
            park_lazy_spin_from(Some("10000"), 8),
            Some(Duration::from_nanos(10_000))
        );
        assert_eq!(
            park_lazy_spin_from(Some("1000000"), 8),
            Some(PARK_LAZY_SPIN_CAP)
        );
        let mut registry = ThreadRegistry::new();
        registry.set_park_lazy_spin_for_test(Some(Duration::from_nanos(2_000)));
        assert_eq!(registry.park_lazy_spin(), Some(Duration::from_nanos(2_000)));
        registry.set_park_lazy_spin_for_test(None);
        assert_eq!(registry.park_lazy_spin(), None);
    }

    /// Interpreter round i3 wave 71, lane L1 (`CRATONVM_PARK_LAZY`): a park's
    /// running phase (flag down, kind `PARKED`) reads its state only with the
    /// switch on; off, that window stays RUNNABLE as before. The switch is set
    /// explicitly both ways.
    #[test]
    fn a_lazy_park_reads_its_state_from_the_published_kind_only_with_the_switch() {
        use crate::threading::jvm_thread::{JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_PARKED};
        let mut registry = ThreadRegistry::new();
        registry.set_published_all_wait_kinds_for_test(true);
        registry.set_park_lazy_budget_for_test(None);
        let tid = ThreadId(9112);
        registry.register(tid, "w71-lazy-parker", None);
        let block = registry.gc_block_state_of(tid).expect("registered");
        block.java_state.store(1, Ordering::Release);
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_PARKED, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 0, "switch off: RUNNABLE");
        registry.set_park_lazy_budget_for_test(Some(std::time::Duration::from_micros(2_000)));
        assert_eq!(registry.java_block_state(tid), 1, "lazy park: WAITING");
        block.java_state.store(3, Ordering::Release);
        assert_eq!(
            registry.java_block_state(tid),
            3,
            "lazy timed park: TIMED_WAITING"
        );
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 0, "park over: RUNNABLE");
    }

    /// Interpreter round i3 wave 72, lane L6 (`CRATONVM_PARK_LAZY_STW_WAKE`):
    /// with the switch on, a park's running phase is listed for its duration
    /// and a pause initiator's `wake_lazy_park_phases` ends a phase that would
    /// otherwise wait out a long budget in one wait; off, nothing is listed.
    /// The switch is set explicitly both ways.
    #[test]
    fn a_pause_initiator_wakes_a_listed_park_phase_only_with_the_switch() {
        let mut registry = ThreadRegistry::new();
        registry.set_park_lazy_stw_wake_for_test(false);
        let ps = Arc::new(ParkState::new());
        registry.lazily_parked(&ps, || {
            assert_eq!(registry.lazy_park_state_count(), 0, "off: not listed");
        });
        registry.wake_lazy_park_phases();

        registry.set_park_lazy_stw_wake_for_test(true);
        let registry = Arc::new(registry);
        let stop = Arc::new(AtomicBool::new(false));
        let not_interrupted = AtomicBool::new(false);
        let long = std::time::Duration::from_secs(30);
        let (r2, stop2) = (registry.clone(), stop.clone());
        let initiator = std::thread::spawn(move || {
            // Wait until the parker is listed, then raise the "pause" and wake.
            while r2.lazy_park_state_count() == 0 {
                std::thread::yield_now();
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            stop2.store(true, Ordering::SeqCst);
            r2.wake_lazy_park_phases();
        });
        let t0 = std::time::Instant::now();
        let over = registry.lazily_parked(&ps, || {
            ps.park_lazily_sliced(long, long, &not_interrupted, &|| {
                !stop.load(Ordering::SeqCst)
            })
        });
        assert!(!over, "a pause is not the end of the park");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "the wake ended the phase"
        );
        assert_eq!(
            registry.lazy_park_state_count(),
            0,
            "unlisted on the way out"
        );
        assert_eq!(registry.lazy_park_phases.load(Ordering::SeqCst), 0);
        initiator.join().unwrap();
    }

    /// Interpreter round i3 wave 72, lane L6 (`CRATONVM_UNPARK_ONE_LOOKUP`):
    /// the one-call lookup answers what the two separate lookups answer, and
    /// nothing for an unknown or zero `tid`.
    #[test]
    fn the_one_call_unpark_target_matches_the_separate_lookups() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(9172);
        registry.register(tid, "w72-unpark-target", None);
        let mut backing = [0u64; 2];
        // SAFETY: the registry only stores, hashes and compares this address.
        let mirror = unsafe { ObjectRef::from_raw(backing.as_mut_ptr() as *mut u8) };
        registry.set_java_thread_obj_with_tid(tid, mirror, 77_172);
        let (id, state) = registry
            .unpark_target_by_java_tid(77_172)
            .expect("registered");
        assert_eq!(id, tid);
        assert_eq!(registry.find_thread_id_by_java_tid(77_172), Some(tid));
        let separate = registry
            .find_park_state_by_java_tid(77_172)
            .expect("registered");
        assert!(Arc::ptr_eq(&state, &separate));
        assert!(registry.unpark_target_by_java_tid(77_173).is_none());
        assert!(registry.unpark_target_by_java_tid(0).is_none());
    }

    /// Interpreter round i2 wave 65 (lane L1, `CRATONVM_SERVED_WAIT_KINDS`):
    /// a served stand-in's monitor wait for JDK code that parks reads
    /// `PARKED` through both kind readers -- the one JVMTI asks without
    /// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` and the one it asks with it -- and
    /// spans the lazy phase as an `Object.wait` does.
    #[test]
    fn a_served_monitor_park_reads_parked_with_or_without_all_wait_kinds() {
        use crate::threading::jvm_thread::{
            JAVA_WAIT_KIND_MONITOR_PARKED, JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_OBJECT_WAIT,
        };
        let mut registry = ThreadRegistry::new();
        registry.set_published_all_wait_kinds_for_test(false);
        registry.set_served_wait_kinds_for_test(true);
        let tid = ThreadId(9111);
        registry.register(tid, "w65-latch-waiter", None);
        let block = registry.gc_block_state_of(tid).expect("registered");
        block.java_state.store(3, Ordering::Release);
        block.in_blocked_region.store(true, Ordering::Release);
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_MONITOR_PARKED, Ordering::Release);
        assert!(registry.published_monitor_parked(tid), "switch on: parked");
        assert_eq!(registry.java_block_state(tid), 3, "timed: TIMED_WAITING");
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_OBJECT_WAIT, Ordering::Release);
        assert!(
            !registry.published_monitor_parked(tid),
            "a plain Object.wait"
        );
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_MONITOR_PARKED, Ordering::Release);
        registry.set_served_wait_kinds_for_test(false);
        assert!(!registry.published_monitor_parked(tid), "switch off");
        registry.set_served_wait_kinds_for_test(true);
        assert!(
            !registry.published_monitor_parked(ThreadId(9112)),
            "unknown thread"
        );

        registry.set_published_all_wait_kinds_for_test(true);
        assert_eq!(
            registry.published_wait_kind_of(tid),
            Some(PublishedWaitKind::Parked)
        );
        // The lazy phase: flag down, the kind and the state up.
        block.in_blocked_region.store(false, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 3, "lazy: TIMED_WAITING");
        block.java_state.store(1, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 1, "lazy untimed: WAITING");
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
        assert_eq!(registry.java_block_state(tid), 0, "wait over: RUNNABLE");
    }
}

/// Interpreter round i2 wave 66, lane L1 (`CRATONVM_DBG_UNRECORDED_WAIT_KINDS`):
/// the census of blocking regions that open with no recorded wait kind.
#[cfg(test)]
mod i66_unrecorded_wait_census_tests {
    use super::*;

    #[test]
    fn the_mode_is_read_from_the_value() {
        assert_eq!(
            UnrecordedWaitMode::from_value(None),
            UnrecordedWaitMode::Off
        );
        assert_eq!(
            UnrecordedWaitMode::from_value(Some("0")),
            UnrecordedWaitMode::Off
        );
        assert_eq!(
            UnrecordedWaitMode::from_value(Some("off")),
            UnrecordedWaitMode::Off
        );
        assert_eq!(
            UnrecordedWaitMode::from_value(Some("1")),
            UnrecordedWaitMode::Count
        );
        assert_eq!(
            UnrecordedWaitMode::from_value(Some("yes")),
            UnrecordedWaitMode::Count
        );
        assert_eq!(
            UnrecordedWaitMode::from_value(Some("trace")),
            UnrecordedWaitMode::Trace
        );
        assert_eq!(
            UnrecordedWaitMode::from_value(Some(" TRACE ")),
            UnrecordedWaitMode::Trace
        );
        assert_eq!(
            UnrecordedWaitMode::from_value(Some("assert")),
            UnrecordedWaitMode::Assert
        );
    }

    #[test]
    fn regions_are_counted_by_call_site_and_door_only_when_on() {
        let mut registry = ThreadRegistry::new();
        registry.set_unrecorded_wait_mode_for_test(UnrecordedWaitMode::Off);
        let here = std::panic::Location::caller();
        registry.note_unrecorded_wait_region(here, "begin_blocking_region");
        assert!(!registry.unrecorded_wait_census_on());
        assert!(
            registry.unrecorded_wait_sites().is_empty(),
            "off: nothing counted"
        );

        registry.set_unrecorded_wait_mode_for_test(UnrecordedWaitMode::Count);
        assert!(registry.unrecorded_wait_census_on());
        let a = std::panic::Location::caller();
        for _ in 0..3 {
            registry.note_unrecorded_wait_region(a, "begin_blocking_region");
        }
        registry.note_unrecorded_wait_region(a, "begin_timed_blocking_region");
        let rows = registry.unrecorded_wait_sites();
        assert_eq!(rows.len(), 2, "one row per site and door: {rows:?}");
        assert_eq!(rows[0].count, 3, "most regions first");
        assert_eq!(rows[0].door, "begin_blocking_region");
        assert_eq!(rows[1].count, 1);
        assert_eq!(rows[1].door, "begin_timed_blocking_region");
        assert!(
            rows[0].site.contains("thread_registry.rs:"),
            "{}",
            rows[0].site
        );
    }
}

/// Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
/// a stand-in region's published waiting monitor.
#[cfg(test)]
mod i68_region_waiting_monitor_tests {
    use super::*;

    /// The region's monitor reads as the JMX lock while it is up, never
    /// replaces a monitor a real wait published, counts no wait, and is gone
    /// once withdrawn.
    #[test]
    fn a_region_publishes_its_monitor_for_its_span_only() {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(9681);
        registry.register(tid, "i68-region-waiter", None);
        // SAFETY: only stored and compared by address; never dereferenced.
        let lock = unsafe { ObjectRef::from_raw(0x68_0000 as *mut u8) };
        // SAFETY: as above.
        let other = unsafe { ObjectRef::from_raw(0x68_1000 as *mut u8) };
        assert!(registry.publish_region_waiting_monitor(tid, lock));
        assert_eq!(registry.peek_jmx_waiting_monitor(tid), Some(lock));
        let (_, waiting, _, _, _, _, _, waited_count) =
            registry.jmx_lock_snapshot(tid).expect("registered");
        assert_eq!(waiting, Some(lock), "JMX reads the region's monitor");
        assert!(
            !registry.publish_region_waiting_monitor(tid, other),
            "a published monitor is never replaced"
        );
        assert_eq!(registry.peek_jmx_waiting_monitor(tid), Some(lock));
        registry.withdraw_region_waiting_monitor(tid);
        assert_eq!(registry.peek_jmx_waiting_monitor(tid), None);
        let (_, _, _, _, _, _, _, after) = registry.jmx_lock_snapshot(tid).expect("registered");
        assert_eq!(after, waited_count, "a region's slices count no JMX wait");
        // A real wait's monitor stays where it is.
        registry.set_jmx_waiting_monitor(tid, other);
        assert!(!registry.publish_region_waiting_monitor(tid, lock));
        assert_eq!(registry.peek_jmx_waiting_monitor(tid), Some(other));
        registry.take_jmx_waiting_monitor(tid);
        assert!(
            !registry.publish_region_waiting_monitor(ThreadId(9682), lock),
            "unknown thread"
        );
    }

    #[test]
    fn the_switch_has_a_test_hook() {
        let mut registry = ThreadRegistry::new();
        registry.set_stand_in_wait_objects_for_test(true);
        assert!(registry.stand_in_wait_objects());
        registry.set_stand_in_wait_objects_for_test(false);
        assert!(!registry.stand_in_wait_objects());
    }
}

/// nio21 wave 5 L1 (`CRATONVM_INTERRUPT_LOCK`): the striped interrupt locks.
#[cfg(test)]
mod nio21_interrupt_lock_tests {
    use super::*;

    /// One `Arc` reaches one stripe from both of its holders, and the kill
    /// switch hands out no guard.
    #[test]
    fn nio21_interrupt_lock_is_keyed_by_the_shared_flag() {
        let mut registry = ThreadRegistry::new();
        let flag = Arc::new(AtomicBool::new(false));
        let other_holder = flag.clone();
        assert_eq!(
            interrupt_lock_stripe(&flag),
            interrupt_lock_stripe(&other_holder)
        );
        registry.set_interrupt_lock_enabled_for_test(true);
        {
            let held = registry.lock_interrupt(&flag);
            assert!(held.is_some());
            let stripe = interrupt_lock_stripe(&other_holder);
            assert!(
                registry.interrupt_locks[stripe].try_lock().is_none(),
                "the other holder of the same flag is excluded"
            );
        }
        assert!(
            registry.lock_interrupt(&other_holder).is_some(),
            "released on drop"
        );
        registry.set_interrupt_lock_enabled_for_test(false);
        assert!(registry.lock_interrupt(&flag).is_none(), "=0 takes no lock");
    }

    /// The stripes spread: 256 distinct flags land in more than a handful of
    /// stripes (a hash on the constant low bits would put them all in one).
    #[test]
    fn nio21_interrupt_lock_stripes_spread() {
        let flags: Vec<Arc<AtomicBool>> =
            (0..256).map(|_| Arc::new(AtomicBool::new(false))).collect();
        let mut seen = [false; INTERRUPT_LOCK_STRIPES];
        for f in &flags {
            seen[interrupt_lock_stripe(f)] = true;
        }
        assert!(seen.iter().filter(|s| **s).count() > 16);
    }
}

/// Interpreter round i3 wave 74, lane L6: the thread-start window of page
/// `i73-L6-unpark-in-the-thread-start-window-is-lost-20261106`.
#[cfg(test)]
mod i74_start_window_tests {
    use super::*;

    /// The window, step by step as `start0` takes it: the entry is
    /// registered with its mirror, the `java_tid` index is recorded, another
    /// thread unparks and interrupts the new thread through both lookups an
    /// `unpark` uses, and only then does the start adopt the states the
    /// thread will park on. The permit and the interrupt are on them, and the
    /// reverse index still answers the same state. Run with the atomic
    /// permit both off and on (set explicitly).
    #[test]
    fn an_unpark_in_the_start_window_reaches_the_state_the_thread_parks_on() {
        for atomic in [false, true] {
            let mut registry = ThreadRegistry::new();
            registry.set_park_atomic_permit_for_test(atomic);
            let tid = ThreadId(9741);
            let mut backing = [0u64; 2];
            // SAFETY: the registry only stores, hashes and compares this address.
            let mirror = unsafe { ObjectRef::from_raw(backing.as_mut_ptr() as *mut u8) };
            registry.register_starting_with_daemon(tid, "i74-start-window", Some(mirror), false);
            registry.set_java_thread_obj_with_tid(tid, mirror, 77_741);

            // Another thread's `unpark(t)` lands in the window.
            let (id, early) = registry.unpark_target_by_java_tid(77_741).expect("indexed");
            assert_eq!(id, tid);
            early.unpark_lock_free();
            registry.set_interrupted(tid, true);

            // The start's park-state step.
            let (park, interrupted) = registry.starting_thread_states(tid);
            registry.set_park_state(tid, park.clone());
            registry.set_interrupted_flag(tid, interrupted.clone());

            assert!(
                Arc::ptr_eq(&park, &early),
                "atomic={atomic}: one state, not two"
            );
            let by_mirror = registry
                .find_park_state_by_thread_obj(mirror)
                .expect("indexed");
            assert!(
                Arc::ptr_eq(&park, &by_mirror),
                "atomic={atomic}: reverse index"
            );
            assert!(
                interrupted.load(Ordering::SeqCst),
                "atomic={atomic}: interrupt kept"
            );
            // The thread's first park finds the permit and returns at once.
            let not_interrupted = AtomicBool::new(false);
            let t0 = std::time::Instant::now();
            park.park_interruptible(Some(std::time::Duration::from_secs(20)), &not_interrupted);
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(10),
                "atomic={atomic}: the window's permit ended the first park"
            );
            assert!(!park.take_permit(), "atomic={atomic}: the park consumed it");
        }
    }

    /// `CRATONVM_PARK_UNTIMED_NO_POLL` (wave 74, set explicitly): with the
    /// switch on, `set_interrupted` alone -- no unpark -- ends a thread's
    /// unpolled untimed park, so no writer of the flag depends on the poll.
    #[test]
    fn set_interrupted_wakes_an_unpolled_park_with_the_switch() {
        let mut registry = ThreadRegistry::new();
        registry.set_park_no_poll_for_test(true);
        assert!(registry.park_no_poll());
        let tid = ThreadId(9743);
        registry.register(tid, "i74-unpolled-parker", None);
        let park = registry.get_park_state(tid).expect("registered");
        park.enable_lock_free();
        let flag = registry.get_interrupted_flag(tid).expect("registered");
        let (tx, rx) = std::sync::mpsc::channel();
        let (park2, flag2) = (park.clone(), flag.clone());
        std::thread::spawn(move || {
            park2.park_interruptible_unpolled(None, &flag2);
            let _ = tx.send(());
        });
        while park.sleeper_count() == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        registry.set_interrupted(tid, true);
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("the interrupt woke the park");
        assert!(flag.load(Ordering::SeqCst), "the flag stays raised");
        assert!(!park.take_permit(), "an interrupt wake gives no permit");
        registry.set_park_no_poll_for_test(false);
        assert!(!registry.park_no_poll());
        registry.set_interrupted(tid, false);
    }

    /// Interpreter round i3 wave 75, lane L6 (`CRATONVM_SLEEP_NO_POLL`): with
    /// that switch on (and `CRATONVM_PARK_UNTIMED_NO_POLL` explicitly off),
    /// `set_interrupted` alone ends a thread's unpolled sleep at once.
    #[test]
    fn set_interrupted_wakes_an_unpolled_sleep_with_the_sleep_switch() {
        let mut registry = ThreadRegistry::new();
        registry.set_park_no_poll_for_test(false);
        registry.set_sleep_no_poll_for_test(true);
        assert!(registry.sleep_no_poll());
        let tid = ThreadId(9744);
        registry.register(tid, "i75-unpolled-sleeper", None);
        let park = registry.get_park_state(tid).expect("registered");
        let flag = registry.get_interrupted_flag(tid).expect("registered");
        let (tx, rx) = std::sync::mpsc::channel();
        let (park2, flag2) = (park.clone(), flag.clone());
        std::thread::spawn(move || {
            let interrupted = park2.sleep_unpolled(std::time::Duration::from_secs(60), &flag2);
            let _ = tx.send(interrupted);
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        registry.set_interrupted(tid, true);
        let interrupted = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the interrupt woke the sleep");
        assert!(interrupted);
        registry.set_sleep_no_poll_for_test(false);
        assert!(!registry.sleep_no_poll());
        registry.set_interrupted(tid, false);
    }

    /// Interpreter round i3 wave 76, lane L6
    /// (`CRATONVM_PARK_WAKE_AFTER_UNLOCK`, set explicitly): `raise_interrupt`
    /// raises the flag WITHOUT waking (an unpolled park waits on) and hands
    /// back the entry's own park state, whose `interrupt_wake_after_unlock`
    /// then ends the park; an unknown id raises nothing.
    #[test]
    fn raise_interrupt_raises_the_flag_and_hands_back_the_state_to_wake() {
        let mut registry = ThreadRegistry::new();
        registry.set_park_wake_after_unlock_for_test(true);
        assert!(registry.park_wake_after_unlock());
        assert!(registry
            .raise_interrupt(ThreadId(9_746_999), true)
            .is_none());
        let tid = ThreadId(9746);
        registry.register(tid, "i76-raise-interrupt", None);
        let park = registry.get_park_state(tid).expect("registered");
        let flag = registry.get_interrupted_flag(tid).expect("registered");
        let (tx, rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (park2, flag2) = (park.clone(), flag.clone());
        std::thread::spawn(move || {
            let _ = ready_tx.send(());
            park2.park_interruptible_unpolled(None, &flag2);
            let _ = tx.send(());
        });
        // Wait for the parker to RUN before the grace period (gci fx): under a
        // loaded full-suite run a bare 20 ms sleep could end before the spawned
        // thread started, so it found the flag already raised at its park's
        // entry and returned -- read here as "raise_interrupt woke it".
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the parker started");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let (raised, monitor) = registry.raise_interrupt(tid, true).expect("registered");
        assert!(Arc::ptr_eq(&raised, &park), "the entry's own state");
        assert!(monitor.is_none(), "not in Object.wait()");
        assert!(flag.load(Ordering::SeqCst), "the flag is raised");
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "raise_interrupt itself wakes nothing"
        );
        raised.interrupt_wake_after_unlock();
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("the one wake ended the park");
        registry.set_park_wake_after_unlock_for_test(false);
        assert!(!registry.park_wake_after_unlock());
        registry.set_interrupted(tid, false);
    }

    /// An id the registry does not know gets fresh, unset states.
    #[test]
    fn an_unregistered_start_gets_fresh_states() {
        let registry = ThreadRegistry::new();
        let (park, interrupted) = registry.starting_thread_states(ThreadId(9742));
        assert!(!park.take_permit());
        assert!(!interrupted.load(Ordering::SeqCst));
    }

    /// `CRATONVM_THREAD_START_ONE_PUBLISH` (interpreter round i3 wave 82,
    /// lane L6): the one read hands out the entry's own `Arc`s, a thread
    /// built from them shares every one with the registry without a `set_*`
    /// call, and an unpark in the start window still reaches its park state.
    /// The switch is set explicitly; the default is not pinned.
    #[test]
    fn one_publish_start_adopts_the_entry_state_without_set_calls() {
        let mut registry = ThreadRegistry::new();
        registry.set_thread_start_one_publish_for_test(true);
        assert!(registry.thread_start_one_publish());
        let tid = ThreadId(9743);
        let mut backing = [0u64; 2];
        // SAFETY: the registry only stores, hashes and compares this address.
        let mirror = unsafe { ObjectRef::from_raw(backing.as_mut_ptr() as *mut u8) };
        registry.register_starting_with_daemon(tid, "i82-one-publish", Some(mirror), false);
        registry.set_java_thread_obj_with_tid(tid, mirror, 77_743);
        let (_, early) = registry.unpark_target_by_java_tid(77_743).expect("indexed");
        early.unpark_lock_free();

        let shared = registry.starting_thread_shared(tid).expect("registered");
        let again = registry.starting_thread_shared(tid).expect("registered");
        assert!(
            Arc::ptr_eq(&shared.park_state, &early),
            "the window's state"
        );
        assert!(Arc::ptr_eq(&shared.interrupted, &again.interrupted));
        assert!(Arc::ptr_eq(&shared.root_snapshot, &again.root_snapshot));
        assert!(Arc::ptr_eq(&shared.frame_trace, &again.frame_trace));
        assert!(Arc::ptr_eq(&shared.vm_state, &again.vm_state));
        assert!(Arc::ptr_eq(&shared.gc_block_state, &again.gc_block_state));

        let thread = crate::threading::jvm_thread::JvmThread::with_shared(tid, "i82", shared);
        assert!(Arc::ptr_eq(&thread.park_state, &again.park_state));
        assert!(Arc::ptr_eq(&thread.interrupted, &again.interrupted));
        assert!(Arc::ptr_eq(&thread.gc_block_state, &again.gc_block_state));
        // A deposit through the thread's own handle is the registry's.
        thread.root_snapshot.lock().push(mirror);
        assert_eq!(registry.root_snapshot_of(tid), vec![mirror]);
        let by_mirror = registry
            .find_park_state_by_thread_obj(mirror)
            .expect("indexed");
        assert!(Arc::ptr_eq(&thread.park_state, &by_mirror), "reverse index");
        assert!(
            thread.park_state.take_permit(),
            "the window's permit is kept"
        );

        assert!(registry.starting_thread_shared(ThreadId(9744)).is_none());
        registry.set_thread_start_one_publish_for_test(false);
        assert!(!registry.thread_start_one_publish());
    }
}

#[cfg(test)]
mod sync619_published_compiled_hold_tests {
    //! JIT round 19 proposal wave 7, lane gc7 (SYNC619-2): a compiled
    //! activation's hold is published only as the lock-stack slot of the
    //! object at that address, merged in report order.
    use super::merge_compiled_hold_entries;
    use crate::types::ObjectRef;

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the merge only compares addresses.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    #[test]
    fn compiled_holds_merge_by_trace_index_and_resolve_to_lock_stack_slots() {
        let keys = [obj(0x1000), obj(0x2000), obj(0x3000)];
        // Interpreter monitors at trace indices 6 and 1 (innermost first).
        let interp = vec![(6, 0), (1, 2)];
        // Compiled holds at index 4 (two blocks, newest first) and 9; one
        // address on no lock-stack slot, one index past the trace.
        let compiled = [
            (9, 0x2000),
            (4, 0x3000),
            (4, 0x1000),
            (4, 0x5000),
            (12, 0x1000),
        ];
        let merged = merge_compiled_hold_entries(interp.clone(), &compiled, &keys, 10);
        assert_eq!(merged, vec![(9, 1), (6, 0), (4, 2), (4, 0), (1, 2)]);
        // Nothing compiled, or nothing that resolves: the interpreter list as is.
        assert_eq!(
            merge_compiled_hold_entries(interp.clone(), &[], &keys, 10),
            interp
        );
        assert_eq!(
            merge_compiled_hold_entries(interp.clone(), &[(3, 0x9000)], &keys, 10),
            interp
        );
    }
}

/// gci i12/vp: the parked-continuation fold filter
/// (`CRATONVM_GC_VT_PARKED_FOLD_FILTER`) and its census. Filter:
/// `cargo test -p cratonvm-vm gci_i12vp`.

/// JIT round 21 lane refs, REFS21-6: `park_credited_os_tids` credits only a
/// blocked, armed park whose deposit scanned its compiled chain.
#[cfg(test)]
mod refs21_park_credit_tests {
    use super::*;

    #[test]
    fn only_a_blocked_armed_and_deposited_park_is_credited() {
        let registry = ThreadRegistry::new();
        let (parked, other) = (ThreadId(1), ThreadId(2));
        registry.register(parked, "parked", None);
        registry.register(other, "other", None);
        registry.publish_os_tid_claim(parked, 6161);
        registry.publish_os_tid_claim(other, 6262);
        registry.mark_stw_ready(parked);
        registry.mark_stw_ready(other);
        let state = |id: ThreadId| {
            registry
                .threads
                .read()
                .get(&id)
                .unwrap()
                .gc_block_state
                .clone()
        };
        assert!(
            registry.park_credited_os_tids().is_empty(),
            "nobody blocked"
        );
        state(parked)
            .park_credit_armed
            .store(true, Ordering::Release);
        assert!(
            registry.park_credited_os_tids().is_empty(),
            "armed but not blocked"
        );
        for id in [parked, other] {
            state(id).in_blocked_region.store(true, Ordering::Release);
        }
        assert!(
            registry.park_credited_os_tids().is_empty(),
            "armed and blocked, but no deposit scanned its chain"
        );
        state(parked)
            .park_credit_deposited
            .store(true, Ordering::Release);
        assert_eq!(registry.park_credited_os_tids(), vec![6161]);
        state(other)
            .park_credit_deposited
            .store(true, Ordering::Release);
        assert_eq!(
            registry.park_credited_os_tids(),
            vec![6161],
            "deposited but not armed is not credited"
        );
        assert_eq!(registry.blocked_os_tids().len(), 2, "both still read");
    }
}

#[cfg(test)]
mod gci_i12vp_tests {
    use super::*;
    use crate::threading::jvm_thread::SlotOrigin;

    // Granule buckets (`(addr >> 20) & 255`): OLD* in 0x57, YOUNG* in 0x12.
    const OLD: usize = 0x0570_0040;
    const OLD_TO: usize = 0x0571_0040;
    const YOUNG: usize = 0x0120_0040;
    const YOUNG_TO: usize = 0x0128_0040;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; a pointer-map key / value only.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn map(pairs: &[(usize, usize)]) -> cratonvm_types::PointerMap {
        let mut pm = cratonvm_types::PointerMap::default();
        for &(k, v) in pairs {
            pm.insert(k, v);
        }
        pm
    }

    /// A blocked entry holding `OLD` in its snapshot and one live slot origin.
    fn blocked_entry(filter_on: bool, unmounted: bool) -> (ThreadRegistry, ThreadId) {
        let mut registry = ThreadRegistry::new();
        registry.set_vt_parked_fold_filter_for_test(filter_on);
        let t = ThreadId(1);
        registry.register(t, "vt-parked", None);
        if unmounted {
            registry.set_mounted(t, false);
        }
        {
            let threads = registry.threads.read();
            let e = threads.get(&t).unwrap();
            e.root_snapshot.lock().push(fake(OLD));
            e.gc_block_state.slot_origins.lock().push(SlotOrigin {
                frame: 0,
                idx: 0,
                is_stack: false,
                live: true,
                orig: OLD,
                cur: OLD,
            });
            e.gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
        (registry, t)
    }

    fn snapshot_of(registry: &ThreadRegistry, t: ThreadId) -> Vec<usize> {
        let threads = registry.threads.read();
        let e = threads.get(&t).unwrap();
        let words = e.root_snapshot.lock().iter().map(|r| r.as_ptr() as usize).collect();
        words
    }

    fn origin_cur(registry: &ThreadRegistry, t: ThreadId) -> Vec<usize> {
        let threads = registry.threads.read();
        let e = threads.get(&t).unwrap();
        let curs = e.gc_block_state.slot_origins.lock().iter().map(|so| so.cur).collect();
        curs
    }

    fn folds(registry: &ThreadRegistry, t: ThreadId) -> u64 {
        let threads = registry.threads.read();
        let e = threads.get(&t).unwrap();
        e.gc_block_state.blocked_folds.load(Ordering::Acquire)
    }

    fn cached(registry: &ThreadRegistry, t: ThreadId) -> Option<FoldAddrFilter> {
        let threads = registry.threads.read();
        let e = threads.get(&t).unwrap();
        e.parked_filter.cached(e.mount_epoch.load(Ordering::Acquire))
    }

    fn epoch(registry: &ThreadRegistry, t: ThreadId) -> u64 {
        let threads = registry.threads.read();
        threads.get(&t).unwrap().mount_epoch.load(Ordering::Acquire)
    }

    fn plant(registry: &ThreadRegistry, t: ThreadId, addr: usize) {
        let threads = registry.threads.read();
        threads.get(&t).unwrap().root_snapshot.lock().push(fake(addr));
    }

    /// The filter is a sound disjointness proof: a shared address always
    /// shares a bit, a wide range saturates it, and the pause's filter covers
    /// the vacated arena as well as the map's keys.
    #[test]
    fn gci_i12vp_fold_addr_filter_proves_disjointness() {
        let mut a = FoldAddrFilter::default();
        a.insert(OLD);
        let mut b = FoldAddrFilter::default();
        b.insert(YOUNG);
        assert!(!a.intersects(&b), "different granules: disjoint");
        b.insert(OLD + 8);
        assert!(a.intersects(&b), "one granule in common");
        // Modulo 256 MiB: the same bucket, so "may intersect" (never a false
        // "disjoint").
        let mut c = FoldAddrFilter::default();
        c.insert(OLD + (256 << 20));
        assert!(a.intersects(&c));
        let mut wide = FoldAddrFilter::default();
        wide.insert_range(0x1000_0000, 0x1000_0000 + (300 << 20));
        assert!(wide.is_full());
        let mut empty_range = FoldAddrFilter::default();
        empty_range.insert_range(0x2000, 0x2000);
        assert_eq!(empty_range, FoldAddrFilter::default());
        // Map keys, not values; and the vacated arena.
        let pm = map(&[(YOUNG, OLD_TO)]);
        let only_keys = pointer_map_fold_filter(&pm, (None, &[]));
        let mut young_only = FoldAddrFilter::default();
        young_only.insert(YOUNG);
        assert!(only_keys.intersects(&young_only));
        assert!(!only_keys.intersects(&a), "a map VALUE is not a lookup key");
        let spans = [(OLD, 64usize)];
        let with_arena = pointer_map_fold_filter(&pm, (Some((YOUNG, YOUNG + 64)), &spans));
        assert!(with_arena.intersects(&a), "the vacated spans are covered");
        // The cache answers only for the epoch it was stored for.
        let cell = ParkedFoldFilter::new();
        assert_eq!(cell.cached(0), None);
        cell.store(7, a);
        assert_eq!(cell.cached(7), Some(a));
        assert_eq!(cell.cached(8), None);
        cell.clear();
        assert_eq!(cell.cached(7), None);
    }

    /// The skip is exact and the epoch is what keeps it so. A word planted
    /// WITHOUT an epoch bump (what the mount rule forbids) is not seen by a
    /// skipped fold -- which proves the skip ran -- and a mount's epoch bump
    /// makes the next fold walk the entry again and remap it.
    #[test]
    fn gci_i12vp_parked_fold_skips_only_a_provably_untouched_continuation() {
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        let (registry, t) = blocked_entry(true, true);
        let young = map(&[(YOUNG, YOUNG_TO)]);
        // Fold 1: no filter yet -- the full fold, which leaves one.
        registry.fold_pointer_map_into_blocked(&young);
        assert_eq!(folds(&registry, t), 1);
        assert_eq!(snapshot_of(&registry, t), vec![OLD]);
        if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
            // The native slots are not walked, so no filter may be cached.
            assert_eq!(cached(&registry, t), None);
            return;
        }
        assert!(cached(&registry, t).is_some(), "an unmounted entry leaves its filter");
        // Plant a young word behind the epoch's back.
        plant(&registry, t, YOUNG);
        // Fold 2: the cached filter (old granule only) is disjoint from the
        // young map: skipped, so the planted word is NOT remapped.
        registry.fold_pointer_map_into_blocked(&young);
        assert_eq!(folds(&registry, t), 2, "a skipped fold still counts as a fold");
        assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG], "the fold was skipped");
        // A mount and an unmount bump the epoch: fold 3 walks the entry.
        registry.set_mounted(t, true);
        registry.set_mounted(t, false);
        assert_eq!(cached(&registry, t), None, "a mount retires the filter");
        registry.fold_pointer_map_into_blocked(&young);
        assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG_TO]);
        let refreshed = cached(&registry, t).expect("refreshed filter");
        let mut young_to = FoldAddrFilter::default();
        young_to.insert(YOUNG_TO);
        assert!(refreshed.intersects(&young_to), "the new word is summarised");
        // Fold 4: an old-granule key (a full / mixed collection) intersects:
        // the full fold runs and moves the old word in every set.
        registry.fold_pointer_map_into_blocked(&map(&[(OLD, OLD_TO)]));
        assert_eq!(snapshot_of(&registry, t), vec![OLD_TO, YOUNG_TO]);
        assert_eq!(origin_cur(&registry, t), vec![OLD_TO]);
        assert_eq!(folds(&registry, t), 4);
    }

    /// A mounted entry (every platform thread, and a continuation blocked
    /// while mounted) is never filtered: it gets no filter and every fold
    /// walks it.
    #[test]
    fn gci_i12vp_mounted_entries_are_never_filtered() {
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        let (registry, t) = blocked_entry(true, false);
        registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
        assert_eq!(cached(&registry, t), None);
        plant(&registry, t, YOUNG);
        registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
        assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG_TO]);
    }

    /// With the switch off (the default) the fold is the old fold: nothing is
    /// cached, nothing skipped.
    #[test]
    fn gci_i12vp_filter_off_is_the_old_fold() {
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        let (mut registry, t) = blocked_entry(false, true);
        // Both filters off (the gather filter is default ON since 2026-10-17
        // and keeps the fold's filter for its own gather).
        registry.set_vt_parked_gather_filter_for_test(false);
        let young = map(&[(YOUNG, YOUNG_TO)]);
        registry.fold_pointer_map_into_blocked(&young);
        assert_eq!(cached(&registry, t), None);
        plant(&registry, t, YOUNG);
        registry.fold_pointer_map_into_blocked(&young);
        assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG_TO]);
    }

    /// Every `set_mounted`, and every replacement of the entry's snapshot or
    /// block-state `Arc`, moves the epoch (and so retires a stored filter).
    #[test]
    fn gci_i12vp_mounts_and_replaced_state_move_the_epoch() {
        let (registry, t) = blocked_entry(true, true);
        let e0 = epoch(&registry, t);
        registry.set_mounted(t, true);
        let e1 = epoch(&registry, t);
        assert!(e1 > e0, "mount");
        registry.set_mounted(t, false);
        let e2 = epoch(&registry, t);
        assert!(e2 > e1, "unmount");
        registry.set_root_snapshot(t, Arc::new(Mutex::new(vec![fake(OLD)])));
        let e3 = epoch(&registry, t);
        assert!(e3 > e2, "new snapshot Arc");
        registry.set_gc_block_state(t, Arc::new(GcBlockState::new()));
        let e4 = epoch(&registry, t);
        assert!(e4 > e3, "new block-state Arc");
    }

    /// The census lines name what the brief asks to narrow with: parked
    /// entries, their words, the skip count and the time.
    #[test]
    fn gci_i12vp_census_lines() {
        let mut g = ParkedGatherCensus::default();
        g.note(false, 10);
        g.note(true, 7);
        g.note(true, 5);
        assert_eq!(
            g.line(30, std::time::Duration::from_micros(42)),
            "[vt-parked-roots] gather alive=3 parked=2 words=22 parked_words=12 roots=30 us=42"
        );
        let f = ParkedFoldCensus {
            blocked: 4,
            parked: 3,
            cached: 2,
            skipped: 1,
            stored: 2,
            parked_words: 9,
            map_filter_nanos: 5_000,
        };
        assert_eq!(
            f.line(100, true, std::time::Duration::from_micros(77)),
            "[vt-parked-roots] fold filter=on map=100 blocked=4 parked=3 cached=2 skipped=1 \
             stored=2 parked_words=9 map_filter_us=5 us=77"
        );
    }

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    /// The text of the item starting at `sig`, up to the next `end`.
    fn body(src: &str, sig: &str, end: &str) -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start..];
        let stop = rest
            .find(end)
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    fn at(text: &str, needle: &str, what: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
    }

    /// The epoch rule's premise, on both mount paths: the mount bumps the
    /// epoch (`set_mounted(tid, true)`) BEFORE the remount's drain rewrites
    /// anything, and the unmount bumps it BEFORE the yield's deposit raises
    /// the flag over new words.
    #[test]
    fn gci_i12vp_mounts_bump_the_epoch_before_the_words_change() {
        let jdk = read("src/vm/jdk_continuation.rs");
        let slice = body(&jdk, "fn run_slice(", "\n}\n");
        let mount = at(&slice, "registry.set_mounted(tid, true);", "run_slice mount");
        let drain = at(&slice, ".check_post_block_gc();", "run_slice drain");
        assert!(mount < drain, "run_slice: mount@{mount} drain@{drain}");
        let arm = &slice[at(
            &slice,
            "VmError::ContinuationYield { .. })) = &result",
            "run_slice yield arm",
        )..];
        let unmount = at(arm, "registry.set_mounted(tid, false);", "run_slice unmount");
        let deposit = at(arm, ".deposit_root_snapshot();", "run_slice deposit");
        assert!(unmount < deposit, "run_slice: unmount@{unmount} deposit@{deposit}");

        let exec = read("src/vm/vm_exec.rs");
        let resume = body(&exec, "fn resume_virtual_continuation(", "\n}\n");
        let mount = at(&resume, ".set_mounted(tid, true);", "bound mount");
        let drain = at(&resume, ".check_post_block_gc();", "bound drain");
        assert!(mount < drain, "resume_virtual_continuation: mount@{mount} drain@{drain}");
        let arm = &resume[at(
            &resume,
            "VmError::ContinuationYield { wake_after_nanos })) =",
            "bound yield arm",
        )..];
        let unmount = at(arm, ".set_mounted(tid, false);", "bound unmount");
        let deposit = at(arm, ".deposit_root_snapshot();", "bound deposit");
        assert!(unmount < deposit, "bound: unmount@{unmount} deposit@{deposit}");
        // And `set_mounted` bumps before it stores.
        let reg = read("src/threading/thread_registry.rs");
        let set = body(&reg, "    pub fn set_mounted(", "\n    }\n");
        let bump = at(&set, "mount_epoch.fetch_add(1", "epoch bump");
        let store = at(&set, "entry.mounted.store(mounted", "mounted store");
        assert!(bump < store, "set_mounted: bump@{bump} store@{store}");
    }
}

/// gci i13/vg: the split root gather (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`),
/// the gather half of
/// `gci-i12vp-parked-continuations-folded-by-every-young-pause-FIXED-20261016`.
/// Filter: `cargo test -p cratonvm-vm gci_i13vg`.
#[cfg(test)]
mod gci_i13vg_tests {
    use super::*;
    use crate::threading::jvm_thread::SlotOrigin;

    // Granules (`(addr >> 20) & 255`): OLD* in 0x57; YOUNG*, MIRROR in 0x12.
    const OLD: usize = 0x0570_0040;
    const OLD_TO: usize = 0x0571_0040;
    const YOUNG: usize = 0x0120_0040;
    const YOUNG_TO: usize = 0x0128_0040;
    const MIRROR: usize = 0x0125_0080;
    /// The collector's young set: granules 0x10-0x12 and 0x20-0x22.
    const YOUNG_SET: [(usize, usize); 2] =
        [(0x0100_0000, 0x0130_0000), (0x0200_0000, 0x0230_0000)];

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; an address only.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn map(pairs: &[(usize, usize)]) -> cratonvm_types::PointerMap {
        let mut pm = cratonvm_types::PointerMap::default();
        for &(k, v) in pairs {
            pm.insert(k, v);
        }
        pm
    }

    fn addrs(v: &[ObjectRef]) -> Vec<usize> {
        v.iter().map(|r| r.as_ptr() as usize).collect()
    }

    /// A blocked entry with a (young) mirror, `OLD` in its snapshot and one
    /// live slot origin; `gather` / `fold` are the two switches.
    fn parked_entry(gather: bool, fold: bool, unmounted: bool) -> (ThreadRegistry, ThreadId) {
        let mut registry = ThreadRegistry::new();
        registry.set_vt_parked_gather_filter_for_test(gather);
        registry.set_vt_parked_fold_filter_for_test(fold);
        let t = ThreadId(1);
        registry.register(t, "vt-parked", Some(fake(MIRROR)));
        if unmounted {
            registry.set_mounted(t, false);
        }
        {
            let threads = registry.threads.read();
            let e = threads.get(&t).unwrap();
            e.root_snapshot.lock().push(fake(OLD));
            e.gc_block_state.slot_origins.lock().push(SlotOrigin {
                frame: 0,
                idx: 0,
                is_stack: false,
                live: true,
                orig: OLD,
                cur: OLD,
            });
            e.gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
        (registry, t)
    }

    fn plant(registry: &ThreadRegistry, t: ThreadId, addr: usize) {
        let threads = registry.threads.read();
        threads.get(&t).unwrap().root_snapshot.lock().push(fake(addr));
    }

    fn snapshot_of(registry: &ThreadRegistry, t: ThreadId) -> Vec<usize> {
        let threads = registry.threads.read();
        let words = addrs(&threads.get(&t).unwrap().root_snapshot.lock());
        words
    }

    fn has_filter(registry: &ThreadRegistry, t: ThreadId) -> bool {
        let threads = registry.threads.read();
        let e = threads.get(&t).unwrap();
        e.parked_filter
            .cached(e.mount_epoch.load(Ordering::Acquire))
            .is_some()
    }

    /// An unmounted, blocked continuation whose filter (left by the last fold,
    /// valid for its epoch) is disjoint from the young set goes to the
    /// old-only section; its young mirror stays a root. No filter, a retired
    /// filter (a mount), or a young word: the whole snapshot stays in `roots`.
    #[test]
    fn gci_i13vg_split_moves_only_a_provably_old_parked_snapshot() {
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        let (registry, t) = parked_entry(true, false, true);
        let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
        assert!(old.is_empty(), "no filter yet");
        assert_eq!(addrs(&roots), vec![OLD, MIRROR]);
        registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
        if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
            // The fold leaves no filter without the native-slot walk.
            assert!(!has_filter(&registry, t));
            return;
        }
        assert!(has_filter(&registry, t), "the gather switch alone keeps the filter");
        let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
        assert_eq!(addrs(&old), vec![OLD], "an old-only parked snapshot");
        assert_eq!(addrs(&roots), vec![MIRROR], "the mirror is a root of every pause");
        // A mount and an unmount retire the filter: back in `roots`.
        registry.set_mounted(t, true);
        registry.set_mounted(t, false);
        let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
        assert!(old.is_empty());
        assert_eq!(addrs(&roots), vec![OLD, MIRROR]);
        // A young word (deposited before the next fold): the fold's new filter
        // names its granule, so the entry stays in `roots`.
        plant(&registry, t, YOUNG);
        registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
        let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
        assert!(old.is_empty(), "a young word keeps the snapshot in the young roots");
        assert_eq!(addrs(&roots), vec![OLD, YOUNG_TO, MIRROR]);
        // A major moves the young word into old space: old-only again.
        registry.fold_pointer_map_into_blocked(&map(&[(YOUNG_TO, OLD_TO)]));
        let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
        assert_eq!(addrs(&old), vec![OLD, OLD_TO]);
        assert_eq!(addrs(&roots), vec![MIRROR]);
    }

    /// A mounted entry gets no filter and an unblocked one is never split,
    /// even with a filter in its cell.
    #[test]
    fn gci_i13vg_mounted_or_unblocked_entries_are_never_split() {
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        let (registry, t) = parked_entry(true, true, false);
        registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
        let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
        assert!(old.is_empty(), "mounted");
        assert_eq!(addrs(&roots), vec![OLD, MIRROR]);

        let (registry, t2) = parked_entry(true, true, true);
        {
            let threads = registry.threads.read();
            let e = threads.get(&t2).unwrap();
            let mut f = FoldAddrFilter::default();
            f.insert(OLD);
            e.parked_filter
                .store(e.mount_epoch.load(Ordering::Acquire), f);
            e.gc_block_state
                .in_blocked_region
                .store(false, Ordering::Release);
        }
        let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
        assert!(old.is_empty(), "not in its blocked region");
        assert_eq!(addrs(&roots), vec![OLD, MIRROR]);
        let _ = t;
    }

    /// The gather switch alone keeps the filters fresh but never skips a
    /// fold: a word planted behind the epoch's back IS remapped (with the
    /// fold switch on, i12/vp's test shows the same fold skipped).
    #[test]
    fn gci_i13vg_the_gather_switch_never_skips_a_fold() {
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        let (registry, t) = parked_entry(true, false, true);
        let young = map(&[(YOUNG, YOUNG_TO)]);
        registry.fold_pointer_map_into_blocked(&young);
        plant(&registry, t, YOUNG);
        registry.fold_pointer_map_into_blocked(&young);
        assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG_TO], "the fold ran");
        // Both switches off: no filter is ever left.
        let (registry, t) = parked_entry(false, false, true);
        registry.fold_pointer_map_into_blocked(&young);
        assert!(!has_filter(&registry, t));
    }

    /// The door's protocol: an armed request is consumed by exactly the next
    /// `collect_all_root_snapshots`, which leaves the old-only section for
    /// `take_parked_old_only`; unarmed gathers are the historical one.
    #[test]
    fn gci_i13vg_an_armed_gather_is_consumed_once() {
        cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
        let (registry, _t) = parked_entry(true, false, true);
        registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
        if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
            return;
        }
        registry.arm_parked_split_gather(&YOUNG_SET);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![MIRROR]);
        assert_eq!(addrs(&registry.take_parked_old_only()), vec![OLD]);
        assert!(registry.take_parked_old_only().is_empty(), "taken once");
        // Not armed: everything in one section.
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![OLD, MIRROR]);
        assert!(registry.take_parked_old_only().is_empty());
        // Armed then taken without a gather: disarmed.
        registry.arm_parked_split_gather(&YOUNG_SET);
        assert!(registry.take_parked_old_only().is_empty());
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![OLD, MIRROR], "the take disarmed it");
    }

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not found"))
    }

    /// The collection door arms before its one registry gather, stages the
    /// section after it, and both before the collection.
    #[test]
    fn gci_i13vg_the_door_arms_before_and_stages_after_its_gather() {
        let src = read("src/runtime/interpreter/gc_and_alloc.rs");
        let start = at(&src, "\nfn run_collection_pause(");
        let rest = &src[start..];
        let body = &rest[..at(rest, "\n}\n")];
        let arm = at(body, "arm_parked_old_only_gather(shared);");
        let gather = at(body, "thread_registry.collect_all_root_snapshots()");
        let stage = at(body, "stage_parked_old_only_roots(shared);");
        let collect = at(body, ".collect_garbage_with_finalizers(");
        assert!(arm < gather && gather < stage && stage < collect, "{arm} {gather} {stage} {collect}");
        assert_eq!(
            body.matches("thread_registry.collect_all_root_snapshots()").count(),
            1,
            "one gather between the arm and the stage"
        );
        // The arm is a disarm-on-drop guard, and the concurrent cycles' own
        // gathers (initial mark / remark doors) never arm: they always see
        // every root.
        assert!(body.contains("let _parked_split_arm = arm_parked_old_only_gather(shared);"));
        for door in ["\npub(super) fn maybe_concurrent_gc_at(", "\npub(super) fn g1_concurrent_mark_cycle("] {
            let s = at(&src, door);
            let rest = &src[s..];
            let b = &rest[..at(rest, "\n}\n")];
            assert!(b.contains("collect_all_root_snapshots()"), "{door}: gathers its own roots");
            assert!(!b.contains("arm_parked"), "{door}: must not split");
        }
    }
}

/// gci i14/ve: the mirror-keyed gather (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`),
/// item 6 of `gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016`.
/// Filter: `cargo test -p cratonvm-vm gci_i14ve`.
#[cfg(test)]
mod gci_i14ve_tests {
    use super::*;

    /// A per-test VM identity for the process-wide `metadata_pin` rows.
    const VM: usize = 0x14e0_0001;
    /// The fake old generation, and addresses inside / outside it.
    const OLD_BASE: usize = 0x4e10_0000;
    const OLD_LEN: usize = 0x0100_0000;
    const MIRROR: usize = OLD_BASE + 0x40;
    const W1: usize = OLD_BASE + 0x1000;
    const W2: usize = OLD_BASE + 0x2000;
    const MOVED: usize = OLD_BASE + 0x8000;
    const YOUNG: usize = 0x0120_0040;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; an address only.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn addrs(v: &[ObjectRef]) -> Vec<usize> {
        v.iter().map(|r| r.as_ptr() as usize).collect()
    }

    /// An unmounted, blocked, precisely deposited entry with an old mirror
    /// and two old words.
    fn parked(mirror: usize, words: &[usize]) -> (ThreadRegistry, ThreadId) {
        let mut registry = ThreadRegistry::new();
        registry.set_vt_parked_mirror_keyed_for_test(true);
        let t = ThreadId(1);
        registry.register(t, "vt-parked", Some(fake(mirror)));
        registry.set_mounted(t, false);
        registry.set_tlab_addr(t, 0x1234_5000);
        {
            let threads = registry.threads.read();
            let e = threads.get(&t).unwrap();
            e.root_snapshot
                .lock()
                .extend(words.iter().map(|&w| fake(w)));
            e.gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
        (registry, t)
    }

    fn arm(registry: &ThreadRegistry, candidates: &[u64]) {
        registry.arm_mirror_keyed_gather(VM, OLD_BASE, OLD_LEN, candidates.to_vec());
    }

    fn rows_of(mirror: usize) -> Vec<usize> {
        cratonvm_types::metadata_pin::roots_for_loader(mirror).unwrap_or_default()
    }

    /// The keyed entry is in no root section -- neither its words nor its
    /// mirror -- and its words are rows owned by the mirror; the next,
    /// unarmed gather is the historical one again.
    #[test]
    fn gci_i14ve_a_keyed_gather_leaves_out_the_parked_frames_and_the_mirror() {
        // Its own VM identity and mirror: the rows are process-wide, and the
        // other tests here file and forget rows under `VM` / `MIRROR`.
        const VM_A: usize = 0x14e0_00a1;
        const MIRROR_A: usize = OLD_BASE + 0x80;
        let (registry, _t) = parked(MIRROR_A, &[W1, W2]);
        registry.arm_mirror_keyed_gather(VM_A, OLD_BASE, OLD_LEN, vec![1]);
        let roots = registry.collect_all_root_snapshots();
        assert!(roots.is_empty(), "no root: {:?}", addrs(&roots));
        let mut rows = rows_of(MIRROR_A);
        rows.sort_unstable();
        assert_eq!(rows, vec![W1, W2], "the frames are the mirror's rows");
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![W1, W2, MIRROR_A], "consumed by one gather");
        cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM_A);
        // Disarmed without a gather: nothing keyed.
        registry.arm_mirror_keyed_gather(VM_A, OLD_BASE, OLD_LEN, vec![1]);
        registry.disarm_mirror_keyed_gather();
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![W1, W2, MIRROR_A]);
        // The deferred pair of the first gather waits for its sweep.
        assert_eq!(registry.mirror_keyed.lock().deferred.len(), 1);
    }

    /// Everything that is not provably a parked, precise, all-old JDK
    /// continuation stays rooted as before.
    #[test]
    fn gci_i14ve_only_a_precise_all_old_deposited_continuation_is_keyed() {
        let expect_rooted = |registry: &ThreadRegistry, what: &str| {
            let roots = registry.collect_all_root_snapshots();
            assert!(
                addrs(&roots).contains(&MIRROR),
                "{what}: the mirror stays a root"
            );
            assert!(registry.mirror_keyed.lock().deferred.is_empty(), "{what}");
        };
        // Not a deposited JDK continuation.
        let (registry, _) = parked(MIRROR, &[W1]);
        arm(&registry, &[2]);
        expect_rooted(&registry, "not a candidate");
        // Mounted.
        let (registry, t) = parked(MIRROR, &[W1]);
        registry.set_mounted(t, true);
        arm(&registry, &[1]);
        expect_rooted(&registry, "mounted");
        // Not in its blocked region.
        let (registry, t) = parked(MIRROR, &[W1]);
        {
            let threads = registry.threads.read();
            let block = &threads.get(&t).unwrap().gc_block_state;
            block.in_blocked_region.store(false, Ordering::Release);
        }
        arm(&registry, &[1]);
        expect_rooted(&registry, "unblocked");
        // A conservative deposit.
        let (registry, t) = parked(MIRROR, &[W1]);
        {
            let threads = registry.threads.read();
            let block = &threads.get(&t).unwrap().gc_block_state;
            block.deposit_conservative.store(true, Ordering::Release);
        }
        arm(&registry, &[1]);
        expect_rooted(&registry, "conservative");
        // A young word.
        let (registry, _) = parked(MIRROR, &[W1, YOUNG]);
        arm(&registry, &[1]);
        expect_rooted(&registry, "young word");
        // A young mirror.
        let (registry, _) = parked(YOUNG, &[W1]);
        arm(&registry, &[1]);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![W1, YOUNG], "young mirror: rooted");
        // No old generation (G1 / ZGC answer (0, 0)).
        let (registry, _) = parked(MIRROR, &[W1]);
        registry.arm_mirror_keyed_gather(VM, 0, 0, vec![1]);
        expect_rooted(&registry, "no old generation");
    }

    /// The verdict: a mirror the pause moved or kept keeps its thread; a
    /// mirror that did not survive retires the entry (dead, TLAB address
    /// withdrawn, listed once for the door), and the list is consumed once.
    #[test]
    fn gci_i14ve_the_sweep_retires_only_an_entry_whose_mirror_died() {
        let empty = cratonvm_types::PointerMap::default();
        // Survived in place.
        let (registry, t) = parked(MIRROR, &[W1]);
        arm(&registry, &[1]);
        let _ = registry.collect_all_root_snapshots();
        assert_eq!(registry.sweep_mirror_keyed_parked(&empty, &|_| true), 0);
        assert!(registry.is_alive(t));
        assert!(registry.take_collected_parked().is_empty());
        // Moved: the map wins over the in-place verdict.
        let mut moved = cratonvm_types::PointerMap::default();
        moved.insert(MIRROR, MOVED);
        arm(&registry, &[1]);
        let _ = registry.collect_all_root_snapshots();
        assert_eq!(registry.sweep_mirror_keyed_parked(&moved, &|_| false), 0);
        assert!(registry.is_alive(t));
        // Died: retired inside the pause.
        arm(&registry, &[1]);
        let _ = registry.collect_all_root_snapshots();
        assert_eq!(registry.sweep_mirror_keyed_parked(&empty, &|a| a != MIRROR), 1);
        assert!(!registry.is_alive(t), "retired");
        {
            let threads = registry.threads.read();
            let e = threads.get(&t).unwrap();
            assert_eq!(e.tlab_addr.load(Ordering::Acquire), 0, "TLAB address withdrawn");
            assert_eq!(e.jvm_thread_addr.load(Ordering::Acquire), 0);
        }
        assert_eq!(registry.take_collected_parked(), vec![t]);
        assert!(registry.take_collected_parked().is_empty(), "taken once");
        assert_eq!(
            registry.sweep_mirror_keyed_parked(&empty, &|_| false),
            0,
            "the deferred list is consumed by one sweep"
        );
        // A dead entry is never gathered again.
        arm(&registry, &[1]);
        assert!(registry.collect_all_root_snapshots().is_empty());
        cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);
    }

    /// A mounted entry (impossible with a dead mirror: a mount needs a
    /// carrier that holds it) is left alone rather than retired under a
    /// running slice.
    #[test]
    fn gci_i14ve_a_remounted_entry_is_never_retired() {
        let empty = cratonvm_types::PointerMap::default();
        let (registry, t) = parked(MIRROR, &[W1]);
        arm(&registry, &[1]);
        let _ = registry.collect_all_root_snapshots();
        registry.set_mounted(t, true);
        assert_eq!(registry.sweep_mirror_keyed_parked(&empty, &|_| false), 0);
        assert!(registry.is_alive(t));
        assert!(registry.take_collected_parked().is_empty());
        cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);
    }

    /// The contended fallback: the door counts the pause and arms nothing,
    /// so the gather roots everything; the count is cumulative per VM.
    #[test]
    fn gci_i14ve_a_contended_arm_keys_nothing_and_is_counted() {
        let (registry, _t) = parked(MIRROR, &[W1]);
        assert_eq!(registry.mirror_keyed_skipped_contended(), 0);
        registry.note_mirror_keyed_contended();
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![W1, MIRROR], "nothing armed: rooted");
        assert!(registry.mirror_keyed.lock().deferred.is_empty());
        registry.note_mirror_keyed_contended();
        assert_eq!(registry.mirror_keyed_skipped_contended(), 2);
        // The door's arm takes the non-blocking read and counts its failure.
        let src = read("src/runtime/interpreter/gc_and_alloc.rs");
        let arm = body(&src, "\nfn arm_parked_mirror_keyed_gather(");
        let try_read = at(arm, ".try_deposited_jdk_continuation_ids()");
        let counted = at(arm, "registry.note_mirror_keyed_contended();");
        assert!(try_read < counted);
        assert!(!arm.contains(".deposited_jdk_continuation_ids()"), "no blocking read in the pause");
    }

    #[test]
    fn gci_i14ve_mirror_keyed_dead_reads_the_map_first() {
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(MIRROR, MOVED);
        let deferred = [(ThreadId(1), MIRROR), (ThreadId(2), W1), (ThreadId(3), W2)];
        let dead = mirror_keyed_dead(&deferred, &map, &|a| a == W2);
        assert_eq!(dead, vec![(ThreadId(2), W1)]);
    }

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not found"))
    }

    fn body<'a>(src: &'a str, start: &str) -> &'a str {
        let rest = &src[at(src, start)..];
        &rest[..at(rest, "\n}\n")]
    }

    /// The door arms before its scan, the verdict runs inside the pause
    /// before the blocked fold reads the words, and the states are released
    /// only after the world is.
    #[test]
    fn gci_i14ve_arm_verdict_and_release_sit_in_pause_order() {
        let src = read("src/runtime/interpreter/gc_and_alloc.rs");
        let door = body(&src, "\nfn run_collection_pause(");
        let arm = at(door, "let _parked_keyed_arm = arm_parked_mirror_keyed_gather(shared);");
        let scan = at(door, "collect_roots_registry_appended(");
        let gather = at(door, "thread_registry.collect_all_root_snapshots()");
        let collect = at(door, ".collect_garbage_with_finalizers(");
        // gci i22/dr: the door lends the pause's one relocation set.
        let roots = at(
            door,
            "update_all_roots(shared, thread, &result.pointer_map, &relocation_targets);",
        );
        let release = at(door, "complete_gc(result.pointer_map);");
        let retire = at(door, "retire_collected_continuations(shared, collected);");
        assert!(
            arm < scan && scan < gather && gather < collect && collect < roots
                && roots < release && release < retire,
            "{arm} {scan} {gather} {collect} {roots} {release} {retire}"
        );
        // The licence: Generational, no concurrent cycle, no debugger, the
        // loader-metadata licence.
        let lic = body(&src, "\nfn parked_mirror_keyed_licensed(");
        for needle in [
            "is_generational()",
            "ConcurrentGcPhase::Idle",
            "debugger_observes_locals(shared)",
            "conditional_loader_metadata(shared)",
        ] {
            assert!(lic.contains(needle), "{needle}");
        }
        let gc = read("src/memory/gc.rs");
        let update = body(&gc, "\npub fn update_all_roots(");
        let sweep = at(update, ".sweep_mirror_keyed_parked(pointer_map, &|addr| in_place.survived(addr));");
        let window = at(update, "drop(in_place);");
        let early = at(update, "if pointer_map.is_empty() {");
        let fold = at(update, ".fold_pointer_map_into_blocked_audited(");
        let remap = at(update, ".update_thread_objs_after_gc(");
        assert!(
            sweep < window && window < early && early < fold && fold < remap,
            "{sweep} {window} {early} {fold} {remap}"
        );
    }
}

/// gci i15/vh: the G1 half of the mirror-keyed gather
/// (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`), item 6 of
/// `gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016`.
/// Filter: `cargo test -p cratonvm-vm gci_i15vh`.
#[cfg(test)]
mod gci_i15vh_tests {
    use super::*;

    /// A per-test VM identity for the process-wide `metadata_pin` rows (not
    /// the i14/ve tests' ones).
    const VM: usize = 0x15f0_0001;
    /// Two fake G1 `Old` regions, out of order (the arm sorts), and
    /// addresses inside / outside them.
    const OLD_A: (usize, usize) = (0x5e40_0000, 0x5e50_0000);
    const OLD_B: (usize, usize) = (0x5e20_0000, 0x5e30_0000);
    const MIRROR: usize = OLD_A.0 + 0x40;
    const W1: usize = OLD_B.0 + 0x1000;
    const W2: usize = OLD_A.0 + 0x2000;
    /// Between the two regions: eden / survivor / humongous / free.
    const NOT_OLD: usize = 0x5e30_0040;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; an address only.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn addrs(v: &[ObjectRef]) -> Vec<usize> {
        v.iter().map(|r| r.as_ptr() as usize).collect()
    }

    /// An unmounted, blocked, precisely deposited entry (registry id 1).
    fn parked(mirror: usize, words: &[usize]) -> (ThreadRegistry, ThreadId) {
        let mut registry = ThreadRegistry::new();
        registry.set_vt_parked_mirror_keyed_for_test(true);
        let t = ThreadId(1);
        registry.register(t, "vt-parked-g1", Some(fake(mirror)));
        registry.set_mounted(t, false);
        registry.set_tlab_addr(t, 0x1234_5000);
        {
            let threads = registry.threads.read();
            let e = threads.get(&t).unwrap();
            e.root_snapshot
                .lock()
                .extend(words.iter().map(|&w| fake(w)));
            e.gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
        (registry, t)
    }

    fn arm(registry: &ThreadRegistry) {
        registry.arm_mirror_keyed_gather_ranges(VM, vec![OLD_A, OLD_B], vec![1]);
    }

    /// The remark judgement with a no-op marker (the row fixed point is
    /// tested on its own below).
    fn judge(registry: &ThreadRegistry, is_live: &dyn Fn(usize) -> bool) -> usize {
        registry.judge_mirror_keyed_parked_after_mark(is_live, &mut |_: &[usize]| {})
    }

    /// The row fixed point: a live owner's values are marked, an owner they
    /// make live has its values marked next, a dead owner's never are; each
    /// owner once; nothing live -> nothing marked.
    #[test]
    fn gci_i15vh_live_owner_rows_are_marked_to_a_fixed_point() {
        const A: usize = 0xa000;
        const B: usize = 0xb000;
        const C: usize = 0xc000;
        // addr-keyed: local to this test (a synthetic heap).
        let mut rows: rustc_hash::FxHashMap<usize, Vec<usize>> = rustc_hash::FxHashMap::default();
        rows.insert(A, vec![0xa010, B]);
        rows.insert(B, vec![0xb010]);
        rows.insert(C, vec![0xc010]);
        let marked = std::cell::RefCell::new(rustc_hash::FxHashSet::<usize>::default());
        marked.borrow_mut().insert(A);
        let calls = std::cell::Cell::new(0usize);
        let is_live = |a: usize| marked.borrow().contains(&a);
        let handed = mark_live_row_values_to_fixpoint(&rows, &is_live, &mut |values: &[usize]| {
            calls.set(calls.get() + 1);
            marked.borrow_mut().extend(values.iter().copied());
        });
        assert_eq!(handed, 3, "A's two values, then B's one");
        assert_eq!(calls.get(), 2, "two rounds: A, then B");
        let m = marked.borrow();
        assert!(m.contains(&0xa010) && m.contains(&B) && m.contains(&0xb010));
        assert!(!m.contains(&C) && !m.contains(&0xc010), "a dead owner's values stay unmarked");
        drop(m);
        let none = mark_live_row_values_to_fixpoint(&rows, &|_| false, &mut |_: &[usize]| {
            panic!("nothing is live")
        });
        assert_eq!(none, 0);
    }

    /// A keyed G1 gather makes the next judgement run the row fixed point
    /// first (once), and a mirror only another live owner's rows reach turns
    /// live inside it, before its own verdict.
    #[test]
    fn gci_i15vh_the_judgement_closes_the_rows_before_its_verdict() {
        const VM_R: usize = 0x15f0_00a2;
        const MIRROR_R: usize = OLD_A.0 + 0x400;
        const HOLDER: usize = OLD_B.0 + 0x400;
        let (registry, t) = parked(MIRROR_R, &[W1]);
        registry.arm_mirror_keyed_gather_ranges(VM_R, vec![OLD_A, OLD_B], vec![1]);
        let _ = registry.collect_all_root_snapshots();
        // A live owner (say, another parked thread's mirror) whose rows name
        // this thread's mirror.
        cratonvm_types::metadata_pin::add_metadata_pins(VM_R, &[(HOLDER, MIRROR_R)]);
        let marked = std::cell::RefCell::new(rustc_hash::FxHashSet::<usize>::default());
        marked.borrow_mut().insert(HOLDER);
        let is_live = |a: usize| (a != MIRROR_R && a != W1) || marked.borrow().contains(&a);
        let dead = registry.judge_mirror_keyed_parked_after_mark(&is_live, &mut |v: &[usize]| {
            marked.borrow_mut().extend(v.iter().copied());
        });
        assert_eq!(dead, 0, "reached through a live owner's rows: kept");
        assert!(marked.borrow().contains(&W1), "and its own frames are marked");
        assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
        assert!(registry.is_alive(t));
        // The flag was taken: an unkeyed judgement marks nothing.
        let n = registry.judge_mirror_keyed_parked_after_mark(&|_| true, &mut |_: &[usize]| {
            panic!("no keyed gather since the last judgement")
        });
        assert_eq!(n, 0);
        cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM_R);
    }

    #[test]
    fn gci_i15vh_sorted_ranges_answer_membership() {
        let ranges = [(0x100, 0x200), (0x300, 0x400)];
        for (addr, inside) in [
            (0x0ff, false),
            (0x100, true),
            (0x1ff, true),
            (0x200, false),
            (0x2ff, false),
            (0x300, true),
            (0x3ff, true),
            (0x400, false),
        ] {
            assert_eq!(addr_in_sorted_ranges(&ranges, addr), inside, "{addr:#x}");
        }
        assert!(!addr_in_sorted_ranges(&[], 0x100));
    }

    /// G1's arm keys an entry whose mirror and every word lie in `Old`
    /// regions (any of them, whatever the caller's order); a word outside
    /// every `Old` region keeps the entry rooted, mirror included.
    #[test]
    fn gci_i15vh_g1_ranges_key_only_an_all_old_entry() {
        // Its own VM identity and mirror: the rows are process-wide, and the
        // other tests here file and forget rows under `VM` / `MIRROR`.
        const VM_K: usize = 0x15f0_00a1;
        const MIRROR_K: usize = OLD_A.0 + 0x80;
        let (registry, _t) = parked(MIRROR_K, &[W1, W2]);
        registry.arm_mirror_keyed_gather_ranges(VM_K, vec![OLD_A, OLD_B], vec![1]);
        let roots = registry.collect_all_root_snapshots();
        assert!(roots.is_empty(), "keyed: no root at all: {:?}", addrs(&roots));
        assert_eq!(registry.mirror_keyed_deferred_mirrors(), vec![MIRROR_K]);
        let mut rows = cratonvm_types::metadata_pin::roots_for_loader(MIRROR_K).unwrap_or_default();
        rows.sort_unstable();
        assert_eq!(rows, vec![W1, W2], "the frames are the mirror's rows");
        cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM_K);
        registry.forget_mirror_keyed_deferred();

        let (registry, _t) = parked(MIRROR, &[W1, NOT_OLD]);
        arm(&registry);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![W1, NOT_OLD, MIRROR], "a non-Old word: rooted");
        assert!(registry.mirror_keyed_deferred_mirrors().is_empty());

        let (registry, _t) = parked(NOT_OLD, &[W1]);
        arm(&registry);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![W1, NOT_OLD], "a non-Old mirror: rooted");

        // No Old region at all: nothing is old, nothing keyed.
        let (registry, _t) = parked(MIRROR, &[W1]);
        registry.arm_mirror_keyed_gather_ranges(VM, Vec::new(), vec![1]);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(addrs(&roots), vec![W1, MIRROR]);
    }

    /// The remark's verdict: judged on the completed mark, retired only
    /// after a TRUSTED cleanup; an untrusted one keeps the thread, and a live
    /// mirror is never retired. Each step consumes what it read.
    #[test]
    fn gci_i15vh_judge_then_settle_retires_only_on_a_trusted_mark() {
        // Unmarked mirror, untrusted cleanup: kept.
        let (registry, t) = parked(MIRROR, &[W1]);
        arm(&registry);
        let _ = registry.collect_all_root_snapshots();
        assert_eq!(judge(&registry, &|a| a != MIRROR), 1);
        assert!(registry.mirror_keyed_deferred_mirrors().is_empty(), "judged once");
        assert_eq!(registry.settle_mirror_keyed_parked_after_mark(false), 0);
        assert!(registry.is_alive(t), "an untrusted mark proves nothing");
        assert!(registry.take_collected_parked().is_empty());
        assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0, "settled once");
        assert!(registry.is_alive(t));

        // Marked mirror, trusted cleanup: kept.
        arm(&registry);
        let _ = registry.collect_all_root_snapshots();
        assert_eq!(judge(&registry, &|_| true), 0);
        assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
        assert!(registry.is_alive(t));

        // Unmarked mirror, trusted cleanup: retired inside the pause.
        arm(&registry);
        let _ = registry.collect_all_root_snapshots();
        assert_eq!(judge(&registry, &|a| a != MIRROR), 1);
        assert!(registry.is_alive(t), "the judgement alone retires nothing");
        assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 1);
        assert!(!registry.is_alive(t), "retired");
        {
            let threads = registry.threads.read();
            let e = threads.get(&t).unwrap();
            assert_eq!(e.tlab_addr.load(Ordering::Acquire), 0, "TLAB address withdrawn");
        }
        assert_eq!(registry.take_collected_parked(), vec![t]);
        // A dead entry is never gathered again.
        arm(&registry);
        assert!(registry.collect_all_root_snapshots().is_empty());
        cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);
    }

    /// A pair with no verdict (the initial mark, a lost or unwinding remark)
    /// is forgotten: the thread stays, nothing is collected. A remounted
    /// entry is never retired even by a trusted verdict.
    #[test]
    fn gci_i15vh_unjudged_pairs_are_forgotten_and_a_mount_wins() {
        let (registry, t) = parked(MIRROR, &[W1]);
        arm(&registry);
        let _ = registry.collect_all_root_snapshots();
        registry.forget_mirror_keyed_deferred();
        assert_eq!(judge(&registry, &|_| false), 0);
        assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
        assert!(registry.is_alive(t));
        // Judged, then forgotten before the settle (the guard's drop).
        arm(&registry);
        let _ = registry.collect_all_root_snapshots();
        assert_eq!(judge(&registry, &|_| false), 1);
        registry.forget_mirror_keyed_deferred();
        assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
        assert!(registry.is_alive(t));
        // Remounted between the judgement and the settle (cannot happen
        // inside one pause; the settle still re-checks).
        arm(&registry);
        let _ = registry.collect_all_root_snapshots();
        assert_eq!(judge(&registry, &|_| false), 1);
        registry.set_mounted(t, true);
        assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
        assert!(registry.is_alive(t));
        assert!(registry.take_collected_parked().is_empty());
        // The Generational sweep never sees a G1 pair: nothing is deferred
        // once the remark settled.
        let empty = cratonvm_types::PointerMap::default();
        assert_eq!(registry.sweep_mirror_keyed_parked(&empty, &|_| false), 0);
        cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);
    }

    /// Step 10b's copy: one occurrence per keyed thread leaves the plain
    /// scan's result; any further occurrence is a real root and stays; a
    /// mirror that is not there removes nothing.
    #[test]
    fn gci_i15vh_strip_takes_exactly_one_copy_per_keyed_mirror() {
        const M: usize = 0x7000;
        const N: usize = 0x7100;
        const X: usize = 0x7200;
        let mut roots = vec![fake(M), fake(X), fake(M), fake(N)];
        assert_eq!(strip_keyed_mirror_registry_roots(&mut roots, &[M]), 1);
        assert_eq!(addrs(&roots), vec![X, M, N], "the second copy is a real root");
        assert_eq!(strip_keyed_mirror_registry_roots(&mut roots, &[0x7300]), 0);
        assert_eq!(addrs(&roots), vec![X, M, N]);
        assert_eq!(strip_keyed_mirror_registry_roots(&mut roots, &[M, N, N]), 2);
        assert_eq!(addrs(&roots), vec![X], "N owed twice, present once");
        assert_eq!(strip_keyed_mirror_registry_roots(&mut roots, &[]), 0);
    }

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not found"))
    }

    fn body<'a>(src: &'a str, start: &str) -> &'a str {
        let rest = &src[at(src, start)..];
        &rest[..at(rest, "\n}\n")]
    }

    /// Both G1 marking scans key between their PLAIN scan (whose step 10b
    /// copy the strip removes) and their gather; the initial mark forgets its
    /// pairs before seeding; the remark judges inside the callback after the
    /// keep closure, settles right after the heap call (before the
    /// address-keyed sweep) and releases the states after the world. No
    /// evacuation door keys on G1, and the licence is G1-and-marking only.
    #[test]
    fn gci_i15vh_the_g1_marking_doors_key_judge_and_settle_in_pause_order() {
        let src = read("src/runtime/interpreter/gc_and_alloc.rs");
        let im = body(&src, "\npub(super) fn g1_concurrent_mark_cycle(");
        let scan = at(im, "with_class_unload_marking(|| collect_roots(shared, thread))");
        let key = at(im, "let g1_keyed = g1_key_parked_for_mark(shared);");
        let gather = at(im, "thread_registry.collect_all_root_snapshots()");
        let strip = at(im, "g1_keyed.strip_registry_mirrors(&mut roots);");
        let forget = at(im, "g1_keyed.forget_without_verdict();");
        let seed = at(im, "g1_mark_roots(&stw, &all_roots)");
        assert!(
            scan < key && key < gather && gather < strip && strip < forget && forget < seed,
            "{scan} {key} {gather} {strip} {forget} {seed}"
        );
        assert!(!im.contains("judge_mirror_keyed"), "the initial mark never judges");

        let rm = body(&src, "\npub(super) fn g1_final_remark_cleanup(");
        let scan = at(rm, "with_class_unload_marking(|| collect_roots(shared, thread))");
        let key = at(rm, "let g1_keyed = g1_key_parked_for_mark(shared);");
        let gather = at(rm, "thread_registry.collect_all_root_snapshots()");
        let strip = at(rm, "g1_keyed.strip_registry_mirrors(&mut roots);");
        let keep_arm = at(rm, "g1_remark_mark_kept_closure(shared, &keep);");
        let judge = at(rm, ".judge_mirror_keyed_parked_after_mark(is_live, &mut |values: &[usize]| ");
        let marker = at(rm, "g1_remark_mark_kept_closure(shared, values)");
        let jni = at(rm, "clear_weak_global_refs_unmarked(shared, is_live, &keep)");
        assert!(judge < marker && marker < jni, "the rows close, then the verdicts read them");
        let cleanup_point = at(rm, "cleanup_at.set(Some(std::time::Instant::now()));");
        let heap = at(rm, ".g1_final_remark_and_cleanup(&stw, &all_roots, Some(&mut process));");
        let settle = at(rm, ".settle_mirror_keyed_parked_after_mark(keyed_trusted);");
        let sweep = at(rm, "sweep_address_keyed_tables(shared);");
        let release = at(rm, ".complete_gc(cratonvm_types::PointerMap::default());");
        let retire = at(rm, "retire_collected_continuations(shared, collected);");
        assert!(
            scan < key && key < gather && gather < strip && strip < heap,
            "{scan} {key} {gather} {strip} {heap}"
        );
        assert!(
            keep_arm < judge && judge < cleanup_point && cleanup_point < heap,
            "{keep_arm} {judge} {cleanup_point} {heap}"
        );
        assert!(
            heap < settle && settle < sweep && sweep < release && release < retire,
            "{heap} {settle} {sweep} {release} {retire}"
        );
        assert!(rm.contains("let keyed_trusted = completed && g1_parked_keyed_mark_trusted(shared);"));

        let lic = body(&src, "\nfn g1_parked_mirror_keyed_licensed(");
        for needle in [
            "is_g1()",
            "g1_is_marking_active()",
            "jdk_continuations()",
            "!crate::runtime::jvmti::debugger_observes_locals(shared)",
        ] {
            assert!(lic.contains(needle), "{needle}");
        }
        let key_fn = body(&src, "\nfn g1_key_parked_for_mark(");
        assert!(key_fn.contains(".try_deposited_jdk_continuation_ids()"), "never blocks");
        assert!(!key_fn.contains(".deposited_jdk_continuation_ids()"), "no blocking read");
        let forget = at(key_fn, "registry.forget_mirror_keyed_deferred();");
        let armed = at(key_fn, "registry.arm_mirror_keyed_gather_ranges(");
        assert!(forget < armed, "no earlier pair reaches this scan's strip or verdict");
        // The Generational door's licence still refuses G1.
        let gen = body(&src, "\nfn parked_mirror_keyed_licensed(");
        assert!(gen.contains("is_generational()"));
    }
}

/// gci i16/mr: the G1 remark closes every `metadata_pin` row over the
/// completed mark on the default path
/// (`gci-i15vh-g1-marking-scan-rows-filed-while-the-marker-runs-FIXED-20261017`).
/// Filter: `cargo test -j 5 -p cratonvm-vm gci_i16mr`.
#[cfg(test)]
mod gci_i16mr_tests {
    use super::*;

    /// A model marker: pop gray, mark, follow the rows of `tables` (the side
    /// tables its batch captured) from each newly marked object.
    fn drain(
        // addr-keyed: local to this test (a synthetic heap).
        marked: &std::cell::RefCell<rustc_hash::FxHashSet<usize>>,
        gray: &mut Vec<usize>,
        // addr-keyed: local to this test (a synthetic heap).
        tables: &rustc_hash::FxHashMap<usize, Vec<usize>>,
    ) {
        while let Some(obj) = gray.pop() {
            if marked.borrow_mut().insert(obj) {
                if let Some(values) = tables.get(&obj) {
                    gray.extend(values.iter().copied());
                }
            }
        }
    }

    /// The window, interleaved: a batch captured the side tables before the
    /// scan filed the rows and blackened the owner; the scan then files them
    /// and the remark's drain (current tables, nothing gray) never rescans the
    /// black owner, so its value stays unmarked. The remark's closure marks
    /// it, and a value that owns a row itself (a loader object behind a
    /// static) brings that row along; a dead owner's value stays unmarked.
    #[test]
    fn gci_i16mr_a_row_filed_after_its_owner_was_scanned_is_closed_by_the_remark() {
        const OWNER: usize = 0x16_6000;
        const VALUE: usize = 0x16_6100;
        const DEEP: usize = 0x16_6200;
        const DEAD_OWNER: usize = 0x16_6300;
        const DEAD_VALUE: usize = 0x16_6400;
        let marked = std::cell::RefCell::new(rustc_hash::FxHashSet::<usize>::default());
        // addr-keyed: local to this test (a synthetic heap).
        let captured: rustc_hash::FxHashMap<usize, Vec<usize>> = rustc_hash::FxHashMap::default();
        drain(&marked, &mut vec![OWNER], &captured);
        // addr-keyed: local to this test (a synthetic heap).
        let mut rows: rustc_hash::FxHashMap<usize, Vec<usize>> = rustc_hash::FxHashMap::default();
        rows.insert(OWNER, vec![VALUE]);
        rows.insert(VALUE, vec![DEEP]);
        rows.insert(DEAD_OWNER, vec![DEAD_VALUE]);
        drain(&marked, &mut Vec::new(), &rows);
        assert!(marked.borrow().contains(&OWNER));
        assert!(!marked.borrow().contains(&VALUE), "the window: the black owner is not rescanned");

        let is_live = |a: usize| marked.borrow().contains(&a);
        let handed = mark_live_row_values_to_fixpoint(&rows, &is_live, &mut |values: &[usize]| {
            let mut gray = values.to_vec();
            drain(&marked, &mut gray, &rows);
        });
        assert_eq!(handed, 2, "OWNER's value, then VALUE's (already marked by the drain)");
        let m = marked.borrow();
        assert!(m.contains(&VALUE) && m.contains(&DEEP), "the row closed over the live owner");
        assert!(!m.contains(&DEAD_OWNER) && !m.contains(&DEAD_VALUE), "a dead owner adds nothing");
    }

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("not found: {needle}"))
    }

    fn body<'a>(src: &'a str, start: &str) -> &'a str {
        let rest = &src[at(src, start)..];
        &rest[..at(rest, "\n}\n")]
    }

    /// Source pin: the remark callback closes the rows FIRST (before the
    /// reference rows, class unloading, the keyed verdicts and the JNI weak
    /// clear read the bitmap), on every G1 remark (no switch), through the
    /// fixed point and the heap's own resurrection drain.
    #[test]
    fn gci_i16mr_the_g1_remark_closes_the_rows_before_any_verdict() {
        let src = read("src/runtime/interpreter/gc_and_alloc.rs");
        let rm = body(&src, "\npub(super) fn g1_final_remark_cleanup(");
        let close = at(rm, "let _ = g1_remark_close_metadata_rows(shared, is_live);");
        let refs = at(rm, "let keep = g1_remark_process_references(shared, is_live);");
        let judge = at(rm, ".judge_mirror_keyed_parked_after_mark(is_live, ");
        let jni = at(rm, "clear_weak_global_refs_unmarked(shared, is_live, &keep)");
        assert!(close < refs && refs < judge && judge < jni, "{close} {refs} {judge} {jni}");
        let f = body(&src, "\nfn g1_remark_close_metadata_rows(");
        for needle in [
            "is_g1()",
            // gci i18/nr (gci i17/rv finding 9): this VM's rows only.
            "metadata_pin::vm_snapshot(shared.vm_identity)",
            "mark_live_row_values_to_fixpoint(",
            "g1_remark_mark_kept_closure(shared, values)",
        ] {
            assert!(f.contains(needle), "{needle}");
        }
        assert!(!f.contains("runtime_flag"), "default path: no switch");
    }
}

/// gci i17/hw (`gci-i15orch-g1-parked-virtual-thread-holder-graph-reads-wrong-on-the-default-path-RETIRED-UNFIXED-20261018`):
/// the parked-continuation chain (`fold_one_blocked_chain` per pause, one
/// lookup per frame slot at the remount's `check_post_block_gc`) keeps a
/// frame-held address on ITS object across the G1 shapes the page asks about:
/// a mixed pause moving the Old holder, the evacuation-failure drain's
/// composed map (a pass-2 copy lands on a pass-1 key, the recycled
/// address another frame object vacated), and a later move of both.
#[cfg(test)]
mod gci_i17hw_tests {
    use super::*;

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: compared and rewritten by address only; never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn addrs(snapshot: &[ObjectRef]) -> Vec<usize> {
        snapshot.iter().map(|r| r.as_ptr() as usize).collect()
    }

    fn map(pairs: &[(usize, usize)]) -> cratonvm_types::PointerMap {
        pairs.iter().copied().collect()
    }

    /// The remount applies the composed fixup with ONE lookup per slot
    /// (`Frame::update_frame_refs_mapped`); this is that lookup.
    fn wake(fixup: &cratonvm_types::PointerMap, frame_word: usize) -> usize {
        fixup.get(&frame_word).copied().unwrap_or(frame_word)
    }

    #[test]
    fn gci_i17hw_a_parked_holder_follows_its_object_through_a_drain_collision() {
        // The parked frame holds the holder P at 0x1000 and another object Q
        // at 0x2000; the deposit's snapshot names both.
        let (p0, q0) = (0x1000usize, 0x2000usize);
        let mut fixup = cratonvm_types::PointerMap::default();
        let mut snapshot = vec![obj(p0), obj(q0)];

        // Pause 1 (mixed): the Old holder moves.
        assert_eq!(
            fold_one_blocked_chain(&mut fixup, &mut snapshot, &map(&[(p0, 0x3000)])),
            (0, 1)
        );
        assert_eq!(addrs(&snapshot), vec![0x3000, q0]);

        // Pause 2 (evacuation failure + drain, the composed map): pass 1
        // copied Q away from 0x2000 and self-forwarded P; the drain copied P
        // INTO 0x2000 (a region pass 1 freed). A pass-2-only key rides along.
        let composed = map(&[(q0, 0x5000), (0x3000, q0), (0x6000, 0x6100)]);
        assert_eq!(
            fold_one_blocked_chain(&mut fixup, &mut snapshot, &composed),
            (1, 1)
        );
        assert_eq!(addrs(&snapshot), vec![q0, 0x5000]);
        assert_eq!(fixup.get(&p0), Some(&q0), "P's chain advanced once");
        assert_eq!(fixup.get(&q0), Some(&0x5000), "Q seeded from its frame word");

        // Pause 3: P moves again (off the recycled address); Q stays.
        assert_eq!(
            fold_one_blocked_chain(&mut fixup, &mut snapshot, &map(&[(q0, 0x7000)])),
            (1, 0)
        );
        assert_eq!(addrs(&snapshot), vec![0x7000, 0x5000]);

        // The remount: each frame word lands on its own object.
        assert_eq!(wake(&fixup, p0), 0x7000, "the holder");
        assert_eq!(wake(&fixup, q0), 0x5000, "the other object");
        // And the snapshot the next pause marks from agrees with the frames.
        assert_eq!(
            addrs(&snapshot),
            vec![wake(&fixup, p0), wake(&fixup, q0)]
        );
    }

    #[test]
    fn gci_i17hw_many_pauses_keep_the_frame_and_snapshot_views_equal() {
        // One holder moved by every pause; from the second pause on, the
        // OTHER frame object moves onto the address the holder vacated two
        // pauses earlier (recycled to-space) -- in pause 2 that is the
        // holder's own FRAME word, the chain key (the ABA shape). The frame
        // words and the snapshot never disagree, which is the property whose
        // failure the page's `weak=split` diagnostic would show.
        let p_at = |k: usize| 0x10_000usize + k * 0x100;
        let other0 = 0x90_000usize;
        let mut fixup = cratonvm_types::PointerMap::default();
        let mut snapshot = vec![obj(p_at(0)), obj(other0)];
        let mut other = other0;
        for k in 1..=20usize {
            let mut pairs = vec![(p_at(k - 1), p_at(k))];
            if k >= 2 {
                pairs.push((other, p_at(k - 2)));
            }
            let _ = fold_one_blocked_chain(&mut fixup, &mut snapshot, &map(&pairs));
            if k >= 2 {
                other = p_at(k - 2);
            }
            assert_eq!(addrs(&snapshot), vec![p_at(k), other], "pause {k}");
            assert_eq!(wake(&fixup, p_at(0)), p_at(k), "pause {k}: holder");
            assert_eq!(wake(&fixup, other0), other, "pause {k}: other");
        }
    }
}

/// gci i18/hw: a virtual thread's continuation deposits precisely on G1
/// (`continuation_deposit_is_precise`, `CRATONVM_G1_VT_PRECISE_DEPOSIT`).
#[cfg(test)]
mod gci_i18hw_tests {
    use super::*;

    fn src(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    /// The text of `fn <sig>` up to its closing 4-space-indented brace line.
    fn method_body(text: &str, sig: &str) -> String {
        let start = text.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &text[start..];
        let end = rest
            .find("\n    \u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..end].to_string()
    }

    #[test]
    fn gci_i18hw_only_a_continuation_on_g1_with_the_switch_deposits_precisely() {
        assert!(continuation_deposit_is_precise(true, true, true));
        // Generational / ZGC keep the probes (non-moving JIT-active sweep on Gen).
        assert!(!continuation_deposit_is_precise(false, true, true));
        // A platform thread may hold a lost-tag local from a compiled callee.
        assert!(!continuation_deposit_is_precise(true, false, true));
        // `CRATONVM_G1_VT_PRECISE_DEPOSIT=0`: the historical deposit.
        assert!(!continuation_deposit_is_precise(true, true, false));
        assert!(!continuation_deposit_is_precise(false, false, false));
    }

    #[test]
    fn gci_i18hw_the_switch_is_read_once_and_defaults_on() {
        // The field is the flag's default-on reading (ON unless set to an
        // off word, which nothing in the test environment does).
        assert_eq!(
            ThreadRegistry::new().g1_vt_precise_deposit(),
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_G1_VT_PRECISE_DEPOSIT")
        );
    }

    /// The blocking deposit (the yield's `deposit_root_snapshot`) consults the
    /// rule before it records `deposit_conservative` and before any probe, so
    /// the flag the mirror-keyed gather and the guards read agrees with the
    /// words deposited.
    #[test]
    fn gci_i18hw_the_blocking_deposit_applies_the_rule_before_its_probes() {
        let exec = src("src/vm/vm_exec.rs");
        let deposit = method_body(&exec, "fn deposit_root_snapshot_inner_with(");
        let rule = deposit
            .find("continuation_deposit_is_precise(")
            .expect("the deposit asks continuation_deposit_is_precise");
        let flag = deposit
            .find(".deposit_conservative")
            .expect("the deposit records deposit_conservative");
        let all_live = deposit
            .find("let all_live_locals")
            .expect("the dead-locals rule");
        let probe = deposit
            .find("scan_locals_conservative(")
            .expect("the conservative locals probe");
        assert!(rule < flag && rule < all_live && rule < probe);
        // gci i19/vd: the continuation input is now
        // `continuation_is_interpreted_only` (kind Virtual AND never entered
        // compiled code), not the bare `ThreadKind::Virtual` match.
        for term in [
            "conservative_locals_enabled()",
            "is_g1()",
            "continuation_is_interpreted_only(self.thread)",
            "g1_vt_precise_deposit()",
        ] {
            assert!(
                deposit[..flag].contains(term),
                "the rule lost its `{term}` input"
            );
        }
    }
}

/// gci i19/vd: the precise G1 continuation deposit covers exactly the
/// continuations that never entered compiled code
/// (`continuation_is_interpreted_only`,
/// `GcBlockState::continuation_entered_compiled`).
#[cfg(test)]
mod gci_i19vd_tests {
    use super::*;

    fn src(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    /// The text of the top-level `fn` at `sig` up to its closing column-0
    /// brace line.
    fn fn_body(text: &str, sig: &str) -> String {
        let start = text.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &text[start..];
        let end = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..end].to_string()
    }

    #[test]
    fn gci_i19vd_only_a_continuation_that_never_ran_compiled_code_is_precise() {
        let mut thread = crate::threading::JvmThread::new(ThreadId(19), "gci-i19vd");
        // A platform thread may hold a lost-tag local from a compiled callee.
        assert!(!continuation_is_interpreted_only(&thread));
        thread.kind = crate::threading::ThreadKind::Virtual;
        // A fresh continuation: the flag starts down.
        assert!(!thread
            .gc_block_state
            .continuation_entered_compiled
            .load(Ordering::Relaxed));
        assert!(continuation_is_interpreted_only(&thread));
        assert!(continuation_deposit_is_precise(
            true,
            continuation_is_interpreted_only(&thread),
            true
        ));
        // A door let it into compiled code: conservative from now on, on G1
        // with the switch on too.
        thread
            .gc_block_state
            .continuation_entered_compiled
            .store(true, Ordering::Relaxed);
        assert!(!continuation_is_interpreted_only(&thread));
        assert!(!continuation_deposit_is_precise(
            true,
            continuation_is_interpreted_only(&thread),
            true
        ));
    }

    /// The one check both interpreter->compiled entries pass records the
    /// breach before its debug assertion, outside any `cfg`, so a release
    /// build that a door let through still deposits conservatively.
    #[test]
    fn gci_i19vd_the_compiled_entry_check_records_the_breach_in_every_build() {
        let bridge = src("src/runtime/interpreter/jit_bridge.rs");
        let fun = fn_body(&bridge, "\npub(super) fn assert_no_compiled_continuation(");
        let gate = fun
            .find("if thread_is_continuation(thread)")
            .expect("the record is gated on the thread kind");
        let store = fun
            .find(".continuation_entered_compiled")
            .expect("the check records the breach");
        let check = fun.find("debug_assert!(").expect("the debug assertion stays");
        assert!(gate < store && store < check);
        assert!(!fun.contains("cfg(debug_assertions)"));
        assert!(fun.contains(".store(true,"));
    }
}

#[cfg(test)]
mod w95_l6_continuation_one_publish_tests {
    //! Interpreter round i4 wave 95, lane L6 (`CRATONVM_VTHREAD_ONE_PUBLISH`,
    //! `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`): the one-publish registration,
    //! mount and death of a JDK continuation thread, the kept dead count, and
    //! the per-thread count of unstructured monitor enters.
    use super::*;
    use crate::threading::jvm_thread::ThreadSharedArcs;

    fn mirror(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the registry keys and compares addresses.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn walked_dead(registry: &ThreadRegistry) -> usize {
        registry
            .threads
            .read()
            .values()
            .filter(|e| !e.alive.load(Ordering::Acquire))
            .count()
    }

    #[test]
    fn a_prepared_registration_is_complete_in_one_publish() {
        let registry = ThreadRegistry::new();
        let tid = registry.next_thread_id();
        let arcs = ThreadSharedArcs::fresh();
        let park = arcs.park_state.clone();
        let flag = arcs.interrupted.clone();
        let vthread = mirror(0x1_0000);
        registry.register_prepared_continuation_thread(tid, "vt", vthread, 77, arcs, 0x40, 0x80);
        assert!(registry.is_alive(tid));
        assert!(registry.is_daemon(tid));
        assert_eq!(registry.java_thread_obj(tid), Some(vthread));
        assert_eq!(registry.find_thread_id_by_java_tid(77), Some(tid));
        assert!(Arc::ptr_eq(&registry.get_park_state(tid).unwrap(), &park));
        assert!(Arc::ptr_eq(&registry.get_interrupted_flag(tid).unwrap(), &flag));
        assert!(Arc::ptr_eq(
            &registry.find_park_state_by_thread_obj(vthread).unwrap(),
            &park
        ));
        assert_eq!(registry.own_jvm_thread_addr(tid), Some(0x80));
        // Not STW-ready until its first slice marks it.
        let threads = registry.threads.read();
        let entry = threads.get(&tid).unwrap();
        assert!(!entry.stw_ready.load(Ordering::Acquire));
        assert_eq!(entry.tlab_addr.load(Ordering::Acquire), 0x40);
    }

    #[test]
    fn a_mount_publishes_and_answers_the_mirror() {
        let registry = ThreadRegistry::new();
        let tid = registry.next_thread_id();
        let vthread = mirror(0x2_0000);
        registry.register_prepared_continuation_thread(
            tid,
            "vt",
            vthread,
            0,
            ThreadSharedArcs::fresh(),
            0,
            0,
        );
        registry.set_mounted(tid, false);
        assert_eq!(
            registry.mount_continuation_thread(tid, 0x100, 0x200),
            Some(Some(vthread))
        );
        assert!(registry.is_mounted(tid));
        assert_eq!(registry.own_jvm_thread_addr(tid), Some(0x100));
        assert_eq!(registry.mount_continuation_thread(ThreadId(9999), 1, 2), None);
    }

    #[test]
    fn the_kept_dead_count_matches_the_walk_and_bounds_the_tail() {
        let registry = ThreadRegistry::new();
        let slack = 64;
        let total = TERMINATED_THREAD_ENTRY_CAP + 3 * slack;
        for i in 0..total {
            let tid = registry.next_thread_id();
            registry.register_prepared_continuation_thread(
                tid,
                "vt",
                mirror(0x10_0000 + i * 0x100),
                1000 + i as u64,
                ThreadSharedArcs::fresh(),
                0,
                0,
            );
            let retired = registry.retire_continuation_thread(tid).unwrap();
            assert!(!registry.is_alive(tid));
            assert_eq!(registry.own_jvm_thread_addr(tid), None);
            registry.finish_retired_continuation_thread(tid, &retired, slack);
            let dead = registry.dead_entries.load(Ordering::Relaxed);
            assert_eq!(dead, walked_dead(&registry));
            assert!(dead <= TERMINATED_THREAD_ENTRY_CAP + slack, "{dead}");
            // A second death of the same id (the newest, never purged) is not
            // counted again.
            assert!(registry.retire_continuation_thread(tid).is_some());
            assert_eq!(
                registry.dead_entries.load(Ordering::Relaxed),
                walked_dead(&registry)
            );
        }
        // The oldest ids were purged, with their `tid` index entries.
        assert_eq!(registry.find_thread_id_by_java_tid(1000), None);
        assert!(registry.find_thread_id_by_java_tid(1000 + total as u64 - 1).is_some());
        // The default death keeps the same count.
        let platform = registry.next_thread_id();
        registry.register(platform, "p", None);
        registry.mark_dead(platform);
        assert_eq!(
            registry.dead_entries.load(Ordering::Relaxed),
            walked_dead(&registry)
        );
        assert_eq!(registry.count() - registry.alive_count(), walked_dead(&registry));
    }

    /// Wave 96 (lane L6): a collection that moves the mirror between the
    /// two halves of a death re-keys the park index to the new address; the
    /// second half used to look the entry up by the old address only and
    /// leave the dead state in the index. The default death (`mark_dead`)
    /// had the same window.
    #[test]
    fn a_death_that_races_a_mirror_move_leaves_no_park_index_entry() {
        // The index itself: `find_park_state_by_thread_obj` falls back to a
        // walk of the entries, dead ones included, by design.
        fn indexed(registry: &ThreadRegistry, obj: ObjectRef) -> bool {
            registry
                .thread_obj_to_park
                .lock()
                .contains_key(&(obj.as_ptr() as usize))
        }
        let registry = ThreadRegistry::new();
        let tid = registry.next_thread_id();
        let old = mirror(0x30_0000);
        let new = mirror(0x31_0000);
        registry.register_prepared_continuation_thread(
            tid,
            "vt",
            old,
            0,
            ThreadSharedArcs::fresh(),
            0,
            0,
        );
        let retired = registry.retire_continuation_thread(tid).unwrap();
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(0x30_0000usize, 0x31_0000usize);
        registry.update_thread_objs_after_gc(&pointer_map);
        assert!(indexed(&registry, new));
        registry.finish_retired_continuation_thread(tid, &retired, 64);
        assert!(!indexed(&registry, new));
        assert!(!indexed(&registry, old));

        // `mark_dead` with the stale address read before the move: the
        // registry's own read is under its lock, so drive the helper the way
        // `mark_dead` calls it after a re-key.
        let platform = registry.next_thread_id();
        let p_old = mirror(0x40_0000);
        let p_new = mirror(0x41_0000);
        registry.register(platform, "p", Some(p_old));
        let state = registry.get_park_state(platform).unwrap();
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(0x40_0000usize, 0x41_0000usize);
        registry.update_thread_objs_after_gc(&pointer_map);
        assert!(indexed(&registry, p_new));
        registry.drop_dead_park_index_entry(p_old, &state, true);
        assert!(!indexed(&registry, p_new));
        // A later death of the same thread (a joiner's) does not search, and
        // a newer thread's state at the address is never dropped.
        let other = registry.next_thread_id();
        registry.register(other, "o", Some(p_new));
        registry.drop_dead_park_index_entry(p_old, &state, false);
        registry.drop_dead_park_index_entry(p_new, &state, true);
        assert!(indexed(&registry, p_new));
    }

    #[test]
    fn unstructured_monitor_enters_are_counted_per_thread() {
        let registry = ThreadRegistry::new();
        let tid = registry.next_thread_id();
        registry.register(tid, "t", None);
        assert_eq!(registry.unstructured_monitor_enters_of(tid), 0);
        registry.note_unstructured_monitor_enter(tid);
        registry.note_unstructured_monitor_enter(tid);
        registry.note_unstructured_monitor_exit(tid);
        assert_eq!(registry.unstructured_monitor_enters_of(tid), 1);
        registry.note_unstructured_monitor_exit(tid);
        // Never below zero.
        registry.note_unstructured_monitor_exit(tid);
        assert_eq!(registry.unstructured_monitor_enters_of(tid), 0);
        // An unknown id never reads "holds nothing".
        assert_eq!(registry.unstructured_monitor_enters_of(ThreadId(4242)), u32::MAX);
    }
}

#[cfg(test)]
mod gci_i20vh_tests {
    use super::*;

    /// gci i20/vh: a tid qualifies only when a blocked entry on it holds a
    /// deferred chain record and none lacks one; the answer is sorted and
    /// deduplicated.
    #[test]
    fn gci_i20vh_deferred_chain_peers_need_every_blocked_entry_on_the_tid_deferred() {
        assert!(deferred_chain_peer_tids(Vec::new()).is_empty());
        assert_eq!(deferred_chain_peer_tids(vec![(7, true)]), vec![7]);
        assert!(deferred_chain_peer_tids(vec![(7, false)]).is_empty());
        // A mounted virtual thread shares its carrier's tid: one entry without
        // a record keeps the tid out, in either order.
        assert!(deferred_chain_peer_tids(vec![(7, true), (7, false)]).is_empty());
        assert!(deferred_chain_peer_tids(vec![(7, false), (7, true)]).is_empty());
        assert_eq!(
            deferred_chain_peer_tids(vec![(9, true), (3, true), (9, true), (5, false)]),
            vec![3, 9]
        );
    }

    /// gci i20/vh: on a registry with no blocked entry the roster answer is
    /// empty, and an entry that is not in a blocked region never qualifies.
    #[test]
    fn gci_i20vh_an_unblocked_registry_has_no_deferred_chain_peer() {
        let registry = ThreadRegistry::new();
        assert!(registry.blocked_os_tids_with_deferred_jit_chain().is_empty());
        let tid = registry.next_thread_id();
        registry.register(tid, "t", None);
        assert!(registry.blocked_os_tids_with_deferred_jit_chain().is_empty());
    }

    /// gci i24/jp: a row counts as holding a record only when the record is
    /// one the collector's walk will resolve (the owner is not dropped); a
    /// dropped owner's row keeps its tid OUT, like a row without a record,
    /// even when another blocked row on the tid holds a record.
    #[test]
    fn gci_i24jp_a_dropped_owner_row_never_demotes_its_tid() {
        assert!(deferred_chain_row(false, true));
        assert!(!deferred_chain_row(true, true), "the resolve skips a dropped owner");
        assert!(!deferred_chain_row(false, false));
        assert!(!deferred_chain_row(true, false));
        let rows = vec![
            (7, deferred_chain_row(false, true)),
            (7, deferred_chain_row(true, true)),
            (9, deferred_chain_row(false, true)),
        ];
        assert_eq!(deferred_chain_peer_tids(rows), vec![9]);
    }

    /// gci i24/jp: the roster reads the row through `deferred_chain_row` with
    /// the owner flag (a source pin; LF line endings assumed).
    #[test]
    fn gci_i24jp_the_roster_applies_the_owner_filter() {
        let src = include_str!("thread_registry.rs");
        let head = concat!("pub(crate) fn blocked_os_tids_with_deferred_jit_chain", "(&self)");
        let at = src.find(head).expect("the roster");
        let body = &src[at..at + src[at..].find("\n    }\n").expect("its end")];
        assert!(body.contains(concat!("deferred_chain_row", "(")), "{body}");
        assert!(body.contains(concat!("gc_block_state.owner_dropped", "()")), "{body}");
    }
}

#[cfg(test)]
mod ki3_covered_chain_tests {
    //! ki3 (`CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY`): the roster of
    //! blocked peers whose window's deposit covered their compiled chain.
    use super::*;

    /// A blocked entry qualifies when its deposit walked the chain itself
    /// (`deposit_walked_jit_chain`) as when it deferred it; an unblocked
    /// entry, a blocked one without either, and a dropped owner never do.
    #[test]
    fn ki3_a_blocked_peer_whose_deposit_walked_its_chain_is_covered() {
        let registry = ThreadRegistry::new();
        assert!(registry.blocked_os_tids_with_covered_jit_chain().is_empty());
        let tid = ThreadId(1);
        registry.register(tid, "parked", None);
        let state = {
            let threads = registry.threads.read();
            let entry = threads.get(&tid).unwrap();
            entry.os_tid.store(4242, Ordering::Release);
            entry.gc_block_state.clone()
        };
        state
            .deposit_walked_jit_chain
            .store(true, Ordering::Release);
        assert!(
            registry.blocked_os_tids_with_covered_jit_chain().is_empty(),
            "not blocked"
        );
        state.in_blocked_region.store(true, Ordering::Release);
        assert_eq!(
            registry.blocked_os_tids_with_covered_jit_chain(),
            vec![4242]
        );
        // The eager twin never makes the deferred roster (G1's demotion) answer.
        assert!(registry
            .blocked_os_tids_with_deferred_jit_chain()
            .is_empty());
        state
            .deposit_walked_jit_chain
            .store(false, Ordering::Release);
        assert!(registry.blocked_os_tids_with_covered_jit_chain().is_empty());
    }

    /// The roster reads the walked flag and the record through
    /// `deferred_chain_row`, with the owner filter (a source pin; LF line
    /// endings assumed).
    #[test]
    fn ki3_the_covered_roster_applies_the_owner_filter() {
        let src = include_str!("thread_registry.rs");
        let head = concat!(
            "pub(crate) fn blocked_os_tids_with_covered_jit_chain",
            "(&self)"
        );
        let at = src.find(head).expect("the roster");
        let body = &src[at..at + src[at..].find("\n    }\n").expect("its end")];
        assert!(body.contains(concat!("deferred_chain_row", "(")), "{body}");
        assert!(
            body.contains(concat!("gc_block_state.owner_dropped", "()")),
            "{body}"
        );
        assert!(
            body.contains(concat!("deposit_walked_jit_chain", "\n")),
            "{body}"
        );
        assert!(
            body.contains(concat!("deferred_jit_chain", ".lock()")),
            "{body}"
        );
    }
}

#[cfg(test)]
mod gci_i21pr_tests {
    //! gci i21/pr (`CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`): the collection
    //! door's gather leaves out the initiator's own snapshot, which
    //! `collect_roots` step 11 already pushed, and nothing else.
    use super::*;

    const INIT_WORD: usize = 0x0570_0040;
    const INIT_WORD_2: usize = 0x0570_0080;
    const PEER_WORD: usize = 0x0571_0040;
    const INIT_MIRROR: usize = 0x0125_0080;
    const PEER_MIRROR: usize = 0x0125_00c0;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; an address only.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn sorted(v: &[ObjectRef]) -> Vec<usize> {
        let mut a: Vec<usize> = v.iter().map(|r| r.as_ptr() as usize).collect();
        a.sort_unstable();
        a
    }

    /// An initiator (its snapshot `Arc` shared with its entry, as
    /// `set_root_snapshot` shares the `JvmThread`'s) and one peer.
    fn two_threads() -> (ThreadRegistry, Arc<Mutex<Vec<ObjectRef>>>) {
        let registry = ThreadRegistry::new();
        let init = ThreadId(1);
        let peer = ThreadId(2);
        registry.register(init, "initiator", Some(fake(INIT_MIRROR)));
        registry.register(peer, "peer", Some(fake(PEER_MIRROR)));
        let snapshot = Arc::new(Mutex::new(vec![fake(INIT_WORD), fake(INIT_WORD_2)]));
        registry.set_root_snapshot(init, Arc::clone(&snapshot));
        {
            let threads = registry.threads.read();
            threads
                .get(&peer)
                .unwrap()
                .root_snapshot
                .lock()
                .push(fake(PEER_WORD));
        }
        (registry, snapshot)
    }

    /// The armed gather leaves out exactly the initiator's snapshot words;
    /// its mirror and the peer's words and mirror stay. Consumed once.
    #[test]
    fn gci_i21pr_an_armed_gather_skips_only_the_initiator_snapshot() {
        let (registry, snapshot) = two_threads();
        registry.arm_initiator_snapshot_skip(&snapshot);
        let roots = registry.collect_all_root_snapshots();
        let mut want = vec![INIT_MIRROR, PEER_MIRROR, PEER_WORD];
        want.sort_unstable();
        assert_eq!(sorted(&roots), want);
        assert_eq!(registry.disarm_initiator_snapshot_skip(), Some(2));
        assert_eq!(registry.disarm_initiator_snapshot_skip(), None, "taken once");
        // The next gather is the historical one.
        let roots = registry.collect_all_root_snapshots();
        let mut want = vec![INIT_MIRROR, PEER_MIRROR, PEER_WORD, INIT_WORD, INIT_WORD_2];
        want.sort_unstable();
        assert_eq!(sorted(&roots), want);
    }

    /// Equality: the armed gather plus the initiator's snapshot (what step 11
    /// pushes) is the unarmed gather, as a multiset -- the root set the
    /// collector sees is unchanged, only the duplicate is gone.
    #[test]
    fn gci_i21pr_step_11_plus_the_armed_gather_is_the_unarmed_gather() {
        let (registry, snapshot) = two_threads();
        let unarmed = registry.collect_all_root_snapshots();
        registry.arm_initiator_snapshot_skip(&snapshot);
        let mut armed = registry.collect_all_root_snapshots();
        let _ = registry.disarm_initiator_snapshot_skip();
        armed.extend(snapshot.lock().iter().copied());
        assert_eq!(sorted(&armed), sorted(&unarmed));
    }

    /// Only the entry SHARING the armed `Arc` is skipped: a different `Arc`
    /// with the same words (another thread that happens to hold the same
    /// objects) skips nothing.
    #[test]
    fn gci_i21pr_an_equal_but_unshared_snapshot_is_not_skipped() {
        let (registry, snapshot) = two_threads();
        let unarmed = registry.collect_all_root_snapshots();
        let lookalike = Arc::new(Mutex::new(snapshot.lock().clone()));
        registry.arm_initiator_snapshot_skip(&lookalike);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(sorted(&roots), sorted(&unarmed));
        assert_eq!(registry.disarm_initiator_snapshot_skip(), Some(0));
    }

    /// An arm disarmed before any gather (the door's guard on an unwind)
    /// leaves the next gather historical and reports `None`.
    #[test]
    fn gci_i21pr_a_disarmed_arm_leaves_the_next_gather_whole() {
        let (registry, snapshot) = two_threads();
        let unarmed = registry.collect_all_root_snapshots();
        registry.arm_initiator_snapshot_skip(&snapshot);
        assert_eq!(registry.disarm_initiator_snapshot_skip(), None);
        let roots = registry.collect_all_root_snapshots();
        assert_eq!(sorted(&roots), sorted(&unarmed));
        assert_eq!(registry.disarm_initiator_snapshot_skip(), None);
    }

    /// The split gather (`collect_all_root_snapshots_split`) neither reads
    /// nor consumes the arm: only the door's plain gather does.
    #[test]
    fn gci_i21pr_the_split_gather_does_not_consume_the_arm() {
        let (registry, snapshot) = two_threads();
        registry.arm_initiator_snapshot_skip(&snapshot);
        let (roots, old_only) = registry.collect_all_root_snapshots_split(&[]);
        assert!(old_only.is_empty());
        assert!(sorted(&roots).contains(&INIT_WORD), "the split gather is whole");
        let roots = registry.collect_all_root_snapshots();
        assert!(!sorted(&roots).contains(&INIT_WORD), "still armed for the plain gather");
        assert_eq!(registry.disarm_initiator_snapshot_skip(), Some(2));
    }

    /// gcj j3/x2 (fail-first: before the arm recorded its thread, the
    /// foreign gather consumed it and left the initiator's words out of a
    /// root set where they are not duplicated): a plain gather on another
    /// thread is whole, leaves the arm, and is counted; the arming thread's
    /// own gather still skips.
    #[test]
    fn gcj3x2_a_foreign_threads_gather_neither_consumes_nor_applies_the_arm() {
        let (registry, snapshot) = two_threads();
        let unarmed = registry.collect_all_root_snapshots();
        registry.arm_initiator_snapshot_skip(&snapshot);
        let foreign = std::thread::scope(|s| {
            s.spawn(|| registry.collect_all_root_snapshots())
                .join()
                .unwrap()
        });
        assert_eq!(sorted(&foreign), sorted(&unarmed), "the foreign gather is whole");
        assert_eq!(registry.initiator_skip_foreign_gathers(), 1);
        let roots = registry.collect_all_root_snapshots();
        assert!(!sorted(&roots).contains(&INIT_WORD), "the arming thread still skips");
        assert_eq!(registry.disarm_initiator_snapshot_skip(), Some(2));
        // Unarmed gathers on another thread are not counted.
        std::thread::scope(|s| {
            s.spawn(|| registry.collect_all_root_snapshots());
        });
        assert_eq!(registry.initiator_skip_foreign_gathers(), 1);
    }

    /// gcj j3/x2 R10 (fail-first: before the split arm recorded its thread,
    /// the foreign gather took it): a plain gather on another thread leaves
    /// an armed parked split for the arming thread, and is counted.
    #[test]
    fn gcj3x2_r10_a_foreign_threads_gather_leaves_the_parked_split_armed() {
        let (registry, _snapshot) = two_threads();
        registry.arm_parked_split_gather(&[(0x1000, 0x2000)]);
        std::thread::scope(|s| {
            s.spawn(|| registry.collect_all_root_snapshots());
        });
        assert_eq!(registry.parked_split_foreign_gathers(), 1);
        assert!(registry.parked_split.lock().armed.is_some(), "still armed");
        let _ = registry.collect_all_root_snapshots();
        assert!(
            registry.parked_split.lock().armed.is_none(),
            "the arming thread took it"
        );
        let _ = registry.take_parked_old_only();
        // Unarmed gathers on another thread are not counted.
        std::thread::scope(|s| {
            s.spawn(|| registry.collect_all_root_snapshots());
        });
        assert_eq!(registry.parked_split_foreign_gathers(), 1);
    }

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not found"))
    }

    /// Source pin: the collection door arms (behind the switch) before the
    /// scan whose step 11 pushes the initiator's snapshot, and before its one
    /// gather; it is the only arm site, and the concurrent cycles' doors
    /// (which have no step 11 in front of their gather) never arm.
    #[test]
    fn gci_i21pr_only_the_collection_door_arms_before_its_scan() {
        let src = read("src/runtime/interpreter/gc_and_alloc.rs");
        let start = at(&src, "\nfn run_collection_pause(");
        let rest = &src[start..];
        let body = &rest[..at(rest, "\n\u{7d}\n")];
        let arm = at(body, "let _initiator_skip_arm = arm_initiator_snapshot_skip(shared, thread);");
        let scan = at(body, "collect_roots_registry_appended(shared, thread)");
        let gather = at(body, "thread_registry.collect_all_root_snapshots()");
        assert!(arm < scan && scan < gather, "{arm} {scan} {gather}");
        assert_eq!(src.matches("registry.arm_initiator_snapshot_skip(").count(), 1);
        let helper = &src[at(&src, "\nfn arm_initiator_snapshot_skip<")..];
        let helper = &helper[..at(helper, "\n\u{7d}\n")];
        assert!(helper.contains("if !dedupe_initiator_snapshot_enabled(shared)"));
        assert!(helper.contains("&thread.root_snapshot"));
        let gate = &src[at(&src, "\nfn dedupe_initiator_snapshot_enabled(")..];
        let gate = &gate[..at(gate, "\n\u{7d}\n")];
        assert!(gate.contains("runtime_flag_on(\"CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT\")"));
        for door in ["\npub(super) fn maybe_concurrent_gc_at(", "\npub(super) fn g1_concurrent_mark_cycle("] {
            let s = at(&src, door);
            let rest = &src[s..];
            let b = &rest[..at(rest, "\n\u{7d}\n")];
            assert!(!b.contains("arm_initiator_snapshot_skip"), "{door}: must not skip");
        }
    }
}

#[cfg(test)]
mod i133_l7_attach_order_tests {
    //! Interpreter round i6 wave 133, lane L7 (`CRATONVM_THREADS_ATTACH_ORDER`):
    //! the attach-ordered listing behind `enumerate_threads` is `ThreadId`
    //! order whatever the map's hash order, drops a dead thread without
    //! reordering the rest, and the gate defaults on.
    use super::*;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; an address only.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    #[test]
    fn i133_l7_live_threads_list_in_attach_order() {
        let registry = ThreadRegistry::new();
        // Registered out of id order, and enough of them that the hash
        // order is not the id order by accident.
        let ids: [u64; 12] = [7, 0, 11, 3, 9, 1, 5, 10, 2, 8, 4, 6];
        for id in ids {
            let addr = 0x0133_0000 + (id as usize) * 0x40;
            registry.register(ThreadId(id), &format!("t{id}"), Some(fake(addr)));
        }
        registry.mark_dead(ThreadId(5));
        let got: Vec<usize> = registry
            .alive_thread_objects_in_id_order()
            .iter()
            .map(|r| r.as_ptr() as usize)
            .collect();
        let want: Vec<usize> = (0u64..12)
            .filter(|&id| id != 5)
            .map(|id| 0x0133_0000 + (id as usize) * 0x40)
            .collect();
        assert_eq!(got, want);
        // The hash-order listing holds the same set.
        let mut unordered: Vec<usize> = registry
            .alive_thread_objects(usize::MAX)
            .iter()
            .map(|r| r.as_ptr() as usize)
            .collect();
        unordered.sort_unstable();
        assert_eq!(unordered, want);
    }

    #[test]
    fn i133_l7_attach_order_gate_defaults_on() {
        if std::env::var_os("CRATONVM_THREADS_ATTACH_ORDER").is_none() {
            assert!(ThreadRegistry::new().threads_attach_order());
        }
    }
}

/// [`NATIVE_SLOTS_DUPLICATE_CAPTURES`], for a test. Test-only since JIT round
/// 22 wave 7 (lane review): no census line ever read it through this getter.
#[cfg(test)]
pub fn native_slot_duplicate_captures() -> u64 {
    NATIVE_SLOTS_DUPLICATE_CAPTURES.load(Ordering::Relaxed)
}
