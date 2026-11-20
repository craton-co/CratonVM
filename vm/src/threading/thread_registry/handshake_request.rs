#[allow(unused_imports)]
use super::*;


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
pub(in crate::threading::thread_registry) fn initiator_counted_verdict(
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
    pub(in crate::threading::thread_registry) contended_monitor: Mutex<Option<ObjectRef>>,
    /// When that block began, for `getBlockedTime()`.
    pub(in crate::threading::thread_registry) contended_started: Mutex<Option<Instant>>,
    /// Monitors this thread currently owns: `getLockedMonitors()`, and a GC
    /// root set that is remapped across a moving collection. Written only by
    /// the owning thread, without an RMW on the common path — see
    /// `JmxLockStack`.
    pub(in crate::threading::thread_registry) locked: JmxLockStack,
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
    pub(in crate::threading::thread_registry) contending: AtomicBool,
    /// Which interpreted frame locked each entry of `locked`, published by the
    /// owner with its frame trace ([`ThreadRegistry::publish_jmx_frame_monitors`])
    /// for cross-thread `ThreadInfo.getLockedMonitors()` (interpreter round i1
    /// wave 24, lane L7). Positions, not addresses: a moving collection
    /// rewrites `locked`'s slots in place and would leave addresses stale.
    pub(in crate::threading::thread_registry) frame_monitors: Mutex<FrameMonitorAttribution>,
    /// `true` while `frame_monitors` holds a publication, so an owner holding
    /// no monitor skips its mutex on every deposit.
    pub(in crate::threading::thread_registry) frame_monitors_published: AtomicBool,
    /// `ThreadMXBean` `getBlockedCount()`: monitor entries that had to block.
    /// Counted when the block BEGINS (`set_jmx_contended_monitor`).
    ///
    /// Here rather than on the `ThreadEntry` since round 11 wave 17 (lane lock,
    /// proposal W17-2): both contended-path writers then reach it through the
    /// thread's cached book, and a parking thread no longer takes the
    /// registry's `threads` read lock -- one RMW on a line every parking thread
    /// in the VM shares -- twice per park.
    pub(in crate::threading::thread_registry) blocked_count: AtomicU64,
    /// `getBlockedTime()`, in nanoseconds: added when a contended entry
    /// completes (`complete_jmx_monitor_enter`).
    pub(in crate::threading::thread_registry) blocked_nanos: AtomicU64,
    /// JIT round 19 wave 12, lane gc12 (`CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`):
    /// the lock stack as it was when a frame-trace PAUSE made this thread
    /// publish its trace ([`ThreadRegistry::record_trace_locks`]), so a reader
    /// of a RUNNING thread pairs the trace with the monitors held at that same
    /// instant, as HotSpot's one safepoint does. `None` until the first.
    pub(in crate::threading::thread_registry) trace_locks: Mutex<Option<TraceLockRecord>>,
    /// JIT round 25 wave 6, lane W6-F fix 4: bumped by every write of this
    /// thread's frame trace that this book is told of -- a pause's live
    /// publish ([`ThreadRegistry::record_trace_locks`], which stores the new
    /// value in its record), a blocking deposit
    /// ([`ThreadRegistry::note_trace_deposit`]), a pause-built excused trace
    /// ([`ThreadRegistry::store_frame_trace`]). A record whose `seq` is the
    /// current value describes the trace published now
    /// ([`ThreadRegistry::live_trace_locks`]).
    pub(in crate::threading::thread_registry) trace_seq: AtomicU64,
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
pub(in crate::threading::thread_registry) fn merge_compiled_hold_entries(
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
#[derive(Default, Clone)]
pub(crate) struct FrameMonitorAttribution {
    pub(in crate::threading::thread_registry) frame_count: usize,
    pub(in crate::threading::thread_registry) lock_len: usize,
    pub(in crate::threading::thread_registry) entries: Vec<(u32, u32)>,
    pub(in crate::threading::thread_registry) trace_count: usize,
    pub(in crate::threading::thread_registry) trace_entries: Vec<(u32, u32)>,
}

/// Owner-written slots in a [`JmxLockStack`] before publishes spill to its
/// mutexed overflow list. Eight distinct monitors held at once covers the
/// nesting of practically every `synchronized` program; a deeper thread pays
/// the pre-lock-stack mutex for its ninth and later monitors only.
pub(in crate::threading::thread_registry) const JMX_LOCK_STACK_SLOTS: usize = 8;

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
pub(in crate::threading::thread_registry) struct JmxLockStack {
    /// Seqlock sequence: odd while the owner is writing.
    pub(in crate::threading::thread_registry) seq: AtomicU64,
    /// One past the highest slot in use; every slot at or above it is 0.
    pub(in crate::threading::thread_registry) top: std::sync::atomic::AtomicUsize,
    /// Object addresses, 0 = empty or retracted (a tombstone).
    pub(in crate::threading::thread_registry) slots: [std::sync::atomic::AtomicUsize; JMX_LOCK_STACK_SLOTS],
    /// `true` while `overflow` may be non-empty. Written by the owner only,
    /// under the `overflow` mutex; `false` guarantees `overflow` is empty.
    pub(in crate::threading::thread_registry) spilled: AtomicBool,
    /// Monitors published while every slot was taken or while `spilled`, and
    /// every publish when `CRATONVM_MONITOR_FASTPATH=0`.
    pub(in crate::threading::thread_registry) overflow: Mutex<Vec<ObjectRef>>,
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
    pub(in crate::threading::thread_registry) fn write_begin(&self) -> u64 {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        // Orders the odd `seq` before the slot stores that follow, for a
        // reader whose `fence(Acquire)` follows its slot loads.
        std::sync::atomic::fence(Ordering::Release);
        s
    }

    /// Owner only: close the write [`Self::write_begin`] opened.
    #[inline]
    pub(in crate::threading::thread_registry) fn write_end(&self, s: u64) {
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Owner only: record `monitor` as held, unless it already is.
    pub(in crate::threading::thread_registry) fn publish(&self, monitor: ObjectRef) {
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
    pub(in crate::threading::thread_registry) fn retract(&self, monitor: ObjectRef) {
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
    pub(in crate::threading::thread_registry) fn snapshot(&self) -> Vec<ObjectRef> {
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
    pub(in crate::threading::thread_registry) fn extend_roots_stw(&self, out: &mut Vec<ObjectRef>) {
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
    pub(in crate::threading::thread_registry) fn remap_stw(&self, pointer_map: &cratonvm_types::PointerMap) {
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
pub(in crate::threading::thread_registry) struct ThreadEntry {
    /// Human-readable name.
    pub(in crate::threading::thread_registry) name: String,
    /// OS thread join handle (None for the main thread).
    pub(in crate::threading::thread_registry) join_handle: Option<JoinHandle<()>>,
    /// The Java `Thread` object on the heap.
    pub(in crate::threading::thread_registry) java_thread_obj: Option<ObjectRef>,
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
    pub(in crate::threading::thread_registry) java_tid: u64,
    /// Whether this thread is still running.
    pub(in crate::threading::thread_registry) alive: AtomicBool,
    /// Whether this thread can currently participate in counted STW barriers.
    ///
    /// `Thread.start()` registers the Java mirror before the spawned carrier has
    /// published its GC state or reached a safepoint-capable startup point. Such
    /// entries are alive for Java APIs but must not contribute to STW `expected`
    /// until the child marks itself ready.
    pub(in crate::threading::thread_registry) stw_ready: AtomicBool,
    /// T19.K1 — Whether this thread is a daemon. Default `false` matches
    /// the JLS rule that newly created threads inherit the parent's
    /// daemon status, which is `false` for the main thread. The CLI
    /// uses this to decide whether the process must wait for this
    /// thread before exiting (per the JVM spec, the VM lives until
    /// every non-daemon thread terminates). Stored as `AtomicBool`
    /// because `Thread.setDaemon(boolean)` may flip it from another
    /// thread before the target is `start()`-ed.
    pub(in crate::threading::thread_registry) daemon: AtomicBool,
    /// Parking permit for LockSupport.park()/unpark().
    pub(in crate::threading::thread_registry) park_state: Arc<ParkState>,
    /// Thread interrupt flag (shared with JvmThread for cross-thread access).
    pub(in crate::threading::thread_registry) interrupted: Arc<AtomicBool>,
    /// Root snapshot: ObjectRefs from this thread's frames, deposited at safepoints
    /// and before blocking operations. Used by GC to scan all threads' roots.
    pub(in crate::threading::thread_registry) root_snapshot: Arc<Mutex<Vec<ObjectRef>>>,
    /// Frame trace snapshot (shared with the JvmThread, like `root_snapshot`):
    /// this thread's Java call stack, published at the same blocking deposit
    /// points. Lets another thread read where this one is parked, backing
    /// cross-thread `Thread.getStackTrace()` / `dumpThreads()`.
    pub(in crate::threading::thread_registry) frame_trace: Arc<Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,
    /// Optional VM-side breadcrumb published by the owning `JvmThread` when
    /// `CRATONVM_DBG_VM_STATE=1` is enabled.
    pub(in crate::threading::thread_registry) vm_state: Arc<Mutex<String>>,
    /// Blocked-region GC state (shared with the JvmThread, like `root_snapshot`):
    /// lets a GC initiator remap this thread's snapshot and accumulate frame
    /// fixups while the thread is parked in a blocking native. See
    /// `GcBlockState` and `fold_pointer_map_into_blocked`.
    pub(in crate::threading::thread_registry) gc_block_state: Arc<GcBlockState>,
    /// JMX diagnostic roots. These are deliberately separate from a blocked
    /// thread's frame snapshot: a lock relationship must remain observable
    /// while the owner is running, and must be remapped across a moving GC.
    /// The three slots every `monitorenter` / `monitorexit` touches, in one
    /// allocation behind an `Arc` so the owning thread can reach them without
    /// the registry map — see [`ThreadRegistry::with_self_jmx_monitors`].
    pub(in crate::threading::thread_registry) jmx_monitors: Arc<JmxMonitorBook>,
    pub(in crate::threading::thread_registry) jmx_waiting_monitor: Mutex<Option<ObjectRef>>,
    /// Shared, so the owning thread can reach its own list without going
    /// through [`ThreadRegistry::threads`] — see
    /// [`ThreadRegistry::jmx_locked_synchronizers_of`].
    pub(in crate::threading::thread_registry) jmx_locked_synchronizers: Arc<Mutex<Vec<ObjectRef>>>,
    pub(in crate::threading::thread_registry) jmx_wait_started: Mutex<Option<Instant>>,
    // `jmx_blocked_count` / `jmx_blocked_nanos` moved into `JmxMonitorBook`
    // (`blocked_count` / `blocked_nanos`) in round 11 wave 17.
    pub(in crate::threading::thread_registry) jmx_waited_count: AtomicU64,
    pub(in crate::threading::thread_registry) jmx_waited_nanos: AtomicU64,
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
    pub(in crate::threading::thread_registry) tlab_addr: std::sync::atomic::AtomicUsize,
    /// XT-FRAME-SCAN: the owning thread's whole `JvmThread` address (0 =
    /// unpublished). Published with the same discipline as `tlab_addr`
    /// (address-stable for the thread's life, published by the owner) and
    /// cleared together with it in [`ThreadRegistry::clear_tlab_addr`]; read
    /// only for OS-frozen takeover peers (see `frozen_peer_thread_addrs`).
    pub(in crate::threading::thread_registry) jvm_thread_addr: std::sync::atomic::AtomicUsize,
    /// xt-hardening (2026-07-03): the OS thread id of the thread executing
    /// this entry, published by the thread itself at startup (next to its
    /// TLAB address). `0` = not yet published. The GC initiator snapshots
    /// the alive set's OS tids atomically with the barrier's `expected`
    /// computation so the cross-thread JIT takeover only excuses
    /// (`reduce_expected`) frozen peers that were actually COUNTED —
    /// excusing an uncounted newcomer releases the barrier while a counted
    /// mutator still runs, racing the collection.
    pub(in crate::threading::thread_registry) os_tid: std::sync::atomic::AtomicU32,
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
    pub(in crate::threading::thread_registry) os_tid_seq: AtomicU64,
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
    pub(in crate::threading::thread_registry) mounted: AtomicBool,
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
    pub(in crate::threading::thread_registry) mount_epoch: AtomicU64,
    /// gci i12/vp (`CRATONVM_GC_VT_PARKED_FOLD_FILTER`): the address filter
    /// of this entry's blocked-window words as the last fold left them,
    /// valid for one [`Self::mount_epoch`]. Written and read only by a
    /// pause's fold.
    pub(in crate::threading::thread_registry) parked_filter: ParkedFoldFilter,
    /// When the entry registered (interpreter round i3 wave 81, lane L6): a
    /// thread dump's `elapsed=`, as HotSpot's thread statistics count from the
    /// thread's creation.
    pub(in crate::threading::thread_registry) registered_at: Instant,
    /// Monitors this thread entered OUTSIDE structured locking and has not yet
    /// exited: JNI `MonitorEnter` and a VM native's
    /// `NativeContext::monitor_enter` / `monitor_enter_gc_safe` (HotSpot's
    /// `JavaThread::_jni_monitor_count`). Counted only under
    /// `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT` (`VmConfig::vthread_death_sweep_by_count`,
    /// interpreter round i4 wave 95, lane L6), where a JDK continuation's death
    /// skips the walk of every inflated monitor when it reads 0
    /// ([`ThreadRegistry::note_unstructured_monitor_enter`]).
    pub(in crate::threading::thread_registry) unstructured_monitor_enters: std::sync::atomic::AtomicU32,
    /// This entry is counted in the JMM's live platform-thread count
    /// ([`ThreadRegistry::note_jmm_thread_start`]); lowered, and the count
    /// with it, exactly once at its death (interpreter round i5 wave 100,
    /// lane L6).
    pub(in crate::threading::thread_registry) jmm_counted: AtomicBool,
    /// zt t15/pl6: the entry is a virtual thread's, fixed at registration
    /// ([`ThreadRegistry::register_starting_virtual`], a JDK continuation's
    /// prepared registration). Keeps it out of the JMM roster
    /// ([`ThreadRegistry::jmm_perf_thread_counts`]), as `ThreadMXBean`'s
    /// mirror walk leaves a `BaseVirtualThread` out.
    pub(in crate::threading::thread_registry) virtual_thread: bool,
}

/// What [`ThreadRegistry::retire_continuation_thread`] read off a dying JDK
/// continuation thread's entry, for the second half of its death
/// ([`ThreadRegistry::finish_retired_continuation_thread`]) and its monitor
/// sweep (interpreter round i4 wave 95, lane L6).
pub(crate) struct RetiredThread {
    pub(in crate::threading::thread_registry) mirror: Option<ObjectRef>,
    pub(in crate::threading::thread_registry) park_state: Arc<ParkState>,
    /// This retire lowered `alive` (wave 96; `drop_dead_park_index_entry`).
    pub(in crate::threading::thread_registry) first_death: bool,
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
pub(in crate::threading::thread_registry) static OS_TID_PUBLISH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Which of several alive entries that claim one OS tid is the thread running
/// there now: the unique one with the newest publish ordinal.
///
/// `claims` is `(id, os_tid_seq)` for every alive entry whose `os_tid` is the
/// caller's. One claim is the answer. Several: the newest ordinal, if exactly
/// one entry holds it. A tie (only possible for ordinal `0`, an entry whose
/// `os_tid` was stored without [`ThreadRegistry::set_os_tid_current`]) is
/// ambiguous, so `None`.
pub(in crate::threading::thread_registry) fn newest_os_tid_claimant(claims: &[(ThreadId, u64)]) -> Option<ThreadId> {
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
pub(in crate::threading::thread_registry) fn newest_claimants_by_os_tid(
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
pub(in crate::threading::thread_registry) fn os_tid_claim_admitted(os_tid: u32, mounted: bool, skip_unmounted: bool) -> bool {
    os_tid != 0 && (mounted || !skip_unmounted)
}

/// `CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED` -- gcj j2/pn2, default ON (`=0`
/// kills: an unmounted continuation claims its last carrier's tid again, the
/// gen r5w1/crash5 rule), read once per fold that captured blocked-peer
/// stack words, and applied only when the fold's heap is the GENERATIONAL one
/// (the blocked-peer capture fold is its moving path; G1 / ZGC unchanged).
pub(in crate::threading::thread_registry) fn claimant_skips_unmounted_enabled() -> bool {
    true
}

/// The capture fold's `skip_unmounted` for a fold whose heap
/// `is_generational`: the switch applies to the Generational fold only; a G1
/// or ZGC fold keeps the gen r5w1/crash5 rule whatever the switch says (their
/// conservative captures pin, so nothing they capture is written back moved).
/// gcj j3/x2 R9: split out so the "G1 / ZGC unchanged" claim is pinned by a
/// test, not by reading.
#[inline]
pub(in crate::threading::thread_registry) fn fold_claimant_skips_unmounted(is_generational: bool) -> bool {
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
pub(in crate::threading::thread_registry) fn note_declined_stale_claim(thread_id: u64, os_tid: u32, declined: usize) {
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
pub(in crate::threading::thread_registry) fn ffm_in_native_signal_exempt(block: &GcBlockState) -> bool {
    block.foreign_native.load(Ordering::Acquire) && block.in_blocked_region.load(Ordering::Acquire)
}

/// gci i24/jp: does one blocked row count as "holds a deferred chain record"
/// for [`deferred_chain_peer_tids`]? Only when the collector's walk of the
/// record will run this pause: `resolve_deferred_blocked_jit_chains` skips a
/// state whose owner was dropped, so such a row counts as a row WITHOUT a
/// record (and keeps every band on its tid a window). Pure.
pub(in crate::threading::thread_registry) fn deferred_chain_row(owner_dropped: bool, has_record: bool) -> bool {
    has_record && !owner_dropped
}

/// gci i20/vh: the rule of
/// [`ThreadRegistry::blocked_os_tids_with_deferred_jit_chain`] over its rows
/// `(os tid, the entry holds a deferred chain record)`: a tid qualifies when at
/// least one blocked entry on it holds a record and NO blocked entry on it
/// lacks one. Sorted, deduplicated. Pure.
pub(in crate::threading::thread_registry) fn deferred_chain_peer_tids(rows: Vec<(u32, bool)>) -> Vec<u32> {
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



/// `CRATONVM_DBG_STW_WATCHDOG_SECS=N` (JIT round 18 wave 10, lane ffmeintr;
/// debug, off when unset, `0` or unparsable): when a stop-the-world wait
/// (the take-over loop of a collection or of a handshake pause, the plain
/// `wait_for_all` of a pause without a take-over) or a lazy in-native leave
/// waiting on a claim outlives `N` seconds, print every registered thread's
/// state to stderr ([`ThreadRegistry::debug_stw_watchdog_census`]) -- once per
/// `N` seconds of the same wait -- so a silent hang names what it waits for.
/// Read once per wait, never on a fast path.
pub fn stw_watchdog_secs() -> Option<u64> {
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
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
    true
        .then_some(STW_STALL_REPORT_AFTER)}

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
pub(in crate::threading::thread_registry) fn in_native_band_of(block: &GcBlockState) -> Option<(usize, usize)> {
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
pub(in crate::threading::thread_registry) unsafe fn scan_in_native_band_words(
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
pub(in crate::threading::thread_registry) fn in_native_credit_admissible(
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
pub(in crate::threading::thread_registry) unsafe fn in_native_band_words_where(
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
    pub(in crate::threading::thread_registry) threads: RwLock<FxHashMap<ThreadId, ThreadEntry>>,
    /// The registry id whose `java.lang.Thread` has ENDED while its entry stays
    /// alive (`u64::MAX`: none): the launcher's main thread once `main` ended
    /// (interpreter round i5 wave 101, lane L6;
    /// `vm_exec::end_main_thread_java_side`). Its `JvmThread` still waits for
    /// the non-daemon threads and runs the shutdown sequence, so the entry is
    /// counted for safepoints and roots as before; only the Java-visible doors
    /// (`isAlive`, `getState`, the enumeration) answer it terminated, as
    /// HotSpot's detached main thread reads.
    pub(in crate::threading::thread_registry) java_ended_thread: AtomicU64,
    /// The [`Self::java_ended_thread`] entry's `java_thread_obj` was replaced
    /// by a `DestroyJavaVM` mirror (interpreter round i5 wave 102, lane L6;
    /// `vm_exec::list_destroy_java_vm_thread`): the doors that are handed a
    /// mirror answer that one alive and RUNNABLE, the ended `main` mirror
    /// still terminated, and the enumeration lists the entry again.
    pub(in crate::threading::thread_registry) destroy_java_vm_listed: AtomicBool,
    /// How many entries of [`Self::threads`] are dead (`alive == false`).
    /// Written only under the `threads` WRITE lock (the one insert, the one
    /// `alive` true -> false edge, the purge), so it always equals the walk
    /// `mark_dead` used to count with. Interpreter round i4 wave 95, lane L6:
    /// a JDK continuation's death reads it instead of walking every entry
    /// (`CRATONVM_VTHREAD_ONE_PUBLISH`, [`Self::retire_continuation_thread`]).
    pub(in crate::threading::thread_registry) dead_entries: std::sync::atomic::AtomicUsize,
    /// The JMM's platform-thread counters (interpreter round i5 wave 100,
    /// lane L6; HotSpot's `ThreadService::_total_threads_count`,
    /// `_atomic_threads_count` and `_peak_threads_count`): threads started
    /// (`Thread.start` of a platform thread, the main thread, a JNI attach;
    /// never a virtual thread, never decremented), live ones of those, and
    /// the high-water mark of the live count, raised at every start so a
    /// spike between two `getPeakThreadCount` reads is seen.
    pub(in crate::threading::thread_registry) jmm_started: AtomicU64,
    pub(in crate::threading::thread_registry) jmm_live: std::sync::atomic::AtomicI32,
    pub(in crate::threading::thread_registry) jmm_peak: std::sync::atomic::AtomicI32,
    /// zt t15/pl6: the JMM roster -- the entries `ThreadMXBean`'s walk lists
    /// (alive, with a `java.lang.Thread` mirror, not a virtual thread's;
    /// [`ThreadRegistry::jmm_roster_lists`]) -- and the daemons among them,
    /// kept at each edge (registration, death, mirror set or withdrawn,
    /// daemon flag), so the perf-data sampler reads the bean's figures
    /// without a walk ([`ThreadRegistry::jmm_perf_thread_counts`]).
    pub(in crate::threading::thread_registry) jmm_roster_live: std::sync::atomic::AtomicI32,
    pub(in crate::threading::thread_registry) jmm_roster_daemon: std::sync::atomic::AtomicI32,
    pub(in crate::threading::thread_registry) next_id: AtomicU64,
    /// Process-unique identity for this registry instance, used to key the
    /// per-thread self-handle cache (see [`ThreadRegistry::with_self_jmx_monitors`]).
    ///
    /// Deliberately NOT the registry's address: a dropped registry's address
    /// can be reused by a later one, and `ThreadId`s restart per registry, so
    /// an address-keyed cache could serve one registry's slot to another's
    /// same-numbered thread. A monotonic id cannot alias.
    pub(in crate::threading::thread_registry) registry_id: u64,
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
    pub(in crate::threading::thread_registry) thread_obj_to_park: Mutex<FxHashMap<usize, Arc<ParkState>>>,
    /// Reverse index from Java-side `Thread.tid` (unique, never reused —
    /// see `ThreadEntry::java_tid`) to registry `ThreadId`. This is the
    /// aliasing-proof identity map: a dead thread's entry stays here for
    /// TERMINATED `getState()` answers, but a NEW `Thread` allocated at the
    /// dead mirror's recycled heap address has a different `tid` and can
    /// never be confused with it (the address-keyed maps can).
    pub(in crate::threading::thread_registry) java_tid_to_id: Mutex<FxHashMap<u64, ThreadId>>,
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
    pub(in crate::threading::thread_registry) former_mirror_addrs: Mutex<(
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
    pub(in crate::threading::thread_registry) synchronizer_owner: [Mutex<FxHashMap<usize, ThreadId>>; SYNCHRONIZER_OWNER_SHARDS],
    /// `CRATONVM_FFM_LAZY_CLAIM_NO_ROOTS_PAUSE` for this VM, read once. The
    /// downcall return path uses it to keep the default lazy-native fast path
    /// free of a barrier lock; `=0` retains the synchronized wait path.
    pub(in crate::threading::thread_registry) no_roots_lazy_claims_enabled: bool,
    /// `CRATONVM_FFM_IN_NATIVE_NO_SIGNAL` for this VM, read once (gci i2/jn,
    /// i1 review R5): the three roster reads (`blocked_os_tids`,
    /// `blocked_os_tids_split_by_monitor_proof`, `alive_count_and_os_tids`)
    /// read the environment on every call before. Default on; `=0` off.
    pub(in crate::threading::thread_registry) ffm_in_native_no_signal: bool,
    /// `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE` for this VM, read once
    /// (interpreter round i2 wave 59, lane L6): a blocking region opened as
    /// `BlockKind::InNative` publishes `jvm_thread::JAVA_STATE_IN_NATIVE`
    /// (HotSpot's `_thread_in_native`: RUNNABLE, in native) instead of
    /// WAITING. See [`Self::blocked_in_native_runnable`].
    pub(in crate::threading::thread_registry) blocked_in_native_runnable: bool,
    /// `CRATONVM_THREAD_INFO_FRAMELESS` for this VM, read once (interpreter
    /// round i2 wave 61, lane L6;
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// a `ThreadInfo` door that reports no frames (`maxDepth` 0, no locked
    /// monitors) builds its snapshot without capturing the target's stack,
    /// so it takes no frame-trace pause. See [`Self::thread_info_frameless`].
    pub(in crate::threading::thread_registry) thread_info_frameless: bool,
    /// `CRATONVM_THREAD_DUMP_ONE_PAUSE` for this VM, read once (interpreter
    /// round i2 wave 61, lane L6;
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// a dump of many threads' stacks takes one frame-trace pause, not one
    /// per running target. See [`Self::thread_dump_one_pause`].
    pub(in crate::threading::thread_registry) thread_dump_one_pause: bool,
    /// `CRATONVM_THREADS_ATTACH_ORDER` for this VM, read once (interpreter
    /// round i6 wave 133, lane L7; HotSpot parity, default on, `=0` off;
    /// item 5 of
    /// `docs/feature-designs/real-jdk-mode.md`).
    /// See [`Self::threads_attach_order`].
    pub(in crate::threading::thread_registry) threads_attach_order: bool,
    /// `CRATONVM_GC_HANDSHAKE_FRAME_TRACE` for this VM, read once (gci i23/hs;
    /// default ON since JIT round 23 wave 6, `=0` kills). See
    /// [`Self::handshake_frame_trace`].
    pub(in crate::threading::thread_registry) handshake_frame_trace: bool,
    /// `CRATONVM_GC_HANDSHAKE_LOOP_KICK` for this VM, read once (gcj j1/cm,
    /// default ON). See [`Self::handshake_loop_kick`].
    pub(in crate::threading::thread_registry) handshake_loop_kick: bool,
    /// `CRATONVM_GC_HANDSHAKE_SPIN_WAIT` for this VM, read once (gcj j1/cm,
    /// default ON, `=0` kills; reached only by [`Self::handshake_and_wait`]):
    /// the requester polls its ticket between slices instead of a timed
    /// condvar wait, which the OS rounds up to its timer tick.
    pub(in crate::threading::thread_registry) handshake_spin_wait: bool,
    /// `CRATONVM_DBG_STW_EXPECTED_IDS` for this VM, latched once (gcj j2/cm2;
    /// `gcj-j1cm-safepoint-barrier-and-registry-review-residuals` item 8):
    /// read by the collection request's census closure under the barrier's
    /// `inner` and by every blocking park, which re-read the environment
    /// snapshot each time. Presence, as before. See
    /// [`Self::dbg_stw_expected_ids`].
    pub(in crate::threading::thread_registry) dbg_stw_expected_ids: bool,
    /// `CRATONVM_GC_HANDSHAKE_TARGET_KICK` for this VM, read once (gcj j2/cm2,
    /// opt-in; reached only under the opt-in `handshake_frame_trace` with the
    /// loop kick on, and only on the Generational backend). See
    /// [`Self::handshake_target_kick`].
    pub(in crate::threading::thread_registry) handshake_target_kick: bool,
    /// `CRATONVM_GC_HANDSHAKE_POLL_BIT` for this VM, read once (t1 lane E,
    /// default ON since 2026-10-26, `=0` kills; every backend). See [`Self::handshake_poll_bit`].
    pub(in crate::threading::thread_registry) handshake_poll_bit: bool,
    /// `CRATONVM_GC_HANDSHAKE_WAIT_SEES_BLOCK` for this VM, read once (t1 lane
    /// E2, opt-in, performance; every backend): the spin slice of
    /// [`Self::handshake_and_wait_kicking_twice`] also ends the moment the
    /// target enters a blocked region or is dropped, instead of at the slice's
    /// end.
    pub(in crate::threading::thread_registry) handshake_wait_sees_block: bool,
    /// `CRATONVM_GC_HANDSHAKE_UNMOUNTED_DEFERS_KICK` for this VM, read once
    /// (t1 wave 8 lane HANDSHK, opt-in, performance; every backend): a target
    /// that is UNMOUNTED when its op is queued is not kicked at once; the kick
    /// runs after the first slice, only if the op is still pending. See
    /// [`Self::handshake_and_wait_kicking_twice`].
    pub(in crate::threading::thread_registry) handshake_unmounted_defers_kick: bool,
    /// `CRATONVM_GC_HANDSHAKE_REQUESTER_SAFE_WHILE_WAITING` for this VM, read once
    /// (t1 wave 8 lane HANDSHK, default ON since 2026-10-26, `=0` kills; every backend):
    /// the requester drains its own pending handshakes while waiting in
    /// [`Self::handshake_and_wait_kicking_twice`], so mutual or cyclic samplers
    /// resolve without safepoint pauses.
    pub handshake_requester_safe_while_waiting: bool,
    /// `CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED` for this VM, read once (gcj
    /// j3/rt3, default ON, `=0` kills; every backend): an unmounted entry
    /// never answers [`Self::thread_id_for_current_os_tid`]. See
    /// [`Self::thread_id_for_os_tid_inner`].
    pub(in crate::threading::thread_registry) os_tid_identity_skips_unmounted: bool,
    /// `CRATONVM_PUBLISHED_WAIT_KIND` for this VM, read once (interpreter
    /// round i2 wave 63, lane L1; stage 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a `BlockKind::Sleeping` region publishes
    /// `jvm_thread::JAVA_WAIT_KIND_SLEEPING`, and JVMTI's `SLEEPING` and
    /// JDWP's `ThreadStatus.SLEEPING` read it ([`Self::published_sleeping`])
    /// instead of the deposited top frame. Default ON since the wave-64 host run
    /// (`L1W63JvmtiSleepKinds --compatible` matches HotSpot only with it on); `=0` is the kill switch.
    pub(in crate::threading::thread_registry) published_wait_kind: bool,
    /// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` for this VM, read once
    /// (interpreter round i2 wave 64, lane L1;
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// every wait publishes its kind -- a park `PARKED`, `Object.wait` and
    /// the served `Thread.join` `IN_OBJECT_WAIT`, a sleep `SLEEPING` (it
    /// implies [`Self::published_wait_kind`]) -- and JVMTI's kind bit
    /// ([`Self::published_wait_kind_of`]) and the lazy-wait reading of
    /// [`Self::java_block_state`] read the word, taking no per-thread mutex.
    /// Default ON since the wave-67 triage: the unrecorded-wait census reads 0
    /// regions in both modes and the JVMTI wait-kind probes (`L1W64JvmtiAllWaitKinds`,
    /// `L1W63JvmtiSleepKinds`, `L1W57JvmtiWaitStates`, `L1W58JvmtiInNative`) match
    /// HotSpot with it on; `=0` is the kill switch.
    pub(in crate::threading::thread_registry) published_all_wait_kinds: bool,
    /// `CRATONVM_SERVED_WAIT_KINDS` for this VM, read once (interpreter round
    /// i2 wave 65, lane L1;
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// a served stand-in's `Object.wait` that stands for JDK code that parks
    /// (`NativeContext::monitor_wait_as` with `BlockKind::Parked` /
    /// `TimedParked`; the `--compatible` `CountDownLatch.await`) reports the
    /// JDK body's state (TIMED_WAITING for a timed one) and publishes
    /// `jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED`, which JVMTI reads as
    /// `PARKED` ([`Self::published_monitor_parked`]). Off, such a wait is a
    /// plain native `Object.wait` (WAITING, `IN_OBJECT_WAIT`). Default on;
    /// `=0` is the kill switch.
    pub(in crate::threading::thread_registry) served_wait_kinds: bool,
    /// `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` for this VM, read once
    /// (interpreter round i2 wave 66, lane L1;
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// the census of the blocking regions that open with no recorded wait
    /// kind, by the Rust call site of the door. Off (the default), one load of
    /// its mode per kind-less door.
    pub(in crate::threading::thread_registry) unrecorded_wait_census: UnrecordedWaitCensus,
    /// `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE` for this VM, read once
    /// (interpreter round i2 wave 66, lane L1): a wait HotSpot performs
    /// inside the VM (a JVMTI function waiting for another thread to stop)
    /// opens `NativeContext::begin_vm_wait_region` and reads RUNNABLE, not a
    /// WAITING region with no wait kind. Default on; `=0` is the kill switch.
    pub(in crate::threading::thread_registry) vm_internal_waits_runnable: bool,
    /// `CRATONVM_PARK_LAZY` for this VM, read once (interpreter round i3 wave
    /// 71, lane L1; `docs/feature-designs/real-jdk-mode.md`):
    /// `Some(budget)` -- the monitor lazy-park budget,
    /// `CRATONVM_MONITOR_LAZY_PARK_US`, 2 000 us unless set -- when a
    /// platform thread's `Unsafe.park` first waits for its permit as a
    /// running mutator, and only a park still waiting after it pays the
    /// blocking prologue (TLAB retire, root deposit with its JIT band scan,
    /// GC-blocked entry, wake-side refresh). While it waits, its
    /// `JAVA_WAIT_KIND_PARKED` kind answers [`Self::java_block_state`] as the
    /// lazy `Object.wait` does. Default OFF (`truthy_word`).
    pub(in crate::threading::thread_registry) park_lazy_budget: Option<std::time::Duration>,
    /// `CRATONVM_STAND_IN_WAIT_OBJECTS` for this VM, read once (interpreter
    /// round i2 wave 68, lane L1;
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// a stand-in's blocking region publishes the object the JDK code it
    /// stands for would wait or park on (`NativeContext::begin_blocking_region_as_on`):
    /// an `Object.wait` kind as the thread's waiting monitor
    /// ([`Self::publish_region_waiting_monitor`]), a park kind as the
    /// thread's `parkBlocker`; and the `LockSupport.park*(Object, ..)`
    /// bridges set `parkBlocker` as the JDK's own bodies do. So
    /// `ThreadInfo.getLockName()` names what HotSpot names. Default on (a
    /// correctness fix toward HotSpot); `=0` is the kill switch.
    pub(in crate::threading::thread_registry) stand_in_wait_objects: bool,
    /// `CRATONVM_PARK_LAZY_STW_WAKE` for this VM, read once (interpreter
    /// round i3 wave 72, lane L6; page
    /// `interpreter-i71-L1-proposal-a-running-phase-before-a-blocking-park-DONE-20261224`,
    /// follow-up (a)): a park's running phase is counted in
    /// [`Self::lazy_park_phases`] and listed in [`Self::lazy_park_states`],
    /// and a stop-the-world initiator wakes it
    /// ([`Self::wake_lazy_park_phases`]) instead of the phase noticing the
    /// pause at its next 100 us slice; the phase then waits in one wait of
    /// its whole budget. Default OFF (`truthy_word`).
    pub(in crate::threading::thread_registry) park_lazy_stw_wake: bool,
    /// How many threads are in a park's running phase right now, with
    /// [`Self::park_lazy_stw_wake`] on (0 otherwise). The handshake with
    /// [`Self::wake_lazy_park_phases`] is `MonitorTable::lazily_parked`'s.
    pub(in crate::threading::thread_registry) lazy_park_phases: AtomicU64,
    /// The `ParkState`s of the threads counted in [`Self::lazy_park_phases`]
    /// (pushed before the count is raised, removed after it is lowered).
    pub(in crate::threading::thread_registry) lazy_park_states: Mutex<Vec<Arc<ParkState>>>,
    /// `CRATONVM_PARK_DEPOSIT_FROZEN_BAND` for this VM, read once
    /// (interpreter round i3 wave 72, lane L6; follow-up (b) of the same
    /// page): a park that blocks deposits its roots under
    /// `conservative_roots::with_frozen_band_memo`, as
    /// `begin_blocking_region_with_state` has since SBR5, so a park from a
    /// compiled caller does not re-scan the frozen native band word by word.
    /// Default OFF (`truthy_word`).
    pub(in crate::threading::thread_registry) park_deposit_frozen_band: bool,
    /// gci i47w lane pr47: `CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`
    /// (default ON, `=0` kills) for this VM, read once: on G1 a blocking
    /// park deposits through the lazy blocking-region deposit, so its
    /// compiled chain is deferred to the collector and the helper-window
    /// pass demotes the parked peer (see `vm_exec::park_deposit_defers_jit_chain`).
    pub(in crate::threading::thread_registry) park_defers_jit_chain: bool,
    /// `CRATONVM_UNPARK_ONE_LOOKUP` for this VM, read once (interpreter round
    /// i3 wave 72, lane L6; follow-up (c) of the same page): `Unsafe.unpark`
    /// of a platform thread reads the mirror's `tid` once and resolves the
    /// thread id and its `ParkState` in one registry call
    /// ([`Self::unpark_target_by_java_tid`]), and asks the virtual-thread
    /// manager only once a virtual thread exists. Default ON
    /// (`runtime_flag_default_on`; `=0` reverts).
    pub(in crate::threading::thread_registry) unpark_one_lookup: bool,
    /// `CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT` for this VM, read once
    /// (interpreter round i4 wave 99, lane L1; page
    /// `interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md`): the leave of a
    /// `begin_blocking_region*` region (`end_blocking_region*`) keeps the
    /// root snapshot its entry deposited, skipping the frame audit and the
    /// full rebuild, when nothing that snapshot is a function of changed in
    /// the window (`vm_exec::blocking_leave_key`). Default ON
    /// (`runtime_flag_default_on`; `=0` restores the rebuild on every leave).
    pub(in crate::threading::thread_registry) blocking_leave_keeps_snapshot: bool,
    /// `CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS` for this VM, read once
    /// (interpreter round i5 wave 100, lane L1; proposal B of
    /// `interpreter-i100-L1-proposal-a-blocked-thread-is-walked-not-deposited-CLOSED-20261224.md`):
    /// the quiet leave's key is built into a recycled per-thread list and the
    /// leave compares it streamed, with no allocation; the comparison stays
    /// exact (`vm_exec::blocking_leave_key_matches`). Default OFF
    /// (`runtime_flag_on`) until the A/B medians.
    pub(in crate::threading::thread_registry) blocking_leave_key_reuses_buffers: bool,
    /// `CRATONVM_PARK_LAZY_SUSPEND_AWARE` for this VM, read once
    /// (interpreter round i3 wave 72, lane L6; follow-up (d) of the same
    /// page): while a JDWP / JVMTI suspension is in force
    /// (`DebuggerGates::suspension_in_force`, `experimental-debug` builds) a
    /// park skips its running phase, and a phase already running leaves it at
    /// its next slice -- or at once, with [`Self::park_lazy_stw_wake`], when
    /// `debug::publish_debugger_gates` wakes it -- so a suspended parker
    /// reaches its blocked region (deposited, frames published for the
    /// debugger) without waiting out the budget. Default OFF (`truthy_word`).
    pub(in crate::threading::thread_registry) park_lazy_suspend_aware: bool,
    /// `CRATONVM_PARK_ATOMIC_PERMIT` for this VM, read once (interpreter
    /// round i3 wave 73, lane L6; page
    /// `i72-L6-proposal-lock-free-park-permit-20261105`): `Unsafe.unpark` of
    /// a platform thread sets the permit with one swap and takes the
    /// `ParkState` mutex (and a wake-up syscall) only when the target waits
    /// (`ParkState::unpark_lock_free`), and a park with the permit set takes
    /// it without the mutex (`ParkState::take_permit_lock_free`). Default
    /// OFF (`truthy_word`).
    pub(in crate::threading::thread_registry) park_atomic_permit: bool,
    /// `CRATONVM_PARK_LAZY_SPIN_NS` for this VM, read once (interpreter round
    /// i3 wave 79, lane L6; page
    /// `i73-L6-proposal-spin-on-the-atomic-permit-before-the-lazy-wait`):
    /// `Some(spin)` -- a platform thread's park spins on its permit for up
    /// to `spin` (capped at [`PARK_LAZY_SPIN_CAP`]) before its running
    /// phase waits on the condvar. `None` (default: unset, `0`, unparsable,
    /// or a 1-CPU host, where the waker cannot run while the parker spins).
    pub(in crate::threading::thread_registry) park_lazy_spin: Option<std::time::Duration>,
    /// `CRATONVM_PARK_LAZY_ADAPTIVE` for this VM, read once (interpreter round
    /// i3 wave 81, lane L6; page `i77-L6-proposal-adaptive-lazy-park-budget`):
    /// a platform thread whose last [`crate::threading::jvm_thread::LAZY_PARK_MISSES_TO_SKIP`]
    /// running phases all ran out their budget skips the phase (straight to
    /// the blocking park), except every
    /// [`crate::threading::jvm_thread::LAZY_PARK_RECHECK_EVERY`]th park; a park
    /// that ends on its permit within the budget resets it
    /// (`ParkState::lazy_phase_skip`). Default OFF (`truthy_word`).
    pub(in crate::threading::thread_registry) park_lazy_adaptive: bool,
    /// `CRATONVM_UNPARK_EETOP` for this VM, read once (interpreter round i3
    /// wave 73, lane L6; page `i72-L6-proposal-unpark-through-eetop-20261105`):
    /// `Unsafe.unpark` of a platform thread reads the registry id from the
    /// mirror's `Thread.eetop` (`vm_exec::live_eetop`) and takes only the
    /// `threads` read lock, not the `java_tid` index mutex. Default OFF
    /// (`truthy_word`).
    pub(in crate::threading::thread_registry) unpark_eetop: bool,
    /// `CRATONVM_PARK_UNTIMED_NO_POLL` for this VM, read once (interpreter
    /// round i3 wave 74, lane L6; page
    /// `i73-L6-proposal-untimed-park-without-a-5ms-poll-20261106`): a
    /// blocking park waits without the 5 ms interrupt poll
    /// (`ParkState::park_interruptible_unpolled`), and
    /// [`Self::set_interrupted`] wakes the target's `ParkState` after raising
    /// its flag. Default ON since the wave-81 triage (`runtime_flag_default_on`;
    /// `L6W74ParkNoPollBench` idle-cpu 1.17 ms -> 0 per idle second, the other
    /// rows unchanged; `=0` reverts).
    pub(in crate::threading::thread_registry) park_no_poll: bool,
    /// `CRATONVM_SLEEP_NO_POLL` for this VM, read once (interpreter round i3
    /// wave 75, lane L6; page
    /// `i74-L6-proposal-sleep-without-the-10ms-poll-20261107`): a platform
    /// thread's `Thread.sleep` waits on its `ParkState`'s condvar for the
    /// whole remainder (`ParkState::sleep_unpolled`) instead of 10 ms
    /// `std::thread::sleep` slices, and [`Self::set_interrupted`] wakes it
    /// after raising the flag. Default ON (`runtime_flag_default_on`; `=0`
    /// reverts).
    pub(in crate::threading::thread_registry) sleep_no_poll: bool,
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
    pub(in crate::threading::thread_registry) park_wake_after_unlock: bool,
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
    pub(in crate::threading::thread_registry) park_untimed_event: bool,
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
    pub(in crate::threading::thread_registry) park_event_signal_after_unlock: bool,
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
    pub(in crate::threading::thread_registry) park_event_lockless_wake: bool,
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
    pub(in crate::threading::thread_registry) sleep_high_resolution_interval: bool,
    /// `CRATONVM_INTERRUPT_ONE_LOOKUP` for this VM, read once (interpreter
    /// round i3 wave 76, lane L6; page
    /// `i75-L6-proposal-one-lookup-thread-interrupt-20261108`):
    /// `Thread.interrupt` resolves its target from `Thread.eetop` and raises
    /// the flag, takes the park state and the JMX waiting monitor in one
    /// `threads` read ([`Self::raise_interrupt`]). Default ON since the
    /// wave-76 triage (`runtime_flag_default_on`; `=0` reverts).
    pub(in crate::threading::thread_registry) interrupt_one_lookup: bool,
    /// `CRATONVM_INTERRUPT_ONE_WAKE` for this VM, read once (interpreter
    /// round i4 wave 91, lane L6; page
    /// `interpreter-i77-L6-proposal-one-trip-interrupt-wake-DONE-20261127`): `Thread.interrupt`
    /// skips the flag wake and wakes its target's park state once, at the
    /// end, with the notify still under the mutex
    /// (`ParkState::interrupt_wake_locked`). Ignored under
    /// [`Self::park_wake_after_unlock`], which already folds the two trips.
    /// Default ON since the wave-92 triage (`runtime_flag_default_on`, `=0` off).
    pub(in crate::threading::thread_registry) interrupt_one_wake: bool,
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
    pub(in crate::threading::thread_registry) interrupt_field_memo: bool,
    /// `CRATONVM_DBG_INTERRUPT` read once at construction (wave 77); asked
    /// only with [`Self::interrupt_field_memo`] on.
    pub(in crate::threading::thread_registry) dbg_interrupt: bool,
    /// `CRATONVM_THREAD_START_ONE_PUBLISH` for this VM, read once
    /// (interpreter round i3 wave 82, lane L6; page
    /// `i74-L6-proposal-one-registry-write-per-thread-start-20261107`): a
    /// platform thread's start takes every `Arc` its registry entry holds in
    /// one read ([`Self::starting_thread_shared`]) and the child adopts them
    /// (`JvmThread::with_shared`), instead of the parent's two and the
    /// child's four `set_*` write locks re-installing them. Default OFF
    /// (`truthy_word`).
    pub(in crate::threading::thread_registry) thread_start_one_publish: bool,
    /// gci i12/vp: `CRATONVM_GC_VT_PARKED_FOLD_FILTER` for this VM, read once
    /// (default ON since 2026-10-17, `=0` kills). A relocating pause's blocked-thread fold
    /// ([`Self::fold_pointer_map_into_blocked_audited`]) skips an UNMOUNTED
    /// continuation whose cached address filter ([`FoldAddrFilter`], kept in
    /// `ThreadEntry::parked_filter` and valid for one `mount_epoch`) proves
    /// none of its words is a key of the pause's pointer map -- the fold would
    /// have rewritten nothing. Page
    /// `gci-i12vp-parked-continuations-folded-by-every-young-pause-FIXED-20261016`.
    pub(in crate::threading::thread_registry) vt_parked_fold_filter: bool,
    /// gci i12/vp: `CRATONVM_DBG_VT_PARKED_ROOTS` for this VM, read once: one
    /// `[vt-parked-roots]` stderr line per root gather
    /// ([`Self::collect_all_root_snapshots`]) and per blocked fold, with the
    /// unmounted continuations' share (entries, words, time). Diagnostic.
    pub(in crate::threading::thread_registry) dbg_vt_parked_roots: bool,
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
    pub(in crate::threading::thread_registry) vt_parked_gather_filter: bool,
    /// gci i13/vg: the collection door's split request for the NEXT
    /// [`Self::collect_all_root_snapshots`] and that gather's old-only
    /// section ([`Self::arm_parked_split_gather`],
    /// [`Self::take_parked_old_only`]). Armed and taken by the same pause on
    /// the same thread (`run_collection_pause`), so the door keeps its one
    /// pinned gather call. A leaf lock, never held across the gather.
    pub(in crate::threading::thread_registry) parked_split: Mutex<ParkedSplitGather>,
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
    pub(in crate::threading::thread_registry) vt_parked_mirror_keyed: bool,
    /// gci i14/ve: the door's mirror-keyed request for the NEXT
    /// [`Self::collect_all_root_snapshots`], the entries that gather left out
    /// (consumed by the same pause's [`Self::sweep_mirror_keyed_parked`]) and
    /// the ids that sweep retired (taken after the pause,
    /// [`Self::take_collected_parked`]). A leaf lock, never held across the
    /// gather's entry walk.
    pub(in crate::threading::thread_registry) mirror_keyed: Mutex<MirrorKeyedGather>,
    /// gci i18/hw: `CRATONVM_G1_VT_PRECISE_DEPOSIT` for this VM, read once
    /// (default ON, `=0` kills). On G1 a virtual thread's continuation
    /// deposits its roots precisely, without the conservative-locals probes
    /// ([`continuation_deposit_is_precise`]).
    pub(in crate::threading::thread_registry) g1_vt_precise_deposit: bool,
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
    pub(in crate::threading::thread_registry) interrupt_locks: [Mutex<()>; INTERRUPT_LOCK_STRIPES],
    /// `CRATONVM_INTERRUPT_LOCK` for this VM, read once (nio21 wave 5 L1):
    /// default on; `=0` is the kill switch (the two records are written with
    /// no exclusion, as before wave 5).
    pub(in crate::threading::thread_registry) interrupt_lock_enabled: bool,
}

/// How many stripes [`ThreadRegistry::interrupt_locks`] has. A power of two
/// so the index is a mask; collisions only serialize two unrelated threads'
/// two-store sections.
pub(in crate::threading::thread_registry) const INTERRUPT_LOCK_STRIPES: usize = 64;

/// The stripe of an interrupt flag, from its address. Multiplicative hash:
/// an `ArcInner<AtomicBool>` is 8- or 16-byte aligned, so the low bits are
/// constant.
#[inline]
pub(in crate::threading::thread_registry) fn interrupt_lock_stripe(flag: &AtomicBool) -> usize {
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
    pub(in crate::threading::thread_registry) mode: UnrecordedWaitMode,
    /// `(file, line, column, door)` -> regions opened.
    pub(in crate::threading::thread_registry) sites: Mutex<FxHashMap<(&'static str, u32, u32, &'static str), u64>>,
}

impl UnrecordedWaitCensus {
    pub(in crate::threading::thread_registry) fn from_env() -> Self {
        let value = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok();
        Self::with_mode(UnrecordedWaitMode::from_value(value.as_deref()))    }

    pub(in crate::threading::thread_registry) fn with_mode(mode: UnrecordedWaitMode) -> Self {
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
pub(in crate::threading::thread_registry) fn gc_block_state_wait_kind(block: &GcBlockState) -> PublishedWaitKind {
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
pub(in crate::threading::thread_registry) fn gc_block_state_blocked_in_native(block: &GcBlockState) -> bool {
    block.in_blocked_region.load(Ordering::Acquire)
        && block.java_state.load(Ordering::Acquire)
            == crate::threading::jvm_thread::JAVA_STATE_IN_NATIVE
}

/// A thread in a `BlockKind::Sleeping` blocking region: the flag up over a
/// TIMED_WAITING `java_state` and `jvm_thread::JAVA_WAIT_KIND_SLEEPING`
/// (both stored before the flag rises; the kind lowered after it falls).
/// Interpreter round i2 wave 63, lane L1.
#[inline]
pub(in crate::threading::thread_registry) fn gc_block_state_sleeping(block: &GcBlockState) -> bool {
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
pub(in crate::threading::thread_registry) const SYNCHRONIZER_OWNER_SHARDS: usize = 64;

/// Which shard owns a synchronizer's heap address.
///
/// Shifted by four first: heap objects are at least 8-byte aligned and in
/// practice 16, so the low bits are constant and hashing on them would put
/// everything in one shard — the failure mode that would leave this change
/// measuring as inert while looking correct.
#[inline]
pub(in crate::threading::thread_registry) fn synchronizer_owner_shard_index(key: usize) -> usize {
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
pub(in crate::threading::thread_registry) fn rekey_in_two_phases<V>(map: &mut rustc_hash::FxHashMap<usize, V>, moves: &[(usize, usize)]) {
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
pub(in crate::threading::thread_registry) const FORMER_MIRROR_CAP: usize = 8192;

// A terminated Java thread's observable state lives in its mirror. Keeping a
// registry entry after its carrier has exited is therefore unnecessary: all
// registry-backed operations on a missing entry already have the terminated
// semantics (not alive, no interrupt target, join returns immediately). More
// importantly, every GC/STW census walks this map. Tomcat's non-blocking test
// repeatedly creates and tears down endpoint pools, and retaining every dead
// carrier turned those censuses into an ever-growing O(total threads ever
// created) cost. Keep a small diagnostic tail while bounding that cost.
pub(in crate::threading::thread_registry) const TERMINATED_THREAD_ENTRY_CAP: usize = 256;

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
pub(in crate::threading::thread_registry) fn entry_is_stw_live(entry: &ThreadEntry) -> bool {
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



/// Report, once per process, a carrier that finished without `mark_dead`.
#[cold]
pub(in crate::threading::thread_registry) fn note_finished_but_alive(entry: &ThreadEntry) {
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

