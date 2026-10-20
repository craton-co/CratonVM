// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Throw, handler search, and the routes back into and out of compiled code.
//!
//! Two handler searches live here and they are not the same search.
//! `find_exception_handler_impl` walks an interpreted frame's exception
//! table. `find_jit_exception_handler` asks whether a *compiled* frame can
//! resume at a handler — which needs the precise locals the JIT recorded,
//! and refuses when it cannot reconstruct them.
//!
//! `route_jit_signal_exception` and `route_jit_exception_through_method`
//! are the two directions across that boundary: a fault raised inside
//! compiled code that has to become a Java exception, and a Java exception
//! that has to unwind through a compiled frame. `jit_local_athrow_pc`
//! recovers the bci an `athrow` came from, because a compiled frame does
//! not carry one.
//!
//! Not to be confused with `crate::runtime::exceptions`, which builds and
//! throws exception *objects*. This module decides where control goes
//! once one exists.

use super::*;

// ---------------------------------------------------------------------------
// Interpreter-loop exception unwind
// ---------------------------------------------------------------------------

/// The `invoke_pc` an OSR'd frame hands [`unwind_to_handler`] when it has
/// ALREADY decided it cannot catch.
///
/// The unwinder's first act is to search `frames[frame_idx]`'s own exception
/// table at `invoke_pc`. For every other producer that is exactly right —
/// `invoke_pc` is the site that threw. For an OSR bail it is not: the pc
/// available there is `entry_pc`, the BACK-EDGE the compiled body was ENTERED
/// at, which has nothing to do with where the throw happened. When the loop sits
/// inside a `try` (`try { for (..) {..} } catch`) that back-edge IS inside a
/// protected range, so the unwinder found a handler that does not guard the
/// throw site and entered it — on the stale pre-OSR locals, since the OSR'd body
/// advanced its own copies and never wrote them back.
///
/// Measured on `probes/OsrThrowOutsideTryProbe.java`, whose `trip()` throws
/// AFTER the `try` block: HotSpot `caught=0 escaped=1 sink=80000200000`,
/// CratonVM `caught=1 escaped=1 sink=80018203000` — the exception taken by a
/// handler that does not cover it, and an accumulator 18 003 000 too high from
/// the iterations the spurious resume re-ran. Both silent.
///
/// `route_osr_exception_out_of_artifact` has already asked this frame's own
/// table, with the PRECISE throw bci, and answered `Propagate`. So the
/// unwinder must not ask again with a worse pc — it must start at the caller.
/// A pc no `[start_pc, end_pc)` can contain says exactly that in the existing
/// signature: the first search matches nothing, the frame pops, and `exc_pc` is
/// then re-read from the caller's `last_instr_pc` as usual.
pub(super) const OSR_FRAME_DECLINED_TO_CATCH: usize = usize::MAX;

/// Search for a handler for `exc`, popping frames until one is found or the
/// exception escapes `initial_frame_idx`.
///
/// Returns `Ok(())` once a handler is installed — the caller resumes at the
/// handler pc. Returns `Err` when the exception escaped the method this
/// `execute()` invocation owns, or when installing the handler failed.
/// `frame_idx` is updated in place as frames are popped.
///
/// ## Why this is a function (ARCH-2026-08-04 A4a)
///
/// The interpreter's dispatch loop carried **two byte-identical copies** of
/// this walk — one draining `pending_java_exception`, one draining
/// `pending_runtime_error` after `throw_runtime_error` produced a throwable.
/// Both sat in the loop *prologue*, ahead of the opcode fetch, so their ~40
/// lines each were in the hot loop's instruction footprint on every bytecode
/// even though they run only when an exception is in flight.
///
/// Duplication was also the more expensive problem. The pin below is subtle and
/// load-bearing, and a fix applied to one copy and not the other is a
/// use-after-free that only reproduces on one of the two throw paths.
///
/// ## The pin is not optional
///
/// This loop can pop MANY frames while searching (unwinding out of the method
/// entirely if no handler exists), and [`search_frame_for_unwind`] resolves an
/// unresolved catch type through `resolve_class_loader_aware` — the same
/// loader-faithful path `checkcast` takes — which can run Java (a user loader's
/// `loadClass`, a `ClassFileLoadHook` transformer) and therefore collect. Once
/// a frame is popped it no longer roots the propagating exception, and nothing
/// else does until a handler is found (which pushes it onto a frame's stack)
/// or the method returns it as an `Err`. So it is pinned in `native_pin_roots`
/// for the whole walk, and re-read from there on every use — a moving collector
/// rewrites the pin slot in place, so a local copy taken before a GC is stale.
/// (Until round i1 wave 2 the resolution was the loader-blind
/// `load_class_concurrent`, which could not collect and the pin was
/// precautionary — see
/// `docs/internal/fixed-bugs/exception-handler-search-callers-hold-raw-oops-across-a-lazy-catch-type-load-FIXED-20260918.md`.
/// It is load-bearing now.)
///
/// ## A catch type that fails to resolve replaces the exception
///
/// JVMS §5.4.3: resolution errors are thrown where the symbolic reference is
/// used, and the handler search is that use. HotSpot
/// (`InterpreterRuntime::exception_handler_for_exception`, bug 4307310) makes
/// the resolution error the in-flight exception and repeats the search in the
/// SAME frame starting at the failing row's `handler_pc`. This loop does the
/// same through [`UnwindSearch::ResolutionFailed`]. The restart count is
/// bounded by the frame's table length so a row whose handler lies inside its
/// own range (never produced by javac) cannot loop forever.
pub(super) fn unwind_to_handler(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: &mut usize,
    initial_frame_idx: usize,
    exc: ObjectRef,
    invoke_pc: usize,
) -> Result<(), MethodCallFailed> {
    // JDWP `Exception` (interpreter round i1 wave 10, lane L1) and JVMTI
    // `Exception` (wave 11): reported at the throw site, before any frame is
    // popped; may park or run an agent callback, so it answers the exception
    // re-read after them. Two loads when unarmed.
    let exc = super::deliver_exception_event_if_armed(shared, thread, *frame_idx, exc, invoke_pc);
    let mut exc_pc = invoke_pc;
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    // Resolution-error restarts taken in the current frame.
    let mut restarts = 0usize;
    // JIT round 21 wave 10, lane finalize: the finalizable receiver of an
    // unwound constructor frame that provably threw before any `<init>` call
    // on it completed (see `unconstructed_mark_of`), while the walk is still
    // in that object's constructor chain.
    let mut unconstructed: Option<UnconstructedMark> = None;
    loop {
        if let Some(mark) = unconstructed {
            unconstructed = unconstructed_mark_step(shared, thread, *frame_idx, exc_pc, mark);
        }
        match search_frame_for_unwind(shared, thread, *frame_idx, exc_pc, pin_base) {
            UnwindSearch::Found(handler_pc) => {
                // Read through the pin: the search may have run Java to resolve
                // a catch type, and the pin slot is the only copy a moving
                // collector rewrote.
                let exc_ref = thread.native_pin_roots[pin_base];
                thread.frames[*frame_idx].stack.clear();
                let push = thread.frames[*frame_idx]
                    .stack
                    .push(Value::Object(Some(exc_ref)));
                if let Err(e) = push {
                    thread.native_pin_roots.truncate(pin_base);
                    return Err(MethodCallFailed::InternalError(VmError::Runtime(e)));
                }
                thread.frames[*frame_idx].pc = handler_pc;
                fire_jvmti_exception_catch(shared.vm_identity, thread, *frame_idx, handler_pc);
                super::note_exception_caught_if_armed(shared, thread);
                thread.native_pin_roots.truncate(pin_base);
                return Ok(());
            }
            UnwindSearch::ResolutionFailed { handler_pc, error } => {
                // The resolution error is the exception from here on. Stored
                // into the pin before anything else runs: it is the only root.
                thread.native_pin_roots[pin_base] = error;
                restarts += 1;
                if restarts <= thread.frames[*frame_idx].exception_table().len() {
                    exc_pc = handler_pc;
                    continue;
                }
                // Restart budget spent: leave this frame with the new error.
            }
            UnwindSearch::NotFound => {}
        }
        restarts = 0;
        // The throwable leaves this frame (popped below, or returned to the
        // caller that owns it). JVMS §2.11.10 (wave 23, lane L7): block
        // monitors it still holds are released and the in-flight throwable
        // becomes a new `IllegalMonitorStateException`, HotSpot's
        // `InterpreterRuntime::new_illegal_monitor_state_exception`. The pin
        // slot is the only copy, so the replacement is written there.
        if !thread.frames[*frame_idx].held_monitors.is_empty() {
            super::held_monitors::replace_unwound_exception_if_locked(
                shared, thread, *frame_idx, pin_base,
            );
        }
        if unconstructed.is_none() {
            unconstructed = unconstructed_mark_of(shared, thread, *frame_idx, exc_pc);
        }
        if *frame_idx > initial_frame_idx {
            // T17.Δ — exception-unwind.
            pop_and_recycle_frame_with_reason(shared, thread, true);
            *frame_idx -= 1;
            exc_pc = thread.frames[*frame_idx].last_instr_pc;
        } else {
            // The creator is not an interpreted frame of this walk. Since JIT
            // round 21 wave 11 (lane finalize) the registration is withdrawn
            // HERE, whoever the creator is: a compiled method (through a JIT
            // dispatch helper), the compiled `linkToSpecial` link, an outer
            // interpreter entry (a nested `execute()`) or a native door. The
            // wave-10 note, which only a native door ever took, is kept for
            // the one shape that fails closed ([`unconstructed_mark_at_entry`]).
            if let Some(mark) = unconstructed {
                unconstructed_mark_at_entry(shared, thread, *frame_idx, mark);
            }
            let current_exc = thread.native_pin_roots[pin_base];
            thread.native_pin_roots.truncate(pin_base);
            return Err(MethodCallFailed::ExceptionThrown(current_exc));
        }
    }
}

/// `CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT` (JIT round 21 wave 10, lane
/// finalize; default ON, `0` restores the defect; page
/// `docs/internal/fixed-bugs/r21-review-w68-native-findings-FIXED-20261019.md` item 4).
///
/// HotSpot registers a finalizable object when its `Object.<init>` returns
/// (`RegisterFinalizersAtInit`, the `_return_register_finalizer` rewrite), so
/// an object whose constructor throws before `super()` / `this(..)` completes
/// is never finalized. This VM registers earlier -- at the interpreter's and
/// the JIT's `new`, at a constructor `MethodHandle`'s `linkToSpecial`, at
/// JNI `NewObject` and a JNI `<init>` call, at the `NativeContext`
/// constructions -- because every compiled tier and several interpreter
/// doors skip `Object.<init>` as an empty body, and a native standing in for
/// a superclass constructor never reaches it, so a registration AT
/// `Object.<init>` would miss objects that HotSpot finalizes. Instead, the
/// switch WITHDRAWS the early registration when the object provably never
/// reached `Object.<init>`: the interpreter's unwinder proves it from the
/// deepest constructor frame on the object ([`unconstructed_mark_of`]) and
/// withdraws at the creator frame ([`unconstructed_mark_step`]), or hands the
/// object to the native door that allocated it
/// (`SharedVm::note_unconstructed_finalizable`). Since wave 11 the walk
/// withdraws at its entry frame whoever the creator is
/// ([`unconstructed_mark_at_entry`]), and the constructors whose throw it
/// could prove stay interpreted ([`finalizer_bars_compiling`]), so a compiled
/// creator is covered too; what remains is on
/// `docs/internal/fixed-bugs/r21-finalize-compiled-ctor-throw-keeps-registration-FIXED-20261019.md`.
///
/// Read once per VM (`VmConfig::finalizer_register_at_object_init`, since
/// wave 13: review rev-jit w13 item 3, rev-native w13 item 5).
#[inline]
pub(crate) fn finalizer_register_at_object_init_enabled(config: &crate::config::VmConfig) -> bool {
    config.finalizer_register_at_object_init
}

/// An object the unwinder proved unconstructed: its address and the VM
/// address epoch (`HeapRealm::address_epoch`) the address is valid in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UnconstructedMark {
    pub(super) addr: usize,
    pub(super) epoch: u64,
}

/// The mark of `frames[frame_idx]`, about to be popped by the unwinder with
/// the exception at `pc`: `Some` when the frame is a constructor whose
/// receiver (local 0) is a finalizable object and the throw provably
/// preceded every `<init>` call on that receiver in this frame
/// ([`ctor_throw_precedes_initialization`]). Such a frame is the deepest
/// frame of the receiver's constructor chain (before its `this(..)` /
/// `super()` call it can pass the receiver to nothing), so the receiver's
/// `Object.<init>` never returned. A frame that is not an `<init>` costs one
/// short string compare.
#[inline]
fn unconstructed_mark_of(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    pc: usize,
) -> Option<UnconstructedMark> {
    if thread.frames[frame_idx].method_name() != "<init>" {
        return None;
    }
    unconstructed_mark_of_ctor(shared, &thread.frames[frame_idx], pc)
}

#[cold]
#[inline(never)]
fn unconstructed_mark_of_ctor(
    shared: &SharedVm,
    frame: &Frame,
    pc: usize,
) -> Option<UnconstructedMark> {
    let Value::Object(Some(receiver)) = frame.get_local(0) else {
        return None;
    };
    if !super::class_has_finalizer_shared(shared, receiver)
        || !finalizer_register_at_object_init_enabled(&shared.config)
    {
        return None;
    }
    ctor_frame_pc_precedes_initialization(shared, frame, pc).then(|| UnconstructedMark {
        addr: receiver.as_ptr() as usize, // Cast: GC object pointer to address
        epoch: shared.mem.address_epoch(),
    })
}

/// Does `pc` of the constructor `frame` provably precede every `<init>` call
/// that could initialize its receiver ([`ctor_throw_precedes_initialization`]
/// over the body the class store holds)? `false` whenever it cannot decide,
/// including a frame of a body a redefinition replaced.
fn ctor_frame_pc_precedes_initialization(shared: &SharedVm, frame: &Frame, pc: usize) -> bool {
    let cm = shared.classes.class_manager.read_recursive();
    let store = cm.class_store();
    let Some(class) = store.get(frame.class_id) else {
        return false;
    };
    let Some(code) = class
        .find_method("<init>", frame.method_descriptor())
        .and_then(|m| m.code())
    else {
        return false;
    };
    let code = &code.code[..];
    // The frame runs the class's current body (a frame of a body a
    // redefinition replaced indexes the old pool): its code is the
    // class-file bytes plus padding.
    if frame.code.get(..code.len()) != Some(code) {
        return false;
    }
    ctor_throw_precedes_initialization_table(
        code,
        pc,
        frame.exception_table(),
        shared.config.finalizer_ctor_prologue_handlers,
        &|cp_index: u16| ctor_pool_entry_may_initialize(store, class, cp_index),
    )
}

/// `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN` (JIT round 21 wave 16,
/// lane finalize-reinit; default ON, `0` restores the defect; residual 4R of
/// `docs/known-issues/jit/r21-review-w10-findings-20261019.md`). Latched once
/// per VM in `VmConfig::finalizer_reregister_at_init_return` since wave 19,
/// which arms the caller's gate bit and is handed to the reference processor
/// (`ReferenceProcessor::reregister_at_init_return`).
///
/// The interpreted constructor `frames[frame_idx]` returns normally at
/// `return_pc` (a `return`). When its receiver (local 0, which since wave 19
/// must provably still be the entry receiver:
/// [`ctor_return_names_its_initialized_receiver`]) is a finalizable object,
/// that object's `Object.<init>` has returned -- HotSpot's registration point
/// (`RegisterFinalizersAtInit`) -- unless the `return` provably precedes
/// every `invokespecial` (unverified code returning without `super(..)`,
/// where HotSpot registers nothing). Then the
/// reference processor (`ReferenceProcessor::note_finalizable_init_returned`):
///
/// - registers the object again when an earlier `<init>` attempt on it threw
///   before `super(..)` and its registration was withdrawn (case (a): a
///   creator, compiled or interpreted, that catches the throw and retries
///   `<init>` on the same object; the withdrawal at the walk's entry
///   ([`unconstructed_mark_at_entry`]) cannot see a compiled creator's
///   handler). One row: a discovery while an active row exists is a no-op;
/// - marks the active row constructed, so a later `<init>` on the object
///   that throws before `super(..)` (case (b): an unverified class, a JNI
///   re-`<init>`) withdraws nothing.
///
/// An object whose `<init>` never returns is untouched: its withdrawal
/// stands and its kept entry dies with it.
///
/// The constructors whose throw the unwinder proves stay interpreted
/// ([`finalizer_bars_compiling`]), so the retry's deepest constructor frame
/// returns through the interpreter; compiled `<init>` returns are not seen
/// (a later withdrawal of a row whose only `<init>` returns were compiled is
/// the pre-switch answer). Returns whether the object was registered again.
///
/// Call site: the interpreter's `return` arm (`0xb1`, `interpreter.rs`), for
/// the frame about to return, before it is popped or the entry returns,
/// behind the per-entry gate bit `DG_FINALIZER_INIT_RETURN`
/// (`VmConfig::finalizer_register_at_object_init`: without it nothing is ever
/// withdrawn; and since wave 19 `VmConfig::finalizer_reregister_at_init_return`;
/// both latched per VM). Cost past the bit, for a method that is not a
/// constructor: the name's length and first byte, no string compare (a
/// six-byte name starting with `<` is `<init>`: JVMS 4.2.2 allows `<` only in
/// `<init>` and `<clinit>`). For a constructor: the receiver's class
/// has-finalizer bit (the published allocation recipe, lock-free). For a
/// finalizable receiver: one lock-free decode of the frame's own body
/// ([`ctor_return_names_its_initialized_receiver`]; once per method body
/// under the opt-in `CRATONVM_FINALIZER_INIT_RETURN_SCAN_MEMO`, wave 22), then
/// the reference processor's lock.
#[inline]
pub(crate) fn finalizer_init_returned(shared: &SharedVm, frame: &Frame, return_pc: usize) -> bool {
    let name = frame.method_name().as_bytes();
    if name.len() != 6 || name[0] != b'<' {
        return false;
    }
    finalizer_init_returned_ctor(shared, frame, return_pc)
}

#[cold]
#[inline(never)]
fn finalizer_init_returned_ctor(shared: &SharedVm, frame: &Frame, return_pc: usize) -> bool {
    if frame.method_name() != "<init>" {
        return false;
    }
    let Value::Object(Some(receiver)) = frame.get_local(0) else {
        return false;
    };
    if !super::class_has_finalizer_shared(shared, receiver)
        || !finalizer_register_at_object_init_enabled(&shared.config)
    {
        return false;
    }
    if !ctor_return_names_its_initialized_receiver_in(
        frame,
        return_pc,
        shared.config.finalizer_init_return_scan_memo,
    ) {
        return false;
    }
    shared
        .mem
        .ref_processor
        .lock()
        .note_finalizable_init_returned(receiver.as_ptr() as usize) // Cast: GC object pointer to address
}

/// Does local 0 of the constructor `frame`, returning at `return_pc`, still
/// name the receiver the frame was entered with, and may that receiver's
/// `Object.<init>` have returned? JIT round 21 wave 19 (lane
/// finalize-reinit; items 5 and 6 of
/// `docs/known-issues/jit/r21-review-w18-findings-20261019.md`).
///
/// - Item 5: local 0 is an ordinary local. Verified bytecode may `astore_0`
///   another reference (javac never does), and the hook then re-registered
///   or marked an object whose `Object.<init>` never returned (a JNI
///   `AllocObject`'s withdrawn object passed to `new G(obj)` whose
///   constructor ends `aload_1; astore_0; return`). The frame keeps no entry
///   receiver, so a body that can write local 0 (`xstore 0`, `iinc 0`, their
///   `wide` forms, `jsr`/`ret`) is refused: [`ctor_prefix_scan`] refuses
///   exactly those. The unwinder's proof ([`unconstructed_mark_of_ctor`])
///   rests on the same scan and so already refused them.
/// - Item 6: one lock-free pass over the frame's OWN body (its code minus the
///   two padding bytes): no class-manager lock, no `find_method`, no pool.
///   Every `invokespecial` counts as a possible initializing call, so the
///   only "never initialized" answer left is a `return` before any
///   `invokespecial` at all (unverified code; a verified `return` in
///   `<init>` always follows the initializing call). Anything the scan cannot
///   decide is refused: the pre-switch answer.
///
/// JIT round 21 wave 21 (lane finalize-reinit; item 8 of
/// `docs/known-issues/jit/r21-review-w21-findings-20261019.md`): its own scan,
/// [`ctor_body_keeps_receiver_and_calls_before`], not [`ctor_prefix_scan`].
/// That scan also refuses an exception handler or a back-branch before the
/// first `invokespecial` -- conditions of the unwinder's "throw precedes
/// initialization" proof, which this question does not need -- so a JEP 513
/// prologue (`try`/`catch` or a loop before `super(..)`, verified javac code
/// in JDK 25) was refused here, and it allocated its `boundaries` (and a
/// handler-pc `Vec`) on every finalizable constructor return. The withdraw
/// side keeps [`ctor_prefix_scan`] unchanged.
fn ctor_return_names_its_initialized_receiver(frame: &Frame, return_pc: usize) -> bool {
    ctor_return_names_its_initialized_receiver_in(frame, return_pc, false)
}

/// [`ctor_return_names_its_initialized_receiver`], with `memo`
/// (`VmConfig::finalizer_init_return_scan_memo`,
/// `CRATONVM_FINALIZER_INIT_RETURN_SCAN_MEMO`, opt-in; JIT round 21 wave 22,
/// lane finalize; item 8 of `r21-review-w21-findings-20261019.md`, "Not
/// done"): the body's answer is decoded once per method body
/// ([`ctor_body_receiver_facts`]) and kept on the frame's quickened stream
/// ([`ctor_receiver_facts_cell`]), so a finalizable class's constructor
/// returns stop decoding their bodies. The memo is a function of the bytes
/// alone and the cell belongs to the exact bytes the frame runs (a redefined
/// body with other bytes has a new code allocation, `padded_bytecode_for_method`
/// compares them, so a new stream and a new cell). A frame
/// with no such stream (an owned frame, one moved onto a replaced body, a
/// method not yet quickened) decodes, as with the switch off; both arms
/// answer the same.
fn ctor_return_names_its_initialized_receiver_in(frame: &Frame, return_pc: usize, memo: bool) -> bool {
    let body = &frame.code[..frame.code.len().saturating_sub(2)];
    if memo {
        if let Some(stream) = ctor_receiver_facts_cell(frame) {
            let mut facts = stream.ctor_receiver_facts();
            if facts == CTOR_FACTS_UNKNOWN {
                facts = ctor_body_receiver_facts(body);
                stream.set_ctor_receiver_facts(facts);
            }
            return ctor_facts_answer(facts, return_pc);
        }
    }
    ctor_body_keeps_receiver_and_calls_before(body, return_pc)
}

/// The quickened stream whose memo cell may hold `frame`'s body answer: the
/// stream its `CachedBytecodeMethod` already built (never built here, no lock
/// taken), and only when it pins the very allocation the frame runs
/// (`Arc::ptr_eq`), which a frame on a replaced body does not.
fn ctor_receiver_facts_cell(frame: &Frame) -> Option<&cratonvm_reader::QuickenedCode> {
    let stream = frame.cached_method()?.quickened.get()?.as_deref()?;
    std::sync::Arc::ptr_eq(stream.code(), &frame.code).then_some(stream)
}

/// [`ctor_body_receiver_facts`]: not computed yet (the cell's initial value).
const CTOR_FACTS_UNKNOWN: u32 = 0;
/// [`ctor_body_receiver_facts`]: refused (a body that can write local 0, uses
/// a subroutine, or does not decode); every return is answered `false`.
const CTOR_FACTS_REFUSED: u32 = 1;
/// [`ctor_body_receiver_facts`]: no `invokespecial` at all; every return is
/// answered `false`.
const CTOR_FACTS_NO_CALL: u32 = u32::MAX;

/// The return-pc-independent half of
/// [`ctor_body_keeps_receiver_and_calls_before`], as one `u32`:
/// [`CTOR_FACTS_REFUSED`], [`CTOR_FACTS_NO_CALL`], or `2 +` the pc of the
/// body's first `invokespecial` (a body is at most 65535 bytes, so this never
/// meets either marker). [`ctor_facts_answer`] turns it back into the answer
/// for one return pc: an `invokespecial` precedes `return_pc` exactly when the
/// first one does.
fn ctor_body_receiver_facts(code: &[u8]) -> u32 {
    let mut first_call: Option<usize> = None;
    let keeps = ctor_body_scan(code, |pc| {
        first_call.get_or_insert(pc);
    });
    if !keeps {
        return CTOR_FACTS_REFUSED;
    }
    match first_call {
        None => CTOR_FACTS_NO_CALL,
        // Narrowing: a pc of a method body (at most 65535) fits.
        Some(pc) => u32::try_from(pc).map_or(CTOR_FACTS_REFUSED, |pc| pc + 2),
    }
}

/// The answer [`ctor_body_keeps_receiver_and_calls_before`] gives at
/// `return_pc` for a body whose [`ctor_body_receiver_facts`] are `facts`.
fn ctor_facts_answer(facts: u32, return_pc: usize) -> bool {
    match facts {
        CTOR_FACTS_UNKNOWN | CTOR_FACTS_REFUSED | CTOR_FACTS_NO_CALL => false,
        // Widening: u32 -> usize.
        first => ((first - 2) as usize) < return_pc,
    }
}

/// One allocation-free linear decode of a constructor body `code` (unpadded):
/// `true` when every instruction decodes, nothing can write local 0
/// (`istore/lstore/fstore/dstore/astore 0` in all forms, `iinc 0`, their
/// `wide` forms) or use a subroutine (`jsr`, `jsr_w`, `ret`, `wide ret`), and
/// an `invokespecial` lies at a pc before `return_pc`. Branches, switches
/// and exception handlers are not examined: they cannot change what local 0
/// holds. Every `invokespecial` counts as a possible initializing call (no
/// pool is read); a `return` placed before all of them in the code is
/// answered `false`, which is also the answer for one reached only after a
/// later `invokespecial` through a backward jump (fail-closed: the
/// pre-switch answer). Wave 21, item 8 of `r21-review-w21-findings-20261019.md`.
fn ctor_body_keeps_receiver_and_calls_before(code: &[u8], return_pc: usize) -> bool {
    let mut called_before = false;
    let keeps = ctor_body_scan(code, |pc| {
        if pc < return_pc {
            called_before = true;
        }
    });
    keeps && called_before
}

/// The decode behind [`ctor_body_keeps_receiver_and_calls_before`] and
/// [`ctor_body_receiver_facts`] (wave 22 split it out unchanged): `true`
/// when every instruction of `code` decodes and none can write local 0 or use
/// a subroutine; `on_invokespecial` sees the pc of every `invokespecial`, in
/// code order.
fn ctor_body_scan(code: &[u8], mut on_invokespecial: impl FnMut(usize)) -> bool {
    let n = code.len();
    let u16_at = |at: usize| -> Option<u16> {
        Some(u16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?]))
    };
    let i32_at = |at: usize| -> Option<i64> {
        Some(i64::from(i32::from_be_bytes([
            *code.get(at)?,
            *code.get(at + 1)?,
            *code.get(at + 2)?,
            *code.get(at + 3)?,
        ])))
    };
    let mut pc = 0usize;
    while pc < n {
        let op = code[pc];
        let len: usize = match op {
            // xstore_0 (istore_0, lstore_0, fstore_0, dstore_0, astore_0).
            0x3b | 0x3f | 0x43 | 0x47 | 0x4b => return false,
            // jsr, ret, jsr_w.
            0xa8 | 0xa9 | 0xc9 => return false,
            0x00..=0x0f | 0x1a..=0x35 | 0x3c..=0x83 | 0x85..=0x98 | 0xac..=0xb1 => 1,
            0x10 | 0x12 | 0x15..=0x19 | 0xbc => 2,
            0x11 | 0x13 | 0x14 | 0x99..=0xa7 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 | 0xc6
            | 0xc7 => 3,
            // xstore <index>: local 0 refused.
            0x36..=0x3a => match code.get(pc + 1) {
                Some(0) | None => return false,
                Some(_) => 2,
            },
            // iinc <index> <const>: local 0 refused.
            0x84 => match code.get(pc + 1) {
                Some(0) | None => return false,
                Some(_) => 3,
            },
            0xb9 | 0xba | 0xc8 => 5,
            0xbe | 0xbf | 0xc2 | 0xc3 => 1,
            0xc5 => 4,
            0xc4 => match code.get(pc + 1).copied() {
                Some(0x15..=0x19) => 4,
                Some(0x36..=0x3a) => match u16_at(pc + 2) {
                    Some(0) | None => return false,
                    Some(_) => 4,
                },
                Some(0x84) => match u16_at(pc + 2) {
                    Some(0) | None => return false,
                    Some(_) => 6,
                },
                // wide ret, and anything else.
                _ => return false,
            },
            0xaa => {
                let base = pc + 1 + (3 - (pc % 4));
                let (Some(low), Some(high)) = (i32_at(base + 4), i32_at(base + 8)) else {
                    return false;
                };
                // Widening: usize -> i64 (a method body is at most 65535 bytes).
                if high < low || high - low >= n as i64 {
                    return false;
                }
                // Cast: bounded by `n` just above.
                base + 12 + 4 * ((high - low + 1) as usize) - pc
            }
            0xab => {
                let base = pc + 1 + (3 - (pc % 4));
                let Some(npairs) = i32_at(base + 4) else {
                    return false;
                };
                // Widening: usize -> i64, as above.
                if npairs < 0 || npairs >= n as i64 {
                    return false;
                }
                // Cast: bounded by `n` just above.
                base + 8 + 8 * (npairs as usize) - pc
            }
            // Reserved / unknown opcodes.
            _ => return false,
        };
        if op == 0xb7 {
            on_invokespecial(pc);
        }
        pc += len;
    }
    pc == n
}

/// Could the `invokespecial` of `class`'s pool entry `cp_index` initialize a
/// constructor's receiver? Only a `<init>` of the class itself (`this(..)`)
/// or of its direct superclass (`super(..)`) can (JVMS 4.10.1.9). Anything
/// that is not PROVABLY a third class's `<init>` counts as one: an entry the
/// pool does not answer plainly, and a target named like the class or its
/// superclass under any spelling.
///
/// JIT round 21 wave 11 (lane finalize; item 1 of
/// `r21-review-w10-findings-20261019.md`): a hidden class is stored as
/// `p/H/0x<hex>` while its constant pool keeps the pre-mangle `p/H`, so its
/// `this(..)` call was read as a third class's and a `this(); throw ..`
/// constructor was "proven" unconstructed: a wrong withdrawal of an object
/// HotSpot finalizes. The name is now compared hidden-aware
/// ([`ctor_target_may_initialize`]).
fn ctor_pool_entry_may_initialize(
    store: &crate::classloading::ClassStore,
    class: &crate::classloading::Class,
    cp_index: u16,
) -> bool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let cp = &class.constant_pool;
    match cp.get(cp_index) {
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }) => {
            let Some((name, _)) = cp.get_name_and_type(*name_and_type_index) else {
                return true;
            };
            if name != "<init>" {
                return false;
            }
            let Some(target) = cp.get_class_name(*class_index) else {
                return true;
            };
            let superclass = class.superclass.and_then(|id| store.get(id));
            // The store's recorded-name answer too (`CRATONVM_HIDDEN_SELF_RECORDED_NAME`;
            // wave 22, lane hidden-resid, item 3 of `r21-review-w22-findings-20261019.md`):
            // a hidden class whose stored name's suffix does not carry its
            // class-file name still "may initialize" (fail closed).
            (store.recorded_self_name()
                && (store.hidden_self_name(class, target)
                    || superclass.is_some_and(|s| store.hidden_self_name(s, target))))
                || ctor_target_may_initialize(
                    &class.name,
                    class.hidden,
                    superclass.map(|s| (&*s.name, s.hidden)),
                    target,
                )
        }
        _ => true,
    }
}

/// The name half of [`ctor_pool_entry_may_initialize`]: `false` only when
/// `target` is provably neither the class (`class_name`, stored mangled
/// `.../0x<hex>` when `hidden`) nor its superclass. A hidden class's pool
/// names it by its pre-mangle name (everything before the last `/0x`).
pub(super) fn ctor_target_may_initialize(
    class_name: &str,
    hidden: bool,
    superclass: Option<(&str, bool)>,
    target: &str,
) -> bool {
    fn names(stored: &str, hidden: bool, target: &str) -> bool {
        if stored == target {
            return true;
        }
        // The one hidden self-name rule (JIT round 21 wave 21, item 4 of
        // `r21-review-w21-findings-20261019.md`): this copy's `rsplit_once`
        // also read `A/0x1$Inner` as `A`.
        hidden && cratonvm_classloading::hidden_stored_name_is_class_file_name(stored, target)
    }
    names(class_name, hidden, target)
        || superclass.is_some_and(|(name, hidden)| names(name, hidden, target))
}

/// Does `frame`'s exception table cover `pc` (a row whose `[start, end)`
/// holds it, whatever its catch type)?
fn frame_rows_cover(frame: &Frame, pc: usize) -> bool {
    frame
        .exception_table()
        .iter()
        // Widening: u16 -> usize (non-negative, fits)
        .any(|e| pc >= e.start_pc as usize && pc < e.end_pc as usize)
}

/// Could a handler of the creator `frame` still reach the unconstructed
/// object at `addr`? Only through a local: a handler starts with an empty
/// operand stack, and verified code can pass an `uninitialized` reference to
/// nothing but its `<init>`, so no field, array, static or callee frame holds
/// it. A local holding it is the retry shape (review item 4 of
/// `r21-review-w10-findings-20261019.md`, bytecode no compiler emits); javac's
/// `try { new X(..); } catch ..` keeps the reference on the operand stack
/// only (JIT round 21 wave 14, lane finalize: the covered-creator rule failed
/// closed for it, `R21Fz3FinCtorChain1` `holderCatchInner` and
/// `R21Fz3FinHidden1` `self`).
fn frame_locals_hold(frame: &Frame, addr: usize) -> bool {
    (0..frame.locals_len()).any(|i| {
        u16::try_from(i).is_ok_and(|i| {
            matches!(
                frame.get_local(i),
                // Cast: GC object pointer to address
                Value::Object(Some(r)) if r.as_ptr() as usize == addr
            )
        })
    })
}

/// The walk has a mark and reached `frames[frame_idx]`, which the exception
/// leaves at `pc`. A frame of the marked object's constructor chain (an
/// `<init>` whose receiver is the object, at its `this(..)` / `super(..)`
/// call) keeps the mark. The frame that called the chain (at an
/// `invokespecial`, the object's construction) is its CREATOR: the object is
/// garbage now, and its registration is withdrawn (when no collection ran
/// since the mark; `SharedVm::withdraw_unconstructed_finalizable_now`). Any
/// other frame shape drops the mark (the pre-switch answer).
///
/// JIT round 21 wave 11 (lane finalize; item 4 of
/// `r21-review-w10-findings-20261019.md`): fails closed for a creator whose
/// exception table covers its `invokespecial`. Bytecode no compiler emits
/// can keep the `uninitialized` reference in a local, catch the
/// constructor's throwable and run `<init>` on the same object again; when
/// that retry completes HotSpot registers the object, and a withdrawal here
/// would have left nothing to finalize. Such a creator keeps the
/// registration (the pre-switch answer), whatever its handler then does --
/// since wave 14 only when a local of it holds the object
/// ([`frame_locals_hold`]), the one place a handler could retry from.
fn unconstructed_mark_step(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    pc: usize,
    mark: UnconstructedMark,
) -> Option<UnconstructedMark> {
    let frame = &thread.frames[frame_idx];
    if frame.code.get(pc) != Some(&0xb7) {
        return None;
    }
    let in_chain = frame.method_name() == "<init>"
        && matches!(
            frame.get_local(0),
            // Cast: GC object pointer to address
            Value::Object(Some(r)) if r.as_ptr() as usize == mark.addr
        );
    if in_chain {
        return Some(mark);
    }
    // Wave 14: a covered creator fails closed only when a handler could
    // still reach the object ([`frame_locals_hold`]).
    if frame_rows_cover(frame, pc) && frame_locals_hold(frame, mark.addr) {
        return None;
    }
    shared.withdraw_unconstructed_finalizable_now(mark.addr, mark.epoch);
    None
}

/// The walk reached its entry frame (`frames[entry_idx]`, the first frame of
/// this interpreter entry) still carrying `mark`: the entry frame is the
/// object's deepest constructor or a frame of its chain, and the object's
/// creator is outside this walk (JIT round 21 wave 11, lane finalize; page
/// `r21-finalize-compiled-ctor-throw-keeps-registration-20261019.md` items
/// 2-4). The registration is withdrawn now: no compiled creator, JIT
/// dispatch helper, compiled `linkToSpecial` link or native door ever runs
/// `<init>` on an object whose constructor threw.
///
/// Fails closed (item 4 of `r21-review-w10-findings-20261019.md`, as
/// [`unconstructed_mark_step`]) when the interpreted frame below the entry
/// stands at an `invokespecial` its exception table covers -- an outer
/// interpreter entry's creator that could retry `<init>` -- and then only
/// notes the object, the wave-10 answer a native door that allocated it takes
/// (`SharedVm::note_unconstructed_finalizable`). A compiled creator that
/// retries `<init>` in a handler is invisible here; since wave 16 its
/// retry's successful `<init>` return registers the object again
/// ([`finalizer_init_returned`], `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`).
///
/// Wave 13 (lane finalize; item 2 of `r21-review-w13-jit-findings-20261019.md`,
/// page `r21-jni-reinit-entry-withdraw-drops-registration-20261019.md`): it
/// also only notes when the entry is a JNI native's own upcall
/// ([`entry_is_jni_upcall`]). JNI runs a `<init>` methodID on any object, a
/// constructed and registered one included, and HotSpot's registration of
/// that object stays. The JNI door knows which call's registration is its own
/// (`jni_constructor_call_withdrawable`) and takes the note for exactly that
/// one; a note nobody takes changes nothing.
fn unconstructed_mark_at_entry(
    shared: &SharedVm,
    thread: &JvmThread,
    entry_idx: usize,
    mark: UnconstructedMark,
) {
    let below_may_retry = entry_idx > 0 && {
        let below = &thread.frames[entry_idx - 1];
        let at = below.last_instr_pc;
        below.code.get(at) == Some(&0xb7)
            && frame_rows_cover(below, at)
            && frame_locals_hold(below, mark.addr)
    };
    let jit_depth = crate::jit::conservative_roots::current_thread_jit_depth();
    if below_may_retry || entry_is_jni_upcall(thread, entry_idx, jit_depth) {
        shared.note_unconstructed_finalizable(mark.addr, mark.epoch);
    } else {
        shared.withdraw_unconstructed_finalizable_now(mark.addr, mark.epoch);
    }
}

/// Is the interpreter entry whose first frame is `frames[entry_idx]` a call
/// made directly by the innermost JNI native this thread runs (its
/// `jni_native_frames` row, pushed at that interpreter depth with the JIT
/// entry chain at `jit_depth`)? A compiled method the native called in
/// between would have lengthened the chain, and an interpreted one would
/// have taken the index.
pub(super) fn entry_is_jni_upcall(thread: &JvmThread, entry_idx: usize, jit_depth: usize) -> bool {
    thread
        .jni_native_frames
        .last()
        .is_some_and(|&(depth, _, _, row_jit_depth)| {
            depth == entry_idx && u32::try_from(jit_depth).unwrap_or(u32::MAX) == row_jit_depth
        })
}

/// `CRATONVM_FINALIZER_CTOR_STAYS_INTERPRETED` (JIT round 21 wave 11, lane
/// finalize; default ON, `0` lets these constructors compile again; page
/// `docs/internal/fixed-bugs/r21-finalize-compiled-ctor-throw-keeps-registration-FIXED-20261019.md`).
/// Read once per VM (`VmConfig::finalizer_ctor_stays_interpreted`, since wave 13).
#[inline]
pub(crate) fn finalizer_ctor_stays_interpreted_enabled(config: &crate::config::VmConfig) -> bool {
    config.finalizer_ctor_stays_interpreted
}

/// Must no compile build, bind or splice a body of the method `name` +
/// `descriptor` declared by `declaring_class_id`, because it is a constructor
/// of a FINALIZABLE class (or, since wave 13, of an ancestor of a loaded
/// finalizable class) that can throw before its `this(..)` / `super(..)`
/// call (JIT round 21 wave 11, lane finalize; switches
/// `CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT` and
/// `CRATONVM_FINALIZER_CTOR_STAYS_INTERPRETED`)?
///
/// HotSpot never finalizes an object whose `Object.<init>` did not return.
/// This VM registers at the allocation and withdraws only what the
/// interpreter's unwinder proves ([`unconstructed_mark_of`]), which needs an
/// interpreted constructor frame. A compiled or spliced constructor leaves
/// none, and its throwable reaches a compiled caller with no door in between
/// (`R21FinalizeCtorThrows`' `before hot` row). Keeping exactly these
/// constructors interpreted makes every throw before initialization an
/// interpreted one, whichever tier the creator runs in: the creator's JIT
/// dispatch helper enters the interpreter, whose walk withdraws at its entry
/// frame ([`unconstructed_mark_at_entry`]).
///
/// Asked where `breakpoint_bars_compiling` is (the tier-up stride, the by-name
/// compile, the optimizing compile, the direct binds, the inline resolver of
/// both tiers) and by `execute`'s first-call door, which memoizes it both ways
/// (a refusal per class id in `JitRealm::finalizer_bar_refusals` through
/// [`finalizer_bars_compiling_memoized`] since wave 14, an allowed answer in
/// the `jit_gate_pass` memo). `false` after one string compare for every method
/// but a constructor, and after one class read and one empty-set test for a
/// constructor of a class that is neither finalizable nor a recorded
/// ancestor of one (wave 13, review rev-jit w13 item 3b: the switches are
/// `VmConfig` fields, and the ancestor arm costs nothing in a VM that never
/// defined a finalizable class with such an ancestor). A class's
/// `has_finalizer` never changes, so the answer for a finalizable class is
/// stable.
///
/// Since JIT round 21 wave 13 (lane finalize, residual 1 on the page,
/// `CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED`) the same holds for a
/// constructor of a class WITHOUT a finalizer that some LOADED finalizable
/// subclass can reach by `super(..)`: `class F extends B` with a finalizer
/// and `B(String s) { this(Integer.parseInt(s)); }`. Its throw before
/// `Object.<init>` is F's object's too, and the interpreted B frame carries
/// the proof ([`unconstructed_mark_of`] reads the frame's class, the
/// receiver's finalizer). This answer can turn `true` when such a subclass is
/// defined; the define listener (`vm_init.rs`, through
/// [`finalizable_define_stale_ancestors`]) records the ancestor in the class
/// store (`ClassStore::mark_finalizable_ancestor`, the set this bar reads),
/// drops the ancestor's constructors from the `jit_gate_pass` memo, and
/// withdraws the bodies built before
/// (`JitCache::invalidate_for_finalizable_subtype`). No instance of the new
/// class exists before its define.
#[inline]
pub(crate) fn finalizer_bars_compiling(
    shared: &SharedVm,
    declaring_class_id: crate::classloading::ClassId,
    name: &str,
    descriptor: &str,
) -> bool {
    if name != "<init>" {
        return false;
    }
    let cm = shared.classes.class_manager.read_recursive();
    finalizer_bars_compiling_in(&shared.config, cm.class_store(), declaring_class_id, descriptor)
}

/// [`finalizer_bars_compiling`] for `execute`'s gate, with its refusals
/// memoized per declaring class id (`JitRealm::finalizer_bar_refusals`; JIT
/// round 21 wave 14, lane finalize, item 3 of
/// `r21-review-w14-findings-20261019.md`). Wave 13 sealed a refusal into the
/// name-keyed `jit_skip_set`, which also sealed every same-named class of
/// another loader. Without a redefinition a refusal never turns back into an
/// allowed answer, so a recorded one is answered with one read lock and a
/// short scan.
///
/// JIT round 21 wave 18 (lane finalize-reinit; item 2 of
/// `r21-review-w16-findings-20261019.md`, its perf half;
/// `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH`, opt-in, `SharedVm` field
/// `finalizer_bar_refusal_epoch`): a redefinition can replace the throwing
/// constructor with one that cannot throw before its initializing call, and
/// the old row kept it interpreted for the life of the VM. Each row is
/// stamped with the JIT cache's redefine epoch when filled (always, the
/// stamp costs nothing); under the switch a row of another epoch is ignored
/// and the bar asks the current bytes again (as `jit_gate_pass` does). Off,
/// every row answers whatever its stamp (the wave-14 memo). Fail-closed either
/// way: a stale row only keeps a constructor interpreted.
pub(crate) fn finalizer_bars_compiling_memoized(
    shared: &SharedVm,
    declaring_class_id: crate::classloading::ClassId,
    name: &str,
    descriptor: &str,
) -> bool {
    if name != "<init>" {
        return false;
    }
    let refusals = &shared.jit.finalizer_bar_refusals;
    let epoch = shared.jit.jit_cache.redefine_epoch();
    let stamped = shared.finalizer_bar_refusal_epoch;
    // `Some(true)`: a current refusal; `Some(false)`: a row of another epoch
    // (under the switch) that names this descriptor; `None`: no such row.
    let row = refusals.read().get(&declaring_class_id).and_then(|(e, descs)| {
        descs
            .iter()
            .any(|d| &**d == descriptor)
            .then_some(!stamped || *e == epoch)
    });
    if row == Some(true) {
        return true;
    }
    if !finalizer_bars_compiling(shared, declaring_class_id, name, descriptor) {
        // JIT round 21 wave 24 (lane flipprep, flip review item 8): the
        // current bytes allow, so the stale row would only make every later
        // visit ask again. Drop this descriptor from it, and the row once it
        // names none; a row restamped meanwhile (another thread refused at a
        // newer epoch) is left alone. Only under the switch, only off the
        // memoized-refusal path, so default behaviour is unchanged.
        if row == Some(false) {
            let current = shared.jit.jit_cache.redefine_epoch();
            let mut refusals = refusals.write();
            let emptied = match refusals.get_mut(&declaring_class_id) {
                Some((e, descs)) if *e != epoch && *e < current => {
                    descs.retain(|d| &**d != descriptor);
                    descs.is_empty()
                }
                _ => false,
            };
            if emptied {
                refusals.remove(&declaring_class_id);
            }
        }
        return false;
    }
    let mut refusals = refusals.write();
    let (e, descs) = refusals.entry(declaring_class_id).or_insert_with(|| (epoch, Vec::new()));
    if stamped && *e != epoch {
        // Another epoch's refusals: the class may have been redefined since.
        *e = epoch;
        descs.clear();
    }
    if !descs.iter().any(|d| &**d == descriptor) {
        descs.push(std::sync::Arc::from(descriptor));
    }
    true
}

/// Has a finalizable define recorded `class_id` as an ancestor whose
/// constructors the bar may now refuse (`ClassStore::mark_finalizable_ancestor`)?
/// `execute`'s gate reads it right after it filled an allowed `jit_gate_pass`
/// row for a constructor, and drops the row when it reads `true`: the define
/// listener records first and drops the memo rows second, so a row filled
/// after that drop is seen here (JIT round 21 wave 14, lane finalize, item 2
/// of `r21-review-w14-findings-20261019.md`).
pub(crate) fn finalizer_ancestor_recorded(
    shared: &SharedVm,
    class_id: crate::classloading::ClassId,
) -> bool {
    shared
        .classes
        .class_manager
        .read_recursive()
        .class_store()
        .is_finalizable_ancestor(class_id)
}

/// [`finalizer_bars_compiling`] for a caller that already holds the class
/// store (the inline resolver); `name` is `<init>`.
#[inline]
pub(crate) fn finalizer_bars_compiling_in(
    config: &crate::config::VmConfig,
    store: &crate::classloading::ClassStore,
    declaring_class_id: crate::classloading::ClassId,
    descriptor: &str,
) -> bool {
    let Some(class) = store.get(declaring_class_id) else {
        return false;
    };
    // Wave 13: a class without a finalizer of its own can still construct a
    // finalizable object (its constructor reached by `super(..)` from a
    // finalizable subclass). The define of such a subclass records it
    // (`ClassStore::mark_finalizable_ancestor`, only when it declares a
    // constructor that can throw before its initializing call), so every
    // other class answers here.
    let own = class.has_finalizer;
    if !own && !store.is_finalizable_ancestor(declaring_class_id) {
        return false;
    }
    finalizer_bars_compiling_cold(config, store, class, own, descriptor)
}

/// The rest of [`finalizer_bars_compiling_in`], for a finalizable class or a
/// recorded ancestor of one.
#[cold]
#[inline(never)]
fn finalizer_bars_compiling_cold(
    config: &crate::config::VmConfig,
    store: &crate::classloading::ClassStore,
    class: &crate::classloading::Class,
    own: bool,
    descriptor: &str,
) -> bool {
    if !finalizer_register_at_object_init_enabled(config)
        || !finalizer_ctor_stays_interpreted_enabled(config)
        || (!own && !finalizer_ancestor_ctor_stays_interpreted_enabled(config))
    {
        return false;
    }
    class_ctor_may_throw_before_initialization_with(
        store,
        class,
        descriptor,
        config.finalizer_ctor_prologue_handlers,
    )
}

/// `CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED` (JIT round 21 wave 13,
/// lane finalize; default ON, `0` lets the ancestors' constructors compile
/// again; residual 1 of
/// `docs/internal/fixed-bugs/r21-finalize-compiled-ctor-throw-keeps-registration-FIXED-20261019.md`).
/// Read once per VM (`VmConfig::finalizer_ancestor_ctor_stays_interpreted`).
#[inline]
pub(crate) fn finalizer_ancestor_ctor_stays_interpreted_enabled(
    config: &crate::config::VmConfig,
) -> bool {
    config.finalizer_ancestor_ctor_stays_interpreted
}

/// [`class_ctor_may_throw_before_initialization_with`] under the default
/// `CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS` (on); the tests' form.
#[cfg(test)]
fn class_ctor_may_throw_before_initialization(
    store: &crate::classloading::ClassStore,
    class: &crate::classloading::Class,
    descriptor: &str,
) -> bool {
    class_ctor_may_throw_before_initialization_with(store, class, descriptor, true)
}

/// [`ctor_may_throw_before_initialization_table`] over `class`'s constructor
/// `descriptor` as the class store holds it; `false` for a constructor the
/// class does not declare or that has no code. `prologue_handlers` is
/// `VmConfig::finalizer_ctor_prologue_handlers` ([`ctor_handler_rows_allow`]).
fn class_ctor_may_throw_before_initialization_with(
    store: &crate::classloading::ClassStore,
    class: &crate::classloading::Class,
    descriptor: &str,
    prologue_handlers: bool,
) -> bool {
    let Some(code_attr) = class.find_method("<init>", descriptor).and_then(|m| m.code()) else {
        return false;
    };
    ctor_may_throw_before_initialization_table(
        &code_attr.code[..],
        &code_attr.exception_table,
        prologue_handlers,
        &|cp_index: u16| ctor_pool_entry_may_initialize(store, class, cp_index),
    )
}

/// The ancestors whose compiled constructor bodies the define of
/// `new_class_id` makes stale (JIT round 21 wave 13, lane finalize; switches
/// as [`finalizer_bars_compiling`]'s plus
/// `CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED`): when the new class
/// is finalizable, every superclass below the first finalizable one (whose
/// own rule already kept its constructors interpreted, and whose define
/// already settled every class above it) that is not `java/lang/Object` and
/// declares a constructor that can throw before its initializing call. From
/// the define on, [`finalizer_bars_compiling`] refuses such a constructor
/// once the caller has recorded the ancestor
/// (`ClassStore::mark_finalizable_ancestor`); the caller then drops the
/// ancestor's constructors from the `jit_gate_pass` memo and withdraws what
/// was compiled before (`JitCache::invalidate_for_finalizable_subtype`).
/// Empty for a class without a finalizer, which is every define but a rare
/// few, after one class read.
///
/// The stale half of [`finalizable_define_ancestors`] with wave 17's default
/// walk (past a finalizable superclass).
pub(crate) fn finalizable_define_stale_ancestors(
    config: &crate::config::VmConfig,
    store: &crate::classloading::ClassStore,
    new_class_id: crate::classloading::ClassId,
) -> Vec<(crate::classloading::ClassId, String)> {
    finalizable_define_ancestors(config, true, store, new_class_id)
        .into_iter()
        .filter(|a| a.stale)
        .map(|a| (a.id, a.name))
        .collect()
}

/// One ancestor a finalizable define reaches ([`finalizable_define_ancestors`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalizableDefineAncestor {
    pub(crate) id: crate::classloading::ClassId,
    pub(crate) name: String,
    /// It declares a constructor that can throw before its initializing call
    /// NOW: its compiled constructor bodies are stale.
    pub(crate) stale: bool,
}

/// The non-finalizable superclasses (not `java/lang/Object`) that the define
/// of the finalizable class `new_class_id` reaches, each with whether its
/// current constructors make compiled bodies stale. Empty for a class without
/// a finalizer and under the switches [`finalizable_define_stale_ancestors`]
/// names.
///
/// JIT round 21 wave 17 (lane finalize-reinit; item 1 of
/// `docs/known-issues/jit/r21-review-w16-findings-20261019.md`,
/// `CRATONVM_FINALIZER_ANCESTOR_WALK_PAST_FINALIZABLE`, `walk_past_finalizable`):
/// the walk no longer stops at the first finalizable superclass. That class's
/// own define settled the classes above it only if it reached a listener, and
/// a superclass loaded recursively by the class manager
/// (`resolve_supertype`, `load_class(super_name)`) never does. A finalizable
/// ancestor is skipped (its own rule keeps its constructors interpreted) and
/// the walk goes on to `java/lang/Object`; recording is idempotent
/// (`ClassStore::mark_finalizable_ancestor`). `false` is the wave-13 walk.
pub(crate) fn finalizable_define_ancestors(
    config: &crate::config::VmConfig,
    walk_past_finalizable: bool,
    store: &crate::classloading::ClassStore,
    new_class_id: crate::classloading::ClassId,
) -> Vec<FinalizableDefineAncestor> {
    let mut out = Vec::new();
    let Some(class) = store.get(new_class_id) else {
        return out;
    };
    if !class.has_finalizer
        || !finalizer_register_at_object_init_enabled(config)
        || !finalizer_ctor_stays_interpreted_enabled(config)
        || !finalizer_ancestor_ctor_stays_interpreted_enabled(config)
    {
        return out;
    }
    let mut next = class.superclass;
    let mut depth = 0usize;
    while let Some(id) = next {
        depth += 1;
        if depth > 1024 {
            break;
        }
        let Some(ancestor) = store.get(id) else {
            break;
        };
        if ancestor.superclass.is_none() {
            break;
        }
        if ancestor.has_finalizer {
            if !walk_past_finalizable {
                break;
            }
            next = ancestor.superclass;
            continue;
        }
        let stale = ancestor.methods.iter().any(|m| {
            &*m.name == "<init>"
                && class_ctor_may_throw_before_initialization_with(
                    store,
                    ancestor,
                    &m.descriptor,
                    config.finalizer_ctor_prologue_handlers,
                )
        });
        out.push(FinalizableDefineAncestor { id, name: ancestor.name.to_string(), stale });
        next = ancestor.superclass;
    }
    out
}

/// Does a constructor body `code` have an instruction that can throw at a pc
/// where [`ctor_throw_precedes_initialization`] would hold? Exactly the
/// constructors whose throw the interpreter's unwinder can prove (and so
/// withdraw), and no other: a body the proof refuses outright (a back edge
/// over the initializing call, a store to local 0, `jsr`, ...) answers
/// `false`, since running it interpreted would change nothing. An `ldc`, a
/// field access, a call, an allocation, an array access, an integer
/// division, a cast and `athrow` can throw; loads, stores, constants, stack
/// shuffles, the rest of the arithmetic, branches and switches cannot.
///
/// The tests' form: only the handler pcs, the pre-wave-26 handler rule
/// ([`ctor_handler_rows_allow`] with `prologue_handlers == false`).
#[cfg(test)]
pub(super) fn ctor_may_throw_before_initialization(
    code: &[u8],
    handler_pcs: &[u16],
    may_initialize: &dyn Fn(u16) -> bool,
) -> bool {
    ctor_may_throw_before_initialization_table(
        code,
        &ctor_rows_of_handler_pcs(handler_pcs),
        false,
        may_initialize,
    )
}

/// Exception-table rows that carry only `handler_pcs` (whole-body ranges):
/// the tests' forms of the proof, under the handler-pc-only rule.
#[cfg(test)]
fn ctor_rows_of_handler_pcs(
    handler_pcs: &[u16],
) -> Vec<cratonvm_reader::attribute::ExceptionTableEntry> {
    handler_pcs
        .iter()
        .map(|&handler_pc| cratonvm_reader::attribute::ExceptionTableEntry {
            start_pc: 0,
            end_pc: u16::MAX,
            handler_pc,
            catch_type: 0,
        })
        .collect()
}

/// [`ctor_may_throw_before_initialization`] over the constructor's
/// exception-table `rows`; `prologue_handlers` is
/// `VmConfig::finalizer_ctor_prologue_handlers` ([`ctor_handler_rows_allow`]).
pub(super) fn ctor_may_throw_before_initialization_table(
    code: &[u8],
    rows: &[cratonvm_reader::attribute::ExceptionTableEntry],
    prologue_handlers: bool,
    may_initialize: &dyn Fn(u16) -> bool,
) -> bool {
    let Some((first, boundaries)) = ctor_prefix_scan(code, rows, prologue_handlers, may_initialize)
    else {
        return false;
    };
    boundaries
        .iter()
        .any(|&pc| pc < first && opcode_may_throw_before_initialization(code[pc]))
}

/// Can the instruction `op` throw (JVMS 6.5) in a constructor's prefix?
/// `wide` is decoded by [`ctor_prefix_scan`] only over loads, stores and
/// `iinc`, none of which throws. A `return` before the initializing call
/// fails verification, and counts as throwing (conservative).
fn opcode_may_throw_before_initialization(op: u8) -> bool {
    !matches!(
        op,
        0x00..=0x11
            | 0x15..=0x2d
            | 0x36..=0x4e
            | 0x57..=0x6b
            | 0x6e
            | 0x6f
            | 0x72..=0xa7
            | 0xaa
            | 0xab
            | 0xc4
            | 0xc6..=0xc8
    )
}

/// Does the throw at `throw_pc` of a constructor whose class-file body is
/// `code` provably precede every `invokespecial` that could initialize the
/// receiver (`may_initialize(cp index)`)? Then no `this(..)` / `super(..)`
/// call on it ever ran in this frame, so its `Object.<init>` never returned.
///
/// Sound by construction, refusing whenever it cannot decide
/// ([`ctor_prefix_scan`]): `throw_pc` must be an instruction boundary before
/// the first initializing candidate.
///
/// The tests' form: only the handler pcs, the pre-wave-26 handler rule
/// ([`ctor_handler_rows_allow`] with `prologue_handlers == false`).
#[cfg(test)]
pub(super) fn ctor_throw_precedes_initialization(
    code: &[u8],
    throw_pc: usize,
    handler_pcs: &[u16],
    may_initialize: &dyn Fn(u16) -> bool,
) -> bool {
    ctor_throw_precedes_initialization_table(
        code,
        throw_pc,
        &ctor_rows_of_handler_pcs(handler_pcs),
        false,
        may_initialize,
    )
}

/// [`ctor_throw_precedes_initialization`] over the constructor's
/// exception-table `rows`; `prologue_handlers` is
/// `VmConfig::finalizer_ctor_prologue_handlers` ([`ctor_handler_rows_allow`]).
pub(super) fn ctor_throw_precedes_initialization_table(
    code: &[u8],
    throw_pc: usize,
    rows: &[cratonvm_reader::attribute::ExceptionTableEntry],
    prologue_handlers: bool,
    may_initialize: &dyn Fn(u16) -> bool,
) -> bool {
    ctor_prefix_scan(code, rows, prologue_handlers, may_initialize)
        .is_some_and(|(first, boundaries)| throw_pc < first && boundaries.contains(&throw_pc))
}

/// `CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS` (JIT round 21 wave 26, lane
/// finprologue; default ON, `=0` restores the handler-pc-only rule; probe
/// `tools/probes/jit/r21/fuzz/R21Fz8FinPrologue1.java`): may the exception
/// table `rows` of a constructor whose first initializing candidate is at
/// `first` send control that passed `first` back below it?
///
/// Off (the wave-10 rule): every handler must start at or after `first`. A
/// JEP 513 prologue (`try { x = Integer.parseInt(s); } catch (..) { .. }`
/// before `super(..)`, verified javac code in JDK 25) has its handler before
/// `first`, so the whole body was refused: its throw before `super(..)` was
/// never proven, the allocation's registration never withdrawn, and the
/// object was finalized though HotSpot never registers it (its
/// `Object.<init>` never returned).
///
/// On: a handler before `first` is allowed when its row covers only pcs
/// before `first` (`end_pc <= first`; `end_pc` is exclusive, so the
/// initializing `invokespecial` itself is not covered). Such a handler is
/// reached only from a throw before `first`, so control that reaches it never
/// passed `first`. A row whose handler starts at or after `first` sends
/// control forward, as before.
fn ctor_handler_rows_allow(
    rows: &[cratonvm_reader::attribute::ExceptionTableEntry],
    first: usize,
    prologue_handlers: bool,
) -> bool {
    rows.iter().all(|e| {
        usize::from(e.handler_pc) >= first
            || (prologue_handlers && e.start_pc < e.end_pc && usize::from(e.end_pc) <= first)
    })
}

/// The decode both [`ctor_throw_precedes_initialization`] and
/// [`ctor_may_throw_before_initialization`] rest on: `Some((first,
/// boundaries))` -- the pc of the first initializing candidate (`code.len()`
/// when there is none) and every instruction boundary up to and including it
/// -- when the body's prefix is provably straight: every instruction is
/// decoded from pc 0 (an unknown opcode, a truncated operand or a branch out
/// of the body refuses); no branch or switch at or after `first` may target a
/// pc before it, and no exception-table row may send control back below it
/// ([`ctor_handler_rows_allow`] over `rows`, under `prologue_handlers`), so
/// control that passed `first` never comes back below it; and nothing may
/// write local 0 (`xstore 0`, `iinc 0`, their `wide` forms) nor use a
/// subroutine (`jsr` / `ret`), so local 0 is still the receiver the unwinder
/// reads. `None` otherwise.
fn ctor_prefix_scan(
    code: &[u8],
    rows: &[cratonvm_reader::attribute::ExceptionTableEntry],
    prologue_handlers: bool,
    may_initialize: &dyn Fn(u16) -> bool,
) -> Option<(usize, Vec<usize>)> {
    let n = code.len();
    let u16_at = |at: usize| -> Option<u16> { Some(u16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?])) };
    let i32_at = |at: usize| -> Option<i64> {
        Some(i64::from(i32::from_be_bytes([
            *code.get(at)?,
            *code.get(at + 1)?,
            *code.get(at + 2)?,
            *code.get(at + 3)?,
        ])))
    };
    let mut first = n;
    let mut boundaries: Vec<usize> = Vec::new();
    // (branch pc, target pc)
    let mut edges: Vec<(usize, usize)> = Vec::new();
    let mut pc = 0usize;
    while pc < n {
        if first == n {
            boundaries.push(pc);
        }
        let op = code[pc];
        let at = pc;
        // Widening: usize -> i64 (a method body is at most 65535 bytes).
        let n_i64 = n as i64;
        let edge = |off: i64, edges: &mut Vec<(usize, usize)>| -> bool {
            // Widening: usize -> i64, as above.
            let target = at as i64 + off;
            if target < 0 || target >= n_i64 {
                return false;
            }
            // Cast: checked non-negative and below `n` just above.
            edges.push((at, target as usize));
            true
        };
        let len = match op {
            // Stores to local 0 (istore_0, lstore_0, fstore_0, dstore_0, astore_0).
            0x3b | 0x3f | 0x43 | 0x47 | 0x4b => return None,
            0x00..=0x0f | 0x1a..=0x35 | 0x3c..=0x83 | 0x85..=0x98 | 0xac..=0xb1 => 1,
            0x10 | 0x12 => 2,
            0x11 | 0x13 | 0x14 => 3,
            0x15..=0x19 => 2,
            // istore .. astore <index>: refused for index 0.
            0x36..=0x3a => match code.get(pc + 1) {
                Some(0) | None => return None,
                Some(_) => 2,
            },
            // iinc <index> <const>: refused for index 0.
            0x84 => match code.get(pc + 1) {
                Some(0) | None => return None,
                Some(_) => 3,
            },
            0x99..=0xa7 | 0xc6 | 0xc7 => {
                let off = u16_at(pc + 1)?;
                // Cast: the branch offset is a signed 16-bit value.
                if !edge(i64::from(off as i16), &mut edges) {
                    return None;
                }
                3
            }
            0xc8 => {
                let off = i32_at(pc + 1)?;
                if !edge(off, &mut edges) {
                    return None;
                }
                5
            }
            0xaa => {
                let base = pc + 1 + (3 - (pc % 4));
                let (Some(default), Some(low), Some(high)) =
                    (i32_at(base), i32_at(base + 4), i32_at(base + 8))
                else {
                    return None;
                };
                if high < low || high - low >= n as i64 {
                    return None;
                }
                if !edge(default, &mut edges) {
                    return None;
                }
                // Cast: bounded by `n` just above.
                let count = (high - low + 1) as usize;
                for i in 0..count {
                    let off = i32_at(base + 12 + 4 * i)?;
                    if !edge(off, &mut edges) {
                        return None;
                    }
                }
                base + 12 + 4 * count - pc
            }
            0xab => {
                let base = pc + 1 + (3 - (pc % 4));
                let (Some(default), Some(npairs)) = (i32_at(base), i32_at(base + 4)) else {
                    return None;
                };
                if npairs < 0 || npairs >= n as i64 {
                    return None;
                }
                if !edge(default, &mut edges) {
                    return None;
                }
                // Cast: bounded by `n` just above.
                let count = npairs as usize;
                for i in 0..count {
                    let off = i32_at(base + 8 + 8 * i + 4)?;
                    if !edge(off, &mut edges) {
                        return None;
                    }
                }
                base + 8 + 8 * count - pc
            }
            0xb2..=0xb6 | 0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 => 3,
            0xb7 => {
                let cp_index = u16_at(pc + 1)?;
                if first == n && may_initialize(cp_index) {
                    first = pc;
                }
                3
            }
            0xb9 | 0xba => 5,
            0xbc => 2,
            0xbe | 0xbf | 0xc2 | 0xc3 => 1,
            0xc4 => match code.get(pc + 1).copied() {
                Some(0x15..=0x19) => 4,
                Some(0x36..=0x3a) => match u16_at(pc + 2) {
                    Some(0) | None => return None,
                    Some(_) => 4,
                },
                Some(0x84) => match u16_at(pc + 2) {
                    Some(0) | None => return None,
                    Some(_) => 6,
                },
                _ => return None,
            },
            0xc5 => 4,
            // jsr, ret, jsr_w, and every reserved / unknown opcode.
            _ => return None,
        };
        pc += len;
    }
    if pc != n {
        return None;
    }
    if edges.iter().any(|&(from, to)| from >= first && to < first) {
        return None;
    }
    if !ctor_handler_rows_allow(rows, first, prologue_handlers) {
        return None;
    }
    Some((first, boundaries))
}

/// What offering the in-flight exception to one interpreted frame decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnwindSearch {
    /// Resume at this handler pc.
    Found(usize),
    /// No row of this frame catches it.
    NotFound,
    /// Resolving a covering row's catch type threw `error` (JVMS §5.4.3). The
    /// caller makes `error` the in-flight exception and searches the same frame
    /// again at `handler_pc` — see [`unwind_to_handler`].
    ResolutionFailed { handler_pc: usize, error: ObjectRef },
}

/// One typed row's answer.
enum CatchRowVerdict {
    Match,
    NoMatch,
    ResolutionError(ObjectRef),
}

impl CatchRowVerdict {
    #[inline]
    fn of(matched: bool) -> Self {
        if matched {
            Self::Match
        } else {
            Self::NoMatch
        }
    }
}

/// The interpreter unwinder's handler search over `thread.frames[frame_idx]`.
///
/// Same row order and range rules as [`find_exception_handler_impl`] (JVMS
/// §2.10), including its lock-free prefilter: a frame with no covering row, or
/// whose first covering row is a catch-all, is answered from the table alone,
/// with no header read and no class-manager lock. What differs is how a typed
/// row's catch type is resolved — see [`catch_row_verdict`]: through the
/// per-thread resolved-class table and `resolve_class_loader_aware`, and a
/// resolution failure is reported instead of being read as "no match".
///
/// The in-flight throwable is read from `native_pin_roots[pin_slot]`, and only
/// its class id is kept across a resolution (which may collect); a class id
/// does not move.
pub(super) fn search_frame_for_unwind(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    pc: usize,
    pin_slot: usize,
) -> UnwindSearch {
    // Widening: u16 -> usize (non-negative, fits)
    let covers = |start: u16, end: u16| pc >= start as usize && pc < end as usize;
    let first_covering = {
        let table = thread.frames[frame_idx].exception_table();
        let Some(first) = table.iter().position(|e| covers(e.start_pc, e.end_pc)) else {
            return UnwindSearch::NotFound;
        };
        if table[first].catch_type == 0 {
            // Widening: u16 -> usize (non-negative, fits)
            return UnwindSearch::Found(table[first].handler_pc as usize);
        }
        first
    };
    // Owned handles from here: resolving a catch type may run Java, which
    // needs `thread` mutably. One refcount bump, paid only by a frame that has
    // a covering typed row (which already costs a class-manager read).
    let table = thread.frames[frame_idx].exception_table_arc();
    let class_id = thread.frames[frame_idx].class_id;
    let exc_class_id = shared
        .mem
        .heap
        .class_id_of(thread.native_pin_roots[pin_slot]);
    for entry in table[first_covering..].iter() {
        if !covers(entry.start_pc, entry.end_pc) {
            continue;
        }
        // Widening: u16 -> usize (non-negative, fits)
        let handler_pc = entry.handler_pc as usize;
        if entry.catch_type == 0 {
            return UnwindSearch::Found(handler_pc);
        }
        // Every value the unwinder's frames hold is rooted in `thread.frames`,
        // and the throwable in the pin: resolution may collect.
        match catch_row_verdict(
            shared,
            thread,
            class_id,
            entry.catch_type,
            exc_class_id,
            true,
        ) {
            CatchRowVerdict::Match => return UnwindSearch::Found(handler_pc),
            CatchRowVerdict::NoMatch => {}
            CatchRowVerdict::ResolutionError(error) => {
                return UnwindSearch::ResolutionFailed { handler_pc, error };
            }
        }
    }
    UnwindSearch::NotFound
}

/// `CRATONVM_CATCH_ROW_CLASS_ENTRY` (default on; `0` / `false` / `off` / `no`
/// turns it off). Round 15 wave 6 (lane patches6, classsite C5): a catch row
/// answers an isolated URL loader's catch type from the `CONSTANT_Class`
/// entry's recorded resolution and resolves it through the entry. Read on a
/// cold row of a loader-namespaced class and on a resolution miss only.
fn catch_row_class_entry_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_CATCH_ROW_CLASS_ENTRY")
}

/// Does the exception class `exc_class_id` match the catch type named by
/// `class_id`'s constant-pool entry `cp_index`?
///
/// **Resolution.** A catch type is a `CONSTANT_Class` resolution with no
/// initialization — exactly what a `checkcast`/`instanceof`/`anewarray` site
/// records for the same `(class, cp index)` in `thread.cast_sites`
/// ([`super::CastSiteCache`]). So this reads and fills that table: a warm row
/// costs an array index and two epoch loads, plus no lock at all when the
/// entry's positive-receiver memo names this exception class. It fills under
/// the cast arms' own admission rule (no loader namespace, not an array name)
/// and only with an answer `resolve_class_loader_aware` produced, so the table
/// never holds an answer a cast site would not have computed itself. Its epochs
/// and redefinition latch are the invalidation story.
///
/// On a miss the catch type is resolved the way `checkcast` resolves it
/// (`resolve_class_loader_aware`, loader-faithful, may run a user
/// `loadClass`), with two exceptions kept from the pre-wave-2 search:
///
/// * a loader-namespaced referencing class whose catch type its loader already
///   sees is answered on the held lock from `find_class_by_name_for_class`,
///   allocation-free, as before (that table cannot cache it);
/// * a name that is not loaded for this class and that the global path would
///   answer by FABRICATING a synthetic stub is not resolved at all — see
///   `find_exception_handler_impl` for the Keycloak/Quarkus
///   `PreventFurtherStepsException` case — and matches by name only.
///
/// A resolution failure that becomes a Java throwable
/// (`convert_class_not_found`: `NoClassDefFoundError`, or whatever a user
/// loader threw) is returned as [`CatchRowVerdict::ResolutionError`]. One that
/// cannot be materialized keeps the old lenient answer (by-name only).
///
/// **Matching** keeps the loader-blind `is_subclass_of_by_name` fallback after
/// the exact `is_subclass_of` under `--compatible`, as `instanceof`'s fail-open
/// name paths do; see `Class::is_subclass_of_by_name` for why. Under
/// `--jdk-only` a row whose catch type resolved matches exactly (round i1
/// wave 5), as the cast arms already do.
///
/// **`may_collect == false`** is for a caller holding object references no
/// root set names (a compiled route's reconstructed frame): nothing here may
/// run Java or allocate. A warm row is answered as usual; a loaded catch type
/// is answered on the held lock without the loader-faithful fill; an unloaded
/// one takes the pre-wave-2 lenient `load_class_concurrent`, and a failure is
/// read as "no match" (by-name only) because materializing the error would
/// allocate. See [`search_compiled_frame_rows`].
fn catch_row_verdict(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: crate::classloading::ClassId,
    cp_index: u16,
    exc_class_id: crate::classloading::ClassId,
    may_collect: bool,
) -> CatchRowVerdict {
    use super::site_cache::{CastSite, CastSiteCache};
    // The loader-blind name rule for a row whose catch type RESOLVED is
    // `--compatible`-only, as the cast arms' `loader_aware_name_assignable`
    // is: under `--jdk-only` a resolved row matches on `is_subclass_of` alone,
    // HotSpot's answer. A row that did NOT resolve (the fabrication guard, an
    // unmaterializable failure) keeps its by-name answer in both modes.
    //
    // Interpreter round i1 wave 52 (lane L7): asked through the
    // `NamespacedClassTypes` alignment row, the one the cast arms ask
    // (`checkcast_full_path`, `object_is_instance_of_resolved`), where it
    // asked the mode. Every row is on under `--jdk-only`, and since wave 52
    // (lane L4) this one is on by default under `--compatible` too, so a
    // `--compatible` VM matches a resolved catch type by class, as its
    // `instanceof` does, instead of catching another loader's same-named
    // exception class; `CRATONVM_COMPAT_ALIGN=-namespaced-class-types`
    // restores the by-name rule.
    let name_rule = !shared
        .classes
        .aligned(cratonvm_types::HotSpotAlignment::NamespacedClassTypes);
    // Read BEFORE the probe and before resolving; see `SiteCache::put`.
    let epochs =
        super::opcodes::cast_site_cache_enabled(shared).then(|| CastSiteCache::epochs_for(shared));
    if let Some(epochs_before) = epochs {
        // `observed`: the memos below are valid only under the definition
        // epoch they were proved in, and a memo written here is stamped with
        // the epoch read now, before its verdict (i7-L2).
        if let Some(site) = thread
            .cast_sites
            .get(class_id, cp_index)
            .copied()
            .map(|site| site.observed_in(shared))
        {
            // A throwable is never an array, so the memo needs none of the
            // array exclusion `cast_memo_answers` applies to a cast receiver.
            // Either positive slot (interpreter round i2 wave 53, lane L7,
            // `VmConfig::cast_memo_two_positive`): a row that sees two
            // exception classes in turn keeps both.
            if site.admits(exc_class_id) {
                return CatchRowVerdict::Match;
            }
            // The refusal memo: an exception unwinding through a `catch` that
            // does not take it (the common shape: a frame catching a checked
            // exception while a runtime one passes through, once per throw)
            // is answered without the lock or either subtype walk.
            if site.negative_catch == Some(exc_class_id) {
                return CatchRowVerdict::NoMatch;
            }
            // Lock-free once the exception class's supers closure is
            // published (`typecheck::class_is_subtype`).
            if super::typecheck::class_is_subtype(shared, exc_class_id, site.target) {
                thread.cast_sites.put(
                    class_id,
                    cp_index,
                    epochs_before,
                    site.with_positive(exc_class_id, shared.config.cast_memo_two_positive),
                );
                return CatchRowVerdict::Match;
            }
            let by_name = name_rule && {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id)
                    .and_then(|c| c.constant_pool.get_class_name(cp_index))
                    .is_some_and(|name| cm.is_subclass_of_by_name(exc_class_id, name))
            };
            if !by_name {
                // Refused by both tests. (A by-name ADMISSION is not memoised:
                // the positive memo must keep meaning `is_subclass_of` for the
                // cast sites that share the entry.)
                thread.cast_sites.put(
                    class_id,
                    cp_index,
                    epochs_before,
                    CastSite {
                        negative_catch: Some(exc_class_id),
                        ..site
                    },
                );
            }
            return CatchRowVerdict::of(by_name);
        }
    }
    // Asked before the class-manager read below: it takes its own read lock,
    // and `parking_lot` read locks must not nest.
    let fillable_class = epochs.is_some()
        && !super::opcodes::referencing_class_has_loader_namespace(shared, class_id);
    // Round 15 wave 6 (lane patches6, classsite C5;
    // `CRATONVM_CATCH_ROW_CLASS_ENTRY`): a catch type is the same
    // `CONSTANT_Class` entry a `checkcast` / `new` of that index resolves (JVMS
    // §5.4.3), so an isolated URL loader's recorded resolution of the entry
    // answers before the flat store's guess. Both reads take their own lock,
    // so before the class-manager read below. `fill_as_of` precedes the pool
    // read of `name`, as `resolve_class_entry_loader_aware` requires.
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let recorded = if !fillable_class && catch_row_class_entry_enabled() {
        super::constants::recorded_class_entry(shared, class_id, cp_index)
    } else {
        None
    };
    let (name, fill) = {
        let cm = shared.classes.class_manager.read();
        let Some(name) = cm
            .get_class(class_id)
            .and_then(|c| c.constant_pool.get_class_name(cp_index))
        else {
            return CatchRowVerdict::NoMatch;
        };
        let fill = fillable_class && !name.starts_with('[');
        let loaded = recorded.or_else(|| cm.find_class_by_name_for_class(name, class_id));
        if !fill || !may_collect {
            if let Some(id) = loaded {
                return CatchRowVerdict::of(
                    cm.is_subclass_of(exc_class_id, id)
                        || (name_rule && cm.is_subclass_of_by_name(exc_class_id, name)),
                );
            }
        }
        if loaded.is_none() && cm.would_fabricate_synthetic_stub(name) {
            return CatchRowVerdict::of(cm.is_subclass_of_by_name(exc_class_id, name));
        }
        (name.to_string(), fill)
    };
    if !may_collect {
        // The loader-blind lazy load, which (unlike the loader-faithful
        // resolution) runs no Java; a failure cannot be materialized here.
        let loaded = shared.load_class_concurrent(&name).ok();
        let cm = shared.classes.class_manager.read();
        return CatchRowVerdict::of(
            loaded.is_some_and(|id| cm.is_subclass_of(exc_class_id, id))
                || ((name_rule || loaded.is_none())
                    && cm.is_subclass_of_by_name(exc_class_id, &name)),
        );
    }
    // C5: through the entry, so an isolated URL loader's catch row shares the
    // entry's one `loadClass` answer with `checkcast` / `new` / field owners
    // (identical to `resolve_class_loader_aware` for every other class).
    let resolved = if catch_row_class_entry_enabled() {
        super::constants::resolve_class_entry_loader_aware(
            shared,
            thread,
            (class_id, cp_index),
            &name,
            fill_as_of,
        )
    } else {
        super::constants::resolve_class_loader_aware(shared, thread, class_id, &name)
    };
    match resolved {
        Ok(id) => {
            // A catch row resolves without the class-access check, so it must
            // not fill an entry a `checkcast` of the same index would then
            // hit past that check (i9-L2; every mode since i10-L2).
            if let (true, Some(epochs_at_entry)) = (fill, epochs) {
                if super::constants::class_constant_fill_admitted(shared, class_id, &name, id) {
                    thread.cast_sites.put(
                        class_id,
                        cp_index,
                        epochs_at_entry,
                        CastSite::resolved(id),
                    );
                }
            }
            let cm = shared.classes.class_manager.read();
            CatchRowVerdict::of(
                cm.is_subclass_of(exc_class_id, id)
                    || (name_rule && cm.is_subclass_of_by_name(exc_class_id, &name)),
            )
        }
        Err(e) => {
            match crate::runtime::exceptions::convert_class_not_found(shared, thread, &name, e) {
                MethodCallFailed::ExceptionThrown(error) => CatchRowVerdict::ResolutionError(error),
                _ => {
                    let cm = shared.classes.class_manager.read();
                    CatchRowVerdict::of(cm.is_subclass_of_by_name(exc_class_id, &name))
                }
            }
        }
    }
}

/// The link-time half of HotSpot's `ClassVerifier::verify_exception_handler_table`,
/// run under `--jdk-only` only.
///
/// HotSpot checks every typed handler's catch type is assignable to
/// `java/lang/Throwable`, and that check LOADS the catch type through the
/// defining loader of the class being verified: a missing catch type fails
/// linking with its `NoClassDefFoundError` before any method of the class runs,
/// and a loaded non-`Throwable` is a `VerifyError`. CratonVM's Pass 3 runs at
/// define time inside `ClassManager`, which cannot run a Java `loadClass`, and
/// only names the catch type (`bytecode_verifier::catch_type_of`). This is the
/// loading half, called from the link step of `initialize_class_shared` (after
/// Pass 2) for exactly the classes that step verifies. `--compatible` keeps the
/// lazy behaviour: the handler search raises the same error when an exception
/// first reaches the row ([`catch_row_verdict`]).
///
/// Rows are visited in method order, then table order, each distinct name once,
/// so the first failure reported is HotSpot's first failure. A name that is not
/// loaded for the class and that the global path would answer by fabricating a
/// synthetic stub is not loaded, as in [`catch_row_verdict`].
///
/// Nothing but ids and names is held across a resolution, which may run Java
/// and collect.
pub(crate) fn load_catch_types_at_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: crate::classloading::ClassId,
) -> Result<(), MethodCallFailed> {
    let catch_rows = {
        let cm = shared.classes.class_manager.read();
        link_catch_rows(&cm, class_id)
    };
    let Some((class_name, rows)) = catch_rows else {
        return Ok(());
    };
    if rows.is_empty() {
        return Ok(());
    }
    let throwable = shared
        .classes
        .class_manager
        .read()
        .find_bootstrap_class_by_name("java/lang/Throwable");
    for (name, method_name, handler_pc, method_ix, catch_cp) in rows {
        let catch_class =
            match super::constants::resolve_class_loader_aware(shared, thread, class_id, &name) {
                Ok(id) => id,
                Err(e) => {
                    return Err(crate::runtime::exceptions::convert_class_not_found(
                        shared, thread, &name, e,
                    ));
                }
            };
        let is_throwable = match throwable {
            Some(throwable) => shared
                .classes
                .class_manager
                .read()
                .is_subclass_of(catch_class, throwable),
            // No `Throwable` to test against (a stripped VM): loading was the
            // part that could fail.
            None => true,
        };
        if !is_throwable {
            // HotSpot's whole message (interpreter round i5 wave 102, lane
            // L7): the split verifier's header and `Exception Details:`
            // (Location at the handler, Reason naming the constant pool
            // entry), or libverify's below major 50; our `Class.method: `
            // prefix only when the method is gone.
            let message = {
                let cm = shared.classes.class_manager.read();
                catch_type_refusal_text(&cm, class_id, method_ix, handler_pc, catch_cp, &name)
            }
            .unwrap_or_else(|| {
                format!("Catch type is not a subclass of Throwable in exception handler {handler_pc}")
            });
            return Err(crate::runtime::exceptions::throw_linkage_error(
                shared,
                thread,
                crate::error::LinkageError::VerifyError {
                    class_name,
                    method_name,
                    message,
                },
            ));
        }
    }
    Ok(())
}

/// The link-time half of HotSpot's reference assignability checks
/// (`VerificationType::resolve_and_check_assignability`), wherever the
/// `VerifierLoadsAtLink` alignment is on.
///
/// HotSpot's split verifier, checking an assignment between two different
/// reference names, loads the target through the verified class's defining
/// loader (unless it is `java/lang/Object`) and, for a class target, the
/// source too. So that loader initiates both
/// (`Instrumentation.getInitiatedClasses`, a user loader's `loadClass`)
/// although the verified method never runs, and a missing one fails linking
/// with its `NoClassDefFoundError`. CratonVM's Pass 3 runs at define time
/// inside `ClassManager`, which cannot run a Java `loadClass`; it records the
/// names in check order, each method's handler table (a catch type and
/// `Throwable`, `verify_exception_handler_table`) before its code
/// (`ClassManager::verifier_link_loads`), and this loads them, from the link
/// step of `initialize_class_shared`, before [`load_catch_types_at_link`]
/// judges the catch types, for exactly the classes that step verifies. The row is dropped
/// once every name loaded; a failure keeps it for the next link attempt.
///
/// A name that is not loaded for the class and that the global path would
/// answer by fabricating a synthetic stub is not loaded, as in
/// [`load_catch_types_at_link`]. Nothing but ids and names is held across a
/// resolution, which may run Java and collect.
/// (`docs/internal/fixed-bugs/i47-verifier-assignability-checks-initiate-no-loads-FIXED-20261007.md`)
pub(crate) fn load_verifier_types_at_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: crate::classloading::ClassId,
) -> Result<(), MethodCallFailed> {
    use crate::classloading::vtype::VerifierLinkLoad;
    let loads: Vec<VerifierLinkLoad> = {
        let cm = shared.classes.class_manager.read();
        cm.verifier_link_loads(class_id)
            .iter()
            .filter(|load| {
                let name = load.loaded_name();
                cm.find_class_by_name_for_class(name, class_id).is_some()
                    || !cm.would_fabricate_synthetic_stub(name)
            })
            .cloned()
            .collect()
    };
    // What each `Name` entry resolved to, for the conditional entries after
    // it (ids and names only: a resolution may run Java and collect).
    let mut resolved: Vec<(std::sync::Arc<str>, crate::classloading::ClassId)> = Vec::new();
    for load in &loads {
        let name = match load {
            VerifierLinkLoad::Name(name) => name,
            // HotSpot loads the source of an assignability check only for a
            // class target; the target, loaded by an earlier entry, decides
            // (a missing target already failed the link there). A target
            // that is still not loaded for the class (its entry was skipped
            // as a would-be synthetic stub) decides nothing: the source is
            // not loaded either, rather than risk a failure HotSpot would
            // report for the target.
            VerifierLinkLoad::SourceUnlessInterface { target, source } => {
                let target_is_class = {
                    let cm = shared.classes.class_manager.read();
                    resolved
                        .iter()
                        .find(|(n, _)| n == target)
                        .map(|(_, id)| *id)
                        .or_else(|| cm.find_class_by_name_for_class(target, class_id))
                        .and_then(|id| cm.get_class(id))
                        .is_some_and(|c| !c.is_interface())
                };
                if !target_is_class {
                    continue;
                }
                source
            }
        };
        match super::constants::resolve_class_loader_aware(shared, thread, class_id, name) {
            Ok(id) => {
                if matches!(load, VerifierLinkLoad::Name(_)) {
                    resolved.push((std::sync::Arc::clone(name), id));
                }
            }
            Err(e) => {
                return Err(crate::runtime::exceptions::convert_class_not_found_for(
                    shared,
                    thread,
                    Some(class_id),
                    name,
                    e,
                ));
            }
        }
    }
    shared
        .classes
        .class_manager_write()
        .verifier_link_loads_done(class_id);
    Ok(())
}

/// Would [`load_verifier_types_at_link`] succeed for `class_id` without
/// running Java? True when it has nothing to load. A class with names still
/// to load is linked on a Java thread, where its loader can be asked
/// (`vm_util::link_class_without_java` leaves it unlinked).
pub(crate) fn verifier_types_settled_without_java(
    cm: &crate::classloading::ClassManager,
    class_id: crate::classloading::ClassId,
) -> bool {
    cm.verifier_link_loads(class_id).is_empty()
}

/// The rows [`load_catch_types_at_link`] loads: `(catch type, method name,
/// handler pc)` of the first row naming each distinct catch type, skipping a
/// name that is not loaded for the class and that the global path would answer
/// by fabricating a synthetic stub, plus the class name for a `VerifyError`.
/// `None` when the class is gone.
fn link_catch_rows(
    cm: &crate::classloading::ClassManager,
    class_id: crate::classloading::ClassId,
) -> Option<(String, Vec<(String, String, u16, usize, u16)>)> {
    let class = cm.get_class(class_id)?;
    let mut seen: rustc_hash::FxHashSet<&str> = rustc_hash::FxHashSet::default();
    // (catch type, method name, handler pc, method index, catch type's
    // constant pool index)
    let mut rows: Vec<(String, String, u16, usize, u16)> = Vec::new();
    for (method_ix, method) in class.methods.iter().enumerate() {
        let Some(code) = method.code() else {
            continue;
        };
        for entry in &code.exception_table {
            if entry.catch_type == 0 {
                continue;
            }
            // A non-`CONSTANT_Class` index was rejected by the verifier's
            // structural pass (`catch_type_of`).
            let Some(name) = class.constant_pool.get_class_name(entry.catch_type) else {
                continue;
            };
            if !seen.insert(name) {
                continue;
            }
            if cm.find_class_by_name_for_class(name, class_id).is_none()
                && cm.would_fabricate_synthetic_stub(name)
            {
                continue;
            }
            rows.push((
                name.to_string(),
                method.name.to_string(),
                entry.handler_pc,
                method_ix,
                entry.catch_type,
            ));
        }
    }
    Some((class.name.to_string(), rows))
}

/// HotSpot's VerifyError message for a catch type that is not a `Throwable`
/// (interpreter round i5 wave 102, lane L7): `verify_exception_handler_table`'s
/// "Catch type is not a subclass of Throwable in exception handler H" with the
/// `Exception Details:` of `bad_type(H, cp(N, catch), implicit(Throwable))`
/// (no frame), or, below major 50, libverify's `initialize_exception_table`
/// text "catch_type not a subclass of Throwable" with its method prefix.
/// `None` when the method is no longer there.
fn catch_type_refusal_text(
    cm: &crate::classloading::ClassManager,
    class_id: crate::classloading::ClassId,
    method_ix: usize,
    handler_pc: u16,
    catch_cp: u16,
    catch_name: &str,
) -> Option<String> {
    use cratonvm_classloading::verify_frame::HsType;
    use cratonvm_classloading::verify_hotspot_text as hs;
    use cratonvm_classloading::vtype::VType;
    let class = cm.get_class(class_id)?;
    let method = class.methods.get(method_ix)?;
    if cratonvm_classloading::verify_old_text::uses_old_texts(class.version.major) {
        return Some(cratonvm_classloading::verify_old_text::message(
            &class.name,
            &method.name,
            &method.descriptor,
            "catch_type not a subclass of Throwable",
        ));
    }
    let code = method.code()?;
    let stack_map_raw =
        cratonvm_classloading::bytecode_verifier::find_stack_map_table(&code.attributes);
    let m = hs::HsMethod {
        class_name: &class.name,
        method_name: &method.name,
        descriptor: &method.descriptor,
        code: &code.code,
        exception_table: &code.exception_table,
        stack_map_raw,
    };
    let actual = if catch_name.starts_with('[') {
        VType::ArrayRef(std::sync::Arc::from(catch_name))
    } else {
        VType::ObjectRef(std::sync::Arc::from(catch_name))
    };
    Some(hs::render(
        &m,
        &format!("Catch type is not a subclass of Throwable in exception handler {handler_pc}"),
        usize::from(handler_pc),
        &hs::Reason::WrongType {
            actual: HsType::V(actual),
            actual_origin: hs::Origin::Cp(catch_cp),
            expected: HsType::V(VType::ObjectRef(std::sync::Arc::from("java/lang/Throwable"))),
            expected_origin: hs::Origin::Implicit,
        },
        None,
        None,
    ))
}

/// Would [`load_catch_types_at_link`] succeed for `class_id` without loading
/// anything, i.e. without running Java? True when every catch type it would
/// load is already loaded for the class and is a `Throwable`. `false` means
/// "unknown here", not "fails": the link-time loading must then run on a Java
/// thread (`vm_util::link_class_without_java` leaves such a class unlinked).
pub(crate) fn catch_types_settled_without_java(
    cm: &crate::classloading::ClassManager,
    class_id: crate::classloading::ClassId,
) -> bool {
    let Some((_, rows)) = link_catch_rows(cm, class_id) else {
        return false;
    };
    if rows.is_empty() {
        return true;
    }
    let throwable = cm.find_bootstrap_class_by_name("java/lang/Throwable");
    rows.iter().all(|(name, _, _, _, _)| {
        cm.find_class_by_name_for_class(name, class_id)
            .is_some_and(|id| throwable.is_none_or(|t| cm.is_subclass_of(id, t)))
    })
}

// ---------------------------------------------------------------------------
// JVMTI event delivery
// ---------------------------------------------------------------------------
//
// The `fire_jvmti_*` sites and the frame-push ordering they depend on:
// `interpreter/jvmti_events.rs`.

/// Best-effort JVMS opcode mnemonic + trailing-operand-byte-count lookup,
/// covering the opcodes relevant to a call-site-shaped bytecode sequence
/// (stack shuffles, loads/stores, field/invoke family, branches, `ldc`/
/// `new`/`checkcast`, returns). Not a complete disassembler -- unknown
/// opcodes report `extra_len=0` (may misalign the dump past that point);
/// good enough for the temporary `CRATONVM_DBG_BYTECODE_DUMP` diagnostic
/// above, which only needs to identify a stack-shape opcode (`dup`,
/// `dup_x1`, `swap`, `aload`, ...) near a known indy call site's operand
/// bytes (which ARE handled precisely, so the scan re-syncs at each
/// `invokedynamic`/`invokestatic`/etc. it passes).
pub(super) fn opcode_mnemonic_and_operand_len(
    op: u8,
    code: &[u8],
    pc: usize,
) -> (&'static str, usize) {
    match op {
        0x00 => ("nop", 0),
        0x01 => ("aconst_null", 0),
        0x02..=0x08 => ("iconst/lconst/fconst/dconst", 0),
        0x10 => ("bipush", 1),
        0x11 => ("sipush", 2),
        0x12 => ("ldc", 1),
        0x13 => ("ldc_w", 2),
        0x14 => ("ldc2_w", 2),
        0x15 => ("iload", 1),
        0x16 => ("lload", 1),
        0x17 => ("fload", 1),
        0x18 => ("dload", 1),
        0x19 => ("aload", 1),
        0x1a..=0x1d => ("iload_N", 0),
        0x1e..=0x21 => ("lload_N", 0),
        0x22..=0x25 => ("fload_N", 0),
        0x26..=0x29 => ("dload_N", 0),
        0x2a..=0x2d => ("aload_N", 0),
        0x36 => ("istore", 1),
        0x37 => ("lstore", 1),
        0x38 => ("fstore", 1),
        0x39 => ("dstore", 1),
        0x3a => ("astore", 1),
        0x3b..=0x3e => ("istore_N", 0),
        0x3f..=0x42 => ("lstore_N", 0),
        0x43..=0x46 => ("fstore_N", 0),
        0x47..=0x4a => ("dstore_N", 0),
        0x4b..=0x4e => ("astore_N", 0),
        0x57 => ("pop", 0),
        0x58 => ("pop2", 0),
        0x59 => ("dup", 0),
        0x5a => ("dup_x1", 0),
        0x5b => ("dup_x2", 0),
        0x5c => ("dup2", 0),
        0x5d => ("dup2_x1", 0),
        0x5e => ("dup2_x2", 0),
        0x5f => ("swap", 0),
        0xa7 => ("goto", 2),
        0xa8 => ("jsr", 2),
        0xac..=0xb1 => ("Xreturn", 0),
        0xb2 => ("getstatic", 2),
        0xb3 => ("putstatic", 2),
        0xb4 => ("getfield", 2),
        0xb5 => ("putfield", 2),
        0xb6 => ("invokevirtual", 2),
        0xb7 => ("invokespecial", 2),
        0xb8 => ("invokestatic", 2),
        0xb9 => ("invokeinterface", 4),
        0xba => ("invokedynamic", 4),
        0xbb => ("new", 2),
        0xbc => ("newarray", 1),
        0xbd => ("anewarray", 2),
        0xbe => ("arraylength", 0),
        0xbf => ("athrow", 0),
        0xc0 => ("checkcast", 2),
        0xc1 => ("instanceof", 2),
        0x99..=0x9e => ("if_icmp/ifxx", 2),
        0x9f..=0xa4 => ("if_icmpXX", 2),
        0xa5 | 0xa6 => ("if_acmpXX", 2),
        0xc6 | 0xc7 => ("ifnull/ifnonnull", 2),
        _ => {
            let _ = (code, pc);
            ("?", 0)
        }
    }
}

/// Convert a [`Value`] to the JVMTI-flavoured [`LocalValue`].
#[inline]
pub(super) fn to_local_value(v: Option<&Value>) -> crate::runtime::jvmti::LocalValue {
    use crate::runtime::jvmti::LocalValue as LV;
    match v {
        Some(Value::Int(i)) => LV::Int(*i),
        Some(Value::Long(l)) => LV::Long(*l),
        Some(Value::Float(f)) => LV::Float(*f),
        Some(Value::Double(d)) => LV::Double(*d),
        Some(Value::Object(None)) => LV::Object(None),
        // Cast: object/code pointer to integer address
        Some(Value::Object(Some(r))) => LV::Object(Some(r.as_ptr() as usize as u64)),
        _ => LV::Object(None),
    }
}

// ---------------------------------------------------------------------------
// Exception handler lookup
// ---------------------------------------------------------------------------

/// Find an applicable exception handler in this frame's exception table.
///
/// Per JVM spec §2.10, the exception table is searched in order and the
/// **first** matching handler wins. A handler matches when:
///   1. The PC is within [start_pc, end_pc)
///   2. catch_type == 0 (catch-all / finally), OR
///   3. The thrown exception is an instance of (or subclass of) the catch type
///
/// The interpreter unwinder no longer calls this: it uses
/// [`search_frame_for_unwind`], which needs the thread (catch-type resolution
/// may run Java) and reports a resolution failure. This thread-less form keeps
/// the pre-wave-2 lenient resolution and remains for the table-order tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn find_exception_handler(
    shared: &SharedVm,
    frame: &Frame,
    pc: usize,
    exc: ObjectRef,
) -> Option<(usize, ObjectRef)> {
    // Delegates to the shared implementation; see
    // `find_exception_handler_impl` for the lock-and-allocation rationale.
    find_exception_handler_impl(shared, frame, pc, exc)
}

/// Scan `thread.frames[frame_idx]`'s exception table for a handler that covers
/// `pc` and whose `catch_type` matches the in-flight throwable, which is read
/// from `thread.native_pin_roots[pin_slot]`.
///
/// Used by the JIT early-exception path and the OSR exception exit. Unlike the
/// interpreter unwinder, callers here may not know the *exact* PC of the throw
/// site (the JIT executed the entire bytecode method as native code). They must
/// still supply a best-known `pc` so that handler matching honors each entry's
/// `[start_pc, end_pc)` range — without that check, a `finally` (catch-all)
/// entry would incorrectly swallow exceptions whose throw site is outside that
/// try region.
///
/// Entries are searched in declaration order; the first matching handler
/// wins, mirroring the JVM spec's handler precedence for nested try/catch.
/// Catch types are resolved exactly as the interpreter unwinder resolves them
/// ([`catch_row_verdict`]), and a resolution error REPLACES the throwable in
/// the pin slot (see [`search_compiled_frame_rows`]), so the caller must
/// re-read the slot for whatever it resumes or propagates. Resolution may run
/// Java and collect, which is why the throwable is taken through a pin; a
/// caller holding other unrooted references passes `may_collect: false` (see
/// [`search_compiled_frame_rows`]).
pub(super) fn find_exception_handler_any_pc(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    pc: usize,
    pin_slot: usize,
    may_collect: bool,
) -> Option<usize> {
    let frame = &thread.frames[frame_idx];
    // `frame.code` is padded with 2 trailing bytes for the interpreter's
    // speculative reads; the real bytecode length is `len() - 2`.
    let code_len = frame.code.len().saturating_sub(2);
    let (class_id, table) = (frame.class_id, frame.exception_table_arc());
    search_compiled_frame_rows(
        shared,
        thread,
        class_id,
        &table,
        code_len,
        CompiledThrowSite::At(pc),
        pin_slot,
        may_collect,
        "frame-at-pc",
    )
    .handler_pc()
}

/// Find a handler in `thread.frames[frame_idx]`'s exception table when the
/// throw-site PC is **unknown** — the case after a JIT-compiled method runs to
/// completion as native code and a callee it dispatched throws.
///
/// The JIT executes the whole bytecode body, so there is no live interpreter
/// PC for the throw site. Passing `0` (a freshly-pushed frame's
/// `last_instr_pc`) to the PC-range check silently misses every handler whose
/// protected region does not start at 0 — e.g. a `try` block that begins a few
/// bytes into the method. That bug made a JIT-compiled `Main.main` propagate a
/// callee exception straight past its own `catch (Throwable)` (Jetty
/// `start.jar` launcher: the launcher's usage-error handling never ran,
/// surfacing a misleading inner NPE instead).
///
/// Mirroring `route_jit_exception_through_method`: without a known PC we
/// cannot range-check a handler, so a catch-all (`catch_type == 0`, i.e.
/// `finally`) is only honoured when its protected region covers the **whole
/// method** (`start_pc == 0 && end_pc >= code_len`) — such an entry catches a
/// throw at any PC, so running it is sound regardless of the (unknown) throw
/// site. This preserves `finally` / synchronized-monitor-exit cleanup for the
/// dominant method-wide case instead of dropping it. Narrower catch-all
/// regions are still skipped (they could swallow an out-of-region exception),
/// and typed handlers match by exception class as before.
///
/// Same pin contract as [`find_exception_handler_any_pc`]; the only caller
/// (the early-JIT route) holds nothing unrooted, so the search may collect.
pub(super) fn find_exception_handler_pc_unknown(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    pin_slot: usize,
) -> Option<usize> {
    let frame = &thread.frames[frame_idx];
    // No table, no handler — and no class-manager lock to learn that.
    if frame.exception_table().is_empty() {
        return None;
    }
    let code_len = frame.code.len().saturating_sub(2);
    let (class_id, table) = (frame.class_id, frame.exception_table_arc());
    search_compiled_frame_rows(
        shared,
        thread,
        class_id,
        &table,
        code_len,
        CompiledThrowSite::Unknown,
        pin_slot,
        true,
        "first-call-door",
    )
    .handler_pc()
}

/// The `throw_pc` a callee service hands [`run_jit_callee_handler`] for an
/// EXPLICIT throwable that carried no stamp (interpreter round i5 wave 105,
/// lane L2). Not a pc and not `usize::MAX`: the handler sink turns it into a
/// definite `NotCaught` unless the callee's own precise frame names the site
/// (see the comment there), or into `usize::MAX` under
/// `CRATONVM_JIT_UNSTAMPED_CALLEE_THROW_ESCAPES=0`. Nothing else may see it:
/// a caller that re-posts the throwable tests `>= ` this value for "no pc".
pub(crate) const JIT_THROW_PC_UNSTAMPED_EXPLICIT: usize = usize::MAX - 1;

/// `CRATONVM_JIT_UNSTAMPED_CALLEE_THROW_ESCAPES` (default ON, `=0` restores the
/// pc-unknown search): see [`JIT_THROW_PC_UNSTAMPED_EXPLICIT`]. Read on the
/// throw path only.
pub(crate) fn unstamped_callee_throw_escapes_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSTAMPED_CALLEE_THROW_ESCAPES")
}

/// Where a compiled frame's throw happened, as far as its handler search knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompiledThrowSite {
    /// The throw bci is known: rows are range-checked (JVMS §2.10).
    At(usize),
    /// Unknown: a catch-all is honoured only when its range spans the whole
    /// method; a typed row matches on exception class alone. See
    /// [`find_exception_handler_pc_unknown`].
    Unknown,
}

impl CompiledThrowSite {
    /// `usize::MAX` is the compiled routes' "pc unknown" sentinel.
    pub(crate) fn of(throw_pc: usize) -> Self {
        if throw_pc == usize::MAX {
            Self::Unknown
        } else {
            Self::At(throw_pc)
        }
    }
}

/// What a compiled frame's handler search decided. The throwable it decided
/// about is the one left in the caller's pin slot — the original, or a
/// resolution error that replaced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompiledHandlerSearch {
    /// Resume at this handler pc.
    Found(usize),
    /// No row catches it. `definite` is false only when every row was judged
    /// without a throw pc (see [`CalleeHandlerMiss::Unknown`]); a resolution
    /// failure restarts the search at a KNOWN pc, so its answer is definite.
    NotFound { definite: bool },
}

impl CompiledHandlerSearch {
    #[inline]
    pub(crate) fn handler_pc(self) -> Option<usize> {
        match self {
            Self::Found(pc) => Some(pc),
            Self::NotFound { .. } => None,
        }
    }
}

/// Pin every object reference in `values` on `thread.native_pin_roots`, in
/// order, and return the pin base; [`reread_pinned_values`] writes the
/// (possibly moved) references back. The caller truncates to the base.
fn pin_object_values(thread: &mut JvmThread, values: &[Value]) -> usize {
    let base = thread.native_pin_roots.len();
    for v in values {
        if let Value::Object(Some(r)) = v {
            thread.native_pin_roots.push(*r);
        }
    }
    base
}

/// The other half of [`pin_object_values`], for the same `values`.
fn reread_pinned_values(thread: &JvmThread, values: &mut [Value], base: usize) {
    let mut pin = base;
    for v in values.iter_mut() {
        if let Value::Object(Some(r)) = v {
            *r = thread.native_pin_roots[pin];
            pin += 1;
        }
    }
}

/// Run a compiled-frame handler search (`search`) with everything the caller
/// will resume the handler on rooted, so the search may collect: `args` —
/// returned as a re-read copy — and, when present, a deferred precise frame's
/// raw object addresses, re-read in place
/// (`deopt_materialize::pin_frame_object_refs`).
///
/// Before wave 4 the deferred-frame routes could not re-read that frame, so
/// they searched with `may_collect: false` and read an unloadable catch type as
/// "no match" instead of throwing its resolution error.
fn pinned_compiled_handler_search<R>(
    thread: &mut JvmThread,
    args: &[Value],
    deferred: Option<&mut cratonvm_jit::deopt::ReconstructedFrame>,
    search: impl FnOnce(&mut JvmThread) -> R,
) -> (R, Vec<Value>) {
    use crate::runtime::deopt_materialize::{pin_frame_object_refs, reread_frame_object_refs};
    let mut resume_args = args.to_vec();
    let args_base = pin_object_values(thread, &resume_args);
    let frame_base = deferred
        .as_deref()
        .map(|frame| pin_frame_object_refs(thread, frame));
    let found = search(thread);
    if let (Some(frame), Some(base)) = (deferred, frame_base) {
        reread_frame_object_refs(thread, frame, base);
    }
    reread_pinned_values(thread, &mut resume_args, args_base);
    thread.native_pin_roots.truncate(args_base);
    (found, resume_args)
}

/// The one handler search the compiled-frame routes share
/// ([`find_jit_exception_handler`], [`find_exception_handler_any_pc`],
/// [`find_exception_handler_pc_unknown`]), with the interpreter unwinder's
/// catch-type semantics.
///
/// Until round i1 wave 3 each of those carried its own copy of the pre-wave-2
/// search: the catch type was loaded through the loader-blind
/// `load_class_concurrent` and a failed load read as "no match", so a missing
/// catch type was skipped with the ORIGINAL exception still in flight — while
/// the same frame, interpreted, threw the resolution error. Now every typed
/// row goes through [`catch_row_verdict`] (the per-thread resolved-class table,
/// then `resolve_class_loader_aware`), and a resolution failure does what
/// HotSpot's `exception_handler_for_exception` does and [`unwind_to_handler`]
/// does: the error replaces the throwable in `native_pin_roots[pin_slot]` and
/// the search restarts in this frame at the failing row's `handler_pc`, a
/// KNOWN pc. The restart count is bounded by the table length, as there.
///
/// Row order and range rules are unchanged: first covering row wins; with an
/// unknown pc a catch-all counts only when it spans the whole method.
///
/// # `may_collect`
///
/// The loader-faithful resolution may run Java and materializing the error
/// allocates, so either can collect. The interpreter unwinder can afford that
/// because every value it resumes on lives in a rooted frame; a compiled route
/// holds raw `Value`s and, with a deferred precise frame or an OSR exit frame,
/// a reconstructed frame's raw object addresses. Since wave 4 every production
/// caller pins all of them around the search
/// ([`pinned_compiled_handler_search`], `deopt_materialize::pin_frame_object_refs`)
/// and passes `true`. `false` remains for a caller that cannot: it gets the
/// pre-wave-3 answer for an unloaded or unresolvable catch type (see
/// [`catch_row_verdict`]).
fn search_compiled_frame_rows(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: crate::classloading::ClassId,
    table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    code_len: usize,
    site: CompiledThrowSite,
    pin_slot: usize,
    may_collect: bool,
    route: &'static str,
) -> CompiledHandlerSearch {
    let mut site = site;
    let mut restarts = 0usize;
    'search: loop {
        // Read lazily (a frame with only catch-alls never needs it), and once
        // per pass: a class id does not move, but a restart changes the
        // throwable.
        let mut exc_class_id = None;
        for entry in table {
            // Widening: u16 -> usize (non-negative, fits)
            let (start, end) = (entry.start_pc as usize, entry.end_pc as usize);
            let covered = match site {
                CompiledThrowSite::At(pc) => pc >= start && pc < end,
                CompiledThrowSite::Unknown => {
                    entry.catch_type != 0 || (start == 0 && end >= code_len)
                }
            };
            if !covered {
                continue;
            }
            // Widening: u16 -> usize (non-negative, fits)
            let handler_pc = entry.handler_pc as usize;
            if entry.catch_type == 0 {
                return CompiledHandlerSearch::Found(handler_pc);
            }
            let exc_cid = match exc_class_id {
                Some(id) => id,
                None => {
                    let id = shared
                        .mem
                        .heap
                        .class_id_of(thread.native_pin_roots[pin_slot]);
                    exc_class_id = Some(id);
                    id
                }
            };
            match catch_row_verdict(
                shared,
                thread,
                class_id,
                entry.catch_type,
                exc_cid,
                may_collect,
            ) {
                CatchRowVerdict::Match => {
                    // Interpreter round i5 wave 101, lane L2 (page
                    // `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
                    // a typed row taken WITHOUT a throw pc, over a range that
                    // does not span the method, is a guess HotSpot never
                    // makes. Said, so a battery run can count them.
                    if site == CompiledThrowSite::Unknown
                        && !(start == 0 && end >= code_len)
                        && crate::runtime::env_cache::dbg_jitc()
                    {
                        eprintln!(
                            "[cratonvm-jitc] JITC-PC-CENSUS pc-unknown typed match: class_id={} range=[{start},{end}) handler={handler_pc} code_len={code_len} route={route}{}",
                            class_id.as_u32(),
                            crate::jit::helpers::jitc_census_backtrace()
                        );
                    }
                    return CompiledHandlerSearch::Found(handler_pc);
                }
                CatchRowVerdict::NoMatch => {}
                CatchRowVerdict::ResolutionError(error) => {
                    // The only root of the new throwable from here on.
                    thread.native_pin_roots[pin_slot] = error;
                    restarts += 1;
                    if restarts > table.len() {
                        return CompiledHandlerSearch::NotFound { definite: true };
                    }
                    site = CompiledThrowSite::At(handler_pc);
                    continue 'search;
                }
            }
        }
        return CompiledHandlerSearch::NotFound {
            definite: matches!(site, CompiledThrowSite::At(_)),
        };
    }
}

/// Core of the thread-less [`find_exception_handler`] (tests only since round
/// i1 wave 3: every production search resolves catch types through
/// [`catch_row_verdict`] — [`search_frame_for_unwind`] for the interpreter
/// unwinder, [`search_compiled_frame_rows`] for the compiled-frame routes).
///
/// HIGH — take the class_manager read lock ONCE for the whole search.
/// The previous implementation acquired up to three read locks per catch
/// entry and cloned each catch-type class name into an owned String. Both
/// are hot on the exception fast-path. We resolve the catch-type name as
/// `&str` from the constant pool and compare via
/// `find_class_by_name(&str)` without allocating.
///
/// The lock is dropped only if a lazy class-load is required (since
/// `load_class_concurrent` takes the write lock internally), then
/// reacquired.
///
/// The lock is also not taken at all until the first TYPED row whose range
/// covers `pc`. An exception unwinding N frames used to pay N class-manager
/// read acquisitions (an atomic RMW on a lock word every thread shares, and a
/// wait behind any class definition holding the write lock) plus a class
/// lookup and a heap header read per frame, even though most frames it
/// crosses have an empty table or no row covering their call site. Neither
/// answer needs the lock, and neither does a covering `finally` (catch-all)
/// row, so those now return before it is taken. Table order is preserved: the
/// rows are still visited first to last, and the lock is acquired in place
/// when the first covering typed row is reached.
#[inline]
pub(super) fn find_exception_handler_impl(
    shared: &SharedVm,
    frame: &Frame,
    pc: usize,
    exc: ObjectRef,
) -> Option<(usize, ObjectRef)> {
    let table = frame.exception_table();
    // Cheapest possible miss: no row covers `pc`. No lock, no header read.
    let first_covering = table.iter().position(|entry| {
        // Widening: index conversion
        pc >= entry.start_pc as usize && pc < entry.end_pc as usize
    })?;
    // A covering catch-all ahead of every covering typed row wins outright.
    let first = &table[first_covering];
    if first.catch_type == 0 {
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        return Some((first.handler_pc as usize, exc));
    }

    let exc_class_id = shared.mem.heap.class_id_of(exc);

    let mut cm_guard = shared.classes.class_manager.read();
    // Verify the owning class exists once — hoist this invariant out
    // of the per-entry loop. We re-fetch the (shared-borrowed) class
    // inside the loop to get the constant pool; that's a cheap
    // `HashMap` get against the held read guard.
    cm_guard.get_class(frame.class_id)?;

    for entry in table[first_covering..].iter() {
        // Widening: index conversion
        if pc < entry.start_pc as usize || pc >= entry.end_pc as usize {
            continue;
        }
        // catch_type == 0 means catch-all (finally block) — always matches
        if entry.catch_type == 0 {
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            return Some((entry.handler_pc as usize, exc));
        }

        // Resolve catch-type class name as a borrowed `&str` from
        // the constant pool — no allocation.
        let owning_class = cm_guard.get_class(frame.class_id)?;
        let Some(catch_class_name) = owning_class.constant_pool.get_class_name(entry.catch_type)
        else {
            continue;
        };
        // Try to find the catch type class on the held lock. The common case --
        // the catch type is loaded -- is answered here on the BORROWED name,
        // with no allocation: the owned copy below is needed only to survive
        // the lock drop of a lazy load. (r9w8 review8b: this used to
        // `to_string()` every typed row of every search.)
        if let Some(id) = cm_guard.find_class_by_name_for_class(catch_class_name, frame.class_id) {
            if cm_guard.is_subclass_of(exc_class_id, id)
                || cm_guard.is_subclass_of_by_name(exc_class_id, catch_class_name)
            {
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                return Some((entry.handler_pc as usize, exc));
            }
            continue;
        }
        // Owned copy for the loader-identity-blind fallback below (see
        // `Class::is_subclass_of_by_name`) — independent of `cm_guard`'s
        // current borrow so it stays valid across the lock drop/reacquire
        // in the lazy-load branch just below.
        let catch_class_name_owned = catch_class_name.to_string();
        let mut catch_class_id = None;
        {
            // A catch type that is not already loaded must NEVER be lazily
            // loaded through the loader-blind global path when that path would
            // fabricate a synthetic stub. `load_class_concurrent` searches only
            // the bootstrap/application classpath, and a class visible solely
            // through a custom loader (Quarkus's `RunnerClassLoader`, which owns
            // every `lib/main/*.jar` of a fast-jar distribution) is not on it —
            // so the "load" succeeded by MINTING a code-less stub and
            // registering it globally under that name, permanently poisoning it
            // for the loader that does own the real class. Observed on the
            // Keycloak 26.6.1 boot: `io/quarkus/runtime/PreventFurtherStepsException`
            // (in `io.quarkus.quarkus-core-*.jar`) was stubbed from this very
            // site every time Quarkus's shutdown path unwound.
            //
            // Nothing is lost by skipping it: the by-name subclass test below
            // walks the THROWN object's own superclass chain and needs no
            // ClassId for the catch type at all.
            if !cm_guard.would_fabricate_synthetic_stub(&catch_class_name_owned) {
                // Lazy load: must drop the read lock since
                // `load_class_concurrent` acquires the write lock.
                drop(cm_guard);
                let loaded = shared.load_class_concurrent(&catch_class_name_owned);
                cm_guard = shared.classes.class_manager.read();
                catch_class_id = loaded.ok();
            }
        }

        if catch_class_id.is_some_and(|id| cm_guard.is_subclass_of(exc_class_id, id))
            || cm_guard.is_subclass_of_by_name(exc_class_id, &catch_class_name_owned)
        {
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            return Some((entry.handler_pc as usize, exc));
        }
    }

    None
}

/// Consume an optional reason-9 frame published by the x64 backend and route
/// a pending Java exception with the exact throw bci and reconstructed locals.
///
/// `None` means the compiled method used the historical params-only route.
/// A matching but unmappable frame fails closed by propagating the exception;
/// entering a handler with zeroed non-parameter locals would be a silent
/// miscompile. A foreign frame is dropped (see the arm below).
///
/// `compiled` is the body that just ran and threw. A stashed frame is this
/// method's only when it names the method AND its point is one of that body's
/// own (`deopt_frame_matches_artifact`; interpreter round i1 wave 11,
/// `interpreter-L7-deopt-sinks-match-stashed-frames-by-name-FIXED-20260925.md`):
/// a same-named frame of another body — another loader's copy of the class,
/// or a nested activation under a different artifact — would otherwise route
/// this exception with that body's throw bci and locals.
#[allow(clippy::too_many_arguments)]
pub(super) fn route_jit_signal_exception(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller_frame_idx: usize,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    fallback_kind: JitThrowPc,
    exc: ObjectRef,
    fallback_locals: &[Value],
) -> Result<CachedCallResult, MethodCallFailed> {
    let fallback_throw_pc = match fallback_kind {
        JitThrowPc::InRange(pc) => pc,
        _ => usize::MAX,
    };
    // A frame that names a scalar-replaced object or an elided lock is held
    // back UNMAPPED and resolved at the last possible moment — see
    // `materialize_and_relock_precise_frame` for why the rebuild must not be
    // separated from the frame that roots it by anything that can collect.
    // Before scalar replacement was admitted under precise exception frames
    // this branch was unreachable and every frame took the `Mapped` arm.
    let mut deferred_precise: Option<cratonvm_jit::deopt::ReconstructedFrame> = None;
    // Interpreter round i1 wave 24, lane L2: the locks this body's compiled
    // code took, as the frame it published names them (`relock == false`),
    // pinned from the take until the route decides. A pushed handler frame
    // inherits them (javac's catch-all releases them); an exception that
    // leaves without the handler releases them. See `CompiledLocksOfAStash`.
    let mut compiled_locks: Option<super::deopt_resume::CompiledLocksOfAStash> = None;
    // Round 17 wave 4 (STD17-1): the take releases the holds of any frame it
    // discards beneath its own (off the self-lock gate it is the plain take).
    let precise =
        match super::deopt_resume::CompiledLocksOfAStash::take_exceptional_frame_releasing_discards(
            shared,
            thread,
            &|_, _| false,
            "door drain",
        ) {
            Some((rframe, _cause, point_addr))
                if deopt_frame_matches_artifact(
                    &rframe,
                    compiled,
                    point_addr,
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                ) =>
            {
                // JIT round 17 wave 2, lane syncdoor2 (SD17-1): naming a handed
                // method monitor, which the pushed handler frame then owns
                // (`route_jit_exception_through_method`).
                compiled_locks = Some(
                    super::deopt_resume::CompiledLocksOfAStash::pin_compiled_locks_of_method(
                        shared,
                        thread,
                        &rframe,
                        cached.declaring_class_id,
                        cached.is_synchronized,
                        cached.is_static,
                        &cached.code,
                    ),
                );
                let bci = rframe.bci as usize;
                // `!is_synchronized` is not belt-and-braces. Two things depend on
                // it and they fail differently: `materialize_and_relock_precise_frame`
                // REFUSES a synchronized method (its method monitor is taken by the
                // invoke path and is not a frame-state entry), and the placeholder
                // below would reach `JitSynchronizedMonitorGuard::acquire` as an
                // EMPTY argument list, which is where that monitor's receiver comes
                // from. The JIT does not create the shape — it refuses to
                // scalar-replace under precise frames in a synchronized method for
                // exactly this reason — so this is the second lock on a door the
                // first one already holds, written out because the placeholder makes
                // the failure silent rather than loud.
                if !cached.is_synchronized && precise_frame_needs_materialization(&rframe) {
                    deferred_precise = Some(rframe);
                    // The locals are a placeholder: the deferred frame replaces
                    // them wholesale below, and nothing between here and there
                    // reads them. `Vec::new()` rather than the incoming arguments
                    // so a path that forgot to substitute fails loudly (every local
                    // uninitialised) instead of quietly resuming on parameters.
                    Some((bci, Vec::new()))
                } else {
                    let Some(mut locals) = ir_deopt_locals(&rframe.locals) else {
                        // The exception leaves this activation without its
                        // handler: nothing will release what its code locked.
                        if let Some(locks) = compiled_locks.take() {
                            locks.release_for_a_propagation(shared, thread);
                        }
                        return Err(MethodCallFailed::ExceptionThrown(exc));
                    };
                    // Round 11 wave 11 (lane irexc,
                    // `r11w10-irexc-reason9-drain-does-not-repair-the-receiver-slot`):
                    // a snapshot publishes a local liveness proves dead as
                    // `Undefined`, which maps to `Int(0)` — and for slot 0 of an
                    // instance method that is `this`, which a `synchronized`
                    // method's monitor re-acquire below
                    // (`JitSynchronizedMonitorGuard::acquire`) and stack traces
                    // still need. `fallback_locals` is the genuine `this` plus
                    // parameters (pinned and re-read by every caller), so put the
                    // receiver back — the repair `precise_handler_frame_for`
                    // makes, restricted to a DEAD slot 0: a live one is whatever
                    // the bytecode stored there, and a handler may read it.
                    restore_dead_receiver(
                        &mut locals,
                        &rframe.locals,
                        cached.is_static,
                        fallback_locals,
                    );
                    Some((bci, locals))
                }
            }
            // A foreign exceptional frame is DROPPED, not re-stashed. Unlike an
            // ordinary deopt frame — whose owner is still on the stack waiting to
            // resume — an exceptional frame's owner is always the compiled body that
            // just unwound to produce this exception, so if it does not name the
            // method being drained here, its owner is gone and nobody can ever claim
            // it. Re-stashing left it to be picked up by a LATER exception in the
            // same method, which would then route with a stale bci and stale
            // (possibly collected) object pointers. A same-named frame whose point
            // is not `compiled`'s is foreign on the same argument: its owner is
            // another body, which has unwound too.
            Some((foreign, _, foreign_point)) => {
                if crate::jit::helpers::rbc6_dbg() {
                    eprintln!(
                        "[rbc6-dbg] route_jit_signal_exception {}.{}{} dropped a foreign \
                     exceptional frame {} bci={} (point {:#x}, owned by this body: {})",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        foreign.method_key,
                        foreign.bci,
                        foreign_point,
                        compiled.owns_deopt_point(foreign_point),
                    );
                }
                // JIT round 17 wave 2, lane syncdoor2 (SD17-1): the owner has
                // unwound, so what its compiled code still held is released here,
                // as a propagation would (`CompiledLocksOfAStash`). Under the
                // exceptional hand-over a self-locking body's precise frame names
                // its METHOD monitor, which only a sink ever releases; dropped
                // silently, the monitor would stay held for good. Gated on the
                // two switches the JIT hands over under, so the default is
                // unchanged (a dropped frame naming a block lock leaks as before;
                // `CRATONVM_DBG_DEOPT` names it at the next take).
                if foreign_frame_hold_released_on_drop() {
                    super::deopt_resume::CompiledLocksOfAStash::pin_compiled_locks(
                        thread, &foreign,
                    )
                    .release_for_a_propagation(shared, thread);
                }
                None
            }
            None => None,
        };
    if matches!(fallback_kind, JitThrowPc::EscapedRedispatch) {
        // Interpreter round i5 wave 103, lane L2: the throwable already left
        // this method through a not-entrant re-dispatch, which searched its
        // table at the real throw pc. No frame this door could claim describes
        // that activation; if one was claimed anyway, nothing resumes it.
        if let Some(locks) = compiled_locks.take() {
            locks.release_for_a_propagation(shared, thread);
        }
        if crate::jit::helpers::rbc6_dbg() {
            eprintln!(
                "[rbc6-dbg] route_jit_signal_exception PROPAGATE {}.{}{} — escaped a not-entrant re-dispatch",
                cached.class_name, cached.method_name, cached.method_descriptor,
            );
        }
        return Err(MethodCallFailed::ExceptionThrown(exc));
    }
    if let (None, JitThrowPc::OutsideAllRanges(stamped_pc)) = (precise.as_ref(), &fallback_kind) {
        // The compiled body stamped a throw site of its OWN that lies inside no
        // protected range. That is not "pc unknown" — it is this method saying
        // it cannot catch this throw — and the pc-unknown search below would
        // match a typed row by exception class alone and swallow it. Propagate,
        // which is what the JVM does for a throw outside every `try`.
        //
        // A SELF-call stamp used to be exempted here and routed to the
        // pc-unknown search instead ("cannot tell", not "not caught"). It is
        // not any more, and the reason is measured rather than argued — see
        // `docs/internal/retired/r10-selfrec-deep-handler-leaks-once-per-million-20260922-RETIRED-20260922.md`.
        // The pc-unknown search skips a narrow catch-ALL but matches a TYPED
        // handler on exception class alone, ignoring its protected range, so
        // the downgrade ran this method's `catch` for a throw at an UNCOVERED
        // bci, in the outermost activation's frame, with that activation's
        // locals. `regression-suite/src/RJitSelfRecUncovered.java` is the
        // deterministic witness: 19,930 of 20,000 calls returned `1004` where
        // the bytecode owes an escaping `IllegalStateException`.
        if crate::jit::helpers::rbc6_dbg() {
            eprintln!(
                "[rbc6-dbg] route_jit_signal_exception PROPAGATE {}.{}{} — stamped throw pc {} is outside every protected range",
                cached.class_name, cached.method_name, cached.method_descriptor, stamped_pc,
            );
        }
        return Err(MethodCallFailed::ExceptionThrown(exc));
    }
    let (throw_pc, locals) = match precise.as_ref() {
        Some((bci, locals)) => (*bci, locals.as_slice()),
        None => {
            // The sibling of `run_jit_callee_handler`'s refusal, for the sink
            // that was ALREADY consuming precise frames. Consuming them is not
            // the whole contract: when none is stashed, `fallback_locals` is
            // this method's `this`-plus-parameters, which describes a handler
            // that reads nothing else. `precise_handler_frames_enabled` retired
            // the compile gate that used to guarantee that, so a method whose
            // handler DOES read further locals reaches here too — and with the
            // throw pc unknown, `find_jit_exception_handler` will still match
            // one of its typed handlers by exception class. Entering it would
            // zero those locals silently. Propagate instead, exactly as this
            // function already does for a frame it cannot map.
            if handler_resume_needs_precise_locals(thread, cached) {
                if crate::jit::helpers::rbc6_dbg() {
                    eprintln!(
                        "[rbc6-dbg] route_jit_signal_exception DECLINED {}.{}{} \
                         — handler needs precise locals and no frame was published",
                        cached.class_name, cached.method_name, cached.method_descriptor,
                    );
                }
                return Err(MethodCallFailed::ExceptionThrown(exc));
            }
            (fallback_throw_pc, fallback_locals)
        }
    };
    if crate::jit::helpers::rbc6_dbg() {
        eprintln!(
            "[rbc6-dbg] route_jit_signal_exception {}.{}{} precise_bci={:?} fallback_throw_pc={} chosen={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            precise.as_ref().map(|(b, _)| *b as i64),
            fallback_throw_pc as i64,
            throw_pc as i64,
        );
    }
    // The handler frame, when there is one, is pushed on top of the stack.
    let handler_frame_idx = thread.frames.len();
    let routed = route_jit_exception_through_method(
        shared,
        thread,
        caller_frame_idx,
        cached,
        throw_pc,
        exc,
        locals,
        deferred_precise.as_mut(),
        // Round 14 wave 1 (lane sync): `fallback_locals` is the genuine
        // `this`-plus-parameters, so its slot 0 is the method monitor's object
        // whatever the precise frame's local 0 holds.
        (!cached.is_static)
            .then(|| fallback_locals.first().copied())
            .flatten(),
        compiled_locks.as_ref(),
    );
    // Only this body's own frame pinned anything, so every other arm (no
    // precise frame, a foreign one) has `None` here.
    if let Some(locks) = compiled_locks {
        match &routed {
            // The handler frame resumes the activation and holds its locks:
            // record them in it (its `monitorexit` finds them there).
            Ok(_) => locks.seed_pushed_frame_and_unpin(thread, handler_frame_idx),
            // No handler covers the throw, a deferred frame could not be
            // materialised, or the frame could not be built: the exception
            // (or the error) leaves the activation, which never resumes.
            Err(_) => {
                locks.release_for_a_propagation(shared, thread);
            }
        }
    } else if routed.is_ok() {
        // Interpreter round i6 wave 120, lane L9 review: no stash, so the
        // pushed handler frame's record describes nothing the compiled body
        // held. Mark it understated where the method has monitor bytecode
        // (see `run_jit_callee_handler`'s twin), so the per-frame strict
        // `monitorexit` arm stays lenient for its catch-any.
        if let Some(frame) = thread.frames.get_mut(handler_frame_idx) {
            crate::runtime::interpreter::held_monitors::clear_untrusted_record(
                frame,
                crate::runtime::interpreter::held_monitors::RecordProducer::SignalHandlerNoStash,
            );
        }
    }
    // Interpreter round i6 wave 121, lane L6 (page
    // `i116-L6-a-compiled-unpaired-monitorexit-releases-its-callers-lock`,
    // "Wave 121"): the elided levels `materialize_and_relock_precise_frame`
    // re-took for the handler frame are named by no record (the stash pins the
    // taken locks only, `CompiledLocksOfAStash::pin_compiled_locks`), so the
    // frame's record is understated whatever the stash seeded.
    if routed.is_ok() && elided_levels_retaken_by_materializer(deferred_precise.as_ref()) {
        if let Some(frame) = thread.frames.get_mut(handler_frame_idx) {
            frame.held_monitors.mark_understated_by(
                crate::runtime::interpreter::held_monitors::RecordProducer::RetakenElidedLevels,
            );
        }
    }
    routed
}

/// Did `materialize_and_relock_precise_frame` re-take any elided level of
/// `rframe` (its trapping scope's monitors with `relock`) for the handler
/// frame? Those levels are held by the frame and named by no record.
/// Interpreter round i6 wave 121, lane L6.
fn elided_levels_retaken_by_materializer<R>(rframe: Option<&R>) -> bool
where
    R: std::borrow::Borrow<cratonvm_jit::deopt::ReconstructedFrame>,
{
    rframe.is_some_and(|r| r.borrow().monitors.iter().any(|m| m.relock && m.lock_depth > 0))
}

/// Put the genuine receiver back into slot 0 of a mapped reason-9 frame of an
/// instance method, when — and only when — the snapshot published that slot as
/// dead (`FrameValue::Undefined`, mapped to `Int(0)`). See the call in
/// [`route_jit_signal_exception`]. `fallback` is the caller's `this`-plus-
/// parameters array; a static method, a live slot 0 or an empty `fallback`
/// leaves `locals` as it is.
fn restore_dead_receiver(
    locals: &mut [Value],
    published: &[cratonvm_jit::deopt::FrameValue],
    is_static: bool,
    fallback: &[Value],
) {
    if is_static
        || !matches!(
            published.first(),
            Some(cratonvm_jit::deopt::FrameValue::Undefined)
        )
    {
        return;
    }
    if let (Some(slot0), Some(receiver)) = (locals.first_mut(), fallback.first()) {
        *slot0 = *receiver;
    }
}

/// The `Frame` sibling of [`jit_local_athrow_pc`], for the sink that has pushed
/// a frame rather than holding a `CachedBytecodeMethod`.
///
/// Same two tests, same reason: the stamp carries no method identity, so it is
/// honoured only when it lands inside one of THIS method's protected ranges and
/// on a real instruction boundary.
pub(super) fn jit_local_athrow_pc_in_frame(frame: &Frame, athrow_bci: i64) -> JitThrowPc {
    if athrow_bci == JIT_THROW_ESCAPED_REDISPATCH {
        return JitThrowPc::EscapedRedispatch;
    }
    if athrow_bci < 0 {
        return JitThrowPc::Unknown;
    }
    let pc = athrow_bci as usize;
    // `frame.code` carries 2 bytes of speculative-read padding.
    let code_len = frame.code.len().saturating_sub(2);
    if pc >= code_len {
        return JitThrowPc::Unknown;
    }
    match cratonvm_reader::verified_code(&frame.code[..code_len]) {
        Ok(verified) if verified.is_instruction_start(pc) => {}
        _ => return JitThrowPc::Unknown,
    }
    let in_a_protected_range = frame
        .exception_table()
        .iter()
        // Widening: u16 -> usize (non-negative, fits)
        .any(|e| pc >= e.start_pc as usize && pc < e.end_pc as usize);
    if in_a_protected_range {
        JitThrowPc::InRange(pc)
    } else {
        JitThrowPc::OutsideAllRanges(pc)
    }
}

/// What a stamped throw bci says about THIS method's exception table.
///
/// The two-state `usize::MAX`-or-pc answer conflated the last two, and that is
/// how an exception got swallowed. A bci that is a real instruction boundary in
/// this method but lies outside every protected range is not "unknown": it is a
/// definite statement that no handler here covers the throw. Reporting it as
/// unknown sent it to the pc-UNKNOWN search, which matches typed rows by
/// exception CLASS ALONE — so `outsideTryStep`'s `catch (RuntimeException)`
/// over `[23,27)` "caught" a throw at bci 20 and returned normally.
/// `JitCalleeExceptionShapes.outsideTryChecksum` is the fixture; BouncyCastle's
/// `CipherInputStream.nextChunk` is the field report, where the swallowed
/// exception was an AEAD tag mismatch and the caller read a clean EOF over
/// tampered ciphertext.
pub(super) enum JitThrowPc {
    /// Inside one of this method's protected ranges — range-check with it.
    InRange(usize),
    /// A real instruction boundary here, but inside no protected range. This
    /// method cannot catch the throw; propagate to the caller.
    ///
    /// **Keeping this distinct from [`Self::Unknown`] is the whole point of the
    /// enum, and it has been given away once.** Between 2026-08-20 and
    /// 2026-09-22 both consumers re-mapped this variant to the pc-unknown
    /// search whenever the stamp named a self-call site of the method being
    /// drained — a self-recursive chain's one thread-global stamp cannot say
    /// which activation wrote it, so the "cannot catch" verdict was downgraded
    /// to "cannot tell". That is the same swallow the paragraph above
    /// describes, reached from the other side: `R10SelfRecCatch.recDeep`'s
    /// `catch (IllegalStateException)` over `[27,37)` "caught" a throw at bci
    /// 23 and returned `1004` where the bytecode owes an escaping exception.
    /// See
    /// `docs/internal/retired/r10-selfrec-deep-handler-leaks-once-per-million-20260922-RETIRED-20260922.md`.
    OutsideAllRanges(usize),
    /// No usable stamp (absent, past the end, not an instruction boundary, or a
    /// foreign method's). Fall back to the pc-unknown search.
    Unknown,
    /// Interpreter round i5 wave 103, lane L2: the throwable already LEFT this
    /// method. The body the door entered had been retired, and its
    /// not-entrant stub ran the method through the VM's invoke path, which
    /// offered it to this method's handlers at the real throw pc
    /// (`DrainedJitSignals::escaped_a_redispatch_of`). Propagate; searching
    /// again, with no pc, could only run a typed row the throw had left. The
    /// door reports no event for it: the re-dispatch already did, at the real
    /// site. Carried in the raw stamp as [`JIT_THROW_ESCAPED_REDISPATCH`].
    EscapedRedispatch,
}

/// The raw-stamp value an interpreter door hands its classifier for a throwable
/// that escaped a not-entrant re-dispatch of the method it entered
/// ([`JitThrowPc::EscapedRedispatch`]). Below `-1` (the "no stamp" value), so
/// every reader that only asks `< 0` still reads "no pc".
pub(super) const JIT_THROW_ESCAPED_REDISPATCH: i64 = -2;

pub(super) fn jit_local_athrow_pc_kind(
    cached: &CachedBytecodeMethod,
    athrow_bci: i64,
) -> JitThrowPc {
    if athrow_bci == JIT_THROW_ESCAPED_REDISPATCH {
        return JitThrowPc::EscapedRedispatch;
    }
    if athrow_bci < 0 {
        return JitThrowPc::Unknown;
    }
    let pc = athrow_bci as usize;
    // `cached.code` carries 2 bytes of speculative-read padding.
    let code_len = cached.code.len().saturating_sub(2);
    if pc >= code_len {
        return JitThrowPc::Unknown;
    }
    match cratonvm_reader::verified_code(&cached.code[..code_len]) {
        Ok(verified) if verified.is_instruction_start(pc) => {}
        _ => return JitThrowPc::Unknown,
    }
    // Widening: u16 -> usize (non-negative, fits)
    let in_a_protected_range = cached
        .exception_table
        .iter()
        .any(|e| pc >= e.start_pc as usize && pc < e.end_pc as usize);
    if in_a_protected_range {
        JitThrowPc::InRange(pc)
    } else {
        JitThrowPc::OutsideAllRanges(pc)
    }
}

/// Interpret `sig.athrow_bci` as a throw pc for `cached`, or `usize::MAX`.
///
/// The signal carries no method identity (see `JitSignals::athrow_bci`), so an
/// exception athrown by a compiled callee and propagated outward arrives here
/// still carrying the CALLEE's bci. Range-checking this method's exception
/// table against a foreign pc silently skips its handler: measured on
/// `JitPreciseHandlerFrame.plainStep`, whose protected range is [0,4),
/// receiving `maybeThrow`'s athrow at bci 13 — `handler_pc=None`, and its own
/// `catch (Boom)` never ran (5,619 of 20,000 iterations).
///
/// Accept the bci when it indexes an instruction boundary in THIS method's
/// code. Anything else falls back to the "throw pc unknown" sentinel, which is
/// the pre-RBC.6 behaviour: typed handlers still match by exception class, and
/// only a narrow catch-all is skipped.
///
/// **The boundary test replaced a `code[pc] == 0xbf` (athrow) test.** That
/// opcode test dated from when a local `athrow` was the ONLY site that stamped
/// a bci. `66548471f` then widened the PRODUCER — `emit_exception_check_stub`
/// now emits one pad per distinct throw-site bci, each calling
/// `JitRuntimeHelpers::set_throw_bci`, across all 19
/// `emit_post_invoke_exception_check` sites plus `emit_post_alloc_oom_check` —
/// but left this consumer still asserting "must be a literal athrow". The two
/// halves then disagreed about what `athrow_bci` means, and every stamped
/// INVOKE bci was thrown away here.
///
/// The observable cost was the whole `finally` family coming back:
/// `FinallyBalanceProbe`'s `guarded` stamps bci 9 (its `invokestatic work`),
/// this function rejected it because `code[9] != 0xbf`,
/// `find_jit_exception_handler` took its `pc_unknown` path, and that path
/// deliberately skips a catch-all whose region does not span the whole method —
/// which is exactly a javac `finally`. Result: `handler_pc=None`, the `finally`
/// never ran, and `CallPathProbe` leaked on every dispatch route.
///
/// The foreign-bci filter that the opcode test used to provide is replaced by a
/// STRICTLY BETTER one: the pc must fall inside one of this method's own
/// protected ranges (plus be a real instruction boundary). `athrow_bci` carries
/// no method identity, so a callee's stamp can still be standing here — see the
/// range check below and `test_compiled_callee_catches_its_own_athrow`, which
/// pins exactly that case. Do not weaken it to a boundary test alone: a foreign
/// bci is usually a valid boundary in this method too.
///
/// (A compiled method that exits through a precise-frame deopt stub instead
/// publishes an exceptional frame, and `route_jit_signal_exception` prefers
/// that — it is method-checked by `deopt_frame_matches_method` — over this
/// fallback entirely.)
///
/// No production caller since interpreter round i1 wave 5 (L7): its last one,
/// `execute`'s first-call door, now passes an `OutsideAllRanges` stamp
/// literally instead of folding it into `usize::MAX` here (the swallow
/// [`JitThrowPc::OutsideAllRanges`] documents). Kept for its documentation of
/// the stamp's meaning; new code should ask [`jit_local_athrow_pc_kind`].
#[allow(dead_code)]
pub(super) fn jit_local_athrow_pc(cached: &CachedBytecodeMethod, athrow_bci: i64) -> usize {
    match jit_local_athrow_pc_kind(cached, athrow_bci) {
        JitThrowPc::InRange(pc) => pc,
        _ => usize::MAX,
    }
}

/// The retired body of [`jit_local_athrow_pc`], kept as documentation of the
/// two tests [`jit_local_athrow_pc_kind`] performs and why.
#[allow(dead_code)]
fn jit_local_athrow_pc_rationale(cached: &CachedBytecodeMethod, athrow_bci: i64) -> usize {
    if athrow_bci < 0 {
        return usize::MAX;
    }
    let pc = athrow_bci as usize;
    // `cached.code` carries 2 bytes of speculative-read padding.
    let code_len = cached.code.len().saturating_sub(2);
    if pc >= code_len {
        return usize::MAX;
    }
    // The bci must land inside one of THIS method's protected ranges.
    //
    // This is what replaces the old opcode test as the foreign-bci filter, and
    // it is a far better one. `JitSignals::athrow_bci` carries no method
    // identity, so a callee's stamp can still be standing when this method's
    // drain runs — `JitPreciseHandlerFrame.plainStep` (protected range [0,4))
    // receives its callee `maybeThrow`'s athrow bci 13, and
    // `test_compiled_callee_catches_its_own_athrow` exists for exactly that.
    // 13 is a perfectly valid instruction boundary in `plainStep` too, so a
    // boundary test alone accepts it; the range test rejects it and falls back
    // to the pc-unknown sentinel, where a TYPED handler still matches by
    // exception class and the `catch (Boom)` runs.
    //
    // Rejecting an out-of-range pc costs nothing even when the pc is genuine:
    // `find_jit_exception_handler` would find no covering entry for it anyway,
    // and the one case the pc-unknown path still honours — a catch-all spanning
    // the whole method — covers every pc by definition, so honouring it there
    // is correct.
    let in_a_protected_range = cached.exception_table.iter().any(|e| {
        // Widening: u16 -> usize (non-negative, fits)
        pc >= e.start_pc as usize && pc < e.end_pc as usize
    });
    if !in_a_protected_range {
        return usize::MAX;
    }
    // `verified_code` is the process-shared, cached decode already used by the
    // compiler frontends, so this is a hash lookup rather than a re-decode.
    match cratonvm_reader::verified_code(&cached.code[..code_len]) {
        Ok(verified) if verified.is_instruction_start(pc) => pc,
        _ => usize::MAX,
    }
}

/// Drop a stashed pending-exception frame, but ONLY if it names this method.
///
/// The sinks that call this handle a JIT-raised exception without going through
/// `route_jit_signal_exception`, so a frame naming THEIR method can never be
/// used and would otherwise linger for a later invocation to mis-claim. A frame
/// naming a *callee*, though, is still in flight: an OSR bail re-stashes the
/// exception and a later drain routes it through that callee's own table, which
/// is precisely where the frame belongs. Dropping it there made a compiled
/// callee's handler read its non-parameter locals as null.
pub(super) fn drop_own_exceptional_frame(class_name: &str, method_name: &str, descriptor: &str) {
    if let Some((frame, cause, point_addr)) =
        cratonvm_jit::deopt::take_exceptional_frame_with_point()
    {
        if !deopt_frame_matches_method(&frame, class_name, method_name, descriptor) {
            // With its point, so the callee's own drain can match by body.
            cratonvm_jit::deopt::restash_exceptional_frame_with_point(frame, cause, point_addr);
        }
    }
}

/// Find the exception-table entry of `cached` that catches the throwable in
/// `thread.native_pin_roots[pin_slot]` thrown at `throw_pc`.
///
/// Extracted from `route_jit_exception_through_method` so the JIT-to-JIT
/// dispatch path (`vm/src/jit/helpers.rs::route_implicit_exc_through_callee`)
/// can run a compiled callee's own handler without re-executing the callee
/// from its entry — see `run_jit_callee_handler`.
///
/// When the throw PC is unknown (`usize::MAX`, sentinel), range membership
/// cannot be verified. A catch-all (`catch_type == 0`) is honoured only when
/// its protected region spans the whole method (`start_pc == 0 && end_pc >=
/// code_len`) — it then covers the (unknown) throw site, so running its
/// `finally` / monitor-exit cleanup is sound. Narrower catch-all regions are
/// skipped (they could catch an out-of-region exception); typed handlers still
/// match on exception class — wrong-type exceptions cannot be silently
/// swallowed. With a known pc every row is range-checked; without that check
/// the first catch-all entry would swallow exceptions thrown anywhere in the
/// method (the original bug here).
///
/// Catch types resolve as in the interpreter unwinder, and a resolution error
/// replaces the throwable in the pin slot — see [`search_compiled_frame_rows`],
/// also for `may_collect`. The caller re-reads the slot for whatever it
/// resumes or propagates.
pub(crate) fn find_jit_exception_handler(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    throw_pc: usize,
    pin_slot: usize,
    may_collect: bool,
    route: &'static str,
) -> CompiledHandlerSearch {
    let site = CompiledThrowSite::of(throw_pc);
    if cached.exception_table.is_empty() {
        return CompiledHandlerSearch::NotFound {
            definite: site != CompiledThrowSite::Unknown,
        };
    }
    // `cached.code` is padded with 2 trailing bytes for speculative reads;
    // the real bytecode length is `len() - 2`.
    let code_len = cached.code.len().saturating_sub(2);
    search_compiled_frame_rows(
        shared,
        thread,
        cached.declaring_class_id,
        &cached.exception_table,
        code_len,
        site,
        pin_slot,
        may_collect,
        route,
    )
}

/// Claim the stashed reason-9 exceptional frame if it names `cached`, mapping
/// it to `(throw bci, locals)`.
///
/// A frame naming a DIFFERENT method is re-stashed, not dropped: unlike
/// `route_jit_signal_exception` (the outermost drain, where a foreign frame's
/// owner is provably gone), this sink runs while the compiled CALLER is still
/// on the stack and will drain later — a frame belonging to it is still in
/// flight. An unmappable frame is consumed and reported as absent, so the
/// caller fails closed rather than resuming on values it could not rebuild.
///
/// `incoming_args` repairs slot 0 of an instance method. The snapshot records
/// `this` as `Undefined` whenever the bytecode has no further *read* of it —
/// which is the common case, and is exactly what the real
/// `BindConverter.convert` frame does (`getfield delegates` at bci 3 is its last
/// use, so local 0 is dropped from bci 4 on). Liveness is the right answer for a
/// bytecode read; it is the wrong answer for the receiver, which the VM itself
/// still needs for a `synchronized` method's monitor and for stack traces. The
/// caller passed the genuine receiver in, so put it back.
///
/// `compiled_locks` receives the pins of the locks a CLAIMED frame's compiled
/// code took (interpreter round i1 wave 24, lane L2), whether or not the frame
/// could be mapped: the claimed activation is resumed at a handler or
/// abandoned, and the caller decides which (see `CompiledLocksOfAStash`). It is
/// left `None` when no frame of this method was claimed.
///
/// JIT round 17 wave 8, lane syncdoor5 (STC17-3 of
/// `jit-r17-stashcap-proposals-RETIRED-20261006.md`): the take releases the holds of the frames
/// it discards from beneath its own, but only of those at or above the
/// innermost JIT door's entry floor
/// ([`super::deopt_resume::CompiledLocksOfAStash::take_exceptional_frame_releasing_discards_above`]);
/// with no floor recorded, or off the exceptional hand-over's switches, it is
/// the plain take.
pub(super) fn precise_handler_frame_for(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    incoming_args: &[Value],
    compiled_locks: &mut Option<super::deopt_resume::CompiledLocksOfAStash>,
) -> Option<(usize, PreciseHandlerLocals)> {
    if params_only_callee_handler_frames() {
        return None;
    }
    // Name-keyed: the dispatch helpers that reach here resolved `cached` by
    // name and do not hold the body that threw. The point is put back with a
    // refused frame so the sink that does hold it can still match by body.
    let taken = match crate::jit::conservative_roots::exceptional_stash_floor() {
        Some(floor) => {
            super::deopt_resume::CompiledLocksOfAStash::take_exceptional_frame_releasing_discards_above(
                shared,
                thread,
                floor,
                "callee handler claim",
            )
        }
        None => cratonvm_jit::deopt::take_exceptional_frame_with_point(),
    };
    let (rframe, cause, point_addr) = taken?;
    if !deopt_frame_matches_method(
        &rframe,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(rframe, cause, point_addr);
        return None;
    }
    // JIT round 17 wave 2, lane syncdoor2 (SD17-1): naming the method monitor
    // a self-locking body handed over with this frame, which the handler frame
    // then adopts (`run_jit_callee_handler`).
    *compiled_locks = Some(
        super::deopt_resume::CompiledLocksOfAStash::pin_compiled_locks_of_method(
            shared,
            thread,
            &rframe,
            cached.declaring_class_id,
            cached.is_synchronized,
            cached.is_static,
            &cached.code,
        ),
    );
    let bci = rframe.bci as usize;
    // Held back for late resolution, exactly as `route_jit_signal_exception`
    // holds one back: rebuilding a scalar-replaced object allocates, and the
    // rebuilt locals must not be separated from the frame that roots them by
    // anything that can collect. The `this` fixup below is re-applied by the
    // caller after it resolves the frame.
    // Same `!is_synchronized` guard as `route_jit_signal_exception`'s, for the
    // same two reasons — see the comment there.
    if !cached.is_synchronized && precise_frame_needs_materialization(&rframe) {
        return Some((bci, PreciseHandlerLocals::Deferred(Box::new(rframe))));
    }
    let mut locals = ir_deopt_locals(&rframe.locals)?;
    if handler_sink_genuine_receiver_enabled_for(shared) {
        // Round 14 wave 1 (lane sync,
        // `r14w1-sync-handler-sinks-conflate-the-method-monitor-with-local-0-FIXED-20260929.md`):
        // only a DEAD slot 0 is the receiver's to fill; a live one is what the
        // bytecode stored there (`astore_0`), which the handler may read. The
        // method monitor no longer comes from here
        // (`acquire_method_monitor_for_handler`).
        restore_dead_receiver(&mut locals, &rframe.locals, cached.is_static, incoming_args);
    } else if !cached.is_static {
        if let (Some(slot0), Some(receiver)) = (locals.first_mut(), incoming_args.first()) {
            *slot0 = *receiver;
        }
    }
    Some((bci, PreciseHandlerLocals::Mapped(locals)))
}

/// A claimed reason-9 frame in one of the two states it can be in before the
/// handler is known.
///
/// The split exists because materialization ALLOCATES. Mapping a frame that
/// names nothing the compiler deleted is a pure read and can happen as early as
/// the frame is claimed; rebuilding one that does has to happen last, with no
/// GC-capable step between it and `Frame::new_pooled`.
pub(super) enum PreciseHandlerLocals {
    /// Mapped on sight: no scalar-replaced object, no elided lock.
    Mapped(Vec<Value>),
    /// Held back for `materialize_and_relock_precise_frame`.
    Deferred(Box<cratonvm_jit::deopt::ReconstructedFrame>),
}

/// Does this reason-9 frame name anything the interpreter cannot resume on as
/// it stands — a scalar-replaced object, or a lock the compiled body elided?
///
/// Both are the single-pass backend's scalar replacement showing through.
/// Before 2026-09-22 neither could occur on this route: `x64/driver.rs` handed
/// `plan_scalar_replacement` the EMPTY set whenever `precise_exception_frames`
/// was set, and a reason-9 frame is only ever published when it is — so the two
/// features were disjoint by construction and every consumer here could map
/// locals with `ir_deopt_locals` alone. Admitting scalar replacement under
/// precise frames is what makes this question have a `true` answer.
///
/// The monitor half is not optional. An elided `monitorenter` was never
/// executed, so a handler resumed in the interpreter would run the matching
/// `monitorexit` against a lock nobody entered — `IllegalMonitorStateException`
/// instead of the rethrow javac's monitor handler exists to perform.
pub(super) fn precise_frame_needs_materialization(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    use cratonvm_jit::deopt::FrameValue;
    let virtual_slot = |v: &FrameValue| {
        matches!(
            v,
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
        )
    };
    rframe.locals.iter().any(virtual_slot)
        || rframe.stack.iter().any(virtual_slot)
        || rframe.monitors.iter().any(|m| virtual_slot(&m.object))
        || rframe.monitors.iter().any(|m| m.relock)
}

/// Rebuild a reason-9 frame's scalar-replaced objects on the heap, re-acquire
/// every lock the compiled body elided, and map the result to interpreter
/// locals.
///
/// # This must be the LAST thing before the handler frame is built
///
/// It allocates, so it is a GC point, and it leaves its shells pinned
/// (`keep_pins = true`) for the caller to release only once the resumed frame
/// roots them. `materialize_virtual_objects` also pins and re-reads every
/// ORDINARY reference the frame holds, so a collection during shell allocation
/// cannot stale the locals either. What none of that survives is a GC-capable
/// step run BETWEEN this call and `Frame::new_pooled` — the returned `Vec` is a
/// detached snapshot and nothing would rewrite it. Both callers therefore call
/// this after their handler search and after any monitor acquire, with nothing
/// but the pool refill in between.
///
/// This is the same protocol `deopt_resume::build_deopt_frame_inner` follows
/// for a guard deopt, and the relock loop below is its counterpart: an elided
/// lock is re-entered `lock_depth` times so the resumed frame's own
/// `monitorexit` (and any nested exits) balance through the MonitorTable. A
/// monitor with `relock == false` is one the compiled code really acquired;
/// this thread still holds it, and entering again would leave it one level too
/// deep after the handler exits.
///
/// Returns `None` when the frame cannot be resolved, and releases its own pins
/// on that path. The caller then does what it did before precise frames could
/// carry a virtual object: propagate, or decline to the whole-method re-run.
/// Neither is right for a method whose handler should have caught, which is why
/// the JIT side refuses to scalar-replace under precise frames in the shapes
/// this can fail on rather than relying on the fallback (see
/// `x64/driver.rs`'s `sr_admitted_under_precise_frames`).
pub(super) fn materialize_and_relock_precise_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<Vec<Value>> {
    use cratonvm_jit::deopt::FrameValue;

    // The same refusal `build_deopt_frame_inner` makes, for the same reason: an
    // `ACC_SYNCHRONIZED` method's monitor is taken by the invoke path, not by a
    // `monitorenter`, so it is not a frame-state entry at all and an elision of
    // it under scalar replacement of the receiver would leave no trace here.
    if cached.is_synchronized {
        return None;
    }

    let pin_base = thread.native_pin_roots.len();
    let mut copy = rframe.clone();
    if crate::runtime::deopt_materialize::materialize_virtual_objects(
        shared, thread, &mut copy, /* stress_gc */ false, /* keep_pins */ true,
    )
    .is_err()
    {
        thread.native_pin_roots.truncate(pin_base);
        return None;
    }

    let Some(locals) = ir_deopt_locals(&copy.locals) else {
        thread.native_pin_roots.truncate(pin_base);
        return None;
    };

    // Re-acquire the elided locks. Uncontended by construction: an object whose
    // lock the compiler removed never escaped this thread, so nothing else can
    // hold it and `monitors.enter` cannot block or GC.
    //
    // Validated in full BEFORE anything is entered (interpreter round i4 wave
    // 98, lane L2): the refusal below used to sit inside the relock loop, so a
    // malformed entry after a well-formed one returned `None` with the earlier
    // monitors already re-entered and nothing left to exit them -- a lock this
    // thread held forever, which hangs the next thread to synchronize on it.
    let mut relocks: Vec<(ObjectRef, u32)> = Vec::new();
    for m in &copy.monitors {
        if !m.relock {
            continue;
        }
        let FrameValue::Object(addr) = &m.object else {
            // The materializer rewrites every virtual monitor object to
            // `Object`, so a non-`Object` here is a malformed frame. Refusing
            // beats relocking a bogus address, which would corrupt the monitor
            // table for every later user of it.
            thread.native_pin_roots.truncate(pin_base);
            return None;
        };
        let Some(obj) =
            (*addr != 0).then(|| unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) })
        else {
            thread.native_pin_roots.truncate(pin_base);
            return None;
        };
        relocks.push((obj, m.lock_depth));
    }
    for (obj, lock_depth) in relocks {
        for _ in 0..lock_depth {
            shared.threads.monitors.enter(obj, thread.thread_id);
        }
        // Publish the relocked monitor for `getLockedMonitors()`, as the deopt
        // resume does (r11w5-sync-deopt-relock-publish-patch).
        if lock_depth > 0 {
            shared
                .threads
                .thread_registry
                .complete_jmx_monitor_enter(thread.thread_id, obj);
        }
    }
    Some(locals)
}

/// A/B opt-out (`CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME=1`): restore the
/// pre-2026-08-01 `run_jit_callee_handler`, which resumed a compiled callee's
/// handler on `this`-plus-parameters and left every other local zeroed.
///
/// It exists so one binary can demonstrate the defect and its fix:
/// `JitPreciseHandlerFrame.loopMismatches` reports 19497 of 20000 with this set
/// and 0 without it. It restores the old behaviour in full, fail-closed
/// branch included, because a decline is not what the old code did and an A/B
/// that silently substitutes a third behaviour proves nothing. This is a
/// wrong-answer switch, not a tuning knob — nothing but a differential run
/// should ever set it.
pub(super) fn params_only_callee_handler_frames() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME")
            .is_some()
    })
}

/// JIT round 17 wave 2, lane syncdoor2 (SD17-1): does a sink that drops a
/// FOREIGN exceptional frame (its owner has unwound) release the holds the
/// frame names? Exactly when the JIT may hand a self-locking body's method
/// monitor over with such a frame: `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS=1`
/// and `CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER` not `0`
/// (`cratonvm_jit` `self_lock_exc_handover_enabled`). Read only on that cold
/// arm, not memoised.
///
/// JIT round 17 wave 3 (lane syncdoor3): the one gate every exceptional-frame
/// drop site now shares
/// ([`super::deopt_resume::CompiledLocksOfAStash::dropped_frame_holds_released`]).
fn foreign_frame_hold_released_on_drop() -> bool {
    super::deopt_resume::CompiledLocksOfAStash::dropped_frame_holds_released()
}

/// `CRATONVM_JIT_HANDLER_SINK_GENUINE_RECEIVER` (round 14 wave 1, lane sync;
/// **default ON**, `0` restores the old reads; both modes): the compiled-handler
/// sinks keep a synchronized method's MONITOR and its handler frame's LOCAL 0
/// apart. JVMS 2.11.10: the monitor is the object the method was invoked on,
/// whatever the bytecode later stores into local 0 (`astore_0`, legal in any
/// instance method but `<init>`). Before, `route_jit_exception_through_method`
/// locked local 0 of the handler frame (another object, or `null` and an
/// `InternalError`), and `precise_handler_frame_for` overwrote a live local 0
/// with the receiver (a silent wrong answer in the handler).
/// `r14w1-sync-handler-sinks-conflate-the-method-monitor-with-local-0-FIXED-20260929.md`.
/// Read only on a compiled-handler route (an exception a compiled body's own
/// handler catches), never per call.
fn handler_sink_genuine_receiver_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_HANDLER_SINK_GENUINE_RECEIVER")
}

/// [`handler_sink_genuine_receiver_enabled`] latched once per VM (nio21 wave
/// 16, lane L5), for the two sinks that ask on every caught throw
/// ([`precise_handler_frame_for`], [`run_jit_callee_handler`]).
fn handler_sink_genuine_receiver_enabled_for(shared: &SharedVm) -> bool {
    shared
        .threads
        .throwable_layout_cache
        .handler_sink_genuine_receiver_enabled()
}

/// Pin the INVOCATION's receiver of a synchronized instance method for
/// [`acquire_method_monitor_for_handler`], or `None` (static, unsynchronized,
/// no object receiver, or the switch off). The pin is released by the
/// caller's `truncate(pin_base)`, below which it is pushed.
fn pin_genuine_receiver(
    thread: &mut JvmThread,
    cached: &CachedBytecodeMethod,
    receiver: Option<Value>,
) -> Option<usize> {
    if !cached.is_synchronized || cached.is_static || !handler_sink_genuine_receiver_enabled() {
        return None;
    }
    match receiver {
        Some(Value::Object(Some(obj))) => {
            let slot = thread.native_pin_roots.len();
            thread.native_pin_roots.push(obj);
            Some(slot)
        }
        _ => None,
    }
}

/// `JitSynchronizedMonitorGuard::acquire` for a handler frame whose locals
/// are `args`, locking the receiver pinned at `receiver_pin`
/// ([`pin_genuine_receiver`]) instead of `args[0]`, then putting the frame's
/// own local 0 back: from its pin at `args_pin_base` (the first pin
/// `pin_object_args` pushed for `args`' shape, current across the acquire's
/// possible collection) when it is an object, as it was otherwise. Without a
/// receiver pin this is exactly `acquire(.., args)`.
fn acquire_method_monitor_for_handler(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &CachedBytecodeMethod,
    args: &mut [Value],
    receiver_pin: Option<usize>,
    args_pin_base: usize,
) -> Result<JitSynchronizedMonitorGuard, MethodCallFailed> {
    let Some(pin) = receiver_pin else {
        return JitSynchronizedMonitorGuard::acquire(shared, thread, cached, args);
    };
    let own_slot0 = args.first().copied();
    if let (Some(slot0), Some(receiver)) =
        (args.first_mut(), thread.native_pin_roots.get(pin).copied())
    {
        *slot0 = Value::Object(Some(receiver));
    }
    let acquired = JitSynchronizedMonitorGuard::acquire(shared, thread, cached, args);
    if let (Some(slot0), Some(own)) = (args.first_mut(), own_slot0) {
        *slot0 = match own {
            Value::Object(Some(stale)) => Value::Object(Some(
                thread
                    .native_pin_roots
                    .get(args_pin_base)
                    .copied()
                    .unwrap_or(stale),
            )),
            other => other,
        };
    }
    acquired
}

/// Would resuming one of `cached`'s handlers on `this`-plus-parameters alone
/// invent values? See `cratonvm_jit::handler_resume_requires_precise_locals`.
///
/// nio21 wave 15 L4: remembered per thread for the record
/// (`ThrowableCaptureMemos::handler_precise_locals`,
/// `CRATONVM_HANDLER_PRECISE_LOCALS_MEMO`, default ON): the answer is a pure
/// function of the method's bytecode, and a caught-exception loop asked it on
/// every throw.
pub(super) fn handler_resume_needs_precise_locals(
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
) -> bool {
    thread
        .throwable_capture_memo
        .handler_precise_locals(cached, || {
            handler_resume_needs_precise_locals_uncached(cached)
        })
}

/// [`handler_resume_needs_precise_locals`]'s computation.
fn handler_resume_needs_precise_locals_uncached(cached: &CachedBytecodeMethod) -> bool {
    // `cached.code` carries 2 bytes of speculative-read padding.
    let code_len = cached.code.len().saturating_sub(2);
    cratonvm_jit::handler_resume_requires_precise_locals(
        &cached.code,
        code_len,
        &cached.exception_table,
        &cached.method_descriptor,
        cached.is_static,
    )
}

/// Why [`run_jit_callee_handler`] did not resume a handler.
///
/// The two are NOT interchangeable, and treating them as one is what let a
/// callee's exception disappear. `NotCaught` means the callee's own exception
/// table does not cover the throw at all — the JVM answer is to propagate to
/// the caller, and re-running the callee from its entry is both wrong and
/// unsound for any callee whose pre-throw prefix has side effects.
/// `Declined` means a handler DID match but could not be resumed with correct
/// locals, where a whole-method re-run is at least a defensible fallback.
/// `Unknown` means no handler matched, but the lookup ran without a throw pc,
/// and the pc-unknown search deliberately under-reports (it skips every
/// catch-all whose region does not span the whole method, i.e. every javac
/// `finally`), so "no handler" there only means "cannot tell".
///
/// The trust decision lives HERE, with the pc the lookup actually used, and not
/// with the caller. The caller used to decide it from the `athrow_bci` stamp it
/// passed in -- but a callee compiled with precise exception frames exits a
/// protected throw site through its reason-9 stub, which publishes a frame
/// instead of stamping a bci. The stamp is then always unknown even though the
/// lookup below ran against the frame's exact bci, so a definite `NotCaught`
/// was read as "cannot tell" and the callee was re-run from its entry.
/// netty `HttpPostMultipartRequestDecoder.findMultipartDisposition` is the
/// witness: its `catch (NullPointerException | IllegalArgumentException)` does
/// not cover the `ErrorDataDecoderException` its callee throws, and the re-run
/// re-created `currentFieldAttributes` over an already-consumed buffer, took
/// the "no filename" branch and swallowed the exception outright
/// (`HttpPostRequestDecoderTest.
/// testDecodeMalformedBadCharsetContentDispositionFieldParameters`).
///
/// [`run_jit_callee_handler`] returns the miss TOGETHER with the throwable the
/// caller must propagate or restore: the handler search may replace it (a catch
/// type that fails to resolve, JVMS §5.4.3 — see [`search_compiled_frame_rows`])
/// and may collect, so the caller's own copy is neither the right object nor
/// necessarily a valid address any more.
pub(crate) enum CalleeHandlerMiss {
    /// No handler in the callee covers this throw site, decided against a
    /// KNOWN throw pc -- the stamped one or a precise frame's bci.
    NotCaught,
    /// A handler matched; resuming it would have needed locals nobody published.
    Declined,
    /// No handler matched, but the throw pc was unknown, so that is not proof.
    Unknown,
}

/// gen r4w3/rooting: push every non-null object entry of `args` onto
/// `thread.native_pin_roots`, in order. The collector remaps those slots in
/// place; pair with [`refresh_args_from_pins`] to read the current addresses
/// back. The caller owns the watermark (truncate to the base it recorded).
fn pin_object_args(thread: &mut JvmThread, args: &[Value]) {
    for value in args {
        if let Value::Object(Some(obj)) = value {
            thread.native_pin_roots.push(*obj);
        }
    }
}

/// gen r4w3/rooting: overwrite each non-null object entry of `args` with the
/// pin [`pin_object_args`] pushed for it, starting at `first_pin`. `args` must
/// have the same object/non-object shape as the slice that was pinned.
fn refresh_args_from_pins(thread: &JvmThread, first_pin: usize, args: &mut [Value]) {
    let mut pin = first_pin;
    for value in args.iter_mut() {
        if matches!(value, Value::Object(Some(_))) {
            if let Some(current) = thread.native_pin_roots.get(pin).copied() {
                *value = Value::Object(Some(current));
            }
            pin += 1;
        }
    }
}

/// JIT round 17 wave 8, lane syncdoor5
/// (`r17-self-locking-handlers-refuse-recursive-exceptional-resume-FIXED-20261005.md`):
/// may the dispatch helpers' callee-handler template (`helpers.rs`
/// `resolve_callee_uncached`, behind `try_run_callee_handler` and
/// `implicit_throw_is_uncovered`) resolve an `ACC_SYNCHRONIZED` callee, so
/// that its escaping exception reaches [`run_jit_callee_handler`]?
///
/// Exactly under `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS` (default ON since the round-18 close).
/// A raw compiled entry of a synchronized method is only ever called when its
/// body is SELF-LOCKING (the doors skip a body with `requires_wrapped_entry`),
/// and a self-locking body with an exception table is published only under
/// that switch. Off, no synchronized callee with a table reaches these doors,
/// and the template keeps refusing every synchronized method as before.
///
/// Why the refusal was wrong under the switch: the template's `None` made
/// `try_run_callee_handler` answer `Declined` without consulting the table or
/// claiming the callee's precise frame. `route_implicit_exc_through_callee`
/// then judged a re-run from entry, found it unsound (the throwing sub-call
/// runs again) and raised `InternalError: precise deoptimization unavailable
/// ... (call-site service: the callee's frame could not be resumed); refusing
/// side-effecting replay` for every exception that merely PROPAGATES out of a
/// self-locking body (`R17StashCap`, `R17SyncDoor4`). [`run_jit_callee_handler`]
/// already serves a synchronized method: it adopts a handed-over method
/// monitor ([`super::deopt_resume::CompiledLocksOfAStash::hands_over_method_monitor`]),
/// acquires one for a catch otherwise (`acquire_method_monitor_for_handler`),
/// and releases the handed hold once on a miss. Read on the cold resolve only.
pub(crate) fn synchronized_callee_handlers_served() -> bool {
    // JIT round 18 wave 9 (lane syncdoor6): the JIT's own reader, so a default
    // flip of the switch moves this door with the admission.
    cratonvm_jit::self_locking_sync_handlers_enabled()
}

/// Run a compiled callee's own exception handler in the interpreter, resuming
/// AT the handler rather than re-executing the method from its entry.
///
/// The JIT-to-JIT dispatch path used to answer a compiled callee's escaping
/// exception with `bail_to_interpreter`, i.e. a full re-run. That is only
/// sound for a callee whose pre-throw prefix has no observable side effects —
/// which a `try { counter++; mayThrow(); } finally { counter--; }` plainly
/// does not: the compiled attempt already ran `counter++` and skipped the
/// `finally`, and the re-run then adds a second balanced pass, leaking one
/// count per throw. `FinallyShapeProbe`/`CallPathProbe` are the witnesses
/// (`docs/known-issues/repros/jitban-remaining-20260726/`).
///
/// Resuming at the handler keeps the compiled prefix's single execution and
/// runs only the cleanup the compiled body skipped.
///
/// Locals come from the reason-9 exceptional frame the compiled body published
/// at its throw site when one is stashed for THIS method, and otherwise from
/// the callee's incoming arguments — the same two-tier choice
/// `route_jit_signal_exception` makes, and for the same reason.
///
/// **The params-only tier was the whole story here until 2026-08-01, and that
/// was a silent miscompile.** Its stated justification was that "a compiled
/// method whose handler reads a local first assigned inside the try never
/// passes the `local_handler_reads_unsafe_local` compile gate" — true when it
/// was written, false since `precise_handler_frames_enabled` began admitting
/// exactly that population on the promise that every throwing site publishes a
/// precise frame. `route_jit_signal_exception` kept that promise; this sink did
/// not, so a compiled callee that threw inside its own protected range resumed
/// its handler with every non-parameter local zeroed.
///
/// The witness is Spring Boot's `BindConverter.convert(Object, TypeDescriptor,
/// TypeDescriptor)`: `for (ConversionService d : this.delegates)` keeps the
/// `Iterator` in local 5, a `canConvert` inside the loop's `try` throws
/// `ConversionException`, and the handler falls through to the loop head. With
/// params-only locals the iterator resumed as null and the next `hasNext()`
/// threw "Cannot invoke java.util.Iterator.hasNext() because <local5> is null"
/// — 27 of 43 `LiquibaseAutoConfigurationTests` methods, deterministically, and
/// clean under `--nojit`. `JitPreciseHandlerFrame.loopStep`
/// (`test_compiled_callee_handler_resume_keeps_the_loop_iterator`) pins the
/// shape.
///
/// Returns `Err` when no handler in `cached` covers `throw_pc`, leaving the
/// caller to propagate the exception — and also when this method needs precise
/// locals but no frame is stashed for it, where resuming would mean inventing
/// them. The `Err` carries the throwable to propagate or restore, re-read from
/// this function's pin: see [`CalleeHandlerMiss`].
///
/// `probes/BindConverterJitProbe.java` is a second, independent witness for the
/// same defect, arrived at from `DevToolsPooledDataSourceAutoConfigurationTests
/// .inMemoryDerbyIsShutdown`: it drives the real `BindConverter.convert` and
/// counts 199,491 wrong results in 200,000 calls before this fix, 0 after. Its
/// `refuse` mode takes the handler out of the picture and passes, which is what
/// identifies the handler resume as the mechanism.
pub(crate) fn run_jit_callee_handler(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    throw_pc: usize,
    exc: ObjectRef,
    incoming_args: &[Value],
) -> Result<MethodCallResult, (CalleeHandlerMiss, ObjectRef)> {
    // Interpreter round i1 wave 24, lane L2: when this callee's own reason-9
    // frame is claimed, the locks its compiled code took are pinned until this
    // function decides — handed to the handler frame it resumes, or released
    // on every arm that does not resume one (the caller then propagates the
    // exception or re-runs the callee from entry, and either way this
    // activation is gone). See `CompiledLocksOfAStash`.
    let mut claimed_locks = None;
    let mut precise =
        precise_handler_frame_for(shared, thread, cached, incoming_args, &mut claimed_locks);
    let compiled_locks = std::cell::Cell::new(claimed_locks);
    // A precise frame's bci is the compiled body's own throw site, recorded by
    // the reason-9 stub. It is strictly better than the `athrow_bci` stamp
    // `throw_pc` comes from (which carries no method identity), so prefer it
    // for the handler's `[start_pc, end_pc)` range test.
    let (throw_pc, precise_locals, mut deferred_precise) = match precise.as_mut() {
        Some((bci, PreciseHandlerLocals::Mapped(locals))) => (*bci, Some(locals.as_slice()), None),
        // A deferred frame counts as "a precise frame was published" for every
        // decision below — that is what the `Some` is asked. It just cannot be
        // READ until the handler is found. (Mutable so the handler search can
        // re-read its object addresses after a collection.)
        Some((bci, PreciseHandlerLocals::Deferred(rframe))) => (*bci, None, Some(&mut **rframe)),
        None => (throw_pc, None, None),
    };
    // An UNCOVERED stamped throw pc is taken literally here, and that is the
    // 2026-09-22 correction. It used to be downgraded to the `usize::MAX`
    // pc-unknown search whenever it named a self-call site of `cached` (and
    // a `trust_throw_pc` ABI bit was the narrower exemption from it), on the
    // argument that a self-recursive chain's one thread-global stamp cannot say
    // WHICH activation wrote it, so "outside every protected range" might be a
    // statement about the wrong activation.
    //
    // The downgrade's own escape hatch is what made it wrong: the pc-unknown
    // search skips a narrow catch-ALL but matches a TYPED handler on exception
    // class alone, IGNORING its protected range. So for a method with both a
    // covered and an uncovered self-call site it ran the `catch` for a throw at
    // a bci no `try` covers, in the wrong activation, with that activation's
    // locals. Measured on `RJitSelfRecUncovered.uncoveredEntry(4)` at 19,930
    // wrong of 20,000 calls, and on `recDeep(11, 4)` at ~8e-7 — the same
    // defect at two rates, because only the deep entry makes the interpreter
    // boundary land on an activation whose stamp is the uncovered site.
    //
    // What replaced it is not a weaker guard but a better-placed one: since
    // round 10 every compiled self-`CALL` runs this function once per
    // activation, immediately after the call — `emit_inline_callee_deopt_check`
    // on the single-pass door, `emit_inline_callee_deopt_service` on the IR
    // one, and `route_implicit_exc_through_callee` for the dispatched routes.
    // Each activation is therefore offered the exception with ITS OWN fresh
    // stamp, before any outer activation overwrites it, so an inner `catch`
    // that covers the real throw site is consulted where it lives — which is
    // what the downgrade was approximating from the outside.
    // `test_jit_self_recursive_activation_catches_its_own_callee_throw` (the
    // Groovy `hasUsableImplementation` shape the downgrade was written for) is
    // green without it, as are `GroovyMarkupViewTests` and
    // `ViewResolutionIntegrationTests` at 33/33. See
    // `docs/internal/retired/r10-selfrec-deep-handler-leaks-once-per-million-20260922-RETIRED-20260922.md`.

    // Root the throwable across the handler search (lazy catch-type load) and
    // the synchronized monitor acquire (may block): see
    // `route_jit_exception_through_method`. Every exit below truncates it.
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    // Round 14 wave 1 (lane sync): the invocation's receiver (this function's
    // `incoming_args[0]`, not the precise frame's local 0), the method
    // monitor's object, pinned across the search below.
    let receiver_pin = pin_genuine_receiver(thread, cached, incoming_args.first().copied());
    // Every miss hands back the PIN's throwable: the search may have replaced
    // it with a resolution error, and may have moved it.
    let miss = |thread: &mut JvmThread,
                why: CalleeHandlerMiss|
     -> Result<MethodCallResult, (CalleeHandlerMiss, ObjectRef)> {
        let exc = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        // No handler frame resumes the claimed activation.
        if let Some(locks) = compiled_locks.take() {
            locks.release_for_a_propagation(shared, thread);
        }
        Err((why, exc))
    };
    // Interpreter round i5 wave 105, lane L2 (page
    // `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
    // an EXPLICIT throwable that reached this callee service with no stamp and
    // no precise frame of the callee. Every compiled exit of a body stamps its
    // own bci before it returns the sentinel (the post-invoke pads, `athrow`,
    // the reason-9 stub since wave 104), so an unstamped one was re-posted by
    // the VM after the callee's bytecode had already run in the interpreter
    // (a resumed trapped frame, a not-entrant re-dispatch, a frameless re-run:
    // `handle_jit_dispatch_error`), whose unwinder searched this very table at
    // the real throw pc. It has LEFT the callee: propagate it. Searching again
    // with the pc unknown could only run a typed row the throw had already
    // left (the w105 battery's `L2W104LambdaImplHandlerRerun` line,
    // `capturedCaller`'s `catch` over `[10,26)` for a throw at bci 44).
    // `CRATONVM_JIT_UNSTAMPED_CALLEE_THROW_ESCAPES=0` restores that search.
    let throw_pc = if throw_pc == JIT_THROW_PC_UNSTAMPED_EXPLICIT {
        if unstamped_callee_throw_escapes_enabled() {
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] JITC-UNSTAMPED-ESCAPE callee service propagates an \
                     unstamped explicit throwable: {}.{}{}{}",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    crate::jit::helpers::jitc_census_backtrace()
                );
            }
            return miss(thread, CalleeHandlerMiss::NotCaught);
        }
        usize::MAX
    } else {
        throw_pc
    };
    // The locals the handler would resume on are raw `Value`s until the frame
    // below holds them, and a deferred frame's are raw addresses. Both are
    // pinned across the search, which may then resolve catch types for real
    // (and collect), and re-read after it.
    let (search, resume_args) = pinned_compiled_handler_search(
        thread,
        precise_locals.unwrap_or(incoming_args),
        deferred_precise.as_deref_mut(),
        |thread| {
            find_jit_exception_handler(
                shared,
                thread,
                cached,
                throw_pc,
                pin_base,
                true,
                "callee-handler",
            )
        },
    );
    // gen r4w3/rooting: the re-read args stay pinned directly above `exc`
    // (every `truncate(pin_base)` exit releases them) because materializing a
    // deferred frame below allocates again; its receiver fix-up reads the
    // first pin at `args_pin_base`.
    let args_pin_base = thread.native_pin_roots.len();
    pin_object_args(thread, &resume_args);
    let handler_pc = match search {
        CompiledHandlerSearch::Found(handler_pc) => handler_pc,
        CompiledHandlerSearch::NotFound { definite } => {
            return miss(
                thread,
                if definite {
                    CalleeHandlerMiss::NotCaught
                } else {
                    CalleeHandlerMiss::Unknown
                },
            );
        }
    };
    if precise_locals.is_none()
        && deferred_precise.is_none()
        && !params_only_callee_handler_frames()
        && handler_resume_needs_precise_locals(thread, cached)
    {
        // Fail closed. Every throwing opcode inside a protected range of such a
        // method is supposed to publish (`precise_exception_frame_sites_supported`),
        // so arriving here means the promise was broken somewhere; resuming the
        // handler now would hand it zeroed locals, which is a wrong answer with
        // no crash to trace it back from. Declining leaves the caller's existing
        // conservative behaviour (propagate, or the whole-method re-run) intact.
        if crate::jit::helpers::rbc6_dbg() {
            eprintln!(
                "[rbc6-dbg] run_jit_callee_handler DECLINED {}.{}{} throw_pc={} \
                 — handler needs precise locals and no frame was published",
                cached.class_name, cached.method_name, cached.method_descriptor, throw_pc as i64,
            );
        }
        return miss(thread, CalleeHandlerMiss::Declined);
    }
    let incoming_args: &[Value] = &resume_args;
    // JIT round 17 wave 2, lane syncdoor2 (SD17-1): a self-locking body's
    // precise exceptional exit kept the method monitor and the claimed frame
    // names it (`CompiledLocksOfAStash::hands_over_method_monitor`). The
    // handler frame adopts that hold as its `monitor_on_exit`
    // (`seed_frame_and_unpin` below), so nothing is acquired here: the thread
    // holds the monitor without a gap from the compiled prologue's enter to
    // the handler's exit, as on HotSpot. Re-acquiring after the stub's release
    // let another thread enter in between (the in-frame lookup DECLINED: a
    // JVMTI exception client, a catch type only a user loader resolves).
    let method_monitor_handed = {
        let locks = compiled_locks.take();
        let handed = locks
            .as_ref()
            .is_some_and(super::deopt_resume::CompiledLocksOfAStash::hands_over_method_monitor);
        compiled_locks.set(locks);
        handed
    };
    let mut synchronized_args =
        (cached.is_synchronized && !method_monitor_handed).then(|| incoming_args.to_vec());
    let synchronized_monitor = match synchronized_args.as_mut() {
        Some(args) => match acquire_method_monitor_for_handler(
            shared,
            thread,
            cached,
            args,
            receiver_pin,
            args_pin_base,
        ) {
            Ok(monitor) => Some(monitor),
            Err(error) => {
                // `acquire` pushes its own pin only on success.
                thread.native_pin_roots.truncate(pin_base);
                // The error leaves the claimed activation.
                if let Some(locks) = compiled_locks.take() {
                    locks.release_for_a_propagation(shared, thread);
                }
                return Ok(Err(error));
            }
        },
        None => None,
    };
    let incoming_args = synchronized_args.as_deref().unwrap_or(incoming_args);
    // ── THE DEFERRED PRECISE FRAME, resolved here and nowhere earlier ─────
    //
    // Same placement rule as the sibling sink: after the handler search and
    // after the synchronized acquire, both of which can collect, and before the
    // pool refill and the frame build, neither of which can. The shells stay
    // pinned until this function's `truncate(pin_base)` below, by which point
    // the frame owns every reference. See
    // `materialize_and_relock_precise_frame`.
    //
    // Interpreter round i6 wave 121, lane L6 (page
    // `i116-L6-a-compiled-unpaired-monitorexit-releases-its-callers-lock`,
    // "Wave 121"): read before the frame is consumed below.
    let elided_levels_retaken = elided_levels_retaken_by_materializer(deferred_precise.as_ref());
    let materialized_locals;
    let incoming_args = match deferred_precise {
        Some(rframe) => {
            // Round 14 wave 1 (lane sync, `handler_sink_genuine_receiver_enabled`):
            // as for a mapped frame, only a slot 0 the frame published DEAD is
            // the receiver's to fill; a live one keeps what the bytecode stored.
            let slot0_is_the_receivers = !handler_sink_genuine_receiver_enabled_for(shared)
                || matches!(
                    rframe.locals.first(),
                    Some(cratonvm_jit::deopt::FrameValue::Undefined)
                );
            match materialize_and_relock_precise_frame(shared, thread, cached, rframe) {
                Some(mut locals) => {
                    // The `this` fixup `precise_handler_frame_for` applies to a
                    // mapped frame, re-applied here for a deferred one: the
                    // caller's receiver is authoritative over whatever the
                    // compiled frame's slot 0 decoded to.
                    //
                    // gen r4w3/rooting: materializing allocates (can collect),
                    // so `incoming_args[0]` may name a vacated address by now.
                    // An object receiver is the FIRST pin at `args_pin_base`
                    // (it is `resume_args[0]`), which the collector kept current.
                    if !cached.is_static && slot0_is_the_receivers {
                        if let (Some(slot0), Some(receiver)) =
                            (locals.first_mut(), incoming_args.first())
                        {
                            *slot0 = match receiver {
                                Value::Object(Some(stale)) => Value::Object(Some(
                                    thread
                                        .native_pin_roots
                                        .get(args_pin_base)
                                        .copied()
                                        .unwrap_or(*stale),
                                )),
                                other => *other,
                            };
                        }
                    }
                    materialized_locals = locals;
                    &materialized_locals[..]
                }
                None => {
                    // Declining is what this sink already does for a frame it
                    // cannot resume on, and it leaves the caller's conservative
                    // behaviour intact.
                    if crate::jit::helpers::rbc6_dbg() {
                        eprintln!(
                            "[rbc6-dbg] run_jit_callee_handler MATERIALIZE-FAILED {}.{}{} bci={}",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            rframe.bci,
                        );
                    }
                    drop(synchronized_monitor);
                    return miss(thread, CalleeHandlerMiss::Declined);
                }
            }
        }
        None => incoming_args,
    };
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    let mut frame = crate::runtime::frame::Frame::new_pooled(
        cached.declaring_class_id,
        cached.class_name.clone(),
        cached.method_name.clone(),
        cached.method_descriptor.clone(),
        cached.source_file.clone(),
        cached.code.clone(),
        cached.exception_table.clone(),
        cached.max_stack,
        cached.max_locals,
        incoming_args,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    frame.note_method_is_static(cached.is_static);
    // Same GC-safety ordering as `route_jit_exception_through_method`: the
    // exception oop must live in a scanned frame slot before anything that can
    // allocate runs. The value is the PIN's: the `exc` parameter predates the
    // search and the acquire above.
    let exc = thread.native_pin_roots[pin_base];
    if frame.stack.push(Value::Object(Some(exc))).is_err() {
        // The monitor guard truncates to its own, deeper pin index.
        drop(synchronized_monitor);
        return miss(thread, CalleeHandlerMiss::Declined);
    }
    frame.pc = handler_pc;
    if let Some(monitor) = synchronized_monitor {
        monitor.transfer_to_handler_frame(&mut frame);
    }
    // `execute_resumed_frame` pushes the frame before anything that can
    // allocate runs, and the frame now owns both the throwable and the monitor.
    //
    // And, when `deferred_precise` fired, the shells
    // `materialize_and_relock_precise_frame` allocated: they are in `frame`'s
    // LOCALS, which this truncate makes their only root. Same window and same
    // argument as the sibling sink -- see the longer note on
    // `route_jit_exception_through_method`'s truncate, which spells out what
    // may and may not go between these two lines.
    thread.native_pin_roots.truncate(pin_base);
    // The handler frame resumes the claimed activation and holds what its
    // compiled code locked: recorded in the frame (wave 24, lane L2), so the
    // handler's `monitorexit` finds it there. Same no-collection window.
    if let Some(locks) = compiled_locks.take() {
        locks.seed_frame_and_unpin(thread, &mut frame);
    } else {
        // Interpreter round i6 wave 120, lane L9 review: no stash, so
        // nothing describes the locks the compiled callee held at the throw.
        // The handler (javac's catch-any of a `synchronized` block) may
        // `monitorexit` one of them, so the fresh record is marked
        // understated where the method has monitor bytecode, as the stash's
        // own empty arm (`seed_frame_and_unpin`) marks it. Without the mark,
        // the wave-119 per-frame strict arm threw a false IMSE there.
        crate::runtime::interpreter::held_monitors::clear_untrusted_record(
            &mut frame,
            crate::runtime::interpreter::held_monitors::RecordProducer::CalleeHandlerNoStash,
        );
    }
    // Interpreter round i6 wave 121, lane L6: the elided levels the
    // materializer re-took for this frame are named by no record (the stash
    // pins taken locks only), so the record is understated whatever the stash
    // seeded; the handler's catch-any `monitorexit` of one must keep the
    // strict arm's leniency.
    if elided_levels_retaken {
        frame.held_monitors.mark_understated_by(
            crate::runtime::interpreter::held_monitors::RecordProducer::RetakenElidedLevels,
        );
    }
    // gen r4w5/oomjit5: the handler frame owns the throwable; the compiled
    // callee's exception never reaches an interpreted throw path, so the
    // leftover-native-return drain that path applies happens here. It sits in
    // the no-collection window described above and is allowed there: it
    // clears one field and, under the census token only, reads two
    // thread-locals and prints. It allocates nothing on the Java heap.
    let _ = crate::jit::helpers::drain_native_return_at_compiled_catch(
        thread,
        "compiled callee handler",
    );
    if crate::jit::helpers::rbc6_dbg() {
        eprintln!(
            "[rbc6-dbg] run_jit_callee_handler {}.{}{} throw_pc={} handler_pc={} precise={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            throw_pc,
            handler_pc,
            precise.is_some(),
        );
    }
    // No MethodEntry: the callee was entered in compiled code. The events of
    // the catch are posted once the frame is pushed (wave 21, L2).
    Ok(execute_resumed_handler_frame(
        shared, thread, frame, throw_pc, handler_pc,
    ))
}

/// Route a Java exception that was thrown inside a JIT-compiled method
/// through that method's exception table.
///
/// The JIT executes the entire bytecode method as native code and has no
/// direct exception handling. When a callee dispatched via
/// `jit_invoke_dispatch` throws, the exception is stashed in
/// `JIT_PENDING_EXCEPTION`. After the JIT entry returns, we must consult
/// the JIT'd method's exception table to see whether the throw should be
/// caught there instead of propagated to the caller.
///
/// `throw_pc` is the bytecode PC of the throw site within the JIT'd method,
/// if known. Pass `usize::MAX` when the throw-site PC cannot be recovered
/// (the JIT currently does not record one in `JIT_PENDING_EXCEPTION`); in
/// that case a catch-all (`catch_type == 0`, i.e. `finally`) entry is honoured
/// only when its protected region spans the **whole method**
/// (`start_pc == 0 && end_pc >= code_len`) — such an entry covers any throw
/// site, so running it is sound and preserves `finally` /
/// synchronized-monitor-exit cleanup. Narrower catch-all regions are skipped
/// (they could swallow an exception thrown outside the protected region).
/// Typed handlers still match by exception class since that is safe regardless
/// of the throw site.
///
/// If a matching handler is found, a bytecode frame for the JIT'd method
/// is pushed with `pc` at the handler and the exception on the operand
/// stack; the interpreter resumes the catch block. Otherwise the exception
/// propagates to the caller.
///
/// `claimed_locks` (JIT round 17 wave 2, lane syncdoor2, SD17-1) are the
/// pinned locks of the claimed reason-9 frame, which the caller keeps and
/// seeds or releases afterwards. When they name the METHOD monitor a
/// self-locking body handed over
/// (`CompiledLocksOfAStash::hands_over_method_monitor`), the handler frame
/// takes that hold as its `monitor_on_exit` and nothing is acquired here. A
/// door that holds a level of its own (`CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP=0`)
/// keeps it: the frame then runs at the same depth it always did.
#[allow(clippy::too_many_arguments)]
pub(super) fn route_jit_exception_through_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller_frame_idx: usize,
    cached: &Arc<CachedBytecodeMethod>,
    throw_pc: usize,
    exc: ObjectRef,
    incoming_args: &[Value],
    mut deferred_precise: Option<&mut cratonvm_jit::deopt::ReconstructedFrame>,
    method_receiver: Option<Value>,
    claimed_locks: Option<&super::deopt_resume::CompiledLocksOfAStash>,
) -> Result<CachedCallResult, MethodCallFailed> {
    if crate::jit::helpers::rbc6_dbg() {
        eprintln!(
            "[rbc6-dbg] route_jit_exception_through_method ENTER {}.{}{} throw_pc={} exception_table_len={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            throw_pc as i64,
            cached.exception_table.len(),
        );
    }
    // Fast path: no exception table at all — propagate.
    if cached.exception_table.is_empty() {
        return Err(MethodCallFailed::ExceptionThrown(exc));
    }

    // Root the throwable for the rest of this function. The handler search can
    // resolve a catch type through a user loader (running Java), and a
    // synchronized method's monitor acquire below can block on contention --
    // both are points a moving collection can run at, and until the handler
    // frame's operand stack holds it, this Rust local is the only copy. Every
    // later use re-reads the pin slot, which the collector rewrites in place
    // (the discipline `unwind_to_handler` documents).
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    // gen r4w3/rooting: `incoming_args` seeds the handler frame's locals below
    // and is a plain Rust slice — the handler search that follows can collect.
    // Pin its objects directly above `exc` (every `truncate(pin_base)` exit
    // releases them) and seed the frame from the pins afterwards.
    let args_pin_base = thread.native_pin_roots.len();
    pin_object_args(thread, incoming_args);
    // Round 14 wave 1 (lane sync): the invocation's receiver, the method
    // monitor's object (`handler_sink_genuine_receiver_enabled`), pinned
    // above the args so their pin order is unchanged.
    let receiver_pin = pin_genuine_receiver(thread, cached, method_receiver);

    // A catch type that fails to resolve replaces the throwable in the pin
    // (JVMS §5.4.3); every read below is of the pin, so the replacement is what
    // the handler receives or the caller sees propagate.
    //
    // `incoming_args` are raw `Value`s the caller handed in, and a deferred
    // precise frame's are raw addresses; both are pinned across the search
    // (`pin_frame_object_refs` is the pin/re-read `materialize_virtual_objects`
    // uses) so it may resolve catch types for real — load through a user
    // loader, materialize a `NoClassDefFoundError` — and collect.
    let (search, resume_args) = pinned_compiled_handler_search(
        thread,
        incoming_args,
        deferred_precise.as_deref_mut(),
        |thread| {
            find_jit_exception_handler(
                shared,
                thread,
                cached,
                throw_pc,
                pin_base,
                true,
                "door-route",
            )
        },
    );
    let handler_pc = search.handler_pc();
    let incoming_args: &[Value] = &resume_args;

    if crate::jit::helpers::rbc6_dbg() {
        eprintln!(
            "[rbc6-dbg] route_jit_exception_through_method RESULT {}.{}{} throw_pc={} handler_pc={:?} incoming_args_len={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            throw_pc as i64,
            handler_pc,
            incoming_args.len(),
        );
    }

    let Some(handler_pc) = handler_pc else {
        // No matching handler — propagate to caller.
        let exc = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        return Err(MethodCallFailed::ExceptionThrown(exc));
    };

    // gen r4w3/rooting: re-read the args from their pins after the search.
    let mut rooted_args: Vec<Value> = incoming_args.to_vec();
    refresh_args_from_pins(thread, args_pin_base, &mut rooted_args);
    let incoming_args: &[Value] = &rooted_args;

    // JIT round 17 wave 2, lane syncdoor2 (SD17-1): the claimed frame hands
    // the method monitor over; the handler frame adopts it below instead of
    // a fresh acquire (see `run_jit_callee_handler`'s note).
    let method_monitor_handed = claimed_locks
        .is_some_and(super::deopt_resume::CompiledLocksOfAStash::hands_over_method_monitor);
    let mut synchronized_args =
        (cached.is_synchronized && !method_monitor_handed).then(|| incoming_args.to_vec());
    let synchronized_monitor = match synchronized_args.as_mut() {
        Some(args) => match acquire_method_monitor_for_handler(
            shared,
            thread,
            cached,
            args,
            receiver_pin,
            args_pin_base,
        ) {
            Ok(monitor) => Some(monitor),
            Err(error) => {
                // `acquire` pushes its own pin only on success.
                thread.native_pin_roots.truncate(pin_base);
                return Err(error);
            }
        },
        None => None,
    };
    let incoming_args = synchronized_args.as_deref().unwrap_or(incoming_args);

    // Before pushing a frame for the JIT'd method, discard the operand-stack
    // slots reserved for the callee's arguments in the caller's frame. The
    // JIT already popped them when it dispatched, but the fast-path caller
    // (execute_invokestatic_cached / execute_invokevirtual_cached) popped
    // them before calling execute_jit_call — so nothing to undo here.

    // Restore the JIT'd method's incoming locals (`this` + declared params)
    // into the handler frame. The earlier "pass NO_ARGS — catch-block locals
    // are re-initialized before use" assumption was WRONG: the Java verifier
    // does NOT require a handler to reassign locals it reads. `this` (local 0
    // of every instance method) and unmodified parameters are live throughout
    // the method, including its catch blocks — e.g. JUnit's
    // `ThrowableCollector.execute` catches and runs `aload_0; … add(t)`, and
    // `add` then does `aload_0; getfield throwable`. With NO_ARGS those slots
    // were `uninitialized()` (→ null), so the handler saw `this == null`
    // ("Cannot read field 'throwable' because the object is null"). This was
    // latent until layer B (instance-method JIT tier-up) routed instance
    // methods — whose handlers overwhelmingly read `this` — through here.
    //
    // The incoming args are a verifier-consistent state for ANY handler in the
    // method: an exception can be thrown at the protected region's first
    // instruction, where locals still equal the method-entry values, so the
    // handler's local-type merge always includes that state. Locals first
    // assigned *inside* the try block (slot >= incoming_args.len()) remain
    // uninitialized — recovering those would need a deopt map the JIT does not
    // record — but `this`/params (the dominant and previously-broken case) are
    // now correct. `Frame::new_pooled` → `copy_args_to_locals` performs the
    // category-2 (long/double) two-slot expansion, matching a normal call.
    // `effective_max_locals` still sizes the slot vec from `cached.max_locals`.

    // ── THE DEFERRED PRECISE FRAME, resolved here and nowhere earlier ─────
    //
    // A reason-9 frame naming a scalar-replaced object or an elided lock is
    // rebuilt HERE, after the handler search and after the synchronized-method
    // acquire, because both of those can collect and the rebuilt locals are a
    // detached snapshot nothing would rewrite. The shells it allocates stay
    // pinned until this function's `truncate(pin_base)` below, which runs only
    // once the frame owns every reference. See
    // `materialize_and_relock_precise_frame`.
    //
    // A refusal propagates, which is what this route did for an unmappable
    // frame before deferral existed. It is not a good answer for a method whose
    // handler should have caught, so the JIT side does not create the shapes it
    // can fail on rather than leaning on it.
    let materialized_locals;
    let incoming_args = match deferred_precise {
        Some(rframe) => {
            match materialize_and_relock_precise_frame(shared, thread, cached, rframe) {
                Some(locals) => {
                    materialized_locals = locals;
                    &materialized_locals[..]
                }
                None => {
                    if crate::jit::helpers::rbc6_dbg() {
                        eprintln!(
                            "[rbc6-dbg] route_jit_exception_through_method MATERIALIZE-FAILED \
                             {}.{}{} bci={} — propagating",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            rframe.bci,
                        );
                    }
                    drop(synchronized_monitor);
                    let exc = thread.native_pin_roots[pin_base];
                    thread.native_pin_roots.truncate(pin_base);
                    return Err(MethodCallFailed::ExceptionThrown(exc));
                }
            }
        }
        None => incoming_args,
    };

    // T10.7 — if the per-thread pool has run dry, replenish it from the
    // shared VM-wide VecPool before building the frame.
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        cached.max_locals as usize,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        (cached.max_stack as usize).max(16) + 8,
    );

    let mut frame = crate::runtime::frame::Frame::new_pooled(
        cached.declaring_class_id,
        cached.class_name.clone(),
        cached.method_name.clone(),
        cached.method_descriptor.clone(),
        cached.source_file.clone(),
        cached.code.clone(),
        cached.exception_table.clone(),
        cached.max_stack,
        cached.max_locals,
        incoming_args,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    frame.note_method_is_static(cached.is_static);
    // GC-safety: push the live exception oop onto the handler frame's operand
    // stack BEFORE the frame is pushed, so it is in a GC-scanned slot the
    // moment the frame exists. (This used to be load-bearing across a JVMTI
    // MethodEntry callback, which can allocate and relocate live oops; the
    // push below no longer fires one — `push_resumed_frame` — but the ordering
    // stays the safe one.) Mirrors `resume_from_ir_deopt`.
    //
    // The value pushed is the PIN's, not the `exc` parameter: see the root
    // taken at the top of this function.
    let exc = thread.native_pin_roots[pin_base];
    if let Err(e) = frame.stack.push(Value::Object(Some(exc))) {
        // Release the monitor guard (it truncates to its own, deeper pin
        // index) before dropping ours.
        drop(synchronized_monitor);
        thread.native_pin_roots.truncate(pin_base);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(e)));
    }
    if let Some(monitor) = synchronized_monitor {
        monitor.transfer_to_handler_frame(&mut frame);
    } else if method_monitor_handed {
        // The handed hold, at its pinned (current) address: the frame owns it
        // from the push on; the caller's seeding skips it
        // (`seed_pushed_frame_and_unpin`). Same no-collection window as the
        // throwable's.
        frame.monitor_on_exit = match claimed_locks {
            Some(locks) => locks.handed_method_monitor(thread),
            None => None,
        };
    }
    // The throwable is now in the frame's operand stack and the monitor in
    // `monitor_on_exit`; nothing allocates before the frame is pushed below.
    //
    // THAT SENTENCE NOW CARRIES MORE THAN THE THROWABLE. When
    // `deferred_precise` fired, this truncate also releases the shells
    // `materialize_and_relock_precise_frame` allocated, and they are rooted
    // from here only by `frame`'s LOCALS -- a Rust local until
    // `push_resumed_frame` puts it on `thread.frames`. The window is
    // safe for the same reason it always was and for no other: between this
    // line and that push there is `harvest_retired_slot` (pool bookkeeping)
    // and one `eprintln!`, neither of which allocates Java heap. Anything added
    // in between that can collect must move this truncate below the push --
    // which is what `deopt_resume::build_deopt_frame_inner` does with the same
    // shells, and it says so.
    thread.native_pin_roots.truncate(pin_base);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/jit_exc_route] depth={} {}.{}{}",
            thread.frames.len(),
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor()
        );
    }
    // No MethodEntry: the method was entered in compiled code, and this frame
    // resumes it at its handler (`push_resumed_frame`).
    push_resumed_frame(thread, frame);
    let new_idx = thread.frames.len() - 1;
    // Exception is already on the operand stack (rooted before the fire above);
    // just position the PC at the handler.
    thread.frames[new_idx].pc = handler_pc;
    // gen r4w5/oomjit5: the pushed handler frame owns the throwable, and the
    // exception it catches never reaches the interpreter's own throw path
    // (`settle_thrown_exception`), so drop a leftover native return here.
    let _ = crate::jit::helpers::drain_native_return_at_compiled_catch(
        thread,
        "compiled method handler frame",
    );
    // `Exception` at the compiled throw site, then `ExceptionCatch` (wave 21,
    // L2): the frame owns the throwable, so the callbacks may collect.
    super::report_exception_caught_by_compiled_door(shared, thread, new_idx, throw_pc, handler_pc);
    // Silence unused parameter warning — caller_frame_idx is kept for
    // future extensions (e.g. return-value coercion into the caller).
    let _ = caller_frame_idx;
    Ok(CachedCallResult::FramePushed)
}

#[cfg(test)]
mod handler_search_tests {
    //! `find_exception_handler_impl` takes the class-manager lock only when it
    //! reaches a covering TYPED row; a covering catch-all ahead of it, or no
    //! covering row at all, is answered from the table alone. These pin the
    //! JVMS §2.10 search order that restructuring had to preserve: rows in
    //! table order, `[start_pc, end_pc)` with an exclusive end, first match
    //! wins whether it is typed or catch-all.
    use super::{
        find_exception_handler, find_exception_handler_any_pc, find_exception_handler_pc_unknown,
        load_catch_types_at_link, run_jit_callee_handler, search_frame_for_unwind,
        unwind_to_handler, CalleeHandlerMiss, UnwindSearch, JIT_THROW_PC_UNSTAMPED_EXPLICIT,
    };
    use crate::classloading::resolution::CachedBytecodeMethod;
    use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
    use crate::error::MethodCallFailed;
    use crate::runtime::frame::Frame;
    use crate::threading::jvm_thread::JvmThread;
    use crate::types::{ObjectRef, Value};
    use crate::vm::SharedVm;
    use cratonvm_reader::attribute::ExceptionTableEntry;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    const A: &str = "cratonvm/test/HandlerOrderExcA";
    /// Named by the owner's cp #4 and defined nowhere. Not a JDK or enterprise
    /// name, so the global path would not fabricate a stub for it.
    const MISSING: &str = "cratonvm/test/HandlerOrderMissingCatchType";

    /// A VM with exception classes A and B (unrelated), and an owner class
    /// whose constant pool slot 2 is `CONSTANT_Class A` and slot 4 is
    /// `CONSTANT_Class MISSING`.
    fn fixture() -> (std::sync::Arc<SharedVm>, ClassId, ClassId, ClassId) {
        let shared = std::sync::Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let (exc_a, exc_b) = {
            let mut cm = shared.classes.class_manager.write();
            (
                cm.try_ensure_synthetic_class(A, 0)
                    .expect("Compatible mode fabricates"),
                cm.try_ensure_synthetic_class("cratonvm/test/HandlerOrderExcB", 0)
                    .expect("Compatible mode fabricates"),
            )
        };
        let owner = {
            let mut cm = shared.classes.class_manager.write();
            let id = cm.class_store.next_id();
            let pool = ConstantPool::new(vec![
                ConstantPoolEntry::Tombstone,
                ConstantPoolEntry::Utf8(A.into()), // 1
                ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
                ConstantPoolEntry::Utf8(MISSING.into()), // 3
                ConstantPoolEntry::ClassReference { name_index: 3 }, // 4
            ]);
            cm.class_store.add(Class {
                id,
                loader_id: ClassLoaderId::Application,
                name: cratonvm_types::intern_arc("cratonvm/test/HandlerOrderOwner"),
                source_file: None,
                version: ClassFileVersion::JAVA_8,
                state: ClassState::Initialized,
                initializing_thread: None,
                constant_pool: pool,
                access_flags: ClassAccessFlags::empty(),
                superclass: None,
                interfaces: vec![],
                fields: vec![],
                methods: vec![],
                first_field_index: 0,
                num_total_fields: 0,
                bootstrap_methods: vec![],
                signature: None,
                annotations: Vec::new(),
                nest_host: None,
                nest_members: Vec::new(),
                record_components: Vec::new(),
                permitted_subclasses: Vec::new(),
                inner_classes: Vec::new(),
                enclosing_method: None,
                hidden: false,
                module_name: None,
                origin: cratonvm_classloading::ClassOrigin::default(),
                has_finalizer: false,
                code_source: None,
                array_info: None,
                record_object_methods: std::sync::atomic::AtomicU8::new(0),
                init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            });
            cm.register_class_name(
                ClassLoaderId::Application,
                "cratonvm/test/HandlerOrderOwner",
                id,
            );
            id
        };
        (shared, owner, exc_a, exc_b)
    }

    fn row(start_pc: u16, end_pc: u16, handler_pc: u16, catch_type: u16) -> ExceptionTableEntry {
        ExceptionTableEntry {
            start_pc,
            end_pc,
            handler_pc,
            catch_type,
        }
    }

    fn frame(owner: ClassId, table: Vec<ExceptionTableEntry>) -> Frame {
        Frame::new(
            owner,
            "cratonvm/test/HandlerOrderOwner".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            table,
            4,
            2,
            &[],
        )
    }

    fn search(shared: &SharedVm, f: &Frame, pc: usize, exc: ObjectRef) -> Option<usize> {
        find_exception_handler(shared, f, pc, exc).map(|(handler, _)| handler)
    }

    #[test]
    fn first_covering_row_wins_in_table_order_typed_or_catch_all() {
        let (shared, owner, exc_a, exc_b) = fixture();
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let b = shared.mem.heap.alloc_object(exc_b, 0);

        // `catch (A) {..} finally {..}` shape: typed row, then catch-all.
        let typed_then_any = frame(owner, vec![row(0, 10, 30, 2), row(0, 10, 40, 0)]);
        assert_eq!(search(&shared, &typed_then_any, 5, a), Some(30));
        assert_eq!(
            search(&shared, &typed_then_any, 5, b),
            Some(40),
            "a typed row that does not match must fall through to the catch-all after it"
        );

        // A catch-all AHEAD of a matching typed row wins: first match, not
        // best match. This is the row the lock-free early return answers.
        let any_then_typed = frame(owner, vec![row(0, 10, 40, 0), row(0, 10, 30, 2)]);
        assert_eq!(search(&shared, &any_then_typed, 5, a), Some(40));
    }

    #[test]
    fn protected_range_is_start_inclusive_end_exclusive() {
        let (shared, owner, exc_a, _) = fixture();
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        for ct in [0u16, 2] {
            let f = frame(owner, vec![row(5, 10, 40, ct)]);
            assert_eq!(
                search(&shared, &f, 4, a),
                None,
                "catch_type {ct}: pc < start"
            );
            assert_eq!(
                search(&shared, &f, 5, a),
                Some(40),
                "catch_type {ct}: pc == start"
            );
            assert_eq!(
                search(&shared, &f, 9, a),
                Some(40),
                "catch_type {ct}: pc == end-1"
            );
            assert_eq!(
                search(&shared, &f, 10, a),
                None,
                "catch_type {ct}: pc == end"
            );
            assert_eq!(
                search(&shared, &f, super::OSR_FRAME_DECLINED_TO_CATCH, a),
                None,
                "the OSR 'already declined' sentinel must match no row"
            );
        }
    }

    #[test]
    fn rows_not_covering_the_pc_are_skipped_before_and_after_the_lock() {
        let (shared, owner, exc_a, exc_b) = fixture();
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let b = shared.mem.heap.alloc_object(exc_b, 0);
        // An uncovering catch-all first, then a covering typed row, then an
        // uncovering catch-all again.
        let f = frame(
            owner,
            vec![row(0, 3, 20, 0), row(3, 10, 30, 2), row(10, 20, 50, 0)],
        );
        assert_eq!(search(&shared, &f, 5, a), Some(30));
        assert_eq!(search(&shared, &f, 5, b), None);
        assert_eq!(search(&shared, &f, 1, b), Some(20));
        // No table at all.
        assert_eq!(search(&shared, &frame(owner, vec![]), 0, a), None);
    }

    fn thread_with(owner: ClassId, table: Vec<ExceptionTableEntry>) -> JvmThread {
        let mut thread = JvmThread::new(crate::ThreadId(0), "test");
        thread.frames.push(frame(owner, table));
        thread
    }

    fn unwind_search(
        shared: &SharedVm,
        thread: &mut JvmThread,
        pc: usize,
        exc: ObjectRef,
    ) -> UnwindSearch {
        let pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(exc);
        let found = search_frame_for_unwind(shared, thread, 0, pc, pin);
        thread.native_pin_roots.truncate(pin);
        found
    }

    /// The unwinder's search keeps the thread-less search's order and range
    /// answers for resolvable catch types, including on the second search of
    /// the same row (which may be answered from `thread.cast_sites`).
    #[test]
    fn unwind_search_agrees_with_the_table_order_search_for_resolvable_rows() {
        let (shared, owner, exc_a, exc_b) = fixture();
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let b = shared.mem.heap.alloc_object(exc_b, 0);
        let mut thread = thread_with(owner, vec![row(0, 10, 30, 2), row(0, 10, 40, 0)]);
        for _ in 0..2 {
            assert_eq!(
                unwind_search(&shared, &mut thread, 5, a),
                UnwindSearch::Found(30)
            );
            assert_eq!(
                unwind_search(&shared, &mut thread, 5, b),
                UnwindSearch::Found(40)
            );
            assert_eq!(
                unwind_search(&shared, &mut thread, 10, a),
                UnwindSearch::NotFound
            );
        }
        // A cached target must not turn a non-matching class into a match.
        let mut typed_only = thread_with(owner, vec![row(0, 10, 30, 2)]);
        for _ in 0..2 {
            assert_eq!(
                unwind_search(&shared, &mut typed_only, 5, a),
                UnwindSearch::Found(30)
            );
            assert_eq!(
                unwind_search(&shared, &mut typed_only, 5, b),
                UnwindSearch::NotFound
            );
        }
    }

    /// A refused catch row memoises the refusal per exception class
    /// (`CastSite::negative_catch`); the memo answers only that class, and a
    /// class the row DOES catch is never recorded as refused, whatever order the
    /// two arrive in.
    #[test]
    fn catch_row_refusal_is_memoised_per_exception_class() {
        let (shared, owner, exc_a, exc_b) = fixture();
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let b = shared.mem.heap.alloc_object(exc_b, 0);
        let mut t = thread_with(owner, vec![row(0, 10, 30, 2)]);
        for _ in 0..3 {
            assert_eq!(unwind_search(&shared, &mut t, 5, b), UnwindSearch::NotFound);
            assert_eq!(
                unwind_search(&shared, &mut t, 5, a),
                UnwindSearch::Found(30)
            );
        }
        // The table can be latched off or an epoch can move under a parallel
        // test, dropping a put; what must never be seen is a wrong memo.
        if let Some(site) = t.cast_sites.get(owner, 2).copied() {
            assert_ne!(site.negative_catch, Some(exc_a));
            assert!(!site.admits(exc_b));
        }
    }

    /// JVMS §5.4.3 / HotSpot `exception_handler_for_exception`: a covering row
    /// whose catch type cannot be resolved throws the resolution error, which
    /// replaces the in-flight exception, and the search restarts in the same
    /// frame at that row's handler pc. It used to be read as "no match" and
    /// the ORIGINAL exception kept unwinding.
    #[test]
    fn unresolvable_catch_type_replaces_the_exception_and_restarts_at_its_handler() {
        let (shared, owner, exc_a, _) = fixture();
        let mut thread = thread_with(owner, vec![]);
        if crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/NoClassDefFoundError",
            Some("probe"),
        )
        .is_err()
        {
            // Stripped VM: the error cannot be materialized, and the search
            // then keeps the lenient answer by design.
            return;
        }
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let error_class_name = |obj: ObjectRef| {
            let cid = shared.mem.heap.class_id_of(obj);
            shared
                .classes
                .class_manager
                .read()
                .get_class(cid)
                .map(|c| c.name.to_string())
        };

        // The search alone reports the failure at the failing row's handler.
        let mut t = thread_with(owner, vec![row(0, 10, 30, 4), row(0, 10, 40, 0)]);
        match unwind_search(&shared, &mut t, 5, a) {
            UnwindSearch::ResolutionFailed { handler_pc, error } => {
                assert_eq!(
                    handler_pc, 30,
                    "the FAILING row's handler, not the catch-all's"
                );
                assert_eq!(
                    error_class_name(error).as_deref(),
                    Some("java/lang/NoClassDefFoundError")
                );
            }
            other => panic!("expected a resolution failure, got {other:?}"),
        }

        // The unwinder: the error restarts the search at pc 30, which the
        // catch-all over [30, 35) covers, so it resumes at 50 holding the ERROR.
        let mut t = thread_with(owner, vec![row(0, 10, 30, 4), row(30, 35, 50, 0)]);
        let mut frame_idx = 0usize;
        let pins_before = t.native_pin_roots.len();
        unwind_to_handler(&shared, &mut t, &mut frame_idx, 0, a, 5)
            .expect("the restarted search finds the catch-all");
        assert_eq!(
            t.native_pin_roots.len(),
            pins_before,
            "the walk releases its pin"
        );
        assert_eq!(t.frames[0].pc, 50);
        let caught = match t.frames[0].stack.pop() {
            Ok(Value::Object(Some(r))) => r,
            other => panic!("handler frame must hold the throwable, got {other:?}"),
        };
        assert_eq!(
            error_class_name(caught).as_deref(),
            Some("java/lang/NoClassDefFoundError")
        );

        // No row covers the restart pc: the ERROR escapes, not the original.
        let mut t = thread_with(owner, vec![row(0, 10, 30, 4)]);
        let mut frame_idx = 0usize;
        match unwind_to_handler(&shared, &mut t, &mut frame_idx, 0, a, 5) {
            Err(MethodCallFailed::ExceptionThrown(escaped)) => {
                assert_ne!(escaped, a);
                assert_eq!(
                    error_class_name(escaped).as_deref(),
                    Some("java/lang/NoClassDefFoundError")
                );
            }
            other => panic!("expected the resolution error to escape, got {other:?}"),
        }

        // A handler inside its own failing row cannot loop forever.
        let mut t = thread_with(owner, vec![row(0, 40, 30, 4)]);
        let mut frame_idx = 0usize;
        assert!(matches!(
            unwind_to_handler(&shared, &mut t, &mut frame_idx, 0, a, 5),
            Err(MethodCallFailed::ExceptionThrown(_))
        ));
    }

    /// `(handler pc, throwable left in the pin)` of a compiled-frame search over
    /// `thread.frames[0]`; `pc == None` is the pc-unknown search.
    fn compiled_search(
        shared: &SharedVm,
        thread: &mut JvmThread,
        pc: Option<usize>,
        exc: ObjectRef,
    ) -> (Option<usize>, ObjectRef) {
        let pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(exc);
        let found = match pc {
            Some(pc) => find_exception_handler_any_pc(shared, thread, 0, pc, pin, true),
            None => find_exception_handler_pc_unknown(shared, thread, 0, pin),
        };
        let now = thread.native_pin_roots[pin];
        thread.native_pin_roots.truncate(pin);
        (found, now)
    }

    /// The compiled-frame routes' search gives the interpreter unwinder's
    /// answers: same table order and ranges for resolvable rows, and a catch
    /// type that fails to resolve REPLACES the throwable and restarts the search
    /// at the failing row's handler. Until round i1 wave 3 these routes kept the
    /// loader-blind lazy load and read the failure as "no match", so a frame
    /// and the same frame compiled disagreed.
    #[test]
    fn compiled_frame_search_matches_the_unwinder_on_resolution_failure() {
        let (shared, owner, exc_a, exc_b) = fixture();
        let mut thread = thread_with(owner, vec![]);
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let b = shared.mem.heap.alloc_object(exc_b, 0);

        // Resolvable rows: range and order as `find_exception_handler`.
        let mut t = thread_with(owner, vec![row(0, 10, 30, 2), row(0, 10, 40, 0)]);
        assert_eq!(compiled_search(&shared, &mut t, Some(5), a), (Some(30), a));
        assert_eq!(compiled_search(&shared, &mut t, Some(5), b), (Some(40), b));
        assert_eq!(compiled_search(&shared, &mut t, Some(10), a), (None, a));
        // pc unknown: the typed row matches on class; the narrow catch-all
        // does not count.
        let mut t = thread_with(owner, vec![row(3, 10, 30, 2), row(3, 10, 40, 0)]);
        assert_eq!(compiled_search(&shared, &mut t, None, a), (Some(30), a));
        assert_eq!(compiled_search(&shared, &mut t, None, b), (None, b));

        if crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/NoClassDefFoundError",
            Some("probe"),
        )
        .is_err()
        {
            // Stripped VM: the error cannot be materialized, and the search
            // then keeps the lenient answer by design.
            return;
        }
        let is_ncdfe = |obj: ObjectRef| {
            let cid = shared.mem.heap.class_id_of(obj);
            shared
                .classes
                .class_manager
                .read()
                .get_class(cid)
                .is_some_and(|c| c.name.to_string() == "java/lang/NoClassDefFoundError")
        };

        // Known pc: the error restarts the search at 30, which the catch-all
        // over [30, 35) covers.
        let mut t = thread_with(owner, vec![row(0, 10, 30, 4), row(30, 35, 50, 0)]);
        let (found, now) = compiled_search(&shared, &mut t, Some(5), a);
        assert_eq!(found, Some(50));
        assert!(is_ncdfe(now), "the handler receives the resolution ERROR");

        // Unknown pc: the typed row is judged on class alone, fails to resolve,
        // and the restart at its handler is a KNOWN pc — so a catch-all that
        // does not span the method is honoured there.
        let (found, now) = compiled_search(&shared, &mut t, None, a);
        assert_eq!(found, Some(50));
        assert!(is_ncdfe(now));

        // Nothing covers the restart pc: the ERROR is what propagates.
        let mut t = thread_with(owner, vec![row(0, 10, 30, 4), row(0, 10, 40, 0)]);
        let (found, now) = compiled_search(&shared, &mut t, Some(5), a);
        assert_eq!(found, None);
        assert_ne!(now, a);
        assert!(is_ncdfe(now));

        // A handler inside its own failing row terminates.
        let mut t = thread_with(owner, vec![row(0, 40, 30, 4)]);
        let (found, now) = compiled_search(&shared, &mut t, Some(5), a);
        assert_eq!(found, None);
        assert!(is_ncdfe(now));
    }

    /// A static `()V` callee of `owner` whose table is `table`, for
    /// [`run_jit_callee_handler`].
    fn cached_callee(
        owner: ClassId,
        table: Vec<ExceptionTableEntry>,
    ) -> std::sync::Arc<CachedBytecodeMethod> {
        let parts = cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: owner,
            class_name: std::sync::Arc::from("cratonvm/test/HandlerOrderOwner"),
            method_name: std::sync::Arc::from("callee"),
            method_descriptor: std::sync::Arc::from("()V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(&[0u8; 40]),
            exception_table: std::sync::Arc::from(table.as_slice()),
            max_stack: 4,
            max_locals: 2,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        };
        std::sync::Arc::new(CachedBytecodeMethod::from_parts(parts))
    }

    /// The implicit-signal doors in `jit/helpers.rs` (`try_run_callee_handler`)
    /// tell a replaced throwable from a merely relocated one by comparing the
    /// miss's throwable with THEIR OWN pin of the original, which
    /// `run_jit_callee_handler`'s search never writes. Pins that contract: a
    /// resolvable miss hands back the pinned original, a failing catch type
    /// hands back a distinct resolution error, and the caller's pin still holds
    /// the original either way.
    #[test]
    fn callee_handler_miss_hands_back_a_replacement_distinct_from_the_callers_pin() {
        let (shared, owner, exc_a, exc_b) = fixture();
        let mut thread = thread_with(owner, vec![]);
        let b = shared.mem.heap.alloc_object(exc_b, 0);

        // The caller's pin, as `try_run_callee_handler` holds it.
        let caller_pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(b);
        // Row #2 (`A`) resolves and does not catch `B`: a definite miss with
        // the original throwable.
        let cached = cached_callee(owner, vec![row(0, 10, 30, 2)]);
        match run_jit_callee_handler(&shared, &mut thread, &cached, 5, b, &[]) {
            Err((CalleeHandlerMiss::NotCaught, now)) => {
                assert_eq!(
                    now, thread.native_pin_roots[caller_pin],
                    "not a replacement"
                );
            }
            Err(_) => panic!("expected a definite miss"),
            Ok(_) => panic!("row 2 does not catch B"),
        }
        assert_eq!(thread.native_pin_roots.len(), caller_pin + 1);
        thread.native_pin_roots.truncate(caller_pin);

        if crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/NoClassDefFoundError",
            Some("probe"),
        )
        .is_err()
        {
            // Stripped VM: no error to replace it with (lenient answer).
            return;
        }
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let caller_pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(a);
        // Row #4 names a missing class: its resolution error replaces `a`, and
        // nothing covers the restart pc 30.
        let cached = cached_callee(owner, vec![row(0, 10, 30, 4)]);
        match run_jit_callee_handler(&shared, &mut thread, &cached, 5, a, &[]) {
            Err((CalleeHandlerMiss::NotCaught, now)) => {
                let original = thread.native_pin_roots[caller_pin];
                assert_ne!(now, original, "the resolution error replaced it");
                let cid = shared.mem.heap.class_id_of(now);
                assert_eq!(
                    shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .as_deref(),
                    Some("java/lang/NoClassDefFoundError")
                );
            }
            Err(_) => panic!("a resolution restart is a definite miss"),
            Ok(_) => panic!("nothing covers the restart pc"),
        }
        assert_eq!(thread.native_pin_roots.len(), caller_pin + 1);
        thread.native_pin_roots.truncate(caller_pin);
    }

    /// Interpreter round i5 wave 105, lane L2: an EXPLICIT throwable a callee
    /// service hands over with no stamp (`JIT_THROW_PC_UNSTAMPED_EXPLICIT`) has
    /// already left the callee, so it is a definite `NotCaught` without a
    /// search; `CRATONVM_JIT_UNSTAMPED_CALLEE_THROW_ESCAPES=0` restores the
    /// pc-unknown search, whose miss is only `Unknown`. The row does not catch
    /// `B` either way, so neither arm resumes a handler.
    #[test]
    fn an_unstamped_explicit_throwable_is_not_searched_again() {
        let (shared, owner, _exc_a, exc_b) = fixture();
        let mut thread = thread_with(owner, vec![]);
        let b = shared.mem.heap.alloc_object(exc_b, 0);
        let caller_pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(b);
        let cached = cached_callee(owner, vec![row(10, 26, 29, 2)]);
        let pc = JIT_THROW_PC_UNSTAMPED_EXPLICIT;
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_UNSTAMPED_CALLEE_THROW_ESCAPES", None)],
            || run_jit_callee_handler(&shared, &mut thread, &cached, pc, b, &[]),
        );
        match on {
            Err((CalleeHandlerMiss::NotCaught, now)) => {
                assert_eq!(now, thread.native_pin_roots[caller_pin], "the original")
            }
            Err(_) => panic!("expected a definite miss"),
            Ok(_) => panic!("no handler may run"),
        }
        assert_eq!(thread.native_pin_roots.len(), caller_pin + 1);
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_UNSTAMPED_CALLEE_THROW_ESCAPES", Some("0"))],
            || run_jit_callee_handler(&shared, &mut thread, &cached, pc, b, &[]),
        );
        match off {
            Err((CalleeHandlerMiss::Unknown, _)) => {}
            Err(_) => panic!("=0: the pc-unknown search's miss is `Unknown`"),
            Ok(_) => panic!("row 2 does not catch B"),
        }
        assert_eq!(thread.native_pin_roots.len(), caller_pin + 1);
        thread.native_pin_roots.truncate(caller_pin);
    }

    /// A compiled body with one `PendingException` point at `bci`, for the
    /// body-identity tests below.
    fn body_with_rethrow_point(bci: u32) -> cratonvm_jit::CompiledMethod {
        use cratonvm_jit::deopt::{DeoptAction, DeoptReason, DeoptimizationPoint, FrameState};
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // ret
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.deopt_points.push(DeoptimizationPoint {
            native_offset: 0,
            bci,
            reason: DeoptReason::PendingException,
            action: DeoptAction::Reinterpret,
            semantics: cratonvm_jit::deopt::ResumeSemantics::for_reason(
                DeoptReason::PendingException,
            ),
            speculation_id: 0,
            frame_state: FrameState {
                method_key: String::new(),
                bci,
                locals: Vec::new(),
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
        });
        cm
    }

    /// Interpreter round i1 wave 11
    /// (`interpreter-L7-deopt-sinks-match-stashed-frames-by-name-FIXED-20260925.md`):
    /// the drain of a compiled body's throw routes through a stashed
    /// exceptional frame only when that body published it. A same-named frame
    /// whose point belongs to another body is dropped, and the throw is then
    /// routed by the body's own stamp — here one outside every protected
    /// range, so it propagates; the body's own frame (bci 5, inside the
    /// catch-all's `[0, 10)`) is caught.
    #[test]
    fn the_exception_drain_takes_only_the_frame_its_own_body_published() {
        use super::{route_jit_signal_exception, JitThrowPc};
        use cratonvm_jit::deopt::{DeoptimizationPoint, ReconstructedFrame};
        let (shared, owner, exc_a, _) = fixture();
        let mut thread = thread_with(owner, vec![]);
        while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
        let ran = body_with_rethrow_point(5);
        let other = body_with_rethrow_point(5);
        let own_point = &ran.deopt_points[0] as *const DeoptimizationPoint as usize;
        let other_point = &other.deopt_points[0] as *const DeoptimizationPoint as usize;
        let cached = cached_callee(owner, vec![row(0, 10, 30, 0)]);
        let published = || ReconstructedFrame {
            method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
            bci: 5,
            ..Default::default()
        };
        let a = shared.mem.heap.alloc_object(exc_a, 0);

        // Another body's frame: dropped; the stamp outside every range wins.
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, other_point);
        let depth = thread.frames.len();
        let r = route_jit_signal_exception(
            &shared,
            &mut thread,
            0,
            &ran,
            &cached,
            JitThrowPc::OutsideAllRanges(20),
            a,
            &[],
        );
        assert!(
            matches!(r, Err(MethodCallFailed::ExceptionThrown(_))),
            "another body's throw bci must not route this body's exception"
        );
        assert_eq!(thread.frames.len(), depth);
        assert!(
            cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none(),
            "a foreign frame is dropped, not left for a later throw"
        );

        // This body's own frame: its bci is inside the catch-all's range.
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let r = route_jit_signal_exception(
            &shared,
            &mut thread,
            0,
            &ran,
            &cached,
            JitThrowPc::OutsideAllRanges(20),
            a,
            &[],
        );
        assert!(
            matches!(r, Ok(super::CachedCallResult::FramePushed)),
            "the body's own frame routes to its handler"
        );
        assert_eq!(thread.frames.len(), depth + 1);
        assert_eq!(thread.frames[depth].pc, 30);
        assert!(cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none());
    }

    /// Interpreter round i1 wave 24, lane L2
    /// (`docs/internal/fixed-bugs/interpreter-L2-a-refused-resume-of-a-frame-holding-a-compiled-lock-leaks-the-lock-FIXED-20260926.md`,
    /// the exceptional channel): a reason-9 frame naming a lock its compiled
    /// code took (`relock == false`) either hands the lock to the handler
    /// frame, whose `held_monitors` record then names it, or — when the
    /// exception leaves the activation without its handler — releases it, as
    /// javac's catch-all would have before rethrowing. Both sinks: the door's
    /// drain (`route_jit_signal_exception`) and the compiled-callee handler
    /// (`run_jit_callee_handler`).
    #[test]
    fn the_exception_routes_hand_compiled_locks_to_the_handler_or_release_them() {
        use super::{route_jit_signal_exception, JitThrowPc};
        use cratonvm_jit::deopt::{
            DeoptimizationPoint, FrameValue, MonitorInfo, ReconstructedFrame,
        };
        let (shared, owner, exc_a, _) = fixture();
        let mut thread = thread_with(owner, vec![]);
        let tid = thread.thread_id;
        while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
        let ran = body_with_rethrow_point(5);
        let own_point = &ran.deopt_points[0] as *const DeoptimizationPoint as usize;
        let lock = shared.mem.heap.alloc_object(exc_a, 0);
        // Cast: object pointer to the raw word a stash carries.
        let lock_word = lock.as_ptr() as usize as u64;
        let published = || ReconstructedFrame {
            method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
            bci: 5,
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(lock_word),
                lock_depth: 1,
                relock: false,
            }],
            ..Default::default()
        };
        let pins = thread.native_pin_roots.len();

        // No row covers bci 5: the exception leaves the activation, and the
        // lock its compiled code took goes with it.
        shared.threads.monitors.enter(lock, tid);
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let r = route_jit_signal_exception(
            &shared,
            &mut thread,
            0,
            &ran,
            &cached_callee(owner, vec![row(20, 30, 40, 0)]),
            JitThrowPc::OutsideAllRanges(20),
            a,
            &[],
        );
        assert!(matches!(r, Err(MethodCallFailed::ExceptionThrown(_))));
        assert!(
            !shared.threads.monitors.holds(lock, tid),
            "a propagated exception releases what the activation's compiled code locked"
        );
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");

        // The catch-all covers bci 5: the handler frame resumes the activation,
        // keeps the lock, and records it.
        shared.threads.monitors.enter(lock, tid);
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let depth = thread.frames.len();
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let r = route_jit_signal_exception(
            &shared,
            &mut thread,
            0,
            &ran,
            &cached_callee(owner, vec![row(0, 10, 30, 0)]),
            JitThrowPc::OutsideAllRanges(20),
            a,
            &[],
        );
        assert!(matches!(r, Ok(super::CachedCallResult::FramePushed)));
        assert_eq!(thread.frames.len(), depth + 1);
        assert!(
            shared.threads.monitors.holds(lock, tid),
            "the handler frame inherits the hold"
        );
        assert_eq!(
            thread.frames[depth].held_monitors.count_of(lock),
            1,
            "the handler frame's record names the compiled code's lock"
        );
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
        assert!(shared.threads.monitors.exit(lock, tid).is_ok());
        assert!(
            shared.threads.monitors.exit(lock, tid).is_err(),
            "held exactly once"
        );

        // The compiled-callee handler sink: a miss abandons the claimed
        // activation (the caller propagates or re-runs it), so it releases.
        shared.threads.monitors.enter(lock, tid);
        let b = shared.mem.heap.alloc_object(exc_a, 0);
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let cached = cached_callee(owner, vec![row(20, 30, 40, 0)]);
        match run_jit_callee_handler(&shared, &mut thread, &cached, 5, b, &[]) {
            Err((CalleeHandlerMiss::NotCaught, _)) => {}
            Err(_) => panic!("a known throw pc no row covers is a definite miss"),
            Ok(_) => panic!("no row covers bci 5"),
        }
        assert!(
            !shared.threads.monitors.holds(lock, tid),
            "a callee-handler miss releases the claimed frame's compiled lock"
        );
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
        assert!(cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none());
    }

    /// A `synchronized` INSTANCE twin of [`cached_callee`].
    fn cached_synchronized_instance_callee(
        owner: ClassId,
        table: Vec<ExceptionTableEntry>,
    ) -> std::sync::Arc<CachedBytecodeMethod> {
        let parts = cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: owner,
            class_name: std::sync::Arc::from("cratonvm/test/HandlerOrderOwner"),
            method_name: std::sync::Arc::from("callee"),
            method_descriptor: std::sync::Arc::from("()V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(&[0u8; 40]),
            exception_table: std::sync::Arc::from(table.as_slice()),
            max_stack: 4,
            max_locals: 2,
            num_params: 0,
            is_synchronized: true,
            is_static: false,
        };
        std::sync::Arc::new(CachedBytecodeMethod::from_parts(parts))
    }

    /// Round 14 wave 1 (lane sync,
    /// `r14w1-sync-handler-sinks-conflate-the-method-monitor-with-local-0-FIXED-20260929.md`):
    /// after `astore_0 other` in a synchronized instance method, the handler
    /// frame the door's drain pushes keeps `other` as its local 0 and holds
    /// the INVOCATION's receiver as its method monitor, never `other`; and the
    /// callee-handler sink's mapping keeps a live local 0 as the frame
    /// published it.
    #[test]
    fn a_handler_frame_locks_the_invocation_receiver_not_its_local_0() {
        use super::{precise_handler_frame_for, route_jit_signal_exception, JitThrowPc};
        use cratonvm_jit::deopt::{DeoptimizationPoint, FrameValue, ReconstructedFrame};
        if !super::handler_sink_genuine_receiver_enabled() {
            return;
        }
        let (shared, owner, exc_a, _) = fixture();
        let mut thread = thread_with(owner, vec![]);
        let tid = thread.thread_id;
        while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
        let ran = body_with_rethrow_point(5);
        let own_point = &ran.deopt_points[0] as *const DeoptimizationPoint as usize;
        let this = shared.mem.heap.alloc_object(owner, 0);
        let other = shared.mem.heap.alloc_object(owner, 0);
        // Cast: object pointer to the raw word a stash carries.
        let other_word = other.as_ptr() as usize as u64;
        let published = || ReconstructedFrame {
            method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
            bci: 5,
            locals: vec![FrameValue::Object(other_word)],
            ..Default::default()
        };
        let cached = cached_synchronized_instance_callee(owner, vec![row(0, 10, 30, 0)]);
        let pins = thread.native_pin_roots.len();

        // The door's drain.
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let depth = thread.frames.len();
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let r = route_jit_signal_exception(
            &shared,
            &mut thread,
            0,
            &ran,
            &cached,
            JitThrowPc::OutsideAllRanges(20),
            a,
            &[Value::Object(Some(this))],
        );
        assert!(matches!(r, Ok(super::CachedCallResult::FramePushed)));
        assert_eq!(thread.frames.len(), depth + 1);
        let handler = &thread.frames[depth];
        assert_eq!(handler.get_local_unchecked(0), Value::Object(Some(other)));
        assert_eq!(
            handler.monitor_on_exit,
            Some(this),
            "the monitor is the receiver's"
        );
        assert!(shared.threads.monitors.holds(this, tid));
        assert!(
            !shared.threads.monitors.holds(other, tid),
            "local 0 is not locked"
        );
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
        assert!(shared.threads.monitors.exit(this, tid).is_ok());
        thread.frames.truncate(depth);

        // The callee-handler sink's mapping: a LIVE local 0 is kept.
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let mut claimed = None;
        match precise_handler_frame_for(
            &shared,
            &mut thread,
            &cached,
            &[Value::Object(Some(this))],
            &mut claimed,
        ) {
            Some((5, super::PreciseHandlerLocals::Mapped(locals))) => {
                assert_eq!(locals.first().copied(), Some(Value::Object(Some(other))));
            }
            _ => panic!("the frame of this method maps"),
        }
        assert!(cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none());
    }

    /// JIT round 17 wave 2, lane syncdoor2 (SD17-1,
    /// `r17-syncdoor-declined-handler-in-self-locking-body-reacquires-FIXED-20261005.md`):
    /// a self-locking body's precise exceptional frame that names the METHOD
    /// monitor (its stub kept the hold) is adopted by the handler frame as its
    /// `monitor_on_exit`, with no second acquire and no block-monitor record;
    /// a miss releases it exactly once; the callee-handler sink's claim names
    /// it and seeds it the same way.
    #[test]
    fn a_handed_method_monitor_is_adopted_by_the_handler_frame_or_released_once() {
        use super::{precise_handler_frame_for, route_jit_signal_exception, JitThrowPc};
        use cratonvm_jit::deopt::{
            DeoptimizationPoint, FrameValue, MonitorInfo, ReconstructedFrame,
        };
        let (shared, owner, exc_a, _) = fixture();
        let mut thread = thread_with(owner, vec![]);
        let tid = thread.thread_id;
        while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
        let ran = body_with_rethrow_point(5);
        let own_point = &ran.deopt_points[0] as *const DeoptimizationPoint as usize;
        let this = shared.mem.heap.alloc_object(owner, 0);
        // Cast: object pointer to the raw word a stash carries.
        let this_word = this.as_ptr() as usize as u64;
        // The shape `x64/deopt_stubs.rs` publishes: local 0 and the one
        // monitor are the receiver, one taken level.
        let published = || ReconstructedFrame {
            method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
            bci: 5,
            locals: vec![FrameValue::Object(this_word)],
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(this_word),
                lock_depth: 1,
                relock: false,
            }],
            ..Default::default()
        };
        let catching = cached_synchronized_instance_callee(owner, vec![row(0, 10, 30, 0)]);
        let missing = cached_synchronized_instance_callee(owner, vec![row(20, 30, 40, 0)]);
        let pins = thread.native_pin_roots.len();
        let receiver = [Value::Object(Some(this))];

        // The door's drain, a handler covers bci 5: the frame owns the one
        // hold the compiled prologue took.
        shared.threads.monitors.enter(this, tid);
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        let depth = thread.frames.len();
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let r = route_jit_signal_exception(
            &shared,
            &mut thread,
            0,
            &ran,
            &catching,
            JitThrowPc::OutsideAllRanges(20),
            a,
            &receiver,
        );
        assert!(matches!(r, Ok(super::CachedCallResult::FramePushed)));
        assert_eq!(thread.frames.len(), depth + 1);
        assert_eq!(thread.frames[depth].monitor_on_exit, Some(this));
        assert_eq!(
            thread.frames[depth].held_monitors.count_of(this),
            0,
            "the method monitor is not a block monitor"
        );
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
        assert!(shared.threads.monitors.exit(this, tid).is_ok());
        assert!(
            shared.threads.monitors.exit(this, tid).is_err(),
            "held exactly once: adopted, not re-acquired"
        );
        thread.frames.truncate(depth);

        // The door's drain, no handler covers bci 5: released once.
        shared.threads.monitors.enter(this, tid);
        let a = shared.mem.heap.alloc_object(exc_a, 0);
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let r = route_jit_signal_exception(
            &shared,
            &mut thread,
            0,
            &ran,
            &missing,
            JitThrowPc::OutsideAllRanges(20),
            a,
            &receiver,
        );
        assert!(matches!(r, Err(MethodCallFailed::ExceptionThrown(_))));
        assert!(
            !shared.threads.monitors.holds(this, tid),
            "a miss releases it"
        );
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");

        // The callee-handler sink, a miss: released once.
        shared.threads.monitors.enter(this, tid);
        let b = shared.mem.heap.alloc_object(exc_a, 0);
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        match run_jit_callee_handler(&shared, &mut thread, &missing, 5, b, &receiver) {
            Err((CalleeHandlerMiss::NotCaught, _)) => {}
            Err(_) => panic!("a known throw pc no row covers is a definite miss"),
            Ok(_) => panic!("no row covers bci 5"),
        }
        assert!(
            !shared.threads.monitors.holds(this, tid),
            "a callee-handler miss releases it"
        );
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");

        // The callee-handler sink's claim (its Found arm's half that runs no
        // Java): it names the hold, so the sink skips its acquire, and the
        // seeding makes it the handler frame's `monitor_on_exit`.
        shared.threads.monitors.enter(this, tid);
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let mut claimed = None;
        assert!(precise_handler_frame_for(
            &shared,
            &mut thread,
            &catching,
            &receiver,
            &mut claimed
        )
        .is_some());
        let locks = claimed.expect("the claim pins the frame's locks");
        assert!(locks.hands_over_method_monitor());
        let mut handler = frame(owner, vec![row(0, 10, 30, 0)]);
        locks.seed_frame_and_unpin(&mut thread, &mut handler);
        assert_eq!(handler.monitor_on_exit, Some(this));
        assert_eq!(handler.held_monitors.count_of(this), 0);
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
        assert!(shared.threads.monitors.exit(this, tid).is_ok());
        assert!(
            shared.threads.monitors.exit(this, tid).is_err(),
            "held exactly once"
        );

        // A frame naming a lock on another object, or of a method that is
        // not synchronized, hands nothing over: it stays a block monitor.
        let other = shared.mem.heap.alloc_object(owner, 0);
        let mut foreign_lock = published();
        // Cast: object pointer to the raw word a stash carries.
        foreign_lock.monitors[0].object = FrameValue::Object(other.as_ptr() as usize as u64);
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(foreign_lock, None, own_point);
        let mut claimed = None;
        let _ = precise_handler_frame_for(&shared, &mut thread, &catching, &receiver, &mut claimed);
        let locks = claimed.expect("claimed");
        assert!(!locks.hands_over_method_monitor());
        locks.unpin_keeping_the_locks(&mut thread);
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let mut claimed = None;
        let plain = cached_callee(owner, vec![row(0, 10, 30, 0)]);
        let _ = precise_handler_frame_for(&shared, &mut thread, &plain, &[], &mut claimed);
        let locks = claimed.expect("claimed");
        assert!(!locks.hands_over_method_monitor(), "not synchronized");
        locks.unpin_keeping_the_locks(&mut thread);
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
        assert!(cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none());
    }

    /// JIT round 17 wave 2, lane syncdoor2 (SD17-1): under the switches the
    /// JIT hands a method monitor over with, a FOREIGN exceptional frame the
    /// door's drain drops (its owner has unwound) has its holds released;
    /// with them off it is dropped as before.
    #[test]
    fn a_dropped_foreign_frame_releases_its_holds_under_the_hand_over() {
        use super::{route_jit_signal_exception, JitThrowPc};
        use cratonvm_jit::deopt::{
            DeoptimizationPoint, FrameValue, MonitorInfo, ReconstructedFrame,
        };
        let (shared, owner, exc_a, _) = fixture();
        let mut thread = thread_with(owner, vec![]);
        let tid = thread.thread_id;
        while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
        let ran = body_with_rethrow_point(5);
        let other = body_with_rethrow_point(5);
        let other_point = &other.deopt_points[0] as *const DeoptimizationPoint as usize;
        let lock = shared.mem.heap.alloc_object(owner, 0);
        // Cast: object pointer to the raw word a stash carries.
        let lock_word = lock.as_ptr() as usize as u64;
        let published = || ReconstructedFrame {
            method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
            bci: 5,
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(lock_word),
                lock_depth: 1,
                relock: false,
            }],
            ..Default::default()
        };
        let cached = cached_callee(owner, vec![row(0, 10, 30, 0)]);
        let pins = thread.native_pin_roots.len();
        let drain = |thread: &mut JvmThread| {
            let a = shared.mem.heap.alloc_object(exc_a, 0);
            cratonvm_jit::deopt::restash_exceptional_frame_with_point(
                published(),
                None,
                other_point,
            );
            route_jit_signal_exception(
                &shared,
                thread,
                0,
                &ran,
                &cached,
                JitThrowPc::OutsideAllRanges(20),
                a,
                &[],
            )
        };
        let on = [
            ("CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS", Some("1")),
            ("CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER", None),
        ];
        cratonvm_types::flags::with_thread_overrides(&on, || {
            shared.threads.monitors.enter(lock, tid);
            assert!(matches!(
                drain(&mut thread),
                Err(MethodCallFailed::ExceptionThrown(_))
            ));
            assert!(
                !shared.threads.monitors.holds(lock, tid),
                "released with its frame"
            );
            assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
        });
        let off = [
            ("CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS", Some("0")),
            ("CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER", None),
        ];
        cratonvm_types::flags::with_thread_overrides(&off, || {
            shared.threads.monitors.enter(lock, tid);
            assert!(matches!(
                drain(&mut thread),
                Err(MethodCallFailed::ExceptionThrown(_))
            ));
            assert!(
                shared.threads.monitors.holds(lock, tid),
                "unchanged with the switch off"
            );
            assert!(shared.threads.monitors.exit(lock, tid).is_ok());
        });
        assert!(cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none());
    }

    /// JIT round 17 wave 3, lane syncdoor3
    /// (`r17-syncdoor2-dropped-exceptional-frames-keep-a-handed-method-monitor-FIXED-20261005.md`):
    /// the release every exceptional-frame drop site shares
    /// (`helpers.rs` `drop_exceptional_frame_of`,
    /// `drop_orphaned_exceptional_frames_at_compiled_catch`) releases each
    /// TAKEN hold the frame names exactly as often as it names it, leaves an
    /// elided level and every other hold alone, and leaks no pin; its gate is
    /// the exceptional hand-over's pair of switches.
    #[test]
    fn a_dropped_exceptional_frame_releases_exactly_its_taken_holds() {
        use super::super::deopt_resume::CompiledLocksOfAStash;
        use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame};
        let (shared, owner, _, _) = fixture();
        let mut thread = thread_with(owner, vec![]);
        let tid = thread.thread_id;
        let lock = shared.mem.heap.alloc_object(owner, 0);
        let elided = shared.mem.heap.alloc_object(owner, 0);
        // Cast: object pointers to the raw words a stash carries.
        let lock_word = lock.as_ptr() as usize as u64;
        let elided_word = elided.as_ptr() as usize as u64;
        let dropped = ReconstructedFrame {
            method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
            bci: 5,
            monitors: vec![
                MonitorInfo {
                    object: FrameValue::Object(lock_word),
                    lock_depth: 2,
                    relock: false,
                },
                MonitorInfo {
                    object: FrameValue::Object(elided_word),
                    lock_depth: 1,
                    relock: true,
                },
            ],
            ..Default::default()
        };
        let pins = thread.native_pin_roots.len();
        // Two holds the frame names, plus one an outer frame owns.
        for _ in 0..3 {
            shared.threads.monitors.enter(lock, tid);
        }
        let released =
            CompiledLocksOfAStash::release_for_a_dropped_frame(&shared, &mut thread, &dropped, "t");
        assert_eq!(released, 2, "both named levels, the elided one untouched");
        assert!(
            shared.threads.monitors.holds(lock, tid),
            "the outer hold stays"
        );
        assert!(shared.threads.monitors.exit(lock, tid).is_ok());
        assert!(
            !shared.threads.monitors.holds(lock, tid),
            "released exactly twice"
        );
        assert!(!shared.threads.monitors.holds(elided, tid));
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
        let bare = ReconstructedFrame {
            method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
            bci: 5,
            ..Default::default()
        };
        assert_eq!(
            CompiledLocksOfAStash::release_for_a_dropped_frame(&shared, &mut thread, &bare, "t"),
            0
        );
        assert_eq!(
            thread.native_pin_roots.len(),
            pins,
            "a lock-free frame pins nothing"
        );

        let gate = |handlers: Option<&str>, handover: Option<&str>| {
            let env = [
                ("CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS", handlers),
                ("CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER", handover),
            ];
            cratonvm_types::flags::with_thread_overrides(&env, || {
                CompiledLocksOfAStash::dropped_frame_holds_released()
            })
        };
        assert!(gate(Some("1"), None));
        assert!(!gate(Some("1"), Some("0")), "the hand-over's kill switch");
        // Default ON since JIT round 18 close (the handlers switch flipped).
        assert!(gate(None, None), "default: released");
        assert!(!gate(Some("0"), None), "the handlers switch off: unchanged");
    }

    /// JIT round 17 wave 8, lane syncdoor5
    /// (`r17-self-locking-handlers-refuse-recursive-exceptional-resume-FIXED-20261005.md`):
    /// the callee-handler template serves a synchronized callee exactly under
    /// the handlers switch, whatever the hand-over's kill switch says (with
    /// the hand-over off the sink re-acquires for a catch, as documented).
    #[test]
    fn synchronized_callees_are_served_only_under_the_handlers_switch() {
        let served = |handlers: Option<&str>, handover: Option<&str>| {
            let env = [
                ("CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS", handlers),
                ("CRATONVM_JIT_SELF_LOCK_EXC_HANDOVER", handover),
            ];
            cratonvm_types::flags::with_thread_overrides(
                &env,
                super::synchronized_callee_handlers_served,
            )
        };
        // Default ON since JIT round 18 close (the handlers switch flipped).
        assert!(served(None, None), "default: served");
        assert!(
            !served(Some("0"), None),
            "the handlers switch off: refused as before"
        );
        assert!(served(Some("1"), None));
        assert!(served(Some("1"), Some("0")));
    }

    /// Round 11 wave 11 (lane irexc,
    /// `r11w10-irexc-reason9-drain-does-not-repair-the-receiver-slot`): the
    /// reason-9 drain puts the caller's receiver back into a slot 0 the
    /// snapshot published as dead — which is what a synchronized instance
    /// method's monitor re-acquire reads — and leaves a live slot 0, a static
    /// method's slot 0 and a missing receiver alone.
    #[test]
    fn a_dead_receiver_slot_is_restored_and_nothing_else_is() {
        use super::restore_dead_receiver;
        use cratonvm_jit::deopt::FrameValue;
        let (shared, owner, _, _) = fixture();
        let this = shared.mem.heap.alloc_object(owner, 0);
        let fallback = [Value::Object(Some(this)), Value::Int(9)];
        let dead = [FrameValue::Undefined, FrameValue::Int(7)];

        // What `ir_deopt_locals` maps `Undefined` to.
        let mut locals = vec![Value::Int(0), Value::Int(7)];
        restore_dead_receiver(&mut locals, &dead, false, &fallback);
        assert_eq!(locals, vec![Value::Object(Some(this)), Value::Int(7)]);

        let mut live = vec![Value::Int(5), Value::Int(7)];
        restore_dead_receiver(
            &mut live,
            &[FrameValue::Int(5), FrameValue::Int(7)],
            false,
            &fallback,
        );
        assert_eq!(
            live,
            vec![Value::Int(5), Value::Int(7)],
            "a live slot 0 is the bytecode's"
        );

        let mut stat = vec![Value::Int(0), Value::Int(7)];
        restore_dead_receiver(&mut stat, &dead, true, &fallback);
        assert_eq!(
            stat,
            vec![Value::Int(0), Value::Int(7)],
            "a static method has no receiver"
        );

        let mut none = vec![Value::Int(0)];
        restore_dead_receiver(&mut none, &dead, false, &[]);
        assert_eq!(none, vec![Value::Int(0)]);
    }

    /// Define and register a field-less, method-less class `name` for `loader`.
    fn define_class(
        shared: &SharedVm,
        name: &str,
        loader: ClassLoaderId,
        constant_pool: ConstantPool,
    ) -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: loader,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool,
            access_flags: ClassAccessFlags::empty(),
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        cm.register_class_name(loader, name, id);
        id
    }

    /// The loader-blind name rule is `--compatible`-only for a catch row whose
    /// type RESOLVED, as the cast arms' `loader_aware_name_assignable` is: a
    /// same-named class from an unrelated loader is caught under
    /// `--compatible` (the Spring forked-loader / AssertJ `IntrospectionError`
    /// accommodation, unchanged) and NOT under `--jdk-only`, where only
    /// `is_subclass_of` admits (HotSpot's answer). Asked twice per mode so the
    /// second answer comes from the warm `cast_sites` row.
    ///
    /// Wave 52 (lane L7): the rule is the `NamespacedClassTypes` alignment
    /// row's, as the cast arms': a `--compatible` VM with every row on (the
    /// third arm) refuses the same-named class as `--jdk-only` does. The
    /// first arm turns the row off, so it keeps the loader-blind answer
    /// (`CRATONVM_COMPAT_ALIGN=-namespaced-class-types`).
    #[test]
    fn resolved_catch_row_name_rule_is_compatible_only() {
        const SAME: &str = "cratonvm/test/HandlerNameRuleExc";
        for (mode, rows_on) in [
            (cratonvm_types::compat::CompatibilityMode::Compatible, false),
            (cratonvm_types::compat::CompatibilityMode::JdkOnly, false),
            (cratonvm_types::compat::CompatibilityMode::Compatible, true),
        ] {
            let mut config = crate::config::VmConfig::default();
            config.compatibility_mode = mode;
            // `JdkOnly` rejects the synthetic library; off in both arms so the
            // two VMs differ only in the mode (as `jit_bridge`'s `vm_with`).
            config.use_synthetic_jdk = false;
            let mut shared = SharedVm::new(config);
            // The row is on by default since wave 52 (lane L4), so the
            // loader-blind `--compatible` arm turns it off explicitly; the
            // `--jdk-only` VM keeps the set it was built with.
            if rows_on {
                shared.classes.hotspot_aligned = cratonvm_types::HotSpotAlignments::all();
            } else if !mode.is_jdk_only() {
                shared.classes.hotspot_aligned = cratonvm_types::HotSpotAlignments::defaults()
                    .with(
                        cratonvm_types::HotSpotAlignment::NamespacedClassTypes,
                        false,
                    );
            }
            let empty_pool = || ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
            let app = define_class(&shared, SAME, ClassLoaderId::Application, empty_pool());
            let other = define_class(&shared, SAME, ClassLoaderId::UserDefined(77), empty_pool());
            let owner = define_class(
                &shared,
                "cratonvm/test/HandlerNameRuleOwner",
                ClassLoaderId::Application,
                ConstantPool::new(vec![
                    ConstantPoolEntry::Tombstone,
                    ConstantPoolEntry::Utf8(SAME.into()), // 1
                    ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
                ]),
            );
            let exact = shared.mem.heap.alloc_object(app, 0);
            let same_name = shared.mem.heap.alloc_object(other, 0);
            let mut t = thread_with(owner, vec![row(0, 10, 30, 2)]);
            for _ in 0..2 {
                assert_eq!(
                    unwind_search(&shared, &mut t, 5, exact),
                    UnwindSearch::Found(30),
                    "{mode:?}: the resolved class itself"
                );
                let expected = if mode.is_jdk_only() || rows_on {
                    UnwindSearch::NotFound
                } else {
                    UnwindSearch::Found(30)
                };
                assert_eq!(
                    unwind_search(&shared, &mut t, 5, same_name),
                    expected,
                    "{mode:?}: a same-named class from another loader"
                );
            }
        }
    }

    /// i5-L5 verifier page: under `--jdk-only` the link step loads every typed
    /// catch type through the class's loader (HotSpot's
    /// `verify_exception_handler_table`). A catch type that will not load is a
    /// linkage error of the class; a loaded non-`Throwable` is a `VerifyError`;
    /// a loadable `Throwable` subclass links.
    #[test]
    fn link_time_catch_type_loading_rejects_a_missing_or_non_throwable_type() {
        use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        const EXC: &str = "cratonvm/test/LinkCatchExc";
        const PLAIN: &str = "cratonvm/test/LinkCatchNotThrowable";
        const GONE: &str = "cratonvm/test/LinkCatchMissing";

        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        config.use_synthetic_jdk = false;
        let shared = SharedVm::new(config);
        let empty_pool = || ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
        let throwable = match shared
            .classes
            .class_manager
            .read()
            .find_bootstrap_class_by_name("java/lang/Throwable")
        {
            Some(id) => id,
            None => define_class(
                &shared,
                "java/lang/Throwable",
                ClassLoaderId::Bootstrap,
                empty_pool(),
            ),
        };
        let exc = define_class(&shared, EXC, ClassLoaderId::Application, empty_pool());
        shared
            .classes
            .class_manager
            .write()
            .set_superclass(exc, Some(throwable));
        define_class(&shared, PLAIN, ClassLoaderId::Application, empty_pool());

        // An owner whose one method has a single typed row naming `catch`.
        let owner_catching = |owner_name: &str, catch: &str| {
            let owner = define_class(
                &shared,
                owner_name,
                ClassLoaderId::Application,
                ConstantPool::new(vec![
                    ConstantPoolEntry::Tombstone,
                    ConstantPoolEntry::Utf8(catch.into()), // 1
                    ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
                ]),
            );
            let method = cratonvm_reader::method::ClassFileMethod {
                access_flags: MethodAccessFlags::STATIC,
                name: std::sync::Arc::from("m"),
                descriptor: std::sync::Arc::from("()V"),
                attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                    CodeAttribute {
                        max_stack: 1,
                        max_locals: 0,
                        // nop; return; (handler) pop; return
                        code: cratonvm_reader::ByteView::from_vec(vec![0x00, 0xb1, 0x57, 0xb1]),
                        exception_table: vec![row(0, 1, 2, 2)],
                        attributes: vec![],
                    },
                )))],
            };
            if let Some(class) = shared.classes.class_manager.write().get_class_mut(owner) {
                class.methods = vec![method];
            }
            owner
        };
        let mut thread = JvmThread::new(crate::ThreadId(0), "test");

        let ok = owner_catching("cratonvm/test/LinkCatchOwnerOk", EXC);
        assert!(
            load_catch_types_at_link(&shared, &mut thread, ok).is_ok(),
            "a loadable Throwable subclass links"
        );
        let missing = owner_catching("cratonvm/test/LinkCatchOwnerMissing", GONE);
        assert!(
            load_catch_types_at_link(&shared, &mut thread, missing).is_err(),
            "a catch type that will not load fails the link"
        );
        let plain = owner_catching("cratonvm/test/LinkCatchOwnerPlain", PLAIN);
        assert!(
            load_catch_types_at_link(&shared, &mut thread, plain).is_err(),
            "a catch type that is not a Throwable is a VerifyError"
        );
    }
}

#[cfg(test)]
mod r21_finalize_ctor_proof_tests {
    //! JIT round 21 wave 10, lane finalize: the proof that a constructor's
    //! throw preceded every `<init>` call on its receiver
    //! (`ctor_throw_precedes_initialization`). Pool entry 5 is another
    //! class's `<init>` (`new IAE` before `super()`); every other entry may
    //! initialize the receiver.
    use super::ctor_throw_precedes_initialization as proven;
    use super::*;

    fn may(i: u16) -> bool {
        i != 5
    }

    /// The constructor class the unwinder tests run on (wave 15). The test
    /// VM boots the synthetic JDK (`EMBEDDED_DEFAULT_JDK_MODE`), whose
    /// `java/lang/StringBuilder` has no `()V` bytecode, so the w10-w14 tests
    /// that borrowed `StringBuilder()` never reached their asserts.
    const FIN_CLASS: &str = "cratonvm/test/R21FinCtor";
    const FIN_BASE: &str = "cratonvm/test/R21FinCtorBase";

    /// Defines `FIN_BASE` and, under it, the finalizable `FIN_CLASS` whose
    /// `<init>()V` is `aload_0; bipush 16; invokespecial #6 (FIN_BASE.<init>(I)V);
    /// return` (the shape `StringBuilder()` has on a real JDK), into the test
    /// VM's class store. Returns the class id, the constructor's code and the
    /// instance field count.
    fn define_finalizable_ctor_class(shared: &SharedVm) -> (ClassId, Vec<u8>, usize) {
        use crate::classloading::{Class, ClassLoaderId, ClassState};
        use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
        use cratonvm_reader::class_file_version::ClassFileVersion;
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        use cratonvm_reader::method::ClassFileMethod;
        const CODE: [u8; 7] = [0x2a, 0x10, 0x10, 0xb7, 0x00, 0x06, 0xb1];
        let class = |id: ClassId,
                     name: &str,
                     superclass: Option<ClassId>,
                     pool: Vec<ConstantPoolEntry>,
                     methods: Vec<ClassFileMethod>,
                     has_finalizer: bool| Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(pool),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass,
            interfaces: vec![],
            fields: vec![],
            methods,
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        };
        let ctor = ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: cratonvm_types::intern_arc("<init>"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(CodeAttribute {
                max_stack: 2,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(CODE.to_vec()),
                exception_table: vec![],
                attributes: vec![],
            })))],
        };
        let mut cm = shared.classes.class_manager.write();
        let base = cm.class_store.next_id();
        cm.class_store.add(class(
            base,
            FIN_BASE,
            None,
            vec![ConstantPoolEntry::Tombstone],
            vec![],
            false,
        ));
        let id = cm.class_store.next_id();
        cm.class_store.add(class(
            id,
            FIN_CLASS,
            Some(base),
            vec![
                ConstantPoolEntry::Tombstone,
                ConstantPoolEntry::Utf8(FIN_BASE.into()),
                ConstantPoolEntry::ClassReference { name_index: 1 },
                ConstantPoolEntry::Utf8("<init>".into()),
                ConstantPoolEntry::Utf8("(I)V".into()),
                ConstantPoolEntry::NameAndType {
                    name_index: 3,
                    descriptor_index: 4,
                },
                ConstantPoolEntry::MethodReference {
                    class_index: 2,
                    name_and_type_index: 5,
                },
            ],
            vec![ctor],
            true,
        ));
        (id, CODE.to_vec(), 0)
    }

    /// The unwinder end to end: `FIN_CLASS()` (`aload_0; bipush 16;
    /// invokespecial FIN_BASE.<init>(I)V; return`, a finalizable class built
    /// in this VM's store, [`define_finalizable_ctor_class`]) unwound under an interpreted
    /// creator frame. A throw at the `bipush` withdraws the registration at
    /// the creator; one at the `super(16)` call keeps it.
    #[test]
    fn the_unwinder_withdraws_a_constructor_that_threw_before_super() {
        use crate::threading::jvm_thread::ThreadId;
        if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
            return; // `CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT=0`: the defect.
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        // Wave 15: a hand-built finalizable class (the synthetic JDK's
        // `StringBuilder` has no `()V` bytecode).
        let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
        if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
            return; // Another JDK's body: the shape this test names is gone.
        }
        let exc_class = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrown", 0)
            .expect("Compatible mode fabricates");
        let registered_after_throw_at = |throw_pc: usize| -> bool {
            let obj = shared.mem.heap.alloc_object(sb, num_fields);
            let addr = obj.as_ptr() as usize;
            shared.register_finalizable(addr);
            let exc = shared.mem.heap.alloc_object(exc_class, 0);
            let mut t = JvmThread::new(ThreadId(0x0F1A), "r21-finalize-unwind");
            // The creator: `invokespecial #1; return`, at its invoke.
            t.frames.push(Frame::new(
                ClassId::new(0x00F1_0C01),
                "cratonvm/test/R21Creator".to_string(),
                "make".to_string(),
                "()V".to_string(),
                None,
                vec![0xb7, 0, 1, 0xb1],
                Vec::new(),
                4,
                2,
                &[],
            ));
            t.frames[0].last_instr_pc = 0;
            t.frames.push(Frame::new(
                sb,
                FIN_CLASS.to_string(),
                "<init>".to_string(),
                "()V".to_string(),
                None,
                code.clone(),
                Vec::new(),
                4,
                2,
                &[Value::Object(Some(obj))],
            ));
            // Wave 13: each step of the chain on its own, so a failure names
            // the step (the w10 build failed this test without saying which).
            assert!(
                shared.mem.ref_processor.lock().is_registered_finalizable(addr),
                "registered before the throw"
            );
            assert!(
                super::super::class_has_finalizer_shared(&shared, obj),
                "the receiver reads finalizable"
            );
            assert_eq!(
                unconstructed_mark_of(&shared, &t, 1, throw_pc).is_some(),
                throw_pc == 1,
                "the proof at pc {throw_pc}"
            );
            let mut idx = 1usize;
            assert!(unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, throw_pc).is_err());
            shared.mem.ref_processor.lock().is_registered_finalizable(addr)
        };
        assert!(!registered_after_throw_at(1), "thrown before super(16): withdrawn");
        assert!(registered_after_throw_at(3), "thrown at the super(16) call: kept");
    }

    // `F(int x) { this(check(x)); }`: aload_0; iload_1; invokestatic #2;
    // invokespecial #3; return.
    const THIS_OF_CHECK: &[u8] = &[0x2a, 0x1b, 0xb8, 0, 2, 0xb7, 0, 3, 0xb1];

    #[test]
    fn a_throw_inside_the_argument_of_this_precedes_initialization() {
        assert!(proven(THIS_OF_CHECK, 2, &[], &may));
        // At the `this(..)` call itself: the callee may have initialized it.
        assert!(!proven(THIS_OF_CHECK, 5, &[], &may));
        // Not an instruction boundary (an operand byte of the invokestatic).
        assert!(!proven(THIS_OF_CHECK, 3, &[], &may));
        // A handler that starts before the call could bring control back.
        assert!(!proven(THIS_OF_CHECK, 2, &[0], &may));
        assert!(proven(THIS_OF_CHECK, 2, &[8], &may));
    }

    #[test]
    fn a_throw_after_super_is_not_proven() {
        // aload_0; invokespecial #1; iload_1; invokestatic #2; return.
        let code = [0x2a, 0xb7, 0, 1, 0x1b, 0xb8, 0, 2, 0xb1];
        assert!(!proven(&code, 5, &[], &may));
    }

    #[test]
    fn a_new_exception_before_super_is_not_an_initialization() {
        // iload_1; ifge +11; new #4; dup; invokespecial #5 (IAE.<init>);
        // athrow; aload_0; invokespecial #1; return.
        let code = [
            0x1b, 0x9c, 0, 11, 0xbb, 0, 4, 0x59, 0xb7, 0, 5, 0xbf, 0x2a, 0xb7, 0, 1, 0xb1,
        ];
        assert!(proven(&code, 11, &[], &may));
        assert!(proven(&code, 8, &[], &may));
        // Were entry 5 a possible `this(..)`, the athrow would follow it.
        assert!(!proven(&code, 11, &[], &|_| true));
    }

    #[test]
    fn a_back_edge_over_the_initialization_refuses() {
        // iload_1; invokestatic #2; aload_0; invokespecial #1; goto -8.
        let code = [0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xa7, 0xff, 0xf8];
        assert!(!proven(&code, 1, &[], &may));
        // The same body with the goto replaced by `return` is proven.
        let code = [0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1];
        assert!(proven(&code, 1, &[], &may));
    }

    #[test]
    fn a_write_of_local_0_or_a_subroutine_refuses() {
        // aconst_null; astore_0; ... (local 0 is no longer the receiver).
        let code = [0x01, 0x4b, 0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1];
        assert!(!proven(&code, 3, &[], &may));
        // wide astore 0.
        let code = [0x01, 0xc4, 0x3a, 0, 0, 0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1];
        assert!(!proven(&code, 6, &[], &may));
        // astore_1 is fine.
        let code = [0x01, 0x4c, 0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1];
        assert!(proven(&code, 3, &[], &may));
        // jsr anywhere.
        let code = [0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1, 0xa8, 0, 0];
        assert!(!proven(&code, 1, &[], &may));
        // A reserved opcode.
        let code = [0x1b, 0xca, 0x2a, 0xb7, 0, 1, 0xb1];
        assert!(!proven(&code, 0, &[], &may));
    }

    #[test]
    fn a_lookupswitch_before_the_call_decodes() {
        // iload_1; lookupswitch (pad 2) default +19, 1 pair (5 -> +19);
        // aload_0; iload_1; invokestatic #2; invokespecial #3; return.
        let code = [
            0x1b, 0xab, 0, 0, 0, 0, 0, 19, 0, 0, 0, 1, 0, 0, 0, 5, 0, 0, 0, 19, 0x2a, 0x1b, 0xb8,
            0, 2, 0xb7, 0, 3, 0xb1,
        ];
        assert!(proven(&code, 22, &[], &may));
        // A truncated body refuses.
        assert!(!proven(&code[..24], 22, &[], &may));
    }

    /// Wave 11 (review item 1): a hidden class is stored `p/H/0x<hex>` and
    /// its pool names `p/H`; its `this(..)` initializes.
    #[test]
    fn a_hidden_class_this_call_is_an_initialization() {
        use super::ctor_target_may_initialize as may_init;
        let sup = Some(("java/lang/Object", false));
        assert!(may_init("p/H/0x1f2e", true, sup, "p/H"), "pre-mangle name");
        assert!(may_init("p/H/0x1f2e", true, sup, "p/H/0x1f2e"));
        assert!(may_init("p/H", false, sup, "p/H"));
        assert!(may_init("p/H", false, sup, "java/lang/Object"), "super(..)");
        assert!(!may_init("p/H", false, sup, "java/lang/RuntimeException"));
        // Only a hidden class's name is read as mangled.
        assert!(!may_init("p/H/0x1f2e", false, sup, "p/H"));
        // A hidden superclass is spelled by its pre-mangle name too.
        assert!(may_init("p/K", false, Some(("p/S/0x77", true)), "p/S"));

        // `H(int x) { this(); throw new RuntimeException(); }`: aload_0;
        // invokespecial #1 (p/H.<init>()V); new #4; dup; invokespecial #5
        // (RuntimeException.<init>); athrow. The pool's #1 names `p/H`.
        let code = [0x2a, 0xb7, 0, 1, 0xbb, 0, 4, 0x59, 0xb7, 0, 5, 0xbf];
        let class_of = |i: u16| if i == 1 { "p/H" } else { "java/lang/RuntimeException" };
        let hidden_aware = |i: u16| may_init("p/H/0x1f2e", true, sup, class_of(i));
        assert!(
            !proven(&code, 11, &[], &hidden_aware),
            "the athrow follows this(): Object.<init> returned"
        );
        // The wave-10 compare (plain names) wrongly proved it.
        let plain = |i: u16| class_of(i) == "p/H/0x1f2e" || class_of(i) == "java/lang/Object";
        assert!(proven(&code, 11, &[], &plain));
    }

    /// Wave 11: the constructors kept interpreted are exactly those with a
    /// throwing instruction where the proof would hold.
    #[test]
    fn only_a_constructor_that_can_throw_before_init_stays_interpreted() {
        use super::ctor_may_throw_before_initialization as may_throw;
        // `this(check(x))`: the invokestatic precedes the call.
        assert!(may_throw(THIS_OF_CHECK, &[], &may));
        // `super(); f(x);`: nothing before the call can throw.
        let code = [0x2a, 0xb7, 0, 1, 0x1b, 0xb8, 0, 2, 0xb1];
        assert!(!may_throw(&code, &[], &may));
        // `super(x, y)`: loads only.
        let code = [0x2a, 0x1b, 0x1c, 0xb7, 0, 1, 0xb1];
        assert!(!may_throw(&code, &[], &may));
        // `super(x / y)`: idiv throws.
        let code = [0x2a, 0x1b, 0x1c, 0x6c, 0xb7, 0, 1, 0xb1];
        assert!(may_throw(&code, &[], &may));
        // `super(x * y)`: imul does not.
        let code = [0x2a, 0x1b, 0x1c, 0x68, 0xb7, 0, 1, 0xb1];
        assert!(!may_throw(&code, &[], &may));
        // A body the proof refuses (a back edge over the call) gains nothing
        // from the interpreter.
        let code = [0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xa7, 0xff, 0xf8];
        assert!(!may_throw(&code, &[], &may));
        // A handler before the call refuses too.
        assert!(!may_throw(THIS_OF_CHECK, &[0], &may));
    }

    /// Wave 11: the walk withdraws at its ENTRY frame (the creator is a
    /// compiled method, a JIT helper or a native door), unless the
    /// interpreted frame below stands at an `invokespecial` a handler covers
    /// (review item 4: a retry of `<init>` could follow); then it only notes.
    #[test]
    fn the_entry_frame_withdraws_unless_the_frame_below_may_retry() {
        use crate::threading::jvm_thread::ThreadId;
        use cratonvm_reader::attribute::ExceptionTableEntry;
        if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        // Wave 15: a hand-built finalizable class (the synthetic JDK's
        // `StringBuilder` has no `()V` bytecode).
        let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
        if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
            return;
        }
        let exc_class = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrownEntry", 0)
            .expect("Compatible mode fabricates");
        // `below`: the interpreted frame under the entry, with or without a
        // row covering its `invokespecial` at pc 0, and (wave 14) with or
        // without a local holding the object (the retry shape).
        let registered_after = |below: Option<(bool, bool)>| -> bool {
            let obj = shared.mem.heap.alloc_object(sb, num_fields);
            let addr = obj.as_ptr() as usize;
            shared.register_finalizable(addr);
            let exc = shared.mem.heap.alloc_object(exc_class, 0);
            let mut t = JvmThread::new(ThreadId(0x0F1B), "r21-finalize-entry");
            if let Some((covered, holds)) = below {
                let held = [Value::Object(Some(obj))];
                let table = if covered {
                    vec![ExceptionTableEntry {
                        start_pc: 0,
                        end_pc: 3,
                        handler_pc: 4,
                        catch_type: 0,
                    }]
                } else {
                    Vec::new()
                };
                t.frames.push(Frame::new(
                    ClassId::new(0x00F1_0C02),
                    "cratonvm/test/R21OuterCreator".to_string(),
                    "make".to_string(),
                    "()V".to_string(),
                    None,
                    vec![0xb7, 0, 1, 0xb1, 0xbf],
                    table,
                    4,
                    2,
                    if holds { &held[..] } else { &[] },
                ));
                t.frames[0].last_instr_pc = 0;
            }
            let entry = t.frames.len();
            t.frames.push(Frame::new(
                sb,
                FIN_CLASS.to_string(),
                "<init>".to_string(),
                "()V".to_string(),
                None,
                code.clone(),
                Vec::new(),
                4,
                2,
                &[Value::Object(Some(obj))],
            ));
            let mut idx = entry;
            // The walk's entry is the constructor frame: `initial_frame_idx == entry`.
            assert!(unwind_to_handler(&shared, &mut t, &mut idx, entry, exc, 1).is_err());
            shared.mem.ref_processor.lock().is_registered_finalizable(addr)
        };
        assert!(!registered_after(None), "no frame below: withdrawn at the entry");
        assert!(!registered_after(Some((false, false))), "an uncovered creator below: withdrawn");
        assert!(!registered_after(Some((false, true))), "uncovered, a local holding it: withdrawn");
        assert!(
            registered_after(Some((true, true))),
            "a covered invokespecial and a local holding the object: kept (noted)"
        );
        assert!(
            !registered_after(Some((true, false))),
            "wave 14: covered, but no handler can reach the object: withdrawn"
        );
    }

    /// Wave 16 (lane finalize-reinit, residual 4R,
    /// `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`). Case (a): the first
    /// `FIN_CLASS()` attempt throws at its `bipush` under a creator outside
    /// the walk (a compiled method) and is withdrawn at the entry; the
    /// creator's retry returns normally from `FIN_CLASS()` on the same object,
    /// which registers it again, exactly once. A `return`-pc the prefix scan
    /// puts before `super(16)` registers nothing. Case (b): an object whose
    /// `<init>` returned keeps its registration when a later `<init>` on it
    /// throws before `super(..)`.
    #[test]
    fn a_later_init_return_registers_a_withdrawn_object_again_once() {
        use crate::threading::jvm_thread::ThreadId;
        if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        if !shared.mem.ref_processor.lock().reregister_at_init_return() {
            return; // `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN=0`: the defect.
        }
        let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
        assert_eq!(code, vec![0x2a, 0x10, 0x10, 0xb7, 0x00, 0x06, 0xb1]);
        let exc_class = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrownRetry", 0)
            .expect("Compatible mode fabricates");
        let ctor_frame = |obj: ObjectRef| {
            Frame::new(
                sb,
                FIN_CLASS.to_string(),
                "<init>".to_string(),
                "()V".to_string(),
                None,
                code.clone(),
                Vec::new(),
                4,
                2,
                &[Value::Object(Some(obj))],
            )
        };
        let registered =
            |addr: usize| shared.mem.ref_processor.lock().is_registered_finalizable(addr);
        // A throw at pc 1 with the constructor as the walk's entry frame.
        let throw_at_entry = |obj: ObjectRef| {
            let exc = shared.mem.heap.alloc_object(exc_class, 0);
            let mut t = JvmThread::new(ThreadId(0x0F1D), "r21-finalize-retry");
            t.frames.push(ctor_frame(obj));
            let mut idx = 0usize;
            assert!(unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 1).is_err());
        };
        let init_returns_at =
            |obj: ObjectRef, pc: usize| -> bool { finalizer_init_returned(&shared, &ctor_frame(obj), pc) };

        // Case (a).
        let obj = shared.mem.heap.alloc_object(sb, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        throw_at_entry(obj);
        assert!(!registered(addr), "the failed attempt withdrew the registration");
        assert!(!init_returns_at(obj, 1), "pc 1 precedes super(16): no Object.<init> return");
        assert!(!registered(addr));
        assert!(init_returns_at(obj, 6), "the retry's return registers it again");
        assert!(registered(addr));
        assert!(!init_returns_at(obj, 6), "a second return adds no second Finalizer");
        assert!(registered(addr));

        // Case (b): the object's `<init>` returned (it is registered and
        // constructed); a re-`<init>` that throws before `super(16)` keeps it.
        let obj = shared.mem.heap.alloc_object(sb, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        assert!(!init_returns_at(obj, 6), "registered already: nothing to add");
        throw_at_entry(obj);
        assert!(registered(addr), "a constructed object keeps its registration");

        // An object whose `<init>` never returns stays withdrawn.
        let obj = shared.mem.heap.alloc_object(sb, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        throw_at_entry(obj);
        assert!(!registered(addr));

        // A non-constructor frame (the name gate: `make`, and a six-byte
        // `<`-less name) answers nothing.
        for name in ["make", "zinit>"] {
            let other = Frame::new(
                sb,
                FIN_CLASS.to_string(),
                name.to_string(),
                "()V".to_string(),
                None,
                vec![0xb1],
                Vec::new(),
                4,
                2,
                &[Value::Object(Some(obj))],
            );
            assert!(!finalizer_init_returned(&shared, &other, 0), "{name}");
            assert!(!registered(addr));
        }

        // Wave 19, item 5 of `r21-review-w18-findings-20261019.md`: a body
        // that can write local 0 is refused, so a withdrawn object stored
        // into local 0 (`aload_1; astore_0` after `super(16)`) is neither
        // registered again nor marked.
        let reassigned = Frame::new(
            sb,
            FIN_CLASS.to_string(),
            "<init>".to_string(),
            "()V".to_string(),
            None,
            vec![0x2a, 0x10, 0x10, 0xb7, 0x00, 0x06, 0x2b, 0x4b, 0xb1],
            Vec::new(),
            4,
            2,
            &[Value::Object(Some(obj)), Value::Object(Some(obj))],
        );
        assert!(!finalizer_init_returned(&shared, &reassigned, 8), "astore_0: refused");
        assert!(!registered(addr));
        // The same object under the plain body registers again (control).
        assert!(init_returns_at(obj, 6));
        assert!(registered(addr));
        // Wave 19, item 6: the gate's switch and the processor's agree.
        assert_eq!(
            shared.config.finalizer_reregister_at_init_return,
            shared.mem.ref_processor.lock().reregister_at_init_return()
        );
    }

    /// Wave 19 (items 5 and 6 of `r21-review-w18-findings-20261019.md`): the
    /// hook's lock-free body check. It refuses every local-0 write and a
    /// `return` before any `invokespecial`, and accepts a `return` after one.
    #[test]
    fn the_init_return_check_refuses_local_0_writes_and_an_early_return() {
        let frame = |code: Vec<u8>| {
            Frame::new(
                ClassId::new(0x00F1_0C77),
                "cratonvm/test/W19Ctor".to_string(),
                "<init>".to_string(),
                "()V".to_string(),
                None,
                code,
                Vec::new(),
                4,
                3,
                &[],
            )
        };
        let check = |code: Vec<u8>, pc: usize| {
            ctor_return_names_its_initialized_receiver(&frame(code), pc)
        };
        // aload_0; invokespecial #1; return
        assert!(check(vec![0x2a, 0xb7, 0, 1, 0xb1], 4));
        // A bare `return` (unverified: Object.<init> never ran).
        assert!(!check(vec![0xb1], 0));
        // A return before the invokespecial.
        assert!(!check(vec![0xb1, 0x2a, 0xb7, 0, 1, 0xb1], 0));
        assert!(check(vec![0xb1, 0x2a, 0xb7, 0, 1, 0xb1], 5));
        // Writes to local 0: astore_0, astore 0, wide astore 0, iinc 0.
        assert!(!check(vec![0x2a, 0xb7, 0, 1, 0x2b, 0x4b, 0xb1], 6));
        assert!(!check(vec![0x2a, 0xb7, 0, 1, 0x2b, 0x3a, 0, 0xb1], 7));
        assert!(!check(vec![0x2a, 0xb7, 0, 1, 0x2b, 0xc4, 0x3a, 0, 0, 0xb1], 9));
        assert!(!check(vec![0x2a, 0xb7, 0, 1, 0x84, 0, 1, 0xb1], 7));
        // A store to another local is fine.
        assert!(check(vec![0x2a, 0xb7, 0, 1, 0x2b, 0x4c, 0xb1], 6));
        // Wave 21: subroutines are refused (jsr, jsr_w, ret, wide ret).
        assert!(!check(vec![0x2a, 0xb7, 0, 1, 0xa8, 0, 3, 0xb1], 7));
        assert!(!check(vec![0x2a, 0xb7, 0, 1, 0xa9, 1, 0xb1], 6));
        assert!(!check(vec![0x2a, 0xb7, 0, 1, 0xc4, 0xa9, 0, 1, 0xb1], 8));
    }

    /// Wave 21 (item 8 of `r21-review-w21-findings-20261019.md`): JEP 513
    /// constructor prologues. A `try`/`catch` before `super(..)` (the
    /// unwinder's prefix scan refused a handler that starts before the first
    /// `invokespecial` until wave 26, and still does under
    /// `CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS=0`) and a loop before it
    /// are accepted by the `<init>` return check, which asks only about local 0 and subroutines; switches
    /// decode.
    #[test]
    fn the_init_return_check_accepts_jep_513_prologues() {
        use cratonvm_reader::attribute::ExceptionTableEntry;
        // 0 invokestatic #2; 3 goto 7; 6 astore_1 (handler of [0, 3));
        // 7 aload_0; 8 invokespecial #1; 11 return
        let try_prologue = vec![0xb8, 0, 2, 0xa7, 0, 4, 0x4c, 0x2a, 0xb7, 0, 1, 0xb1];
        let table = vec![ExceptionTableEntry {
            start_pc: 0,
            end_pc: 3,
            handler_pc: 6,
            catch_type: 0,
        }];
        let frame = Frame::new(
            ClassId::new(0x00F1_0C78),
            "cratonvm/test/W21Prologue".to_string(),
            "<init>".to_string(),
            "()V".to_string(),
            None,
            try_prologue.clone(),
            table.clone(),
            4,
            3,
            &[],
        );
        assert!(
            ctor_prefix_scan(&try_prologue, &table, false, &|_| true).is_none(),
            "the withdraw side's scan refuses it with CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS=0"
        );
        // Wave 26 (lane finprologue): the row covers only [0, 3), before the
        // `invokespecial` at 8, so the scan accepts it by default.
        assert_eq!(
            ctor_prefix_scan(&try_prologue, &table, true, &|_| true).map(|(first, _)| first),
            Some(8)
        );
        assert!(ctor_return_names_its_initialized_receiver(&frame, 11), "the return check accepts it");
        assert!(!ctor_return_names_its_initialized_receiver(&frame, 7), "before super(..)");

        // 0 iconst_0; 1 istore_1; 2 iload_1; 3 iconst_3; 4 if_icmpge 13;
        // 7 iinc 1 1; 10 goto 2; 13 aload_0; 14 invokespecial #1; 17 return
        let loop_prologue = vec![
            0x03, 0x3c, 0x1b, 0x06, 0xa2, 0, 9, 0x84, 1, 1, 0xa7, 0xff, 0xf8, 0x2a, 0xb7, 0, 1, 0xb1,
        ];
        assert!(ctor_body_keeps_receiver_and_calls_before(&loop_prologue, 17));
        // 0 iload_1; 1 tableswitch (pad 2) default/low/high = 0..0 -> 1 target;
        // then aload_0; invokespecial #1; return.
        let mut switch = vec![0x1b, 0xaa, 0, 0];
        for word in [23i32, 0, 0, 23] {
            switch.extend_from_slice(&word.to_be_bytes());
        }
        switch.extend_from_slice(&[0, 0, 0, 0x2a, 0xb7, 0, 1, 0xb1]);
        let ret = switch.len() - 1;
        assert!(ctor_body_keeps_receiver_and_calls_before(&switch, ret));
        // A truncated body is refused.
        assert!(!ctor_body_keeps_receiver_and_calls_before(&[0x2a, 0xb7, 0], 3));
    }

    /// Wave 26 (lane finprologue, `CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS`;
    /// probe `R21Fz8FinPrologue1`): a JEP 513 `try`/`catch` before
    /// `super(..)` no longer refuses the unwinder's proof, so a throw before
    /// `super(..)` is proven (and the constructor stays interpreted); a row
    /// that covers the initializing call, or sends control from past it back
    /// below it, still refuses.
    #[test]
    fn the_unwinder_proof_accepts_a_jep_513_prologue_handler() {
        use cratonvm_reader::attribute::ExceptionTableEntry;
        // 0 invokestatic #2; 3 istore_3; 4 goto 11; 7 astore 4 (handler);
        // 9 iconst_m1; 10 istore_3; 11 iload_2; 12 iconst_1; 13 if_icmpne 24;
        // 16 new #5; 19 dup; 20 invokespecial #5 (another class's <init>);
        // 23 athrow; 24 aload_0; 25 iload_3; 26 invokespecial #1; 29 return
        let code = vec![
            0xb8, 0, 2, 0x3e, 0xa7, 0, 7, 0x3a, 4, 0x02, 0x3e, 0x1c, 0x04, 0xa0, 0, 11, 0xbb, 0, 5,
            0x59, 0xb7, 0, 5, 0xbf, 0x2a, 0x1d, 0xb7, 0, 1, 0xb1,
        ];
        let may = |i: u16| i != 5;
        let row = |start_pc: u16, end_pc: u16, handler_pc: u16| ExceptionTableEntry {
            start_pc,
            end_pc,
            handler_pc,
            catch_type: 0,
        };
        let prologue = [row(0, 3, 7)];
        assert!(ctor_throw_precedes_initialization_table(&code, 23, &prologue, true, &may));
        assert!(ctor_throw_precedes_initialization_table(&code, 0, &prologue, true, &may));
        assert!(!ctor_throw_precedes_initialization_table(&code, 23, &prologue, false, &may), "=0");
        assert!(!ctor_throw_precedes_initialization_table(&code, 29, &prologue, true, &may), "after");
        assert!(ctor_may_throw_before_initialization_table(&code, &prologue, true, &may));
        assert!(!ctor_may_throw_before_initialization_table(&code, &prologue, false, &may));
        // A row that covers the initializing call (its handler would run on
        // an object whose super(..) threw) refuses.
        let covers_call = [row(0, 29, 7)];
        assert!(!ctor_throw_precedes_initialization_table(&code, 23, &covers_call, true, &may));
        // A row past the call whose handler lies before it refuses.
        let backward = [row(29, 30, 7)];
        assert!(!ctor_throw_precedes_initialization_table(&code, 23, &backward, true, &may));
        // A forward handler is allowed either way (the pre-wave-26 rule).
        let forward = [row(0, 3, 29)];
        assert!(ctor_throw_precedes_initialization_table(&code, 23, &forward, false, &may));
        // An empty row is never an allowance.
        let empty = [row(3, 3, 7)];
        assert!(!ctor_throw_precedes_initialization_table(&code, 23, &empty, true, &may));
    }

    /// Wave 22 (lane finalize; item 8 of `r21-review-w21-findings-20261019.md`,
    /// `CRATONVM_FINALIZER_INIT_RETURN_SCAN_MEMO`): the memoized answer is the
    /// decode's at every return pc; the cell on the frame's quickened stream is
    /// filled once and then read instead of decoding; a stream of other bytes
    /// (a redefined body) is never the frame's cell; the switch off decodes.
    #[test]
    fn the_init_return_scan_memo_agrees_with_the_decode() {
        use std::sync::Arc;
        let bodies: [&[u8]; 7] = [
            &[0x2a, 0xb7, 0, 1, 0xb1],
            &[0xb1, 0x2a, 0xb7, 0, 1, 0xb1],
            &[0x2a, 0xb7, 0, 1, 0x2b, 0x4b, 0xb1],
            &[0x2a, 0xb7, 0, 1, 0xa9, 1, 0xb1],
            &[0xb1],
            &[0x2a, 0xb7, 0],
            &[0x2a, 0xb7, 0, 1, 0x2a, 0xb7, 0, 2, 0xb1],
        ];
        for body in bodies {
            let facts = ctor_body_receiver_facts(body);
            assert_ne!(facts, CTOR_FACTS_UNKNOWN, "{body:?}");
            for pc in 0..=body.len() + 1 {
                assert_eq!(
                    ctor_facts_answer(facts, pc),
                    ctor_body_keeps_receiver_and_calls_before(body, pc),
                    "{body:?} at {pc}"
                );
            }
        }
        let padded = |body: &[u8]| -> Arc<[u8]> {
            let mut v = body.to_vec();
            v.extend_from_slice(&[0, 0]);
            Arc::from(v.into_boxed_slice())
        };
        let record = |code: Arc<[u8]>, stream_of: &Arc<[u8]>| {
            let cached = Arc::new(crate::classloading::resolution::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: ClassId::new(0x00F1_0C79),
                    class_name: Arc::from("cratonvm/test/W22Memo"),
                    method_name: Arc::from("<init>"),
                    method_descriptor: Arc::from("()V"),
                    source_file: None,
                    code,
                    exception_table: Arc::from(Vec::new()),
                    max_stack: 4,
                    max_locals: 3,
                    num_params: 0,
                    is_synchronized: false,
                    is_static: false,
                },
            ));
            let stream = cratonvm_reader::QuickenedCode::build(stream_of).expect("decodes");
            assert!(cached.quickened.set(Some(Arc::clone(&stream))).is_ok());
            (cached, stream)
        };
        // 0 return; 1 aload_0; 2 invokespecial #1; 5 return
        let code = padded(&[0xb1, 0x2a, 0xb7, 0, 1, 0xb1]);
        let (cached, stream) = record(Arc::clone(&code), &code);
        let frame = Frame::new_cached_unpooled(cached, &[]);
        assert!(ctor_receiver_facts_cell(&frame).is_some(), "the record's own stream");
        assert_eq!(stream.ctor_receiver_facts(), CTOR_FACTS_UNKNOWN);
        assert!(!ctor_return_names_its_initialized_receiver_in(&frame, 0, true));
        assert_eq!(stream.ctor_receiver_facts(), 2 + 2, "the first invokespecial is at pc 2");
        assert!(ctor_return_names_its_initialized_receiver_in(&frame, 5, true));
        // A poisoned cell answers: the memo is read, not a decode.
        stream.set_ctor_receiver_facts(CTOR_FACTS_REFUSED);
        assert!(!ctor_return_names_its_initialized_receiver_in(&frame, 5, true));
        assert!(
            ctor_return_names_its_initialized_receiver_in(&frame, 5, false),
            "the switch off decodes"
        );
        // A stream that pins other bytes (equal or not) is not this frame's cell.
        let other = padded(&[0xb1, 0x2a, 0xb7, 0, 1, 0xb1]);
        let (cached2, _) = record(Arc::clone(&code), &other);
        let frame2 = Frame::new_cached_unpooled(cached2, &[]);
        assert!(ctor_receiver_facts_cell(&frame2).is_none());
        assert!(ctor_return_names_its_initialized_receiver_in(&frame2, 5, true), "decodes");
    }

    /// Wave 19 source pin (item 6): the `return` arm's gate bit is armed by
    /// both switches.
    #[test]
    fn the_init_return_gate_bit_is_armed_by_the_reregister_switch() {
        let src = include_str!("../interpreter.rs").replace("\r\n", "\n");
        let at = src.find("DG_FINALIZER_INIT_RETURN,\n").expect("the gate word row");
        let row = &src[src[..at].rfind("(\n").expect("the row's open paren")..at];
        assert!(row.contains("finalizer_register_at_object_init_enabled(&shared.config)"));
        assert!(row.contains("shared.config.finalizer_reregister_at_init_return"));
    }

    /// Wave 14 (`R21Fz3FinCtorChain1` `holderCatchInner`): an interpreted
    /// creator whose `try` covers `new Bf2(v)` (javac: the reference only on
    /// the operand stack) withdraws the unconstructed object's registration;
    /// the same creator with the object in a local keeps it (the retry shape).
    #[test]
    fn a_covered_creator_withdraws_unless_a_local_holds_the_object() {
        use crate::threading::jvm_thread::ThreadId;
        use cratonvm_reader::attribute::ExceptionTableEntry;
        if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        // Wave 15: a hand-built finalizable class (the synthetic JDK's
        // `StringBuilder` has no `()V` bytecode).
        let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
        if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
            return;
        }
        let exc_class = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrownCovered", 0)
            .expect("Compatible mode fabricates");
        let registered_after = |holds: bool| -> bool {
            let obj = shared.mem.heap.alloc_object(sb, num_fields);
            let addr = obj.as_ptr() as usize;
            shared.register_finalizable(addr);
            let exc = shared.mem.heap.alloc_object(exc_class, 0);
            let mut t = JvmThread::new(ThreadId(0x0F1E), "r21-finalize-covered");
            let held = [Value::Object(Some(obj))];
            // The creator: `invokespecial #1; return; athrow`, a row over the
            // invoke with its handler at the `athrow`.
            t.frames.push(Frame::new(
                ClassId::new(0x00F1_0C05),
                "cratonvm/test/R21CoveredCreator".to_string(),
                "make".to_string(),
                "()V".to_string(),
                None,
                vec![0xb7, 0, 1, 0xb1, 0xbf],
                vec![ExceptionTableEntry {
                    start_pc: 0,
                    end_pc: 3,
                    handler_pc: 4,
                    catch_type: 0,
                }],
                4,
                2,
                if holds { &held[..] } else { &[] },
            ));
            t.frames[0].last_instr_pc = 0;
            t.frames.push(Frame::new(
                sb,
                FIN_CLASS.to_string(),
                "<init>".to_string(),
                "()V".to_string(),
                None,
                code.clone(),
                Vec::new(),
                4,
                2,
                &[Value::Object(Some(obj))],
            ));
            let mut idx = 1usize;
            // The creator's catch-all row catches it: `Ok`, handler installed.
            assert!(unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 1).is_ok());
            assert_eq!(idx, 0, "caught in the creator");
            shared.mem.ref_processor.lock().is_registered_finalizable(addr)
        };
        assert!(!registered_after(false), "javac's shape: withdrawn although caught");
        assert!(registered_after(true), "a local could retry <init>: kept");
    }

    /// Wave 17 (lane finalize-reinit, items 1 and 2 of
    /// `r21-review-w16-findings-20261019.md`), on a hand-built hierarchy
    /// `Root > Quiet > Base > Fin > Leaf`. `Base(String)` throws before its
    /// initializing call; `Quiet` has no constructor that can; `Fin` declares
    /// a finalizer and `Leaf` inherits it. Only `Leaf` reaches the listener
    /// (`Fin` was "loaded as its superclass").
    /// - Item 1: the wave-13 walk stopped at `Fin` and recorded nothing; the
    ///   walk past it records `Base`, and the bar refuses `Base(String)`.
    /// - Item 2: `Quiet` is recorded although nothing in it throws now; once
    ///   its constructor is replaced by a throwing one (a redefinition), the
    ///   bar refuses it with no further define.
    #[test]
    fn a_finalizable_superclass_does_not_end_the_ancestor_walk() {
        use crate::classloading::{Class, ClassLoaderId, ClassState};
        use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
        use cratonvm_reader::class_file_version::ClassFileVersion;
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        use cratonvm_reader::method::ClassFileMethod;
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let config = &shared.config;
        if !finalizer_register_at_object_init_enabled(config)
            || !finalizer_ctor_stays_interpreted_enabled(config)
            || !finalizer_ancestor_ctor_stays_interpreted_enabled(config)
        {
            return; // A kill switch: the residual.
        }
        const THROWS: &str = "(Ljava/lang/String;)V";
        // `aconst_null; athrow`: throws before any initializing call.
        let throwing_ctor = || ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: cratonvm_types::intern_arc("<init>"),
            descriptor: cratonvm_types::intern_arc(THROWS),
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(CodeAttribute {
                max_stack: 1,
                max_locals: 2,
                code: cratonvm_reader::ByteView::from_vec(vec![0x01, 0xbf]),
                exception_table: vec![],
                attributes: vec![],
            })))],
        };
        let class = |id: ClassId,
                     name: &str,
                     superclass: Option<ClassId>,
                     methods: Vec<ClassFileMethod>,
                     has_finalizer: bool| Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass,
            interfaces: vec![],
            fields: vec![],
            methods,
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        };
        let (quiet, base, fin, leaf) = {
            let mut cm = shared.classes.class_manager.write();
            let mut add = |name: &str, sup: Option<ClassId>, m: Vec<ClassFileMethod>, f: bool| {
                let id = cm.class_store.next_id();
                cm.class_store.add(class(id, name, sup, m, f));
                id
            };
            let root = add("cratonvm/test/W17Root", None, vec![], false);
            let quiet = add("cratonvm/test/W17Quiet", Some(root), vec![], false);
            let base = add("cratonvm/test/W17Base", Some(quiet), vec![throwing_ctor()], false);
            let fin = add("cratonvm/test/W17Fin", Some(base), vec![], true);
            let leaf = add("cratonvm/test/W17Leaf", Some(fin), vec![], true);
            (quiet, base, fin, leaf)
        };
        let walk = |past: bool| {
            let cm = shared.classes.class_manager.read();
            finalizable_define_ancestors(&shared.config, past, cm.class_store(), leaf)
        };
        assert!(walk(false).is_empty(), "wave 13: the walk stopped at the finalizable Fin");
        let reached = walk(true);
        assert_eq!(
            reached.iter().map(|a| (a.id, a.stale)).collect::<Vec<_>>(),
            vec![(base, true), (quiet, false)],
            "past Fin, up to (not including) the root: {reached:?}"
        );
        assert!(reached.iter().all(|a| a.id != fin && a.id != leaf));
        assert!(!finalizer_bars_compiling(&shared, base, "<init>", THROWS), "unrecorded yet");

        shared.note_finalizable_define(leaf);
        if shared.finalizer_ancestor_walk_past_finalizable {
            assert!(
                finalizer_bars_compiling(&shared, base, "<init>", THROWS),
                "item 1: Base(String) stays interpreted"
            );
        }
        if !shared.finalizer_ancestor_record_reachable {
            return;
        }
        assert!(
            shared
                .classes
                .class_manager
                .read()
                .class_store()
                .is_finalizable_ancestor(quiet),
            "item 2: reachable, recorded although nothing throws"
        );
        assert!(!finalizer_bars_compiling(&shared, quiet, "<init>", THROWS), "no such ctor yet");
        // A redefinition replaces Quiet's methods with a throwing constructor.
        shared
            .classes
            .class_manager
            .write()
            .class_store
            .get_mut(quiet)
            .expect("defined")
            .methods = vec![throwing_ctor()];
        assert!(
            finalizer_bars_compiling(&shared, quiet, "<init>", THROWS),
            "item 2: the bar reads the redefined bytes"
        );
    }

    /// Wave 13 (residual 1 of the page): `IndexOutOfBoundsException(int)`
    /// (`super("Index out of range: " + index)`, an `invokedynamic` before
    /// `super(..)`) has no finalizer of its own. The define of a finalizable
    /// subclass (here `ArrayIndexOutOfBoundsException`, made finalizable in
    /// this VM's store only) names it as stale, and once the listener's
    /// record is made (`ClassStore::mark_finalizable_ancestor`) it stays
    /// interpreted; `IndexOutOfBoundsException(String)` (`super(s)`) still
    /// compiles. Before the record the bar answers from the class alone
    /// (review rev-jit w13 item 3b: no prefix decode, no subtree walk).
    #[test]
    fn an_ancestor_constructor_of_a_loaded_finalizable_class_stays_interpreted() {
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let config = &shared.config;
        if !finalizer_register_at_object_init_enabled(config)
            || !finalizer_ctor_stays_interpreted_enabled(config)
            || !finalizer_ancestor_ctor_stays_interpreted_enabled(config)
        {
            return; // A kill switch: the residual.
        }
        const BASE: &str = "java/lang/IndexOutOfBoundsException";
        let leaf = shared
            .load_class_concurrent("java/lang/ArrayIndexOutOfBoundsException")
            .expect("load ArrayIndexOutOfBoundsException");
        let shape = {
            let cm = shared.classes.class_manager.read();
            let store = cm.class_store();
            store
                .get(leaf)
                .and_then(|c| c.superclass)
                .and_then(|id| store.get(id).map(|b| (id, b)))
                .filter(|(_, b)| &*b.name == BASE && !b.has_finalizer)
                .map(|(id, b)| {
                    (
                        id,
                        class_ctor_may_throw_before_initialization(store, b, "(I)V"),
                        class_ctor_may_throw_before_initialization(
                            store,
                            b,
                            "(Ljava/lang/String;)V",
                        ),
                    )
                })
        };
        let Some((base, int_may_throw, string_may_throw)) = shape else {
            return; // Another JDK's hierarchy.
        };
        if !int_may_throw || string_may_throw {
            return; // Another JDK's bodies: the shapes this test names are gone.
        }
        let stale_now = || {
            let cm = shared.classes.class_manager.read();
            finalizable_define_stale_ancestors(&shared.config, cm.class_store(), leaf)
        };
        assert!(
            !finalizer_bars_compiling(&shared, base, "<init>", "(I)V"),
            "no finalizable subclass defined yet"
        );
        assert!(stale_now().is_empty(), "the leaf has no finalizer yet");

        shared
            .classes
            .class_manager
            .write()
            .class_store
            .get_mut(leaf)
            .expect("loaded")
            .has_finalizer = true;

        let stale = stale_now();
        assert!(
            stale.iter().any(|(id, name)| *id == base && name == BASE),
            "the define names the ancestor: {stale:?}"
        );
        assert!(
            stale.iter().all(|(id, name)| *id != leaf && name != "java/lang/Object"),
            "neither the new class itself nor java/lang/Object: {stale:?}"
        );
        assert!(
            !finalizer_bars_compiling(&shared, base, "<init>", "(I)V"),
            "unrecorded: the bar reads the class store's record, not the subtree"
        );
        {
            // The listener's record (`vm_init.rs`).
            let mut cm = shared.classes.class_manager.write();
            for (id, _) in &stale {
                assert!(cm.class_store.mark_finalizable_ancestor(*id));
            }
        }
        assert!(
            finalizer_bars_compiling(&shared, base, "<init>", "(I)V"),
            "throws before super(..) and reaches a finalizable object"
        );
        assert!(
            !finalizer_bars_compiling(&shared, base, "<init>", "(Ljava/lang/String;)V"),
            "super(s) cannot throw before it"
        );
        assert!(!finalizer_bars_compiling(&shared, base, "toString", "()Ljava/lang/String;"));
    }

    /// Wave 13 (review rev-jit w13 item 2): the entry is a JNI native's own
    /// upcall only at the row's interpreter depth with the row's JIT chain.
    #[test]
    fn a_jni_upcall_entry_is_the_innermost_row_at_its_depth() {
        use crate::threading::jvm_thread::ThreadId;
        let mut t = JvmThread::new(ThreadId(0x0F1C), "r21-finalize-jni-row");
        assert!(!entry_is_jni_upcall(&t, 1, 0), "no JNI native runs");
        t.jni_native_frames.push((1, ClassId::new(0x00F1_0C03), 0x1000, 0));
        assert!(entry_is_jni_upcall(&t, 1, 0));
        assert!(!entry_is_jni_upcall(&t, 2, 0), "an interpreted frame in between");
        assert!(!entry_is_jni_upcall(&t, 1, 1), "a compiled method in between");
        t.jni_native_frames.push((3, ClassId::new(0x00F1_0C03), 0x1000, 0));
        assert!(!entry_is_jni_upcall(&t, 1, 0), "only the innermost native counts");
        assert!(entry_is_jni_upcall(&t, 3, 0));
    }

    /// Wave 13 (review rev-jit w13 item 2, page
    /// `r21-jni-reinit-entry-withdraw-drops-registration-20261019.md`): a JNI
    /// `<init>` call on an object some earlier construction registered, whose
    /// constructor throws before `super(..)`, keeps the registration (HotSpot
    /// finalizes the object once). The walk's entry is the JNI upcall, so it
    /// only notes the object; the note is what the JNI door takes when the
    /// registration is its own call's (`jni_constructor_call_withdrawable`).
    #[test]
    fn a_jni_reinit_upcall_keeps_an_earlier_registration() {
        use crate::threading::jvm_thread::ThreadId;
        if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        // Wave 15: a hand-built finalizable class (the synthetic JDK's
        // `StringBuilder` has no `()V` bytecode).
        let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
        if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
            return;
        }
        let exc_class = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrownJni", 0)
            .expect("Compatible mode fabricates");
        let obj = shared.mem.heap.alloc_object(sb, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        let epoch = shared.mem.address_epoch();
        let exc = shared.mem.heap.alloc_object(exc_class, 0);
        let mut t = JvmThread::new(ThreadId(0x0F1D), "r21-finalize-jni-reinit");
        // The Java frame that called the JNI native: `invokestatic #1; return`.
        t.frames.push(Frame::new(
            ClassId::new(0x00F1_0C04),
            "cratonvm/test/R21JniCaller".to_string(),
            "callNative".to_string(),
            "()V".to_string(),
            None,
            vec![0xb8, 0, 1, 0xb1],
            Vec::new(),
            4,
            2,
            &[],
        ));
        t.frames[0].last_instr_pc = 0;
        let jit_depth = crate::jit::conservative_roots::current_thread_jit_depth();
        let row_jit_depth = u32::try_from(jit_depth).unwrap_or(u32::MAX);
        t.jni_native_frames
            .push((1, ClassId::new(0x00F1_0C04), 0x2000, row_jit_depth));
        t.frames.push(Frame::new(
            sb,
            FIN_CLASS.to_string(),
            "<init>".to_string(),
            "()V".to_string(),
            None,
            code.clone(),
            Vec::new(),
            4,
            2,
            &[Value::Object(Some(obj))],
        ));
        let mut idx = 1usize;
        assert!(unwind_to_handler(&shared, &mut t, &mut idx, 1, exc, 1).is_err());
        assert!(
            shared.mem.ref_processor.lock().is_registered_finalizable(addr),
            "the earlier construction's registration stays"
        );
        // The door of a call whose registration WAS its own takes the note.
        assert!(
            shared.withdraw_unconstructed_finalizable_noted(addr, epoch),
            "noted for the JNI door"
        );
    }

    /// Wave 14 (items 2 and 3 of `r21-review-w14-findings-20261019.md`): the
    /// gate's refusal is memoized per class id, not sealed by name, and the
    /// ancestor record the gate re-reads after a memo fill is what
    /// `mark_finalizable_ancestor` set.
    #[test]
    fn the_gate_memoizes_a_refusal_per_class_id_and_rereads_the_record() {
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let config = &shared.config;
        if !finalizer_register_at_object_init_enabled(config)
            || !finalizer_ctor_stays_interpreted_enabled(config)
            || !finalizer_ancestor_ctor_stays_interpreted_enabled(config)
        {
            return;
        }
        let leaf = shared
            .load_class_concurrent("java/lang/ArrayIndexOutOfBoundsException")
            .expect("load ArrayIndexOutOfBoundsException");
        let other = shared
            .load_class_concurrent("java/lang/StringBuilder")
            .expect("load StringBuilder");
        let base = {
            let cm = shared.classes.class_manager.read();
            let store = cm.class_store();
            let Some(base) = store.get(leaf).and_then(|c| c.superclass) else {
                return;
            };
            let throws = store.get(base).is_some_and(|b| {
                &*b.name == "java/lang/IndexOutOfBoundsException"
                    && !b.has_finalizer
                    && class_ctor_may_throw_before_initialization(store, b, "(I)V")
            });
            if !throws {
                return; // Another JDK's hierarchy or body.
            }
            base
        };
        assert!(!finalizer_ancestor_recorded(&shared, base));
        assert!(!finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"));
        assert!(
            shared.jit.finalizer_bar_refusals.read().is_empty(),
            "an allowed answer records nothing"
        );
        shared
            .classes
            .class_manager
            .write()
            .class_store
            .get_mut(leaf)
            .expect("loaded")
            .has_finalizer = true;
        assert!(shared
            .classes
            .class_manager
            .write()
            .class_store
            .mark_finalizable_ancestor(base));
        assert!(finalizer_ancestor_recorded(&shared, base), "the gate's re-read sees the record");
        assert!(finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"));
        assert!(finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"), "answered from the row");
        {
            let refusals = shared.jit.finalizer_bar_refusals.read();
            assert_eq!(refusals.len(), 1, "keyed by the declaring class id only");
            assert_eq!(
                refusals
                    .get(&base)
                    .map(|(_, d)| d.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
                Some(vec!["(I)V".to_string()])
            );
        }
        assert!(!finalizer_bars_compiling_memoized(&shared, other, "<init>", "()V"));
        assert!(!finalizer_bars_compiling_memoized(&shared, base, "toString", "()Ljava/lang/String;"));

        // Wave 18 (`CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH`): the row carries
        // the redefine epoch it was filled in. Simulate a redefinition that
        // makes the constructor unable to throw before init: under the
        // switch the old row no longer answers; off, it still does.
        let filled = shared.jit.jit_cache.redefine_epoch();
        assert_eq!(shared.jit.finalizer_bar_refusals.read().get(&base).map(|(e, _)| *e), Some(filled));
        shared
            .classes
            .class_manager
            .write()
            .class_store
            .get_mut(base)
            .expect("loaded")
            .methods
            .retain(|m| !(&*m.name == "<init>" && &*m.descriptor == "(I)V"));
        shared.jit.jit_cache.bump_redefine_epoch();
        assert_eq!(
            finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"),
            !shared.finalizer_bar_refusal_epoch,
            "a stale row answers only without the switch"
        );
        // Wave 24 (lane flipprep, flip review item 8): under the switch the
        // allowed re-ask drops the stale descriptor, and the row with it, so
        // later visits do not re-ask through it; off, the row stays as filled.
        assert_eq!(
            shared.jit.finalizer_bar_refusals.read().get(&base).map(|(e, _)| *e),
            if shared.finalizer_bar_refusal_epoch { None } else { Some(filled) },
            "an allowed re-ask clears the stale row only under the switch"
        );
        if shared.finalizer_bar_refusal_epoch {
            assert!(!finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"));
            assert!(shared.jit.finalizer_bar_refusals.read().get(&base).is_none());
        }
    }

    /// Wave 14 source pins (`interpreter.rs`): the refusal is no longer
    /// sealed into the name-keyed `jit_skip_set`; an allowed `jit_gate_pass`
    /// fill for a constructor re-reads the ancestor record and drops its row;
    /// the eager first-call door asks the bar again before its `put`.
    #[test]
    fn the_execute_gate_rereads_the_record_and_seals_no_finalizer_refusal() {
        let src = include_str!("../interpreter.rs").replace("\r\n", "\n");
        let seal = src
            .find("if static_skip_reason.is_some() || fjp_skip || native_skip || clinit_skip {")
            .expect("the static seal");
        let seal_end = seal + src[seal..].find("\n        } else {").expect("its end");
        assert!(
            !src[seal..seal_end].contains("finalizer_skip"),
            "a finalizer refusal must not be sealed by class name"
        );
        let fill = src
            .find("shared.jit.jit_gate_pass.write().insert(")
            .expect("the gate-pass fill");
        let after = &src[fill..];
        let reread = after
            .find("finalizer_ancestor_recorded(shared, class_id)")
            .expect("the fill re-reads the ancestor record");
        assert!(reread < 2000, "right after the insert");
        let drop_at = after[reread..]
            .find("shared.jit.jit_gate_pass.write().remove(&gate_pass_key)")
            .expect("and drops the row");
        assert!(drop_at < 400, "in the same block");
        let put = src.find("let stale = crate::jit::helpers::varhandle_thin_bound_artifact_is_stale(")
            .expect("the eager door's stale test");
        let tail = &src[put..];
        let asked = tail.find("|| finalizer_bars_compiling(").expect("the eager door asks the bar again");
        let put_at = tail.find("jit_cache.put(").expect("the eager door's put");
        assert!(asked < put_at && put_at < 4000, "before its put");
    }
}
