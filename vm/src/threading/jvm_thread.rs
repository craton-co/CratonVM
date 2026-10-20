// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-thread JVM execution state.
//!
//! Each Java thread has its own `JvmThread` containing:
//! - Call stack (for stack traces)
//! - Test output buffer (`printed`)
//! - Thread identity and flags
//
// T1.8.2 — production-code panic gate. Per-thread state is on the
// hottest paths in the VM; an `.unwrap()` here would panic the entire
// runtime. Test code is allowed unwraps so the gate is opt-out under
// `#[cfg(test)]`.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,)
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize};
use std::sync::{Arc, OnceLock};

use parking_lot::{Condvar as PLCondvar, Mutex as PLMutex};

use crate::classloading::resolution::InvokeCache;
use crate::runtime::frame::{Frame, FrameStack};
use crate::runtime::fx_collections::FxHashMap;
use crate::types::{ObjectRef, Value};
use cratonvm_types::ClassId;

/// Pool type for SoA locals and stack vecs: (values, tags).
pub type SoaPool = Vec<(Vec<u64>, Vec<u8>)>;

/// A two-entry per-thread cache for repeated ASCII case conversion.  Returning
/// alternating immutable results avoids the observable same-object shortcut
/// while eliminating allocation/collection in hot use-and-discard patterns.
#[derive(Clone)]
pub struct StringCaseCacheEntry {
    pub source: ObjectRef,
    pub locale: Option<ObjectRef>,
    pub upper: bool,
    pub first: ObjectRef,
    pub second: ObjectRef,
    pub next: bool,
}

#[derive(Clone)]
pub struct JitHashMapStringNodeCacheEntry {
    pub map: ObjectRef,
    pub node: ObjectRef,
    /// Exact String object passed to a HashMap or ConcurrentHashMap get. This
    /// is rooted alongside the map/node pair and avoids content scans.
    pub key_object: Option<ObjectRef>,
    pub key: String,
    pub mod_count_slot: usize,
    pub mod_count: i32,
    /// `Some` selects the ConcurrentHashMap segment seqlock validity rule;
    /// `None` retains the ordinary HashMap `modCount` rule above.
    pub chm_generation: Option<u64>,
    /// Stable identity hash of the CHM segment owning `node` when this is a
    /// CHM entry. Storing the integer avoids a second GC root per cache entry.
    pub chm_segment_id: Option<i32>,
}

// Pool size limits — prevent unbounded growth
const MAX_POOL_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// GcBlockState — blocked-region GC maintenance state (shared with registry)
// ---------------------------------------------------------------------------

/// Blocked-region GC state for one thread, shared `JvmThread` ↔ `ThreadRegistry`
/// (same pattern as `root_snapshot`).
///
/// A thread parked in a blocking native (`Object.wait`, `Thread.join`,
/// `LockSupport.park`, `ReferenceQueue.remove`, …) is excluded from the
/// stop-the-world barrier (`GcBarrier::threads_blocked`), so any number of
/// GCs can complete while it sleeps. Two things would otherwise go stale:
///
/// 1. its deposited `root_snapshot` — scanned as roots by every GC; after the
///    first missed *moving* collection the snapshot addresses point into a
///    vacated semispace, and once that space cycles back around and is
///    collected again the collector evacuates garbage "objects" through
///    them (writing forwarding state into the interior of innocent live
///    objects — the H2 TestScript stale-receiver SEGV);
/// 2. its frames — `check_post_block_gc` only applied the pointer map when a
///    STW was active at the exact wake instant; a GC that completed mid-block
///    left every local/operand-stack ref pointing at recycled from-space.
///
/// The GC initiator therefore calls
/// `ThreadRegistry::fold_pointer_map_into_blocked` (`update_all_roots`
/// step 20, under STW) for every thread whose `in_blocked_region` flag is
/// set: it remaps the thread's `root_snapshot` in place and composes the
/// GC's pointer map into `fixup` (chaining `orig → cur → new` across
/// multiple missed GCs, keyed by the address the frames still hold). On
/// wake the thread applies and clears `fixup` in `check_post_block_gc`.
/// cceres3: one precisely-tracked frame slot for the blocked window — see
/// `GcBlockState::slot_origins`.
#[derive(Clone, Copy)]
pub struct SlotOrigin {
    pub frame: u32,
    pub idx: u32,
    pub is_stack: bool,
    /// Was this slot LIVE at its frame's pc when the deposit ran — i.e. one of
    /// the roots the collector was required to keep?
    ///
    /// The write-back does not need this (healing a dead slot is harmless and
    /// keeps the frame self-consistent), but the fold's invariant check does:
    /// a DEAD local pointing into a reclaimed span is the per-bci liveness
    /// analysis working exactly as designed — `ExecutorService.invokeAll`'s
    /// `tasks` argument, scoped out at the `f.get()` the caller is parked in,
    /// hits this on every round of `probes/BlockedFrameRootProbe`. Reporting
    /// those would bury the case that matters. Operand-stack slots are always
    /// live.
    pub live: bool,
    /// Address the slot held at the blocking deposit.
    pub orig: usize,
    /// The object's current address, advanced by every GC initiator's fold
    /// through that collection's pointer map (exact per-map lookup).
    pub cur: usize,
}

/// One raw word in a BLOCKED peer's native stack that a cross-thread
/// conservative scan resolved to a heap object.
///
/// `SlotOrigin` cannot express this: it is keyed by `(frame, idx)` into the
/// INTERPRETER frames, and these words live in the peer's machine stack, below
/// or between its compiled frames, named by no oop map. They are the population
/// §12 of
/// `internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
/// measured as stale on essentially every relocating cycle.
#[derive(Clone, Copy)]
pub struct NativeSlotFixup {
    /// Absolute address of the word inside the peer's own stack.
    pub addr: usize,
    /// Value the word held when the scan captured it. The wake write-back
    /// refuses to store unless the word STILL reads this, which is what keeps
    /// it off a C local the native call has since reused.
    pub orig: usize,
    /// The object's current address, advanced by each collection's fold.
    pub cur: usize,
}

/// The compiled activations a thread blocked in native code stands under, as
/// a JDWP frame listing splices them into its interpreter frames (interpreter
/// round i1 wave 21, lane L3; `interpreter::capture_blocked_compiled_view`).
/// Plain data: no object reference, so no collection has to know about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockedCompiledView {
    /// The compiled (and inlined) activations, bottom first, each listed
    /// immediately below interpreter frame `below` (`frames.len()`: above the
    /// top interpreter frame).
    pub rows: Vec<BlockedCompiledRow>,
    /// Interpreter frames whose body runs compiled (an OSR'd loop, or a body
    /// entered compiled under its own frame): `(frame index, the compiled
    /// half's bytecode index or -1)`. Their `Frame` locals are the values at
    /// the compiled entry, so a listing withholds them.
    pub stale: Vec<(u32, i64)>,
    /// Interpreter round i6 wave 120, lane L2 (`CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`,
    /// stage 2 of `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`):
    /// `(index into rows, one value per local)` for the compiled activations
    /// whose locals the capture could read from their home slots
    /// (`interpreter::capture_blocked_compiled_view_with_homes`). Empty
    /// without the switch, for inlined rows, and for every capture that is
    /// not its own thread's.
    ///
    /// Wave 123 (lane L2, stage 3b of i43-L1): each entry also names the
    /// activation's body and frame base ([`CompiledHomeBody`]), the key a
    /// debugger's `SetValues` to it is recorded and applied by. The element
    /// grew, not the `Vec`, so no layout ratchet row moves.
    pub homes: Vec<(u32, Box<[CompiledHomeValue]>, CompiledHomeBody)>,
}

/// The body and frame base of a compiled activation whose locals were read
/// from their home slots ([`BlockedCompiledView::homes`]; interpreter round i6
/// wave 123, lane L2, stage 3b of
/// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`). Both
/// are addresses valid only while the activation is live, on its own thread:
/// `cm_ptr` is dereferenced only there, `frame_rbp` only ever compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompiledHomeBody {
    /// `ActiveCompiledFrame::cm_ptr`: the `CompiledMethod` the activation runs.
    pub cm_ptr: usize,
    /// `ActiveCompiledFrame::frame_rbp`: the activation's frame pointer, the
    /// base its home slots were read against and the `rbp` its deopt stub
    /// hands the trampoline (`cratonvm_jit::deopt::taken_frame_rbp`).
    pub frame_rbp: usize,
}

/// One local of a compiled activation read from its home slot
/// ([`BlockedCompiledView::homes`]). A reference is an ADDRESS read on the
/// owning thread at capture, valid only until that thread next reaches a
/// safepoint: only a capture that publishes before its thread can reach one
/// (`interpreter::publish_debugger_frames_under`) records it; every other
/// capture records none, which keeps the frame `OPAQUE_FRAME`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompiledHomeValue {
    /// Never written here: an `int` 0, as an interpreter frame's.
    Undefined,
    Int(i32),
    Long(i64),
    /// A `float`'s bits.
    Float(u32),
    /// A `double`'s bits.
    Double(u64),
    /// A reference's address (`0`: null).
    RefAddr(usize),
}

/// The classes of a blocked thread's interpreter frames, as its blocking
/// deposit found them: the per-class half of the stale-frame census
/// (interpreter round i1 wave 24, lane L3;
/// `interpreter::obsolete_frames::prune_histories_by_census`). A thread pushes
/// no frame while it is blocked, so a class this list does not name has no
/// frame on the thread for as long as `GcBlockState::in_blocked_region`
/// stays up. Plain data: no object reference.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockedFrameClasses {
    /// `classes` names the class of every interpreter frame the thread had at
    /// its last flag-raising deposit, and it had no compiled activation then
    /// (whose sites translate through a class's history without a frame).
    /// `false`: unknown -- the census counts the thread for every class.
    pub exact: bool,
    /// Each class once per run of equal neighbours, bottom frame first; the
    /// allocation is reused from one deposit to the next.
    pub classes: Vec<ClassId>,
}

/// One row of [`BlockedCompiledView::rows`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockedCompiledRow {
    /// The interpreter frame (bottom-first index) this row is listed below.
    pub below: u32,
    /// `ClassId` of the method's class.
    pub class_id: u32,
    /// `debug::jdwp_method_id` of the method.
    pub method_id: u64,
    /// Bytecode index the activation stands at, or -1 when unknown.
    pub bci: i64,
    /// The JIT entry-chain index of the compiled activation this row is
    /// (`ActiveCompiledFrame::chain_index`); `None` for a callee it inlined
    /// (interpreter round i1 wave 47, lane L1): a debugger's listing places a
    /// JNI native's row or a reflective call's JDK frames below the first
    /// activation entered after the call (`interpreter::build_frame_listing`),
    /// as a stack trace places them.
    pub chain: Option<u32>,
}

/// `GcBlockState::java_state` of a thread inside a blocking region a
/// registered native opened for a JDK `native` method that blocks in C
/// (`NativeContext::begin_blocking_region_as(BlockKind::InNative)`, with
/// `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE`; interpreter round i2 wave 59, lane
/// L6): HotSpot's `_thread_in_native`. `Thread.getState()` reads RUNNABLE
/// (`ThreadRegistry::java_block_state` answers 0 for it), JVMTI and JMX add
/// in-native (`ThreadRegistry::blocked_in_native`), and another thread's
/// stack trace lists the native on top. Lowered to 0 at the region's end.
pub const JAVA_STATE_IN_NATIVE: u8 = 5;

/// `GcBlockState::java_wait_kind` outside a published sleep: the wait kind is
/// not recorded, and a reader falls back to what it read before (a waiting
/// monitor for `Object.wait`, `PARKED` otherwise).
pub const JAVA_WAIT_KIND_NONE: u8 = 0;

/// `GcBlockState::java_wait_kind` of a thread inside `Thread.sleep`'s
/// TIMED_WAITING region (`NativeContext::begin_blocking_region_as(BlockKind::Sleeping)`
/// with `CRATONVM_PUBLISHED_WAIT_KIND`; interpreter round i2 wave 63, lane
/// L1, stage 2 of
/// `docs/internal/fixed-bugs/interpreter-i58-L1-proposal-one-published-thread-state-word-DONE-20261104.md`):
/// HotSpot's `JavaThreadSleepState`, which JVMTI reads as `SLEEPING` and
/// JDWP as `ThreadStatus.SLEEPING` whatever frame the sleep was entered from.
pub const JAVA_WAIT_KIND_SLEEPING: u8 = 1;

/// `GcBlockState::java_wait_kind` of a thread parked in `LockSupport.park*`
/// (`NativeThreadAccess::park`, `vm_exec::park_with_state`), or in a blocking
/// region that stands for JDK code that parks
/// (`BlockKind::Parked` / `BlockKind::TimedParked`): HotSpot's
/// `JavaThreadParkedState`, JVMTI `PARKED`. Published only with
/// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` (interpreter round i2 wave 64, lane L1;
/// `docs/internal/fixed-bugs/interpreter-i63-L1-proposal-all-wait-kinds-and-suspended-in-the-word-DONE-20261104.md`).
pub const JAVA_WAIT_KIND_PARKED: u8 = 2;

/// `GcBlockState::java_wait_kind` of a thread inside `Object.wait`
/// (`vm_exec::monitor_wait_with_state`): HotSpot's
/// `JavaThreadInObjectWaitState`, JVMTI `IN_OBJECT_WAIT`. Stored before the
/// wait's `java_state` and its waiting monitor, lowered after the waiting
/// monitor is taken, so it spans exactly the span the JMX waiting monitor
/// does -- the lazy running phase included -- and
/// `ThreadRegistry::java_block_state` reads it instead of that monitor's
/// mutex. Only with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` (wave 64, lane L1).
pub const JAVA_WAIT_KIND_OBJECT_WAIT: u8 = 3;

/// `GcBlockState::java_wait_kind` of a thread in a blocking region that
/// stands for a JDK `Object.wait` loop it does not run (the served
/// `Thread.join`: `BlockKind::ObjectWait` / `BlockKind::TimedObjectWait`):
/// JVMTI `IN_OBJECT_WAIT`, as HotSpot's `join` is `wait`. Kept apart from
/// [`JAVA_WAIT_KIND_OBJECT_WAIT`] because no waiting monitor goes with it: it
/// is lowered after the region's flag falls, so the lazy-wait reading of
/// `ThreadRegistry::java_block_state` must not count it. Only with
/// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` (wave 64, lane L1).
pub const JAVA_WAIT_KIND_REGION_OBJECT_WAIT: u8 = 4;

/// `GcBlockState::java_wait_kind` of a thread inside a served stand-in's
/// `Object.wait` that stands for JDK code that parks
/// (`NativeContext::monitor_wait_as` with `BlockKind::Parked` /
/// `BlockKind::TimedParked`: the `--compatible` `CountDownLatch.await`,
/// whose JDK body is an AQS acquire that parks): JVMTI `PARKED`, as HotSpot
/// reads it, where the monitor wait under it would read `IN_OBJECT_WAIT`.
/// Spans exactly what [`JAVA_WAIT_KIND_OBJECT_WAIT`] spans (stored before the
/// wait's state and waiting monitor, lowered after the monitor is taken), so
/// the lazy-wait reading of `ThreadRegistry::java_block_state` counts it as
/// that one. Published with `CRATONVM_SERVED_WAIT_KINDS` (default on),
/// whether or not `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` is (interpreter round
/// i2 wave 65, lane L1;
/// `docs/internal/fixed-bugs/interpreter-i64-L1-proposal-classify-the-unrecorded-wait-regions-DONE-20261028.md`).
pub const JAVA_WAIT_KIND_MONITOR_PARKED: u8 = 5;

/// `JvmThread::region_wait_object`: the open stand-in region published
/// nothing (interpreter round i2 wave 68, lane L1).
pub const REGION_WAIT_OBJECT_NONE: u8 = 0;
/// `JvmThread::region_wait_object`: the open region published its object as
/// the thread's waiting monitor (`ThreadRegistry::publish_region_waiting_monitor`).
pub const REGION_WAIT_OBJECT_MONITOR: u8 = 1;
/// `JvmThread::region_wait_object`: the open region wrote its object into the
/// thread mirror's `parkBlocker` field, which the region's end clears.
pub const REGION_WAIT_OBJECT_PARK_BLOCKER: u8 = 2;

/// `GcBlockState::agent_requests`: a C agent's `MethodEntry` callback recorded
/// a `PopFrame` or a `ForceEarlyReturn*` of the frame just pushed (interpreter
/// round i4 wave 95, lane L7, `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`);
/// the dispatch loop takes it before that frame's bytecode 0 runs.
pub const AGENT_REQUEST_FRAME_CONTROL: u32 = 1 << 0;

pub struct GcBlockState {
    /// gen r5w2/roots6: raised when the owning `JvmThread` is dropped; see
    /// [`GcBlockState::owner_dropped`].
    owner_dropped: AtomicBool,
    /// True from `deposit_root_snapshot` (just before the thread blocks)
    /// until the end of `check_post_block_gc` (after the fixup is applied).
    pub in_blocked_region: AtomicBool,
    /// gci i20/jt (`CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE`): raised by the
    /// owning thread for the span of a lock-free in-native leave
    /// (`GcBarrier::leave_in_native_lockfree_if`), before its `SeqCst` fence
    /// and `stw_requested` load. A stop-the-world census COUNTS a thread whose
    /// word is up, whatever its flag says ([`Self::census_blocked`]). Down at
    /// every other time.
    pub in_native_leaving: AtomicBool,
    /// gcj j2/cm2 (`CRATONVM_GC_HANDSHAKE_TARGET_KICK`, opt-in;
    /// `gcj-j1cm-proposal-target-only-handshake-kick`): the dispatch-loop poll
    /// word (`gc_barrier::LoopPollWord`, one per OS thread) the owning thread's
    /// interpreter loops last ran on, published at a loop entry whose word
    /// differs from [`Self::loop_poll_word_addr`]
    /// ([`Self::publish_loop_poll_word`]), so a handshake requester kicks only
    /// this word ([`Self::kick_published_loop_poll_word`]) instead of every
    /// loop of the VM. `None` until the first published entry.
    loop_poll_word: PLMutex<Option<Arc<crate::threading::gc_barrier::LoopPollWord>>>,
    /// The address of [`Self::loop_poll_word`]'s word (`0`: none), written by
    /// the owning thread only: its loop entry's one compare.
    loop_poll_word_addr: AtomicUsize,
    /// Java-visible blocking kind while `in_blocked_region` is true:
    /// 1 = WAITING (wait/park/join), 2 = BLOCKED (monitor acquisition),
    /// 3 = TIMED_WAITING, 4 = terminating, [`JAVA_STATE_IN_NATIVE`] = a
    /// registered native blocked in C (RUNNABLE, in native).
    /// The GC protocol only needs the boolean above; preserving this small
    /// distinction lets `Thread.getState()` report the JDK state correctly.
    pub java_state: AtomicU8,
    /// What kind of wait a TIMED_WAITING / WAITING `java_state` stands for,
    /// when the region that published it said so ([`JAVA_WAIT_KIND_SLEEPING`]
    /// for `Thread.sleep`), else [`JAVA_WAIT_KIND_NONE`] (interpreter round i2
    /// wave 63, lane L1; `CRATONVM_PUBLISHED_WAIT_KIND`). Written only by the
    /// owning thread: stored before `java_state` (so before
    /// `in_blocked_region` rises), lowered at the region's end after the flag
    /// is down (`vm_exec::lower_wait_kind`). Meaningful only while
    /// `in_blocked_region` is up; read through
    /// `ThreadRegistry::published_sleeping`.
    ///
    /// Wave 64 (lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`): also
    /// [`JAVA_WAIT_KIND_PARKED`], [`JAVA_WAIT_KIND_OBJECT_WAIT`] (which, alone
    /// among the kinds, is meaningful with the flag down: the lazy phase of
    /// `Object.wait`) and [`JAVA_WAIT_KIND_REGION_OBJECT_WAIT`]; read through
    /// `ThreadRegistry::published_wait_kind_of`.
    pub java_wait_kind: AtomicU8,
    /// Composed `frame-held address → current address` map accumulated by GC
    /// initiators for every collection that completed while the thread was
    /// in a blocked region. Applied + cleared on wake.
    pub fixup: PLMutex<cratonvm_types::PointerMap>,
    /// cceres3 (WildFly boot stale-frame family): exact per-slot tracking for
    /// the blocked window. `fixup` above is keyed by the address the frames
    /// held when each object FIRST moved — a chain that breaks if any link's
    /// seed was missed (filtered snapshot, multi-block chains, recycled-address
    /// ABA), permanently stranding the slot. Each entry here instead pins down
    /// one (frame, slot) with the address it held at the blocking deposit;
    /// folds advance `cur` with an exact per-collection lookup and the wake
    /// write-back stores `cur` straight into the slot. Filled only by the
    /// flag-raising deposit; taken (and cleared) on wake.
    pub slot_origins: PLMutex<Vec<SlotOrigin>>,
    /// nio21 wave 17 L3 (`CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT`, opt-in): the
    /// compiled-chain walk the flag-raising blocking deposit deferred to the
    /// collector (`conservative_roots::DeferredJitChain`). Set before
    /// `in_blocked_region` goes up, cleared by every deposit and taken by the
    /// wake right after the flag goes down; read only by a pause's root
    /// gathering (`conservative_roots::resolve_deferred_blocked_jit_chains`)
    /// while the flag is up. `None` with the switch off.
    pub(crate) deferred_jit_chain:
        PLMutex<Option<crate::jit::conservative_roots::DeferredJitChain>>,
    /// ki3 (`CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY`): the
    /// flag-raising deposit of the CURRENT blocked window walked this
    /// thread's whole compiled chain itself, into its snapshot and pins (the
    /// eager twin of a `deferred_jit_chain` record), or found it proven
    /// precise and folded its shadow-stack oops. Same life as the record:
    /// cleared by every deposit made with the flag down, set before
    /// `in_blocked_region` goes up, cleared by the wake right after the flag
    /// goes down; read only by a pause's root gathering while the flag is up
    /// (`ThreadRegistry::blocked_os_tids_with_covered_jit_chain`).
    pub(crate) deposit_walked_jit_chain: AtomicBool,
    /// Raw native-stack words this thread's blocked window had scanned out of
    /// it by a cross-thread conservative scan. Appended by the fold, advanced
    /// by every later fold, applied and cleared on wake. See
    /// [`NativeSlotFixup`].
    pub native_slots: PLMutex<Vec<NativeSlotFixup>>,
    /// The JDWP inspection window of a blocking native region (interpreter
    /// round i1 wave 12): 0, or the thread's `JvmThread` address with flag
    /// bits in its low three bits, open from the blocking deposit to the
    /// start of the leave while a debugger is attached. A debugger command
    /// reads a suspended blocked thread's frames through it
    /// (`interpreter::publish_frames_of_blocked_thread`); the leave waits out
    /// a reader. Not a GC field; only `experimental-debug` builds write it.
    pub debugger_inspect: std::sync::atomic::AtomicUsize,
    /// The thread runs a native through the native-call funnel, outside a
    /// blocking region and with no Java above it, while a debugger is
    /// attached (interpreter round i2 wave 53, lane L6;
    /// `CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`): a JDWP suspension counts it
    /// stopped, as HotSpot's handshake counts a thread in native, because it
    /// parks at the native's return (`debug::await_suspension_settled`).
    /// Raised by `interpreter::enter_running_native`, lowered by
    /// `interpreter::leave_running_native` and, while Java the native runs
    /// executes, by the interpreter's entries (`interpreter::NestedJavaAck`).
    /// Written only by the owning thread. Not a GC field; only
    /// `experimental-debug` builds write it.
    pub debugger_native_ack: AtomicBool,
    /// The thread's agent-request word (interpreter round i4 wave 95, lane
    /// L7; stage 1 of
    /// `docs/internal/proposals/interpreter/i62-L1-proposal-one-agent-request-word-the-dispatch-loop-reads-20261026.md`,
    /// `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`): flag bits
    /// ([`AGENT_REQUEST_FRAME_CONTROL`]) an agent callback run on this thread
    /// leaves for the dispatch loop that will run the frame the callback was
    /// about. The writer raises a bit, then bumps this OS thread's loop poll
    /// word (`gc_barrier::note_code_moved_on_this_thread`), so the loop reads
    /// the word only in its slow path, never per bytecode or per invoke.
    /// Written and cleared only by the owning thread. Not a GC field; only
    /// `experimental-debug` builds raise a bit.
    pub agent_requests: AtomicU32,
    /// The thread's own answer to "is it suspended?" while it waits out a
    /// debugger or C JVMTI suspension (interpreter round i2 wave 63, lane L6;
    /// `CRATONVM_DEBUG_THREAD_SUSPEND_WORD`,
    /// `docs/internal/fixed-bugs/interpreter-i54-L6-proposal-a-per-thread-suspend-word-instead-of-the-debug-state-lock-DONE-20261104.md`):
    /// 0 while the thread is not enrolled; else flag bits
    /// (`debug::SW_ENROLLED`, `SW_SUSPENDED`, `SW_HOLDS_OFF`, `SW_TASK`)
    /// under a change sequence, written under the debug-state lock by every
    /// change of a suspend count, a JVMTI suspension or the thread's task
    /// queue (`DebugState::republish_suspend_word`). The waiting thread
    /// compares it with the word its last locked round saw and takes the
    /// debug-state lock only when it moved. Not a GC field; only
    /// `experimental-debug` builds write it.
    pub debugger_suspend_word: AtomicU32,
    /// The thread runs a JNI native method's C code with no Java above it,
    /// while a C JVMTI env is bound to the VM (interpreter round i2 wave 58,
    /// lane L1; `CRATONVM_JVMTI_C_IN_NATIVE`,
    /// `docs/internal/fixed-bugs/interpreter-L1-jvmti-thread-state-lacks-in-native-FIXED-20261022.md`):
    /// the C table's `GetThreadState` adds `JVMTI_THREAD_STATE_IN_NATIVE`, as
    /// HotSpot does for a thread in `_thread_in_native`. Raised by
    /// `jvmti::native_env::CInNativeMark` around the JNI door's C call,
    /// lowered while Java the native calls runs (`interpreter::NestedJavaAck`).
    /// Written only by the owning thread. Not a GC field; only
    /// `experimental-debug` builds write it.
    pub jvmti_in_native: AtomicBool,
    /// The JDWP frames window of a thread running a native outside a
    /// blocking region (interpreter round i2 wave 56, lane L6,
    /// `CRATONVM_DEBUG_NATIVE_FRAMES_WINDOW`;
    /// `docs/internal/fixed-bugs/interpreter-L6-a-thread-suspended-in-a-running-native-lists-no-frames-FIXED-20261007.md`):
    /// flag bits (`interpreter::NW_OPEN` / `NW_READING` / `NW_PUBLISHED`),
    /// 0 when none. Opened by `interpreter::enter_running_native`, closed by
    /// the native's leave, by an upcall's Java (`interpreter::NestedJavaAck`),
    /// by a root deposit and by the frame stack's growth; a debugger command
    /// reads `debugger_native_frames[..debugger_native_depth]` through it
    /// (`interpreter::publish_frames_of_native_thread`). Not a GC field; only
    /// `experimental-debug` builds write it.
    pub debugger_native_window: std::sync::atomic::AtomicUsize,
    /// The address of the thread's frame buffer (`*const Frame`) when the
    /// native frames window opened. Written by the owning thread before it
    /// opens the window (Release), read by the window's reader.
    pub debugger_native_frames: std::sync::atomic::AtomicUsize,
    /// How many frames stood below the native when the window opened.
    pub debugger_native_depth: std::sync::atomic::AtomicUsize,
    /// The native's callback (`JvmThread::running_native` at the opening).
    pub debugger_native_callback: std::sync::atomic::AtomicUsize,
    /// The callback of the native this thread runs through the native-call
    /// funnel, 0 for none, recorded while a debugger is attached or a C JVMTI
    /// env is bound with `CRATONVM_JVMTI_C_RUNNING_NATIVE` on (interpreter
    /// round i2 wave 61, lane L6; `interpreter::enter_running_native`,
    /// restored by `leave_running_native`). Another thread reads it to tell
    /// a native standing in for a JDK `native` method (in native, as HotSpot's
    /// `_thread_in_native`) from one standing in for a Java method
    /// (`jvmti::native_env::thread_in_native`); it says nothing about Java
    /// above the native (the native-excuse word does). Written only by the
    /// owning thread; only `experimental-debug` builds write it.
    pub funnel_native_callback: std::sync::atomic::AtomicUsize,
    /// This thread is dumping other threads' stacks and has taken the one
    /// frame-trace pause that made every running thread publish its trace
    /// (interpreter round i2 wave 61, lane L6, `CRATONVM_THREAD_DUMP_ONE_PAUSE`;
    /// `NativeContext::begin_thread_dump`): its cross-thread captures read
    /// the published traces without a pause of their own until the dump ends
    /// -- while this holds `GcBarrier::gc_generation` as that pause left it,
    /// so any later pause (or a dump a panic never ended) costs at most one
    /// capture's own pause again. 0: no dump. Written and read only by the
    /// owning thread. Not a GC field.
    pub dump_traces_gen: std::sync::atomic::AtomicU64,
    /// The per-thread "frames current as of collection N" word (interpreter
    /// round i2 wave 57, lane L6;
    /// `docs/internal/fixed-bugs/interpreter-i56-L6-proposal-a-per-thread-frames-current-generation-DONE-20261014.md`):
    /// `GcBarrier::moving_generation` as of which the frames below the
    /// native are current. Stored by the thread as it opens the native frames
    /// window and after each remap it applies to its own frames while the
    /// window is open (`interpreter::note_native_frames_current`: a
    /// safepoint's resume, a blocking region's wake). A moving collection
    /// never completes while the thread runs the native outside a blocking
    /// region (the census waits for a running native), so the word lags only
    /// after one the thread slept through blocked, until its wake remaps; the
    /// reader refuses while it lags. Pauses that move nothing (a loop-exit,
    /// frame-trace or concurrent-mark pause) leave it current: wave 56 keyed
    /// it on `gc_generation`, which counts those too, and every read was
    /// refused.
    pub debugger_native_frames_gen: std::sync::atomic::AtomicU64,
    /// The compiled activations above or between this thread's interpreter
    /// frames, captured by the thread itself as it opens the inspection
    /// window under compiled code (interpreter round i1 wave 21, lane L3):
    /// only the thread can walk its own JIT entry chain, and the reader lists
    /// them from here. Empty otherwise; cleared when the window closes. Not a
    /// GC field; only `experimental-debug` builds write it.
    pub debugger_compiled: PLMutex<BlockedCompiledView>,
    /// The compiled activations below a native the thread runs, captured by
    /// the thread as it opens the native frames window under compiled code
    /// (interpreter round i2 wave 57, lane L6; `interpreter::NW_COMPILED`
    /// says the window carries one). Separate from `debugger_compiled`: a
    /// blocking region inside the native records its own there. They do not
    /// change while the native runs (Java the native calls shuts the window).
    /// Not a GC field; only `experimental-debug` builds write it.
    pub debugger_native_compiled: PLMutex<BlockedCompiledView>,
    /// Interpreter round i2 wave 58, lane L6
    /// (`CRATONVM_DEBUG_NATIVE_WINDOW_CHAIN`): instead of
    /// `debugger_native_compiled`, a copy of the thread's JIT entry chain and
    /// stack pointer, taken as it opens the native frames window under
    /// compiled code; the window's reader walks the compiled activations from
    /// it (`interpreter::NW_CHAIN` says the window carries one). Not a GC
    /// field; only `experimental-debug` builds write it.
    pub(crate) debugger_native_chain: PLMutex<crate::jit::conservative_roots::JitChainSnapshot>,
    /// The reflective calls and JNI natives' rows below the native the
    /// thread runs, recorded by it at the native-call funnel's entry when it
    /// has any ([`NativeRowsSnapshot`]; interpreter round i2, the i53
    /// retirement): what a handshake pause that excused it with
    /// [`NX_REFLECTIVE`] and the JDWP native frames window's reader splice.
    /// Not a GC field: no row names an object.
    pub native_rows: PLMutex<NativeRowsSnapshot>,
    /// The stale-frame census's input (interpreter round i1 wave 23, lane
    /// L3; `interpreter::obsolete_frames`): no frame of this thread is
    /// stamped below it, as of its last conversion pass
    /// (`convert_obsolete_frames_if_redefined`) that found no compiled
    /// activation on its stack, or of its creation. A step of a class's
    /// redefinition history retired at or below every thread's floor
    /// translates for nobody. Written only by the owning thread; read by a
    /// redefining thread through the registry, which shares this `Arc`.
    /// Not a GC field.
    pub obsolete_frame_floor: std::sync::atomic::AtomicU64,
    /// The classes of this thread's frames at its last flag-raising deposit
    /// (interpreter round i1 wave 24, lane L3; [`BlockedFrameClasses`]): while
    /// `in_blocked_region` is up, the census counts this thread's floor only
    /// for the classes it names. Written by the owning thread before it
    /// raises the flag, and marked inexact by the registry's native-block
    /// marker (which raises the flag without a deposit). Not a GC field.
    pub blocked_frame_classes: PLMutex<BlockedFrameClasses>,
    /// gcd d2/i (opt-in `CRATONVM_XT_BLOCKED_MONITOR_PROOF`): the JIT depth
    /// this thread PROVED rewritable at the flag-raising deposit of a block in
    /// the compiled `monitorenter` helper, or 0 (none, or not proven), or
    /// [`MONITOR_BLOCK_PROOF_ARMED`] (armed by the helper, deposit not yet
    /// taken). Written only by the owning thread: armed by
    /// `vm_exec::monitor_enter_blocking_from_compiled`, settled by
    /// `deposit_root_snapshot_inner` BEFORE it raises `in_blocked_region`
    /// (Release), cleared by the same helper after the wake. Read by the
    /// take-over (`ThreadRegistry::blocked_os_tids_split_by_monitor_proof`)
    /// only while `in_blocked_region` is up, i.e. while the deposit's proof
    /// still describes the thread's frames.
    pub monitor_block_proven_jit_depth: std::sync::atomic::AtomicUsize,
    /// gcd d5/f: how many relocating folds
    /// (`ThreadRegistry::fold_pointer_map_into_blocked_audited`) have targeted
    /// this thread's blocked state, ever. Bumped by the fold (inside a pause,
    /// while `in_blocked_region` is up), read by the owning thread: a JNI
    /// native in native (`native::jni`) compares it with the value it read
    /// before its deposit to prove nothing it holds has moved. Monotonic;
    /// compared, never reset.
    pub blocked_folds: std::sync::atomic::AtomicU64,
    /// gcg g2/k (`common-w2g-proposal-blocked-threads-replay-pause-maps`):
    /// [`Self::blocked_folds`] as the owning thread's last full blocked-region
    /// wake (`vm_exec::check_post_block_gc_refs`) read it. The wake reports
    /// `k = blocked_folds - folds_at_last_wake` -- how many pauses' maps the
    /// fold composed into the chain it applies -- on its `[blockgc] wake`
    /// line, which is the measurement the proposal is gated on. Written and
    /// read only by the owning thread. A wake path that applies the chain
    /// without updating it (the leaked-region heal) makes the next `k` an
    /// over-count; diagnostics only.
    pub folds_at_last_wake: std::sync::atomic::AtomicU64,
    /// Interpreter round i4 wave 99, lane L1
    /// (`CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT`, default on): what the entry
    /// of the owning thread's current `begin_blocking_region*` region read
    /// ([`BlockingLeaveKey`]), for the quiet leave in
    /// `vm_exec::NativeContextImpl::check_post_block_gc_inner`. Replaced by
    /// every such entry (`None` when the entry is not eligible) and TAKEN by
    /// every blocked-region wake, so a key never outlives its window. Written
    /// and read only by the owning thread; a mutex only because the state is
    /// shared through an `Arc`.
    pub blocking_leave_key: PLMutex<Option<BlockingLeaveKey>>,
    /// gcg g6/jn (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS`): the last
    /// root-snapshot deposit of the owning thread ran the conservative
    /// interpreter-local probe (`roots::conservative_locals_enabled()` was
    /// true: some thread was in compiled code), so the snapshot may hold dead
    /// locals' stale words. Written by `deposit_root_snapshot_inner` and
    /// `update_root_snapshot`; read by `ThreadRegistry::collect_all_root_snapshots`.
    pub deposit_conservative: AtomicBool,
    /// gci i19/vd: a virtual thread's continuation (`ThreadKind::Virtual`)
    /// that has EVER entered compiled code. Set (never cleared) by
    /// `jit_bridge::assert_no_compiled_continuation`, the one check both
    /// interpreter->compiled entries pass, in release builds too: the rule
    /// that a continuation never runs compiled code is enforced by per-door
    /// declines and only debug-asserted there. While it is down, no compiled
    /// callee can have left a lost-tag local in the continuation's frames, so
    /// its blocking deposit may skip the conservative-locals probes on G1
    /// (`thread_registry::continuation_is_interpreted_only`); once up, that
    /// thread's deposits are conservative again for the rest of its life.
    /// Written and read by the owning thread only (the deposit runs on it).
    pub continuation_entered_compiled: AtomicBool,
    /// gch h3/yc (`gch-h2yg-parked-conservative-snapshot-is-excluded-whole-...`):
    /// the PRECISE words of the owning thread's last conservative SAFEPOINT
    /// deposit (`update_root_snapshot`): the values the liveness-mapped local
    /// scan, the ruled operand-stack scan and the frames' monitors published,
    /// before the conservative probes added their extras. The parked twin of
    /// a blocked thread's `slot_origins`: the unrecorded-young-root guard
    /// keeps these under its check when it excludes the rest of a
    /// conservative snapshot. Unsorted; written whole by the deposit (empty
    /// for a non-conservative one), read by
    /// `ThreadRegistry::collect_all_root_snapshots`. Not remapped by a fold: a
    /// stale entry only keeps a word under the guard (at most one refused
    /// cycle per address, the switch-off behaviour), never excludes one.
    pub precise_deposit_words: PLMutex<Vec<usize>>,
    /// gcg g3/k (`common-a-proposal-per-thread-handshakes`, step 1): the
    /// operations other threads asked THIS thread to run on itself
    /// ([`HandshakeQueue`]). Queued by
    /// `ThreadRegistry::request_handshake`, run by the owner at
    /// [`JvmThread::run_pending_handshakes`], abandoned when the owner is
    /// dropped (`GcBlockStateDropNotice`).
    pub handshakes: HandshakeQueue,
    /// JIT round 16 wave 9b (lane ffm): this thread is running FOREIGN code (an
    /// FFM downcall's C function) inside a GC-safe region it entered with a
    /// fresh root deposit (`NativeContextImpl::begin_native_region` /
    /// `begin_native_region_quiet`). Raised by the owning thread before that
    /// deposit, lowered once the region is left. While it is up together with
    /// `in_blocked_region`, the registry leaves the thread out of the OS tids
    /// a pause SIGNALS (`ThreadRegistry::blocked_os_tids`,
    /// `alive_count_and_os_tids`): the take-over's `SIGUSR2` made a blocking
    /// syscall in the C code (`nanosleep`, `poll`, `epoll_wait`, `read` on a
    /// pipe ...) return `EINTR`, which HotSpot never does to a thread in native.
    /// See `r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native-20261002`.
    pub foreign_native: AtomicBool,
    /// JIT round 16 wave 14 (lane ffm3): the LAZY in-native state of an FFM
    /// downcall (`CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY`), HotSpot's
    /// `_thread_in_native` without a root deposit: [`LAZY_NATIVE_OFF`],
    /// [`LAZY_NATIVE_IN`] (the thread is in C and stays a COUNTED mutator; its
    /// frames above [`Self::lazy_native_lo`] are frozen) or
    /// [`LAZY_NATIVE_CLAIMED`] (a collection's take-over claimed it, scans it
    /// like a peer frozen in compiled code and excuses it from the barrier;
    /// the thread cannot leave C until the pause releases it). Written by the
    /// owning thread ([`Self::enter_lazy_native`], [`Self::leave_lazy_native`])
    /// and by a pause ([`Self::try_claim_lazy_native`],
    /// [`Self::release_lazy_native_claim`]); every transition but the entry is
    /// a compare-exchange on this one word.
    pub lazy_native: AtomicU32,
    /// The low end of the stack band a claim scans: an address below every
    /// frame the thread had when it went into C, its callee-saved registers
    /// spilled inside the band. Written by the owning thread before it
    /// publishes [`LAZY_NATIVE_IN`] (Release), read by a claim after its
    /// compare-exchange (Acquire).
    pub lazy_native_lo: AtomicUsize,
    /// The exclusive high end of that band: the thread's stack top.
    pub lazy_native_hi: AtomicUsize,
    /// No JNI-local or deopt TLS holder survives C; exact thread fields may
    /// be deposited. Compiled/shadow holders additionally need complete pins.
    pub lazy_native_deposit_eligible: AtomicBool,
    /// The owner published no live JIT entry or shadow slot at native entry.
    pub lazy_native_machine_roots_absent: AtomicBool,
    /// This pause deposited on the owner's behalf.
    pub lazy_native_precise_claim: AtomicBool,
    /// JIT round 18 wave 10 (lane ffmeintr, EINTR2-1,
    /// `CRATONVM_XT_HELPER_UNPARKED_READ`): while the owning thread is inside a
    /// blocked-wait trampoline ([`run_blocked_wait`]), the low end of its
    /// frozen stack band -- the trampoline's callee-saved register spill;
    /// every frame above it, and every register value of those frames, is in
    /// `[lo, stack top)`. `0` outside a trampoline. Bit 0 (the address is
    /// 8-aligned) is the collector's HOLD: set by the helper-window pass while
    /// it copies the band without parking the thread
    /// ([`Self::try_hold_blocked_wait_band`]); the thread cannot leave the
    /// trampoline while it is set.
    pub blocked_wait_lo: AtomicUsize,
    /// gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`): the low end of the
    /// compiled band a JNI native call left to the pause
    /// (`native::jni::in_native_band_at_pause`), 0 for none. Written only by the
    /// owning thread with `in_blocked_region` down (Relaxed; the flag's Release
    /// store publishes it); read by `ThreadRegistry::scan_in_native_bands` only
    /// while `in_blocked_region` and `foreign_native` are both up.
    pub in_native_band_lo: AtomicUsize,
    /// gch h4/bp: the exclusive high end of that band (the thread's stack top).
    pub in_native_band_hi: AtomicUsize,
    /// gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in;
    /// `gch-h4bp-review-findings-RETIRED-UNFIXED-20261018` item 1): the JIT depth the
    /// flag-raising deposit of this thread's current JNI in-native window
    /// PROVED rewritable (its per-thread moving-young coverage proof held), or
    /// 0 (none, not proven, switch off). Travels with the band: zeroed by the
    /// owning thread before the entry's deposit (flag down), written by that
    /// deposit before it raises `in_blocked_region` (Release), zeroed with the
    /// band once the flag is down again (`native::jni::clear_in_native_band`).
    /// Read by the pause (`ThreadRegistry::credit_in_native_proven_bands`)
    /// only while the flag is up and the band is recorded.
    pub in_native_proven_jit_depth: AtomicUsize,
    /// gci i24/bp: the young-pin-ledger pause stamp
    /// (`gc_quiescence::young_pin_pause_stamp`) of the pause that credited this
    /// thread's in-native band (depth to the coverage account, band words to
    /// the young pin ledger), 0 for never. Written only by the collecting
    /// thread inside a pause; read by the same pause's band scan
    /// (`gc_and_alloc::append_in_native_band_roots`) to know the band was
    /// credited, and by a second credit call of the same pause to credit once.
    pub in_native_credit_stamp: std::sync::atomic::AtomicU64,
    /// gcj j2/jn2 (cut 2 of
    /// `gcj-j1jn-proposal-jni-transition-cost-ledger-and-three-cuts-20261019`,
    /// `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB=1`, opt-in, Generational only): this
    /// thread went into native for a JNI call WITHOUT retiring its TLAB.
    /// Raised by the owning thread while it is still a counted mutator, before
    /// `in_blocked_region` goes up (Release), so a pause that reads the flag up
    /// reads this. Whoever swaps it from `true` owns the retire: a collecting
    /// pause, with the thread in native and unable to leave
    /// (`ThreadRegistry::retire_in_native_kept_tlabs`), or the owner once its
    /// leave has the flag down (`native::jni::native_call_leave_native`), who
    /// then finds the TLAB intact and allocates on from it.
    pub in_native_tlab_kept: AtomicBool,
    /// gci i9/jw (`gci-i8orch-takeover-promotes-jna-upcall-wrong-result-FIXED-20261017`):
    /// the address of the `JNI_LOCAL_FRAMES` table of the OS thread that last
    /// installed this `JvmThread` as its JNI thread
    /// (`native::jni::publish_local_frames_owner`), 0 for none. Read only by
    /// the take-over's frozen-peer walk, and only while
    /// [`Self::jni_local_frames_os_tid`] names a peer that pause holds frozen
    /// ([`Self::published_jni_local_frames`]). Written only by the owning
    /// thread.
    jni_local_frames_addr: AtomicUsize,
    /// gci i9/jw: the OS tid (`thread_registry::current_os_tid`'s encoding)
    /// of the thread whose table [`Self::jni_local_frames_addr`] names.
    jni_local_frames_os_tid: AtomicU32,
    /// gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`): the
    /// address of the JIT's `LAST_DEOPT` stash cell of the OS thread that last
    /// published its tid for this thread (`cratonvm_jit::deopt::stash_cell_addrs`,
    /// via `ThreadRegistry::publish_os_tid_claim`), 0 for none. Same protocol
    /// as [`Self::jni_local_frames_addr`]: read only by the take-over's
    /// frozen-peer walk while [`Self::deopt_stash_os_tid`] names a peer that
    /// pause holds frozen ([`Self::published_deopt_stash_cells`]).
    deopt_stash_cell_addr: AtomicUsize,
    /// gci i9/ds: the `LAST_EXCEPTIONAL` stash cell beside
    /// [`Self::deopt_stash_cell_addr`].
    deopt_exceptional_cell_addr: AtomicUsize,
    /// gci i9/ds: the OS tid whose stash cells the two fields above name.
    deopt_stash_os_tid: AtomicU32,
    /// Interpreter round i2 wave 58, lane L6 (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE`;
    /// `docs/internal/fixed-bugs/interpreter-L6-a-cross-thread-stack-trace-waits-for-a-running-native-to-return-FIXED-20261024.md`):
    /// the thread runs a native through the native-call funnel outside a
    /// blocking region, and a HANDSHAKE pause (one that collects nothing) may
    /// excuse it instead of waiting for the native to return -- HotSpot's
    /// handshake treats `_thread_in_native` as safe. See [`NativeExcuse`].
    pub native_excuse: NativeExcuse,
}

/// The per-thread word of a handshake pause's excusal of a thread running a
/// native (interpreter round i2 wave 58, lane L6;
/// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE`, `GcBarrier::native_excuse_enabled`).
///
/// # The protocol
///
/// * The owning thread raises [`NX_IN`] (with [`NX_LISTABLE`] and the depth
///   of the interpreter frames below the native when the frames below are all
///   interpreter frames another thread may list) as it enters a native in
///   the funnel (`vm_exec::safe_native_call_impl`), and lowers it as the
///   native returns, as Java the native runs starts (an upcall,
///   `interpreter::NestedJavaAck`), and around the frame stack's one
///   relocation (`FrameStack::grow_for`). Every lowering is a
///   compare-exchange of the exact value raised, so it fails while a pause
///   holds [`NX_EXCUSED`]: the thread then waits, executing nothing, until the
///   pause releases it.
/// * A non-collection handshake pause sets [`NX_EXCUSED`] on a counted thread
///   it finds raised, in its census, under the barrier lock
///   (`ThreadRegistry::excuse_running_natives`), and takes the thread out of
///   the quota: the barrier never counts an arrival of it in that pause
///   (`GcBarrier`'s `excused_running`). The pause clears the bit after its
///   `complete_gc` ([`Self::release`]).
/// * So while excused, the thread may still run its native's Rust code -- a
///   pause that collects nothing has no heap invariant a native can break --
///   but it runs no Java, its frames below the native stay where they are
///   and as they are, and it cannot return to its caller. A COLLECTION never
///   excuses (its census waits for a running native, as before): a native
///   holds raw object references a moving collector cannot rewrite.
///
/// `frames` is the address of the thread's frame buffer, republished by the
/// frame stack at its every relocation, which a pause reads (with the depth)
/// only while it holds the excusal.
///
/// Interpreter round i2 wave 60, lane L6
/// (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED`): `jit_cells` are the
/// addresses of the thread's JIT entry chain, frame-record mirrors and
/// OSR-continuation registry (`conservative_roots::JitChainCells`,
/// `jit_bridge::osr_continuations_cell_addr`), published once with the frame
/// buffer: a pause that excused the thread in a native entered under
/// compiled code ([`NX_COMPILED`]) copies the chain through them and walks
/// the compiled frames below the native, which the excusal holds in place.
///
/// Interpreter round i2 wave 62, lane L6
/// (`CRATONVM_HANDSHAKE_NATIVE_CALLBACK`, `GcBarrier::excused_native_callback`):
/// `callback` is the funnel callback the raised word is for, stored by the
/// owning thread before the raise (and put back with an outer native's word),
/// so a reader that finds [`NX_IN`] can ask which native runs without a
/// stack capture (`ThreadRegistry::running_native_method`).
pub struct NativeExcuse {
    word: AtomicUsize,
    frames: AtomicUsize,
    jit_cells: [AtomicUsize; 4],
    callback: AtomicUsize,
}

/// [`NativeExcuse`]: in a native, excusable by a handshake pause.
pub const NX_IN: usize = 1;
/// [`NativeExcuse`]: a frame-trace pause can list the frames below the
/// native: interpreter frames, with the compiled activations among them
/// ([`NX_COMPILED`], wave 60) and the reflective calls ([`NX_REFLECTIVE`],
/// the i53 retirement) when the word says so. A JNI native's or a lambda
/// proxy's row in `JvmThread::jni_native_frames` does not count since
/// interpreter round i2 wave 62, lane L6 -- the reader lists no such row.
pub const NX_LISTABLE: usize = 2;
/// [`NativeExcuse`]: a pause excused the thread; it may not lower the word.
pub const NX_EXCUSED: usize = 4;
/// [`NativeExcuse`], with [`NX_LISTABLE`]: compiled activations are among the
/// frames below the native; a pause lists them through the published JIT
/// cells (interpreter round i2 wave 60, lane L6,
/// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED`).
pub const NX_COMPILED: usize = 8;
/// [`NativeExcuse`], with [`NX_LISTABLE`]: reflective calls are among the
/// frames below the native; their rows are in `GcBlockState::native_rows`,
/// recorded by the thread before the raise, and a pause splices their JDK
/// frames as the thread's own published trace does (interpreter round i2,
/// the i53 retirement;
/// `docs/internal/fixed-bugs/interpreter-L6-a-native-below-a-reflective-call-is-listed-only-after-it-returns-FIXED-20261007.md`).
pub const NX_REFLECTIVE: usize = 16;
/// [`NativeExcuse`]: the interpreter depth below the native, from this bit up.
pub const NX_DEPTH_SHIFT: u32 = 5;

/// The rows of a thread's stack that no reader of its frames alone can see,
/// as they stood when it entered the native it runs (interpreter round i2,
/// the i53 retirement): its reflective calls with their named JDK frames,
/// its served `Thread.run` calls, and its JNI natives' rows. Recorded by the
/// owning thread at the native-call funnel's entry, only when it has such
/// rows (copying only what changed since the last record), so that another
/// thread -- a handshake pause that excused it ([`NX_REFLECTIVE`]), or the
/// JDWP native frames window's reader -- splices them as the thread's own
/// capture would, without reading the thread's own lists, which the native's
/// Rust body or an upcall may push to and reallocate meanwhile. The rows
/// below the native do not change while it runs: each is withdrawn only by
/// the code that pushed it, after the native returns.
#[derive(Default)]
pub struct NativeRowsSnapshot {
    /// `JvmThread::reflective_calls` at the record.
    pub(crate) reflective_calls: Vec<crate::runtime::stackwalker::ReflectiveCallRow>,
    /// `JvmThread::reflective_frames_named` at the record.
    pub(crate) reflective_named: crate::runtime::stackwalker::ReflectiveFramesNamed,
    /// `JvmThread::served_thread_run` at the record.
    pub(crate) served: crate::runtime::stackwalker::ServedThreadRunCalls,
    /// `JvmThread::jni_native_frames` at the record.
    pub(crate) jni_rows: Vec<(usize, ClassId, u64, u32)>,
}

impl NativeRowsSnapshot {
    /// Make the record `calls` / `named` / `served` / `jni`, copying only
    /// what changed since the last one (a thread that runs many natives
    /// inside one reflective call copies its rows once).
    pub(crate) fn record(
        &mut self,
        calls: &[crate::runtime::stackwalker::ReflectiveCallRow],
        named: &crate::runtime::stackwalker::ReflectiveFramesNamed,
        served: &crate::runtime::stackwalker::ServedThreadRunCalls,
        jni: &[(usize, ClassId, u64, u32)],
    ) {
        if self.reflective_calls.as_slice() != calls {
            self.reflective_calls.clear();
            self.reflective_calls.extend_from_slice(calls);
        }
        if !crate::runtime::stackwalker::reflective_named_same(&self.reflective_named, named) {
            self.reflective_named = named.clone();
        }
        if !self.served.same_rows(served) {
            self.served = served.clone();
        }
        if self.jni_rows.as_slice() != jni {
            self.jni_rows.clear();
            self.jni_rows.extend_from_slice(jni);
        }
    }
}

impl NativeExcuse {
    pub const fn new() -> Self {
        Self {
            word: AtomicUsize::new(0),
            frames: AtomicUsize::new(0),
            jit_cells: [
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ],
            callback: AtomicUsize::new(0),
        }
    }

    /// Record `callback` as the native the next raise is for (the owning
    /// thread, before [`Self::raise`], whose Release orders it); answers the
    /// one it replaces, for the leave to put back (wave 62).
    #[inline]
    pub fn replace_callback(&self, callback: usize) -> usize {
        use std::sync::atomic::Ordering;
        let prior = self.callback.load(Ordering::Relaxed);
        self.callback.store(callback, Ordering::Relaxed);
        prior
    }

    /// Put back the callback [`Self::replace_callback`] replaced.
    #[inline]
    pub fn restore_callback(&self, callback: usize) {
        self.callback
            .store(callback, std::sync::atomic::Ordering::Relaxed);
    }

    /// The callback recorded for the raised word (0 when none was, or the
    /// switch is off). Read after an Acquire [`Self::load`] that saw
    /// [`NX_IN`]: it is that native's, or a later one's.
    #[inline]
    pub fn callback(&self) -> usize {
        self.callback.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Publish the owning thread's JIT cells (`[chain, rbp slot, compile-id
    /// slot, OSR registry]`), once, before the first word that carries
    /// [`NX_COMPILED`] (whose Release raise orders them).
    pub fn publish_jit_cells(&self, cells: [usize; 4]) {
        for (slot, v) in self.jit_cells.iter().zip(cells) {
            slot.store(v, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The JIT cells as published (all 0 before).
    pub fn jit_cells(&self) -> [usize; 4] {
        self.jit_cells
            .each_ref()
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Have the JIT cells been published?
    #[inline]
    pub fn has_jit_cells(&self) -> bool {
        let [chain, ..] = &self.jit_cells;
        chain.load(std::sync::atomic::Ordering::Relaxed) != 0
    }

    /// The word a native entry raises: [`NX_IN`], plus [`NX_LISTABLE`] and
    /// `depth` when `listable`.
    #[inline]
    pub fn raised(listable: bool, depth: usize) -> usize {
        if listable && depth <= (usize::MAX >> NX_DEPTH_SHIFT) {
            NX_IN | NX_LISTABLE | (depth << NX_DEPTH_SHIFT)
        } else {
            NX_IN
        }
    }

    /// The word as it stands (with [`NX_EXCUSED`] when a pause holds it).
    #[inline]
    pub fn load(&self) -> usize {
        self.word.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Raise `word` (from lowered). Release: a pause that sees it sees the
    /// frame buffer published before it.
    #[inline]
    pub fn raise(&self, word: usize) {
        self.word.store(word, std::sync::atomic::Ordering::Release);
    }

    /// Lower `word`, the value this thread raised: one compare-exchange, and
    /// a wait while a pause holds the excusal. `true` if it waited.
    #[inline]
    pub fn lower(&self, word: usize) -> bool {
        use std::sync::atomic::Ordering;
        match self
            .word
            .compare_exchange(word, 0, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => false,
            Err(_) => self.lower_slow(word),
        }
    }

    #[cold]
    #[inline(never)]
    fn lower_slow(&self, word: usize) -> bool {
        use std::sync::atomic::Ordering;
        let mut waited = false;
        let mut spins = 0u32;
        loop {
            match self
                .word
                .compare_exchange(word, 0, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return waited,
                // Excused: wait for the pause to release it.
                Err(cur) if cur == word | NX_EXCUSED => {
                    waited = true;
                    lazy_native_backoff(&mut spins);
                }
                // Not the word this thread raised (a lowering with nothing
                // raised): nothing to wait for. Unreachable on the paired
                // paths.
                Err(_) => return waited,
            }
        }
    }

    /// A pause excuses the thread: `raised -> raised | EXCUSED`, only when
    /// the word is raised (and [`NX_LISTABLE`] when `need_listable`) and not
    /// already excused. Answers the word it excused.
    pub fn try_excuse(&self, need_listable: bool) -> Option<usize> {
        use std::sync::atomic::Ordering;
        let w = self.word.load(Ordering::Acquire);
        if w & NX_IN == 0 || w & NX_EXCUSED != 0 || (need_listable && w & NX_LISTABLE == 0) {
            return None;
        }
        self.word
            .compare_exchange(w, w | NX_EXCUSED, Ordering::AcqRel, Ordering::Acquire)
            .ok()
    }

    /// The pause that excused the thread is over: clear [`NX_EXCUSED`]
    /// (Release: the thread's lowering acquires what the pause wrote).
    pub fn release(&self) {
        self.word
            .fetch_and(!NX_EXCUSED, std::sync::atomic::Ordering::AcqRel);
    }

    /// Publish the frame buffer's address (the frame stack, at its
    /// relocation and when it learns this word).
    #[inline]
    pub fn publish_frames(&self, base: usize) {
        self.frames
            .store(base, std::sync::atomic::Ordering::Release);
    }

    /// The frame buffer's address as last published.
    #[inline]
    pub fn frames(&self) -> usize {
        self.frames.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Default for NativeExcuse {
    fn default() -> Self {
        Self::new()
    }
}

/// [`GcBlockState::lazy_native`]: not in a lazy in-native downcall.
pub const LAZY_NATIVE_OFF: u32 = 0;
/// [`GcBlockState::lazy_native`]: in C, counted, claimable by a pause.
pub const LAZY_NATIVE_IN: u32 = 1;
/// [`GcBlockState::lazy_native`]: in C, claimed (scanned and excused) by a
/// pause that has not released it yet.
pub const LAZY_NATIVE_CLAIMED: u32 = 2;
/// [`GcBlockState::lazy_native`] (JIT round 18 wave 10, lane ffmeintr): still
/// claimed, and the owning thread is BACK from C, waiting at its leave
/// (`CRATONVM_FFM_LAZY_LEAVE_HANDOFF`). The release hands it the leave
/// (`-> OFF`) instead of putting it back in C (`-> IN`), so the next pause
/// cannot claim it again before it is scheduled; see
/// [`GcBlockState::release_lazy_native_claim`].
pub const LAZY_NATIVE_CLAIMED_LEAVING: u32 = 3;
/// [`GcBlockState::lazy_native`] (JIT round 18 wave 10b, lane ffmeintr): the
/// Linux take-over is signalling this thread, which is NOT in a lazy
/// downcall; until the pass has its answer the thread may not go into C
/// lazily ([`GcBlockState::enter_lazy_native`] waits), so the signal is
/// delivered in Rust or Java, never inside the C call it was about to make
/// (`CRATONVM_FFM_LAZY_SIGNAL_GATE`).
pub const LAZY_NATIVE_SIGNALLING: u32 = 4;

/// `CRATONVM_FFM_LAZY_SIGNAL_GATE` (JIT round 18 wave 10b, lane ffmeintr; a
/// correctness fix, default ON, `=0` kills it): the Linux take-over pass
/// closes a counted thread's lazy in-native entry for the instant it signals
/// it ([`GcBlockState::try_gate_takeover_signal`]). Read by the initiator
/// once per take-over pass; the thread side has no switch (with the gate
/// off nobody ever writes [`LAZY_NATIVE_SIGNALLING`]).
pub fn lazy_signal_gate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_LAZY_SIGNAL_GATE")
}

/// What [`GcBlockState::try_gate_takeover_signal`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakeoverSignalGate {
    /// The thread was not in a lazy downcall: its entry is closed until
    /// [`GcBlockState::ungate_takeover_signal`]; signal it.
    Gated,
    /// The thread is in C through a lazy downcall: do not signal it (the
    /// next round claims it; a pause that does not claim waits for it).
    InNative,
    /// Any other state (claimed, leaving, already gated): signal as before.
    Other,
}

/// `CRATONVM_FFM_LAZY_LEAVE_HANDOFF` (JIT round 18 wave 10, lane ffmeintr; a
/// liveness fix, default ON, `=0` restores the round 16 leave): a thread whose
/// leave from a lazy in-native downcall found it claimed marks itself
/// [`LAZY_NATIVE_CLAIMED_LEAVING`], and the release completes its leave.
/// Read by the owning thread once per waited leave (the cold path only).
pub fn lazy_leave_handoff_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_LAZY_LEAVE_HANDOFF")
}

/// `CRATONVM_XT_HELPER_UNPARKED_READ` (JIT round 18 wave 10, lane ffmeintr,
/// EINTR2-1 of `jit-r17-eintr2-proposals-RETIRED-20261006.md`; opt-in, a performance change):
/// the VM's blocking waits run inside [`run_blocked_wait`], and the Linux
/// helper-window pass reads such a blocked peer's band without signalling or
/// parking it (`xt_root_scan`, `helper_window_pass`). Off: no trampoline
/// publishes a band, and every blocked peer is parked as before. Read per
/// wait by the waiting thread and per pass by the collector.
pub fn helper_unparked_read_enabled() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
        && cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_HELPER_UNPARKED_READ")
}

/// JIT round 18 wave 10 (lane ffmeintr, EINTR2-1): run `wait` -- ONE OS-level
/// wait of a thread already inside a blocked region (a condvar wait, a
/// sleep), nothing else -- with the thread's stack band published for the
/// helper-window pass to read without parking it
/// (`CRATONVM_XT_HELPER_UNPARKED_READ`; with the switch off this is `wait()`).
///
/// Why the band `[spill, stack top)` is the whole of what the thread can
/// name while it waits: every frame above this one is suspended in a call
/// that has not returned; a value one of them keeps in a callee-saved
/// register (rbx, rbp, r12-r15, and rdi/rsi for Win64) is, at this
/// function's entry, either still in that register -- then the spill below
/// copies it -- or was saved by the prologue of a frame in between, this
/// one's included, all of which are above the spill. Caller-saved registers
/// hold nothing across a call. Frames BELOW the spill are `wait`'s, and the
/// contract is that they hold no Java reference: `wait` is a library wait
/// (parking_lot, `std::thread::sleep`), and its closure captures only
/// non-reference values.
///
/// Why the band does not change under the reader: the thread publishes it
/// only after the spill is stored, and cannot leave this function while the
/// collector holds it ([`GcBlockState::try_hold_blocked_wait_band`] /
/// `leave_blocked_wait_band`); frames above are suspended in their calls.
/// The words below the spill may change (the thread may wake inside `wait`)
/// and are not read.
#[inline(never)]
pub fn run_blocked_wait<R>(block: &GcBlockState, wait: impl FnOnce() -> R) -> R {
    if !helper_unparked_read_enabled() {
        return wait();
    }
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
    let mut spill = [0usize; 8];
    #[cfg(target_arch = "x86_64")]
    // SAFETY: eight stores into the local array `spill` (64 bytes); no
    // register is changed and the stack is not touched. The pointer is in
    // `rax`, caller-saved, so no callee-saved register is borrowed to hold it
    // (the `panama_libffi::CalleeSavedSpill::capture` sequence).
    unsafe {
        core::arch::asm!(
            "mov qword ptr [rax], rbx",
            "mov qword ptr [rax + 8], rbp",
            "mov qword ptr [rax + 16], rdi",
            "mov qword ptr [rax + 24], rsi",
            "mov qword ptr [rax + 32], r12",
            "mov qword ptr [rax + 40], r13",
            "mov qword ptr [rax + 48], r14",
            "mov qword ptr [rax + 56], r15",
            in("rax") spill.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
    let lo = std::hint::black_box(&spill) as *const [usize; 8] as usize & !7;
    if lo == 0 {
        return wait();
    }
    /// Withdraws the band on every path out of `wait`, an unwind included.
    struct Withdraw<'a>(&'a GcBlockState, usize);
    impl Drop for Withdraw<'_> {
        fn drop(&mut self) {
            self.0.leave_blocked_wait_band(self.1);
        }
    }
    block.enter_blocked_wait_band(lo);
    let withdraw = Withdraw(block, lo);
    let r = wait();
    drop(withdraw);
    // Keep the spill's slot live (and unreused) until the band is withdrawn.
    std::hint::black_box(&spill);
    r
}

/// The widest band a lazy in-native claim scans (the take-over's own cap,
/// `MAX_TAKEOVER_STACK_SCAN`).
pub const LAZY_NATIVE_MAX_BAND: usize = 512 * 1024 * 1024;

/// JIT round 16 wave 14 (lane ffm3): may a claim scan `[lo, hi)`? Non-empty,
/// word-aligned at its low end, and no wider than [`LAZY_NATIVE_MAX_BAND`].
/// A band that fails is never entered (`NativeContextImpl::begin_native_region_lazy`
/// declines) and never claimed.
pub fn lazy_native_band_ok(lo: usize, hi: usize) -> bool {
    lo != 0 && lo & 7 == 0 && hi > lo && hi - lo <= LAZY_NATIVE_MAX_BAND
}

/// The wait of a thread whose lazy in-native state a pause has claimed: spin
/// briefly, then yield, then sleep in short slices (a pause lasts
/// milliseconds; the thread holds no lock while it waits).
fn lazy_native_backoff(spins: &mut u32) {
    if *spins < 64 {
        std::hint::spin_loop();
    } else if *spins < 256 {
        std::thread::yield_now();
    } else {
        std::thread::sleep(std::time::Duration::from_micros(50));
    }
    *spins = spins.saturating_add(1);
}

/// One per-thread handshake operation: run by the TARGET thread on itself,
/// at a point where it is not mid-instruction (gcg g3/k, step 1 of
/// `common-a-proposal-per-thread-handshakes`). gci i23/hs: the op gets the
/// VM the target polls for (`safepoint_check`'s `shared`), since a frame-trace
/// op publishes through it and a `'static` op cannot borrow the requester's.
///
/// gcj j3/rt3 (`gcj-j1cm-safepoint-barrier-and-registry-review-residuals`
/// item 3, decided): an op must not wait for a stop-the-world pause -- no
/// `safepoint_check`, no heap allocation that can collect, no lock a thread
/// parked at a pause can hold. Its requester is a counted, non-GC-safe
/// mutator that keeps waiting for an op the target already took even after
/// a pause was requested (`ThreadRegistry::handshake_and_wait`), so an op
/// that waited for the pause would hold it until
/// `thread_registry::HANDSHAKE_TAKEN_OP_WAIT` (1 s). The one op today, the
/// frame-trace publish (`interpreter::frame_trace_handshake_op`), only copies
/// the target's own frames into Rust buffers under short registry / trace
/// locks, so the requester's hold is that copy's length.
pub type HandshakeOp = Box<dyn FnOnce(&crate::vm::SharedVm, &mut JvmThread) + Send>;

/// The requester's side of one queued [`HandshakeOp`]: whether it ran.
///
/// States: pending, ran (the target ran it), abandoned (the target was
/// dropped first), withdrawn (the requester took it back with
/// [`HandshakeQueue::withdraw`] before the target dequeued it). Exactly one
/// of the last three ever follows pending.
pub struct HandshakeTicket {
    state: PLMutex<HandshakeState>,
    settled: PLCondvar,
}

/// See [`HandshakeTicket`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeState {
    Pending,
    Ran,
    Abandoned,
    Withdrawn,
}

impl HandshakeTicket {
    fn new() -> Self {
        Self {
            state: PLMutex::new(HandshakeState::Pending),
            settled: PLCondvar::new(),
        }
    }

    fn settle(&self, to: HandshakeState) {
        let mut state = self.state.lock();
        if *state == HandshakeState::Pending {
            *state = to;
            self.settled.notify_all();
        }
    }

    /// The ticket's state now.
    pub fn state(&self) -> HandshakeState {
        *self.state.lock()
    }

    /// Wait up to `timeout` for the operation to settle; answers the state
    /// then (still `Pending` on a timeout).
    pub fn wait_timeout(&self, timeout: std::time::Duration) -> HandshakeState {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.state.lock();
        while *state == HandshakeState::Pending {
            if self.settled.wait_until(&mut state, deadline).timed_out() {
                break;
            }
        }
        *state
    }
}

/// A thread's queue of [`HandshakeOp`]s (gcg g3/k). `pending` is the one word
/// the owner's poll reads (`Acquire`), raised under the queue lock after a
/// push and lowered under it by the drain, so a raised word always has an op
/// behind it or is about to be lowered by a drain that took it. The lock is a
/// leaf: no other lock is taken under it, and no op runs under it.
pub struct HandshakeQueue {
    pending: AtomicBool,
    ops: PLMutex<Vec<(HandshakeOp, Arc<HandshakeTicket>)>>,
}

impl HandshakeQueue {
    pub fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            ops: PLMutex::new(Vec::new()),
        }
    }

    /// Is any operation queued? One `Acquire` load: the owner's poll.
    #[inline]
    pub fn is_pending(&self) -> bool {
        self.pending.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Queue `op` for the owner; the returned ticket settles when it runs, is
    /// abandoned, or is withdrawn.
    pub fn push(&self, op: HandshakeOp) -> Arc<HandshakeTicket> {
        let ticket = Arc::new(HandshakeTicket::new());
        let mut ops = self.ops.lock();
        ops.push((op, Arc::clone(&ticket)));
        self.pending
            .store(true, std::sync::atomic::Ordering::Release);
        ticket
    }

    /// Take every queued op (lowering `pending`), for the owner to run with
    /// the lock released.
    fn take_all(&self) -> Vec<(HandshakeOp, Arc<HandshakeTicket>)> {
        if !self.is_pending() {
            return Vec::new();
        }
        let mut ops = self.ops.lock();
        self.pending
            .store(false, std::sync::atomic::Ordering::Release);
        std::mem::take(&mut *ops)
    }

    /// The requester takes `ticket`'s op back if the owner has not dequeued
    /// it yet (settling the ticket `Withdrawn`), e.g. to run the
    /// blocked-target path itself after the target parked. `None`: the owner
    /// already took it (it has run or is running) or it was abandoned.
    pub fn withdraw(&self, ticket: &Arc<HandshakeTicket>) -> Option<HandshakeOp> {
        let mut ops = self.ops.lock();
        let i = ops.iter().position(|(_, t)| Arc::ptr_eq(t, ticket))?;
        let (op, t) = ops.swap_remove(i);
        if ops.is_empty() {
            self.pending
                .store(false, std::sync::atomic::Ordering::Release);
        }
        drop(ops);
        t.settle(HandshakeState::Withdrawn);
        Some(op)
    }

    /// The owner is gone: settle every queued op `Abandoned` without running
    /// it.
    pub fn abandon_all(&self) {
        let abandoned = {
            let mut ops = self.ops.lock();
            self.pending
                .store(false, std::sync::atomic::Ordering::Release);
            std::mem::take(&mut *ops)
        };
        for (op, ticket) in abandoned {
            drop(op);
            ticket.settle(HandshakeState::Abandoned);
        }
    }
}

impl Default for HandshakeQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// gci i23/hs: a dequeued op's ticket, settled `Abandoned` when dropped
/// before the op settled it `Ran` (an op that unwound, or one after it).
/// `settle` is a no-op on a settled ticket.
struct SettleOnUnwind(Arc<HandshakeTicket>);

impl Drop for SettleOnUnwind {
    fn drop(&mut self) {
        self.0.settle(HandshakeState::Abandoned);
    }
}

/// The "armed, not yet deposited" value of
/// [`GcBlockState::monitor_block_proven_jit_depth`]. Never a depth a reader
/// credits.
pub const MONITOR_BLOCK_PROOF_ARMED: usize = usize::MAX;

/// Interpreter round i4 wave 99, lane L1
/// (`CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT`;
/// `docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md`):
/// everything the root snapshot a `begin_blocking_region*` entry deposited is
/// a function of, read at that entry; [`GcBlockState::blocking_leave_key`].
/// The region's leave keeps that snapshot (no frame audit, no rebuild) only
/// when the key it reads after the flag-clear is EQUAL to this one -- see the
/// soundness argument at `vm_exec::blocking_leave_key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockingLeaveKey {
    /// `GcBarrier::gc_generation`, read BEFORE the entry's deposit: every
    /// completed pause moves it.
    pub gc_generation: u64,
    /// The heap's collection count (before the deposit).
    pub collections: u64,
    /// [`GcBlockState::blocked_folds`] (before the deposit): every relocating
    /// fold into this thread moves it.
    pub blocked_folds: u64,
    /// `class_redefinition_count` (before the deposit): a redefinition may
    /// convert the frames.
    pub redefinitions: u64,
    /// `JvmThread::root_publish_seq` AFTER the entry's deposit: any other
    /// deposit of the thread since (a nested region, a JNI in-native entry,
    /// the debugger park's reuse) moves it.
    pub publish_seq: u64,
    /// `frames.len()`: a frame pushed or popped.
    pub frames_len: usize,
    /// The top frame's `(seq, exec_epoch, pc, operand-stack depth)`: the only
    /// frame the owning thread can change without pushing or popping one.
    pub top_frame: [u64; 4],
    /// Every off-frame reference the deposit published, in its order: the
    /// `JvmThread` fields (`memory::roots::push_off_frame_thread_roots`), the
    /// JNI local references and the JIT's stashed deopt objects.
    pub off_frame: Vec<usize>,
}

impl GcBlockState {
    /// gen r5w2/roots6: has the `JvmThread` that owns this state been dropped?
    ///
    /// The registry publishes raw addresses INTO that `JvmThread` (its `Tlab`,
    /// the whole struct) for the collector to read, and clears them on the
    /// thread's normal exit. A platform thread whose closure UNWINDS drops its
    /// `JvmThread` (a stack local) without that clear, and the entry stays
    /// alive until the OS thread finishes -- so a pause in between read the
    /// dead frame (`collect_reserved_tlab_tails` published whatever `(cursor,
    /// end)` it found there as a TLAB skip span). The drop now raises this
    /// flag first (`JvmThread::gc_block_state_drop_notice`), and every reader
    /// of a published
    /// address skips an entry that has it. See
    /// `docs/internal/gc/gengc-r5w1-crash5-an-unwinding-carrier-publishes-a-dropped-jvmthread-FIXED-20260927.md`.
    #[inline]
    pub fn owner_dropped(&self) -> bool {
        self.owner_dropped
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// gci i20/jt: is this thread BLOCKED for a stop-the-world census
    /// (excluded from `expected`)? Its blocked flag, unless a lock-free
    /// in-native leave has raised `in_native_leaving` -- read FIRST (Acquire),
    /// so a census that reads the leaver's Release reset of the word reads its
    /// flag already down (`GcBarrier::leave_in_native_lockfree_if`).
    #[inline]
    pub fn census_blocked(&self) -> bool {
        !self.in_native_leaving.load(std::sync::atomic::Ordering::Acquire)
            && self.in_blocked_region.load(std::sync::atomic::Ordering::Acquire)
    }

    /// gcj j2/cm2 (`CRATONVM_GC_HANDSHAKE_TARGET_KICK`, opt-in): publish `word`
    /// as the poll word this thread's interpreter loops run on. Called by the
    /// OWNING thread at a dispatch-loop entry with the word that loop reads
    /// (`GcBarrier::loop_poll_word_for_this_thread`); one relaxed compare when
    /// it is already published, the lock only when it changed (the first loop,
    /// or a virtual thread mounted on another carrier). A word that is not this
    /// OS thread's own (the barrier's always-raised fallback, once the
    /// thread-local is gone) is published as none.
    #[inline]
    pub(crate) fn publish_loop_poll_word(
        &self,
        word: *const crate::threading::gc_barrier::LoopPollWord,
    ) {
        use std::sync::atomic::Ordering;
        if self.loop_poll_word_addr.load(Ordering::Relaxed) == word as usize {
            return;
        }
        let own = crate::threading::gc_barrier::this_threads_loop_poll_word()
            .filter(|w| Arc::as_ptr(w) == word);
        *self.loop_poll_word.lock() = own;
        self.loop_poll_word_addr
            .store(word as usize, Ordering::Relaxed);
    }

    /// gcj j2/cm2 (`CRATONVM_GC_HANDSHAKE_TARGET_KICK`, opt-in): kick the poll
    /// word this thread last published (`LoopPollWord::kick`), so its running
    /// interpreter loop polls its handshake queue at its next bytecode.
    /// `false` when nothing is published or its OS thread has ended: the
    /// caller then kicks every loop. Called by a handshake REQUESTER after it
    /// queued its op; a target that moved to another carrier since its last
    /// loop entry is kicked on the old one and serves at its next poll, or the
    /// requester falls back to its pause at the budget, as without a kick.
    pub(crate) fn kick_published_loop_poll_word(&self) -> bool {
        self.loop_poll_word
            .lock()
            .as_ref()
            .is_some_and(|word| word.kick())
    }

    pub fn new() -> Self {
        Self {
            owner_dropped: AtomicBool::new(false),
            in_blocked_region: AtomicBool::new(false),
            in_native_leaving: AtomicBool::new(false),
            loop_poll_word: PLMutex::new(None),
            loop_poll_word_addr: AtomicUsize::new(0),
            java_state: AtomicU8::new(0),
            java_wait_kind: AtomicU8::new(JAVA_WAIT_KIND_NONE),
            fixup: PLMutex::new(cratonvm_types::PointerMap::default()),
            slot_origins: PLMutex::new(Vec::new()),
            deferred_jit_chain: PLMutex::new(None),
            deposit_walked_jit_chain: AtomicBool::new(false),
            native_slots: PLMutex::new(Vec::new()),
            debugger_inspect: std::sync::atomic::AtomicUsize::new(0),
            debugger_native_ack: AtomicBool::new(false),
            agent_requests: AtomicU32::new(0),
            debugger_suspend_word: AtomicU32::new(0),
            jvmti_in_native: AtomicBool::new(false),
            debugger_native_window: std::sync::atomic::AtomicUsize::new(0),
            debugger_native_frames: std::sync::atomic::AtomicUsize::new(0),
            debugger_native_depth: std::sync::atomic::AtomicUsize::new(0),
            debugger_native_callback: std::sync::atomic::AtomicUsize::new(0),
            funnel_native_callback: std::sync::atomic::AtomicUsize::new(0),
            dump_traces_gen: std::sync::atomic::AtomicU64::new(0),
            debugger_native_frames_gen: std::sync::atomic::AtomicU64::new(0),
            debugger_compiled: PLMutex::new(BlockedCompiledView {
                rows: Vec::new(),
                stale: Vec::new(),
                homes: Vec::new(),
            }),
            debugger_native_compiled: PLMutex::new(BlockedCompiledView {
                rows: Vec::new(),
                stale: Vec::new(),
                homes: Vec::new(),
            }),
            debugger_native_chain: PLMutex::new(Default::default()),
            native_rows: PLMutex::new(NativeRowsSnapshot::default()),
            // Frames the owning thread builds from now on carry at least this
            // stamp (as `JvmThread::redefinitions_seen` starts).
            obsolete_frame_floor: std::sync::atomic::AtomicU64::new(
                cratonvm_classloading::class_redefinition_count(),
            ),
            blocked_frame_classes: PLMutex::new(BlockedFrameClasses::default()),
            monitor_block_proven_jit_depth: std::sync::atomic::AtomicUsize::new(0),
            blocked_folds: std::sync::atomic::AtomicU64::new(0),
            folds_at_last_wake: std::sync::atomic::AtomicU64::new(0),
            blocking_leave_key: PLMutex::new(None),
            deposit_conservative: AtomicBool::new(false),
            continuation_entered_compiled: AtomicBool::new(false),
            precise_deposit_words: PLMutex::new(Vec::new()),
            handshakes: HandshakeQueue::new(),
            foreign_native: AtomicBool::new(false),
            lazy_native: AtomicU32::new(LAZY_NATIVE_OFF),
            lazy_native_lo: AtomicUsize::new(0),
            lazy_native_hi: AtomicUsize::new(0),
            lazy_native_deposit_eligible: AtomicBool::new(false),
            lazy_native_machine_roots_absent: AtomicBool::new(false),
            lazy_native_precise_claim: AtomicBool::new(false),
            blocked_wait_lo: AtomicUsize::new(0),
            in_native_band_lo: AtomicUsize::new(0),
            in_native_band_hi: AtomicUsize::new(0),
            in_native_proven_jit_depth: AtomicUsize::new(0),
            in_native_credit_stamp: std::sync::atomic::AtomicU64::new(0),
            in_native_tlab_kept: AtomicBool::new(false),
            jni_local_frames_addr: AtomicUsize::new(0),
            jni_local_frames_os_tid: AtomicU32::new(0),
            deopt_stash_cell_addr: AtomicUsize::new(0),
            deopt_exceptional_cell_addr: AtomicUsize::new(0),
            deopt_stash_os_tid: AtomicU32::new(0),
            native_excuse: NativeExcuse::new(),
        }
    }

    /// gci i9/jw: record that the OS thread `os_tid` holds this `JvmThread`'s
    /// JNI local references in the table at `addr`. A no-op when that pair is
    /// already published (every JNI native dispatch calls this; the stores
    /// happen once per OS thread the `JvmThread` runs JNI on). The address is
    /// withdrawn while the tid changes, so a reader never pairs a new tid with
    /// an old address. Called only by the owning thread.
    pub fn publish_jni_local_frames(&self, os_tid: u32, addr: usize) {
        use std::sync::atomic::Ordering;
        if os_tid == 0 || addr == 0 {
            return;
        }
        if self.jni_local_frames_addr.load(Ordering::Relaxed) == addr
            && self.jni_local_frames_os_tid.load(Ordering::Relaxed) == os_tid
        {
            return;
        }
        self.jni_local_frames_addr.store(0, Ordering::Release);
        self.jni_local_frames_os_tid.store(os_tid, Ordering::Release);
        self.jni_local_frames_addr.store(addr, Ordering::Release);
    }

    /// gci i9/jw: the `(os_tid, table address)` pair
    /// [`Self::publish_jni_local_frames`] last stored, `None` when none is
    /// published (or a publication raced this read). The caller may
    /// dereference the address only while it holds `os_tid` frozen.
    pub fn published_jni_local_frames(&self) -> Option<(u32, usize)> {
        use std::sync::atomic::Ordering;
        let addr = self.jni_local_frames_addr.load(Ordering::Acquire);
        if addr == 0 {
            return None;
        }
        let os_tid = self.jni_local_frames_os_tid.load(Ordering::Acquire);
        if os_tid == 0 || self.jni_local_frames_addr.load(Ordering::Acquire) != addr {
            return None;
        }
        Some((os_tid, addr))
    }

    /// gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`):
    /// record that the OS thread `os_tid` holds this thread's JIT deopt stashes
    /// in the cells at `deopt` / `exceptional`
    /// (`cratonvm_jit::deopt::stash_cell_addrs` on that OS thread). Same
    /// discipline as [`Self::publish_jni_local_frames`]: a no-op when the
    /// triple is unchanged, and the addresses are withdrawn while the tid
    /// changes. Called only by the OS thread `os_tid` itself.
    pub fn publish_deopt_stash_cells(&self, os_tid: u32, deopt: usize, exceptional: usize) {
        use std::sync::atomic::Ordering;
        if os_tid == 0 || deopt == 0 || exceptional == 0 {
            return;
        }
        if self.deopt_stash_cell_addr.load(Ordering::Relaxed) == deopt
            && self.deopt_exceptional_cell_addr.load(Ordering::Relaxed) == exceptional
            && self.deopt_stash_os_tid.load(Ordering::Relaxed) == os_tid
        {
            return;
        }
        self.deopt_stash_cell_addr.store(0, Ordering::Release);
        self.deopt_stash_os_tid.store(os_tid, Ordering::Release);
        self.deopt_exceptional_cell_addr.store(exceptional, Ordering::Release);
        self.deopt_stash_cell_addr.store(deopt, Ordering::Release);
    }

    /// gci i9/ds: the `(os_tid, LAST_DEOPT cell, LAST_EXCEPTIONAL cell)`
    /// [`Self::publish_deopt_stash_cells`] last stored; `None` when none is
    /// published or a publication raced this read. The caller may dereference
    /// the cells only while it holds `os_tid` frozen.
    pub fn published_deopt_stash_cells(&self) -> Option<(u32, usize, usize)> {
        use std::sync::atomic::Ordering;
        let deopt = self.deopt_stash_cell_addr.load(Ordering::Acquire);
        if deopt == 0 {
            return None;
        }
        let os_tid = self.deopt_stash_os_tid.load(Ordering::Acquire);
        let exceptional = self.deopt_exceptional_cell_addr.load(Ordering::Acquire);
        if os_tid == 0
            || exceptional == 0
            || self.deopt_stash_cell_addr.load(Ordering::Acquire) != deopt
        {
            return None;
        }
        Some((os_tid, deopt, exceptional))
    }

    /// JIT round 18 wave 10 (lane ffmeintr, EINTR2-1): the collector holds
    /// the band of a thread inside a blocked-wait trampoline: `lo -> lo | 1`.
    /// `Some(lo)` when it did; until [`Self::release_blocked_wait_band`] the
    /// thread cannot leave the trampoline, so `[lo, stack top)` is frozen.
    /// `None` when the thread is not in a trampoline (or another holder has
    /// it): the caller parks it as before.
    pub fn try_hold_blocked_wait_band(&self) -> Option<usize> {
        use std::sync::atomic::Ordering;
        let lo = self.blocked_wait_lo.load(Ordering::Acquire);
        if lo == 0 || lo & 1 != 0 {
            return None;
        }
        self.blocked_wait_lo
            .compare_exchange(lo, lo | 1, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| lo)
    }

    /// The collector is done reading the band it held: `lo | 1 -> lo`.
    pub fn release_blocked_wait_band(&self, lo: usize) {
        use std::sync::atomic::Ordering;
        let _ =
            self.blocked_wait_lo
                .compare_exchange(lo | 1, lo, Ordering::AcqRel, Ordering::Acquire);
    }

    /// The owning thread enters a blocked-wait trampoline whose band starts
    /// at `lo` (8-aligned, non-zero). Release: a holder that sees `lo` sees
    /// the spill stored before it.
    fn enter_blocked_wait_band(&self, lo: usize) {
        self.blocked_wait_lo
            .store(lo, std::sync::atomic::Ordering::Release);
    }

    /// The owning thread leaves its trampoline: `lo -> 0`, waiting out a
    /// collector that holds the band (microseconds: one band copy).
    fn leave_blocked_wait_band(&self, lo: usize) {
        use std::sync::atomic::Ordering;
        let mut spins = 0u32;
        loop {
            match self
                .blocked_wait_lo
                .compare_exchange(lo, 0, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(seen) if seen == (lo | 1) => lazy_native_backoff(&mut spins),
                // Not ours (never entered, or already cleared): nothing to do.
                Err(_) => return,
            }
        }
    }

    /// JIT round 16 wave 14 (lane ffm3): the owning thread goes into C lazily.
    /// `[lo, hi)` is its frozen stack band ([`lazy_native_band_ok`]); the
    /// band is published first and the state last (Release), so a claim that
    /// sees [`LAZY_NATIVE_IN`] sees this band. One release store of the
    /// state, as HotSpot's transition to `_thread_in_native`; the thread
    /// stays counted, so a pause that does not claim it waits for it as for
    /// any running native.
    ///
    /// JIT round 18 wave 10b (lane ffmeintr): a compare-exchange `OFF -> IN`,
    /// not a store, so it cannot pass a take-over pass that is signalling
    /// this thread ([`LAZY_NATIVE_SIGNALLING`]); it waits for that pass's
    /// answer instead, and the signal lands here, in Rust, not in C.
    #[inline]
    pub fn enter_lazy_native(&self, lo: usize, hi: usize) {
        use std::sync::atomic::Ordering;
        self.lazy_native_lo.store(lo, Ordering::Relaxed);
        self.lazy_native_hi.store(hi, Ordering::Relaxed);
        match self.lazy_native.compare_exchange(
            LAZY_NATIVE_OFF,
            LAZY_NATIVE_IN,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(LAZY_NATIVE_SIGNALLING) => self.enter_lazy_native_gated(),
            // Not reachable by the protocol (an entry without a leave); the
            // pre-gate behaviour.
            Err(_) => self.lazy_native.store(LAZY_NATIVE_IN, Ordering::Release),
        }
    }

    #[cold]
    #[inline(never)]
    fn enter_lazy_native_gated(&self) {
        use std::sync::atomic::Ordering;
        let mut spins = 0u32;
        // JIT round 19 wave 1 (lane eintr19, FFMEINTR18-5): the gate is held
        // for one signal round trip; one stderr line if it outlives the
        // last-resort stall threshold (a gate nobody reopened).
        let stall = crate::threading::thread_registry::stw_stall_report_after()
            .map(|after| (std::time::Instant::now(), after));
        let mut reported = false;
        loop {
            match self.lazy_native.compare_exchange(
                LAZY_NATIVE_OFF,
                LAZY_NATIVE_IN,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(LAZY_NATIVE_SIGNALLING) => {
                    if let Some((t0, after)) = stall {
                        if !reported && t0.elapsed() >= after {
                            reported = true;
                            eprintln!(
                                "[stw-stall] lazy in-native entry has waited {:?} for a take-over \
                                 pass to reopen its signal gate (state=SIGNALLING; \
                                 CRATONVM_STW_STALL_REPORT=0 silences)",
                                t0.elapsed()
                            );
                        }
                    }
                    lazy_native_backoff(&mut spins)
                }
                Err(_) => {
                    self.lazy_native.store(LAZY_NATIVE_IN, Ordering::Release);
                    return;
                }
            }
        }
    }

    /// JIT round 18 wave 10b (lane ffmeintr): the Linux take-over pass is
    /// about to signal this thread. `OFF -> SIGNALLING` closes its lazy
    /// in-native entry until [`Self::ungate_takeover_signal`]; a thread
    /// already `IN` C is reported, not signalled.
    pub fn try_gate_takeover_signal(&self) -> TakeoverSignalGate {
        use std::sync::atomic::Ordering;
        match self.lazy_native.compare_exchange(
            LAZY_NATIVE_OFF,
            LAZY_NATIVE_SIGNALLING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => TakeoverSignalGate::Gated,
            Err(LAZY_NATIVE_IN) => TakeoverSignalGate::InNative,
            Err(_) => TakeoverSignalGate::Other,
        }
    }

    /// The pass has its answer (or gave up): `SIGNALLING -> OFF`.
    pub fn ungate_takeover_signal(&self) {
        use std::sync::atomic::Ordering;
        let _ = self.lazy_native.compare_exchange(
            LAZY_NATIVE_SIGNALLING,
            LAZY_NATIVE_OFF,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// The owning thread is back from C: one compare-exchange `IN -> OFF`.
    /// When a pause has claimed the thread it waits until that pause releases
    /// it ([`Self::release_lazy_native_claim`], after the collection), then
    /// retries: until then the pause may be reading its frames, and nothing
    /// it holds may be touched. `true` if it waited.
    #[inline]
    pub fn leave_lazy_native(&self) -> bool {
        use std::sync::atomic::Ordering;
        match self.lazy_native.compare_exchange(
            LAZY_NATIVE_IN,
            LAZY_NATIVE_OFF,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => false,
            Err(LAZY_NATIVE_OFF) => false,
            Err(_) => {
                let waited = self.leave_lazy_native_slow();
                waited
            }
        }
    }

    /// Leave an unclaimed lazy-native region through the GC barrier. A pause
    /// that has disabled lazy claims waits for the C call to finish, then must
    /// keep the returning thread out of Java heap access until the pause ends.
    /// The barrier lock makes clearing `IN` and the pause census mutually
    /// exclusive. Claimed peers retain the existing handoff path.
    pub fn leave_lazy_native_at_barrier(
        &self,
        barrier: &crate::threading::GcBarrier,
    ) -> Option<bool> {
        use std::sync::atomic::Ordering;
        let mut left_unclaimed = false;
        let left = barrier.leave_lazy_native_if(|| {
            match self.lazy_native.compare_exchange(
                LAZY_NATIVE_IN,
                LAZY_NATIVE_OFF,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    left_unclaimed = true;
                    true
                }
                Err(_) => false,
            }
        });
        if left_unclaimed {
            Some(left)
        } else {
            None
        }
    }

    #[cold]
    #[inline(never)]
    fn leave_lazy_native_slow(&self) -> bool {
        use std::sync::atomic::Ordering;
        let mut waited = false;
        let mut spins = 0u32;
        // JIT round 18 wave 10 (lane ffmeintr): read once per waited leave.
        let handoff = lazy_leave_handoff_enabled();
        // `CRATONVM_DBG_STW_WATCHDOG_SECS`: one stderr line when this leave
        // has waited longer than that for the pause that claimed it.
        // JIT round 19 wave 1 (lane eintr19, FFMEINTR18-5): without the
        // watchdog, the default-on last-resort threshold
        // (`CRATONVM_STW_STALL_REPORT`): a claim nobody released is a silent
        // hang of this thread and of every later pause.
        let watchdog = crate::threading::thread_registry::stw_watchdog_secs()
            .map(std::time::Duration::from_secs)
            .or_else(crate::threading::thread_registry::stw_stall_report_after)
            .map(|limit| (std::time::Instant::now(), limit));
        let mut reported = false;
        loop {
            match self.lazy_native.compare_exchange(
                LAZY_NATIVE_IN,
                LAZY_NATIVE_OFF,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return waited,
                // Not in a lazy region at all (a leave without an entry), or
                // a release that completed this leave (the handoff): nothing
                // left to do.
                Err(LAZY_NATIVE_OFF) => return waited,
                Err(seen) => {
                    // JIT round 18 wave 10 (lane ffmeintr): say "back from C,
                    // waiting" so the release hands the leave over. Before,
                    // the release put the thread back to `IN`, and under load
                    // the NEXT pause (an allocation loop's `System.gc()` is
                    // microseconds away) claimed it again before it was
                    // scheduled to make its own compare-exchange -- one
                    // downcall return could wait out pause after pause
                    // (`docs/internal/fixed-bugs/r17-ffm3-eintr-race-rare-silent-hang-in-the-battery-FIXED-20261007.md`).
                    // HotSpot's `_thread_in_native_trans` is the same rule: a
                    // thread on its way back from native is not safepoint-safe
                    // again, the next safepoint waits for it to reach a poll.
                    if seen == LAZY_NATIVE_CLAIMED && handoff {
                        let _ = self.lazy_native.compare_exchange(
                            LAZY_NATIVE_CLAIMED,
                            LAZY_NATIVE_CLAIMED_LEAVING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                    waited = true;
                    if let Some((t0, limit)) = watchdog {
                        if !reported && t0.elapsed() >= limit {
                            reported = true;
                            eprintln!(
                                "[stw-watchdog] lazy in-native leave waited {:?} for the pause that \
                                 claimed it (state={}, handoff={handoff})",
                                t0.elapsed(),
                                self.lazy_native.load(Ordering::Acquire)
                            );
                        }
                    }
                    lazy_native_backoff(&mut spins);
                }
            }
        }
    }

    /// A pause claims a thread in a lazy in-native downcall: `IN -> CLAIMED`.
    /// `Some(band)` when it did; the thread then stays in C (or waits at its
    /// leave) until [`Self::release_lazy_native_claim`]. A claim whose band
    /// fails [`lazy_native_band_ok`] is given straight back (`None`), so the
    /// pause waits for that thread as before.
    pub fn try_claim_lazy_native(&self) -> Option<(usize, usize)> {
        use std::sync::atomic::Ordering;
        if self.lazy_native.load(Ordering::Acquire) != LAZY_NATIVE_IN {
            return None;
        }
        self.lazy_native
            .compare_exchange(
                LAZY_NATIVE_IN,
                LAZY_NATIVE_CLAIMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;
        let lo = self.lazy_native_lo.load(Ordering::Relaxed);
        let hi = self.lazy_native_hi.load(Ordering::Relaxed);
        if lazy_native_band_ok(lo, hi) {
            Some((lo, hi))
        } else {
            self.release_lazy_native_claim();
            None
        }
    }

    /// The pause that claimed this thread is done with it: `CLAIMED -> IN`
    /// (Release: the thread's leave acquires what the collection wrote).
    /// `false` if it was not claimed.
    ///
    /// JIT round 18 wave 10 (lane ffmeintr): a thread that is already back
    /// from C and waiting at its leave ([`LAZY_NATIVE_CLAIMED_LEAVING`]) is
    /// handed the leave instead: `CLAIMED_LEAVING -> OFF`. It is then an
    /// ordinary counted mutator in Rust, which the next pause waits for at a
    /// poll (it cannot be claimed again: it is no longer in C). Sound for the
    /// same reason the thread's own `IN -> OFF` is: the collection is over,
    /// and the thread has run nothing since its C code returned.
    pub fn release_lazy_native_claim(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self
            .lazy_native
            .compare_exchange(
                LAZY_NATIVE_CLAIMED,
                LAZY_NATIVE_IN,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            return true;
        }
        self.lazy_native
            .compare_exchange(
                LAZY_NATIVE_CLAIMED_LEAVING,
                LAZY_NATIVE_OFF,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }
}

impl Default for GcBlockState {
    fn default() -> Self {
        Self::new()
    }
}

/// gen r5w2/roots6: a field of [`JvmThread`] whose `Drop` raises
/// [`GcBlockState::owner_dropped`] on the thread's shared block state, so a
/// registry reader never dereferences an address published into a
/// `JvmThread` that is gone -- including one dropped by an UNWIND, which
/// skips the normal-exit clear.
///
/// A field rather than `impl Drop for JvmThread`, so no code that moves a
/// field out of a `JvmThread` has to change. Moving the `JvmThread` (into a
/// `Box` on a yield) moves this too and drops nothing.
pub struct GcBlockStateDropNotice(Arc<GcBlockState>);

impl GcBlockStateDropNotice {
    /// A notice for `state`, which must be the owning `JvmThread`'s
    /// `gc_block_state`.
    pub fn new(state: &Arc<GcBlockState>) -> Self {
        Self(Arc::clone(state))
    }
}

impl Drop for GcBlockStateDropNotice {
    fn drop(&mut self) {
        self.0
            .owner_dropped
            .store(true, std::sync::atomic::Ordering::Release);
        // gcg g3/k: no handshake queued for this thread can run any more.
        self.0.handshakes.abandon_all();
    }
}

impl std::fmt::Debug for GcBlockStateDropNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GcBlockStateDropNotice")
    }
}

// ---------------------------------------------------------------------------
// win_park — accurate timed parking on Windows
// ---------------------------------------------------------------------------

/// Windows timed waits are aligned to the system clock tick (15.625 ms by
/// default), and a parked thread's *deadline* inherits that alignment:
/// `WaitForSingleObject`, `SleepConditionVariableSRW`, a plain waitable timer
/// and `parking_lot`'s condvar all return at the first tick at or after the
/// requested instant, so a 50 ms wait measures 62.5 ms. `timeBeginPeriod(1)`
/// does NOT lift it — measured on this host, every one of those primitives sat
/// at p50 = 62.3 ms for a 50 ms request both before and after that call, with
/// the system-wide resolution already reported as 1 ms.
///
/// The one primitive that is accurate is a waitable timer created with
/// `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (Windows 10 1803+): p50 = 50.3 ms
/// for the same request. It is what Rust's own `std::thread::sleep` uses,
/// which is why `Thread.sleep` was already accurate here while every
/// `LockSupport.parkNanos` deadline was not.
///
/// The visible cost of the tick alignment is not a slow park — the mean is
/// right, because a scheduler like netty's re-arms from an ABSOLUTE deadline —
/// it is a *periodic* one: four short cycles then one long one. A 50 ms
/// fixed-rate task fires at 62.5, 109.4, 156.3, 203.1, 250.0 ms, i.e. gaps of
/// 46.9 ms x4 then 62.5 ms. `AutoScalingEventExecutorChooserFactoryTest`
/// polls its group every 50 ms and asserts on a state its monitor holds for
/// exactly one cycle; a 46.9 ms cycle is shorter than the poll, so the state
/// can be stepped over entirely and the test reads the NEXT one.
///
/// So the wait is bounded by a high-resolution timer and the unpark signal by
/// an auto-reset event, and both are waited on together.
/// `CRATONVM_WIN_HIRES_PARK=0` reverts to the condvar path.
#[cfg(target_os = "windows")]
pub(crate) mod win_park {
    use std::time::Duration;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateEventW(
            attrs: *mut core::ffi::c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> isize;
        fn SetEvent(handle: isize) -> i32;
        fn ResetEvent(handle: isize) -> i32;
        fn CloseHandle(handle: isize) -> i32;
        fn CreateWaitableTimerExW(
            attrs: *mut core::ffi::c_void,
            name: *const u16,
            flags: u32,
            desired_access: u32,
        ) -> isize;
        fn SetWaitableTimer(
            timer: isize,
            due_time: *const i64,
            period: i32,
            routine: *mut core::ffi::c_void,
            arg: *mut core::ffi::c_void,
            resume: i32,
        ) -> i32;
        fn CancelWaitableTimer(timer: isize) -> i32;
        fn WaitForMultipleObjects(
            count: u32,
            handles: *const isize,
            wait_all: i32,
            millis: u32,
        ) -> u32;
    }

    const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x0000_0002;
    const TIMER_ALL_ACCESS: u32 = 0x001F_0003;
    const INFINITE: u32 = 0xFFFF_FFFF;

    /// `CRATONVM_WIN_HIRES_PARK=0` reverts every timed park to the condvar
    /// path this module replaces (the pre-2026-08-29 behaviour).
    pub(crate) fn enabled() -> bool {
        use std::sync::OnceLock;
        static G: OnceLock<bool> = OnceLock::new();
        *G.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_WIN_HIRES_PARK")
                .map(|v| v != *"0")
                .unwrap_or(true)
        })
    }

    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
        fn timeEndPeriod(period: u32) -> u32;
    }

    /// HotSpot 25's Windows `HighResolutionInterval` (os_windows.cpp), held
    /// around one wait (interpreter round i5 wave 109, lane L6,
    /// `CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL`, default off): a wait of
    /// `millis` that is not a multiple of 10 raises the system timer
    /// resolution to 1 ms (`timeBeginPeriod(1)`) and drops it again on drop
    /// (`timeEndPeriod(1)`), as `PlatformEvent::park(jlong)` does for every
    /// `Thread.sleep` and timed `Object.wait` there. It does not make the
    /// wait itself more accurate (the high-resolution timer already is, see
    /// the module comment): what it changes is the clock interrupt rate for
    /// the wait's length, and with it how deep the idle processors sleep --
    /// i.e. how fast a thread readied right after the sleep (the sleeper's
    /// `unpark` of a parked peer) starts running.
    pub(crate) struct HighResolutionInterval {
        raised: bool,
    }

    impl HighResolutionInterval {
        /// Raise the resolution for a wait of `millis` ms when HotSpot would
        /// (`millis % 10 != 0`).
        pub(crate) fn new(millis: u64) -> Self {
            if millis % 10 == 0 {
                return Self { raised: false };
            }
            // SAFETY: a documented winmm call with a constant argument;
            // `TIMERR_NOERROR` (0) means it must be paired with
            // `timeEndPeriod(1)`, which `Drop` does.
            let rc = unsafe { timeBeginPeriod(1) };
            Self { raised: rc == 0 }
        }
    }

    impl Drop for HighResolutionInterval {
        fn drop(&mut self) {
            if self.raised {
                // SAFETY: pairs the successful `timeBeginPeriod(1)` above.
                unsafe { timeEndPeriod(1) };
            }
        }
    }

    /// An auto-reset event for one `ParkState`, or 0 if it could not be made
    /// (every caller then takes the condvar path).
    pub(crate) fn create_wake_event() -> isize {
        if !enabled() {
            return 0;
        }
        // SAFETY: a documented Win32 call with a null security descriptor and
        // a null name; it only creates a kernel object and returns its handle.
        unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) }
    }

    /// Release a handle from `create_wake_event`.
    pub(crate) fn close(handle: isize) {
        if handle != 0 {
            // SAFETY: `handle` came from `CreateEventW` above and is closed
            // exactly once, from `ParkState`'s `Drop`.
            unsafe { CloseHandle(handle) };
        }
    }

    /// Signal a parked thread's wake event. Since interpreter round i3 wave
    /// 73 (lane L6; page
    /// `i72-L6-windows-park-event-left-signalled-by-a-lazy-park-20261105`)
    /// `ParkState` signals only while a parker is on the accurate path
    /// (`ParkState::event_waiter`), and resets a signal that wait did not
    /// consume, so a permit taken off the event path (the permit fast path,
    /// the lazy running phase) no longer leaves the event set for the
    /// thread's next timed park to wake on spuriously.
    pub(crate) fn signal(handle: isize) {
        if handle != 0 {
            // SAFETY: `handle` is a live auto-reset event owned by the
            // `ParkState` this call reached through an `Arc`.
            unsafe { SetEvent(handle) };
        }
    }

    /// Clear a signal nobody consumed (interpreter round i3 wave 73, lane
    /// L6): `ParkState::accurate_wait` after its wait ended on the timer, or
    /// never started, while an `unpark` had signalled the event.
    pub(crate) fn reset(handle: isize) {
        if handle != 0 {
            // SAFETY: as in `signal`: a live auto-reset event owned by the
            // `ParkState` the caller borrows.
            unsafe { ResetEvent(handle) };
        }
    }

    /// Test hook: is the event signalled now? Consumes the signal (an
    /// auto-reset event), as a wait would.
    #[cfg(test)]
    pub(crate) fn is_signalled(handle: isize) -> bool {
        if handle == 0 {
            return false;
        }
        let handles = [handle];
        // SAFETY: a live event handle in a one-element array; a zero timeout
        // only polls.
        unsafe { WaitForMultipleObjects(1, handles.as_ptr(), 0, 0) == 0 }
    }

    thread_local! {
        /// One high-resolution timer per parking thread, reused across parks.
        /// `-1` means "not probed yet"; `0` means the OS refused one, so the
        /// probe runs once per thread and never again.
        static HIRES_TIMER: std::cell::Cell<isize> = const { std::cell::Cell::new(-1) };
    }

    /// This thread's high-resolution timer, or 0 if the OS has none.
    fn timer() -> isize {
        HIRES_TIMER.with(|c| {
            let cached = c.get();
            if cached != -1 {
                return cached;
            }
            // SAFETY: documented Win32 call, null attributes and null name.
            let h = unsafe {
                CreateWaitableTimerExW(
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS,
                )
            };
            c.set(h);
            h
        })
    }

    /// How [`wait_until_reporting`] ended (interpreter round i3 wave 73, lane
    /// L6).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Woke {
        /// The accurate path is unavailable; nothing was waited on.
        Unavailable,
        /// The wake event was signalled (and the wait consumed the signal).
        Event,
        /// The timer fired (or the wait failed) without the event.
        Timer,
    }

    /// Wait until `dur` elapses or `wake_event` is signalled, whichever comes
    /// first. Returns `false` when the accurate path is unavailable and the
    /// caller must fall back to the condvar.
    pub(crate) fn wait_until(wake_event: isize, dur: Duration) -> bool {
        wait_until_reporting(wake_event, dur) != Woke::Unavailable
    }

    /// [`wait_until`], saying which handle ended the wait, so a caller can
    /// reset a signal the wait did not consume.
    pub(crate) fn wait_until_reporting(wake_event: isize, dur: Duration) -> Woke {
        if wake_event == 0 || !enabled() {
            return Woke::Unavailable;
        }
        let timer = timer();
        if timer == 0 {
            return Woke::Unavailable;
        }
        // A negative due time is a RELATIVE interval in 100 ns units.
        let hundred_nanos = (dur.as_nanos() / 100).min(i64::MAX as u128) as i64;
        let due: i64 = -hundred_nanos.max(1);
        // SAFETY: `timer` is this thread's live timer handle and `due` points
        // at a live local for the duration of the call.
        let armed = unsafe {
            SetWaitableTimer(
                timer,
                &due,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if armed == 0 {
            return Woke::Unavailable;
        }
        let handles = [wake_event, timer];
        // SAFETY: both handles are live for the call; `handles` is a valid
        // two-element array and the count matches.
        let which = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        // SAFETY: same live timer handle; cancelling an already-signalled
        // timer is defined and leaves it unsignalled for the next park.
        unsafe { CancelWaitableTimer(timer) };
        // `WAIT_OBJECT_0 + 0`: the event (the lowest index wins when both are
        // signalled, and the auto-reset event is then consumed).
        if which == 0 {
            Woke::Event
        } else {
            Woke::Timer
        }
    }

    /// One wait of at most `millis` milliseconds on `wake_event` alone, at
    /// the default system timer resolution: HotSpot 25's Windows
    /// `Parker::park` (os_windows.cpp), whose one
    /// `WaitForSingleObject(_ParkHandle, time)` neither uses a
    /// high-resolution timer nor raises the timer resolution, so it ends on
    /// a clock tick, possibly before `millis` have passed (interpreter round
    /// i4 wave 99, lane L6; page `i98-L6-windows-parknanos-whole-milliseconds`).
    /// `Woke::Unavailable` when there is no event (the caller then waits on
    /// the condvar once instead). `millis` is capped below `INFINITE`.
    pub(crate) fn wait_event_coarse(wake_event: isize, millis: u64) -> Woke {
        if wake_event == 0 {
            return Woke::Unavailable;
        }
        let millis = u32::try_from(millis).unwrap_or(INFINITE - 1).min(INFINITE - 1);
        let handles = [wake_event];
        // SAFETY: a live event handle in a one-element array; the count
        // matches and the timeout is finite.
        let which = unsafe { WaitForMultipleObjects(1, handles.as_ptr(), 0, millis) };
        if which == 0 {
            Woke::Event
        } else {
            Woke::Timer
        }
    }

    /// One untimed wait on `wake_event` alone: HotSpot 25's Windows
    /// `Parker::park` with no deadline (`WaitForSingleObject(_ParkHandle,
    /// INFINITE)`). Interpreter round i5 wave 107, lane L6
    /// (`CRATONVM_PARK_UNTIMED_EVENT`). `Woke::Unavailable` when there is no
    /// event or the wait FAILED (nothing was waited on: the caller falls back
    /// to the condvar); `Woke::Timer` cannot come back from an infinite wait
    /// otherwise (`WAIT_ABANDONED` is for mutexes), but is answered for any
    /// other code so the caller re-checks and waits again.
    pub(crate) fn wait_event_untimed(wake_event: isize) -> Woke {
        const WAIT_FAILED: u32 = 0xFFFF_FFFF;
        if wake_event == 0 {
            return Woke::Unavailable;
        }
        let handles = [wake_event];
        // SAFETY: a live event handle in a one-element array; the count
        // matches.
        let which = unsafe { WaitForMultipleObjects(1, handles.as_ptr(), 0, INFINITE) };
        match which {
            0 => Woke::Event,
            WAIT_FAILED => Woke::Unavailable,
            _ => Woke::Timer,
        }
    }
}

// ---------------------------------------------------------------------------
// ParkState — binary semaphore for LockSupport.park() / unpark()
// ---------------------------------------------------------------------------

/// Per-thread parking permit for `LockSupport.park()` / `unpark()`.
///
/// Implements a binary semaphore: `unpark()` sets the permit, `park()` consumes
/// it (or blocks until one is available). If `unpark()` is called before
/// `park()`, the next `park()` returns immediately.
/// The longest single wait of [`ParkState::park_lazily`]: how long a
/// stop-the-world pause requested during a park's running phase can wait for
/// that thread to notice it (`CRATONVM_PARK_LAZY`; interpreter round i3 wave
/// 71, lane L1). Short against the default 2 ms budget, long against a
/// condvar wake-up.
pub const LAZY_PARK_SLICE: std::time::Duration = std::time::Duration::from_micros(100);

/// The interrupt poll of a polled blocking park ([`ParkState::park_wait`]):
/// the longest condvar wait before the flag is re-read.
const PARK_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// The longest wait for a deferred wake-event signal a waker claimed but had
/// not raised yet when the wait ended another way
/// ([`ParkState::settle_unconsumed_signal`]; interpreter round i5 wave 108,
/// lane L6). The claimer raises it right after releasing the state's mutex,
/// so this is a bound for a claimer held up (preempted, suspended), not a
/// wait the common case pays.
#[cfg(target_os = "windows")]
const DEFERRED_SIGNAL_SETTLE_MS: u64 = 20;

/// How a VM's `LockSupport.unpark` sets a permit and wakes the waiter
/// ([`ParkState::unpark_with`]; interpreter round i3 wave 77, lane L6), from
/// the three per-VM park switches (`ThreadRegistry::unpark_shape`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnparkShape {
    /// `CRATONVM_PARK_ATOMIC_PERMIT`: the permit by one swap, the mutex only
    /// for a counted sleeper (on a state set up lock-free).
    pub lock_free: bool,
    /// `CRATONVM_PARK_UNTIMED_NO_POLL`: `notify_all`, not `notify_one`.
    pub wake_all: bool,
    /// `CRATONVM_PARK_WAKE_AFTER_UNLOCK`: the wake after the mutex is
    /// released, only when this call set the permit.
    pub after_unlock: bool,
}

impl UnparkShape {
    /// Every switch off: the permit and a `notify_one` under the mutex.
    pub const LOCKED: Self = Self {
        lock_free: false,
        wake_all: false,
        after_unlock: false,
    };
}

pub struct ParkState {
    /// The handshake lock of every wait: a waiter holds it from its last
    /// permit check until it is inside the condvar wait, and an `unpark`
    /// that may have a waiter to wake notifies under it. Since interpreter
    /// round i3 wave 73 (lane L6) the permit itself is [`Self::permit`];
    /// every access the mutex used to guard still happens under it on the
    /// default path ([`Self::unpark`], [`Self::take_permit`]), so that path
    /// behaves exactly as the `Mutex<bool>` did.
    mutex: PLMutex<()>,
    condvar: PLCondvar,
    /// The permit (`LockSupport`'s one-bit semaphore; HotSpot's
    /// `Parker::_counter`). Written under [`Self::mutex`] except by
    /// [`Self::unpark_lock_free`] and read off it only by
    /// [`Self::take_permit_lock_free`] (`CRATONVM_PARK_ATOMIC_PERMIT`).
    permit: AtomicBool,
    /// How many threads are inside a wait on this state (the owner's park
    /// and, rarely, a test's second waiter). Raised under the mutex BEFORE
    /// the waiter's first permit check and lowered on its way out; the
    /// Dekker pair with [`Self::unpark_lock_free`]'s permit swap (both
    /// `SeqCst`) means an unpark either sees the waiter and notifies under
    /// the mutex, or the waiter sees the permit (interpreter round i3 wave
    /// 73, lane L6; page `i72-L6-proposal-lock-free-park-permit-20261105`).
    /// Kept only when [`Self::lock_free`] is set; zero otherwise.
    sleepers: AtomicU32,
    /// `CRATONVM_PARK_ATOMIC_PERMIT` for this state (wave 73): set once by
    /// the registry ([`Self::enable_lock_free`]) before the state is
    /// published or its owner can park. Off, waits keep no sleeper count,
    /// the permit is a plain load/store under the mutex (the wave-72 cost),
    /// and [`Self::unpark_lock_free`] is [`Self::unpark`].
    lock_free: AtomicBool,
    /// DBG (`CRATONVM_DBG_PARKLAT`): monotonic nanos of the most recent
    /// `unpark()` that SET the permit (0 = none). `park_interruptible`
    /// reads it on wake-with-permit and reports the unpark→wake latency
    /// when it exceeds a threshold, separating "the signal was generated
    /// late" (Java-side / protocol) from "the signal was delivered late"
    /// (VM park machinery) in the RRWL crawl/join-stall investigation.
    last_unpark_nanos: std::sync::atomic::AtomicU64,
    /// DBG (`CRATONVM_DBG_PARKLAT`; interpreter round i5 wave 110, lane L6):
    /// monotonic nanos at which the owner's last coarse event wait
    /// ([`Self::coarse_wait`]) returned from the OS, before it re-took the
    /// mutex. Splits a timed wake's `unpark->wake` into the OS wake and the
    /// mutex re-take. Written only with the diagnostic on.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    last_os_wake_nanos: std::sync::atomic::AtomicU64,
    /// DBG (`CRATONVM_DBG_PARKLAT`; interpreter round i5 wave 114, lane L1):
    /// monotonic nanos at which a waker's `SetEvent` of this state's wake
    /// event RETURNED. Splits a wake's `unpark->os` into the waker's own leg
    /// (permit store to `SetEvent` return) and the OS wake proper (`SetEvent`
    /// return to the waiter's `WaitForMultipleObjects` return). Written only
    /// with the diagnostic on.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    last_signal_nanos: std::sync::atomic::AtomicU64,
    /// `CRATONVM_PARK_LAZY_ADAPTIVE` (interpreter round i3 wave 81, lane L6;
    /// page `i77-L6-proposal-adaptive-lazy-park-budget`): the owner's streak,
    /// on a cache line of its own ([`LazyParkStreak`]).
    lazy: LazyParkStreak,
    /// Windows only: auto-reset event this thread's timed parks wait on
    /// alongside a high-resolution timer, so the deadline is not rounded up
    /// to the 15.625 ms system tick. 0 when the event could not be created
    /// or `CRATONVM_WIN_HIRES_PARK=0` turned the path off; every timed park
    /// then takes the condvar exactly as before. See `win_park` above.
    #[cfg(target_os = "windows")]
    wake_event: isize,
    /// Windows only, under [`Self::mutex`]: a waiter is on the accurate path
    /// (waiting on [`Self::wake_event`], the mutex released). An `unpark`
    /// signals the event only then (interpreter round i3 wave 73, lane L6;
    /// page `i72-L6-windows-park-event-left-signalled-by-a-lazy-park-20261105`):
    /// signalled always, a permit taken off the event path left it set and
    /// the thread's next timed park woke on it spuriously.
    #[cfg(target_os = "windows")]
    event_waiter: AtomicBool,
    /// Windows only, under [`Self::mutex`]: an `unpark` signalled the event
    /// during the current accurate wait (at most one signal per wait).
    #[cfg(target_os = "windows")]
    event_signalled: AtomicBool,
    /// Windows only, under [`Self::mutex`]: the waiter on the event asked for
    /// its signal to be raised AFTER the waker releases the mutex
    /// ([`Self::signal_event_waiter`] then answers "deferred"; interpreter
    /// round i5 wave 108, lane L6), as HotSpot's Windows `Parker::unpark`
    /// calls `SetEvent` holding no lock. An event set with the mutex held
    /// readies the waiter -- with Windows' wake boost, often on the waker's
    /// own core, preempting it -- and the waiter's first act is to re-take
    /// that mutex, still held by the preempted waker: a second sleep and a
    /// second wake on the critical path. Raised by an untimed event wait
    /// (`CRATONVM_PARK_UNTIMED_EVENT`) always, and by a Java-level timed
    /// park's coarse wait under `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK`.
    /// A wait that ends without the event while a deferred claim is pending
    /// waits for that signal ([`Self::settle_unconsumed_signal`]).
    #[cfg(target_os = "windows")]
    event_defer: AtomicBool,
    /// Windows only (interpreter round i5 wave 114, lane L1,
    /// `CRATONVM_PARK_EVENT_LOCKLESS_WAKE`, default off): the hand-shake of a
    /// wait on the wake event that does NOT re-take [`Self::mutex`] after
    /// the OS wait ([`Self::park_wait_once_lockless`],
    /// [`Self::park_wait_untimed_lockless`]), HotSpot 25's Windows `Parker`
    /// shape (`WaitForSingleObject(_ParkHandle)` and back to Java, no lock).
    /// One word, so the waiter can leave it without the mutex:
    /// [`LOCKLESS_IDLE`] (no such waiter), [`LOCKLESS_WAITING`] (stored
    /// under the mutex after the waiter's last permit / flag check, before it
    /// releases the mutex and waits), [`LOCKLESS_CLAIMED`] (a waker, under the
    /// mutex, claimed the one signal of this wait and raises the event in the
    /// same critical section). The waiter swaps it back to idle after its OS
    /// wait; a claim it did not consume is reset under the mutex (wave 73).
    #[cfg(target_os = "windows")]
    lockless_slot: AtomicU8,
}

/// [`ParkState::lockless_slot`]: no lockless event waiter.
#[cfg(target_os = "windows")]
const LOCKLESS_IDLE: u8 = 0;
/// [`ParkState::lockless_slot`]: a lockless waiter is (about to be) inside
/// its OS wait on the wake event, with the mutex released.
#[cfg(target_os = "windows")]
const LOCKLESS_WAITING: u8 = 1;
/// [`ParkState::lockless_slot`]: a waker claimed this wait's signal (and
/// raised, or is raising, the event while it holds the mutex).
#[cfg(target_os = "windows")]
const LOCKLESS_CLAIMED: u8 = 2;

#[cfg(target_os = "windows")]
impl Drop for ParkState {
    fn drop(&mut self) {
        win_park::close(self.wake_event);
    }
}

/// Cached `CRATONVM_DBG_PARKLAT` gate.
#[inline]
fn parklat_enabled() -> bool {
    parklat_threshold_nanos().is_some()
}

/// `CRATONVM_DBG_PARKLAT`'s report threshold, in nanoseconds (`None`: the
/// diagnostic is off). A value of 2 or more is the threshold in MICROSECONDS
/// (interpreter round i3 wave 75, lane L6: `CRATONVM_DBG_PARKLAT=200` reports
/// every wake slower than 200 us, which the `--compatible` unpark-wake
/// investigation needs); any other value (`1`, `CRATONVM_DBG=parklat`) keeps
/// the original 50 ms.
pub(crate) fn parklat_threshold_nanos() -> Option<u64> {
    use std::sync::OnceLock;
    static G: OnceLock<Option<u64>> = OnceLock::new();
    *G.get_or_init(|| {
        let raw = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_PARKLAT")?;
        match raw.to_str().and_then(|s| s.trim().parse::<u64>().ok()) {
            Some(us) if us >= 2 => Some(us.saturating_mul(1_000)),
            _ => Some(50_000_000),
        }
    })
}

/// DBG (`CRATONVM_DBG_PARKLAT`, wave 75): report a wait that took the
/// permit `lat` after the unpark that set it, naming the wait (`path`) and
/// whether its last OS wait ended on a timeout (`tick`: a poll slice or the
/// deadline found the permit, so the unpark's notify did NOT end that wait)
/// or on a wake. Cold; called only with the diagnostic on.
#[cold]
#[inline(never)]
fn parklat_report_wake(path: &str, set_at: u64, tick: bool) {
    let Some(threshold) = parklat_threshold_nanos() else {
        return;
    };
    if set_at == 0 {
        return;
    }
    let lat = parklat_now_nanos().saturating_sub(set_at);
    if lat >= threshold {
        eprintln!(
            "[parklat] unpark->wake {}us path={path} woke_by={} (thread {:?})",
            lat / 1_000,
            if tick { "timeout-tick" } else { "notify" },
            std::thread::current().id(),
        );
    }
}

/// DBG (`CRATONVM_DBG_PARKLAT`; interpreter round i5 wave 110, lane L6): a
/// Windows timed park's wake split at the OS return of its coarse event wait:
/// `unpark->os` (the waker's permit store to the waiter's
/// `WaitForMultipleObjects` return: the waker's signal plus the OS wake) and
/// `os->relocked` (the waiter's re-take of the state's mutex and its permit
/// re-check). Printed when the whole is at least the threshold. Cold.
///
/// Wave 114 (lane L1): `signal_at` (the waker's `SetEvent` return,
/// [`ParkState::last_signal_nanos`]) splits `unpark->os` once more, into
/// `unpark->signal` (the waker's own leg inside the VM) and `signal->os` (the
/// OS readying and switching to this thread). Printed as a trailing
/// `signal-split` group when the stamp belongs to this wake (between the
/// permit store and the OS return); the leading fields are unchanged.
#[cfg(target_os = "windows")]
#[cold]
#[inline(never)]
fn parklat_report_coarse_split(label: &str, set_at: u64, signal_at: u64, os_at: u64) {
    let Some(threshold) = parklat_threshold_nanos() else {
        return;
    };
    if set_at == 0 || os_at < set_at {
        return;
    }
    let now = parklat_now_nanos();
    if now.saturating_sub(set_at) >= threshold {
        let signal = if signal_at >= set_at && signal_at <= os_at {
            format!(
                " signal-split unpark->signal {}us signal->os {}us",
                (signal_at - set_at) / 1_000,
                (os_at - signal_at) / 1_000,
            )
        } else {
            String::new()
        };
        eprintln!(
            "[parklat] {label} split unpark->os {}us os->relocked {}us{signal} (thread {:?})",
            (os_at - set_at) / 1_000,
            now.saturating_sub(os_at) / 1_000,
            std::thread::current().id(),
        );
    }
}

/// DBG (`CRATONVM_DBG_PARKLAT`, wave 75): report a park phase -- `label` --
/// that took at least the threshold since `start` (the blocking prologue,
/// the post-wake epilogue of `NativeContextImpl::park_with_state`). Cold.
#[cold]
#[inline(never)]
pub(crate) fn parklat_report_phase(label: &str, start: std::time::Instant) {
    let Some(threshold) = parklat_threshold_nanos() else {
        return;
    };
    let took = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    if took >= threshold {
        eprintln!(
            "[parklat] park {label} {}us (thread {:?})",
            took / 1_000,
            std::thread::current().id(),
        );
    }
}

/// Monotonic nanos for the PARKLAT diagnostic (process-relative).
#[inline]
fn parklat_now_nanos() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as u64
}

/// What a `jdk.ThreadPark` event of one park covers (interpreter round i3
/// wave 73, lane L6; page
/// `i72-L6-jfr-thread-park-misses-the-lazy-park-phase-20261105`). HotSpot's
/// `Unsafe_Park` starts its `EventThreadPark` before `Parker::park` (only if
/// the event is enabled then) and commits it after, from every exit, with the
/// caller's time: so the event spans the whole park -- the permit fast path,
/// a park's running phase (`CRATONVM_PARK_LAZY`) and the blocking wait -- and
/// carries the timeout the caller asked for, not what was left of it when the
/// blocking path began. The recording's threshold then filters on that whole
/// duration (`duration < threshold` is dropped, as `JfrEvent::should_commit`).
#[derive(Clone, Copy, Debug)]
pub struct ParkEventSpan {
    start: std::time::Instant,
    caller_timeout: Option<std::time::Duration>,
    /// An absolute park's deadline, milliseconds since the epoch (`parkUntil`,
    /// `Unsafe.park(true, time)`); wave 79, lane L6.
    until_ms: Option<i64>,
    /// A negative relative park's raw nanoseconds (`Unsafe.park(false, -1)`),
    /// which `caller_timeout` (a `Duration`, `ZERO` for it) cannot carry; gci
    /// i23/pk, [`Self::with_raw_timeout`].
    raw_timeout_ns: Option<i64>,
}

impl ParkEventSpan {
    /// The span of a park entered now with `caller_timeout`, when an event
    /// may be wanted (`wanted`: a recording runs); `None` otherwise, so a park
    /// with no recording reads no clock.
    #[inline]
    pub fn begin(wanted: bool, caller_timeout: Option<std::time::Duration>) -> Option<Self> {
        Self::begin_until(wanted, caller_timeout, None)
    }

    /// [`Self::begin`] for a park that may be absolute: `until_ms` is the
    /// caller's deadline in epoch milliseconds (interpreter round i3 wave 79,
    /// lane L6; page `i73-L6-jfr-thread-park-fields-differ-from-hotspot`).
    /// HotSpot's `Unsafe_Park` posts an absolute park with `timeout =
    /// Long.MIN_VALUE` and `until = time`.
    #[inline]
    pub fn begin_until(
        wanted: bool,
        caller_timeout: Option<std::time::Duration>,
        until_ms: Option<i64>,
    ) -> Option<Self> {
        wanted.then(|| Self {
            start: std::time::Instant::now(),
            caller_timeout,
            until_ms,
            raw_timeout_ns: None,
        })
    }

    /// The span with the caller's raw relative `time` as the event's
    /// `timeout` (gci i23/pk, finding 6 of
    /// `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): HotSpot 25's
    /// `Unsafe_Park` posts a relative park as `(timeout = time, until =
    /// Long.MIN_VALUE)`, so `U.park(false, -1)` reports `-1`, where the
    /// `Duration` answer was 0. `None` keeps the `Duration` answer. An
    /// absolute park still reports `Long.MIN_VALUE`.
    #[inline]
    pub fn with_raw_timeout(mut self, raw_timeout_ns: Option<i64>) -> Self {
        self.raw_timeout_ns = raw_timeout_ns;
        self
    }

    /// The park's whole duration so far.
    #[inline]
    pub fn duration(&self) -> std::time::Duration {
        self.start.elapsed()
    }

    /// The event's `timeout` field, as HotSpot's `post_thread_park_event`
    /// writes it for a relative park: the caller's nanoseconds, or
    /// `Long.MIN_VALUE` for an untimed park.
    pub fn timeout_field(&self) -> i64 {
        if self.until_ms.is_some() {
            return i64::MIN;
        }
        if let Some(raw) = self.raw_timeout_ns {
            return raw;
        }
        self.caller_timeout
            .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
            .unwrap_or(i64::MIN)
    }

    /// The event's `until` field: an absolute park's deadline (epoch ms),
    /// `Long.MIN_VALUE` for every other park, as `post_thread_park_event`
    /// writes it (wave 79, lane L6).
    pub fn until_field(&self) -> i64 {
        self.until_ms.unwrap_or(i64::MIN)
    }

    /// The legacy recorder's `timeout` (`cratonvm_jfr::builtin::emit_thread_park_event`):
    /// the caller's nanoseconds, 0 for an untimed park.
    pub fn legacy_timeout_ns(&self) -> i64 {
        self.caller_timeout
            .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
            .unwrap_or(0)
    }
}

/// Lowers [`ParkState::sleepers`] when a wait leaves, however it leaves.
struct Sleeper<'a>(&'a AtomicU32);

impl Drop for Sleeper<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ParkState {
    /// Create a new ParkState with no permit available.
    pub fn new() -> Self {
        Self {
            mutex: PLMutex::new(()),
            condvar: PLCondvar::new(),
            permit: AtomicBool::new(false),
            sleepers: AtomicU32::new(0),
            lock_free: AtomicBool::new(false),
            last_unpark_nanos: std::sync::atomic::AtomicU64::new(0),
            last_os_wake_nanos: std::sync::atomic::AtomicU64::new(0),
            last_signal_nanos: std::sync::atomic::AtomicU64::new(0),
            lazy: LazyParkStreak {
                misses: AtomicU8::new(0),
                skips: AtomicU8::new(0),
            },
            #[cfg(target_os = "windows")]
            wake_event: win_park::create_wake_event(),
            #[cfg(target_os = "windows")]
            event_waiter: AtomicBool::new(false),
            #[cfg(target_os = "windows")]
            event_signalled: AtomicBool::new(false),
            #[cfg(target_os = "windows")]
            event_defer: AtomicBool::new(false),
            #[cfg(target_os = "windows")]
            lockless_slot: AtomicU8::new(LOCKLESS_IDLE),
        }
    }

    /// `CRATONVM_PARK_LAZY_ADAPTIVE` (interpreter round i3 wave 81, lane L6):
    /// should the owner's next park skip its running phase? Yes once
    /// [`LAZY_PARK_MISSES_TO_SKIP`] phases in a row ran out their budget,
    /// except every [`LAZY_PARK_RECHECK_EVERY`]th park, which runs the phase
    /// again so a thread that became a handoff partner is seen. Owner-only;
    /// two relaxed loads on the park's slow path, and a store only when the
    /// count changes.
    pub fn lazy_phase_skip(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let old = self.lazy.skips.load(Relaxed);
        let (skip, skips) = lazy_park_skip_decision(self.lazy.misses.load(Relaxed), old);
        if skips != old {
            self.lazy.skips.store(skips, Relaxed);
        }
        skip
    }

    /// `CRATONVM_PARK_LAZY_ADAPTIVE`: the owner's running phase ran out its
    /// budget (no permit within it).
    pub fn note_lazy_phase_ran_out(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let misses = self.lazy.misses.load(Relaxed);
        if misses != u8::MAX {
            self.lazy.misses.store(misses + 1, Relaxed);
        }
    }

    /// `CRATONVM_PARK_LAZY_ADAPTIVE`: the owner's park ended on its permit
    /// within the budget -- a handoff; its parks run the phase again. Stores
    /// only a non-zero count: a handoff partner (the common case, every park
    /// of a ping-pong) writes nothing (wave-81 host: the unconditional
    /// stores cost `L6W79ParkSpinBench park-pingpong` 466 -> 666 ns).
    pub fn note_lazy_handoff(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.lazy.misses.load(Relaxed) != 0 {
            self.lazy.misses.store(0, Relaxed);
        }
        if self.lazy.skips.load(Relaxed) != 0 {
            self.lazy.skips.store(0, Relaxed);
        }
    }

    /// `CRATONVM_PARK_ATOMIC_PERMIT` for this state: called by the registry
    /// when it creates or installs a state for a VM with the switch on,
    /// before the state is reachable by an unpark and before its owner can
    /// park on it (thread start, attach, registration), so every wait and
    /// every unpark of the state sees the same answer.
    pub fn enable_lock_free(&self) {
        self.lock_free
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Count this thread as a waiter -- only on a lock-free state. Called
    /// with the mutex held, before the wait's first permit check
    /// ([`Self::take_first`]).
    #[inline]
    fn enter_wait(&self) -> Option<Sleeper<'_>> {
        if !self.lock_free.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        self.sleepers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(Sleeper(&self.sleepers))
    }

    /// A wait's first permit check, with the mutex held. Counted
    /// (`counted`, a lock-free state): a `SeqCst` swap, the waiter's half of
    /// the Dekker pair with [`Self::unpark_lock_free`]. Otherwise a plain
    /// load and store, as the `Mutex<bool>` was.
    #[inline]
    fn take_first(&self, counted: bool) -> bool {
        if counted {
            self.permit.swap(false, std::sync::atomic::Ordering::SeqCst)
        } else {
            self.take_locked()
        }
    }

    /// Take the permit with the mutex held: no locked instruction.
    #[inline]
    fn take_locked(&self) -> bool {
        if self.permit.load(std::sync::atomic::Ordering::Relaxed) {
            self.permit
                .store(false, std::sync::atomic::Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// A re-check with the mutex held (after a wake: an unparker that saw
    /// this waiter set the permit before it took the mutex to notify).
    #[inline]
    fn permit_set(&self) -> bool {
        self.permit.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Clear the permit with the mutex held (a wait's end consumes it).
    #[inline]
    fn clear_locked(&self) {
        self.permit
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Windows: one wait of at most `dur` on the wake event and a
    /// high-resolution timer, the mutex released for its length (`guard` is
    /// re-held on return). `true` when the accurate path ran; `false` when it
    /// is unavailable and the caller must wait on the condvar. A signal the
    /// wait did not consume (it ended on the timer, or never started) is
    /// reset, so it cannot end a later wait spuriously.
    #[cfg(target_os = "windows")]
    fn accurate_wait(
        &self,
        guard: &mut parking_lot::MutexGuard<'_, ()>,
        dur: std::time::Duration,
    ) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if !(win_park::enabled() && self.wake_event != 0) {
            return false;
        }
        self.event_waiter.store(true, Relaxed);
        let woke = parking_lot::MutexGuard::unlocked(guard, || {
            win_park::wait_until_reporting(self.wake_event, dur)
        });
        self.event_waiter.store(false, Relaxed);
        let signalled = self.event_signalled.swap(false, Relaxed);
        if signalled && woke != win_park::Woke::Event {
            win_park::reset(self.wake_event);
        }
        woke != win_park::Woke::Unavailable
    }

    /// Windows: [`Self::accurate_wait`] at the default timer resolution --
    /// one wait of at most `millis` on the wake event alone
    /// (`win_park::wait_event_coarse`), the mutex released for its length.
    /// The event hand-shake is the accurate path's (`event_waiter`,
    /// `event_signalled`, the reset of an unconsumed signal), so an unpark
    /// or an interrupt reaches it the same way. `false`: no event, the
    /// caller waits on the condvar. Interpreter round i4 wave 99, lane L6.
    ///
    /// `defer` (interpreter round i5 wave 108, lane L6,
    /// `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK`): the waker raises this
    /// wait's signal after releasing the mutex (`event_defer`).
    #[cfg(target_os = "windows")]
    fn coarse_wait(
        &self,
        guard: &mut parking_lot::MutexGuard<'_, ()>,
        millis: u64,
        defer: bool,
    ) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if self.wake_event == 0 {
            return false;
        }
        self.event_waiter.store(true, Relaxed);
        self.event_defer.store(defer, Relaxed);
        let woke = parking_lot::MutexGuard::unlocked(guard, || {
            let woke = win_park::wait_event_coarse(self.wake_event, millis);
            // DBG (CRATONVM_DBG_PARKLAT, wave 110): the OS return, before the
            // mutex re-take `unlocked` does next.
            if parklat_enabled() {
                self.last_os_wake_nanos.store(parklat_now_nanos(), Relaxed);
            }
            woke
        });
        self.event_defer.store(false, Relaxed);
        self.event_waiter.store(false, Relaxed);
        let signalled = self.event_signalled.swap(false, Relaxed);
        if signalled && woke != win_park::Woke::Event {
            self.settle_unconsumed_signal(guard, defer);
        }
        woke != win_park::Woke::Unavailable
    }

    /// Windows: a wait ended without consuming the event while a waker had
    /// claimed its signal (`event_signalled`). Not deferred: the signal was
    /// raised under the mutex, so it is set now -- reset it (wave 73). Deferred
    /// (`event_defer`, wave 108): the waker raises it after its unlock, so it
    /// may not be set YET and a reset now would leave the later `SetEvent`
    /// for this thread's next park to wake on spuriously. Wait for it instead,
    /// with the mutex released (the claimer needs no lock to raise it), for
    /// at most [`DEFERRED_SIGNAL_SETTLE_MS`]; a claimer held up past that (a
    /// suspended thread) gets the old reset, and its late signal can end one
    /// later park early, which a park is allowed to do.
    #[cfg(target_os = "windows")]
    fn settle_unconsumed_signal(&self, guard: &mut parking_lot::MutexGuard<'_, ()>, defer: bool) {
        if defer {
            let woke = parking_lot::MutexGuard::unlocked(guard, || {
                win_park::wait_event_coarse(self.wake_event, DEFERRED_SIGNAL_SETTLE_MS)
            });
            if woke == win_park::Woke::Event {
                return;
            }
        }
        win_park::reset(self.wake_event);
    }

    /// Windows: [`Self::coarse_wait`] with no deadline -- one untimed wait on
    /// the wake event, the mutex released for its length
    /// (`win_park::wait_event_untimed`; interpreter round i5 wave 107, lane
    /// L6, `CRATONVM_PARK_UNTIMED_EVENT`). The hand-shake is the accurate
    /// path's: `event_waiter` is raised under the mutex before the wait, so an
    /// unpark or interrupt wake that takes the mutex after this waiter's last
    /// permit / flag check signals the event (every one of them calls
    /// [`Self::signal_event_waiter`] under the mutex), and a signal the wait
    /// did not consume is reset. `false`: nothing was waited on (no event, or
    /// the OS wait failed); the caller re-checks the permit and the flag
    /// under the mutex before it waits on the condvar instead.
    #[cfg(target_os = "windows")]
    fn untimed_event_wait(&self, guard: &mut parking_lot::MutexGuard<'_, ()>) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if self.wake_event == 0 {
            return false;
        }
        self.event_waiter.store(true, Relaxed);
        // Wave 108 (lane L6): a waker defers this waiter's signal until it
        // has released the mutex (`event_defer`, `signal_event_waiter`).
        self.event_defer.store(true, Relaxed);
        let woke = parking_lot::MutexGuard::unlocked(guard, || {
            win_park::wait_event_untimed(self.wake_event)
        });
        self.event_defer.store(false, Relaxed);
        self.event_waiter.store(false, Relaxed);
        let signalled = self.event_signalled.swap(false, Relaxed);
        // Nothing but the event ends an infinite wait, so `woke` is `Event`
        // whenever a signal was claimed, bar a FAILED wait (`Unavailable`).
        if signalled && woke != win_park::Woke::Event {
            self.settle_unconsumed_signal(guard, true);
        }
        woke != win_park::Woke::Unavailable
    }

    /// Windows, under the mutex: signal the event for a waiter on the
    /// accurate path, once per wait.
    ///
    /// Interpreter round i5 wave 108, lane L6: answers `true` -- "deferred"
    /// -- instead of signalling when the waiter asked for it (`event_defer`:
    /// an untimed event wait under `CRATONVM_PARK_UNTIMED_EVENT`, a coarse
    /// timed wait under `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK`); the
    /// caller then raises the signal with [`Self::raise_deferred_event_signal`]
    /// after it has released the mutex. Every other waiter is signalled here,
    /// under the mutex, as before, and the answer is `false`.
    #[cfg(target_os = "windows")]
    #[inline]
    #[must_use]
    fn signal_event_waiter(&self) -> bool {
        use std::sync::atomic::Ordering::{AcqRel, Relaxed};
        // Interpreter round i5 wave 114, lane L1
        // (`CRATONVM_PARK_EVENT_LOCKLESS_WAKE`): a lockless waiter's one
        // signal is claimed with a CAS on its slot and raised here, under the
        // mutex, in the same critical section as the claim -- the waiter
        // does not re-take the mutex after its wake, so a waker preempted
        // inside this section no longer holds it up, and its "claimed but not
        // consumed" reset only has to take the mutex once to know the event
        // is set (`lockless_leave`). The permit / flag store this wake
        // publishes came before, under the mutex; the CAS releases it.
        if self.lockless_slot.load(Relaxed) == LOCKLESS_WAITING
            && self
                .lockless_slot
                .compare_exchange(LOCKLESS_WAITING, LOCKLESS_CLAIMED, AcqRel, Relaxed)
                .is_ok()
        {
            win_park::signal(self.wake_event);
            self.stamp_signal();
            return false;
        }
        if self.event_waiter.load(Relaxed) && !self.event_signalled.swap(true, Relaxed) {
            if self.event_defer.load(Relaxed) {
                return true;
            }
            win_park::signal(self.wake_event);
            self.stamp_signal();
        }
        false
    }

    /// DBG (`CRATONVM_DBG_PARKLAT`; interpreter round i5 wave 114, lane L1):
    /// record when a waker's `SetEvent` of this state returned.
    #[cfg(target_os = "windows")]
    #[inline]
    fn stamp_signal(&self) {
        if parklat_enabled() {
            self.last_signal_nanos
                .store(parklat_now_nanos(), std::sync::atomic::Ordering::Release);
        }
    }

    /// Under the mutex, on every target: [`Self::signal_event_waiter`] on
    /// Windows, `false` elsewhere (no event). Pair with
    /// [`Self::raise_deferred_event_signal`] after the mutex is released.
    #[inline]
    #[must_use]
    fn claim_event_signal(&self) -> bool {
        #[cfg(target_os = "windows")]
        let deferred = self.signal_event_waiter();
        #[cfg(not(target_os = "windows"))]
        let deferred = false;
        deferred
    }

    /// With the mutex RELEASED: raise a signal [`Self::claim_event_signal`]
    /// deferred (interpreter round i5 wave 108, lane L6). A no-op for
    /// `false` and on every target but Windows.
    #[inline]
    fn raise_deferred_event_signal(&self, deferred: bool) {
        #[cfg(target_os = "windows")]
        if deferred {
            win_park::signal(self.wake_event);
            self.stamp_signal();
        }
        #[cfg(not(target_os = "windows"))]
        let _ = deferred;
    }

    /// Consume the permit or block until one is available.
    ///
    /// If a permit is available (from a prior `unpark()`), consumes it and
    /// returns immediately. Otherwise, blocks until `unpark()` is called or
    /// the timeout expires.
    pub fn park(&self, timeout: Option<std::time::Duration>) {
        let mut guard = self.mutex.lock();
        let sleeper = self.enter_wait();
        let counted = sleeper.is_some();
        if self.take_first(counted) {
            return;
        }
        match timeout {
            Some(dur) if !dur.is_zero() => {
                // Windows: an accurate deadline needs a high-resolution timer,
                // and staying wakeable by `unpark` needs the event waited on
                // beside it — the condvar can be neither. The lock is released
                // for the wait; an `unpark` landing in the gap signals the
                // event (the waiter flag is up), so the wait returns at once.
                #[cfg(target_os = "windows")]
                if self.accurate_wait(&mut guard, dur) {
                    self.clear_locked();
                    return;
                }
                if !self.permit_set() {
                    self.condvar.wait_for(&mut guard, dur);
                }
            }
            None => {
                if !self.permit_set() {
                    self.condvar.wait(&mut guard);
                }
            }
            _ => {} // zero timeout = no-op
        }
        // consume permit even on timeout/spurious wakeup
        self.clear_locked();
    }

    /// Consume the permit or block until one is available, with interrupt awareness.
    ///
    /// Like `park()`, but periodically checks the interrupt flag so that
    /// `Thread.interrupt()` can break a parked thread out of the wait.
    /// [`Self::park_wait`] with the poll (the default blocking park).
    #[cfg(test)] // only tests call it since the wave-77 one-wait/one-unpark refactor
    pub fn park_interruptible(
        &self,
        timeout: Option<std::time::Duration>,
        interrupted: &std::sync::atomic::AtomicBool,
    ) {
        self.park_wait(timeout, interrupted, true);
    }

    /// [`Self::park_interruptible`] without the interrupt poll (interpreter
    /// round i3 wave 74, lane L6, `CRATONVM_PARK_UNTIMED_NO_POLL`; page
    /// `i73-L6-proposal-untimed-park-without-a-5ms-poll-20261106`): an untimed
    /// park waits on the condvar until it is woken, and a timed one waits for
    /// its whole remaining time in one wait (on Windows one accurate wait of
    /// the remainder), as HotSpot's `Parker::park` does. Both loop only on a
    /// wake that found neither the permit nor the interrupt flag.
    ///
    /// The poll's 5 ms slices ended a wait only on the permit or the flag,
    /// so the one wake source it covered is an interrupt that sets the flag
    /// without waking the state. With the switch on,
    /// `ThreadRegistry::set_interrupted` wakes the state after setting the
    /// flag ([`Self::wake_for_interrupt`]), under this state's mutex, which
    /// a waiter holds from its flag read until it is inside the wait; and
    /// `Thread.interrupt` / the JDWP and JVMTI interrupts unpark the target
    /// anyway. A stop-the-world pause, a suspension and VM exit never ended
    /// the poll loop (a parked thread is GC-blocked and needs no wake), so
    /// they need nothing here.
    pub fn park_interruptible_unpolled(
        &self,
        timeout: Option<std::time::Duration>,
        interrupted: &std::sync::atomic::AtomicBool,
    ) {
        self.park_wait(timeout, interrupted, false);
    }

    /// The one blocking wait of a park (interpreter round i3 wave 77, lane
    /// L6; page `i76-L6-proposal-one-park-wait-and-one-unpark-20261109`):
    /// [`Self::park_interruptible`] (`polled`) and
    /// [`Self::park_interruptible_unpolled`] (not), which were two copies of
    /// the same wait differing only in how long one OS wait may last.
    ///
    /// The permit is taken first (the waiter counted a sleeper of a lock-free
    /// state before that check); a raised interrupt flag or a zero timeout
    /// returns at once. Then the thread waits until the permit is set, the
    /// flag is raised or the deadline passes, re-checking both under the
    /// mutex after every OS wait, and consumes the permit on the way out
    /// (even on a timeout or a spurious wake).
    ///
    /// * `polled`: no OS wait is longer than the poll slice -- 5 ms on the
    ///   condvar; on Windows' accurate path 15 ms (see
    ///   [`Self::poll_slice`]), so an interrupt that only raised the flag is
    ///   noticed within a slice; an untimed park waits in condvar slices (never
    ///   on the accurate path);
    /// * not `polled` (`CRATONVM_PARK_UNTIMED_NO_POLL`): one OS wait of the
    ///   whole remainder (untimed: until woken).
    ///
    /// A deadline past `Instant`'s range is no deadline at all (before wave
    /// 77 the polled wait computed it with a panicking `+`).
    pub fn park_wait(
        &self,
        timeout: Option<std::time::Duration>,
        interrupted: &std::sync::atomic::AtomicBool,
        polled: bool,
    ) {
        self.park_wait_with(timeout, interrupted, polled, false);
    }

    /// [`Self::park_wait`], and with `untimed_event` (interpreter round i5
    /// wave 107, lane L6, `CRATONVM_PARK_UNTIMED_EVENT`) an untimed unpolled
    /// wait on Windows waits on the state's wake event
    /// ([`Self::untimed_event_wait`]) rather than on the condvar: HotSpot 25's
    /// Windows `Parker` is one auto-reset event, waited on with
    /// `WaitForSingleObject` and set by `SetEvent`, and the waiter it wakes
    /// does not first queue on a mutex. The loop, the permit and flag checks
    /// under the mutex, and the consume on the way out are unchanged; a
    /// timed or polled wait ignores `untimed_event`, and so does every other
    /// target.
    pub fn park_wait_with(
        &self,
        timeout: Option<std::time::Duration>,
        interrupted: &std::sync::atomic::AtomicBool,
        polled: bool,
        untimed_event: bool,
    ) {
        use std::sync::atomic::Ordering;
        #[cfg(not(target_os = "windows"))]
        let _ = untimed_event;
        let mut guard = self.mutex.lock();
        let sleeper = self.enter_wait();
        let counted = sleeper.is_some();
        if self.take_first(counted) {
            return;
        }
        if interrupted.load(Ordering::Acquire) {
            return;
        }
        let deadline = match timeout {
            Some(dur) if dur.is_zero() => return,
            Some(dur) => std::time::Instant::now().checked_add(dur),
            None => None,
        };
        // DBG (`CRATONVM_DBG_PARKLAT`, wave 75): did the last OS wait end on
        // its timeout (a poll tick or the deadline) rather than on a wake?
        let mut tick = false;
        loop {
            match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    // The polled wait also re-checked the permit here, before
                    // each slice (a lock-free unpark may set it without the
                    // mutex; it then notifies under the mutex anyway).
                    if remaining.is_zero() || (polled && self.permit_set()) {
                        break;
                    }
                    let wait_time = if polled {
                        remaining.min(self.poll_slice())
                    } else {
                        remaining
                    };
                    #[cfg(target_os = "windows")]
                    let accurate = self.accurate_wait(&mut guard, wait_time);
                    #[cfg(not(target_os = "windows"))]
                    let accurate = false;
                    if !accurate {
                        tick = self.condvar.wait_for(&mut guard, wait_time).timed_out();
                    }
                }
                None if polled => {
                    tick = self.condvar.wait_for(&mut guard, PARK_POLL).timed_out();
                }
                None => {
                    #[cfg(target_os = "windows")]
                    if untimed_event && self.untimed_event_wait(&mut guard) {
                        if self.permit_set() || interrupted.load(Ordering::Acquire) {
                            break;
                        }
                        continue;
                    }
                    // No event wait ran (or none was asked): the mutex was
                    // released and re-taken if one was tried, so re-check
                    // before the condvar wait (a wake meant for the event
                    // may have come in between).
                    if self.permit_set() || interrupted.load(Ordering::Acquire) {
                        break;
                    }
                    self.condvar.wait(&mut guard)
                }
            }
            if self.permit_set() || interrupted.load(Ordering::Acquire) {
                break;
            }
        }
        // DBG (CRATONVM_DBG_PARKLAT): report a tardy delivery — the wake
        // observed the permit long after the unpark that set it.
        if self.permit_set() && parklat_enabled() {
            parklat_report_wake(
                if polled { "polled" } else { "unpolled" },
                self.last_unpark_nanos.load(Ordering::Acquire),
                tick,
            );
        }
        self.clear_locked();
    }

    /// Windows: a Java-level timed park's blocking wait as HotSpot 25's
    /// Windows `Parker::park` (os_windows.cpp) makes it (interpreter round
    /// i4 wave 99, lane L6; page `i98-L6-windows-parknanos-whole-milliseconds`):
    /// the permit is taken first and a raised interrupt flag or a zero
    /// `timeout` returns at once, as in [`Self::park_wait`]; otherwise ONE
    /// wait of `timeout` in whole milliseconds (rounded up, at least 1) on the
    /// wake event at the default timer resolution, and return -- whether it
    /// ended on an unpark, an interrupt, its timeout, or a tick before its
    /// timeout (a park may return early). No loop to a deadline and no poll:
    /// `Thread.interrupt` unparks its target (the event), as HotSpot's
    /// `JavaThread::interrupt` unparks the `Parker`. The permit is consumed on
    /// the way out (even on a timeout), as `park_wait` does and as HotSpot's
    /// `ResetEvent` does. Without an event (`CRATONVM_WIN_HIRES_PARK=0`, or
    /// the OS refused one) the one wait is the condvar's.
    #[cfg(target_os = "windows")]
    pub fn park_wait_once(
        &self,
        timeout: std::time::Duration,
        interrupted: &std::sync::atomic::AtomicBool,
    ) {
        self.park_wait_once_with(timeout, interrupted, false);
    }

    /// [`Self::park_wait_once`], and with `signal_after_unlock` (interpreter
    /// round i5 wave 108, lane L6, `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK`,
    /// default off) the waker raises this wait's event after it has released
    /// the state's mutex (`event_defer`), as HotSpot's `Parker::unpark` sets
    /// its event holding no lock.
    #[cfg(target_os = "windows")]
    pub fn park_wait_once_with(
        &self,
        timeout: std::time::Duration,
        interrupted: &std::sync::atomic::AtomicBool,
        signal_after_unlock: bool,
    ) {
        use std::sync::atomic::Ordering;
        let mut guard = self.mutex.lock();
        let sleeper = self.enter_wait();
        let counted = sleeper.is_some();
        if self.take_first(counted) {
            return;
        }
        if interrupted.load(Ordering::Acquire) || timeout.is_zero() {
            return;
        }
        let nanos = timeout.as_nanos();
        let millis = u64::try_from(nanos.div_ceil(1_000_000))
            .unwrap_or(u64::MAX)
            .max(1);
        if !self.coarse_wait(&mut guard, millis, signal_after_unlock) && !self.permit_set() {
            let _ = self.condvar.wait_for(&mut guard, timeout);
        }
        // DBG (CRATONVM_DBG_PARKLAT; interpreter round i5 wave 109, lane L6):
        // the timed park's unpark->wake too (the OS wake of the waiter, from
        // the waker's permit store to this re-check), as `park_wait` reports
        // the untimed one. Before wave 109 a Java-level timed park on Windows
        // printed only its prologue / epilogue, so the timed-wake gap of
        // `L6W108TimedParkWakeSplit` could not be split.
        if self.permit_set() && parklat_enabled() {
            // Wave 110 (lane L6): the same wake split at the OS return; first,
            // so its clock read precedes the other line's print.
            parklat_report_coarse_split(
                "coarse",
                self.last_unpark_nanos.load(Ordering::Acquire),
                self.last_signal_nanos.load(Ordering::Acquire),
                self.last_os_wake_nanos.load(Ordering::Acquire),
            );
            parklat_report_wake(
                "coarse",
                self.last_unpark_nanos.load(Ordering::Acquire),
                false,
            );
        }
        self.clear_locked();
    }

    /// Windows, `CRATONVM_PARK_EVENT_LOCKLESS_WAKE` (interpreter round i5
    /// wave 114, lane L1; page `i97-L1-process-exit-wake-latency`, "Wave
    /// 114"; default off): [`Self::park_wait_once`] whose waiter does not
    /// re-take the state's mutex after its OS wait. HotSpot 25's Windows
    /// `Parker::park` returns from `WaitForSingleObject(_ParkHandle, time)`
    /// straight to `ResetEvent` and Java; this wait's way back was the
    /// mutex re-take of `MutexGuard::unlocked` (`os->relocked`), which, when
    /// the event readied the waiter while the waker still held the mutex
    /// (`SetEvent` is made under it), is a second sleep and a second OS wake
    /// on the critical path. Here the waiter:
    ///
    /// 1. under the mutex, as before: the permit (counted on a lock-free
    ///    state), the interrupt flag, a zero timeout; then it stores
    ///    [`LOCKLESS_WAITING`] and RELEASES the mutex;
    /// 2. waits once on the event, `timeout` in whole milliseconds rounded up
    ///    (as `park_wait_once`);
    /// 3. without the mutex: leaves the slot ([`Self::lockless_leave`]) and
    ///    consumes the permit with one store (an unpark that lands during the
    ///    return is absorbed by this park, as `ResetEvent` absorbs it on
    ///    HotSpot and as the mutex path's `clear_locked` did).
    ///
    /// Every waker reaches it: an unpark or interrupt wake that takes the
    /// mutex after step 1 finds the slot waiting and claims it
    /// ([`Self::signal_event_waiter`]); one that took it before step 1 set
    /// the permit or flag the waiter's checks then saw. Without an event the
    /// mutex path runs instead.
    #[cfg(target_os = "windows")]
    pub fn park_wait_once_lockless(
        &self,
        timeout: std::time::Duration,
        interrupted: &std::sync::atomic::AtomicBool,
    ) {
        use std::sync::atomic::Ordering;
        if self.wake_event == 0 {
            self.park_wait_once_with(timeout, interrupted, false);
            return;
        }
        let guard = self.mutex.lock();
        let sleeper = self.enter_wait();
        let counted = sleeper.is_some();
        if self.take_first(counted) {
            return;
        }
        if interrupted.load(Ordering::Acquire) || timeout.is_zero() {
            return;
        }
        let nanos = timeout.as_nanos();
        let millis = u64::try_from(nanos.div_ceil(1_000_000))
            .unwrap_or(u64::MAX)
            .max(1);
        self.lockless_slot.store(LOCKLESS_WAITING, Ordering::Relaxed);
        drop(guard);
        let woke = win_park::wait_event_coarse(self.wake_event, millis);
        let os_at = if parklat_enabled() { parklat_now_nanos() } else { 0 };
        self.lockless_leave(woke);
        if os_at != 0 && self.permit.load(Ordering::Acquire) {
            self.parklat_report_lockless("lockless-timed", os_at);
        }
        self.permit.store(false, Ordering::Release);
        drop(sleeper);
    }

    /// Windows, `CRATONVM_PARK_EVENT_LOCKLESS_WAKE` (wave 114, lane L1): an
    /// untimed unpolled park ([`Self::park_wait`] with no timeout and no
    /// poll) as one untimed wait on the wake event, with the hand-shake of
    /// [`Self::park_wait_once_lockless`]: the waiter does not re-take the
    /// mutex after a wake that found the permit or the interrupt flag (both
    /// atomics, published by the waker's claim). A wake that found neither (a
    /// stale interrupt wake) re-takes the mutex, re-checks, and waits again.
    /// A FAILED OS wait waits on the condvar under the mutex instead, as
    /// `park_wait_with`'s fallback does. Unlike `CRATONVM_PARK_UNTIMED_EVENT`
    /// (kept off: its waiter re-took the mutex the waker held while it
    /// signalled), nothing is deferred and nothing waits for a late signal.
    #[cfg(target_os = "windows")]
    pub fn park_wait_untimed_lockless(&self, interrupted: &std::sync::atomic::AtomicBool) {
        use std::sync::atomic::Ordering;
        if self.wake_event == 0 {
            self.park_wait_with(None, interrupted, false, false);
            return;
        }
        let mut guard = self.mutex.lock();
        let sleeper = self.enter_wait();
        let counted = sleeper.is_some();
        if self.take_first(counted) {
            return;
        }
        if interrupted.load(Ordering::Acquire) {
            return;
        }
        loop {
            // The mutex is held here: the permit and the flag were checked
            // under it (above, or at the end of the previous round).
            self.lockless_slot.store(LOCKLESS_WAITING, Ordering::Relaxed);
            drop(guard);
            let woke = win_park::wait_event_untimed(self.wake_event);
            let os_at = if parklat_enabled() { parklat_now_nanos() } else { 0 };
            self.lockless_leave(woke);
            if woke == win_park::Woke::Unavailable {
                guard = self.mutex.lock();
                while !(self.permit_set() || interrupted.load(Ordering::Acquire)) {
                    self.condvar.wait(&mut guard);
                }
                self.clear_locked();
                return;
            }
            if self.permit.load(Ordering::Acquire) || interrupted.load(Ordering::Acquire) {
                if os_at != 0 && self.permit.load(Ordering::Acquire) {
                    self.parklat_report_lockless("lockless-untimed", os_at);
                }
                self.permit.store(false, Ordering::Release);
                drop(sleeper);
                return;
            }
            guard = self.mutex.lock();
            if self.permit_set() || interrupted.load(Ordering::Acquire) {
                self.clear_locked();
                return;
            }
        }
    }

    /// Windows (wave 114, lane L1): a lockless event waiter leaves its slot
    /// after the OS wait ended `woke`. A claim the wait did not consume (it
    /// ended on its timeout, or the OS wait failed, while a waker was
    /// claiming) is reset: the claimer raises the event inside the critical
    /// section of its claim, so once this thread holds the mutex the event is
    /// set, and the reset leaves nothing for the thread's next wait to wake
    /// on (the wave-73 rule). The common case -- the event consumed, or no
    /// claim -- takes no lock.
    #[cfg(target_os = "windows")]
    fn lockless_leave(&self, woke: win_park::Woke) {
        let prev = self
            .lockless_slot
            .swap(LOCKLESS_IDLE, std::sync::atomic::Ordering::AcqRel);
        if prev == LOCKLESS_CLAIMED && woke != win_park::Woke::Event {
            let _guard = self.mutex.lock();
            win_park::reset(self.wake_event);
        }
    }

    /// Windows (interpreter round i6 wave 115, lane L1,
    /// `CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT`; page
    /// `i114-L1-proposal-windows-process-wait-wakes-on-interrupt`): announce
    /// the owner as a waiter on this state's wake event for a wait that is
    /// NOT a park -- a native that adds the event to its own
    /// `WaitForMultipleObjects` so `Thread.interrupt` ends it at once, as the
    /// JDK's Windows `ProcessImpl` natives wait on
    /// `JVM_GetThreadInterruptEvent()`. The hand-shake is the lockless
    /// park's ([`Self::lockless_slot`]): stored under the mutex, so every
    /// interrupt wake (`wake_for_interrupt_with`, `interrupt_wake_locked`,
    /// `interrupt_wake_after_unlock`, the interrupt's unpark) that takes the
    /// mutex after this call claims the slot and raises the event, and one
    /// that took it before raised the interrupt flag first -- the caller
    /// tests the flag AFTER this call, before it waits. An ordinary unpark
    /// of the owner raises the event too (the waiter re-tests and waits
    /// again); the permit is neither taken nor left. `None`: no event (the
    /// caller waits as before). Pair every `Some` with exactly one
    /// [`Self::withdraw_event_waiter`]. Owner only, one wait at a time (a
    /// thread is never parked and in such a wait at once).
    #[cfg(target_os = "windows")]
    pub fn announce_event_waiter(&self) -> Option<isize> {
        if self.wake_event == 0 {
            return None;
        }
        let _guard = self.mutex.lock();
        self.lockless_slot
            .store(LOCKLESS_WAITING, std::sync::atomic::Ordering::Relaxed);
        Some(self.wake_event)
    }

    /// Windows (wave 115, lane L1): leave a wait announced with
    /// [`Self::announce_event_waiter`]. `consumed`: the caller's OS wait
    /// returned ON this state's event (an auto-reset event: the signal is
    /// gone). Otherwise a claim made during the wait is reset, as
    /// [`Self::lockless_leave`] does for a lockless park, so no signal is
    /// left for the thread's next park to wake on (the wave-73 rule).
    #[cfg(target_os = "windows")]
    pub fn withdraw_event_waiter(&self, consumed: bool) {
        self.lockless_leave(if consumed {
            win_park::Woke::Event
        } else {
            win_park::Woke::Timer
        });
    }

    /// DBG (`CRATONVM_DBG_PARKLAT`, wave 114): a lockless wake's split, in
    /// the format of the coarse one (`os->relocked` is here the waiter's
    /// slot leave and permit read, no mutex), then the `unpark->wake` line.
    #[cfg(target_os = "windows")]
    #[cold]
    #[inline(never)]
    fn parklat_report_lockless(&self, path: &str, os_at: u64) {
        use std::sync::atomic::Ordering;
        let set_at = self.last_unpark_nanos.load(Ordering::Acquire);
        parklat_report_coarse_split(
            path,
            set_at,
            self.last_signal_nanos.load(Ordering::Acquire),
            os_at,
        );
        parklat_report_wake(path, set_at, false);
    }

    /// The longest OS wait of a polled timed park ([`Self::park_wait`]):
    /// [`PARK_POLL`] on the condvar.
    ///
    /// Windows' accurate path takes the longer 15 ms on purpose. A 5 ms
    /// condvar slice does not cost a wakeup every 5 ms — the OS rounds it out
    /// to ~15.6 ms — so asking the timer for 5 ms would TRIPLE the wakeup
    /// rate of every timed park in the VM to buy interrupt latency nothing
    /// was waiting on (`thread_interrupt` already unparks, and that signal
    /// reaches the wait through the event). 15 ms keeps both the wakeup rate
    /// and the interrupt latency where the condvar path actually had them;
    /// each slice is accurate, so the last one ends ON the deadline instead
    /// of at the next tick after it.
    #[inline]
    fn poll_slice(&self) -> std::time::Duration {
        #[cfg(target_os = "windows")]
        if win_park::enabled() && self.wake_event != 0 {
            return std::time::Duration::from_millis(15);
        }
        PARK_POLL
    }

    /// Wake every waiter on this state without giving a permit, for an
    /// interrupt that has just set its target's flag (interpreter round i3
    /// wave 74, lane L6, `CRATONVM_PARK_UNTIMED_NO_POLL`; called by
    /// `ThreadRegistry::set_interrupted`). Under the mutex, the handshake of
    /// [`Self::park_interruptible_unpolled`]'s flag read; reaches a Windows
    /// waiter on the accurate path through its event. A waiter that finds
    /// no flag (a stale call) waits again.
    pub fn wake_for_interrupt(&self) {
        self.wake_for_interrupt_with(false);
    }

    /// One `Thread.sleep` wait of at most `dur` on this state's condvar
    /// (interpreter round i3 wave 75, lane L6, `CRATONVM_SLEEP_NO_POLL`; page
    /// `i74-L6-proposal-sleep-without-the-10ms-poll-20261107`), in place of
    /// the sleep body's 10 ms `std::thread::sleep` slices. `true`: the
    /// interrupt flag is up (read under the mutex before every wait, so a
    /// [`Self::wake_for_interrupt`] after the flag is raised is never
    /// missed); `false`: `dur` elapsed. HotSpot's `JavaThread::sleep_nanos`
    /// waits on the thread's `_SleepEvent` the same way, and
    /// `JavaThread::interrupt` unparks it.
    ///
    /// The permit is neither taken nor left: a sleep does not end on an
    /// `unpark` (an unpark's wake, or a stale one, finds no flag and waits
    /// out the rest) and keeps the permit for the thread's next park, as
    /// HotSpot's separate `_SleepEvent` does. The wait does not count itself
    /// a sleeper of the lock-free permit (`CRATONVM_PARK_ATOMIC_PERMIT`): a
    /// lock-free unpark need not wake it. Windows: one accurate wait of the
    /// remainder on the event and the high-resolution timer, as a timed park.
    /// A `dur` past `Instant`'s range waits untimed (a sleep-forever).
    pub fn sleep_unpolled(
        &self,
        dur: std::time::Duration,
        interrupted: &std::sync::atomic::AtomicBool,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let deadline = std::time::Instant::now().checked_add(dur);
        let mut guard = self.mutex.lock();
        loop {
            if interrupted.load(Ordering::Acquire) {
                return true;
            }
            match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        return false;
                    }
                    #[cfg(target_os = "windows")]
                    let accurate = self.accurate_wait(&mut guard, remaining);
                    #[cfg(not(target_os = "windows"))]
                    let accurate = false;
                    if !accurate {
                        self.condvar.wait_for(&mut guard, remaining);
                    }
                }
                None => self.condvar.wait(&mut guard),
            }
        }
    }

    /// Take the permit without blocking: `true` when one was set (it is now
    /// consumed). A virtual thread's carrier park drops with it a permit that
    /// is not the thread's `LockSupport` permit (interpreter round i1 wave 28,
    /// lane L4; `VirtualThreadManager::begin_carrier_park`).
    pub fn take_permit(&self) -> bool {
        let _guard = self.mutex.lock();
        self.take_locked()
    }

    /// [`Self::take_permit`] without the mutex (interpreter round i3 wave 73,
    /// lane L6, `CRATONVM_PARK_ATOMIC_PERMIT`; HotSpot's `Parker::park`
    /// starts with `Atomic::xchg(&_counter, 0)` the same way): one load when
    /// no permit is set, one swap when one is. A permit this misses (set
    /// after the load) is found by the wait that follows, which re-checks it
    /// after counting itself a sleeper.
    #[inline]
    pub fn take_permit_lock_free(&self) -> bool {
        self.permit.load(std::sync::atomic::Ordering::Relaxed)
            && self
                .permit
                .swap(false, std::sync::atomic::Ordering::Acquire)
    }

    /// Spin for the permit for up to `budget` before a park's running phase
    /// waits (interpreter round i3 wave 79, lane L6, `CRATONVM_PARK_LAZY_SPIN_NS`;
    /// page `i73-L6-proposal-spin-on-the-atomic-permit-before-the-lazy-wait`).
    /// `true`: the park is over -- the permit was taken (lock-free: the
    /// permit is an `AtomicBool` whatever `CRATONVM_PARK_ATOMIC_PERMIT` says,
    /// and a waker that finds no condvar waiter pays no syscall) or an
    /// interrupt is pending (left set, as a park leaves it). `false`: the
    /// budget ran out or `keep_spinning` turned false (a pause or a
    /// suspension requested); nothing was consumed. The clock, the
    /// interrupt flag and `keep_spinning` are read every 64 iterations; the
    /// permit on every one. The caller is a running mutator throughout.
    pub fn spin_for_permit(
        &self,
        budget: std::time::Duration,
        interrupted: &AtomicBool,
        keep_spinning: &dyn Fn() -> bool,
    ) -> bool {
        let start = std::time::Instant::now();
        let mut spins: u32 = 0;
        loop {
            if self.take_permit_lock_free() {
                return true;
            }
            spins = spins.wrapping_add(1);
            if spins % 64 == 0 {
                if interrupted.load(std::sync::atomic::Ordering::Acquire) {
                    return true;
                }
                if !keep_spinning() || start.elapsed() >= budget {
                    return false;
                }
            }
            std::hint::spin_loop();
        }
    }

    /// The running phase of a park (interpreter round i3 wave 71, lane L1,
    /// `CRATONVM_PARK_LAZY`): wait for the permit for up to `budget`, in
    /// waits of at most [`LAZY_PARK_SLICE`], while `keep_running` says so (the
    /// caller's "no stop-the-world pause is requested"). `true`: the park is
    /// over -- the permit was taken, or an interrupt is pending (a park returns
    /// on one without clearing it). `false`: the budget ran out or
    /// `keep_running` turned false; the permit, if any arrives later, is left
    /// for the blocking park that follows. The caller is a running mutator
    /// throughout; this takes no GC-visible action.
    pub fn park_lazily(
        &self,
        budget: std::time::Duration,
        interrupted: &std::sync::atomic::AtomicBool,
        keep_running: &dyn Fn() -> bool,
    ) -> bool {
        self.park_lazily_sliced(budget, LAZY_PARK_SLICE, interrupted, keep_running)
    }

    /// [`Self::park_lazily`] with the longest single wait given: `slice`.
    /// `keep_running` is read under this state's mutex before every wait, so
    /// a waker that changes its answer and then calls
    /// [`Self::wake_lazy_phase`] (which takes the same mutex) is never
    /// missed: either the read sees the change, or the parker is already
    /// inside the wait when the notify comes. With such a waker the slice can
    /// be the whole budget (interpreter round i3 wave 72, lane L6,
    /// `CRATONVM_PARK_LAZY_STW_WAKE`).
    pub fn park_lazily_sliced(
        &self,
        budget: std::time::Duration,
        slice: std::time::Duration,
        interrupted: &std::sync::atomic::AtomicBool,
        keep_running: &dyn Fn() -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + budget;
        let mut guard = self.mutex.lock();
        let sleeper = self.enter_wait();
        let counted = sleeper.is_some();
        loop {
            if self.take_first(counted) {
                return true;
            }
            if interrupted.load(std::sync::atomic::Ordering::Acquire) {
                return true;
            }
            if !keep_running() {
                return false;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            self.condvar.wait_for(&mut guard, remaining.min(slice));
        }
    }

    /// Wake this state's waiter WITHOUT giving it a permit, so a thread in
    /// the running phase of a park ([`Self::park_lazily_sliced`]) re-reads
    /// its `keep_running` at once (a stop-the-world initiator,
    /// `ThreadRegistry::wake_lazy_park_phases`; interpreter round i3 wave 72,
    /// lane L6, `CRATONVM_PARK_LAZY_STW_WAKE`). Taken under the mutex, which
    /// is the handshake: a parker holds it from its `keep_running` read until
    /// it is inside the wait. A blocking park that happens to be woken by a
    /// stale call finds no permit and waits again (both of
    /// `park_interruptible`'s loops re-check), so a spurious call is harmless.
    pub fn wake_lazy_phase(&self) {
        let _guard = self.mutex.lock();
        self.condvar.notify_all();
    }

    /// Make a permit available; unblock a parked thread.
    ///
    /// If the thread is currently parked, it will be unblocked. If not,
    /// the next call to `park()` will return immediately.
    pub fn unpark(&self) {
        self.unpark_with(UnparkShape::LOCKED);
    }

    /// [`Self::unpark`] that wakes EVERY waiter on the condvar, not one
    /// (interpreter round i3 wave 75, lane L6; with
    /// `CRATONVM_PARK_UNTIMED_NO_POLL`, `vm_exec::unpark_park_state`). A
    /// state's one waiter is its owner, so the two are the same answer --
    /// unless something else waits on the condvar too, in which case a
    /// `notify_one` can land on that waiter and leave the owner asleep. The
    /// 5 ms poll hid such a stolen wake (the owner found the permit on its
    /// next slice); without the poll nothing would, which is what
    /// `L6W74ParkNoPollBench`'s `--compatible` `unpark-wake` row looks like
    /// (page `i73-L6-proposal-untimed-park-without-a-5ms-poll-20261106`,
    /// wave 75). Every waiter re-checks its own condition, so an extra wake
    /// is harmless.
    #[cfg(test)] // only tests call it since the wave-77 one-wait/one-unpark refactor
    pub fn unpark_waking_all(&self) {
        self.unpark_with(UnparkShape {
            wake_all: true,
            ..UnparkShape::LOCKED
        });
    }

    /// [`Self::unpark`] for a thread that is not waiting, without the mutex
    /// or a wake-up syscall (interpreter round i3 wave 73, lane L6,
    /// `CRATONVM_PARK_ATOMIC_PERMIT`; page
    /// `i72-L6-proposal-lock-free-park-permit-20261105`). The permit is
    /// swapped in; if it was already set, whoever set it took care of any
    /// waiter. Otherwise [`Self::sleepers`] is read after the swap (both
    /// `SeqCst`, the Dekker pair with a waiter's count-then-check): zero means
    /// any later waiter finds the permit before it waits; non-zero takes the
    /// mutex -- which a waiter holds from its check until it is inside the
    /// wait -- and notifies, as [`Self::unpark`] does. Safe to mix with
    /// [`Self::unpark`] on the same state.
    #[cfg(test)] // only tests call it since the wave-77 one-wait/one-unpark refactor
    pub fn unpark_lock_free(&self) {
        self.unpark_with(UnparkShape {
            lock_free: true,
            ..UnparkShape::LOCKED
        });
    }

    /// [`Self::unpark_lock_free`] waking every waiter (see
    /// [`Self::unpark_waking_all`]; wave 75).
    #[cfg(test)] // only tests call it since the wave-77 one-wait/one-unpark refactor
    pub fn unpark_lock_free_waking_all(&self) {
        self.unpark_with(UnparkShape {
            lock_free: true,
            wake_all: true,
            after_unlock: false,
        });
    }

    /// [`Self::unpark`] shaped as HotSpot's `Parker::unpark` (interpreter
    /// round i3 wave 76, lane L6, `CRATONVM_PARK_WAKE_AFTER_UNLOCK`; page
    /// `i73-L6-proposal-untimed-park-without-a-5ms-poll-20261106`): the
    /// permit is set under the mutex, the mutex is RELEASED, and only then are
    /// the condvar's waiters woken -- and only when this call set the permit
    /// (`s < 1` in HotSpot; an unpark that finds the permit already set leaves
    /// the wake to the call that set it).
    ///
    /// Notifying with the mutex held (the default) makes `parking_lot` requeue
    /// the waiter onto the mutex; the waker's unlock then wakes it, and with
    /// the mutex's eventual fairness (true for a wake that comes milliseconds
    /// after the last one, as every park handoff does) HANDS the mutex to the
    /// not-yet-running waiter. A second lock of the same state by the waker
    /// -- `unpark(); unpark()`, or `Thread.interrupt`'s flag wake followed by
    /// its unpark -- then blocks until the waiter has run and released it,
    /// and the waiter's release has to wake the waker again: an extra
    /// syscall and an extra context switch on the wake's critical path. Here
    /// the waiter is unparked directly and takes an uncontended mutex.
    ///
    /// Safe without the mutex for the notify: every waiter reads the permit
    /// under the mutex before each wait and the condvar wait releases the
    /// mutex atomically, so a waiter either saw the permit or is in the wait
    /// when the notify comes. Every waiter is woken (`notify_all`, as
    /// [`Self::unpark_waking_all`]): a state's waiter is its owner, and an
    /// extra waiter re-checks its own condition. Windows: the accurate-path
    /// event is signalled under the mutex, as [`Self::unpark`] does.
    #[cfg(test)] // only tests call it since the wave-77 one-wait/one-unpark refactor
    pub fn unpark_after_unlock(&self) {
        self.unpark_with(UnparkShape {
            after_unlock: true,
            ..UnparkShape::LOCKED
        });
    }

    /// [`Self::unpark_lock_free`] with the wake after the mutex is released
    /// (see [`Self::unpark_after_unlock`]; wave 76,
    /// `CRATONVM_PARK_WAKE_AFTER_UNLOCK` with `CRATONVM_PARK_ATOMIC_PERMIT`).
    /// The mutex is still taken (and released) before the notify when a
    /// waiter is counted: a waiter holds it from its permit check until it is
    /// inside the wait, so after the release it is either in the wait or has
    /// seen the permit.
    #[cfg(test)] // only tests call it since the wave-77 one-wait/one-unpark refactor
    pub fn unpark_lock_free_after_unlock(&self) {
        self.unpark_with(UnparkShape {
            lock_free: true,
            wake_all: false,
            after_unlock: true,
        });
    }

    /// The one `LockSupport.unpark` of a state (interpreter round i3 wave 77,
    /// lane L6; page `i76-L6-proposal-one-park-wait-and-one-unpark-20261109`):
    /// the six wrappers above are this with a fixed shape, and
    /// `vm_exec::unpark_park_state` calls it with the VM's
    /// (`ThreadRegistry::unpark_shape`). Each shape behaves exactly as the
    /// separate body it replaced:
    ///
    /// * the permit: under the mutex, or (`lock_free`, on a state whose
    ///   waiters count themselves sleepers) one `SeqCst` swap, the mutex taken
    ///   only when the swap set it and a sleeper is counted;
    /// * the wake: under the mutex, `notify_one` or (`wake_all`)
    ///   `notify_all`, always (locked) or when a sleeper is counted
    ///   (lock-free); or (`after_unlock`) a `notify_all` after the mutex is
    ///   released, only when this call set the permit (`wake_all` is then
    ///   moot);
    /// * Windows: the accurate-path event is signalled under the mutex, on
    ///   the paths that notify.
    pub fn unpark_with(&self, shape: UnparkShape) {
        use std::sync::atomic::Ordering;
        if shape.lock_free && self.lock_free.load(Ordering::Relaxed) {
            if self.permit.swap(true, Ordering::SeqCst) {
                return;
            }
            self.stamp_unpark();
            if self.sleepers.load(Ordering::SeqCst) == 0 {
                return;
            }
            // Wave 108 (lane L6): an untimed event waiter's signal is raised
            // after the mutex is released (`claim_event_signal`).
            let deferred = if shape.after_unlock {
                let deferred = {
                    let _guard = self.mutex.lock();
                    self.claim_event_signal()
                };
                self.condvar.notify_all();
                deferred
            } else {
                let _guard = self.mutex.lock();
                self.notify(shape.wake_all);
                self.claim_event_signal()
            };
            self.raise_deferred_event_signal(deferred);
            return;
        }
        // Not lock-free (asked or kept): its waiters keep no sleeper count,
        // so the permit is set under the mutex.
        if shape.after_unlock {
            let (set_now, deferred) = {
                let _guard = self.mutex.lock();
                let had = self.permit.load(Ordering::Relaxed);
                let mut deferred = false;
                if !had {
                    self.permit.store(true, Ordering::Release);
                    self.stamp_unpark();
                    deferred = self.claim_event_signal();
                }
                (!had, deferred)
            };
            self.raise_deferred_event_signal(deferred);
            if set_now {
                self.condvar.notify_all();
            }
        } else {
            let deferred = {
                let _guard = self.mutex.lock();
                self.permit.store(true, Ordering::Release);
                self.stamp_unpark();
                self.notify(shape.wake_all);
                // A thread on the accurate path is not on the condvar; wake it
                // too (only then: wave 73, see `event_waiter`).
                self.claim_event_signal()
            };
            self.raise_deferred_event_signal(deferred);
        }
    }

    /// `notify_all` or `notify_one` on the condvar.
    #[inline]
    fn notify(&self, all: bool) {
        if all {
            self.condvar.notify_all();
        } else {
            self.condvar.notify_one();
        }
    }

    /// DBG (`CRATONVM_DBG_PARKLAT`): record when an unpark set the permit.
    #[inline]
    fn stamp_unpark(&self) {
        if parklat_enabled() {
            self.last_unpark_nanos
                .store(parklat_now_nanos(), std::sync::atomic::Ordering::Release);
        }
    }

    /// `Thread.interrupt`'s whole wake of this state in ONE trip through the
    /// mutex (wave 76, `CRATONVM_PARK_WAKE_AFTER_UNLOCK`): the caller has
    /// raised the target's interrupt flag; this sets the permit (HotSpot's
    /// `JavaThread::interrupt` unparks the `Parker`, so a park that has
    /// already returned finds a permit on its next call, as before) and then,
    /// with the mutex released, wakes every waiter -- a park, a running-phase
    /// park and a `Thread.sleep` alike. ALWAYS wakes, even when the permit
    /// was already set: a sleep does not consume the permit, so a sleeper
    /// with a leftover permit still has to be woken to see the flag.
    /// Replaces the two trips of the default (`set_interrupted`'s
    /// [`Self::wake_for_interrupt`], then `thread_interrupt`'s unpark), whose
    /// second lock met the mutex handed to the woken waiter (see
    /// [`Self::unpark_after_unlock`]).
    pub fn interrupt_wake_after_unlock(&self) {
        use std::sync::atomic::Ordering;
        let deferred = {
            let _guard = self.mutex.lock();
            if !self.permit.load(Ordering::Relaxed) {
                self.permit.store(true, Ordering::Release);
                self.stamp_unpark();
            }
            self.claim_event_signal()
        };
        self.raise_deferred_event_signal(deferred);
        self.condvar.notify_all();
    }

    /// `Thread.interrupt`'s whole wake of this state in ONE trip through the
    /// mutex, with the notify still UNDER it (interpreter round i4 wave 91,
    /// lane L6, `CRATONVM_INTERRUPT_ONE_WAKE`; page
    /// `interpreter-i77-L6-proposal-one-trip-interrupt-wake-DONE-20261127`): the caller has
    /// raised the target's flag without a wake; this sets the permit (when
    /// not set already), wakes every waiter -- a park, a running-phase park,
    /// a `Thread.sleep` -- and signals a Windows waiter on the accurate path.
    /// The union of the default's two trips ([`Self::wake_for_interrupt`]'s
    /// `notify_all`, then the locked unpark's permit and notify), minus the
    /// second lock; unlike [`Self::interrupt_wake_after_unlock`] the
    /// notify's placement is the default's. Correct on a lock-free state too:
    /// the permit is set under the mutex, which a counted waiter holds from
    /// its check until it is inside the wait.
    pub fn interrupt_wake_locked(&self) {
        use std::sync::atomic::Ordering;
        let deferred = {
            let _guard = self.mutex.lock();
            if !self.permit.load(Ordering::Relaxed) {
                self.permit.store(true, Ordering::Release);
                self.stamp_unpark();
            }
            self.condvar.notify_all();
            self.claim_event_signal()
        };
        // Wave 108 (lane L6): an untimed event waiter is signalled here,
        // after the mutex is released (`claim_event_signal`).
        self.raise_deferred_event_signal(deferred);
    }

    /// `Thread.interrupt`'s unpark of its target's state, after the flag is
    /// raised (interpreter round i3 wave 77, lane L6; the choice
    /// `vm_exec`'s `thread_interrupt` made inline): with `after_unlock`
    /// [`Self::interrupt_wake_after_unlock`] (the flag wake folded in);
    /// otherwise the locked unpark -- `notify_all` with `wake_all`
    /// (`CRATONVM_PARK_UNTIMED_NO_POLL`), else `notify_one`. The shape's
    /// `lock_free` is not used: the interrupt's unpark has always set the
    /// permit under the mutex, which is correct on a lock-free state too
    /// (its waiter holds the mutex from its count-then-check until it is
    /// inside the wait).
    pub fn interrupt_unpark(&self, shape: UnparkShape) {
        if shape.after_unlock {
            self.interrupt_wake_after_unlock();
        } else {
            self.unpark_with(UnparkShape {
                lock_free: false,
                ..shape
            });
        }
    }

    /// [`Self::wake_for_interrupt`] with the wake after the mutex is released
    /// (wave 76, `CRATONVM_PARK_WAKE_AFTER_UNLOCK`): no permit is given. The
    /// mutex is taken and released first -- the handshake with a waiter's
    /// flag read -- so the waiter has either seen the flag or is in the wait.
    #[cfg(test)] // only tests call it since the wave-77 one-wait/one-unpark refactor
    pub fn wake_for_interrupt_after_unlock(&self) {
        self.wake_for_interrupt_with(true);
    }

    /// The flag-only wake of an interrupt: [`Self::wake_for_interrupt`]
    /// (`after_unlock` false: `notify_all` under the mutex) or
    /// [`Self::wake_for_interrupt_after_unlock`] (interpreter round i3 wave
    /// 77, lane L6: one body for both). Windows: the accurate-path event is
    /// signalled under the mutex either way.
    pub fn wake_for_interrupt_with(&self, after_unlock: bool) {
        let deferred = if after_unlock {
            let deferred = {
                let _guard = self.mutex.lock();
                self.claim_event_signal()
            };
            self.condvar.notify_all();
            deferred
        } else {
            let _guard = self.mutex.lock();
            self.condvar.notify_all();
            self.claim_event_signal()
        };
        self.raise_deferred_event_signal(deferred);
    }

    /// How many threads wait on this state now. Tests.
    #[cfg(test)]
    pub(crate) fn sleeper_count(&self) -> u32 {
        self.sleepers.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Windows test hook: is the wake event left signalled? (Consumes it.)
    #[cfg(all(test, target_os = "windows"))]
    pub(crate) fn wake_event_left_signalled(&self) -> bool {
        win_park::is_signalled(self.wake_event)
    }
}

impl Default for ParkState {
    fn default() -> Self {
        Self::new()
    }
}

/// `CRATONVM_PARK_LAZY_ADAPTIVE`'s per-thread streak (interpreter round i3
/// wave 81, lane L6), owner-only, aligned to a cache line of its own: the
/// permit, the mutex and the condvar beside it are written by the unparking
/// thread on every handoff, and a streak write on their line made the two
/// sides of a ping-pong trade it.
#[repr(align(64))]
#[derive(Debug)]
struct LazyParkStreak {
    /// Running phases in a row that ran out their budget (saturating). At
    /// [`LAZY_PARK_MISSES_TO_SKIP`] the owner's parks skip the phase.
    misses: AtomicU8,
    /// Parks that skipped the phase since the last one that ran it: every
    /// [`LAZY_PARK_RECHECK_EVERY`]th runs it again.
    skips: AtomicU8,
}

/// `CRATONVM_PARK_LAZY_ADAPTIVE` (interpreter round i3 wave 81, lane L6): a
/// thread whose last this-many running phases all ran out their budget skips
/// the phase (HotSpot's adaptive spinning shrinks `_SpinDuration` alike).
pub const LAZY_PARK_MISSES_TO_SKIP: u8 = 4;

/// `CRATONVM_PARK_LAZY_ADAPTIVE`: a skipping thread still runs the phase on
/// every this-many-th park, to see a handoff partner again.
pub const LAZY_PARK_RECHECK_EVERY: u8 = 16;

/// [`ParkState::lazy_phase_skip`]'s decision from the miss streak and the
/// skips since the phase last ran: `(skip this park, the skips to store)`.
pub fn lazy_park_skip_decision(misses: u8, skips: u8) -> (bool, u8) {
    if misses < LAZY_PARK_MISSES_TO_SKIP {
        return (false, 0);
    }
    let next = skips.saturating_add(1);
    if next >= LAZY_PARK_RECHECK_EVERY {
        (false, 0)
    } else {
        (true, next)
    }
}

#[cfg(test)]
mod i81_l6_adaptive_lazy_park_tests {
    use super::*;

    /// Below the streak every park runs the phase; at it, fifteen parks skip
    /// and the sixteenth runs it again; a handoff resets both counters.
    #[test]
    fn the_skip_decision_follows_the_streak_and_rechecks() {
        assert_eq!(lazy_park_skip_decision(0, 0), (false, 0));
        assert_eq!(
            lazy_park_skip_decision(LAZY_PARK_MISSES_TO_SKIP - 1, 7),
            (false, 0)
        );
        let state = ParkState::new();
        for _ in 0..LAZY_PARK_MISSES_TO_SKIP {
            assert!(!state.lazy_phase_skip());
            state.note_lazy_phase_ran_out();
        }
        let skipped = (0..LAZY_PARK_RECHECK_EVERY)
            .filter(|_| state.lazy_phase_skip())
            .count();
        assert_eq!(skipped, usize::from(LAZY_PARK_RECHECK_EVERY) - 1);
        state.note_lazy_handoff();
        assert!(!state.lazy_phase_skip());
        // The streak saturates instead of wrapping back below the threshold.
        for _ in 0..300 {
            state.note_lazy_phase_ran_out();
        }
        assert!(state.lazy_phase_skip());
    }
}

/// Whether a thread is a platform (OS) thread or a virtual thread (JEP 444).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadKind {
    /// Traditional platform thread backed by an OS thread.
    Platform,
    /// Virtual thread (Java 21+) — lightweight, scheduled onto carrier threads.
    Virtual,
}

/// Why a virtual thread's continuation yielded (interpreter round i1 wave 24,
/// lane L7; page
/// `interpreter-L7-an-interrupt-does-not-reach-an-unmounted-virtual-thread`).
///
/// A remounted continuation resumes AFTER the yielding native's invoke, as if
/// the native had returned normally. That is right for a park (which returns
/// on an unpark, an interrupt or spuriously), and wrong for a sleep or a
/// keyed wait woken by an interrupt: HotSpot throws `InterruptedException`
/// out of those. The remount reads this record to raise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtYield {
    /// `LockSupport.park` / `parkNanos`: resumes normally, whatever woke it.
    Park,
    /// `Thread.sleep`: an interrupt throws `InterruptedException("sleep
    /// interrupted")`; any other early wake sleeps out the rest. `None`: the
    /// deadline overflowed `Instant` (a sleep-forever).
    Sleep {
        deadline: Option<std::time::Instant>,
    },
    /// A wait on a VM-local key (`CountDownLatch.await`'s native): an
    /// interrupt cancels the registration and throws `InterruptedException`.
    KeyedWait { key: u64 },
    /// `jdk.internal.vm.Continuation.doYield()` of a JDK-driven continuation
    /// (`JvmThread::jdk_continuation`): the remount resumes past the native's
    /// invoke with its `int` result, `0` ("yielded"), on the operand stack.
    DoYield,
    /// A contended interpreted `monitorenter` unmounted the JDK-driven
    /// continuation (`CRATONVM_VTHREAD_MONITOR_UNMOUNT`, interpreter round i4
    /// wave 87, lane L6): the leaf frame stands AT the `monitorenter` with the
    /// object back on its operand stack, so the remount re-executes it.
    MonitorEnter,
    /// `Object.wait` unmounted the JDK-driven continuation (wave 89, same
    /// switch): the leaf stands past `wait0`'s invoke, and the remount
    /// re-acquires the monitor (`vm_exec::vthread_wait_remount`) before the
    /// frame runs on, or throws `InterruptedException`.
    MonitorWait,
}

/// A thread that runs a `jdk.internal.vm.Continuation` the JDK drives
/// (`ContinuationSupport.isSupported0()` answers `true`;
/// `VmConfig::jdk_continuations`): a `java.lang.VirtualThread`'s continuation,
/// mounted by its carrier's `Continuation.enterSpecial` and unmounted by its
/// own `doYield` (`vm::jdk_continuation`). One word (the struct's tail
/// padding, `runtime::layout_ratchet_tests`): bit 63 is [`Self::jdk`], bit
/// 62 [`Self::post_yield_window`] (gci i25/vt), the rest [`Self::carrier`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JdkContinuationState(u64);

impl JdkContinuationState {
    const JDK: u64 = 1 << 63;
    /// gci i25/vt: see [`Self::post_yield_window`].
    const POST_YIELD_WINDOW: u64 = 1 << 62;
    /// Every flag bit; [`Self::carrier`] is the rest.
    const FLAGS: u64 = Self::JDK | Self::POST_YIELD_WINDOW;

    /// gci i25/vt (`gci-i24vl-g1-virtual-thread-lost-when-after-yield-throws-oome-FIXED-20261017`
    /// residuals 1 and 2): on a CARRIER, its last `Continuation.enterSpecial`
    /// left a post-yield window open -- the slice yielded with the virtual
    /// thread TIMED_PARKING or TIMED_WAITING, whose `VirtualThread.afterYield()`
    /// allocates with no `OutOfMemoryError` handling. The window itself is
    /// kept per VM (`jdk_continuation::ContinuationRunners`); this bit only
    /// lets the carrier's next Java-level park skip that lookup when no
    /// window is open (`jdk_continuation::resolve_post_yield_window`).
    #[inline]
    pub fn post_yield_window(self) -> bool {
        self.0 & Self::POST_YIELD_WINDOW != 0
    }

    /// Raise or lower [`Self::post_yield_window`].
    #[inline]
    pub fn set_post_yield_window(&mut self, open: bool) {
        if open {
            self.0 |= Self::POST_YIELD_WINDOW;
        } else {
            self.0 &= !Self::POST_YIELD_WINDOW;
        }
    }

    /// This thread runs a JDK-driven continuation, for life. Its parks,
    /// sleeps and waits never yield through the VM's own continuation path
    /// (`vt_park_for` and the like answer no): the JDK yields it, through
    /// `Continuation.yield` and `doYield`.
    #[inline]
    pub fn jdk(self) -> bool {
        self.0 & Self::JDK != 0
    }

    /// Mark this thread a JDK-driven continuation's (see [`Self::jdk`]).
    #[inline]
    pub fn set_jdk(&mut self) {
        self.0 |= Self::JDK;
    }

    /// The registry id of the carrier the continuation is mounted on, `0`
    /// while it is not mounted. On a carrier whose `Thread.currentThread()`
    /// was re-pointed (`Thread.setCurrentThread`), the carrier's own id: what
    /// `Thread.currentCarrierThread()` answers.
    #[inline]
    pub fn carrier(self) -> u64 {
        self.0 & !Self::FLAGS
    }

    /// Set [`Self::carrier`] (`0`: none). The flag bits are kept.
    #[inline]
    pub fn set_carrier(&mut self, carrier: u64) {
        self.0 = (self.0 & Self::FLAGS) | (carrier & !Self::FLAGS);
    }
}

/// A frame of a yielded continuation whose return value must be converted
/// before it reaches its caller (interpreter round i1 wave 25, lane L7; see
/// `interpreter::note_continuation_return_boundary`).
///
/// A lambda proxy's SAM call is served by a nested Rust call into the impl
/// method (`try_lambda_dispatch`), and that Rust frame converts the impl's
/// return to the SAM's (`coerce_return`: box, unbox, widen, or drop it for a
/// `void` SAM). A continuation keeps only the Java frames: when it remounts,
/// the impl frame returns straight into the frame that made the SAM call, so
/// the conversion the Rust frame would have done is recorded here and applied
/// by the interpreter's return arms instead.
#[derive(Debug, Clone)]
pub struct ContinuationReturnAdapter {
    /// Index of the impl frame in `JvmThread::frames`.
    pub frame_index: usize,
    /// That frame's `Frame::seq`: an entry whose frame has left (an exception
    /// unwound it, or a new frame reuses the index) no longer matches.
    pub frame_seq: u64,
    /// The return token the caller's call site expects (the SAM call's
    /// descriptor), e.g. `V`, `I`, `Ljava/lang/Object;`.
    pub caller_return: Box<str>,
}

/// Unique identifier for a JVM thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Thread-{}", self.0)
    }
}

/// What compiled code needs to take an uncontended `monitorenter` /
/// `monitorexit` inline (both JIT tiers, `jit/src/runtime_lowering.rs`
/// `emit_inline_thin_lock`): the thin-lock owner bits to CAS into the 32-bit
/// mark word, this thread's JMX lock stack to record the acquisition in, and
/// the lease slot's `LeaseBlock` (its thin-lock count, and the inflated-monitor
/// cache the inflated arm reads). Located through
/// `JitRuntimeHelpers::monitor_block_offset_in_thread`
/// ([`JvmThread::jit_monitor_block_offset`]).
///
/// Compiled code reads `thin_owner`, `lock_stack` and `held`, writes the four
/// census words and adds to / subtracts from the count at `*held` (a plain
/// `ADD` / `SUB`: this thread is its only writer), and reads the inflated
/// cache behind it, all at `cratonvm_jit_api::JIT_MONITOR_BLOCK_*` /
/// `cratonvm_jit::runtime_lowering::LEASE_BLOCK_*`
/// (pinned below). `thin_owner == u64::MAX` means "no inline path": the thread
/// is not armed yet (armed lazily by the first helper-path `monitorenter`, see
/// `ThreadRegistry::arm_jit_monitor_block`), every lock slot of the VM is
/// leased (the helpers inflate for such a thread), it is not registered, or
/// `CRATONVM_MONITOR_FASTPATH=0`.
///
/// `thin_owner` names the thread's per-VM lock-slot LEASE
/// (`monitor::LockSlots`), not its id. A lease lasts until the thread dies
/// (`LockSlots::release`, from `release_monitors_held_by_except`, after which
/// no Java code runs on it), so an armed block can never name a slot another
/// thread holds. `held` points into the same `MonitorTable`'s `LockSlots`;
/// `registry_id` ties both to one VM (the table and the registry live side by
/// side in `SharedVm::threads`), and arming from another registry disarms
/// first. `book` owns the allocation `lock_stack` points into, so the address
/// cannot dangle while this `JvmThread` lives — it is the very `Arc` the
/// registry entry holds, so compiled code and the helpers write ONE lock
/// stack.
///
/// The four census counters (round 11 wave 7) answer "did the inline path
/// fire?", which timing alone could not: compiled monitor sites built under
/// `CRATONVM_DBG_JITC` add 1 to the matching counter on the inline path and on
/// the fall to the helper; `vm_exec::monitor_enter_blocking` and this block's
/// `Drop` print them (`[monitor-inline census]`). Without the flag no site
/// writes them and they stay zero.
#[repr(C)]
pub(crate) struct JitMonitorBlock {
    /// `MARK_THIN_LOCKED | slot << THIN_LOCK_OWNER_SHIFT`, or `u64::MAX`.
    thin_owner: u64,
    lock_stack: usize,
    /// Census counters at `JIT_MONITOR_BLOCK_{INLINE,SLOW}_{ENTERS,EXITS}_OFFSET`.
    /// Written only by this thread's compiled code (a plain `ADD`); atomics so
    /// Rust reads them as memory the emitted code may have changed behind its
    /// references. Nothing here needs a read-modify-write.
    census_inline_enters: std::sync::atomic::AtomicU64,
    census_inline_exits: std::sync::atomic::AtomicU64,
    census_slow_enters: std::sync::atomic::AtomicU64,
    census_slow_exits: std::sync::atomic::AtomicU64,
    /// Address of the lease slot's `LeaseBlock` (inside the VM's
    /// `MonitorTable`, which outlives every thread of that VM; its first word
    /// is the lessee's `u32` thin-lock count); `0` while unarmed.
    held: usize,
    /// `ThreadRegistry::registry_id` the block was armed from; `0` = unarmed.
    registry_id: u64,
    book: Option<Arc<crate::threading::thread_registry::JmxMonitorBook>>,
    /// `CRATONVM_DBG_JITC`, read once per thread: 0 = not read yet, 1 = off,
    /// 2 = on. Per thread rather than a process-wide latch.
    census: u8,
    /// Bit length of the counters' total at the last census line, so a thread
    /// prints at most one line per doubling.
    census_reported_bits: u32,
}

const _: () = {
    use cratonvm_jit_api as api;
    assert!(
        std::mem::offset_of!(JitMonitorBlock, thin_owner)
            == api::JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET
    );
    assert!(std::mem::offset_of!(JitMonitorBlock, held) == api::JIT_MONITOR_BLOCK_HELD_OFFSET);
    assert!(
        std::mem::offset_of!(JitMonitorBlock, lock_stack)
            == api::JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET
    );
    assert!(
        std::mem::offset_of!(JitMonitorBlock, census_inline_enters)
            == api::JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET
    );
    assert!(
        std::mem::offset_of!(JitMonitorBlock, census_inline_exits)
            == api::JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET
    );
    assert!(
        std::mem::offset_of!(JitMonitorBlock, census_slow_enters)
            == api::JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET
    );
    assert!(
        std::mem::offset_of!(JitMonitorBlock, census_slow_exits)
            == api::JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET
    );
    assert!(std::mem::size_of::<std::sync::atomic::AtomicU64>() == 8);
};

/// `JitMonitorBlock::census`: the flag has not been read on this thread yet.
const CENSUS_UNREAD: u8 = 0;
/// `JitMonitorBlock::census`: `CRATONVM_DBG_JITC` is on.
const CENSUS_ON: u8 = 2;

impl JitMonitorBlock {
    /// The "no inline path" state every thread starts in.
    pub(crate) const fn unarmed() -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            thin_owner: u64::MAX,
            lock_stack: 0,
            census_inline_enters: AtomicU64::new(0),
            census_inline_exits: AtomicU64::new(0),
            census_slow_enters: AtomicU64::new(0),
            census_slow_exits: AtomicU64::new(0),
            held: 0,
            registry_id: 0,
            book: None,
            census: CENSUS_UNREAD,
            census_reported_bits: 0,
        }
    }

    /// Whether this block was armed from the registry `registry_id`.
    #[inline]
    pub(crate) fn is_armed_for(&self, registry_id: u64) -> bool {
        self.thin_owner != u64::MAX && self.registry_id == registry_id
    }

    /// Whether compiled code on this thread may take the inline path now.
    #[inline]
    pub(crate) fn is_armed(&self) -> bool {
        self.thin_owner != u64::MAX
    }

    /// Whether the monitor census is on for this thread (`CRATONVM_DBG_JITC`,
    /// read on the first call and kept per thread): one byte compare after that.
    #[inline]
    pub(crate) fn census_on(&mut self) -> bool {
        if self.census == CENSUS_UNREAD {
            self.census = if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                CENSUS_ON
            } else {
                1
            };
        }
        self.census == CENSUS_ON
    }

    /// `(inline enters, inline exits, helper enters, helper exits)` taken by
    /// this thread's census-compiled monitor sites.
    pub(crate) fn census_counts(&self) -> (u64, u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.census_inline_enters.load(Relaxed),
            self.census_inline_exits.load(Relaxed),
            self.census_slow_enters.load(Relaxed),
            self.census_slow_exits.load(Relaxed),
        )
    }

    /// Whether a census line is due: the counters' total has reached a new
    /// power of two since the last line (so a thread prints O(log n) lines).
    pub(crate) fn census_due(&mut self) -> bool {
        let (a, b, c, d) = self.census_counts();
        let total = a.saturating_add(b).saturating_add(c).saturating_add(d);
        let bits = u64::BITS - total.leading_zeros();
        if bits > self.census_reported_bits {
            self.census_reported_bits = bits;
            true
        } else {
            false
        }
    }

    /// Arm the inline path: `lock_stack` must be an address inside `book`, and
    /// `lease` the calling thread's own lock-slot lease from the
    /// `MonitorTable` of the same VM (`MonitorTable::jit_thin_lease`). A null
    /// counter or lock-stack address leaves the block unarmed.
    ///
    /// Order matters only against compiled code on THIS thread, which cannot
    /// run concurrently with this call (the caller is a helper it called), so
    /// plain stores suffice. `thin_owner` is written last and is what enables
    /// it.
    pub(crate) fn arm(
        &mut self,
        lease: crate::threading::monitor::JitThinLease,
        registry_id: u64,
        book: Arc<crate::threading::thread_registry::JmxMonitorBook>,
        lock_stack: usize,
    ) {
        self.disarm();
        if lease.held == 0 || lock_stack == 0 {
            return;
        }
        self.book = Some(book);
        self.lock_stack = lock_stack;
        self.held = lease.held;
        self.registry_id = registry_id;
        self.thin_owner = u64::from(lease.owner_bits);
    }

    /// Back to the unarmed state: compiled code takes the helper path again.
    /// `thin_owner` first, which is what disables the inline path. The census
    /// counters and the flag reading survive: they describe the thread, not
    /// the arming.
    pub(crate) fn disarm(&mut self) {
        self.thin_owner = u64::MAX;
        self.lock_stack = 0;
        self.held = 0;
        self.registry_id = 0;
        self.book = None;
    }
}

impl Drop for JitMonitorBlock {
    /// The census's last word for a thread that runs no helper-path
    /// `monitorenter` after its hot loop (a worker that locks inline and then
    /// ends): the only line that reports its totals.
    fn drop(&mut self) {
        if self.census != CENSUS_ON {
            return;
        }
        let (ie, ix, se, sx) = self.census_counts();
        if (ie | ix | se | sx) == 0 {
            return;
        }
        eprintln!(
            "[monitor-inline census] thread-end thin_owner={:#x} inline_enters={ie} \
             inline_exits={ix} helper_enters={se} helper_exits={sx}",
            self.thin_owner
        );
    }
}

/// One local slot of a lambda proxy method's frame ([`LambdaProxyLocals`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ProxyLocalSlot {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    /// A reference: `JvmThread::native_pin_roots[pin]` holds its current
    /// address (the proxy itself standing in for `null`, so a write can
    /// always replace the pin in place).
    Ref {
        pin: usize,
        null: bool,
    },
    /// The second slot of a `long` or a `double`.
    Second,
}

/// The locals of a lambda proxy method's frame while a call runs through
/// it under a C JVMTI agent (the i54-L1 retirement,
/// `docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md`):
/// slot 0 the proxy, then the functional method's arguments as the caller
/// passed them, which HotSpot's spun frame holds
/// (`tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.java`).
/// The VM runs no such frame, so the C table's local-variable functions read
/// and write these (`jvmti::native_env`). Pushed with the frame's row
/// (`interpreter::lambda`'s proxy dispatch), popped with it.
#[derive(Clone, Debug, Default)]
pub struct LambdaProxyLocals {
    pub slots: Vec<ProxyLocalSlot>,
}

/// Per-thread execution state.
///
/// Each Java thread (including the main thread) has its own `JvmThread`.
/// This struct holds all state that is local to a single thread of execution.
///
/// # Layout
///
/// `#[repr(C, align(64))]` (interpreter round i2 wave 59, lane L7; proposal
/// `docs/internal/fixed-bugs/interpreter-L7-proposal-one-layout-for-the-debugger-build-and-the-plain-build-FIXED-20261023.md`):
/// declaration order, a hot head first (see the comment that opens the
/// fields), and a cache-line-aligned base, so an offset in the layout ratchet
/// (`runtime::frame::layout_ratchet_tests`) IS the field's cache line. Under
/// rustc's own order the hot fields moved whenever any field was added
/// anywhere (one `FrameStack` word moved `frames` from 792 to 240 at the
/// wave-58 merge), and with an 8- or 16-aligned base (a `Box` from the
/// allocator, a stack slot) which hot fields shared a line depended on where
/// the thread happened to be allocated, not on the binary alone.
#[repr(C, align(64))]
pub struct JvmThread {
    // ── The hot head (interpreter round i2 wave 59, lane L7) ────────────────
    //
    // `#[repr(C)]` keeps declaration order, so the fields the dispatch loop,
    // the invoke doors, the allocation path and the native-call funnel read
    // per bytecode or per call are declared HERE, first, and sit together in
    // the struct's first lines whatever is added below. A field added after
    // the head moves no hot field (rustc's own ordering, by alignment,
    // reshuffled all of them for one word: waves 53 and 58); a field added to
    // the head moves only the head fields after it, and the layout ratchet
    // (`runtime::frame::layout_ratchet_tests`) names every row that moved.
    // Add a field to the head only when a measurement says it is read per
    // bytecode or per call; everything else goes below the head.
    /// Live execution frames. The last element is the currently executing frame.
    /// Frames are pushed on method entry and popped on method return.
    /// Stack traces are derived from frames on demand (in capture_stack_trace).
    ///
    /// [`FrameStack`] is a drop-in for the `Vec<Frame>` this used to be — same
    /// methods, same `Index`, same iteration, and it still coerces to
    /// `&[Frame]` — but it additionally guarantees that a frame's **address**
    /// does not change while it is on the stack, except across an explicit,
    /// observable relocation (`FrameStack::reloc_epoch`). Reserving with
    /// `FrameStack::reserve_stable(n)` rules relocation out for the next `n`
    /// pushes, which is what lets the interpreter hoist a single
    /// `*mut Frame` across the dispatch loop instead of re-indexing
    /// `thread.frames[frame_idx]` on every access. See the `FrameStack` docs
    /// for the aliasing rules that come with the raw-pointer API.
    pub frames: FrameStack,

    /// Thread-local invoke cache — maps (caller_class, cp_index) to resolved targets.
    /// No locking needed since each thread owns its cache.
    ///
    /// The JIT arm holds [`cratonvm_jit::RetainedCode`] rather than a bare
    /// `Arc<CompiledMethod>`. This cache is evicted by the thread that
    /// dispatches through it — `get` auto-evicts a stale entry, `put`
    /// replaces one, `evict`/`clear` drop whole call sites — and those
    /// evictions can run *while a frame of the evicted body is on this very
    /// stack*. Once the JIT cache has retired the body, this entry is its last
    /// owner, so a plain `Arc` drop would `munmap` the code under the
    /// thread's own return address. Measured on
    /// `BasicErrorControllerIntegrationTests`: released at
    /// `active_jit_executions` = 2 and 3.
    pub invoke_cache: InvokeCache<cratonvm_jit::RetainedCode>,

    /// Per-thread resolved-field site cache — the interpreter's "resolved
    /// constant pool" for `getfield`/`putfield`/`getstatic`/`putstatic`.
    ///
    /// The authoritative `SharedVm::resolution_cache` already memoizes
    /// `(referencing class, cp index) -> ResolvedField`, but reaching it costs an
    /// `OrderedPlRwLock` read, and `resolve_field_ref_loader_aware` then
    /// *revalidates* every hit by re-deriving the field-owning class from its
    /// name — two `String` allocations, two further `class_manager` read
    /// acquisitions and a full `resolve_class_loader_aware`. This side table
    /// skips all of it for the sites where that revalidation is a tautology.
    /// See [`crate::runtime::interpreter::site_cache`] for the validity
    /// argument — read it before adding a `put` call site.
    pub field_sites: crate::runtime::interpreter::FieldSiteCache,
    /// The site tables the dispatch loop's fused helpers read, grouped so a
    /// helper takes one argument reaching all three (interpreter round i3
    /// wave 83, lane L7; `site_cache::InterpSiteTables`):
    ///
    /// * `fast_field_sites` -- quickened instance-field sites: the receiver
    ///   class the site last resolved against and the compact-layout offset /
    ///   storage kind of the field in that class, so the `getfield` /
    ///   `putfield` fast arms can load or store without resolving anything.
    ///   Same key and epoch validation as `field_sites`; filled by the slow
    ///   handler on the access that resolved the field.
    /// * `static_field_sites` -- quickened `getstatic` sites: the resolved
    ///   field of a site whose declaring class was already initialized when it
    ///   was filled, so a hit skips resolution AND the class-initialization
    ///   check. Same key and epoch validation as `field_sites`; admission
    ///   rule in `interpreter::field_fast::fill_static_site`.
    /// * `prim_const_sites` -- numeric `ldc` / `ldc2_w` constants, so a
    ///   warm site takes neither the `class_manager` nor the
    ///   `resolution_cache` read lock. See
    ///   `site_cache::PrimitiveConstantSiteCache`.
    ///
    /// Until wave 83 these were three fields (`fast_field_sites` at 224,
    /// `static_field_sites` at 248, `prim_const_sites` at 368); the first
    /// two keep their offsets, the constant table moved up to 272.
    pub site_tables: crate::runtime::interpreter::InterpSiteTables,
    /// The trivial-getter fast path's descriptor verdict per `(getter's
    /// class, field cp index)`, with the getter's descriptor it was taken
    /// for: whether the field reference's descriptor is exactly the getter's
    /// return descriptor. A pure function of the class's constant pool, so it
    /// is tagged with the same epochs as `field_sites` and a hit skips the
    /// `class_manager` read lock the check otherwise takes on every call.
    /// JIT round 16 wave 17 (lane fieldsite3, FS16-4), behind
    /// `CRATONVM_TRIVIAL_GETTER_DESC_MEMO` (default OFF).
    pub trivial_getter_verdicts:
        crate::runtime::interpreter::site_cache::SiteCache<(std::sync::Arc<str>, bool)>,

    /// Per-thread resolved-method site cache — the same "resolved constant
    /// pool" for the `(descriptor, num_params)` pair that the argument-popping
    /// helpers need. On the inline-cache HIT path those used to call
    /// `resolve_method_ref` for two of its four return values, paying a
    /// `resolution_cache` read lock, a hash probe and three `Arc<str>`
    /// clone/drop pairs per invoke.
    pub method_sites: crate::runtime::interpreter::MethodSiteCache,

    /// Per-thread resolved `new`-site cache — the same "resolved constant pool"
    /// for the class an allocation site allocates.
    ///
    /// The interpreter's `new` handler re-derived its answer on every single
    /// execution: a `String` allocated for the class name, a full
    /// `resolve_class_loader_aware`, and FOUR `class_manager` read
    /// acquisitions (name, access check, initialization, field count). At
    /// netty's `AdaptivePoolingAllocator` allocation rate that is the
    /// per-allocation cost the throughput page asked to explain. See
    /// [`crate::runtime::interpreter::site_cache::ClassSiteCache`] for which
    /// sites are admissible and what a hit is allowed to skip.
    pub class_sites: crate::runtime::interpreter::ClassSiteCache,

    /// Per-thread resolved `checkcast`/`instanceof` target classes. Separate
    /// from `class_sites` on purpose — see `CastSiteCache`'s docs for why a
    /// shared table would let a `checkcast` fill answer a `new`.
    pub cast_sites: crate::runtime::interpreter::CastSiteCache,

    /// Per-thread memo for the interface receiver-selection re-check that
    /// `execute_invokevirtual_cached` performs on every `invokeinterface`
    /// cache hit. See `IfaceSelectSiteCache` for what it stores and why the
    /// value has to carry the receiver class as well as the site.
    pub iface_select_sites: crate::runtime::interpreter::IfaceSelectSiteCache,

    /// Per-thread copy of the linked JDK-factory `invokedynamic` sites, so a
    /// warm site is executed without the `resolution_cache` lock and hash.
    /// See `runtime::invokedynamic::IndySiteCache`.
    pub indy_sites: crate::runtime::invokedynamic::IndySiteCache,

    /// The signature-polymorphic site routes this thread's interpreter
    /// cached, and the cell its slow path records a route into
    /// (`runtime::interpreter::sig_poly_site`; interpreter round i1 wave 48,
    /// lane L7). Empty unless `VmConfig::sig_poly_site_cache` is on.
    pub(crate) sig_poly_sites: crate::runtime::interpreter::sig_poly_site::SigPolySiteCache,
    pub(crate) sig_poly_record: crate::runtime::interpreter::sig_poly_site::SigPolyRecord,

    /// Thread-local allocation buffer for lock-free young-gen allocation.
    pub tlab: cratonvm_gc::Tlab,

    /// The `tlab.thread_allocated_bytes()` total at which the interpreter's
    /// `maybe_gc` next asks the heap-occupancy triggers; 0 = at the next call.
    /// Reset to 0 by a TLAB refill and by an allocation outside the TLAB. See
    /// `interpreter::gc_and_alloc::occupancy_poll_due`.
    pub gc_occupancy_poll_at: u64,

    /// Per-thread **shadow stack** of live object references for precise,
    /// rewritable GC roots inside JIT-compiled code (see
    /// `cratonvm_gc::shadow_stack`). JIT code pushes live oops onto it before
    /// a GC-capable call and reloads them after; the collector marks and
    /// rewrites the pushed slots precisely, which lets a *moving* young-gen
    /// collection run safely while JIT frames are live. Unallocated (empty)
    /// until the thread first enters JIT code with the mechanism enabled.
    pub shadow_stack: cratonvm_gc::shadow_stack::ShadowStack,

    /// The inline thin-lock's per-thread block (owner id + JMX lock stack),
    /// read by compiled `monitorenter`/`monitorexit` at
    /// [`Self::jit_monitor_block_offset`]. Unarmed until this thread's first
    /// helper-path `monitorenter`; see [`JitMonitorBlock`].
    pub(crate) jit_monitor_block: JitMonitorBlock,

    /// The compiled-recursion native-stack floor of the OS thread this thread
    /// last ran compiled code on (round 13 wave 4, lane mega3, proposal
    /// M13-3): written by `jit::helpers::set_jit_thread` whenever it publishes
    /// this thread, and read by compiled inline-cache stack checks through the
    /// `JIT_THREAD` mirror at `JitRuntimeHelpers::jit_stack_floor_offset_in_thread`
    /// (`CMP RSP, [thread + off]; JBE <helper>`). `0` never trips.
    pub(crate) jit_stack_floor: usize,

    /// Blocked-region GC state: shared with ThreadRegistry so a GC initiator
    /// can maintain this thread's roots while it is parked in a blocking
    /// native (`Object.wait` / `Thread.join` / `LockSupport.park` /
    /// `ReferenceQueue.remove`). See [`GcBlockState`].
    pub gc_block_state: Arc<GcBlockState>,

    /// GC roots for `Value::Object` arguments popped from the operand stack into a
    /// Rust `Vec` while a registered native runs (`safe_native_call`). Those refs
    /// are no longer on the Java stack until the callee returns, so without this
    /// list a safepoint GC can collect them mid-native (Letsgo AV after CCE
    /// `enhance` returns the original `Class`).
    pub native_pin_roots: Vec<ObjectRef>,

    /// The address of the innermost native callback this thread runs through
    /// the native-call funnel (`vm_exec::safe_native_call_impl`), recorded only
    /// while a debugger is attached, `0` for none (interpreter round i1 wave
    /// 17, `interpreter::enter_running_native`). Read by the JDWP
    /// blocked-thread reader to name an overridable native a virtual call
    /// selected (`interpreter::blocked_native_method`). Plain data, in
    /// neither per-thread root list.
    pub running_native: usize,
    /// The word this thread raised in its `GcBlockState::native_excuse` for
    /// the native it runs now (interpreter round i2 wave 58, lane L6,
    /// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE`), 0 when none: a plain copy, so the
    /// interpreter's entries for Java the native runs (`NestedJavaAck`) test
    /// it with `running_native` in one branch. Written only by this thread.
    pub native_excuse_raised: usize,

    /// Object result from the last `safe_native_call`: either an object return
    /// value before the interpreter pushes it onto the operand stack, or a
    /// native-thrown Java exception before the interpreter routes it through a
    /// catch handler. Covers the cross-thread GC window after the call returns.
    pub native_pending_return: Option<ObjectRef>,

    /// The JIT's out-of-band **pending Java exception**, awaiting the
    /// interpreter's post-JIT drain.
    ///
    /// A compiled method cannot return a throwable through the JIT ABI, so
    /// every helper that raises one (`jit_alloc_oom`, the class-init guards,
    /// `handle_jit_dispatch_error`, `jit_throw_exception`, the stack-overflow
    /// guards, …) stashes it here and returns the `i64::MIN` deopt sentinel;
    /// `take_all_jit_signals` / `take_jit_pending_exception` drain it and route
    /// it through the method's exception table. The *non-reference* siblings of
    /// this signal (`athrow_bci`, `aioobe`, `arithmetic`, `npe`, `npe_action`,
    /// `deopt`) stay in the `JIT_SIGNALS` thread-local; only this one is a heap
    /// reference, and that is why it lives here instead.
    ///
    /// **It is a `JvmThread` field for exactly one reason: GC reachability.**
    /// It used to be `JitSignals::exception`, a `Cell<Option<ObjectRef>>`
    /// inside a `thread_local!`. A collecting thread cannot reach another
    /// thread's TLS, and the `VM_ROOT_SOURCES` callbacks all run on the
    /// collector, so the stashed throwable was neither scanned nor remapped —
    /// an unrooted live reference across a window that includes a *guaranteed*
    /// collection in the `OutOfMemoryError` case (`jit_alloc_oom` stashes the
    /// OOME precisely because the heap is exhausted). As a `JvmThread` field it
    /// sits next to `java_thread_obj` in both halves of the root
    /// machinery: `memory/roots.rs` §10 pushes it, `memory/gc.rs` §10 rewrites
    /// it.
    ///
    /// A collection between the stash and the drain therefore keeps the
    /// throwable alive and hands the drain its post-move address.
    /// See `jit-signals-root-gap.md`.
    pub jit_pending_exception: Option<ObjectRef>,

    /// Unique thread identifier.
    pub thread_id: ThreadId,

    /// Whether this is a platform or virtual thread (JEP 444, Java 21).
    pub kind: ThreadKind,

    // ── End of the hot head. Cold and per-event state below. ───────────────
    /// Human-readable thread name.
    pub name: String,

    /// The heap collection count as of the last time SOMETHING remapped this
    /// thread's frames: the initiator's own `update_all_roots`, the
    /// safepoint-arrival `apply_pointer_map_to_thread`, or the blocked-region
    /// wake write-back.
    ///
    /// `CRATONVM_DBG_VACATED_FRAMES` prints it beside the heap's current count.
    /// The two disagreeing at a slot that still names a vacated address is the
    /// difference between "a heal path ran and missed this slot" and "no heal
    /// path ran for this thread at all", which want completely different fixes.
    pub last_heal_collection: u64,

    /// Pool of reusable locals (values, tags) pairs (avoids allocation on recursive calls).
    pub locals_pool: SoaPool,

    /// Pool of reusable stack (values, tags) pairs (avoids allocation on recursive calls).
    pub stacks_pool: SoaPool,

    /// Values printed by `tempPrint` — used by integration tests.
    pub printed: Vec<Value>,

    /// Lines printed via System.out.println — captured as plain Rust strings.
    pub printed_lines: Vec<String>,

    /// Whether this is a daemon thread.
    pub daemon: bool,

    /// What the open stand-in blocking region published for its span
    /// (interpreter round i2 wave 68, lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`;
    /// `NativeContext::begin_blocking_region_as_on`): one of the
    /// `REGION_WAIT_OBJECT_*` codes, so the region's end withdraws exactly
    /// that. Holds no reference: the published object lives in the registry's
    /// waiting-monitor slot or in the mirror's `parkBlocker` field.
    /// Declared beside `daemon` so it fills that `bool`'s padding: a field
    /// added further down grew `size_of JvmThread` a 64-byte step (the
    /// layout ratchet, `runtime/layout_ratchet_tests.rs`).
    pub region_wait_object: u8,

    /// Sticky: a JNI up-call on this thread failed with a VM error
    /// (`MethodCallFailed::InternalError`), so frames above it were dropped
    /// without a Java return or exception unwind and may have left a monitor
    /// owned. A platform thread's death then keeps the full monitor sweep
    /// (the platform twin of wave 98's `ended_structurally`; interpreter
    /// round i4 wave 99, lane L6). Beside `daemon` and `region_wait_object`,
    /// in their padding (the layout ratchet).
    pub vm_error_upcall_seen: bool,

    /// Thread interrupt flag (shared with ThreadRegistry for cross-thread access).
    pub interrupted: Arc<AtomicBool>,

    /// Cached Java Thread object on the heap for this thread.
    pub java_thread_obj: Option<ObjectRef>,

    /// `(Thread.holder slot, Thread$FieldHolder.threadStatus slot)` packed as
    /// `1 << 63 | holder << 32 | status` (0 = not resolved yet), resolved on
    /// the first block-state write of this thread (SBR round 2, lane `thread`;
    /// see `NativeContextImpl::publish_thread_status`). Class layout never
    /// changes after load, so the pair is cached for the thread's life. An
    /// atomic because the deposit that reads it takes `&self`.
    pub thread_status_slots: std::sync::atomic::AtomicU64,

    /// Parking permit for LockSupport.park()/unpark().
    pub park_state: Arc<ParkState>,

    /// Root snapshot: shared with ThreadRegistry for GC root scanning across threads.
    /// Updated at safepoints and before blocking operations.
    pub root_snapshot: Arc<parking_lot::Mutex<Vec<ObjectRef>>>,

    /// Frame trace snapshot: shared with ThreadRegistry so another thread can
    /// read this thread's Java call stack (cross-thread `Thread.getStackTrace()`
    /// / `dumpThreads()`). Published at the same blocking deposit points as
    /// `root_snapshot`, so for a parked thread it reflects where it is stuck.
    /// Line-less (see `stackwalker::capture_frames_no_lines`) to stay lock-free.
    pub frame_trace: Arc<parking_lot::Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,

    /// Optional diagnostic breadcrumb shared with the thread registry. When
    /// `CRATONVM_DBG_VM_STATE=1` is set, long-running VM helper paths publish a
    /// compact state here so STW census can identify non-bytecode stalls.
    pub vm_state: Arc<parking_lot::Mutex<String>>,

    /// Opt-in root-snapshot cache (`CRATONVM_ROOTSNAP_CACHE`): per *frozen*
    /// frame, `((frame.seq, frame.exec_epoch), that frame's scanned GC roots)`,
    /// indexed parallel to `frames[0..rs_cache.len()]`. Lets
    /// `update_root_snapshot` reuse the deep, continuously-frozen frames and
    /// re-scan only the churning top. Valid only while `rs_cache_gen ==
    /// heap.collection_count()` (a GC may have moved/promoted objects,
    /// invalidating the cached addresses). The key is `(seq, exec_epoch)` — NOT
    /// `seq` alone: `seq` proves the frame was never popped, but a still-present
    /// frame can RE-EXECUTE and reassign its locals; `exec_epoch` (bumped on
    /// callee-return and local-slot writes) detects that so stale roots are never
    /// reused (see `Frame::seq` / `Frame::exec_epoch`). Empty/unused when the
    /// gate is off.
    pub rs_cache: Vec<((u64, u64), Vec<ObjectRef>)>,
    /// GC collection count at which `rs_cache` was built (move/promote generation).
    pub rs_cache_gen: u64,

    /// gen r5w2/roots6: raises `gc_block_state`'s `owner_dropped` when this
    /// `JvmThread` is dropped. See [`GcBlockStateDropNotice`].
    pub gc_block_state_drop_notice: GcBlockStateDropNotice,
    /// This thread's own `ThreadEntry::jmx_locked_synchronizers` list, fetched
    /// on the first AQS ownership transition this thread performs.
    ///
    /// `AbstractOwnableSynchronizer.setExclusiveOwnerThread` is intercepted so
    /// `ThreadInfo.getLockedSynchronizers()` has something to report, and it
    /// runs twice per uncontended `ReentrantLock.lock()`/`unlock()` pair. Going
    /// through `ThreadRegistry::threads` to find this thread's own list put a
    /// registry-wide `RwLock` read and a `ThreadId` hash on that path; the
    /// handle removes both, exactly as [`GcBlockState`] above does for the
    /// blocked-region protocol.
    ///
    /// `None` until first use, and left `None` for a thread the registry does
    /// not know — which falls back to `set_jmx_owned_synchronizer`.
    pub jmx_locked_synchronizers: Option<Arc<parking_lot::Mutex<Vec<ObjectRef>>>>,

    /// The exception the interpreter's unwinder last posted a debugger
    /// `Exception` event for (JDWP or JVMTI; interpreter round i1 wave 11),
    /// as a JNI weak global handle, `0` for none. An exception reaches the
    /// unwinder again at every interpreter entry it unwinds through; this
    /// tells those from a new throw (HotSpot's
    /// `JvmtiThreadState::is_exception_detected`). Freed and cleared when an
    /// interpreter handler catches (`interpreter::note_exception_caught_if_armed`),
    /// replaced by the next reported exception. A weak handle, so it is in
    /// neither per-thread root list.
    pub reported_exception: u64,

    /// Unused objects reserved in one old-generation allocation batch for
    /// small native allocations. Keeping the pool on the owning thread makes
    /// it a normal GC root set; every GC snapshot/remap path treats these
    /// entries exactly like `native_pin_roots` until `alloc_object` hands one
    /// to a native callback.
    pub native_alloc_pool: Vec<ObjectRef>,
    /// `(class_id, slot_count)` shared by every entry in
    /// `native_alloc_pool`. `None` when the pool is empty.
    pub native_alloc_pool_layout: Option<(ClassId, usize)>,

    // ---- handle scope support (arch/handles) ----
    //
    // Slot storage backing `NativeContext::handle_root`/`handle_get` (see
    // `native_api::registry` and `cratonvm_types::handle`) — the GC-safe
    // sibling of `native_pin_roots` just above. A handle reads through its
    // slot on every access instead of caching an `ObjectRef` copy, so it
    // cannot go stale across an allocating call the way a raw pinned-and-
    // re-read local still can if a caller forgets the re-read step.
    //
    /// One entry per live handle slot. A `None` hole is a released
    /// (`handle_scope_pop`-truncated-past or otherwise unrooted) slot; root
    /// scanning and both cross-thread snapshot paths visit only `Some` entries;
    /// every ordinary and fallback pointer-map consumer rewrites those entries.
    /// Entries are appended by `handle_root` and released in bulk by
    /// `handle_scope_pop` truncating back to a recorded base — never
    /// reused mid-scope — mirroring `native_pin_roots`' own append/truncate
    /// discipline.
    pub handle_slots: Vec<Option<ObjectRef>>,
    /// Stack of `handle_slots` lengths recorded by each `handle_scope_push`,
    /// popped (and used to truncate `handle_slots`) by the matching
    /// `handle_scope_pop`. Tracking the base on the thread itself (rather
    /// than making every caller thread a base index through, as
    /// `pin_native_root`'s callers must) is what lets handle scopes nest
    /// across native call frames with a single push/pop pair each.
    pub handle_scope_bases: Vec<usize>,

    /// The class declaring the JNI native this thread is running, set and
    /// restored by `vm_exec.rs`'s `JniContextGuard::install_for_native`.
    /// JNI `FindClass` resolves through its loader, as HotSpot's
    /// `jni_FindClass` does (the native's own holder is the caller class).
    pub jni_native_class: Option<ClassId>,

    /// The JNI natives this thread is running, outermost first, as
    /// `(interpreter depth at the call, declaring class, key)` (the key: the JNI
    /// function pointer called, or a method-event post's tagged hash;
    /// interpreter round i1 wave 42, lane L1): pushed and popped around each
    /// JNI native call by `jvmti::native_env::NativeFrameRow` (natives nest
    /// through upcalls), read by the JVMTI stack functions
    /// (`jvmti::native_env::frames_of`), which list a native method's frame
    /// at location -1 as HotSpot does. Plain data, in neither per-thread root
    /// list. The fourth member is the thread's JIT entry-chain length at the
    /// call (`jit::conservative_roots::current_thread_jit_depth`; wave 44,
    /// lane L3): a compiled method the native's own upcall entered is listed
    /// above it (`stackwalker::capture_trace_with_anchor_positions`).
    pub jni_native_frames: Vec<(usize, ClassId, u64, u32)>,

    /// The reflective calls (`Method.invoke`, `Constructor.newInstance`) this
    /// thread is inside, outermost first: pushed and truncated around each by
    /// the natives that serve them (`NativeExceptionAccess::
    /// enter_reflective_call`), read by the stack captures, which list the
    /// JDK frames HotSpot shows for such a call
    /// (`stackwalker::reflective_splices`; interpreter round i1 wave 43, lane
    /// L3). Plain data, in neither per-thread root list.
    pub reflective_calls: Vec<crate::runtime::stackwalker::ReflectiveCallRow>,

    /// Bitmask of the `ReflectiveCallKind`s whose accessor class this thread
    /// has already asked the class manager to load (bit 0 `MethodInvoke`,
    /// bit 1 `ConstructorNewInstance`): a reflective call's JDK frames name
    /// it, so it must be loaded before a capture can list them.
    pub reflective_accessors_loaded: u8,

    /// The JDK frames of each reflective-call kind as this thread's captures
    /// last named them (`stackwalker::ReflectiveFramesNamed`), reused while no
    /// class is redefined.
    pub reflective_frames_named: crate::runtime::stackwalker::ReflectiveFramesNamed,

    /// Bounded JIT cache for immutable String keys in exact HashMaps. Entries
    /// are cleared lazily on a structural change, and wholesale when
    /// [`Self::jit_memo_epoch`] goes stale.
    pub jit_hashmap_string_node_cache: Vec<JitHashMapStringNodeCacheEntry>,
    pub string_case_cache: Vec<StringCaseCacheEntry>,
    /// The GC-pause generation (`GcBarrier::gc_generation`) the two JIT memo
    /// caches above were last validated at. A probe or put, and the safepoint
    /// publish, at a later generation clear both first — so a cache the thread
    /// has not touched since the last pause stops keeping a dropped `HashMap`
    /// or `String` alive, and no entry is ever served across a pause. Present
    /// entries are always roots. See `memory::roots::validate_jit_memo_caches`.
    pub jit_memo_epoch: u64,

    /// This thread's `ThreadLocal` values while it is NOT mounted on an OS
    /// thread: a virtual thread between continuation slices. The intrinsics
    /// keep the RUNNING thread's map in an OS-thread `thread_local!`;
    /// `vm_exec::resume_virtual_continuation` swaps this state in at every
    /// mount and back out at every unmount, so a virtual thread's values
    /// neither leak to the next virtual thread on the carrier nor get lost when
    /// it migrates. Always empty for a platform thread. Object values are JNI
    /// global roots already, so this needs no scan.
    pub detached_thread_locals: cratonvm_native_builtins::phases_early::DetachedThreadLocals,

    /// `cratonvm_jit::jit_withdrawn_bodies()` when this thread last released
    /// the withdrawn bodies its dispatch caches pin. See
    /// `jit_bridge::release_withdrawn_code_owners`.
    pub jit_withdrawals_seen: u64,

    /// `cratonvm_classloading::class_redefinition_count()` when this thread
    /// last moved its frames that run a replaced method body onto the
    /// translated one (`interpreter::obsolete_frames`, interpreter round i1
    /// wave 19, lane L3). One load and a compare at every safepoint and
    /// blocking-region exit until a class is redefined.
    pub redefinitions_seen: u64,

    /// `Some(withdrawn)` while this thread runs a batch of redefinitions
    /// (`NativeContext::begin_redefinition_batch`, one
    /// `Instrumentation.retransformClasses` / `redefineClasses` call):
    /// `JitCache::bodies_withdrawn_by_redefinition` when it opened. Each
    /// redefinition in it defers its loop-exit handshake, and the batch's end
    /// takes one if any body was withdrawn since (interpreter round i1 wave
    /// 25, lane L6).
    pub(crate) redefinition_batch: Option<u64>,

    /// Round 14 wave 6 (lane trace5; TR5-2): this thread's last bottom
    /// `Thread.run` decision (`vm_exec::prepend_thread_run_standin_frame`),
    /// so a throw on an executor worker skips the class-manager lock. `None`
    /// until the first such capture.
    pub(crate) thread_run_bottom_memo: Option<crate::runtime::stackwalker::ThreadRunBottomMemo>,

    /// JIT round 16 wave 7 (lane throwpath, TP16-6): this thread's recent
    /// throwable captures, reused by the next one made on the same stack
    /// (`vm_exec::capture_throwable_stack_trace`; default on since
    /// 2026-10-07, `CRATONVM_THROWABLE_CAPTURE_MEMO=0` off). One pointer
    /// inline, boxed on first use. Holds no heap reference.
    pub(crate) throwable_capture_memo: crate::runtime::stackwalker::ThrowableCaptureMemos,

    /// JIT round 16 wave 11 (lane throw2, TP16-10): this thread's recent
    /// compiled throw sites whose innermost frame gets no stand-in frames
    /// (`vm_exec::append_native_standin_frames`,
    /// `CRATONVM_THROWABLE_STANDIN_SITE_MEMO=1`). Holds no heap reference.
    pub(crate) throwable_standin_site_memo: crate::runtime::stackwalker::StandinSiteMemo,

    /// JIT round 16 wave 11 (lane throw2, TP16-12): this thread's recent
    /// fill-frame trim `is_a` answers (`vm_exec::capture_throwable_stack_trace`,
    /// `CRATONVM_THROWABLE_TRIM_IS_A_MEMO=1`). Class ids only.
    pub(crate) throwable_is_a_memo: crate::runtime::stackwalker::ThrowableIsAMemo,

    /// Round 14 wave 6 (lane trace5; TR5-3): `Some(timed)` from a served
    /// `Thread.join(long)` about to raise `InterruptedException`
    /// (`NativeThreadAccess::set_served_timed_join_hint`); taken by the next
    /// throwable capture on this thread.
    pub(crate) served_timed_join_hint: Option<bool>,

    /// Round 15 wave 2 (lane trace2): the class of the built-in loader whose
    /// served `loadClass(String)` is about to raise `ClassNotFoundException`
    /// (`NativeThreadAccess::set_served_loader_miss_hint`); taken by the next
    /// throwable capture on this thread.
    pub(crate) served_loader_miss_hint: Option<ClassId>,
    /// nio21 wave 18 L2 (`CRATONVM_BY_NAME_OVERRIDE_MEMO`): this thread's
    /// memo of the by-name native-override chain
    /// (`vm_exec::name_override_memoized`). A field rather than a
    /// `thread_local!`, so it adds no process static.
    pub(crate) name_override_memo: crate::vm::vm_exec::NameOverrideMemo,

    /// Interpreter round i2 wave 53, lane L3: the innermost interpreted frame
    /// a native returned an unbuilt `RuntimeError` under, and an implicit
    /// NPE's action (`vm_exec::note_served_native_raise`, the funnel's mark);
    /// taken by the next throwable capture on this thread, which believes it
    /// only while that frame still stands there
    /// (`stackwalker::served_raise_frames`).
    ///
    /// Boxed (interpreter round i2 wave 55, lane L7): the mark is about 100
    /// bytes and written only on a native's error path, and inline it moved
    /// every later field of this struct, which the dispatch loop reads on
    /// every call and type check
    /// (`docs/internal/fixed-bugs/interpreter-orch-wave-53-door-and-loop-rows-are-slower-under-nojit-FIXED-20261007.md`).
    pub(crate) served_native_raise: Option<Box<crate::runtime::stackwalker::ServedNativeRaise>>,

    /// Interpreter round i2 wave 54, lane L3: the JDK chain a native noted
    /// for the raise it is about to return
    /// (`NativeExceptionAccess::note_served_raise_chain`); the funnel's next
    /// mark takes it (`vm_exec::note_served_native_raise`), and drops it when
    /// the raise is of another class.
    pub(crate) served_raise_chain: Option<Box<cratonvm_native_api::ServedRaiseChain>>,

    /// JIT round 16 wave 13 (lane wmte): the JDK method a `MethodHandle`
    /// door's `WrongMethodTypeException`, about to be built, stands for
    /// (`NativeThreadAccess::set_mh_wmte_raise_hint`); taken by the next
    /// capture of a `WrongMethodTypeException` on this thread.
    pub(crate) mh_wmte_raise_hint: Option<cratonvm_native_api::MhWmteRaise>,

    /// Round 15 wave 3 (lane trace4; TR2W2-2): the served `Thread.run` calls
    /// this thread is inside (`NativeExceptionAccess::enter_served_thread_run`),
    /// read by its stack captures (`stackwalker::ServedThreadRunCalls`).
    pub(crate) served_thread_run: crate::runtime::stackwalker::ServedThreadRunCalls,
    /// Interpreter round i1 wave 48, lane L1: `Some(during)` from a
    /// virtual-thread sleep about to raise `InterruptedException` in the shape
    /// of HotSpot's default virtual threads (`VmConfig::virtual_sleep_continuation_shape`):
    /// `false` = interrupted before it slept (`VirtualThread.sleepNanos`'s first
    /// `new InterruptedException()`), `true` = during or after its park (the
    /// second). Taken by the next throwable capture on this thread
    /// (`stackwalker::virtual_sleep_continuation_frames`).
    pub(crate) virtual_sleep_hint: Option<bool>,
    // The code those frames ran before they were moved is kept in each frame
    // (`ReplacedBody::retired_code`), not here: interpreter round i1 wave 22,
    // lane L3 retired `retired_obsolete_code`, which kept it until the thread
    // ended.
    /// Thread-local cache for the vtable-fast native-shadow guard.
    ///
    /// On invoke-cache misses, `execute_invokevirtual_vtable_fast` checks whether
    /// the receiver class or one of its native-shadowed ancestors should cede
    /// dispatch to the native/intrinsic path. HQL/lambda-heavy workloads can miss
    /// the invoke cache repeatedly, and the old guard paid a native-registry hash
    /// lookup for every ancestor on every miss.
    ///
    /// Keyed by `(receiver_class_id, hierarchy_redefine_fingerprint,
    /// method_name_hash, descriptor_hash)`. A redefine changes the
    /// fingerprint of the affected class hierarchy, so those entries
    /// self-invalidate without disabling this cache process-wide.
    pub native_shadow_cache: FxHashMap<(u32, u64, u64, u64), bool>,

    /// Pin depth for virtual threads (JEP 491).
    ///
    /// Each monitor enter increments this; each monitor exit decrements.
    /// When non-zero on a virtual thread, the thread is "pinned" to its
    /// carrier — attempting to park/sleep will keep the carrier blocked
    /// and emit a `jdk.VirtualThreadPinned` JFR event.
    pub pin_count: u32,

    /// Monitors a VM native entered on this PLATFORM thread through
    /// `NativeContext::monitor_enter` / `monitor_enter_gc_safe` and has not
    /// exited through `NativeContext::monitor_exit` (saturating at 0): the
    /// native half of HotSpot's `_jni_monitor_count` for a platform thread's
    /// death (`CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`; interpreter round i4
    /// wave 99, lane L6; page `i94-L6-thread-death-sweeps-every-inflated-monitor`).
    /// Kept here, not in the registry entry
    /// (`ThreadEntry::unstructured_monitor_enters`, which JNI `MonitorEnter`
    /// and a virtual thread's native enters raise), because natives like the
    /// `ConcurrentHashMap.tabAt` stand-ins enter a monitor per call and a
    /// registry lookup each would cost every one of them; a platform thread's
    /// death has its own `JvmThread` in hand and reads both. Declared beside
    /// `pin_count`, in the padding before `pin_reason` (the struct is
    /// `repr(C)`; `runtime/layout_ratchet_tests.rs` freezes its size).
    pub native_monitor_enters: u32,

    /// Last pin reason (for JFR event classification).
    /// "Monitor", "NativeMethod", "ClassInit", or empty when not pinned.
    pub pin_reason: &'static str,

    /// Why this virtual thread's continuation last yielded, written by the
    /// yielding native's `NativeContext` call (`vt_park_for`,
    /// `vt_sleep_for`, `vt_wait_on_key`) and taken by the remount
    /// (`interpreter::resume_continuation`). See [`VtYield`].
    pub vt_yield: VtYield,

    /// When this JDK continuation's thread last started running: its mount
    /// (`jdk_continuation::run_slice`) or the return of its last pinned park
    /// (`NativeContextImpl::park_with_state_until`). Only under
    /// `CRATONVM_VTHREAD_COMPILED_LEARNED` (interpreter round i5 wave 110, lane
    /// L6): `learn_compiled_pin` leaves the deny set alone for a pin after a
    /// long run. `None` everywhere else.
    pub vt_slice_started: Option<std::time::Instant>,

    /// Return conversions a remounted continuation still owes, one per impl
    /// frame a lambda dispatch entered through a nested Rust call (wave 25,
    /// lane L7). Empty on every platform thread and on almost every virtual
    /// one; the interpreter's return arms test `is_empty()` only. See
    /// [`ContinuationReturnAdapter`].
    pub continuation_return_adapters: Vec<ContinuationReturnAdapter>,

    /// Whether this thread's JMX book holds a frame-monitor publication
    /// (`ThreadRegistry::publish_jmx_frame_monitors`), so a deposit with no
    /// monitor held skips the registry entirely (wave 24, lane L7).
    /// Owner-only; atomic only so the `&self` deposit path can write it.
    pub jmx_frame_monitors_published: AtomicBool,

    /// Scoped value binding stack (JEP 446, Java 25).
    /// Each entry is (key_id, key_ref, bound_value). Searched top-to-bottom.
    ///
    /// Round-9 GC fix: `key_ref` is the `ScopedValue` ObjectRef that owns
    /// `key_id`. When set, the GC root scanner reports it so the key
    /// object cannot be collected while a binding for it is live (only
    /// the value used to be retained; the key itself could be reclaimed
    /// while user code still observed the binding via
    /// `ScopedValue.isBound()` / `Carrier.get`).  `None` for legacy
    /// callers that pre-date the with-key API — those paths fall back to
    /// the value-only behaviour and are no worse than before.
    pub scoped_values: Vec<(u64, Option<ObjectRef>, Value)>,

    /// The class ids of the `checkcast` failure being raised on this thread:
    /// the receiver's class and the resolved target's, for the
    /// `ClassCastException` message funnel, which otherwise resolves each
    /// operand by NAME and cannot tell two loaders' same-named classes apart
    /// (interpreter round i1 wave 32, `exceptions::note_cast_operands`). The
    /// two ids packed high/low, `u64::MAX` when empty; atomic because the
    /// funnel holds the thread shared, and it takes the note.
    pub cast_operands_note: std::sync::atomic::AtomicU64,

    /// The last cast refusal message the `ClassCastException` funnel rebuilt
    /// on this thread (round 15 wave 7, lane throw2, TH1-2;
    /// `exceptions::CastMessageMemo`). `None` until a refusal with noted
    /// class ids is rebuilt.
    pub(crate) cast_message_memo: Option<Box<crate::runtime::exceptions::CastMessageMemo>>,

    /// The VM-initiated `loadClass` drives in flight on this thread,
    /// innermost last: `(referencing class id, internal name, referencing
    /// loader's native id or u32::MAX)`. Read by
    /// `interpreter::constants::drive_defining_loader_load_named` for its
    /// re-entry guard, its `ClassCircularityError` and its parallel-capable
    /// re-ask nesting. On the thread, not in a `thread_local!`, so the
    /// rows are this VM's by construction: two VMs driven on one OS thread
    /// (an embedder, or a native of one VM calling into another) no longer
    /// see each other's class ids (interpreter round i1 wave 40, lane L5).
    pub loader_drives_in_flight: Vec<(u32, String, u32)>,

    /// gcf f4/o: non-zero while this thread builds the throwable of a
    /// VM-raised `OutOfMemoryError` (`exceptions::create_vm_oome_object`);
    /// under `CRATONVM_GC_OOME_DEBT_SKIP_CONSTRUCTION` the native funnel does
    /// not pay the post-OOME debt from inside that construction
    /// (`gc_and_alloc::native_call_owes_oome_major`).
    pub oome_construct_depth: u32,

    /// Extra compiled self-recursion budget (bytes) of a thread whose carrier
    /// `thread_start` grew for a `stackSize` / `-Xss` request: the growth over
    /// the default carrier. 0 for every other thread. Read by
    /// `jit::helpers::compute_self_call_stack_floor` (gcd d6/f item 2).
    pub(crate) self_call_extra_budget: usize,

    /// gce e2/t: `CRATONVM_DBG_XT_FORCE_TAKEOVER`, read the first time this
    /// thread's compiled poll finds a pause (never on a thread that does not),
    /// plus the pause it is declining its compiled polls for. See
    /// `jit::xt_root_scan::ForceTakeover`.
    pub(crate) xt_force_takeover: crate::jit::xt_root_scan::ForceTakeover,

    /// The uncaught throwable the launcher is about to render, parked here for
    /// the duration of SHUTDOWN HOOK execution.
    ///
    /// The launcher gets an `ObjectRef` out of `MethodCallFailed::
    /// ExceptionThrown`, then runs shutdown hooks BEFORE rendering it (HotSpot
    /// runs hooks on the uncaught path too). A hook is arbitrary Java: it
    /// allocates, and it can collect. Held only in a Rust local across that
    /// call the throwable is reachable from nothing the collector can see, so
    /// a collection during the hooks reclaims it — and the render then reads a
    /// zeroed header, which decodes as `ClassId(0)`, which IS
    /// `java.lang.Object`.
    ///
    /// That is `bug-h2-testopenclose-throwable-is-java-lang-object-20260829.md`:
    /// `Exception in thread "main" java/lang/Object` with no message and no
    /// frames, because the whole object is gone. H2's own
    /// `OnExitDatabaseCloser` hook closes a database from the hook, which is
    /// as much allocation as the window needs.
    ///
    /// Same two halves as `jit_pending_exception` above — `memory/roots.rs`
    /// §10 pushes it, `memory/gc.rs` rewrites it — so the throwable survives
    /// the hooks AND the launcher gets its post-move address.
    pub uncaught_exception_pending: Option<ObjectRef>,

    /// T17.Δ.5 — per-thread JVMTI frame-pop requests.
    ///
    /// Each entry is a frame depth (index into `frames`) for which
    /// `NotifyFramePop` was called.  When the interpreter is about to drop
    /// a frame it consults this list; if the frame's depth matches any
    /// registered request, `FramePop` fires and the entry is removed.
    ///
    /// Implemented as a small `Vec<u32>` because in practice only a handful
    /// of frames are marked at any given time (debugger "step out" uses
    /// one; profilers typically use zero).  Linear scan cost is negligible
    /// next to the frame-pop work.
    pub frame_pop_requests: Vec<u32>,

    /// The exact `VarHandle` stand-in site memo (interpreter round i1 wave
    /// 50, lane L4; `CRATONVM_VH_EXACT_SITE_MEMO`): the call-site descriptor
    /// recovered for a thin `VarHandle` helper's compiled site, keyed on the
    /// innermost compiled activation's compile id, safepoint id, method and
    /// the stand-in site (`jit::helpers::varhandle_stand_in_site_descriptor`).
    /// A few entries, most recent first; empty unless an exact handle reached
    /// a thin helper site.
    pub vh_exact_site_memo: Vec<VhExactSiteMemo>,
    /// gcf f2/j: bumped by every root deposit of this thread
    /// (`NativeContextImpl::deposit_root_snapshot_inner`, flag-raising or
    /// not), always on the owning thread. The JNI in-native repeat
    /// (`native::jni::InNativeRepeat`) reuses its previous deposit's
    /// interpreter half only while no other deposit ran in between: a
    /// blocking one (an `Object.wait`, a debugger suspension, whose wake may
    /// apply deferred frame writes) replaces the published trace and may
    /// change the frames without a pause. Atomic only because the deposit
    /// reads the thread through `&`; `Relaxed` (one thread writes and reads).
    pub(crate) root_publish_seq: std::sync::atomic::AtomicU64,
    /// Interpreter round i2 wave 63, lane L3 (`VmConfig::served_raise_door_key`):
    /// the static JDK method the innermost by-name door is entering, set
    /// right before the door's call and restored to the outer door's record
    /// right after (`stackwalker::DoorInFlight`); read, never taken, by this
    /// thread's throwable captures. LAST on purpose: it sits in the tail
    /// padding of the 64-byte step, so no field moves
    /// (`runtime::layout_ratchet_tests`). `None` unless the switch is on.
    pub(crate) served_door_in_flight: Option<crate::runtime::stackwalker::DoorInFlight>,
    /// Interpreter round i2 wave 64, lane L3 (`VmConfig::served_raise_door_memo`):
    /// the methods this thread's static JDK doors selected for their
    /// [`Self::served_door_in_flight`] notes (`stackwalker::DoorKeyMemo`),
    /// allocated by the first noted door with the switch on. After the record,
    /// in the same tail padding (an 8-byte word at 3056 in the 3072-byte
    /// struct): no field moves (`runtime::layout_ratchet_tests`).
    pub(crate) served_door_key_memo: Option<Box<crate::runtime::stackwalker::DoorKeyMemo>>,
    /// The i54 retirements (2026-10-07): the two fields below, cold, past
    /// every field `runtime::layout_ratchet_tests` names, so none moves; the
    /// struct grows one 64-byte step (3072 to 3136).
    /// The interpreter depth of a synchronized method's frame whose JVMTI
    /// `MethodEntry` waits for its contended monitor, 0 for none (the i54-L6
    /// retirement): `vm_exec::monitor_enter_or_defer_frame_first` sets it
    /// when a C agent listens for `MethodEntry`, the push leaves the event
    /// out (`interpreter::fire_method_entry_after_push`), and
    /// `vm_exec::enter_synchronized_method_in_pushed_frame` posts it once the
    /// monitor is held, as HotSpot posts it after `lock_method`. Plain data.
    pub deferred_method_entry_depth: usize,

    /// The locals of the lambda proxy method frames this thread is in,
    /// outermost first (the i54-L1 retirement): one per proxy row of
    /// [`Self::jni_native_frames`] that has one, which names it by index.
    /// Only while a C JVMTI agent is bound. Its references are pinned in
    /// [`Self::native_pin_roots`].
    pub lambda_proxy_locals: Vec<LambdaProxyLocals>,

    /// The JDK-continuation role of this thread (`vm::jdk_continuation`):
    /// whether it runs a JDK-driven continuation, and the carrier answering
    /// `Thread.currentCarrierThread()`. See [`JdkContinuationState`]. Cold,
    /// last, one word in the tail padding: no field moves
    /// (`runtime::layout_ratchet_tests`).
    pub jdk_continuation: JdkContinuationState,

    /// gci i13/oj (oomjit10 item 1, opt-in `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE`):
    /// the promising interpreter door's state -- the switch, read once per
    /// thread, and the record of the argument words the door in flight
    /// handed over (`jit_bridge::publish_door_promise` writes it, for the three weak-args doors,
    /// `conservative_roots::door_promise_claim` reads it). Cold, last, one
    /// word (`None` until the weak-args door first runs on this thread): no
    /// field moves (`runtime::layout_ratchet_tests`).
    pub(crate) door_promise: Option<Box<DoorPromiseState>>,
}

/// How many argument words a promising door publishes (gci i13/oj); a door
/// with more reference arguments publishes the first ones and keeps the rest.
pub(crate) const DOOR_HANDED_ARG_WORDS: usize = 8;

/// The record a promising interpreter door publishes for the length of its
/// compiled call (gci i13/oj, [`JvmThread::door_promise`]). Plain data, copied
/// in and out: the GC side reads it through the installed thread pointer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DoorHandedArgWords {
    /// A door is in flight and this record is its own.
    pub(crate) active: bool,
    /// The index the door's `JitEntryGuard` is pushed at in this thread's
    /// JIT entry chain: only the walk bounded by that entry's `entry_sp` is
    /// the door's segment.
    pub(crate) chain_depth: usize,
    /// How many of `words` are set.
    pub(crate) len: usize,
    /// The reference arguments the door handed over, as their address bits.
    pub(crate) words: [usize; DOOR_HANDED_ARG_WORDS],
}

impl DoorHandedArgWords {
    /// Is `word` one of the handed-over argument words?
    pub(crate) fn handed(&self, word: usize) -> bool {
        word != 0 && self.words[..self.len.min(DOOR_HANDED_ARG_WORDS)].contains(&word)
    }
}

/// [`JvmThread::door_promise`]'s box (gci i13/oj).
#[derive(Debug)]
pub(crate) struct DoorPromiseState {
    /// `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE`, read when the box was made.
    pub(crate) enabled: bool,
    /// The record of the innermost promising door in flight (inactive when
    /// none is).
    pub(crate) record: DoorHandedArgWords,
}

/// One entry of [`JvmThread::vh_exact_site_memo`].
pub struct VhExactSiteMemo {
    /// `(compile id, safepoint id, CompiledMethod address, stand-in site
    /// `JitInvokeInfo` address)`.
    pub key: (u32, u32, usize, usize),
    /// The compiled method's label, compared on a hit (a recycled compile id
    /// and address naming another method is refused).
    pub label: Box<str>,
    /// The recovered call-site descriptor.
    pub descriptor: Box<str>,
}

/// The state a [`JvmThread`] shares with its registry entry, by `Arc`
/// (interpreter round i3 wave 82, lane L6; `CRATONVM_THREAD_START_ONE_PUBLISH`):
/// what [`JvmThread::with_shared`] adopts and
/// `ThreadRegistry::starting_thread_shared` hands out in one read.
pub(crate) struct ThreadSharedArcs {
    pub(crate) park_state: Arc<ParkState>,
    pub(crate) interrupted: Arc<AtomicBool>,
    pub(crate) root_snapshot: Arc<parking_lot::Mutex<Vec<ObjectRef>>>,
    pub(crate) frame_trace: Arc<parking_lot::Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,
    pub(crate) vm_state: Arc<parking_lot::Mutex<String>>,
    pub(crate) gc_block_state: Arc<GcBlockState>,
}

impl ThreadSharedArcs {
    /// Fresh, unshared state: what [`JvmThread::new`] has always built.
    pub(crate) fn fresh() -> Self {
        Self {
            park_state: Arc::new(ParkState::new()),
            interrupted: Arc::new(AtomicBool::new(false)),
            root_snapshot: Arc::new(parking_lot::Mutex::new(Vec::new())),
            frame_trace: Arc::new(parking_lot::Mutex::new(Vec::new())),
            vm_state: Arc::new(parking_lot::Mutex::new(String::new())),
            gc_block_state: Arc::new(GcBlockState::new()),
        }
    }
}

impl JvmThread {
    /// Byte offset of the `tlab` field from the start of `JvmThread`.
    ///
    /// Used by the JIT (`jit/src/x64.rs` `new` opcode) to emit an inline
    /// TLAB bump-pointer fast path. The JIT loads the per-thread TLAB
    /// cursor/end at `JvmThread base + TLAB_OFFSET + Tlab::CURSOR/END_OFFSET`.
    ///
    /// **MSRV note**: this crate targets Rust 1.75. `core::mem::offset_of!`
    /// is `const` only on 1.77+, so we compute the offset lazily on first
    /// access via a sentinel `JvmThread::default()` and cache it in a
    /// [`OnceLock`]. The computation is a single subtraction of two
    /// `&raw const` pointers — no allocation beyond the one-time default
    /// thread.
    ///
    /// Callers should treat the returned value as effectively `const` and
    /// cache it themselves (e.g. into a `JitRuntimeHelpers` field at
    /// helper-table init).
    pub fn tlab_offset() -> usize {
        use std::sync::OnceLock;
        static OFFSET: OnceLock<usize> = OnceLock::new();
        *OFFSET.get_or_init(|| {
            let t = JvmThread::default();
            let base = &t as *const JvmThread as usize;
            let tlab_addr = &t.tlab as *const cratonvm_gc::Tlab as usize;
            tlab_addr - base
        })
    }

    /// Byte offset of the `shadow_stack` field from the start of `JvmThread`.
    ///
    /// Used by the JIT (`jit/src/x64.rs`) to emit the inline shadow-stack push
    /// at GC-capable safepoints: the codegen loads/stores the shadow `top` at
    /// `JvmThread base + shadow_stack_offset() + ShadowStack::TOP_OFFSET`.
    /// Computed lazily like [`Self::tlab_offset`] (MSRV: no const `offset_of!`).
    pub fn shadow_stack_offset() -> usize {
        use std::sync::OnceLock;
        static OFFSET: OnceLock<usize> = OnceLock::new();
        *OFFSET.get_or_init(|| {
            let t = JvmThread::default();
            let base = &t as *const JvmThread as usize;
            let ss_addr = &t.shadow_stack as *const cratonvm_gc::shadow_stack::ShadowStack as usize;
            ss_addr - base
        })
    }

    /// Byte offset of the [`JitMonitorBlock`] from the start of `JvmThread`,
    /// published to the JIT as `JitRuntimeHelpers::monitor_block_offset_in_thread`.
    /// A const `offset_of!`, unlike the two lazily-probed offsets above: the
    /// workspace MSRV (1.88) has had it since 1.77.
    pub(crate) const fn jit_monitor_block_offset() -> usize {
        std::mem::offset_of!(JvmThread, jit_monitor_block)
    }

    /// Whether VM-state breadcrumbs should be published.
    #[inline]
    pub fn vm_state_diagnostics_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VM_STATE").is_some()
        })
    }

    /// gcg g3/k (`common-a-proposal-per-thread-handshakes`, step 1): run every
    /// handshake operation other threads queued for this thread, on this
    /// thread, with the queue lock released. One `Acquire` load when nothing
    /// is queued. For a safepoint poll of the owning thread (the hunk for
    /// `gc_and_alloc::safepoint_check` is in `g3-k-report.md`); not for a
    /// thread in a blocked region (its requester runs the blocked path).
    ///
    /// gci i23/hs: polled by `gc_and_alloc::safepoint_check` (step 1), with
    /// the VM it polls for. An op that unwinds settles its own ticket and
    /// every later one `Abandoned` on the way out (`SettleOnUnwind`), so no
    /// requester waits for an op that will never run.
    #[inline]
    pub fn run_pending_handshakes(&mut self, shared: &crate::vm::SharedVm) {
        if !self.gc_block_state.handshakes.is_pending() {
            return;
        }
        let state = Arc::clone(&self.gc_block_state);
        let ops: Vec<(HandshakeOp, SettleOnUnwind)> = state
            .handshakes
            .take_all()
            .into_iter()
            .map(|(op, ticket)| (op, SettleOnUnwind(ticket)))
            .collect();
        for (op, guard) in ops {
            op(shared, self);
            guard.0.settle(HandshakeState::Ran);
        }
    }

    /// Publish a short diagnostic state for STW census. No-op unless enabled.
    #[inline]
    pub fn set_vm_state<S: Into<String>>(&self, state: S) {
        if Self::vm_state_diagnostics_enabled() {
            *self.vm_state.lock() = state.into();
        }
    }

    /// Create a new thread with the given id and name.
    pub fn new(thread_id: ThreadId, name: &str) -> Self {
        Self::with_shared(thread_id, name, ThreadSharedArcs::fresh())
    }

    /// A thread whose registry-shared state is `shared` -- the `Arc`s its
    /// registry entry already holds (`CRATONVM_THREAD_START_ONE_PUBLISH`,
    /// interpreter round i3 wave 82, lane L6;
    /// `ThreadRegistry::starting_thread_shared`), so the starting thread need
    /// not re-install them with one `threads` write lock each. [`Self::new`]
    /// passes fresh ones. No new field: the `Arc`s move into the existing
    /// ones (`JvmThread`'s size is ratcheted).
    pub(crate) fn with_shared(thread_id: ThreadId, name: &str, shared: ThreadSharedArcs) -> Self {
        let ThreadSharedArcs {
            park_state,
            interrupted,
            root_snapshot,
            frame_trace,
            vm_state,
            gc_block_state,
        } = shared;
        Self {
            gc_block_state_drop_notice: GcBlockStateDropNotice::new(&gc_block_state),
            thread_id,
            name: name.to_string(),
            last_heal_collection: 0,
            frames: FrameStack::new(),
            locals_pool: Vec::new(),
            stacks_pool: Vec::new(),
            printed: Vec::new(),
            printed_lines: Vec::new(),
            daemon: false,
            interrupted,
            java_thread_obj: None,
            thread_status_slots: std::sync::atomic::AtomicU64::new(0),
            park_state,
            root_snapshot,
            frame_trace,
            vm_state,
            rs_cache: Vec::new(),
            rs_cache_gen: 0,
            gc_block_state,
            jmx_locked_synchronizers: None,
            region_wait_object: REGION_WAIT_OBJECT_NONE,
            native_pin_roots: Vec::new(),
            reported_exception: 0,
            running_native: 0,
            native_excuse_raised: 0,
            native_alloc_pool: Vec::new(),
            native_alloc_pool_layout: None,
            handle_slots: Vec::new(),
            handle_scope_bases: Vec::new(),
            native_pending_return: None,
            jni_native_class: None,
            jni_native_frames: Vec::new(),
            deferred_method_entry_depth: 0,
            lambda_proxy_locals: Vec::new(),
            reflective_calls: Vec::new(),
            reflective_accessors_loaded: 0,
            reflective_frames_named: Default::default(),
            jit_hashmap_string_node_cache: Vec::new(),
            string_case_cache: Vec::new(),
            jit_memo_epoch: 0,
            detached_thread_locals: Default::default(),
            invoke_cache: InvokeCache::new(),
            sig_poly_sites: Default::default(),
            sig_poly_record: Default::default(),
            jit_withdrawals_seen: 0,
            // Frames this thread builds from now on index the current pools.
            redefinitions_seen: cratonvm_classloading::class_redefinition_count(),
            redefinition_batch: None,
            thread_run_bottom_memo: None,
            throwable_capture_memo: Default::default(),
            throwable_standin_site_memo: Default::default(),
            throwable_is_a_memo: Default::default(),
            served_timed_join_hint: None,
            served_loader_miss_hint: None,
            name_override_memo: crate::vm::vm_exec::NameOverrideMemo::default(),
            served_native_raise: None,
            served_raise_chain: None,
            mh_wmte_raise_hint: None,
            served_thread_run: Default::default(),
            virtual_sleep_hint: None,
            field_sites: crate::runtime::interpreter::FieldSiteCache::new(),
            site_tables: crate::runtime::interpreter::InterpSiteTables::new(),
            trivial_getter_verdicts: crate::runtime::interpreter::site_cache::SiteCache::new(),
            method_sites: crate::runtime::interpreter::MethodSiteCache::new(),
            class_sites: crate::runtime::interpreter::ClassSiteCache::new(),
            cast_sites: crate::runtime::interpreter::CastSiteCache::new(),
            iface_select_sites: crate::runtime::interpreter::IfaceSelectSiteCache::new(),
            indy_sites: crate::runtime::invokedynamic::IndySiteCache::new(),
            native_shadow_cache: FxHashMap::default(),
            kind: ThreadKind::Platform,
            pin_count: 0,
            pin_reason: "",
            native_monitor_enters: 0,
            vm_error_upcall_seen: false,
            vt_yield: VtYield::Park,
            vt_slice_started: None,
            jdk_continuation: JdkContinuationState::default(),
            continuation_return_adapters: Vec::new(),
            jmx_frame_monitors_published: AtomicBool::new(false),
            scoped_values: Vec::new(),
            cast_operands_note: std::sync::atomic::AtomicU64::new(u64::MAX),
            cast_message_memo: None,
            loader_drives_in_flight: Vec::new(),
            tlab: cratonvm_gc::Tlab::empty(),
            gc_occupancy_poll_at: 0,
            oome_construct_depth: 0,
            shadow_stack: cratonvm_gc::shadow_stack::ShadowStack::empty(),
            jit_monitor_block: JitMonitorBlock::unarmed(),
            jit_stack_floor: 0,
            self_call_extra_budget: 0,
            xt_force_takeover: crate::jit::xt_root_scan::ForceTakeover::new(),
            jit_pending_exception: None,
            uncaught_exception_pending: None,
            frame_pop_requests: Vec::new(),
            vh_exact_site_memo: Vec::new(),
            root_publish_seq: std::sync::atomic::AtomicU64::new(0),
            served_door_in_flight: None,
            served_door_key_memo: None,
            door_promise: None,
        }
    }

    /// Return a popped frame's Vec allocations to the pool for reuse.
    ///
    /// Overflow (thread-owned pool already at `MAX_POOL_SIZE`) is offered to
    /// the per-OS-thread SoA pool in `runtime::frame`, which backs the
    /// *non-pooled* constructors `Frame::new` / `Frame::new_from_arcs` — the
    /// latter being the hot uncached invoke path, which previously allocated
    /// and zero-filled four `Vec`s per frame. Buffers that pool declines
    /// (cap reached, or oversized) are dropped exactly as before, so this is
    /// never worse than the old behaviour.
    ///
    /// The thread-owned `locals_pool` / `stacks_pool` keep first claim, so the
    /// cached invoke path's pool dynamics are unchanged.
    pub fn recycle_frame(&mut self, frame: Frame) {
        if self.locals_pool.len() < MAX_POOL_SIZE {
            frame.recycle(&mut self.locals_pool, &mut self.stacks_pool);
            return;
        }
        let (local_vals, local_tags, stack_vals, stack_tags) = frame.take_pool_parts();
        crate::runtime::frame::offer_frame_parts_to_tls_pool(
            local_vals, local_tags, stack_vals, stack_tags,
        );
    }

    /// T10.7 — recycle a frame, spilling any overflow into the VM-wide
    /// `VecPool`s on `SharedVm`.
    ///
    /// Semantics:
    ///   * When the thread-local `locals_pool` / `stacks_pool` has room, the
    ///     frame's Vecs are retained there exactly as before (zero locking).
    ///   * Once the thread-local pool is full (`MAX_POOL_SIZE`), we keep the
    ///     frame's Vecs alive by handing them to the VM-wide shared pools
    ///     (`operand_stack_pool` for the `Vec<u64>` halves and `tag_pool` for
    ///     the `Vec<u8>` halves).  Sibling threads later pull from the shared
    ///     pool when their own thread-local pool is empty (see
    ///     `refill_pools_from_shared`).
    ///
    /// This preserves allocations across thread boundaries without impacting
    /// the hot path — the shared mutex is only touched on the cold
    /// "thread-local pool full" edge.
    pub fn recycle_frame_with_shared(
        &mut self,
        frame: Frame,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
    ) {
        // Split the frame's inner Vecs out of it exactly once.
        let (local_vals, local_tags, stack_vals, stack_tags) = frame.take_pool_parts();
        self.route_pool_parts(
            local_vals,
            local_tags,
            stack_vals,
            stack_tags,
            operand_stack_pool,
            tag_pool,
        );
    }

    /// [`Self::recycle_frame_with_shared`] for a frame that is still sitting in
    /// its `FrameStack` slot: harvest its four pooled `Vec`s in place, then let
    /// `FrameStack::truncate` drop the husk where it lies.
    ///
    /// # Why
    ///
    /// `Frame` is a ~300-byte by-value struct and the return path used to move
    /// it three times — out of the buffer (`FrameStack::pop`), into
    /// `recycle_frame_with_shared`, and again into `take_pool_parts` — to
    /// arrive at four `Vec` headers. `perf` on the interpreted-invoke probe put
    /// `memcpy` under `Vec::pop<Frame>` and `pop_and_recycle_frame_with_reason`
    /// at 6.97% of the invoke arm, inside a frame-lifecycle group worth ~24.7%
    /// of it. See
    /// `docs/internal/performance/interpreted-invoke-cost-350ns-RETIRED-20260911.md`.
    ///
    /// Returns `false` when there was no frame to pop, so callers keep the
    /// `if let Some(..)` shape the `pop()` form gave them.
    ///
    /// `slot_reuse` is the VM's `VmConfig::frame_slot_reuse`
    /// (`CRATONVM_JIT_NO_FRAME_SLOT_REUSE` clear), a field the caller's
    /// `shared.config` line already holds (interpreter round i2 wave 61, lane
    /// L7: it was a process memo slot read on every return).
    #[inline]
    pub fn recycle_top_frame_in_place(
        &mut self,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
        slot_reuse: bool,
    ) -> bool {
        if self.frames.is_empty() {
            return false;
        }
        if slot_reuse {
            // Retire the frame where it lies. Its four buffers stay in the
            // slot, and the next call at this depth is rebuilt in them by
            // `FrameStack::push_cached_compact_reusing` -- no pool round
            // trip, no `Vec` headers moved, no `Frame` moved. Nothing above
            // the new depth is live, so nothing scans what it left.
            //
            // A by-value push at this depth still harvests the slot first
            // (see `push_frame_and_fire_entry`), so the pooled constructors
            // keep the recycling they have always had.
            return self.frames.retire_top();
        }
        self.recycle_top_frame_pooled(operand_stack_pool, tag_pool)
    }

    /// [`Self::recycle_top_frame_in_place`] under
    /// `CRATONVM_JIT_NO_FRAME_SLOT_REUSE`: harvest the top frame's buffers
    /// into the pools and drop its husk. Out of line so every return keeps
    /// only the one-line retire (interpreter round i1 wave 32).
    #[inline(never)]
    fn recycle_top_frame_pooled(
        &mut self,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
    ) -> bool {
        let depth = self.frames.len();
        let Some(top) = self.frames.last_mut() else {
            return false;
        };
        let (local_vals, local_tags, stack_vals, stack_tags) = top.take_pool_parts_in_place();
        // `truncate_hard` runs `Drop` on the element where it sits and never
        // moves a surviving frame — the address-stability contract
        // `FrameStack` exists for. The husk's remaining fields (metadata
        // `Arc`s, the code, `osr_attempt_counts`) are released by that drop
        // exactly as they were by the old move. Plain `truncate` (used here
        // until interpreter round i1 wave 37) only lowers the depth since
        // frames are retired in place, so every pop under this switch left an
        // emptied husk holding its method's `Arc`s until the next push at
        // that depth -- the husk `truncate_hard` exists to drop.
        self.frames.truncate_hard(depth - 1);
        // A slab-windowed frame (interpreter round i1 wave 29,
        // `runtime::slot_slab`) hands back four EMPTY `Vec`s: routing them
        // would poison the pools, as `harvest_retired_slot` explains.
        if local_vals.capacity() == 0 && stack_vals.capacity() == 0 {
            return true;
        }
        self.route_pool_parts(
            local_vals,
            local_tags,
            stack_vals,
            stack_tags,
            operand_stack_pool,
            tag_pool,
        );
        true
    }

    /// Hand the retired slot's pooled buffers to the thread pools and leave
    /// the slot where it is, a husk that the next cached install rebuilds in
    /// a slot-slab window (`FrameStack::retired_slot_takes_a_window`'s husk
    /// rule). Interpreter round i1 wave 32: a slot that first held a pooled
    /// frame used to reset its own four buffers on every later call at its
    /// depth, and that is most slots on a call-heavy workload. Unlike
    /// [`Self::harvest_retired_slot`] nothing above it is trimmed, and the
    /// buffers are pooled, not freed, so a by-value push at this depth finds
    /// them again.
    #[cold]
    #[inline(never)]
    pub fn convert_retired_slot_to_window(&mut self) {
        let Some(slot) = self.frames.retired_slot_mut() else {
            return;
        };
        let (local_vals, local_tags, stack_vals, stack_tags) = slot.take_pool_parts_in_place();
        if local_vals.capacity() == 0 && stack_vals.capacity() == 0 {
            return;
        }
        if self.locals_pool.len() < MAX_POOL_SIZE {
            self.locals_pool.push((local_vals, local_tags));
            self.stacks_pool.push((stack_vals, stack_tags));
        }
    }

    /// Thread-local pool first, VM-wide pools once it is full. Shared by both
    /// recycle entry points so the routing policy cannot drift between them.
    /// Move a retired frame slot's buffers into the thread pools, so an
    /// ordinary by-value push may overwrite the slot without losing them.
    ///
    /// No-op when no slot is retired, which is the case on every first call at
    /// a given depth and whenever `CRATONVM_JIT_NO_FRAME_SLOT_REUSE` is set.
    pub fn harvest_retired_slot(&mut self) {
        let Some(slot) = self.frames.retired_slot_mut() else {
            return;
        };
        let (local_vals, local_tags, stack_vals, stack_tags) = slot.take_pool_parts_in_place();
        self.frames.trim_retired();
        // A husk whose buffers were already harvested (the pooled recycle path,
        // i.e. `CRATONVM_JIT_NO_FRAME_SLOT_REUSE`) hands back four EMPTY `Vec`s.
        // Pushing those into the pools poisons them: the next frame build pops
        // a zero-capacity buffer and reallocates from scratch, which measured
        // as a 2-3x regression with `ret_recycle` at 427 cycles against 17.
        if local_vals.capacity() == 0 && stack_vals.capacity() == 0 {
            return;
        }
        if self.locals_pool.len() < MAX_POOL_SIZE {
            self.locals_pool.push((local_vals, local_tags));
            self.stacks_pool.push((stack_vals, stack_tags));
        }
    }

    fn route_pool_parts(
        &mut self,
        local_vals: Vec<u64>,
        local_tags: Vec<u8>,
        stack_vals: Vec<u64>,
        stack_tags: Vec<u8>,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
    ) {
        if self.locals_pool.len() < MAX_POOL_SIZE {
            self.locals_pool.push((local_vals, local_tags));
            self.stacks_pool.push((stack_vals, stack_tags));
            return;
        }
        // Thread-local pool is full — spill into the shared pools.
        operand_stack_pool.release(local_vals);
        tag_pool.release(local_tags);
        operand_stack_pool.release(stack_vals);
        tag_pool.release(stack_tags);
    }

    /// T10.7 — replenish the thread-local `locals_pool` / `stacks_pool` from
    /// the VM-wide shared pools when empty.
    ///
    /// Invoked just before a frame-push that will consume pooled Vecs so the
    /// shared-pool mutex is touched only on the cold refill path, not per call.
    pub fn refill_pools_from_shared(
        &mut self,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
        max_locals: usize,
        max_stack: usize,
    ) {
        if self.locals_pool.is_empty() {
            let vals = operand_stack_pool.acquire(max_locals);
            let tags = tag_pool.acquire(max_locals);
            self.locals_pool.push((vals, tags));
        }
        if self.stacks_pool.is_empty() {
            let vals = operand_stack_pool.acquire(max_stack);
            let tags = tag_pool.acquire(max_stack);
            self.stacks_pool.push((vals, tags));
        }
    }
}

impl Default for JvmThread {
    /// Creates a default "main" thread with id 0.
    /// Used with `std::mem::take` for borrow-splitting.
    fn default() -> Self {
        Self::new(ThreadId(0), "main")
    }
}

impl std::fmt::Debug for JvmThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JvmThread")
            .field("thread_id", &self.thread_id)
            .field("name", &self.name)
            .field("frames_depth", &self.frames.len())
            .field("daemon", &self.daemon)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JIT contract — `JvmThread::tlab_offset()` must report the actual
    /// byte offset of the `tlab` field. The JIT-emitted inline bump in
    /// `jit/src/x64.rs` reads `[thread + tlab_offset + CURSOR_OFFSET]`,
    /// so a wrong answer here corrupts the heap.
    #[test]
    fn tlab_offset_matches_field_address() {
        let t = JvmThread::default();
        let base = &t as *const JvmThread as usize;
        let tlab_addr = &t.tlab as *const cratonvm_gc::Tlab as usize;
        let expected = tlab_addr - base;
        assert_eq!(
            JvmThread::tlab_offset(),
            expected,
            "JvmThread::tlab_offset() out of sync with the actual field offset"
        );
        // Caching: a second call must return the same value (OnceLock).
        assert_eq!(JvmThread::tlab_offset(), expected);
    }

    #[test]
    fn thread_creation() {
        let thread = JvmThread::new(ThreadId(1), "worker-1");
        assert_eq!(thread.thread_id, ThreadId(1));
        assert_eq!(thread.name, "worker-1");
        assert!(thread.frames.is_empty());
        assert!(thread.printed.is_empty());
        assert!(!thread.daemon);
    }

    #[test]
    fn thread_default_is_main() {
        let thread = JvmThread::default();
        assert_eq!(thread.thread_id, ThreadId(0));
        assert_eq!(thread.name, "main");
    }

    #[test]
    fn thread_id_display() {
        assert_eq!(format!("{}", ThreadId(0)), "Thread-0");
        assert_eq!(format!("{}", ThreadId(42)), "Thread-42");
    }

    #[test]
    fn park_state_unpark_before_park() {
        let ps = ParkState::new();
        // Unpark first → next park returns immediately
        ps.unpark();
        ps.park(Some(std::time::Duration::from_millis(100)));
        // If park didn't return immediately, the test would timeout
    }

    #[test]
    fn park_state_park_with_timeout() {
        let ps = ParkState::new();
        let start = std::time::Instant::now();
        ps.park(Some(std::time::Duration::from_millis(50)));
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 40,
            "Park should have blocked for ~50ms, got {}ms",
            elapsed.as_millis()
        );
    }

    /// A timed park must end near its deadline, not at the next system tick.
    ///
    /// On Windows every condvar/`WaitForSingleObject`-shaped wait is rounded up
    /// to the 15.625 ms clock tick, so this 50 ms park measured 62.5 ms — four
    /// short cycles then one long one for anything re-arming from an absolute
    /// deadline, which is what walked `AutoScalingEventExecutorChooserFactoryTest`
    /// past the state it asserts on. `win_park` bounds it with a
    /// high-resolution waitable timer instead. The 40 ms floor of
    /// `park_state_park_with_timeout` above cannot see any of that; this is the
    /// ceiling that can.
    ///
    /// The bound is deliberately loose (a loaded CI box can delay any wakeup)
    /// and still an order of magnitude tighter than the 15.6 ms quantum: 62 ms
    /// was the OLD median, not an outlier, so a regression fails this on the
    /// median run rather than needing an unlucky one. Non-Windows hosts have
    /// no such rounding and pass it for free.
    #[test]
    fn park_state_timed_park_does_not_overshoot_to_the_system_tick() {
        let ps = ParkState::new();
        // One warm-up park: the first one on a thread creates the
        // high-resolution timer, and that is not what is being measured.
        ps.park(Some(std::time::Duration::from_millis(5)));
        let mut best = std::time::Duration::from_secs(1);
        for _ in 0..5 {
            let start = std::time::Instant::now();
            ps.park(Some(std::time::Duration::from_millis(50)));
            best = best.min(start.elapsed());
        }
        assert!(
            best >= std::time::Duration::from_millis(45),
            "a 50 ms park returned after only {best:?}"
        );
        assert!(
            best < std::time::Duration::from_millis(58),
            "a 50 ms park took {best:?} at best -- the deadline is being rounded \
             up to the 15.625 ms system tick (62.5 ms was the pre-fix median)"
        );
    }

    #[test]
    fn park_state_unpark_wakes_parked_thread() {
        let ps = Arc::new(ParkState::new());
        let ps2 = ps.clone();
        let handle = std::thread::spawn(move || {
            // This will block until unparked
            ps2.park(Some(std::time::Duration::from_secs(5)));
        });
        // Give the thread time to park
        std::thread::sleep(std::time::Duration::from_millis(20));
        ps.unpark();
        handle.join().unwrap();
    }

    /// Interpreter round i3 wave 71, lane L1 (`CRATONVM_PARK_LAZY`): the
    /// running phase takes a permit already there, returns on an interrupt,
    /// gives up at once when told to stop running, and gives up when its
    /// budget runs out -- leaving no permit behind in any of those.
    #[test]
    fn park_lazily_takes_a_permit_and_gives_up_on_stop_or_budget() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let ps = ParkState::new();
        let not_interrupted = AtomicBool::new(false);
        let budget = std::time::Duration::from_millis(5);
        ps.unpark();
        assert!(
            ps.park_lazily(budget, &not_interrupted, &|| true),
            "permit already set"
        );
        assert!(!ps.take_permit(), "the permit was consumed");

        let interrupted = AtomicBool::new(true);
        assert!(
            ps.park_lazily(budget, &interrupted, &|| true),
            "an interrupt ends a park"
        );

        let t0 = std::time::Instant::now();
        assert!(
            !ps.park_lazily(std::time::Duration::from_secs(5), &not_interrupted, &|| {
                false
            })
        );
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "a stop request is not waited out"
        );

        let t0 = std::time::Instant::now();
        assert!(
            !ps.park_lazily(budget, &not_interrupted, &|| true),
            "budget ran out"
        );
        assert!(t0.elapsed() >= budget);
        assert!(!ps.take_permit());

        // A stop request raised mid-phase is seen within a slice or so.
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                stop.store(true, Ordering::Release);
            });
            let t0 = std::time::Instant::now();
            assert!(
                !ps.park_lazily(std::time::Duration::from_secs(5), &not_interrupted, &|| {
                    !stop.load(Ordering::Acquire)
                })
            );
            assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        });
    }

    /// Interpreter round i3 wave 71, lane L1: an `unpark` from another thread
    /// ends the running phase well before its budget.
    #[test]
    fn park_lazily_is_woken_by_an_unpark() {
        let ps = Arc::new(ParkState::new());
        let ps2 = ps.clone();
        let not_interrupted = std::sync::atomic::AtomicBool::new(false);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            ps2.unpark();
        });
        let t0 = std::time::Instant::now();
        assert!(
            ps.park_lazily(std::time::Duration::from_secs(5), &not_interrupted, &|| {
                true
            })
        );
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        handle.join().unwrap();
    }

    /// Interpreter round i3 wave 72, lane L6 (`CRATONVM_PARK_LAZY_STW_WAKE`):
    /// with one wait as long as the whole budget, a stop request followed by
    /// `wake_lazy_phase` ends the running phase at once (no slice to wait
    /// out), leaves no permit, and a later permit still ends the next phase.
    #[test]
    fn park_lazily_sliced_is_ended_by_a_stop_and_a_wake() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let ps = Arc::new(ParkState::new());
        let not_interrupted = AtomicBool::new(false);
        let stop = Arc::new(AtomicBool::new(false));
        let long = std::time::Duration::from_secs(30);
        let (ps2, stop2) = (ps.clone(), stop.clone());
        let waker = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            stop2.store(true, Ordering::SeqCst);
            ps2.wake_lazy_phase();
        });
        let t0 = std::time::Instant::now();
        let over = ps.park_lazily_sliced(long, long, &not_interrupted, &|| {
            !stop.load(Ordering::SeqCst)
        });
        assert!(!over, "a stop is not the end of the park");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "the wake was not waited out"
        );
        assert!(!ps.take_permit(), "a wake is not a permit");
        waker.join().unwrap();

        // A stray wake with nothing to stop for does not end a phase.
        ps.wake_lazy_phase();
        let short = std::time::Duration::from_millis(5);
        assert!(!ps.park_lazily_sliced(short, short, &not_interrupted, &|| true));
        ps.unpark();
        assert!(ps.park_lazily_sliced(long, long, &not_interrupted, &|| true));
    }

    /// Interpreter round i3 wave 73, lane L6 (page
    /// `i72-L6-jfr-thread-park-misses-the-lazy-park-phase-20261105`): a
    /// park's event span starts before the running phase and keeps the
    /// caller's timeout, however much of it the phase used; no recording, no
    /// span (no clock read).
    #[test]
    fn a_park_event_span_covers_the_running_phase_with_the_callers_timeout() {
        use std::sync::atomic::AtomicBool;
        assert!(
            ParkEventSpan::begin(false, None).is_none(),
            "not recording: no span"
        );
        let caller = std::time::Duration::from_millis(30);
        let span = ParkEventSpan::begin(true, Some(caller)).expect("recording");
        // The running phase uses part of the timeout, as `park_lazily` does,
        // and the blocking path then waits out the rest.
        let ps = ParkState::new();
        let not_interrupted = AtomicBool::new(false);
        let phase = std::time::Duration::from_millis(5);
        assert!(!ps.park_lazily(phase, &not_interrupted, &|| true));
        let rest = caller.saturating_sub(span.duration());
        ps.park_interruptible(Some(rest), &not_interrupted);
        assert!(span.duration() >= caller, "the span covers phase and wait");
        assert_eq!(
            span.timeout_field(),
            30_000_000,
            "the caller's timeout, not the rest"
        );
        assert_eq!(span.legacy_timeout_ns(), 30_000_000);
        let untimed = ParkEventSpan::begin(true, None).expect("recording");
        assert_eq!(untimed.timeout_field(), i64::MIN, "HotSpot: Long.MIN_VALUE");
        assert_eq!(untimed.legacy_timeout_ns(), 0);
        assert_eq!(untimed.until_field(), i64::MIN);
        assert_eq!(span.until_field(), i64::MIN, "a relative park has no until");
    }

    /// Interpreter round i3 wave 79, lane L6 (`CRATONVM_PARK_LAZY_SPIN_NS`):
    /// the spin takes a permit that is there or arrives within the budget,
    /// ends on a pending interrupt without clearing it, and gives up -- with
    /// nothing consumed -- when the budget runs out or `keep_spinning` says
    /// stop; on a plain and on a lock-free state alike.
    #[test]
    fn spin_for_permit_takes_a_permit_and_gives_up_cleanly() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        for lock_free in [false, true] {
            let ps = Arc::new(ParkState::new());
            if lock_free {
                ps.enable_lock_free();
            }
            let not_interrupted = AtomicBool::new(false);
            let budget = std::time::Duration::from_millis(2);
            // No permit: the budget runs out, nothing consumed.
            let t0 = std::time::Instant::now();
            assert!(!ps.spin_for_permit(budget, &not_interrupted, &|| true));
            assert!(t0.elapsed() >= budget);
            // A permit already there: taken at once, and only once.
            ps.unpark();
            assert!(ps.spin_for_permit(budget, &not_interrupted, &|| true));
            assert!(
                !ps.take_permit_lock_free(),
                "lock_free={lock_free}: taken once"
            );
            // A permit that arrives while it spins.
            let waker = {
                let ps = Arc::clone(&ps);
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    ps.unpark();
                })
            };
            assert!(ps.spin_for_permit(
                std::time::Duration::from_secs(5),
                &not_interrupted,
                &|| true
            ));
            waker.join().unwrap();
            // A pending interrupt ends it and stays set.
            let interrupted = AtomicBool::new(true);
            assert!(ps.spin_for_permit(budget, &interrupted, &|| true));
            assert!(interrupted.load(std::sync::atomic::Ordering::Relaxed));
            // `keep_spinning` false: gives up long before a long budget.
            let t0 = std::time::Instant::now();
            assert!(!ps.spin_for_permit(
                std::time::Duration::from_secs(5),
                &not_interrupted,
                &|| false
            ));
            assert!(t0.elapsed() < std::time::Duration::from_secs(1));
        }
    }

    /// Interpreter round i3 wave 79, lane L6 (page
    /// `i73-L6-jfr-thread-park-fields-differ-from-hotspot`): an absolute
    /// park's event carries `timeout = Long.MIN_VALUE` and `until` = the
    /// caller's deadline, as HotSpot's `Unsafe_Park` posts `parkUntil`.
    #[test]
    fn an_absolute_park_event_span_reports_until_not_timeout() {
        let remaining = std::time::Duration::from_millis(250);
        let span = ParkEventSpan::begin_until(true, Some(remaining), Some(1_700_000_000_123))
            .expect("recording");
        assert_eq!(span.timeout_field(), i64::MIN);
        assert_eq!(span.until_field(), 1_700_000_000_123);
        assert!(ParkEventSpan::begin_until(false, Some(remaining), Some(1)).is_none());
    }

    /// gci i23/pk (finding 6 of
    /// `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): a negative
    /// relative park reports the caller's raw `time` as `timeout` (HotSpot's
    /// `Unsafe_Park` posts `timeout = time`), not the `Duration`'s 0; the
    /// legacy recorder and `until` are unchanged, and an absolute park keeps
    /// `timeout = Long.MIN_VALUE` whatever the raw value says.
    #[test]
    fn gci_i23pk_a_negative_relative_park_reports_the_raw_timeout() {
        let zero = Some(std::time::Duration::ZERO);
        let span = ParkEventSpan::begin(true, zero).expect("recording");
        assert_eq!(span.timeout_field(), 0, "the Duration answer");
        let raw = span.with_raw_timeout(Some(-1));
        assert_eq!(raw.timeout_field(), -1);
        assert_eq!(raw.until_field(), i64::MIN);
        assert_eq!(raw.legacy_timeout_ns(), 0);
        let min = span.with_raw_timeout(Some(i64::MIN));
        assert_eq!(min.timeout_field(), i64::MIN);
        assert_eq!(span.with_raw_timeout(None).timeout_field(), 0);
        let abs = ParkEventSpan::begin_until(true, zero, Some(5))
            .expect("recording")
            .with_raw_timeout(Some(-1));
        assert_eq!(abs.timeout_field(), i64::MIN);
        assert_eq!(abs.until_field(), 5);
    }

    /// Interpreter round i3 wave 73, lane L6 (`CRATONVM_PARK_ATOMIC_PERMIT`):
    /// the lock-free halves keep the permit's one-bit semantics -- a permit
    /// set before a park is taken once, a second unpark does not stack, the
    /// mutex and lock-free halves see each other's permit, and no waiter
    /// stays counted after any kind of wait returns.
    #[test]
    fn lock_free_permit_halves_keep_the_one_bit_semantics() {
        use std::sync::atomic::AtomicBool;
        let ps = ParkState::new();
        ps.enable_lock_free();
        let not_interrupted = AtomicBool::new(false);
        assert!(!ps.take_permit_lock_free(), "no permit yet");
        ps.unpark_lock_free();
        ps.unpark_lock_free();
        assert!(ps.take_permit_lock_free(), "one permit");
        assert!(!ps.take_permit_lock_free(), "permits do not stack");
        ps.unpark_lock_free();
        assert!(ps.take_permit(), "the mutex half sees a lock-free permit");
        ps.unpark();
        assert!(
            ps.take_permit_lock_free(),
            "the lock-free half sees a mutex permit"
        );
        ps.unpark_lock_free();
        let t0 = std::time::Instant::now();
        ps.park_interruptible(Some(std::time::Duration::from_secs(5)), &not_interrupted);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "the permit ended the park"
        );
        assert!(!ps.take_permit(), "and was consumed");
        ps.unpark_lock_free();
        assert!(
            ps.park_lazily(std::time::Duration::from_secs(5), &not_interrupted, &|| {
                true
            })
        );
        ps.park_interruptible(Some(std::time::Duration::from_millis(2)), &not_interrupted);
        ps.park(Some(std::time::Duration::from_millis(2)));
        assert_eq!(ps.sleeper_count(), 0, "no waiter left counted");
    }

    /// A lock-free unpark wakes a thread already blocked in an untimed or a
    /// long timed park (it sees the sleeper and notifies under the mutex).
    #[test]
    fn lock_free_unpark_wakes_a_blocked_parker() {
        use std::sync::atomic::AtomicBool;
        let ps = Arc::new(ParkState::new());
        ps.enable_lock_free();
        for timed in [false, true] {
            let ps2 = ps.clone();
            let parker = std::thread::spawn(move || {
                let not_interrupted = AtomicBool::new(false);
                let t0 = std::time::Instant::now();
                let timeout = timed.then_some(std::time::Duration::from_secs(20));
                ps2.park_interruptible(timeout, &not_interrupted);
                t0.elapsed()
            });
            while ps.sleeper_count() == 0 {
                std::thread::yield_now();
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            ps.unpark_lock_free();
            let waited = parker.join().unwrap();
            assert!(
                waited < std::time::Duration::from_secs(10),
                "timed={timed}: woken"
            );
        }
        assert_eq!(ps.sleeper_count(), 0);
    }

    /// Without `enable_lock_free` (the switch off) a wait keeps no sleeper
    /// count, and `unpark_lock_free` falls back to the mutex path.
    #[test]
    fn lock_free_unpark_of_a_plain_state_takes_the_mutex_path() {
        use std::sync::atomic::AtomicBool;
        let ps = Arc::new(ParkState::new());
        let ps2 = ps.clone();
        let parker = std::thread::spawn(move || {
            let not_interrupted = AtomicBool::new(false);
            let t0 = std::time::Instant::now();
            ps2.park_interruptible(None, &not_interrupted);
            t0.elapsed()
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(
            ps.sleeper_count(),
            0,
            "a plain state keeps no sleeper count"
        );
        ps.unpark_lock_free();
        assert!(parker.join().unwrap() < std::time::Duration::from_secs(10));
    }

    /// Lost-wakeup stress: two threads hand a turn back and forth with
    /// lock-free unparks and short timed parks (a park returns on its permit,
    /// its timeout or spuriously; the turn variable says whose turn it is).
    /// Every round's unpark must wake its parker without waiting out a long
    /// timeout: a lost wake-up shows as a round that ends only on the 5 s
    /// fallback timeout, counted and asserted zero. Also mixes in 1 us parks,
    /// which race the unpark against the waiter's count-then-check.
    #[test]
    fn lock_free_unpark_loses_no_wakeup_under_stress() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        const ROUNDS: u64 = 20_000;
        let a = Arc::new(ParkState::new());
        let b = Arc::new(ParkState::new());
        a.enable_lock_free();
        b.enable_lock_free();
        let turn = Arc::new(AtomicU64::new(0));
        let lost = Arc::new(AtomicU64::new(0));
        const FALLBACK: std::time::Duration = std::time::Duration::from_secs(5);
        let side = |me: Arc<ParkState>, peer: Arc<ParkState>, parity: u64| {
            let (turn, lost) = (turn.clone(), lost.clone());
            std::thread::spawn(move || {
                let not_interrupted = AtomicBool::new(false);
                for round in 0..ROUNDS {
                    let mine = round * 2 + parity;
                    loop {
                        if turn.load(Ordering::SeqCst) == mine {
                            break;
                        }
                        if me.take_permit_lock_free() {
                            continue;
                        }
                        let timeout = if round % 3 == 0 {
                            std::time::Duration::from_micros(1)
                        } else {
                            FALLBACK
                        };
                        let t0 = std::time::Instant::now();
                        me.park_interruptible(Some(timeout), &not_interrupted);
                        if timeout == FALLBACK
                            && t0.elapsed() >= FALLBACK
                            && turn.load(Ordering::SeqCst) == mine
                        {
                            lost.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    turn.store(mine + 1, Ordering::SeqCst);
                    peer.unpark_lock_free();
                }
            })
        };
        let t_a = side(a.clone(), b.clone(), 0);
        let t_b = side(b.clone(), a.clone(), 1);
        t_a.join().unwrap();
        t_b.join().unwrap();
        assert_eq!(
            lost.load(Ordering::SeqCst),
            0,
            "a lock-free unpark was lost"
        );
        assert_eq!(turn.load(Ordering::SeqCst), ROUNDS * 2);
        assert_eq!(a.sleeper_count(), 0);
        assert_eq!(b.sleeper_count(), 0);
    }

    /// Interpreter round i3 wave 73, lane L6 (page
    /// `i72-L6-windows-park-event-left-signalled-by-a-lazy-park-20261105`):
    /// a permit taken off the event path -- the fast path, the lazy running
    /// phase -- leaves the wake event unsignalled, with either unpark half.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_permit_taken_off_the_event_path_leaves_the_wake_event_unsignalled() {
        use std::sync::atomic::AtomicBool;
        let ps = ParkState::new();
        let not_interrupted = AtomicBool::new(false);
        ps.unpark();
        assert!(ps.take_permit());
        assert!(!ps.wake_event_left_signalled(), "fast path, mutex unpark");
        ps.unpark_lock_free();
        assert!(ps.take_permit_lock_free());
        assert!(
            !ps.wake_event_left_signalled(),
            "fast path, lock-free unpark"
        );
        ps.unpark();
        assert!(ps.park_lazily(
            std::time::Duration::from_millis(50),
            &not_interrupted,
            &|| true
        ));
        assert!(!ps.wake_event_left_signalled(), "lazy phase");
        // A timed park that times out with no unpark leaves nothing either.
        ps.park_interruptible(Some(std::time::Duration::from_millis(2)), &not_interrupted);
        assert!(!ps.wake_event_left_signalled(), "timed out");
    }

    /// Windows: an unpark during an accurate timed park still wakes it
    /// through the event, and leaves the event unsignalled afterwards.
    #[cfg(target_os = "windows")]
    #[test]
    fn an_accurate_park_is_woken_by_the_event_and_leaves_it_clear() {
        use std::sync::atomic::AtomicBool;
        let ps = Arc::new(ParkState::new());
        ps.enable_lock_free();
        let ps2 = ps.clone();
        let parker = std::thread::spawn(move || {
            let not_interrupted = AtomicBool::new(false);
            let t0 = std::time::Instant::now();
            ps2.park_interruptible(Some(std::time::Duration::from_secs(20)), &not_interrupted);
            t0.elapsed()
        });
        while ps.sleeper_count() == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        ps.unpark();
        assert!(parker.join().unwrap() < std::time::Duration::from_secs(10));
        assert!(!ps.wake_event_left_signalled());
    }

    /// The monitor census counters are where compiled code adds to them (the
    /// pinned `JIT_MONITOR_BLOCK_*` offsets), `census_counts` reads those
    /// words, a disarm keeps them, and `census_due` fires once per doubling.
    #[test]
    fn monitor_census_counters_sit_at_the_offsets_compiled_code_bumps() {
        use cratonvm_jit_api as api;
        let mut block = JitMonitorBlock::unarmed();
        let base = &mut block as *mut JitMonitorBlock as *mut u8;
        let bump = |off: usize, by: u64| {
            // SAFETY: `off` is one of the four pinned counter offsets of the
            // `#[repr(C)]` block, each an 8-aligned `AtomicU64`; this is the
            // plain `ADD QWORD [thread + block + off]` the JIT emits.
            unsafe {
                let p = base.add(off) as *mut u64;
                p.write(p.read() + by);
            }
        };
        bump(api::JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET, 1);
        bump(api::JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET, 2);
        bump(api::JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET, 3);
        bump(api::JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET, 4);
        assert_eq!(block.census_counts(), (1, 2, 3, 4));
        block.disarm();
        assert_eq!(
            block.census_counts(),
            (1, 2, 3, 4),
            "disarm keeps the census"
        );
        assert!(!block.is_armed());
        // Total 10 (bit length 4): due once, then not again until 16.
        assert!(block.census_due());
        assert!(!block.census_due());
        let base = &mut block as *mut JitMonitorBlock as *mut u8;
        // SAFETY: as above.
        unsafe {
            let p = base.add(api::JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET) as *mut u64;
            p.write(p.read() + 6);
        }
        assert!(block.census_due(), "16 is a new doubling");
        // Never read the flag here: `Drop` must stay silent for this block.
        assert_eq!(block.census, CENSUS_UNREAD);
    }
}

/// JIT round 16 wave 14 (lane ffm3): the lazy in-native state machine of
/// [`GcBlockState`] (`CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY`).
#[cfg(test)]
mod lazy_native_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn a_band_must_be_aligned_non_empty_and_bounded() {
        assert!(lazy_native_band_ok(0x1000, 0x2000));
        assert!(!lazy_native_band_ok(0, 0x2000), "no band");
        assert!(!lazy_native_band_ok(0x1004, 0x2000), "misaligned low end");
        assert!(!lazy_native_band_ok(0x2000, 0x2000), "empty");
        assert!(!lazy_native_band_ok(0x3000, 0x2000), "inverted");
        assert!(!lazy_native_band_ok(
            0x1000,
            0x1000 + LAZY_NATIVE_MAX_BAND + 8
        ));
    }

    #[test]
    fn enter_publishes_the_band_and_leave_clears_the_state() {
        let b = GcBlockState::new();
        assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
        b.enter_lazy_native(0x1000, 0x9000);
        assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert!(!b.leave_lazy_native(), "an unclaimed leave never waits");
        assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
        // A leave without an entry is a no-op.
        assert!(!b.leave_lazy_native());
        assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
    }

    #[test]
    fn only_a_thread_in_native_is_claimed_and_only_once() {
        let b = GcBlockState::new();
        assert_eq!(b.try_claim_lazy_native(), None, "a running thread");
        b.enter_lazy_native(0x1000, 0x9000);
        assert_eq!(b.try_claim_lazy_native(), Some((0x1000, 0x9000)));
        assert_eq!(b.try_claim_lazy_native(), None, "already claimed");
        assert!(b.release_lazy_native_claim());
        assert!(!b.release_lazy_native_claim(), "released once");
        assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert!(!b.leave_lazy_native());
    }

    #[test]
    fn a_claim_with_an_unusable_band_is_given_back() {
        let b = GcBlockState::new();
        b.enter_lazy_native(0x1004, 0x9000);
        assert_eq!(b.try_claim_lazy_native(), None);
        assert_eq!(
            b.lazy_native.load(Ordering::Acquire),
            LAZY_NATIVE_IN,
            "the thread stays in native, unclaimed: the pause waits for it"
        );
    }

    /// The leave of a claimed thread waits until the pause releases it, and
    /// only then returns -- the property that keeps its frames frozen while
    /// the pause reads them.
    #[test]
    fn a_claimed_thread_leaves_only_after_the_release() {
        let b = Arc::new(GcBlockState::new());
        b.enter_lazy_native(0x1000, 0x9000);
        assert!(b.try_claim_lazy_native().is_some());
        let left = Arc::new(AtomicBool::new(false));
        let t = {
            let b = Arc::clone(&b);
            let left = Arc::clone(&left);
            std::thread::spawn(move || {
                let waited = b.leave_lazy_native();
                left.store(true, Ordering::Release);
                waited
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(
            !left.load(Ordering::Acquire),
            "the leave must not pass a claim"
        );
        assert!(b.release_lazy_native_claim());
        let waited = t.join().unwrap_or(false);
        assert!(waited, "the leave reports that it waited");
        assert!(left.load(Ordering::Acquire));
        assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
    }

    /// JIT round 18 wave 10 (lane ffmeintr, `CRATONVM_FFM_LAZY_LEAVE_HANDOFF`,
    /// default ON): a leave that found its thread claimed marks it
    /// `CLAIMED_LEAVING`; no later claim can take it, and the release
    /// completes the leave (`-> OFF`) instead of putting the thread back in C,
    /// where the next pause could claim it again before it ran.
    #[test]
    fn a_waiting_leave_is_handed_over_by_the_release() {
        if !lazy_leave_handoff_enabled() {
            return; // `=0` in the environment: the round 16 leave, tested above.
        }
        let b = Arc::new(GcBlockState::new());
        b.enter_lazy_native(0x1000, 0x9000);
        assert!(b.try_claim_lazy_native().is_some());
        let t = {
            let b = Arc::clone(&b);
            std::thread::spawn(move || b.leave_lazy_native())
        };
        let t0 = std::time::Instant::now();
        while b.lazy_native.load(Ordering::Acquire) != LAZY_NATIVE_CLAIMED_LEAVING {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(20),
                "the waiting leave never marked itself"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            b.try_claim_lazy_native(),
            None,
            "a leaving thread is not in C"
        );
        assert!(
            b.release_lazy_native_claim(),
            "the release takes the leaving state"
        );
        assert_eq!(
            b.lazy_native.load(Ordering::Acquire),
            LAZY_NATIVE_OFF,
            "handed the leave: the next pause has nothing to claim"
        );
        assert_eq!(b.try_claim_lazy_native(), None);
        assert!(
            t.join().unwrap_or(false),
            "the leave reports that it waited"
        );
        assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
        assert!(!b.release_lazy_native_claim(), "released once");
    }

    /// JIT round 18 wave 10b (lane ffmeintr, `CRATONVM_FFM_LAZY_SIGNAL_GATE`):
    /// a gated thread cannot go into C lazily until the pass ungates it; a
    /// thread already in C is reported, not gated; a claim never takes a
    /// gated thread.
    #[test]
    fn a_gated_thread_waits_to_go_into_c_until_the_signal_is_answered() {
        let b = Arc::new(GcBlockState::new());
        assert_eq!(b.try_gate_takeover_signal(), TakeoverSignalGate::Gated);
        assert_eq!(
            b.try_gate_takeover_signal(),
            TakeoverSignalGate::Other,
            "gated once"
        );
        assert_eq!(
            b.try_claim_lazy_native(),
            None,
            "a gated thread is not in C"
        );
        let entered = Arc::new(AtomicBool::new(false));
        let t = {
            let b = Arc::clone(&b);
            let entered = Arc::clone(&entered);
            std::thread::spawn(move || {
                b.enter_lazy_native(0x1000, 0x9000);
                entered.store(true, Ordering::Release);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(
            !entered.load(Ordering::Acquire),
            "the entry must not pass the gate"
        );
        b.ungate_takeover_signal();
        let _ = t.join();
        assert!(entered.load(Ordering::Acquire));
        assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert_eq!(
            b.try_gate_takeover_signal(),
            TakeoverSignalGate::InNative,
            "a thread in C is not signalled"
        );
        assert_eq!(
            b.lazy_native.load(Ordering::Acquire),
            LAZY_NATIVE_IN,
            "untouched"
        );
        assert!(!b.leave_lazy_native());
        assert_eq!(b.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
    }

    /// JIT round 18 wave 10 (lane ffmeintr, EINTR2-1): a collector's hold on
    /// a blocked-wait band keeps the owner from leaving its trampoline until
    /// the release, one holder at a time; no band, no hold.
    #[test]
    fn a_held_blocked_wait_band_keeps_its_thread_inside() {
        let b = Arc::new(GcBlockState::new());
        assert_eq!(b.try_hold_blocked_wait_band(), None, "not in a trampoline");
        b.enter_blocked_wait_band(0x7000);
        assert_eq!(b.try_hold_blocked_wait_band(), Some(0x7000));
        assert_eq!(b.try_hold_blocked_wait_band(), None, "one holder");
        let left = Arc::new(AtomicBool::new(false));
        let t = {
            let b = Arc::clone(&b);
            let left = Arc::clone(&left);
            std::thread::spawn(move || {
                b.leave_blocked_wait_band(0x7000);
                left.store(true, Ordering::Release);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(
            !left.load(Ordering::Acquire),
            "the leave must not pass a hold"
        );
        b.release_blocked_wait_band(0x7000);
        let _ = t.join();
        assert!(left.load(Ordering::Acquire));
        assert_eq!(b.blocked_wait_lo.load(Ordering::Acquire), 0);
        assert_eq!(b.try_hold_blocked_wait_band(), None);
    }

    /// The trampoline runs its wait and returns its value; with the switch
    /// off (the default) it publishes nothing.
    #[test]
    fn the_blocked_wait_trampoline_returns_the_wait_and_withdraws_its_band() {
        let b = GcBlockState::new();
        let seen = run_blocked_wait(&b, || b.blocked_wait_lo.load(Ordering::Acquire));
        if helper_unparked_read_enabled() {
            assert_ne!(seen, 0, "published while waiting");
            assert_eq!(seen & 7, 0, "8-aligned");
        } else {
            assert_eq!(seen, 0, "the switch off publishes nothing");
        }
        assert_eq!(b.blocked_wait_lo.load(Ordering::Acquire), 0, "withdrawn");
        assert_eq!(run_blocked_wait(&b, || 41 + 1), 42);
    }
}

/// Interpreter round i3 wave 74, lane L6 (`CRATONVM_PARK_UNTIMED_NO_POLL`):
/// `ParkState::park_interruptible_unpolled` and its interrupt wake. Every
/// wait is bounded by a channel timeout, so a lost wake fails the test
/// instead of hanging it.
#[cfg(test)]
mod unpolled_park_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    const BOUND: Duration = Duration::from_secs(10);

    /// Park `ps` unpolled on a new thread; the receiver gets how long it
    /// waited.
    fn spawn_parker(
        ps: &Arc<ParkState>,
        flag: &Arc<AtomicBool>,
        timeout: Option<Duration>,
    ) -> mpsc::Receiver<Duration> {
        let (tx, rx) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            ps2.park_interruptible_unpolled(timeout, &flag2);
            let _ = tx.send(t0.elapsed());
        });
        rx
    }

    /// A lock-free state counts its parker as a sleeper before the permit
    /// check: wait for that, then let it reach the wait itself.
    fn wait_until_sleeping(ps: &ParkState) {
        while ps.sleeper_count() == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    /// An untimed and a long timed park are each ended by an `unpark`, on a
    /// plain state and on a lock-free one, and consume the permit.
    #[test]
    fn an_unpolled_park_is_ended_by_an_unpark() {
        for lock_free in [false, true] {
            for timed in [false, true] {
                let ps = Arc::new(ParkState::new());
                if lock_free {
                    ps.enable_lock_free();
                }
                let flag = Arc::new(AtomicBool::new(false));
                let timeout = timed.then_some(Duration::from_secs(30));
                let rx = spawn_parker(&ps, &flag, timeout);
                if lock_free {
                    wait_until_sleeping(&ps);
                } else {
                    std::thread::sleep(Duration::from_millis(20));
                }
                ps.unpark_lock_free();
                let waited = rx.recv_timeout(BOUND).expect("the unpark ended the park");
                assert!(waited < BOUND, "lock_free={lock_free} timed={timed}");
                assert!(
                    !ps.take_permit(),
                    "lock_free={lock_free} timed={timed}: consumed"
                );
            }
        }
    }

    /// An interrupt that raises the flag and calls `wake_for_interrupt` --
    /// no permit -- ends an untimed and a long timed park, and leaves no
    /// permit behind.
    #[test]
    fn an_unpolled_park_is_ended_by_an_interrupt_wake() {
        for timed in [false, true] {
            let ps = Arc::new(ParkState::new());
            ps.enable_lock_free();
            let flag = Arc::new(AtomicBool::new(false));
            let rx = spawn_parker(&ps, &flag, timed.then_some(Duration::from_secs(30)));
            wait_until_sleeping(&ps);
            flag.store(true, Ordering::SeqCst);
            ps.wake_for_interrupt();
            rx.recv_timeout(BOUND)
                .expect("the interrupt wake ended the park");
            assert!(
                !ps.take_permit(),
                "timed={timed}: an interrupt gives no permit"
            );
        }
    }

    /// A wake that finds neither the flag nor the permit (a stale
    /// `wake_for_interrupt`) does not end the park; the next unpark does.
    #[test]
    fn a_stale_interrupt_wake_does_not_end_an_unpolled_park() {
        let ps = Arc::new(ParkState::new());
        ps.enable_lock_free();
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_parker(&ps, &flag, None);
        wait_until_sleeping(&ps);
        ps.wake_for_interrupt();
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "nothing to wake for: still parked"
        );
        ps.unpark();
        rx.recv_timeout(BOUND).expect("the unpark ended the park");
    }

    /// A timed park ends on its deadline; a zero timeout, a permit already
    /// set or a flag already raised return at once; a timeout past
    /// `Instant`'s range is an untimed park.
    #[test]
    fn an_unpolled_timed_park_keeps_its_deadline_and_its_fast_paths() {
        let flag = Arc::new(AtomicBool::new(false));
        let ps = Arc::new(ParkState::new());
        let rx = spawn_parker(&ps, &flag, Some(Duration::from_millis(30)));
        let waited = rx.recv_timeout(BOUND).expect("the deadline ended the park");
        assert!(
            waited >= Duration::from_millis(29),
            "not early: {:?}",
            waited
        );

        ps.park_interruptible_unpolled(Some(Duration::ZERO), &flag);
        ps.unpark();
        ps.park_interruptible_unpolled(None, &flag);
        assert!(!ps.take_permit(), "the permit fast path consumed it");
        let raised = AtomicBool::new(true);
        ps.park_interruptible_unpolled(None, &raised);
        assert!(
            raised.load(Ordering::SeqCst),
            "a park never clears the flag"
        );

        let rx = spawn_parker(&ps, &flag, Some(Duration::MAX));
        std::thread::sleep(Duration::from_millis(20));
        ps.unpark();
        rx.recv_timeout(BOUND)
            .expect("an unbounded timeout is still woken");
    }
}

/// Interpreter round i3 wave 75, lane L6: `ParkState::sleep_unpolled`
/// (`CRATONVM_SLEEP_NO_POLL`) and `ParkState::unpark_waking_all`
/// (`CRATONVM_PARK_UNTIMED_NO_POLL`). Every wait is bounded by a channel
/// timeout, so a lost wake fails the test instead of hanging it.
#[cfg(test)]
mod sleep_unpolled_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    const BOUND: Duration = Duration::from_secs(10);

    /// Sleep on `ps` for `dur` on a new thread; the receiver gets whether
    /// the sleep saw the flag and how long it waited.
    fn spawn_sleeper(
        ps: &Arc<ParkState>,
        flag: &Arc<AtomicBool>,
        dur: Duration,
    ) -> mpsc::Receiver<(bool, Duration)> {
        let (tx, rx) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let interrupted = ps2.sleep_unpolled(dur, &flag2);
            let _ = tx.send((interrupted, t0.elapsed()));
        });
        rx
    }

    /// A sleep with nothing to wake it lasts its whole duration (not early)
    /// and reports no interrupt; a flag already raised returns at once.
    #[test]
    fn an_unpolled_sleep_keeps_its_duration_and_its_flag_fast_path() {
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_sleeper(&ps, &flag, Duration::from_millis(30));
        let (interrupted, waited) = rx.recv_timeout(BOUND).expect("the sleep ended");
        assert!(!interrupted);
        assert!(waited >= Duration::from_millis(29), "not early: {waited:?}");

        let raised = AtomicBool::new(true);
        assert!(ps.sleep_unpolled(Duration::from_secs(30), &raised));
        assert!(
            raised.load(Ordering::SeqCst),
            "a sleep's wait never clears the flag"
        );
    }

    /// An interrupt (the flag, then `wake_for_interrupt`, as
    /// `ThreadRegistry::set_interrupted` does) ends a long sleep at once, on
    /// a plain state and on a lock-free one, and leaves no permit.
    #[test]
    fn an_unpolled_sleep_is_ended_by_an_interrupt_wake() {
        for lock_free in [false, true] {
            let ps = Arc::new(ParkState::new());
            if lock_free {
                ps.enable_lock_free();
            }
            let flag = Arc::new(AtomicBool::new(false));
            let rx = spawn_sleeper(&ps, &flag, Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(20));
            flag.store(true, Ordering::SeqCst);
            ps.wake_for_interrupt();
            let (interrupted, waited) = rx
                .recv_timeout(BOUND)
                .expect("the interrupt wake ended the sleep");
            assert!(interrupted, "lock_free={lock_free}");
            assert!(waited < BOUND, "lock_free={lock_free}");
            assert!(
                !ps.take_permit(),
                "lock_free={lock_free}: an interrupt gives no permit"
            );
        }
    }

    /// An `unpark` does not end a sleep, and its permit is left for the
    /// thread's next park (HotSpot's separate `_SleepEvent`).
    #[test]
    fn an_unpark_neither_ends_an_unpolled_sleep_nor_is_consumed_by_it() {
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_sleeper(&ps, &flag, Duration::from_millis(300));
        std::thread::sleep(Duration::from_millis(20));
        ps.unpark();
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "an unpark does not end a sleep"
        );
        let (interrupted, waited) = rx.recv_timeout(BOUND).expect("the sleep ended");
        assert!(!interrupted);
        assert!(
            waited >= Duration::from_millis(299),
            "slept out: {waited:?}"
        );
        assert!(
            ps.take_permit(),
            "the permit is still there for the next park"
        );
    }

    /// A duration past `Instant`'s range sleeps until interrupted.
    #[test]
    fn an_unbounded_unpolled_sleep_is_still_interruptible() {
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_sleeper(&ps, &flag, Duration::MAX);
        std::thread::sleep(Duration::from_millis(20));
        flag.store(true, Ordering::SeqCst);
        ps.wake_for_interrupt();
        let (interrupted, _) = rx.recv_timeout(BOUND).expect("woken");
        assert!(interrupted);
    }

    /// `unpark_waking_all` (and its lock-free twin) ends a park even when
    /// another thread waits on the same condvar (here a sleep, which ignores
    /// the permit): every waiter is woken, the parker takes the permit, the
    /// sleeper waits on.
    #[test]
    fn unpark_waking_all_reaches_the_parker_beside_another_waiter() {
        for lock_free in [false, true] {
            let ps = Arc::new(ParkState::new());
            if lock_free {
                ps.enable_lock_free();
            }
            let flag = Arc::new(AtomicBool::new(false));
            let sleeper = spawn_sleeper(&ps, &flag, Duration::from_secs(60));
            let (tx, parked) = mpsc::channel();
            let (ps2, flag2) = (ps.clone(), flag.clone());
            std::thread::spawn(move || {
                ps2.park_interruptible_unpolled(None, &flag2);
                let _ = tx.send(());
            });
            if lock_free {
                while ps.sleeper_count() == 0 {
                    std::thread::yield_now();
                }
            }
            std::thread::sleep(Duration::from_millis(20));
            if lock_free {
                ps.unpark_lock_free_waking_all();
            } else {
                ps.unpark_waking_all();
            }
            parked.recv_timeout(BOUND).expect("the park ended");
            assert!(
                !ps.take_permit(),
                "lock_free={lock_free}: the park consumed the permit"
            );
            assert!(
                sleeper.recv_timeout(Duration::from_millis(50)).is_err(),
                "lock_free={lock_free}: the sleep waits on"
            );
            flag.store(true, Ordering::SeqCst);
            ps.wake_for_interrupt();
            let (interrupted, _) = sleeper.recv_timeout(BOUND).expect("woken");
            assert!(interrupted);
        }
    }
}

/// Interpreter round i3 wave 76, lane L6 (`CRATONVM_PARK_WAKE_AFTER_UNLOCK`):
/// `ParkState::unpark_after_unlock`, its lock-free twin,
/// `interrupt_wake_after_unlock` and `wake_for_interrupt_after_unlock`. Every
/// wait is bounded by a channel timeout, so a lost wake fails the test
/// instead of hanging it.
#[cfg(test)]
mod wake_after_unlock_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    const BOUND: Duration = Duration::from_secs(10);

    fn spawn_parker(
        ps: &Arc<ParkState>,
        flag: &Arc<AtomicBool>,
        polled: bool,
        timeout: Option<Duration>,
    ) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            if polled {
                ps2.park_interruptible(timeout, &flag2);
            } else {
                ps2.park_interruptible_unpolled(timeout, &flag2);
            }
            let _ = tx.send(());
        });
        rx
    }

    fn spawn_sleeper(
        ps: &Arc<ParkState>,
        flag: &Arc<AtomicBool>,
        dur: Duration,
    ) -> mpsc::Receiver<bool> {
        let (tx, rx) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            let _ = tx.send(ps2.sleep_unpolled(dur, &flag2));
        });
        rx
    }

    /// Let a spawned parker reach its wait (a lock-free state counts it a
    /// sleeper first).
    fn settle(ps: &ParkState, lock_free: bool) {
        if lock_free {
            while ps.sleeper_count() == 0 {
                std::thread::yield_now();
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    /// The after-unlock unpark ends an untimed and a long timed park, polled
    /// and unpolled, on a plain and a lock-free state, and the park consumes
    /// the permit.
    #[test]
    fn an_after_unlock_unpark_ends_every_park_shape() {
        for lock_free in [false, true] {
            for polled in [false, true] {
                for timed in [false, true] {
                    let ps = Arc::new(ParkState::new());
                    if lock_free {
                        ps.enable_lock_free();
                    }
                    let flag = Arc::new(AtomicBool::new(false));
                    let timeout = timed.then_some(Duration::from_secs(30));
                    let rx = spawn_parker(&ps, &flag, polled, timeout);
                    settle(&ps, lock_free);
                    if lock_free {
                        ps.unpark_lock_free_after_unlock();
                    } else {
                        ps.unpark_after_unlock();
                    }
                    rx.recv_timeout(BOUND).unwrap_or_else(|_| {
                        panic!("lock_free={lock_free} polled={polled} timed={timed}: not woken")
                    });
                    assert!(
                        !ps.take_permit(),
                        "lock_free={lock_free} polled={polled} timed={timed}: consumed"
                    );
                }
            }
        }
    }

    /// Two unparks before the park leave ONE permit (the second finds it set
    /// and wakes nobody): the park takes it at once, the next park waits.
    #[test]
    fn an_after_unlock_unpark_of_a_set_permit_is_one_permit() {
        for lock_free in [false, true] {
            let ps = Arc::new(ParkState::new());
            if lock_free {
                ps.enable_lock_free();
            }
            let flag = Arc::new(AtomicBool::new(false));
            if lock_free {
                ps.unpark_lock_free_after_unlock();
                ps.unpark_lock_free_after_unlock();
            } else {
                ps.unpark_after_unlock();
                ps.unpark_after_unlock();
            }
            ps.park_interruptible_unpolled(None, &flag);
            let rx = spawn_parker(&ps, &flag, false, None);
            assert!(
                rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "lock_free={lock_free}: one permit only"
            );
            ps.unpark_after_unlock();
            rx.recv_timeout(BOUND).expect("woken");
        }
    }

    /// Back-to-back unparks of a parked thread end the park; at most one
    /// permit is left once the parker has run.
    #[test]
    fn two_after_unlock_unparks_end_the_park_once() {
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_parker(&ps, &flag, false, None);
        settle(&ps, false);
        ps.unpark_after_unlock();
        ps.unpark_after_unlock();
        rx.recv_timeout(BOUND).expect("woken");
        let _ = ps.take_permit();
        assert!(!ps.take_permit(), "a permit is one bit");
    }

    /// `interrupt_wake_after_unlock` (the flag raised first, as
    /// `Thread.interrupt` does) ends an unpolled park and a sleep -- even a
    /// sleep with a leftover permit, which the unpark-shaped wake would skip
    /// -- and leaves the permit set for the sleeper's next park.
    #[test]
    fn an_interrupt_wake_ends_a_park_and_a_sleep_with_a_leftover_permit() {
        for lock_free in [false, true] {
            let ps = Arc::new(ParkState::new());
            if lock_free {
                ps.enable_lock_free();
            }
            let flag = Arc::new(AtomicBool::new(false));
            let rx = spawn_parker(&ps, &flag, false, None);
            settle(&ps, lock_free);
            flag.store(true, Ordering::SeqCst);
            ps.interrupt_wake_after_unlock();
            rx.recv_timeout(BOUND)
                .expect("the interrupt wake ended the park");
            let _ = ps.take_permit();

            flag.store(false, Ordering::SeqCst);
            ps.unpark_after_unlock();
            let sleeper = spawn_sleeper(&ps, &flag, Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(20));
            flag.store(true, Ordering::SeqCst);
            ps.interrupt_wake_after_unlock();
            assert!(
                sleeper
                    .recv_timeout(BOUND)
                    .expect("the interrupt wake ended the sleep"),
                "lock_free={lock_free}: the sleep saw the flag"
            );
            assert!(
                ps.take_permit(),
                "lock_free={lock_free}: the permit is left"
            );
        }
    }

    /// `interrupt_wake_locked` (interpreter round i4 wave 91, lane L6,
    /// `CRATONVM_INTERRUPT_ONE_WAKE`), the flag raised first: ends an unpolled
    /// park and a sleep with a leftover permit, and leaves the permit set, as
    /// `interrupt_wake_after_unlock` does.
    #[test]
    fn i91_l6_a_locked_interrupt_wake_ends_a_park_and_a_sleep() {
        for lock_free in [false, true] {
            let ps = Arc::new(ParkState::new());
            if lock_free {
                ps.enable_lock_free();
            }
            let flag = Arc::new(AtomicBool::new(false));
            let rx = spawn_parker(&ps, &flag, false, None);
            settle(&ps, lock_free);
            flag.store(true, Ordering::SeqCst);
            ps.interrupt_wake_locked();
            rx.recv_timeout(BOUND)
                .expect("the locked interrupt wake ended the park");
            let _ = ps.take_permit();

            flag.store(false, Ordering::SeqCst);
            ps.unpark_after_unlock();
            let sleeper = spawn_sleeper(&ps, &flag, Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(20));
            flag.store(true, Ordering::SeqCst);
            ps.interrupt_wake_locked();
            assert!(
                sleeper
                    .recv_timeout(BOUND)
                    .expect("the locked interrupt wake ended the sleep"),
                "lock_free={lock_free}: the sleep saw the flag"
            );
            assert!(
                ps.take_permit(),
                "lock_free={lock_free}: the permit is left"
            );
        }
    }

    /// `wake_for_interrupt_after_unlock` gives no permit: with no flag raised
    /// the park waits on; with the flag it ends.
    #[test]
    fn a_flag_wake_after_unlock_gives_no_permit() {
        let ps = Arc::new(ParkState::new());
        ps.enable_lock_free();
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_parker(&ps, &flag, false, None);
        settle(&ps, true);
        ps.wake_for_interrupt_after_unlock();
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "a stale wake does not end the park"
        );
        flag.store(true, Ordering::SeqCst);
        ps.wake_for_interrupt_after_unlock();
        rx.recv_timeout(BOUND).expect("woken by the flag");
        assert!(!ps.take_permit(), "no permit given");
    }
}

/// Interpreter round i3 wave 77, lane L6 (page
/// `i76-L6-proposal-one-park-wait-and-one-unpark-20261109`): the one wait
/// (`ParkState::park_wait`) and the one unpark (`ParkState::unpark_with`,
/// `ParkState::interrupt_unpark`) in every switch combination. Every wait is
/// bounded by a channel timeout, so a lost wake fails the test instead of
/// hanging it.
#[cfg(test)]
mod one_wait_one_unpark_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    const BOUND: Duration = Duration::from_secs(10);

    fn shapes() -> Vec<UnparkShape> {
        let mut all = Vec::new();
        for lock_free in [false, true] {
            for wake_all in [false, true] {
                for after_unlock in [false, true] {
                    all.push(UnparkShape {
                        lock_free,
                        wake_all,
                        after_unlock,
                    });
                }
            }
        }
        all
    }

    fn spawn_waiter(
        ps: &Arc<ParkState>,
        flag: &Arc<AtomicBool>,
        polled: bool,
        timeout: Option<Duration>,
    ) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            ps2.park_wait(timeout, &flag2, polled);
            let _ = tx.send(());
        });
        rx
    }

    fn settle(ps: &ParkState, lock_free_state: bool) {
        if lock_free_state {
            while ps.sleeper_count() == 0 {
                std::thread::yield_now();
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    /// Every unpark shape ends every wait shape -- polled and unpolled,
    /// untimed and long-timed, on a plain and a lock-free state -- and the
    /// wait consumes the permit.
    #[test]
    fn every_unpark_shape_ends_every_wait_shape() {
        for shape in shapes() {
            for lock_free_state in [false, true] {
                for polled in [false, true] {
                    for timed in [false, true] {
                        let ps = Arc::new(ParkState::new());
                        if lock_free_state {
                            ps.enable_lock_free();
                        }
                        let flag = Arc::new(AtomicBool::new(false));
                        let timeout = timed.then_some(Duration::from_secs(30));
                        let rx = spawn_waiter(&ps, &flag, polled, timeout);
                        settle(&ps, lock_free_state);
                        ps.unpark_with(shape);
                        rx.recv_timeout(BOUND).unwrap_or_else(|_| {
                            panic!(
                                "{shape:?} lock_free_state={lock_free_state} polled={polled} \
                                 timed={timed}: not woken"
                            )
                        });
                        assert!(
                            !ps.take_permit(),
                            "{shape:?} lock_free_state={lock_free_state} polled={polled}: consumed"
                        );
                    }
                }
            }
        }
    }

    /// An unpark before the wait, in every shape, leaves the permit the
    /// wait takes at once; a second one leaves no second permit.
    #[test]
    fn every_unpark_shape_leaves_one_permit_for_a_later_wait() {
        for shape in shapes() {
            for lock_free_state in [false, true] {
                let ps = Arc::new(ParkState::new());
                if lock_free_state {
                    ps.enable_lock_free();
                }
                let flag = Arc::new(AtomicBool::new(false));
                ps.unpark_with(shape);
                ps.unpark_with(shape);
                ps.park_wait(None, &flag, false);
                assert!(
                    !ps.take_permit(),
                    "{shape:?} lock_free_state={lock_free_state}: a permit is one bit"
                );
            }
        }
    }

    /// `interrupt_unpark` after the flag is raised ends a wait in every
    /// shape and leaves the permit set for a park that has not begun yet.
    #[test]
    fn every_interrupt_unpark_shape_ends_a_wait_and_leaves_the_permit() {
        for shape in shapes() {
            for polled in [false, true] {
                let ps = Arc::new(ParkState::new());
                let flag = Arc::new(AtomicBool::new(false));
                let rx = spawn_waiter(&ps, &flag, polled, None);
                settle(&ps, false);
                flag.store(true, Ordering::SeqCst);
                ps.interrupt_unpark(shape);
                rx.recv_timeout(BOUND).unwrap_or_else(|_| {
                    panic!("{shape:?} polled={polled}: the interrupt did not end the wait")
                });

                let fresh = ParkState::new();
                fresh.interrupt_unpark(shape);
                assert!(
                    fresh.take_permit(),
                    "{shape:?}: the interrupt's unpark sets the permit"
                );
            }
        }
    }

    /// The flag-only wake, both bodies: no permit; a raised flag ends the
    /// wait, a stale wake does not.
    #[test]
    fn the_flag_wake_gives_no_permit_either_way() {
        for after_unlock in [false, true] {
            let ps = Arc::new(ParkState::new());
            let flag = Arc::new(AtomicBool::new(false));
            let rx = spawn_waiter(&ps, &flag, false, None);
            settle(&ps, false);
            ps.wake_for_interrupt_with(after_unlock);
            assert!(
                rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "after_unlock={after_unlock}: a stale wake does not end the wait"
            );
            flag.store(true, Ordering::SeqCst);
            ps.wake_for_interrupt_with(after_unlock);
            rx.recv_timeout(BOUND).expect("woken by the flag");
            assert!(
                !ps.take_permit(),
                "after_unlock={after_unlock}: no permit given"
            );
        }
    }

    /// The wait's own exits, polled and not: a zero timeout and a raised flag
    /// return at once without consuming a permit that is not there; a short
    /// timed wait returns after (about) its deadline.
    #[test]
    fn park_wait_returns_on_zero_timeout_flag_and_deadline() {
        for polled in [false, true] {
            let ps = ParkState::new();
            let down = AtomicBool::new(false);
            let up = AtomicBool::new(true);
            let t0 = std::time::Instant::now();
            ps.park_wait(Some(Duration::ZERO), &down, polled);
            ps.park_wait(None, &up, polled);
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "polled={polled}: returned at once"
            );
            let t1 = std::time::Instant::now();
            ps.park_wait(Some(Duration::from_millis(12)), &down, polled);
            assert!(
                t1.elapsed() >= Duration::from_millis(10),
                "polled={polled}: waited out the deadline"
            );
            assert!(!ps.take_permit(), "polled={polled}: no permit appeared");
        }
    }
}

/// Interpreter round i5 wave 107, lane L6 (`CRATONVM_PARK_UNTIMED_EVENT`):
/// the untimed event wait of `ParkState::park_wait_with` on Windows. Every
/// unpark shape and the interrupt wakes end it, a stale flag wake does not,
/// the permit is consumed on the way out, and no signal is left on the event
/// for the thread's next timed park.
#[cfg(all(test, target_os = "windows"))]
mod untimed_event_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    const BOUND: Duration = Duration::from_secs(10);

    fn spawn_event_parker(ps: &Arc<ParkState>, flag: &Arc<AtomicBool>) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            ps2.park_wait_with(None, &flag2, false, true);
            let _ = tx.send(());
        });
        rx
    }

    /// Let the parker reach its wait (the event waiter flag goes up under
    /// the mutex, right before the wait).
    fn wait_until_on_event(ps: &ParkState) {
        let t0 = std::time::Instant::now();
        while !ps.event_waiter.load(Ordering::Relaxed) && t0.elapsed() < BOUND {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    #[test]
    fn every_unpark_shape_ends_an_untimed_event_wait() {
        for lock_free in [false, true] {
            for wake_all in [false, true] {
                for after_unlock in [false, true] {
                    let shape = UnparkShape {
                        lock_free,
                        wake_all,
                        after_unlock,
                    };
                    let ps = Arc::new(ParkState::new());
                    if lock_free {
                        ps.enable_lock_free();
                    }
                    let flag = Arc::new(AtomicBool::new(false));
                    let rx = spawn_event_parker(&ps, &flag);
                    wait_until_on_event(&ps);
                    ps.unpark_with(shape);
                    rx.recv_timeout(BOUND)
                        .unwrap_or_else(|_| panic!("{shape:?}: the unpark did not end the wait"));
                    assert!(!ps.take_permit(), "{shape:?}: the wait consumed the permit");
                    assert!(
                        !ps.wake_event_left_signalled(),
                        "{shape:?}: no signal left on the event"
                    );
                }
            }
        }
    }

    #[test]
    fn interrupt_wakes_end_an_untimed_event_wait_and_a_stale_one_does_not() {
        for after_unlock in [false, true] {
            let ps = Arc::new(ParkState::new());
            let flag = Arc::new(AtomicBool::new(false));
            let rx = spawn_event_parker(&ps, &flag);
            wait_until_on_event(&ps);
            ps.wake_for_interrupt_with(after_unlock);
            assert!(
                rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "after_unlock={after_unlock}: a stale wake does not end the wait"
            );
            wait_until_on_event(&ps);
            flag.store(true, Ordering::SeqCst);
            ps.wake_for_interrupt_with(after_unlock);
            rx.recv_timeout(BOUND).expect("woken by the flag");
            assert!(!ps.take_permit(), "after_unlock={after_unlock}: no permit given");
            assert!(!ps.wake_event_left_signalled());
        }
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_event_parker(&ps, &flag);
        wait_until_on_event(&ps);
        flag.store(true, Ordering::SeqCst);
        ps.interrupt_wake_locked();
        rx.recv_timeout(BOUND).expect("woken by the one-trip interrupt wake");
    }

    /// A permit already set returns at once, without touching the event.
    #[test]
    fn a_set_permit_skips_the_event_wait() {
        let ps = ParkState::new();
        let flag = AtomicBool::new(false);
        ps.unpark();
        ps.park_wait_with(None, &flag, false, true);
        assert!(!ps.take_permit());
        assert!(!ps.wake_event_left_signalled());
    }

    /// Interpreter round i5 wave 108, lane L6
    /// (`CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK`): a timed coarse wait whose
    /// waker raises the event after its unlock is ended by every unpark shape
    /// and by the one-trip interrupt wake, consumes the permit, and leaves no
    /// signal behind; a wait that times out leaves none either.
    #[test]
    fn a_deferred_signal_ends_a_timed_wait_and_leaves_nothing() {
        for after_unlock in [false, true] {
            let shape = UnparkShape {
                after_unlock,
                ..UnparkShape::LOCKED
            };
            let ps = Arc::new(ParkState::new());
            let flag = Arc::new(AtomicBool::new(false));
            let (tx, rx) = mpsc::channel();
            let (ps2, flag2) = (ps.clone(), flag.clone());
            std::thread::spawn(move || {
                ps2.park_wait_once_with(Duration::from_secs(30), &flag2, true);
                let _ = tx.send(());
            });
            wait_until_on_event(&ps);
            ps.unpark_with(shape);
            rx.recv_timeout(BOUND)
                .unwrap_or_else(|_| panic!("{shape:?}: the unpark did not end the wait"));
            assert!(!ps.take_permit(), "{shape:?}: the wait consumed the permit");
            assert!(!ps.wake_event_left_signalled(), "{shape:?}: no signal left");
        }
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            ps2.park_wait_once_with(Duration::from_secs(30), &flag2, true);
            let _ = tx.send(());
        });
        wait_until_on_event(&ps);
        flag.store(true, Ordering::SeqCst);
        ps.interrupt_wake_locked();
        rx.recv_timeout(BOUND).expect("woken by the one-trip interrupt wake");
        assert!(!ps.wake_event_left_signalled());
        let ps = ParkState::new();
        let flag = AtomicBool::new(false);
        ps.park_wait_once_with(Duration::from_millis(2), &flag, true);
        assert!(!ps.wake_event_left_signalled());
    }
}

/// Interpreter round i5 wave 114, lane L1 (`CRATONVM_PARK_EVENT_LOCKLESS_WAKE`):
/// the lockless event waits of `ParkState::park_wait_once_lockless` and
/// `park_wait_untimed_lockless` on Windows. Every unpark shape and the
/// interrupt wakes end them, a stale flag wake does not end the untimed one,
/// the permit is consumed on the way out, and no signal is left on the event
/// -- also when unparks race the timed wait's timeout.
#[cfg(all(test, target_os = "windows"))]
mod lockless_wake_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    const BOUND: Duration = Duration::from_secs(10);

    /// Let the parker reach its wait (the slot goes to waiting under the
    /// mutex, right before the mutex is released and the wait begins).
    fn wait_until_lockless_waiting(ps: &ParkState) {
        let t0 = std::time::Instant::now();
        while ps.lockless_slot.load(Ordering::Acquire) != LOCKLESS_WAITING && t0.elapsed() < BOUND
        {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    fn spawn_lockless_parker(
        ps: &Arc<ParkState>,
        flag: &Arc<AtomicBool>,
        timed: bool,
    ) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let (ps2, flag2) = (ps.clone(), flag.clone());
        std::thread::spawn(move || {
            if timed {
                ps2.park_wait_once_lockless(Duration::from_secs(30), &flag2);
            } else {
                ps2.park_wait_untimed_lockless(&flag2);
            }
            let _ = tx.send(());
        });
        rx
    }

    #[test]
    fn every_unpark_shape_ends_a_lockless_wait() {
        for timed in [true, false] {
            for lock_free in [false, true] {
                for wake_all in [false, true] {
                    for after_unlock in [false, true] {
                        let shape = UnparkShape {
                            lock_free,
                            wake_all,
                            after_unlock,
                        };
                        let ps = Arc::new(ParkState::new());
                        if lock_free {
                            ps.enable_lock_free();
                        }
                        let flag = Arc::new(AtomicBool::new(false));
                        let rx = spawn_lockless_parker(&ps, &flag, timed);
                        wait_until_lockless_waiting(&ps);
                        ps.unpark_with(shape);
                        rx.recv_timeout(BOUND).unwrap_or_else(|_| {
                            panic!("timed={timed} {shape:?}: the unpark did not end the wait")
                        });
                        assert!(!ps.take_permit(), "timed={timed} {shape:?}: permit consumed");
                        assert!(
                            !ps.wake_event_left_signalled(),
                            "timed={timed} {shape:?}: no signal left on the event"
                        );
                        assert_eq!(ps.lockless_slot.load(Ordering::Acquire), LOCKLESS_IDLE);
                    }
                }
            }
        }
    }

    #[test]
    fn interrupt_wakes_end_a_lockless_wait_and_a_stale_one_does_not() {
        for after_unlock in [false, true] {
            let ps = Arc::new(ParkState::new());
            let flag = Arc::new(AtomicBool::new(false));
            let rx = spawn_lockless_parker(&ps, &flag, false);
            wait_until_lockless_waiting(&ps);
            ps.wake_for_interrupt_with(after_unlock);
            assert!(
                rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "after_unlock={after_unlock}: a stale wake does not end the wait"
            );
            wait_until_lockless_waiting(&ps);
            flag.store(true, Ordering::SeqCst);
            ps.wake_for_interrupt_with(after_unlock);
            rx.recv_timeout(BOUND).expect("woken by the flag");
            assert!(!ps.take_permit(), "after_unlock={after_unlock}: no permit given");
            assert!(!ps.wake_event_left_signalled());
        }
        for timed in [true, false] {
            let ps = Arc::new(ParkState::new());
            let flag = Arc::new(AtomicBool::new(false));
            let rx = spawn_lockless_parker(&ps, &flag, timed);
            wait_until_lockless_waiting(&ps);
            flag.store(true, Ordering::SeqCst);
            ps.interrupt_wake_locked();
            rx.recv_timeout(BOUND).expect("woken by the one-trip interrupt wake");
            assert!(!ps.wake_event_left_signalled(), "timed={timed}: no signal left");
        }
    }

    /// A permit already set, a raised flag, or a timeout end the wait at once
    /// or on time, and leave nothing on the event.
    #[test]
    fn a_set_permit_a_flag_and_a_timeout_leave_nothing() {
        let ps = ParkState::new();
        let flag = AtomicBool::new(false);
        ps.unpark();
        ps.park_wait_untimed_lockless(&flag);
        assert!(!ps.take_permit());
        ps.unpark();
        ps.park_wait_once_lockless(Duration::from_secs(30), &flag);
        assert!(!ps.take_permit());
        let raised = AtomicBool::new(true);
        ps.park_wait_untimed_lockless(&raised);
        ps.park_wait_once_lockless(Duration::from_secs(30), &raised);
        let t0 = std::time::Instant::now();
        ps.park_wait_once_lockless(Duration::from_millis(2), &flag);
        assert!(t0.elapsed() < BOUND);
        assert!(!ps.wake_event_left_signalled());
        assert_eq!(ps.lockless_slot.load(Ordering::Acquire), LOCKLESS_IDLE);
    }

    /// Unparks racing short timed waits: a claim the wait did not consume
    /// (it ended on its timeout first) is reset, so once both sides stop no
    /// signal is left for a later wait (the wave-73 rule).
    #[test]
    fn unparks_racing_the_timeout_leave_no_signal() {
        let ps = Arc::new(ParkState::new());
        let stop = Arc::new(AtomicBool::new(false));
        let (ps2, stop2) = (ps.clone(), stop.clone());
        let waker = std::thread::spawn(move || {
            while !stop2.load(Ordering::Acquire) {
                ps2.unpark();
                std::hint::spin_loop();
            }
        });
        let flag = AtomicBool::new(false);
        for _ in 0..300 {
            ps.park_wait_once_lockless(Duration::from_millis(1), &flag);
        }
        stop.store(true, Ordering::Release);
        waker.join().expect("the waker");
        assert_eq!(ps.lockless_slot.load(Ordering::Acquire), LOCKLESS_IDLE);
        assert!(!ps.wake_event_left_signalled(), "no signal left behind");
    }

    /// Interpreter round i6 wave 115, lane L1
    /// (`CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT`): a wait announced with
    /// `announce_event_waiter` (a process wait's second handle) is signalled
    /// by every interrupt wake and by an unpark, keeps the permit an unpark
    /// gave, and a claim it did not consume is reset by the withdraw.
    #[test]
    fn an_announced_event_waiter_is_signalled_and_leaves_nothing() {
        let ps = ParkState::new();
        let Some(ev) = ps.announce_event_waiter() else {
            return; // no event on this host (`CRATONVM_WIN_HIRES_PARK=0`)
        };
        ps.wake_for_interrupt_with(false);
        assert_eq!(win_park::wait_event_coarse(ev, 1_000), win_park::Woke::Event);
        ps.withdraw_event_waiter(true);
        assert!(!ps.wake_event_left_signalled());
        assert!(!ps.take_permit(), "a flag wake gives no permit");

        let _ = ps.announce_event_waiter();
        ps.wake_for_interrupt_with(true);
        assert_eq!(win_park::wait_event_coarse(ev, 1_000), win_park::Woke::Event);
        ps.withdraw_event_waiter(true);

        // Claimed, not consumed (the wait ended on the process handle).
        let _ = ps.announce_event_waiter();
        ps.interrupt_wake_locked();
        ps.withdraw_event_waiter(false);
        assert!(!ps.wake_event_left_signalled(), "the unconsumed claim is reset");
        assert!(ps.take_permit(), "the interrupt's unpark left its permit");

        // An unpark wakes the wait and its permit stays for the next park.
        let _ = ps.announce_event_waiter();
        ps.unpark();
        assert_eq!(win_park::wait_event_coarse(ev, 1_000), win_park::Woke::Event);
        ps.withdraw_event_waiter(true);
        assert!(ps.take_permit(), "the permit is left");

        // Withdrawn: a later wake signals nothing.
        ps.wake_for_interrupt_with(false);
        assert!(!ps.wake_event_left_signalled());
        assert_eq!(ps.lockless_slot.load(Ordering::Acquire), LOCKLESS_IDLE);
    }
}
