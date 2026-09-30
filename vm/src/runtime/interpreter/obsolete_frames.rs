// SPDX-License-Identifier: Apache-2.0
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
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
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
/// (`docs/internal/fixed-bugs/interpreter-L3-a-fenced-loop-waits-out-the-retry-bound-when-its-thread-holds-the-class-manager-lock-FIXED-20261010.md`;
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
        if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
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
    if thread.frames.conversion_retries() <= crate::runtime::frame::FrameStack::CONVERSION_RETRY_YIELDS {
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
        && cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok()
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
/// `docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md`),
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
                && cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok()
            {
                eprintln!(
                    "[redefine] fenced loop top holds the class-manager lock: thread {} count {now}",
                    thread.thread_id.0
                );
            }
        } else if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
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
    let answered = cratonvm_classloading::for_each_redefined_class_between(seen, now, |class| {
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
/// `docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md`.
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
/// (`tools/probes/interp/L3/L3W37HotSwapSpinningReads.java`). HotSpot runs the
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
        let dbg = cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM")
            .is_ok()
            .then(|| {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map_or_else(|| class_id.as_u32().to_string(), |class| class.name.to_string())
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
/// (`tools/probes/interp/L3/L3W37RedefineTwoClassesAtOnce.java`; HotSpot 25
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
        "skipped (no class moves a constant)"
    };
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!(
            "[redefine] fence raised for a call of {} classes; handshake {handshake}",
            classes.len()
        );
    }
    true
}

/// Lower the fence [`raise_install_fence`] raised; the held loops move their
/// frames at their next top.
pub(crate) fn lower_install_fence(shared: &SharedVm, owner: crate::threading::jvm_thread::ThreadId) {
    shared.mem.gc_barrier.lower_redefinition_fence(owner);
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
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
                    barrier.redefinition_fence_waits().saturating_sub(self.waits_before)
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
        shared.threads.thread_registry.for_each_gc_block_state(|state| {
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
    let answered = cratonvm_classloading::for_each_redefined_class_between(oldest, now, |class| {
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
/// `docs/internal/fixed-bugs/interpreter-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool-RETIRED-20261003.md`.
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
    stamp_frame_at_rebuilt_from_compiled_code(shared, thread, top, compile_cp_stamp);
}

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
    let Some(pause) = super::NonMovingPause::request_handshake(
        shared,
        thread.thread_id,
        super::gc_events::NonCollectionPause::Redefinition,
        REDEFINITION_GRACE,
    ) else {
        return false;
    };
    let every_peer_polled = !pause.froze_peers();
    drop(pause);
    every_peer_polled
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
/// wave 22, lane L3; `tools/probes/interp/L3/L3ObsoleteTraceLines.java`).
/// A Throwable's stored backtrace carries its capture count instead and is
/// resolved against that version when read ([`resolve_lines_as_captured`]);
/// a frame moved onto a replaced body reports its own lines
/// (`Frame::own_line_number`).
///
/// A snapshot published between this walk and the swap is still resolved
/// later, from the new body: that is the not-atomic window of
/// `docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md`.
/// One pass over the published snapshots (one per thread) per redefinition.
pub(crate) fn resolve_captured_lines_of_class(
    shared: &SharedVm,
    class_store: &crate::classloading::ClassStore,
    class_id: crate::classloading::ClassId,
) {
    use crate::runtime::stackwalker::{resolve_line_numbers_in_place, LINE_NUMBER_UNKNOWN};
    shared.threads.thread_registry.for_each_frame_trace(|trace| {
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
/// (`tools/probes/interp/L3/L3ObsoleteTraceLines.java`).
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
/// (`tools/probes/interp/L3/L3W38HotSwapCompiledLines.java`, row `entry`).
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
                        cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok()
                    })
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
/// `docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md`,
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
        .try_with(|ways| ways.get(way).is_some_and(|w| w.get() == (key.0, key.1, now)))
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
                w.set((key.0, key.1, now));
            }
        });
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
/// `i40-L3-proposal-frames-carry-the-pool-generation-their-door-validated`):
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
/// `docs/internal/fixed-bugs/interpreter-L3-retired-obsolete-code-is-kept-for-the-threads-lifetime-FIXED-20260927.md`).
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
/// `docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md`).
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
            history.latest_redefinition_began().saturating_sub(1)
        } else {
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
            match body_generation.and_then(|generation| {
                translated_body(cm, history, frame, generation)
            }) {
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
        let (mut retired, reached_top, fresh): (Vec<Arc<[u8]>>, bool, bool) = frame
            .replaced_body()
            .map_or_else(
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
mod tests {
    use super::*;
    use crate::classloading::ClassId;
    use crate::threading::jvm_thread::ThreadId;
    use crate::types::Value;
    use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

    /// A version-52 class `obsolete/VmProbe` whose one method
    /// `static value()I` is `ldc #8; ireturn`, `#8` being
    /// `CONSTANT_Integer constant`.
    fn ldc_class(constant: i32) -> Vec<u8> {
        ldc_class_with_code(constant, &[0x12, 8, 0xac]) // ldc #8; ireturn
    }

    /// [`ldc_class`] whose `value()I` is `code` (max_stack 1, no locals).
    fn ldc_class_with_code(constant: i32, code: &[u8]) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, "obsolete/VmProbe"); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()I"); // #6
        utf8(&mut b, "Code"); // #7
        b.push(3); // #8 Integer
        b.extend_from_slice(&constant.to_be_bytes());
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(code);
        b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    /// A frame of `value()` as a call would have pushed it, from the body
    /// the class has now.
    fn value_frame(shared: &SharedVm, cid: ClassId) -> Option<Frame> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        let method = class.methods.iter().find(|m| &*m.name == "value")?;
        let code = method.code()?;
        Some(Frame::new_from_arcs(
            cid,
            Arc::clone(&class.name),
            Arc::from("value"),
            Arc::from("()I"),
            None,
            crate::runtime::frame::padded_bytecode(&code.code[..]),
            Arc::from(Vec::<ExceptionTableEntry>::new()),
            code.max_stack,
            code.max_locals,
            &[],
        ))
    }

    /// The i18-L3 page's unit test: a frame running `value()` when its class
    /// is redefined with a pool that puts another constant at the index its
    /// `ldc` names still loads the OLD constant -- as HotSpot runs an
    /// obsolete method -- while a fresh call runs the new body. Before wave
    /// 19 the old frame read the new pool and returned 80000.
    #[test]
    fn a_frame_running_across_a_redefinition_keeps_its_constants() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_119), "obsolete-frames");
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            match cm.define_class_with_options(
                "obsolete/VmProbe",
                &ldc_class(70_000),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                // The stripped test VM could not define it: nothing to run.
                Err(_) => return,
            }
        };
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(frame);
        let stamp = thread.frames[0].redefine_stamp();
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        assert!(
            frame_runs_replaced_code(&shared, &thread.frames[0]),
            "built before the redefinition, not yet moved"
        );

        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        let moved = &thread.frames[0];
        assert!(moved.redefine_stamp() > stamp);
        assert!(
            moved.runs_obsolete_method(),
            "its constant differs from the current body's: an obsolete method"
        );
        assert!(frame_runs_replaced_code(&shared, moved));
        assert_eq!(retired(moved), 1, "the code it ran is kept with it");
        let operand = moved.code[1];
        assert_ne!(
            operand, 8,
            "the ldc operand now names the old constant's merged index"
        );
        {
            let cm = shared.classes.class_manager.read();
            let pool = &cm.get_class(cid).expect("loaded").constant_pool;
            assert!(matches!(
                pool.get(u16::from(operand)),
                Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                    70_000
                ))
            ));
        }
        // Idempotent: nothing moved since.
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        assert_eq!(retired(&thread.frames[0]), 1);

        // Run the old frame to its `ireturn`: the old constant.
        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the frame that was running keeps its constant, got {old:?}"
        );
        // A fresh call runs the new body.
        let fresh =
            crate::runtime::interpreter::execute(&shared, &mut thread, cid, "value", "()I", &[]);
        assert!(
            matches!(fresh, Ok(Some(Value::Int(80_000)))),
            "a new call runs the new body, got {fresh:?}"
        );
    }

    /// A frame built after the redefinition runs the current body and is
    /// left alone; a frame whose body equals the new one modulo the pool
    /// (EMCP) is moved but not obsolete. Here every constant kept its index,
    /// so the older frame keeps its code and is only restamped (wave 21,
    /// lane L4): nothing is retired.
    #[test]
    fn current_and_emcp_frames_are_not_obsolete() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_120), "obsolete-frames-emcp");
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            match cm.define_class_with_options(
                "obsolete/VmProbe",
                &ldc_class(5_000_000),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                Err(_) => return,
            }
        };
        let Some(before) = value_frame(&shared, cid) else {
            return;
        };
        let stamp = before.redefine_stamp();
        let code = Arc::clone(&before.code);
        thread.frames.push(before);
        // The same bytes again: every constant keeps its index.
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(5_000_000), RedefineOptions::default())
            .expect("an identical redefinition succeeds");
        let Some(after) = value_frame(&shared, cid) else {
            return;
        };
        assert!(!frame_runs_replaced_code(&shared, &after), "built after");
        thread.frames.push(after);
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        assert!(
            !thread.frames[0].runs_obsolete_method(),
            "EMCP: not obsolete"
        );
        assert!(!frame_runs_replaced_code(&shared, &thread.frames[0]));
        assert!(thread.frames[0].redefine_stamp() > stamp, "restamped");
        assert!(
            Arc::ptr_eq(&thread.frames[0].code, &code),
            "an unchanged body keeps its code"
        );
        assert_eq!(retired(&thread.frames[0]), 0, "no private copy, nothing retired");
    }

    /// The code allocations `frame` keeps from its earlier moves.
    fn retired(frame: &Frame) -> usize {
        frame
            .replaced_body()
            .map_or(0, |replaced| replaced.retired_code.len())
    }

    /// The i21-L4 page's unit test (interpreter round i1 wave 22, lane L3):
    /// one frame across 101 redefinitions that alternate its constant between
    /// two values at the same index, moved after each. It used to retire one
    /// private copy per move onto the thread (100 by the end); it now
    /// alternates between two allocations -- the one it was built on and one
    /// private copy -- and keeps the other with itself. It still returns its
    /// own constant, and a fresh call the class's.
    #[test]
    fn a_frame_toggled_between_two_versions_keeps_two_code_copies() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_123), "obsolete-frames-toggle");
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(frame);
        let mut seen: Vec<usize> = Vec::new();
        for round in 1..=101 {
            let constant = if round % 2 == 1 { 80_000 } else { 70_000 };
            redefine_probe(&shared, cid, ldc_class(constant));
            convert_obsolete_frames_if_redefined(&shared, &mut thread);
            let moved = &thread.frames[0];
            assert!(
                retired(moved) <= 1,
                "round {round}: {} copies kept",
                retired(moved)
            );
            // Cast: an allocation address, compared only.
            let at = moved.code.as_ptr() as usize;
            if !seen.contains(&at) {
                seen.push(at);
            }
            assert_eq!(
                moved.runs_obsolete_method(),
                round % 2 == 1,
                "round {round}: obsolete only while the class holds the other constant"
            );
        }
        assert_eq!(seen.len(), 2, "two allocations, reused in turn");

        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the frame that was running keeps its constant, got {old:?}"
        );
        let fresh =
            crate::runtime::interpreter::execute(&shared, &mut thread, cid, "value", "()I", &[]);
        assert!(
            matches!(fresh, Ok(Some(Value::Int(80_000)))),
            "a new call runs the new body, got {fresh:?}"
        );
    }

    /// A version-52 `obsolete/VmProbe` like [`ldc_class`] (`value()I` is
    /// `ldc #8; ireturn`, `#8` = `CONSTANT_Integer constant`), whose method
    /// has a `LineNumberTable` putting pc 0 on `line`, and whose pool ends
    /// with `padding` unused UTF-8 entries.
    fn probe_class(constant: i32, line: u16, padding: u16) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&(10 + padding).to_be_bytes()); // constant_pool_count
        utf8(&mut b, "obsolete/VmProbe"); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()I"); // #6
        utf8(&mut b, "Code"); // #7
        b.push(3); // #8 Integer
        b.extend_from_slice(&constant.to_be_bytes());
        utf8(&mut b, "LineNumberTable"); // #9
        for i in 0..padding {
            utf8(&mut b, &("pad".to_string() + &i.to_string())); // #10..
        }
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
        let code = [0x12, 8, 0xac]; // ldc #8; ireturn
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32 + 12).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 0]); // no handlers
        b.extend_from_slice(&[0, 1]); // one attribute
        b.extend_from_slice(&[0, 9, 0, 0, 0, 6, 0, 1, 0, 0]); // LineNumberTable, pc 0
        b.extend_from_slice(&line.to_be_bytes());
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    fn define_probe(shared: &SharedVm, bytes: &[u8]) -> Option<ClassId> {
        shared
            .classes
            .class_manager
            .write()
            .define_class_with_options(
                "obsolete/VmProbe",
                bytes,
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            )
            .ok()
    }

    /// Redefine the probe class the way the VM's path leaves THIS VM: the
    /// swap, then what `vm_init::resolution_invalidate_adapter` does for the
    /// VM that owns the class store -- a new resolution epoch and a sweep of
    /// the class's recorded constants. A bare `SharedVm` is not in that
    /// adapter's live-VM registry (`Vm::new` registers), so without the last
    /// two lines a constant one frame recorded at an index before the swap is
    /// served to another frame's `ldc` of that index after it (interpreter
    /// round i1 wave 25, lane L3: the thaw half of
    /// `a_conversion_the_busy_class_manager_deferred_runs_before_the_next_bytecode`
    /// read `#9`'s old 70000 from the resolution cache, not from the pool).
    fn redefine_probe(shared: &SharedVm, cid: ClassId, bytes: Vec<u8>) {
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, bytes, RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        crate::runtime::interpreter::bump_resolution_epoch_in(shared);
        shared
            .classes
            .resolution_cache
            .write()
            .invalidate_class(cid);
    }

    /// The i19-L3 page's case 1 (interpreter round i1 wave 21, lane L4): the
    /// new pool has 309 entries, so the old `ldc #8`'s constant is appended
    /// above index 255 where one operand byte cannot name it. The frame used
    /// to be left untranslated and read the NEW constant at `#8`; it now
    /// runs on a stream that reads the merged index, off the fast path, and
    /// returns the old constant as HotSpot's obsolete method does.
    #[test]
    fn an_ldc_whose_old_constant_lands_above_a_byte_runs_on_a_widened_stream() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_121), "obsolete-frames-wide");
        let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
            return;
        };
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(frame);
        redefine_probe(&shared, cid, probe_class(80_000, 10, 299));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        let moved = &thread.frames[0];
        assert!(moved.runs_obsolete_method(), "a widened body is obsolete");
        assert_eq!(moved.code[1], 8, "the operand byte is left as it was");
        let Some(stream) = moved.widened_stream() else {
            unreachable!("the frame runs on a widened stream");
        };
        let Some((cratonvm_reader::instruction::Instruction::LdcW(merged), 2)) = stream.resolve(0)
        else {
            unreachable!("pc 0 decodes as ldc_w");
        };
        let merged = *merged;
        assert!(merged > 255, "appended after the 309-entry pool: #{merged}");
        {
            let cm = shared.classes.class_manager.read();
            let pool = &cm.get_class(cid).expect("loaded").constant_pool;
            assert!(matches!(
                pool.get(merged),
                Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                    70_000
                ))
            ));
        }
        assert!(
            !super::super::frame_fast_path_admitted(&shared, &thread.frames[0]),
            "the fast path would read the stale operand byte"
        );
        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the frame that was running keeps its constant, got {old:?}"
        );
        let fresh =
            crate::runtime::interpreter::execute(&shared, &mut thread, cid, "value", "()I", &[]);
        assert!(
            matches!(fresh, Ok(Some(Value::Int(80_000)))),
            "a new call runs the new body, got {fresh:?}"
        );
    }

    /// The i19-L3 line-number page (interpreter round i1 wave 21, lane L4):
    /// a frame running a body whose `LineNumberTable` a redefinition replaced
    /// reports the old body's line in a stack trace, through a second
    /// redefinition too; a frame of the current body resolves from the class.
    #[test]
    fn a_moved_frame_reports_the_lines_of_its_own_body() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_122), "obsolete-frames-lines");
        let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
            return;
        };
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(frame);
        // Same code and pool, the statement moved to line 20: EMCP.
        redefine_probe(&shared, cid, probe_class(70_000, 20, 0));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        assert!(!thread.frames[0].runs_obsolete_method(), "EMCP");
        assert_eq!(thread.frames[0].own_line_number(0), Some(10));
        let trace = crate::runtime::stackwalker::capture_frames_no_lines_exact(&thread.frames);
        assert_eq!(trace[0].line_number, 10, "the line of the source it runs");

        // Moved again, it keeps its own table rather than the one it was
        // stamped against (line 20's).
        redefine_probe(&shared, cid, probe_class(70_000, 30, 0));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        assert_eq!(thread.frames[0].own_line_number(0), Some(10));

        let Some(current) = value_frame(&shared, cid) else {
            return;
        };
        assert_eq!(current.own_line_number(0), None, "the class answers");
        thread.frames.push(current);
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        let trace = crate::runtime::stackwalker::capture_frames_no_lines_exact(&thread.frames);
        assert_eq!(trace[0].line_number, 10);
        assert_eq!(
            trace[1].line_number,
            crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN,
            "resolved later, from the class"
        );
        // Wave 22, lane L3: the EMCP frame with its own lines stays off OSR
        // (an OSR body would report the current body's lines); the current
        // frame does not.
        assert!(!thread.frames[0].runs_obsolete_method());
        assert!(frame_runs_replaced_code(&shared, &thread.frames[0]));
        assert!(!frame_runs_replaced_code(&shared, &thread.frames[1]));
    }

    /// Interpreter round i1 wave 39, lane L3: a `StackWalker` capture gives
    /// an activation begun before its class's redefinition no line -- an
    /// EMCP one with identical bytes and lines too, as HotSpot's walk does
    /// (`tools/probes/interp/L3/L3W39HotSwapWalkerLines.java`) -- before and
    /// after its thread moved it, while a frame of the current body keeps its
    /// line and the `Throwable`-shaped capture keeps the old activation's.
    #[test]
    fn a_stack_walk_gives_an_old_activation_no_line() {
        use crate::runtime::stackwalker::{
            capture_full_trace, capture_stack_walk_trace, LINE_NUMBER_UNKNOWN,
        };
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_139), "obsolete-frames-walk");
        let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
            return;
        };
        let Some(old) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(old);
        redefine_probe(&shared, cid, probe_class(70_000, 10, 0));
        {
            // Not moved yet: its stale stamp says it.
            let cm = shared.classes.class_manager.read();
            let walk = capture_stack_walk_trace(&cm, &thread.frames);
            assert_eq!(walk[0].line_number, LINE_NUMBER_UNKNOWN, "unmoved");
        }
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        assert!(!thread.frames[0].runs_obsolete_method(), "identical: EMCP");
        assert!(thread.frames[0].predates_its_class_redefinition());
        let Some(current) = value_frame(&shared, cid) else {
            return;
        };
        assert!(!current.predates_its_class_redefinition());
        thread.frames.push(current);
        let cm = shared.classes.class_manager.read();
        let walk = capture_stack_walk_trace(&cm, &thread.frames);
        assert_eq!(walk[0].line_number, LINE_NUMBER_UNKNOWN, "the old activation");
        assert_eq!(walk[1].line_number, 10, "the current body's line");
        let full = capture_full_trace(&cm.class_store, &thread.frames);
        assert_eq!(full[0].line_number, 10, "a Throwable keeps the old line");
        assert_eq!(full[1].line_number, 10);
    }

    /// Interpreter round i1 wave 22, lane L3: a frame frozen into a
    /// continuation before its class was redefined, thawed onto a carrier
    /// that had already looked at that redefinition, is moved before it runs
    /// (`convert_thawed_frames`, from `resume_continuation`) and loads its
    /// own constant. Before, the carrier's `redefinitions_seen` kept every
    /// conversion pass away until the next redefinition anywhere, and the
    /// frame read the new pool: 80000.
    #[test]
    fn a_frame_thawed_after_a_redefinition_is_moved_before_it_runs() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_125), "obsolete-frames-thaw");
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        let frozen = frame.to_frozen_frame();
        redefine_probe(&shared, cid, ldc_class(80_000));
        // The carrier looks at the redefinition with no frame of the class.
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        thread.frames.push(Frame::from_frozen_frame(frozen));
        convert_thawed_frames(&shared, &mut thread);
        assert!(
            thread.frames[0].runs_obsolete_method(),
            "moved onto its translated body before it runs"
        );
        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the thawed frame keeps its constant, got {old:?}"
        );
    }

    /// Interpreter round i1 wave 28, lane L3: a deoptimization sink rebuilds
    /// the frame of a compiled activation from the code the body was compiled
    /// from, after the class was redefined under the running activation.
    /// `Frame::new_pooled` stamps that frame current, so no conversion pass
    /// ever moved it and its `ldc #8` read the new pool: 80000.
    /// `stamp_frame_rebuilt_from_compiled_code` restamps it with the body's
    /// compile stamp and moves it before it runs.
    #[test]
    fn a_frame_rebuilt_from_a_stale_compiled_body_reads_its_own_constants() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_133), "obsolete-frames-deopt");
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        // The body is compiled now, from the class's current code.
        let compile_stamp = cratonvm_classloading::class_redefinition_count();
        let Some(compiled_from) = value_frame(&shared, cid) else {
            return;
        };
        let old_code = Arc::clone(&compiled_from.code);
        redefine_probe(&shared, cid, ldc_class(80_000));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        // The trap: the sink rebuilds the frame from the old code, stamped now.
        let rebuilt = || {
            Frame::new_from_arcs(
                cid,
                Arc::from("obsolete/VmProbe"),
                Arc::from("value"),
                Arc::from("()I"),
                None,
                Arc::clone(&old_code),
                Arc::from(Vec::<ExceptionTableEntry>::new()),
                1,
                0,
                &[],
            )
        };
        // The control: stamped current, the frame is never moved.
        thread.frames.push(rebuilt());
        convert_thawed_frames(&shared, &mut thread);
        assert!(
            !thread.frames[0].runs_obsolete_method(),
            "a frame stamped current is not taken for a replaced body"
        );
        let unstamped = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(unstamped, Ok(Some(Value::Int(80_000)))),
            "the control reads the new pool at its old index, got {unstamped:?}"
        );

        // `execute_frame` leaves the finished frame on the stack: start each
        // case from an empty one, so `frames[0]` is the frame under test.
        thread.frames.clear();
        thread.frames.push(rebuilt());
        stamp_frame_rebuilt_from_compiled_code(&shared, &mut thread, Some(compile_stamp));
        assert!(
            thread.frames[0].runs_obsolete_method(),
            "moved onto its translated body before it runs"
        );
        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the rebuilt frame keeps its constant, got {old:?}"
        );

        // A body compiled after the redefinition: its stamp changes nothing.
        let now = cratonvm_classloading::class_redefinition_count();
        let Some(current) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.clear();
        thread.frames.push(current);
        stamp_frame_rebuilt_from_compiled_code(&shared, &mut thread, Some(now));
        assert!(!thread.frames[0].runs_obsolete_method());
        let fresh = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(fresh, Ok(Some(Value::Int(80_000)))),
            "the current body reads the current pool, got {fresh:?}"
        );
    }

    /// Interpreter round i1 wave 25, lane L3: a continuation remounted after
    /// the redefinition of a class none of its frames runs takes no
    /// conversion pass -- it used to take the class-manager lock and look up
    /// every frame's class on every remount once any class of the process
    /// had been redefined -- when the redefinition ring can say so.
    #[test]
    fn a_thaw_after_an_unrelated_redefinition_takes_no_conversion_pass() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let unrelated = Frame::new(
            ClassId::new(0x00F1_0C78),
            "obsolete/Unrelated".to_string(),
            "run".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            Vec::new(),
            1,
            1,
            &[],
        );
        let stamp = unrelated.redefine_stamp();
        let mut thread = JvmThread::new(ThreadId(4_153), "obsolete-frames-unrelated-thaw");
        thread.frames.push(unrelated);
        redefine_probe(&shared, cid, ldc_class(80_000));
        let seen = thread.redefinitions_seen;
        let before = cratonvm_classloading::class_redefinition_count();
        let answerable =
            cratonvm_classloading::for_each_redefined_class_between(stamp, before, |_| {});
        convert_thawed_frames(&shared, &mut thread);
        // Other tests redefine classes concurrently: only a quiet span, which
        // the ring answered, says what the pass would have done.
        if answerable && cratonvm_classloading::class_redefinition_count() == before {
            assert_eq!(
                thread.redefinitions_seen, seen,
                "no pass for frames no recorded redefinition names"
            );
        }
    }

    /// Interpreter round i1 wave 22, lane L3: a Throwable's backtrace and a
    /// blocked thread's published snapshot, both captured in a body whose
    /// line table a redefinition then replaces, report the line of the body
    /// they were captured in -- the backtrace through its capture count
    /// (`resolve_lines_as_captured`), the snapshot because
    /// `resolve_captured_lines_of_class` ran before the swap (as
    /// `redefine_class_with` does). Both used to be resolved when read, from
    /// the new table (line 20 here); HotSpot keeps the class version the
    /// element was captured in.
    #[test]
    fn traces_captured_before_a_redefinition_keep_their_lines() {
        use crate::runtime::stackwalker::{capture_frames_no_lines, capture_frames_no_lines_exact};
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
            return;
        };
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        let frames = [frame];

        let throwable = shared.mem.heap.alloc_object(ClassId::new(7), 1);
        shared.store_throwable_stack_trace(
            throwable,
            Arc::from(capture_frames_no_lines_exact(&frames)),
        );
        let registry = &shared.threads.thread_registry;
        let tid = ThreadId(4_124);
        registry.register(tid, "obsolete-frames-blocked", None);
        registry.set_frame_trace(
            tid,
            Arc::new(parking_lot::Mutex::new(capture_frames_no_lines(&frames))),
        );

        {
            let cm = shared.classes.class_manager.read();
            resolve_captured_lines_of_class(&shared, &cm.class_store, cid);
        }
        // Same code and pool; the statement moved to line 20.
        redefine_probe(&shared, cid, probe_class(70_000, 20, 0));

        let thrown = shared
            .throwable_stack_trace_for(throwable)
            .expect("the backtrace is stored");
        assert_eq!(thrown[0].line_number, 10, "the Throwable's own body");
        let blocked = {
            let cm = shared.classes.class_manager.read();
            registry.frame_trace_of_resolved(tid, &cm.class_store)
        };
        assert_eq!(blocked[0].line_number, 10, "the blocked thread's own body");

        // A trace captured after the swap resolves from the new table.
        let Some(fresh) = value_frame(&shared, cid) else {
            return;
        };
        let later = shared.mem.heap.alloc_object(ClassId::new(7), 1);
        shared.store_throwable_stack_trace(later, Arc::from(capture_frames_no_lines_exact(&[fresh])));
        let thrown = shared
            .throwable_stack_trace_for(later)
            .expect("the backtrace is stored");
        assert_eq!(thrown[0].line_number, 20, "the current body");
    }

    /// Interpreter round i1 wave 38, lane L3
    /// (`L3/L3W38HotSwapCompiledLines`, row `entry`): a COMPILED activation of
    /// a body compiled before its class's redefinition keeps running it, and
    /// a Throwable it captures AFTER the redefinition reports the line of that
    /// body -- through the frame's compile stamp -- not the new body's table
    /// read at the old bci. A compiled frame with no stamp, and one compiled
    /// after the redefinition, resolve from the class as it is.
    #[test]
    fn a_compiled_frame_captured_after_a_redefinition_reports_its_own_body() {
        use crate::runtime::stackwalker::BacktraceFrame;
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
            return;
        };
        let compiled_before = cratonvm_classloading::class_redefinition_count();
        // Same code and pool; the statement moved to line 20.
        redefine_probe(&shared, cid, probe_class(70_000, 20, 0));
        let compiled_after = cratonvm_classloading::class_redefinition_count();
        let stored = |cp_stamp: Option<u64>| {
            let throwable = shared.mem.heap.alloc_object(ClassId::new(7), 1);
            let frames = vec![BacktraceFrame::Compiled {
                label: Arc::from("obsolete/VmProbe.value:()I"),
                owner_class_id: cid.as_u32(),
                bci: 0,
                cp_stamp,
            }];
            // Captured now, after the redefinition.
            shared.store_throwable_backtrace(throwable, frames.into_boxed_slice());
            let whole = shared
                .throwable_stack_trace_for(throwable)
                .expect("the backtrace is stored");
            let one = shared
                .throwable_stack_trace_entry_for(throwable, 0)
                .expect("the entry is stored");
            assert_eq!(whole[0].line_number, one.line_number, "both readers agree");
            one.line_number
        };
        assert_eq!(stored(Some(compiled_before)), 10, "the body it was compiled from");
        assert_eq!(stored(None), 20, "no stamp: the class as it is");
        assert_eq!(stored(Some(compiled_after)), 20, "compiled from the new body");
    }

    /// Interpreter round i1 wave 23, lane L3 (the i19-L3 atomicity page's
    /// "found but not pinned down" item, and its window 3): a frame stamped
    /// while its class's redefinition was under way -- a lock-free door built
    /// it between the swap's two advances -- is translated only when its code
    /// is not the class's current body; and a frame of the OLD body stamped
    /// after the swap (pushed from an invoke-cache entry validated before it)
    /// is translated when its thread next looks. Here the new body (`nop; ldc
    /// #8; ireturn`) differs from the old (`ldc #8; ireturn`): a frame of the
    /// NEW body with an in-swap stamp used to be translated through the old
    /// pool's map, its `ldc #8` read as the OLD `#8`, and it returned 70000;
    /// the old-body frame stamped after the swap was left alone and returned
    /// 80000.
    #[test]
    fn a_frame_built_during_the_swap_is_translated_only_from_the_old_body() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let (Some(mut old_frame), Some(mut late_frame)) =
            (value_frame(&shared, cid), value_frame(&shared, cid))
        else {
            return;
        };
        // Created before the redefinition: each looks at it when converting.
        let mut current = JvmThread::new(ThreadId(4_127), "obsolete-frames-window-new");
        let mut stale = JvmThread::new(ThreadId(4_128), "obsolete-frames-window-old");
        let mut late = JvmThread::new(ThreadId(4_131), "obsolete-frames-window-late");
        redefine_probe(
            &shared,
            cid,
            ldc_class_with_code(80_000, &[0x00, 0x12, 8, 0xac]),
        );
        let Some(mut new_frame) = value_frame(&shared, cid) else {
            return;
        };
        let (began, latest) = {
            let cm = shared.classes.class_manager.read();
            let Some(history) = cm.redefinition_history(cid) else {
                unreachable!("the class was redefined");
            };
            (
                history.latest_redefinition_began(),
                history.latest_redefinition(),
            )
        };
        assert!(began < latest, "the swap spans two advances");
        // As a door that held no class-manager lock would have stamped them.
        old_frame.restamp_redefined_body(began, false, None);
        new_frame.restamp_redefined_body(began, false, None);
        let new_code = Arc::clone(&new_frame.code);

        current.frames.push(new_frame);
        convert_obsolete_frames_if_redefined(&shared, &mut current);
        assert!(!current.frames[0].runs_obsolete_method());
        assert!(
            Arc::ptr_eq(&current.frames[0].code, &new_code),
            "the current body keeps its code"
        );
        assert_eq!(current.frames[0].redefine_stamp(), latest, "restamped");
        let fresh = super::super::execute_frame(&shared, &mut current);
        assert!(
            matches!(fresh, Ok(Some(Value::Int(80_000)))),
            "a frame of the new body reads the new pool, got {fresh:?}"
        );

        stale.frames.push(old_frame);
        convert_obsolete_frames_if_redefined(&shared, &mut stale);
        assert!(stale.frames[0].runs_obsolete_method(), "translated");
        let old = super::super::execute_frame(&shared, &mut stale);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "a frame of the old body keeps its constant, got {old:?}"
        );

        // Window 3: the old body, stamped as if pushed after the swap.
        late_frame.restamp_redefined_body(latest, false, None);
        // Its stamp says current, but a back edge must not OSR it into a
        // body compiled from the new bytecode before its thread converts it
        // (wave 24); a frame of the new body may be.
        assert!(frame_runs_replaced_code(&shared, &late_frame), "window 3");
        if let Some(fresh_frame) = value_frame(&shared, cid) {
            assert!(!frame_runs_replaced_code(&shared, &fresh_frame), "the new body");
        }
        late.frames.push(late_frame);
        convert_obsolete_frames_if_redefined(&shared, &mut late);
        assert!(late.frames[0].runs_obsolete_method(), "translated");
        let old = super::super::execute_frame(&shared, &mut late);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "a frame of the old body pushed after the swap keeps its constant, got {old:?}"
        );
    }

    /// Interpreter round i1 wave 23, lane L3 (the i21-L4 page): a frame
    /// moved through many redefinitions that each give it DIFFERENT bytes
    /// (every one appends the previous constant, so the old `ldc` moves one
    /// index up each time) keeps one retired copy per move while its dispatch
    /// loop has not reached its top on it, and frees them once it has
    /// (`ReplacedBody::code_reached_top`, set where the loop refreshes its
    /// fast-path gate). Each move that changes the code allocation also
    /// bumps the stack's `code_moves`, which the loop's gate compares.
    #[test]
    fn a_frame_run_since_its_last_move_frees_the_copies_it_ran_before() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_129), "obsolete-frames-drain");
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(frame);
        for round in 1..=50 {
            redefine_probe(&shared, cid, ldc_class(80_000 + round));
            let moves = thread.frames.code_moves();
            convert_obsolete_frames_if_redefined(&shared, &mut thread);
            assert_ne!(thread.frames.code_moves(), moves, "round {round}: moved");
            let moved = &thread.frames[0];
            assert!(
                retired(moved) <= 1,
                "round {round}: {} copies kept",
                retired(moved)
            );
            // The dispatch loop reaches its top on the moved frame.
            if let Some(replaced) = moved.replaced_body() {
                replaced.note_code_reached_top();
            }
        }
        // Two more moves with no loop top in between: the copies stay.
        redefine_probe(&shared, cid, ldc_class(90_001));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        assert_eq!(retired(&thread.frames[0]), 1);
        redefine_probe(&shared, cid, ldc_class(90_002));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        assert_eq!(
            retired(&thread.frames[0]),
            2,
            "a loop suspended since the last move may still hold the older copy"
        );
        // Wave 24: more moves with no loop top -- a frame suspended in one
        // instruction while nested code keeps moving it -- free each copy no
        // loop ever loaded instead of keeping one per move.
        for round in 3..=12 {
            redefine_probe(&shared, cid, ldc_class(90_000 + round));
            convert_obsolete_frames_if_redefined(&shared, &mut thread);
            assert_eq!(
                retired(&thread.frames[0]),
                UNLOADED_COPIES_KEPT,
                "round {round}: the held copy and one more"
            );
        }

        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the frame keeps its constant, got {old:?}"
        );
    }

    /// Interpreter round i1 wave 23, lane L3 (the i22-L3 census proposal and
    /// the i19-L3 untranslatable-frames page, case 2): a thread parked in a
    /// method across 300 redefinitions that each renumber its class's pool
    /// differently -- past both of the history's soft caps -- wakes with its
    /// frame translatable, because the census reports its frame's stamp
    /// (`prune_histories_by_census`) and the history keeps what it needs. Before, the
    /// 257th step dropped the first and the frame read the new pool: 70300.
    /// Once it has moved, the census lets the history go.
    #[test]
    fn a_thread_parked_across_hundreds_of_renumberings_stays_translatable() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let mut parked = JvmThread::new(ThreadId(4_130), "obsolete-frames-parked");
        let registry = &shared.threads.thread_registry;
        registry.register(parked.thread_id, "obsolete-frames-parked", None);
        registry.set_gc_block_state(parked.thread_id, Arc::clone(&parked.gc_block_state));
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        let stamp = frame.redefine_stamp();
        parked.frames.push(frame);
        for round in 1..=300 {
            // As `redefine_class_with` does, under the writer.
            let mut cm = shared.classes.class_manager.write();
            let floor = StaleFrameCensus::take(&shared).floor_of(cid);
            assert!(
                floor.is_some_and(|floor| floor <= stamp),
                "the parked frame holds the floor"
            );
            prune_histories_by_census(&shared, &mut cm);
            cm.redefine_class(cid, ldc_class(70_000 + round), RedefineOptions::default())
                .expect("a same-shape redefinition succeeds");
        }
        let seen = cratonvm_classloading::class_redefinition_count();
        convert_obsolete_frames_if_redefined(&shared, &mut parked);
        assert!(parked.frames[0].runs_obsolete_method(), "translated");
        let floor = parked
            .gc_block_state
            .obsolete_frame_floor
            .load(std::sync::atomic::Ordering::Acquire);
        assert!(floor >= seen, "the woken thread publishes its pass");
        {
            let mut cm = shared.classes.class_manager.write();
            cm.prune_redefinition_histories(|_| Some(floor));
            let Some(history) = cm.redefinition_history(cid) else {
                unreachable!("the class was redefined");
            };
            assert_eq!(
                history.translate(stamp, 8),
                None,
                "nobody runs code that old any more: its steps are gone"
            );
        }
        let old = super::super::execute_frame(&shared, &mut parked);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the parked frame keeps its constant, got {old:?}"
        );
    }

    /// Interpreter round i1 wave 25, lane L3 (the host run of
    /// `tools/probes/interp/L3/L3W24ParkedAcrossManyRenames.java` in the
    /// default mode: `parked=parkQqA,parkQxC`). `retransformClasses`
    /// redefines a class twice here -- back to its retransformation base,
    /// then to the transformers' output (`instrument::native_retransform_classes0`)
    /// -- and both halves move the renamed constant, so 600 retransforms of a
    /// class a thread stays parked in are 1,199 history steps. The census kept
    /// them past the soft caps, but the hard cap was 1,024 steps: the parked
    /// frame's first steps were dropped and it read the new pool at its old
    /// index (80600 here). `--compatible` hid it: its latch waits in 10 ms
    /// slices, and each slice's blocking-region exit moves the frame, which
    /// then never needs more than a few steps; the real JDK's latch parks once.
    #[test]
    fn a_thread_parked_across_six_hundred_retransforms_stays_translatable() {
        use std::sync::atomic::Ordering;
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let original = ldc_class(70_000);
        let Some(cid) = define_probe(&shared, &original) else {
            return;
        };
        let mut parked = JvmThread::new(ThreadId(4_150), "obsolete-frames-parked-600");
        let registry = &shared.threads.thread_registry;
        registry.register(parked.thread_id, "obsolete-frames-parked-600", None);
        registry.set_gc_block_state(parked.thread_id, Arc::clone(&parked.gc_block_state));
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        let stamp = frame.redefine_stamp();
        parked.frames.push(frame);
        // Blocked in the frame, as its park's blocking deposit publishes it.
        publish_blocked_frame_classes(&parked);
        parked
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        for round in 1..=600 {
            // As `native_retransform_classes0` does: the base, then the output.
            for bytes in [original.clone(), ldc_class(80_000 + round)] {
                let mut cm = shared.classes.class_manager.write();
                prune_histories_by_census(&shared, &mut cm);
                cm.redefine_class(cid, bytes, RedefineOptions::default())
                    .expect("a same-shape redefinition succeeds");
            }
        }
        {
            let cm = shared.classes.class_manager.read();
            let Some(history) = cm.redefinition_history(cid) else {
                unreachable!("the class was redefined");
            };
            assert!(
                history.translate(stamp, 8).is_some(),
                "every step the parked frame needs is kept"
            );
        }
        // It wakes: the flag goes down, then its frames move.
        parked
            .gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
        convert_obsolete_frames_if_redefined(&shared, &mut parked);
        let old = super::super::execute_frame(&shared, &mut parked);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the parked frame keeps its constant, got {old:?}"
        );
    }

    /// Interpreter round i1 wave 25, lane L3: a frame moved onto a private
    /// copy of a replaced body decodes it into a stream kept with that body
    /// (`ReplacedBody::decoded_stream`), not in the process-wide intern table,
    /// whose entry holds the code it decoded -- which kept every such copy
    /// allocated until its shard was cleared, long after the frame moved on
    /// or was popped. And a frame of a class never redefined answers
    /// `frame_runs_replaced_code` from the thread's memo once it has asked.
    #[test]
    fn a_moved_frame_decodes_its_code_outside_the_intern_table() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_152), "obsolete-frames-stream");
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(frame);
        redefine_probe(&shared, cid, ldc_class(80_000));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        let moved = &thread.frames[0];
        assert!(moved.runs_obsolete_method(), "moved onto a private copy");
        let Some(stream) = super::super::quickened_for_frame(moved) else {
            unreachable!("ldc; ireturn decodes");
        };
        assert!(Arc::ptr_eq(stream.code(), &moved.code), "the frame's own code");
        assert!(
            super::super::quickened_for_frame(moved).is_some_and(|again| Arc::ptr_eq(&again, &stream)),
            "built once, kept with the body"
        );
        assert!(
            !cratonvm_reader::quickened::intern(&moved.code)
                .is_some_and(|interned| Arc::ptr_eq(&interned, &stream)),
            "not the intern table's entry"
        );
        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the moved frame keeps its constant, got {old:?}"
        );

        // A class with no history: asked once under the lock, then memoised.
        let never = ClassId::new(0x00F1_0C77);
        let other = Frame::new(
            never,
            "obsolete/Never".to_string(),
            "run".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            Vec::new(),
            1,
            1,
            &[],
        );
        assert!(!frame_runs_replaced_code(&shared, &other));
        let memoised = NO_HISTORY_SEEN.with(|ways| {
            ways.iter()
                .any(|way| way.get().0 == shared.vm_identity && way.get().1 == never.as_u32())
        });
        assert!(memoised, "the no-history answer is memoised for this thread");
        assert!(!frame_runs_replaced_code(&shared, &other));
    }

    /// Interpreter round i1 wave 46, lane L3: the fenced-loop debug lines'
    /// holder answer follows this thread's class-manager guard whenever
    /// lock-order tracking records it (a debug test build), and is `None`
    /// otherwise -- never the wrong way round.
    #[test]
    fn the_class_manager_holder_answer_follows_this_threads_guard() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        {
            let _reader = shared.classes.class_manager.read();
            assert_ne!(class_manager_held_here(), Some(false), "held while the guard lives");
        }
        assert_ne!(class_manager_held_here(), Some(true), "released with the guard");
    }

    /// Interpreter round i1 wave 24, lane L3 (the orchestrator's host runs of
    /// `L3ObsoleteParkedAcrossToggles` / `...Renames` /
    /// `...VirtualThreadResume`: a rare `parkQqA,parkQqB`): a thread whose
    /// conversion finds the class-manager lock busy -- another thread defining
    /// or redefining a class as it wakes -- used to give up until its next
    /// safepoint or blocking-region exit, and its frame ran its old `ldc`
    /// index against the new pool meanwhile: 80000 here. The conversion is
    /// now retried at the dispatch loop's next top, before any bytecode runs.
    #[test]
    fn a_conversion_the_busy_class_manager_deferred_runs_before_the_next_bytecode() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let mut thread = JvmThread::new(ThreadId(4_141), "obsolete-frames-deferred");
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(frame);
        redefine_probe(&shared, cid, ldc_class(80_000));
        {
            // Another thread's class definition, as this one leaves its
            // blocking region (`check_post_block_gc`).
            let _writer = shared.classes.class_manager.write();
            convert_obsolete_frames_if_redefined(&shared, &mut thread);
        }
        assert!(thread.frames.conversion_pending(), "deferred");
        assert!(
            !thread.frames[0].runs_obsolete_method(),
            "not moved while the lock was busy"
        );
        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the frame keeps its constant, got {old:?}"
        );
        assert!(!thread.frames.conversion_pending(), "retried and done");

        // A thawed continuation's frames, the same way
        // (`interpreter::resume_continuation`).
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        let frozen = frame.to_frozen_frame();
        redefine_probe(&shared, cid, ldc_class(90_000));
        let mut resumed = JvmThread::new(ThreadId(4_142), "obsolete-frames-deferred-thaw");
        resumed.frames.push(Frame::from_frozen_frame(frozen));
        {
            let _writer = shared.classes.class_manager.write();
            convert_thawed_frames(&shared, &mut resumed);
        }
        assert!(resumed.frames.conversion_pending(), "deferred");
        // Straight into the dispatch loop, as `resume_continuation` does after
        // its own `convert_thawed_frames`.
        let thawed = super::super::execute_frame(&shared, &mut resumed);
        assert!(
            matches!(thawed, Ok(Some(Value::Int(80_000)))),
            "the thawed frame keeps its constant, got {thawed:?}"
        );
    }

    /// Interpreter round i1 wave 38, lane L3 (the wave-37 regression of
    /// `L6/RedefineRunningSpliceProbe`): a thread NONE of whose frames runs
    /// the redefined class, whose conversion was deferred because the
    /// redefining thread still held the writer (a withdrawn body's forced
    /// poll takes it right then), clears the pending mark at its first retry
    /// once the lock is free. Wave 37 retried through `convert_at_loop_top`,
    /// whose "no frame of a redefined class" shortcut left the mark set, and
    /// the loop ran no bytecode for the whole retry bound.
    #[test]
    fn a_deferred_conversion_that_owes_nothing_clears_at_the_next_top() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let unrelated = Frame::new(
            ClassId::new(0x00F1_0C79),
            "obsolete/Unrelated".to_string(),
            "spin".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            Vec::new(),
            1,
            1,
            &[],
        );
        let mut thread = JvmThread::new(ThreadId(4_160), "obsolete-frames-owes-nothing");
        thread.frames.push(unrelated);
        redefine_probe(&shared, cid, ldc_class(80_000));
        {
            // The redefining thread's writer, still held when this one polls.
            let _writer = shared.classes.class_manager.write();
            convert_obsolete_frames_if_redefined(&shared, &mut thread);
        }
        assert!(thread.frames.conversion_pending(), "deferred while the writer held the lock");
        // The dispatch loop's next top, the lock free and no fence up.
        retry_deferred_conversion(&shared, &mut thread);
        assert!(
            !thread.frames.conversion_pending(),
            "one retry once the lock is free, not the whole retry bound"
        );
        assert_eq!(thread.frames.conversion_retries(), 0);
    }

    /// Interpreter round i1 wave 24, lane L3 (the i22-L3 census proposal's
    /// wave-23 note): a thread blocked since before every redefinition --
    /// an idle pool worker, a JDK service thread -- used to hold the ONE
    /// census floor back for every class, so no history was ever trimmed
    /// below its soft caps. Blocked with an exact list of its frames' classes
    /// that does not name the class, it no longer counts for it; blocked in a
    /// frame of the class, or under compiled code (an inexact list), or
    /// running, it still does.
    #[test]
    fn a_thread_blocked_in_other_classes_does_not_hold_a_classs_history() {
        use std::sync::atomic::Ordering;
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let mut idle = JvmThread::new(ThreadId(4_140), "obsolete-frames-idle");
        let registry = &shared.threads.thread_registry;
        registry.register(idle.thread_id, "obsolete-frames-idle", None);
        registry.set_gc_block_state(idle.thread_id, Arc::clone(&idle.gc_block_state));
        let idle_floor = idle.gc_block_state.obsolete_frame_floor.load(Ordering::Acquire);
        // Its blocking deposit, as `deposit_root_snapshot` makes it: no frame
        // of the probe class.
        publish_blocked_frame_classes(&idle);
        idle.gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        let stamp = cratonvm_classloading::class_redefinition_count();
        for round in 1..=20 {
            let mut cm = shared.classes.class_manager.write();
            prune_histories_by_census(&shared, &mut cm);
            cm.redefine_class(cid, ldc_class(70_000 + round), RedefineOptions::default())
                .expect("a same-shape redefinition succeeds");
        }
        {
            let mut cm = shared.classes.class_manager.write();
            let census = StaleFrameCensus::take(&shared);
            assert!(
                census.floor_of(cid).is_some_and(|floor| floor > idle_floor),
                "the idle thread runs no frame of the class"
            );
            prune_histories_by_census(&shared, &mut cm);
            let Some(history) = cm.redefinition_history(cid) else {
                unreachable!("the class was redefined");
            };
            assert_eq!(
                history.translate(stamp, 8),
                None,
                "nobody runs code that old: past the last eight, its steps are gone"
            );
        }
        // Blocked in a frame of the class, it holds the class's floor.
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        idle.frames.push(frame);
        publish_blocked_frame_classes(&idle);
        assert_eq!(
            StaleFrameCensus::take(&shared).floor_of(cid),
            Some(idle_floor),
            "listed"
        );
        // An inexact list (the registry's native-block marker) counts for
        // every class, and so does a running thread.
        let _ = idle.frames.pop();
        publish_blocked_frame_classes(&idle);
        idle.gc_block_state.blocked_frame_classes.lock().exact = false;
        assert_eq!(StaleFrameCensus::take(&shared).floor_of(cid), Some(idle_floor));
        publish_blocked_frame_classes(&idle);
        idle.gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
        assert_eq!(StaleFrameCensus::take(&shared).floor_of(cid), Some(idle_floor));
    }

    /// `value()`'s current body as a door's template, as a publish site
    /// hands it to `deopt_resume::stamp_compiled_source_of`.
    fn value_template(
        shared: &SharedVm,
        cid: ClassId,
    ) -> Option<Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        let method = class.methods.iter().find(|m| &*m.name == "value")?;
        let code = method.code()?;
        Some(Arc::new(
            crate::classloading::resolution::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: cid,
                    class_name: Arc::clone(&class.name),
                    method_name: Arc::from("value"),
                    method_descriptor: Arc::from("()I"),
                    source_file: None,
                    code: crate::runtime::frame::padded_bytecode(&code.code[..]),
                    exception_table: Arc::from(Vec::<ExceptionTableEntry>::new()),
                    max_stack: code.max_stack,
                    max_locals: code.max_locals,
                    num_params: 0,
                    is_synchronized: false,
                    is_static: true,
                },
            ),
        ))
    }

    fn body_built_now() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("a code buffer");
        buf.emit(&[0xC3]); // ret
        cratonvm_jit::CompiledMethod::new(buf)
    }

    /// Round 13 wave 3 (lane replay2; proposal R13-1): a trap out of a body
    /// compiled before its class's redefinition resumes in the bytecode the
    /// body was compiled from (its publish site's template), which the
    /// class's history can move onto the new pool, and never in the door's
    /// NEW template. Nothing to translate before any redefinition; a body
    /// with no stamped source, or stamped with the door's own template,
    /// keeps the door's template.
    #[test]
    fn a_body_of_a_redefined_class_resumes_in_its_own_source() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let cid = {
            let mut cm = shared.classes.class_manager.write();
            match cm.define_class_with_options(
                "obsolete/VmProbe",
                &ldc_class(70_000),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            ) {
                Ok(cid) => cid,
                Err(_) => return,
            }
        };
        let Some(old) = value_template(&shared, cid) else {
            return;
        };
        // Built with the VM's constant-pool stamp source registered: the
        // generation `old` indexes.
        let body = body_built_now();
        let unstamped = body_built_now();
        let Some(stamp) = body.compile_cp_stamp() else {
            return;
        };
        super::super::deopt_resume::stamp_compiled_source_of(&body, Arc::clone(&old));
        assert!(
            !rebuilt_body_translates(&shared, cid, &old.code, &old.exception_table, stamp),
            "a class with no history has nothing to translate from"
        );
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        let Some(new) = value_template(&shared, cid) else {
            return;
        };
        assert!(rebuilt_body_translates(
            &shared,
            cid,
            &old.code,
            &old.exception_table,
            stamp
        ));
        let chosen = super::super::deopt_resume::obsolete_activation_source(&shared, &body, &new);
        assert!(
            chosen.is_some_and(|src| Arc::ptr_eq(&src, &old)),
            "the body's own source, not the door's new template"
        );
        assert!(
            super::super::deopt_resume::obsolete_activation_source(&shared, &unstamped, &new)
                .is_none(),
            "no stamped source: the door's template, as before"
        );
        let own = body_built_now();
        super::super::deopt_resume::stamp_compiled_source_of(&own, Arc::clone(&new));
        assert!(
            super::super::deopt_resume::obsolete_activation_source(&shared, &own, &new).is_none(),
            "stamped with the door's own template: nothing changes"
        );
    }

    /// Interpreter round i1 wave 37, lane L3 (window 1 of
    /// `interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001`): a
    /// redefinition that gives an old index another constant raises a fence
    /// that holds every other thread's dispatch loop at its top -- the loop's
    /// slow path defers the thread's conversion instead of running a bytecode
    /// -- and never its owner's. Once the fence is down the held loop moves
    /// its frame before its next bytecode, and the frame reads its own
    /// constant. An unchanged pool raises none.
    #[test]
    fn a_redefinition_fence_holds_other_loops_until_it_is_down() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        assert!(
            redefinition_moves_constants(&shared, cid, &ldc_class(80_000)),
            "#8 names 80000 in the new pool, 70000 in the old one"
        );
        assert!(
            !redefinition_moves_constants(&shared, cid, &ldc_class(70_000)),
            "the same pool moves nothing"
        );
        let barrier = &shared.mem.gc_barrier;
        let owner = ThreadId(4_151);
        let quiet = RedefinitionFence::raise(&shared, owner, cid, &ldc_class(70_000));
        assert!(
            !barrier.redefinition_fence_holds(ThreadId(4_150)),
            "no fence for a pool that moves nothing"
        );
        drop(quiet);

        let mut thread = JvmThread::new(ThreadId(4_150), "obsolete-frames-fenced");
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(frame);
        let waits = barrier.redefinition_fence_waits();
        let fence = RedefinitionFence::raise(&shared, owner, cid, &ldc_class(80_000));
        assert!(barrier.redefinition_fence_holds(thread.thread_id));
        assert!(!barrier.redefinition_fence_holds(owner), "never its owner");
        redefine_probe(&shared, cid, ldc_class(80_000));
        // The held loop's top, as `interpreter::loop_top_poll` calls it.
        convert_at_loop_top(&shared, &mut thread);
        assert!(thread.frames.conversion_pending(), "held at its top");
        assert!(barrier.redefinition_fence_waits() > waits, "counted");
        assert!(
            !thread.frames[0].runs_obsolete_method(),
            "not moved while the fence is up"
        );
        // Its next top, still fenced: still pending, nothing run.
        retry_deferred_conversion(&shared, &mut thread);
        assert!(thread.frames.conversion_pending());
        drop(fence);
        assert!(!barrier.redefinition_fence_holds(thread.thread_id));
        retry_deferred_conversion(&shared, &mut thread);
        assert!(!thread.frames.conversion_pending(), "converted once the fence is down");
        assert!(thread.frames[0].runs_obsolete_method(), "moved onto its own body");
        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the held frame keeps its constant, got {old:?}"
        );
    }

    /// Interpreter round i1 wave 37, lane L3
    /// (`L3/L3W37RedefineTwoClassesAtOnce`): a call of two classes holds the
    /// other threads from before its first install to after its last; the
    /// fence of each class inside it adds nothing and takes it down with
    /// nothing; a call of one class raises no call-wide fence.
    #[test]
    fn a_call_wide_fence_spans_its_classes() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let barrier = &shared.mem.gc_barrier;
        let owner = ThreadId(4_154);
        let other = ThreadId(4_155);
        let moving = ldc_class(80_000);
        let same = ldc_class(70_000);
        assert!(
            !raise_install_fence(&shared, owner, &[(cid, &moving[..])]),
            "one class: its own fence"
        );
        assert!(!barrier.redefinition_fence_holds(other));
        // Wave 38: a call none of whose classes moves a constant is still one
        // operation (the fence goes up), with no handshake.
        assert!(raise_install_fence(&shared, owner, &[(cid, &same[..]), (cid, &same[..])]));
        assert!(barrier.redefinition_fence_holds(other));
        lower_install_fence(&shared, owner);
        assert!(!barrier.redefinition_fence_holds(other));
        assert!(raise_install_fence(&shared, owner, &[(cid, &same[..]), (cid, &moving[..])]));
        assert!(barrier.redefinition_fence_holds(other));
        assert!(barrier.redefinition_fence_owned_by(owner));
        {
            // A class of the call that moves constants: held already.
            let class_fence = RedefinitionFence::raise(&shared, owner, cid, &ldc_class(80_000));
            drop(class_fence);
            assert!(
                barrier.redefinition_fence_holds(other),
                "the class's fence did not take the call's down"
            );
        }
        lower_install_fence(&shared, owner);
        assert!(!barrier.redefinition_fence_holds(other));
        assert!(!barrier.redefinition_fence_owned_by(owner));
    }

    /// Interpreter round i1 wave 37, lane L3 (the i19-L3 compiled-bodies
    /// page's "inlined chain" item): a sink resuming an inlined chain out of a
    /// body compiled before its class's redefinition pushes the OUTERMOST
    /// frame from the old code beneath the chain's current inner frames.
    /// Restamped at its index with the compile stamp, it is moved onto its
    /// translated body and reads its own constant; the frame above it, current,
    /// is left alone.
    #[test]
    fn the_outermost_frame_of_a_rebuilt_chain_reads_its_own_constants() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(4_153), "obsolete-frames-chain");
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let compile_stamp = cratonvm_classloading::class_redefinition_count();
        let Some(compiled_from) = value_frame(&shared, cid) else {
            return;
        };
        redefine_probe(&shared, cid, ldc_class(80_000));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        // The chain as a sink pushes it: the outermost frame from the old code,
        // stamped now, and an inner frame of current code above it.
        let outermost = Frame::new_from_arcs(
            cid,
            Arc::from("obsolete/VmProbe"),
            Arc::from("value"),
            Arc::from("()I"),
            None,
            Arc::clone(&compiled_from.code),
            Arc::from(Vec::<ExceptionTableEntry>::new()),
            1,
            0,
            &[],
        );
        let Some(inner) = value_frame(&shared, cid) else {
            return;
        };
        let inner_code = Arc::clone(&inner.code);
        thread.frames.push(outermost);
        thread.frames.push(inner);
        stamp_frame_at_rebuilt_from_compiled_code(&shared, &mut thread, 0, Some(compile_stamp));
        assert!(
            thread.frames[0].runs_obsolete_method(),
            "the outermost frame is moved onto its translated body"
        );
        assert!(
            Arc::ptr_eq(&thread.frames[1].code, &inner_code),
            "the current inner frame keeps its code"
        );
        let _ = thread.frames.pop();
        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "the outermost frame keeps its constant, got {old:?}"
        );
    }

    /// Interpreter round i1 wave 37, lane L3 (windows 2 and 4 of the same
    /// page): a running thread's loop, sent to its slow path by the
    /// redefinition (`GcBarrier::note_code_moved_on_every_loop`), moves its
    /// frame there -- it used to wait for its next safepoint or blocking exit,
    /// which a thread returning from a compiled callee frozen through the
    /// handshake, or any thread after a redefinition that took no handshake,
    /// may not reach for a long time.
    #[test]
    fn a_loop_top_moves_a_frame_of_a_redefined_class() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let mut thread = JvmThread::new(ThreadId(4_152), "obsolete-frames-loop-top");
        let Some(frame) = value_frame(&shared, cid) else {
            return;
        };
        thread.frames.push(frame);
        redefine_probe(&shared, cid, ldc_class(80_000));
        convert_at_loop_top(&shared, &mut thread);
        assert!(
            thread.frames[0].runs_obsolete_method(),
            "moved at the loop top"
        );
        let old = super::super::execute_frame(&shared, &mut thread);
        assert!(
            matches!(old, Ok(Some(Value::Int(70_000)))),
            "its own constant, got {old:?}"
        );
    }

    /// A frame of `value()` pushed from a cached-invoke entry built from the
    /// body the class has now (`Frame::new_pooled_cached`), the entry naming
    /// the constant-pool generation it was read at when `name_generation`
    /// (as the invoke-cache fills do since wave 43).
    fn cached_value_frame(shared: &SharedVm, cid: ClassId, name_generation: bool) -> Option<Frame> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        let method = class.methods.iter().find(|m| &*m.name == "value")?;
        let code = method.code()?;
        let cached = crate::classloading::resolution::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: cid,
                class_name: Arc::clone(&class.name),
                method_name: Arc::from("value"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(&code.code[..]),
                exception_table: Arc::from(Vec::<ExceptionTableEntry>::new()),
                max_stack: code.max_stack,
                max_locals: code.max_locals,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        );
        let cached = if name_generation {
            // Read under the guard the code was read under, as the fills do.
            cached.with_pool_generation(cratonvm_classloading::class_redefinition_count())
        } else {
            cached
        };
        drop(cm);
        Some(Frame::new_pooled_cached(
            Arc::new(cached),
            &[],
            &mut Vec::new(),
            &mut Vec::new(),
        ))
    }

    /// Interpreter round i1 wave 43, lane L2
    /// (`i39-L2-a-frame-pushed-inside-a-constant-only-redefinition-reads-the-new-constant`,
    /// through `i40-L3-proposal-frames-carry-the-pool-generation-their-door-validated`):
    /// a constant-only redefinition (`value()` is `ldc #8; ireturn` before
    /// and after, `#8` 70000 then 80000). A frame pushed inside the swap from
    /// an entry read before it carries a stamp inside the window and the
    /// current bytes, which made the conversion take it for the new body
    /// (80000). With the entry's pool generation trusted
    /// (`FRAMES_TRUST_THEIR_BODYS_POOL_GENERATION`) it is translated as the
    /// body its door chose and keeps its constant; an entry that names no
    /// generation is converted as before.
    #[test]
    fn a_frame_pushed_inside_a_constant_only_swap_runs_the_body_its_door_chose() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
            return;
        };
        let (Some(mut named), Some(mut unnamed)) = (
            cached_value_frame(&shared, cid, true),
            cached_value_frame(&shared, cid, false),
        ) else {
            return;
        };
        let mut named_thread = JvmThread::new(ThreadId(4_243), "obsolete-frames-named-generation");
        let mut unnamed_thread = JvmThread::new(ThreadId(4_244), "obsolete-frames-no-generation");
        // The same code; only the constant at #8 changes.
        redefine_probe(&shared, cid, ldc_class(80_000));
        let began = {
            let cm = shared.classes.class_manager.read();
            let Some(history) = cm.redefinition_history(cid) else {
                unreachable!("the class was redefined");
            };
            history.latest_redefinition_began()
        };
        // As a door that held no class-manager lock would have stamped them.
        named.restamp_redefined_body(began, false, None);
        unnamed.restamp_redefined_body(began, false, None);
        // A stamp inside the swap is stale for the OSR door either way.
        assert!(frame_runs_replaced_code(&shared, &named));
        assert!(frame_runs_replaced_code(&shared, &unnamed));

        named_thread.frames.push(named);
        convert_obsolete_frames_if_redefined(&shared, &mut named_thread);
        let named_value = super::super::execute_frame(&shared, &mut named_thread);
        let expected = if FRAMES_TRUST_THEIR_BODYS_POOL_GENERATION {
            70_000
        } else {
            80_000
        };
        assert!(
            matches!(named_value, Ok(Some(Value::Int(v))) if v == expected),
            "the entry's generation decides, got {named_value:?}"
        );

        unnamed_thread.frames.push(unnamed);
        convert_obsolete_frames_if_redefined(&shared, &mut unnamed_thread);
        let unnamed_value = super::super::execute_frame(&shared, &mut unnamed_thread);
        assert!(
            matches!(unnamed_value, Ok(Some(Value::Int(80_000)))),
            "no generation: taken for the new body as before, got {unnamed_value:?}"
        );
    }
}
