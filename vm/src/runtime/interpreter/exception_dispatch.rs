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
/// entirely if no handler exists), and `find_exception_handler` ->
/// `find_exception_handler_impl` lazily loads an unresolved catch-type class on
/// a cache miss (`load_class_concurrent`). That load does NOT run `<clinit>`
/// and does not allocate on the Java heap (r9w9 exc9 traced it: parse + define
/// under the class-manager write lock, JIT CHA invalidation, and deferred
/// JVMTI hooks that take only ids — see
/// `docs/internal/fixed-bugs/exception-handler-search-callers-hold-raw-oops-across-a-lazy-catch-type-load-FIXED-20260918.md`),
/// so today it cannot collect. The pin is kept anyway: it costs one `Vec` push,
/// and it is what stays correct if that load ever grows a Java-running step (a
/// `ClassFileLoadHook` transformer, a user loader). Once a frame is popped it no longer roots the
/// propagating exception, and nothing else does until a handler is found (which
/// pushes it onto a frame's stack) or the method returns it as an `Err`. So it
/// is pinned in `native_pin_roots` for the whole walk, and re-read from there
/// on every iteration — a moving collector rewrites the pin slot in place, so
/// the local copy taken before a GC is stale.
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

pub(super) fn unwind_to_handler(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: &mut usize,
    initial_frame_idx: usize,
    exc: ObjectRef,
    invoke_pc: usize,
) -> Result<(), MethodCallFailed> {
    let mut exc_pc = invoke_pc;
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    loop {
        // Re-read through the pin: a collection during the previous iteration's
        // lazy class load may have relocated the throwable.
        let current_exc = thread.native_pin_roots[pin_base];
        match find_exception_handler(shared, &thread.frames[*frame_idx], exc_pc, current_exc) {
            Some((handler_pc, _stale_exc_ref)) => {
                // Re-read through the pin HERE too, not only at the top of the
                // loop. The search that just answered may itself have run the
                // lazy catch-type load the note above describes, and the ref it
                // hands back is the copy it was given BEFORE that load. The pin
                // slot is the only copy a moving collector rewrote.
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
                fire_jvmti_exception_catch(
                    shared.vm_identity,
                    &thread.frames[*frame_idx],
                    handler_pc,
                );
                thread.native_pin_roots.truncate(pin_base);
                return Ok(());
            }
            None => {
                if *frame_idx > initial_frame_idx {
                    // T17.Δ — exception-unwind.
                    pop_and_recycle_frame_with_reason(shared, thread, true);
                    *frame_idx -= 1;
                    exc_pc = thread.frames[*frame_idx].last_instr_pc;
                } else {
                    let current_exc = thread.native_pin_roots[pin_base];
                    thread.native_pin_roots.truncate(pin_base);
                    return Err(MethodCallFailed::ExceptionThrown(current_exc));
                }
            }
        }
    }
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

/// Scan the frame's exception table for a handler that covers `pc` and
/// whose `catch_type` matches the thrown exception's class.
///
/// Used by the JIT early-exception path. Unlike `find_exception_handler`,
/// callers here may not know the *exact* PC of the throw site (the JIT
/// executed the entire bytecode method as native code). They must still
/// supply a best-known `pc` (typically the frame's `last_instr_pc`, or 0
/// for a freshly-pushed frame) so that handler matching honors each
/// entry's `[start_pc, end_pc)` range — without that check, a `finally`
/// (catch-all) entry would incorrectly swallow exceptions whose throw
/// site is outside that try region.
///
/// Entries are searched in declaration order; the first matching handler
/// wins, mirroring the JVM spec's handler precedence for nested try/catch.
pub(super) fn find_exception_handler_any_pc(
    shared: &SharedVm,
    frame: &Frame,
    pc: usize,
    exc: ObjectRef,
) -> Option<(usize, ObjectRef)> {
    // Same semantics as `find_exception_handler` (and historically a verbatim
    // copy of its body — round-5 vm #11 noted the 70-line near-duplicate).
    // The two siblings are kept for call-site documentation purposes (one is
    // bytecode-throw routing, the other is JIT-throw routing) but share the
    // implementation below.
    find_exception_handler_impl(shared, frame, pc, exc)
}

/// Find a handler in `frame`'s exception table when the throw-site PC is
/// **unknown** — the case after a JIT-compiled method runs to completion as
/// native code and a callee it dispatched throws.
///
/// The JIT executes the whole bytecode body, so there is no live interpreter
/// PC for the throw site. Passing `0` (a freshly-pushed frame's
/// `last_instr_pc`) to the PC-range check in `find_exception_handler_impl`
/// silently misses every handler whose protected region does not start at 0
/// — e.g. a `try` block that begins a few bytes into the method. That bug
/// made a JIT-compiled `Main.main` propagate a callee exception straight past
/// its own `catch (Throwable)` (Jetty `start.jar` launcher: the launcher's
/// usage-error handling never ran, surfacing a misleading inner NPE instead).
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
pub(super) fn find_exception_handler_pc_unknown(
    shared: &SharedVm,
    frame: &Frame,
    exc: ObjectRef,
) -> Option<(usize, ObjectRef)> {
    let exc_class_id = shared.mem.heap.class_id_of(exc);
    // `frame.code` is padded with 2 trailing bytes for the interpreter's
    // speculative reads; the real bytecode length is `len() - 2`.
    let code_len = frame.code.len().saturating_sub(2);
    let mut cm_guard = shared.classes.class_manager.read();
    cm_guard.get_class(frame.class_id)?;
    for entry in frame.exception_table().iter() {
        // PC unknown → cannot verify range membership. Honour a catch-all only
        // when it spans the entire method (always covers the throw site);
        // otherwise skip it (it could catch an out-of-region exception).
        if entry.catch_type == 0 {
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            if entry.start_pc == 0 && entry.end_pc as usize >= code_len {
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                return Some((entry.handler_pc as usize, exc));
            }
            continue;
        }
        let owning_class = cm_guard.get_class(frame.class_id)?;
        let Some(catch_class_name) = owning_class.constant_pool.get_class_name(entry.catch_type)
        else {
            continue;
        };
        // Owned copy, independent of `cm_guard`'s current borrow, so it
        // survives the lock drop/reacquire below and stays usable in the
        // loader-identity-blind fallback match (see
        // `Class::is_subclass_of_by_name`).
        let catch_class_name_owned = catch_class_name.to_string();
        // Same lazy-load rule as `find_exception_handler_impl`, and for the same
        // reason: never load a catch type through the loader-blind global path
        // when that would MINT a synthetic stub and register it globally,
        // poisoning the name for the custom loader that owns the real class. The
        // by-name test below walks the thrown object's own chain and needs no
        // ClassId. A failed load also still gets that by-name test (it used to
        // `continue` past it).
        let mut catch_class_id =
            cm_guard.find_class_by_name_for_class(&catch_class_name_owned, frame.class_id);
        if catch_class_id.is_none()
            && !cm_guard.would_fabricate_synthetic_stub(&catch_class_name_owned)
        {
            drop(cm_guard);
            let loaded = shared.load_class_concurrent(&catch_class_name_owned);
            cm_guard = shared.classes.class_manager.read();
            catch_class_id = loaded.ok();
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

/// Shared core of [`find_exception_handler`] and
/// [`find_exception_handler_any_pc`].
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
#[inline]
pub(super) fn find_exception_handler_impl(
    shared: &SharedVm,
    frame: &Frame,
    pc: usize,
    exc: ObjectRef,
) -> Option<(usize, ObjectRef)> {
    let exc_class_id = shared.mem.heap.class_id_of(exc);

    let mut cm_guard = shared.classes.class_manager.read();
    // Verify the owning class exists once — hoist this invariant out
    // of the per-entry loop. We re-fetch the (shared-borrowed) class
    // inside the loop to get the constant pool; that's a cheap
    // `HashMap` get against the held read guard.
    cm_guard.get_class(frame.class_id)?;

    for entry in frame.exception_table().iter() {
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
/// Consume an optional reason-9 frame published by the x64 backend and route
/// a pending Java exception with the exact throw bci and reconstructed locals.
///
/// `None` means the compiled method used the historical params-only route.
/// A matching but unmappable frame fails closed by propagating the exception;
/// entering a handler with zeroed non-parameter locals would be a silent
/// miscompile. A foreign nested-callee frame is restored for its owner.
pub(super) fn route_jit_signal_exception(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller_frame_idx: usize,
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
    let precise = match cratonvm_jit::deopt::take_exceptional_frame() {
        Some(rframe)
            if deopt_frame_matches_method(
                &rframe,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            ) =>
        {
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
                let Some(locals) = ir_deopt_locals(&rframe.locals) else {
                    return Err(MethodCallFailed::ExceptionThrown(exc));
                };
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
        // (possibly collected) object pointers.
        Some(_foreign) => None,
        None => None,
    };
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
            if handler_resume_needs_precise_locals(cached) {
                if crate::jit::helpers::rbc6_dbg() {
                    eprintln!(
                        "[rbc6-dbg] route_jit_signal_exception DECLINED {}.{}{}                          — handler needs precise locals and no frame was published",
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
    route_jit_exception_through_method(
        shared,
        thread,
        caller_frame_idx,
        cached,
        throw_pc,
        exc,
        locals,
        deferred_precise.as_ref(),
    )
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
/// The `Frame` sibling of [`jit_local_athrow_pc`], for the sink that has pushed
/// a frame rather than holding a `CachedBytecodeMethod`.
///
/// Same two tests, same reason: the stamp carries no method identity, so it is
/// honoured only when it lands inside one of THIS method's protected ranges and
/// on a real instruction boundary.
pub(super) fn jit_local_athrow_pc_in_frame(frame: &Frame, athrow_bci: i64) -> JitThrowPc {
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
}

pub(super) fn jit_local_athrow_pc_kind(
    cached: &CachedBytecodeMethod,
    athrow_bci: i64,
) -> JitThrowPc {
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
    if let Some(frame) = cratonvm_jit::deopt::take_exceptional_frame() {
        if !deopt_frame_matches_method(&frame, class_name, method_name, descriptor) {
            cratonvm_jit::deopt::restash_exceptional_frame(frame);
        }
    }
}

/// Search the cached exception table and construct the interpreter handler
/// frame from the locals selected by `route_jit_signal_exception`.
/// Find the exception-table entry of `cached` that catches `exc` thrown at
/// `throw_pc`, returning its handler pc.
///
/// Extracted from `route_jit_exception_through_method` so the JIT-to-JIT
/// dispatch path (`vm/src/jit/helpers.rs::route_implicit_exc_through_callee`)
/// can run a compiled callee's own handler without re-executing the callee
/// from its entry — see `run_jit_callee_handler`.
pub(crate) fn find_jit_exception_handler(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    throw_pc: usize,
    exc: ObjectRef,
) -> Option<usize> {
    if cached.exception_table.is_empty() {
        return None;
    }
    let pc_unknown = throw_pc == usize::MAX;
    // `cached.code` is padded with 2 trailing bytes for speculative reads;
    // the real bytecode length is `len() - 2`. Used to recognise a catch-all
    // whose region covers the whole method when the throw PC is unknown.
    let code_len = cached.code.len().saturating_sub(2);
    let exc_class_id = shared.mem.heap.class_id_of(exc);
    let mut handler_pc: Option<usize> = None;
    // HIGH — same fast-path treatment as `find_exception_handler`:
    // hold the class_manager read lock for the duration of the search
    // and avoid `to_string()` on catch-type names.
    let mut cm_guard = shared.classes.class_manager.read();
    for entry in cached.exception_table.iter() {
        // Mirror the PC-range check used by `find_exception_handler_any_pc`:
        // a handler only applies when the throw site is inside its
        // `[start_pc, end_pc)` protected region. Without this check, the
        // first catch-all entry would swallow exceptions thrown anywhere
        // in the method (the original bug here).
        //
        // When the throw PC is unknown (`usize::MAX`, sentinel), we cannot
        // verify range membership. A catch-all (`catch_type == 0`) is honoured
        // only when its protected region spans the whole method
        // (`start_pc == 0 && end_pc >= code_len`) — it then covers the
        // (unknown) throw site, so running its `finally` / monitor-exit
        // cleanup is sound. Narrower catch-all regions are skipped (they could
        // catch an out-of-region exception); typed handlers still match on
        // exception class — wrong-type exceptions cannot be silently swallowed.
        if pc_unknown {
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            if entry.catch_type == 0 && !(entry.start_pc == 0 && entry.end_pc as usize >= code_len)
            {
                continue;
            }
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        } else if throw_pc < entry.start_pc as usize || throw_pc >= entry.end_pc as usize {
            continue;
        }

        if entry.catch_type == 0 {
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            handler_pc = Some(entry.handler_pc as usize);
            break;
        }
        let class = match cm_guard.get_class(cached.declaring_class_id) {
            Some(c) => c,
            None => continue,
        };
        let Some(catch_class_name) = class.constant_pool.get_class_name(entry.catch_type) else {
            continue;
        };
        // Loaded catch type (the common case): answer on the borrowed name,
        // allocation-free, exactly as `find_exception_handler_impl` does.
        if let Some(id) =
            cm_guard.find_class_by_name_for_class(catch_class_name, cached.declaring_class_id)
        {
            if cm_guard.is_subclass_of(exc_class_id, id)
                || cm_guard.is_subclass_of_by_name(exc_class_id, catch_class_name)
            {
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                handler_pc = Some(entry.handler_pc as usize);
                break;
            }
            continue;
        }
        // Owned copy for the loader-identity-blind fallback below (see
        // `Class::is_subclass_of_by_name`) — independent of `cm_guard`'s
        // current borrow so it stays valid across the lock drop/reacquire
        // in the lazy-load branch just below.
        let catch_class_name_owned = catch_class_name.to_string();
        // The synthetic-stub guard of `find_exception_handler_impl`, which this
        // compiled-frame twin lacked: a Quarkus-style custom-loader catch type
        // reached through a JIT-compiled frame was lazily "loaded" as a globally
        // registered stub. See the comment there.
        let mut catch_class_id = None;
        if !cm_guard.would_fabricate_synthetic_stub(&catch_class_name_owned) {
            drop(cm_guard);
            let loaded = shared.load_class_concurrent(&catch_class_name_owned);
            cm_guard = shared.classes.class_manager.read();
            catch_class_id = loaded.ok();
        }
        if catch_class_id.is_some_and(|id| cm_guard.is_subclass_of(exc_class_id, id))
            || cm_guard.is_subclass_of_by_name(exc_class_id, &catch_class_name_owned)
        {
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            handler_pc = Some(entry.handler_pc as usize);
            break;
        }
    }
    drop(cm_guard);
    handler_pc
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
pub(super) fn precise_handler_frame_for(
    cached: &Arc<CachedBytecodeMethod>,
    incoming_args: &[Value],
) -> Option<(usize, PreciseHandlerLocals)> {
    if params_only_callee_handler_frames() {
        return None;
    }
    let rframe = cratonvm_jit::deopt::take_exceptional_frame()?;
    if !deopt_frame_matches_method(
        &rframe,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        cratonvm_jit::deopt::restash_exceptional_frame(rframe);
        return None;
    }
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
    if !cached.is_static {
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
        for _ in 0..m.lock_depth {
            shared.threads.monitors.enter(obj, thread.thread_id);
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

/// Would resuming one of `cached`'s handlers on `this`-plus-parameters alone
/// invent values? See `cratonvm_jit::handler_resume_requires_precise_locals`.
pub(super) fn handler_resume_needs_precise_locals(cached: &Arc<CachedBytecodeMethod>) -> bool {
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
/// Returns `None` when no handler in `cached` covers `throw_pc`, leaving the
/// caller to propagate the exception unchanged — and also when this method
/// needs precise locals but no frame is stashed for it, where resuming would
/// mean inventing them.
///
/// `probes/BindConverterJitProbe.java` is a second, independent witness for the
/// same defect, arrived at from `DevToolsPooledDataSourceAutoConfigurationTests
/// .inMemoryDerbyIsShutdown`: it drives the real `BindConverter.convert` and
/// counts 199,491 wrong results in 200,000 calls before this fix, 0 after. Its
/// `refuse` mode takes the handler out of the picture and passes, which is what
/// identifies the handler resume as the mechanism.
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
pub(crate) enum CalleeHandlerMiss {
    /// No handler in the callee covers this throw site, decided against a
    /// KNOWN throw pc -- the stamped one or a precise frame's bci.
    NotCaught,
    /// A handler matched; resuming it would have needed locals nobody published.
    Declined,
    /// No handler matched, but the throw pc was unknown, so that is not proof.
    Unknown,
}

pub(crate) fn run_jit_callee_handler(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    throw_pc: usize,
    exc: ObjectRef,
    incoming_args: &[Value],
) -> Result<MethodCallResult, CalleeHandlerMiss> {
    let precise = precise_handler_frame_for(cached, incoming_args);
    // A precise frame's bci is the compiled body's own throw site, recorded by
    // the reason-9 stub. It is strictly better than the `athrow_bci` stamp
    // `throw_pc` comes from (which carries no method identity), so prefer it
    // for the handler's `[start_pc, end_pc)` range test.
    let (throw_pc, precise_locals, deferred_precise) = match precise.as_ref() {
        Some((bci, PreciseHandlerLocals::Mapped(locals))) => (*bci, Some(locals.as_slice()), None),
        // A deferred frame counts as "a precise frame was published" for every
        // decision below — that is what the `Some` is asked. It just cannot be
        // READ until the handler is found.
        Some((bci, PreciseHandlerLocals::Deferred(rframe))) => (*bci, None, Some(rframe.as_ref())),
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
    let Some(handler_pc) = find_jit_exception_handler(shared, cached, throw_pc, exc) else {
        thread.native_pin_roots.truncate(pin_base);
        return Err(if throw_pc == usize::MAX {
            CalleeHandlerMiss::Unknown
        } else {
            CalleeHandlerMiss::NotCaught
        });
    };
    if precise_locals.is_none()
        && deferred_precise.is_none()
        && !params_only_callee_handler_frames()
        && handler_resume_needs_precise_locals(cached)
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
        thread.native_pin_roots.truncate(pin_base);
        return Err(CalleeHandlerMiss::Declined);
    }
    let incoming_args = precise_locals.unwrap_or(incoming_args);
    let mut synchronized_args = cached.is_synchronized.then(|| incoming_args.to_vec());
    let synchronized_monitor = match synchronized_args.as_mut() {
        Some(args) => match JitSynchronizedMonitorGuard::acquire(shared, thread, cached, args) {
            Ok(monitor) => Some(monitor),
            Err(error) => {
                // `acquire` pushes its own pin only on success.
                thread.native_pin_roots.truncate(pin_base);
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
    let materialized_locals;
    let incoming_args = match deferred_precise {
        Some(rframe) => {
            match materialize_and_relock_precise_frame(shared, thread, cached, rframe) {
                Some(mut locals) => {
                    // The `this` fixup `precise_handler_frame_for` applies to a
                    // mapped frame, re-applied here for a deferred one: the
                    // caller's receiver is authoritative over whatever the
                    // compiled frame's slot 0 decoded to.
                    if !cached.is_static {
                        if let (Some(slot0), Some(receiver)) =
                            (locals.first_mut(), incoming_args.first())
                        {
                            *slot0 = *receiver;
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
                    thread.native_pin_roots.truncate(pin_base);
                    return Err(CalleeHandlerMiss::Declined);
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
    // Same GC-safety ordering as `route_jit_exception_through_method`: the
    // exception oop must live in a scanned frame slot before anything that can
    // allocate runs. The value is the PIN's: the `exc` parameter predates the
    // search and the acquire above.
    let exc = thread.native_pin_roots[pin_base];
    if frame.stack.push(Value::Object(Some(exc))).is_err() {
        // The monitor guard truncates to its own, deeper pin index.
        drop(synchronized_monitor);
        thread.native_pin_roots.truncate(pin_base);
        return Err(CalleeHandlerMiss::Declined);
    }
    frame.pc = handler_pc;
    if let Some(monitor) = synchronized_monitor {
        monitor.transfer_to_handler_frame(&mut frame);
    }
    // `execute_prebuilt_frame` pushes the frame before anything that can
    // allocate runs, and the frame now owns both the throwable and the monitor.
    //
    // And, when `deferred_precise` fired, the shells
    // `materialize_and_relock_precise_frame` allocated: they are in `frame`'s
    // LOCALS, which this truncate makes their only root. Same window and same
    // argument as the sibling sink -- see the longer note on
    // `route_jit_exception_through_method`'s truncate, which spells out what
    // may and may not go between these two lines.
    thread.native_pin_roots.truncate(pin_base);
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
    Ok(execute_prebuilt_frame(shared, thread, frame))
}

pub(super) fn route_jit_exception_through_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller_frame_idx: usize,
    cached: &Arc<CachedBytecodeMethod>,
    throw_pc: usize,
    exc: ObjectRef,
    incoming_args: &[Value],
    deferred_precise: Option<&cratonvm_jit::deopt::ReconstructedFrame>,
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
    // drop the class_manager lock to lazily load a catch type, and a
    // synchronized method's monitor acquire below can block on contention --
    // both are points a moving collection can run at, and until the handler
    // frame's operand stack holds it, this Rust local is the only copy. Every
    // later use re-reads the pin slot, which the collector rewrites in place
    // (the discipline `unwind_to_handler` documents).
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);

    let handler_pc = find_jit_exception_handler(shared, cached, throw_pc, exc);

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

    let mut synchronized_args = cached.is_synchronized.then(|| incoming_args.to_vec());
    let synchronized_monitor = match synchronized_args.as_mut() {
        Some(args) => match JitSynchronizedMonitorGuard::acquire(shared, thread, cached, args) {
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
    // GC-safety: push the live exception oop onto the handler frame's operand
    // stack BEFORE `push_frame_and_fire_entry`. That helper fires a JVMTI
    // MethodEntry callback (when a listener is active); the callback can
    // allocate Java heap and trigger a young-gen GC that relocates live oops
    // (gen_heap.rs selective promotion). If `exc` were pushed only AFTER the
    // fire, it would be reachable solely through this Rust local across the
    // callback — unrooted — and a relocation/collection would leave a stale
    // pointer on the operand stack. Populating the GC-scanned frame slot first
    // keeps it rooted across the callback. Mirrors the same ordering fix in
    // `resume_from_ir_deopt`.
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
    }
    // The throwable is now in the frame's operand stack and the monitor in
    // `monitor_on_exit`; nothing allocates before the frame is pushed below.
    //
    // THAT SENTENCE NOW CARRIES MORE THAN THE THROWABLE. When
    // `deferred_precise` fired, this truncate also releases the shells
    // `materialize_and_relock_precise_frame` allocated, and they are rooted
    // from here only by `frame`'s LOCALS -- a Rust local until
    // `push_frame_and_fire_entry` puts it on `thread.frames`. The window is
    // safe for the same reason it always was and for no other: between this
    // line and that push there is `harvest_retired_slot` (pool bookkeeping)
    // and one `eprintln!`, neither of which allocates Java heap, and
    // `push_frame_and_fire_entry` pushes BEFORE it fires JVMTI. Anything added
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
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);
    let new_idx = thread.frames.len() - 1;
    // Exception is already on the operand stack (rooted before the fire above);
    // just position the PC at the handler.
    thread.frames[new_idx].pc = handler_pc;
    // Silence unused parameter warning — caller_frame_idx is kept for
    // future extensions (e.g. return-value coercion into the caller).
    let _ = caller_frame_idx;
    Ok(CachedCallResult::FramePushed)
}
