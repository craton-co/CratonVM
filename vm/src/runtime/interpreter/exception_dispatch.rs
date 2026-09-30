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
    loop {
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
                fire_jvmti_exception_catch(
                    shared.vm_identity,
                    thread.thread_id.0,
                    &thread.frames[*frame_idx],
                    handler_pc,
                );
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
    let name_rule = !shared.config.is_jdk_only();
    // Read BEFORE the probe and before resolving; see `SiteCache::put`.
    let epochs =
        super::opcodes::cast_site_cache_enabled().then(|| CastSiteCache::epochs_for(shared));
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
            if site.positive_receiver == Some(exc_class_id) {
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
                    CastSite {
                        positive_receiver: Some(exc_class_id),
                        ..site
                    },
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
    let (name, fill) = {
        let cm = shared.classes.class_manager.read();
        let Some(name) = cm
            .get_class(class_id)
            .and_then(|c| c.constant_pool.get_class_name(cp_index))
        else {
            return CatchRowVerdict::NoMatch;
        };
        let fill = fillable_class && !name.starts_with('[');
        let loaded = cm.find_class_by_name_for_class(name, class_id);
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
    match super::constants::resolve_class_loader_aware(shared, thread, class_id, &name) {
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
    for (name, method_name, handler_pc) in rows {
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
            return Err(crate::runtime::exceptions::throw_linkage_error(
                shared,
                thread,
                crate::error::LinkageError::VerifyError {
                    class_name,
                    method_name,
                    // HotSpot's wording.
                    message: format!(
                        "Catch type is not a subclass of Throwable in exception handler {handler_pc}"
                    ),
                },
            ));
        }
    }
    Ok(())
}

/// The rows [`load_catch_types_at_link`] loads: `(catch type, method name,
/// handler pc)` of the first row naming each distinct catch type, skipping a
/// name that is not loaded for the class and that the global path would answer
/// by fabricating a synthetic stub, plus the class name for a `VerifyError`.
/// `None` when the class is gone.
fn link_catch_rows(
    cm: &crate::classloading::ClassManager,
    class_id: crate::classloading::ClassId,
) -> Option<(String, Vec<(String, String, u16)>)> {
    let class = cm.get_class(class_id)?;
    let mut seen: rustc_hash::FxHashSet<&str> = rustc_hash::FxHashSet::default();
    let mut rows: Vec<(String, String, u16)> = Vec::new();
    for method in &class.methods {
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
            rows.push((name.to_string(), method.name.to_string(), entry.handler_pc));
        }
    }
    Some((class.name.to_string(), rows))
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
    rows.iter().all(|(name, _, _)| {
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
    )
    .handler_pc()
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
                CatchRowVerdict::Match => return CompiledHandlerSearch::Found(handler_pc),
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
    let precise = match cratonvm_jit::deopt::take_exceptional_frame_with_point() {
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
            compiled_locks = Some(
                super::deopt_resume::CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe),
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
            None
        }
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
    }
    routed
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
pub(super) fn precise_handler_frame_for(
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
    let (rframe, cause, point_addr) = cratonvm_jit::deopt::take_exceptional_frame_with_point()?;
    if !deopt_frame_matches_method(
        &rframe,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(rframe, cause, point_addr);
        return None;
    }
    *compiled_locks = Some(
        super::deopt_resume::CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe),
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
    if handler_sink_genuine_receiver_enabled() {
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
        // Publish the relocked monitor for `getLockedMonitors()`, as the deopt
        // resume does (r11w5-sync-deopt-relock-publish-patch).
        if m.lock_depth > 0 {
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
                thread.native_pin_roots.get(args_pin_base).copied().unwrap_or(stale),
            )),
            other => other,
        };
    }
    acquired
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
    let mut precise = precise_handler_frame_for(thread, cached, incoming_args, &mut claimed_locks);
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
    // The locals the handler would resume on are raw `Value`s until the frame
    // below holds them, and a deferred frame's are raw addresses. Both are
    // pinned across the search, which may then resolve catch types for real
    // (and collect), and re-read after it.
    let (search, resume_args) = pinned_compiled_handler_search(
        thread,
        precise_locals.unwrap_or(incoming_args),
        deferred_precise.as_deref_mut(),
        |thread| find_jit_exception_handler(shared, thread, cached, throw_pc, pin_base, true),
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
        return miss(thread, CalleeHandlerMiss::Declined);
    }
    let incoming_args: &[Value] = &resume_args;
    let mut synchronized_args = cached.is_synchronized.then(|| incoming_args.to_vec());
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
    let materialized_locals;
    let incoming_args = match deferred_precise {
        Some(rframe) => {
            // Round 14 wave 1 (lane sync, `handler_sink_genuine_receiver_enabled`):
            // as for a mapped frame, only a slot 0 the frame published DEAD is
            // the receiver's to fill; a live one keeps what the bytecode stored.
            let slot0_is_the_receivers = !handler_sink_genuine_receiver_enabled()
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
        |thread| find_jit_exception_handler(shared, thread, cached, throw_pc, pin_base, true),
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

    let mut synchronized_args = cached.is_synchronized.then(|| incoming_args.to_vec());
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
        unwind_to_handler, CalleeHandlerMiss, UnwindSearch,
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
                ConstantPoolEntry::Utf8(std::sync::Arc::from(A)), // 1
                ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
                ConstantPoolEntry::Utf8(std::sync::Arc::from(MISSING)), // 3
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
            assert_ne!(site.positive_receiver, Some(exc_b));
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
        assert_eq!(handler.monitor_on_exit, Some(this), "the monitor is the receiver's");
        assert!(shared.threads.monitors.holds(this, tid));
        assert!(!shared.threads.monitors.holds(other, tid), "local 0 is not locked");
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
        assert!(shared.threads.monitors.exit(this, tid).is_ok());
        thread.frames.truncate(depth);

        // The callee-handler sink's mapping: a LIVE local 0 is kept.
        cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
        let mut claimed = None;
        match precise_handler_frame_for(
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
    #[test]
    fn resolved_catch_row_name_rule_is_compatible_only() {
        const SAME: &str = "cratonvm/test/HandlerNameRuleExc";
        for mode in [
            cratonvm_types::compat::CompatibilityMode::Compatible,
            cratonvm_types::compat::CompatibilityMode::JdkOnly,
        ] {
            let mut config = crate::config::VmConfig::default();
            config.compatibility_mode = mode;
            // `JdkOnly` rejects the synthetic library; off in both arms so the
            // two VMs differ only in the mode (as `jit_bridge`'s `vm_with`).
            config.use_synthetic_jdk = false;
            let shared = SharedVm::new(config);
            let empty_pool = || ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
            let app = define_class(&shared, SAME, ClassLoaderId::Application, empty_pool());
            let other = define_class(&shared, SAME, ClassLoaderId::UserDefined(77), empty_pool());
            let owner = define_class(
                &shared,
                "cratonvm/test/HandlerNameRuleOwner",
                ClassLoaderId::Application,
                ConstantPool::new(vec![
                    ConstantPoolEntry::Tombstone,
                    ConstantPoolEntry::Utf8(std::sync::Arc::from(SAME)), // 1
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
                let expected = if mode.is_jdk_only() {
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
                    ConstantPoolEntry::Utf8(std::sync::Arc::from(catch)), // 1
                    ConstantPoolEntry::ClassReference { name_index: 1 },  // 2
                ]),
            );
            let method = cratonvm_reader::method::ClassFileMethod {
                access_flags: MethodAccessFlags::STATIC,
                name: std::sync::Arc::from("m"),
                descriptor: std::sync::Arc::from("()V"),
                attributes: vec![LazyAttribute::Decoded(Attribute::Code(CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    // nop; return; (handler) pop; return
                    code: cratonvm_reader::ByteView::from_vec(vec![0x00, 0xb1, 0x57, 0xb1]),
                    exception_table: vec![row(0, 1, 2, 2)],
                    attributes: vec![],
                }))],
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
