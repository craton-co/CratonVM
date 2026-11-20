// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Frames still running a method body a JVMTI redefinition replaced (JEP 109
//! obsolete methods; interpreter round i1 wave 19, lane L3).
//!
//! "If a redefined method has active stack frames, those active frames
//! continue to run the bytecodes of the original method." A `Frame` keeps its
//! own `code`, so the bytecodes did carry on -- but every constant-pool read
//! goes through the frame's class, whose pool the redefinition replaced, so
//! an old `ldc #8` read whatever the NEW pool keeps at `#8`.
//!
//! `ClassManager::redefine_class` now installs the new pool with every old
//! constant appended and records the index translation
//! (`cratonvm_classloading::obsolete_code`). Here a thread moves each of its
//! frames that runs a replaced body onto a copy whose constant-pool operands
//! (and exception-table catch types) are translated into the class's current
//! pool: same instruction layout, so `pc`, locals and stack carry over, and
//! from then on every resolver and every `(class, cp index)` site cache reads
//! the right constant with nothing to change on their side.
//!
//! A thread only ever moves its OWN frames (`JvmThread::frames` belongs to
//! its thread): at every `safepoint_check` and at the end of every blocking
//! region, once `class_redefinition_count()` moved since it last looked, and
//! the redefining thread straight after the redefinition, which then takes a
//! handshake pause so every running thread passes a poll
//! ([`after_redefinition`]). Since wave 37 also at every dispatch loop's next
//! top ([`convert_at_loop_top`]), and a redefinition that renumbers the pool
//! first holds every other thread's loops at their top, so none runs an
//! instruction of an old body across the swap ([`RedefinitionFence`]). A
//! frame whose class was redefined more times than the kept history is left
//! as it was -- the pre-wave-19 behaviour. How long the history is kept
//! follows a census of the oldest stamp any thread may still carry
//! ([`prune_histories_by_census`], wave 23; per class since wave 24): steps
//! nobody needs go, steps a parked thread needs outlive the soft caps. One
//! whose `ldc` constant lands above index 255 runs on a decoded stream that
//! reads the merged index (wave 21, lane L4; see [`translated_body`]).
//!
//! A moved frame also carries its body's own line table when a redefinition
//! replaced it (`Frame::own_line_number`): a stack trace through an obsolete
//! method reports the lines of the source it runs, as HotSpot does.
//!
//! A frame that runs a replaced body must not enter an OSR body either: those
//! are compiled from the class's CURRENT bytecode and keyed by method name,
//! so their pcs mean something else ([`frame_runs_replaced_code`]).

use std::sync::Arc;

use cratonvm_classloading::obsolete_code::{
    translate_code_widening, translate_exception_table, RedefinitionHistory,
};
use cratonvm_reader::attribute::ExceptionTableEntry;

use crate::classloading::ClassManager;
use crate::runtime::frame::{Frame, ReplacedBody};
use crate::threading::jvm_thread::JvmThread;
use crate::vm::SharedVm;

/// The cooperative slice of the handshake pause [`after_redefinition`]
/// takes: the frame-trace pause's, for the same reason (every compiled loop
/// polls at each back edge and every compiled method at entry).
const REDEFINITION_GRACE: std::time::Duration = std::time::Duration::from_millis(2);

/// Ways in [`NO_HISTORY_SEEN`]. A power of two.
const NO_HISTORY_SEEN_WAYS: usize = 8;

thread_local! {
    /// `(vm_identity, class, count)` per way: at class-redefinition count
    /// `count`, this thread found class `class` of that VM with no
    /// redefinition history, so none of its frames runs a replaced body
    /// ([`frame_runs_replaced_code`]; interpreter round i1 wave 25, lane L3).
    /// Void once the count moves. A zero `vm_identity` is an empty way. A
    /// per-thread memo of a per-VM answer, keyed by the VM, as
    /// `jit::helpers`' `CP_STAMP_CHECKED` is.
    static NO_HISTORY_SEEN: [std::cell::Cell<(usize, u32, u64)>; NO_HISTORY_SEEN_WAYS] =
        const { [const { std::cell::Cell::new((0, 0, 0)) }; NO_HISTORY_SEEN_WAYS] };
}

/// Move this thread's frames that run a replaced body onto their translated
/// copies, if a class was redefined since the thread last looked. One load
/// and a compare otherwise.
///
/// `try_read`: this runs inside `safepoint_check`, which a thread can reach
/// while it holds the class-manager lock. When the lock is busy the
/// conversion is deferred to the dispatch loop's next top, which retries it
/// before running another bytecode ([`retry_deferred_conversion`];
/// interpreter round i1 wave 24, lane L3). Until wave 23 a deferred
/// conversion waited for the thread's next safepoint or blocking-region
/// exit, and the frames ran their old constant-pool indices against the new
/// pool until then: a thread woken from a wait while another thread held the
/// class-manager writer (a class definition, the next redefinition) resumed
/// its parked frame on its old `ldc` index -- the rare `parkQqA,parkQqB`
/// rows of `L3ObsoleteParkedAcrossToggles` / `...Renames` /
/// `...VirtualThreadResume` on the Linux host.
///
/// `#[track_caller]` for the deferral's debug line only (interpreter round i1
/// wave 41, lane L3): under `CRATONVM_DBG_RETRANSFORM=1` a deferral names the
/// call site that met the busy lock -- a safepoint, a blocking-region exit,
/// the loop top -- which is where the thread was while the redefinition held
/// the class-manager writer.
#[track_caller]
pub(crate) fn convert_obsolete_frames_if_redefined(shared: &SharedVm, thread: &mut JvmThread) {
    let now = cratonvm_classloading::class_redefinition_count();
    if now == thread.redefinitions_seen {
        return;
    }
    let Some(cm) = shared.classes.class_manager.try_read() else {
        let marked = thread.frames.note_conversion_deferred(now);
        note_deferral_site(thread, now, marked, std::panic::Location::caller());
        return;
    };
    thread.frames.clear_conversion_pending();
    let seen_before = thread.redefinitions_seen;
    thread.redefinitions_seen = now;
    let converted = convert_frames(&cm, &mut thread.frames, seen_before);
    drop(cm);
    if converted.code_moved {
        // The dispatch loop refreshes its fast-path gate at its next top
        // (interpreter round i1 wave 23, lane L3).
        thread.frames.note_code_moved();
    }
    publish_stale_frame_floor(thread, now);
    for index in converted.moved {
        // After the class-manager guard is gone: the verdict's own-code
        // proof reads the class's pool with `try_read`.
        if let Some(frame) = thread.frames.get(index) {
            super::refresh_fast_path_verdict(shared, frame);
        }
    }
}

/// `CRATONVM_DBG_RETRANSFORM=1`: one `[redefine] conversion deferred (class
/// manager busy): thread T count N marked=B at FILE:LINE` line per deferral
/// [`convert_obsolete_frames_if_redefined`] makes, naming its caller. The
/// wave-41 host runs of `L3W41FirstSwapHoldsTheLoop` showed four `deferred
/// conversion done` lines and no fence wait: the workers deferred somewhere
/// other than the fenced loop top, and this names where. Cold.
#[cold]
#[inline(never)]
fn note_deferral_site(
    thread: &JvmThread,
    count: u64,
    marked: bool,
    site: &'static std::panic::Location<'static>,
) {
    if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
        eprintln!(
            "[redefine] conversion deferred (class manager busy): thread {} count {count} \
             marked={marked} at {}:{}",
            thread.thread_id.0,
            site.file(),
            site.line()
        );
    }
}

/// Whether this thread holds the class-manager lock (read or write), when
/// lock-order tracking records it -- always in a debug build, and in a
/// release build run with `CRATONVM_LOCK_ORDER_CHECK=1`; `None` when it does
/// not (a release build's default: `parking_lot` records no owner). For the
/// debug lines of a loop a redefinition fence holds only
/// (`docs/feature-designs/real-jdk-mode.md`;
/// interpreter round i1 wave 46, lane L3).
fn class_manager_held_here() -> Option<bool> {
    use cratonvm_types::lock_order::{tracking, LockLevel};
    tracking::enforced().then(|| tracking::is_held(LockLevel::ClassManager))
}

/// The dispatch loop, at its top, when a conversion of this thread's frames
/// was deferred (`FrameStack::conversion_pending`; interpreter round i1 wave
/// 24, lane L3): yield to the thread holding the class-manager lock and try
/// again. The loop runs no bytecode of this thread until the conversion ran,
/// or `FrameStack::MAX_CONVERSION_RETRIES` tries were spent (a thread running
/// Java while it holds that lock itself), after which the frames run as they
/// are until the next safepoint or blocking-region exit, as before. The loop
/// polls safepoints between tries.
///
/// Since wave 37 (lane L3) a loop a redefinition fence holds
/// ([`RedefinitionFence`]) waits here too, with the same bound, until the fence
/// is down and its thread has converted.
///
/// Once no fence holds the loop, each retry is the conversion pass itself
/// ([`convert_obsolete_frames_if_redefined`]), as before wave 37: it clears
/// the pending mark as soon as the class-manager lock is free, whether or not
/// a frame moved. Wave 37 retried through [`convert_at_loop_top`] instead,
/// whose shortcut for a stack none of whose frames' classes was redefined
/// returns WITHOUT clearing the mark -- so a thread whose conversion was
/// deferred while the redefining thread still held the writer (the forced
/// exit poll of a withdrawn body polls right then) spun here, running no
/// bytecode, for the whole retry bound (seconds) although it owed nothing:
/// `L6/RedefineRunningSpliceProbe` printed `after-new=false` in 10 of 30
/// runs on the wave-37 build (interpreter round i1 wave 38, lane L3).
pub(crate) fn retry_deferred_conversion(shared: &SharedVm, thread: &mut JvmThread) {
    let now = cratonvm_classloading::class_redefinition_count();
    let fenced = shared
        .mem
        .gc_barrier
        .redefinition_fence_holds(thread.thread_id);
    if !fenced && now == thread.redefinitions_seen {
        // Converted meanwhile (a safepoint's pass), or the fence came down
        // over a redefinition that was refused.
        thread.frames.clear_conversion_pending();
        return;
    }
    if !thread.frames.count_conversion_retry() {
        // Not again at this count: see `FrameStack::give_up_conversion`. The
        // bound is a count and, past the yields, a wall time (wave 39).
        let retries = thread.frames.conversion_retries();
        let elapsed = thread.frames.conversion_retry_elapsed();
        thread.frames.give_up_conversion(now);
        if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
            // `class_manager_held=`: `yes` / `no` under lock-order tracking,
            // `unknown` without it (wave 46, lane L3).
            let held = match class_manager_held_here() {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown",
            };
            eprintln!(
                "[redefine] deferred conversion GIVEN UP after {} retries in {} ms: thread {} \
                 fenced={fenced} class_manager_held={held}",
                retries.min(crate::runtime::frame::FrameStack::MAX_CONVERSION_RETRIES),
                elapsed.as_millis(),
                thread.thread_id.0
            );
        }
        return;
    }
    // Yield first; past that, the writer is doing real work (verifying a
    // large class): sleep instead of burning the bound in microseconds
    // (interpreter round i1 wave 25, lane L3).
    if thread.frames.conversion_retries()
        <= crate::runtime::frame::FrameStack::CONVERSION_RETRY_YIELDS
    {
        std::thread::yield_now();
    } else {
        std::thread::sleep(std::time::Duration::from_micros(20));
    }
    if shared
        .mem
        .gc_barrier
        .redefinition_fence_holds(thread.thread_id)
    {
        // Still held: the mark stays, and the next top retries.
        shared.mem.gc_barrier.note_redefinition_fence_wait();
        return;
    }
    let retries = thread.frames.conversion_retries();
    convert_obsolete_frames_if_redefined(shared, thread);
    if !thread.frames.conversion_pending()
        && Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok()
    {
        eprintln!(
            "[redefine] deferred conversion done after {retries} retries: thread {}",
            thread.thread_id.0
        );
    }
}

/// The dispatch loop's slow path (`interpreter::loop_top_poll`), before it
/// compares the stack's move count: a class redefinition since this thread
/// last looked moves its frames here, before the loop runs another bytecode
/// (interpreter round i1 wave 37, lane L3). The loop gets here at its next
/// top because the redefinition arms every loop's poll word
/// (`GcBarrier::note_code_moved_on_every_loop`, from [`RedefinitionFence`]).
///
/// Before wave 37 a running thread moved its frames only at its next
/// `safepoint_check` -- which the loop calls only while a pause is requested
/// -- or blocking-region exit. So a thread whose compiled callee was frozen
/// through the redefinition's handshake returned into a frame of the replaced
/// body and ran its old constant-pool indices against the new pool until the
/// next pause of any kind (window 2 of
/// `docs/feature-designs/real-jdk-mode.md`),
/// and after a redefinition that moved no constant (no handshake at all) a
/// running frame reported the new body's line numbers until then (window 4's
/// running-thread half).
///
/// A loop held by a fence ([`RedefinitionFence`]) defers instead: the
/// conversion is marked pending and retried at every top
/// ([`retry_deferred_conversion`]) until the fence is down.
///
/// Cost: one load (no fence up) and a compare of the redefinition count, on
/// the slow path only; a thread none of whose frames' classes the ring names
/// as redefined since it last looked takes no lock
/// (`for_each_redefined_class_between`, as [`convert_thawed_frames`]).
pub(crate) fn convert_at_loop_top(shared: &SharedVm, thread: &mut JvmThread) {
    let now = cratonvm_classloading::class_redefinition_count();
    if shared
        .mem
        .gc_barrier
        .redefinition_fence_holds(thread.thread_id)
    {
        // Counted only when the loop is held: a loop that gave the conversion
        // owed at this count up runs on, and its top is not a wait. Until wave
        // 41 that was also any loop meeting the process's first redefinition
        // (count 0, which `FrameStack` took for "never gave up"), and the
        // positive control's `loop-top deferrals=` still counted it.
        if thread.frames.note_conversion_deferred(now) {
            shared.mem.gc_barrier.note_redefinition_fence_wait();
            // The one case the fence stalls on (wave 46, lane L3): a held loop
            // whose own thread holds the class-manager lock. No path is known
            // to reach it; lock-order tracking (a debug build, or
            // `CRATONVM_LOCK_ORDER_CHECK=1`) makes it confess.
            if class_manager_held_here() == Some(true)
                && Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok()
            {
                eprintln!(
                    "[redefine] fenced loop top holds the class-manager lock: thread {} count {now}",
                    thread.thread_id.0
                );            }        } else if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
            eprintln!(
                "[redefine] fence not held: thread {} gave the conversion at count {now} up",
                thread.thread_id.0
            );
        }
        return;
    }
    let seen = thread.redefinitions_seen;
    if now == seen {
        return;
    }
    let mut names_a_frame_class = false;
    let per_vm = thread.invoke_cache.has_redefinition_log();
    let answered = vm_redefined_classes_between(shared, per_vm, seen, now, |class| {
        names_a_frame_class |= thread.frames.iter().any(|frame| frame.class_id == class);
    });
    if answered && !names_a_frame_class {
        // Nothing of this stack was redefined. `redefinitions_seen` stays: a
        // frame pushed later from an invoke-cache entry validated before a
        // swap is told apart by it (`convert_frames`' window-3 rule).
        return;
    }
    convert_obsolete_frames_if_redefined(shared, thread);
}

/// The classes redefined in `(after, upto]` that the obsolete-frame passes
/// must consider (nio21 wave 5 L1; page
/// `r21-invoke-cache-redefinition-count-is-process-wide-FIXED-20261012.md`):
/// `per_vm` -- the thread's invoke cache is bound to its VM's log, i.e.
/// `CRATONVM_INVOKE_CACHE_PER_VM_REDEFINITIONS` is on and the VM registered
/// the thread -- asks THIS VM's log, so another VM's redefinition of a class
/// with a frame's id costs no conversion pass; otherwise the process ring.
/// `false`: the record cannot answer (treat every class as redefined). Sound:
/// every redefinition of this VM records in its log before the process clock
/// reaches the record's stamp.
#[inline]
fn vm_redefined_classes_between(
    shared: &SharedVm,
    per_vm: bool,
    after: u64,
    upto: u64,
    visit: impl FnMut(crate::classloading::ClassId),
) -> bool {
    if per_vm {
        shared
            .classes
            .redefinition_log
            .for_each_between(after, upto, visit)
    } else {
        cratonvm_classloading::for_each_redefined_class_between(after, upto, visit)
    }
}

/// Would installing `new_bytes` over class `class_id` give some index of the
/// class's current constant pool a different constant? The merge the
/// redefinition itself makes (`ClassManager::redefine_class_typed`, through
/// `merge_for_obsolete_code_compacting`), run ahead on the class as it is:
/// `true` when its translation moves an old index (a class recompiled by
/// `javac` after an edit -- an IDE's HotSwap -- or rebuilt by an agent that
/// does not keep the pool), `false` when every old index keeps its constant
/// (an unchanged or append-only pool: ASM-based agents such as Mockito's
/// inline mock maker and Byte Buddy, measured in wave 29) or when the bytes do
/// not parse / the merge does not fit (the redefinition refuses them, or
/// installs the new pool with no translation, which nothing can repair).
///
/// Approximate in one direction only: the redefinition may compact the old
/// constants no kept frame names, which renumbers the pool's appended tail
/// when this says `false`; its own `last_redefinition_moved_constants` then
/// still takes the handshake after it, as before wave 37. A native agent's
/// `ClassFileLoadHook` may substitute other bytes. Interpreter round i1 wave
/// 37, lane L3.
pub(crate) fn redefinition_moves_constants(
    shared: &SharedVm,
    class_id: crate::classloading::ClassId,
    new_bytes: &[u8],
) -> bool {
    use cratonvm_reader::attribute::Attribute;
    let Ok(mut fresh) = cratonvm_reader::read_class(new_bytes) else {
        return false;
    };
    if cratonvm_reader::force_decode_all(&mut fresh.attributes, &fresh.constant_pool).is_err() {
        return false;
    }
    let fresh_bootstrap_methods = fresh
        .attributes
        .iter()
        .find_map(|attribute| match attribute.as_decoded() {
            Some(Attribute::BootstrapMethods(methods)) => Some(methods.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    let Some(merged) = cratonvm_classloading::obsolete_code::merge_for_obsolete_code(
        &class.constant_pool,
        &class.bootstrap_methods,
        &fresh.constant_pool,
        &fresh_bootstrap_methods,
        &cratonvm_classloading::obsolete_code::ldc_operands_of(&class.methods),
    ) else {
        return false;
    };
    merged
        .translation
        .iter()
        .enumerate()
        .any(|(index, &at)| at != 0 && usize::from(at) != index)
}

/// A class redefinition in progress on this thread (interpreter round i1
/// wave 37, lane L3): taken by `NativeContextImpl::redefine_class_with`
/// before it takes the class-manager writer, dropped once the writer is
/// released and the redefinition recorded.
///
/// **Window 1** of
/// `docs/feature-designs/real-jdk-mode.md`.
/// A frame running the replaced body resolves its constant-pool operands in
/// the class's pool, and it is moved onto a translated copy only when its
/// thread converts. The redefinition advances the resolution epoch under the
/// writer, so a thread spinning in such a frame misses its site caches at its
/// next constant-pool instruction, decodes the OLD index, blocks on the
/// class-manager read lock inside the slow path and -- released after the
/// swap -- reads that index in the NEW pool: a different constant, or a
/// Methodref where its `getstatic` wants a Fieldref. For a pool a `javac`
/// recompilation renumbered, that is the first such instruction of every
/// running thread in the class, at every redefinition
/// (`L3W37HotSwapSpinningReads.java`). HotSpot runs the
/// swap in a safepoint operation, between two bytecodes of every thread.
///
/// When the redefinition moves some constant ([`redefinition_moves_constants`];
/// never for an unchanged or append-only pool, so a single-class Mockito /
/// Byte Buddy retransform pays nothing new; a call of several classes has
/// one fence for all of them, [`raise_install_fence`]), the fence first
/// holds every other thread's dispatch loops at their top
/// (`GcBarrier::raise_redefinition_fence`, which arms every loop's poll
/// word), then takes a handshake pause, so every
/// running thread has passed a poll -- and so either waits at a loop top, or
/// finishes the instruction it was in against the OLD pool (a poll inside an
/// instruction is at an allocation, after its constant-pool read), or runs
/// compiled code, whose sites translate through their compile stamp
/// (`jit::helpers::stale_cp_site_index`) -- before the writer is taken. A
/// held loop waits in [`retry_deferred_conversion`] (bounded, polling
/// safepoints) and converts once the fence is down, before its next bytecode.
/// The redefining thread's own loops are never held (a class its verifier
/// loads may run Java on it).
///
/// Every drop arms every loop's poll word, fence or not, so every running
/// thread moves its frames at its next loop top ([`convert_at_loop_top`]):
/// windows 2 and 4 (running-thread half) of the same page.
///
/// Positive control: `CRATONVM_DBG_RETRANSFORM=1` prints a `[redefine] fence
/// raised:` line per fenced redefinition and a `[redefine] fence lowered:`
/// line with the loop tops it held.
pub(crate) struct RedefinitionFence<'a> {
    shared: &'a SharedVm,
    owner: crate::threading::jvm_thread::ThreadId,
    /// The fence is up (the redefinition moves constants).
    raised: bool,
    /// `GcBarrier::redefinition_fence_waits` when it went up.
    waits_before: u64,
    /// `CRATONVM_DBG_RETRANSFORM`, and the class's name when set.
    dbg: Option<String>,
}

impl<'a> RedefinitionFence<'a> {
    /// See the type. `owner` is the redefining thread; nothing is held yet
    /// (the class-manager lock in particular).
    pub(crate) fn raise(
        shared: &'a SharedVm,
        owner: crate::threading::jvm_thread::ThreadId,
        class_id: crate::classloading::ClassId,
        new_bytes: &[u8],
    ) -> Self {
        let dbg = Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .is_ok()
            .then(|| {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map_or_else(
                        || class_id.as_u32().to_string(),
                        |class| class.name.to_string(),
                    )
            });
        let waits_before = shared.mem.gc_barrier.redefinition_fence_waits();
        // Inside a call-wide fence ([`raise_install_fence`]) the other threads
        // are held already: this class adds nothing to it.
        let raised = !shared.mem.gc_barrier.redefinition_fence_owned_by(owner)
            && redefinition_moves_constants(shared, class_id, new_bytes);
        if raised {
            let handshake = raise_fence_and_handshake(shared, owner);
            if let Some(name) = &dbg {
                eprintln!("[redefine] fence raised: {name} moves constants; handshake {handshake}");
            }
        }
        Self {
            shared,
            owner,
            raised,
            waits_before,
            dbg,
        }
    }
}

/// Raise `owner`'s redefinition fence (`GcBarrier::raise_redefinition_fence`,
/// which arms every loop word) and take a handshake pause, so every running
/// thread passes a poll after the fence went up: what it says about the pause.
/// A `None` from the request (another pause owns the world) leaves the threads
/// it did not stop as they are; the fence still holds each at its next loop
/// top.
fn raise_fence_and_handshake(
    shared: &SharedVm,
    owner: crate::threading::jvm_thread::ThreadId,
) -> &'static str {
    shared.mem.gc_barrier.raise_redefinition_fence(owner);
    let pause = super::NonMovingPause::request_handshake(
        shared,
        owner,
        super::gc_events::NonCollectionPause::Redefinition,
        REDEFINITION_GRACE,
    );
    let handshake = match &pause {
        None => "not taken",
        Some(pause) if pause.froze_peers() => "froze peers",
        Some(_) => "every peer polled",
    };
    drop(pause);
    handshake
}

/// The fence for a whole `redefineClasses` / `retransformClasses` call of
/// more than one class (`NativeContext::begin_redefinition_install`, from
/// `instrument::install_redefinitions` once every class is checked;
/// interpreter round i1 wave 37, lane L3): up from before the call's first
/// class is installed to after its last, so no other thread's loop runs
/// between two of them. JVMTI installs a call in one VM operation; installed
/// class by class, a thread could call the new body of the first class and
/// the old body of the second
/// (`L3W37RedefineTwoClassesAtOnce.java`; HotSpot 25
/// prints `torn=0`). Each class's own [`RedefinitionFence`] then adds nothing.
/// A single class keeps its own fence (up only when it moves constants).
/// Returns whether the fence went up.
///
/// The handshake is taken only when some class of the call moves a constant
/// ([`redefinition_moves_constants`], the question each class's own fence
/// asks; interpreter round i1 wave 38, lane L3). It is what window 1 needs --
/// every running thread past a poll, so none is between a loop top and a
/// constant-pool read of an old body across the swap -- and a pool that only
/// grew (Mockito's inline mock maker and Byte Buddy retransform a type's
/// whole hierarchy in one call, appending to each pool) has no such read to
/// protect. The call's atomicity does not need it: a held loop runs no
/// instruction of either class until the fence is down, and a thread that
/// was inside ONE instruction when it went up finishes only that one (an
/// invoke it resolved before the swap pushes the old body, which it runs
/// once the fence is down -- a state the call order allows). Compiled code
/// is not held by the fence, handshake or not: the pause is released before
/// the first class is installed. Wave 37 took the handshake for every call of
/// two or more classes, a pause of every mutator per mock type.
pub(crate) fn raise_install_fence(
    shared: &SharedVm,
    owner: crate::threading::jvm_thread::ThreadId,
    classes: &[(crate::classloading::ClassId, &[u8])],
) -> bool {
    if classes.len() < 2 {
        return false;
    }
    let moves = classes
        .iter()
        .any(|&(class_id, bytes)| redefinition_moves_constants(shared, class_id, bytes));
    let handshake = if moves {
        raise_fence_and_handshake(shared, owner)
    } else {
        shared.mem.gc_barrier.raise_redefinition_fence(owner);
        "skipped (no class moves a constant)"    };
    if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
        eprintln!(
            "[redefine] fence raised for a call of {} classes; handshake {handshake}",
            classes.len()
        );
    }
    true
}

/// Lower the fence [`raise_install_fence`] raised; the held loops move their
/// frames at their next top.
pub(crate) fn lower_install_fence(
    shared: &SharedVm,
    owner: crate::threading::jvm_thread::ThreadId,
) {
    shared.mem.gc_barrier.lower_redefinition_fence(owner);
    if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
        eprintln!(
            "[redefine] fence lowered for the call; loop-top deferrals so far={}",
            shared.mem.gc_barrier.redefinition_fence_waits()
        );
    }
}

impl Drop for RedefinitionFence<'_> {
    fn drop(&mut self) {
        let barrier = &self.shared.mem.gc_barrier;
        if self.raised {
            // Arms every loop word itself.
            barrier.lower_redefinition_fence(self.owner);
            if let Some(name) = &self.dbg {
                eprintln!(
                    "[redefine] fence lowered: {name} loop-top deferrals={}",
                    barrier
                        .redefinition_fence_waits()
                        .saturating_sub(self.waits_before)
                );
            }
        } else {
            barrier.note_code_moved_on_every_loop();
        }
    }
}

/// Publish `floor` as this thread's input to the stale-frame census
/// ([`prune_histories_by_census`]; interpreter round i1 wave 23, lane L3): every
/// frame of the thread was just moved into its class's current pool. A
/// frame `translated_body` refused is left out: every refusal is final (the
/// history no longer reaches its stamp, or its body cannot be decoded), so
/// keeping steps for it would help nothing and hold every class's history
/// at its hard caps for as long as the frame lives.
///
/// Not while a compiled activation is on the thread's stack: its constant-pool
/// sites translate through the class's history from the stamp it was compiled
/// at (`jit::helpers::stale_cp_site_index`), which no frame shows. The floor
/// published before it began stays -- still a lower bound, because a body
/// compiled before a redefinition of its class is made not entrant by it, so
/// an activation that began after that floor needs no step retired before it.
fn publish_stale_frame_floor(thread: &JvmThread, floor: u64) {
    if crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
        return;
    }
    thread
        .gc_block_state
        .obsolete_frame_floor
        .store(floor, std::sync::atomic::Ordering::Release);
}

/// The blocking deposit's half of the per-class stale-frame census
/// (interpreter round i1 wave 24, lane L3): the classes of this thread's
/// interpreter frames, published in `GcBlockState::blocked_frame_classes`
/// just before the thread raises `in_blocked_region`
/// (`NativeContextImpl::deposit_root_snapshot`). A blocked thread pushes no
/// frame, so while the flag stays up a class the list does not name has no
/// frame on this thread, and [`prune_histories_by_census`] does not let this
/// thread's floor hold that class's history back. Inexact (counted for every
/// class, as before) when a compiled activation is on the stack: its
/// constant-pool sites translate through a class's history from the stamp
/// it was compiled at, which no frame shows.
///
/// One pass over the frames into a reused allocation, next to the frame
/// trace the same deposit already captures.
pub(crate) fn publish_blocked_frame_classes(thread: &JvmThread) {
    let exact = crate::jit::conservative_roots::current_thread_jit_depth() == 0;
    let mut published = thread.gc_block_state.blocked_frame_classes.lock();
    published.exact = exact;
    published.classes.clear();
    if exact {
        for frame in thread.frames.iter() {
            if published.classes.last() != Some(&frame.class_id) {
                published.classes.push(frame.class_id);
            }
        }
    }
}

/// The stale-frame census (interpreter round i1 wave 23, lane L3, the
/// `i22-L3` census proposal; per class since wave 24): for each class, the
/// least stamp a frame of that class on a registered thread may still carry
/// below its class's current pool, as the threads published it at their last
/// conversion pass ([`publish_stale_frame_floor`]; a parked virtual thread's
/// boxed `JvmThread` is registered too).
///
/// A thread counts for every class unless it is blocked with an exact list
/// of its frames' classes ([`publish_blocked_frame_classes`]); then it counts
/// only for the classes it names. So a thread parked since long ago (an idle
/// pool worker, a JDK service thread blocked since boot) no longer holds back
/// the history of a class it runs no frame of -- the wave-23 census had one
/// global floor, which such a thread held for every class.
struct StaleFrameCensus {
    /// Some thread is registered: without one there is no census.
    any_thread: bool,
    /// The least floor of the threads counted for every class.
    everyone: Option<u64>,
    /// The least floor of the blocked threads whose list names the class.
    by_class: crate::runtime::fx_collections::FxHashMap<crate::classloading::ClassId, u64>,
    /// `class_redefinition_count()` at the census: no recorded step is
    /// retired after it, so it is the floor of a class nobody counts for.
    now: u64,
}

impl StaleFrameCensus {
    fn take(shared: &SharedVm) -> Self {
        use std::sync::atomic::Ordering;
        let mut census = Self {
            any_thread: false,
            everyone: None,
            by_class: crate::runtime::fx_collections::FxHashMap::default(),
            now: cratonvm_classloading::class_redefinition_count(),
        };
        shared
            .threads
            .thread_registry
            .for_each_gc_block_state(|state| {
                census.any_thread = true;
                let floor = state.obsolete_frame_floor.load(Ordering::Acquire);
                // The flag first: the list the thread wrote before raising it
                // (or a later deposit's, as complete) is what the lock hands out.
                // A thread that woke since pushes frames stamped at or above
                // `now`, which need no step recorded so far.
                // Read in place under the list's lock (it used to be cloned per
                // thread per census; wave 25).
                let listed = state.in_blocked_region.load(Ordering::Acquire) && {
                    let published = state.blocked_frame_classes.lock();
                    if published.exact {
                        for &class_id in &published.classes {
                            let slot = census.by_class.entry(class_id).or_insert(floor);
                            *slot = (*slot).min(floor);
                        }
                    }
                    published.exact
                };
                if !listed {
                    census.everyone = Some(census.everyone.map_or(floor, |least| least.min(floor)));
                }
            });
        census
    }

    /// The floor for `class_id`'s history, `None` without a census.
    fn floor_of(&self, class_id: crate::classloading::ClassId) -> Option<u64> {
        if !self.any_thread {
            return None;
        }
        let listed = self.by_class.get(&class_id).copied();
        Some(match (self.everyone, listed) {
            (Some(everyone), Some(listed)) => everyone.min(listed),
            (Some(floor), None) | (None, Some(floor)) => floor,
            (None, None) => self.now,
        })
    }
}

/// Apply the stale-frame census to every redefined class's history
/// (`ClassManager::prune_redefinition_histories`): steps no thread's frames
/// need any more are dropped past the last few, and the steps a parked
/// thread still needs outlive the soft caps. Called by the redefining thread
/// under the class-manager writer, before the swap (`redefine_class_with`).
/// Lock order: the class manager, then the registry.
///
/// A lower bound per class, never exact: a thread publishes its floor only
/// when it converts (at a safepoint or blocking-region exit after a
/// redefinition) with no compiled activation on its stack, and one that is
/// running or blocked under compiled code counts for every class. That only
/// keeps steps longer.
pub(crate) fn prune_histories_by_census(shared: &SharedVm, cm: &mut ClassManager) {
    let census = StaleFrameCensus::take(shared);
    // Interpreter round i1 wave 25, lane L6: a compiled body a redefinition
    // retired but could not make not entrant can still be entered by a baked
    // caller -- an activation no census has seen yet -- and translates its
    // sites' indices from its compile's stamp on
    // (`JitCache::min_unpatched_cp_stamp`). Keep those steps.
    let unpatched = shared.jit.jit_cache.min_unpatched_cp_stamp();
    cm.prune_redefinition_histories(|class_id| {
        census
            .floor_of(class_id)
            .map(|floor| unpatched.map_or(floor, |least| floor.min(least)))
    });
}

/// Frames of a continuation about to be resumed on this thread
/// (`interpreter::resume_continuation`): one of them may run a body a
/// redefinition replaced while the continuation was unmounted. The
/// redefinition's handshake pause reaches mounted threads only, and the
/// resumed dispatch loop polls (`safepoint_check`, which moves frames) only
/// when a pause is requested, so the frame's old indices could read the new
/// pool until the next pause -- or, for frames thawed from `FrozenFrame`s
/// onto a thread that already looked at that redefinition
/// (`thread.redefinitions_seen`), until the next redefinition anywhere.
/// Nothing but a compare per frame unless some frame is older than the
/// current count. Interpreter round i1 wave 22, lane L3.
///
/// Since wave 25 (lane L3) a frame older than the count costs a walk of the
/// lock-free redefinition ring, not a conversion pass: the count is
/// process-wide, so once ANY class was redefined every frame built before it
/// is "older", and every remount of every continuation used to take the
/// class-manager lock and look up each frame's class. When the ring names
/// every redefinition since the oldest frame and none of them is of a class
/// these frames run, nothing here can be stale (a class id of another VM
/// only over-matches, which costs the pass).
pub(crate) fn convert_thawed_frames(shared: &SharedVm, thread: &mut JvmThread) {
    let now = cratonvm_classloading::class_redefinition_count();
    let Some(oldest) = thread
        .frames
        .iter()
        .map(|frame| frame.redefine_stamp())
        .min()
        .filter(|&oldest| oldest < now)
    else {
        return;
    };
    let mut names_a_frame_class = false;
    let per_vm = thread.invoke_cache.has_redefinition_log();
    let answered = vm_redefined_classes_between(shared, per_vm, oldest, now, |class| {
        names_a_frame_class |= thread.frames.iter().any(|frame| frame.class_id == class);
    });
    if answered && !names_a_frame_class {
        return;
    }
    // Some frame predates `now` (so `now > 0`): look again, now, or -- when
    // the class-manager lock is busy -- at this thread's next safepoint.
    thread.redefinitions_seen = now.wrapping_sub(1);
    convert_obsolete_frames_if_redefined(shared, thread);
}

/// The frame a deoptimization sink just rebuilt from a trapped compiled
/// activation and pushed on top of `thread`'s stack
/// (`deopt_resume::real_frame_deopt_resume_or_throw_and_despeculate`), whose
/// body was compiled at constant-pool stamp `compile_cp_stamp`
/// (`CompiledMethod::compile_cp_stamp`).
///
/// The frame runs the bytecode the door that entered the compiled body held:
/// the code the body was compiled from, of the pool generation current when
/// its compilation began. `Frame::new_pooled` stamped it with the current
/// count, so when the class was redefined while the activation ran (a
/// compiled body keeps running after its class's redefinition made it not
/// entrant), the frame counted as current and its old `ldc` / `new` /
/// field / invoke operands read the new pool: another constant, another
/// class, or a linkage error. The compiled body's own sites translate from
/// that stamp (`jit::helpers::stale_cp_site_index`); the rebuilt frame now
/// does too -- restamped with it, then moved onto its translated body before
/// it runs ([`convert_thawed_frames`]; when the class-manager lock is busy,
/// at the dispatch loop's next top). HotSpot's deoptimized frame of an old
/// compiled method runs the obsolete `Method*`, with its own constants.
/// Interpreter round i1 wave 28, lane L3: the trap-sink part of half 2 of
/// `docs/feature-designs/class-resolution-and-caching.md`.
///
/// A stamp at or after the class's last redefinition changes nothing: the
/// frame is then not stale for its class, and a later redefinition applies
/// the steps after the stamp, as for any frame. Nothing without a stamp.
pub(crate) fn stamp_frame_rebuilt_from_compiled_code(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compile_cp_stamp: Option<u64>,
) {
    let Some(top) = thread.frames.len().checked_sub(1) else {
        return;
    };
    stamp_frame_at_rebuilt_from_compiled_code(shared, thread, top, compile_cp_stamp);}

/// [`stamp_frame_rebuilt_from_compiled_code`] for the frame at `index` of
/// `thread`'s stack: the OUTERMOST frame of an inlined chain a sink just
/// pushed (`deopt_resume::push_inlined_chain`), which runs the bytecode the
/// door that entered the compiled body held -- the body the compile read,
/// of pool generation `compile_cp_stamp` -- while the chain's inner frames,
/// resolved from their classes now, are current (a chain whose spliced
/// callee's class was redefined since the compile is never resumed:
/// `deopt_resume::chain_inner_scope_redefined_since_compile`). Interpreter
/// round i1 wave 37, lane L3: until then only a single rebuilt frame was
/// restamped, and an outermost chain frame whose class was redefined while
/// the activation ran read its old constant-pool indices in the new pool once
/// its inlined callees returned into it.
pub(crate) fn stamp_frame_at_rebuilt_from_compiled_code(
    shared: &SharedVm,
    thread: &mut JvmThread,
    index: usize,
    compile_cp_stamp: Option<u64>,
) {
    let Some(stamp) = compile_cp_stamp else {
        return;
    };
    let Some(frame) = thread.frames.get_mut(index) else {
        return;
    };
    if stamp >= frame.redefine_stamp() {
        return;
    }
    frame.restamp_redefined_body(stamp, false, None);
    convert_thawed_frames(shared, thread);
}

/// Can a frame of `class_id` running `code` (padded, as every frame's code
/// is) with `exception_table`, stamped at constant-pool generation `stamp`,
/// be moved onto the class's current pool by [`convert_frames`]? True when
/// the stamp is not stale for the class, or when every constant-pool operand
/// and catch type translates through the kept history with no widened `ldc`
/// (the case [`translated_body`] could still refuse). `false` for a class
/// with no history. Asked by a deoptimization sink before it rebuilds a
/// trapped frame in the bytecode a compiled body was compiled from, which
/// its class's redefinition has replaced since
/// (`deopt_resume::obsolete_activation_source`; round 13 wave 3, lane
/// replay2): a frame whose translation would be refused would run its old
/// indices against the new pool, so the sink keeps its re-run instead.
pub(crate) fn rebuilt_body_translates(
    shared: &SharedVm,
    class_id: crate::classloading::ClassId,
    code: &[u8],
    exception_table: &[ExceptionTableEntry],
    stamp: u64,
) -> bool {
    let cm = shared.classes.class_manager.read();
    rebuilt_body_translates_in(&cm, class_id, code, exception_table, stamp)
}

/// [`rebuilt_body_translates`] under a class-manager guard the caller holds
/// (interpreter round i1 wave 46, lane L2: the safepoint verdict of a
/// method-entry body forced to leave as its class's obsolete activation
/// asks it under `try_read`, so a compiled frame never blocks on the lock).
pub(crate) fn rebuilt_body_translates_in(
    cm: &ClassManager,
    class_id: crate::classloading::ClassId,
    code: &[u8],
    exception_table: &[ExceptionTableEntry],
    stamp: u64,
) -> bool {
    let Some(history) = cm.redefinition_history(class_id) else {
        return false;
    };
    if !history.is_stale(stamp) {
        return true;
    }
    let Some(unpadded) = code.len().checked_sub(2).and_then(|len| code.get(..len)) else {
        return false;
    };
    let map = |index: u16| history.translate(stamp, index);
    translate_code_widening(unpadded, &map, &[]).is_some_and(|(_, widened)| widened.is_empty())
        && translate_exception_table(exception_table, &map).is_some()
}

/// The redefining thread, right after `ClassManager::redefine_class`
/// released the class-manager lock: move its own frames, then -- when the
/// redefinition of `class_id` gave some old constant-pool index a different
/// constant -- take a handshake pause so every other running thread passes a
/// poll, and so `safepoint_check`, which moves ITS frames, before it resolves
/// another constant of a replaced body. A thread blocked in native code moves
/// its frames when it leaves (`check_post_block_gc`); one frozen in compiled
/// code that never polls moves at its next safepoint -- since wave 37 at its
/// next dispatch-loop top ([`convert_at_loop_top`]; the redefinition armed
/// every loop's poll word, [`RedefinitionFence`]). `None` from the pause
/// request (another pause owns the world) is fine: that pause's polls run
/// `safepoint_check` too.
///
/// No pause when every old index still names its constant -- a transformer
/// that only appended to the pool, as ByteBuddy's inline mock maker does on
/// every retransform: an old body then reads the right constants as it is,
/// and its frames are moved lazily (for the JDWP obsolete mark).
///
/// Returns whether it took a pause in which every other running thread
/// passed a poll (none had to be frozen). The redefinition's loop-exit
/// handshake (`request_withdrawn_body_exits`, interpreter round i1 wave 24,
/// lane L6) is then redundant: every compiled body polled after the
/// redefinition marked its withdrawn bodies, and read the loop-exit verdict
/// on release.
pub(crate) fn after_redefinition(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: crate::classloading::ClassId,
) -> bool {
    convert_obsolete_frames_if_redefined(shared, thread);
    let moved_constants = shared
        .classes
        .class_manager
        .read()
        .redefinition_history(class_id)
        .is_some_and(RedefinitionHistory::last_redefinition_moved_constants);
    if !moved_constants {
        return false;
    }
    // Interpreter round i6 wave 115, lane L3 (proposal
    // `i38-L3-proposal-drop-the-post-swap-conversion-handshake`;
    // `CRATONVM_REDEFINE_NO_POST_SWAP_HANDSHAKE`, default ON since i7, performance):
    // no pause here. Since wave 37 every case this pause covered converts
    // without it -- the fence's drop arms every loop's poll word, so a running
    // loop converts at its next top before its next bytecode
    // (`convert_at_loop_top`); a held loop at its retry
    // (`retry_deferred_conversion`); a blocked thread when it leaves its
    // region (`check_post_block_gc`); a thawed continuation at its remount;
    // compiled code through its compile stamp. Answering "no peer polled"
    // leaves the caller's loop-exit handshake (`request_withdrawn_body_exits`)
    // to run when a compiled body was withdrawn and no batch is open, and a
    // batch takes it once at its end: one pause per HotSwap call at most
    // instead of one per class that moved a constant.
    {
        if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
            eprintln!(
                "[redefine] post-swap handshake skipped (class {})",
                class_id.as_u32()
            );
        }
        return false;
    }
}

/// The redefining thread, under the class-manager write lock and BEFORE
/// `ClassManager::redefine_class` replaces `class_id`'s methods: every
/// thread's published frame snapshot (`ThreadRegistry::for_each_frame_trace`;
/// for a blocked thread, where it is blocked) gets, for its line-less frames
/// of `class_id`, the line the class's current table gives, which is the
/// table of the body they were captured in. Those lines are otherwise
/// resolved when someone reads the snapshot
/// (`ThreadRegistry::frame_trace_of_resolved`), from the class as it is
/// THEN: after the redefinition, the new body's table (interpreter round i1
/// wave 22, lane L3; `L3ObsoleteTraceLines.java`).
/// A Throwable's stored backtrace carries its capture count instead and is
/// resolved against that version when read ([`resolve_lines_as_captured`]);
/// a frame moved onto a replaced body reports its own lines
/// (`Frame::own_line_number`).
///
/// A snapshot published between this walk and the swap is still resolved
/// later, from the new body: that is the not-atomic window of
/// `docs/feature-designs/real-jdk-mode.md`.
/// One pass over the published snapshots (one per thread) per redefinition.
pub(crate) fn resolve_captured_lines_of_class(
    shared: &SharedVm,
    class_store: &crate::classloading::ClassStore,
    class_id: crate::classloading::ClassId,
) {
    use crate::runtime::stackwalker::{resolve_line_numbers_in_place, LINE_NUMBER_UNKNOWN};
    shared
        .threads
        .thread_registry
        .for_each_frame_trace(|trace| {
            if trace
                .iter()
                .any(|e| e.class_id == Some(class_id) && e.line_number == LINE_NUMBER_UNKNOWN)
            {
                // Every other entry is resolved later from an unchanged class, to
                // the same line.
                for entry in trace.iter_mut() {
                    if entry.class_id == Some(class_id) {
                        resolve_line_numbers_in_place(class_store, std::slice::from_mut(entry));
                    }
                }
            }
        });
}

/// The lines of `entries` -- a Throwable's backtrace, captured when
/// `class_redefinition_count()` was `captured_at` -- whose class was
/// redefined since and whose method's `LineNumberTable` a redefinition since
/// replaced: from the table the method had at the capture
/// (`RedefinitionHistory::line_table_at`), as HotSpot resolves a backtrace
/// element against the class version it was captured in. Every other entry
/// is left for `stackwalker::resolve_line_numbers_in_place`, which reads
/// the class as it is. Nothing but a compare unless some class was
/// redefined after the capture. Interpreter round i1 wave 22, lane L3
/// (`L3ObsoleteTraceLines.java`).
///
/// An entry whose descriptor was not captured, or whose version the kept
/// history no longer reaches, keeps the current-class answer. HotSpot
/// answers -1 for a version no frame ran at the redefinition (it drops it);
/// CratonVM answers that version's line while the history keeps it.
///
/// `compiled_at` (interpreter round i1 wave 38, lane L3), when given, holds
/// one stamp per entry: for a COMPILED activation and a callee it inlined,
/// the constant-pool stamp of its compilation
/// (`stackwalker::BacktraceFrame::compile_cp_stamp`). Such a frame ran the
/// bytecode its compilation read, whatever the count at the capture: a body
/// compiled before its class's redefinition keeps running as the obsolete
/// activation (JEP 109; `withdrawn_body_may_leave` does not let it leave), and
/// its bci indexes the OLD body's line table. Before wave 38 its line was
/// resolved at the capture count -- after the redefinition, so from the new
/// body's table
/// (`L3W38HotSwapCompiledLines.java`, row `entry`).
/// An interpreter frame needs none: a thread moves its frames onto their own
/// bodies (which carry their own lines, `Frame::own_line_number`) before it
/// runs them. `CRATONVM_DBG_RETRANSFORM=1` prints a
/// `[redefine] compiled frame resolved at its compile stamp:` line per entry
/// this resolves (the positive control).
pub(crate) fn resolve_lines_as_captured(
    cm: &ClassManager,
    captured_at: u64,
    entries: &mut [crate::native::registry::StackTraceEntry],
    compiled_at: Option<&[Option<u64>]>,
) {
    use crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN;
    let now = cratonvm_classloading::class_redefinition_count();
    let oldest = compiled_at
        .into_iter()
        .flatten()
        .flatten()
        .fold(captured_at, |least, &stamp| least.min(stamp));
    if oldest >= now {
        return;
    }
    let mut dbg = None;
    for (index, entry) in entries.iter_mut().enumerate() {
        if entry.line_number != LINE_NUMBER_UNKNOWN {
            continue;
        }
        let compiled = compiled_at
            .and_then(|stamps| stamps.get(index).copied().flatten())
            .filter(|&stamp| stamp < captured_at);
        let version = compiled.unwrap_or(captured_at);
        let (Some(class_id), Some(descriptor), Ok(bci)) = (
            entry.class_id,
            entry.method_descriptor.as_deref(),
            usize::try_from(entry.byte_code_index),
        ) else {
            continue;
        };
        let Some(history) = cm.redefinition_history(class_id) else {
            continue;
        };
        if !history.is_stale(version) {
            continue;
        }
        if let Some(Some(lines)) = history.line_table_at(version, &entry.method_name, descriptor) {
            if let Some(line) = cratonvm_classloading::obsolete_code::line_number_at(&lines, bci) {
                entry.line_number = i32::from(line);
                if compiled.is_some()
                    && *dbg.get_or_insert_with(|| {
                        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok()                    })
                {
                    eprintln!(
                        "[redefine] compiled frame resolved at its compile stamp: {}.{} bci={bci} \
                         stamp={version} captured_at={captured_at} line={line}",
                        entry.class_name, entry.method_name
                    );
                }
            }
        }
    }
}

/// Does `frame` run a method body its class's redefinition replaced? True
/// for a frame moved onto an obsolete body, and for one built before its
/// class's last redefinition and not (yet) moved. One load for a frame built
/// after the last redefinition in the process.
///
/// When the class-manager lock is held by a writer the answer is `true`: the
/// caller (the OSR door) declines this once and asks again at its next poll.
///
/// Also true for an EMCP frame (bytes equal to the current body's) that
/// carries its body's own line table (interpreter round i1 wave 22, lane
/// L3): an OSR body is compiled from the current method, so the activation
/// would report the NEW body's lines from then on -- its compiled frames and
/// its OSR bci are resolved from the class (`stackwalker::interleave_compiled_frames`,
/// `reline_entry`) -- where HotSpot's old method keeps its own.
///
/// And true for a frame stamped after its class's last redefinition began
/// whose code is not the class's current body and that carries no replaced
/// body (interpreter round i1 wave 24, lane L3): window 3 of
/// `docs/feature-designs/real-jdk-mode.md`,
/// a frame pushed from an invoke-cache entry validated just before the swap,
/// which runs the OLD body although its stamp says current. Its thread's
/// next conversion pass translates it (`convert_frames`); until then its
/// stamp alone let a back edge OSR it into a body compiled from the NEW
/// bytecode. One load while no class was ever redefined; otherwise the
/// class-manager read lock and a lookup of the frame's class (the caller
/// asks only when its back-edge schedule says "attempt") -- unless this
/// thread found the class with no history at the current count
/// ([`NO_HISTORY_SEEN`], interpreter round i1 wave 25, lane L3): once any
/// class of the process was redefined (a mocking agent at test start), every
/// OSR offer of every frame used to take the shared class-manager lock.
pub(crate) fn frame_runs_replaced_code(shared: &SharedVm, frame: &Frame) -> bool {
    if frame.runs_obsolete_method()
        || frame
            .replaced_body()
            .is_some_and(|replaced| replaced.line_numbers.is_some())
    {
        return true;
    }
    let stamp = frame.redefine_stamp();
    let now = cratonvm_classloading::class_redefinition_count();
    if now == 0 {
        return false;
    }
    let key = (shared.vm_identity, frame.class_id.as_u32());
    // Cast: a class id, masked into the memo's index range.
    let way = key.1 as usize & (NO_HISTORY_SEEN_WAYS - 1);
    if NO_HISTORY_SEEN
        .try_with(|ways| {
            ways.get(way)
                .is_some_and(|w| w.get() == (key.0, key.1, now))
        })
        .unwrap_or(false)
    {
        return false;
    }
    let Some(cm) = shared.classes.class_manager.try_read() else {
        // A writer holds it: decline once when the stamp may be stale, and
        // ask again at the next poll.
        return stamp < now;
    };
    let Some(history) = cm.redefinition_history(frame.class_id) else {
        // Read under the lock, which no redefinition holds now: the class
        // was never redefined, and stays so while the count stays `now`.
        let _ = NO_HISTORY_SEEN.try_with(|ways| {
            if let Some(w) = ways.get(way) {
                w.set((key.0, key.1, now));            }        });
        return false;
    };
    history.is_stale(stamp)
        || (stamp >= history.latest_redefinition_began()
            && frame.replaced_body().is_none()
            && runs_current_body(&cm, frame) == Some(false))
        // Interpreter round i1 wave 43, lane L2 (CROSS-LANE): a body its
        // producer read before the last swap, whatever the stamp and the
        // bytes say, until this thread's conversion moves it.
        || body_generation_before(frame, history.latest_redefinition_began()).is_some()
}

/// Interpreter round i1 wave 43, lane L2 (CROSS-LANE;
/// `interpreter-L3-proposal-frames-carry-the-pool-generation-their-door-validated-FIXED-20261011`):
/// may the conversion read which pool a frame's body indexes from the body
/// itself (`CachedBytecodeMethod::pool_generation`, recorded by the
/// invoke-cache fills under the class-manager guard) instead of from the
/// frame's stamp and bytes? `false` keeps the wave-42 rules exactly.
pub(crate) const FRAMES_TRUST_THEIR_BODYS_POOL_GENERATION: bool = true;

/// The constant-pool generation of the body `frame` runs, when its producer
/// recorded one older than `before` (a redefinition's
/// `latest_redefinition_began`) and the frame was not yet moved: such a frame
/// runs the body its door chose before that redefinition's swap, whatever
/// its stamp (pushed inside the swap, or just after it from an entry
/// validated before) and its bytes (a constant-only redefinition leaves them
/// equal) say. `None` for an owned-metadata frame, a producer that recorded
/// nothing, a frame a conversion already moved (obsolete, carrying a replaced
/// body, or marked as predating its class's redefinition), and while
/// [`FRAMES_TRUST_THEIR_BODYS_POOL_GENERATION`] is off.
///
/// A producer reads the generation under the guard it copies the code under,
/// and no class-manager reader sees a count inside a swap, so a generation
/// below `before` names the replaced pool. The only bodies produced inside a
/// swap (the redefining thread's own) record a generation at or above
/// `before` and are the new ones.
fn body_generation_before(frame: &Frame, before: u64) -> Option<u64> {
    if !FRAMES_TRUST_THEIR_BODYS_POOL_GENERATION
        || frame.runs_obsolete_method()
        || frame.predates_its_class_redefinition()
        || frame.replaced_body().is_some()
    {
        return None;
    }
    frame
        .cached_method()?
        .known_pool_generation()
        .filter(|&generation| generation < before)
}

/// How many retired copies a move keeps while the copy the frame runs was
/// never loaded by a dispatch loop ([`convert_frames`]): enough for a frame
/// toggled between two versions to go back onto the other copy instead of
/// minting one per toggle.
const UNLOADED_COPIES_KEPT: usize = 2;

/// What [`convert_frames`] did.
struct Converted {
    /// The index of each moved (or restamped) frame.
    moved: Vec<usize>,
    /// Some frame now runs another code allocation.
    code_moved: bool,
}

/// Move every frame in `frames` that runs a replaced body of a class `cm`
/// holds a history for.
///
/// A body whose translation changed none of its bytes nor its exception
/// table (every old index it names kept its constant -- an identical or
/// append-only redefinition) keeps its code and, when it carries nothing new,
/// its cached method: only its stamp moves (`Frame::restamp_redefined_body`,
/// interpreter round i1 wave 21, lane L4).
///
/// The code a moved frame ran until now stays alive in the frame
/// (`ReplacedBody::retired_code`): a dispatch loop suspended in one of its
/// instructions may still hold a raw pointer into it. Once the loop has
/// reached its top on the frame since the last move
/// (`ReplacedBody::code_reached_top`), no iteration holds such a pointer any
/// more, and the next move frees every copy but the one the frame runs
/// (interpreter round i1 wave 23, lane L3); until then they are kept until
/// the frame is popped (wave 22; before, until the thread ended,
/// `docs/feature-designs/real-jdk-mode.md`).
/// A frame whose translated bytes equal code it still keeps is moved back
/// onto that allocation instead of a new one, and a body equal to the
/// class's current one (EMCP) runs on the class's shared code: an agent that
/// toggles a class between two versions makes each frame alternate between
/// two allocations, not mint one per toggle (interpreter round i1 wave 22,
/// lane L3).
///
/// A frame stamped while its class's last redefinition was under way
/// (`RedefinitionHistory::latest_redefinition_began`) may run either body: a
/// door that did not hold the class-manager lock built it from an invoke
/// cache entry not yet retired (the old body) or from a re-installed vtable
/// snapshot (the new one). Translating the NEW body's indices as if they
/// indexed the old pool would read arbitrary members of the merged pool, so
/// such a frame whose code and exception table are the class's current ones
/// is taken for a frame of the current body and only restamped (interpreter
/// round i1 wave 23, lane L3). An old body byte-identical to the new one is
/// taken for it too and reads the new pool's constants -- which the bytes
/// verified against -- as a frame pushed from a stale cache entry just after
/// the swap does (window 3 of
/// `docs/feature-designs/real-jdk-mode.md`).
///
/// The other way round, window 3 itself: a frame stamped AFTER its class's
/// last redefinition, by a door that validated its invoke-cache entry just
/// before the swap, runs the OLD body although its stamp says current. When
/// that redefinition began after this thread last looked (`seen_before`),
/// such a frame's code differs from the class's current body, and it carries
/// no replaced body and no obsolete mark (which every frame this function
/// restamps onto a body other than the current one gets), it is translated
/// as a frame of the body the last redefinition replaced (interpreter round
/// i1 wave 23, lane L3). A frame pushed that way still runs its own
/// constant-pool instructions until this thread's next conversion pass.
fn convert_frames(cm: &ClassManager, frames: &mut [Frame], seen_before: u64) -> Converted {
    let mut converted = Converted {
        moved: Vec::new(),
        code_moved: false,
    };
    for (index, frame) in frames.iter_mut().enumerate() {
        let Some(history) = cm.redefinition_history(frame.class_id) else {
            continue;
        };
        let stamp = frame.redefine_stamp();
        // Interpreter round i1 wave 43, lane L2 (CROSS-LANE): the generation
        // of the pool the frame's body indexes, as its producer recorded it,
        // when that is older than the last redefinition's swap
        // ([`body_generation_before`]).
        let body_generation = body_generation_before(frame, history.latest_redefinition_began());
        let stamp = if history.is_stale(stamp) {
            stamp
        } else if let Some(generation) = body_generation {
            // Window 3 with the bytes alike: pushed after the swap from an
            // entry chosen before it, which the bytes cannot tell.
            generation
        } else if history.latest_redefinition_began() > seen_before
            && !frame.runs_obsolete_method()
            && frame.replaced_body().is_none()
            && runs_current_body(cm, frame) == Some(false)
        {
            // Pushed from an entry validated before the swap: the pool the
            // last redefinition replaced is the one its code indexes.
            history.latest_redefinition_began().saturating_sub(1)        } else {
            continue;
        };
        let latest = history.latest_redefinition();
        let body = if stamp >= history.latest_redefinition_began()
            && frame.replaced_body().is_none()
            && runs_current_body(cm, frame) == Some(true)
        {
            // Stamped inside the swap with the current bytes: the new body,
            // unless its producer recorded an older generation (the old body,
            // byte-identical to the new one: a constant-only redefinition),
            // which is translated from that generation when the history
            // still reaches it.
            match body_generation
                .and_then(|generation| translated_body(cm, history, frame, generation))
            {
                Some(body) => body,
                None => {
                    frame.restamp_redefined_body(latest, false, None);
                    converted.moved.push(index);
                    continue;
                }
            }
        } else {
            let Some(body) = translated_body(cm, history, frame, stamp) else {
                continue;
            };
            body
        };
        let TranslatedBody {
            code,
            exception_table,
            obsolete,
            replaced,
        } = body;
        // What the frame keeps from its earlier moves, and whether its loop
        // has run it since the last one.
        let (mut retired, reached_top, fresh): (Vec<Arc<[u8]>>, bool, bool) =
            frame.replaced_body().map_or_else(
                || (Vec::new(), false, false),
                |kept| {
                    (
                        kept.retired_code.to_vec(),
                        kept.code_reached_top(),
                        kept.fresh_code,
                    )
                },
            );
        let unchanged = replaced.widened_stream.is_none()
            && frame.code[..] == code[..]
            && frame.exception_table() == &exception_table[..];
        if unchanged {
            // The copies stay as they are: this adds none, and an emptied
            // body would be dropped by `restamp_redefined_body`, leaving the
            // frame the one it carries now.
            let replaced = ReplacedBody {
                retired_code: retired.into_boxed_slice(),
                // Same code: what the loop was seen running still is, and a
                // copy no loop has loaded yet still has not been.
                code_reached_top: std::sync::atomic::AtomicBool::new(reached_top),
                fresh_code: fresh,
                ..replaced
            };
            frame.restamp_redefined_body(latest, obsolete, Some(Arc::new(replaced)));
            // An old version of its method, obsolete or EMCP: its
            // `StackWalker` frame has no line (wave 39).
            frame.mark_predates_its_class_redefinition();
            converted.moved.push(index);
            continue;
        }
        // Back onto an allocation this frame still keeps, when the bytes are
        // the same. Not for a widened body: its stream is built over `code`.
        // Such an allocation may be the one a suspended iteration holds.
        let (code, fresh_code) = match retired.iter().position(|kept| kept[..] == code[..]) {
            Some(at) if replaced.widened_stream.is_none() => (retired.swap_remove(at), false),
            _ => (code, true),
        };
        if reached_top {
            // No loop iteration holds a pointer into anything older than the
            // code the frame runs now.
            retired.clear();
        }
        let running = Arc::clone(&frame.code);
        // A copy an earlier move installed that no loop has loaded at a top
        // since is held by no iteration (interpreter round i1 wave 24, lane
        // L3; `ReplacedBody::fresh_code`): it is kept only while fewer than
        // `UNLOADED_COPIES_KEPT` copies are, for a move back onto its bytes
        // (an agent toggling two versions), and freed otherwise. So a frame
        // suspended in one instruction across many moves keeps the copy that
        // iteration holds, one more and its current one -- not one per move.
        let never_loaded = fresh && !reached_top;
        if (!never_loaded || retired.len() < UNLOADED_COPIES_KEPT)
            && !retired.iter().any(|kept| Arc::ptr_eq(kept, &running))
        {
            retired.push(running);
        }
        let replaced = ReplacedBody {
            retired_code: retired.into_boxed_slice(),
            fresh_code,
            ..replaced
        };
        // The code it ran until now is in `replaced.retired_code`.
        let _ = frame.adopt_redefined_body(
            code,
            exception_table,
            latest,
            obsolete,
            Some(Arc::new(replaced)),
        );
        frame.mark_predates_its_class_redefinition();
        converted.moved.push(index);
        converted.code_moved = true;
    }
    converted
}

/// Are `frame`'s code and exception table the ones the class's current
/// method of its name and descriptor has? `None` when the frame has no code
/// (a synthetic bridge frame) or the class has no such method with code (a
/// native method's frame, say): nothing to compare.
fn runs_current_body(cm: &ClassManager, frame: &Frame) -> Option<bool> {
    if frame.code.len() <= 2 {
        return None;
    }
    let current = cm
        .get_class(frame.class_id)
        .and_then(|class| {
            class.methods.iter().find(|m| {
                &*m.name == frame.method_name() && &*m.descriptor == frame.method_descriptor()
            })
        })
        .and_then(|m| m.code())?;
    let own = frame.code.get(..frame.code.len().checked_sub(2)?)?;
    Some(current.code[..] == own[..] && current.exception_table[..] == frame.exception_table()[..])
}

/// A frame's body with its constant-pool operands in the current pool.
struct TranslatedBody {
    /// Padded, as every frame's code is.
    code: Arc<[u8]>,
    exception_table: Arc<[ExceptionTableEntry]>,
    /// The translated body differs from the class's current body of the same
    /// method: the frame runs an obsolete method (JDWP `Method.IsObsolete`).
    /// A body equal modulo the constant pool (HotSpot's EMCP) is not.
    obsolete: bool,
    /// Its line table and widened `ldc` sites (interpreter round i1 wave 21,
    /// lane L4).
    replaced: ReplacedBody,
}

/// `frame`'s code and exception table translated from the pool generation
/// `stamp` names into `cm`'s current pool, or `None` when some operand has no
/// translation (the history no longer reaches the stamp) or the body cannot
/// be pre-decoded whole where an `ldc` needs a widened stream.
///
/// An `ldc` whose constant the merged pool holds above index 255 keeps its
/// (now stale) operand byte and is decoded from a stream that reads the
/// merged index instead (`QuickenedCode::with_widened_ldc`); such a frame is
/// kept off the raw-bytecode fast path (`replaced_body_fast_path_verdict`)
/// and is always obsolete. Before wave 21 it was left untranslated, reading
/// the new pool at its old indices.
fn translated_body(
    cm: &ClassManager,
    history: &RedefinitionHistory,
    frame: &Frame,
    stamp: u64,
) -> Option<TranslatedBody> {
    // Every index the translated body names, for the history's live set
    // (`RedefinitionHistory::note_moved_names`; interpreter round i1 wave 24,
    // lane L3): the class's merged pool is compacted to what kept frames can
    // name, and a moved frame's code names what these produce.
    let named = std::cell::RefCell::new(Vec::new());
    let map = |index: u16| {
        let merged = history.translate(stamp, index);
        if let Some(merged) = merged {
            named.borrow_mut().push(merged);
        }
        merged
    };
    let unpadded = frame.code.len().checked_sub(2)?;
    let prior = frame
        .replaced_body()
        .map_or(&[][..], |replaced| &replaced.widened_ldc[..]);
    let (code, widened) = translate_code_widening(frame.code.get(..unpadded)?, &map, prior)?;
    let exception_table = translate_exception_table(frame.exception_table(), &map)?;
    history.note_moved_names(&named.borrow());
    let current = cm
        .get_class(frame.class_id)
        .and_then(|class| {
            class.methods.iter().find(|m| {
                &*m.name == frame.method_name() && &*m.descriptor == frame.method_descriptor()
            })
        })
        .and_then(|m| m.code());
    let obsolete = !widened.is_empty()
        || !current.is_some_and(|current| {
            current.code[..] == code[..] && current.exception_table[..] == exception_table[..]
        });
    // An EMCP body is the class's current one byte for byte: run the frame on
    // the class's shared allocation (the one its fresh frames get) rather
    // than a private copy (interpreter round i1 wave 22, lane L3).
    let code = if obsolete {
        crate::runtime::frame::padded_bytecode(&code)
    } else {
        crate::runtime::frame::padded_bytecode_for_method(
            frame.class_id,
            frame.method_name(),
            frame.method_descriptor(),
            &code,
            &exception_table,
        )
    };
    let widened_stream = if widened.is_empty() {
        None
    } else {
        Some(cratonvm_reader::QuickenedCode::with_widened_ldc(
            &code, &widened,
        )?)
    };
    // The body's own lines: those it already carries from an earlier move,
    // else the table the class's method had at `stamp` if a redefinition
    // since replaced it. A stamp older than the history keeps (reachable here
    // only by a body with no constant-pool operand) leaves the class's table,
    // as before wave 21.
    let line_numbers = match frame
        .replaced_body()
        .and_then(|replaced| replaced.line_numbers.clone())
    {
        Some(own) => Some(own),
        None => history
            .line_table_at(stamp, frame.method_name(), frame.method_descriptor())
            .flatten(),
    };
    Some(TranslatedBody {
        code,
        exception_table: Arc::from(exception_table),
        obsolete,
        replaced: ReplacedBody {
            line_numbers,
            widened_ldc: widened.into_boxed_slice(),
            widened_stream,
            // Filled by `convert_frames`.
            retired_code: Box::new([]),
            // A fresh body: no loop has reached its top on it yet.
            code_reached_top: std::sync::atomic::AtomicBool::new(false),
            // Filled by `convert_frames`.
            fresh_code: false,
            // Built when the dispatch loop first decodes the frame's code.
            decoded: std::sync::OnceLock::new(),
        },
    })
}

#[cfg(test)]
mod tests;
