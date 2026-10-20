// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Structured locking (JVMS §2.11.10): the monitors an interpreted frame's own
//! `monitorenter`s hold, and what the interpreter does with them.
//!
//! HotSpot's interpreter keeps a monitor block per frame and enforces the
//! structured-locking rules from it; this is CratonVM's copy of that record
//! (interpreter round i1 wave 23, lane L7; page
//! `interpreter-L7-unstructured-locking-is-not-detected`):
//!
//! * `monitorexit` releases an object only through the record of the frame
//!   that executes it. An object the frame did not enter — even one the thread
//!   owns through a CALLER's frame — is `IllegalMonitorStateException` with a
//!   null message ([`monitorexit_unrecorded`]).
//! * A return with an entry still recorded throws `IllegalMonitorStateException`
//!   at the return's bci ([`return_with_held_monitors`]); like HotSpot's
//!   `remove_activation(throw_monitor_exception = true)` it does not unlock, so
//!   a handler of the method that covers the return still sees the lock held.
//! * Exception unwinding out of a frame releases every entry still recorded
//!   and replaces the in-flight throwable with a new
//!   `IllegalMonitorStateException` ([`replace_unwound_exception_if_locked`],
//!   HotSpot's `InterpreterRuntime::new_illegal_monitor_state_exception`).
//! * Every other removal of a frame (`pop_and_recycle_frame_with_reason`)
//!   releases what the record still holds — the JVMTI `PopFrame` rule, and the
//!   safety net for the paths that leave `execute_frame_from_index` through a
//!   VM error rather than a Java throwable.
//!
//! # The record may under-report, never over-report
//!
//! Only `op_monitorenter` adds an entry, only this frame's `monitorexit` (or
//! the rules above) removes one, and entering OSR'd code clears the record
//! (`try_osr_with_backoff`: from there on the compiled body owns the frame's
//! monitors and may release them itself). A frame the interpreter RESUMES
//! mid-method — a deoptimized frame, an OSR body that exited back into its
//! frame — holds monitors compiled code acquired and no record names. So a
//! `monitorexit` that misses the record is not trusted blindly: when this
//! thread holds the object more often than every frame's record (and
//! `monitor_on_exit`) accounts for, the unaccounted acquisition is the one being
//! released, and it is released as before this record existed. Only a miss the
//! monitor table cannot explain throws. A false `IllegalMonitorStateException`
//! on javac's structured shape would be a hang (its catch-any handler covers
//! its own `monitorexit`), so every direction of doubt resolves to the
//! pre-record behaviour.
//!
//! For the same reason an entry whose object this thread does not hold as
//! often as the records claim is STALE (a relocation a remap site missed, a
//! release made on the frame's behalf) and is dropped, with a debug line,
//! instead of being reported ([`prune_stale`]).
//!
//! # Cost
//!
//! The record is an inline two-slot `SmallVec`: no allocation for up to two
//! nested block monitors in one frame, a push on `monitorenter`, a compare and
//! pop on `monitorexit`, and one `is_empty` test on each return and frame pop.
//! Everything else here is `#[cold]`.
//!
//! # GC
//!
//! The entries are object references: every per-thread root scan and remap
//! that visits `Frame::monitor_on_exit` visits the record too
//! (`memory/roots.rs` `collect_roots`, `memory/gc.rs` `update_all_roots`,
//! `gc_and_alloc.rs`'s peer, cached, safepoint deposits and
//! `apply_pointer_map_to_thread`, and `vm_exec.rs`'s blocked deposit and both
//! wake remaps), through [`HeldMonitors::as_slice`] / [`HeldMonitors::remap_with`].

use super::*;
use crate::threading::jvm_thread::ThreadId;
use smallvec::SmallVec;

/// The objects a frame's own `monitorenter`s locked and its `monitorexit`s
/// have not released, oldest first. See the module doc.
///
/// Interpreter round i6 wave 117, lane L6 (page
/// `i116-L9-strict-monitorexit-throws-on-an-understated-record`): a record a
/// resume path emptied because the compiled exit frame's lock description is
/// not trusted (a single-pass body's, where its monitor analysis has no
/// answer) is marked UNDERSTATED ([`Self::clear_untrusted`]), so the strict
/// `monitorexit` arm ([`monitorexit_unrecorded`]) keeps the leniency for a
/// thread with such a frame. The mark is a reserved first entry
/// ([`UNDERSTATED_MARK`], never an object address), so the record keeps its
/// one-`SmallVec` layout (`Frame` is ratcheted); every accessor below skips
/// it, and the hot `is_empty` of a frame with no block monitor is unchanged.
#[derive(Debug, Default)]
pub(crate) struct HeldMonitors {
    objs: SmallVec<[ObjectRef; 2]>,
}

/// The mark entry's addresses lie in `[8, MARK_LIMIT)`: the never-mapped null
/// page, so never an object start, and each 8-aligned (`ObjectRef::from_raw`'s
/// debug alignment assertion). Interpreter round i6 wave 121, lane L6: the
/// address encodes the record's PRODUCER and the understated bit,
/// `8 * (1 + 2 * producer + understated)` ([`RecordProducer`]), so the
/// `CRATONVM_DBG_MONITOR_LENIENCY` census can name which resume path left a
/// frame's record as it is. Wave 117's single mark (address 8) is producer
/// `Unnamed`, not understated.
const MARK_LIMIT: usize = 4096;

/// Which path produced a frame's record from a compiled description
/// (interpreter round i6 wave 121, lane L6; page
/// `i116-L9-strict-monitorexit-throws-on-an-understated-record`, the producer
/// list of "Wave 119" plus the wave-120 sinks and the wave-121 materializer).
/// Carried in the record's mark entry, as provenance (a trusted seed) or with
/// the understated bit (an untrusted one); a frame the interpreter entered
/// itself carries none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RecordProducer {
    /// A mark set without a producer (tests, older callers).
    Unnamed = 0,
    /// `CompiledLocksOfAStash::replace_live_record_and_unpin`.
    OsrExitTransfer = 1,
    /// `CompiledLocksOfAStash::replace_live_record_with_retaken_levels_and_unpin`.
    PlanlessTransfer = 2,
    /// `CompiledLocksOfAStash::seed_frame_and_unpin`.
    HandlerSeed = 3,
    /// `CompiledLocksOfAStash::seed_pushed_frame_and_unpin`.
    PushedHandlerSeed = 4,
    /// `deopt_resume::build_deopt_frame_inner`.
    DeoptBuild = 5,
    /// `deopt_resume::resume_from_ir_deopt`.
    IrDeoptResume = 6,
    /// `deopt_resume::push_inlined_chain`.
    InlinedChain = 7,
    /// `jit_bridge`: the OSR exception exit with no stash.
    OsrExceptionNoStash = 8,
    /// `jit_bridge`: the committed single-pass OSR exit naming no lock.
    SinglePassOsrExit = 9,
    /// `interpreter::try_osr_offer`: an OSR'd body returned or threw.
    OsrBodyLeft = 10,
    /// `interpreter::try_osr_offer`: the return adapter's `ThrowJava` arm.
    OsrReturnAdapterThrow = 11,
    /// `exception_dispatch::route_jit_signal_exception`, no stash.
    SignalHandlerNoStash = 12,
    /// `exception_dispatch::run_jit_callee_handler`, no stash.
    CalleeHandlerNoStash = 13,
    /// `exception_dispatch`: the materializer re-took elided levels no record
    /// names (`materialize_and_relock_precise_frame`).
    RetakenElidedLevels = 14,
    /// `Frame::from_frozen_frame` (a thaw carries the frozen mark's producer).
    Thaw = 15,
}

impl RecordProducer {
    const ALL: [RecordProducer; 16] = [
        Self::Unnamed,
        Self::OsrExitTransfer,
        Self::PlanlessTransfer,
        Self::HandlerSeed,
        Self::PushedHandlerSeed,
        Self::DeoptBuild,
        Self::IrDeoptResume,
        Self::InlinedChain,
        Self::OsrExceptionNoStash,
        Self::SinglePassOsrExit,
        Self::OsrBodyLeft,
        Self::OsrReturnAdapterThrow,
        Self::SignalHandlerNoStash,
        Self::CalleeHandlerNoStash,
        Self::RetakenElidedLevels,
        Self::Thaw,
    ];

    fn from_code(code: usize) -> Self {
        Self::ALL.get(code).copied().unwrap_or(Self::Unnamed)
    }

    /// The census name.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Unnamed => "unnamed",
            Self::OsrExitTransfer => "osr-exit-transfer",
            Self::PlanlessTransfer => "planless-transfer",
            Self::HandlerSeed => "handler-seed",
            Self::PushedHandlerSeed => "pushed-handler-seed",
            Self::DeoptBuild => "deopt-build",
            Self::IrDeoptResume => "ir-deopt-resume",
            Self::InlinedChain => "inlined-chain",
            Self::OsrExceptionNoStash => "osr-exception-no-stash",
            Self::SinglePassOsrExit => "single-pass-osr-exit",
            Self::OsrBodyLeft => "osr-body-left",
            Self::OsrReturnAdapterThrow => "osr-return-adapter-throw",
            Self::SignalHandlerNoStash => "signal-handler-no-stash",
            Self::CalleeHandlerNoStash => "callee-handler-no-stash",
            Self::RetakenElidedLevels => "retaken-elided-levels",
            Self::Thaw => "thaw",
        }
    }
}

/// The mark address of `producer` with the understated bit `understated`.
fn mark_addr(producer: RecordProducer, understated: bool) -> usize {
    8 * (1 + 2 * producer as usize + usize::from(understated))
}

impl HeldMonitors {
    /// An empty record (no allocation).
    #[inline(always)]
    pub(crate) fn new() -> Self {
        Self {
            objs: SmallVec::new(),
        }
    }

    /// A record of `entries` (oldest first), marked understated when
    /// `understated`: a thawed frame's (`Frame::from_frozen_frame`;
    /// interpreter round i6 wave 118, lane L6).
    pub(crate) fn from_entries(entries: &[ObjectRef], understated: bool) -> Self {
        let mut record = Self::new();
        if understated {
            record.mark_understated_by(RecordProducer::Thaw);
        }
        for &obj in entries {
            record.push(obj);
        }
        record
    }

    /// The mark entry's address, when the record has one (always at index 0).
    #[inline(always)]
    fn mark_word(&self) -> Option<usize> {
        // Cast: object pointer to integer address (the mark's sentinel).
        let first = self.objs.first()?.as_ptr() as usize;
        (first < MARK_LIMIT).then_some(first)
    }

    /// 1 when the record carries a mark (always at index 0), else 0: the
    /// offset of the first real entry.
    #[inline(always)]
    fn mark_len(&self) -> usize {
        usize::from(self.mark_word().is_some())
    }

    /// Install `addr` as the mark (replacing one already there).
    fn set_mark(&mut self, addr: usize) {
        // SAFETY: the mark is never dereferenced: every accessor skips index 0
        // when it holds an address below `MARK_LIMIT`, and the GC scans and
        // remaps see only `as_slice` / `remap_with`. `addr` is a non-zero
        // multiple of 8 below `MARK_LIMIT`.
        let mark = unsafe { ObjectRef::from_raw(addr as *mut u8) };
        if self.mark_len() == 1 {
            self.objs[0] = mark;
        } else {
            self.objs.insert(0, mark);
        }
    }

    /// The producer named by the record's mark, if any (interpreter round i6
    /// wave 121, lane L6; the `CRATONVM_DBG_MONITOR_LENIENCY` census).
    pub(crate) fn producer(&self) -> Option<RecordProducer> {
        self.mark_word()
            .map(|w| RecordProducer::from_code((w / 8 - 1) >> 1))
    }

    /// Record that `producer` seeded this record from a description it trusts
    /// (the record is exact): provenance for the census only, not
    /// understated. Replaces an earlier mark, the understated bit included.
    pub(crate) fn set_provenance(&mut self, producer: RecordProducer) {
        self.set_mark(mark_addr(producer, false));
    }

    /// Whether the frame holds no block monitor — the only question the hot
    /// paths (every return, every frame pop) ask. A frame with no entry pays
    /// the one `is_empty` it always paid.
    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.objs.is_empty() || self.objs.len() == self.mark_len()
    }

    /// Number of recorded entries (an object entered twice counts twice).
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.objs.len() - self.mark_len()
    }

    /// Record one `monitorenter` of `obj` by this frame.
    #[inline(always)]
    pub(crate) fn push(&mut self, obj: ObjectRef) {
        self.objs.push(obj);
    }

    /// Remove the newest entry for `obj`; `false` when the frame holds none.
    /// The newest entry is the common case (javac nests its blocks), so it is
    /// one compare and a pop. The mark never equals an object.
    #[inline(always)]
    pub(crate) fn remove_newest(&mut self, obj: ObjectRef) -> bool {
        match self.objs.last() {
            Some(&top) if top == obj => {
                self.objs.pop();
                true
            }
            None => false,
            Some(_) => self.remove_newest_slow(obj),
        }
    }

    #[cold]
    #[inline(never)]
    fn remove_newest_slow(&mut self, obj: ObjectRef) -> bool {
        match self.objs.iter().rposition(|&o| o == obj) {
            Some(i) => {
                self.objs.remove(i);
                true
            }
            None => false,
        }
    }

    /// Remove the entry at `index` of [`Self::as_slice`] (no-op past the end).
    fn remove_at(&mut self, index: usize) {
        let at = index + self.mark_len();
        if at < self.objs.len() {
            self.objs.remove(at);
        }
    }

    /// How many entries name `obj`.
    pub(crate) fn count_of(&self, obj: ObjectRef) -> usize {
        self.as_slice().iter().filter(|&&o| o == obj).count()
    }

    /// The entries, oldest first — the GC root scans' view (never the mark).
    #[inline]
    pub(crate) fn as_slice(&self) -> &[ObjectRef] {
        &self.objs[self.mark_len()..]
    }

    /// Remove and return the newest entry (never the mark).
    #[inline]
    pub(crate) fn pop(&mut self) -> Option<ObjectRef> {
        if self.objs.len() > self.mark_len() {
            self.objs.pop()
        } else {
            None
        }
    }

    /// Forget every entry and the mark (frame reuse; entering OSR'd code).
    #[inline]
    pub(crate) fn clear(&mut self) {
        self.objs.clear();
    }

    /// Forget every entry because the resume path's description of the
    /// frame's locks is not trusted, and mark the record understated when the
    /// frame may still hold a lock no entry names: `may_hold` (its method has
    /// monitor bytecode), or the record held entries or the mark already.
    /// Interpreter round i6 wave 117, lane L6.
    ///
    /// Wave 121 (lane L6): `producer` is recorded in the mark; a record that
    /// is not marked understated still names it as provenance.
    pub(crate) fn clear_untrusted(&mut self, may_hold: bool, producer: RecordProducer) {
        let understated = may_hold || !self.is_empty() || self.is_understated();
        self.objs.clear();
        self.set_mark(mark_addr(producer, understated));
    }

    /// Mark the record understated, producer `Unnamed` (idempotent on the
    /// bit; an existing mark's producer is kept).
    pub(crate) fn mark_understated(&mut self) {
        let producer = self.producer().unwrap_or(RecordProducer::Unnamed);
        self.mark_understated_by(producer);
    }

    /// Mark the record understated by `producer` (wave 121, lane L6).
    pub(crate) fn mark_understated_by(&mut self, producer: RecordProducer) {
        self.set_mark(mark_addr(producer, true));
    }

    /// Does the record carry the understated bit?
    #[inline]
    pub(crate) fn is_understated(&self) -> bool {
        self.mark_word().is_some_and(|w| (w / 8 - 1) & 1 == 1)
    }

    /// Rewrite each entry through `lookup` (old address -> new address), the
    /// GC remap twin of [`Self::as_slice`]. Each entry is looked up exactly
    /// once — the blocked-wake `fixup` chains must not be applied twice — and
    /// an entry the map does not name is left alone. The mark is skipped.
    pub(crate) fn remap_with(&mut self, mut lookup: impl FnMut(usize) -> Option<usize>) {
        let skip = self.mark_len();
        for o in self.objs.iter_mut().skip(skip) {
            // Cast: object pointer to integer address (pointer-map key)
            let old_addr = o.as_ptr() as usize;
            if let Some(new_addr) = lookup(old_addr) {
                if new_addr != 0 {
                    // SAFETY: `new_addr` came from the collector's pointer map
                    // and is the relocated object's (non-null, aligned) header,
                    // exactly as the `monitor_on_exit` remap next to every call
                    // of this function trusts it.
                    *o = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }
}

/// Does the executing frame `frames[frame_idx]` carry an understated record
/// ([`HeldMonitors::is_understated`])? Then the unaccounted hold its
/// `monitorexit` names may be its own, taken by compiled code whose exit
/// description named no lock, and the strict `monitorexit` arm stays
/// lenient. Interpreter round i6 wave 119, lane L6: only this frame's mark,
/// as HotSpot's interpreter looks only in the executing frame's monitor
/// block. A caller's understated record does not make this frame's exit of
/// that caller's lock legal: this frame's own record is exact unless marked,
/// so the hold is not this frame's, and HotSpot throws there (wave 117 asked
/// every frame, which was lenient for that case too).
fn executing_record_understated(thread: &JvmThread, frame_idx: usize) -> bool {
    thread
        .frames
        .get(frame_idx)
        .is_some_and(|f| f.held_monitors.is_understated())
}

/// The record-clearing of a resume path whose compiled lock description is
/// not trusted, for `frame` (interpreter round i6 wave 117, lane L6; page
/// `i116-L9-strict-monitorexit-throws-on-an-understated-record`): empty the
/// record, and mark it understated when the frame may hold a block monitor
/// (it held entries, or its method has `monitorenter` / `monitorexit`).
/// Wave 121 (lane L6): `producer` names the calling path in the record's mark
/// (understated or not), for the `CRATONVM_DBG_MONITOR_LENIENCY` census.
pub(crate) fn clear_untrusted_record(
    frame: &mut crate::runtime::frame::Frame,
    producer: RecordProducer,
) {
    let code_len = frame.code.len();
    let may_lock = cratonvm_jit::bytecode_holds_monitor(&frame.code, code_len);
    frame.held_monitors.clear_untrusted(may_lock, producer);
}

/// A resume path that seeded `frame`'s record from a description it trusts
/// names itself in the record's mark as provenance (not understated), for the
/// `CRATONVM_DBG_MONITOR_LENIENCY` census: a `released` line for a frame
/// whose mark names a TRUSTED producer is that producer under-describing its
/// locks. Interpreter round i6 wave 121, lane L6.
pub(crate) fn note_trusted_record(
    frame: &mut crate::runtime::frame::Frame,
    producer: RecordProducer,
) {
    frame.held_monitors.set_provenance(producer);
}

/// `IllegalMonitorStateException` with a null message, HotSpot's
/// `throw_illegal_monitor_state_exception` (an empty message is
/// `RuntimeError`'s "no message" marker).
#[inline]
pub(crate) fn imse_no_message() -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::IllegalMonitorStateException {
            message: String::new(),
        },
    ))
}

/// How many times this thread holds `obj` according to the monitor table
/// (0 when it does not own it). `from_record`: `obj` came out of a record, not
/// off the operand stack, and may be a stale address (see the module doc), so
/// it is first checked to lie inside the heap — containment only
/// (`is_heap_addr`), because the header-reading `is_object_address` can
/// reject a genuine young object, and a genuine entry read as stale loses the
/// check it exists for.
fn table_holds(shared: &SharedVm, thread: &JvmThread, obj: ObjectRef, from_record: bool) -> usize {
    // Cast: object pointer to integer address
    if from_record
        && shared
            .mem
            .heap
            .is_heap_addr(obj.as_ptr() as usize)
            .is_none()
    {
        return 0;
    }
    let monitors = &shared.threads.monitors;
    if !monitors.holds(obj, thread.thread_id) {
        return 0;
    }
    usize::try_from(monitors.entry_count(obj)).unwrap_or(usize::MAX)
}

/// How many acquisitions of `obj` this thread's interpreter frames account
/// for: every frame's record, plus each synchronized frame's method monitor.
fn recorded_holds(thread: &JvmThread, obj: ObjectRef) -> usize {
    thread
        .frames
        .iter()
        .map(|f| f.held_monitors.count_of(obj) + usize::from(f.monitor_on_exit == Some(obj)))
        .sum()
}

/// Release `obj` through the table, answering a failure with the Java-visible
/// null-message `IllegalMonitorStateException` HotSpot throws. The table's own
/// text names the thread and the object's address; it is a VM diagnostic (GC
/// relocation bugs surface through it) and goes to the log, not to Java.
fn release_or_imse(
    shared: &SharedVm,
    thread_id: ThreadId,
    obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    match crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, obj, thread_id) {
        Ok(()) => Ok(()),
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalMonitorStateException { message },
        ))) => {
            tracing::warn!(
                thread_id = ?thread_id,
                detail = %message,
                "monitorexit: the monitor table refused the release"
            );
            Err(imse_no_message())
        }
        Err(other) => Err(other),
    }
}

/// `monitorexit` of a monitor the executing frame's record names: the
/// common case, one table release. A refusal here means the table and the
/// record disagree — a VM defect (a relocation the table and the frame saw
/// differently), which [`release_or_imse`] logs at `warn` with the table's
/// text ("thread N does not own the monitor for object at 0x…").
#[inline]
pub(super) fn monitorexit_recorded(
    shared: &SharedVm,
    thread_id: ThreadId,
    obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    release_or_imse(shared, thread_id, obj)
}

/// `monitorexit` of `obj` by `frames[frame_idx]`, whose record does not name
/// it. See the module doc: released when the monitor table shows an
/// acquisition no record accounts for (a frame resumed from compiled code),
/// otherwise `IllegalMonitorStateException` with a null message and the table
/// untouched.
#[cold]
#[inline(never)]
pub(super) fn monitorexit_unrecorded(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let owned = table_holds(shared, thread, obj, false);
    let recorded = recorded_holds(thread, obj);
    // JIT round 21 wave 4, lane monitors (`CRATONVM_MONITOREXIT_FOREIGN_HOLD_IMSE`,
    // default ON): an unbalanced method whose record already names every
    // block monitor its own code holds here proves the unrecorded hold is
    // another frame's, and the exit throws below (`record_proves_foreign_hold`).
    let foreign_hold_proven = {
        let me: &JvmThread = thread;
        owned > recorded
            && me.frames.get(frame_idx).is_some_and(|frame| {
                record_proves_foreign_hold(
                    frame,
                    |o| table_holds(shared, me, o, true),
                    |o| recorded_holds(me, o),
                )
            })
    };
    if owned > recorded && !foreign_hold_proven {
        // Interpreter round i6 wave 115, lane L6 (stage 3 of proposal
        // `i23-L7-proposal-per-frame-locked-monitors`): the leniency, made
        // visible and switchable. Every interpreter resume path seeds its
        // frame's record since stage 2, so what is left for this release is
        // an acquisition no interpreter frame made at all: a compiled
        // caller's lock, a JNI `MonitorEnter`, a `synchronized` native's
        // receiver. HotSpot's interpreter looks only in the executing frame's
        // monitor block and throws (`TemplateTable::monitorexit` ->
        // `throw_illegal_monitor_state_exception`).
        // `CRATONVM_DBG_MONITOR_LENIENCY=1` names each release (the positive
        // control: it must print nothing over the regression suite before the
        // strict arm may be flipped); `CRATONVM_MONITOREXIT_FRAME_STRICT`
        // (default ON since wave 134 by user decision, `=0` off; HotSpot
        // parity) throws instead. Both are read here, on this cold path only.
        // Wave 117 (lane L6, page
        // `i116-L9-strict-monitorexit-throws-on-an-understated-record`): not
        // while the executing frame carries an understated record
        // (`clear_untrusted_record`): the hold may be its own, which its
        // compiled exit description did not name (wave 119: this frame's
        // mark only, `executing_record_understated`).
        let understated = executing_record_understated(thread, frame_idx);
        // WARNING(i6-user-flip): CRATONVM_MONITOREXIT_FRAME_STRICT default ON by user decision 2026-10-19; =0 restores the old path
        let mut strict =
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MONITOREXIT_FRAME_STRICT")
                && !understated;
        // Interpreter round i6 wave 132, lane L3
        // (`CRATONVM_MONITOREXIT_COMPILED_HOLDER_IMSE`, parity, default ON,
        // `=0` off; page
        // `i116-L6-a-compiled-unpaired-monitorexit-releases-its-callers-lock`,
        // the caller leg): with the strict arm off, the hold is still not this
        // frame's when the thread's live COMPILED activations name every
        // unaccounted acquisition as their own. Releasing it took the lock from
        // under a compiled caller, whose own `monitorexit` then threw inside
        // javac's self-covering catch-any and spun (`caller-leg: timeout`).
        // Proof only: a compiled hold the artifact does not describe counts
        // nothing, and the frame stays lenient.
        let mut compiled = None;
        if !strict && !understated && monitorexit_compiled_holder_imse_enabled() {
            let n = compiled_activation_holds(shared, thread, frame_idx, obj);
            compiled = Some(n);
            strict = compiled_holds_explain_the_excess(owned, recorded, n);
        }
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_MONITOR_LENIENCY") {
            note_monitorexit_leniency(thread, frame_idx, owned, recorded, strict, compiled);
        }
        if !strict {
            return release_or_imse(shared, thread.thread_id, obj);
        }
        return Err(imse_no_message());
    }
    if let Some(f) = thread.frames.get(frame_idx) {
        tracing::debug!(
            class = %f.class_name(),
            method = %f.method_name(),
            pc = f.last_instr_pc,
            owned,
            recorded,
            object = ?obj.as_ptr(),
            "monitorexit of a monitor this frame did not enter (JVMS 2.11.10)"
        );
    }
    Err(imse_no_message())
}

/// `CRATONVM_DBG_MONITOR_LENIENCY` (interpreter round i6 wave 115, lane L6):
/// one stderr line per `monitorexit` the leniency of
/// [`monitorexit_unrecorded`] answered, naming the frame. Cold.
#[cold]
#[inline(never)]
fn note_monitorexit_leniency(
    thread: &JvmThread,
    frame_idx: usize,
    owned: usize,
    recorded: usize,
    strict: bool,
    compiled: Option<usize>,
) {
    let (class, method, descriptor, pc) = thread.frames.get(frame_idx).map_or_else(
        || (String::from("?"), String::from("?"), String::new(), 0),
        |f| {
            (
                f.class_name().to_string(),
                f.method_name().to_string(),
                f.method_descriptor().to_string(),
                f.last_instr_pc,
            )
        },
    );
    // Interpreter round i6 wave 121, lane L6: the frame's record producer and
    // its understated bit. `producer=none` is a frame the interpreter entered
    // itself (no resume path wrote its record): its release is a genuine
    // HotSpot IMSE case (a compiled caller's lock, JNI, a `synchronized`
    // native). `understated=false` with a producer named is that TRUSTED
    // producer under-describing the frame's locks: the producer to fix before
    // `CRATONVM_MONITOREXIT_FRAME_STRICT` can be flipped.
    let (producer, understated) = thread.frames.get(frame_idx).map_or(("none", false), |f| {
        (
            f.held_monitors.producer().map_or("none", RecordProducer::name),
            f.held_monitors.is_understated(),
        )
    });
    // Wave 132 (lane L3): the compiled activations' own holds of the object
    // (`-` when not counted: the strict arm decided, the record is
    // understated, or `CRATONVM_MONITOREXIT_COMPILED_HOLDER_IMSE=0`).
    let compiled = compiled.map_or_else(|| String::from("-"), |n| n.to_string());
    eprintln!(
        "[monitor-leniency] {} monitorexit of an unrecorded monitor in {class}.{method}{descriptor} \
         pc={pc} producer={producer} understated={understated} owned={owned} recorded={recorded} \
         compiled={compiled} (thread {})",
        if strict { "strict-throw" } else { "released" },
        thread.thread_id.0,
    );
}

/// `CRATONVM_MONITOREXIT_COMPILED_HOLDER_IMSE`, **default ON** (interpreter
/// round i6 wave 132, lane L3; HotSpot parity, `=0` is the kill switch): see
/// [`monitorexit_unrecorded`]. Read only on that cold path, per call; no memo
/// and no static.
fn monitorexit_compiled_holder_imse_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MONITOREXIT_COMPILED_HOLDER_IMSE")
}

/// Do the compiled activations' own holds (`compiled`) account for every
/// acquisition of the object the interpreter records do not (`owned -
/// recorded`)? Then none of it is the executing frame's, and HotSpot's
/// interpreter throws. At least one compiled hold is required: with none, the
/// excess is unexplained and the leniency keeps its old answer.
fn compiled_holds_explain_the_excess(owned: usize, recorded: usize, compiled: usize) -> bool {
    compiled > 0 && owned <= recorded.saturating_add(compiled)
}

/// Does compiled activation label `label` (`class/Name.method:descriptor`)
/// name the method of interpreter frame `frame`? By class and method name
/// only: a looser test than the stack walk's, which here can only skip more
/// activations (count fewer holds) and so only keeps the leniency.
fn compiled_label_names_frame(label: &str, frame: &Frame) -> bool {
    label_class_and_method(label)
        .is_some_and(|(class, method)| class == frame.class_name() && method == frame.method_name())
}

/// How many times this thread's live compiled activations below
/// `frames[frame_idx]` name `obj` as a monitor of their OWN (a `synchronized`
/// block, a self-locking body's method monitor), read through
/// [`compiled_frame_hold_words`] -- the artifact's own description at the
/// activation's active safepoint, every word checked against the active oop
/// map (interpreter round i6 wave 132, lane L3).
///
/// An activation that names the interpreter frame it was pushed from, or the
/// one right above it, is skipped: it may be that frame's own body (an OSR
/// continuation, a call-dedupe twin, a body resumed in the interpreter), whose
/// holds that frame's record may already count. A
/// caller-held synchronized body's method monitor is not read (it is its
/// caller's). Every omission counts fewer holds, the lenient direction.
#[cold]
#[inline(never)]
fn compiled_activation_holds(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    obj: ObjectRef,
) -> usize {
    use crate::jit::conservative_roots as cr;
    let jit = cr::active_compiled_frames_whole_opts(false, cr::CompiledWalkOptions::default());
    if jit.is_empty() {
        return 0;
    }
    let words = compiled_frame_word_holds_enabled();
    let ir_homes = words && ir_frame_monitor_homes_enabled();
    // Cast: object pointer to integer address (compared, never dereferenced).
    let target = obj.as_ptr() as usize;
    let mirrors = shared.classes.class_mirrors.read();
    let mirror = |class_id: u32| {
        mirrors
            .get(&crate::classloading::ClassId::new(class_id))
            .map(|m| m.as_ptr() as usize)
    };
    let mut held = 0usize;
    for f in &jit {
        let depth = f.interp_depth as usize;
        // A chain entry pushed at depth `d` sits below `frames[d]`; one above
        // the executing frame cannot be live while that frame executes.
        if depth > frame_idx {
            continue;
        }
        // A twin: the frame it was pushed from (an OSR continuation, a
        // call-dedupe body) or the frame right above it (a body resumed in
        // the interpreter while its compiled frame is still on the stack).
        let names_a_neighbour = |i: usize| {
            thread
                .frames
                .get(i)
                .is_some_and(|frame| compiled_label_names_frame(&f.label, frame))
        };
        if depth.checked_sub(1).is_some_and(names_a_neighbour) || names_a_neighbour(depth) {
            continue;
        }
        // SAFETY: `f` comes from the walk of the CALLING thread just above,
        // and this thread is inside its own `monitorexit`, so its compiled
        // frames have not changed since (`f.cm_ptr` live, `f.frame_rbp` its
        // frame).
        if !unsafe { compiled_frame_may_hold(f) } {
            continue;
        }
        // SAFETY: as above.
        let words_of_f = unsafe { compiled_frame_hold_words(f, words, ir_homes, false, &mirror) };
        held += words_of_f.iter().filter(|&&w| w == target).count();
    }
    held
}

/// `CRATONVM_MONITOREXIT_FOREIGN_HOLD_IMSE` -- default ON (JIT round 21 wave
/// 4, lane monitors; residual 3 of
/// `docs/internal/fixed-bugs/r21-single-pass-compiles-unbalanced-monitor-bytecode-FIXED-20261018.md`):
/// does `frame`'s record already name every block monitor its
/// OWN code can hold at the `monitorexit` it is executing?
///
/// [`monitorexit_unrecorded`] releases an acquisition no interpreter record
/// accounts for, because a frame resumed from compiled code (a deopt, an OSR
/// exit) holds locks its record does not name. But an acquisition no record
/// names is also exactly the hold of a COMPILED frame below this one, and
/// releasing it let an interpreted callee's stray `monitorexit` unlock its
/// compiled caller's `synchronized` block, where HotSpot throws
/// `IllegalMonitorStateException` and the caller keeps the lock.
///
/// The bytecode tells the two apart: at this pc the frame's own code holds
/// exactly `cratonvm_jit::monitor_balance::block_monitor_depth_at` block
/// monitors on every path. When the record already names that many (and
/// [`record_backed_by_table`] confirms every entry against the live table),
/// none of the frame's own holds is missing from it, the object (which it
/// does not name) is another frame's, and the exit throws.
///
/// Only for a method whose block monitors are UNBALANCED
/// (`cratonvm_jit::monitor_balance::unbalanced_monitor_site`, every rule):
/// HotSpot never compiles those, and only they can execute a `monitorexit`
/// of a lock their frame did not take. In a balanced method (all javac,
/// ecj, kotlinc output) every `monitorexit` releases the frame's own lock,
/// so a miss there is always an under-reporting record and releases as
/// before. That is what makes a false throw impossible where it would hang:
/// javac's catch-any covers its own `monitorexit`, so an exception there
/// re-enters the same exit forever (review rev-vmjit, item 1 of
/// `docs/internal/fixed-bugs/r21-review-vmjit-findings-FIXED-20261019.md`).
///
/// When the depth is unknown (paths disagree, `jsr`/`ret`, a table the walk
/// cannot read), the record names fewer, or an entry disagrees with the
/// table, the release goes ahead as before. `0|false|off|no`: always as
/// before.
/// Cold: asked only on an exit whose object the table holds more often than
/// the records account for.
#[cold]
fn frame_record_is_complete(frame: &Frame) -> bool {
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MONITOREXIT_FOREIGN_HOLD_IMSE") {
        return false;
    }
    // Only for the `monitorexit` the frame is executing: a pc that names
    // anything else answers nothing, and the release goes ahead as before.
    if frame.code.get(frame.last_instr_pc) != Some(&0xc3) {
        return false;
    }
    let table = frame.exception_table();
    let ranges: Vec<(usize, usize, usize)> = table
        .iter()
        .map(|e| {
            (
                usize::from(e.start_pc),
                usize::from(e.end_pc),
                usize::from(e.handler_pc),
            )
        })
        .collect();
    let catch_all: Vec<bool> = table.iter().map(|e| e.catch_type == 0).collect();
    if cratonvm_jit::monitor_balance::unbalanced_monitor_site(
        &frame.code,
        frame.code.len(),
        &ranges,
        &catch_all,
        cratonvm_jit::monitor_balance::BalanceRules::ALL,
    )
    .is_none()
    {
        return false;
    }
    match cratonvm_jit::monitor_balance::block_monitor_depth_at(
        &frame.code,
        frame.code.len(),
        &ranges,
        &catch_all,
        frame.last_instr_pc,
    ) {
        Some(depth) => usize::try_from(depth).is_ok_and(|d| frame.held_monitors.len() >= d),
        None => false,
    }
}

/// Does the live monitor table back every entry of `frame`'s record? `owned`
/// is how often this thread holds an object by the table (0 for an address
/// that is not a heap object any more), `recorded` how often every frame's
/// record and method monitor name it. An entry the table holds less often
/// than the records claim is stale ([`prune_stale`]'s test, which runs only
/// at a return or an unwind), so the record may over-report and
/// [`frame_record_is_complete`]'s answer is not trusted.
fn record_backed_by_table(
    frame: &Frame,
    owned: impl Fn(ObjectRef) -> usize,
    recorded: impl Fn(ObjectRef) -> usize,
) -> bool {
    frame.held_monitors.as_slice().iter().all(|&o| {
        let held = owned(o);
        held > 0 && held >= recorded(o)
    })
}

/// [`monitorexit_unrecorded`]'s whole test that an unrecorded hold is another
/// frame's: the record names every block monitor the frame's own code holds
/// ([`frame_record_is_complete`]) AND each of those entries is backed by the
/// live table ([`record_backed_by_table`], [`prune_stale`]'s own test), so a
/// stale entry can never stand in for one of the frame's own holds (review
/// rev-vmjit item 1). The depth test runs first: it is pure bytecode and
/// answers `false` for every balanced method, so the table is read only for
/// an unbalanced one.
fn record_proves_foreign_hold(
    frame: &Frame,
    owned: impl Fn(ObjectRef) -> usize,
    recorded: impl Fn(ObjectRef) -> usize,
) -> bool {
    frame_record_is_complete(frame) && record_backed_by_table(frame, owned, recorded)
}

/// Drop the entries of `frames[frame_idx]` that the monitor table does not
/// back (see the module doc), newest first.
fn prune_stale(shared: &SharedVm, thread: &mut JvmThread, frame_idx: usize) {
    let Some(len) = thread.frames.get(frame_idx).map(|f| f.held_monitors.len()) else {
        return;
    };
    let mut i = len;
    while i > 0 {
        i -= 1;
        let Some(obj) = thread.frames[frame_idx]
            .held_monitors
            .as_slice()
            .get(i)
            .copied()
        else {
            continue;
        };
        let owned = table_holds(shared, thread, obj, true);
        let recorded = recorded_holds(thread, obj);
        if owned < recorded {
            tracing::debug!(
                owned,
                recorded,
                object = ?obj.as_ptr(),
                "held-monitor record: dropping an entry the monitor table does not back"
            );
            thread.frames[frame_idx].held_monitors.remove_at(i);
        }
    }
}

/// A return is executing in `frames[frame_idx]` while its record is not
/// empty. Answers the `IllegalMonitorStateException` to throw at the return's
/// bci, or `None` when every entry was stale and the return may proceed.
#[cold]
#[inline(never)]
pub(super) fn return_with_held_monitors(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Option<MethodCallFailed> {
    prune_stale(shared, thread, frame_idx);
    let still_held = thread
        .frames
        .get(frame_idx)
        .is_some_and(|f| !f.held_monitors.is_empty());
    still_held.then(imse_no_message)
}

/// The decoded return arms' form of the return check: `Err` is the
/// `IllegalMonitorStateException` to throw at the return. One `is_empty` test
/// when the frame holds no block monitor.
#[inline(always)]
pub(super) fn check_structured_return(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Result<(), MethodCallFailed> {
    if thread
        .frames
        .get(frame_idx)
        .is_none_or(|f| f.held_monitors.is_empty())
    {
        return Ok(());
    }
    match return_with_held_monitors(shared, thread, frame_idx) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Release, newest first, every monitor `frames[frame_idx]`'s record still
/// holds, and empty the record. Answers whether anything genuine was released.
#[cold]
#[inline(never)]
pub(super) fn release_all_recorded(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> bool {
    prune_stale(shared, thread, frame_idx);
    let tid = thread.thread_id;
    let mut released = false;
    while let Some(obj) = thread
        .frames
        .get_mut(frame_idx)
        .and_then(|f| f.held_monitors.pop())
    {
        released = true;
        if let Err(e) = crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, obj, tid) {
            tracing::warn!(
                thread_id = ?tid,
                error = ?e,
                "releasing a block monitor of a frame being removed failed"
            );
        }
    }
    released
}

/// Exception unwinding is about to leave `frames[frame_idx]` with monitors
/// still recorded: release them and make a new `IllegalMonitorStateException`
/// the in-flight throwable (`native_pin_roots[pin_slot]`), as HotSpot's
/// unwinding `remove_activation` does. If the new throwable cannot be built,
/// the original one keeps propagating.
#[cold]
#[inline(never)]
pub(super) fn replace_unwound_exception_if_locked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    pin_slot: usize,
) {
    if !release_all_recorded(shared, thread, frame_idx) {
        return;
    }
    // Built after the release, with the frame still on the stack (its line is
    // the top of the new trace, as in HotSpot); the frames and the pinned
    // original throwable are rooted across the allocation.
    if let MethodCallFailed::ExceptionThrown(exc) = crate::runtime::exceptions::throw_runtime_error(
        shared,
        thread,
        RuntimeError::IllegalMonitorStateException {
            message: String::new(),
        },
    ) {
        let exc = super::settle_thrown_exception(shared, thread, exc);
        if let Some(slot) = thread.native_pin_roots.get_mut(pin_slot) {
            *slot = exc;
        }
    }
}

/// Every monitor the interpreted `frames` hold, as `(frame index, object)`
/// with the index into `frames` (0 = the bottom frame), in the order HotSpot's
/// `javaVFrame::locked_monitors` reports them to `ThreadInfo` / `jstack`: the
/// innermost frame first, and within a frame the newest block monitor first
/// and the synchronized method's own monitor (entered before any block) last.
/// An object a frame entered twice appears twice, and one entered by two
/// frames appears under each, as in HotSpot.
///
/// Stage 1 of proposal `i23-L7-proposal-per-frame-locked-monitors`
/// (interpreter round i1 wave 24, lane L7). Frames the interpreter resumed
/// from compiled code may under-report (see the module doc); a caller keeps
/// what the monitor table says the thread owns beyond this list. Empty (no
/// allocation) when no frame holds a monitor.
///
/// Stable contract (wave 25): the JVMTI / JDWP owned-monitor functions build on
/// this and [`attribute_locked_monitors`]; see the proposal's "Wave 25 note"
/// for what they must add (one entry per object, depth against the reported
/// frames). Change the signature or the order only together with that note.
pub(crate) fn frame_locked_monitors(frames: &[Frame]) -> Vec<(usize, ObjectRef)> {
    let mut out = Vec::new();
    for (index, frame) in frames.iter().enumerate().rev() {
        if frame.held_monitors.is_empty() && frame.monitor_on_exit.is_none() {
            continue;
        }
        out.extend(
            frame
                .held_monitors
                .as_slice()
                .iter()
                .rev()
                .map(|&o| (index, o)),
        );
        if let Some(o) = frame.monitor_on_exit {
            out.push((index, o));
        }
    }
    out
}

/// Attribute the monitors a thread owns (`owned`, the JMX lock stack: one
/// entry per object) to stack-trace depths, for `ThreadInfo.getLockedMonitors`.
///
/// `frame_monitors` is [`frame_locked_monitors`] of the frames `trace_len`
/// stack-trace entries were captured from (index 0 = bottom), or `None` when
/// no attribution matching that trace exists. `waiting` is the monitor the
/// thread is parked in `Object.wait` on: it is released while the thread
/// waits, and HotSpot reports it as the thread's lock, never as locked.
///
/// Answers the monitors to report and a parallel depth for each (`0` = the
/// innermost trace entry, `-1` = held but attributed to no interpreted frame:
/// compiled code, JNI). With no attribution the depth list is empty, which
/// the JMX builder reads as "every monitor at the innermost frame" (the
/// behaviour before wave 24).
pub(crate) fn attribute_locked_monitors(
    trace_len: usize,
    owned: Vec<ObjectRef>,
    frame_monitors: Option<Vec<(usize, ObjectRef)>>,
    waiting: Option<ObjectRef>,
) -> (Vec<ObjectRef>, Vec<i32>) {
    let owned: Vec<ObjectRef> = owned.into_iter().filter(|&o| Some(o) != waiting).collect();
    let Some(frame_monitors) = frame_monitors else {
        return (owned, Vec::new());
    };
    let mut monitors = Vec::with_capacity(frame_monitors.len() + owned.len());
    let mut depths = Vec::with_capacity(frame_monitors.len() + owned.len());
    for (index, obj) in frame_monitors {
        if Some(obj) == waiting || !owned.contains(&obj) || index >= trace_len {
            continue;
        }
        monitors.push(obj);
        depths.push(i32::try_from(trace_len - 1 - index).unwrap_or(i32::MAX));
    }
    for obj in owned {
        if !monitors.contains(&obj) {
            monitors.push(obj);
            depths.push(-1);
        }
    }
    (monitors, depths)
}

/// `CRATONVM_JMX_COMPILED_FRAME_MONITOR_DEPTHS`, **default ON** (JIT round 19
/// proposal wave 6, lane sync6, SYNCDOOR618-5; a HotSpot-parity fix, `=0` is
/// the kill switch): the calling thread's `ThreadInfo.getLockedMonitors`
/// attributes its monitors with compiled activations on the stack
/// ([`frame_locked_monitors_with_compiled`]). Off: as before, one compiled
/// frame anywhere drops every attribution and each monitor reports depth 0.
/// Read per snapshot (a cold JMX path); no memo.
pub(crate) fn compiled_frame_monitor_depths_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JMX_COMPILED_FRAME_MONITOR_DEPTHS")
}

/// One position of the calling thread's captured trace, as
/// [`frame_locked_monitors_with_compiled`] lays it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TraceOwner {
    /// Interpreter frame `frames[i]`.
    Interp(usize),
    /// Compiled activation `jit[k]` (after the OSR dedupe).
    Compiled(usize),
    /// A callee inlined into the activation before it: holds nothing of its
    /// own that this attribution names.
    Inlined,
}

/// `"class/Name.method:descriptor"` -> `(class, method)`, refusing exactly
/// where the trace builders refuse a compiled or inlined entry.
fn label_class_and_method(label: &str) -> Option<(&str, &str)> {
    let (owner_and_method, _descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    (!class_name.is_empty() && !method_name.is_empty()).then_some((class_name, method_name))
}

/// The inlined levels the trace builders push for `chain` (innermost first):
/// outermost first, the first level that cannot be named ends it
/// (`stackwalker::push_inlined_chain`).
fn inlined_levels_named(chain: &[crate::jit::conservative_roots::InlinedLevel]) -> Vec<&str> {
    const MAX_CODE_LENGTH: u32 = 65_536;
    let mut out = Vec::new();
    for level in chain.iter().rev() {
        match label_class_and_method(&level.label) {
            Some((_, method)) if level.bci < MAX_CODE_LENGTH => out.push(method),
            _ => break,
        }
    }
    out
}

/// [`frame_locked_monitors`] for the CALLING thread when its trace
/// interleaves compiled activations (`captured != frames.len()`): every
/// interpreted frame's monitors at the trace position of that frame, plus the
/// class mirror a compiled `static synchronized` self-locking activation holds
/// (`CompiledMethod::self_lock_hold`) at the activation's own position, as
/// `(trace index as captured, object)` pairs (0 = outermost), innermost first,
/// in HotSpot's `javaVFrame::locked_monitors` order. JIT round 19 proposal
/// wave 6, lane sync6 (SYNCDOOR618-5;
/// `CRATONVM_JMX_COMPILED_FRAME_MONITOR_DEPTHS`).
///
/// `trace` is the trace the snapshot reports, `captured` its length as
/// captured and `inserted_before` the captured indices a served frame went in
/// before (`vm_exec::thread_stack_trace_served`). The layout is rebuilt from
/// the same walk the capture made (the stack has not changed: this thread is
/// in the JMX native) and CHECKED against `trace` entry by entry: any length
/// or name disagreement answers `None`, which is the behaviour before this
/// wave (every monitor at depth 0). So an attribution is never shifted.
///
/// JIT round 19 proposal wave 7, lane gc7 (SYNC619-1,
/// `CRATONVM_JMX_COMPILED_FRAME_WORD_HOLDS`, default ON): also an INSTANCE
/// self-locking single-pass activation's receiver (its frame word
/// `SelfLockHold::Instance { obj_rbp_off }`) and the `synchronized` blocks a
/// single-pass activation holds (`CompiledMethod::taken_monitor_slots`), each
/// read from the frame through the activation's frame pointer, only where the
/// active oop map names the word ([`compiled_frame_hold_words`]). A word is
/// kept only when it IS one of the monitors the thread's lock stack holds
/// (matched by address; no object is made from a frame word).
///
/// JIT round 19 proposal wave 8, lane gc8 (GC719-1,
/// `CRATONVM_JMX_IR_FRAME_MONITOR_HOMES`, default ON): an OPTIMIZING-tier
/// activation's receiver (its prologue home) and blocks (the homes its
/// artifact published per safepoint, `CompiledMethod::ir_monitor_homes`) the
/// same way.
///
/// Not attributed (left to `attribute_locked_monitors`'s `-1`, depth 0 in the
/// JMX builder, as before): an optimizing-tier activation's blocks at a point
/// its artifact published none for (a lock with no named home, snapshots that
/// disagree); a block an inlined callee holds; a caller-held synchronized
/// splice's monitor.
pub(crate) fn frame_locked_monitors_with_compiled(
    shared: &SharedVm,
    frames: &[Frame],
    trace: &[crate::native::registry::StackTraceEntry],
    captured: usize,
    inserted_before: &[usize],
) -> Option<Vec<(usize, ObjectRef)>> {
    use crate::jit::conservative_roots as cr;
    let jit = cr::active_compiled_frames_whole_opts(
        false,
        cr::CompiledWalkOptions {
            shared_inline_chains: cr::throws4_walk_shared_inline_chains_enabled(),
            activation_memo: cr::throws2_activation_memo_enabled(),
        },
    );
    let words = compiled_frame_word_holds_enabled();
    // GC1119-1 (round 19 wave 12, lane gc12): the dropped OSR continuations
    // too, for the blocks they took after their entry.
    let (jit, osr_bci, osr_holders) = crate::runtime::stackwalker::drop_osr_continuations_keeping(
        frames,
        jit,
        words && osr_body_block_holds_enabled(),
    );
    // GC719-1 (round 19 wave 8, lane gc8): an optimizing-tier activation's
    // published homes too, under the frame-word switch.
    let ir_homes = words && ir_frame_monitor_homes_enabled();
    // GC819-3 (round 19 wave 9, lane gc9): a caller-held synchronized
    // callee's method monitor at the callee's own activation.
    let caller_held = caller_held_method_monitor_depths_enabled();
    // The calling thread's lock stack: a frame word names a hold only when it
    // is one of these objects. Read once; empty (nothing matches) when the
    // thread is not registered.
    let owned: Vec<ObjectRef> = if words {
        crate::native::jni::current_jvm_thread_of(shared)
            .and_then(|thread| {
                // SAFETY: the calling thread's own `JvmThread`, inside this
                // JMX native; `thread_id` is `Copy`.
                let tid = unsafe { (*thread).thread_id };
                shared.threads.thread_registry.jmx_lock_snapshot(tid)
            })
            .map(|snapshot| snapshot.2)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mirrors = shared.classes.class_mirrors.read();
    let owned_word = |word: usize| owned.iter().copied().find(|o| o.as_ptr() as usize == word);
    attribute_over_compiled_layout(
        frames,
        &jit,
        &osr_bci,
        trace,
        captured,
        inserted_before,
        |i| {
            osr_holders
                .iter()
                .filter(|(at, _)| *at == i)
                .flat_map(|(_, holder)| {
                    // SAFETY: `holder` is a dropped OSR continuation of the
                    // walk above, live for this read (as `f` below).
                    unsafe {
                        osr_body_block_words(
                            holder,
                            &frames[i],
                            ir_homes,
                            crate::jit::conservative_roots::FrameWordReader::Own,
                        )
                    }
                })
                .filter_map(owned_word)
                .collect()
        },
        |f| {
            let mirror = |class_id: u32| {
                mirrors
                    .get(&crate::classloading::ClassId::new(class_id))
                    .map(|m| m.as_ptr() as usize)
            };
            // SAFETY: `cm_ptr` names the artifact of a live activation of this
            // thread, valid for as long as the frame is live, which covers this
            // read (the thread is inside the JMX native, its frames unchanged
            // since the walk above).
            let held =
                unsafe { compiled_frame_hold_words(f, words, ir_homes, caller_held, mirror) };
            held.into_iter()
                .filter_map(|word| {
                    // The static mirror is a live object from the mirror table;
                    // a frame word only by address equality with a held one.
                    owned
                        .iter()
                        .copied()
                        .find(|o| o.as_ptr() as usize == word)
                        .or_else(|| {
                            mirrors
                                .get(&crate::classloading::ClassId::new(f.owner_class_id))
                                .copied()
                                .filter(|m| m.as_ptr() as usize == word)
                        })
                })
                .collect()
        },
    )
}

/// `CRATONVM_JMX_COMPILED_FRAME_WORD_HOLDS`, **default ON** (JIT round 19
/// proposal wave 7, lane gc7, SYNC619-1/2; a HotSpot-parity fix, `=0` is the
/// kill switch): the locked-monitor attribution reads the holds a compiled
/// activation keeps in its FRAME -- an instance self-locking single-pass
/// body's receiver and a single-pass body's `synchronized` blocks
/// ([`compiled_frame_hold_words`]) -- for the calling thread
/// ([`frame_locked_monitors_with_compiled`]) and for the attribution a thread
/// publishes with its trace (`ThreadRegistry::publish_jmx_frame_monitors`).
/// Off: only a static self-locking activation's mirror is attributed, as in
/// wave 6. Read per snapshot (a cold JMX path) and per publish only when an
/// activation has such a hold; no memo.
pub(crate) fn compiled_frame_word_holds_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JMX_COMPILED_FRAME_WORD_HOLDS")
}

/// `CRATONVM_JVMTI_COMPILED_FRAME_MONITOR_DEPTHS`, **default ON** (JIT round
/// 19 proposal wave 7, lane gc7, SYNC619-3; a HotSpot-parity fix, `=0` is the
/// kill switch): the calling thread's JVMTI `GetOwnedMonitorStackDepthInfo`
/// with compiled activations on its stack attributes by trace index through
/// [`frame_locked_monitors_with_compiled`] (interpreter frames' monitors, a
/// compiled activation's own holds), under
/// `CRATONVM_JMX_COMPILED_FRAME_MONITOR_DEPTHS` too. Off: interpreter frames
/// only, as before (a compiled hold at depth `-1`). Read per call (cold).
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn jvmti_compiled_frame_monitors_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JVMTI_COMPILED_FRAME_MONITOR_DEPTHS")
}

/// `CRATONVM_JMX_IR_FRAME_MONITOR_HOMES`, **default ON** (JIT round 19
/// proposal wave 8, lane gc8, GC719-1; a HotSpot-parity fix, `=0` is the kill
/// switch): with `CRATONVM_JMX_COMPILED_FRAME_WORD_HOLDS` on, an
/// OPTIMIZING-tier activation's own holds are read from its frame too -- an
/// instance self-locking body's receiver (its prologue home,
/// `SelfLockHold::Instance { obj_rbp_off }`) and its `synchronized` blocks
/// (`CompiledMethod::ir_monitor_homes`) -- through the same checked read
/// single-pass frames take ([`compiled_frame_hold_words`]). Every read is a
/// candidate matched against the thread's lock stack, so a word that is not
/// a held lock (a stale or refused one) drops out and the hold keeps wave 7's
/// answer. Off: an optimizing-tier activation names only a static
/// self-locking body's mirror, as in wave 7. Read once per snapshot or
/// publish that has an activation which can hold something.
pub(crate) fn ir_frame_monitor_homes_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JMX_IR_FRAME_MONITOR_HOMES")
}

/// Can activation `f` hold a monitor [`compiled_frame_hold_words`] would
/// name? One field test each, no frame read, no flag read: the publish path
/// asks this first, so a stack with no such activation pays nothing more.
///
/// # Safety
///
/// `f.cm_ptr` must name the artifact of a live activation (as every walk's
/// output does while its frame is live), or be 0.
pub(crate) unsafe fn compiled_frame_may_hold(
    f: &crate::jit::conservative_roots::ActiveCompiledFrame,
) -> bool {
    // SAFETY: the caller's contract.
    let Some(cm) = (unsafe { (f.cm_ptr as *const cratonvm_jit::CompiledMethod).as_ref() }) else {
        return false;
    };
    cm.self_lock_hold.is_some()
        || !cm.taken_monitor_slots.is_empty()
        || !cm.ir_monitor_homes.is_empty()
        || caller_held_method_body(cm)
}

/// `CRATONVM_JMX_CALLER_HELD_METHOD_MONITOR_DEPTHS`, **opt-in** (JIT round
/// 19 proposal wave 9, lane gc9, GC819-3; kept OFF at the round's close: no
/// probe shape shows a Java-visible difference -- the JMX builder already
/// reports the unattributed monitor at the innermost frame, which is the
/// callee in every observed shape -- and the instance arm reads the receiver
/// from local 0, which a body that stores to local 0 would misname): an activation of a CALLER-HELD synchronized body -- a
/// compiled `ACC_SYNCHRONIZED` method's monitor-free body
/// (`CompiledMethod::requires_wrapped_entry`), whose monitor the caller took
/// for it: a compiled caller's caller-held direct CALL (`ir_lower` /
/// `x64/op_invoke.rs`, the `sync_direct_row_key` rows) or the interpreter's
/// wrapped entry (`jit_bridge::JitSynchronizedMonitorGuard`) -- names that
/// method monitor at its OWN position, as HotSpot's
/// `javaVFrame::locked_monitors` lists a synchronized method's monitor at its
/// frame ([`caller_held_method_monitor_word`]). Off: the monitor is attributed
/// to no frame (depth `-1`), as in wave 8. Read once per snapshot or publish
/// that has an activation which can hold something.
///
/// The synchronized SPLICE window the proposal also names needs nothing: its
/// body admits no call, back edge or trapping node (`ir.rs`, the three gates
/// above `SyncSpliceWindow`), so no safepoint lies inside the window and no
/// stack observation can find the thread holding the splice's monitor.
pub(crate) fn caller_held_method_monitor_depths_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JMX_CALLER_HELD_METHOD_MONITOR_DEPTHS")
}

/// `CRATONVM_JMX_CALLER_HELD_IR_RECEIVER_HOME`, **opt-in** (JIT round 19
/// proposal wave 11, lane gc11, GC920-1; read only under
/// `CRATONVM_JMX_CALLER_HELD_METHOD_MONITOR_DEPTHS`): an OPTIMIZING-tier
/// caller-held instance body names its receiver too, read from `Param(0)`'s
/// prologue home `[rbp - 8]` through the checked frame-word read.
///
/// No JIT change is needed for it: `ir_lower` already republishes EVERY
/// reference parameter's prologue home in every safepoint map
/// (`Lowerer::ref_param_homes`, "the one class of live oop that is not a node
/// result"), the same fact `Lowerer::self_lock_obj_home` relies on for a
/// self-locking body, and nothing but the prologue writes that home (SSA
/// locals never store back to it; the dead-home clears touch only the spill
/// band). So the home holds the receiver the caller locked for the whole
/// activation -- even for a body that stores to local 0, unlike the
/// single-pass `[rbp - 8]`, which IS local 0. A graph with no `Ref`
/// `Param(0)` node publishes no home there, and the read (every active map
/// must name the word) refuses. Off: such an activation names nothing, as in
/// wave 9 (the monitor stays unattributed: the innermost frame in the JMX
/// builder). Read per caller-held IR instance activation, on the cold
/// JMX/JVMTI paths only.
pub(crate) fn caller_held_ir_receiver_home_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JMX_CALLER_HELD_IR_RECEIVER_HOME")
}

/// Is `cm` a caller-held synchronized body whose activation GC819-3 lets name
/// its method monitor? A monitor-free body of an `ACC_SYNCHRONIZED` method
/// (`requires_wrapped_entry`: legal to enter only through a caller that holds
/// the monitor for it), not a self-locking one (that one names its own hold,
/// `self_lock_hold`), and not an OSR body (its interpreter frame's
/// `monitor_on_exit` is the record, and the OSR dedupe keeps that frame).
fn caller_held_method_body(cm: &cratonvm_jit::CompiledMethod) -> bool {
    cm.requires_wrapped_entry
        && !cm.self_locks_monitor
        && cm.self_lock_hold.is_none()
        && !cm.compiled_via_osr
}

/// Is the method of `cm` static, by its stamped Java argument count against
/// its label's descriptor (`CompiledMethod::entry_java_args`, the fact the
/// not-entrant record decides the same question with)? `None` when either is
/// missing or they disagree.
fn caller_held_body_is_static(cm: &cratonvm_jit::CompiledMethod) -> Option<bool> {
    if cm.entry_java_args == u16::MAX {
        return None;
    }
    let (_, descriptor) = cm.method_label.rsplit_once(':')?;
    if !descriptor.starts_with('(') {
        return None;
    }
    let declared = cratonvm_jit::count_param_slots(descriptor);
    let n = usize::from(cm.entry_java_args);
    if n == declared {
        Some(true)
    } else if n == declared + 1 {
        Some(false)
    } else {
        None
    }
}

/// GC819-3 (JIT round 19 proposal wave 9, lane gc9): the method monitor a
/// caller-held synchronized body's activation `f` runs under
/// ([`caller_held_method_body`]), as an address, or `None`. A static method's
/// is its declaring class's mirror (`mirror(f.owner_class_id)`, the class the
/// body is published under; the caller-held route binds only a method the
/// Methodref's own class declares, and the wrapped entry locks
/// `declaring_class_id`'s mirror). An instance method's is its receiver, read
/// only for a SINGLE-PASS body with `frame_words`, from local 0's home
/// `[rbp - 8]` through the checked frame-word read (the active map must name
/// it; a register-homed local 0 only under
/// `CRATONVM_JMX_REGISTER_LOCAL_BLOCK_HOLDS`); an optimizing-tier body names
/// its receiver only under `CRATONVM_JMX_CALLER_HELD_IR_RECEIVER_HOME`
/// (GC920-1, round 19 wave 11, lane gc11), from `Param(0)`'s prologue home
/// `[rbp - 8]`, which every IR safepoint map republishes
/// ([`caller_held_ir_receiver_home_enabled`]); off, it names nothing.
/// `reader` (GC920-4) says whose stack the frame words are read from
/// ([`crate::jit::conservative_roots::FrameWordReader`]). Every answer is a CANDIDATE the
/// caller matches against the thread's lock stack. Residual: a body that
/// stores into local 0 (no javac output does) could name another held object
/// at this depth -- a depth, never a reference.
///
/// # Safety
///
/// As [`compiled_frame_hold_words`].
unsafe fn caller_held_method_monitor_word(
    f: &crate::jit::conservative_roots::ActiveCompiledFrame,
    cm: &cratonvm_jit::CompiledMethod,
    frame_words: bool,
    reader: crate::jit::conservative_roots::FrameWordReader,
    mirror: impl Fn(u32) -> Option<usize>,
) -> Option<usize> {
    if !caller_held_method_body(cm) {
        return None;
    }
    match caller_held_body_is_static(cm)? {
        true => mirror(f.owner_class_id),
        // GC920-1 (round 19 wave 11, lane gc11): an optimizing-tier body's
        // receiver from `Param(0)`'s prologue home, which every map names
        // ([`caller_held_ir_receiver_home_enabled`]).
        false if frame_words && cm.used_ir_backend => {
            if !caller_held_ir_receiver_home_enabled() {
                return None;
            }
            crate::jit::conservative_roots::compiled_frame_reference_word_by(f, cm, 8, reader)
        }
        false if frame_words && !cm.used_ir_backend => {
            // A register-homed local 0 is read from its canonical slot under
            // the same rule as a block's lock local (the pre-call spill
            // published it; the map must name it).
            let in_register = !crate::jit::conservative_roots::register_local_block_holds_enabled()
                && cm
                    .osr_local_assignments
                    .as_ref()
                    .is_some_and(|regs| regs.first().is_some_and(Option::is_some));
            if in_register {
                return None;
            }
            crate::jit::conservative_roots::compiled_frame_reference_word_by(f, cm, 8, reader)
        }
        false => None,
    }
}

/// The monitors compiled activation `f` holds as its own, as ADDRESSES,
/// innermost first in HotSpot's `javaVFrame::locked_monitors` order: its
/// `synchronized` blocks newest first, then its method monitor (the class
/// mirror of a `static synchronized` self-locking body, from `mirror`; the
/// receiver of an instance one). With `frame_words` off, only the mirror.
/// An OPTIMIZING-tier activation's receiver and blocks are read only with
/// `ir_homes` too (GC719-1, round 19 wave 8, lane gc8;
/// [`ir_frame_monitor_homes_enabled`], which callers read once). With
/// `caller_held` (GC819-3, round 19 wave 9, lane gc9;
/// [`caller_held_method_monitor_depths_enabled`]), a caller-held synchronized
/// body's method monitor in the method-monitor position
/// ([`caller_held_method_monitor_word`]).
///
/// The receiver and the blocks are frame words, read only through
/// `conservative_roots::compiled_frame_reference_word` (where the frame's
/// active oop map names the word); each is a CANDIDATE a caller must match
/// against the thread's lock stack before using it, and nothing here makes
/// an object of it. JIT round 19 proposal wave 7, lane gc7 (SYNC619-1).
///
/// # Safety
///
/// `f` must come from a walk of the CALLING thread whose frames have not
/// changed since (`f.cm_ptr` live, `f.frame_rbp` its frame), or have
/// `cm_ptr == 0`.
pub(crate) unsafe fn compiled_frame_hold_words(
    f: &crate::jit::conservative_roots::ActiveCompiledFrame,
    frame_words: bool,
    ir_homes: bool,
    caller_held: bool,
    mirror: impl Fn(u32) -> Option<usize>,
) -> SmallVec<[usize; 4]> {
    // SAFETY: the caller's contract, forwarded.
    unsafe {
        compiled_frame_hold_words_by(
            f,
            frame_words,
            ir_homes,
            caller_held,
            crate::jit::conservative_roots::FrameWordReader::Own,
            mirror,
        )
    }
}

/// [`compiled_frame_hold_words`] with the frame-word reader named (JIT round
/// 19 proposal wave 11, lane gc11, GC920-4): `FrameWordReader::HeldThread`
/// reads an activation of ANOTHER thread that a frame-trace pause holds in a
/// native, each word within the activation's own band.
///
/// # Safety
///
/// For `FrameWordReader::Own`, as [`compiled_frame_hold_words`]. For
/// `FrameWordReader::HeldThread`, `f` must come from a walk of a copy of a
/// HELD thread's chain (`JitChainSnapshot::of_held_thread`) taken in the
/// pause that still holds it, so `f.cm_ptr` is live and `f.frame_rbp` its
/// unchanged frame.
pub(crate) unsafe fn compiled_frame_hold_words_by(
    f: &crate::jit::conservative_roots::ActiveCompiledFrame,
    frame_words: bool,
    ir_homes: bool,
    caller_held: bool,
    reader: crate::jit::conservative_roots::FrameWordReader,
    mirror: impl Fn(u32) -> Option<usize>,
) -> SmallVec<[usize; 4]> {
    use crate::jit::conservative_roots as cr;
    let mut out: SmallVec<[usize; 4]> = SmallVec::new();
    // SAFETY: the caller's contract.
    let Some(cm) = (unsafe { (f.cm_ptr as *const cratonvm_jit::CompiledMethod).as_ref() }) else {
        return out;
    };
    if frame_words {
        for word in cr::compiled_frame_block_lock_words_by(f, cm, ir_homes, reader)
            .into_iter()
            .rev()
        {
            if !out.contains(&word) {
                out.push(word);
            }
        }
    }
    let method = match cm.self_lock_hold {
        Some(cratonvm_jit::SelfLockHold::Static { .. }) => mirror(f.owner_class_id),
        Some(cratonvm_jit::SelfLockHold::Instance {
            obj_rbp_off: Some(off),
        }) if frame_words && (!cm.used_ir_backend || ir_homes) => {
            cr::compiled_frame_reference_word_by(f, cm, off, reader)
        }
        // GC819-3 (round 19 wave 9, lane gc9): a caller-held synchronized
        // body's method monitor, which its caller holds for it.
        // SAFETY: the caller's contract, forwarded.
        None if caller_held => unsafe {
            caller_held_method_monitor_word(f, cm, frame_words, reader, &mirror)
        },
        _ => None,
    };
    if let Some(word) = method {
        if !out.contains(&word) {
            out.push(word);
        }
    }
    out
}

/// Whose a position of the calling thread's trace is, as
/// [`trace_position_owners`] names it: interpreter frame `frames[i]`, the
/// compiled activation whose JIT entry-chain index is `chain`
/// (`ActiveCompiledFrame::chain_index`, the index a debugger listing's
/// compiled row carries), or an inlined callee.
#[cfg(feature = "experimental-debug")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TracePositionOwner {
    Interp(usize),
    Compiled(u32),
    Inlined,
}

/// The owner of each position of the trace [`frame_locked_monitors_with_compiled`]
/// attributes over, for the CALLING thread (interpreter round i6 wave 120,
/// lane L1): the same walk and the same layout, so position `c` of a pair it
/// answers is `owners[c]`. The C JVMTI table's shared listing
/// (`native_env::listed_rows` under `CRATONVM_JVMTI_C_LISTING_SHARED`) maps
/// its rows to trace positions by it, interpreter frame by index and
/// compiled activation by chain index, so its owned-monitor depths need no
/// trace of their own. Empty with no compiled code active.
#[cfg(feature = "experimental-debug")]
pub(crate) fn trace_position_owners(frames: &[Frame]) -> Vec<TracePositionOwner> {
    use crate::jit::conservative_roots as cr;
    if cr::current_thread_jit_depth() == 0 {
        return Vec::new();
    }
    let jit = cr::active_compiled_frames_whole_opts(
        false,
        cr::CompiledWalkOptions {
            shared_inline_chains: cr::throws4_walk_shared_inline_chains_enabled(),
            activation_memo: cr::throws2_activation_memo_enabled(),
        },
    );
    let (jit, osr_bci, _) =
        crate::runtime::stackwalker::drop_osr_continuations_keeping(frames, jit, false);
    compiled_layout(frames, &jit, &osr_bci)
        .into_iter()
        .map(|(owner, _)| match owner {
            TraceOwner::Interp(i) => TracePositionOwner::Interp(i),
            TraceOwner::Compiled(k) => TracePositionOwner::Compiled(jit[k].chain_index),
            TraceOwner::Inlined => TracePositionOwner::Inlined,
        })
        .collect()
}

/// The trace layout `stackwalker::interleave_compiled_frames` builds over
/// `frames` and the walk `jit` (after the OSR dedupe): one `(owner, method
/// name)` per trace entry, outermost first.
fn compiled_layout<'a>(
    frames: &'a [Frame],
    jit: &'a [crate::jit::conservative_roots::ActiveCompiledFrame],
    osr_bci: &'a crate::runtime::stackwalker::OsrBciOverrides,
) -> Vec<(TraceOwner, &'a str)> {
    let mut layout: Vec<(TraceOwner, &str)> = Vec::with_capacity(frames.len() + jit.len());
    fn push_compiled<'a>(
        layout: &mut Vec<(TraceOwner, &'a str)>,
        jit: &'a [crate::jit::conservative_roots::ActiveCompiledFrame],
        k: usize,
    ) {
        if let Some((_, method)) = label_class_and_method(&jit[k].label) {
            layout.push((TraceOwner::Compiled(k), method));
            for m in inlined_levels_named(jit[k].chain()) {
                layout.push((TraceOwner::Inlined, m));
            }
        }
    }
    let mut next = 0usize;
    for (i, frame) in frames.iter().enumerate() {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled(&mut layout, jit, next);
            next += 1;
        }
        layout.push((TraceOwner::Interp(i), frame.method_name()));
        if let Some((_, chain)) = osr_bci.get(&i) {
            for m in inlined_levels_named(chain) {
                layout.push((TraceOwner::Inlined, m));
            }
        }
    }
    while next < jit.len() {
        push_compiled(&mut layout, jit, next);
        next += 1;
    }
    layout
}

/// JIT round 19 proposal wave 7, lane gc7 (SYNC619-2,
/// `CRATONVM_JMX_PUBLISHED_COMPILED_HOLDS`, default ON): the monitors the
/// compiled activations of the CALLING thread hold as their own, at their
/// positions in `entries` -- the trace this thread is about to publish,
/// captured from the same `frames` / `jit` / `osr_bci`
/// (`stackwalker::capture_published_trace`) -- as `(entry index, address)`
/// pairs, innermost first ([`compiled_frame_hold_words`] per activation).
///
/// The addresses are CANDIDATES: `ThreadRegistry::publish_jmx_frame_monitors`
/// keeps one only when it is one of the thread's lock-stack objects (matched
/// by address, stored as that lock-stack slot, which a moving collection
/// remaps in place), so no raw reference outlives the publish. Empty -- the
/// wave-6 answer, compiled holds at no depth -- when no activation can hold
/// anything (one field test each, no flag read), the switch is off, or the
/// layout disagrees with `entries` in length or any name.
pub(crate) fn published_compiled_holds(
    shared: &SharedVm,
    frames: &[Frame],
    jit: &[crate::jit::conservative_roots::ActiveCompiledFrame],
    osr_bci: &crate::runtime::stackwalker::OsrBciOverrides,
    osr_holders: &[(usize, crate::jit::conservative_roots::ActiveCompiledFrame)],
    entries: &[crate::native::registry::StackTraceEntry],
) -> Vec<(u32, usize)> {
    // SAFETY: `jit` is this thread's own walk, its frames live (the caller is
    // publishing the trace it just captured from them).
    if (osr_holders.is_empty() && !jit.iter().any(|f| unsafe { compiled_frame_may_hold(f) }))
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JMX_PUBLISHED_COMPILED_HOLDS")
    {
        return Vec::new();
    }
    // SAFETY: as above (`osr_holders` are activations of the same walk).
    unsafe {
        compiled_holds_over_layout(
            shared,
            frames,
            jit,
            osr_bci,
            osr_holders,
            entries,
            crate::jit::conservative_roots::FrameWordReader::Own,
        )
    }
}

/// `CRATONVM_JMX_HELD_NATIVE_COMPILED_HOLDS`, **default ON** since the wave 11
/// integration (`=0` kill; a HotSpot parity fix: `R19Sync6LockedDepths`
/// `other-thread-hot` equals HotSpot 25 in both modes only with it) (JIT round 19
/// proposal wave 11, lane gc11, GC920-4 / GC1119-2): the frame-trace pause that stores
/// the trace of a thread it holds excused in a native it entered under
/// COMPILED code (`gc_and_alloc::publish_excused_native_traces`,
/// `NX_COMPILED`) publishes that thread's compiled activations' own holds
/// with the per-frame attribution ([`held_thread_compiled_holds`]), as the
/// thread's own publish does ([`published_compiled_holds`]). Off: the pause
/// publishes the interpreter frames' monitors only, as in wave 9 (a block a
/// compiled `tCold` holds reports at the innermost frame). Read once per
/// excused thread whose walk has an activation that can hold something.
pub(crate) fn held_native_compiled_holds_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JMX_HELD_NATIVE_COMPILED_HOLDS")
}

/// `CRATONVM_JMX_OSR_BODY_BLOCK_HOLDS`, **default ON** since JIT round 19
/// wave 14 (GC1219-1: `R19Sync6LockedDepths` `other-thread-osr` matched
/// HotSpot in 5/6 runs with it and 0/6 without; the miss is the shared
/// `other-thread-hot` race), `=0` the kill switch (proposal wave 12, lane
/// gc12, GC1119-1): a `synchronized` block an OSR body took
/// AFTER its entry is attributed to the interpreter frame that body
/// continues ([`osr_body_block_words`]). Without it the trace builders drop
/// the OSR continuation in favour of that frame
/// (`stackwalker::drop_osr_continuations`), whose `held_monitors` record only
/// what the interpreter took before the transfer (a compiled
/// `monitorenter` records into no frame), and the block's monitor reports at
/// the innermost frame. For the calling thread's JMX/JVMTI read, its own
/// publish, and (with `CRATONVM_JMX_HELD_NATIVE_COMPILED_HOLDS`) a held
/// thread's. Read once per capture that may keep the continuations.
pub(crate) fn osr_body_block_holds_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JMX_OSR_BODY_BLOCK_HOLDS")
}

/// GC1119-1 (round 19 wave 12, lane gc12): the block lock words of the OSR
/// continuation `holder` that the interpreter frame `frame` it continues does
/// not already record (its `held_monitors` / `monitor_on_exit`: the blocks
/// taken before the transfer, which the OSR body's own description at its
/// safepoint also names), innermost first. Candidates, as every frame word.
///
/// # Safety
///
/// As [`compiled_frame_hold_words_by`] for `holder` and `reader`.
pub(crate) unsafe fn osr_body_block_words(
    holder: &crate::jit::conservative_roots::ActiveCompiledFrame,
    frame: &Frame,
    ir_homes: bool,
    reader: crate::jit::conservative_roots::FrameWordReader,
) -> Vec<usize> {
    // SAFETY: the caller's contract.
    let Some(cm) = (unsafe { (holder.cm_ptr as *const cratonvm_jit::CompiledMethod).as_ref() })
    else {
        return Vec::new();
    };
    let recorded = |word: usize| {
        frame
            .held_monitors
            .as_slice()
            .iter()
            .chain(frame.monitor_on_exit.as_ref())
            .any(|o| o.as_ptr() as usize == word)
    };
    let mut out: Vec<usize> = Vec::new();
    for word in crate::jit::conservative_roots::compiled_frame_block_lock_words_by(
        holder, cm, ir_homes, reader,
    )
    .into_iter()
    .rev()
    {
        if !recorded(word) && !out.contains(&word) {
            out.push(word);
        }
    }
    out
}

/// GC920-4 (JIT round 19 proposal wave 11, lane gc11): [`published_compiled_holds`]
/// for ANOTHER thread, held by a frame-trace pause in a native it entered
/// under compiled code: `jit` is the walk of a copy of its chain
/// (`JitChainSnapshot::of_held_thread`), `frames` / `osr_bci` its
/// interpreter frames and OSR overrides, `entries` the trace the pause just
/// captured from them (before any spliced rows, which shift the answer as
/// they shift the thread's own). Each frame word is read within its
/// activation's own band (`FrameWordReader::HeldThread`), never against the
/// reader's stack pointer. The same candidates contract: the publish keeps a
/// word only when it IS one of the held thread's lock-stack objects. Empty
/// unless [`held_native_compiled_holds_enabled`].
///
/// # Safety
///
/// The pause must still hold the thread excused in its native (it runs no
/// Java and no compiled code, so the activations `jit` names and their
/// artifacts stay as walked).
pub(crate) unsafe fn held_thread_compiled_holds(
    shared: &SharedVm,
    frames: &[Frame],
    jit: &[crate::jit::conservative_roots::ActiveCompiledFrame],
    osr_bci: &crate::runtime::stackwalker::OsrBciOverrides,
    osr_holders: &[(usize, crate::jit::conservative_roots::ActiveCompiledFrame)],
    entries: &[crate::native::registry::StackTraceEntry],
) -> Vec<(u32, usize)> {
    // SAFETY: the caller's contract (the held thread's artifacts are live).
    if (osr_holders.is_empty() && !jit.iter().any(|f| unsafe { compiled_frame_may_hold(f) }))
        || !held_native_compiled_holds_enabled()
    {
        return Vec::new();
    }
    // SAFETY: the caller's contract.
    let out = unsafe {
        compiled_holds_over_layout(
            shared,
            frames,
            jit,
            osr_bci,
            osr_holders,
            entries,
            crate::jit::conservative_roots::FrameWordReader::HeldThread,
        )
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JMX_HELD_COMPILED_HOLDS] activations={} entries={} holds={:x?}",
            jit.len(),
            entries.len(),
            out
        );
    }
    out
}

/// The compiled activations' own holds at their positions in `entries`
/// (innermost first), the shared body of [`published_compiled_holds`] and
/// [`held_thread_compiled_holds`]; empty when the layout disagrees with
/// `entries`.
///
/// # Safety
///
/// `jit`'s activations must be live and unchanged, read as `reader` says
/// ([`compiled_frame_hold_words_by`]).
unsafe fn compiled_holds_over_layout(
    shared: &SharedVm,
    frames: &[Frame],
    jit: &[crate::jit::conservative_roots::ActiveCompiledFrame],
    osr_bci: &crate::runtime::stackwalker::OsrBciOverrides,
    osr_holders: &[(usize, crate::jit::conservative_roots::ActiveCompiledFrame)],
    entries: &[crate::native::registry::StackTraceEntry],
    reader: crate::jit::conservative_roots::FrameWordReader,
) -> Vec<(u32, usize)> {
    let layout = compiled_layout(frames, jit, osr_bci);
    if !layout_matches_trace(&layout, frames, entries, entries.len(), &[]) {
        return Vec::new();
    }
    let words = compiled_frame_word_holds_enabled();
    // GC719-1 (round 19 wave 8, lane gc8).
    let ir_homes = words && ir_frame_monitor_homes_enabled();
    // GC819-3 (round 19 wave 9, lane gc9).
    let caller_held = caller_held_method_monitor_depths_enabled();
    // Tried, never waited for: a publish may run where this thread holds the
    // mirror table's lock. Busy, a static hold goes unnamed.
    let mirrors = shared.classes.class_mirrors.try_read();
    let mirror = |class_id: u32| {
        mirrors
            .as_ref()?
            .get(&crate::classloading::ClassId::new(class_id))
            .map(|m| m.as_ptr() as usize)
    };
    let mut out = Vec::new();
    for (c, &(owner, _)) in layout.iter().enumerate().rev() {
        let Ok(c32) = u32::try_from(c) else {
            return Vec::new();
        };
        match owner {
            TraceOwner::Compiled(k) => {
                // SAFETY: the caller's contract.
                let held = unsafe {
                    compiled_frame_hold_words_by(
                        &jit[k],
                        words,
                        ir_homes,
                        caller_held,
                        reader,
                        mirror,
                    )
                };
                out.extend(held.into_iter().map(|word| (c32, word)));
            }
            // GC1119-1 (round 19 wave 12, lane gc12): the blocks the OSR body
            // continuing this interpreter frame took after its entry.
            TraceOwner::Interp(i) if words => {
                for (_, holder) in osr_holders.iter().filter(|(at, _)| *at == i) {
                    // SAFETY: the caller's contract covers the dropped
                    // continuation too (the same live activation).
                    let held =
                        unsafe { osr_body_block_words(holder, &frames[i], ir_homes, reader) };
                    out.extend(held.into_iter().map(|word| (c32, word)));
                }
            }
            _ => {}
        }
    }
    out
}

/// Does `layout` ([`compiled_layout`]) describe `trace`, captured with
/// `captured` entries and served frames inserted before the captured indices
/// `inserted_before`? Length, every method name, and every interpreter
/// frame's class (captured index `c` sits at `c + |{at <= c}|` after the
/// served insertions).
fn layout_matches_trace(
    layout: &[(TraceOwner, &str)],
    frames: &[Frame],
    trace: &[crate::native::registry::StackTraceEntry],
    captured: usize,
    inserted_before: &[usize],
) -> bool {
    let ok = layout_matches_trace_inner(layout, frames, trace, captured, inserted_before);
    if !ok && crate::runtime::env_cache::frame_trace() {
        // Round 19 wave 9 (lane gc9): name the disagreement that drops the
        // whole attribution (every monitor then reports at the innermost
        // frame), so a probe row can be traced to it.
        let layout_names: Vec<&str> = layout.iter().map(|&(_, m)| m).collect();
        let trace_names: Vec<&str> = trace.iter().map(|e| &*e.method_name).collect();
        eprintln!(
            "[JMX_MONITOR_LAYOUT] mismatch captured={captured} inserted={inserted_before:?} \
             frames={} layout={layout_names:?} trace={trace_names:?}",
            frames.len()
        );
    }
    ok
}

fn layout_matches_trace_inner(
    layout: &[(TraceOwner, &str)],
    frames: &[Frame],
    trace: &[crate::native::registry::StackTraceEntry],
    captured: usize,
    inserted_before: &[usize],
) -> bool {
    if layout.len() != captured {
        return false;
    }
    layout.iter().enumerate().all(|(c, &(owner, method))| {
        let shift = inserted_before.iter().filter(|&&at| at <= c).count();
        let Some(entry) = trace.get(c + shift) else {
            return false;
        };
        &*entry.method_name == method
            && match owner {
                TraceOwner::Interp(i) => &*entry.class_name == frames[i].class_name(),
                _ => true,
            }
    })
}

/// The layout and attribution of [`frame_locked_monitors_with_compiled`]
/// over a given walk (`jit`, after the OSR dedupe), with `compiled_holds`
/// naming the monitors a compiled activation holds, innermost first.
fn attribute_over_compiled_layout(
    frames: &[Frame],
    jit: &[crate::jit::conservative_roots::ActiveCompiledFrame],
    osr_bci: &crate::runtime::stackwalker::OsrBciOverrides,
    trace: &[crate::native::registry::StackTraceEntry],
    captured: usize,
    inserted_before: &[usize],
    osr_body_holds: impl Fn(usize) -> Vec<ObjectRef>,
    compiled_holds: impl Fn(&crate::jit::conservative_roots::ActiveCompiledFrame) -> Vec<ObjectRef>,
) -> Option<Vec<(usize, ObjectRef)>> {
    let layout = compiled_layout(frames, jit, osr_bci);
    if !layout_matches_trace(&layout, frames, trace, captured, inserted_before) {
        return None;
    }
    let mut out = Vec::new();
    for (c, &(owner, _)) in layout.iter().enumerate().rev() {
        match owner {
            TraceOwner::Interp(i) => {
                let frame = &frames[i];
                // GC1119-1: the newer blocks an OSR body continuing this frame
                // took first (innermost first; empty unless kept).
                out.extend(osr_body_holds(i).into_iter().map(|o| (c, o)));
                out.extend(frame.held_monitors.as_slice().iter().rev().map(|&o| (c, o)));
                if let Some(o) = frame.monitor_on_exit {
                    out.push((c, o));
                }
            }
            TraceOwner::Compiled(k) => {
                out.extend(compiled_holds(&jit[k]).into_iter().map(|hold| (c, hold)));
            }
            TraceOwner::Inlined => {}
        }
    }
    Some(out)
}

#[cfg(test)]
mod r19_sync6_compiled_layout_tests {
    use super::*;
    use crate::jit::conservative_roots::{ActiveCompiledFrame, InlinedLevel};
    use crate::native::registry::StackTraceEntry;
    use std::sync::Arc;

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the attribution only compares.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// No OSR body holds (GC1119-1's argument when nothing was kept).
    fn none(_: usize) -> Vec<ObjectRef> {
        Vec::new()
    }

    /// GC1119-1 (round 19 wave 12, lane gc12): a block an OSR body took
    /// after its entry lands at the interpreter frame it continues, before
    /// (innermost of) the blocks that frame recorded itself.
    #[test]
    fn an_osr_bodys_newer_block_lands_at_the_frame_it_continues() {
        let (a, p) = (obj(0x1000), obj(0x2000));
        let mut main = interp("main");
        main.held_monitors.push(a);
        let frames = vec![main, interp("hot")];
        let jit: Vec<ActiveCompiledFrame> = Vec::new();
        let trace = vec![entry("probe/T", "main"), entry("probe/T", "hot")];
        let osr = Default::default();
        let osr_holds = |i: usize| if i == 0 { vec![p] } else { Vec::new() };
        let pairs =
            attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 2, &[], osr_holds, |_| {
                Vec::new()
            })
            .expect("the layout matches");
        assert_eq!(pairs, vec![(0, p), (0, a)]);
        let (monitors, depths) = attribute_locked_monitors(2, vec![a, p], Some(pairs), None);
        assert_eq!(monitors, vec![p, a]);
        assert_eq!(depths, vec![1, 1]);
    }

    fn interp(method: &str) -> Frame {
        Frame::new(
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
    }

    fn compiled(
        depth: u32,
        method: &str,
        owner: u32,
        chain: Vec<InlinedLevel>,
    ) -> ActiveCompiledFrame {
        ActiveCompiledFrame {
            inline_chain_shared: None,
            interp_depth: depth,
            chain_index: 0,
            label: Arc::from(format!("probe/T.{method}:()V")),
            owner_class_id: owner,
            cm_ptr: 0,
            bci: 1,
            inline_chain: chain,
            chain_exact: true,
            trap_site_exact: false,
            chain_poisoned: false,
            compile_cp_stamp: None,
            fast_throw_c2_tier: false,
            frame_rbp: None,
        }
    }

    fn entry(class: &str, method: &str) -> StackTraceEntry {
        StackTraceEntry {
            class_name: Arc::from(class),
            method_name: Arc::from(method),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: 0,
            class_id: None,
            method_index: None,
        }
    }

    /// `main` (interpreted, holding `a` in a block) -> `hot` (compiled) ->
    /// `sync` (compiled static self-lock, holding the mirror `m`, with one
    /// inlined callee) -> `report` (interpreted, the innermost). Every
    /// monitor lands at its own frame's trace index, innermost first; a
    /// disagreeing trace answers `None`.
    #[test]
    fn interpreted_and_static_compiled_holds_land_at_their_own_positions() {
        let (a, m) = (obj(0x1000), obj(0x2000));
        let mut main = interp("main");
        main.held_monitors.push(a);
        let frames = vec![main, interp("report")];
        let inl = InlinedLevel {
            label: Arc::from("probe/T.inl:()V"),
            bci: 2,
            class_id: 0,
            hidden: None,
        };
        let jit = vec![
            compiled(1, "hot", 9, Vec::new()),
            compiled(1, "sync", 7, vec![inl]),
        ];
        let trace = vec![
            entry("probe/T", "main"),
            entry("probe/T", "hot"),
            entry("probe/T", "sync"),
            entry("probe/T", "inl"),
            entry("probe/T", "report"),
        ];
        let osr = Default::default();
        let hold = |f: &ActiveCompiledFrame| {
            if f.owner_class_id == 7 {
                vec![m]
            } else {
                Vec::new()
            }
        };
        let pairs = attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 5, &[], none, hold)
            .expect("the layout matches the trace");
        assert_eq!(pairs, vec![(2, m), (0, a)]);
        let (monitors, depths) = attribute_locked_monitors(5, vec![a, m], Some(pairs), None);
        assert_eq!(monitors, vec![m, a]);
        assert_eq!(depths, vec![2, 4]);

        // A served frame inserted before captured index 0 shifts the check.
        let mut served = trace.clone();
        served.insert(0, entry("java/lang/Thread", "run"));
        assert!(
            attribute_over_compiled_layout(&frames, &jit, &osr, &served, 5, &[0], none, hold)
                .is_some()
        );
        // A name disagreement, or a length one, falls back to `None`.
        let mut wrong = trace.clone();
        wrong[1] = entry("probe/T", "other");
        assert!(
            attribute_over_compiled_layout(&frames, &jit, &osr, &wrong, 5, &[], none, hold)
                .is_none()
        );
        assert!(
            attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 4, &[], none, hold)
                .is_none()
        );
        let mut class = trace.clone();
        class[0] = entry("probe/U", "main");
        assert!(
            attribute_over_compiled_layout(&frames, &jit, &osr, &class, 5, &[], none, hold)
                .is_none()
        );
    }

    /// SYNC619-1 (round 19 wave 7, lane gc7): what one activation names. A
    /// static self-locking body: its class mirror, with or without frame
    /// words. An instance one: its receiver only through a frame word, so
    /// nothing with the words off or without a frame pointer. No artifact
    /// (`cm_ptr == 0`), or a body that holds nothing: nothing.
    #[test]
    fn an_activation_names_its_mirror_and_only_frame_checked_words() {
        let artifact = |hold: Option<cratonvm_jit::SelfLockHold>| {
            let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
            buf.emit_byte(0xC3);
            let mut cm = cratonvm_jit::CompiledMethod::new(buf);
            cm.self_lock_hold = hold;
            cm
        };
        let at = |cm: &cratonvm_jit::CompiledMethod| {
            let mut f = compiled(1, "sync", 7, Vec::new());
            f.cm_ptr = cm as *const cratonvm_jit::CompiledMethod as usize;
            f
        };
        let mirror = |id: u32| (id == 7).then_some(0x2000usize);
        let stat = artifact(Some(cratonvm_jit::SelfLockHold::Static {
            mirror_word_rbp_off: Some(8),
        }));
        for words in [false, true] {
            // SAFETY: `cm_ptr` names `stat`, alive for the call; no frame read
            // is made (the static mirror needs none, and there is no RBP).
            let held =
                unsafe { compiled_frame_hold_words(&at(&stat), words, words, words, mirror) };
            assert_eq!(held.as_slice(), &[0x2000], "words={words}");
            // SAFETY: as above.
            assert!(unsafe { compiled_frame_may_hold(&at(&stat)) });
        }
        let inst = artifact(Some(cratonvm_jit::SelfLockHold::Instance {
            obj_rbp_off: Some(8),
        }));
        for words in [false, true] {
            // SAFETY: as above; `frame_rbp` is `None`, so no word is read.
            let held =
                unsafe { compiled_frame_hold_words(&at(&inst), words, words, words, mirror) };
            assert!(held.is_empty(), "words={words}");
        }
        let plain = artifact(None);
        // SAFETY: as above.
        assert!(
            unsafe { compiled_frame_hold_words(&at(&plain), true, true, true, mirror) }.is_empty()
        );
        // SAFETY: as above.
        assert!(!unsafe { compiled_frame_may_hold(&at(&plain)) });
        let none = compiled(1, "x", 7, Vec::new());
        // SAFETY: `cm_ptr == 0` names no artifact.
        assert!(unsafe { compiled_frame_hold_words(&none, true, true, true, mirror) }.is_empty());
        // SAFETY: as above.
        assert!(!unsafe { compiled_frame_may_hold(&none) });
    }

    /// GC819-3 (round 19 wave 9, lane gc9): a caller-held synchronized body
    /// (`requires_wrapped_entry`, monitor-free) names its method monitor: a
    /// static one its class mirror with the switch on, nothing with it off;
    /// an instance one only through a frame word (none without a frame
    /// pointer); a self-locking, OSR, or arity-unknown body nothing extra.
    #[test]
    fn a_caller_held_synchronized_body_names_its_method_monitor() {
        let artifact = |java_args: u16, label: &str| {
            let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
            buf.emit_byte(0xC3);
            let mut cm = cratonvm_jit::CompiledMethod::new(buf);
            cm.requires_wrapped_entry = true;
            cm.entry_java_args = java_args;
            cm.method_label = label.to_string();
            cm
        };
        let at = |cm: &cratonvm_jit::CompiledMethod| {
            let mut f = compiled(1, "sync", 7, Vec::new());
            f.cm_ptr = cm as *const cratonvm_jit::CompiledMethod as usize;
            f
        };
        let mirror = |id: u32| (id == 7).then_some(0x2000usize);
        let stat = artifact(2, "probe/T.sync:(IJ)V");
        assert_eq!(caller_held_body_is_static(&stat), Some(true));
        // SAFETY: `cm_ptr` names `stat`, alive for the call; the static
        // mirror needs no frame read, and there is no RBP.
        assert!(unsafe { compiled_frame_may_hold(&at(&stat)) });
        for words in [false, true] {
            // SAFETY: as above.
            let held = unsafe { compiled_frame_hold_words(&at(&stat), words, words, true, mirror) };
            assert_eq!(held.as_slice(), &[0x2000], "words={words}");
            // SAFETY: as above.
            let off = unsafe { compiled_frame_hold_words(&at(&stat), words, words, false, mirror) };
            assert!(off.is_empty(), "words={words}: the kill switch");
        }
        let inst = artifact(3, "probe/T.sync:(IJ)V");
        assert_eq!(caller_held_body_is_static(&inst), Some(false));
        // SAFETY: as above; `frame_rbp` is `None`, so no word is read.
        assert!(
            unsafe { compiled_frame_hold_words(&at(&inst), true, true, true, mirror) }.is_empty()
        );
        for (what, cm) in [
            ("unknown arity", artifact(u16::MAX, "probe/T.sync:(IJ)V")),
            ("arity disagrees", artifact(5, "probe/T.sync:(IJ)V")),
            ("no descriptor", artifact(2, "probe/T.sync")),
        ] {
            assert_eq!(caller_held_body_is_static(&cm), None, "{what}");
            // SAFETY: as above.
            let held = unsafe { compiled_frame_hold_words(&at(&cm), true, true, true, mirror) };
            assert!(held.is_empty(), "{what}");
        }
        let mut osr = artifact(2, "probe/T.sync:(IJ)V");
        osr.compiled_via_osr = true;
        assert!(!caller_held_method_body(&osr));
        let mut self_locking = artifact(2, "probe/T.sync:(IJ)V");
        self_locking.self_locks_monitor = true;
        assert!(!caller_held_method_body(&self_locking));
        let mut plain = artifact(2, "probe/T.sync:(IJ)V");
        plain.requires_wrapped_entry = false;
        // SAFETY: as above.
        assert!(!unsafe { compiled_frame_may_hold(&at(&plain)) });
    }

    /// GC920-1 / GC920-4 (round 19 wave 11, lane gc11): a caller-held
    /// INSTANCE body read through the held-thread reader, over a synthetic
    /// band (`[rbp - 8]` the receiver home, `[rbp - 16]` the safepoint id,
    /// a 32-byte frame): a single-pass body names its receiver where the
    /// active map names the home; an optimizing-tier body names it only with
    /// `CRATONVM_JMX_CALLER_HELD_IR_RECEIVER_HOME` (unset here: nothing); no
    /// published frame size, nothing.
    #[test]
    fn a_caller_held_instance_receiver_is_read_through_the_held_reader() {
        use crate::jit::conservative_roots::FrameWordReader;
        const SP_ID: u32 = 3;
        const RECV: usize = 0x7200_0040;
        let mut band = Box::new([0usize; 16]);
        let rbp = band.as_ptr() as usize + 8 * 12;
        band[11] = RECV;
        band[10] = SP_ID as usize;
        let artifact = |ir: bool, size: i32| {
            let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
            buf.emit_byte(0xC3);
            let mut cm = cratonvm_jit::CompiledMethod::new(buf);
            cm.requires_wrapped_entry = true;
            cm.entry_java_args = 2;
            cm.method_label = "probe/T.spin:(J)J".to_string();
            cm.used_ir_backend = ir;
            cm.sp_id_slot_off = 16;
            cm.frame_layout.frame_size = size;
            let mut m = cratonvm_jit::OopMapEntry::new(0x10);
            m.bytecode_pc = SP_ID;
            m.frame_slot_offsets = vec![8];
            cm.push_oop_map(m);
            cm
        };
        let at = |cm: &cratonvm_jit::CompiledMethod| {
            let mut f = compiled(1, "spin", 7, Vec::new());
            f.cm_ptr = cm as *const cratonvm_jit::CompiledMethod as usize;
            f.frame_rbp = Some(rbp);
            f
        };
        let mirror = |_: u32| None;
        let held = FrameWordReader::HeldThread;
        let sp = artifact(false, 32);
        assert_eq!(caller_held_body_is_static(&sp), Some(false));
        // SAFETY: `cm_ptr` names `sp`, alive for the call; `frame_rbp` lies
        // inside `band`, which outlives it, and the read stays in its frame.
        let words =
            unsafe { compiled_frame_hold_words_by(&at(&sp), true, true, true, held, mirror) };
        assert_eq!(words.as_slice(), &[RECV]);
        // SAFETY: as above.
        let off =
            unsafe { compiled_frame_hold_words_by(&at(&sp), true, true, false, held, mirror) };
        assert!(off.is_empty(), "the GC819-3 switch");
        let unsized_frame = artifact(false, 0);
        let unsized_at = at(&unsized_frame);
        // SAFETY: as above.
        let none =
            unsafe { compiled_frame_hold_words_by(&unsized_at, true, true, true, held, mirror) };
        assert!(none.is_empty(), "no published frame size");
        if !caller_held_ir_receiver_home_enabled() {
            let ir = artifact(true, 32);
            // SAFETY: as above.
            let ir_words =
                unsafe { compiled_frame_hold_words_by(&at(&ir), true, true, true, held, mirror) };
            assert!(ir_words.is_empty(), "GC920-1 is opt-in");
        }
        drop(band);
    }

    /// SYNC619-2: two holds of one compiled activation land at its position,
    /// innermost first, beside an interpreter frame's.
    #[test]
    fn several_holds_of_one_activation_keep_their_order_at_its_position() {
        let (a, b, c) = (obj(0x1000), obj(0x2000), obj(0x3000));
        let mut main = interp("main");
        main.held_monitors.push(a);
        let frames = vec![main];
        let jit = vec![compiled(1, "blocks", 7, Vec::new())];
        let trace = vec![entry("probe/T", "main"), entry("probe/T", "blocks")];
        let osr = Default::default();
        let holds = |_: &ActiveCompiledFrame| vec![c, b];
        let pairs =
            attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 2, &[], none, holds)
                .expect("the layout matches");
        assert_eq!(pairs, vec![(1, c), (1, b), (0, a)]);
        let layout = compiled_layout(&frames, &jit, &osr);
        assert!(layout_matches_trace(&layout, &frames, &trace, 2, &[]));
        assert!(!layout_matches_trace(&layout, &frames, &trace[..1], 1, &[]));
    }

    /// A compiled activation whose label does not parse contributes no
    /// position (the trace builders drop it), and its holds are not named.
    #[test]
    fn an_unparsable_compiled_label_takes_no_position() {
        let frames = vec![interp("main")];
        let mut bad = compiled(1, "x", 7, Vec::new());
        bad.label = Arc::from("no-separators");
        let jit = vec![bad];
        let trace = vec![entry("probe/T", "main")];
        let osr = Default::default();
        let pairs =
            attribute_over_compiled_layout(&frames, &jit, &osr, &trace, 1, &[], none, |_| {
                vec![obj(0x3000)]
            })
            .expect("one interpreter frame");
        assert!(pairs.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::{attribute_locked_monitors, HeldMonitors};
    use crate::types::ObjectRef;

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the record only compares and remaps.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    #[test]
    fn two_entries_stay_inline_and_a_third_spills() {
        let mut h = HeldMonitors::new();
        assert!(h.is_empty());
        h.push(obj(0x1000));
        h.push(obj(0x2000));
        assert!(!h.objs.spilled(), "0-2 monitors must not allocate");
        h.push(obj(0x3000));
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn remove_newest_takes_the_newest_matching_entry() {
        let mut h = HeldMonitors::new();
        h.push(obj(0x1000));
        h.push(obj(0x2000));
        h.push(obj(0x1000));
        assert!(h.remove_newest(obj(0x2000)));
        assert_eq!(h.as_slice(), &[obj(0x1000), obj(0x1000)]);
        assert!(h.remove_newest(obj(0x1000)));
        assert_eq!(h.count_of(obj(0x1000)), 1);
        assert!(
            !h.remove_newest(obj(0x4000)),
            "an object the frame never entered"
        );
        assert!(h.remove_newest(obj(0x1000)));
        assert!(h.is_empty());
        assert!(!h.remove_newest(obj(0x1000)));
    }

    #[test]
    fn remap_rewrites_each_entry_once() {
        let mut h = HeldMonitors::new();
        h.push(obj(0x1000));
        h.push(obj(0x2000));
        // A chained map (0x1000 -> 0x2000 -> 0x3000) must not move an entry
        // twice: 0x1000 lands on 0x2000, and 0x2000 on 0x3000.
        h.remap_with(|a| match a {
            0x1000 => Some(0x2000),
            0x2000 => Some(0x3000),
            _ => None,
        });
        assert_eq!(h.as_slice(), &[obj(0x2000), obj(0x3000)]);
        // A null target is never installed.
        h.remap_with(|_| Some(0));
        assert_eq!(h.as_slice(), &[obj(0x2000), obj(0x3000)]);
    }

    /// Wave 24 (L7): the `L7W24LockedMonitorDepths` shape -- `outer` (a
    /// synchronized method on A with a block on B), `middle` (block on C),
    /// `inner` (block re-entering A) -- reported innermost first, A twice,
    /// B before the method monitor A in `outer`; the waited-on monitor and a
    /// frame entry the lock stack does not back are dropped; an owned monitor
    /// no frame names is kept at depth -1.
    #[test]
    fn locked_monitors_are_attributed_innermost_first() {
        let (a, b, c, w, jit) = (obj(0x10), obj(0x20), obj(0x30), obj(0x40), obj(0x50));
        // frames: 0 = main, 1 = outer, 2 = middle, 3 = inner, 4 = leaf
        let frames = vec![(3, a), (2, c), (1, b), (1, a), (2, w), (0, obj(0x60))];
        let owned = vec![a, b, c, w, jit];
        let (monitors, depths) = attribute_locked_monitors(5, owned, Some(frames), Some(w));
        assert_eq!(monitors, vec![a, c, b, a, jit]);
        assert_eq!(depths, vec![1, 2, 3, 3, -1]);
    }

    #[test]
    fn without_attribution_only_the_waited_monitor_is_dropped() {
        let (a, w) = (obj(0x10), obj(0x40));
        let (monitors, depths) = attribute_locked_monitors(3, vec![a, w], None, Some(w));
        assert_eq!(monitors, vec![a]);
        assert!(
            depths.is_empty(),
            "no attribution: the JMX builder's old default"
        );
    }

    #[test]
    fn pop_and_clear() {
        let mut h = HeldMonitors::new();
        h.push(obj(0x1000));
        h.push(obj(0x2000));
        assert_eq!(h.pop(), Some(obj(0x2000)));
        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.pop(), None);
    }
}

/// Interpreter round i6 wave 117, lane L6 (page
/// `i116-L9-strict-monitorexit-throws-on-an-understated-record`): the
/// understated mark is invisible to every accessor, survives pushes and pops,
/// and goes with `clear`.
#[cfg(test)]
mod i117_l6_understated_record_tests {
    use super::{HeldMonitors, RecordProducer};
    use crate::types::ObjectRef;

    const P: RecordProducer = RecordProducer::Unnamed;

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the record only compares and remaps.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    #[test]
    fn an_untrusted_clear_marks_only_a_frame_that_may_hold_a_lock() {
        let mut h = HeldMonitors::new();
        h.clear_untrusted(false, P);
        assert!(!h.is_understated(), "no entry, no monitor bytecode");
        h.clear_untrusted(true, P);
        assert!(h.is_understated(), "monitor bytecode");
        assert!(h.is_empty());
        assert_eq!(h.len(), 0);
        assert!(h.as_slice().is_empty());
        assert_eq!(h.pop(), None, "the mark is never popped");
        let mut held = HeldMonitors::new();
        held.push(obj(0x1000));
        held.clear_untrusted(false, P);
        assert!(held.is_understated(), "it held an entry");
        assert!(held.is_empty());
    }

    /// Interpreter round i6 wave 121, lane L6: the mark names its producer,
    /// with or without the understated bit, and a trusted seed's provenance
    /// replaces an untrusted mark (the record is exact again).
    #[test]
    fn the_mark_names_its_producer() {
        for producer in RecordProducer::ALL {
            let mut h = HeldMonitors::new();
            assert_eq!(h.producer(), None, "an interpreter-entered frame");
            h.mark_understated_by(producer);
            assert_eq!(h.producer(), Some(producer));
            assert!(h.is_understated());
            h.push(obj(0x1000));
            h.set_provenance(producer);
            assert_eq!(h.producer(), Some(producer));
            assert!(!h.is_understated(), "a trusted seed is exact");
            assert_eq!(h.as_slice(), &[obj(0x1000)]);
            h.clear_untrusted(false, producer);
            assert!(h.is_understated(), "it held an entry");
            assert_eq!(h.producer(), Some(producer));
        }
        let mut h = HeldMonitors::new();
        h.set_provenance(RecordProducer::DeoptBuild);
        h.mark_understated();
        assert_eq!(h.producer(), Some(RecordProducer::DeoptBuild), "kept");
    }

    #[test]
    fn the_mark_is_invisible_to_entries_and_remaps() {
        let mut h = HeldMonitors::new();
        h.mark_understated();
        h.mark_understated();
        h.push(obj(0x1000));
        h.push(obj(0x2000));
        assert_eq!(h.len(), 2);
        assert_eq!(h.as_slice(), &[obj(0x1000), obj(0x2000)]);
        assert_eq!(h.count_of(obj(0x1000)), 1);
        h.remap_with(|a| (a == 0x1000).then_some(0x3000));
        assert_eq!(h.as_slice(), &[obj(0x3000), obj(0x2000)]);
        h.remove_at(0);
        assert_eq!(h.as_slice(), &[obj(0x2000)]);
        assert!(h.remove_newest(obj(0x2000)));
        assert!(!h.remove_newest(obj(0x2000)));
        assert!(h.is_empty() && h.is_understated());
        h.clear();
        assert!(!h.is_understated(), "frame reuse forgets the mark");
    }
}

/// Interpreter round i6 wave 120, lane L6 (page
/// `i116-L9-strict-monitorexit-throws-on-an-understated-record`, "Wave 119"):
/// the strict arm of [`monitorexit_unrecorded`] reads only the EXECUTING
/// frame's understated mark. The thread holds `obj` once and no frame's record
/// names it (a compiled caller's lock, as the monitor table sees it).
#[cfg(test)]
mod i120_l6_strict_monitorexit_tests {
    use super::monitorexit_unrecorded;
    use crate::config::VmConfig;
    use crate::runtime::frame::Frame;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use std::sync::Arc;

    /// `static void m()` = `aload_0; monitorexit; return`: monitor bytecode, so
    /// an untrusted clear would mark it.
    fn frame(thread: &mut JvmThread) -> Frame {
        Frame::new_pooled(
            cratonvm_types::ClassId::new(0),
            Arc::from("T"),
            Arc::from("m"),
            Arc::from("()V"),
            None,
            Arc::from(&[0x2au8, 0xc3, 0xb1][..]),
            Arc::from(Vec::new().into_boxed_slice()),
            4,
            4,
            &[],
            &mut thread.locals_pool,
            &mut thread.stacks_pool,
        )
    }

    /// One case: the caller (frame 0) and the executing frame (frame 1) are
    /// marked as given; answers whether the `monitorexit` was refused, and
    /// whether the thread still holds the object after it.
    fn case(strict: bool, caller_marked: bool, executing_marked: bool) -> (bool, bool) {
        let word = if strict { "1" } else { "0" };
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MONITOREXIT_FRAME_STRICT", Some(word))],
            || {
                let shared = Arc::new(SharedVm::new(VmConfig::default()));
                let mut thread = JvmThread::new(ThreadId(120), "i120-l6-strict");
                let obj = shared
                    .mem
                    .heap
                    .alloc_object(cratonvm_types::ClassId::new(7), 0);
                shared.threads.monitors.enter(obj, thread.thread_id);
                let mut caller = frame(&mut thread);
                if caller_marked {
                    caller.held_monitors.mark_understated();
                }
                let mut executing = frame(&mut thread);
                if executing_marked {
                    executing.held_monitors.mark_understated();
                }
                thread.frames.push(caller);
                thread.frames.push(executing);
                let refused = monitorexit_unrecorded(&shared, &mut thread, 1, obj).is_err();
                let held = shared.threads.monitors.holds(obj, thread.thread_id);
                (refused, held)
            },
        )
    }

    #[test]
    fn the_strict_arm_reads_only_the_executing_frames_mark() {
        // Lenient: the unaccounted hold is released, as before wave 115.
        assert_eq!(case(false, false, false), (false, false));
        // Strict, nothing marked: HotSpot's IMSE, and the lock stays held.
        assert_eq!(case(true, false, false), (true, true));
        // Strict, the executing frame understated: the hold may be its own.
        assert_eq!(case(true, false, true), (false, false));
        // Strict, only a CALLER understated: this frame's record is exact, so
        // the hold is not its own (wave 117 was lenient here).
        assert_eq!(case(true, true, false), (true, true));
    }
}

/// Interpreter round i6 wave 132, lane L3
/// (`CRATONVM_MONITOREXIT_COMPILED_HOLDER_IMSE`; page
/// `i116-L6-a-compiled-unpaired-monitorexit-releases-its-callers-lock`): the
/// compiled-holder arm of [`monitorexit_unrecorded`] throws only on proof.
#[cfg(test)]
mod i132_l3_compiled_holder_tests {
    use super::{compiled_holds_explain_the_excess, compiled_label_names_frame};
    use crate::runtime::frame::Frame;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use std::sync::Arc;

    #[test]
    fn compiled_holds_must_account_for_every_unrecorded_acquisition() {
        // A compiled caller's lock, nothing recorded: HotSpot's IMSE.
        assert!(compiled_holds_explain_the_excess(1, 0, 1));
        // A recursive hold split between a record and a compiled frame.
        assert!(compiled_holds_explain_the_excess(2, 1, 1));
        // No compiled hold named: the old leniency.
        assert!(!compiled_holds_explain_the_excess(1, 0, 0));
        // One acquisition nobody describes: may be the frame's own.
        assert!(!compiled_holds_explain_the_excess(2, 0, 1));
    }

    #[test]
    fn a_label_names_a_frame_by_class_and_method() {
        let mut thread = JvmThread::new(ThreadId(132), "i132-l3-label");
        let frame = Frame::new_pooled(
            cratonvm_types::ClassId::new(0),
            Arc::from("p/C"),
            Arc::from("m"),
            Arc::from("(I)V"),
            None,
            Arc::from(&[0xb1u8][..]),
            Arc::from(Vec::new().into_boxed_slice()),
            2,
            2,
            &[],
            &mut thread.locals_pool,
            &mut thread.stacks_pool,
        );
        assert!(compiled_label_names_frame("p/C.m:(I)V", &frame));
        // Looser than the walk: another overload still counts as a twin.
        assert!(compiled_label_names_frame("p/C.m:()V", &frame));
        assert!(!compiled_label_names_frame("p/C.n:(I)V", &frame));
        assert!(!compiled_label_names_frame("p/D.m:(I)V", &frame));
        assert!(!compiled_label_names_frame("garbage", &frame));
    }
}

#[cfg(test)]
mod r21_monitors_foreign_hold_tests {
    use super::*;
    use cratonvm_reader::attribute::ExceptionTableEntry;

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the record only compares.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn frame(code: &[u8], table: Vec<ExceptionTableEntry>, pc: usize) -> Frame {
        let mut f = Frame::new(
            crate::classloading::ClassId::new(3),
            "probe/T".to_string(),
            "m".to_string(),
            "(Ljava/lang/Object;Ljava/lang/Object;)V".to_string(),
            None,
            code.to_vec(),
            table,
            2,
            4,
            &[],
        );
        f.last_instr_pc = pc;
        f
    }

    /// `exitOnly(o)` (`aload_0; monitorexit; return`): its own code holds
    /// nothing at the exit, so a hold the table shows is a caller's and the
    /// exit throws instead of releasing it.
    #[test]
    fn an_exit_with_nothing_of_its_own_held_never_takes_a_callers_hold() {
        if cratonvm_types::flags::runtime_var("CRATONVM_MONITOREXIT_FOREIGN_HOLD_IMSE").is_ok() {
            return; // an explicit setting is the caller's business
        }
        assert!(frame_record_is_complete(&frame(&[0x2a, 0xc3, 0xb1], Vec::new(), 1)));
    }

    /// `exitWrong(a, b)` (`aload_0; monitorenter; aload_1; monitorexit;
    /// return`) with `a` recorded: the record names the one block its code
    /// holds, so `b` is another frame's.
    #[test]
    fn a_full_record_means_the_object_is_another_frames() {
        if cratonvm_types::flags::runtime_var("CRATONVM_MONITOREXIT_FOREIGN_HOLD_IMSE").is_ok() {
            return;
        }
        let code = [0x2a, 0xc2, 0x2b, 0xc3, 0xb1];
        let mut f = frame(&code, Vec::new(), 3);
        assert!(!frame_record_is_complete(&f), "an empty record may be a resumed frame's");
        f.held_monitors.push(obj(0x1000));
        assert!(frame_record_is_complete(&f));
    }

    /// javac's block, resumed from compiled code with an empty record: the
    /// release of the block's own lock goes ahead as before, at both exits.
    #[test]
    fn a_resumed_javac_block_still_releases_its_own_lock() {
        #[rustfmt::skip]
        let code = [
            0x2a, 0x59, 0x4c, 0xc2,       // 0: aload_0 dup astore_1 monitorenter
            0x2a, 0xb6, 0x00, 0x02, 0x57, // 4: aload_0 invokevirtual pop
            0x2b, 0xc3,                   // 9: aload_1 monitorexit
            0xa7, 0x00, 0x08,             // 11: goto 19
            0x4d, 0x2b, 0xc3, 0x2c, 0xbf, // 14: astore_2 aload_1 monitorexit aload_2 athrow
            0xb1,                         // 19: return
        ];
        let row = |start_pc, end_pc| ExceptionTableEntry {
            start_pc,
            end_pc,
            handler_pc: 14,
            catch_type: 0,
        };
        for pc in [10usize, 16] {
            let f = frame(&code, vec![row(4, 11), row(14, 17)], pc);
            assert!(!frame_record_is_complete(&f), "pc {pc}");
        }
    }

    /// Review rev-vmjit item 1: a stale entry in a resumed javac block's
    /// record must never turn its own release into a throw (javac's catch-any
    /// covers that exit, so it would loop forever). The method is balanced,
    /// so the record's length is never trusted there, stale entry or not.
    #[test]
    fn a_stale_entry_in_a_balanced_method_still_releases() {
        #[rustfmt::skip]
        let code = [
            0x2a, 0x59, 0x4c, 0xc2,
            0x2a, 0xb6, 0x00, 0x02, 0x57,
            0x2b, 0xc3,
            0xa7, 0x00, 0x08,
            0x4d, 0x2b, 0xc3, 0x2c, 0xbf,
            0xb1,
        ];
        let row = |start_pc, end_pc| ExceptionTableEntry {
            start_pc,
            end_pc,
            handler_pc: 14,
            catch_type: 0,
        };
        for pc in [10usize, 16] {
            let mut f = frame(&code, vec![row(4, 11), row(14, 17)], pc);
            f.held_monitors.push(obj(0xdead_0000)); // stale: no longer a held object
            assert!(!frame_record_is_complete(&f), "pc {pc}");
        }
    }

    /// An unbalanced method whose record holds a stale entry: the table does
    /// not back it, so the record is not trusted and the exit releases.
    #[test]
    fn a_record_the_table_does_not_back_is_not_trusted() {
        let mut f = frame(&[0x2a, 0xc2, 0x2b, 0xc3, 0xb1], Vec::new(), 3);
        let stale = obj(0xdead_0000);
        let live = obj(0x1000);
        f.held_monitors.push(stale);
        assert!(!record_backed_by_table(&f, |_| 0, |_| 1), "a stale entry");
        let mut g = frame(&[0x2a, 0xc2, 0x2b, 0xc3, 0xb1], Vec::new(), 3);
        g.held_monitors.push(live);
        assert!(record_backed_by_table(&g, |o| usize::from(o == live), |_| 1));
        // Held, but named by more records than the table counts.
        assert!(!record_backed_by_table(&g, |o| usize::from(o == live), |_| 2));
    }

    /// Review rev-vmjit item 1, wave 6: the composed test the exit asks. An
    /// unbalanced `exitWrong(a, b)` whose only entry is stale reads complete
    /// by depth, but the table does not back the entry, so the hold is not
    /// proven foreign and the exit releases; a backed entry proves it; a
    /// balanced javac block never reads the table at all.
    #[test]
    fn a_stale_entry_never_proves_a_foreign_hold() {
        if cratonvm_types::flags::runtime_var("CRATONVM_MONITOREXIT_FOREIGN_HOLD_IMSE").is_ok() {
            return;
        }
        let code = [0x2a, 0xc2, 0x2b, 0xc3, 0xb1];
        let stale = obj(0xdead_0000);
        let live = obj(0x1000);
        let mut f = frame(&code, Vec::new(), 3);
        f.held_monitors.push(stale);
        assert!(frame_record_is_complete(&f), "complete by depth alone");
        assert!(!record_proves_foreign_hold(&f, |_| 0, |_| 1), "a stale entry");
        let mut g = frame(&code, Vec::new(), 3);
        g.held_monitors.push(live);
        assert!(record_proves_foreign_hold(&g, |o| usize::from(o == live), |_| 1));
        // Balanced javac block: never proven, and the table is never asked.
        #[rustfmt::skip]
        let javac = [
            0x2a, 0x59, 0x4c, 0xc2,
            0x2a, 0xb6, 0x00, 0x02, 0x57,
            0x2b, 0xc3,
            0xa7, 0x00, 0x08,
            0x4d, 0x2b, 0xc3, 0x2c, 0xbf,
            0xb1,
        ];
        let row = |start_pc, end_pc| ExceptionTableEntry {
            start_pc,
            end_pc,
            handler_pc: 14,
            catch_type: 0,
        };
        let mut h = frame(&javac, vec![row(4, 11), row(14, 17)], 10);
        h.held_monitors.push(live);
        assert!(!record_proves_foreign_hold(
            &h,
            |_| panic!("table read for a balanced method"),
            |_| panic!("records read for a balanced method"),
        ));
    }

    /// The kill switch restores the release everywhere.
    #[test]
    fn the_switch_off_never_claims_a_full_record() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MONITOREXIT_FOREIGN_HOLD_IMSE", Some("0"))],
            || assert!(!frame_record_is_complete(&frame(&[0x2a, 0xc3, 0xb1], Vec::new(), 1))),
        );
    }
}
