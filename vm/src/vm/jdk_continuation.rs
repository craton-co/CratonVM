//! JDK-driven continuations: `java.lang.VirtualThread` on its own carriers.
//!
//! With `VmConfig::jdk_continuations` (default on over a real JDK 21+)
//! `ContinuationSupport.isSupported0()` answers `true`, so `Thread.ofVirtual()`
//! builds a `java.lang.VirtualThread`: the JDK schedules it on its default
//! `ForkJoinPool`, whose workers (`jdk.internal.misc.CarrierThread`) are
//! ordinary started platform threads, listed everywhere as on HotSpot, and
//! `VirtualThread.<clinit>` starts the `VirtualThread-unblocker`. A carrier
//! runs `VirtualThread.runContinuation` -> `mount()` (which re-points the
//! carrier's `Thread.currentThread()` at the virtual thread,
//! `Thread.setCurrentThread`) -> `Continuation.run` -> the native
//! `Continuation.enterSpecial`, served here.
//!
//! **The continuation's thread.** Each continuation runs on a `JvmThread` of
//! its own (`ThreadKind::Virtual`, `JvmThread::jdk_continuation.jdk`, its own
//! registry entry and id): the virtual thread's identity, monitors, frames and
//! GC state, exactly as the VM's bound virtual threads already have them. Its
//! frames live in heap stack chunks, so a whole-stack yield
//! (`VmError::ContinuationYield`, unwound through the interpreter without
//! popping a frame) unmounts it and `interpreter::resume_continuation`
//! remounts it, on any OS thread -- the machinery the bound path runs on
//! (`vm_exec::resume_virtual_continuation`), reused slice for slice.
//!
//! **Where a slice runs.** `enterSpecial` is called on the carrier with the
//! carrier's own frames below it (compiled ones included). The carrier enters
//! a VM wait (RUNNABLE, GC-safe, `begin_vm_wait_region`) and hands the
//! continuation's thread to a runner OS thread of this VM
//! ([`ContinuationRunners`]), which mounts it, runs it to its next `doYield`
//! (or its end) and gives it back; the carrier wakes and returns into
//! `Continuation.run`. One machine stack, one claimant per OS thread: the
//! carrier is an ordinary platform thread blocked in a native, the runner an
//! ordinary carrier of the VM's bound path. While the slice runs, the
//! continuation's thread shares its carrier's `ParkState` and interrupt flag
//! (HotSpot's virtual thread and carrier are one `JavaThread` with one
//! `Parker`): `VirtualThread.parkOnCarrierThread`'s `U.park` and the
//! `U.unpark(carrier)` / `carrier.setInterrupt()` that end it meet there.
//!
//! **Yield.** `Continuation.doYield()` on the continuation's thread answers a
//! pin reason when it cannot unmount (a held monitor or `Continuation.pin()`
//! (`pin_count`), a native frame beneath, a class initializer), else records
//! `VtYield::DoYield` and unwinds; the remount pushes `doYield`'s `0`.
//! `Continuation.isStarted()` is `tail != null`: the first yield gives the
//! continuation a `StackChunk` (non-empty while frozen, empty while mounted,
//! so `-ea`'s `isEmpty()` asserts hold).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;

use super::vm_exec::NativeContextImpl;
use super::SharedVm;
use crate::error::{MethodCallFailed, VmError};
use crate::native::registry::{NativeHeapAccess, NativeThreadAccess};
use crate::threading::jvm_thread::{JvmThread, ThreadId, VtYield};
use crate::threading::ParkState;
use crate::types::{ObjectRef, Value};

/// The native stack of a runner OS thread: the VM carrier pool's
/// (`virtual_threads.rs`), which sizes a virtual thread's
/// `StackOverflowError` ceiling the same way.
const RUNNER_STACK_BYTES: usize = 8 * 1024 * 1024;

/// An idle runner retires after this long without a slice (the JDK's
/// carriers keep alive 30 s too).
const RUNNER_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

/// `Continuation`'s class and the static the first slice calls.
const CONTINUATION: &str = "jdk/internal/vm/Continuation";
const ENTER: (&str, &str) = ("enter", "(Ljdk/internal/vm/Continuation;Z)V");
const STACK_CHUNK: &str = "jdk/internal/vm/StackChunk";

/// HotSpot's freeze results, as `Continuation.pinnedReason` reads them.
const PINNED_CRITICAL_SECTION: i32 = 2;
const PINNED_NATIVE: i32 = 3;
const PINNED_MONITOR: i32 = 4;

/// How a slice ended, handed from the runner back to the carrier.
enum SliceOutcome {
    /// `doYield` unmounted the continuation; its state is deposited with the
    /// virtual-thread manager (`VirtualThreadManager::deposit_continuation`).
    Yielded,
    /// `Continuation.enter` returned: the continuation is done and its thread
    /// torn down.
    Done,
    /// `Continuation.enter` threw this throwable (a global-root handle the
    /// carrier releases); the continuation's thread is torn down.
    Threw(usize),
    /// The slice failed inside the VM before anything ran; the state is
    /// deposited back where a yield would put it.
    Failed(String),
    /// gci i21/rv (finding 4 of
    /// `gci-i21rv-i20-default-path-review-findings-FIXED-20261017`): the slice
    /// failed inside the VM and the continuation's thread is torn down (a
    /// raw VM error unwound `Continuation.enter`, or a first slice found no
    /// runner and its registration was retired). No `enterSpecial` takes it
    /// back, so the carrier marks the continuation `done`: the
    /// `finally { c.finish(); }` of `Continuation.enter` that the raw error
    /// skipped.
    FailedEnded(String),
}

/// One slice to run: the continuation's thread and what its carrier lends it.
struct Job {
    thread: Box<JvmThread>,
    first: bool,
    /// Global-root handle of the `Continuation` (a first slice calls
    /// `Continuation.enter` with it).
    cont_root: usize,
    carrier: ThreadId,
    carrier_park: Arc<ParkState>,
    carrier_interrupted: Arc<AtomicBool>,
    /// When the carrier posted the job (`CRATONVM_DBG_VTHREAD_CENSUS` only;
    /// the census's `pickup_ns`).
    posted: Option<std::time::Instant>,
    /// gci i24/vl: the carrier refused the yield this resumed state ended
    /// with ([`refuse_yield_without_headroom`]): its `doYield` answers
    /// [`YIELD_REFUSED_PIN_REASON`] instead of `0` ([`refuse_deposited_yield`]).
    refuse_yield: bool,
}

/// gci i24/vl (`gci-i24vl-g1-virtual-thread-lost-when-after-yield-throws-oome-FIXED-20261017`):
/// JDK 25 `java.lang.VirtualThread.TIMED_PARKING`, the state `parkNanos` sets
/// before its yield. The carrier's `afterYield()` then allocates the timeout
/// task (`schedule(this::parkTimeoutExpired, ..)`) with no `OutOfMemoryError`
/// handling: an OOME there leaves the thread TIMED_PARKING with no task and no
/// timer -- nothing ever runs it again (`unpark` only acts on PARKED /
/// TIMED_PARKED / PINNED). Every other state a `doYield` leaves is safe
/// (`afterYield`'s submits retry on OOME; PARKING allocates nothing). On JDK 21
/// the value is PINNED, which never yields, so it cannot match a yield there.
const VTHREAD_TIMED_PARKING: i32 = 6;

/// gci i25/vt: JDK 25 `VirtualThread.TIMED_WAITING`, the state a timed
/// `Object.wait` leaves before its unmount (`vm_exec`'s
/// `vt_monitor_wait_unmount`). Its `afterYield()` schedules
/// `waitTimeoutExpired` exactly as a timed park schedules its timeout, with
/// no `OutOfMemoryError` handling either. Not a state of JDK 21.
const VTHREAD_TIMED_WAITING: i32 = 17;

/// gci i25/vt: JDK 25 `VirtualThread.BLOCKED` -- what a re-queued orphan is
/// moved to, so the unblocker's `unblock()` (BLOCKED -> UNBLOCKED) submits it
/// with `submitRunContinuation`'s retry on `OutOfMemoryError`.
const VTHREAD_BLOCKED: i32 = 13;

/// gci i24/vl: what a refused yield's `doYield` answers -- a pin, which every
/// JDK's `Continuation.pinnedReason` accepts and `VirtualThread.parkNanos`
/// handles as HotSpot handles a freeze that failed with `OutOfMemoryError`
/// (`catch (OutOfMemoryError e) { // park on carrier }`): `yielded` stays
/// false and the thread parks on its carrier for the rest of the timeout.
const YIELD_REFUSED_PIN_REASON: i32 = PINNED_NATIVE;

/// gci i24/vl: the length of the carrier's post-yield probe (a `byte[]`
/// through the collecting allocation ladder, `gc_alloc_array`). Zero: the
/// probe asks only whether the carrier can still allocate (its TLAB was
/// retired when it entered the slice's wait, so the probe refills it, as the
/// first allocation of `afterYield` would have).
const POST_YIELD_PROBE_LENGTH: usize = 0;

enum SlotState {
    Idle,
    Job(Box<Job>),
    Running,
    Done(SliceOutcome),
}

/// A runner OS thread's mailbox: the carrier posts a [`Job`] and waits for
/// the [`SliceOutcome`] on the same condition variable. `phase` mirrors
/// `state` (written under its lock) so each side can spin on it for a while
/// ([`CARRIER_HANDOFF_SPIN`], [`RUNNER_HANDOFF_SPIN`]) before it sleeps: a
/// slice is often tens of microseconds of Java between a mount and a park,
/// and a sleep-and-wake on both sides of it costs as much again (the JDK's
/// carrier and its runner hand off twice per slice).
/// `sleepers` counts the threads waiting on `cv`; a writer notifies only when
/// there is one.
struct RunnerSlot {
    state: Mutex<SlotState>,
    cv: Condvar,
    phase: AtomicU8,
    sleepers: AtomicU32,
}

const PHASE_IDLE: u8 = 0;
const PHASE_JOB: u8 = 1;
const PHASE_RUNNING: u8 = 2;
const PHASE_DONE: u8 = 3;

/// How long each side of a hand-off spins before it sleeps.
///
/// The carrier waits out the slice it handed over: a park-to-park slice of
/// the JDK's scheduling code runs tens of microseconds here, and a carrier
/// woken from a sleep costs as much again. The runner waits for its next
/// slice, which the JDK's scheduler hands it a carrier-side turn later. Both
/// measured on the host's `VtcPing` (`tools/probes/interp/L6/`): no spin,
/// ~95 us per switch; these, ~70 us; 50 us both ways, worse than none (the
/// spins expire just before the hand-off).
const CARRIER_HANDOFF_SPIN: std::time::Duration = std::time::Duration::from_micros(200);
const RUNNER_HANDOFF_SPIN: std::time::Duration = std::time::Duration::from_micros(100);

/// Spin until `phase` reads `want` (`true`) or `limit` passes. `yields`
/// (`CRATONVM_VTHREAD_HANDOFF_SPIN_YIELDS`, interpreter round i5 wave 106,
/// lane L6): every 64 turns the spinner also gives its core away, so on a
/// machine with as many carriers as cores the runner doing the slice is not
/// sharing a core with its own carrier's spin.
fn spin_for_phase(phase: &AtomicU8, want: u8, limit: std::time::Duration, yields: bool) -> bool {
    let start = std::time::Instant::now();
    let mut turns = 0u32;
    loop {
        if phase.load(Ordering::Acquire) == want {
            return true;
        }
        std::hint::spin_loop();
        turns = turns.wrapping_add(1);
        if turns % 64 == 0 {
            if start.elapsed() >= limit {
                return false;
            }
            if yields {
                std::thread::yield_now();
            }
        }
    }
}

impl RunnerSlot {
    /// Set `state` / `phase` (the caller holds the lock) and wake a sleeper.
    fn publish(&self, st: &mut SlotState, next: SlotState, phase: u8) {
        *st = next;
        self.phase.store(phase, Ordering::Release);
        if self.sleepers.load(Ordering::Acquire) > 0 {
            self.cv.notify_all();
        }
    }
}

/// The runner OS threads of one VM's JDK continuations, and which carrier
/// hosts which continuation right now.
#[derive(Default)]
pub(crate) struct ContinuationRunners {
    idle: Mutex<Vec<Arc<RunnerSlot>>>,
    spawned: AtomicU64,
    /// Carrier registry id -> the registry id of the continuation it hosts
    /// (between the hand-off and the slice's end): an interrupt of the carrier
    /// also wakes that thread's `Object.wait` (`vm_exec::thread_interrupt`).
    hosted: Mutex<FxHashMap<u64, u64>>,
    /// Interpreter site caches of finished continuation threads, for the next
    /// ones (`VmConfig::vthread_recycle_exec_caches`; at most
    /// [`EXEC_CACHE_POOL_CAP`]).
    exec_caches: Mutex<Vec<Box<ExecCaches>>>,
    /// `CRATONVM_DBG_VTHREAD_CENSUS` (`VmConfig::dbg_vthread_census`).
    census: ContinuationCensus,
    /// gci i25/vt: carrier registry id -> `(continuation thread id, its
    /// manager record's mount count)` of the post-yield window the carrier's
    /// last `enterSpecial` left open ([`open_post_yield_window`]); taken by
    /// the carrier's next `enterSpecial` or Java-level park
    /// ([`resolve_post_yield_window`]). At most one entry per carrier.
    post_yield_windows: Mutex<FxHashMap<u64, (u64, u64)>>,
    /// gci i25/vt: continuation thread ids whose `afterYield()` died before
    /// it moved them on, already moved to BLOCKED by
    /// [`resolve_post_yield_window`], for the JDK's unblocker
    /// (`takeVirtualThreadListToUnblock`, [`with_post_yield_orphans`]).
    post_yield_orphans: Mutex<Vec<u64>>,
    /// Methods whose compiled body a yield found beneath it on a virtual
    /// thread (`VmConfig::vthread_compiled_learned`; interpreter round i5
    /// wave 101, lane L2): refused on virtual threads from then on.
    learned_denials: VthreadCompiledDenySet,
    /// Interpreter round i6 wave 117, lane L6 (page
    /// `interpreter-i117-L6-an-entry-arm-pin-closes-the-learned-osr-door-FIXED-20261019`):
    /// methods refused at the ENTRY doors only, learned from a pin whose
    /// compiled stack held no OSR activation while an entry arm was on
    /// (`CRATONVM_VTHREAD_COMPILED_APP_ENTRY` /
    /// `CRATONVM_VTHREAD_COMPILED_LEARNED_ENTRY`). The OSR door does not read
    /// it, so a method whose entry-compiled body pinned keeps its OSR'd loops
    /// ([`learn_compiled_pin`]); `CRATONVM_VTHREAD_ENTRY_PIN_DENIES_ENTRY_ONLY=0`
    /// records such pins in [`Self::learned_denials`] as before.
    learned_entry_denials: VthreadCompiledDenySet,
    /// Interpreter round i6 wave 118, lane L6
    /// (`CRATONVM_VTHREAD_LEAF_VERDICT_MEMO`, default off; page
    /// `i114-L6-proposal-the-virtual-door-on-virtual-threads`, "the second
    /// per-call cost"): the leaf scan's answer per method body
    /// ([`LeafVerdictMemo`]).
    leaf_verdicts: LeafVerdictMemo,
    /// Interpreter round i5 wave 104, lane L6: carrier-compensated pinned
    /// parks taken under `CRATONVM_VTHREAD_COMPILED_LEARNED`
    /// ([`vm_pinned_park_compensates`]), and the trip-wire they feed
    /// ([`LEARNED_COMPENSATION_SLACK`]): once set, the ENTRY doors stop
    /// admitting bodies by the learned arm on this VM (the OSR door keeps
    /// it), because the pins are not converging -- each one costs a
    /// compensating carrier, and a workload that keeps paying them is slower
    /// than with the switch off (`L6W103VirtualThreadLearnedShortBench`
    /// `helpers` row, 6.9x on w103b).
    learned_compensations: AtomicU64,
    learned_entry_tripped: AtomicBool,
    /// Interpreter round i5 wave 105, lane L6 (page
    /// `i104-L6-virtual-thread-pinned-event-fields`): continuation thread id
    /// -> `(pin reason, JFR ticks)` of its last refused `doYield`, HotSpot's
    /// `last_freeze_fail_result` / `last_freeze_fail_time`, kept only while a
    /// JFR recording runs. `jdk.VirtualThreadPinned` (`postPinnedEvent`)
    /// takes it: the event starts at the failed yield and names its cause.
    freeze_fails: Mutex<FxHashMap<u64, (i32, i64)>>,
}

/// Slots of [`LeafVerdictMemo`] (a power of two).
const LEAF_VERDICT_SLOTS: usize = 512;

/// One [`LeafVerdictMemo`] slot: the body it answers for, kept alive by the
/// memo so its address is never reused while the slot names it, and the
/// verdict word: `(redefinition epoch << 2) | v`, `v` 0 unknown, 1 can reach
/// a yield, 2 cannot.
struct LeafVerdictSlot {
    code: std::sync::OnceLock<Arc<[u8]>>,
    verdict: AtomicU64,
}

/// The answers of `bytecode_cannot_reach_a_yield` per method body, for the
/// leaf arm of [`vthread_may_enter_compiled_method`], which a JDK virtual
/// thread asks on every call of an admitted static leaf (the static door's
/// `Jit` arm re-asks per hit) and on every interpreted call of a candidate
/// (interpreter round i6 wave 118, lane L6; `CRATONVM_VTHREAD_LEAF_VERDICT_MEMO`,
/// default off). A leaf has no invoke, so its scan never stops early: up to
/// `VTHREAD_LEAF_MAX_CODE_BYTES` bytes per call. Here it is one acquire load,
/// a pointer compare and a byte load.
///
/// Direct-mapped by the body's address; the first body to claim a slot keeps
/// it for the life of the VM (the slot holds an `Arc` of the code, so the
/// address cannot be reused by another body and the verdict cannot go stale:
/// at most `LEAF_VERDICT_SLOTS` bodies of at most
/// `VTHREAD_LEAF_MAX_CODE_BYTES` bytes stay mapped). A body whose slot another
/// claimed is scanned as before. The scan is deterministic, so two threads
/// that race on one slot store the same verdict. Per VM; no process global.
pub(crate) struct LeafVerdictMemo {
    slots: Box<[LeafVerdictSlot]>,
}

impl Default for LeafVerdictMemo {
    fn default() -> Self {
        Self {
            slots: (0..LEAF_VERDICT_SLOTS)
                .map(|_| LeafVerdictSlot {
                    code: std::sync::OnceLock::new(),
                    verdict: AtomicU64::new(0),
                })
                .collect(),
        }
    }
}

impl LeafVerdictMemo {
    /// The verdict for `code`: memoized, or `scan()` (memoized when `code`
    /// owns its slot).
    ///
    /// Interpreter round i6 wave 119, lane L6: a verdict stored before the
    /// VM's last class redefinition (`class_redefinition_count`) is not
    /// answered; the body is scanned again and the slot re-stamped. The
    /// answer is a function of the body's bytes alone, which an `Arc<[u8]>`
    /// never changes (a redefinition installs a new body), so this only
    /// guards against a future in-place rewrite of a body; it costs one
    /// acquire load of the counter.
    pub(crate) fn verdict(&self, code: &Arc<[u8]>, scan: impl FnOnce() -> bool) -> bool {
        let epoch = crate::classloading::class_redefinition_count() & (u64::MAX >> 2);
        // Cast: the body's address, for the slot index only.
        let at = code.as_ptr() as usize;
        let slot = &self.slots[((at >> 3) ^ (at >> 13)) & (LEAF_VERDICT_SLOTS - 1)];
        match slot.code.get() {
            Some(held) if Arc::ptr_eq(held, code) => {
                let word = slot.verdict.load(Ordering::Acquire);
                if word >> 2 == epoch {
                    match word & 3 {
                        1 => return false,
                        2 => return true,
                        _ => {}
                    }
                }
            }
            Some(_) => return scan(),
            None => {}
        }
        let answer = scan();
        if slot.code.get().is_none() {
            let _ = slot.code.set(Arc::clone(code));
        }
        if slot.code.get().is_some_and(|held| Arc::ptr_eq(held, code)) {
            slot.verdict
                .store((epoch << 2) | if answer { 2 } else { 1 }, Ordering::Release);
        }
        answer
    }
}

/// Entries past which [`ContinuationRunners::freeze_fails`] is cleared (a
/// pinned yield whose park never posts the event, under a long recording).
const FREEZE_FAILS_CAP: usize = 4096;

/// Compensated pinned parks a VM may take under
/// `CRATONVM_VTHREAD_COMPILED_LEARNED` beyond [`LEARNED_COMPENSATIONS_PER_DENIAL`]
/// per learned method before the learned arm's entry doors close
/// (interpreter round i5 wave 104, lane L6). A converging workload pays one
/// pin per blocking method plus the activations already running compiled
/// when the method was learned (bounded by the carriers); a park whose pin
/// teaches nothing (a deopt-resumed frame run to completion under a VM pin,
/// a compiled activation entered before its denial and parking in a loop)
/// pays one on every park, forever.
const LEARNED_COMPENSATION_SLACK: u64 = 512;

/// See [`LEARNED_COMPENSATION_SLACK`].
const LEARNED_COMPENSATIONS_PER_DENIAL: u64 = 32;

/// Slots of [`VthreadCompiledDenySet`] (a power of two).
const VTHREAD_DENY_SLOTS: usize = 1024;

/// Entries past which [`VthreadCompiledDenySet`] answers "denied" for every
/// method: a full table cannot prove a method absent, and refusing is the
/// safe direction (the method only runs interpreted on virtual threads).
const VTHREAD_DENY_SATURATED: usize = VTHREAD_DENY_SLOTS * 3 / 4;

/// The per-VM set of methods refused compiled entry on JDK virtual threads
/// under `CRATONVM_VTHREAD_COMPILED_LEARNED` (interpreter round i5 wave 101,
/// lane L2; proposal `i100-L6-proposal-learned-compiled-entry-on-virtual-
/// threads`). Keyed by a 64-bit fingerprint of the method's invocation key
/// (`cratonvm_jit_api::invoc_key_parts`: class id, name, descriptor), so
/// every door that holds a `CachedBytecodeMethod` asks with its memoised
/// `invoc_key()` and no string is hashed per call. Lock-free open addressing;
/// entries are never removed (a redefinition or a re-load gets new keys
/// anyway, and a stale entry only keeps one method interpreted on virtual
/// threads). A fingerprint collision refuses a second method too: the safe
/// direction. Not a process static: it lives in the VM's
/// [`ContinuationRunners`].
pub(crate) struct VthreadCompiledDenySet {
    slots: Box<[AtomicU64]>,
    len: AtomicU64,
}

impl Default for VthreadCompiledDenySet {
    fn default() -> Self {
        Self {
            slots: (0..VTHREAD_DENY_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            len: AtomicU64::new(0),
        }
    }
}

impl VthreadCompiledDenySet {
    /// The non-zero fingerprint of invocation key `key`.
    #[inline]
    fn fingerprint(key: u128) -> u64 {
        // Truncations: the two halves of the key, folded.
        let folded = (key as u64) ^ ((key >> 64) as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        if folded == 0 {
            1
        } else {
            folded
        }
    }

    /// Entries recorded so far.
    pub(crate) fn len(&self) -> u64 {
        self.len.load(Ordering::Relaxed)
    }

    /// Is the method of invocation key `key` refused? One load while the set
    /// is empty. A racing insert may be missed once: that admits the method
    /// one more time, and its next pin records it again (sound).
    #[inline]
    pub(crate) fn contains(&self, key: u128) -> bool {
        let len = self.len.load(Ordering::Acquire);
        if len == 0 {
            return false;
        }
        // Widening: the bound is a small constant.
        if len >= VTHREAD_DENY_SATURATED as u64 {
            return true;
        }
        let fp = Self::fingerprint(key);
        let mask = VTHREAD_DENY_SLOTS - 1;
        // Truncation: only the low bits pick the slot.
        let mut at = (fp as usize) & mask;
        for _ in 0..VTHREAD_DENY_SLOTS {
            match self.slots[at].load(Ordering::Acquire) {
                0 => return false,
                v if v == fp => return true,
                _ => at = (at + 1) & mask,
            }
        }
        true
    }

    /// Record `key`; `true` when it was not there yet.
    pub(crate) fn insert(&self, key: u128) -> bool {
        if self.len.load(Ordering::Acquire) >= VTHREAD_DENY_SATURATED as u64 {
            return false;
        }
        let fp = Self::fingerprint(key);
        let mask = VTHREAD_DENY_SLOTS - 1;
        // Truncation: only the low bits pick the slot.
        let mut at = (fp as usize) & mask;
        for _ in 0..VTHREAD_DENY_SLOTS {
            match self.slots[at].compare_exchange(0, fp, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    self.len.fetch_add(1, Ordering::AcqRel);
                    return true;
                }
                Err(v) if v == fp => return false,
                Err(_) => at = (at + 1) & mask,
            }
        }
        // Full: saturate, which refuses every method from now on.
        self.len
            .fetch_max(VTHREAD_DENY_SATURATED as u64, Ordering::AcqRel);
        false
    }
}

/// Finished continuation threads' site caches kept for the next ones. A
/// carrier pool's worth (one per runner at a time) is enough; the cap bounds
/// what a burst of deaths can keep alive (each table is at most a few hundred
/// KiB once filled, and a `Jit` entry keeps its body mapped until the next
/// owner's withdrawal sweep).
const EXEC_CACHE_POOL_CAP: usize = 64;

/// The interpreter site caches of a `JvmThread` that a finished continuation
/// thread hands to the next one (`VmConfig::vthread_recycle_exec_caches`;
/// interpreter round i4 wave 94, lane L6, page
/// `i93-L6-virtual-thread-start-pays-a-cold-invoke-cache`).
///
/// Why moving them between threads is sound: none of them holds a heap
/// reference (no GC root scan reads any of them; they hold class ids, `Arc`s
/// of resolved metadata and compiled bodies), and every entry validates itself
/// on each hit -- the invoke cache against its redefinition stamp and each
/// target's `RedefineGate` / supersede gate, the site tables against the
/// per-entry epochs they were filled under -- never against the identity of
/// the thread. So a recycled table answers what the same table would answer
/// on one long-lived thread that ran both tasks, which is what a platform
/// pool thread is. The invoke-dynamic table (`indy_sites`, which lends `Arc`s
/// of linked call sites) and the signature-polymorphic routes are left out.
/// The new owner starts with `jit_withdrawals_seen == 0`, so its first
/// withdrawal sweep (`jit_bridge::release_withdrawn_code_owners`) drops any
/// withdrawn body a recycled `Jit` entry still owns.
pub(crate) struct ExecCaches {
    invoke_cache: crate::classloading::resolution::InvokeCache<cratonvm_jit::RetainedCode>,
    field_sites: crate::runtime::interpreter::FieldSiteCache,
    site_tables: crate::runtime::interpreter::InterpSiteTables,
    trivial_getter_verdicts:
        crate::runtime::interpreter::site_cache::SiteCache<(std::sync::Arc<str>, bool)>,
    method_sites: crate::runtime::interpreter::MethodSiteCache,
    class_sites: crate::runtime::interpreter::ClassSiteCache,
    cast_sites: crate::runtime::interpreter::CastSiteCache,
    iface_select_sites: crate::runtime::interpreter::IfaceSelectSiteCache,
}

impl ExecCaches {
    /// Take the caches of a dead continuation thread; the rest of it drops.
    fn take_from(thread: Box<JvmThread>) -> Box<Self> {
        let thread = *thread;
        Box::new(Self {
            invoke_cache: thread.invoke_cache,
            field_sites: thread.field_sites,
            site_tables: thread.site_tables,
            trivial_getter_verdicts: thread.trivial_getter_verdicts,
            method_sites: thread.method_sites,
            class_sites: thread.class_sites,
            cast_sites: thread.cast_sites,
            iface_select_sites: thread.iface_select_sites,
        })
    }

    /// Give a new continuation thread these caches (its own empty ones drop).
    fn install(self: Box<Self>, thread: &mut JvmThread) {
        let caches = *self;
        thread.invoke_cache = caches.invoke_cache;
        thread.field_sites = caches.field_sites;
        thread.site_tables = caches.site_tables;
        thread.trivial_getter_verdicts = caches.trivial_getter_verdicts;
        thread.method_sites = caches.method_sites;
        thread.class_sites = caches.class_sites;
        thread.cast_sites = caches.cast_sites;
        thread.iface_select_sites = caches.iface_select_sites;
    }
}

/// `CRATONVM_DBG_VTHREAD_CENSUS` (interpreter round i4 wave 94, lane L6; page
/// `i91-L6-virtual-thread-code-is-never-compiled`, wave 94): where a JDK
/// virtual thread's time goes, per VM, summed over every slice and printed at
/// exit (`SharedVm::report_vthread_census_at_exit`). Nanoseconds unless named
/// otherwise. Only `enter_special` / `run_slice` / `do_yield` write it, and
/// only with the switch on.
#[derive(Default)]
pub(crate) struct ContinuationCensus {
    /// First slices (`enterSpecial(c, false, true)`): one per started thread.
    first_slices: AtomicU64,
    /// Remounts (`enterSpecial(c, true, true)`).
    resumed_slices: AtomicU64,
    /// Slices that ended in a yield (unmount).
    yielded: AtomicU64,
    /// Slices that ended the continuation.
    done: AtomicU64,
    /// Carrier: registering a new continuation thread (`register_continuation_thread`,
    /// the manager entry, the start events).
    register_ns: AtomicU64,
    /// ... of which (wave 95) the lookup of an earlier registration (the
    /// `Thread.tid` index, then -- without `CRATONVM_VTHREAD_ONE_PUBLISH` or
    /// `CRATONVM_VTHREAD_LEAN_REGISTRY` -- the pointer walk of every entry
    /// under the registry's write lock) ...
    register_lookup_ns: AtomicU64,
    /// ... `register_continuation_thread` (the `JvmThread`, the registry
    /// publishes) ...
    register_publish_ns: AtomicU64,
    /// ... and the manager entry with the start events.
    register_manager_ns: AtomicU64,
    /// Carrier: `run_on_runner` wall time (hand-off both ways plus the slice).
    carrier_wait_ns: AtomicU64,
    /// Wave 95: from the carrier's post of the job to the runner taking it
    /// (the outbound leg of the hand-off; the return leg is
    /// `carrier_wait_ns - slice_ns - pickup_ns`).
    pickup_ns: AtomicU64,
    /// Wave 95: jobs posted while the runner slept on its condition variable
    /// (its 100 us spin had expired, so the post paid a wake-up) ...
    jobs_to_sleeping_runner: AtomicU64,
    /// ... slices whose outcome the carrier saw in its 200 us spin, and
    /// slices it did not (its spin expired, or `CRATONVM_VTHREAD_CARRIER_SLEEPS`;
    /// it went to the condition variable and, unless the slice ended in
    /// between, paid a wake-up on the return leg) ...
    carrier_spin_hits: AtomicU64,
    carrier_sleeps: AtomicU64,
    /// ... and runner OS threads spawned (no idle one was free).
    runners_spawned: AtomicU64,
    /// Runner: `run_slice` wall time. `carrier_wait_ns - slice_ns` is the
    /// hand-off between the two OS threads.
    slice_ns: AtomicU64,
    /// Runner: mount (park / interrupt swap, registry publishes, GC state).
    mount_ns: AtomicU64,
    /// ... of which (wave 95) the registry publishes (mirror read, OS tid,
    /// `mounted`, the `JvmThread` and TLAB addresses).
    mount_registry_ns: AtomicU64,
    /// Runner: Java (`Continuation.enter` or the resumed frames), until the
    /// yield or the end.
    java_ns: AtomicU64,
    /// Runner: unmount after a yield.
    unmount_ns: AtomicU64,
    /// Runner: teardown after the end, all of it ...
    teardown_ns: AtomicU64,
    /// ... of which the registry death (`mark_dead` under the GC barrier).
    teardown_registry_ns: AtomicU64,
    /// ... of which the monitor sweep (`release_monitors_held_by_except`,
    /// a walk of every inflated monitor in the VM).
    teardown_monitor_sweep_ns: AtomicU64,
    /// Wave 95, `CRATONVM_VTHREAD_ONE_PUBLISH`: the second half of the
    /// registry death (mirror index, the terminated tail's purge) after the
    /// barrier lock, and how many deaths purged.
    teardown_purge_ns: AtomicU64,
    teardown_purges: AtomicU64,
    /// Wave 95, `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`: deaths whose sweep
    /// was skipped (no monitor entered outside structured locking), and of
    /// those, the ones whose check walk (census only, outside the timed
    /// sweep) still force-released a monitor -- each one a monitor the count
    /// missed; must stay 0.
    death_sweeps_skipped: AtomicU64,
    death_sweep_skip_misses: AtomicU64,
    /// Wave 99 (lane L6): the same two for PLATFORM thread deaths
    /// (`thread_start`'s termination, JNI `DetachCurrentThread`, the
    /// generational concurrent service; [`platform_death_releases_monitors`]).
    /// Their misses also count in `death_sweep_skip_misses`, the one gate.
    platform_death_sweeps_skipped: AtomicU64,
    platform_death_sweep_skip_misses: AtomicU64,
    /// New continuation threads that got recycled caches / fresh ones.
    caches_recycled: AtomicU64,
    caches_fresh: AtomicU64,
    /// Dead threads' caches pooled / dropped (pool full).
    caches_pooled: AtomicU64,
    caches_dropped: AtomicU64,
    /// `Continuation.doYield` answers: pinned because a compiled frame is
    /// below (the stage-3 demand: what a deopt at yield would unmount),
    /// pinned for any other reason, and unmounted.
    yields_pinned_by_compiled: AtomicU64,
    yields_pinned_other: AtomicU64,
    yields_taken: AtomicU64,
    /// gci i24/vl: yields the carrier refused because its post-yield probe
    /// found no heap ([`refuse_yield_without_headroom`]); each re-ran its
    /// slice with `doYield` answering a pin. Not counted in `resumed_slices`.
    yields_refused_no_headroom: AtomicU64,
    /// gci i25/vt: post-yield windows opened (yields that left the virtual
    /// thread TIMED_PARKING / TIMED_WAITING, [`open_post_yield_window`]) and
    /// virtual threads re-queued because their `afterYield()` died before it
    /// moved them on ([`resolve_post_yield_window`]; 0 unless the heap ran
    /// out in that window).
    post_yield_windows_opened: AtomicU64,
    post_yield_orphans_requeued: AtomicU64,
    /// Wave 101 (lane L2, `CRATONVM_VTHREAD_COMPILED_LEARNED`): pins by a
    /// compiled frame that recorded at least one new method in the deny set
    /// (must plateau on a steady workload), and pinned parks that took the
    /// carrier compensation for a VM-made pin.
    learned_pins: AtomicU64,
    learned_compensated_parks: AtomicU64,
    /// Wave 108 (lane L6, page `i91-L6`): static-door nominations the learned
    /// arm let through to the tiered manager, and due strides it refused
    /// because the method's nomination window was spent
    /// ([`LEARNED_NOMINATION_STRIDES`]). The first must plateau at a few per
    /// hot method; a growing second row is a method whose body never
    /// published (a declined, deoptimized or differently keyed compile).
    learned_nominations: AtomicU64,
    learned_nominations_past_window: AtomicU64,
    /// Wave 108: of `learned_compensated_parks`, those with NO compiled frame
    /// on the chain (only a VM pin: a deopt sink's `vthread_resume_pin`, a
    /// lambda door's constructor pin). Such a pin teaches the deny set
    /// nothing ([`learn_compiled_pin`] needs a compiled frame), so a row that
    /// grows with the rep count is a pin paid on every park.
    learned_vm_pin_parks: AtomicU64,
    /// Wave 110 (lane L6, page `i91-L6`): compiled pins [`learn_compiled_pin`]
    /// did NOT record because the slice had already run
    /// [`LEARNED_DENY_MIN_SLICE`] (a long compiled stretch, e.g. an OSR'd
    /// loop, paying one pinned park per stretch).
    learned_pins_kept_long_slice: AtomicU64,
    /// Interpreter round i6 wave 117, lane L6 (page
    /// `i117-L6-an-entry-arm-pin-closes-the-learned-osr-door`): the
    /// entry-arm storm's rows.
    ///
    /// * `learned_entry_pins`: compiled pins with no OSR activation on the
    ///   stack that recorded a new method in the ENTRY-only deny set.
    /// * `entry_admits_app` / `entry_admits_learned` / `entry_admits_leaf`:
    ///   interpreted-to-compiled entry admissions on a JDK virtual thread, per
    ///   arm of [`vthread_may_enter_compiled_method`] (counted per call with
    ///   the census on only). Millions per rep is the per-call transition
    ///   cost of an admitted non-leaf callee.
    /// * `osr_refused_by_denial`: back-edge OSR offers the learned arm
    ///   refused because the method is in the (OSR) deny set. Before this
    ///   wave an entry-arm pin landed there, and the method's loops ran
    ///   interpreted for the life of the VM: this row grew with every
    ///   activation.
    /// * `deopt_resume_pins`: deopt-resumed runs on a JDK virtual thread that
    ///   held the continuation pin (`deopt_resume::vthread_resume_pin`); each
    ///   park in one is a compensated, unlearned pin (`learned_vm_pin_parks`).
    learned_entry_pins: AtomicU64,
    entry_admits_app: AtomicU64,
    entry_admits_learned: AtomicU64,
    entry_admits_leaf: AtomicU64,
    osr_refused_by_denial: AtomicU64,
    deopt_resume_pins: AtomicU64,
    /// Interpreter round i6 wave 120, lane L6 (`CRATONVM_VTHREAD_SLICE_ON_CARRIER`):
    /// `enterSpecial` calls whose slices ran on the carrier's own OS thread,
    /// and those the admission sent to a runner (a compiled frame or an open
    /// JNI local frame of the carrier on this OS thread). Counted only with
    /// the switch on.
    slices_on_carrier: AtomicU64,
    slices_on_carrier_refused: AtomicU64,
}

/// A census clock: `Some(now)` only with the census on.
#[inline]
fn census_clock(on: bool) -> Option<std::time::Instant> {
    if on {
        Some(std::time::Instant::now())
    } else {
        None
    }
}

/// Add the time since `since` (if timing) to `cell`.
#[inline]
fn census_add(cell: &AtomicU64, since: Option<std::time::Instant>) {
    if let Some(start) = since {
        let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        cell.fetch_add(ns, Ordering::Relaxed);
    }
}

/// The nanoseconds since `since` (if timing), else 0.
#[inline]
fn census_elapsed(since: Option<std::time::Instant>) -> u64 {
    since.map_or(0, |start| {
        u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    })
}

impl ContinuationCensus {
    fn rows(&self) -> Vec<(&'static str, u64)> {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        vec![
            ("first_slices", get(&self.first_slices)),
            ("resumed_slices", get(&self.resumed_slices)),
            ("yielded", get(&self.yielded)),
            ("done", get(&self.done)),
            ("register_ns", get(&self.register_ns)),
            ("register_lookup_ns", get(&self.register_lookup_ns)),
            ("register_publish_ns", get(&self.register_publish_ns)),
            ("register_manager_ns", get(&self.register_manager_ns)),
            ("carrier_wait_ns", get(&self.carrier_wait_ns)),
            ("pickup_ns", get(&self.pickup_ns)),
            ("jobs_to_sleeping_runner", get(&self.jobs_to_sleeping_runner)),
            ("carrier_spin_hits", get(&self.carrier_spin_hits)),
            ("carrier_sleeps", get(&self.carrier_sleeps)),
            ("runners_spawned", get(&self.runners_spawned)),
            ("slice_ns", get(&self.slice_ns)),
            ("mount_ns", get(&self.mount_ns)),
            ("mount_registry_ns", get(&self.mount_registry_ns)),
            ("java_ns", get(&self.java_ns)),
            ("unmount_ns", get(&self.unmount_ns)),
            ("teardown_ns", get(&self.teardown_ns)),
            ("teardown_registry_ns", get(&self.teardown_registry_ns)),
            ("teardown_monitor_sweep_ns", get(&self.teardown_monitor_sweep_ns)),
            ("teardown_purge_ns", get(&self.teardown_purge_ns)),
            ("teardown_purges", get(&self.teardown_purges)),
            ("death_sweeps_skipped", get(&self.death_sweeps_skipped)),
            ("death_sweep_skip_misses", get(&self.death_sweep_skip_misses)),
            (
                "platform_death_sweeps_skipped",
                get(&self.platform_death_sweeps_skipped),
            ),
            (
                "platform_death_sweep_skip_misses",
                get(&self.platform_death_sweep_skip_misses),
            ),
            ("caches_recycled", get(&self.caches_recycled)),
            ("caches_fresh", get(&self.caches_fresh)),
            ("caches_pooled", get(&self.caches_pooled)),
            ("caches_dropped", get(&self.caches_dropped)),
            ("yields_pinned_by_compiled", get(&self.yields_pinned_by_compiled)),
            ("yields_pinned_other", get(&self.yields_pinned_other)),
            ("yields_taken", get(&self.yields_taken)),
            (
                "yields_refused_no_headroom",
                get(&self.yields_refused_no_headroom),
            ),
            (
                "post_yield_windows_opened",
                get(&self.post_yield_windows_opened),
            ),
            (
                "post_yield_orphans_requeued",
                get(&self.post_yield_orphans_requeued),
            ),
            ("learned_pins", get(&self.learned_pins)),
            (
                "learned_compensated_parks",
                get(&self.learned_compensated_parks),
            ),
            ("learned_nominations", get(&self.learned_nominations)),
            (
                "learned_nominations_past_window",
                get(&self.learned_nominations_past_window),
            ),
            ("learned_vm_pin_parks", get(&self.learned_vm_pin_parks)),
            (
                "learned_pins_kept_long_slice",
                get(&self.learned_pins_kept_long_slice),
            ),
            ("learned_entry_pins", get(&self.learned_entry_pins)),
            ("entry_admits_app", get(&self.entry_admits_app)),
            ("entry_admits_learned", get(&self.entry_admits_learned)),
            ("entry_admits_leaf", get(&self.entry_admits_leaf)),
            ("osr_refused_by_denial", get(&self.osr_refused_by_denial)),
            ("deopt_resume_pins", get(&self.deopt_resume_pins)),
            ("slices_on_carrier", get(&self.slices_on_carrier)),
            (
                "slices_on_carrier_refused",
                get(&self.slices_on_carrier_refused),
            ),
        ]
    }
}

impl SharedVm {
    /// Print the `CRATONVM_DBG_VTHREAD_CENSUS` rows (silent when off): one
    /// line per counter, then the per-slice and per-thread averages a reader
    /// compares with the bench's per-task time (ms / tasks).
    pub fn report_vthread_census_at_exit(&self) {
        if !self.config.dbg_vthread_census {
            return;
        }
        let census = &self
            .threads
            .virtual_thread_manager
            .continuation_runners()
            .census;
        let rows = census.rows();
        for (name, value) in &rows {
            eprintln!("[vthread-census] {name}={value}");
        }
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let slices = get(&census.first_slices) + get(&census.resumed_slices);
        let threads = get(&census.first_slices);
        if slices > 0 {
            let carrier = get(&census.carrier_wait_ns);
            let slice = get(&census.slice_ns);
            eprintln!(
                "[vthread-census] per-slice ns: carrier_wait={} slice={} handoff={} mount={} java={} unmount={}",
                carrier / slices,
                slice / slices,
                carrier.saturating_sub(slice) / slices,
                get(&census.mount_ns) / slices,
                get(&census.java_ns) / slices,
                get(&census.unmount_ns) / slices,
            );
            // Wave 95: the hand-off's two legs, and the mount's registry part.
            eprintln!(
                "[vthread-census] per-slice ns: pickup={} return={} mount_registry={}",
                get(&census.pickup_ns) / slices,
                carrier
                    .saturating_sub(slice)
                    .saturating_sub(get(&census.pickup_ns))
                    / slices,
                get(&census.mount_registry_ns) / slices,
            );
        }
        if threads > 0 {
            eprintln!(
                "[vthread-census] per-thread ns: register={} teardown={} teardown_registry={} teardown_monitor_sweep={}",
                get(&census.register_ns) / threads,
                get(&census.teardown_ns) / threads,
                get(&census.teardown_registry_ns) / threads,
                get(&census.teardown_monitor_sweep_ns) / threads,
            );
            // Wave 95: the registration's three parts and the purge.
            eprintln!(
                "[vthread-census] per-thread ns: register_lookup={} register_publish={} register_manager={} teardown_purge={}",
                get(&census.register_lookup_ns) / threads,
                get(&census.register_publish_ns) / threads,
                get(&census.register_manager_ns) / threads,
                get(&census.teardown_purge_ns) / threads,
            );
        }
        // Wave 95: what the manager and the registry still hold at exit (the
        // leak row of `i94-L6-terminated-jdk-virtual-threads-stay-in-the-manager-map`).
        eprintln!(
            "[vthread-census] at-exit: manager_entries={} registry_entries={}",
            self.threads.virtual_thread_manager.thread_count(),
            self.threads.thread_registry.count(),
        );
        // Wave 101 (lane L2): the learned deny set's size.
        eprintln!(
            "[vthread-census] at-exit: learned_denied_methods={}",
            self.threads
                .virtual_thread_manager
                .continuation_runners()
                .learned_denials
                .len(),
        );
        // Wave 117 (lane L6): the entry-only deny set's size.
        eprintln!(
            "[vthread-census] at-exit: learned_entry_denied_methods={}",
            self.threads
                .virtual_thread_manager
                .continuation_runners()
                .learned_entry_denials
                .len(),
        );
        // Wave 104 (lane L6): the learned arm's compensation trip-wire.
        let runners = self.threads.virtual_thread_manager.continuation_runners();
        eprintln!(
            "[vthread-census] at-exit: learned_compensations={} learned_entry_tripped={}",
            runners.learned_compensations.load(Ordering::Relaxed),
            runners.learned_entry_tripped.load(Ordering::Relaxed),
        );
    }
}

impl ContinuationRunners {
    /// The continuation carrier `carrier` hosts now, if any.
    pub(crate) fn hosted_by(&self, carrier: ThreadId) -> Option<ThreadId> {
        self.hosted.lock().get(&carrier.0).copied().map(ThreadId)
    }

    /// The carrier hosting continuation thread `vt` now, if any.
    pub(crate) fn carrier_of(&self, vt: ThreadId) -> Option<ThreadId> {
        self.hosted
            .lock()
            .iter()
            .find(|&(_, &hosted)| hosted == vt.0)
            .map(|(&carrier, _)| ThreadId(carrier))
    }

    fn acquire(&self, shared: &Arc<SharedVm>) -> Result<Arc<RunnerSlot>, String> {
        if let Some(slot) = self.idle.lock().pop() {
            return Ok(slot);
        }
        let slot = Arc::new(RunnerSlot {
            state: Mutex::new(SlotState::Idle),
            cv: Condvar::new(),
            phase: AtomicU8::new(PHASE_IDLE),
            sleepers: AtomicU32::new(0),
        });
        let n = self.spawned.fetch_add(1, Ordering::Relaxed);
        let runner_slot = slot.clone();
        let runner_shared = shared.clone();
        std::thread::Builder::new()
            .name(format!("ContinuationRunner-{n}"))
            .stack_size(RUNNER_STACK_BYTES)
            .spawn(move || runner_main(runner_shared, runner_slot))
            .map_err(|e| format!("Continuation.enterSpecial: no runner thread: {e}"))?;
        // gci i30/gv: counted once the runner exists -- a refused spawn
        // (now retried by `acquire_runner_with_backoff`) spawned nothing.
        if shared.config.dbg_vthread_census {
            self.census.runners_spawned.fetch_add(1, Ordering::Relaxed);
        }
        Ok(slot)
    }

    fn release(&self, slot: Arc<RunnerSlot>) {
        self.idle.lock().push(slot);
    }

    /// A dead continuation thread's caches for the next one, if any.
    fn take_exec_caches(&self) -> Option<Box<ExecCaches>> {
        self.exec_caches.lock().pop()
    }

    /// Keep `thread`'s caches for the next continuation thread (or let them
    /// drop with it when the pool is full); `thread` is dead and unregistered.
    fn recycle_exec_caches(&self, thread: Box<JvmThread>, census_on: bool) {
        let room = self.exec_caches.lock().len() < EXEC_CACHE_POOL_CAP;
        if !room {
            if census_on {
                self.census.caches_dropped.fetch_add(1, Ordering::Relaxed);
            }
            drop(thread);
            return;
        }
        // Harvested outside the lock: the rest of the thread drops here.
        let caches = ExecCaches::take_from(thread);
        let mut pool = self.exec_caches.lock();
        if pool.len() < EXEC_CACHE_POOL_CAP {
            pool.push(caches);
            drop(pool);
            if census_on {
                self.census.caches_pooled.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            drop(pool);
            if census_on {
                self.census.caches_dropped.fetch_add(1, Ordering::Relaxed);
            }
            drop(caches);
        }
    }

    /// Take `slot` out of the idle list for good (its runner is retiring);
    /// `false` when a carrier has just acquired it.
    fn retire(&self, slot: &Arc<RunnerSlot>) -> bool {
        let mut idle = self.idle.lock();
        match idle.iter().position(|s| Arc::ptr_eq(s, slot)) {
            Some(i) => {
                idle.swap_remove(i);
                true
            }
            None => false,
        }
    }
}

fn runner_main(shared: Arc<SharedVm>, slot: Arc<RunnerSlot>) {
    loop {
        spin_for_phase(
            &slot.phase,
            PHASE_JOB,
            RUNNER_HANDOFF_SPIN,
            shared.config.vthread_handoff_spin_yields,
        );
        let job = {
            let mut st = slot.state.lock();
            loop {
                if matches!(*st, SlotState::Job(_)) {
                    match std::mem::replace(&mut *st, SlotState::Running) {
                        SlotState::Job(job) => {
                            slot.phase.store(PHASE_RUNNING, Ordering::Release);
                            break job;
                        }
                        _ => unreachable!(),
                    }
                }
                slot.sleepers.fetch_add(1, Ordering::AcqRel);
                let timed_out = slot.cv.wait_for(&mut st, RUNNER_IDLE).timed_out();
                slot.sleepers.fetch_sub(1, Ordering::AcqRel);
                if timed_out
                    && matches!(*st, SlotState::Idle)
                    && shared
                        .threads
                        .virtual_thread_manager
                        .continuation_runners()
                        .retire(&slot)
                {
                    return;
                }
            }
        };
        let outcome = run_slice(&shared, *job, false);
        let mut st = slot.state.lock();
        slot.publish(&mut st, SlotState::Done(outcome), PHASE_DONE);
    }
}

/// gci i30/gv (finding 5 of
/// `gci-i21rv-i20-default-path-review-findings-FIXED-20261017`): how long a carrier
/// waits for a runner when none is idle and no new one can be spawned (an OS
/// thread-spawn failure) before the slice fails. The wait doubles from 1 ms to
/// 64 ms per try; the sum is about 2 s.
const RUNNER_ACQUIRE_RETRIES: u32 = 36;

/// gci i30/gv (finding 5): [`ContinuationRunners::acquire`], retried with a
/// backoff while it fails. `acquire` fails only when no runner is idle AND an
/// OS thread cannot be spawned; a runner that finishes its slice goes back to
/// the idle list, so a wait usually finds one. Failing at once stranded a
/// RESUMED virtual thread: its state is deposited, but `enterSpecial` throws
/// with the Java thread RUNNING, `afterYield()` matches no state and nobody
/// resubmits it (every `join()` hangs). HotSpot's carriers never spawn
/// anything here, so waiting for one is its shape. The caller is in a
/// GC-blocked region (`begin_vm_wait_region`): a pause does not wait for it.
fn acquire_runner_with_backoff(
    runners: &ContinuationRunners,
    shared: &Arc<SharedVm>,
) -> Result<Arc<RunnerSlot>, String> {
    let mut wait = std::time::Duration::from_millis(1);
    let mut tries = 0u32;
    loop {
        match runners.acquire(shared) {
            Ok(slot) => return Ok(slot),
            Err(message) if tries >= RUNNER_ACQUIRE_RETRIES => return Err(message),
            Err(_) => {
                tries += 1;
                std::thread::sleep(wait);
                wait = (wait * 2).min(std::time::Duration::from_millis(64));
            }
        }
    }
}

/// Run `job` on a runner and wait for its outcome (the carrier's side; the
/// caller is in a GC-blocked region).
fn run_on_runner(shared: &Arc<SharedVm>, job: Job) -> SliceOutcome {
    let runners = shared.threads.virtual_thread_manager.continuation_runners();
    let slot = match acquire_runner_with_backoff(runners, shared) {
        Ok(slot) => slot,
        Err(message) => {
            let Job { thread, first, .. } = job;
            let tid = thread.thread_id;
            if first {
                // gci i11/vt item 8: a first slice was never mounted -- no
                // deposit, flag down, an empty snapshot -- and `enter_special`
                // fails it, so no second `enterSpecial` ever takes it back.
                // Depositing it left a live "starting" registry entry whose
                // mirror stayed a strong root for the life of the VM. Retire
                // the registration instead. The published TLAB / `JvmThread`
                // addresses point into the box dropped below, and a pause
                // reads them (`collect_reserved_tlab_tails`, any alive
                // entry): withdraw them and mark the entry dead under the
                // barrier's census lock, between pauses (this carrier is
                // GC-blocked, so a pause in progress completes without it).
                let registry = &shared.threads.thread_registry;
                while !shared.mem.gc_barrier.run_if_no_stw_requested(|| {
                    registry.clear_tlab_addr(tid);
                    registry.mark_dead(tid);
                }) {
                    std::thread::yield_now();
                }
                end_manager_record(shared, tid);
                drop(thread);
                // gci i21/rv (finding 4): retired, so ended.
                return SliceOutcome::FailedEnded(message);
            }
            // Nothing ran: the state goes back where a yield would put it.
            shared
                .threads
                .virtual_thread_manager
                .deposit_continuation(tid.0, thread);
            return SliceOutcome::Failed(message);
        }
    };
    let census_on = shared.config.dbg_vthread_census;
    let mut job = job;
    job.posted = census_clock(census_on);
    {
        let mut st = slot.state.lock();
        // Wave 95 census: a runner counted in `sleepers` now pays a wake-up.
        if census_on && slot.sleepers.load(Ordering::Acquire) > 0 {
            runners
                .census
                .jobs_to_sleeping_runner
                .fetch_add(1, Ordering::Relaxed);
        }
        slot.publish(&mut st, SlotState::Job(Box::new(job)), PHASE_JOB);
    }
    // `CRATONVM_VTHREAD_CARRIER_SLEEPS` (interpreter round i4 wave 94, lane
    // L6): sleep at once, leaving the core to the runner.
    let spun_to_done = !shared.config.vthread_carrier_sleeps
        && spin_for_phase(
            &slot.phase,
            PHASE_DONE,
            CARRIER_HANDOFF_SPIN,
            shared.config.vthread_handoff_spin_yields,
        );
    if census_on {
        let count = if spun_to_done {
            &runners.census.carrier_spin_hits
        } else {
            &runners.census.carrier_sleeps
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
    let outcome = {
        let mut st = slot.state.lock();
        loop {
            if matches!(*st, SlotState::Done(_)) {
                match std::mem::replace(&mut *st, SlotState::Idle) {
                    SlotState::Done(outcome) => {
                        slot.phase.store(PHASE_IDLE, Ordering::Release);
                        break outcome;
                    }
                    _ => unreachable!(),
                }
            }
            slot.sleepers.fetch_add(1, Ordering::AcqRel);
            slot.cv.wait(&mut st);
            slot.sleepers.fetch_sub(1, Ordering::AcqRel);
        }
    };
    runners.release(slot);
    outcome
}

/// The manager's half of a continuation's end: mark its record terminated
/// and, under `CRATONVM_VT_REAP_TERMINATED` (gci i11/vt item 4; per VM,
/// `VirtualThreadManager::reap_terminated`), forget it, so the manager does
/// not keep one record per virtual thread ever started. Not while a JDWP
/// debugger is configured: `ThreadReference.IsVirtual` of a dead thread
/// reads the record (`debug::thread_is_virtual`).
fn end_manager_record(shared: &SharedVm, tid: ThreadId) {
    let manager = &shared.threads.virtual_thread_manager;
    manager.terminate(tid.0);
    if manager.reap_terminated() && !crate::runtime::jvmti::debugger_observes_locals(shared) {
        manager.forget_terminated(tid.0);
    }
}

/// gci i14/ve (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`, item 6 of
/// `gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016`):
/// release the execution states of parked virtual threads a collection found
/// unreachable. HotSpot frees such a thread's stack chunks with its
/// `VirtualThread`; here the pause already retired each registry entry
/// (`ThreadRegistry::sweep_mirror_keyed_parked`: TLAB / `JvmThread`
/// addresses withdrawn, entry dead), so no pause reads the box any more and
/// nothing can mount it (a mount needs the mirror). Called by the door that
/// ran that pause, after the world was released (`run_collection_pause`).
///
/// What `run_slice`'s termination arm releases, minus every Java-visible
/// step (no `VirtualThread` code runs, no JFR `ThreadEnd`, no JDWP
/// `ThreadDeath`: HotSpot posts none for a collected thread): the
/// `ThreadLocal` values' global roots (released from the detached map
/// without installing it -- this OS thread is the door's, maybe inside a
/// `ThreadLocal` native), the JFR thread buffer's root, any monitor record,
/// and the manager's record (`end_manager_record`). The frames' references
/// are plain words in the box; dropping it frees no heap object.
///
/// It takes the manager's lock BLOCKING (`take_collected_continuation`,
/// `end_manager_record`), which is sound only because it runs after
/// `complete_gc`: no peer is stopped any more, so a holder releases it, and
/// the door itself holds no manager lock (no manager method allocates under
/// it, so no collection starts inside one). The pause's own read of the
/// manager is the non-blocking `try_deposited_jdk_continuation_ids`.
pub(crate) fn retire_collected_continuations(shared: &SharedVm, tids: Vec<ThreadId>) {
    let manager = &shared.threads.virtual_thread_manager;
    for tid in tids {
        let Some(mut thread) = manager.take_collected_continuation(tid.0) else {
            tracing::warn!(
                "gci i14/ve: collected virtual thread {} has no deposited state to release",
                tid.0
            );
            continue;
        };
        let locals = std::mem::take(&mut thread.detached_thread_locals);
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut thread,
        };
        cratonvm_native_builtins::phases_early::release_detached_thread_locals(&mut ctx, locals);
        cratonvm_native_builtins::jfr_vm_events::thread_buffer_released(&mut ctx);
        shared
            .threads
            .monitors
            .release_monitors_held_by_except(tid, None);
        end_manager_record(shared, tid);
        drop(thread);
    }
}

/// Run one slice of a continuation on this (runner) OS thread: mount its
/// thread, run it to its next yield or its end, unmount or tear it down.
/// The mount and unmount are `vm_exec::resume_virtual_continuation`'s, step
/// for step (see the comments there); what differs is what the slice runs
/// (`Continuation.enter`, not `Thread.run`), where an unmounted state goes
/// (back to the manager, for the JDK's scheduler to remount), and that the
/// thread's end runs no Java (`VirtualThread` ends itself).
///
/// `on_carrier` (interpreter round i6 wave 120, lane L6,
/// `CRATONVM_VTHREAD_SLICE_ON_CARRIER`): the slice runs on its CARRIER's own
/// OS thread, inside the carrier's VM wait region and a [`CarrierSliceScope`]
/// (`enter_special`), not on a runner. Then the `StackOverflowError` ceiling
/// is the carrier's own, and the depth counts on from `enterSpecial`'s: the
/// runner's fresh-stack ceiling is not installed.
fn run_slice(shared: &Arc<SharedVm>, job: Job, on_carrier: bool) -> SliceOutcome {
    let Job {
        mut thread,
        first,
        cont_root,
        carrier,
        carrier_park,
        carrier_interrupted,
        posted,
        refuse_yield,
    } = job;
    let tid = thread.thread_id;
    let registry = &shared.threads.thread_registry;
    let runners = shared.threads.virtual_thread_manager.continuation_runners();
    // `CRATONVM_DBG_VTHREAD_CENSUS` (interpreter round i4 wave 94, lane L6).
    let census_on = shared.config.dbg_vthread_census;
    let census = &runners.census;
    census_add(&census.pickup_ns, posted);
    let slice_clock = census_clock(census_on);
    // `CRATONVM_VTHREAD_ONE_PUBLISH` (interpreter round i4 wave 95, lane L6):
    // the mount's five registry trips are one read
    // (`ThreadRegistry::mount_continuation_thread`), below. Not with
    // `CRATONVM_DBG_BLOCKED_ACCESS`, whose canary `set_os_tid_current` registers.
    let one_publish =
        shared.config.vthread_one_publish && !cratonvm_gc::blocked_access_debug::enabled();
    // The carrier's `Parker` and interrupt status, for the slice.
    let own_park = std::mem::replace(&mut thread.park_state, carrier_park);
    let own_interrupted = std::mem::replace(&mut thread.interrupted, carrier_interrupted);
    thread.jdk_continuation.set_carrier(carrier.0);
    // Wave 110 (lane L6, `CRATONVM_VTHREAD_COMPILED_LEARNED`): the slice's
    // start, for `learn_compiled_pin`'s long-run exemption.
    thread.vt_slice_started = shared
        .config
        .vthread_compiled_learned
        .then(std::time::Instant::now);
    // The registry's mirror (remapped by every collection,
    // `update_thread_objs_after_gc`) is read only AFTER the remount's drain
    // below (gci i11/vt finding 2): `check_post_block_gc` applies the
    // blocked-window fixup -- keyed by the addresses the thread held when it
    // yielded -- to `java_thread_obj` (`remap_thread_single_slots`), and a
    // CURRENT address looked up in that chain is moved a second time under a
    // sliding compactor's `A -> B, B -> C` map (the mirror landed on another
    // root's vacated address): the continuation would resume as that object.
    // (Merge of 2026-10-15: dev's pre-drain read here, and the mirror
    // `mount_continuation_thread` answers, are not stored for that reason.)
    let mut mount_registry_ns = 0u64;
    let carrier_thread_locals = cratonvm_native_builtins::phases_early::swap_current_thread_locals(
        std::mem::take(&mut thread.detached_thread_locals),
    );
    // JIT round 20 wave 8 (lane huntFFM): this runner OS thread's FFM
    // verdicts and segment-door memo answered confinement for whichever
    // virtual thread ran here last (see `set_current_thread`).
    cratonvm_native_builtins::ffm_fast::forget_thread_verdicts();
    crate::jit::helpers::forget_real_seg_entries();
    let crash_frames = super::vm_util::PublishedFrameTrace::publish(
        &thread.name,
        tid.0,
        thread.frame_trace.clone(),
    );
    let registry_clock = census_clock(census_on);
    let jvm_thread_addr = (&*thread as *const JvmThread) as usize;
    let tlab_addr = &thread.tlab as *const cratonvm_gc::Tlab as usize;
    if one_publish {
        // The mirror it answers is pre-drain (see above): not stored.
        let _ = registry.mount_continuation_thread(tid, jvm_thread_addr, tlab_addr);
    } else {
        registry.set_os_tid_current(tid);
        registry.set_mounted(tid, true);
        registry.set_jvm_thread_addr(tid, jvm_thread_addr);
        registry.set_tlab_addr(tid, tlab_addr);
    }
    mount_registry_ns += census_elapsed(registry_clock);
    if census_on {
        census
            .mount_registry_ns
            .fetch_add(mount_registry_ns, Ordering::Relaxed);
    }
    if first {
        // A fresh `GcBlockState`, as the bound path's first mount -- and
        // pinned the way that path pins it (`resume_virtual_continuation`'s
        // CENSUS-RECONCILE asserts; gci i11/vt finding 5): a raised flag or a
        // non-empty fixup here would be a blocked window this bare store
        // drops unapplied.
        debug_assert!(
            !thread
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire),
            "first slice of a JDK continuation must have a virgin GcBlockState \
             (in_blocked_region down); a raised flag needs check_post_block_gc",
        );
        debug_assert!(
            thread.gc_block_state.fixup.lock().is_empty(),
            "first slice of a JDK continuation must have a virgin GcBlockState \
             (empty fixup)",
        );
        thread
            .gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
    } else {
        NativeContextImpl {
            shared,
            thread: &mut thread,
        }
        .check_post_block_gc();
    }
    // After the drain (see above): the registry's mirror is current, and a
    // first slice's `java_thread_obj` (set at registration, never deposited)
    // is refreshed from it before any Java runs.
    if let Some(mirror) = registry.java_thread_obj(tid) {
        thread.java_thread_obj = Some(mirror);
    }
    thread.gc_block_state.java_state.store(0, Ordering::Release);
    // Wave 120 (lane L6): on a runner, a fresh `RUNNER_STACK_BYTES` stack. On
    // the carrier, its own ceiling stays, and `interpreter::execute`'s
    // per-OS-thread depth counter keeps counting from `enterSpecial`'s depth,
    // so the slice's budget is the carrier stack left below `enterSpecial`.
    if !on_carrier {
        crate::runtime::interpreter::init_thread_exec_depth_ceiling(RUNNER_STACK_BYTES);
    }
    census_add(&census.mount_ns, slice_clock);
    let java_clock = census_clock(census_on);

    let result = if first {
        loop {
            let ready = shared.mem.gc_barrier.run_if_no_stw_requested(|| {
                registry.mark_stw_ready(tid);
            });
            if ready {
                break;
            }
            let pointer_map = shared.mem.gc_barrier.arrive_and_wait_excluded(tid);
            if !pointer_map.is_empty() {
                crate::runtime::interpreter::apply_pointer_map_to_thread(
                    &mut thread,
                    &pointer_map,
                    &shared.mem.heap,
                    &shared.classes.type_maps,
                );
            }
        }
        let cont = NativeContextImpl {
            shared,
            thread: &mut thread,
        }
        .resolve_global_root(cont_root);
        match cont {
            Some(cont) => super::vm_exec::invoke_static_shared(
                shared,
                &mut thread,
                CONTINUATION,
                ENTER.0,
                ENTER.1,
                &[Value::Object(Some(cont)), Value::Int(0)],
            ),
            None => Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "Continuation.enterSpecial: the continuation was not kept".to_string(),
            })),
        }
    } else {
        if shared.mem.gc_barrier.stw_requested.load(Ordering::Acquire) {
            crate::runtime::interpreter::safepoint_check(shared, &mut thread);
        }
        // gci i24/vl: after the drain, where the remount would push
        // `doYield`'s `0` (`interpreter::remount_action`).
        match refuse_deposited_yield(&mut thread, refuse_yield) {
            Ok(()) => crate::runtime::interpreter::resume_continuation(shared, &mut thread),
            Err(e) => Err(e),
        }
    };
    census_add(&census.java_ns, java_clock);
    let end_clock = census_clock(census_on);

    if let Err(MethodCallFailed::InternalError(VmError::ContinuationYield { .. })) = &result {
        // Unmount: `resume_virtual_continuation`'s yield arm.
        registry.set_mounted(tid, false);
        // RUNNABLE in the VM's books: the virtual thread's state is the JDK's
        // (`VirtualThread.threadState`), and a debugger asks the JDK for it.
        thread.gc_block_state.java_state.store(0, Ordering::Release);
        // Retire the TLAB while still a COUNTED mutator, before the deposit
        // raises `in_blocked_region` (gci i11/vt finding 1): the invariant of
        // `begin_blocking_region_with_state`'s CRIT (TLAB UAF) note and of
        // `ThreadRegistry::collect_reserved_tlab_tails` -- no blocked thread
        // keeps a TLAB. `deposit_continuation` used to retire it only after
        // the flag was up (and after `arrive_and_wait_auto` had waited out a
        // pause), racing a collector that reads the tail through the
        // published `tlab_addr` and resets / regrows the young arena under it,
        // then writing a filler (or handing a sink the stale tail) into memory
        // the pause had already recycled. Its retire is now an idempotent
        // no-op backstop.
        thread.tlab.retire();
        NativeContextImpl {
            shared,
            thread: &mut thread,
        }
        .deposit_root_snapshot();
        {
            let blocked = shared.mem.gc_barrier.enter_blocked();
            if blocked.pre_stw {
                let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
            }
        }
        thread.detached_thread_locals =
            cratonvm_native_builtins::phases_early::swap_current_thread_locals(
                carrier_thread_locals,
            );
        thread.park_state = own_park;
        thread.interrupted = own_interrupted;
        thread.jdk_continuation.set_carrier(0);
        drop(crash_frames);
        shared
            .threads
            .virtual_thread_manager
            .deposit_continuation(tid.0, thread);
        if census_on {
            census_add(&census.unmount_ns, end_clock);
            census_add(&census.slice_ns, slice_clock);
            census.yielded.fetch_add(1, Ordering::Relaxed);
        }
        return SliceOutcome::Yielded;
    }

    // The continuation is done: tear its thread down (the VM half of the
    // bound path's death, `resume_virtual_continuation`'s tail; the Java half
    // -- the uncaught handler, `afterDone`, the termination latch -- is
    // `VirtualThread`'s own).
    // From here the thread is no longer its carrier's: its end must not
    // publish into the carrier's state (`publish_thread_status`).
    thread.jdk_continuation.set_carrier(0);
    let outcome = match result {
        Ok(_) => SliceOutcome::Done,
        Err(MethodCallFailed::ExceptionThrown(exc)) => SliceOutcome::Threw(
            NativeContextImpl {
                shared,
                thread: &mut thread,
            }
            .add_global_root(exc),
        ),
        // gci i21/rv (finding 4): torn down below like `Done`, so ended.
        Err(e) => SliceOutcome::FailedEnded(format!("{e:?}")),
    };
    cratonvm_native_builtins::phases_early::release_current_thread_locals(&mut NativeContextImpl {
        shared,
        thread: &mut thread,
    });
    let _released =
        cratonvm_native_builtins::phases_early::swap_current_thread_locals(carrier_thread_locals);
    crate::runtime::interpreter::release_debugger_thread_state(shared, &mut thread);
    cratonvm_native_builtins::jfr_vm_events::thread_buffer_released(&mut NativeContextImpl {
        shared,
        thread: &mut thread,
    });
    // The legacy JFR `ThreadEnd`. Interpreter round i4 wave 94, lane L6: not
    // even the recorder lock (shared by every runner and carrier) when JFR is
    // off -- `emit_thread_end_event_arc` returns at once then anyway.
    if cratonvm_jfr::is_enabled() {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let mut recorder = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_thread_end_event_arc(
            &mut recorder,
            Arc::from(thread.name.as_str()),
            tid.0,
            now_ns,
        );
    }
    shared.mem.heap.flush_thread_satb();
    thread
        .gc_block_state
        .java_state
        .store(super::vm_exec::JAVA_STATE_TERMINATING, Ordering::Release);
    thread.tlab.retire();
    NativeContextImpl {
        shared,
        thread: &mut thread,
    }
    .deposit_root_snapshot();
    let registry_clock = census_clock(census_on);
    // `CRATONVM_VTHREAD_LEAN_REGISTRY` (interpreter round i4 wave 94, lane
    // L6): terminated entries are purged in batches, not one per death.
    let purge_slack = if shared.config.vthread_lean_registry {
        DEAD_ENTRY_PURGE_SLACK
    } else {
        0
    };
    let blocked = shared.mem.gc_barrier.enter_blocked();
    if blocked.pre_stw {
        let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
    }
    // `CRATONVM_VTHREAD_ONE_PUBLISH` (interpreter round i4 wave 95, lane
    // L6): inside the barrier's transition lock -- which every blocked-region
    // enter and leave in the VM takes -- only the O(1) half of the death (one
    // write: addresses, OS tid, `alive`), not three registry trips and a walk
    // (and, past the cap, a collect-and-sort) of every entry; the purge after
    // the lock, by the kept dead count, in batches of
    // `DEAD_ENTRY_PURGE_SLACK`.
    let mut retired = None;
    let mut unstructured_enters = None;
    if shared.config.vthread_one_publish {
        blocked.finish_after(|| {
            retired = registry.retire_continuation_thread(tid);
        });
        census_add(&census.teardown_registry_ns, registry_clock);
        let purge_clock = census_clock(census_on);
        if let Some(retired) = &retired {
            unstructured_enters = Some(retired.unstructured_monitor_enters);
            let purged =
                registry.finish_retired_continuation_thread(tid, retired, DEAD_ENTRY_PURGE_SLACK);
            if census_on && purged {
                census.teardown_purges.fetch_add(1, Ordering::Relaxed);
            }
        }
        census_add(&census.teardown_purge_ns, purge_clock);
    } else {
        blocked.finish_after(|| {
            registry.clear_tlab_addr(tid);
            registry.set_jvm_thread_addr(tid, 0);
            registry.mark_dead_with_purge_slack(tid, purge_slack);
        });
        census_add(&census.teardown_registry_ns, registry_clock);
    }
    let sweep_clock = census_clock(census_on);
    // `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT` (interpreter round i4 wave 95,
    // lane L6; page `i94-L6-thread-death-sweeps-every-inflated-monitor`): as
    // HotSpot's `JavaThread::exit`, the walk of every inflated monitor only
    // for a thread still holding a monitor it entered outside structured
    // locking (JNI `MonitorEnter`, a VM native's `monitor_enter`); bytecode
    // and compiled locking are balanced by the time `Continuation.enter`
    // returns. The thin-lock slot goes back either way.
    //
    // Wave 98 (lane L6): only for a continuation that ENDED -- `enter`
    // returned or threw, so every frame was popped by a return or by the
    // exception unwind, each of which releases its frame's monitors. A slice
    // that failed inside the VM (`SliceOutcome::FailedEnded`, an internal error
    // propagated past the frames' handlers) may leave a `synchronized`
    // region's monitor owned by this dying id, the case the sweep exists
    // for; it keeps the full walk, as before the switch.
    let ended_structurally = matches!(outcome, SliceOutcome::Done | SliceOutcome::Threw(_));
    let skip_sweep = shared.config.vthread_death_sweep_by_count
        && ended_structurally
        && unstructured_enters
            .unwrap_or_else(|| registry.unstructured_monitor_enters_of(tid))
            == 0;
    if skip_sweep {
        shared.threads.monitors.release_lock_slot_of_dead(tid);
    } else {
        shared
            .threads
            .monitors
            .release_monitors_held_by_except(tid, None);
    }
    census_add(&census.teardown_monitor_sweep_ns, sweep_clock);
    if census_on && skip_sweep {
        census.death_sweeps_skipped.fetch_add(1, Ordering::Relaxed);
        // The check walk, outside the timed sweep: anything it releases is a
        // monitor the count missed (`death_sweep_skip_misses` must stay 0).
        if shared
            .threads
            .monitors
            .release_monitors_held_by_counting(tid, None)
            != 0
        {
            census
                .death_sweep_skip_misses
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    // gci i11/vt item 4: the manager's record through `end_manager_record`.
    end_manager_record(shared, tid);
    thread.park_state = own_park;
    thread.interrupted = own_interrupted;
    thread.jdk_continuation.set_carrier(0);
    drop(crash_frames);
    // `CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES` (interpreter round i4 wave 94,
    // lane L6): the dead thread's site caches seed the next continuation
    // thread. Nothing reaches this thread any more: its registry entry is
    // dead, its `JvmThread` address withdrawn, and its manager entry
    // terminated.
    if shared.config.vthread_recycle_exec_caches {
        runners.recycle_exec_caches(thread, census_on);
    } else {
        drop(thread);
    }
    if census_on {
        census_add(&census.teardown_ns, end_clock);
        census_add(&census.slice_ns, slice_clock);
        census.done.fetch_add(1, Ordering::Relaxed);
    }
    outcome
}

/// `CRATONVM_VTHREAD_LEAN_REGISTRY`: how many terminated registry entries a
/// continuation's death lets accumulate past the registry's retained tail
/// before one purge removes them all
/// (`ThreadRegistry::mark_dead_with_purge_slack`).
const DEAD_ENTRY_PURGE_SLACK: usize = 64;

/// May a dying PLATFORM thread skip the walk of every inflated monitor
/// (`CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`; interpreter round i4 wave 99,
/// lane L6; page `i94-L6-thread-death-sweeps-every-inflated-monitor`)? The
/// rule of a continuation's death in [`run_slice`], for `thread`:
///
/// * it ended structurally (`ended_structurally`: its Java returned or threw,
///   never a VM error propagated past its frames -- the caller's to say), and
///   no JNI up-call on it failed with a VM error
///   (`JvmThread::vm_error_upcall_seen`);
/// * it holds no monitor it entered outside structured locking: JNI
///   `MonitorEnter`s (`ThreadEntry::unstructured_monitor_enters`) and VM
///   natives' `NativeContext::monitor_enter`s (`JvmThread::native_monitor_enters`),
///   each net of its successful exits.
///
/// Read while the registry entry is alive (before `mark_dead`); an unknown id
/// answers "sweep".
pub(crate) fn platform_death_may_skip_sweep(
    shared: &SharedVm,
    thread: &JvmThread,
    ended_structurally: bool,
) -> bool {
    shared.config.vthread_death_sweep_by_count
        && ended_structurally
        && !thread.vm_error_upcall_seen
        && thread.native_monitor_enters == 0
        && shared
            .threads
            .thread_registry
            .unstructured_monitor_enters_of(thread.thread_id)
            == 0
}

/// A dying platform thread's monitor release (wave 99, lane L6): the walk of
/// every inflated monitor it owns (`except` aside: the `Thread` mirror's
/// monitor its `join` notify still needs), or, when
/// [`platform_death_may_skip_sweep`] said so (`skip`), only its thin-lock slot
/// (`MonitorTable::release_lock_slot_of_dead`; the walk gives it back
/// unconditionally too). With `CRATONVM_DBG_VTHREAD_CENSUS`, a skipped death
/// still runs the walk afterwards and counts a release it makes as a miss
/// (`platform_death_sweep_skip_misses` and the shared
/// `death_sweep_skip_misses`, which must stay 0), as a continuation's does.
pub(crate) fn platform_death_releases_monitors(
    shared: &SharedVm,
    tid: ThreadId,
    except: Option<&Arc<crate::threading::monitor::Monitor>>,
    skip: bool,
) {
    let monitors = &shared.threads.monitors;
    if !skip {
        monitors.release_monitors_held_by_except(tid, except);
        return;
    }
    monitors.release_lock_slot_of_dead(tid);
    if shared.config.dbg_vthread_census {
        let census = &shared
            .threads
            .virtual_thread_manager
            .continuation_runners()
            .census;
        census
            .platform_death_sweeps_skipped
            .fetch_add(1, Ordering::Relaxed);
        if monitors.release_monitors_held_by_counting(tid, except) != 0 {
            census
                .platform_death_sweep_skip_misses
                .fetch_add(1, Ordering::Relaxed);
            census
                .death_sweep_skip_misses
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The name of virtual thread `vthread` (its `Thread.name`, `""` unnamed).
fn virtual_thread_name(shared: &SharedVm, vthread: ObjectRef) -> String {
    let heap = &shared.mem.heap;
    super::vm_exec::thread_field_slot(shared, heap.class_id_of(vthread), "name")
        .and_then(|slot| match heap.get_field(vthread, slot) {
            Value::Object(Some(s)) => super::vm_object::read_java_string(heap, s),
            _ => None,
        })
        .unwrap_or_default()
}

/// Register the continuation thread of virtual thread `vthread`: a registry
/// entry (as the bound path registers one, `vm_exec::thread_start`'s virtual
/// arm) and its execution state, not yet run. Posts no event: the thread
/// starts at its first mount ([`post_thread_started`]).
fn register_continuation_thread(shared: &SharedVm, vthread: ObjectRef) -> Box<JvmThread> {
    if shared.config.vthread_one_publish {
        return register_continuation_thread_once(shared, vthread);
    }
    let registry = &shared.threads.thread_registry;
    let tid = registry.next_thread_id();
    let name = virtual_thread_name(shared, vthread);
    registry.register_starting_with_daemon(tid, &name, Some(vthread), true);
    if let Some(java_tid) = super::vm_exec::read_java_thread_tid(shared, vthread) {
        registry.set_java_thread_obj_with_tid(tid, vthread, java_tid);
    }
    let (park_state, interrupted) = registry.starting_thread_states(tid);
    registry.set_park_state(tid, park_state.clone());
    registry.set_interrupted_flag(tid, interrupted.clone());
    let mut runtime = Box::new(JvmThread::new(tid, &name));
    runtime
        .invoke_cache
        .bind_redefinition_log(shared.classes.redefinition_log.clone());
    // `CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES` (interpreter round i4 wave 94,
    // lane L6): a dead continuation thread's site caches, when the pool has
    // one. Its invoke cache is bound to this VM's redefinition log already.
    if shared.config.vthread_recycle_exec_caches {
        let runners = shared.threads.virtual_thread_manager.continuation_runners();
        let recycled = runners.take_exec_caches();
        if shared.config.dbg_vthread_census {
            let count = if recycled.is_some() {
                &runners.census.caches_recycled
            } else {
                &runners.census.caches_fresh
            };
            count.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(caches) = recycled {
            caches.install(&mut runtime);
        }
    }
    runtime.kind = crate::threading::ThreadKind::Virtual;
    runtime.jdk_continuation.set_jdk();
    runtime.park_state = park_state;
    runtime.interrupted = interrupted;
    runtime.java_thread_obj = registry.java_thread_obj(tid).or(Some(vthread));
    registry.set_root_snapshot(tid, runtime.root_snapshot.clone());
    registry.set_frame_trace(tid, runtime.frame_trace.clone());
    registry.set_vm_state(tid, runtime.vm_state.clone());
    registry.set_gc_block_state(tid, runtime.gc_block_state.clone());
    registry.set_tlab_addr(tid, &runtime.tlab as *const cratonvm_gc::Tlab as usize);
    registry.set_jvm_thread_addr(tid, (&*runtime as *const JvmThread) as usize);
    runtime
}

/// [`register_continuation_thread`] under `CRATONVM_VTHREAD_ONE_PUBLISH`
/// (interpreter round i4 wave 95, lane L6): the `JvmThread` first, then its
/// registry entry COMPLETE in one `threads` write
/// (`ThreadRegistry::register_prepared_continuation_thread`) -- the same end
/// state as the thirteen `threads` trips above (nine of them write locks)
/// and four index-mutex trips. The
/// entry's `Arc`s are the `JvmThread`'s own (the trips above built fresh ones
/// and then replaced every one of them with the `JvmThread`'s); nothing could
/// have reached a fresh id's entry before it existed, so there is no
/// start-window `unpark` / interrupt to carry over (`starting_thread_states`).
fn register_continuation_thread_once(shared: &SharedVm, vthread: ObjectRef) -> Box<JvmThread> {
    let registry = &shared.threads.thread_registry;
    let tid = registry.next_thread_id();
    let name = virtual_thread_name(shared, vthread);
    let mut runtime = Box::new(JvmThread::new(tid, &name));
    runtime
        .invoke_cache
        .bind_redefinition_log(shared.classes.redefinition_log.clone());
    if shared.config.vthread_recycle_exec_caches {
        let runners = shared.threads.virtual_thread_manager.continuation_runners();
        let recycled = runners.take_exec_caches();
        if shared.config.dbg_vthread_census {
            let count = if recycled.is_some() {
                &runners.census.caches_recycled
            } else {
                &runners.census.caches_fresh
            };
            count.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(caches) = recycled {
            caches.install(&mut runtime);
        }
    }
    runtime.kind = crate::threading::ThreadKind::Virtual;
    runtime.jdk_continuation.set_jdk();
    runtime.java_thread_obj = Some(vthread);
    let java_tid = super::vm_exec::read_java_thread_tid(shared, vthread).unwrap_or(0);
    let arcs = crate::threading::jvm_thread::ThreadSharedArcs {
        park_state: runtime.park_state.clone(),
        interrupted: runtime.interrupted.clone(),
        root_snapshot: runtime.root_snapshot.clone(),
        frame_trace: runtime.frame_trace.clone(),
        vm_state: runtime.vm_state.clone(),
        gc_block_state: runtime.gc_block_state.clone(),
    };
    let tlab_addr = &runtime.tlab as *const cratonvm_gc::Tlab as usize;
    let jvm_thread_addr = (&*runtime as *const JvmThread) as usize;
    registry.register_prepared_continuation_thread(
        tid,
        &name,
        vthread,
        java_tid,
        arcs,
        tlab_addr,
        jvm_thread_addr,
    );
    runtime
}

/// The thread's start, at its first mount: the legacy JFR `ThreadStart` and
/// JDWP's `ThreadStart` (a virtual one), as the bound path posts them at
/// `Thread.start`.
fn post_thread_started(shared: &SharedVm, tid: ThreadId, name: &str) {
    // Interpreter round i4 wave 94, lane L6: no recorder lock (shared by every
    // carrier) while JFR is off, where `emit_thread_start_event_arc` returns
    // at once anyway.
    if cratonvm_jfr::is_enabled() {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let mut jfr = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_thread_start_event_arc(
            &mut jfr,
            Arc::from(name),
            "virtual",
            tid.0,
            now_ns,
        );
    }
    #[cfg(feature = "experimental-debug")]
    crate::debug::send_thread_event_of(
        shared,
        crate::debug::events::EventKind::ThreadStart,
        tid.0 as u64,
        true,
    );
}

/// The registry id of JDK virtual thread `vthread`'s continuation thread,
/// registering it (not yet run) when it has none: a STARTED virtual thread
/// that no carrier has mounted yet is still a thread a debugger names (JDWP
/// tags it a `ThreadReference`, as HotSpot tags every `Thread`). `None` for a
/// virtual thread not started or terminated, or any other object.
pub(crate) fn ensure_continuation_thread(
    shared: &SharedVm,
    vthread: ObjectRef,
) -> Option<ThreadId> {
    let vtm = &shared.threads.virtual_thread_manager;
    if !vtm.jdk_continuations() {
        return None;
    }
    let heap = &shared.mem.heap;
    let class_id = heap.class_id_of(vthread);
    let is_vt = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .is_some_and(|c| &*c.name == "java/lang/VirtualThread");
    if !is_vt {
        return None;
    }
    if let Some(tid) = super::vm_exec::resolve_thread_id_from_thread_obj(shared, vthread) {
        return Some(tid);
    }
    // `state` is `VirtualThread`'s own field (`thread_field_slot` answers
    // `Thread`'s).
    let state = {
        let cm = shared.classes.class_manager.read();
        super::vm_exec::resolve_field_index_in_hierarchy(class_id, "state", &cm.class_store)
    }
    .map(|slot| heap.get_field(vthread, slot));
    // NEW (0) or TERMINATED (99): no thread to name.
    if !matches!(state, Some(Value::Int(s)) if s != 0 && s != 99) {
        return None;
    }
    let runtime = register_continuation_thread(shared, vthread);
    let tid = runtime.thread_id;
    let name = runtime.name.clone();
    vtm.register_unstarted_jdk_continuation(tid.0, &name, runtime);
    Some(tid)
}

/// Make `cont`'s `tail` read as frozen (`non_empty`) or thawed, allocating
/// the `StackChunk` at the first yield. `cont` is the current address. The
/// chunk's `size` records the registry id of the continuation's thread
/// (`continuation_thread`): a walk of an unmounted continuation
/// (`Continuation.getStackTrace`) reads its frames there.
fn mark_tail(
    ctx: &mut NativeContextImpl<'_>,
    cont: ObjectRef,
    non_empty: bool,
    thread: ThreadId,
) -> Result<(), MethodCallFailed> {
    let tail = match ctx.get_field_by_name(cont, "tail") {
        Value::Object(Some(chunk)) => chunk,
        _ => {
            if !non_empty {
                return Ok(());
            }
            let root = ctx.add_global_root(cont);
            let chunk = ctx.new_object_initialized(STACK_CHUNK, "()V", &[]);
            let cont = ctx.resolve_global_root(root).unwrap_or(cont);
            ctx.remove_global_root(root);
            let chunk = match chunk? {
                Some(Value::Object(Some(chunk))) => chunk,
                _ => {
                    return Err(MethodCallFailed::InternalError(VmError::Internal {
                        message: "Continuation: no StackChunk".to_string(),
                    }))
                }
            };
            ctx.set_field_by_name(cont, "tail", Value::Object(Some(chunk)));
            chunk
        }
    };
    // `isEmpty()` is `sp == bottom`.
    ctx.set_field_by_name(
        tail,
        "size",
        Value::Int(i32::try_from(thread.0).unwrap_or(0)),
    );
    ctx.set_field_by_name(tail, "bottom", Value::Int(0));
    ctx.set_field_by_name(tail, "sp", Value::Int(i32::from(non_empty)));
    Ok(())
}

/// The per-OS-thread state a slice run on its CARRIER's OS thread must not
/// share with the carrier, switched for the slice and restored on drop
/// (interpreter round i6 wave 120, lane L6, `CRATONVM_VTHREAD_SLICE_ON_CARRIER`;
/// stage 1 of proposal
/// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`).
/// Entered after the carrier's VM wait region is raised and dropped before it
/// is lowered, so the carrier is GC-blocked throughout; a yield unwinds to
/// `run_slice`, which returns normally, so the drop runs on every outcome.
///
/// * The registry's OS-tid claim: the carrier's published OS tid is withdrawn
///   for the slice (`clear_os_tid`; a `0` tid matches no claimant query), so
///   this OS thread is claimed by the continuation alone -- the take-over
///   roster (`alive_count_and_os_tids`) does not list it twice, and
///   `thread_id_for_current_os_tid` (and through it `jni::current_jvm_thread_of`'s
///   fallback) answers the continuation. The drop republishes the carrier's
///   claim with a fresh ordinal, so the carrier is again the newest claimant.
/// * `JNI_THREAD`: null for the slice (the continuation's JNI natives install
///   their own), the carrier's back on drop, with the carrier's JNI
///   local-frame table republished as this OS thread's (`publish_local_frames_owner`;
///   the slice's natives published the continuation's).
/// * The JIT thread pointer and its mirror: null for the slice (its first JIT
///   entry installs the continuation; the carrier has no compiled frame, the
///   admission's condition), the carrier's value back on drop.
/// * The FFM verdicts and segment-door memo answered for the continuation
///   during the slice: forgotten again on drop, as `run_slice` forgets the
///   previous occupant's at its start.
///
/// Not switched, and why: the `ThreadLocal` map, the crash frame trace and
/// the carrier's `Parker` / interrupt status are `run_slice`'s own (swapped in
/// and back on every outcome); the shadow-stack window is republished by the
/// carrier's next JIT entry and withdrawn when the continuation's shadow stack
/// drops (`ShadowStack`'s `Drop`); the JIT entry chain, its depth slot and the
/// deopt stash are empty on both sides (no carrier compiled frame at entry,
/// and a slice cannot yield with a compiled frame); the stack band above
/// `enterSpecial` is the carrier's frozen one, untouched by the slice, which
/// runs below it.
///
/// Wave 130 (lane L6; the wave-121 battery's five DIFFs, three of them hangs):
/// the per-OS-thread "what is on this native stack" counts that the yield
/// decisions read are set aside too, so the slice starts from a runner's
/// zeros ([`CarrierNativeBase`]). Without that, the carrier's own
/// `enterSpecial` native counted as a native frame under the slice, and every
/// park, `Object.wait` and contended `monitorenter` of the slice pinned. The
/// thread-state census cell is re-bound to the carrier on drop.
struct CarrierSliceScope<'a> {
    shared: &'a SharedVm,
    carrier: ThreadId,
    carrier_state: Arc<crate::threading::jvm_thread::GcBlockState>,
    prev_jni: *mut JvmThread,
    prev_jit: *mut JvmThread,
    native_base: Option<CarrierNativeBase>,
}

/// The carrier's per-OS-thread native-frame counts, held aside for a slice run
/// on its OS thread (interpreter round i6 wave 130, lane L6). Each is read by
/// the yield decisions (`pinned_reason`, `vthread_wait_unmount_allowed`,
/// `vt_pinned_by_native`, the monitor unmount's
/// `continuation_has_native_frame`) or by the allocators, and each is `0` at
/// the start of a runner's slice:
///
/// * `native_callee_memo`'s native depth (the carrier's `enterSpecial` is 1)
///   and its two dispatch witnesses (`native_callee_memo::SliceNativeBase`);
/// * `continuation_pin`'s pins and transparent-native counts;
/// * the `<clinit>` nesting depth (`vm_util::in_clinit_shared`);
/// * the native-OOM unwind permission (`native_oom::unwind_ok`): raised by
///   `enterSpecial`'s funnel, it let an allocation in the slice's own Rust
///   unwind with the OOM payload past the interpreter to the carrier arm's
///   `catch_unwind`, which aborts (i121-L9); on a runner that allocation takes
///   its non-unwinding arm.
struct CarrierNativeBase {
    memo: crate::runtime::native_callee_memo::SliceNativeBase,
    pins: (u32, u32),
    clinit_depth: u32,
    unwind_ok: u32,
}

impl CarrierNativeBase {
    fn enter() -> Self {
        Self {
            memo: crate::runtime::native_callee_memo::SliceNativeBase::enter(),
            pins: cratonvm_native_api::continuation_pin::replace_counts(0, 0),
            clinit_depth: super::vm_util::replace_clinit_depth(0),
            unwind_ok: crate::runtime::native_oom::suspend_for_jit(),
        }
    }

    fn restore(self) {
        crate::runtime::native_oom::restore(self.unwind_ok);
        let _ = super::vm_util::replace_clinit_depth(self.clinit_depth);
        let _ = cratonvm_native_api::continuation_pin::replace_counts(self.pins.0, self.pins.1);
        self.memo.restore();
    }
}

impl<'a> CarrierSliceScope<'a> {
    fn enter(
        shared: &'a SharedVm,
        carrier: ThreadId,
        carrier_state: Arc<crate::threading::jvm_thread::GcBlockState>,
    ) -> Self {
        shared.threads.thread_registry.clear_os_tid(carrier);
        // SAFETY: null is the "no thread installed" value every reader of
        // `JNI_THREAD` handles; the previous pointer is put back on drop.
        let prev_jni = unsafe { crate::native::jni::replace_jni_thread(std::ptr::null_mut()) };
        let prev_jit = crate::jit::helpers::replace_jit_thread_ptr(std::ptr::null_mut());
        Self {
            shared,
            carrier,
            carrier_state,
            prev_jni,
            prev_jit,
            native_base: Some(CarrierNativeBase::enter()),
        }
    }
}

impl Drop for CarrierSliceScope<'_> {
    fn drop(&mut self) {
        if let Some(base) = self.native_base.take() {
            base.restore();
        }
        // Wave 130 (lane L6): the slice bound this OS thread's thread-state
        // census cell to the continuation (`mark_stw_ready`, the barrier's
        // arrivals); the carrier's own transitions follow from here.
        crate::threading::thread_state::bind_current_thread(self.carrier.0);
        let _ = crate::jit::helpers::replace_jit_thread_ptr(self.prev_jit);
        crate::native::jni::publish_local_frames_owner(&self.carrier_state);
        // SAFETY: `prev_jni` is what this OS thread had installed when the
        // scope was entered: the carrier's own thread (blocked, live) or null.
        unsafe { crate::native::jni::restore_jni_thread(self.prev_jni) };
        cratonvm_native_builtins::ffm_fast::forget_thread_verdicts();
        crate::jit::helpers::forget_real_seg_entries();
        let registry = &self.shared.threads.thread_registry;
        if cratonvm_gc::blocked_access_debug::enabled() {
            // `CRATONVM_DBG_BLOCKED_ACCESS`: also re-register the carrier's
            // blocked flag as this OS thread's canary (the slice registered
            // the continuation's); debug only, one leaked `Arc` per slice.
            registry.set_os_tid_current(self.carrier);
        } else {
            registry.republish_os_tid_current(self.carrier);
        }
    }
}

/// `Continuation.enterSpecial(c, isContinue, isVirtualThread)` on carrier
/// `ctx.thread` (see the module docs).
pub(crate) fn enter_special(
    ctx: &mut NativeContextImpl<'_>,
    cont: ObjectRef,
    is_continue: bool,
    is_virtual_thread: bool,
) -> Result<(), MethodCallFailed> {
    // gci i25/vt: this carrier's previous `runContinuation` has returned, so
    // its `afterYield()` has run or died; an orphan it left is re-queued.
    resolve_post_yield_window(ctx);
    let shared = ctx.shared;
    let Some(shared_arc) = shared.try_get_arc() else {
        return Err(internal(
            "Continuation.enterSpecial: this VM cannot run continuations",
        ));
    };
    if !is_virtual_thread {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            crate::error::RuntimeError::UnsupportedOperationException {
                message:
                    "Continuation.enterSpecial: only a virtual thread's continuation runs here"
                        .to_string(),
            },
        )));
    }
    // `VirtualThread.mount()` re-pointed the carrier's current thread at the
    // virtual thread before `Continuation.run`.
    let Some(vthread) = ctx.thread.java_thread_obj else {
        return Err(internal(
            "Continuation.enterSpecial: no virtual thread is mounted",
        ));
    };
    let carrier = ctx.thread.thread_id;
    let cont_root = ctx.add_global_root(cont);
    let census_on = shared.config.dbg_vthread_census;
    let census = &shared
        .threads
        .virtual_thread_manager
        .continuation_runners()
        .census;
    let register_clock = census_clock(census_on && !is_continue);
    // gci i20/st: set when a `false` call finds a yielded state (below).
    let mut resume_untailed = false;
    let thread = if is_continue {
        let vt_tid = super::vm_exec::resolve_thread_id_from_thread_obj(shared, vthread);
        let taken = vt_tid.and_then(|t| {
            shared
                .threads
                .virtual_thread_manager
                .take_runtime_for_mount(t.0, carrier.0)
        });
        match taken {
            Some((thread, _)) => thread,
            None => {
                ctx.remove_global_root(cont_root);
                // gci i21/rv (finding 3): a Java throwable, so
                // `Continuation.run`'s `finally` unmounts the carrier (a raw
                // internal error skipped it: i20-st report section 6 item 4).
                return Err(slice_failure_as_java(
                    ctx,
                    "Continuation.enterSpecial: the continuation has no frames",
                ));
            }
        }
    } else {
        // A debugger may have named the thread already
        // (`ensure_continuation_thread`): its state waits in the manager.
        let manager = &shared.threads.virtual_thread_manager;
        // `CRATONVM_VTHREAD_LEAN_REGISTRY` (interpreter round i4 wave 94,
        // lane L6): a continuation thread registered ahead of its first mount
        // (`ensure_continuation_thread` -> `register_continuation_thread`)
        // is in the `Thread.tid` index whenever the tid reads non-zero, so a
        // miss there is final; the full lookup's fallback is a pointer walk
        // of every registry entry under the `threads` WRITE lock, paid by
        // every new virtual thread.
        // Interpreter round i4 wave 95, lane L6: `CRATONVM_VTHREAD_ONE_PUBLISH`
        // takes the same index-only lookup (an early registration is
        // `register_prepared_continuation_thread`'s, which indexes the tid).
        let lean_tid = if shared.config.vthread_lean_registry || shared.config.vthread_one_publish
        {
            super::vm_exec::read_java_thread_tid(shared, vthread).filter(|&t| t != 0)
        } else {
            None
        };
        let lookup_clock = census_clock(census_on);
        let found = match lean_tid {
            Some(java_tid) => shared
                .threads
                .thread_registry
                .find_thread_id_by_java_tid(java_tid),
            None => super::vm_exec::resolve_thread_id_from_thread_obj(shared, vthread),
        };
        let registered = found
            .and_then(|t| manager.take_runtime_for_mount(t.0, carrier.0))
            .map(|(thread, _)| thread);
        census_add(&census.register_lookup_ns, lookup_clock);
        let thread = match registered {
            Some(thread) => thread,
            None => {
                let publish_clock = census_clock(census_on);
                let thread = register_continuation_thread(shared, vthread);
                census_add(&census.register_publish_ns, publish_clock);
                let manager_clock = census_clock(census_on);
                manager.create_jdk_continuation(thread.thread_id.0, &thread.name, carrier.0);
                census_add(&census.register_manager_ns, manager_clock);
                thread
            }
        };
        // gci i20/st: a "first" run whose deposited state already has frames
        // is a continuation that yielded while its `tail` stayed null -- the
        // `StackChunk` allocation of `mark_tail` after its first yield failed
        // (an `OutOfMemoryError` under heap pressure; see the `Yielded` arm
        // below), so `Continuation.run` read `isStarted()` false and called
        // `enterSpecial(c, false, ..)` again. Resume it, as the `true` call
        // would: running it as a first slice re-entered `Continuation.enter`
        // on top of the parked frames with a virgin `GcBlockState` store that
        // dropped the deposited blocked-window fixup unapplied (stale frame
        // words, the body run twice). A debugger-registered thread that never
        // ran (`ensure_continuation_thread`) has no frames and stays a first
        // slice; its start events are posted once, here.
        if thread.frames.is_empty() {
            let events_clock = census_clock(census_on);
            post_thread_started(shared, thread.thread_id, &thread.name);
            census_add(&census.register_manager_ns, events_clock);
        } else {
            resume_untailed = true;
        }
        thread
    };
    if census_on {
        census_add(&census.register_ns, register_clock);
        let count = if is_continue {
            &census.resumed_slices
        } else {
            &census.first_slices
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
    let vt_tid = thread.thread_id;
    // Thawed: the frames run again (`isEmpty()` while mounted).
    if is_continue {
        let cont_now = ctx.resolve_global_root(cont_root).unwrap_or(cont);
        mark_tail(ctx, cont_now, false, thread.thread_id)?;
    }
    let runners = shared.threads.virtual_thread_manager.continuation_runners();
    runners.hosted.lock().insert(carrier.0, vt_tid.0);
    let mut job = Job {
        thread,
        first: !is_continue && !resume_untailed,
        cont_root,
        carrier,
        carrier_park: ctx.thread.park_state.clone(),
        carrier_interrupted: ctx.thread.interrupted.clone(),
        posted: None,
        refuse_yield: false,
    };
    // Interpreter round i6 wave 120, lane L6 (`CRATONVM_VTHREAD_SLICE_ON_CARRIER`,
    // default off; stage 1 of proposal
    // `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`):
    // run the slice HERE, on the carrier's OS thread, when nothing of the
    // carrier's on this OS thread would be shared with the slice: no compiled
    // frame (the JIT entry chain is per OS thread, and a non-empty chain pins
    // every yield of the slice and teaches the deny set the carrier's
    // methods) and no open JNI local frame (`JNI_LOCAL_FRAMES` is per OS
    // thread: the slice's deposits would root, and its wakes remap, the
    // carrier's local references a second time). Otherwise, or with the
    // switch off, the runner (`run_on_runner`) as before.
    let carrier_state = (shared.config.vthread_slice_on_carrier
        && crate::jit::conservative_roots::current_thread_jit_depth() == 0
        && crate::native::jni::local_frame_depth() == 0)
        .then(|| Arc::clone(&ctx.thread.gc_block_state));
    if census_on && shared.config.vthread_slice_on_carrier {
        let row = if carrier_state.is_some() {
            &census.slices_on_carrier
        } else {
            &census.slices_on_carrier_refused
        };
        row.fetch_add(1, Ordering::Relaxed);
    }
    let outcome = loop {
        ctx.begin_vm_wait_region();
        let wait_clock = census_clock(census_on);
        // The carrier stays GC-blocked (its VM wait region, raised just
        // above) for the whole slice: its frames do not run, and its deposit
        // stands for it in every pause, as while it waits for a runner.
        let outcome = match &carrier_state {
            Some(state) => {
                let _scope = CarrierSliceScope::enter(shared, carrier, Arc::clone(state));
                // Wave 121 (lane L9, page
                // `i121-L9-a-panicking-carrier-slice-resumes-the-carrier-gc-blocked`):
                // a panic out of the slice must not reach `enterSpecial`'s
                // native funnel, whose `catch_unwind` turns it into an
                // `InternalError` and lets the carrier run Java with its VM
                // wait region still raised (the `end_blocking_region` below is
                // skipped) and the dropped continuation's `JvmThread` / TLAB
                // addresses still published as alive. On a runner the same
                // panic ends the runner thread and the carrier waits on, still
                // GC-safe. Neither state is recoverable here: stop the VM.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_slice(&shared_arc, job, true)
                })) {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        eprintln!(
                            "cratonvm: a virtual-thread slice run on its carrier \
                             (CRATONVM_VTHREAD_SLICE_ON_CARRIER) panicked; aborting"
                        );
                        std::process::abort()
                    }
                }
            }
            None => run_on_runner(&shared_arc, job),
        };
        census_add(&census.carrier_wait_ns, wait_clock);
        ctx.end_blocking_region();
        // gci i24/vl: a timed park's yield is kept only when this carrier
        // can still allocate what its `afterYield()` needs; otherwise the
        // state is taken back and the same slice runs on with `doYield`
        // answering a pin (HotSpot's freeze that failed with
        // `OutOfMemoryError`), so the thread parks on this carrier instead of
        // being lost (`refuse_yield_without_headroom`).
        match refuse_yield_without_headroom(ctx, &outcome, vt_tid, carrier) {
            Some(thread) => {
                if census_on {
                    census
                        .yields_refused_no_headroom
                        .fetch_add(1, Ordering::Relaxed);
                }
                job = Job {
                    thread,
                    first: false,
                    cont_root,
                    carrier,
                    carrier_park: ctx.thread.park_state.clone(),
                    carrier_interrupted: ctx.thread.interrupted.clone(),
                    posted: None,
                    refuse_yield: true,
                };
            }
            None => break outcome,
        }
    };
    // gci i25/vt: a yield that stands and leaves the virtual thread
    // TIMED_PARKING / TIMED_WAITING opens this carrier's post-yield window
    // (`afterYield()`'s unguarded `schedule(..)` comes next, in Java). So
    // does a `Failed` slice, whose state is deposited back as a yield's
    // (rv25 item 4: a refused re-run that found no runner): `enterSpecial`
    // throws, `runContinuation`'s `finally` still runs `afterYield`.
    if matches!(outcome, SliceOutcome::Yielded | SliceOutcome::Failed(_)) {
        open_post_yield_window(ctx, vt_tid, carrier);
    }
    runners.hosted.lock().remove(&carrier.0);
    let cont = ctx.resolve_global_root(cont_root).unwrap_or(cont);
    ctx.remove_global_root(cont_root);
    match outcome {
        // gci i20/st: the state is deposited, so a failed `StackChunk`
        // allocation (the first yield's) must not reach Java as an
        // `OutOfMemoryError` thrown out of a yield that HAPPENED: the thread
        // would be scheduled again with `tail` null, and the next `false`
        // call resumes it (`resume_untailed` above), which retries the chunk.
        //
        // gci i21/rv (finding 1 of
        // `gci-i21rv-i20-default-path-review-findings-FIXED-20261017`): the
        // `StackChunk` allocation's own failure is NOT a thrown exception --
        // `NativeContextImpl::new_object_initialized` answers heap exhaustion
        // with `InternalError(VmError::Runtime(OutOfMemoryError))`, which the
        // `ExceptionThrown` arm alone let through (a Java OOME out of a yield
        // that happened, the very state this arm exists to avoid); and its
        // "no StackChunk" arm is a raw internal error, which would unwind
        // `Continuation.run`'s `finally` unrun (the carrier left virtual).
        // Every failure of the tail write after a real yield is swallowed:
        // the state is deposited either way.
        SliceOutcome::Yielded => match mark_tail(ctx, cont, true, vt_tid) {
            Err(MethodCallFailed::ExceptionThrown(_)) => Ok(()),
            Err(MethodCallFailed::InternalError(_)) => Ok(()),
            Ok(()) => Ok(()),
        },
        SliceOutcome::Done => Ok(()),
        SliceOutcome::Threw(handle) => {
            let exc = ctx.resolve_global_root(handle);
            ctx.remove_global_root(handle);
            match exc {
                Some(exc) => Err(MethodCallFailed::ExceptionThrown(exc)),
                // gci i21/rv (finding 3): as a Java throwable (see above).
                None => Err(slice_failure_as_java(
                    ctx,
                    "Continuation.enterSpecial: the throwable was not kept",
                )),
            }
        }
        SliceOutcome::Failed(message) => Err(slice_failure_as_java(ctx, &message)),
        // gci i21/rv (finding 4): the VM tore the thread down, so the Java
        // half ends too -- `done` as `Continuation.enter`'s skipped
        // `finally { c.finish(); }` would have left it (before the throwable is
        // built: that allocation may move `cont`). `Continuation.run`'s
        // `finally` then clears `tail` (`postYieldCleanup`) and
        // `VirtualThread.runContinuation` takes `afterDone()`: TERMINATED,
        // joiners released. Left `false`, `afterYield()` found the thread
        // RUNNING, matched no state and returned (`assert false` only under
        // `-esa`): the thread stayed RUNNING forever and every `join()` of it
        // hung with nothing printed but the carrier's `InternalError`.
        SliceOutcome::FailedEnded(message) => {
            ctx.set_field_by_name(cont, "done", Value::Int(1));
            Err(slice_failure_as_java(ctx, &message))
        }
    }
}

/// gci i24/vl: does a yield that left the virtual thread in `state` need the
/// carrier's post-yield probe? Only JDK 25's TIMED_PARKING (see
/// [`VTHREAD_TIMED_PARKING`]).
fn yield_needs_post_yield_probe(state: &Value) -> bool {
    matches!(state, Value::Int(VTHREAD_TIMED_PARKING))
}

/// gci i24/vl (`gci-i24vl-g1-virtual-thread-lost-when-after-yield-throws-oome-FIXED-20261017`,
/// battery FAIL `vi20-st` L): the carrier's half of a refused yield. `Some`
/// with the continuation's state, taken back from the manager, when the slice
/// just ended in a `doYield` that left the virtual thread TIMED_PARKING and
/// this carrier cannot allocate any more; `None` (the yield stands) otherwise.
///
/// Why: a yield here allocates nothing on the heap (the frames stay in the
/// boxed `JvmThread`), so on an exhausted heap the first allocation after the
/// unmount is the carrier's `afterYield()` -> `schedule(..)` of the park's
/// timeout task, which has no `OutOfMemoryError` handling: the thread stays
/// TIMED_PARKING (`Thread.getState()` RUNNABLE) with no task and no timer, and
/// no `unpark` ever moves it -- the "RUNNABLE/in-park, carriers in
/// `awaitWork`, `none-progressed`" hang of `vi20-st` L (G1 `-Xmx64m` under the
/// JIT pin retention). HotSpot's yield is a freeze that allocates the frames'
/// `StackChunk`; on an exhausted heap THAT allocation fails, before the
/// unmount, and `VirtualThread.parkNanos` catches the error and parks on the
/// carrier. The probe stands in for that allocation: a `byte[0]` through the
/// collecting ladder (`gc_alloc_array`: forced collection, last-ditch cycle,
/// and the report `-XX:+ExitOnOutOfMemoryError` / `HeapDumpOnOutOfMemoryError`
/// act on, as HotSpot reports a failed freeze). It also refills the carrier's
/// TLAB (retired when it entered the slice's wait), so `afterYield`'s two
/// small objects come from it.
///
/// Residual (on the page): a pause another thread runs between the probe and
/// `afterYield`'s allocation retires that TLAB, and if the heap is exhausted
/// again after it the allocation can still fail. Not covered either: a timed
/// `Object.wait` (`VtYield::MonitorWait`, TIMED_WAITING), whose `afterYield`
/// schedules the same way.
///
/// The probe may collect: the caller re-reads `cont` from its global root
/// afterwards, and nothing here keeps a raw reference across it.
fn refuse_yield_without_headroom(
    ctx: &mut NativeContextImpl<'_>,
    outcome: &SliceOutcome,
    vt_tid: ThreadId,
    carrier: ThreadId,
) -> Option<Box<JvmThread>> {
    if !matches!(outcome, SliceOutcome::Yielded) {
        return None;
    }
    let shared = ctx.shared;
    // The carrier's current thread is the virtual thread (`mount()`), and
    // `end_blocking_region`'s drain has forwarded it.
    let vthread = ctx.thread.java_thread_obj?;
    if !yield_needs_post_yield_probe(&ctx.get_field_by_name(vthread, "state")) {
        return None;
    }
    // gci i25/vt (rv25 item 5): the unsampled door -- no JVMTI
    // `SampledObjectAlloc` / JFR sample of an array the program never made.
    let probe = crate::runtime::interpreter::gc_alloc_array_unsampled(
        shared,
        ctx.thread,
        cratonvm_types::ClassId::new(0),
        cratonvm_gc::heap::ArrayElementType::Byte,
        POST_YIELD_PROBE_LENGTH,
    );
    if probe.is_ok() {
        return None;
    }
    // Nobody else can have taken the state: a TIMED_PARKING thread is
    // resubmitted only by its own `afterYield`, which has not run.
    let manager = &shared.threads.virtual_thread_manager;
    let (thread, _) = manager.take_runtime_for_mount(vt_tid.0, carrier.0)?;
    if !matches!(thread.vt_yield, VtYield::DoYield) {
        // Not a `doYield` (cannot happen for TIMED_PARKING): leave it.
        manager.deposit_continuation(vt_tid.0, thread);
        return None;
    }
    Some(thread)
}

/// gci i24/vl: the runner's half of a refused yield ([`Job::refuse_yield`]):
/// `doYield` returns [`YIELD_REFUSED_PIN_REASON`] into `yield0`, which stands
/// past its `invokestatic`, instead of the remount's `0`
/// (`interpreter::remount_action`'s `VtYield::DoYield` arm, which the
/// `VtYield::Park` record set here skips). `Ok` and nothing changed when not
/// refused.
fn refuse_deposited_yield(thread: &mut JvmThread, refuse: bool) -> Result<(), MethodCallFailed> {
    if !refuse {
        return Ok(());
    }
    thread.vt_yield = VtYield::Park;
    match thread.frames.last_mut() {
        Some(leaf) => leaf
            .stack
            .push(Value::Int(YIELD_REFUSED_PIN_REASON))
            .map_err(|e| MethodCallFailed::InternalError(VmError::Runtime(e))),
        None => Ok(()),
    }
}

/// gci i25/vt: does a yield that left the virtual thread in `state` open a
/// post-yield window? The two JDK 25 states whose `afterYield()` allocates
/// (`schedule(..)` of a timeout task) with no `OutOfMemoryError` handling:
/// TIMED_PARKING and TIMED_WAITING. Every other one is OOME-safe (PARKING,
/// WAITING and BLOCKING allocate nothing; YIELDING and the hand-offs submit
/// with `submitRunContinuation`'s retry). Exact values: a SUSPENDED bit
/// (`1 << 8`) is never set on a transitional state.
fn after_yield_allocates(state: &Value) -> bool {
    matches!(
        state,
        Value::Int(VTHREAD_TIMED_PARKING) | Value::Int(VTHREAD_TIMED_WAITING)
    )
}

/// gci i25/vt (`gci-i24vl-g1-virtual-thread-lost-when-after-yield-throws-oome-FIXED-20261017`
/// residuals 1 and 2): open carrier `carrier`'s post-yield window after a
/// slice of `vt_tid` that ended in a yield which stands (or failed with its
/// state deposited back, `SliceOutcome::Failed`). Only when the
/// virtual thread (still the carrier's current thread: `unmount()` has not
/// run) is TIMED_PARKING or TIMED_WAITING ([`after_yield_allocates`]) and its
/// state is deposited: the window records the manager record's mount count,
/// which any later mount of the thread changes.
///
/// Why: `afterYield()` runs on this carrier right after `enterSpecial`
/// returns, and its `schedule(..)` of the timeout task has no
/// `OutOfMemoryError` handling; an OOME there (a pause between the i24/vl
/// probe and that allocation, or a timed `Object.wait`, which has no probe)
/// leaves the thread TIMED_PARKING / TIMED_WAITING with no task, and nothing
/// in the JDK moves it again. [`resolve_post_yield_window`] finds such an
/// orphan once the carrier is provably past `afterYield`.
fn open_post_yield_window(ctx: &mut NativeContextImpl<'_>, vt_tid: ThreadId, carrier: ThreadId) {
    let Some(vthread) = ctx.thread.java_thread_obj else {
        return;
    };
    if !after_yield_allocates(&ctx.get_field_by_name(vthread, "state")) {
        return;
    }
    let shared = ctx.shared;
    let manager = &shared.threads.virtual_thread_manager;
    let Some(mounts) = manager.deposited_mount_count(vt_tid.0) else {
        return;
    };
    let runners = manager.continuation_runners();
    runners
        .post_yield_windows
        .lock()
        .insert(carrier.0, (vt_tid.0, mounts));
    ctx.thread.jdk_continuation.set_post_yield_window(true);
    if shared.config.dbg_vthread_census {
        runners
            .census
            .post_yield_windows_opened
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// gci i25/vt: resolve the post-yield window `ctx.thread` (a carrier) left
/// open ([`open_post_yield_window`]), if any. Called where the carrier is
/// provably past the `afterYield()` of that yield: at its next
/// `Continuation.enterSpecial` (its previous `runContinuation` returned) and
/// at its next Java-level park (`vm_exec`'s `park_with_state_until`, `timed`
/// doors only: nothing from `enterSpecial`'s return to `afterYield`'s state
/// write parks through `Unsafe.park` -- `unmount()` and `timedWaitLock()` are
/// monitors, `CarrierThread.endBlocking()` a CAS, and `afterYield`'s own
/// OOME-retrying submits run only after the state write).
///
/// An orphan is a thread that, under the manager's record lock, is still
/// deposited with the window's mount count (never remounted since) and still
/// TIMED_PARKING / TIMED_WAITING: `afterYield` writes TIMED_PARKED /
/// TIMED_WAIT before anything else can move it, so it died first. The
/// orphan is moved to BLOCKED (one CAS, under that lock, so no remount can
/// slip between the test and the move) and queued for the JDK's unblocker
/// ([`with_post_yield_orphans`]), whose `unblock()` submits it with
/// `submitRunContinuation`'s retry on `OutOfMemoryError`. It resumes as a
/// spurious wake-up: `parkNanos` returns early (the JDK allows it; the park
/// permit is kept), `Object.wait` returns as on a timeout
/// (`vthread_wait_remount` re-acquires the monitor). HotSpot never reaches
/// this state: its freeze allocates first and parks on the carrier.
///
/// Nothing here allocates; the carrier is a running mutator (no pause can
/// move the mirror between its read and the CAS).
pub(crate) fn resolve_post_yield_window(ctx: &mut NativeContextImpl<'_>) {
    if !ctx.thread.jdk_continuation.post_yield_window() {
        return;
    }
    ctx.thread.jdk_continuation.set_post_yield_window(false);
    let shared = ctx.shared;
    let manager = &shared.threads.virtual_thread_manager;
    let runners = manager.continuation_runners();
    let window = runners
        .post_yield_windows
        .lock()
        .remove(&ctx.thread.thread_id.0);
    let Some((vt, mounts)) = window else {
        return;
    };
    let Some(vthread) = shared.threads.thread_registry.java_thread_obj(ThreadId(vt)) else {
        return;
    };
    // The common case, without a lock: `afterYield` moved the thread on.
    if !after_yield_allocates(&ctx.get_field_by_name(vthread, "state")) {
        return;
    }
    let Some(slot) = virtual_thread_state_slot(shared, vthread) else {
        return;
    };
    let requeued = manager.with_deposit_unmounted_since(vt, mounts, || {
        [VTHREAD_TIMED_PARKING, VTHREAD_TIMED_WAITING]
            .into_iter()
            .any(|from| {
                super::vm_exec::compare_and_swap_field_shared(
                    shared,
                    vthread,
                    slot,
                    Value::Int(from),
                    Value::Int(VTHREAD_BLOCKED),
                )
            })
    });
    if requeued != Some(true) {
        return;
    }
    runners.post_yield_orphans.lock().push(vt);
    if shared.config.dbg_vthread_census {
        runners
            .census
            .post_yield_orphans_requeued
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// gci i26/rv (`gci-i26rv-i25-default-path-review-findings-FIXED-20261016` item
/// V1): the park seam of [`resolve_post_yield_window`]. A Java-level park
/// (`vm_exec`'s `park_with_state_until`, `timed`: every `Unsafe.park` door,
/// untimed `LockSupport.park` included) is NOT always past `afterYield()`'s
/// state write: `afterYield` calls `schedule(..)` BEFORE it writes
/// TIMED_PARKED / TIMED_WAIT, and `schedule` can park -- JDK 25's
/// `ForkJoinPool.startDelayScheduler` -> `lockRunState` ->
/// `spinLockRunState` -> `LockSupport.parkNanos` on the first delayed task
/// under run-state contention, and a custom scheduler's
/// `ScheduledThreadPoolExecutor` queue `ReentrantLock` on every contended
/// one. Resolving there moved a live TIMED_PARKING thread to BLOCKED and
/// queued it; `afterYield`'s plain `setState(TIMED_PARKED)` then overwrote
/// whatever the unblocker had made of it, and the timeout task could submit
/// a thread already running elsewhere (a double mount).
///
/// So while the window's thread is still TIMED_PARKING / TIMED_WAITING and
/// the carrier's own stack still holds `VirtualThread.runContinuation` /
/// `afterYield` (the yield's `finally` has not finished), the window stays
/// open; the carrier's next park outside it (`awaitWork`) or next
/// `enterSpecial` resolves it. The stack is read only on that rare arm.
pub(crate) fn resolve_post_yield_window_at_park(ctx: &mut NativeContextImpl<'_>) {
    if !ctx.thread.jdk_continuation.post_yield_window() {
        return;
    }
    let shared = ctx.shared;
    let window = shared
        .threads
        .virtual_thread_manager
        .continuation_runners()
        .post_yield_windows
        .lock()
        .get(&ctx.thread.thread_id.0)
        .copied();
    if let Some((vt, _)) = window {
        if let Some(vthread) = shared.threads.thread_registry.java_thread_obj(ThreadId(vt)) {
            if after_yield_allocates(&ctx.get_field_by_name(vthread, "state"))
                && carrier_inside_run_continuation(ctx)
            {
                return;
            }
        }
    }
    resolve_post_yield_window(ctx);
}

/// gci i26/rv: does the calling carrier's own stack still hold the
/// `runContinuation` (or its `afterYield`) whose yield opened the window?
fn carrier_inside_run_continuation(ctx: &NativeContextImpl<'_>) -> bool {
    let cm = ctx.shared.classes.class_manager.read();
    let trace = crate::runtime::stackwalker::capture_full_trace_without_lines(
        &cm.class_store,
        &ctx.thread.frames,
    );
    drop(cm);
    frames_are_inside_run_continuation(
        trace
            .iter()
            .map(|e| (&*e.class_name, &*e.method_name)),
    )
}

/// gci i26/rv: pure half of [`carrier_inside_run_continuation`], over
/// `(internal class name, method name)` pairs.
fn frames_are_inside_run_continuation<'a>(
    frames: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> bool {
    frames.into_iter().any(|(class, method)| {
        class == "java/lang/VirtualThread"
            && matches!(method, "runContinuation" | "afterYield")
    })
}

/// gci i25/vt: the slot of `VirtualThread.state` on `vthread`'s class.
fn virtual_thread_state_slot(shared: &SharedVm, vthread: ObjectRef) -> Option<usize> {
    let class_id = shared.mem.heap.class_id_of(vthread);
    let cm = shared.classes.class_manager.read();
    super::vm_exec::resolve_field_index_in_hierarchy(class_id, "state", &cm.class_store)
}

/// gci i25/vt: `ids` (the monitors' unblock list) plus every orphan
/// [`resolve_post_yield_window`] queued since the last call, each id once,
/// for `VirtualThread.takeVirtualThreadListToUnblock()` (`vm_exec`). The
/// unblocker links the whole list before it asks again, so an id is never on
/// two lists at once; a duplicate within one would corrupt `next`.
pub(crate) fn with_post_yield_orphans(shared: &SharedVm, mut ids: Vec<ThreadId>) -> Vec<ThreadId> {
    let orphans = std::mem::take(
        &mut *shared
            .threads
            .virtual_thread_manager
            .continuation_runners()
            .post_yield_orphans
            .lock(),
    );
    merge_unblock_ids(&mut ids, orphans);
    ids
}

/// gci i25/vt: append each of `orphans` to `ids` unless already there.
fn merge_unblock_ids(ids: &mut Vec<ThreadId>, orphans: Vec<u64>) {
    for vt in orphans {
        let tid = ThreadId(vt);
        if !ids.contains(&tid) {
            ids.push(tid);
        }
    }
}

/// gci i20/st: a slice that failed inside the VM (`SliceOutcome::Failed`)
/// reaches the carrier's Java as a `java.lang.InternalError` thrown by
/// `enterSpecial`, not as a VM-internal error. A `VmError::Internal` unwinds
/// the interpreted frames WITHOUT running their `finally` blocks, so
/// `Continuation.run`'s and `VirtualThread.runContinuation`'s `unmount()` were
/// skipped and the carrier kept the virtual thread as `currentThread()`: its
/// next task then threw `WrongThreadException` at
/// `VirtualThread.runContinuation` (i18a `vd19_g_churn_g1_small`), and every
/// virtual thread it took was lost. As a Java throwable the frames unmount
/// and the failure is printed by the worker's uncaught-exception handler.
/// When even the throwable cannot be built, the internal error as before.
///
/// gci i21/rv (finding 2 of
/// `gci-i21rv-i20-default-path-review-findings-FIXED-20261017`): a slice fails
/// mostly under heap pressure, where building the `InternalError` itself can
/// fail -- with a Java-convertible error (a thrown `OutOfMemoryError`, or a
/// `VmError::Runtime` the interpreter turns into its Java throwable). That
/// error is returned as is, so the frames still unwind through their
/// `finally` blocks; only a failure with no Java form falls back to the raw
/// internal error.
fn slice_failure_as_java(ctx: &mut NativeContextImpl<'_>, message: &str) -> MethodCallFailed {
    match crate::runtime::exceptions::create_exception_object(
        ctx.shared,
        ctx.thread,
        "java/lang/InternalError",
        Some(message),
    ) {
        Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
        Err(thrown @ MethodCallFailed::ExceptionThrown(_)) => thrown,
        Err(MethodCallFailed::InternalError(VmError::Runtime(error)))
            if error.as_java_throwable().is_some() =>
        {
            MethodCallFailed::InternalError(VmError::Runtime(error))
        }
        Err(_) => internal(message),
    }
}

/// The thread of continuation `cont`, from its `tail` chunk (`mark_tail`):
/// `None` before its first yield or after its end.
pub(crate) fn continuation_thread(
    ctx: &NativeContextImpl<'_>,
    cont: ObjectRef,
) -> Option<ThreadId> {
    let Value::Object(Some(tail)) = ctx.get_field_by_name(cont, "tail") else {
        return None;
    };
    match ctx.get_field_by_name(tail, "size") {
        Value::Int(id) if id > 0 => Some(ThreadId(id as u64)),
        _ => None,
    }
}

fn internal(message: &str) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Internal {
        message: message.to_string(),
    })
}

/// Why the continuation running on `thread` cannot unmount now, `0` when it
/// can (`Continuation.doYield` / `isPinned0`).
pub(crate) fn pinned_reason(thread: &JvmThread) -> i32 {
    if !thread.jdk_continuation.jdk() || thread.jdk_continuation.carrier() == 0 {
        return PINNED_NATIVE;
    }
    if thread.pin_count > 0 {
        return if thread.pin_reason == "Continuation.pin" {
            PINNED_CRITICAL_SECTION
        } else {
            PINNED_MONITOR
        };
    }
    // Interpreter round i4 wave 92, lane L6: a compiled frame beneath (this
    // runner OS thread's JIT entry chain is the continuation's own -- the
    // runner's base is Rust) pins as HotSpot's native frame does. A yield
    // keeps only interpreter frames as the frozen stack; there is no deopt at
    // a yield. Reached only under `CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY` and
    // the wave-93 `CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF` /
    // `CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS`
    // (`vthread_may_enter_compiled_method`), whose bodies reach Java only
    // through a VM upcall (a class initializer, a class load, a bootstrap) or
    // a static leaf, which cannot yield, so HotSpot pins too. With the
    // switches off the chain is empty here.
    if crate::runtime::native_callee_memo::continuation_pinned_by_native()
        || super::vm_util::in_clinit_shared()
        || crate::jit::conservative_roots::current_thread_jit_depth() != 0
    {
        return PINNED_NATIVE;
    }
    0
}

// Stage 1 (interpreter round i4 wave 92, lane L6,
// `CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY`, `VmConfig::vthread_compiled_leaf_entry`;
// page `i91-L6-virtual-thread-code-is-never-compiled`) was
// `vthread_may_enter_compiled(shared, thread, code, is_synchronized)`; since
// wave 93 every door asks [`vthread_may_enter_compiled_method`], which answers
// stage 1's question unchanged for a static callee while the stage-2
// switches are off.
//
// The first stage of compiled code on a JDK virtual thread: only a body that
// cannot reach a yield point by itself -- no invoke of any kind (so nothing
// inlined either), no `monitorenter` / `monitorexit`, not `synchronized` --
// so a yield can never find the compiled frame on the stack except under a
// VM upcall, where [`pinned_reason`] and the monitor unmounts'
// `current_thread_jit_depth() == 0` pin it, as HotSpot pins the native
// frame of its own upcall. `false` for every other thread kind's caller to
// read as "keep refusing": a bound (VM-scheduled) virtual thread yields
// through `vt_park_for` / `vt_sleep_for`, which do not read the JIT chain.

/// The longest `Code` [`vthread_may_enter_compiled_method`] scans. The scan runs on
/// every admitted call (a compiled `Jit` hit can come from a resolution shared
/// with platform threads, so the hit re-checks), with no memo: a memo cell
/// would be a new process static (`vm/tests/per_vm_state_statics_ratchet.rs`)
/// or a `JvmThread` / `CachedBytecodeMethod` field (the layout ratchet, ~50
/// struct literals). The bound keeps that scan to a few dozen cycles on the
/// short loops stage 1 is for; a longer body stays interpreted.
const VTHREAD_LEAF_MAX_CODE_BYTES: usize = 256;

/// A big-endian `i32` operand at `at`, `None` past the end.
fn read_i32_operand(code: &[u8], at: usize) -> Option<i64> {
    let bytes: [u8; 4] = code.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(i64::from(i32::from_be_bytes(bytes)))
}

/// Does the method whose `Code` is `code` contain no instruction that can
/// reach a yield point of a JDK virtual thread by itself: no `invokevirtual`,
/// `invokespecial`, `invokestatic`, `invokeinterface`, `invokedynamic`, no
/// `monitorenter` / `monitorexit`? Strict: a byte that is not a JVMS §6.5
/// opcode (`breakpoint`, `impdep1/2`, an unassigned one), a truncated
/// instruction or a malformed switch answers `false`, as does an empty array.
pub(crate) fn bytecode_cannot_reach_a_yield(code: &[u8]) -> bool {
    scan_for_yield_points(code, &mut |_, _| false)
}

/// The most `invokestatic` sites a body may have and still be admitted as a
/// caller of leaves (`CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS`): each costs one
/// invoke-cache probe on every admitted call.
const VTHREAD_LEAF_CALLER_MAX_SITES: usize = 4;

/// May `thread` enter (and offer for tier-up) the compiled body of `cached`?
/// Interpreter round i4 wave 93, lane L6, stage 2 of page
/// `i91-L6-virtual-thread-code-is-never-compiled`: the door switch is
/// `CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY` for a static callee and
/// `CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF` (default OFF) for an instance
/// one. The body must be a leaf ([`bytecode_cannot_reach_a_yield`], as stage
/// 1), or, under `CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS` (default OFF), a
/// caller of leaves: its only invokes are at most
/// [`VTHREAD_LEAF_CALLER_MAX_SITES`] `invokestatic` sites, each cached on THIS
/// thread (`JvmThread::invoke_cache`, keyed by the body's declaring class, the
/// constant-pool index and the pc) as a bytecode or compiled entry of a
/// static, non-`synchronized` leaf. A site not cached yet answers "no": the
/// body runs interpreted, which fills it.
///
/// The invariant is stage 1's: no unmount with a compiled frame on the stack.
/// The predicate keeps the body from reaching a yield by itself; anything the
/// compiled body still reaches as Java (an upcall, or a callee that differs
/// from the one cached here, e.g. after a redefinition) runs under the
/// compiled frame, where [`pinned_reason`] and the monitor unmounts'
/// `current_thread_jit_depth() == 0` pin it. With every stage-2 switch off
/// this answers exactly as stage 1's `vthread_may_enter_compiled` did for a
/// static callee, and `false` for an instance one.
pub(crate) fn vthread_may_enter_compiled_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &crate::classloading::resolution::CachedBytecodeMethod,
) -> bool {
    // Wave 101 (lane L2, `CRATONVM_VTHREAD_COMPILED_LEARNED`, default off):
    // any body the VM has not learned to refuse. Asked first: it is cheaper
    // than the leaf scan, and the union is the same either way. Off: one
    // config load, and the answer below is unchanged.
    if vthread_learned_admits(shared, thread, cached, true) {
        note_entry_admit(shared, EntryArm::Learned);
        return true;
    }
    // Wave 114 (lane L6, `CRATONVM_VTHREAD_COMPILED_APP_ENTRY`, default off):
    // any non-denied application body, with no caller walk. Off: one config
    // load.
    if vthread_app_entry_admits(shared, thread, cached) {
        note_entry_admit(shared, EntryArm::App);
        return true;
    }
    let config = &shared.config;
    let door_on = if cached.is_static {
        config.vthread_compiled_leaf_entry
    } else {
        config.vthread_compiled_instance_leaf
    };
    if !door_on {
        return false;
    }
    let leaf = vthread_body_cannot_yield(shared, thread, cached);
    if leaf {
        note_entry_admit(shared, EntryArm::Leaf);
    }
    leaf
}

/// Which arm of [`vthread_may_enter_compiled_method`] admitted an entry
/// (the `entry_admits_*` census rows; interpreter round i6 wave 117, lane L6).
#[derive(Clone, Copy)]
enum EntryArm {
    Learned,
    App,
    Leaf,
}

/// `CRATONVM_DBG_VTHREAD_CENSUS`: count one admitted entry of `arm`. One
/// config load when the census is off.
#[inline]
fn note_entry_admit(shared: &SharedVm, arm: EntryArm) {
    if !shared.config.dbg_vthread_census {
        return;
    }
    let census = &shared
        .threads
        .virtual_thread_manager
        .continuation_runners()
        .census;
    let row = match arm {
        EntryArm::Learned => &census.entry_admits_learned,
        EntryArm::App => &census.entry_admits_app,
        EntryArm::Leaf => &census.entry_admits_leaf,
    };
    row.fetch_add(1, Ordering::Relaxed);
}

/// `CRATONVM_DBG_VTHREAD_CENSUS` (interpreter round i6 wave 117, lane L6):
/// a deopt-resumed run on a JDK virtual thread took the continuation pin
/// (`deopt_resume::vthread_resume_pin`).
pub(crate) fn note_deopt_resume_pin(shared: &SharedVm) {
    if shared.config.dbg_vthread_census {
        shared
            .threads
            .virtual_thread_manager
            .continuation_runners()
            .census
            .deopt_resume_pins
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// The learned arm of [`vthread_may_enter_compiled_method`] and
/// [`vthread_may_osr_method`] (`CRATONVM_VTHREAD_COMPILED_LEARNED`,
/// `VmConfig::vthread_compiled_learned`; interpreter round i5 wave 101, lane
/// L2; proposal `i100-L6-proposal-learned-compiled-entry-on-virtual-threads`):
/// a JDK continuation's thread, a non-`synchronized` method, and no pin of
/// this VM has recorded the method in its deny set ([`learn_compiled_pin`]).
///
/// Why it is sound without a deopt at yield: no compiled frame is ever
/// frozen. Every JDK-continuation unmount asks the JIT entry chain first --
/// `Continuation.doYield` ([`pinned_reason`]) and the monitor / `Object.wait`
/// unmounts (`vm_exec::vthread_monitor_unmount_allowed`,
/// `vthread_wait_unmount_allowed`) -- and a non-empty chain pins (the runner's
/// own base is Rust, so every entry on it is the continuation's). The bound
/// (VM-scheduled) virtual-thread yields (`vt_park_for`, `vt_sleep_for`,
/// `vt_wait_on_key`) never apply to a JDK continuation. What the pin costs is
/// HotSpot-visible only as carrier time: a pinned park takes the scheduler's
/// compensation ([`vm_pinned_park_compensates`]), and the pin teaches the
/// deny set, so each blocking method pins at most once per VM.
///
/// `entry` (interpreter round i5 wave 104, lane L6): asked by an entry door
/// rather than the OSR door. Entry doors stop admitting once this VM's
/// compensated pinned parks outran what the deny set learned
/// (`ContinuationRunners::learned_entry_tripped`, [`LEARNED_COMPENSATION_SLACK`]);
/// a back-edge OSR offer keeps the learned arm either way.
#[inline]
fn vthread_learned_admits(
    shared: &SharedVm,
    thread: &JvmThread,
    cached: &crate::classloading::resolution::CachedBytecodeMethod,
    entry: bool,
) -> bool {
    // Wave 106 (lane L6): the entry doors' learned arm is its own switch,
    // `CRATONVM_VTHREAD_COMPILED_LEARNED_ENTRY` (`VmConfig::
    // vthread_compiled_learned_entry`, which implies the learned switch).
    // Off, an entry door answers as with the learned switch off, and pays
    // one config load; the OSR door keeps the learned arm.
    if !shared.config.vthread_compiled_learned
        || (entry && !shared.config.vthread_compiled_learned_entry)
    {
        return false;
    }
    let runners = shared.threads.virtual_thread_manager.continuation_runners();
    thread.jdk_continuation.jdk()
        && !cached.is_synchronized
        && !learned_skips_jdk_class(&cached.class_name)
        && !(entry && runners.learned_entry_tripped.load(Ordering::Relaxed))
        && !runners.learned_denials.contains(cached.invoc_key())
        && !(entry && runners.learned_entry_denials.contains(cached.invoc_key()))
        && !(entry && learned_caller_denied(thread, runners))
}

/// Does the learned arm leave `class_name`'s methods to the switch-off
/// answer? The JDK's own packages (interpreter round i5 wave 110, lane L6,
/// page `i91-L6`, "Wave 110"). What a virtual thread runs of the JDK is the
/// park / unpark / scheduler path (`VirtualThread.parkNanos` ->
/// `scheduleUnpark` -> `ScheduledThreadPoolExecutor` -> the delayed work
/// queue under its `ReentrantLock` -> `AbstractQueuedSynchronizer.acquire`'s
/// spin-then-park loop): short loops around locks and parks, whose OSR'd
/// bodies are the likeliest to pin a yield (and, untimed, to take a carrier
/// compensation) before the deny set learns them, and which differ between
/// `--jdk-only` and `--compatible` (the HotSpotAlignment families decide
/// what runs as JDK bytecode) -- the w107-w109 `helpers virtual` losses
/// moved between the modes from run to run. The learned switch is for
/// application loops (`osrTask`); the JDK's keep the switch-off answer.
#[inline]
fn learned_skips_jdk_class(class_name: &str) -> bool {
    class_name.starts_with("java/")
        || class_name.starts_with("jdk/")
        || class_name.starts_with("sun/")
        || class_name.starts_with("com/sun/")
}

/// The application entry arm of [`vthread_may_enter_compiled_method`]
/// (`CRATONVM_VTHREAD_COMPILED_APP_ENTRY`, `VmConfig::vthread_compiled_app_entry`,
/// default off, needs the learned switch; interpreter round i5 wave 114, lane
/// L6, page `i113-orch-virtual-thread-frames-enter-only-leaf-bodies-compiled`):
/// a JDK continuation's thread, a non-`synchronized` method of a class outside
/// the JDK's packages ([`learned_skips_jdk_class`]), not named by the deny set,
/// while the entry trip-wire has not fired.
///
/// What it drops from the wave 101-105 entry arm ([`vthread_learned_admits`]
/// with `entry`), and why that arm lost on `helpers` (by reading; waves
/// 103-106 measured it 1.05x to 19x worse):
///
/// * the [`learned_caller_denied`] walk (up to [`LEARNED_CALLER_DEPTH`] frames,
///   a deny-set probe each, on every door call). Its purpose was to keep a
///   denied method's callees interpreted, because those entries looked like
///   the cost; wave 113 found the cost was armed branch profiling of the
///   nominated methods on 32 carriers (`CRATONVM_VTHREAD_SKIP_BRANCH_ARM`,
///   now default on). A denied method is one that parks; the helpers it calls
///   are exactly what should run compiled, and entering them is sound (a park
///   under one pins and denies it).
/// * nothing else: the JDK classes were already left out since wave 110 (the
///   park path's `ScheduledThreadPoolExecutor` / AQS bodies, entered from
///   interpreted frames on every park in waves 103-106).
///
/// Per call: one config load when off; on, a byte compare or four
/// (`starts_with`), a relaxed load, and the deny set's lock-free probe (one
/// load while the set is empty), all of read-only shared lines. The answer
/// can only move from "admit" to "refuse" (the set and the trip-wire only
/// grow), so the static door's `Jit` arm, which re-asks on every hit, drops a
/// stale entry on the first call after a denial.
#[inline]
fn vthread_app_entry_admits(
    shared: &SharedVm,
    thread: &JvmThread,
    cached: &crate::classloading::resolution::CachedBytecodeMethod,
) -> bool {
    if !shared.config.vthread_compiled_app_entry
        || !thread.jdk_continuation.jdk()
        || cached.is_synchronized
        || learned_skips_jdk_class(&cached.class_name)
    {
        return false;
    }
    let runners = shared.threads.virtual_thread_manager.continuation_runners();
    !runners.learned_entry_tripped.load(Ordering::Relaxed)
        && !runners.learned_denials.contains(cached.invoc_key())
        && !runners.learned_entry_denials.contains(cached.invoc_key())
        // Interpreter round i6 wave 120, lane L6
        // (`CRATONVM_VTHREAD_APP_ENTRY_CALLER_WALK`, default off; page
        // `i113-orch-virtual-thread-frames-enter-only-leaf-bodies-compiled`,
        // "Wave 120"): the learned entry arm's walk for this arm too. A
        // denied (parking) method runs interpreted, and without the walk its
        // callees (`work`, then `next`, `area`, `mix` under `helpersTask`) were
        // admitted compiled one interpreted-to-compiled transition per call,
        // the wave-104/105 shape the walk was written for. Off: one config
        // load.
        && !(shared.config.vthread_app_entry_caller_walk
            && learned_caller_denied(thread, runners))
}

/// Is the frame making this call (the top frame at an entry door) a method
/// the deny set refuses? Then its callees are judged as with the switch off
/// (interpreter round i5 wave 104 hand-back, lane L6): a denied method runs
/// interpreted on virtual threads, and its callees admitted compiled by the
/// learned arm made the denied body slower than the switch-off answer --
/// `L6W103VirtualThreadLearnedShortBench` `helpers` virtual 7.1x worse on
/// w104 with `helpersTask` denied and no compensated park at all
/// (`learned_compensated_parks=0`). The fallback of a denied method is
/// exactly the switch-off behaviour, callees included. One load while the set
/// is empty; otherwise one key over the frame's name and descriptor.
///
/// Wave 105 (lane L6): not only the calling frame but the
/// [`LEARNED_CALLER_DEPTH`] innermost frames. With the caller alone, a denied
/// method's callee (`work` under the denied `helpersTask`) was refused and ran
/// interpreted, but ITS callees (`next`, `area`, `mix`) were admitted again:
/// an interpreted-to-compiled transition per leaf call in the denied method's
/// inner loop, a shape the switch-off arm never runs (still 7-19x worse on
/// the w104 builds). Now the methods a denied frame reaches within that depth
/// run as with the switch off as well. The key is the frame's memoized
/// `CachedBytecodeMethod::invoc_key` (the hash of name and descriptor only
/// for an owned frame), so a probe costs one load and the set's lookup.
#[inline]
fn learned_caller_denied(thread: &JvmThread, runners: &ContinuationRunners) -> bool {
    // Wave 117 (lane L6): either deny set; an entry-only denial refuses its
    // callees as a full one does (it was a full one before wave 117).
    let (denials, entry_denials) = (&runners.learned_denials, &runners.learned_entry_denials);
    if denials.len() == 0 && entry_denials.len() == 0 {
        return false;
    }
    thread
        .frames
        .iter()
        .rev()
        .take(LEARNED_CALLER_DEPTH)
        .any(|frame| {
            let key = match frame.cached_method() {
                Some(cached) => cached.invoc_key(),
                None => cratonvm_jit_api::invoc_key_parts(
                    frame.class_id.as_u32(),
                    frame.method_name(),
                    frame.method_descriptor(),
                ),
            };
            denials.contains(key) || entry_denials.contains(key)
        })
}

/// How many innermost frames [`learned_caller_denied`] asks (interpreter
/// round i5 wave 105, lane L6): a denied method's calls, their calls and so
/// on down to this depth fall back to the switch-off answer.
const LEARNED_CALLER_DEPTH: usize = 4;

/// May the static door, which refused to ENTER `cached`'s compiled body on
/// `thread` (a JDK virtual thread), still NOMINATE it for a background
/// compile at a tier-up stride? Under `CRATONVM_VTHREAD_COMPILED_LEARNED`
/// (interpreter round i5 wave 106, lane L6): a JDK continuation's thread, a
/// non-`synchronized` method the deny set does not name. Nothing is entered:
/// the published body serves the learned OSR door's compiled loop
/// (compiled-to-compiled) and platform threads. Asked only when a stride is
/// due (`should_offer_tier_up`), never per call, and the caller has already
/// required background compilation (an inline compile may run Java under
/// Rust frames a yield could not unwind through). No caller walk: a
/// nomination enters nothing, so the context of the call does not matter.
///
/// Wave 108 (lane L6, page `i91-L6`, "Wave 108"): and only within the
/// method's nomination window, the first [`LEARNED_NOMINATION_STRIDES`] due
/// strides past the warmup threshold (`invoc_count` is the method's own
/// persistent count, `threshold` the door's warmup bar). Wave 107's
/// `learned_nominee_published` stops the strides once a body is published,
/// but a body that never publishes under the probed key -- a compile the
/// policy or the compiler declines without settling the call site, a
/// method still `queued_for_compilation` behind a long background queue
/// (each such stride takes the manager's global `methods` mutex, on every
/// carrier at once), a body published and then invalidated by a deopt and
/// re-queued, round after round -- kept the stride offering for the life of
/// the process. Past the window the static door answers exactly as with the
/// switch off: refused before the offer, no lookup, no lock. Pure arithmetic
/// on a count the door already holds, so the steady state of a method that
/// will never publish costs nothing.
#[inline]
pub(crate) fn vthread_learned_nominates(
    shared: &SharedVm,
    thread: &JvmThread,
    cached: &crate::classloading::resolution::CachedBytecodeMethod,
    invoc_count: u32,
    threshold: u32,
) -> bool {
    if !shared.config.vthread_compiled_learned
        || !thread.jdk_continuation.jdk()
        || cached.is_synchronized
    {
        return false;
    }
    let runners = shared.threads.virtual_thread_manager.continuation_runners();
    if invoc_count.saturating_sub(threshold) >= LEARNED_NOMINATION_WINDOW_CALLS {
        if shared.config.dbg_vthread_census {
            runners
                .census
                .learned_nominations_past_window
                .fetch_add(1, Ordering::Relaxed);
        }
        return false;
    }
    let nominates = !runners.learned_denials.contains(cached.invoc_key())
        && !learned_nominee_published(shared, cached);
    if nominates && shared.config.dbg_vthread_census {
        runners
            .census
            .learned_nominations
            .fetch_add(1, Ordering::Relaxed);
    }
    nominates
}

/// How many due tier-up strides of one method the learned arm may nominate
/// from virtual threads (interpreter round i5 wave 108, lane L6;
/// [`vthread_learned_nominates`]). Eight strides of
/// `JIT_RETRY_STRIDE` (64) calls: enough for a transient refusal (a gate
/// that was busy, a queue that was full) to be retried, as the platform
/// threads' door retries, and a bound on everything else.
const LEARNED_NOMINATION_STRIDES: u32 = 8;

/// How long a slice of a JDK continuation must already have run when a
/// compiled frame pins its yield for [`learn_compiled_pin`] to leave the
/// deny set alone (interpreter round i5 wave 110, lane L6). Well above the
/// gap between two parks of a park-heavy body (`L6W103` / `L6W108`
/// `helpersTask`: ~0.1 ms) and below the compiled stretch of a long loop
/// between parks (`osrTask`: ~10-15 ms compiled per pass).
const LEARNED_DENY_MIN_SLICE: std::time::Duration = std::time::Duration::from_millis(5);

/// [`LEARNED_NOMINATION_STRIDES`] in calls past the warmup threshold. Also the
/// count past which a refused virtual-thread call stops counting under
/// `CRATONVM_VTHREAD_LOCAL_CALL_COUNT` (`dispatch_static`,
/// `vthread_static_call_count`; wave 114).
pub(crate) const LEARNED_NOMINATION_WINDOW_CALLS: u32 = LEARNED_NOMINATION_STRIDES
    * crate::runtime::interpreter::jit_bridge::JIT_RETRY_STRIDE;

/// Has a compiled body of `cached`'s method been published (any tier)?
/// Interpreter round i5 wave 107, lane L6 (page `i91-L6`, "Wave 107"): the
/// static door's nomination ([`vthread_learned_nominates`]) stops there.
///
/// Why: a virtual-thread call site whose entry is refused never upgrades its
/// invoke-cache entry to `Jit`, so `should_offer_tier_up` stays due every
/// `JIT_RETRY_STRIDE` (64) calls of the method for the life of the process,
/// and each due stride took the whole tier-up offer
/// (`offer_invocation_to_tiered_manager`: the manager's global `methods`
/// mutex, the `ldc` slot mint walk, the VarHandle refresh, a C2 re-queue and
/// its branch-profile arm) -- on every carrier at once, where the switch-off
/// arm refuses before counting. The w106 `helpers virtual` row (1.10x
/// `--jdk-only`, 1.29x `--compatible` against the switch-off arm) is, by
/// reading, those strides: ~4 800 offers per rep for `work` alone. Once a
/// body exists the nomination's purpose -- a compiled callee for the learned
/// OSR door's compiled loop -- is served; later tiers come from the compiled
/// callers' own counters and from platform threads, as for any compiled
/// method.
///
/// The probe is the `Bytecode` arm's (`dispatch_static`): the per-call-site
/// miss memo (`jit_probe_is_current`) answers "none" without a lookup while
/// the JIT cache's generation is unchanged; a hit is not memoized here (the
/// found tag belongs to the bytecode-callee doors), so a published method
/// pays one read-locked lookup per due stride, the price of an offer's first
/// line.
fn learned_nominee_published(
    shared: &SharedVm,
    cached: &crate::classloading::resolution::CachedBytecodeMethod,
) -> bool {
    let generation = shared.jit.jit_cache.generation();
    if cached.jit_probe_is_current(generation) {
        return false;
    }
    let found = shared
        .jit
        .jit_cache
        .read()
        .get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        )
        .is_some();
    if !found {
        cached.record_jit_probe_miss(generation);
    }
    found
}

/// Is a back-edge OSR offer on `thread`, a virtual thread, worth judging at
/// all: one of the two switches that can admit one is on and the thread is a
/// JDK continuation's? (`interpreter::try_osr_offer` and the
/// `osr_call_floor`; the method itself is judged by [`vthread_may_osr_method`]
/// once an offer is due.)
#[inline]
pub(crate) fn vthread_osr_door_on(shared: &SharedVm, thread: &JvmThread) -> bool {
    (shared.config.vthread_compiled_leaf_osr || shared.config.vthread_compiled_learned)
        && thread.jdk_continuation.jdk()
}

/// A park of `thread` that is about to block its carrier because the yield
/// before it was pinned by something HotSpot would not pin on: a compiled
/// frame beneath (the JIT entry chain is not empty) or a VM door's
/// `native_callee_memo::pin_continuation` (a deopt-resumed frame run to
/// completion, a lambda door's constructor reference). Only under
/// `CRATONVM_VTHREAD_COMPILED_LEARNED`, the switch that makes the first
/// common (interpreter round i5 wave 101, lane L2). HotSpot unmounts there,
/// so the scheduler compensates for the carrier this VM keeps
/// (`begin_carrier_compensation`), exactly as for a pinned `Object.wait`;
/// without it a pinned park on the only carrier could wait for an unparker
/// that never gets one. `false` for every other park: a real native frame
/// or a monitor pin keeps HotSpot's uncompensated pinned park.
pub(crate) fn vm_pinned_park_compensates(shared: &SharedVm, thread: &JvmThread) -> bool {
    let compensates = shared.config.vthread_compiled_learned
        && thread.jdk_continuation.jdk()
        && thread.jdk_continuation.carrier() != 0
        && thread.pin_count == 0
        && (crate::jit::conservative_roots::current_thread_jit_depth() != 0
            || cratonvm_native_api::continuation_pin::pins() > 0);
    if compensates {
        let runners = shared.threads.virtual_thread_manager.continuation_runners();
        if shared.config.dbg_vthread_census {
            runners
                .census
                .learned_compensated_parks
                .fetch_add(1, Ordering::Relaxed);
            if crate::jit::conservative_roots::current_thread_jit_depth() == 0 {
                runners
                    .census
                    .learned_vm_pin_parks
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        // Wave 104 (lane L6): the trip-wire of the learned arm's entry
        // doors (`LEARNED_COMPENSATION_SLACK`). One relaxed add per
        // compensated park, a path that is about to block its carrier.
        let paid = runners.learned_compensations.fetch_add(1, Ordering::Relaxed) + 1;
        let allowed = LEARNED_COMPENSATION_SLACK.saturating_add(
            runners
                .learned_denials
                .len()
                .saturating_add(runners.learned_entry_denials.len())
                .saturating_mul(LEARNED_COMPENSATIONS_PER_DENIAL),
        );
        // The swap runs whenever the budget is spent; the line prints once.
        if paid > allowed
            && !runners.learned_entry_tripped.swap(true, Ordering::Relaxed)
            && shared.config.dbg_vthread_census
        {
            eprintln!(
                "[vthread-census] learned-entry-tripped compensations={paid} denied={}",
                runners.learned_denials.len()
            );
        }
    }
    compensates
}

/// A `Continuation.doYield` of `thread` was just pinned (`reason`) under
/// `CRATONVM_VTHREAD_COMPILED_LEARNED`: when the only cause is a compiled
/// frame beneath -- no `Continuation.pin`, no monitor pin, no class
/// initializer (since wave 104 a native frame or a VM door's pin on the stack
/// as well does not stop it) -- record every compiled activation
/// on this runner's stack in the VM's deny set, so the doors stop admitting
/// those methods on virtual threads (interpreter round i5 wave 101, lane L2).
/// Every activation, not only the innermost: each is a frame a yield would
/// have to freeze, and a method entered compiled-to-compiled has no entry of
/// its own on the chain, so only the stack walk names it.
///
/// Each activation is recorded under its artifact's owner class and, when it
/// differs, its holder (`CompiledMethod::frame_class_id`: the by-name callee
/// door publishes an inherited body under the receiver's key, while the doors
/// ask with the declaring class's `CachedBytecodeMethod`).
#[cold]
#[inline(never)]
fn learn_compiled_pin(shared: &SharedVm, thread: &JvmThread, reason: i32) {
    // Wave 104 (lane L6): a native or VM-door pin on the stack as well no
    // longer stops the record. The compiled activations are still frames a
    // yield would have to freeze, the park is still compensated
    // (`vm_pinned_park_compensates` asks only the JIT chain and the VM pins),
    // and with the switch on the commonest such pin is the deopt sink's own
    // (`deopt_resume::vthread_resume_pin`, under the compiled caller of a
    // trapped callee): skipping it could leave the caller admitted, pinning
    // every park of every activation (the suspected cause of the
    // `L6W103VirtualThreadLearnedShortBench` `helpers` loss; not measured).
    // Recording refuses more methods only, the safe direction.
    if reason != PINNED_NATIVE
        || thread.pin_count != 0
        || crate::jit::conservative_roots::current_thread_jit_depth() == 0
        || super::vm_util::in_clinit_shared()
    {
        return;
    }
    let runners = shared.threads.virtual_thread_manager.continuation_runners();
    // Wave 110 (lane L6, page `i91-L6`, "Wave 110"): a pin after a LONG run
    // of the slice teaches nothing. The deny set exists to bound pinned parks
    // per unit of work: `helpersTask` (150 short calls between parks) pinned
    // every ~100 us and had to go back to the interpreter. But the record is
    // for the life of the VM, so an OSR'd loop that runs for milliseconds
    // between two parks (`osrTask`: 80 000 iterations, then one
    // `parkNanos(1000)`) was denied at its first pin, and every later
    // activation ran interpreted: the OSR win showed only in the activations
    // before that pin -- the best-of-3 TIME line of
    // `L6W103VirtualThreadLearnedShortBench` (0.03x) -- and not in the median
    // of `L6W108VirtualThreadLearnedRepsBench` (1.00x, w109). Such a pin costs
    // one pinned park per >= `LEARNED_DENY_MIN_SLICE` of compiled work.
    if thread
        .vt_slice_started
        .is_some_and(|started| started.elapsed() >= LEARNED_DENY_MIN_SLICE)
    {
        if shared.config.dbg_vthread_census {
            runners
                .census
                .learned_pins_kept_long_slice
                .fetch_add(1, Ordering::Relaxed);
        }
        return;
    }
    let mut learned = 0u64;
    let frames = crate::jit::conservative_roots::active_compiled_frames();
    // Interpreter round i6 wave 117, lane L6 (page
    // `i117-L6-an-entry-arm-pin-closes-the-learned-osr-door`): a pin with no
    // OSR activation on the stack, while an entry arm is on, was made by a
    // body an ENTRY door admitted. It denies the entry doors only: recorded in
    // the full set, it also closed the learned OSR door
    // ([`vthread_may_osr_method`]) for the life of the VM, so `osrTask` /
    // `helpersTask` (entered compiled once the loop-work credit made them
    // hot, then pinned at their park) ran their loops interpreted from then
    // on -- `L6W108` `osr-virtual` 24-33x, `helpers-virtual` 2.5-3.1x with
    // `CRATONVM_VTHREAD_COMPILED_APP_ENTRY` or `_LEARNED_ENTRY` on. A frame
    // whose `CompiledMethod` is unknown counts as OSR (the old, full record).
    let entry_only = shared.config.vthread_entry_pin_denies_entry_only
        && (shared.config.vthread_compiled_app_entry
            || shared.config.vthread_compiled_learned_entry)
        && frames.iter().all(|frame| {
            frame.cm_ptr != 0
                // SAFETY: as below: the activation's own `CompiledMethod`,
                // live while the frame is, read on the frame's own thread.
                && !unsafe { &*(frame.cm_ptr as *const cratonvm_jit::CompiledMethod) }
                    .compiled_via_osr
        });
    let set = if entry_only {
        &runners.learned_entry_denials
    } else {
        &runners.learned_denials
    };
    for frame in frames {
        let Some((owner_and_name, descriptor)) = frame.label.rsplit_once(':') else {
            continue;
        };
        let Some((_, method_name)) = owner_and_name.rsplit_once('.') else {
            continue;
        };
        let mut classes = [frame.owner_class_id, frame.owner_class_id];
        if frame.cm_ptr != 0 {
            // SAFETY: `cm_ptr` names the `CompiledMethod` this activation is
            // running, valid while the frame is live
            // (`ActiveCompiledFrame::cm_ptr`); this runs on the frame's own
            // thread, beneath it, inside the yield it pinned.
            let cm = unsafe { &*(frame.cm_ptr as *const cratonvm_jit::CompiledMethod) };
            classes[1] = cm.frame_class_id();
        }
        for (i, class_id) in classes.iter().copied().enumerate() {
            if class_id == cratonvm_types::jit_activation::NO_OWNER_CLASS
                || (i == 1 && class_id == classes[0])
            {
                continue;
            }
            let key = cratonvm_jit_api::invoc_key_parts(class_id, method_name, descriptor);
            if set.insert(key) {
                learned += 1;
                if shared.config.dbg_vthread_census {
                    eprintln!(
                        "[vthread-census] {} {}",
                        if entry_only {
                            "learned-entry-deny"
                        } else {
                            "learned-deny"
                        },
                        frame.label
                    );
                }
            }
        }
    }
    if learned != 0 && shared.config.dbg_vthread_census {
        let row = if entry_only {
            &runners.census.learned_entry_pins
        } else {
            &runners.census.learned_pins
        };
        row.fetch_add(1, Ordering::Relaxed);
    }
}

/// Back-edge OSR of a frame of `cached` on `thread`
/// (`CRATONVM_VTHREAD_COMPILED_LEAF_OSR`, `VmConfig::vthread_compiled_leaf_osr`;
/// interpreter round i5 wave 100, lane L6; page
/// `i91-L6-virtual-thread-code-is-never-compiled`): the switch, background
/// compilation (an inline OSR compile may run Java -- a verifier load --
/// under Rust compiler frames a yield could not unwind through, as the entry
/// doors' inline tier-up), and the body predicate of
/// [`vthread_may_enter_compiled_method`] without its door switch. An OSR
/// body runs the rest of the activation compiled, so the whole method must
/// be unable to reach a yield, not only the loop. `false` for every thread
/// that is not a mounted JDK virtual thread's.
pub(crate) fn vthread_may_osr_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &crate::classloading::resolution::CachedBytecodeMethod,
) -> bool {
    // Wave 101 (lane L2): or any body the learned arm admits
    // (`CRATONVM_VTHREAD_COMPILED_LEARNED`); background compilation either way.
    let admitted = crate::runtime::env_cache::bg_compile()
        && ((shared.config.vthread_compiled_leaf_osr
            && vthread_body_cannot_yield(shared, thread, cached))
            || vthread_learned_admits(shared, thread, cached, false));
    // Wave 117 (lane L6): the `osr_refused_by_denial` census row. Asked
    // only for a refused offer, which retires its header for the activation.
    if !admitted && shared.config.dbg_vthread_census && shared.config.vthread_compiled_learned {
        let runners = shared.threads.virtual_thread_manager.continuation_runners();
        if runners.learned_denials.contains(cached.invoc_key()) {
            runners
                .census
                .osr_refused_by_denial
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    admitted
}

/// The body half of [`vthread_may_enter_compiled_method`] and
/// [`vthread_may_osr_method`]: a JDK continuation, not `synchronized`, `Code`
/// within [`VTHREAD_LEAF_MAX_CODE_BYTES`], and the leaf scan (widened to
/// callers of cached static leaves under `CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS`).
fn vthread_body_cannot_yield(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &crate::classloading::resolution::CachedBytecodeMethod,
) -> bool {
    let config = &shared.config;
    if !thread.jdk_continuation.jdk()
        || cached.is_synchronized
        || cached.code.len() > VTHREAD_LEAF_MAX_CODE_BYTES
    {
        return false;
    }
    if !config.vthread_compiled_leaf_callers {
        // Interpreter round i6 wave 118, lane L6
        // (`CRATONVM_VTHREAD_LEAF_VERDICT_MEMO`, default off): the scan is a
        // pure function of the code here, so its answer is memoized per body.
        if config.vthread_leaf_verdict_memo {
            return shared
                .threads
                .virtual_thread_manager
                .continuation_runners()
                .leaf_verdicts
                .verdict(&cached.code, || bytecode_cannot_reach_a_yield(&cached.code));
        }
        return bytecode_cannot_reach_a_yield(&cached.code);
    }
    let caller = cached.declaring_class_id;
    let mut sites = 0usize;
    scan_for_yield_points(&cached.code, &mut |pc, cp_index| {
        sites += 1;
        sites <= VTHREAD_LEAF_CALLER_MAX_SITES
            && cached_static_leaf_site(thread, caller, cp_index, pc)
    })
}

/// Is `thread`'s inline-cache entry for the `invokestatic` at `pc` of a body
/// of `caller` (constant-pool index `cp_index`) a bytecode or compiled entry
/// of a static leaf? `false` for a miss, a native, an intrinsic or any other
/// shape.
fn cached_static_leaf_site(
    thread: &mut JvmThread,
    caller: crate::classloading::ClassId,
    cp_index: u16,
    pc: usize,
) -> bool {
    let Ok(pc) = u32::try_from(pc) else {
        return false;
    };
    match thread.invoke_cache.get(caller, cp_index, false, pc) {
        Some(
            crate::classloading::resolution::CachedInvokeTarget::Bytecode { cached, .. }
            | crate::classloading::resolution::CachedInvokeTarget::Jit { cached, .. },
        ) => {
            cached.is_static
                && !cached.is_synchronized
                && cached.code.len() <= VTHREAD_LEAF_MAX_CODE_BYTES
                && bytecode_cannot_reach_a_yield(&cached.code)
        }
        _ => false,
    }
}

/// The walk under [`bytecode_cannot_reach_a_yield`]: `false` at the first
/// instruction that can reach a yield point by itself, except that an
/// `invokestatic` at `pc` with constant-pool index `cp` is passed to
/// `invokestatic(pc, cp)`, whose `false` ends the walk with `false`.
fn scan_for_yield_points(code: &[u8], invokestatic: &mut dyn FnMut(usize, u16) -> bool) -> bool {
    let mut pc = 0usize;
    while pc < code.len() {
        let len: usize = match code[pc] {
            // invokestatic: the caller decides.
            0xb8 => {
                let Some(&[hi, lo]) = code.get(pc + 1..pc + 3) else {
                    return false;
                };
                if !invokestatic(pc, u16::from_be_bytes([hi, lo])) {
                    return false;
                }
                3
            }
            // invokevirtual, invokespecial, invokeinterface, invokedynamic.
            0xb6 | 0xb7 | 0xb9 | 0xba => return false,
            // monitorenter, monitorexit.
            0xc2 | 0xc3 => return false,
            // breakpoint, unassigned, impdep1, impdep2.
            0xca..=0xff => return false,
            // bipush, ldc, [ilfda]load, [ilfda]store, ret, newarray.
            0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
            // sipush, ldc_w, ldc2_w, iinc, if* / goto / jsr, field access,
            // new, anewarray, checkcast, instanceof, ifnull, ifnonnull.
            0x11
            | 0x13
            | 0x14
            | 0x84
            | 0x99..=0xa8
            | 0xb2..=0xb5
            | 0xbb
            | 0xbd
            | 0xc0
            | 0xc1
            | 0xc6
            | 0xc7 => 3,
            // multianewarray.
            0xc5 => 4,
            // goto_w, jsr_w.
            0xc8 | 0xc9 => 5,
            // wide: `wide iinc` is six bytes, every other widened form four.
            0xc4 => match code.get(pc + 1) {
                Some(0x84) => 6,
                Some(0x15..=0x19 | 0x36..=0x3a | 0xa9) => 4,
                _ => return false,
            },
            // tableswitch: padding to a multiple of four, default, low, high,
            // then `high - low + 1` offsets.
            0xaa => {
                let base = (pc + 4) & !3;
                let (Some(low), Some(high)) = (
                    read_i32_operand(code, base + 4),
                    read_i32_operand(code, base + 8),
                ) else {
                    return false;
                };
                if high < low {
                    return false;
                }
                let Ok(count) = usize::try_from(high - low + 1) else {
                    return false;
                };
                let Some(end) = count
                    .checked_mul(4)
                    .and_then(|n| n.checked_add(base + 12))
                else {
                    return false;
                };
                end - pc
            }
            // lookupswitch: padding, default, npairs, then npairs pairs.
            0xab => {
                let base = (pc + 4) & !3;
                let Some(npairs) = read_i32_operand(code, base + 4) else {
                    return false;
                };
                let Ok(npairs) = usize::try_from(npairs) else {
                    return false;
                };
                let Some(end) = npairs
                    .checked_mul(8)
                    .and_then(|n| n.checked_add(base + 8))
                else {
                    return false;
                };
                end - pc
            }
            _ => 1,
        };
        pc = match pc.checked_add(len) {
            Some(next) => next,
            None => return false,
        };
    }
    pc == code.len() && !code.is_empty()
}

/// The `pinnedReason` and start (JFR ticks, `None` = now) of the
/// `jdk.VirtualThreadPinned` event `thread` posts now: HotSpot 25's
/// `JVM_VirtualThreadPinnedEvent` starts the event at the thread's last
/// failed freeze, and `JavaThread::post_vthread_pinned_event` names
/// `"VM call to <K>.<clinit> on stack"` while a class initializer runs,
/// `"Native or VM frame on stack"` for `freeze_pinned_native`, and
/// `"Freeze or preempt failed (<result>)"` otherwise (this VM's pin codes are
/// HotSpot's `freeze_result` values). Interpreter round i5 wave 105, lane L6.
pub(crate) fn pinned_event_cause(shared: &SharedVm, thread: &JvmThread) -> (String, Option<i64>) {
    let recorded = shared
        .threads
        .virtual_thread_manager
        .continuation_runners()
        .freeze_fails
        .lock()
        .remove(&thread.thread_id.0);
    let (reason, start) = match recorded {
        Some((reason, ticks)) => (reason, Some(ticks)),
        None => (pinned_reason(thread), None),
    };
    // `class_being_initialized`: the innermost class initializer on the stack.
    let clinit = thread
        .frames
        .iter()
        .rev()
        .find(|f| f.method_name() == "<clinit>")
        .map(|f| f.class_id);
    if let Some(class_id) = clinit {
        let name = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.replace('/', "."));
        if let Some(name) = name {
            return (format!("VM call to {name}.<clinit> on stack"), start);
        }
    }
    let text = if reason == PINNED_NATIVE || reason == 0 {
        "Native or VM frame on stack".to_string()
    } else {
        format!("Freeze or preempt failed ({reason})")
    };
    (text, start)
}

/// `Continuation.doYield()` on `thread`.
pub(crate) fn do_yield(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> Result<i32, MethodCallFailed> {
    let reason = pinned_reason(thread);
    // `CRATONVM_DBG_VTHREAD_CENSUS` (interpreter round i4 wave 94, lane L6):
    // a pin by a compiled frame beneath is what a deopt at yield (stage 3 of
    // `i91-L6-virtual-thread-code-is-never-compiled`) would turn into an
    // unmount.
    if shared.config.dbg_vthread_census {
        let census = &shared
            .threads
            .virtual_thread_manager
            .continuation_runners()
            .census;
        let count = if reason == 0 {
            &census.yields_taken
        } else if reason == PINNED_NATIVE
            && crate::jit::conservative_roots::current_thread_jit_depth() != 0
        {
            &census.yields_pinned_by_compiled
        } else {
            &census.yields_pinned_other
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
    if reason != 0 {
        // Wave 101 (lane L2, `CRATONVM_VTHREAD_COMPILED_LEARNED`): a pin by a
        // compiled frame teaches the doors to keep that method interpreted
        // on virtual threads.
        if shared.config.vthread_compiled_learned {
            learn_compiled_pin(shared, thread, reason);
        }
        if cratonvm_native_builtins::jfr_vm_events::any_recording() {
            let runners = shared.threads.virtual_thread_manager.continuation_runners();
            let mut fails = runners.freeze_fails.lock();
            if fails.len() >= FREEZE_FAILS_CAP {
                fails.clear();
            }
            fails.insert(
                thread.thread_id.0,
                (reason, cratonvm_native_builtins::jfr_vm_events::ticks_now()),
            );
        }
        return Ok(reason);
    }
    thread.vt_yield = VtYield::DoYield;
    Err(MethodCallFailed::InternalError(
        VmError::ContinuationYield {
            wake_after_nanos: 0,
        },
    ))
}

/// `Thread.setCurrentThread(t)` on `ctx.thread`: `Thread.currentThread()`
/// answers `t` from now on; `Thread.currentCarrierThread()` keeps answering
/// the platform thread underneath (this thread's own, or a continuation's
/// carrier).
pub(crate) fn set_current_thread(ctx: &mut NativeContextImpl<'_>, t: ObjectRef) {
    let registry = &ctx.shared.threads.thread_registry;
    if ctx.thread.jdk_continuation.carrier() == 0 {
        // The first re-point of a platform thread: remember who it is.
        ctx.thread
            .jdk_continuation
            .set_carrier(ctx.thread.thread_id.0);
    }
    ctx.thread.java_thread_obj = Some(t);
    // JIT round 20 wave 8 (lane huntFFM): the FFM verdicts and the compiled
    // segment door's memo of this OS thread hold confined-owner answers about
    // the Java thread that earned them; another one is current from here on.
    cratonvm_native_builtins::ffm_fast::forget_thread_verdicts();
    crate::jit::helpers::forget_real_seg_entries();
    // Back to its own identity: nothing to remember.
    if !ctx.thread.jdk_continuation.jdk()
        && registry.java_thread_obj(ctx.thread.thread_id) == Some(t)
    {
        ctx.thread.jdk_continuation.set_carrier(0);
    }
}

/// `Thread.currentCarrierThread()` on `ctx.thread`.
pub(crate) fn current_carrier_thread(ctx: &mut NativeContextImpl<'_>) -> ObjectRef {
    let carrier = ctx.thread.jdk_continuation.carrier();
    if carrier != 0 {
        if let Some(obj) = ctx
            .shared
            .threads
            .thread_registry
            .java_thread_obj(ThreadId(carrier))
        {
            return obj;
        }
    }
    ctx.current_thread_object()
}

/// `jdk.internal.misc.Blocker`: the JDK's compensation for a virtual thread
/// that blocks its carrier (JDK 22+: `begin()Z` / `end(Z)V`; JDK 21's
/// `Object.wait` compensates in Java, through `begin()J`, so its absence here
/// is what keeps that image from compensating twice).
const BLOCKER: &str = "jdk/internal/misc/Blocker";

/// A pinned block of a mounted JDK virtual thread is about to hold its
/// carrier (`NativeContext::begin_carrier_compensation`): `Blocker.begin()`.
/// HotSpot 25 unmounts a virtual thread waiting in `Object.wait` and lets a
/// pinned park's monitor owner go; this VM keeps both mounted, so the
/// scheduler adds a carrier for the time of the block instead (JDK 21's
/// shape for `Object.wait`), and threads that would end the block are never
/// starved of one.
pub(crate) fn begin_carrier_compensation(ctx: &mut NativeContextImpl<'_>) -> bool {
    if !ctx.thread.jdk_continuation.jdk() || ctx.thread.jdk_continuation.carrier() == 0 {
        return false;
    }
    matches!(
        super::vm_exec::invoke_static_shared(ctx.shared, ctx.thread, BLOCKER, "begin", "()Z", &[]),
        Ok(Some(Value::Int(v))) if v != 0
    )
}

/// `Blocker.end(attempted)` after a block [`begin_carrier_compensation`] began.
pub(crate) fn end_carrier_compensation(ctx: &mut NativeContextImpl<'_>, attempted: bool) {
    if attempted {
        let _ = super::vm_exec::invoke_static_shared(
            ctx.shared,
            ctx.thread,
            BLOCKER,
            "end",
            "(Z)V",
            &[Value::Int(1)],
        );
    }
}

/// The virtual-thread JVMTI transitions of `VirtualThread.notifyJvmti*`, in
/// HotSpot's order (`JvmtiVTMSTransitionDisabler::VTMS_vthread_start` /
/// `_end` / `_mount` / `_unmount`): a start posts `VirtualThreadStart` then
/// `VirtualThreadMount`, an end `VirtualThreadUnmount` then
/// `VirtualThreadEnd`; a mount's end posts `VirtualThreadMount`, an
/// unmount's beginning `VirtualThreadUnmount`.
#[cfg_attr(not(feature = "experimental-debug"), allow(unused_variables))]
pub(crate) fn post_transition(ctx: &mut NativeContextImpl<'_>, kind: u8) {
    #[cfg(feature = "experimental-debug")]
    {
        let shared = ctx.shared;
        if !shared
            .debug
            .debugger_gates
            .native_event_armed(crate::debug::NativeEvent::VirtualThread)
        {
            return;
        }
        use crate::jvmti::native_env::{post_virtual_thread, VirtualThreadEvent};
        let shared_arc = shared;
        let tid = ctx.thread.thread_id.0;
        // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
        // item 1c): the agents' callbacks get this context's own handle, not
        // the registry's address of the thread whose `&mut` it holds.
        crate::runtime::interpreter::post_on_own_thread(ctx.thread, || match kind {
            0 => {
                post_virtual_thread(&shared_arc, tid, VirtualThreadEvent::Start);
                post_virtual_thread(&shared_arc, tid, VirtualThreadEvent::Mount);
            }
            1 => {
                post_virtual_thread(&shared_arc, tid, VirtualThreadEvent::Unmount);
                post_virtual_thread(&shared_arc, tid, VirtualThreadEvent::End);
            }
            2 => post_virtual_thread(&shared_arc, tid, VirtualThreadEvent::Mount),
            3 => post_virtual_thread(&shared_arc, tid, VirtualThreadEvent::Unmount),
            _ => {}
        });
    }
}

/// `Thread.getState()`'s run-state code (`NativeContext::thread_run_state`:
/// `0` NEW, `1` RUNNABLE, `2` TERMINATED, `3` WAITING, `4` BLOCKED, `5`
/// TIMED_WAITING) of `thread_obj` when it is a JDK-driven
/// `java.lang.VirtualThread`: its `state` field, mapped as JDK 25's
/// `VirtualThread.threadState()` maps it -- a RUNNING (mounted) one answers
/// its carrier's state, a STARTED one not yet in a container NEW. `None` for
/// any other thread.
pub(crate) fn virtual_thread_run_state(
    ctx: &NativeContextImpl<'_>,
    thread_obj: ObjectRef,
) -> Option<u8> {
    let shared = ctx.shared;
    if !shared.threads.virtual_thread_manager.jdk_continuations() {
        return None;
    }
    let class_id = shared.mem.heap.class_id_of(thread_obj);
    let is_vt = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .is_some_and(|c| &*c.name == "java/lang/VirtualThread");
    if !is_vt {
        return None;
    }
    let Value::Int(state) = ctx.get_field_by_name(thread_obj, "state") else {
        return None;
    };
    const SUSPENDED: i32 = 1 << 8;
    Some(match state & !SUSPENDED {
        0 => 0,
        // STARTED: NEW until the container holds it.
        1 => match ctx.get_field_by_name(thread_obj, "container") {
            Value::Object(Some(_)) => 1,
            _ => 0,
        },
        // RUNNING: the carrier's state, asked by another thread.
        2 => {
            if ctx.thread.java_thread_obj != Some(thread_obj) {
                if let Value::Object(Some(carrier)) =
                    ctx.get_field_by_name(thread_obj, "carrierThread")
                {
                    return Some(match ctx.thread_run_state(carrier) {
                        // A carrier is alive while it carries.
                        0 | 2 => 1,
                        other => other,
                    });
                }
            }
            1
        }
        // PARKED, PINNED, WAIT
        4 | 5 | 16 => 3,
        // TIMED_PARKED, TIMED_PINNED, TIMED_WAIT
        7 | 8 | 18 => 5,
        // BLOCKING, BLOCKED
        12 | 13 => 4,
        // TERMINATED
        99 => 2,
        // UNPARKED, UNBLOCKED, YIELDED and the transitions (PARKING,
        // TIMED_PARKING, WAITING, TIMED_WAITING, YIELDING)
        _ => 1,
    })
}

/// gci i11/vt (virtual threads as GC roots review,
/// `docs/internal/gc/gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016.md`).
#[cfg(test)]
mod gci_i11vt_tests {
    use super::*;

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    /// The text of the item starting at `sig`, up to its column-0 `}`.
    fn body(src: &str, sig: &str) -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start..];
        let stop = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    fn at(text: &str, needle: &str, what: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
    }

    // `fake` addresses: never dereferenced, only pointer-map keys / values.
    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; a pointer-map key / value only.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Finding 1: the JDK continuation's yield retires its TLAB BEFORE the
    /// deposit raises `in_blocked_region` (no blocked thread keeps a TLAB).
    #[test]
    fn gci_i11vt_jdk_yield_retires_the_tlab_before_the_deposit() {
        let src = read("src/vm/jdk_continuation.rs");
        let slice = body(&src, "fn run_slice(");
        let arm = &slice[at(
            &slice,
            "VmError::ContinuationYield { .. })) = &result",
            "run_slice yield arm",
        )..];
        let retire = at(arm, "thread.tlab.retire();", "yield-arm retire");
        let deposit = at(arm, ".deposit_root_snapshot();", "yield-arm deposit");
        let handoff = at(arm, ".deposit_continuation(", "yield-arm hand-off");
        assert!(
            retire < deposit && deposit < handoff,
            "run_slice's yield arm must retire the TLAB before its deposit raises \
             in_blocked_region (retire@{retire} deposit@{deposit} handoff@{handoff})"
        );
    }

    /// Finding 1, the bound path (`CRATONVM_VT_CONTINUATIONS=0`).
    #[test]
    fn gci_i11vt_bound_yield_retires_the_tlab_before_the_deposit() {
        let src = read("src/vm/vm_exec.rs");
        let resume = body(&src, "fn resume_virtual_continuation(");
        let arm = &resume[at(
            &resume,
            "VmError::ContinuationYield { wake_after_nanos })) =",
            "resume_virtual_continuation yield arm",
        )..];
        let retire = at(arm, "thread.tlab.retire();", "bound yield-arm retire");
        let deposit = at(arm, ".deposit_root_snapshot();", "bound yield-arm deposit");
        let handoff = at(arm, ".suspend_runtime(", "bound yield-arm hand-off");
        assert!(
            retire < deposit && deposit < handoff,
            "resume_virtual_continuation's yield arm must retire the TLAB before its \
             deposit (retire@{retire} deposit@{deposit} handoff@{handoff})"
        );
    }

    /// Finding 2: a remount reads the registry's (current) mirror only after
    /// the drain applied the yield-keyed fixup to the yield-time one.
    #[test]
    fn gci_i11vt_jdk_remount_reads_the_mirror_after_the_drain() {
        let src = read("src/vm/jdk_continuation.rs");
        let slice = body(&src, "fn run_slice(");
        let drain = at(&slice, ".check_post_block_gc();", "remount drain");
        let mirror = at(&slice, "registry.java_thread_obj(tid)", "mirror refresh");
        assert!(
            drain < mirror,
            "run_slice must refresh java_thread_obj from the registry after \
             check_post_block_gc (drain@{drain} mirror@{mirror})"
        );
    }

    /// Finding 2's hazard, shown on the shared remap the drain uses: the
    /// blocked chain is keyed by YIELD-TIME addresses, so under a sliding
    /// compactor's `A -> B, B -> C` it forwards the yield-time mirror `A` to
    /// `B` and an already-current `B` on to `C` -- another root's object.
    #[test]
    fn gci_i11vt_the_blocked_chain_moves_a_current_address_twice() {
        let mut chain = cratonvm_types::PointerMap::default();
        chain.insert(0xA000, 0xB000);
        chain.insert(0xB000, 0xC000);
        let mut thread = JvmThread::new(ThreadId(7), "gci-i11vt");
        thread.java_thread_obj = Some(fake(0xA000));
        crate::memory::gc::remap_thread_single_slots(&mut thread, &chain);
        assert_eq!(
            thread.java_thread_obj.map(|o| o.as_ptr() as usize),
            Some(0xB000),
            "the yield-time mirror is forwarded once"
        );
        thread.java_thread_obj = Some(fake(0xB000));
        crate::memory::gc::remap_thread_single_slots(&mut thread, &chain);
        assert_eq!(
            thread.java_thread_obj.map(|o| o.as_ptr() as usize),
            Some(0xC000),
            "a current address fed to the chain lands on another root (why the \
             refresh follows the drain)"
        );
    }

    /// Finding 3: a debugger reads an unmounted continuation's reference
    /// locals through its yield deposit's `slot_origins`, never the raw
    /// (yield-time) slot.
    #[test]
    fn gci_i11vt_unmounted_frame_readers_go_through_slot_origins() {
        let src = read("src/runtime/interpreter.rs");
        let work = body(&src, "pub(crate) fn with_unmounted_frames<R>(");
        assert!(
            work.contains("state: Some(&*thread.gc_block_state)") && work.contains("unmounted: true"),
            "with_unmounted_frames must hand current_reference the continuation's GcBlockState"
        );
        assert!(
            !work.contains("state: None"),
            "with_unmounted_frames must not read raw slots"
        );
        let publish = body(&src, "fn publish_unmounted_frames(");
        assert!(
            publish.contains(".slot_origins") && publish.contains("is_object_address(cur)"),
            "publish_unmounted_frames must publish reference locals through slot_origins"
        );
        let unmounted = body(&src, "impl BlockedFrames<'_> {");
        assert!(
            unmounted.contains("self.unmounted\n"),
            "BlockedFrames::unmounted must read its own flag, not state.is_none()"
        );
    }

    /// Item 4: both ends of a continuation (its slice's end and the no-runner
    /// failure of a first slice) go through `end_manager_record`, which reaps
    /// only under the per-VM switch and never while JDWP is configured; VM
    /// init reads the switch once, into the manager.
    #[test]
    fn gci_i11vt_continuation_ends_reap_through_the_per_vm_switch() {
        let src = read("src/vm/jdk_continuation.rs");
        let end = body(&src, "fn end_manager_record(");
        let terminate = at(&end, "manager.terminate(tid.0);", "terminate");
        let gate = at(&end, "manager.reap_terminated()", "per-VM switch");
        let forget = at(&end, "manager.forget_terminated(tid.0);", "reap");
        assert!(terminate < gate && gate < forget);
        assert!(end.contains("debugger_observes_locals(shared)"));
        let slice = body(&src, "fn run_slice(");
        assert!(
            slice.contains("end_manager_record(shared, tid);")
                && !slice.contains("virtual_thread_manager.terminate("),
            "run_slice's end must go through end_manager_record"
        );
        let init = read("src/vm/vm_init.rs");
        assert!(
            init.contains(".with_reap_terminated(cratonvm_types::flags::runtime_flag_default_on("),
            "VM init reads CRATONVM_VT_REAP_TERMINATED once, default ON since 2026-10-17"
        );
    }

    /// Item 8: a first slice that found no runner is retired (registry entry
    /// dead, addresses withdrawn between pauses, manager record ended) and
    /// dropped -- never deposited as a zombie.
    #[test]
    fn gci_i11vt_a_never_mounted_first_slice_is_retired_not_deposited() {
        let src = read("src/vm/jdk_continuation.rs");
        let runner = body(&src, "fn run_on_runner(");
        let arm = &runner[at(&runner, "Err(message) => {", "no-runner arm")..];
        let first = at(arm, "if first {", "first-slice branch");
        let census = at(arm, "run_if_no_stw_requested(", "census-serialized retire");
        let withdraw = at(arm, "registry.clear_tlab_addr(tid);", "address withdrawal");
        let dead = at(arm, "registry.mark_dead(tid);", "mark_dead");
        let ended = at(arm, "end_manager_record(shared, tid);", "manager record");
        let dropped = at(arm, "drop(thread);", "drop");
        let deposit = at(arm, ".deposit_continuation(", "resumed-slice deposit");
        assert!(
            first < census
                && census < withdraw
                && withdraw < dead
                && dead < ended
                && ended < dropped
                && dropped < deposit,
            "the first-slice branch retires and returns before the deposit"
        );
    }

    /// Item 9: `Thread.print` (`thread_snapshots`) leaves JDK continuations'
    /// entries out, as HotSpot 25 lists only platform threads there.
    #[test]
    fn gci_i11vt_thread_print_leaves_jdk_continuations_out() {
        let init = read("src/vm/vm_init.rs");
        let snapshots = body(&init, "    fn thread_snapshots(&self)");
        assert!(
            snapshots.contains("vtm.is_jdk_continuation(tid.0)")
                && snapshots.contains(".filter(|e| e.alive && !continuation_entry(e.thread_id))"),
            "thread_snapshots must filter JDK continuation entries"
        );
    }

    /// Item 10: the one check both interpreter->compiled entries pass, before
    /// they install the JIT thread; and what it asks.
    #[test]
    fn gci_i11vt_compiled_entries_refuse_a_continuation() {
        let bridge = read("src/runtime/interpreter/jit_bridge.rs");
        for door in ["\npub(super) fn run_jit_body_raw(", "\npub(super) fn try_osr("] {
            let fun = body(&bridge, door);
            let check = at(&fun, "assert_no_compiled_continuation(thread);", door);
            let enter = at(&fun, "crate::jit::helpers::set_jit_thread(thread)", door);
            assert!(check < enter, "{door}: the check precedes the entry");
        }
        let mut thread = JvmThread::new(ThreadId(11), "gci-i11vt-kind");
        assert!(!crate::runtime::interpreter::jit_bridge::thread_is_continuation(&thread));
        thread.kind = crate::threading::ThreadKind::Virtual;
        assert!(crate::runtime::interpreter::jit_bridge::thread_is_continuation(&thread));
    }
}

#[cfg(test)]
mod vthread_compiled_leaf_tests {
    //! Interpreter round i4 wave 92, lane L6: the leaf test of
    //! `CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY` (no switch is read here).
    use super::{bytecode_cannot_reach_a_yield, VTHREAD_LEAF_MAX_CODE_BYTES};

    #[test]
    fn an_array_sum_loop_is_a_leaf() {
        // static long sum(int[] a) { long s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }
        let code = [
            0x09, 0x40, 0x03, 0x36, 0x04, 0x15, 0x04, 0x2a, 0xbe, 0xa2, 0x00, 0x11, 0x1f, 0x2a,
            0x15, 0x04, 0x2e, 0x85, 0x61, 0x40, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xee, 0x1f, 0xad,
        ];
        assert!(bytecode_cannot_reach_a_yield(&code));
    }

    #[test]
    fn any_invoke_or_monitor_op_is_not_a_leaf() {
        for op in [0xb6u8, 0xb7, 0xb8] {
            assert!(!bytecode_cannot_reach_a_yield(&[0x2a, op, 0x00, 0x01, 0xb1]));
        }
        assert!(!bytecode_cannot_reach_a_yield(&[0x2a, 0xb9, 0x00, 0x01, 0x01, 0x00, 0xb1]));
        assert!(!bytecode_cannot_reach_a_yield(&[0xba, 0x00, 0x01, 0x00, 0x00, 0xb0]));
        assert!(!bytecode_cannot_reach_a_yield(&[0x2a, 0xc2, 0x2a, 0xc3, 0xb1]));
        // An invoke hidden behind a field access and a branch still counts.
        assert!(!bytecode_cannot_reach_a_yield(&[
            0xb2, 0x00, 0x02, 0x99, 0x00, 0x06, 0xb8, 0x00, 0x03, 0xb1
        ]));
    }

    #[test]
    fn switches_and_wide_forms_are_decoded() {
        // iload_0; tableswitch (pad 2) default low=0 high=1 two offsets; return
        let mut table = vec![0x1a, 0xaa, 0x00, 0x00];
        for word in [0i32, 0, 1, 0, 0] {
            table.extend_from_slice(&word.to_be_bytes());
        }
        table.push(0xb1);
        assert!(bytecode_cannot_reach_a_yield(&table));
        // The same with an invoke after the switch.
        let mut tail = table.clone();
        tail.pop();
        tail.extend_from_slice(&[0xb8, 0x00, 0x01, 0xb1]);
        assert!(!bytecode_cannot_reach_a_yield(&tail));
        // lookupswitch at pc 0 (pad 3), npairs 1.
        let mut lookup = vec![0xab, 0x00, 0x00, 0x00];
        for word in [0i32, 1, 7, 0] {
            lookup.extend_from_slice(&word.to_be_bytes());
        }
        lookup.push(0xb1);
        assert!(bytecode_cannot_reach_a_yield(&lookup));
        // wide iinc, wide iload.
        assert!(bytecode_cannot_reach_a_yield(&[
            0xc4, 0x84, 0x01, 0x00, 0x00, 0x01, 0xc4, 0x15, 0x01, 0x00, 0xac
        ]));
    }

    #[test]
    fn malformed_code_is_not_a_leaf() {
        assert!(!bytecode_cannot_reach_a_yield(&[]));
        // A truncated sipush, a breakpoint, an impdep, a bad wide.
        assert!(!bytecode_cannot_reach_a_yield(&[0x11, 0x00]));
        assert!(!bytecode_cannot_reach_a_yield(&[0xca, 0xb1]));
        assert!(!bytecode_cannot_reach_a_yield(&[0xfe, 0xb1]));
        assert!(!bytecode_cannot_reach_a_yield(&[0xc4, 0x60, 0xb1]));
        // A tableswitch with high < low.
        let mut table = vec![0x1a, 0xaa, 0x00, 0x00];
        for word in [0i32, 2, 1] {
            table.extend_from_slice(&word.to_be_bytes());
        }
        table.push(0xb1);
        assert!(!bytecode_cannot_reach_a_yield(&table));
    }

    #[test]
    fn the_scan_bound_covers_a_short_loop() {
        // `sum` above is 28 bytes; a body at the bound is still scanned.
        let mut code = vec![0x00u8; VTHREAD_LEAF_MAX_CODE_BYTES - 1];
        code.push(0xb1);
        assert!(bytecode_cannot_reach_a_yield(&code));
    }

    /// Interpreter round i4 wave 93, lane L6
    /// (`CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS`): the walk hands each
    /// `invokestatic` (pc, constant-pool index) to the caller and still
    /// refuses every other invoke and monitor op.
    #[test]
    fn the_caller_walk_hands_over_invokestatic_only() {
        use super::scan_for_yield_points;
        // iload_0; invokestatic #3; iload_0; invokestatic #0x0104; iadd; ireturn
        let code = [0x1a, 0xb8, 0x00, 0x03, 0x1a, 0xb8, 0x01, 0x04, 0x60, 0xac];
        let mut seen = Vec::new();
        assert!(scan_for_yield_points(&code, &mut |pc, cp| {
            seen.push((pc, cp));
            true
        }));
        assert_eq!(seen, vec![(1, 3), (5, 0x0104)]);
        // The callback's "no" ends the walk with "no".
        let mut calls = 0;
        assert!(!scan_for_yield_points(&code, &mut |_, _| {
            calls += 1;
            false
        }));
        assert_eq!(calls, 1);
        // Every other invoke and the monitor ops are refused without asking.
        for op in [0xb6u8, 0xb7] {
            assert!(!scan_for_yield_points(&[0x2a, op, 0x00, 0x01, 0xb1], &mut |_, _| true));
        }
        assert!(!scan_for_yield_points(
            &[0x2a, 0xb9, 0x00, 0x01, 0x01, 0x00, 0xb1],
            &mut |_, _| true
        ));
        assert!(!scan_for_yield_points(&[0xba, 0x00, 0x01, 0x00, 0x00, 0xb0], &mut |_, _| true));
        assert!(!scan_for_yield_points(&[0x2a, 0xc2, 0x2a, 0xc3, 0xb1], &mut |_, _| true));
        // A truncated invokestatic.
        assert!(!scan_for_yield_points(&[0xb8, 0x00], &mut |_, _| true));
    }
}

/// gci i20/st (`gci-i19st-g1-virtual-thread-churn-stop-the-world-take-over-stalls-FIXED-20261017`):
/// `enterSpecial`'s two failure exits keep the carrier and the continuation
/// consistent.
#[cfg(test)]
mod gci_i20st_tests {
    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn body(src: &str, sig: &str) -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start..];
        let stop = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    fn at(text: &str, needle: &str, what: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
    }

    /// A `false` call that finds a state with frames resumes it (not a first
    /// slice), and only a frameless first slice posts the start events.
    #[test]
    fn gci_i20st_a_yielded_state_with_no_tail_is_resumed_not_restarted() {
        let src = read("src/vm/jdk_continuation.rs");
        let enter = body(&src, "pub(crate) fn enter_special(");
        let check = at(&enter, "if thread.frames.is_empty() {", "frames check");
        let post = at(&enter, "post_thread_started(shared, thread.thread_id", "start events");
        let heal = at(&enter, "resume_untailed = true;", "resume flag");
        assert!(check < post && post < heal, "the start events belong to the frameless arm only");
        assert!(
            enter.contains("first: !is_continue && !resume_untailed,"),
            "a resumed untailed state must not run as a first slice"
        );
    }

    /// The first yield's failed `StackChunk` does not escape as a throwable,
    /// and a failed slice reaches Java as a `java.lang.InternalError`.
    #[test]
    fn gci_i20st_failure_exits_reach_java_as_throwables() {
        let src = read("src/vm/jdk_continuation.rs");
        let enter = body(&src, "pub(crate) fn enter_special(");
        let yielded = at(&enter, "SliceOutcome::Yielded => match mark_tail(", "yield arm");
        let swallow = at(&enter, "Err(MethodCallFailed::ExceptionThrown(_)) => Ok(()),", "chunk OOM");
        assert!(yielded < swallow);
        assert!(enter.contains("SliceOutcome::Failed(message) => Err(slice_failure_as_java(ctx, &message)),"));
        let conv = body(&src, "fn slice_failure_as_java(");
        assert!(conv.contains("\"java/lang/InternalError\""));
        assert!(conv.contains("MethodCallFailed::ExceptionThrown(exc)"));
    }
}

/// gci i21/rv (`gci-i21rv-i20-default-path-review-findings-FIXED-20261017` findings
/// 1-3): every `enterSpecial` failure after the carrier mounted reaches Java
/// in a form that runs `Continuation.run`'s `finally` (or not at all, for a
/// yield that happened).
#[cfg(test)]
mod gci_i21rv_tests {
    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn body(src: &str, sig: &str) -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start..];
        let stop = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// Finding 1: the `StackChunk` allocation reports heap exhaustion as
    /// `InternalError(VmError::Runtime(OutOfMemoryError))`, not as a thrown
    /// exception, so the yield arm swallows the `InternalError` shape too and
    /// propagates nothing.
    #[test]
    fn gci_i21rv_the_yield_arm_swallows_every_tail_failure() {
        let src = read("src/vm/jdk_continuation.rs");
        let enter = body(&src, "pub(crate) fn enter_special(");
        let arm_at = enter
            .find("SliceOutcome::Yielded => match mark_tail(")
            .expect("the yield arm");
        let arm = &enter[arm_at..];
        let arm = &arm[..arm.find("SliceOutcome::Done").expect("the next arm")];
        assert!(arm.contains("Err(MethodCallFailed::ExceptionThrown(_)) => Ok(()),"));
        assert!(arm.contains("Err(MethodCallFailed::InternalError(_)) => Ok(()),"));
        assert!(!arm.contains("other => other"), "no failure of the tail write escapes");
        // The allocation door's failure shape the arm must catch.
        let exec = read("src/vm/vm_exec.rs");
        let alloc_at = exec
            .find("    fn new_object_initialized(\n")
            .expect("NativeContextImpl::new_object_initialized");
        let alloc = &exec[alloc_at..];
        let alloc = &alloc[..alloc.find("\n    \u{7d}\n").expect("its end")];
        assert!(alloc.contains("MethodCallFailed::InternalError(VmError::Runtime("));
        assert!(alloc.contains("RuntimeError::OutOfMemoryError \u{7b}"));
    }

    /// Finding 2: an `OutOfMemoryError` has a Java form, so building the
    /// `InternalError` under heap pressure falls back to it, not to the raw
    /// internal error.
    #[test]
    fn gci_i21rv_a_failed_throwable_falls_back_to_its_java_form() {
        let oome = crate::error::RuntimeError::OutOfMemoryError {
            message: "Java heap space".to_string(),
        };
        assert!(oome.as_java_throwable().is_some());
        let src = read("src/vm/jdk_continuation.rs");
        let conv = body(&src, "fn slice_failure_as_java(");
        let thrown = conv
            .find("Err(thrown @ MethodCallFailed::ExceptionThrown(_)) => thrown,")
            .expect("a thrown error is kept");
        let runtime = conv
            .find("if error.as_java_throwable().is_some() =>")
            .expect("a Java-convertible runtime error is kept");
        let raw = conv.find("Err(_) => internal(message),").expect("the raw fallback");
        assert!(thrown < raw && runtime < raw, "the raw fallback is the last arm");
    }

    /// Finding 4: a failed slice whose thread the VM tore down marks the
    /// continuation `done` (before the throwable is built); one whose state
    /// was deposited back does not.
    #[test]
    fn gci_i21rv_an_ended_failed_slice_marks_the_continuation_done() {
        let src = read("src/vm/jdk_continuation.rs");
        let enter = body(&src, "pub(crate) fn enter_special(");
        let arm = enter
            .find("SliceOutcome::FailedEnded(message) => ")
            .expect("the ended arm");
        let arm = &enter[arm..];
        let done = arm
            .find("ctx.set_field_by_name(cont, \"done\", Value::Int(1));")
            .expect("done is set");
        let conv = arm
            .find("Err(slice_failure_as_java(ctx, &message))")
            .expect("then thrown");
        assert!(done < conv, "done before the allocation that may move `cont`");
        let deposited_arm = enter
            .find("SliceOutcome::Failed(message) => Err(slice_failure_as_java(ctx, &message)),")
            .expect("the deposited arm");
        assert!(!enter[deposited_arm..]
            .split("SliceOutcome::FailedEnded")
            .next()
            .expect("the deposited arm's text")
            .contains("\"done\""));
        let slice = body(&src, "fn run_slice(");
        assert!(slice.contains("Err(e) => SliceOutcome::FailedEnded(format!("));
        let runner = body(&src, "fn run_on_runner(");
        let retired = runner.find("end_manager_record(shared, tid);").expect("the retire");
        let ended = runner
            .find("return SliceOutcome::FailedEnded(message);")
            .expect("ended");
        let deposit = runner.find(".deposit_continuation(tid.0, thread);").expect("deposit");
        let failed = runner.find("return SliceOutcome::Failed(message);").expect("deposited");
        assert!(retired < ended && ended < deposit && deposit < failed);
    }

    /// Finding 3: the two other post-mount exits are Java throwables too.
    #[test]
    fn gci_i21rv_no_post_mount_exit_is_a_raw_internal_error() {
        let src = read("src/vm/jdk_continuation.rs");
        let enter = body(&src, "pub(crate) fn enter_special(");
        for message in [
            "\"Continuation.enterSpecial: the continuation has no frames\"",
            "\"Continuation.enterSpecial: the throwable was not kept\"",
        ] {
            let at = enter.find(message).unwrap_or_else(|| panic!("{message}"));
            let before = &enter[..at];
            let conv = before.rfind("slice_failure_as_java(").expect("converted");
            let raw = before.rfind("internal(").unwrap_or(0);
            assert!(conv > raw, "{message} goes through slice_failure_as_java");
        }
    }
}

/// gci i24/vl (`gci-i24vl-g1-virtual-thread-lost-when-after-yield-throws-oome-FIXED-20261017`):
/// a timed park's yield is refused, and the slice runs on with `doYield`
/// answering a pin, when the carrier's post-yield probe finds no heap.
#[cfg(test)]
mod gci_i24vl_tests {
    use super::*;

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn body(src: &str, sig: &str) -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start..];
        let stop = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    fn at(text: &str, needle: &str, what: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
    }

    /// Only JDK 25's TIMED_PARKING is probed: PARKING (3) allocates nothing
    /// after the yield, YIELDING (10) / BLOCKING (12) submit with OOME retry.
    #[test]
    fn gci_i24vl_only_a_timed_parking_yield_is_probed() {
        assert_eq!(VTHREAD_TIMED_PARKING, 6);
        assert!(yield_needs_post_yield_probe(&Value::Int(6)));
        for other in [0, 1, 2, 3, 4, 5, 7, 9, 10, 11, 12, 99, 6 | 256] {
            assert!(
                !yield_needs_post_yield_probe(&Value::Int(other)),
                "state {other} must not be probed"
            );
        }
        assert!(!yield_needs_post_yield_probe(&Value::Object(None)));
        // A pin every JDK's `Continuation.pinnedReason` accepts (2..=4).
        assert!((PINNED_CRITICAL_SECTION..=PINNED_MONITOR).contains(&YIELD_REFUSED_PIN_REASON));
        assert_ne!(YIELD_REFUSED_PIN_REASON, 0, "0 is \"yielded\"");
    }

    /// The runner's half: a refused remount leaves `VtYield::Park`, so
    /// `remount_action` pushes no `0` on top of the pin; an ordinary one is
    /// untouched.
    #[test]
    fn gci_i24vl_a_refused_remount_skips_the_yield_result() {
        let mut thread = JvmThread::new(ThreadId(24), "gci-i24vl");
        thread.vt_yield = VtYield::DoYield;
        assert!(refuse_deposited_yield(&mut thread, false).is_ok());
        assert!(matches!(thread.vt_yield, VtYield::DoYield), "not refused: unchanged");
        assert!(refuse_deposited_yield(&mut thread, true).is_ok());
        assert!(matches!(thread.vt_yield, VtYield::Park));
        let interp = read("src/runtime/interpreter.rs");
        let remount = body(&interp, "fn remount_action(");
        assert!(
            remount.contains("VtYield::Park => return RemountAction::Run,"),
            "a Park record must resume without a push"
        );
        assert!(remount.contains("leaf.stack.push(Value::Int(0))"));
    }

    /// The carrier probes after the slice's wait and before `cont` is re-read
    /// and its root dropped; a refused yield re-runs as a resumed slice.
    #[test]
    fn gci_i24vl_the_carrier_probes_between_the_wait_and_the_outcome() {
        let src = read("src/vm/jdk_continuation.rs");
        let enter = body(&src, "pub(crate) fn enter_special(");
        let wait = at(&enter, "ctx.end_blocking_region();", "end of the wait");
        let refuse = at(
            &enter,
            "refuse_yield_without_headroom(ctx, &outcome, vt_tid, carrier)",
            "the probe",
        );
        let hosted = at(&enter, "runners.hosted.lock().remove(&carrier.0);", "hosted");
        let reread = at(&enter, "let cont = ctx.resolve_global_root(cont_root)", "cont re-read");
        let dropped = at(&enter, "ctx.remove_global_root(cont_root);", "root dropped");
        let arm = at(&enter, "SliceOutcome::Yielded => match mark_tail(", "yield arm");
        assert!(wait < refuse && refuse < hosted && hosted < reread && reread < arm);
        assert!(refuse < enter.rfind("ctx.remove_global_root(cont_root);").unwrap_or(0));
        assert!(dropped > 0);
        let rerun = &enter[refuse..hosted];
        assert!(rerun.contains("first: false,") && rerun.contains("refuse_yield: true,"));
        assert!(rerun.contains("None => break outcome,"));
        // The probe is the collecting ladder, then the take-back.
        let probe = body(&src, "fn refuse_yield_without_headroom(");
        let state = at(&probe, "yield_needs_post_yield_probe(", "state test");
        let alloc = at(
            &probe,
            "crate::runtime::interpreter::gc_alloc_array_unsampled(",
            "probe",
        );
        let take = at(&probe, ".take_runtime_for_mount(vt_tid.0, carrier.0)", "take-back");
        assert!(state < alloc && alloc < take);
        assert!(!probe.contains("new_array("), "not the no-GC native door");
        assert!(probe.contains("if !matches!(outcome, SliceOutcome::Yielded)"));
        // The runner applies it after the drain, before the frames run.
        let slice = body(&src, "fn run_slice(");
        let drain = at(&slice, ".check_post_block_gc();", "drain");
        let apply = at(&slice, "refuse_deposited_yield(&mut thread, refuse_yield)", "apply");
        let resume = at(&slice, "resume_continuation(shared, &mut thread)", "resume");
        assert!(drain < apply && apply < resume);
    }
}

/// gci i25/vt (`gci-i24vl-g1-virtual-thread-lost-when-after-yield-throws-oome-FIXED-20261017`
/// residuals 1 and 2): a virtual thread whose `afterYield()` died before it
/// scheduled the timeout task (TIMED_PARKING, or TIMED_WAITING after a timed
/// `Object.wait`) is re-queued through the JDK's unblocker once its carrier is
/// provably past that `afterYield`.
#[cfg(test)]
mod gci_i25vt_tests {
    use super::*;
    use crate::threading::jvm_thread::JdkContinuationState;

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    /// The end of a free function / of a method.
    const FN_END: &str = "\n\u{7d}\n";
    const METHOD_END: &str = "\n    \u{7d}\n";

    /// From `sig` to the end of the item (`end`: [`FN_END`] or
    /// [`METHOD_END`]).
    fn item(src: &str, sig: &str, end: &str) -> String {
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

    /// Exactly the two JDK 25 states whose `afterYield()` allocates with no
    /// OOME handling open a window; BLOCKED is what an orphan is moved to.
    #[test]
    fn gci_i25vt_only_the_timed_states_open_a_window() {
        assert_eq!(VTHREAD_TIMED_PARKING, 6);
        assert_eq!(VTHREAD_TIMED_WAITING, 17);
        assert_eq!(VTHREAD_BLOCKED, 13);
        assert!(after_yield_allocates(&Value::Int(6)));
        assert!(after_yield_allocates(&Value::Int(17)));
        // PARKING, WAITING, BLOCKING, YIELDING, the settled states, and a
        // SUSPENDED bit.
        for other in [0, 1, 2, 3, 4, 5, 7, 9, 10, 11, 12, 13, 14, 15, 16, 18, 99, 6 | 256, 17 | 256] {
            assert!(
                !after_yield_allocates(&Value::Int(other)),
                "state {other} must not open a window"
            );
        }
        assert!(!after_yield_allocates(&Value::Object(None)));
    }

    /// The window bit shares the carrier word: setting either keeps the
    /// other and the JDK bit.
    #[test]
    fn gci_i25vt_the_window_bit_keeps_the_carrier_and_jdk_bits() {
        let mut state = JdkContinuationState::default();
        assert!(!state.post_yield_window());
        state.set_carrier(42);
        state.set_post_yield_window(true);
        assert!(state.post_yield_window());
        assert_eq!(state.carrier(), 42, "the flag is not part of the carrier id");
        state.set_carrier(0);
        assert!(state.post_yield_window(), "set_carrier keeps the flag");
        state.set_jdk();
        state.set_carrier(7);
        state.set_post_yield_window(false);
        assert!(state.jdk() && !state.post_yield_window());
        assert_eq!(state.carrier(), 7);
    }

    /// Orphans join the monitors' list once each, after it.
    #[test]
    fn gci_i25vt_orphans_join_the_unblock_list_once() {
        let mut ids = vec![ThreadId(5), ThreadId(9)];
        merge_unblock_ids(&mut ids, vec![9, 11, 11, 3]);
        assert_eq!(ids, vec![ThreadId(5), ThreadId(9), ThreadId(11), ThreadId(3)]);
        let mut none = Vec::new();
        merge_unblock_ids(&mut none, Vec::new());
        assert!(none.is_empty());
    }

    /// The carrier side: the window is resolved first thing in
    /// `enterSpecial`, opened only after the refusal loop (a yield that
    /// stands) and before the carrier stops hosting; the resolution tests
    /// "not mounted since" and moves the state under the record lock, then
    /// queues the orphan.
    #[test]
    fn gci_i25vt_enter_special_resolves_then_opens_the_window() {
        let src = read("src/vm/jdk_continuation.rs");
        let enter = item(&src, "pub(crate) fn enter_special(", FN_END);
        let resolve = at(&enter, "resolve_post_yield_window(ctx);", "resolve");
        let arc = at(&enter, "shared.try_get_arc()", "first VM work");
        assert!(resolve < arc, "resolved before anything else");
        let refuse = at(&enter, "refuse_yield_without_headroom(ctx, &outcome, vt_tid, carrier)", "loop");
        let open = at(&enter, "open_post_yield_window(ctx, vt_tid, carrier);", "open");
        let hosted = at(&enter, "runners.hosted.lock().remove(&carrier.0);", "hosted");
        assert!(refuse < open && open < hosted);
        assert!(enter[..open]
            .contains("if matches!(outcome, SliceOutcome::Yielded | SliceOutcome::Failed(_))"));

        let open_fn = item(&src, "fn open_post_yield_window(", FN_END);
        let state = at(&open_fn, "after_yield_allocates(", "state test");
        let count = at(&open_fn, ".deposited_mount_count(vt_tid.0)", "mount count");
        let insert = at(&open_fn, ".insert(carrier.0, (vt_tid.0, mounts));", "window");
        let flag = at(&open_fn, ".set_post_yield_window(true);", "flag");
        assert!(state < count && count < insert && insert < flag);

        let res = item(&src, "pub(crate) fn resolve_post_yield_window(", FN_END);
        let gate = at(&res, "if !ctx.thread.jdk_continuation.post_yield_window()", "gate");
        let take = at(&res, ".remove(&ctx.thread.thread_id.0);", "take the window");
        let locked = at(&res, ".with_deposit_unmounted_since(vt, mounts, ||", "locked test");
        let cas = at(&res, "super::vm_exec::compare_and_swap_field_shared(", "CAS");
        let queued = at(&res, "runners.post_yield_orphans.lock().push(vt);", "queue");
        assert!(gate < take && take < locked && locked < cas && cas < queued);
        assert!(res.contains("Value::Int(VTHREAD_BLOCKED)"));
        assert!(!res.contains("gc_alloc"), "the resolution allocates nothing");
    }

    /// The other two seams: a carrier's Java-level park resolves before it
    /// waits, and the unblocker hands the orphans to the JDK in both
    /// `CRATONVM_VTHREAD_MONITOR_UNMOUNT` arms.
    #[test]
    fn gci_i25vt_park_and_unblocker_seams() {
        let exec = read("src/vm/vm_exec.rs");
        let park = item(&exec, "    fn park_with_state_until(", METHOD_END);
        let hook = at(
            &park,
            "if timed && self.thread.jdk_continuation.post_yield_window()",
            "park hook",
        );
        let call = at(
            &park,
            "super::jdk_continuation::resolve_post_yield_window_at_park(self);",
            "call",
        );
        let span = at(&park, "ParkEventSpan::begin_until(", "first park work");
        assert!(hook < call && call < span, "resolved before the park does anything");

        let take = item(&exec, "    fn take_virtual_thread_list_to_unblock(", METHOD_END);
        let off = at(&take, "std::thread::sleep(std::time::Duration::from_millis(50));", "off arm");
        let on = at(&take, "monitors.take_vthread_unblock_list(", "on arm");
        let merge = at(
            &take,
            "super::jdk_continuation::with_post_yield_orphans(self.shared, ids);",
            "merge",
        );
        let empty = at(&take, "if ids.is_empty()", "empty test");
        assert!(off < merge && on < merge && merge < empty, "both arms reach the merge");
        assert!(!take[..merge].contains("return None;"), "no arm returns before the merge");
    }
}

/// gci i26/rv (`gci-i26rv-i25-default-path-review-findings-FIXED-20261016` item
/// V1): the park seam of the post-yield window keeps the window while the
/// carrier is still inside the `afterYield()` whose `schedule(..)` parked.
#[cfg(test)]
mod gci_i26rv_tests {
    use super::*;

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn at(text: &str, needle: &str, what: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
    }

    #[test]
    fn gci_i26rv_a_park_inside_run_continuation_is_not_past_after_yield() {
        // The carrier parks in `schedule(..)` called by `afterYield`.
        assert!(frames_are_inside_run_continuation([
            ("java/util/concurrent/ForkJoinWorkerThread", "run"),
            ("java/lang/VirtualThread", "runContinuation"),
            ("java/lang/VirtualThread", "afterYield"),
            ("java/util/concurrent/ForkJoinPool", "spinLockRunState"),
            ("java/util/concurrent/locks/LockSupport", "parkNanos"),
        ]));
        // An inlined `afterYield` alone is enough.
        assert!(frames_are_inside_run_continuation([(
            "java/lang/VirtualThread",
            "afterYield"
        )]));
        // `awaitWork`: the task returned, the window may be resolved.
        assert!(!frames_are_inside_run_continuation([
            ("java/util/concurrent/ForkJoinWorkerThread", "run"),
            ("java/util/concurrent/ForkJoinPool", "runWorker"),
            ("java/util/concurrent/ForkJoinPool", "awaitWork"),
            ("java/util/concurrent/locks/LockSupport", "park"),
        ]));
        // Another class's method of the same name does not count.
        assert!(!frames_are_inside_run_continuation([(
            "com/example/VirtualThread",
            "runContinuation"
        )]));
        assert!(!frames_are_inside_run_continuation(std::iter::empty()));
    }

    #[test]
    fn gci_i26rv_the_park_seam_tests_the_stack_before_resolving() {
        let src = read("src/vm/jdk_continuation.rs");
        let start = at(
            &src,
            "pub(crate) fn resolve_post_yield_window_at_park(",
            "park seam",
        );
        let body = &src[start..];
        let body = &body[..at(body, "\n\u{7d}\n", "end of the park seam")];
        let gate = at(body, "post_yield_window()", "bit gate");
        let peek = at(body, ".get(&ctx.thread.thread_id.0)", "peek, not remove");
        let state = at(body, "after_yield_allocates(", "state filter");
        let stack = at(body, "carrier_inside_run_continuation(ctx)", "stack test");
        let resolve = at(body, "resolve_post_yield_window(ctx);", "resolution");
        assert!(gate < peek && peek < state && state < stack && stack < resolve);
        assert!(!body.contains(".remove("), "the park seam never takes the window itself");
        let exec = read("src/vm/vm_exec.rs");
        assert!(exec.contains("super::jdk_continuation::resolve_post_yield_window_at_park(self);"));
        assert!(!exec.contains("super::jdk_continuation::resolve_post_yield_window(self);"));
    }
}

/// gci i30/gv (`gci-i21rv-i20-default-path-review-findings-FIXED-20261017` finding
/// 5): a carrier that finds no runner waits for one (bounded backoff) before
/// it fails the slice, so a resumed virtual thread is not stranded RUNNING.
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i30gv`.
#[cfg(test)]
mod gci_i30gv_tests {
    use super::*;

    fn read(rel: &str) -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    }

    fn body(src: &str, sig: &str) -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start..];
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// The slice door retries the runner before either failure arm.
    #[test]
    fn gci_i30gv_run_on_runner_waits_for_a_runner_before_failing() {
        let src = read("src/vm/jdk_continuation.rs");
        let runner = body(&src, "fn run_on_runner(");
        assert!(runner.contains("match acquire_runner_with_backoff(runners, shared) {"));
        assert!(!runner.contains("runners.acquire(shared)"), "no un-retried acquire");
        let retry = body(&src, "fn acquire_runner_with_backoff(");
        let acquire = retry.find("runners.acquire(shared)").expect("acquire");
        let give_up = retry
            .find("if tries >= RUNNER_ACQUIRE_RETRIES => return Err(message)")
            .expect("bounded");
        let sleep = retry.find("std::thread::sleep(wait);").expect("backoff");
        assert!(acquire < give_up && give_up < sleep);
    }

    /// The backoff is bounded: about two seconds in all, never unbounded.
    #[test]
    fn gci_i30gv_the_runner_backoff_totals_about_two_seconds() {
        let mut wait = std::time::Duration::from_millis(1);
        let mut total = std::time::Duration::ZERO;
        for _ in 0..RUNNER_ACQUIRE_RETRIES {
            total += wait;
            wait = (wait * 2).min(std::time::Duration::from_millis(64));
        }
        assert!(total >= std::time::Duration::from_millis(1500), "{total:?}");
        assert!(total <= std::time::Duration::from_millis(2500), "{total:?}");
    }
}

/// Interpreter round i6 wave 118, lane L6 (`CRATONVM_VTHREAD_LEAF_VERDICT_MEMO`):
/// the leaf-verdict memo scans a body once, answers it from the slot after,
/// and scans a body whose slot another body holds every time.
#[cfg(test)]
mod i118_l6_leaf_verdict_memo_tests {
    use super::LeafVerdictMemo;
    use std::cell::Cell;
    use std::sync::Arc;

    #[test]
    fn a_body_is_scanned_once_and_a_stranger_every_time() {
        let memo = LeafVerdictMemo::default();
        let code: Arc<[u8]> = Arc::from(&[0x1a_u8, 0xac][..]);
        let scans = Cell::new(0u32);
        let scan = |answer: bool| {
            scans.set(scans.get() + 1);
            answer
        };
        assert!(memo.verdict(&code, || scan(true)));
        assert!(memo.verdict(&code, || scan(false)), "the memoized answer");
        assert_eq!(scans.get(), 1);
        // A different body: its own slot, or a shared one it does not own.
        let other: Arc<[u8]> = Arc::from(&[0x2a_u8, 0xb6, 0, 1, 0xb1][..]);
        assert!(!memo.verdict(&other, || scan(false)));
        let before = scans.get();
        let again = memo.verdict(&other, || scan(false));
        assert!(!again);
        assert!(scans.get() <= before + 1);
        // The same bytes in another allocation are another body.
        let copy: Arc<[u8]> = Arc::from(&code[..]);
        let before = scans.get();
        let _ = memo.verdict(&copy, || scan(true));
        assert_eq!(scans.get(), before + 1, "not answered from `code`'s slot");
    }
}
