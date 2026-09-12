// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Every place the interpreter asks for, enters, or leaves compiled code.
//!
//! This module exists to make that list *enumerable*. It was not, and the cost
//! is on the record: the OSR path compiles through `compile_osr_artifact`,
//! which calls the x64 backend directly instead of going through
//! `try_jit_compile_callee`, so everything the ordinary compile door does at
//! entry is silently skipped on that one. Nobody chose that; it was invisible
//! because the two doors were 6,000 lines apart in a 24,000-line file.
//!
//! What lives here:
//!
//! * **Requesting code.** `try_jit_compile_callee` and its slow path, the
//!   background compile task, the negative cache, and `try_jit_upgrade_with_gate`
//!   — the admission gate that decides a method is worth compiling.
//! * **On-stack replacement.** `try_osr` and `compile_osr_artifact`, the second
//!   compile door.
//! * **Entering and leaving.** `execute_jit_call` and its decoded form, the
//!   monitor guard that keeps a `synchronized` compiled frame balanced when it
//!   unwinds, and `jit_panic_to_exception`.
//! * **Deciding what a compile may assume.** The native-shadow and
//!   forced-metadata probes, `resolve_inline_site`, and the elidable-construction
//!   analysis.
//!
//! Adding a new way to reach compiled code means adding it here. That is the
//! point of the file.

use super::*;

/// Class name for the `CRATONVM_DBG_COMPACT_INLINE` engagement census, or a
/// `<class_id=N>` placeholder when the class store cannot name it.
///
/// Diagnostic-only, and deliberately `#[cold]`: the census fires once per
/// unresolved field site at COMPILE time, never on the execution path it is
/// reporting about.
#[inline(never)]
#[cold]
fn declaring_class_name_for_diag(shared: &SharedVm, class_id: ClassId) -> String {
    let cm = shared.classes.class_manager.read();
    cm.get_class(class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("<class_id={}>", class_id.as_u32()))
}

/// `CRATONVM_DBG_JITC` diagnostic: name the cache state that forced an OSR
/// recompile — `no-cached-artifact` (the one legitimate case),
/// `cached-not-via-osr`, or `cached-cannot-enter-at-pc`.
///
/// The last one is the interesting one: a PUBLISHED `compiled_via_osr` artifact
/// that cannot be entered at `entry_pc` will never become enterable
/// (`osr_pc_to_native[entry_pc]` is a pure function of the bytecode and the
/// entry pc — the codegen writes `-1` for a pc strictly inside a LICM-hoisted
/// loop body), so every recompile rebuilds it byte for byte. Measured on the
/// `CallRate.allocPutOld` probe: 200 full C2 pipelines per 200 000 iterations,
/// 199 of them this case, with the loop interpreted throughout.
///
/// `#[inline(never)]` + `#[cold]` keep the formatting temporaries of a
/// debug-only path out of `compile_osr_artifact`'s frame. That caller is ~1100
/// lines and runs on the mutator stack, and a frame reservation is
/// unconditional even for a branch that never executes without the env var —
/// so this is worth keeping out of line on principle, cheaply.
///
/// (Honesty note for the next reader: this was briefly *suspected* of causing
/// a `main-vm` stack overflow in `TestDefaultServlet`. It does not. That crash
/// reproduces on binaries with no diagnostic and no OSR change at all — it is
/// a pre-existing flaky, load-dependent overflow on `dev`, seen once in four
/// runs of an unmodified baseline binary on a heavily loaded host. Do not read
/// these attributes as fixing anything.)
#[inline(never)]
#[cold]
pub(super) fn dbg_osr_recompile_reason(
    cached_osr: Option<&crate::jit::CompiledMethod>,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    entry_pc: usize,
) {
    if !cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
        return;
    }
    let why = match cached_osr {
        None => "no-cached-artifact",
        Some(c) if !c.compiled_via_osr => "cached-not-via-osr",
        Some(_) => "cached-cannot-enter-at-pc",
    };
    eprintln!(
        "[cratonvm-jitc] OSR-recompile reason={why} {class_name}.{method_name}{method_descriptor} entry_pc={entry_pc}"
    );
}

/// Why the OSR door must leave a statically bound callee on the checked
/// dispatch helper — `None` when it may bake a direct machine-code `CALL`.
///
/// This is the OSR door's copy of the refusal `callee_compiler` and
/// `direct_callee_lookup` each spell out, and until 2026-08-24 it disagreed
/// with both: it refused **every** callee declaring an exception table,
/// unconditionally, while the other two doors had asked
/// `direct_call_exc_table_publish_enabled` since that gate existed and stopped
/// refusing when it was flipped default-ON on 2026-08-21.
///
/// That disagreement is the whole of the OSR half of
/// `osr-door-refused-every-exception-table-callee-FIXED-20260824.md`. The gate's flip
/// moved the ordinary-frame rung of `probes/NativeFunnelFloorProbe.java` from
/// 107.30 to 10.57 ns/op and left the OSR rung at ~101 in both arms, and the
/// page read that — correctly, given what it could see — as "an OSR body never
/// consults the gate at all". It does not consult it because this predicate
/// never asked; a hot loop is always an OSR body, so the sites that stand to
/// gain most from a direct call were by construction the ones excluded.
///
/// Nothing else about the OSR door needed to change for this to be sound. The
/// gate's two stated preconditions are properties of the EMITTER, not of the
/// door: `emit_inline_callee_deopt_check` is emitted after the baked `CALL` by
/// the same `x64/bytecode_walk.rs` arms this door reaches through
/// `compile_with_param_slots`, and a site that cannot reserve the service
/// slots fails the compile (`direct-call-service-slots`) rather than emitting
/// an unserviced edge. The OSR ladder already registers a `JitInvokeInfo` for
/// every direct-bound site, which is what that check reads.
///
/// The two refusals that are NOT about the gate stay unconditional: a callee
/// this door cannot resolve, and a callee with no `Code` attribute at all —
/// there is no body to bake a `CALL` to. `direct_callee_lookup` maps the
/// no-`Code` case onto `CalleeExceptionTable` as well, and this mirrors it so
/// the census table means the same thing at all three doors.
pub(super) fn osr_callee_bars_direct_call(
    shared: &SharedVm,
    caller_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> Option<cratonvm_jit::DirectBindRefusal> {
    let cm = shared.classes.class_manager.read();
    let Some(callee_cid) = cm.find_class_by_name_for_class(callee_class, caller_class_id) else {
        return Some(cratonvm_jit::DirectBindRefusal::CalleeClassNotFound);
    };
    let store = cm.class_store();
    let Some((method, _decl)) =
        crate::classloading::find_method_recursive(callee_cid, callee_method, callee_desc, store)
    else {
        return Some(cratonvm_jit::DirectBindRefusal::CalleeMethodNotFound);
    };
    match method.code() {
        None => Some(cratonvm_jit::DirectBindRefusal::CalleeExceptionTable),
        Some(code)
            if !code.exception_table.is_empty()
                && !cratonvm_jit::direct_call_exc_table_publish_enabled() =>
        {
            Some(cratonvm_jit::DirectBindRefusal::CalleeExceptionTable)
        }
        Some(_) => None,
    }
}

thread_local! {
    /// The last checkpoint [`compile_osr_artifact`] reached on this thread.
    ///
    /// That function has ~25 bare `return None`s and as many `?` operators, and
    /// an OSR refusal is SILENT: the loop just runs interpreted forever while
    /// `OSR-recompile reason=no-cached-artifact` repeats. The comment on
    /// `pending_callee_compiles` already records one defect that cost 5x and
    /// looked exactly like its own fix; this is the same failure mode with no
    /// diagnostic at all.
    ///
    /// Measured on netty's `FastLz.compress` — a 1617-byte method whose whole
    /// job is one loop, so OSR is its ONLY route to compiled code. It refuses,
    /// is marked OSR-denied for the process, and 256 MiB of compression runs in
    /// the interpreter at 138x HotSpot. The named denies (athrow, indy,
    /// exception table, newarray) all printed nothing, because none of them was
    /// the one that fired.
    static OSR_STAGE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("entry") };
}

/// Record the region [`compile_osr_artifact`] has reached.
#[inline]
fn osr_stage(stage: &'static str) {
    OSR_STAGE.with(|c| c.set(stage));
}

/// The last region [`compile_osr_artifact`] reached on this thread, for the
/// refusal report. Read on the SAME thread that ran the compile — the
/// background worker — which is where the `if !published` arm runs.
fn osr_stage_get() -> &'static str {
    OSR_STAGE.with(std::cell::Cell::get)
}

// ---------------------------------------------------------------------------
// Which interpreter frames are, right now, being run by compiled code
// ---------------------------------------------------------------------------
//
// jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902, defect (3):
// a trace captured after `main` has OSR'd says `main:62` -- the back-edge it
// tiered up at -- where HotSpot says `main:66`, the call that was executing.
//
// ## The mechanism, read off this file rather than assumed
//
// `try_osr` enters through `osr_enter_planned` and the artifact runs the method
// to its RETURN: the value comes back through that function's `ret_type`
// conversion and `try_osr_with_backoff` turns it into
// `OsrBackoffOutcome::ReturnOuter`. Compiled code therefore does NOT stop at
// the loop exit -- everything after the loop, and every call the method makes
// from there on, executes inside the artifact while the interpreter `Frame` for
// that same activation sits untouched on `thread.frames` with `pc == entry_pc`.
// A capture taken from inside that window (a throw in a callee, or another
// thread's `Thread.getStackTrace()`) walks `thread.frames` and reports the loop
// header for a method that is executing far below it. That is the *during*
// sub-case, and it is the only one `probes/StackTraceAfterOsr.java` exercises.
//
// The *after* sub-case -- the interpreter continuing past the loop with a stale
// pc -- does not exist here, and that was checked rather than assumed. Every
// exit that leaves this frame alive already writes a pc: the OSR-exit transfer
// (`deopt_resume::transfer_osr_exit_into_live_frame`) assigns
// `frame.pc = resume_bci`, the RBC.6b handler entry assigns
// `frame.pc = handler_pc`, and the safe-reject path deliberately leaves
// `entry_pc` standing because by admission nothing was committed before it.
// The only other way out is the normal return, which pops the frame.
//
// ## Why the pc itself must not be moved
//
// Two independent reasons, either one sufficient:
//
//   * `Frame::live_locals_mask_here` and `Frame::scan_local_objects_inner`
//     compute the per-bci live-locals ROOT FILTER from
//     `[self.pc, self.last_instr_pc]`. Advancing `pc` to where compiled code
//     really is would make every slot that dies in between stop being a root --
//     on a frame whose locals are the pre-OSR copies that the conservative half
//     of the JIT root scan is leaning on.
//   * The safe-reject exit above is correct only BECAUSE `frame.pc` is still
//     `entry_pc`. Moving it would resume the interpreter at a bci this
//     activation never reached, which is the silent-corruption shape RBC.7 is
//     named for.
//
// So the refresh has to be a SIDE CHANNEL that the trace assembler reads and
// neither the root scan nor any resume path can see. `Frame` is not this file's
// to widen, so the channel is this registry.
//
// ## What this registry can and cannot answer
//
// It answers, authoritatively, WHICH interpreter frame is a live OSR
// continuation and OF WHICH artifact -- two facts only the entry site knows.
// `stackwalker::drop_osr_continuations` infers the first from
// `cm.can_osr_enter(frame.pc)`, which is a property of a pc and not of an
// activation: an interpreted frame genuinely parked on a back-edge while a
// RECURSIVE compiled activation of the same method is live satisfies it too,
// and that frame's compiled entry is then dropped from the trace.
//
// It cannot answer the current bci by itself, and no honest version of it can.
// The bci compiled code is at lives in that activation's own frame at
// `[rbp - cm.sp_id_slot_off]` -- the safepoint-id slot every GC-capable site
// stores its `OopMapEntry::bytecode_pc` into, which is the mapping deopt and
// the precise root scan already share. The OSR activation's RBP is reachable
// only from the saved-RBP chain walk in `vm/src/jit/conservative_roots.rs`:
// `top_rbp_mirror_read` names the INNERMOST compiled frame, and by capture time
// that is some callee's, while the chain entry's `exact_rbp` has been
// overwritten by every prologue that ran underneath it. Re-deriving the bci any
// other way would be a SECOND pc->bci mapping beside deopt's, which is how this
// class of defect gets made in the first place. The bci must therefore come
// from the compiled entry `conservative_roots::active_compiled_frames` already
// reports for this same activation, matched to the interpreter frame by the
// pair below. See `.agent-requests/A7-wiring.txt`.

/// Kill switch for the OSR-continuation registry.
///
/// Default ON. `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` stops the registry being
/// written and makes `live_osr_continuation_artifact` answer `None` everywhere,
/// so every consumer falls back to the pc-shaped heuristic it used before --
/// one binary, both answers.
fn osr_pc_refresh_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_OSR_PC_REFRESH"))
}

/// One interpreter frame that compiled code is running right now.
#[derive(Clone, Copy)]
struct OsrContinuation {
    /// `thread.frames.len()` at the moment of entry -- deliberately the SAME
    /// number `JitEntryGuard::enter_with_compiled_at` records as the chain
    /// entry's `interp_depth`, so a consumer holding one of
    /// `active_compiled_frames`' `(depth, label, class_id, cm_ptr)` tuples can
    /// compare directly instead of inventing a second convention. The frame
    /// itself is `frames[interp_depth - 1]`.
    interp_depth: u32,
    /// The artifact running this activation, as `Arc::as_ptr(..) as usize`.
    /// Bit-identical to the `cm_ptr` that tuple carries: both are the address
    /// of the payload of the same `Arc<CompiledMethod>`.
    cm_ptr: usize,
}

thread_local! {
    /// This thread's live OSR continuations, outermost first.
    ///
    /// A `Vec` rather than one slot because an OSR'd body can call a method
    /// that itself OSRs; each is a separate activation at a different depth.
    /// Written once per OSR ENTRY -- never per back-edge -- and read only by a
    /// stack capture.
    static OSR_CONTINUATIONS: std::cell::RefCell<Vec<OsrContinuation>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Publishes one `OsrContinuation` for exactly as long as the artifact is on
/// the stack, and withdraws it however control leaves -- normal return, OSR
/// exit, routed exception, or a panic unwinding through `catch_unwind`.
struct OsrContinuationGuard {
    /// The registry length before this guard pushed. `Drop` truncates back to
    /// it rather than popping once, for the same reason `JitEntryGuard::drop`
    /// restores its depth first: a non-local exit out of a NESTED OSR entry
    /// could otherwise strand that descendant's record above ours, and a stale
    /// record names a frame depth that by then belongs to a different
    /// activation.
    depth_at_push: usize,
    /// `false` when the kill switch is set or the registry was already
    /// borrowed; `Drop` must then truncate nothing.
    armed: bool,
}

impl OsrContinuationGuard {
    fn publish(interp_depth: usize, cm_ptr: usize) -> Self {
        if !osr_pc_refresh_enabled() {
            return Self {
                depth_at_push: 0,
                armed: false,
            };
        }
        OSR_CONTINUATIONS.with(|c| match c.try_borrow_mut() {
            Ok(mut v) => {
                let depth_at_push = v.len();
                v.push(OsrContinuation {
                    // Saturate rather than panic: a depth this record cannot
                    // represent must degrade to "no information", never take
                    // the process down on a path that only feeds a diagnostic.
                    interp_depth: u32::try_from(interp_depth).unwrap_or(u32::MAX),
                    cm_ptr,
                });
                Self {
                    depth_at_push,
                    armed: true,
                }
            }
            // Not reachable today -- the only reader holds the borrow for the
            // length of one lookup and cannot re-enter OSR from inside it --
            // but refusing to publish is the safe direction: the consumer then
            // sees exactly what it saw before this registry existed.
            Err(_) => Self {
                depth_at_push: 0,
                armed: false,
            },
        })
    }
}

impl Drop for OsrContinuationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        OSR_CONTINUATIONS.with(|c| {
            if let Ok(mut v) = c.try_borrow_mut() {
                v.truncate(self.depth_at_push);
            }
        });
    }
}

/// The artifact currently running `thread.frames[frame_index]` as an OSR
/// continuation, as a `*const cratonvm_jit::CompiledMethod` cast to `usize` --
/// the same encoding `conservative_roots::active_compiled_frames` uses for the
/// `cm_ptr` in its tuples, so the two compare with `==`.
///
/// `None` means "no information", not "this frame is interpreted": it is also
/// what the kill switch and a contended borrow report. A caller must fall back
/// to whatever it did before rather than read it as a negative claim.
///
/// The consumer is `runtime::stackwalker`; the exact call site is written out
/// in `.agent-requests/A7-wiring.txt`. It lives here because the OSR entry is
/// the only place that knows the pairing -- nothing else on the thread can tell
/// an OSR continuation apart from an interpreted frame that merely happens to
/// be parked on a back-edge the artifact could have been entered at.
#[allow(dead_code)] // wired up from `runtime::stackwalker`; see A7-wiring.txt
pub(crate) fn live_osr_continuation_artifact(frame_index: usize) -> Option<usize> {
    if !osr_pc_refresh_enabled() {
        return None;
    }
    let depth = u32::try_from(frame_index.checked_add(1)?).ok()?;
    OSR_CONTINUATIONS.with(|c| -> Option<usize> {
        let live = c.try_borrow().ok()?;
        live.iter()
            .rev()
            .find(|r| r.interp_depth == depth)
            .map(|r| r.cm_ptr)
    })
}

/// The `int`-family value a compiled method returned, narrowed to its
/// descriptor's type.
///
/// JVMS §6.5 `ireturn`: a method whose return type is `boolean`, `byte`,
/// `char` or `short` returns its value truncated to that type (`boolean` to
/// bit 0). The interpreter does this at `ireturn`; neither JIT tier does, so a
/// compiled body fed non-javac bytecode (`iconst_2; ireturn` from a `Z` method)
/// handed the interpreter a 2. Narrowing at the bridge is where the compiled
/// value re-enters interpreted code.
fn narrow_int_return(ret_type: u8, raw: i64) -> i32 {
    // Casts: the JIT ABI returns every int-family value in the i64 register.
    match ret_type {
        b'Z' => (raw & 1) as i32,
        b'B' => i32::from(raw as i8),
        b'C' => i32::from(raw as u16),
        b'S' => i32::from(raw as i16),
        _ => raw as i32,
    }
}

/// Review #80: the name of the class that DECLARES the method the `Methodref`
/// at `cp_idx` in `holder`'s constant pool resolves to. It backs the
/// `cp_invoke_declaring_class_resolver` each
/// `try_compile_with_invokespecial_resolver` door hands the JIT, which admits a
/// `Counter extends AtomicInteger` site to the `Atomic*` intrinsics only when
/// this names the JDK class.
///
/// The constant-pool class is resolved in `holder`'s loader, and the method is
/// found with `find_method_recursive`, the walk `try_jit_compile_callee_slow`
/// uses to find a callee's declaring class. An `InterfaceMethodref` is not
/// answered: the intrinsics this feeds are class methods.
///
/// Takes its own class-manager read, like the other resolver closures handed
/// to `try_compile_with_invokespecial_resolver`. It must NOT be called from
/// `compile_osr_artifact`'s invoke loop, which already holds the guard; that
/// door resolves through `cm_lock` instead.
fn cp_method_ref_declaring_class_name(
    shared: &SharedVm,
    holder: ClassId,
    cp_idx: u16,
) -> Option<String> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(holder)?;
    let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
            ..
        }) => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let target_class = class.constant_pool.get_class_name(class_idx)?;
    let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
    site_class_and_declaring_class_name(&cm, holder, target_class, method_name, descriptor)
        .map(|(_, declaring_class)| declaring_class)
}

/// For an invoke site in `holder` that names `cp_class`, returns two things.
/// The first is the id of `cp_class` resolved in `holder`'s loader, which is
/// the class an exact receiver guard compares against. The second is the name
/// of the class that declares `name descriptor` as resolved from `cp_class`.
///
/// Reads through a class-manager guard the caller already holds and takes no
/// lock of its own. That is what lets `compile_osr_artifact`'s invoke loop,
/// which holds `cm_lock` for its whole body, ask it without a recursive read.
fn site_class_and_declaring_class_name(
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    cp_class: &str,
    name: &str,
    descriptor: &str,
) -> Option<(u32, String)> {
    let cp_class_id = cm.find_class_by_name_for_class(cp_class, holder)?;
    let store = cm.class_store();
    let (_, declaring_id) =
        crate::classloading::find_method_recursive(cp_class_id, name, descriptor, store)?;
    let declaring_class = store.get(declaring_id)?.name.to_string();
    Some((cp_class_id.as_u32(), declaring_class))
}

pub(super) fn compile_osr_artifact(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: String,
    method_name: String,
    method_descriptor: String,
    code: &[u8],
    max_locals: usize,
    entry_pc: usize,
) -> Option<Arc<crate::jit::CompiledMethod>> {
    // x86-64 ONLY: this door calls `x64::compile_with_param_slots` directly,
    // and no other backend publishes OSR entry points. On any other target it
    // would publish x86-64 bytes. `cfg!` keeps the body type-checked there.
    if cfg!(not(target_arch = "x86_64")) {
        return None;
    }
    // Loader-aware, and asked of this VM's manager: OSR denials belong to a
    // class identity and expire when the install epoch moves.
    let osr_key = crate::jit::tiered::MethodKey::with_class_id(
        class_id,
        class_name.as_str(),
        method_name.as_str(),
        method_descriptor.as_str(),
    );
    osr_stage("entry");
    cratonvm_types::osr_refusal_census::note_attempt();
    if shared.jit.tiered_manager.is_osr_denied(&osr_key) {
        return None;
    }
    // The two whole-method vetoes that must also stop a CACHED artifact from
    // being reused, not merely stop a new compile:
    //
    //   * `CRATONVM_DISABLE_JIT=1` forces interpreter-only execution. OSR is a
    //     JIT entry point distinct from `try_jit_compile_callee` /
    //     `try_jit_upgrade_with_gate`, so the user-facing flag has to be asked
    //     here for it to disable all three.
    //   * the BISECT levers. `CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY`
    //     were applied only inside `cratonvm_jit::try_compile`, and this
    //     function reaches `x64::compile_with_param_slots` directly, so an OSR
    //     body could be force-interpreted by neither. A bisect step that cannot
    //     actually stop the compile reads as an exoneration — see
    //     `cratonvm_jit::jit_force_interpret`.
    //
    // Both now come from `compile_gate`, so this door and `try_compile` cannot
    // disagree about them. The rest of the admission chain (the permanent
    // bail-list, the code-cache cap, the compile-epoch witness) is asked at the
    // compile itself, further down — refusing to *reuse* a body that is already
    // committed on either of those grounds would cost throughput and buy
    // nothing.
    if cratonvm_jit::compile_gate::compiled_execution_forbidden(&class_name, &method_name) {
        // Each early gate names itself before returning. Without this they
        // all report `stage=entry` and a real refusal looks like a method
        // that was never considered. See `osr_refusal_census`.
        osr_stage("gate:compiled-execution-forbidden");
        cratonvm_types::osr_refusal_census::note_refusal(
            "compiled-execution-forbidden",
            &format!("{class_name}.{method_name}"),
        );
        return None;
    }
    // A compiled entry has no ACC_SYNCHRONIZED monitor prologue/epilogue.
    // Keep synchronized methods out of OSR until that monitor contract is
    // implemented for compiled frames.
    if shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .and_then(|class| {
            class.methods.iter().find(|m| {
                &*m.name == method_name.as_str() && &*m.descriptor == method_descriptor.as_str()
            })
        })
        .is_some_and(|method| method.is_synchronized())
    {
        osr_stage("gate:synchronized");
        cratonvm_types::osr_refusal_census::note_refusal(
            "synchronized",
            &format!("{class_name}.{method_name}"),
        );
        return None;
    }
    // Respect the JIT skip list for OSR — classes that are skipped from
    // normal JIT compilation must also be skipped from OSR to avoid
    // re-executing loop bodies with buggy compiled code. Use the canonical
    // predicate so OSR and the first-call compile path agree exactly.
    // The static JIT ban list was deleted 2026-07-31 (see
    // docs/known-issues/jit-bans/jit-bans-all-disabled-20260731.md).
    // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
    // remaining force-interpret lever, applied in `jit::try_compile`.
    let class_name_check = class_name.as_str();
    let method_name_check = method_name.as_str();
    let _ = (class_name_check, method_name_check);
    // GPU-offload JIT admission gate — the OSR path is the exact case
    // the gate exists for: OSR-compiling a hot loop that contains an
    // offload-eligible invokestatic would silently end GPU dispatch at
    // that call site (known-issues followups item 2).
    #[cfg(feature = "gpu-offload")]
    if crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
        shared,
        class_id,
        method_name_check,
        &method_descriptor,
    ) {
        osr_stage("gate:gpu-offload");
        cratonvm_types::osr_refusal_census::note_refusal(
            "gpu-offload",
            &format!("{class_name}.{method_name}"),
        );
        return None;
    }
    // Get method info from frame metadata
    // S111r15 — same native-shadow guard as the other JIT entry points
    // (`try_jit_compile_callee`, `try_jit_upgrade_with_gate`, first-call
    // compile path). OSR must respect the native registration too.
    // `registered_native_will_run`, not `find(..).is_some()`: a stub-tagged
    // native on an allow-listed class never runs while the real body is loaded,
    // so refusing to compile that body is refusing to compile the code that
    // actually executes. See the predicate's doc for the measurements.
    if registered_native_will_run(
        shared,
        class_name_check,
        method_name_check,
        &method_descriptor,
    ) {
        osr_stage("gate:registered-native");
        cratonvm_types::osr_refusal_census::note_refusal(
            "registered-native",
            &format!("{class_name}.{method_name}"),
        );
        return None;
    }

    osr_stage("past-early-gates");
    // Check if already compiled
    let class_name_arc: Arc<str> = Arc::from(class_name.as_str());
    let method_name_arc: Arc<str> = Arc::from(method_name.as_str());
    let descriptor_arc: Arc<str> = Arc::from(method_descriptor.as_str());

    // RBC.2 — reuse a cached artifact when it can OSR-enter at this pc.
    // The historical "always recompile in OSR" policy (kept because an
    // early-compile artifact may lack direct-call wiring for callees
    // compiled later) re-ran the FULL x64 pipeline on every OSR trigger of
    // the same method: BC's `SecP521R1Curve$1.lookup` under DualECDRBG was
    // recompiled 2,610× in one crypto-prng suite run (its interface call
    // sites never promote it to method-entry JIT, so every call re-trips
    // the back-edge threshold). A cached artifact exposing an OSR entry for
    // this pc is at worst missing newer direct-call wiring — a throughput
    // nuance, not correctness — so prefer it. Artifacts without an entry
    // here (or no cached artifact) recompile exactly as before.
    let cached_osr = {
        let jit_cache = shared.jit.jit_cache.read();
        jit_cache.get_osr(&class_name_arc, &method_name_arc, &descriptor_arc, class_id)
    };
    // Only reuse artifacts the OSR path itself produced: those carry the
    // eager invokestatic callee wiring (direct calls). A first-call/upgrade
    // artifact can OSR-enter too, but pinning it into a hot loop forever
    // routes its callees through the slow dispatch helper — reusing those
    // regressed the DEFAULT (ban-on) BC suites ~2×. Such artifacts get one
    // fresh OSR recompile below (replacing them in the cache), after which
    // reuse kicks in.
    let osr_reused =
        matches!(&cached_osr, Some(c) if c.compiled_via_osr && c.can_osr_enter(entry_pc));
    // DBG: name WHY a cached artifact was not reused — see
    // `dbg_osr_recompile_reason`. MUST stay an `#[inline(never)]` call: this
    // function runs on the mutator's stack and is already ~1100 lines, and
    // `main-vm`'s remaining headroom here is thin enough that inlining even a
    // cold `eprintln!`'s formatting temporaries into this frame overflowed the
    // stack outright (`TestDefaultServlet`/`TestStandardWrapper`/`TestTomcat`
    // all died with "thread 'main-vm' has overflowed its stack"; the same
    // binaries pass with the diagnostic out of line). The branch never
    // executes without the env var, but the frame reservation is unconditional.
    if !osr_reused {
        dbg_osr_recompile_reason(
            cached_osr.as_deref(),
            &class_name,
            &method_name,
            &method_descriptor,
            entry_pc,
        );
    }
    let compiled = if osr_reused {
        cached_osr
    } else {
        (|| -> Option<_> {
            // ── The admission gate ────────────────────────────────────────
            //
            // The ONE door. This path reaches `x64::compile_with_param_slots`
            // directly rather than through `jit::try_compile`, and for a long
            // time that meant it applied whatever subset of `try_compile`'s
            // admission chain someone had noticed was missing:
            //
            //   * RBC.2 — the permanent bail-list, hand-copied here after the
            //     full compile pipeline re-ran on every OSR trigger of a
            //     permanently uncompilable hot method (35,923 wasted pipelines
            //     on `Nat.inc`'s dup_x2 bail in one crypto-prng suite run);
            //   * the bisect levers, hand-copied after every bisect step on the
            //     annotation-scan SIGSEGV read "no effect" while 11 methods
            //     kept compiling;
            //   * the code-cache cap, never copied at all — an OSR compile
            //     could commit code past a cap the ordinary door respected.
            //
            // `compile_gate::admit` asks all of them, in one place, for all
            // three doors, and the token it returns owns the compile-epoch
            // witness. That witness used to be opened ~1,000 lines below, after
            // every class load and constant-pool read this function performs:
            // a redefinition landing in that window produced a body stamped
            // with the CURRENT epoch, which the install barrier then accepted.
            // Holding the token from here is what closes it.
            let admission = match cratonvm_jit::compile_gate::admit(
                class_id,
                &class_name,
                &method_name,
                &method_descriptor,
                cratonvm_jit::compile_gate::CompileDoor::Osr,
            ) {
                Ok(a) => a,
                Err(reason) => {
                    // NAME the refusal. `.ok()?` threw the `CompileRefusal`
                    // away, and the four it can carry want opposite responses:
                    // `PermanentlyBailListed` points at an EARLIER compile of
                    // this method that the backend refused (and whose cause
                    // `jit_bail_reason_for` still holds), `CodeCacheAtCapacity`
                    // is transient, and the other two are configuration. An OSR
                    // refusal is silent and permanent, so the one that fired is
                    // the whole diagnosis.
                    osr_stage("admission-refused");
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] osr-DENY (admission: {}) {}.{}{} — earlier bail: {}",
                            reason,
                            class_name,
                            method_name,
                            method_descriptor,
                            cratonvm_jit::jit_bail_reason_for(
                                class_id,
                                &class_name,
                                &method_name,
                                &method_descriptor,
                            )
                            .unwrap_or_else(|| "none recorded".to_string()),
                        );
                    }
                    return None;
                }
            };
            osr_stage("past-admission");
            // A previous compile for exactly this back-edge produced a body
            // whose `osr_dead_mask` refuses entry there. That verdict is a pure
            // function of a deterministic compile, so re-running the pipeline
            // can only reach it again — 256 times over ten H2 `nioMemLZF:`
            // operations before this memo existed. See `mark_osr_entry_rejected`.
            if crate::jit::is_osr_entry_rejected(
                class_id,
                &class_name,
                &method_name,
                &method_descriptor,
                entry_pc,
            ) {
                return None;
            }
            let code_len = code.len().saturating_sub(2); // padded_bytecode adds 2
            let scan = match crate::jit::x64::jit_scan(&code, code_len, &method_descriptor) {
                Some(s) => s,
                None => {
                    // RBC.4 — scan rejects are permanent (see jit::try_compile_inner).
                    //
                    // NAME the opcode. A scan reject here bail-lists the method
                    // for EVERY door, so a method whose only route to compiled
                    // code is OSR — one big method that is one big loop — runs
                    // interpreted for the life of the process, and until this
                    // line existed it did so with no output whatsoever.
                    // `jit_scan` records the site it refused at; taking it is
                    // the difference between "OSR failed" and a bytecode to go
                    // look at.
                    osr_stage("jit-scan-refused");
                    if crate::runtime::env_cache::dbg_jitc() {
                        let (site, pc, op) =
                            cratonvm_jit::take_jit_bail_site().unwrap_or(("<unrecorded>", 0, 0));
                        eprintln!(
                            "[cratonvm-jitc] osr-DENY (jit_scan refused: {site} @pc={pc} op=0x{op:02x}) {}.{}{}",
                            class_name, method_name, method_descriptor,
                        );
                    }
                    crate::jit::mark_jit_bail_listed(
                        class_id,
                        &class_name,
                        &method_name,
                        &method_descriptor,
                    );
                    return None;
                }
            };
            osr_stage("past-jit-scan");
            // ── Would the optimizing tier have taken this method? ─────────
            //
            // INERT here, and deliberately so: this door reaches
            // `x64::compile_with_param_slots` and has no promotion to refuse.
            // It is a COUNTER, the same shape the String-intrinsic pin already
            // takes at this door and for the same reason -- a zero from a
            // one-door instrument is indistinguishable from "there was nothing
            // to ask about", and that is what made the reach of the optimizing
            // tier unfalsifiable.
            //
            // `ir_compatible_sized` is the FIRST of four gates, so this is an
            // UPPER BOUND on what an OSR route could deliver, which is exactly
            // what a go/no-go on building that route needs. The conjunct that
            // refuses is named on stderr by `ir_reject` under
            // `CRATONVM_DBG_IR_COMPILES`, so the reasons come free.
            //
            // Pure and lock-free: `scan` is already in hand and
            // `ir_compatible_sized` reads nothing else.
            let osr_ir_eligible = cratonvm_jit::ir::ir_compatible_sized(&scan, code_len);
            cratonvm_types::osr_refusal_census::note_ir_eligibility(osr_ir_eligible);
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] osr ir-eligibility: {} for {}.{}{} -- INERT at this door,                      which is single-pass only",
                    if osr_ir_eligible { "ACCEPTED" } else { "refused" },
                    class_name, method_name, method_descriptor,
                );
            }
            // This method's own exception table. Read ONCE, here, because both
            // of the RBC gates below need it: RBC.6 (immediately below) admits
            // a bare `athrow` only when it is EMPTY, and RBC.6b (further down)
            // admits a non-empty one only when every throwing site inside a
            // protected range publishes a precise exceptional frame.
            let osr_exception_table = match shared.classes.class_manager.read().get_class(class_id)
            {
                Some(class) => class
                    .methods
                    .iter()
                    .find(|m| {
                        &*m.name == method_name_check
                            && &*m.descriptor == method_descriptor.as_str()
                    })
                    .and_then(|m| {
                        m.attributes.iter().find_map(|a| match a.as_decoded() {
                            Some(cratonvm_reader::attribute::Attribute::Code(ca)) => {
                                Some(ca.exception_table.clone())
                            }
                            _ => None,
                        })
                    })
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            // RBC.6 (2026-07-18; LIFTED 2026-08-17 for the no-handler case) —
            // this door used to refuse **any** method containing a bare
            // `athrow` (0xbf), whatever its exception table looked like, on the
            // grounds stated in its own comment: "the OSR bail path resumes
            // interpretation at the back-edge, so an athrow lowering that ran
            // side effects natively before throwing could see them re-applied".
            // That hazard is RBC.7's silent-corruption shape and it was real
            // when the comment was written — the athrow drain's only move was
            // to re-stash the throwable and resume the live interpreter frame
            // at the STALE pre-OSR back-edge pc, re-running every iteration the
            // OSR'd code had already committed.
            //
            // Its blast radius was not deliberate. OSR is the ONLY door out of
            // the interpreter for a method invoked once — which is what a
            // `@Test` body, a `main`, and any one-shot driver is — so a `throw`
            // anywhere in such a method, even on a path never taken, kept its
            // hot loop interpreted for the method's whole life. Witness:
            // `BOBYQAOptimizerTest`, whose `trsbox`/`bobyqb` (each called once
            // per test; translated-from-Fortran numerical code that `throw`s a
            // `MathIllegalStateException` on an internal assertion and catches
            // nothing) turned a sub-second `optimize()` call into an unbounded
            // hang. See the known-issue page cited from that suite's RESULTS.
            //
            // What makes the lift safe is not new machinery but a PRECONDITION
            // that is checkable right here: with an EMPTY exception table, an
            // `athrow` in this body cannot be caught by the OSR'd frame, so no
            // drain ever has to resume that frame. `route_osr_exception_out_of_
            // artifact` answers `Propagate` on its first line for exactly this
            // population, and the throwable goes to the dispatch loop's
            // unwinder as `OsrBackoffOutcome::ThrowJava`: the frame is torn
            // down, and there is no stale resume for already-committed
            // iterations to be re-run from. That path is not new either — it is
            // the one the callee-throw fix already routes an unwinding
            // exception through — and it re-checks the empty table rather than
            // assuming it.
            //
            // A NON-empty table stays refused here, and the reason is NOT
            // RBC.6b's (which, since its own 2026-08-17 lift, admits such a
            // method whenever every throwing site inside a protected range
            // publishes a reason-9 frame — and `athrow`'s lowering is one of
            // the few that does not, so an `athrow` INSIDE a `try` is already
            // refused there). The residual case is an `athrow` OUTSIDE every
            // protected range of a method that has one elsewhere. There,
            // `route_osr_exception_out_of_artifact` correctly answers
            // `Propagate` — no precise frame, so the throw site is outside
            // every range — but `OsrBackoffOutcome::ThrowJava` then hands the
            // throwable to `unwind_to_handler` keyed on `entry_pc`, the
            // BACK-EDGE the body was entered at, not the throw site. When that
            // back-edge lies inside a protected range (`try { for (..) {..} }
            // catch`), the unwinder finds a handler that does not cover the
            // throw at all and enters it — on the stale pre-OSR locals. Until
            // `ThrowJava` carries "this frame has already declined to catch",
            // admitting that shape would trade a throughput bug for a silent
            // wrong-answer bug, which is the wrong direction.
            //
            // Not bail-listed, for the original reason: method-entry
            // compilation propagates cleanly through the JIT-return exception
            // drains and stays available either way.
            //
            // `CRATONVM_JIT_OSR_ATHROW=0` restores the blanket refusal, so one
            // binary can A/B the lift.
            if scan.has_athrow
                && (!osr_exception_table.is_empty()
                    || !crate::runtime::env_cache::osr_athrow_allowed())
            {
                if crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] osr-DENY (RBC.6 athrow, handlers={}) {}.{}{}",
                        osr_exception_table.len(),
                        class_name,
                        method_name,
                        method_descriptor
                    );
                }
                return None;
            }
            // RBC.7 (jit-osr-loop-duplicate-execution, silent data corruption,
            // 2026-07-20) — never OSR a method containing `invokedynamic`. The
            // 0xba codegen arm lowers every indy call site to an unconditional
            // `DeoptReason::UnreachedCode` trap (it never links/inlines the
            // bootstrap), so entering that site while OSR-compiled always
            // bails. For a NORMAL (method-entry) compile that bail resumes by
            // building a brand-new frame from scratch (no prior live frame to
            // reconcile), which is precise. For OSR the bail must instead
            // transfer the JIT-advanced state IN PLACE into the pre-existing
            // live interpreter frame (`transfer_osr_exit_into_live_frame`) —
            // and that transfer routinely fails: the operand-stack values
            // live at an indy call site (its recipe/constant args) are exactly
            // the kind of value the single-pass backend's per-site stack-slot
            // classifier cannot always precisely re-type (see `FrameValue::
            // Unsupported`'s doc in jit/src/deopt.rs), which correctly rejects
            // the transfer rather than fabricate one. The OSR trigger then
            // falls back to its "safe reject" default — resuming interpretation
            // at the STALE pre-OSR back-edge pc/locals — which is only actually
            // safe when the bail precedes any committed loop iteration. An indy
            // trap reached *after* a hot loop that already ran to completion
            // inside the OSR'd continuation (e.g. a `System.out.println("..." +
            // n + ...)` immediately following the loop, "..." string-concat
            // compiling to `invokedynamic`) violates that precondition: the
            // loop's real side effects (already committed once, correctly, by
            // the OSR'd code) get silently RE-EXECUTED by the interpreter from
            // the stale resume state — e.g. an `ArrayList` ending up with extra
            // duplicate elements with no exception anywhere. See
            // jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md
            // for the full repro and trace. Like `has_athrow` above,
            // method-entry compilation (unaffected by this OSR-only bail path)
            // remains available, so do NOT bail-list here.
            //
            // RELAXED 2026-07-30 (tomcat known-issue 30): the blanket refusal
            // moved below, to after `indy_info` is resolved. A site that lowers
            // to the StringConcatFactory bridge emits a real call, not a trap,
            // so there is no imprecise resume for an OSR frame to take. Only
            // methods with an UNBRIDGED indy are still refused.
            // RBC.6b (dohead-residuals, 2026-07-18; LIFTED 2026-08-17) — this
            // door used to refuse **any** method with a non-empty exception
            // table, on the grounds that `compile_with_param_slots` below is
            // never handed one, so an OSR artifact carries no handler ranges
            // and a callee exception unwinding into that frame escapes a
            // `catch` that textually guards the call. That hazard was real and
            // was observed (a servlet's `try { resp.resetBuffer(); } catch
            // (IllegalStateException)` silently ceasing to catch, once the
            // blanket `ldc`-string OSR denial that had masked it was lifted).
            //
            // The blast radius was not deliberate. OSR is the ONLY door out of
            // the interpreter for a method invoked once — which is what a
            // `@Test` body, a `main`, and any one-shot driver is — so "a hot
            // loop with a try/catch in it", ordinary Java, ran interpreted for
            // its whole life. Measured: netty's two
            // `HttpHeaderValidationUtilTest` exhaustive loops at 19 242 and
            // 309 423 ns/iteration against HotSpot's 8.2 and 9.4.
            //
            // What replaces it is the method-entry path's own contract, staged
            // here for the first time (see the three `set_*_request` calls
            // immediately before `compile_with_param_slots` below):
            //
            //   1. `set_precise_exception_frame_request(true)` makes every
            //      invoke inside a protected range publish a **reason-9**
            //      (`DeoptReason::PendingException`) frame keyed on the
            //      THROWING bci, not on the stale back-edge pc the live
            //      interpreter frame is parked at;
            //   2. `set_protected_ranges_request` suppresses the sibling
            //      tail-call inside a `try` (which would tear this frame down
            //      and `JMP`, unwinding past the handler);
            //   3. `set_pending_exception_ranges` makes the handler entry edges
            //      visible to `find_bypassable_loop_headers`.
            //
            // and the ADMISSION rule that makes the stale-resume fallback
            // unreachable rather than merely unlikely: every throwing site
            // inside a protected range must publish such a frame. That is
            // exactly `first_unsupported_precise_frame_site`, the predicate
            // RBC.6 already uses on the method-entry path — asked here rather
            // than copied, so the two doors cannot drift. A method with an
            // `ldc`, an array access, an `athrow`, a `new` or an
            // `invokedynamic` inside a `try` is still refused, and named.
            //
            // The refusal has to be a COMPILE-time one. Deciding it at the exit
            // instead would leave `transfer_osr_exit_into_live_frame`'s
            // fail-closed reject as the only backstop, and for an exception
            // exit that reject is not merely slow: it resumes interpretation at
            // the STALE pre-OSR back-edge pc, re-running every iteration the
            // OSR'd code already committed (RBC.7's silent-corruption shape).
            //
            // `CRATONVM_JIT_OSR_EXC_TABLE=0` restores the blanket refusal, so
            // one binary can A/B the lift.
            // (`osr_exception_table` is read once, above RBC.6, which gates on
            // the same table.)
            if !osr_exception_table.is_empty() {
                if !crate::runtime::env_cache::osr_exception_table_allowed() {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] osr-DENY (exception table, CRATONVM_JIT_OSR_EXC_TABLE=0) {}.{}{}",
                            class_name, method_name, method_descriptor
                        );
                    }
                    crate::jit::mark_jit_bail_listed(
                        class_id,
                        &class_name,
                        &method_name,
                        &method_descriptor,
                    );
                    return None;
                }
                // Name the ONE site that blocks the method, the way the
                // method-entry path's `rbc6-handler-reads-unsafe-local` bail
                // does. A bare "OSR denied" here is what cost this defect a
                // six-arm shape bisect to find in the first place.
                // x86-64 only: `first_unsupported_precise_frame_site` is
                // `#[cfg(target_arch = "x86_64")]`, because the precise-frame
                // publication it screens for is a property of that backend's
                // lowering. On another architecture there is no OSR at all (the
                // aarch64 backend publishes no `osr_pc_to_native`), so there is
                // nothing to deny and nothing to name.
                #[cfg(target_arch = "x86_64")]
                if let Some((pc, op)) = cratonvm_jit::first_unsupported_precise_frame_site(
                    &code,
                    code_len,
                    &osr_exception_table,
                ) {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] osr-DENY (osr-exc-site-unpublished pc={} opcode={:#04x}) {}.{}{}",
                            pc, op, class_name, method_name, method_descriptor
                        );
                    }
                    crate::jit::mark_jit_bail_listed(
                        class_id,
                        &class_name,
                        &method_name,
                        &method_descriptor,
                    );
                    return None;
                }
            }
            // 2026-07-10 BC-crypto session: OSR of `GOST3412_2015Engine.
            // init_gf256_mul_table` (a nested primitive-array allocation loop)
            // was observed to "resume with corrupt stack state for the next
            // newarray length", and a blanket per-method OSR deny for any
            // `newarray`-containing method was added as a workaround.
            //
            // perf/throughput-20260710: the deny is now DEFAULT-OFF. It was a
            // huge hammer — any hot loop in any method that allocates a
            // primitive array anywhere ran interpreted forever (BenchSuite
            // sieve250k: 3.2s → 177s, ~55x; every BC math/EC kernel under
            // JIT-allow lost OSR) — and the corruption does not reproduce on
            // the current tree (GOST3412Test 10/10 at -Xmx256m across both
            // getfield modes; an exact-shape nested-allocation repro is
            // checksum-identical to HotSpot under heap pressure; EC AllTests
            // passes under full JIT-allow). See `osr_newarray_allowed` for the
            // full evidence trail; `CRATONVM_OSR_NEWARRAY=0` restores the deny
            // for bisection.
            if scan.has_newarray && !crate::runtime::env_cache::osr_newarray_allowed() {
                if crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] osr-DENY (has_newarray, CRATONVM_OSR_NEWARRAY=0) {}.{}{}",
                        class_name, method_name, method_descriptor
                    );
                }
                shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
                return None;
            }

            // Resolve multianewarray
            let mut mna_info = Vec::new();
            if !scan.multianewarray_ops.is_empty() {
                let cm = shared.classes.class_manager.read();
                let class = cm.get_class(class_id)?;
                for &(pc, cp_idx, _ndims) in &scan.multianewarray_ops {
                    // A malformed CP entry is still a whole-compile refusal.
                    let _ = class.constant_pool.get_class_name(cp_idx)?;
                    mna_info.push((
                        pc,
                        crate::jit::pack_multianewarray_site(class_id.as_u32(), cp_idx),
                    ));
                }
            }

            // Resolve typecheck
            let mut typecheck_info: Vec<(usize, *const u8, usize)> = Vec::new();
            let mut owned_jit_strings2: Vec<Box<str>> = Vec::new();
            if !scan.typecheck_ops.is_empty() {
                let cm_lock = shared.classes.class_manager.read();
                let class = cm_lock.get_class(class_id)?;
                for &(pc, cp_idx) in &scan.typecheck_ops {
                    let cn = class.constant_pool.get_class_name(cp_idx)?;
                    // THE THIRD DOOR, and the last one still handing the
                    // runtime helper a bare name. The other two — the ordinary
                    // tiering door in `jit::try_compile_inner` and the eager
                    // first-call door in `interpreter.rs` — both resolve the
                    // site's `CONSTANT_Class` through THIS class's own defining
                    // loader and intern the name under that `ClassId`, so that
                    // `jit_checkcast` can compare ids instead of re-resolving a
                    // name against a `(ClassLoaderId, name)`-keyed dictionary,
                    // and so the JIT can compare them INLINE.
                    //
                    // This door did neither: it boxed a per-compilation copy of
                    // the name, which recorded no id and, being a fresh address
                    // every compile, could not even share the helper's
                    // `(ptr, len)` memo with the other two doors' copies of the
                    // same site.
                    //
                    // Measured: `CcProbe2`, a 20-million-iteration loop whose
                    // body is one `(Node) o` cast, reported
                    // `checkcast inline sites: single-pass=0 optimizing=0
                    // refused-no-target-id=2` and 63,989,000 membership walks —
                    // the inline compare could not fire ANYWHERE in it. A loop
                    // is exactly what reaches this door, so "the hot case" and
                    // "the door with no target id" were the same set. Third
                    // time this shape has been recorded (the thin native binds,
                    // the `String` call-site intrinsics): a bind at one compile
                    // door is not a bind.
                    let target_id = cm_lock
                        .find_class_by_name_for_class(cn, class_id)
                        .map(|id| id.as_u32());
                    let (ptr, len) = cratonvm_jit::intern_typecheck_target(cn, target_id);
                    typecheck_info.push((pc, ptr, len));
                }
            }

            // Resolve static fields
            let mut static_field_info: Vec<(usize, u32, usize, u8, bool)> = Vec::new();
            if !scan.static_field_ops.is_empty() {
                for &(pc, cp_idx) in &scan.static_field_ops {
                    let field = resolve_field_ref(shared, class_id, cp_idx).ok()?;
                    let cm_lock = shared.classes.class_manager.read();
                    let class = cm_lock.get_class(class_id)?;
                    let nat_idx = match class.constant_pool.get(cp_idx) {
                        Some(ConstantPoolEntry::FieldReference {
                            name_and_type_index,
                            ..
                        }) => *name_and_type_index,
                        _ => return None,
                    };
                    let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
                    let type_tag = *descriptor.as_bytes().first()?;
                    if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
                        return None;
                    }
                    static_field_info.push((
                        pc,
                        field.declaring_class_id.as_u32(),
                        field.field_index,
                        type_tag,
                        field.is_volatile,
                    ));
                }
            }

            // Resolve instance fields for getfield/putfield. Without this, the
            // OSR-compiled method had no `field_info` and every getfield/putfield
            // fell back to the `(pc, 0, b'I')` default in `compile_op_getfield` /
            // `compile_op_putfield` — silently routing every instance-field write
            // through `jit_putfield_int` into slot 0 with the wrong type tag.
            // Manifested in `org/eclipse/jdt/internal/compiler/util/HashtableOfInt`
            // (Eclipse JDT BatchCompiler boot): the OSR-compiled `rehash()`
            // stored the new `int[]` into `keyTable`'s slot tagged as
            // `Value::Int(low32_of_ptr)`, and the next `put()` then read back the
            // bogus tag and crashed at `arraylength` with
            //   `expected object reference, got int(N)`.
            // Mirrors the field_info collection in the first-call JIT compile
            // path (this file, ~line 1915) and `resolve_inline_site` (~line 13367).
            let mut field_info: Vec<(usize, usize, u8)> = Vec::new();
            // Compact reference-field layout: per-pc packed offset + ref-ness for
            // inline compact getfield/putfield in the OSR-recompiled method.
            let mut compact_field_info: Vec<(usize, u32, bool)> = Vec::new();
            let compact_fields = cratonvm_types::compact_ref_fields_enabled();
            if !scan.field_ops.is_empty() {
                for &(pc, cp_idx) in &scan.field_ops {
                    let field = resolve_field_ref(shared, class_id, cp_idx).ok()?;
                    let cm_lock = shared.classes.class_manager.read();
                    let class = cm_lock.get_class(class_id)?;
                    let nat_idx = match class.constant_pool.get(cp_idx) {
                        Some(ConstantPoolEntry::FieldReference {
                            name_and_type_index,
                            ..
                        }) => *name_and_type_index,
                        _ => return None,
                    };
                    let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
                    let type_tag = *descriptor.as_bytes().first()?;
                    if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
                        return None;
                    }
                    field_info.push((pc, field.field_index, type_tag));
                    if compact_fields {
                        let slot = cratonvm_types::compact_field_slot(
                            field.declaring_class_id.as_u32(),
                            field.field_index,
                        );
                        if let Some((c_off, c_ref)) = slot {
                            compact_field_info.push((pc, c_off as u32, c_ref));
                        } else if cratonvm_types::flags::runtime_flag_on(
                            "CRATONVM_DBG_COMPACT_INLINE",
                        )
                        {
                            // ENGAGEMENT CENSUS. A `None` here is not a missing
                            // optimisation, it is a *helper call on every access*:
                            // this pc takes the guarded uniform arm, which keys on
                            // GC_FLAG_COMPACT and routes every COMPACT receiver —
                            // the default layout — to `jit_getfield`, whose
                            // `is_object_address` walk the profile shows at ~31% of
                            // a field-dense run. Naming the declaring class and
                            // index is what separates "no layout registered for
                            // this class" from "index outside the layout it has".
                            let declaring =
                                declaring_class_name_for_diag(shared, field.declaring_class_id);
                            eprintln!(
                                "[compact-inline] MISS osr pc={pc} declaring={declaring} class_id={} field_index={} -> guarded-uniform arm (helper on every compact receiver)",
                                field.declaring_class_id.as_u32(),
                                field.field_index,
                            );
                        }
                    }
                }
            }

            // This method's own receiver profile, for the guarded String
            // call-site intrinsics resolved below. The method-entry door gets
            // one as a parameter (`try_compile_inner`'s `profile`) and screens
            // its guarded intrinsics against it; this door never had one, so
            // an OSR artifact would emit a `String` receiver guard at a
            // `CharSequence` site the interpreter had already recorded
            // thousands of non-`String` receivers at. Fetched once for the
            // whole scan, not per site.
            let osr_receiver_profile = {
                let profile_key = crate::jit::profile::MethodKey {
                    class_id: class_id.as_u32(),
                    method_name: method_name.as_str().into(),
                    descriptor: method_descriptor.as_str().into(),
                };
                shared.jit.profile_store.get_profile(&profile_key)
            };
            // Same `"<class>.<method>:<descriptor>"` key the deopt log and
            // `method_epochs` use; see `DespecRegistry::contains`.
            let osr_despec_key = format!("{class_name}.{method_name}:{method_descriptor}");

            // Resolve invokes — collect info under lock, then compile callees after release
            let mut invoke_info: Vec<(usize, *const crate::jit::JitInvokeInfo)> = Vec::new();
            let mut owned_jit_invoke_infos2: Vec<Box<crate::jit::JitInvokeInfo>> = Vec::new();
            let mut direct_calls2: Vec<(usize, crate::jit::JitDirectCall)> = Vec::new();
            // FIX (jit-osr-loop-direct-call-retired-code-segv): the entry
            // addresses of COMPILED callees baked into this body as raw
            // machine-code CALLs. `JitCache::prepare_for_publication` turns
            // these into `_direct_callee_roots` — strong `Arc`s that keep each
            // callee's `ExecutableBuffer` mapped for as long as this artifact
            // can run. The method-entry tier has always populated
            // `_direct_callee_entries` (jit/src/lib.rs); this OSR tier never
            // did, so its baked CALLs were rooted by NOTHING: the first
            // background tier-up `put` for such a callee dropped the artifact
            // this body calls, `ExecutableBuffer::drop` unmapped it, and the
            // next OSR entry jumped into freed memory (SIGSEGV with
            // `pc == addr` at the callee's page-aligned entry — ~1-2 % of runs
            // idle, ~20 % under load, on the `JitOsrLoopProgress` fixture).
            //
            // Only real compiled-artifact entries belong here: the intrinsic /
            // helper direct calls emitted above (`Math.sqrt`,
            // `Integer.valueOf`, `Integer.intValue`, the `HashMap` fast paths)
            // are Rust function addresses with no owning artifact, and
            // `resolve_jit_entry_owner` would fail on them — inflating the
            // unrooted-callee counter and, under
            // `CRATONVM_JIT_STRICT_CALLEE_ROOTS`, refusing publication.
            let mut osr_direct_callee_entries: Vec<usize> = Vec::new();
            let mut mic_slots2: Vec<(usize, *const crate::jit::JitMICSlot)> = Vec::new();
            let mut owned_mic_slots2: Vec<Box<crate::jit::JitMICSlot>> = Vec::new();
            let mut pic_slots2: Vec<(usize, *const crate::jit::JitPICSlot)> = Vec::new();
            let mut owned_pic_slots2: Vec<Box<crate::jit::JitPICSlot>> = Vec::new();
            // Pending statically-bound callee compilations:
            // `(pc, class, method, desc, num_jit_args, invoke_kind)`.
            //
            // `param_count` is the JLS argument count, receiver EXCLUDED, for
            // both kinds. The two consumers below disagree about the receiver
            // and each must be fed its own convention:
            //
            //   * `JitDirectCall.num_params` wants it receiver-EXCLUDED — the
            //     codegen's instance arm computes `let n = callee_params + 1`
            //     itself ("the JLS argument count is `callee_params`, and total
            //     operands popped is `callee_params + 1`");
            //   * `JitInvokeInfo.num_jit_args` wants it receiver-INCLUDED.
            //
            // Passing the receiver-included count to BOTH made the emitter pop
            // three operands off a two-operand stack for `F.<init>(I)V`, so
            // `compile_with_param_slots` refused the method — and an OSR
            // refusal is silent: the loop just runs interpreted forever while
            // `OSR-recompile reason=no-cached-artifact` repeats. Measured 5x
            // SLOWER, with `disp_calls` at 0, which is what the intended fix
            // also looks like.
            //
            // Kind 1 was admitted 2026-08-13. It had never been: this door
            // eagerly compiled and direct-bound `invokestatic` callees only, so
            // in an OSR'd loop — which is what a hot loop always is — every
            // `new X(...)` paid a full `jit_invoke_dispatch` round trip for its
            // constructor, forever, while the identical body reached through
            // `invokestatic` or `invokevirtual` was bound. Measured on the same
            // loop, same body: 26.7 ns (static, bound), 29.1 ns (virtual, bound
            // via the MIC), **317.9 ns** (`new Holder(i)`, dispatched — the only
            // one of the three producing dispatch-trace entries at all).
            //
            // The other two compile doors already bind kind 1: the single-pass
            // ladder on `matches!(invoke_kind, 1 | 3)`, and the IR ladder since
            // its `!is_ctor` term was removed. This is the third.
            let mut pending_callee_compiles: Vec<(usize, String, String, String, usize, u8)> =
                Vec::new();
            // Trivial-ctor elision (OSR tier) — same deferred mechanism as the
            // `execute` first-call path: record `invokespecial …<init>()V` sites
            // here, resolve their target via `load_class_concurrent` after the
            // lock drops, and emit elidable ones as `Object.<init>` so codegen
            // drops the per-object dispatch. See `execute` for the rationale.
            let ctor_direct_call_off = crate::runtime::env_cache::ctor_direct_call_disabled();
            let osr_ctor_bind_off = crate::runtime::env_cache::osr_ctor_bind_disabled();
            let mut pending_ctor_sites: Vec<(usize, String, usize)> = Vec::new();
            // THIRD COMPILE DOOR, 2026-08-13. `java/lang/String`'s call-site
            // intrinsics (`length`/`isEmpty`/`charAt`/`hashCode`/`equals`/
            // `compareTo`/`indexOf`) were bound only in `jit::try_compile`'s
            // ladder, and this door passed `string_layout: None` under the
            // comment "String intrinsics land in a later wave". The wave never
            // came, so in a hot loop — the one place they matter, and the one
            // place compiled HERE — every one of them was inert: measured on
            // this branch before the fix, `String.charAt(i)` in a 20M-iteration
            // loop cost **408 ns/call** (HotSpot: 0.6 ns), because the site ran
            // the real `charAt` → `isLatin1` → `StringLatin1.charAt` →
            // `String.checkIndex` → `Preconditions.checkIndex` chain instead of
            // the inline decode. `String.length()` likewise cost 28 ns.
            //
            // This is verbatim the lesson the `Thread.currentThread()` bind
            // above records ("binding it in all THREE compile doors is the
            // whole lesson of that document") — and, like it, no timing could
            // have found it: a resolver, a codegen ladder and two green unit
            // suites all agree the intrinsic exists. `CRATONVM_DBG_INTRINSIC=1`
            // (jit/src/lib.rs) is the lever that names which door produced a
            // body, so the next one of these is a one-run question.
            //
            // Resolved BEFORE the `class_manager` read lock below: this helper
            // takes that same lock, and a recursive read on a `parking_lot`
            // RwLock can deadlock against a queued writer.
            let osr_string_layout = super::dispatch_static::resolve_string_field_layout(shared);
            // -- The String-intrinsic pin, ASKED at this door -- D1, 2026-09-01
            //
            // Measured on the built branch, ONE binary, two probes:
            //
            //     CharAtCostCurve   JIT String-intrinsic pin: fired=2   (method entry)
            //     CharAtWarmShape   JIT String-intrinsic pin: fired=0   (OSR)
            //
            // `CharAtWarmShape`'s entire body is `charAt`. Its `fired=0` was
            // never a method that FAILED the pin's test -- it was a method the
            // pin was never shown. The pin was a term of `try_compile_inner`'s
            // eligibility conjunction, i.e. of `CompileDoor::MethodEntry` and
            // of nothing else, and this door reaches
            // `x64::compile_with_param_slots` directly. A zero from a one-door
            // counter is indistinguishable from "there was nothing to pin",
            // and that is what let the five hypotheses in
            // `string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901`
            // each be refuted without converging: every one of them varied the
            // METHOD, and the discriminator was the DOOR.
            //
            // # The answer is INERT at this door today -- stated, not implied
            //
            // The pin means "keep this method on the backend that HAS the
            // inline `charAt` decode", i.e. do not promote it to the optimizing
            // tier. This door has no promotion to refuse. `compile_osr_artifact`
            // reaches `x64::compile_with_param_slots` -- the single-pass backend
            // -- unconditionally; the only production call of
            // `ir_lower::lower_inner` in the tree is inside `try_compile_inner`,
            // and nothing under `vm/**` names `ir_lower` at all. So "do not tier
            // this up" is already true here BY TOPOLOGY, and there is no machine
            // code this ask can change today. The ~3x that LIFTING the pin
            // bought at the method-entry door is not available here, because
            // there is nothing here to refuse -- do not read this as closing a
            // live hole.
            //
            // What is not inert is the ASKING. `string_pin_asked(Osr)` and
            // `string_pin_declined(Osr)` now say how much of the population the
            // pin governs is compiled through this door, and
            // `string_pin_not_asked(Osr)` stops being this door's whole row --
            // the one number that would have named the defect above in a single
            // run. This is a COUNTER, deliberately, and not a fix.
            //
            // It stops being inert if either half of the topology moves: an OSR
            // route to the optimizing tier (then this answer must GATE it), or
            // an IR String-intrinsic emitter (which retires the pin instead).
            // The `if` below is the tripwire for the first, and is silent today.
            //
            // What this does NOT claim: a method OSR-compiled here can still be
            // promoted later through `try_jit_upgrade_with_gate`, which goes
            // through `try_compile_with_invokespecial_resolver` -- the
            // method-entry door, which DOES ask the pin. Nothing on that route
            // changed.
            //
            // COST: once per OSR compile, never per back-edge. The back-edge
            // counter reaches a cached artifact; `compile_osr_artifact` runs
            // once per (method, entry_pc) compile, and this sits on that path,
            // not on the loop. The resolver takes the `class_manager` read lock
            // per site -- the same shape as `c_invoke_resolver` in this file --
            // and `string_intrinsic_pin_verdict` calls it only for
            // `invokevirtual`/`invokeinterface` sites, stops at the first
            // String-family receiver, and asks nothing at all for a method with
            // no `0xb6`/`0xb9` site. No lock is held at this point:
            // `resolve_string_field_layout` above took and released its own,
            // and the invoke loop below takes its own AFTER this -- never
            // nested, which is the recursive-read deadlock the comment above
            // warns about.
            let osr_pin_invoke_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
                let cm = shared.classes.class_manager.read();
                let class = cm.get_class(class_id)?;
                let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::MethodReference {
                        class_index,
                        name_and_type_index,
                        ..
                    }) => (*class_index, *name_and_type_index),
                    Some(ConstantPoolEntry::InterfaceMethodReference {
                        class_index,
                        name_and_type_index,
                        ..
                    }) => (*class_index, *name_and_type_index),
                    _ => return None,
                };
                let target_class = class.constant_pool.get_class_name(class_idx)?;
                let (mn, desc) = class.constant_pool.get_name_and_type(nat_idx)?;
                Some((target_class.to_string(), mn.to_string(), desc.to_string()))
            };
            // The SAME `osr_string_layout` the invoke loop below screens with
            // and that `compile_with_param_slots` is handed. Asking the pin
            // about a different layout than the one this compile uses would
            // make the census describe a compile that did not happen.
            let osr_string_pin_declines = admission.string_intrinsic_pin_declines(
                &scan.invoke_ops,
                Some(&osr_pin_invoke_resolver),
                osr_string_layout,
            );
            if osr_string_pin_declines && crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] osr String-intrinsic pin declines the optimizing tier for \
                     {}.{}{} -- INERT at this door, which is single-pass only. If this door ever \
                     gains a route to the optimizing tier, THIS is the answer that must gate it.",
                    class_name, method_name, method_descriptor,
                );
            }
            if !scan.invoke_ops.is_empty() {
                let cm_lock = shared.classes.class_manager.read();
                let class = cm_lock.get_class(class_id)?;
                for &(pc, cp_idx, opcode) in &scan.invoke_ops {
                    let (ref_class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
                        Some(ConstantPoolEntry::MethodReference {
                            class_index,
                            name_and_type_index,
                            ..
                        }) => (*class_index, *name_and_type_index),
                        Some(ConstantPoolEntry::InterfaceMethodReference {
                            class_index,
                            name_and_type_index,
                            ..
                        }) => (*class_index, *name_and_type_index),
                        _ => continue,
                    };
                    let target_class = class.constant_pool.get_class_name(ref_class_idx)?;
                    let (mn, desc) = class.constant_pool.get_name_and_type(nat_idx)?;
                    let param_count = crate::jit::count_param_slots(desc);
                    let mut invoke_kind = match opcode {
                        0xb6 => 0u8,
                        0xb7 => 1,
                        0xb9 => 2,
                        0xb8 => 3,
                        _ => continue,
                    };
                    // JVMS 5.4.6 — an `invokevirtual` naming a PRIVATE method is
                    // not a dispatch site. The OSR door reaches
                    // `x64::compile_with_param_slots` directly, so like the
                    // eager first-call door it has to make the reclassification
                    // itself rather than inheriting `try_compile`'s. See
                    // `invoke::invokevirtual_site_targets_private`.
                    if invoke_kind == 0
                        && super::invoke::invokevirtual_site_targets_private(
                            &cm_lock,
                            class_id,
                            target_class,
                            mn,
                            desc,
                        )
                    {
                        invoke_kind = 1;
                        cratonvm_jit::PRIVATE_INVOKEVIRTUAL_PINNED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    let invoke_kind = invoke_kind;
                    let is_recursive_call = target_class == class_name.as_str()
                        && mn == method_name.as_str()
                        && desc == method_descriptor.as_str();
                    let use_raw_tail_self_call = invoke_kind == 3
                        && is_recursive_call
                        && crate::jit::invokestatic_self_call_uses_tail_jump(code, code_len, pc);
                    if use_raw_tail_self_call {
                        continue;
                    }
                    // BUG-1 companion (OSR/callee tier parity with
                    // `jit::try_compile`'s routing): NON-tail static
                    // self-recursive sites stay on the raw direct-CALL path —
                    // the backend emits the cheap `self_call_stack_guard` call
                    // (always wired via `build_helpers`) instead of the full
                    // `jit_invoke_dispatch` round trip. `scan.needs_heap` is
                    // already true (every invoke op sets it), so the guard's
                    // vm_ptr frame slot exists.
                    if invoke_kind == 3 && is_recursive_call {
                        continue;
                    }

                    // `java/lang/String` / `java/lang/CharSequence` call-site
                    // intrinsics — see `osr_string_layout` above for why this
                    // arm exists and what its absence cost. The matcher and the
                    // codegen must agree about the layout or the codegen would
                    // fall through and `CALL` an intrinsic sentinel address, so
                    // the SAME `osr_string_layout` value is handed to
                    // `compile_with_param_slots` below (the `string_layout`
                    // argument) — exactly the invariant
                    // `try_compile_inner` documents for its own copy.
                    //
                    // `guard_class_id` comes from the resolver: 0 for a
                    // `java/lang/String` site (final class, monomorphic), the
                    // real String class id for a `java/lang/CharSequence` site
                    // so the codegen guards the receiver and deopts for any
                    // non-String `CharSequence`.
                    if matches!(invoke_kind, 0 | 2) {
                        if let Some((entry, num_params, ret, guard_class_id)) =
                            cratonvm_jit::try_resolve_string_intrinsic(
                                target_class,
                                mn,
                                desc,
                                osr_string_layout,
                            )
                            // The same screen the method-entry door applies: a
                            // `CharSequence` site's `String` class-id guard
                            // DEOPTS on a miss, so a bci the profile says is
                            // hardly ever a `String` must not get one. See
                            // `cratonvm_jit::receiver_profile_rejects_guard`.
                            .filter(|&(entry, _, _, guard_class_id)| {
                                if guard_class_id == 0 || !cratonvm_jit::receiver_despec_enabled() {
                                    return true;
                                }
                                // A declining intrinsic's guard miss is a CALL,
                                // not a deopt, so this screen has nothing to
                                // protect against — see
                                // `string_intrinsic_declines_to_a_call`.
                                if cratonvm_jit::string_intrinsic_declines_to_a_call(entry) {
                                    return true;
                                }
                                let supported = cratonvm_jit::receiver_profile_supports_guard(
                                    osr_receiver_profile.as_ref(),
                                    pc,
                                    guard_class_id,
                                ) || cratonvm_jit::charseq_blind_guard_enabled();
                                let by_profile = !supported
                                    || cratonvm_jit::receiver_profile_rejects_guard(
                                        osr_receiver_profile.as_ref(),
                                        pc,
                                        guard_class_id,
                                    );
                                let by_despec = shared.jit.despec_registry.contains(
                                    &osr_despec_key,
                                    pc as u32,
                                );
                                if by_profile {
                                    cratonvm_jit::metrics::note_receiver_despec(
                                        cratonvm_jit::metrics::RECEIVER_DESPEC_PROFILE_DECLINED,
                                    );
                                }
                                if by_despec {
                                    cratonvm_jit::metrics::note_receiver_despec(
                                        cratonvm_jit::metrics::RECEIVER_DESPEC_DECLINED,
                                    );
                                }
                                !(by_profile || by_despec)
                            })
                        {
                            // A declining intrinsic needs its own
                            // `JitInvokeInfo` at THIS door too: the emitted
                            // fast path declines into this exact dispatch, and
                            // a site the resolver registers as an intrinsic
                            // never reaches the generic `invoke_info.push`
                            // below. Fixing only the method-entry door left
                            // every once-invoked hot loop — which is every
                            // method this door exists for — failing to compile
                            // and running interpreted.
                            if cratonvm_jit::string_intrinsic_declines_to_a_call(entry) {
                                let class_box: Box<str> =
                                    target_class.to_string().into_boxed_str();
                                let method_box: Box<str> = mn.to_string().into_boxed_str();
                                let desc_box: Box<str> = desc.to_string().into_boxed_str();
                                let class_ref = &*class_box as *const str;
                                let method_ref = &*method_box as *const str;
                                let desc_ref = &*desc_box as *const str;
                                owned_jit_strings2.push(class_box);
                                owned_jit_strings2.push(method_box);
                                owned_jit_strings2.push(desc_box);
                                // SAFETY: the three `Box<str>` were just pushed
                                // to `owned_jit_strings2`, which outlives the
                                // `JitInvokeInfo` and the code compiled against
                                // it.
                                let info = Box::new(crate::jit::JitInvokeInfo {
                                    class_name: unsafe { &*class_ref },
                                    method_name: unsafe { &*method_ref },
                                    descriptor: unsafe { &*desc_ref },
                                    // Receiver-INCLUDED, unlike
                                    // `JitDirectCall.num_params`.
                                    num_jit_args: num_params + 1,
                                    return_type: ret,
                                    invoke_kind,
                                    declaring_class_id: class_id.as_u32(),
                                });
                                let info_ptr: *const _ = &*info;
                                owned_jit_invoke_infos2.push(info);
                                invoke_info.push((pc, info_ptr));
                            }
                            direct_calls2.push((
                                pc,
                                crate::jit::JitDirectCall {
                                    entry,
                                    needs_context: false,
                                    num_params,
                                    return_type: ret,
                                    guard_class_id,
                                },
                            ));
                            continue;
                        }
                    }

                    // The layout-independent STATIC intrinsic families
                    // (`Math`/`StrictMath`, `Integer`/`Long` bit ops, …). This
                    // door previously recognised exactly one of them by hand —
                    // `Math.sqrt`, immediately below — so an OSR body paid full
                    // dispatch for `Math.abs`, `Math.min`/`max`,
                    // `Integer.bitCount`, `Long.numberOfTrailingZeros` and the
                    // rest, all of which lower to one or two instructions.
                    //
                    // Restricted to `invokestatic`: every member of those
                    // families is static, so `guard_class_id: 0` (no receiver
                    // guard) is exactly right, and the restriction also keeps
                    // the CRC32/CRC32C members — the only ones in
                    // `try_resolve_intrinsic` whose inline code is sound ONLY
                    // behind a resolved receiver class-id guard, which this
                    // door has no resolver for — off this path entirely.
                    if invoke_kind == 3 {
                        if let Some((entry, num_params, ret)) =
                            cratonvm_jit::try_resolve_intrinsic(target_class, mn, desc)
                        {
                            if !cratonvm_jit::JitIntrinsic::from_entry(entry)
                                .is_some_and(|i| i.is_crc32_family())
                            {
                                direct_calls2.push((
                                    pc,
                                    crate::jit::JitDirectCall {
                                        entry,
                                        needs_context: false,
                                        num_params,
                                        return_type: ret,
                                        guard_class_id: 0,
                                    },
                                ));
                                continue;
                            }
                        }
                    }

                    // ===== INTRINSIC REGION BEGIN: FFM_SEGMENT =====
                    // `MemorySegment.getAtIndex`/`setAtIndex` on the OSR /
                    // direct-bind door.
                    //
                    // Registered HERE as well as in the single-pass scan
                    // because an intrinsic registered in one door is inert in
                    // the others — the lesson this file already records for the
                    // `Thread.currentThread` and String binds. An engagement
                    // counter is what caught it: `publishes=4000001
                    // fast_hits=0` said the native was publishing verdicts on
                    // every element and compiled code was never asking, because
                    // the only door that ran was this one.
                    //
                    // `invoke_kind` 0/2 (virtual/interface), unlike the
                    // `try_resolve_intrinsic` block above which is
                    // `invokestatic`-only: these accessors are interface calls.
                    // No receiver class-id guard is needed or wanted — the
                    // helper asks the native for a verdict rather than
                    // speculating on a receiver class, and DECLINES into this
                    // site's ordinary dispatch for anything it does not
                    // recognise.
                    if matches!(invoke_kind, 0 | 2)
                        && target_class == "java/lang/foreign/MemorySegment"
                        && matches!(mn, "getAtIndex" | "setAtIndex")
                        && cratonvm_jit::ffm_kind_for_descriptor(desc).is_some()
                    {
                        let entry = if mn == "getAtIndex" {
                            cratonvm_jit::JitIntrinsic::FfmSegmentGetAtIndex.as_entry()
                        } else {
                            cratonvm_jit::JitIntrinsic::FfmSegmentSetAtIndex.as_entry()
                        };
                        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_FFM") {
                            eprintln!(
                                "[ffm] REGISTERED(bridge) {target_class}.{mn}{desc} @pc={pc} kind={invoke_kind}"
                            );
                        }
                        // The site's own dispatch info. REQUIRED, not
                        // incidental: the emitted fast path DECLINES into this
                        // exact dispatch for any carrier it does not recognise,
                        // and the emitter also reads the descriptor back off it
                        // to recover the element kind. Without it the emitter
                        // refuses the site (`info=false`) and nothing is gained.
                        let class_box: Box<str> = target_class.to_string().into_boxed_str();
                        let method_box: Box<str> = mn.to_string().into_boxed_str();
                        let desc_box: Box<str> = desc.to_string().into_boxed_str();
                        let class_ref = &*class_box as *const str;
                        let method_ref = &*method_box as *const str;
                        let desc_ref = &*desc_box as *const str;
                        owned_jit_strings2.push(class_box);
                        owned_jit_strings2.push(method_box);
                        owned_jit_strings2.push(desc_box);
                        // SAFETY: the three `Box<str>` were just pushed to
                        // `owned_jit_strings2`, which outlives the JitInvokeInfo
                        // and the code compiled against it.
                        let info = Box::new(crate::jit::JitInvokeInfo {
                            class_name: unsafe { &*class_ref },
                            method_name: unsafe { &*method_ref },
                            descriptor: unsafe { &*desc_ref },
                            // Receiver-INCLUDED, unlike `JitDirectCall.num_params`.
                            num_jit_args: param_count + 1,
                            return_type: cratonvm_jit::return_type(desc),
                            invoke_kind,
                            declaring_class_id: class_id.as_u32(),
                        });
                        let info_ptr: *const _ = &*info;
                        owned_jit_invoke_infos2.push(info);
                        invoke_info.push((pc, info_ptr));
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: false,
                                num_params: param_count,
                                return_type: cratonvm_jit::return_type(desc),
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // ===== INTRINSIC REGION END: FFM_SEGMENT =====

                    // Math.sqrt intrinsic: inline as SQRTSD (no dispatch overhead)
                    if invoke_kind == 3
                        && target_class == "java/lang/Math"
                        && mn == "sqrt"
                        && desc == "(D)D"
                    {
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry: crate::jit::MATH_SQRT_INTRINSIC,
                                needs_context: false,
                                num_params: 1,
                                return_type: b'D',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // `Thread.currentThread()` thin direct call — the JIT half
                    // of the funnel bypass the interpreter already has
                    // (`InterpIntrinsic::ThreadCurrentThread`). A statically
                    // bound NATIVE callee, so exactly like `Integer.valueOf`
                    // below the eager callee compile can never succeed, and
                    // every call paid `jit_invoke_dispatch` →
                    // `vm_exec::invoke_or_native`, which re-resolves the callee
                    // BY NAME and only then enters the funnel. The JDK calls it
                    // twice per uncontended `ReentrantLock` lock/unlock pair.
                    // See `jit::helpers::jit_thread_current_thread_direct` and
                    // native-call-funnel-per-call-floor-item2-20260805.md.
                    //
                    // Binding it in all THREE compile doors is the whole lesson
                    // of that document. A version bound only in
                    // `jit::try_compile`'s two ladders was completely inert:
                    // `CRATONVM_INTRINSIC_STATS=1` reported 0 bypasses across a
                    // loop that made 8,000,000 calls — and 0 invokestatic sites
                    // even EXAMINED by either of those ladders — because a hot
                    // loop is compiled HERE, by the OSR door, which reaches
                    // `x64::compile_with_param_slots` directly and carries its
                    // own copy of the ladder. No timing could have shown that:
                    // a 0 % change and a fast path that was never installed
                    // produce the same table.
                    if invoke_kind == 3
                        && target_class == "java/lang/Thread"
                        && mn == "currentThread"
                        && desc == "()Ljava/lang/Thread;"
                    {
                        // Address taken directly, for the same reason the
                        // `Integer.valueOf` bind below states: `build_helpers`
                        // registers the jit-crate atomic only AFTER this
                        // construction block, so reading it here would give 0
                        // on the first OSR compile in a process.
                        let entry = crate::jit::helpers::jit_thread_current_thread_direct
                            as *const () as usize;
                        cratonvm_jit::THREAD_CURRENT_THREAD_SITES_OSR
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // `Preconditions.checkIndex` / `Reference.reachabilityFence`
                    // thin direct calls — the THIRD door.
                    //
                    // Both are recognised in `jit::try_compile`'s single-pass
                    // ladder and in its IR path, and for `checkIndex` that was
                    // enough: it is reached through `Objects.checkIndex`, a JDK
                    // method the method-entry door compiles, so the bind landed
                    // inside the callee. `reachabilityFence` has no such
                    // intermediary — a hot loop calls it directly — and a hot
                    // loop's body is compiled HERE, by the OSR door, which runs
                    // its own callee-binding loop rather than that ladder.
                    // Wiring the other two doors and not this one bound
                    // `Preconditions.checkIndex=2 Reference.reachabilityFence=0`
                    // (`CRATONVM_DBG=jit-method-stats`) while the fence's cost
                    // did not move — 361 ns before, 142 after, against
                    // `Objects.checkIndex`'s 352 -> 15. Three doors, and the
                    // counter is what said which one was missing.
                    //
                    // Address taken directly rather than through the jit-crate
                    // atomic, for the reason the `Thread.currentThread` bind
                    // above states: `build_helpers` registers those cells only
                    // after this construction block, so reading one here yields
                    // 0 on the first OSR compile in a process.
                    if invoke_kind == 3
                        && cratonvm_jit::census_direct_helpers_enabled()
                        && target_class == "jdk/internal/util/Preconditions"
                        && mn == "checkIndex"
                        && desc == "(IILjava/util/function/BiFunction;)I"
                    {
                        let entry = crate::jit::helpers::jit_preconditions_check_index_direct
                            as *const () as usize;
                        cratonvm_jit::PRECONDITIONS_CHECK_INDEX_SITES
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 3,
                                return_type: b'I',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // `ByteBuffer.put(int,byte)` / `get(int)` and
                    // `MessageDigest.update(byte)` — AT THIS DOOR TOO, and this
                    // is the door that mattered.
                    //
                    // All three were bound in `try_compile_inner`'s ladder
                    // first, and the binds moved NOTHING: `MessageDigest.
                    // update(byte)` went 216 -> 195 ns and `ByteBuf.writeByte`
                    // 700 -> 607, against an expected ~16x. The reason is the
                    // one this file already states two arms up for
                    // `reachabilityFence`: a hot LOOP body is compiled HERE, by
                    // the OSR door, which runs its own callee-binding loop
                    // rather than that ladder — and `testHugeDecompress` is one
                    // loop, 268 million iterations, calling all three directly.
                    //
                    // Address taken directly rather than through the jit-crate
                    // atomic, for the reason the arms above state: `build_helpers`
                    // registers those cells only after this construction block.
                    //
                    // No `jdk_only` term here, matching every other arm at this
                    // door — and it is not a hole: each helper asks
                    // `jit_direct_helper_refused` on its own fast path and
                    // declines to the generic dispatcher, which is policy-checked.
                    //
                    // GATED, like the other two doors, and this is the door
                    // where the gate does the work — for the same reason the
                    // paragraph above gives about the bind itself. The helper
                    // serves DIRECT receivers; on a `HeapByteBuffer` it
                    // declines, and a declined call is worse than no bind at
                    // all, because an unbound site takes the inline cache
                    // straight to the compiled `HeapByteBuffer.get` (a bounds
                    // check and an array load). Measured on the two
                    // monomorphic control probes, one binary, bind on/off:
                    // heap 237 ns bound against 18-22 unbound, direct 52 ns
                    // bound against 132-138 unbound. Neither blanket answer is
                    // right, so the question is asked per site.
                    //
                    // `osr_receiver_profile` is the same profile this door
                    // already screens its guarded `String` intrinsics against,
                    // two hundred lines up — and the reason that fetch exists
                    // is the reason this gate does: a door with no profile
                    // emits a decision the interpreter's own observations
                    // contradict.
                    if invoke_kind == 0
                        && cratonvm_jit::nio_byte_direct_helpers_enabled()
                        && target_class == "java/nio/ByteBuffer"
                        && ((mn == "put" && desc == "(IB)Ljava/nio/ByteBuffer;")
                            || (mn == "get" && desc == "(I)B"))
                        && !cratonvm_jit::nio_byte_element_bind_refused(
                            osr_receiver_profile.as_ref(),
                            pc,
                            mn == "put",
                            "osr",
                        )
                    {
                        let is_put = mn == "put";
                        let entry = if is_put {
                            crate::jit::helpers::jit_dbb_put_byte_direct as *const () as usize
                        } else {
                            crate::jit::helpers::jit_dbb_get_byte_direct as *const () as usize
                        };
                        cratonvm_jit::NIO_BYTE_ELEMENT_SITES_OSR
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: if is_put { 2 } else { 1 },
                                return_type: if is_put { b'L' } else { b'I' },
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // `Buffer.session()` — a constant null behind a ~160 ns
                    // funnel, one crossing per multi-byte heap accessor. Bound
                    // at this door for the reason the block above is: a hot
                    // loop body is compiled HERE, and this is where a
                    // `ByteBuffer.getInt` loop's `session()` site lives.
                    //
                    // No receiver screen at compile time, and none is
                    // possible: `session()` is `final` on `java/nio/Buffer`, so
                    // a site names it through whatever buffer class the caller
                    // is, and whether the null-returning SHIM (rather than the
                    // real `getfield segment` bytecode) owns that receiver is a
                    // per-receiver runtime fact. The helper makes that check
                    // itself, against the classes the shim has actually served,
                    // and declines to the generic dispatcher otherwise.
                    if invoke_kind == 0
                        && cratonvm_jit::buffer_session_direct_enabled()
                        && cratonvm_jit::is_buffer_session_site(mn, desc)
                    {
                        cratonvm_jit::BUFFER_SESSION_SITES_OSR
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry: crate::jit::helpers::jit_buffer_session_direct as *const ()
                                    as usize,
                                needs_context: true,
                                num_params: 0,
                                // A `MemorySessionImpl` reference: the
                                // return-value ladder must oop-mark it, and the
                                // decline edge can return a real one.
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    if invoke_kind == 0
                        && cratonvm_jit::md_update_direct_helper_enabled()
                        && target_class == "java/security/MessageDigest"
                        && mn == "update"
                        && desc == "(B)V"
                    {
                        let entry =
                            crate::jit::helpers::jit_md_update_byte_direct as *const () as usize;
                        cratonvm_jit::MD_UPDATE_BYTE_SITES
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 1,
                                return_type: b'V',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    if invoke_kind == 3
                        && cratonvm_jit::census_direct_helpers_enabled()
                        && target_class == "java/lang/ref/Reference"
                        && mn == "reachabilityFence"
                        && desc == "(Ljava/lang/Object;)V"
                    {
                        let entry = crate::jit::helpers::jit_reachability_fence_direct as *const ()
                            as usize;
                        cratonvm_jit::REACHABILITY_FENCE_SITES
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 1,
                                return_type: b'V',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // `Integer.valueOf(I)` thin direct call — statically bound
                    // NATIVE callee, so the eager callee compile below can never
                    // succeed and the generic dispatch round trip is pure fixed
                    // overhead on the hottest autoboxing path. Parity with the
                    // recognition in `jit::try_compile`; see
                    // `jit::helpers::jit_integer_value_of_direct`. This OSR-tier
                    // site matters most: a single-invocation harness method
                    // (e.g. a benchmark main loop) runs its entire life inside
                    // the OSR body.
                    if invoke_kind == 3
                        && target_class == "java/lang/Integer"
                        && mn == "valueOf"
                        && desc == "(I)Ljava/lang/Integer;"
                    {
                        // VM crate: take the helper's address directly — no
                        // registration-order dependency. (This OSR path calls
                        // `build_helpers` — which registers the jit-crate
                        // atomic — only AFTER this construction block, so the
                        // first OSR compile in a process would read 0 there.)
                        let entry =
                            crate::jit::helpers::jit_integer_value_of_direct as *const () as usize;
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 1,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // `Long.valueOf(J)` — the twin of the `Integer.valueOf(I)`
                    // recognition directly above, at this door for the same
                    // stated reason: an OSR body is where a hot boxing loop
                    // actually runs. `num_params: 1`, not 2 — the JIT counts one
                    // operand slot per PARAMETER, not JVMS category-2 pairs.
                    if invoke_kind == 3
                        && cratonvm_jit::long_box_direct_helpers_enabled()
                        && target_class == "java/lang/Long"
                        && mn == "valueOf"
                        && desc == "(J)Ljava/lang/Long;"
                    {
                        let entry =
                            crate::jit::helpers::jit_long_value_of_direct as *const () as usize;
                        cratonvm_jit::LONG_VALUE_OF_SITES
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 1,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // ===== INTRINSIC REGION BEGIN: ATOMIC_INT =====
                    // `AtomicInteger` read-modify-write family, through the
                    // SAME matcher `jit::try_compile_inner` uses so the two
                    // doors cannot drift on which shapes are admitted.
                    //
                    // Registered here because this door reaches
                    // `x64::compile_with_param_slots` directly. For this family
                    // the OSR site is the load-bearing one, for the same reason
                    // spelled out on the HashMap arm below: a counter loop
                    // written inside ONE method never passes through
                    // `jit::try_compile`, and that is exactly the shape
                    // (`while (nextIndex.getAndIncrement() < MAX)`) this
                    // intrinsic exists to speed up.
                    //
                    // The class id comes off `cm_lock`, the guard this loop
                    // ALREADY holds, and not from a fresh `.read()`.
                    //
                    // This comment used to say "the class-manager guard is read
                    // and dropped inside the `let` so no lock is held across the
                    // matcher call". That was true and it was the wrong half of
                    // the question: the matcher never held one, and the LOOP
                    // did — `cm_lock` at the top of this `if
                    // !scan.invoke_ops.is_empty()` block is live for every
                    // iteration, because `class` is borrowed out of it. A second
                    // `.read()` here is therefore recursive.
                    //
                    // `parking_lot`'s `RwLock` read is not reentrant. A writer
                    // that arrives between the outer acquisition and the inner
                    // one parks the inner read BEHIND itself (readers do not
                    // barge past a queued writer), and the guard that writer is
                    // waiting for is the outer one this thread is holding. A
                    // background compile racing any class load is the whole
                    // window, and the `cratonvm-jit-co` thread is where it was
                    // caught: `vm-cli/tests/jit_compile_gate_doors.rs` failed
                    // with `lock order violation: attempted to acquire
                    // ClassManager (level 10) while holding ClassManager (level
                    // 10)` — the L10 `OrderedPlRwLock` reporting the deadlock
                    // one step before it could happen.
                    //
                    // Three regions in this loop had it (`AtomicInteger`,
                    // `AtomicLong`, and the box/unbox pair); all three now read
                    // through `cm_lock`, which is also strictly cheaper.
                    //
                    // The rule is not new here. `dispatch_virtual.rs`'s proxy
                    // walk states it in full — "walk the chain using the
                    // ALREADY-HELD `cm` guard rather than calling
                    // `class_chain_reaches_proxy_instance` (which takes its own
                    // read) -- a nested second read acquisition on the same
                    // thread self-deadlocks under parking_lot's writer-preferring
                    // fairness once any writer is queued" — and these three
                    // arms were written without it. `vm-cli/tests/
                    // jit_compile_gate_doors.rs::
                    // the_osr_door_takes_no_recursive_class_manager_lock` is the
                    // guard that now names the families rather than waiting for
                    // one to turn up in an unrelated probe.
                    //
                    // Review #80: a site naming a SUBCLASS (`Counter extends
                    // AtomicInteger`) matches only through the class that
                    // declares the resolved method, and only for a method `final`
                    // in the JDK. Its guard is the subclass's own id, so any
                    // other receiver class falls back. Both answers come off
                    // `cm_lock` too (`site_class_and_declaring_class_name`).
                    if invoke_kind == 0
                        && cratonvm_jit::atomic_intrinsic_site_may_match(
                            "java/util/concurrent/atomic/AtomicInteger",
                            &target_class,
                            &mn,
                            &desc,
                        )
                    {
                        let atomic_site: Option<(u32, Option<String>)> =
                            if target_class == "java/util/concurrent/atomic/AtomicInteger" {
                                cm_lock
                                    .find_bootstrap_class_by_name(
                                        "java/util/concurrent/atomic/AtomicInteger",
                                    )
                                    .map(|id| (id.as_u32(), None))
                            } else {
                                site_class_and_declaring_class_name(
                                    &cm_lock,
                                    class_id,
                                    &target_class,
                                    &mn,
                                    &desc,
                                )
                                .map(|(cid, declaring_class)| (cid, Some(declaring_class)))
                            };
                        if let Some((entry, num_params, ret, guard_class_id)) =
                            atomic_site.and_then(|(cid, declaring_class)| {
                                cratonvm_jit::try_resolve_atomic_intrinsic_for_site(
                                    &target_class,
                                    declaring_class.as_deref(),
                                    &mn,
                                    &desc,
                                    cid,
                                )
                            })
                        {
                            direct_calls2.push((
                                pc,
                                crate::jit::JitDirectCall {
                                    entry,
                                    needs_context: false,
                                    num_params,
                                    return_type: ret,
                                    guard_class_id,
                                },
                            ));
                            continue;
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_INT =====

                    // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG =====
                    // The OSR door's copy of the 64-bit family. It has to be
                    // here as well as in `jit::try_compile`: an intrinsic
                    // registered in one compile door is INERT in the others,
                    // which is the failure this file's own
                    // `Thread.currentThread` and String binds each record once.
                    //
                    // It matters most exactly where OSR matters: a
                    // single-invocation method whose whole life is one hot
                    // loop. `HashedWheelTimer`'s worker is that shape, and its
                    // `pendingTimeouts` is an `AtomicLong` incremented once per
                    // scheduled timeout and decremented once per expiry.
                    // Subclass sites match through the declaring class, on the
                    // AtomicInteger arm's terms (which excludes `longValue()`).
                    if invoke_kind == 0
                        && cratonvm_jit::atomic_intrinsic_site_may_match(
                            "java/util/concurrent/atomic/AtomicLong",
                            &target_class,
                            &mn,
                            &desc,
                        )
                    {
                        // Through `cm_lock` — see the AtomicInteger arm above for
                        // why a fresh `.read()` here is a recursive acquisition.
                        let atomic_long_site: Option<(u32, Option<String>)> =
                            if target_class == "java/util/concurrent/atomic/AtomicLong" {
                                cm_lock
                                    .find_bootstrap_class_by_name(
                                        "java/util/concurrent/atomic/AtomicLong",
                                    )
                                    .map(|id| (id.as_u32(), None))
                            } else {
                                site_class_and_declaring_class_name(
                                    &cm_lock,
                                    class_id,
                                    &target_class,
                                    &mn,
                                    &desc,
                                )
                                .map(|(cid, declaring_class)| (cid, Some(declaring_class)))
                            };
                        if let Some((entry, num_params, ret, guard_class_id)) = atomic_long_site
                            .and_then(|(cid, declaring_class)| {
                                cratonvm_jit::try_resolve_atomic_long_intrinsic_for_site(
                                    &target_class,
                                    declaring_class.as_deref(),
                                    &mn,
                                    &desc,
                                    cid,
                                )
                            })
                        {
                            direct_calls2.push((
                                pc,
                                crate::jit::JitDirectCall {
                                    entry,
                                    needs_context: false,
                                    num_params,
                                    return_type: ret,
                                    guard_class_id,
                                },
                            ));
                            continue;
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_LONG =====

                    // ===== INTRINSIC REGION BEGIN: BOX_UNBOX =====
                    // `Long.longValue()` / `Integer.intValue()` emitted INLINE.
                    //
                    // Deliberately placed BEFORE the two thin direct binds
                    // below, which recognise the same two triples: whichever
                    // arm runs first `continue`s, so this ordering is what
                    // decides that the site gets an inline `MOV` rather than a
                    // CALL. The binds stay as the fallback for a site whose
                    // receiver class id does not resolve.
                    //
                    // THIS is the load-bearing door for the workload that
                    // motivates the intrinsic, and the reason it is not enough
                    // to add it to `try_compile` alone. An autoboxed counter
                    // lives in a LOOP BODY — `probes/BlobStreamCostCpu.java`'s
                    // `boxed Long counter` arm is `if (count > 0) { count--; }`
                    // — and a loop body is what OSR compiles: that probe
                    // reports `osr_entered=51` against `method-entry:
                    // admitted=2`. A single-pass-only intrinsic would report
                    // sites and move nothing, which is the exact failure the
                    // `VarHandle` bind below records for itself.
                    if invoke_kind == 0
                        && (target_class == "java/lang/Long" || target_class == "java/lang/Integer")
                    {
                        // Through `cm_lock` — see the AtomicInteger arm above for
                        // why a fresh `.read()` here is a recursive acquisition.
                        let box_cid = cm_lock
                            .find_bootstrap_class_by_name(&target_class)
                            .map(|id| id.as_u32());
                        if let Some((entry, num_params, ret, guard_class_id)) =
                            box_cid.and_then(|cid| {
                                cratonvm_jit::try_resolve_box_unbox_intrinsic(
                                    &target_class,
                                    &mn,
                                    &desc,
                                    cid,
                                )
                            })
                        {
                            direct_calls2.push((
                                pc,
                                crate::jit::JitDirectCall {
                                    entry,
                                    needs_context: false,
                                    num_params,
                                    return_type: ret,
                                    guard_class_id,
                                },
                            ));
                            continue;
                        }
                    }
                    // ===== INTRINSIC REGION END: BOX_UNBOX =====

                    // `Integer.intValue()` thin direct call — `Integer` is
                    // `final`, so a site declared against it is statically
                    // monomorphic (guard-free); the helper handles the
                    // null-receiver NPE itself.
                    if invoke_kind == 0
                        && target_class == "java/lang/Integer"
                        && mn == "intValue"
                        && desc == "()I"
                    {
                        let entry =
                            crate::jit::helpers::jit_integer_int_value_direct as *const () as usize;
                        cratonvm_jit::note_integer_int_value_direct_site();
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'I',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // `Long.longValue()` — the twin of `Integer.intValue()`
                    // directly above. `java/lang/Long` is `final` on the same
                    // terms, so a site declared against it is statically
                    // monomorphic and needs no receiver guard; the helper
                    // handles the null-receiver NPE and declines anything that
                    // fails heap validation to the generic dispatcher.
                    if invoke_kind == 0
                        && cratonvm_jit::long_box_direct_helpers_enabled()
                        && target_class == "java/lang/Long"
                        && mn == "longValue"
                        && desc == "()J"
                    {
                        let entry =
                            crate::jit::helpers::jit_long_long_value_direct as *const () as usize;
                        cratonvm_jit::LONG_LONG_VALUE_SITES
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        direct_calls2.push((
                            pc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'J',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                    // `VarHandle` read modes on an instance field with a
                    // primitive return — parity with `jit::try_compile`'s
                    // recognition (see `cratonvm_jit::VARHANDLE_READ_DIRECT_FNS`).
                    //
                    // THIS is the load-bearing door for the workload that
                    // motivates the bind. netty checks `refCnt` on every
                    // buffer accessor, so the reads happen inside the
                    // byte-transfer LOOPS of `SnappyFrameDecoder` and
                    // `ByteToMessageDecoder`, and a loop body is what OSR
                    // compiles. A single-pass-only bind would report sites and
                    // move nothing.
                    //
                    // Unlike its neighbours here this one asks the policy
                    // question, because the answer is not the same for every
                    // bind: the four read modes are registered
                    // `NativeKind::Bridge`, which JDK-ONLY-WAVE2 §1.4 does not
                    // permit a compile-time bake of. `dispatch_policy` is the
                    // same source `jit::try_compile`'s `jdk_only` argument comes
                    // from, so both doors refuse together.
                    if invoke_kind == 0
                        && cratonvm_jit::varhandle_read_direct_helpers_enabled()
                        && target_class == "java/lang/invoke/VarHandle"
                        && !crate::vm::dispatch_policy(shared).is_jdk_only()
                    {
                        if let Some(slot) = cratonvm_jit::varhandle_read_helper_slot(&mn, &desc) {
                            // The helper address is taken directly rather than
                            // read out of the jit-crate cell, for the
                            // registration-order reason spelled out on the
                            // `Integer.valueOf` recognition above: this path can
                            // run before `build_helpers` has published them.
                            let entry = crate::jit::helpers::varhandle_read_direct_fn(slot);
                            cratonvm_jit::VARHANDLE_READ_SITES_OSR
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            direct_calls2.push((
                                pc,
                                crate::jit::JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 1,
                                    return_type: cratonvm_jit::varhandle_read_slot_return(slot),
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                    // `VarHandle` write modes on an instance field — parity
                    // with `jit::try_compile`'s recognition (see
                    // `cratonvm_jit::VARHANDLE_WRITE_DIRECT_FNS`).
                    //
                    // THIS door is the load-bearing one, for the same reason
                    // the read bind above says it is, and the write bind
                    // learned it the expensive way: bound in the single-pass
                    // ladder ALONE, a native census of `HibfixVarHandleProbe`
                    // still counted 698 000 `VarHandle.set` dispatches with the
                    // bind on and 698 000 with it off — identical, because the
                    // probe's stores are in a `main` loop and a loop body is
                    // what OSR compiles. The site counter said "bound"; the
                    // census said "moved nothing".
                    //
                    // Asks the policy question for the same reason: `set` is
                    // registered `NativeKind::Bridge`, which JDK-ONLY-WAVE2
                    // §1.4 does not permit a compile-time bake of.
                    if invoke_kind == 0
                        && cratonvm_jit::varhandle_write_direct_helpers_enabled()
                        && target_class == "java/lang/invoke/VarHandle"
                        && !crate::vm::dispatch_policy(shared).is_jdk_only()
                    {
                        if let Some(slot) = cratonvm_jit::varhandle_write_helper_slot(&mn, &desc) {
                            // Address taken directly rather than out of the
                            // jit-crate cell — this path can run before
                            // `build_helpers` has published them.
                            let entry = crate::jit::helpers::varhandle_write_direct_fn(slot);
                            cratonvm_jit::VARHANDLE_WRITE_SITES_OSR
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            direct_calls2.push((
                                pc,
                                crate::jit::JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 2,
                                    return_type: b'V',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                    // `VarHandle.compareAndSet` on an instance field — parity
                    // with `jit::try_compile`'s recognition (see
                    // `cratonvm_jit::VARHANDLE_CAS_DIRECT_FNS`).
                    //
                    // THIS door is the load-bearing one, for exactly the reason
                    // the write bind above records in full: a benchmark-style
                    // `main` loop, and `CompletableFuture.tryPushStack` reached
                    // from one, live their whole lives inside an OSR body and
                    // never pass through `jit::try_compile`.
                    if invoke_kind == 0
                        && cratonvm_jit::varhandle_cas_direct_helpers_enabled()
                        && target_class == "java/lang/invoke/VarHandle"
                        && !crate::vm::dispatch_policy(shared).is_jdk_only()
                    {
                        if let Some(slot) = cratonvm_jit::varhandle_cas_helper_slot(&mn, &desc) {
                            // Address taken directly rather than out of the
                            // jit-crate cell — this path can run before
                            // `build_helpers` has published them.
                            let entry = crate::jit::helpers::varhandle_cas_direct_fn(slot);
                            cratonvm_jit::VARHANDLE_CAS_SITES_OSR
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            direct_calls2.push((
                                pc,
                                crate::jit::JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 3,
                                    return_type: b'Z',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                    // Exact-HashMap `put`/`get` thin direct calls — parity
                    // with `jit::try_compile`'s recognition (guard-free: the
                    // helper verifies the receiver's EXACT class and routes
                    // subclasses / non-overlay cases to the full generic
                    // dispatcher). As with `Integer.valueOf` above, this
                    // OSR-tier site is the load-bearing one: a once-invoked
                    // benchmark-style method runs its whole life inside the
                    // OSR body and never passes through `jit::try_compile`.
                    if invoke_kind == 0 && target_class == "java/util/HashMap" {
                        let recognized = if mn == "put"
                            && desc == "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
                        {
                            Some((
                                crate::jit::helpers::jit_hashmap_put_direct as *const () as usize,
                                2usize,
                            ))
                        } else if mn == "get" && desc == "(Ljava/lang/Object;)Ljava/lang/Object;" {
                            Some((
                                crate::jit::helpers::jit_hashmap_get_direct as *const () as usize,
                                1usize,
                            ))
                        } else {
                            None
                        };
                        if let Some((entry, num_params)) = recognized {
                            direct_calls2.push((
                                pc,
                                crate::jit::JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    // Trivial constructor: defer for after-lock elidability
                    // resolution. Checked BEFORE the eager-compile scheduling
                    // below, because a `<init>()V` that turns out to be
                    // elidable must be elided rather than called — the
                    // resolution needs `load_class_concurrent`, so it cannot
                    // happen here under the lock.
                    if invoke_kind == 1 && mn == "<init>" && desc == "()V" && !ctor_direct_call_off
                    {
                        pending_ctor_sites.push((pc, target_class.to_string(), param_count));
                        continue;
                    }

                    // Schedule eager callee compilation (after lock release) for
                    // every STATICALLY BOUND site: `invokestatic` (kind 3) and
                    // `invokespecial` (kind 1). See `pending_callee_compiles`
                    // for what admitting kind 1 was worth and why it is safe —
                    // the bind applies the same four refusals either way.
                    if matches!(invoke_kind, 1 | 3) && !is_recursive_call {
                        pending_callee_compiles.push((
                            pc,
                            target_class.to_string(),
                            mn.to_string(),
                            desc.to_string(),
                            param_count,
                            invoke_kind,
                        ));
                        continue;
                    }

                    let num_jit_args = if invoke_kind == 3 {
                        param_count
                    } else {
                        param_count + 1
                    };
                    let return_type = crate::jit::return_type(desc);
                    let class_box: Box<str> = target_class.to_string().into_boxed_str();
                    let method_box: Box<str> = mn.to_string().into_boxed_str();
                    let desc_box: Box<str> = desc.to_string().into_boxed_str();
                    let class_ref = &*class_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                    let method_ref = &*method_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                    let desc_ref = &*desc_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                    owned_jit_strings2.push(class_box);
                    owned_jit_strings2.push(method_box);
                    owned_jit_strings2.push(desc_box);
                    // SAFETY: class_ref, method_ref, desc_ref point into the boxed strs that were just pushed to owned_jit_strings2, which outlives the JitInvokeInfo.
                    let info = Box::new(crate::jit::JitInvokeInfo {
                        class_name: unsafe { &*class_ref },
                        method_name: unsafe { &*method_ref },
                        descriptor: unsafe { &*desc_ref },
                        num_jit_args,
                        return_type,
                        invoke_kind,
                        declaring_class_id: class_id.as_u32(),
                    });
                    let info_ptr: *const _ = &*info;
                    owned_jit_invoke_infos2.push(info);
                    invoke_info.push((pc, info_ptr));
                    allocate_dynamic_dispatch_slots(
                        invoke_kind,
                        pc,
                        &mut mic_slots2,
                        &mut owned_mic_slots2,
                        &mut pic_slots2,
                        &mut owned_pic_slots2,
                    );
                }
            }

            // invokedynamic-uncommon-trap fix: resolve each `indy_ops` site's
            // target descriptor to the minimal stack-effect info the codegen
            // needs (arg slot count + return type tag) — see the `indy_info`
            // field doc on the x64 `Compiler` struct. A site that cannot be
            // resolved is simply omitted; the x64 codegen's 0xba arm then
            // bails the whole compile (`return false`) rather than guessing.
            let mut indy_info: Vec<(usize, usize, u8, Vec<u8>, usize)> = Vec::new();
            if !scan.indy_ops.is_empty() {
                let cm_lock = shared.classes.class_manager.read();
                if let Some(class) = cm_lock.get_class(class_id) {
                    for &(pc_indy, cp_idx) in &scan.indy_ops {
                        if let Some(ConstantPoolEntry::InvokeDynamic {
                            name_and_type_index,
                            ..
                        }) = class.constant_pool.get(cp_idx)
                        {
                            if let Some((_, descriptor)) =
                                class.constant_pool.get_name_and_type(*name_and_type_index)
                            {
                                let arg_slots = crate::jit::count_param_slots(descriptor);
                                let ret_type = crate::jit::return_type(descriptor);
                                let arg_type_tags = crate::jit::indy_arg_type_tags(descriptor);
                                let bridge_site = crate::runtime::invokedynamic::make_jit_indy_bridge_site_from_parts(
                                    &class.constant_pool,
                                    &class.bootstrap_methods,
                                    cp_idx,
                                    class_id,
                                )
                                .unwrap_or(0);
                                indy_info.push((
                                    pc_indy,
                                    arg_slots,
                                    ret_type,
                                    arg_type_tags,
                                    bridge_site,
                                ));
                            }
                        }
                    }
                }
            }

            // RBC.7 (relaxed) — an OSR frame cannot safely take the generic
            // indy uncommon trap: the bail resumes the pre-OSR interpreter
            // frame at the stale back-edge, silently re-running a loop whose
            // side effects already committed (see
            // jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md).
            // Admit only BRIDGED sites, which emit a direct call and never
            // deopt at the indy bci. `indy_info` drops sites it cannot resolve,
            // so a length mismatch also means "not fully bridged" and is
            // refused here.
            //
            // The bridged set grew on 2026-08-23 from `StringConcatFactory`
            // alone to `LambdaMetafactory` as well, and this gate is the reason
            // that matters as much as the whole-method one: `osr-DENY
            // (unbridged invokedynamic)` is METHOD-WIDE — one lambda creation
            // anywhere in a method denied OSR to every loop in it, for the life
            // of the process.
            if indy_info.len() != scan.indy_ops.len()
                || indy_info
                    .iter()
                    .any(|(_, _, ret_type, _, bridge_site)| *bridge_site == 0 || *ret_type == b'V')
            {
                if crate::runtime::env_cache::dbg_jitc() && !scan.indy_ops.is_empty() {
                    eprintln!(
                        "[cratonvm-jitc] osr-DENY (unbridged invokedynamic) {}.{}{}",
                        class_name, method_name, method_descriptor
                    );
                }
                return None;
            }

            // Resolve the deferred trivial-ctor sites FIRST (cm_lock released),
            // because the answer decides which of two later paths each site
            // takes: an elidable `C.<init>()V` is emitted AS
            // `java/lang/Object.<init>` so the codegen elision drops it
            // entirely, while a NON-elidable one now joins
            // `pending_callee_compiles` for the same eager-compile + direct
            // bind every other statically-bound site gets. It used to fall
            // straight to dispatch — the `new FastThreadLocal<Boolean>()`
            // shape, and the reason that loop paid ~300 ns per iteration.
            //
            // Pcs whose `<init>()V` target `is_elidable_construction` PROVED empty. The
            // backend may elide only these; a no-arg constructor that is NOT proven empty
            // keeps both its allocation and its call, because eliding it would drop
            // whatever the body writes to global state (see
            // jit-elided-constructor-side-effects-FIXED-20260812.md).
            let mut elidable_init_pcs: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            for (pc, tclass, pcount) in pending_ctor_sites {
                let elidable = resolve_cp_class_for_owner(shared, class_id, &tclass)
                    .map(|tid| {
                        let cm2 = shared.classes.class_manager.read();
                        is_elidable_construction(shared, &cm2, tid)
                    })
                    .unwrap_or(false);
                if elidable {
                    elidable_init_pcs.insert(pc);
                    // Emitted as `Object.<init>` so the codegen elision fires;
                    // no call survives, so there is nothing to bind.
                    let class_box: Box<str> = "java/lang/Object".to_string().into_boxed_str();
                    let method_box: Box<str> = "<init>".to_string().into_boxed_str();
                    let desc_box: Box<str> = "()V".to_string().into_boxed_str();
                    let class_ref = &*class_box as *const str;
                    let method_ref = &*method_box as *const str;
                    let desc_ref = &*desc_box as *const str;
                    owned_jit_strings2.push(class_box);
                    owned_jit_strings2.push(method_box);
                    owned_jit_strings2.push(desc_box);
                    // SAFETY: refs point into the boxed strs just pushed to
                    // owned_jit_strings2, which outlives the JitInvokeInfo.
                    let info = Box::new(crate::jit::JitInvokeInfo {
                        class_name: unsafe { &*class_ref },
                        method_name: unsafe { &*method_ref },
                        descriptor: unsafe { &*desc_ref },
                        num_jit_args: pcount + 1, // receiver + params
                        return_type: b'V',
                        invoke_kind: 1,
                        declaring_class_id: class_id.as_u32(),
                    });
                    let info_ptr: *const _ = &*info;
                    owned_jit_invoke_infos2.push(info);
                    invoke_info.push((pc, info_ptr));
                } else if !osr_ctor_bind_off {
                    // NOT elidable, and the site joins `pending_callee_compiles`
                    // for the same eager-compile + direct bind every other
                    // statically-bound site in this door gets. This is what the
                    // `new FastThreadLocal<Boolean>()` loop was paying a
                    // per-allocation `jit_invoke_dispatch` for.
                    //
                    // HISTORY, because the obvious reading of it is wrong.
                    // This reroute was tried on 2026-08-13 and REVERTED: it made
                    // `compile_with_param_slots` refuse the enclosing method,
                    // and an OSR refusal is not a fallback to a slower compile —
                    // it marks the method OSR-denied for the process lifetime,
                    // so the hot loop interpreted forever (`new A()` 311 ns ->
                    // 1412 ns). The page filed that as "why the codegen refuses
                    // that shape is unresolved".
                    //
                    // It was not the shape. A direct-bound site ALSO needs a
                    // `JitInvokeInfo` — the codegen's direct-call arm reads it
                    // to name the callee for the exceptional-return service —
                    // and this door pushed none, which is exactly the refusal
                    // the sibling non-`()V` `invokespecial` admission hit and
                    // fixed a few hundred lines below ("an `invokespecial` bind
                    // without it makes `compile_with_param_slots` refuse the
                    // whole method"). That fix landed for kind-1 sites arriving
                    // through the scan loop; `()V` ctor sites arrive through
                    // HERE, bypassed it, and so still had none. Routing them
                    // into the same list makes them take the same bind, and the
                    // `JitInvokeInfo` comes with it.
                    //
                    // `CRATONVM_NO_OSR_CTOR_BIND=1` restores the dispatch.
                    pending_callee_compiles.push((
                        pc,
                        tclass,
                        "<init>".to_string(),
                        "()V".to_string(),
                        pcount,
                        1u8,
                    ));
                } else {
                    // `CRATONVM_NO_OSR_CTOR_BIND=1`: keep the per-allocation
                    // dispatch, the pre-2026-08-17 behaviour.
                    let class_box: Box<str> = tclass.into_boxed_str();
                    let method_box: Box<str> = "<init>".to_string().into_boxed_str();
                    let desc_box: Box<str> = "()V".to_string().into_boxed_str();
                    let class_ref = &*class_box as *const str;
                    let method_ref = &*method_box as *const str;
                    let desc_ref = &*desc_box as *const str;
                    owned_jit_strings2.push(class_box);
                    owned_jit_strings2.push(method_box);
                    owned_jit_strings2.push(desc_box);
                    // SAFETY: refs point into the boxed strs just pushed to
                    // owned_jit_strings2, which outlives the JitInvokeInfo.
                    let info = Box::new(crate::jit::JitInvokeInfo {
                        class_name: unsafe { &*class_ref },
                        method_name: unsafe { &*method_ref },
                        descriptor: unsafe { &*desc_ref },
                        num_jit_args: pcount + 1, // receiver + params
                        return_type: b'V',
                        invoke_kind: 1,
                        declaring_class_id: class_id.as_u32(),
                    });
                    let info_ptr: *const _ = &*info;
                    owned_jit_invoke_infos2.push(info);
                    invoke_info.push((pc, info_ptr));
                }
            }

            // Eagerly compile statically-bound callees (class_manager lock released)
            //
            // Every baked direct-call target must stay mapped until this caller
            // is published, because publication is what roots them
            // (`JitCache::prepare_for_publication` -> `_direct_callee_roots`).
            // Dropping the callee `Arc` here would let a concurrent tier-up
            // `put` unmap a body whose address is already baked into the
            // machine code being emitted.
            let mut baked_callee_pins: Vec<std::sync::Arc<cratonvm_jit::CompiledMethod>> =
                Vec::new();
            for (ipc, callee_class, callee_method, callee_desc, param_count, site_invoke_kind) in
                pending_callee_compiles
            {
                let compiled_callee =
                    // Eager direct-call callee compile — optimized (C2) tier.
                    try_jit_compile_callee(
                        shared,
                        &callee_class,
                        &callee_method,
                        &callee_desc,
                        true,
                    );
                // `CRATONVM_DBG_OSR_BIND=1` names, per call site, whether this
                // OSR artifact bound a direct machine-code CALL or fell back to
                // the dispatch helper, and WHICH gate refused. Without it the
                // two outcomes are indistinguishable from outside, and they are
                // ~4.6x apart on a call-dense loop — see
                // `docs/known-issues/netty/httpresponsestatustest-exhaustive-loop-timeout-20260816.md`.
                let dbg_bind =
                    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OSR_BIND");
                if let Some((callee_pin, entry, needs_ctx)) = compiled_callee {
                    baked_callee_pins.push(callee_pin);
                    let refuse_dispatch = crate::jit::jit_direct_call_requires_dispatch(
                        &callee_class,
                        &callee_method,
                        &callee_desc,
                    );
                    let refuse_handlers = osr_callee_bars_direct_call(
                        shared,
                        class_id,
                        &callee_class,
                        &callee_method,
                        &callee_desc,
                    );
                    // jit-invokedynamic-groovy-regression fix: never bake a
                    // direct machine-code CALL to an indy-trap-bearing
                    // artifact — see the matching gate in `callee_compiler`.
                    let refuse_indy = crate::jit::helpers::compiled_entry_has_indy_trap(
                        shared,
                        &callee_class,
                        &callee_method,
                        &callee_desc,
                    );
                    if dbg_bind {
                        eprintln!(
                            "[osr-bind] {}.{}{} @pc={} compiled=yes direct={} entry={:#x} requires_dispatch={} declares_handlers={} indy_trap={}",
                            callee_class,
                            callee_method,
                            callee_desc,
                            ipc,
                            !(refuse_dispatch || refuse_handlers.is_some() || refuse_indy),
                            entry,
                            refuse_dispatch,
                            refuse_handlers
                                .map(|r| cratonvm_jit::DIRECT_BIND_REFUSAL_NAMES[r as usize])
                                .unwrap_or("no"),
                            refuse_indy
                        );
                    }
                    if !refuse_dispatch && refuse_handlers.is_none() && !refuse_indy {
                        // The OSR door's engagement counter. See
                        // `note_osr_direct_callee_bind_hit`: this ladder used to
                        // bind and refuse without touching the census at all, so
                        // `direct callee binds: 0 bound, 0 left` was printed by a
                        // process whose hot loops were doing both.
                        cratonvm_jit::note_osr_direct_callee_bind_hit();
                        direct_calls2.push((
                            ipc,
                            crate::jit::JitDirectCall {
                                entry,
                                needs_context: needs_ctx,
                                // Receiver-EXCLUDED: the instance arm of the
                                // codegen adds it back (`callee_params + 1`).
                                num_params: param_count,
                                return_type: crate::jit::return_type(&callee_desc),
                                guard_class_id: 0,
                            },
                        ));
                        // Keep this callee's body mapped for the lifetime of
                        // the artifact we are about to emit — see the
                        // declaration of `osr_direct_callee_entries`.
                        // `baked_callee_pins` only covers the emit window.
                        osr_direct_callee_entries.push(entry);
                        // A direct-bound site ALSO needs its `JitInvokeInfo`.
                        // The codegen's direct-call arm reads it to name the
                        // callee for the exceptional-return service, and
                        // `jit/src/lib.rs`'s single-pass ladder registers one
                        // for exactly this reason ("582 sites in one run had no
                        // service check; every one of them is a place an orphan
                        // can be minted"). This door never did — tolerated
                        // while only `invokestatic` was bound here, but an
                        // `invokespecial` bind without it makes
                        // `compile_with_param_slots` refuse the whole method,
                        // so the OSR artifact never materialises and the loop
                        // silently runs interpreted forever
                        // (`OSR-recompile reason=no-cached-artifact`, repeating).
                        let class_box: Box<str> = callee_class.clone().into_boxed_str();
                        let method_box: Box<str> = callee_method.clone().into_boxed_str();
                        let desc_box: Box<str> = callee_desc.clone().into_boxed_str();
                        let class_ref = &*class_box as *const str;
                        let method_ref = &*method_box as *const str;
                        let desc_ref = &*desc_box as *const str;
                        owned_jit_strings2.push(class_box);
                        owned_jit_strings2.push(method_box);
                        owned_jit_strings2.push(desc_box);
                        // SAFETY: refs point into the boxed strs just pushed to
                        // owned_jit_strings2, which outlives the JitInvokeInfo.
                        let info = Box::new(crate::jit::JitInvokeInfo {
                            class_name: unsafe { &*class_ref },
                            method_name: unsafe { &*method_ref },
                            descriptor: unsafe { &*desc_ref },
                            // Receiver-INCLUDED here, unlike `num_params` above.
                            num_jit_args: if site_invoke_kind == 3 {
                                param_count
                            } else {
                                param_count + 1
                            },
                            return_type: crate::jit::return_type(&callee_desc),
                            invoke_kind: site_invoke_kind,
                            declaring_class_id: class_id.as_u32(),
                        });
                        let info_ptr: *const _ = &*info;
                        owned_jit_invoke_infos2.push(info);
                        invoke_info.push((ipc, info_ptr));
                        continue;
                    }
                    // Bound-then-refused: the callee compiled, and one of the
                    // three gates above sent the site to the helper anyway.
                    // Attributed rather than lumped in with "not compiled yet",
                    // because the two want opposite fixes -- a compile-ORDER
                    // miss is re-bindable, a policy refusal is not.
                    cratonvm_jit::note_osr_direct_callee_bind_miss(if refuse_dispatch {
                        cratonvm_jit::DirectBindRefusal::EagerChainCycle
                    } else if let Some(reason) = refuse_handlers {
                        reason
                    } else {
                        cratonvm_jit::DirectBindRefusal::IndyTrap
                    });
                } else {
                    cratonvm_jit::note_osr_direct_callee_bind_miss(
                        cratonvm_jit::DirectBindRefusal::CalleeNotYetCompiled,
                    );
                }

                // Compilation failed, or the callee participates in a recursive
                // compile cycle. Fall back to the guarded dispatch helper.
                if dbg_bind {
                    eprintln!(
                        "[osr-bind] {}.{}{} @pc={} -> DISPATCH HELPER",
                        callee_class, callee_method, callee_desc, ipc
                    );
                }
                let class_box: Box<str> = callee_class.into_boxed_str();
                let method_box: Box<str> = callee_method.into_boxed_str();
                let desc_box: Box<str> = callee_desc.clone().into_boxed_str();
                let class_ref = &*class_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                let method_ref = &*method_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                let desc_ref = &*desc_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                owned_jit_strings2.push(class_box);
                owned_jit_strings2.push(method_box);
                owned_jit_strings2.push(desc_box);
                let return_type = crate::jit::return_type(&callee_desc);
                // SAFETY: class_ref, method_ref, desc_ref point into the boxed strs that were just pushed to owned_jit_strings2, which outlives the JitInvokeInfo.
                let info = Box::new(crate::jit::JitInvokeInfo {
                    class_name: unsafe { &*class_ref },
                    method_name: unsafe { &*method_ref },
                    descriptor: unsafe { &*desc_ref },
                    // Receiver-INCLUDED, unlike `JitDirectCall.num_params`.
                    num_jit_args: if site_invoke_kind == 3 {
                        param_count
                    } else {
                        param_count + 1
                    },
                    return_type,
                    // The site's own kind. Hard-coding 3 here was correct while
                    // only `invokestatic` reached this loop; with kind 1
                    // admitted it would make the dispatch helper resolve a
                    // constructor as a static call and drop the receiver.
                    invoke_kind: site_invoke_kind,
                    declaring_class_id: class_id.as_u32(),
                });
                let info_ptr: *const _ = &*info;
                owned_jit_invoke_infos2.push(info);
                invoke_info.push((ipc, info_ptr));
            }

            // Resolve ldc/ldc_w constants. String constants are wired the
            // same way as `jit::try_compile`'s cp_ldc_resolver (boxed text
            // retained via `owned_jit_strings2` → `cm._jit_strings`, codegen
            // materializes through `helpers.ldc_string`). Before this
            // (perf/halfgap-20260717), ANY method containing `ldc "str"`
            // silently failed its OSR-artifact compile and got permanently
            // OSR-denied — a once-invoked method with a hot loop after a
            // string constant (StringRegexOnly.run: `Pattern.compile("(\\d+)")`)
            // then interpreted its entire workload.
            let mut ldc_info2: Vec<(usize, i64)> = Vec::new();
            let mut ldc_string_info2: Vec<(usize, u32, u16)> = Vec::new();
            // Class-`ldc` sites, served at run time by `helpers.ldc_class_cp`.
            // Before this an OSR artifact refused any method containing one.
            let mut ldc_class_info2: Vec<(usize, u32, u16)> = Vec::new();
            // The `ldc`-family pcs whose constant is floating-point. Codegen
            // types these by their consuming opcode; the deopt operand-stack
            // snapshot has none to ask, and without the tag every numeric `ldc`
            // read as `Unsupported` and refused OSR entry for the whole
            // artifact — see `x64::Compiler::ldc_fp_pcs`.
            let mut ldc_fp_pcs2: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
            if !scan.ldc_ops.is_empty() {
                let cm_lock = shared.classes.class_manager.read();
                let class = cm_lock.get_class(class_id)?;
                for &(pc, cp_idx) in &scan.ldc_ops {
                    let val = match class.constant_pool.get(cp_idx) {
                        Some(ConstantPoolEntry::Integer(v)) => *v as i64, // JVM spec: bounded float-to-long conversion
                        Some(ConstantPoolEntry::Float(v)) => {
                            ldc_fp_pcs2.insert(pc);
                            v.to_bits() as i64 // Cast: JIT ABI -- float bits to i64
                        }
                        Some(ConstantPoolEntry::StringReference { string_index })
                            if class.constant_pool.get_utf8_wide(*string_index).is_none() =>
                        {
                            // The SITE, for the reason the `try_compile`
                            // resolver records one: the recorded resolution is
                            // keyed `(class, cp index)`. `get_utf8` is only the
                            // representability test.
                            match class.constant_pool.get_utf8(*string_index) {
                                Some(_) => {
                                    ldc_string_info2.push((pc, class_id.as_u32(), cp_idx));
                                    continue;
                                }
                                None => return None,
                            }
                        }
                        Some(ConstantPoolEntry::ClassReference { .. }) => {
                            ldc_class_info2.push((pc, class_id.as_u32(), cp_idx));
                            continue;
                        }
                        _ => return None, // wide-string/MethodHandle/… — bail out of OSR
                    };
                    ldc_info2.push((pc, val));
                }
            }

            // Resolve ldc2_w constants
            let mut ldc2w_info2: Vec<(usize, i64)> = Vec::new();
            if !scan.ldc2w_ops.is_empty() {
                let cm_lock = shared.classes.class_manager.read();
                let class = cm_lock.get_class(class_id)?;
                for &(pc, cp_idx) in &scan.ldc2w_ops {
                    let val = match class.constant_pool.get(cp_idx)? {
                        ConstantPoolEntry::Long(v) => *v,
                        ConstantPoolEntry::Double(v) => {
                            ldc_fp_pcs2.insert(pc);
                            v.to_bits() as i64 // Cast: JIT ABI -- float bits to i64
                        }
                        _ => return None,
                    };
                    ldc2w_info2.push((pc, val));
                }
            }

            // Resolve new/anewarray info for stackless path (mirrors first JIT site)
            let mut new_info2: Vec<(usize, u32, usize, bool, bool)> = Vec::new();
            let mut anewarray_info2: Vec<(usize, u32)> = Vec::new();
            // Cold-`new` fix — sites this path cannot resolve at compile time.
            // Previously they were baked as the nonsense sentinel entry
            // `(pc, class_id 0, 0 fields, true, true)`: a compiled `new` that
            // allocated against class id 0 rather than the class the bytecode
            // names. They now compile to the CP-indexed helper, which does the
            // real loader-faithful resolution + access check + `<clinit>` at the
            // actual program point, exactly like `Instruction::New`.
            let mut new_deferred2: Vec<(usize, u32, u16)> = Vec::new();
            let mut anewarray_deferred2: Vec<(usize, u32, u16)> = Vec::new();
            let is_real_class2 = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| !c.origin.is_compatibility_stub())
                .unwrap_or(false);
            if is_real_class2 && (!scan.new_ops.is_empty() || !scan.anewarray_ops.is_empty()) {
                let new_class_names: Vec<(usize, u16, Option<String>)> = {
                    let cm_lock = shared.classes.class_manager.read();
                    if let Some(class) = cm_lock.get_class(class_id) {
                        scan.new_ops
                            .iter()
                            .map(|&(pc_new, cp_idx)| {
                                (
                                    pc_new,
                                    cp_idx,
                                    class
                                        .constant_pool
                                        .get_class_name(cp_idx)
                                        .map(|s| s.to_string()),
                                )
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                };
                let arr_class_names: Vec<(usize, u16, Option<String>)> = {
                    let cm_lock = shared.classes.class_manager.read();
                    if let Some(class) = cm_lock.get_class(class_id) {
                        scan.anewarray_ops
                            .iter()
                            .map(|&(pc_arr, cp_idx)| {
                                (
                                    pc_arr,
                                    cp_idx,
                                    class
                                        .constant_pool
                                        .get_class_name(cp_idx)
                                        .map(|s| s.to_string()),
                                )
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                };
                for (pc_new, cp_idx_new, name_opt) in new_class_names {
                    if let Some(name) = name_opt {
                        let load_result = resolve_cp_class_for_owner(shared, class_id, &name);
                        if let Some(target_id) = load_result {
                            // JVMS 5.4.4 / 6.5 `new` access check -- mirrors
                            // the `new_info` site above (same rationale: an
                            // inaccessible `new` site must not be baked into
                            // an inlined JIT fast path; fall back to the
                            // sentinel entry so the interpreter's real check
                            // (`Instruction::New`) is what actually fires).
                            let accessible = {
                                let cm_lock = shared.classes.class_manager.read();
                                match (cm_lock.get_class(class_id), cm_lock.get_class(target_id)) {
                                    (Some(accessor), Some(target)) => {
                                        crate::classloading::access_control::check_class_access(
                                            accessor, target,
                                        )
                                        .is_ok()
                                    }
                                    _ => true,
                                }
                            };
                            if accessible {
                                // `(true, true)` unless `CRATONVM_JIT_REAL_NEW_SITE_FLAGS`
                                // is set — see `jit_new_site_flags` for what
                                // the literal costs (nothing measurable, as it
                                // turns out) and why the real flags are behind
                                // a lever rather than on.
                                let (num_fields, has_prim_init, has_finalizer) = {
                                    let cm = shared.classes.class_manager.read();
                                    if crate::runtime::env_cache::jit_real_new_site_flags() {
                                        jit_new_site_flags(&cm, target_id)
                                    } else {
                                        (
                                            cm.get_class(target_id)
                                                .map(|c| c.num_total_fields)
                                                .unwrap_or(0),
                                            true,
                                            true,
                                        )
                                    }
                                };
                                new_info2.push((
                                    pc_new,
                                    target_id.as_u32(),
                                    num_fields,
                                    has_prim_init,
                                    has_finalizer,
                                ));
                            } else {
                                // Inaccessible at compile time — defer, so the
                                // helper raises the JVMS 5.4.4 IllegalAccessError
                                // at the site instead of the old class-0 bake.
                                new_deferred2.push((pc_new, class_id.as_u32(), cp_idx_new));
                            }
                        } else {
                            new_deferred2.push((pc_new, class_id.as_u32(), cp_idx_new));
                        }
                    }
                }
                for (pc_arr, cp_idx_arr, name_opt) in arr_class_names {
                    if let Some(name) = name_opt {
                        if let Some(target_id) = resolve_cp_class_for_owner(shared, class_id, &name) {
                            anewarray_info2.push((pc_arr, target_id.as_u32()));
                        } else {
                            anewarray_deferred2.push((pc_arr, class_id.as_u32(), cp_idx_arr));
                        }
                    }
                }
            }

            // Prologue argument-slot count. `count_param_slots` returns only
            // the declared descriptor parameters; an instance method also
            // receives the implicit `this` as JVM local 0, so the prologue
            // must load `1 + declared` argument registers. Omitting `this`
            // makes the prologue zero-init local 0 — the OSR-compiled method
            // would then run with a null receiver.
            let osr_method_is_static = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|class| {
                    class
                        .methods
                        .iter()
                        .find(|m| {
                            &*m.name == method_name_check
                                && &*m.descriptor == method_descriptor.as_str()
                        })
                        .map(|m| m.is_static())
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let param_slots = crate::jit::count_param_slots(&method_descriptor)
                + if osr_method_is_static { 0 } else { 1 };
            let (param_jvm_slots, param_slot_span) =
                crate::jit::compute_param_jvm_slots(&method_descriptor, osr_method_is_static);
            let helpers = crate::jit::helpers::build_helpers_for(shared);
            // HIB-CV-20 — seed the local-oop dataflow with this method's reference
            // PARAMETERS, exactly as the hot-path `jit::try_compile` does. The
            // legacy `x64::compile` wrapper hardcodes `param_oop_mask = 0`, so an
            // oop parameter (e.g. the `byte[]` of `Arrays.fill([BIIB)V`, JVM local
            // 0) that lives in a callee-saved register across an early safepoint
            // (its pre-loop `rangeCheck(III)V` call) was never marked an oop:
            // `emit_post_safepoint_reload` skipped it, so a moving young GC during
            // that call rewrote the canonical frame slot but left the register
            // stale → the OSR loop then wrote through the pre-GC address → heap
            // corruption / hang (Hibernate XSD-parse, MappingXsdSupport bootstrap).
            // Seed it via `compile_with_param_slots` (legacy `&[]`/`0` slot layout,
            // unchanged) so the reload covers oop params too. Gate on the precise-
            // maps flag so the gate-off path stays byte-identical (mask = 0).
            // Also seed under `deopt_real_enabled()` — see the matching
            // comment at the hot-path `jit::try_compile` seed site
            // (jit/src/lib.rs) for why an unseeded mask makes every deopt at
            // this compile silently double-execute side effects via the
            // whole-method re-run fallback.
            let param_oop_mask = if crate::jit::x64::precise_jit_maps_enabled()
                || crate::jit::x64::moving_young_enabled()
                || cratonvm_jit::deopt_real_enabled()
            {
                crate::jit::compute_param_oop_mask(&method_descriptor, osr_method_is_static)
            } else {
                0
            };
            // Pure-kernel GPR local homes, OSR tier (perf/halfgap-20260717):
            // request them like `jit::try_compile` does for method-entry
            // compiles. The backend engages ONLY when its own purity
            // conditions hold (no invokes/direct-calls/fields/allocs/
            // typechecks/spec-BCE — checked against the exact vectors passed
            // below), keeps reference locals frame-homed, and — unlike the
            // method-entry tier — publishes real OSR entries: the trampoline
            // seeds every local into its `osr_local_assignments` register,
            // which for a kernel body IS its home. Once-invoked kernels
            // (benchArithmetic, matmul) live entirely in this artifact and
            // previously ran memory-homed. Opt out:
            // `CRATONVM_JIT_KERNEL_REG_OSR=0`.
            crate::jit::x64::set_kernel_reg_homes_osr_request(true);
            // ── The RBC.6b lift's three staged requests ──────────────────
            //
            // `jit::try_compile` has always staged these for a method-entry
            // compile; this door never did, which is the whole of why an OSR
            // artifact "NEVER carries handler ranges" and why RBC.6b refused
            // every method with an exception table. Staged here, at the same
            // point `try_compile` stages them — after every early return above,
            // so a refused attempt cannot leak a request into the next method
            // compiled on this worker thread, and all three are consumed
            // (`take`n) at backend entry.
            //
            // Set unconditionally, including the empty-table case, so the
            // request state this compile runs under is decided HERE rather than
            // inherited from whatever ran before it.
            //
            //   * precise frames — every invoke inside a protected range
            //     publishes a reason-9 (`PendingException`) deopt frame keyed
            //     on the THROWING bci. That bci is the whole point: the live
            //     interpreter frame is parked at the stale pre-OSR back-edge
            //     pc, so without it the handler `[start_pc, end_pc)` test runs
            //     against a pc that has nothing to do with where the throw
            //     happened. `admit_osr_exception_frame` consumes it.
            //
            //     The method-entry path asks for these only when a handler
            //     reads a non-parameter local (`local_handler_reads_unsafe_
            //     local`), because there it is a cost paid to keep a compile
            //     that would otherwise be refused. Here it is asked for EVERY
            //     method with a table: the alternative is not a coarser frame,
            //     it is no bci at all.
            //
            //   * protected ranges — suppresses the sibling tail-call for a
            //     call inside a `try`. A tail-call tears this frame down and
            //     `JMP`s into the callee, so a callee exception unwinds past a
            //     handler that was supposed to catch it — and for an OSR frame
            //     that frame is the live interpreter one.
            //
            //   * pending exception ranges — handler entry edges are invisible
            //     to the backend's bytecode branch decoding, so
            //     `find_bypassable_loop_headers` needs them or a handler
            //     entered from outside a loop lands in the body without running
            //     its pre-header.
            crate::jit::x64::set_precise_exception_frame_request(!osr_exception_table.is_empty());
            crate::jit::x64::set_protected_ranges_request(
                osr_exception_table
                    .iter()
                    .map(|e| (e.start_pc as u32, e.end_pc as u32))
                    .collect(),
            );
            crate::jit::x64::set_pending_exception_ranges(
                osr_exception_table
                    .iter()
                    .map(|e| {
                        (
                            e.start_pc as usize,
                            e.end_pc as usize,
                            e.handler_pc as usize,
                        )
                    })
                    .collect(),
            );
            // ── Compiled local exception handlers, OSR tier ─────────────────
            //
            // The same table a fourth time, with catch TYPES, so this artifact
            // can run its own `catch` blocks instead of leaving compiled code
            // for every one of them.
            //
            // This door matters MORE than the method-entry one, not less: a
            // `@Test` body is invoked once, so OSR is its only route out of the
            // interpreter, and `HttpHeaderValidationUtilTest`'s two exhaustive
            // loops — the class this feature was written for — are exactly that
            // shape. Staging it only in `jit::try_compile` would have left the
            // feature structurally inert on the population it exists for.
            //
            // Staged only when every catch type resolves: a partial table would
            // make a site answer "propagate" where the real table has a match —
            // a slower answer arrived at by a lie.
            if cratonvm_jit::local_handlers_enabled() && !osr_exception_table.is_empty() {
                let cm_lock = shared.classes.class_manager.read();
                let table = cm_lock.get_class(class_id).and_then(|class| {
                    let mut rows: Vec<(usize, usize, usize, &'static str)> =
                        Vec::with_capacity(osr_exception_table.len());
                    for e in osr_exception_table.iter() {
                        let name: &'static str = if e.catch_type == 0 {
                            ""
                        } else {
                            match class.constant_pool.get_class_name(e.catch_type) {
                                Some(n) => cratonvm_jit::intern_catch_type_name(n),
                                None => return None,
                            }
                        };
                        rows.push((
                            // Widening: classfile pcs are u16.
                            e.start_pc as usize,
                            e.end_pc as usize,
                            e.handler_pc as usize,
                            name,
                        ));
                    }
                    Some(rows)
                });
                drop(cm_lock);
                if let Some(table) = table {
                    crate::jit::x64::set_pending_local_handler_table(table, class_id.as_u32());
                }
            }
            // This artifact's install epoch was stamped by the `compile_gate`
            // admission at the top of this closure — before the class loading
            // and constant-pool resolution above, not here. A witness opened at
            // this line covered only the backend call, so a redefinition that
            // landed while the resolvers ran produced a body the install
            // barrier could not tell from a current one.
            osr_stage("backend");
            let mut cm = crate::jit::x64::compile_with_param_slots(
                &admission,
                &code,
                code_len,
                param_slots,
                max_locals, // Widening: u16 to usize (OSR target's max_locals, frame-free)
                // A BRIDGED indy site calls a helper, and every helper call in
                // this backend loads the hidden `SharedVm` pointer out of the
                // frame slot `heap_local_offset` names — a slot that only
                // EXISTS when `needs_heap` is set. `scan.needs_heap` does not
                // know about it: an `invokedynamic` used to lower to a trap
                // that calls nothing. Same hazard, same one-line remedy, as the
                // spliced-call site records at `needs_heap = has_field_ops ||
                // …` below; an OSR body has usually asked for the slot for some
                // other reason, which is precisely why this was invisible for
                // as long as the concat bridge was OSR-only.
                scan.needs_heap
                    || indy_info
                        .iter()
                        .any(|&(_, _, _, _, bridge_site)| bridge_site != 0),
                mna_info,
                field_info,
                typecheck_info,
                static_field_info,
                new_info2,
                new_deferred2,
                anewarray_info2,
                anewarray_deferred2,
                invoke_info,
                direct_calls2,
                mic_slots2,
                pic_slots2,
                ldc_info2,
                ldc_string_info2, // wired (perf/halfgap-20260717) — see the
                // resolve block above; bytes owned by owned_jit_strings2 →
                // cm._jit_strings, same retention as the invoke-info strs.
                ldc_class_info2,
                ldc2w_info2,
                ldc_fp_pcs2,
                std::collections::HashMap::new(), // branch_hints
                std::collections::HashMap::new(), // loop_unroll_hints
                &helpers,
                scan.non_escaping_new.clone(), // escape analysis results
                std::collections::HashMap::new(), // inline_sites
                std::collections::HashMap::new(), // inline_guard_variants (PGO-02, no guarded plan from this scan-based fast path)
                // string_layout — the SAME value the matcher above used, so a
                // registered String sentinel is never one this codegen cannot
                // emit. Was `None` ("String intrinsics land in a later wave"),
                // which made every String intrinsic inert in OSR bodies.
                osr_string_layout,
                &param_jvm_slots,
                param_slot_span,
                param_oop_mask,
                compact_field_info,
                // method_key — needed so OSR-artifact deopt snapshots carry
                // this method's identity (the resume sinks verify a stashed
                // frame's `method_key` before resuming it; an empty key would
                // force every OSR-frame deopt onto the imprecise safe-reject
                // path). Also enables the per-bci de-spec consult below.
                &format!("{class_name}.{method_name}:{method_descriptor}"),
                // This VM's de-spec registry — per VM, never another VM's.
                Some(&shared.jit.despec_registry),
                indy_info,
                Some(elidable_init_pcs),
            );
            let Some(mut cm) = cm else {
                // RBC.2 — a backend bail here is just as permanent as one in
                // `jit::try_compile`; record it so neither this OSR path nor
                // the invocation-counter path re-runs the pipeline.
                //
                // Record the REASON as well, which this door never did.
                // `jit::try_compile` records one, so `CRATONVM_DBG=jit-method-stats`
                // can say WHY a permanently uncompilable method is stuck; an OSR
                // bail reached the same table as `reason=unrecorded`. That is the
                // bail that matters most — this door compiles a `@Test` method's
                // hot loop, and a method denied here runs its whole life in the
                // interpreter with no other diagnostic. See
                // osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md,
                // which took a six-arm shape bisect to find for exactly this reason.
                crate::jit::mark_jit_bail_listed_with_site(
                    class_id,
                    &class_name,
                    &method_name,
                    &method_descriptor,
                );
                return None;
            };
            cm.compiled_via_osr = true;
            cm.osr_compiled_entry_pc = Some(entry_pc);
            // Hand the baked callee entries to `put_osr` ->
            // `prepare_for_publication`, which upgrades each to a strong `Arc`
            // in `_direct_callee_roots`. Without this the OSR body's direct
            // CALLs are unrooted; see the declaration above.
            osr_direct_callee_entries.sort_unstable();
            osr_direct_callee_entries.dedup();
            cm._direct_callee_entries = osr_direct_callee_entries;
            cm._jit_strings = owned_jit_strings2;
            cm._jit_invoke_infos = owned_jit_invoke_infos2;
            cm._jit_mic_slots.extend(owned_mic_slots2);
            cm._jit_pic_slots.extend(owned_pic_slots2);
            // `CRATONVM_DBG_JIT_CODE=<substring>` dumped single-pass and IR
            // bodies but never an OSR one, so the artifact that actually runs a
            // `@Test` method's hot loop was the one body no diff could see.
            // That is precisely the artifact the compile-order question turns
            // on — see
            // docs/known-issues/netty/httpresponsestatustest-exhaustive-loop-timeout-20260816.md,
            // where the callee's body was byte-comparable between the fast and
            // slow arms and the CALLER's was not observable at all. Same
            // format as the other two dumps, tagged `backend=osr`.
            if let Ok(want) = cratonvm_types::flags::runtime_var("CRATONVM_DBG_JIT_CODE") {
                let full = format!("{class_name}.{method_name}{method_descriptor}");
                if full.contains(&want) {
                    let slice = cm._buffer_slice_for_debug();
                    let mut hex = String::new();
                    for b in slice {
                        hex.push_str(&format!("{:02x}", b));
                    }
                    eprintln!(
                        "[JIT_CODE] backend=osr {} entry={:p} entry_pc={} len={}
{}",
                        full,
                        cm.entry_ptr(),
                        entry_pc,
                        slice.len(),
                        hex
                    );
                }
            }
            stamp_compilation_epoch(
                shared,
                &class_name_arc,
                &method_name_arc,
                &descriptor_arc,
                &mut cm,
            );
            let mut jit_cache = shared.jit.jit_cache.write();
            jit_cache.put_osr(
                class_name_arc.clone(),
                method_name_arc.clone(),
                descriptor_arc.clone(),
                class_id,
                cm,
            );
            jit_cache.get_osr(&class_name_arc, &method_name_arc, &descriptor_arc, class_id)
        })()
    };

    let compiled = match compiled {
        Some(c) => c,
        None => return None,
    };
    osr_stage("published");

    // The compile succeeded but the body may still refuse to enter at the PC it
    // was compiled for: no published native offset (the codegen writes -1 for a
    // pc strictly inside a LICM-hoisted loop body, whose preheader an OSR entry
    // would skip), or — only under `CRATONVM_JIT_OSR_DEAD_LOCALS=0` — a
    // non-zero `osr_dead_mask[entry_pc]`. Memo that so the next trip over this
    // back-edge does not re-run the whole pipeline to the same conclusion; the
    // artifact stays cached and other PCs are unaffected.
    if !osr_reused && !compiled.can_osr_enter(entry_pc) {
        cratonvm_jit::metrics::record_osr_event("osr_refused_entry");
        crate::jit::mark_osr_entry_rejected(
            class_id,
            &class_name,
            &method_name,
            &method_descriptor,
            entry_pc,
        );
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] OSR-reject {}.{}{} entry_pc={} (no enterable native offset\
                 , or dead_mask non-zero with CRATONVM_JIT_OSR_DEAD_LOCALS=0; memoed)",
                &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc
            );
        }
        return None;
    }

    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] OSR-{} {}.{}{} entry_pc={} entry={:p} len={}",
            if osr_reused { "reuse" } else { "compile" },
            &*class_name_arc,
            &*method_name_arc,
            &*descriptor_arc,
            entry_pc,
            compiled.entry_ptr(),
            compiled.code_bytes().len()
        );
    }
    if !osr_reused {
        crate::jit::disasm::maybe_dump(
            // The DOOR and the BACKEND are different questions and only the
            // first was ever printed. `used_ir_backend` has recorded the
            // second all along; a reader chasing a miscompiled body needs both
            // to know which emitter to go and read.
            if compiled.used_ir_backend { "osr/ir" } else { "osr/sp" },
            &class_name_arc,
            &method_name_arc,
            &descriptor_arc,
            compiled.entry_ptr(),
            compiled.code_bytes(),
        );
    }
    Some(compiled)
}

/// Where an exception raised inside an OSR'd body must go.
///
/// See [`route_osr_exception_out_of_artifact`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OsrExceptionExit {
    /// The live interpreter frame now sits at one of this method's handlers,
    /// with the throwable on its operand stack. Keep interpreting it.
    EnteredHandler,
    /// The exception escapes this method. Hand it to `throw_out`.
    Propagate,
}

/// Route an exception raised inside an OSR'd body out of the artifact —
/// through this method's own exception table when it can catch, and out of the
/// frame when it cannot.
///
/// **This is the consumer half of the RBC.6b lift.** Before 2026-08-17 the OSR
/// door refused every method with an exception table, so every exception
/// reaching an OSR bail was by construction one this frame could not catch, and
/// the four drains in `try_osr` could simply propagate. Now that such methods
/// compile, the frame CAN catch — and the one thing it must not do is what the
/// drains used to do for a `NullPointerException`: search the table at
/// `entry_pc`. `entry_pc` is the back-edge the OSR'd body was ENTERED at, which
/// has nothing to do with where the throw happened, and the live frame's locals
/// are the stale pre-OSR ones the compiled code never advanced. Entering a
/// handler on those is a silent wrong answer.
///
/// The precise answer is the reason-9 (`DeoptReason::PendingException`) frame
/// the compiled body publishes at every throwing site inside a protected range:
/// it carries the THROWING bci and the live locals. `compile_osr_artifact`
/// admits a method with an exception table only when every such site publishes
/// one (`first_unsupported_precise_frame_site`), which is what makes the two
/// deductions below sound:
///
///  * a frame IS stashed ⇒ its bci is the exact throw site; range-test the
///    table against it;
///  * NO frame is stashed ⇒ the throw site lies outside every protected range
///    of this method, i.e. this method saying it cannot catch. Propagate. This
///    is the same reasoning `route_jit_signal_exception` encodes as
///    `JitThrowPc::OutsideAllRanges` on the method-entry path.
///
/// A stashed frame naming a *callee* is re-stashed untouched: its owner's own
/// drain routes it, and dropping it there made a compiled callee's handler read
/// its non-parameter locals as null once already (see
/// `drop_own_exceptional_frame`).
pub(super) fn route_osr_exception_out_of_artifact(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    compiled: &crate::jit::CompiledMethod,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    exc: ObjectRef,
) -> OsrExceptionExit {
    let trace = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT");
    let precise = match cratonvm_jit::deopt::take_exceptional_frame() {
        Some(rframe)
            if deopt_frame_matches_method(&rframe, class_name, method_name, method_descriptor) =>
        {
            Some(rframe)
        }
        Some(foreign) => {
            cratonvm_jit::deopt::restash_exceptional_frame(foreign);
            None
        }
        None => None,
    };

    if thread.frames[frame_idx].exception_table().is_empty() {
        // The pre-lift population, and still the overwhelming majority: no
        // handler can exist here, so there is nothing to route.
        return OsrExceptionExit::Propagate;
    }

    let Some(rframe) = precise else {
        if trace {
            eprintln!(
                "[cratonvm-deopt] OSR exception with no precise frame in {class_name}.\
                 {method_name}{method_descriptor} — throw site is outside every protected \
                 range; propagating"
            );
        }
        return OsrExceptionExit::Propagate;
    };
    let throw_pc = rframe.bci as usize;
    let Some((handler_pc, exc_ref)) =
        find_exception_handler_any_pc(shared, &thread.frames[frame_idx], throw_pc, exc)
    else {
        if trace {
            eprintln!(
                "[cratonvm-deopt] OSR exception at bci={throw_pc} in {class_name}.\
                 {method_name}{method_descriptor} — no handler covers it; propagating"
            );
        }
        return OsrExceptionExit::Propagate;
    };

    match super::deopt_resume::transfer_osr_exception_exit_into_live_frame(
        shared, thread, frame_idx, &rframe, compiled, handler_pc, exc_ref,
    ) {
        Ok(()) => {
            // The engagement counter for the whole lift. `osr_entered` says an
            // artifact was entered; only this says an exception raised inside
            // one was routed through the method's own handler — the thing
            // RBC.6b refused to allow at all. Read it beside `osr_entered` and
            // `osr_exited`: a run with `osr_entered` climbing and this at zero
            // is a loop whose `catch` never fires, not a working lift.
            cratonvm_jit::metrics::record_osr_event("osr_exception_handler_entered");
            fire_jvmti_exception_catch(shared.vm_identity, &thread.frames[frame_idx], handler_pc);
            OsrExceptionExit::EnteredHandler
        }
        // Unreachable by admission: `validate_osr_entry`'s `osr_exit_policy`
        // walks EVERY deopt point of the artifact — reason-9 ones included —
        // and refuses the entry outright when one reconstructs an unresumable
        // frame, so an artifact that was entered cannot publish one here.
        // Propagating rather than entering the handler on locals we could not
        // map is the fail-closed direction: a visibly uncaught exception beats
        // a handler silently running on stale values.
        Err(why) => {
            if trace {
                eprintln!(
                    "[cratonvm-deopt] OSR exception-exit transfer refused ({why}) at bci={} \
                     in {class_name}.{method_name}{method_descriptor} — propagating",
                    rframe.bci
                );
            }
            OsrExceptionExit::Propagate
        }
    }
}

/// Let the OSR door reach the OPTIMIZING tier -- **default ON** since
/// 2026-09-05; `CRATONVM_JIT_OSR_OPTIMIZING=0` is the kill switch.
///
/// This door has always reached `x64::compile_with_param_slots` directly, so a
/// method whose only route to compiled code is a back edge -- one big method
/// that is one big loop -- has never had an optimizing body, whatever the tier
/// settings said. `CRATONVM_DBG=ir-linear-scan` measured what that costs: on
/// CratonBench, `arithmetic`, `hashmap`, `sieve` and `matrix` get no optimizing
/// body at all, while six of seven methods compiled here would pass the tier's
/// structural gate.
fn osr_optimizing_tier_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        // 2026-09-05: DEFAULT ON. `=0` is the kill switch — which is why
        // this reads the VALUE now rather than only asking whether the
        // variable is present.
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_OSR_OPTIMIZING").as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

/// `(class, method+descriptor, entry pc)` triples whose optimizing OSR
/// artifact this door has already built and REFUSED.
///
/// # Why a negative cache is the whole point
///
/// The door's admission is `ir_osr_entry_addr(entry_pc).is_some() &&
/// ir_osr_sentinel_free`, and `ir_osr_sentinel_free` means the body emits no
/// deopt stub AND no call-exception stub — so **any method containing a call
/// fails it**. That is most methods. Without a memo the door pays a FULL
/// optimizing compile on every OSR attempt for every one of them, throws the
/// artifact away, and falls through to the single-pass path it was always
/// going to take.
///
/// Measured on `LambdaJitTierUp.warmChecksum` (`entries=[] sentinel_free=false`,
/// refused every time): the repeated compiles delay the single-pass OSR entry
/// the frame actually gets, and while the loop is still interpreted its in-loop
/// SAM call site has no compiled caller to hang an inline-cache thunk on — so
/// every dispatch is answered by Rust. That is the intermittent
/// `test_inline_cache_takes_over_the_sam_call_site` failure, at ~3% on a
/// contended host and 0 in 60 runs with the door off.
///
/// Keyed by name rather than by `CachedBytecodeMethod` identity because the
/// frame's handle is resolved afresh on some paths, and a memo that missed
/// would be no memo at all.
static OSR_OPTIMIZING_REFUSED: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<(u32, u64, usize)>>,
> = std::sync::OnceLock::new();

fn osr_optimizing_refusal_key(
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    entry_pc: usize,
) -> (u32, u64, usize) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    method_name.hash(&mut h);
    descriptor.hash(&mut h);
    (class_id.as_u32(), h.finish(), entry_pc)
}

/// Has this door already built and refused an optimizing artifact here?
fn osr_optimizing_already_refused(key: (u32, u64, usize)) -> bool {
    // Kill switch, so the memo can be A/B'd inside ONE binary. Comparing an
    // intermittent event across two builds is not a comparison.
    if matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_OSR_OPTIMIZING_MEMO").as_deref(),
        Ok("0") | Ok("false")
    ) {
        return false;
    }
    OSR_OPTIMIZING_REFUSED
        .get_or_init(Default::default)
        .lock()
        .map(|s| s.contains(&key))
        .unwrap_or(false)
}

/// May the optimizing tier splice a callee containing `ldc` / `ldc_w` /
/// `ldc2_w`? **Default ON**; `CRATONVM_JIT_IR_SPLICE_LDC=0` restores the
/// blanket refusal, so one binary can be A/B'd against its own pre-change
/// behaviour. See the `0x12 | 0x13 | 0x14` arm of the splice scanner.
fn ir_splice_ldc_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SPLICE_LDC").as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

/// Optimizing OSR artifacts this door has already built and ACCEPTED, keyed
/// exactly like the refusal memo beside it.
///
/// # Why this exists
///
/// Only refusals were remembered. A success was wrapped in a fresh `Arc`,
/// entered, and dropped when the OSR'd frame left — so the next back edge
/// rebuilt the same artifact from bytecode, and the one after that, for the
/// life of the process. The single-pass door has never worked this way: it
/// publishes its artifact and every later entry reports `OSR-reuse`.
///
/// Measured before this, on a counted-loop probe entered 20 000 times
/// (`CRATONVM_JIT_METRICS=1`): **502 compiles of one method**, 63.3 ms of
/// compile wall on a 262 ms run — 24 % of the process, spent rebuilding a body
/// that was byte-identical every time. Switching the door off with
/// `CRATONVM_JIT_OSR_OPTIMIZING=0` recovered all of it, which is what
/// identified the cache rather than the codegen as the cost.
///
/// # Lifetime
///
/// The map holds an `Arc`, so a cached artifact's `ExecutableBuffer` stays
/// mapped for the life of the process. That is *safer* than the previous
/// behaviour, not less safe: an artifact used to be unmapped as soon as the
/// last OSR frame in it returned, which is precisely the retired-code hazard
/// `ExecutableBuffer::drop` and `defer_jit_owner` exist to police. Retained
/// executable code is this VM's standing policy.
///
/// # Redefinition
///
/// Unlike the refusal memo — where a stale entry costs only a missed
/// optimization — a stale entry HERE would run pre-redefinition code. So the
/// read is gated on `class_was_redefined` for the exact class, and a redefined
/// class is never served from, nor added to, the cache. `any_class_redefined`
/// makes that one relaxed load on every run that never redefines anything.
static OSR_OPTIMIZING_ACCEPTED: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<(u32, u64, usize), Arc<cratonvm_jit::CompiledMethod>>,
    >,
> = std::sync::OnceLock::new();

/// The artifact this door built for `key`, when it may still be entered.
///
/// Re-checks the two properties the build site checked before accepting, rather
/// than trusting that they were checked once: an entry stub for THIS pc, and a
/// body that cannot return the deopt sentinel. They are properties of the
/// artifact and cannot change while it is cached, so this is a cheap assertion
/// of the contract at the point of use, not a second policy.
fn osr_optimizing_cached(
    shared: &SharedVm,
    class_id: ClassId,
    key: (u32, u64, usize),
    entry_pc: usize,
) -> Option<Arc<cratonvm_jit::CompiledMethod>> {
    if osr_optimizing_cache_disabled() {
        return None;
    }
    if crate::runtime::redefine_state::class_was_redefined(shared, class_id) {
        return None;
    }
    let cached = OSR_OPTIMIZING_ACCEPTED
        .get_or_init(Default::default)
        .lock()
        .ok()?
        .get(&key)
        .cloned()?;
    // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE` is far below it).
    if cached.ir_osr_entry_addr(entry_pc as u32).is_some() && cached.ir_osr_sentinel_free {
        Some(cached)
    } else {
        None
    }
}

/// Keep an accepted optimizing OSR artifact for the next entry at this pc.
fn remember_osr_optimizing_artifact(
    key: (u32, u64, usize),
    artifact: &Arc<cratonvm_jit::CompiledMethod>,
) {
    if osr_optimizing_cache_disabled() {
        return;
    }
    if let Ok(mut m) = OSR_OPTIMIZING_ACCEPTED.get_or_init(Default::default).lock() {
        m.insert(key, Arc::clone(artifact));
    }
}

/// `CRATONVM_JIT_OSR_OPTIMIZING_CACHE=0` restores the pre-cache behaviour —
/// rebuild the artifact on every entry — so the two arms can be measured from
/// ONE binary. Comparing an intermittent event across two builds is not a
/// comparison; this is the same argument `CRATONVM_JIT_OSR_OPTIMIZING_MEMO`
/// makes for the refusal memo, and the same spelling.
fn osr_optimizing_cache_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_OSR_OPTIMIZING_CACHE").as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

/// Record a refusal so the next attempt at this pc skips straight to
/// single-pass.
///
/// Deliberately NOT cleared on class redefinition: `redefine_class` clears the
/// JIT cache, so the worst a stale entry costs is that a method which would now
/// be admitted keeps the single-pass OSR body it had before — a missed
/// optimization on a path that is already the fallback, never a wrong answer.
fn note_osr_optimizing_refusal(key: (u32, u64, usize)) {
    if let Ok(mut s) = OSR_OPTIMIZING_REFUSED.get_or_init(Default::default).lock() {
        s.insert(key);
    }
}

pub(super) fn try_osr(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    class_id: ClassId,
    entry_pc: usize,
    // Out-channel: set to the in-flight throwable when the OSR'd body exited
    // exceptionally and this frame cannot catch it. Written on exactly the
    // paths that used to re-stash + safe-reject; see `OsrBackoffOutcome::
    // ThrowJava` for why the safe reject was wrong there.
    throw_out: &mut Option<ObjectRef>,
    // Out-channel: set when the OSR'd body RAN and this frame was advanced by
    // it, even though the return is `None`. Today that is exactly one path —
    // the RBC.6b lift's handler entry, where the frame is left parked at a
    // `catch` block with the throwable on its stack.
    //
    // `None` from this function otherwise means "the OSR attempt was rejected,
    // nothing ran", and the caller charges it against the per-pc rejection
    // budget (`record_osr_rejection`, permanent after `OSR_MAX_ATTEMPTS = 5`).
    // Charging a caught exception against that budget would turn OSR off after
    // the fifth `catch` — on a loop with netty's measured 7.7% throw rate, some
    // sixty-five compiled iterations out of four billion.
    committed_out: &mut bool,
) -> Option<Option<Value>> {
    // Not gated on `class_was_redefined`. That predicate is true forever once
    // a class has been redefined, so it barred OSR from a redefined class for
    // the life of the process rather than for the duration of the change.
    // `redefine_class` clears the entire JIT cache, and OSR compiles from the
    // frame's current bytecode, so there is nothing stale left to protect
    // against here. See the note in `interpreter.rs`'s compile gate.
    let frame = &thread.frames[frame_idx];
    // Six heap allocations — three `to_string()` and three `Arc::from(&str)` —
    // used to run here, on EVERY back-edge that reaches this function, which is
    // once per OSR entry and not once per compile. The frame already holds all
    // three as `Arc<str>`, so `*_arc()` is a refcount bump; the `&str` views the
    // rest of the function wants come straight off those.
    //
    // It matters because entries are not rare: since the RBC.6b lift a caught
    // exception is an OSR exit plus a re-entry, so a `try`/`catch` loop takes
    // one entry per throw — measured `osr_entered=2280923` on
    // `probes/OsrExcRateProbe.java`, i.e. 13.7 million allocations that bought
    // nothing. `perf record` on that arm put 8.6% of the run in
    // `mi_malloc`/`mi_free`.
    //
    // `frame.code` was already an `Arc<[u8]>` clone and stays one.
    let class_name_arc: Arc<str> = frame.class_name_arc();
    let method_name_arc: Arc<str> = frame.method_name_arc();
    let descriptor_arc: Arc<str> = frame.method_descriptor_arc();
    let class_name: &str = &class_name_arc;
    let method_name: &str = &method_name_arc;
    let method_descriptor: &str = &descriptor_arc;
    let code = frame.code.clone();
    let max_locals = frame.max_locals as usize;
    // wire-tiered-manager Step 5: the OSR compile (or cache reuse) now lives in
    // `compile_osr_artifact`, which the background worker can also call off-thread.
    // The live-frame entry/transfer below stays on the mutator.
    // ── The optimizing tier, reached through the shared assembly ──────
    //
    // `compile_optimizing_artifact` is the input assembly WITHOUT the
    // method-entry door's admission policy — the split exists because asking
    // that door instead marks the method bail-listed on refusal, and
    // `compile_gate::admit` honours the bail-list here too, so the question
    // could switch off the single-pass OSR that works today.
    //
    // Used only when what comes back carries an entry stub for THIS bci. The
    // lowerer refuses a bci whose live-in set the interpreter cannot supply
    // (`emit_osr_entry_stubs`), so a stub's presence already means seeding here
    // is sound. And only when the body CANNOT RETURN THE SENTINEL: an entered body
    // that exits through one hands back a reconstructed frame, and resuming it
    // in place is what `OsrEntryPlan::resume_after_exit` proves — a proof an
    // optimizing entry does not have, leaving replay-or-lose at the exit. No
    // deopt points means nothing returns the sentinel and that fork is
    // unreachable.
    //
    // Anything else falls through to the single-pass path below, unchanged.
    let osr_opt_key =
        osr_optimizing_refusal_key(class_id, &method_name, &method_descriptor, entry_pc);
    // The artifact this door built the LAST time it was asked for this pc.
    //
    // Reusing it is the whole point: without this the door is a compiler, not a
    // cache, and the single-pass door beside it — which publishes its artifact
    // and reports `OSR-reuse` on every later entry — was the only one of the
    // two that behaved like a tier.
    let ir_osr: Option<Arc<cratonvm_jit::CompiledMethod>> = if !osr_optimizing_tier_enabled() {
        None
    } else if let Some(cached) = osr_optimizing_cached(shared, class_id, osr_opt_key, entry_pc) {
        cratonvm_jit::metrics::record_osr_event("osr_optimizing_artifact_reused");
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr optimizing REUSE {class_name}.{method_name} pc={entry_pc}"
            );
        }
        Some(cached)
    } else if !osr_optimizing_already_refused(osr_opt_key) {
        // The frame's own handle when it has one, and a resolution when it does
        // not. `main` is entered by the launcher rather than through the invoke
        // cache, so its frame carries no `CachedBytecodeMethod` — and a method
        // whose only route to compiled code is a back edge is very often
        // exactly that shape, so taking the frame's handle alone made this
        // inert on the population it was built for.
        let cached_for_compile = thread.frames[frame_idx]
            .cached_method()
            .cloned()
            .or_else(|| {
                let key = format!("{class_name}.{method_name}:{method_descriptor}");
                super::deopt_resume::resolve_inlined_callee(shared, class_id, &key).ok()
            });
        cached_for_compile
            .or_else(|| {
                if crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] osr optimizing {class_name}.{method_name}: no cached method"
                    );
                }
                None
            })
            .and_then(|c| {
                let cm = compile_optimizing_artifact(shared, &c);
                if crate::runtime::env_cache::dbg_jitc() {
                    match &cm {
                        None => eprintln!(
                            "[cratonvm-jitc] osr optimizing {class_name}.{method_name} pc={entry_pc}: compile declined"
                        ),
                        Some(cm) => eprintln!(
                            "[cratonvm-jitc] osr optimizing {class_name}.{method_name} pc={entry_pc}: stub={} entries={:?} sentinel_free={}",
                            cm.ir_osr_entry_addr(entry_pc as u32).is_some(),
                            cm.ir_osr_entries.iter().map(|(b, _, _)| *b).collect::<Vec<_>>(),
                            cm.ir_osr_sentinel_free,
                        ),
                    }
                }
                let Some(cm) = cm else {
                    // The compile itself declined. Same conclusion as a refused
                    // artifact for this door's purposes, and the same waste if
                    // it is repeated.
                    note_osr_optimizing_refusal(osr_opt_key);
                    return None;
                };
                // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE` is far below it).
                if cm.ir_osr_entry_addr(entry_pc as u32).is_some() && cm.ir_osr_sentinel_free {
                    // Keep it. Every OSR entry at this pc used to rebuild this
                    // artifact from bytecode and drop it again when the frame
                    // left — see `remember_osr_optimizing_artifact` for the
                    // measurement that motivates the cache.
                    let artifact = Arc::new(cm);
                    remember_osr_optimizing_artifact(osr_opt_key, &artifact);
                    Some(artifact)
                } else {
                    note_osr_optimizing_refusal(osr_opt_key);
                    None
                }
            })
    } else {
        None
    };
    let compiled = match ir_osr {
        Some(c) => {
            cratonvm_jit::metrics::record_osr_event("osr_entered_optimizing");
            // Dump the body this door is about to ENTER, under a label that
            // distinguishes it from the single-pass one.
            //
            // Without this the optimizing artifact is invisible to
            // `CRATONVM_DBG_JIT_DISASM`: the only `osr` dump comes from inside
            // `compile_osr_artifact`, which this arm SKIPS — and the background
            // tier worker calls that function anyway, so a dump appears, is
            // labelled `osr`, and is the single-pass body. Reading it while the
            // door was on showed code that did not change when the residency
            // flags changed, which is exactly the wrong conclusion and cost
            // several rounds to catch. The counter said the door had engaged
            // and the disassembly said it had not; the disassembly was of
            // another artifact.
            crate::jit::disasm::maybe_dump(
                if c.used_ir_backend { "osr-optimizing/ir" } else { "osr-optimizing/sp" },
                &class_name_arc,
                &method_name_arc,
                &descriptor_arc,
                c.entry_ptr(),
                c.code_bytes(),
            );
            c
        }
        None => compile_osr_artifact(
            shared,
            class_id,
            class_name.to_string(),
            method_name.to_string(),
            method_descriptor.to_string(),
            &code,
            max_locals,
            entry_pc,
        )?,
    };

    // Convert interpreter locals to i64 for JIT frame (raw u64 → i64 reinterpret),
    // reading each slot's VTAG byte from the SAME snapshot as its word.
    // `Frame::get_local_tag` exists precisely "for JIT/OSR interop"
    // (`vm/src/runtime/frame.rs`) and was never actually passed to the JIT: until
    // the validated entry landed, `osr_enter` took raw words with no types
    // attached, so nothing compared the interpreter's idea of a slot against the
    // compiled entry's. A `double` seeded into a GPR home, or a `long` seeded
    // where the compiled body reads a reference, is a silent miscompile.
    //
    // Both halves must come from one uninterrupted read of the live frame, BEFORE
    // `set_jit_thread`, so no intervening safepoint can retype a slot between its
    // word and its tag. That is an invariant of the validated entry, not an
    // accident of this call site — see `docs/jit/on-stack-replacement.md` §6.
    let frame = &thread.frames[frame_idx];
    let num_locals = frame.locals_len();
    let mut jit_locals = Vec::with_capacity(num_locals);
    let mut jit_local_tags = Vec::with_capacity(num_locals);
    for i in 0..num_locals {
        jit_locals.push(frame.get_local_raw(i) as i64); // Cast: JIT ABI -- i64 register convention
        jit_local_tags.push(frame.get_local_tag(i));
    }
    // The other invariant of §6: `OsrEntryState::pc` is the frame's CURRENT pc.
    // Every back-edge site captures `entry_pc = frame.pc` after the branch was
    // taken (`interpreter.rs`, 14 sites) and nothing between there and here moves
    // it, so the entry bci and the "nothing ran" fallback bci are the same value —
    // which is what makes a refusal cost no replay. Check it instead of trusting
    // it: a future trigger passing some other pc must be refused, not silently
    // entered at a bci the interpreter is not standing on.
    if entry_pc != frame.pc {
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OSR") {
            eprintln!(
                "[cratonvm-osr] REFUSE {}.{}{} entry_pc={} != frame.pc={} \
                 (OsrEntryState::pc must be the frame's current pc)",
                &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc, frame.pc
            );
        }
        return None;
    }
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OSR") {
        eprintln!(
            "[cratonvm-osr] enter {}.{}{} entry_pc={} num_locals={} locals={:?} tags={:?}",
            &*class_name_arc,
            &*method_name_arc,
            &*descriptor_arc,
            entry_pc,
            num_locals,
            jit_locals,
            jit_local_tags
        );
    }

    // The interpreter's offer. The operand stack at a taken back-edge is empty by
    // construction (the branch already consumed its operands); pass it explicitly
    // so a future trigger at a bci with live operands is REFUSED
    // (`osr-entry-operand-stack`) rather than silently truncated — `osr_trampoline`
    // seeds locals only and has no stack-seeding path.
    let osr_state = cratonvm_jit::OsrEntryState {
        pc: entry_pc,
        locals: &jit_locals,
        local_tags: &jit_local_tags,
        stack: &[],
        stack_tags: &[],
    };

    // ADMISSION. `validate_osr_entry` type-checks every offered slot against the
    // compiled entry's contract and — the part that matters for correctness —
    // walks every deopt point the artifact can exit through, refusing the entry
    // outright (`osr-entry-unresumable-exit`) when one of them reconstructs a
    // frame the in-place transfer could not resume. Discovering that AFTER
    // entering is useless: by then the body has committed iterations, and the only
    // remaining options are to replay them (the recorded
    // `jit-osr-bail-reruns-loop-iterations` defect) or to lose them.
    //
    // A refusal is free of side effects — the check reads metadata and these two
    // slices, allocates one `Vec`, and never enters compiled code — so falling
    // back to `entry_pc`, where the interpreter already is, replays nothing.
    // An optimizing-tier stub is admitted by its OWN construction, not by
    // `validate_osr_entry`: that check proves the offered slots against the
    // single-pass entry contract — `osr_num_locals`, the per-method local
    // assignments, the deopt points `osr_trampoline` would seed through — and
    // an SSA body has none of those. What stands in its place is the refusal
    // the lowerer already made: a bci gets a stub only when every value live on
    // entry is one the snapshot names, and `ir_osr_enter` refuses a locals
    // slice shorter than the stub reads.
    // Cast: a bci fits u32.
    let ir_entry = compiled.ir_osr_entry_addr(entry_pc as u32);
    let plan = match if ir_entry.is_some() {
        // Nothing to validate, and nothing validated: skip straight past.
        Ok(None)
    } else {
        compiled.validate_osr_entry(&osr_state).map(Some)
    } {
        Ok(plan) => plan,
        Err(b) => {
            // Only an ARTIFACT-level verdict may be memoed: it is a pure function
            // of a deterministic compile, so it reproduces for every future
            // back-edge over this pc and re-running the pipeline can only reach it
            // again. A state-dependent refusal (a slot's type, the local count,
            // live operands) must NOT be memoed — the next trip over the back-edge
            // carries different locals and may well be admissible.
            cratonvm_jit::metrics::record_osr_event("osr_refused_entry");
            let permanent = cratonvm_jit::osr_refusal_is_permanent(&b);
            if permanent {
                // `_by`: a refusal that depends on compile-time state expires
                // when that state is flushed, instead of standing for good.
                crate::jit::mark_osr_entry_rejected_by(
                    class_id,
                    &class_name,
                    &method_name,
                    &method_descriptor,
                    entry_pc,
                    &b,
                );
            }
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] OSR-refuse {}.{}{} entry_pc={entry_pc} {b}{}",
                    &*class_name_arc,
                    &*method_name_arc,
                    &*descriptor_arc,
                    if permanent { " (memoed)" } else { "" }
                );
            }
            // Nothing ran: keep interpreting THIS frame at `entry_pc`.
            return None;
        }
    };

    // osr-02 frame comparator: record the ENTRY frame — the state compiled code
    // is about to start from. Here, because the entry has validated and nothing
    // has run yet.
    //
    // Without this record the comparator cannot see a replay at all: compiled
    // iterations produce no back-edge arrivals, so "entered at frame 5, ran to
    // 12, resumed at 5" and "entered at 5 and advanced nothing" are the same
    // sequence of arrival indices, both strictly increasing. A hand-written
    // fixture modelling the historical defect passed without it. Paired with
    // the next exit record this makes the advance a MEASURED quantity —
    // `index(X) - index(E)` over the un-compiled run's own trajectory, not
    // anything the JIT claims.
    if super::osr_frame_trace::enabled() {
        super::osr_frame_trace::record_entry(&thread.frames[frame_idx], entry_pc);
    }

    // Set JIT thread for invoke dispatch callbacks (save/restore for re-entrancy)
    let saved_jit_thread = crate::jit::helpers::set_jit_thread(thread);
    // Capture this `*mut JvmThread` so the OSR trampoline can cache it and the
    // OSR-entered frame's safepoints push/reload precisely. `set_jit_thread`
    // just allocated this thread's shadow stack; the value is a raw address
    // (Copy `i64`, holds no borrow), so the closure below captures it by value
    // and `thread` stays free for later use.
    // Cast: reinterpret pointer/address to typed pointer
    let thread_ptr = thread as *mut JvmThread as i64;
    // NEW-1.5 + T1.1.a: record native stack pointer for GC root scan.
    // Uses the precise-oop-map path when the compiled method has
    // populated maps; falls back to conservative otherwise.
    let _qd0 = cratonvm_gc::gc_quiescence::depth();
    let vm_ptr = shared as *const _ as i64; // Cast: JIT ABI -- pointer to i64 register
    let result_i64 = {
        let _jit_root_guard = crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
            &*compiled,
            Some(thread.frames.len()),
        );
        // Publish this activation for exactly the window it is on the stack --
        // see the "Which interpreter frames are, right now, being run by
        // compiled code" block near the top of this file for why a trace needs
        // it and why `frame.pc` itself must not be moved instead. Deliberately
        // in the same scope as `_jit_root_guard` and taking the same
        // `thread.frames.len()`, so the two records agree about the depth by
        // construction rather than by convention. Declared second, so it is
        // withdrawn FIRST: there is never an instant where the registry claims
        // an activation whose chain entry has already gone.
        //
        // Cost: one push and one truncate per OSR ENTRY. Nothing is added to
        // the back-edge poll (`should_try_osr` returns long before this
        // function is reached) and nothing at all to the compiled loop; an
        // entry already pays a cache lookup, two `Vec`s of locals and tags, and
        // `validate_osr_entry`'s walk over every deopt point, each of which
        // dwarfs this.
        let _osr_continuation =
            OsrContinuationGuard::publish(thread.frames.len(), Arc::as_ptr(&compiled) as usize);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: compiled is a finalized JIT CompiledMethod whose entry point
            // was validated; `plan` is the proof, obtained from
            // `validate_osr_entry` on THIS artifact with THIS state just above, so
            // every seeded slot's JVM type has been checked against the compiled
            // entry's contract and `osr_state.locals` matches `osr_num_locals`.
            //
            // x86-64 only: `osr_enter_planned` is `#[cfg(target_arch =
            // "x86_64")]`. Reaching here off x86-64 would mean an artifact
            // published OSR entry points, and no other backend does, so
            // `should_try_osr` refuses long before here.
            #[cfg(target_arch = "x86_64")]
            {
                match &plan {
                    // The single-pass trampoline, with its proof.
                    // SAFETY: `plan` is `validate_osr_entry`'s result for THIS
                    // artifact at THIS bci, so every seeded slot's JVM type has
                    // been checked against the compiled entry's contract — the
                    // argument spelled out above this `cfg` block.
                    Some(plan) => unsafe {
                        compiled.osr_enter_planned(vm_ptr, &osr_state, plan, thread_ptr)
                    },
                    // The optimizing tier's own stub. It builds this tier's
                    // frame, seeds the locals the snapshot at this bci names,
                    // and jumps into the body — two arguments where the
                    // trampoline takes twenty layout fields, because the
                    // lowerer knows the layout and the trampoline never could.
                    // SAFETY: a DIFFERENT contract from the arm above, and the
                    // reason each arm states its own: this entry takes no plan,
                    // so what must hold is that `entry_pc` is an OSR entry the
                    // artifact published and `jit_locals` matches the snapshot
                    // that bci names.
                    None => unsafe {
                        compiled.ir_osr_enter(entry_pc as u32, vm_ptr, &jit_locals)
                    },
                }
            }
            // `None`, NOT `unreachable!()`. The reasoning above is sound and
            // the panic was still the wrong answer twice over. `None` is this
            // call's existing word for "no entry was taken" -- the caller
            // matches `Ok(None) => return None` and the interpreter carries on
            // in the frame it is already in -- so an argument that turns out to
            // be wrong on some future backend degrades to running the loop
            // interpreted instead of aborting the VM. And the panic-free gate
            // over this module is a TEXT scanner: it does not evaluate `cfg`,
            // so a site compiled out of every x86-64 build still counted
            // against a budget of zero and turned the gate red for everyone.
            #[cfg(not(target_arch = "x86_64"))]
            {
                let _ = (&osr_state, &plan, thread_ptr, vm_ptr, &jit_locals);
                None
            }
        }));
        // DBG: detect a quiescence LEAK across the OSR call (a nested JIT entry
        // that did not pop). Before this site's own guard drops, depth should be
        // back to _qd0 + 1. Anything higher leaked.
        {
            let now = cratonvm_gc::gc_quiescence::depth();
            if now > _qd0 + 1
                && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_CORRUPT_FRAMES")
            {
                eprintln!(
                    "[quiesce-leak] OSR site leaked: depth before={} after={} (expected {})",
                    _qd0,
                    now,
                    _qd0 + 1
                );
            }
        }
        result
    };

    crate::jit::helpers::restore_jit_thread(saved_jit_thread);

    // An entry was taken. Counted here rather than before the call so a panic
    // inside compiled code is not reported as a successful entry, and counted
    // unconditionally because the whole point of `osr_entered` is to be the
    // denominator `osr_exited` is read against.
    cratonvm_jit::metrics::record_osr_event("osr_entered");

    // FIX (OSR uncommon-trap fallthrough, HHH-15895 `InPredicateTest`):
    // `jit_uncommon_trap` (used by, among others, the invokedynamic 0xba arm's
    // unconditional deopt stub — see its `DeoptReason::UnreachedCode` doc
    // comment) signals a deopt via `set_jit_deopt_pending()` alone; unlike the
    // guard-based `x64_deopt_entry` / `ir_deopt_entry` trampolines, it does NOT
    // stash a frame in `cratonvm_jit::deopt::LAST_DEOPT`. Drain the flag now,
    // before the exception-specific drains below, so the `result_i64 ==
    // i64::MIN` check further down can distinguish "a real uncommon-trap
    // deopt with no reconstructed frame" from "a genuine `Long.MIN_VALUE`
    // return". Without this, the former fell through to the return-value
    // conversion, which for a reference-typed method reinterprets the raw
    // `i64::MIN` sentinel bits as a heap pointer — observed as `values` (a
    // live, non-empty `List` local read back null) in Hibernate's
    // `InPredicateTest`, whose `getNames()`-style hot loop contains a live
    // (non-dead-assert) invokedynamic string concatenation.
    let deopt_signaled = crate::jit::helpers::take_jit_deopt_pending();

    // Round-9 vm CRIT fix (audit `round9-vm.md` CRIT-2): the previous OSR
    // exit drain *consumed* the pending-exception, pending-NPE, and
    // pending-AIOOBE flags with `let _ =` / `if let Some(_exc)` on the
    // (false) assumption that falling back to the interpreter "re-executes
    // from the interpreter PC, which will issue the same null-deref / oob
    // access and surface the exception". That assumption is wrong: OSR
    // hands control back at the back-edge PC, NOT at the JIT helper's PC,
    // so the failing access is never re-executed and the exception was
    // silently lost (visible only as a wrong-result or downstream NPE at
    // an unrelated site).
    //
    // The fix: take the flags so we can inspect them, but if any were set,
    // re-stash them onto the same TLS slot via the new
    // `stash_jit_pending_*` helpers. The interpreter dispatch loop drains
    // `take_jit_pending_exception` and the NPE / AIOOBE flags at the next
    // JIT helper return (see lines ~2249 / ~2267 and the post-JIT path at
    // ~12696). Re-stashing preserves the exception across the
    // OSR→interpreter handoff without expanding the OSR signature
    // (`Option<Option<Value>>`, no error channel).
    if let Some(exc) = crate::jit::helpers::take_jit_pending_exception(thread) {
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OSR") {
            let cid = shared.mem.heap.class_id_of(exc);
            let cname = shared
                .classes
                .class_manager
                .read()
                .get_class(cid)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            eprintln!(
                "[cratonvm-osr] BAIL pending_exception {}.{}{} entry_pc={} exc_class={}",
                &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc, cname
            );
        }
        // FIX (jit-osr-bail-on-callee-exception-reruns-loop-iterations,
        // silent wrong answers on default settings): re-stashing and
        // continuing is "OSR rejected, keep interpreting THIS frame from where
        // it was". That is correct only when the bail precedes any committed
        // loop iteration — true for the unconditional-at-header OSR-exit
        // trigger it was written for, false here: the exception surfaces at an
        // arbitrary invoke, possibly thousands of iterations into the loop, and
        // the interpreter frame's induction variable and accumulators were
        // never advanced by the OSR'd code. Every iteration between OSR entry
        // and the throw was therefore executed a SECOND time (measured: 20 000
        // requested, 20 008 executed on the doc's repro; 12 346 requested,
        // 42 730 executed when the exception escapes the OSR'd method
        // entirely).
        //
        // Since the RBC.6b lift (2026-08-17) this frame may also CATCH, which
        // it never could before — `route_osr_exception_out_of_artifact` is
        // where that is decided, from the precise reason-9 throw bci rather
        // than from the stale back-edge pc. It also owns the exceptional-frame
        // stash (taking a frame that names this method, re-stashing one that
        // names a callee), which is what `drop_own_exceptional_frame` used to
        // do here.
        match route_osr_exception_out_of_artifact(
            shared,
            thread,
            frame_idx,
            &compiled,
            &class_name,
            &method_name,
            &method_descriptor,
            exc,
        ) {
            OsrExceptionExit::EnteredHandler => {
                *committed_out = true;
                return None;
            }
            OsrExceptionExit::Propagate => {
                *throw_out = Some(exc);
                return None;
            }
        }
    }
    if crate::jit::helpers::take_jit_pending_npe() {
        // Taken BEFORE the construction below re-captures a stack the compiled
        // frames have already left — see `attach_snapshotted_trap_frames`.
        let npe_snapshot = crate::jit::helpers::take_jit_pending_trap_frames();
        // The JEP 358 action code the null-check stub recorded. Consuming it
        // here is what `take_jit_pending_npe_action`'s own contract says the
        // drain does ("the interpreter drain calls this right after
        // `take_jit_pending_npe` to build the action-only message") and no
        // drain did until 2026-09-06 — `jit_npe_message_gated` had no
        // production caller at all.
        //
        // It was LATENT when it was wired, and measured to be: on 2026-09-06
        // the only setter of an action code was `jit_npe_with_action`, which
        // pairs it with `set_jit_deopt_pending`, so a recorded action always
        // bailed to the interpreter and every NPE that reached this drain
        // carried `NONE`. It was wired anyway because the alternative is a
        // silent drop — which is exactly how this machinery came to be fully
        // built, unit-tested and connected to nothing.
        //
        // It stopped being latent on 2026-09-11. The helpers that detect a null
        // RECEIVER — `jit_getfield`, `jit_invoke_dispatch`,
        // `jit_invoke_virtual_mic` and the direct-bound intrinsics — record a
        // trap-site key or an `INVOKE_RECEIVER` code and DO reach here, because
        // they do not deopt; that is the defect
        // `the-helpful-npe-message-is-lost-in-compiled-code-FIXED-20260911.md`
        // is about. The code is now read by `jit_npe_message` as the
        // corroboration for a bci, and as the fallback message when it declines.
        let npe_action = crate::jit::helpers::take_jit_pending_npe_action();
        // Round-9/10 HIGH fix: route the NPE through the OSR'd method's own
        // exception table rather than losing it. The OSR target IS the method
        // whose code raised the NPE, so this frame's table is the one to
        // search.
        //
        // It used to be searched at `entry_pc` — "our best-known throw site —
        // the back-edge OSR entry, which dominates the failing helper call".
        // That was inert while RBC.6b refused every method with a table, and
        // is not a defensible throw site now that they compile: the back-edge
        // pc has nothing to do with where the null was dereferenced, and the
        // live frame's locals are the stale pre-OSR ones. A protected
        // `putfield` on a null receiver publishes a precise frame at the
        // trapping bci (`emit_precise_null_check_field_store`, which records a
        // `PendingException` deopt point and raises the NPE from the stub), so
        // the same router the pending-exception drain uses has the real answer.
        match crate::runtime::exceptions::throw_runtime_error(
            shared,
            thread,
            RuntimeError::NullPointerException {
                // See `super::jit_npe_message` — the trapping method's own
                // bytecode first, the action-only string as the fallback.
                message: super::jit_npe_message::jit_npe_message(
                    shared,
                    npe_snapshot.as_deref(),
                    npe_action,
                ),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    npe_snapshot,
                );
                match route_osr_exception_out_of_artifact(
                    shared,
                    thread,
                    frame_idx,
                    &compiled,
                    &class_name,
                    &method_name,
                    &method_descriptor,
                    exc,
                ) {
                    OsrExceptionExit::EnteredHandler => {
                        *committed_out = true;
                        return None;
                    }
                    OsrExceptionExit::Propagate => {
                        *throw_out = Some(exc);
                        return None;
                    }
                }
            }
            _ => {
                // Couldn't construct a Java NPE object (e.g. rt.jar not
                // loaded) — re-stash the raw flag as before so the next
                // JIT drain still surfaces it. The frame snapshot is NOT
                // re-stashed: it was taken for this raise, and by the time a
                // later drain surfaced the flag it would describe frames that
                // are long gone. A short trace beats a confidently wrong one.
                //
                // `set_jit_pending_npe_flag_only` is what makes that sentence
                // true of the CODE: `stash_jit_pending_npe` takes a fresh
                // snapshot of its own, so the frames were not dropped here at
                // all — they were silently replaced by a shallower set.
                crate::jit::helpers::set_jit_pending_npe_flag_only();
            }
        }
        return None;
    }
    if let Some((index, length)) = crate::jit::helpers::take_jit_pending_aioobe() {
        // Round-11 fix: mirror the NPE block above for AIOOBE on the OSR bail
        // path — routed through the same `route_osr_exception_out_of_artifact`,
        // for the same reason that block no longer searches at `entry_pc`.
        //
        // Every array access is one of the sites
        // `first_unsupported_precise_frame_site` refuses inside a protected
        // range, so the OSR door never admits a method where a handler could
        // cover this throw; the router's "no precise frame ⇒ outside every
        // protected range ⇒ propagate" deduction is the whole answer here.
        let msg = cratonvm_types::error::out_of_bounds_message::check_index(index, length);
        match crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some(&msg),
        ) {
            Ok(exc_obj) => {
                match route_osr_exception_out_of_artifact(
                    shared,
                    thread,
                    frame_idx,
                    &compiled,
                    &class_name,
                    &method_name,
                    &method_descriptor,
                    exc_obj,
                ) {
                    OsrExceptionExit::EnteredHandler => {
                        *committed_out = true;
                        return None;
                    }
                    OsrExceptionExit::Propagate => {
                        *throw_out = Some(exc_obj);
                        return None;
                    }
                }
            }
            Err(_) => {
                // Couldn't construct the Java object (e.g. rt.jar not loaded) —
                // re-stash the raw flag as before so the next JIT drain still
                // surfaces it.
                crate::jit::helpers::stash_jit_pending_aioobe(index, length);
            }
        }
        return None;
    }
    // Divide-by-zero direct-throw drain on the OSR bail path (sibling of the
    // NPE/AIOOBE OSR blocks above). Routed through the same
    // `route_osr_exception_out_of_artifact` as the other three, and for the
    // same reason the NPE block above no longer searches at `entry_pc`.
    //
    // An `idiv`/`irem` inside a protected range is one of the sites
    // `first_unsupported_precise_frame_site` refuses, so the OSR door never
    // admits a method that could raise this one where a handler covers it —
    // which is why the router's "no precise frame ⇒ outside every protected
    // range ⇒ propagate" deduction is the whole of the answer here.
    if crate::jit::helpers::take_jit_pending_arithmetic() {
        match crate::runtime::exceptions::throw_runtime_error(
            shared,
            thread,
            RuntimeError::ArithmeticException {
                message: "/ by zero".to_string(),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                match route_osr_exception_out_of_artifact(
                    shared,
                    thread,
                    frame_idx,
                    &compiled,
                    &class_name,
                    &method_name,
                    &method_descriptor,
                    exc,
                ) {
                    OsrExceptionExit::EnteredHandler => {
                        *committed_out = true;
                        return None;
                    }
                    OsrExceptionExit::Propagate => {
                        *throw_out = Some(exc);
                        return None;
                    }
                }
            }
            _ => {
                // Couldn't construct the Java object — re-stash the raw flag.
                crate::jit::helpers::stash_jit_pending_arithmetic();
            }
        }
        return None;
    }
    let result_i64 = match result_i64 {
        Ok(Some(v)) => v,
        Ok(None) => return None,
        Err(_) => return None,
    };

    // deopt-osr Step 8: OSR-exit. A frame-deopt taken inside the OSR'd code (the
    // deopt-osr loop-boundary trigger, or any future guard) stashes a reconstructed
    // frame in LAST_DEOPT and returns the i64::MIN sentinel. OSR is *same-frame*
    // replacement, so unlike the `execute_jit_call` sink we must NOT push a new
    // frame or treat the sentinel as the return value (i64::MIN as i32 == 0 → the
    // corrupt result this guards against). Two handlings, gated:
    //
    //   * TRUE OSR-exit (P4, `CRATONVM_OSR_EXIT_TRANSFER`): transfer the
    //     JIT-advanced loop state into THIS live frame and resume the loop body
    //     there (`return None` ⇒ the interpreter continues the mutated frame). This
    //     is required for correctness once the OSR'd code commits per-iteration side
    //     effects — re-running them in the interpreter (the reject below) would
    //     double-execute them.
    //   * Safe reject (gate off / out-of-scope / unmappable): clear the stash and
    //     `return None` so the interpreter continues executing THIS frame from where
    //     it was — correct only when the bail precedes any committed loop iteration
    //     (the unconditional-at-header trigger). The validated Step-8 default.
    if result_i64 == i64::MIN {
        // The entered frame is leaving compiled code without a value. This is
        // the event the doc leads with: an OSR bail that resumes at the wrong
        // interpreter state re-runs loop iterations, which is a wrong-answer
        // bug that no termination test can see. Counting it does not fix that
        // — it makes "entered and immediately left, every time" visible in a
        // default run, which is the shape of the livelock.
        cratonvm_jit::metrics::record_osr_event("osr_exited");
        if let Some(rframe) = cratonvm_jit::deopt::take_last_deopt() {
            dbg_deopt_sink("osr-exit", &rframe, "");
            // The lane's step 4, and the only place it can be answered: the
            // artifact records a set of loop-boundary exit-map bcis
            // (`osr_exit_points`) and until now nothing compared it with where
            // exits are actually taken. Counted before the identity gate,
            // because the classification is about THIS artifact's own view of
            // the bci the frame names; whether the frame is ours to transfer is
            // the separate question the gate below answers.
            //
            // Ungated, for the same reason `osr_entered` / `osr_exited` are: an
            // exit at a bci this artifact records nothing for otherwise leaves
            // no trace at all.
            let exit_site = compiled.classify_osr_exit_site(rframe.bci);
            cratonvm_jit::metrics::record_osr_event(exit_site.metric());
            if matches!(exit_site, cratonvm_jit::osr_exit::OsrExitSite::Unrecorded)
                && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT")
            {
                eprintln!(
                    "[cratonvm-deopt] OSR-exit at bci={} which {}.{}{} records neither an \
                     exit map nor a deopt point for ({} exit points, {} deopt points)",
                    rframe.bci,
                    &*class_name_arc,
                    &*method_name_arc,
                    &*descriptor_arc,
                    compiled.osr_exit_points.len(),
                    compiled.deopt_points.len(),
                );
            }
            // Identity gate (jit-invokedynamic-groovy-regression): the stash
            // could belong to a NESTED compiled callee of the OSR'd code whose
            // sentinel bubbled up here; transferring THAT frame into this live
            // frame would resume this method's bytecode at the callee's bci
            // with the callee's locals/stack. Verify the frame's baked
            // `method_key` names THIS method before transferring; on a
            // mismatch, despeculate the frame's real owner and safe-reject.
            let identity_ok =
                deopt_frame_matches_method(&rframe, &class_name, &method_name, &method_descriptor);
            if !identity_ok {
                despeculate_stashed_frame_method(shared, &rframe);
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
                    eprintln!(
                        "[cratonvm-deopt] OSR-exit stash identity mismatch (frame={} bci={}) \
                         for {}.{}{} — safe reject",
                        rframe.method_key,
                        rframe.bci,
                        &*class_name_arc,
                        &*method_name_arc,
                        &*descriptor_arc
                    );
                }
                return None;
            }
            // FIX (silent data corruption, HHH-15895 InPredicateTest / AccumRepro3
            // residual): `compiled.can_osr_exit` is already a precise gate — it's
            // false unless this compile genuinely recorded an OSR-exit snapshot
            // (either the experimental `deopt_real_enabled()`-gated loop-header
            // guards, or the now-unconditional invokedynamic uncommon-trap
            // snapshot — see the fix notes in `jit/src/x64.rs`). The separate
            // `osr_exit_transfer_enabled()` opt-in gate was redundant on top of
            // that and, left in place, would silently keep the invokedynamic
            // trap on the corruption-prone "safe reject" path in default builds
            // (`CRATONVM_OSR_EXIT_TRANSFER` unset). Dropped in favor of
            // `can_osr_exit` alone.
            // `plan` is the validated entry (see the admission block above). The
            // transfer spends it on `OsrEntryPlan::resume_after_exit`, which names
            // the EXACT bci the OSR'd body stopped at — a recorded deopt point of
            // this artifact whose `ResumeSemantics` is `REEXECUTE` — instead of
            // trusting `rframe.bci` verbatim. It is the only sanctioned resume
            // point once compiled code has run.
            // `plan` is `None` for an optimizing-tier entry, which is admitted
            // only for an artifact with no deopt points — so this arm is
            // unreachable for one, and guarding rather than unwrapping is what
            // makes that a refusal instead of a panic if the admission above
            // ever widens.
            if compiled.can_osr_exit
                && plan.as_ref().is_some_and(|plan| {
                    transfer_osr_exit_into_live_frame(
                        shared, thread, frame_idx, &rframe, &compiled, plan,
                    )
                    .is_some()
                })
            {
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
                    eprintln!(
                        "[cratonvm-deopt] OSR-exit TRANSFER {}.{}{} entry_pc={} resume_bci={}",
                        &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc, rframe.bci
                    );
                }
                return None;
            }
            // Safe reject: the method is not OSR-exit-capable (`can_osr_exit`
            // false ⇒ this artifact recorded no exit map, so no compiled
            // iteration was committed through one), or the reconstructed frame
            // was out of scope / unmappable.
            //
            // The second case is the one that used to be a correctness bug:
            // "continue interpreting THIS frame from where it was" is correct
            // only when the bail precedes any committed iteration, and a
            // mid-loop transfer failure does not. It is now closed at ADMISSION
            // rather than here — `validate_osr_entry` refuses
            // (`osr-entry-unresumable-exit`, permanent, memoed) any artifact
            // whose deopt points include one that reconstructs an unresumable
            // frame, which is exactly the set this branch could receive:
            // `MaterializationRequired` / `Unsupported` slots
            // (`deopt::frame_state_is_resumable`), held monitors, an inlined
            // caller scope, or non-`REEXECUTE` semantics. An admitted entry is
            // therefore `OsrExitPolicy::ExactTransfer`, under which every exit
            // this body can take transfers cleanly; and `can_osr_exit` implies
            // a non-empty `deopt_points`, so the `PropagateOnly` artifacts never
            // reach the transfer at all.
            //
            // Reaching here after a committed body would mean that invariant
            // broke. Do not add a resume path for it — the fix belongs at
            // admission, where nothing has run yet.
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
                eprintln!(
                    "[cratonvm-deopt] OSR-exit bail rejected (continue interpreting) {}.{}{} entry_pc={}",
                    &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc
                );
            }
            // FIX (2026-07-07, jit-invokedynamic-groovy-regression, FOURTH-pass
            // root cause): same missing-despeculation gap as the other two
            // fixed call sites (see the fix note at the first-call tier-up
            // site in `execute()`). A rejected bail here previously just
            // continued interpreting THIS frame with no blacklist, so an
            // `UnreachedCode` (reason 8) trap reached through the OSR-exit
            // path also kept re-triggering on every subsequent call. Recover
            // the reason from the matching deopt point (falling back to
            // `UnreachedCode`) and drive the same de-speculation call the
            // other two fixed sites do.
            let despec_reason = compiled
                .deopt_points
                .iter()
                .find(|dp| dp.bci == rframe.bci)
                .map(|dp| dp.reason)
                .unwrap_or(cratonvm_jit::deopt::DeoptReason::UnreachedCode);
            let _ = crate::jit::helpers::DeoptimizationController::deoptimize(
                shared,
                &class_name_arc,
                &method_name_arc,
                &descriptor_arc,
                despec_reason,
                rframe.bci,
            );
            return None;
        }
        // No stashed deopt frame. If the uncommon-trap path signaled a deopt
        // (`jit_uncommon_trap`'s `set_jit_deopt_pending`, e.g. `UnreachedCode`
        // for a live invokedynamic — see the fix note above), this is NOT a
        // genuine method result: safe-reject exactly like the stashed-frame
        // case above, so the interpreter resumes THIS frame from where it
        // was instead of reinterpreting the `i64::MIN` sentinel as a value.
        if deopt_signaled {
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
                eprintln!(
                    "[cratonvm-deopt] OSR-exit bail rejected (uncommon trap, no frame) {}.{}{} entry_pc={}",
                    &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc
                );
            }
            return None;
        }
        // Otherwise this is a genuine `Long.MIN_VALUE` method result — fall
        // through to the normal return-value conversion below.
    }

    // Convert i64 result back to Value based on return type
    let ret_type = crate::jit::return_type(&method_descriptor);
    match ret_type {
        b'V' => Some(None),
        b'I' | b'B' | b'C' | b'S' | b'Z' => Some(Some(Value::Int(narrow_int_return(ret_type, result_i64)))),
        b'J' => Some(Some(Value::Long(result_i64))),
        b'F' => Some(Some(Value::Float(f32::from_bits(result_i64 as u32)))), // Cast: JIT ABI -- i64 register convention
        b'D' => Some(Some(Value::Double(f64::from_bits(result_i64 as u64)))), // Cast: JIT ABI -- i64 register convention
        b'L' | b'[' => {
            if result_i64 == 0 {
                Some(Some(Value::Object(None)))
            } else {
                // SAFETY: result_i64 is a non-zero JIT/OSR return value encoding a heap pointer to a valid object header.
                Some(Some(Value::Object(Some(unsafe {
                    ObjectRef::from_raw(result_i64 as usize as *mut u8) // Cast: JIT ABI — i64 register convention
                }))))
            }
        }
        _ => Some(None),
    }
}

/// Re-offer every HELD deferred-`new` retry whose class has since loaded.
///
/// Holding a retry rather than burning it on an attempt that would bail keeps
/// the method's one chance alive, but a kept chance nobody offers again is the
/// same outcome as a spent one. This is what offers it.
///
/// Cost when nothing has loaded is one acquire load: `class_definition_epoch`
/// is bumped by every class definition, so an unchanged epoch means no `new`
/// site anywhere can have become resolvable since the last sweep. That is the
/// whole rate limit — deliberately not a time or count budget, because those
/// silence a trigger whose rate depends on the workload rather than on whether
/// there is anything to do.
pub(super) fn resweep_held_deferred_new_retries(shared: &SharedVm, on_class_definition: bool) {
    if !crate::runtime::env_cache::c2_supersede() {
        return;
    }
    // One relaxed load, and almost always zero.
    if cratonvm_jit::held_deferred_new_count() == 0 {
        return;
    }
    // The epoch gate is for the COMPILE door, which fires constantly and where
    // an unchanged epoch means no `new` site can have become resolvable since
    // the last look. The class-definition caller IS the event, so it never
    // needs the gate — and must not take it, or two callers racing on the swap
    // would let one of them skip the definition that mattered.
    if !on_class_definition {
        let epoch = crate::classloading::class_definition_epoch();
        if LAST_DEFERRED_NEW_SWEEP_EPOCH.swap(epoch, std::sync::atomic::Ordering::AcqRel) == epoch {
            return;
        }
    }
    let held = cratonvm_jit::held_deferred_new_methods();
    if held.is_empty() {
        return;
    }
    for (class_name, method_name, descriptor) in held {
        let granted = cratonvm_jit::take_deferred_new_retry(
            &class_name,
            &method_name,
            &descriptor,
            &|holder, cp_idx| {
                let cm = shared.classes.class_manager.read();
                matches!(
                    resolve_jit_new_site(&cm, ClassId::new(holder), cp_idx),
                    Some(cratonvm_jit::JitNewSite::Resolved { .. })
                )
            },
        );
        if granted {
            DEFERRED_NEW_REOFFERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] deferred-new RE-OFFERED {class_name}.{method_name}{descriptor} — its class has loaded"
                );
            }
            // The held list records names only, so resolve the class the way
            // this file's other by-name doors do. A key without the class's
            // identity would not match the state the invocation hooks built,
            // and the retry would take a second in-flight slot beside it.
            let class_id = shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(&class_name)
                .unwrap_or(ClassId::new(0));
            shared
                .jit
                .tiered_manager
                .request_deferred_new_retry(&crate::jit::tiered::MethodKey::with_class_id(
                    class_id,
                    &*class_name,
                    &*method_name,
                    &*descriptor,
                ));
        }
    }
}

/// The class-definition epoch the sweep above last ran at.
static LAST_DEFERRED_NEW_SWEEP_EPOCH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Retries handed back out by the sweep. A sweep that re-offers nothing is a
/// sweep that is not running, or one running where no class ever loads after.
static DEFERRED_NEW_REOFFERED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many held deferred-`new` retries the sweep has re-offered.
pub fn deferred_new_reoffered() -> u64 {
    DEFERRED_NEW_REOFFERED.load(std::sync::atomic::Ordering::Relaxed)
}

/// CRIT-2 — shared body for the JIT `cp_new_resolver` closures: resolve a
/// `new`/`anewarray` CP index in `holder_cid`'s constant pool to a
/// [`cratonvm_jit::JitNewSite`] — `Resolved { class_id, num_fields,
/// has_prim_init, has_finalizer }` when the target class is already loaded,
/// `Deferred` when it is not (see the COLD-`new` GAP note below).
///
/// The two flags feed the inline-TLAB `new` fast path, which skips the
/// `jit_post_tlab_init` helper call when BOTH are false — i.e. no
/// long/float/double instance field anywhere in the hierarchy (the JIT now
/// explicitly clears the body, so int/byte/char/short/boolean are already the
/// correct all-zero `Value::Int(0)`) and no finalizer to register.
/// `has_nonzero_tag_primitive_init` mirrors the hierarchy walk in
/// `crate::jit::helpers::jit_init_primitive_fields`; `has_finalizer` mirrors
/// the single-class read in `jit_post_tlab_init`. Unresolvable metadata
/// reports `(true, true)` so the helper call stays in place.
///
/// COLD-`new` GAP (2026-07-31): `find_class_by_name_for_class` only sees
/// ALREADY-LOADED classes — it deliberately does not run a user
/// `ClassLoader.loadClass` from inside the compile path. A miss therefore used
/// to return `None`, which bailed the WHOLE compile and, after
/// `MAX_TIER_FAIL_RETRIES`, left the method interpreted forever. That silently
/// disqualified every hot method whose only un-taken branch does
/// `throw new SomeException(...)`: nothing had loaded the exception class yet
/// (json-smart's `JSONParserBase.readMain` — 293,940 interpreted invocations of
/// the workload's hottest method). A miss is now reported as
/// [`cratonvm_jit::JitNewSite::Deferred`] and the site compiles to the
/// CP-indexed helper, which resolves at run time. `None` stays reserved for a
/// site no runtime resolution can rescue: the holder class is gone, or the CP
/// entry at `cp_idx` is not a class reference at all.
pub(super) fn resolve_jit_new_site(
    cm: &crate::classloading::ClassManager,
    holder_cid: ClassId,
    cp_idx: u16,
) -> Option<cratonvm_jit::JitNewSite> {
    use cratonvm_jit::JitNewSite;
    let class = cm.get_class(holder_cid)?;
    let class_name = class.constant_pool.get_class_name(cp_idx)?;
    let Some(target_id) = cm.find_class_by_name_for_class(class_name, holder_cid) else {
        // A `Deferred` site gets no `new_info` row, and the IR builder's 0xbb
        // arm then bails the WHOLE method to single-pass — which also costs it
        // escape analysis, so every allocation in it survives. Name the class
        // that could not be resolved: "the method bailed" is not actionable,
        // "Short2 was not found from VolumeShort2's loader" is.
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_IR_COMPILES") {
            // Separate the two ways this lookup fails: the holder has no loader
            // id at all, versus the class simply not being visible from that
            // loader. `find_class_by_name` is the context-free probe, so a hit
            // there with a miss above means the LOADER SCOPE is the problem and
            // not the class being unloaded.
            let loader = cm.get_loader_id(holder_cid);
            #[allow(deprecated)]
            let anywhere = cm.find_class_by_name(class_name).is_some();
            eprintln!(
                "[ir] new-site DEFERRED: {class_name} holder={} loader={loader:?}                  loaded_anywhere={anywhere} cp_idx={cp_idx}",
                cm.get_class(holder_cid)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("{holder_cid:?}")),
            );
        }
        return Some(JitNewSite::Deferred {
            holder_class_id: holder_cid.as_u32(),
            cp_idx,
        });
    };
    let Some(target) = cm.get_class(target_id) else {
        return Some(JitNewSite::Resolved {
            class_id: target_id.as_u32(),
            num_fields: 0,
            has_prim_init: true,
            has_finalizer: true,
        });
    };
    let _ = target;
    let (num_fields, has_prim_init, has_finalizer) = jit_new_site_flags(cm, target_id);
    Some(JitNewSite::Resolved {
        class_id: target_id.as_u32(),
        num_fields,
        has_prim_init,
        has_finalizer,
    })
}

/// `(num_fields, has_prim_init, has_finalizer)` for an already-resolved `new`
/// target — the three values every compile door has to put in its `new_info`
/// row, computed once here so the doors cannot disagree about them.
///
/// # Why this is not just an extract-method
///
/// The two flags decide whether the codegen may take the pure inline-TLAB
/// path: `bytecode_walk`'s `skip_helper = !has_prim_init && !has_finalizer`,
/// and only that arm emits an allocation with **no call in it**. Everything
/// else routes through `jit_post_tlab_init`, or — when `can_inline` is false
/// outright — through the full `jit_new_object` helper.
///
/// [`resolve_jit_new_site`] has computed the real flags since it was written.
/// The other two doors did not call it: the interpreter's first-call compile
/// path and `compile_osr_artifact` both pushed a literal
///
/// ```text
/// new_info.push((pc_new, target_id.as_u32(), num_fields, true, true));
/// ```
///
/// under a comment promising "a follow-up should extract the real flags from
/// class metadata to enable the skip path". So on those two doors — which
/// includes **every OSR-compiled hot loop** — no `new` site could ever be
/// inline-allocated, whatever the class actually looked like.
///
/// That is what made `CRATONVM_JIT_ENABLE_INLINE_NEW=1` look like a dead
/// lever. The flag forces `can_inline`, but it does not touch `skip_helper`,
/// so it swapped a `jit_new_object` call for an inline bump plus a
/// `jit_post_tlab_init` call and measured flat (5 243 769/s vs 5 277 311/s
/// in 2026-08-12's table, 108.7 vs 108.6 ns/op when re-taken 2026-08-17).
/// The `fastthreadlocal-2e9-iteration-throughput-wall` page read that flat
/// result as "the gating flags are not what this loop is paying for" and
/// filed the in-tree TODO as measured-and-refuted. The A/B was sound; what it
/// could not show is that the arm never reached the path being tested.
///
/// Conservative in exactly the two places the old code was: an unresolvable
/// class or an unknown superclass reports `(.., true, true)`, which keeps the
/// helper call.
pub(super) fn jit_new_site_flags(
    cm: &crate::classloading::ClassManager,
    target_id: ClassId,
) -> (usize, bool, bool) {
    let Some(target) = cm.get_class(target_id) else {
        return (0, true, true);
    };
    let num_fields = target.num_total_fields;
    let has_finalizer = target.has_finalizer;
    // Mirrors the hierarchy walk in `crate::jit::helpers::jit_init_primitive_fields`:
    // only long/float/double need a non-zero `Value` tag, so an int-family
    // field is already correct in a body the codegen has cleared to zero.
    let mut has_prim_init = false;
    let mut cid = Some(target_id);
    while let Some(current) = cid {
        let Some(c) = cm.get_class(current) else {
            // Unknown superclass — conservatively keep the helper call.
            has_prim_init = true;
            break;
        };
        if c.fields.iter().any(|f| {
            !f.is_static() && matches!(f.descriptor.as_bytes().first(), Some(b'J' | b'F' | b'D'))
        }) {
            has_prim_init = true;
            break;
        }
        cid = c.superclass;
    }
    (num_fields, has_prim_init, has_finalizer)
}

/// Would dispatching `class_name.<init>()V` reach a native, rather than the
/// bytecode constructor?
///
/// Split out of [`is_elidable_construction`] so it can be tested directly: the
/// elision decision is a *compile-time prediction* of what a later dispatch will
/// do, and a prediction that drifts from the dispatch is exactly the class of
/// bug the caller's json-smart comment describes.
///
/// # Why this is `resolve_native_dispatch_wave1` and not `find(..).is_some()`
///
/// It used to be the latter, which is the wrong question under strict policy:
/// `JdkOnly` sends a non-`Intrinsic` bridge standing in front of concrete
/// bytecode to the bytecode (§7 step 3), so the native the old check refused
/// over never runs, and the refusal was pure pessimism — the JIT declined to
/// elide a constructor that provably does nothing.
///
/// The two inputs a name triple cannot supply:
///
/// * `compat_native_wins: true` — this site's pre-existing verdict, and a
///   faithful one: today a registered `<init>()V` native wins over the bytecode
///   constructor, which is the whole reason the caller checks at all.
/// * `bytecode_available: true` — established by the caller, not assumed: it
///   only asks after `init.code()` returned `Some`.
///
/// # Why the prediction cannot drift
///
/// The dispatch side of this decision — `admit_forced_native`, reached from
/// `intercept_force_registered_native{,_cached}` — calls the SAME resolver with
/// the SAME two constants (`compat_native_wins: true`, `bytecode_available:
/// true`) for the same triple. One function, one pair of inputs, so compile-time
/// and run-time cannot answer differently. That is the property to preserve if
/// either side is ever changed.
///
/// A `Some(_)` of any shape means "not the trivial bytecode body": `NativeBridge`
/// and `Intrinsic` both run other code, and a strict `Reject` throws, which
/// eliding would silently turn into success. `None` means the bytecode is what
/// executes.
///
/// `Compatible` is bit-for-bit the old behaviour: with `compat_native_wins ==
/// true` the resolver answers `Some` for every registration and `None` for none,
/// which is `find(..).is_some()` spelled through the policy.
fn elidable_ctor_native_would_run(shared: &SharedVm, class_name: &str) -> bool {
    let registered = shared
        .natives
        .native_methods
        .find_with_kind(class_name, "<init>", "()V");
    crate::vm::resolve_native_dispatch_wave1(
        crate::vm::DispatchDoor::ElidableCtor,
        crate::vm::dispatch_policy(shared),
        class_name,
        "<init>",
        "()V",
        registered,
        true,
        true,
    )
    .is_some()
}

/// Whether constructing `class_id` via its no-arg constructor is *elidable* for
/// JIT escape-analysis scalar replacement — i.e. `new C(); dup; invokespecial
/// C.<init>()V` may be replaced by a zero-initialised scalar object with no call.
///
/// SOUND only for the empty default constructor of a direct `java/lang/Object`
/// subclass: `C.<init>()V`'s body is exactly `aload_0; invokespecial
/// java/lang/Object.<init>()V; return` (bytes `2a b7 hi lo b1`). That guarantees
/// the constructor (a) writes NO field (the object stays zero-initialised, so the
/// scalar slots' zero defaults are correct), (b) does NOT escape its receiver,
/// and (c) has NO other side effect (the only call is the empty `Object.<init>`).
///
/// This is deliberately narrower than `classify_init_complexity`'s `Trivial`,
/// which admits arbitrary calls (e.g. `register(this)`) that escape the receiver
/// — unsound to elide. (A future refinement may recurse the super chain to admit
/// non-`Object` supers whose `<init>` is itself elidable.)
pub(super) fn is_elidable_construction(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
) -> bool {
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    let Some(init) = class.find_method("<init>", "()V") else {
        return false;
    };
    let Some(code) = init.code() else {
        return false;
    };
    // A REGISTERED NATIVE SHADOWS THE BYTECODE CONSTRUCTOR. `invokespecial`
    // prefers a registered native over bytecode, so a trivial-looking
    // `<init>()V` body says nothing about what actually runs — and eliding the
    // call skips the native's side effects entirely.
    //
    // `java/util/HashMap.<init>()V` is exactly this shape: an empty bytecode
    // constructor plus `native_map_init`, which allocates the 16-bucket table
    // and initialises size/threshold. With the call elided, a JIT-compiled
    // `new HashMap<>()` left `table` null; the first `put` then materialised
    // the table through `map_resize`, which DOUBLED the assumed default to 32
    // buckets. Every JIT-created HashMap therefore iterated its keys in a
    // different order than an interpreter-created one holding the same keys —
    // found as a json-smart parse/serialize/re-parse round-trip mismatch at the
    // exact iteration `JSONParserBase.readObject` tiered up
    // (jsonsmart-parser-jit-retired-20260727.md). The companion
    // `map_resize` fix makes the fallback capacity correct; this one keeps the
    // native constructor running in the first place.
    //
    // The §3 item-4 residual of the retired wave-2 markers record.
    // The question is NOT "is a native registered" but "would dispatching this
    // `<init>` reach one" — see [`elidable_ctor_native_would_run`], which is
    // where that distinction and its `bytecode_available` premise are argued.
    // The check sits BELOW the body lookup because that premise is `init.code()`
    // having returned `Some`.
    if elidable_ctor_native_would_run(shared, &class.name) {
        return false;
    }
    let bc = &code.code;
    // aload_0 (0x2a); invokespecial (0xb7) hi lo; return (0xb1) — exactly 5 bytes.
    if bc.len() != 5 || bc[0] != 0x2a || bc[1] != 0xb7 || bc[4] != 0xb1 {
        return false;
    }
    // Cast: numeric/representation conversion
    let mref_idx = ((bc[2] as u16) << 8) | bc[3] as u16;
    let cp = &class.constant_pool;
    let nat_idx = match cp.get(mref_idx) {
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }) => {
            if cp.get_class_name(*class_index) != Some("java/lang/Object") {
                return false;
            }
            *name_and_type_index
        }
        _ => return false,
    };
    matches!(cp.get_name_and_type(nat_idx), Some(("<init>", "()V")))
}

/// Return the constant-pool index for a dynamically dispatched invoke.
#[inline]
pub(super) fn jit_native_shadow_dynamic_invoke_index(instruction: &Instruction) -> Option<u16> {
    match instruction {
        // A native-shadowed direct call uses the generic JIT dispatch fallback
        // when no bytecode target is available, preserving its native behavior.
        // Retain the guard for virtual/interface calls: an override can be
        // selected at runtime, which is the ByteBuddy/Object.equals safety case.
        Instruction::Invokevirtual(index) => Some(*index),
        Instruction::Invokeinterface { index, .. } => Some(*index),
        _ => None,
    }
}

/// A final wrapper's primitive accessor is non-overridable and the JIT
/// dispatcher preserves the registered native implementation.  It is therefore
/// safe to compile callers containing these unboxing calls; unlike a general
/// virtual native call, there is no runtime override to bypass.
pub(super) fn jit_native_shadow_is_final_wrapper_unbox(
    target_class: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if !matches!(
        target_class,
        "java/lang/Boolean"
            | "java/lang/Byte"
            | "java/lang/Character"
            | "java/lang/Short"
            | "java/lang/Integer"
            | "java/lang/Long"
            | "java/lang/Float"
            | "java/lang/Double"
    ) {
        return false;
    }
    matches!(
        (method_name, descriptor),
        ("booleanValue", "()Z")
            | ("byteValue", "()B")
            | ("charValue", "()C")
            | ("shortValue", "()S")
            | ("intValue", "()I")
            | ("longValue", "()J")
            | ("floatValue", "()F")
            | ("doubleValue", "()D")
    )
}

/// The `java.lang.Double` bit reinterpretations the JIT now lowers itself.
///
/// Same shape of exemption as [`jit_native_shadow_is_final_wrapper_unbox`] and
/// for a stronger version of the same reason. The seal exists because a
/// compiled direct call bypasses the interpreter's native-vs-bytecode
/// decision; for these two there is nothing to bypass, because the compiled
/// form is not a call at all. `try_resolve_intrinsic`'s FP_BITS region lowers
/// each to a single `MOVQ` that is bit-exact with the native it replaces --
/// including the NaN payload, which is the whole content of the RAW contract.
///
/// Both are `public static native` on a `final` class, so no override can
/// exist and the target is unambiguous.
///
/// Without this the intrinsic could never fire on the workload it was built
/// for: `jit_method_calls_native_shadowed` seals a method out of the JIT for
/// CONTAINING the call, and the intrinsic only resolves once the method is
/// admitted to a compile. Measured on `PSquarePercentileTest`, whose
/// `--dump-native-registry` census reported 361M invocations of exactly these
/// two.
///
/// `doubleToLongBits` is absent, matching the resolver: it canonicalises NaN,
/// so no `MOVQ` implements it.
pub(super) fn jit_native_shadow_is_intrinsified_fp_bits(
    target_class: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    target_class == "java/lang/Double"
        && matches!(
            (method_name, descriptor),
            ("doubleToRawLongBits", "(D)J") | ("longBitsToDouble", "(J)D")
        )
}

/// # A negative result, kept so it is not re-derived
///
/// `AbstractOwnableSynchronizer.setExclusiveOwnerThread` looks like it belongs
/// beside the two exemptions above, and the argument for it is sound as far as
/// it goes. The seal is not a tier-up delay — `jit_method_calls_native_shadowed`
/// memoizes its verdict through `mark_jit_bail_listed`, so a method containing
/// such a call is JIT-denied for the lifetime of the process — and three
/// methods on the JDK's uncontended `ReentrantLock` path contain one:
/// `ReentrantLock$NonfairSync.initialTryLock`, `.tryAcquire` and
/// `ReentrantLock$Sync.tryRelease`. That is every `LinkedBlockingQueue.add`
/// and `.take`. The seal's own reason ("a compiled direct call bypasses the
/// interpreter's native-vs-bytecode decision") does not apply to that triple:
/// the callee is never compiled (`registered_native_will_run` refuses it),
/// never spliced (`resolve_inline_site_from` refuses any site whose selected
/// method's declaring class carries a registered native, unconditionally), and
/// cannot be direct-bound (the call is `invokevirtual`, and
/// `pending_callee_compiles` admits kinds 1 and 3 only).
///
/// It was implemented on 2026-08-27, and it is not here because it **bought
/// nothing measurable**. The exemption engaged — `aqs-owner-exempt=4`, and the
/// skip-seal census went `calls-native-shadowed-method=36` to `32`, so four
/// methods really did stop being sealed — and then:
///
/// | | seal lifted | seal kept |
/// |---|---:|---:|
/// | `AqsAttributionProbe` ReentrantLock lock+unlock | 2349 ns | 2336 ns |
/// | `HwtScaleProbe` n=100 000 drain | 767 / 718 ms | 745 / 647 ms |
///
/// One binary, the switch the only variable, interleaved. Unsealing a method
/// is not the same as compiling it, and nothing here says those four ever
/// reached a tier — that is the question anyone reviving this should answer
/// FIRST, with a compile counter, rather than by re-writing the exemption.
/// A relaxation that widens what the JIT will compile is not free of risk, and
/// this one has no measurement to pay for it.
pub(super) fn jit_invoke_targets_native_shadow(
    shared: &SharedVm,
    caller_class_id: ClassId,
    cp_idx: u16,
) -> bool {
    let (target_class, method_name, descriptor, declaring_class, is_interface_ref) = {
        let cm = shared.classes.class_manager.read();
        let Some(caller) = cm.get_class(caller_class_id) else {
            return true;
        };
        let (class_index, nat_index, is_interface_ref) = match caller.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index, false),
            Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index, true),
            _ => return true,
        };
        let Some(target_class) = caller
            .constant_pool
            .get_class_name(class_index)
            .map(str::to_string)
        else {
            return true;
        };
        let Some((method_name, descriptor)) = caller.constant_pool.get_name_and_type(nat_index)
        else {
            return true;
        };
        let method_name = method_name.to_string();
        let descriptor = descriptor.to_string();
        let declaring_class = if let Some(target_id) =
            cm.find_class_by_name_for_class(&target_class, caller_class_id)
        {
            let store = cm.class_store();
            crate::classloading::find_method_recursive(target_id, &method_name, &descriptor, store)
                .and_then(|(_, declaring_id)| {
                    store
                        .get(declaring_id)
                        .map(|declaring| declaring.name.to_string())
                })
        } else {
            None
        };
        (
            target_class,
            method_name,
            descriptor,
            declaring_class,
            is_interface_ref,
        )
    };

    if jit_native_shadow_is_final_wrapper_unbox(&target_class, &method_name, &descriptor) {
        return false;
    }
    if jit_native_shadow_is_intrinsified_fp_bits(&target_class, &method_name, &descriptor) {
        return false;
    }
    // A compiled direct call bypasses the interpreter's native-vs-bytecode
    // decision. Treat forced real-JDK overrides exactly like registered
    // native shadows, so a caller of Class's Signature bridge cannot enter
    // the incompatible JDK bytecode body.
    // Both arms ask `registered_native_will_run` rather than
    // `find(..).is_some()`: a native the arbitration always yields is not a
    // shadow, and sealing the caller for it costs tier-up while protecting
    // nothing. Every other triple answers exactly as before.
    let direct = force_native_over_real_jdk_bytecode(&target_class, &method_name, &descriptor)
        || registered_native_will_run(shared, &target_class, &method_name, &descriptor);
    let inherited = declaring_class.as_ref().is_some_and(|declaring_class| {
        registered_native_will_run(shared, declaring_class, &method_name, &descriptor)
    });
    // Interface-dispatch blind spot: for `invokeinterface`, `declaring_class`
    // above is resolved by walking UP FROM THE INTERFACE (`find_method_recursive`
    // starting at the CP-referenced interface's own class_id) — it can only ever
    // land on that same interface (its own abstract/default declaration) or a
    // super-INTERFACE. It can never see a concrete implementor's SUPERCLASS
    // chain, because interfaces carry no knowledge of their implementors. So
    // for a receiver that implements this interface but inherits the actual
    // method body from an unrelated ancestor CLASS — exactly
    // `org.codehaus.groovy.reflection.v7.GroovyClassValueJava7 implements
    // GroovyClassValue, extends java.lang.ClassValue` inheriting `get()` from
    // `ClassValue`, which IS natively registered — `direct`/`inherited` above
    // both come back false even though the call is genuinely native-shadowed
    // at every concrete receiver. Fall back to a cheap, class-blind "does ANY
    // registered native have this exact (name, descriptor)" probe — the same
    // idiom `might_have_method_descriptor` already serves as a pre-filter
    // elsewhere (e.g. `execute_invokevirtual_vtable_fast`) — and treat a hit
    // as a possible shadow. This can only ever ADD conservatism (a same-named,
    // same-descriptor native for a genuinely unrelated interface is rare and
    // merely costs a missed tier-up opportunity for that one caller, never a
    // correctness bug).
    // `CRATONVM_JIT=-native-shadow-interface-blind` suppresses this arm, so its
    // cost can be A/B'd on a real workload before anyone decides whether to make
    // it precise. Default-on: it is a CORRECTNESS guard (a compiled direct call
    // bypasses the interpreter's native-vs-bytecode choice), and the shape it
    // covers is real — `GroovyClassValueJava7 implements GroovyClassValue,
    // extends java.lang.ClassValue` inheriting a natively-registered `get()`.
    // The lever exists to measure the arm, not to be shipped off.
    let interface_blind_possible_shadow = is_interface_ref
        && !direct
        && !inherited
        && crate::runtime::env_cache::jit_native_shadow_interface_blind()
        && shared
            .natives
            .native_methods
            .might_have_method_descriptor(&method_name, &descriptor);
    // Which arm fired, counted. The three have very different standing:
    // `direct`/`inherited` are precise facts about THIS call, while
    // `interface_blind` is a class-blind "does ANY registered native have this
    // (name, descriptor)" probe whose own comment concedes it "can only ever ADD
    // conservatism". On a Spring Boot context startup this whole predicate seals
    // 1,279 methods out of the JIT — more than the 1,155 that reach C2 — and
    // until now nothing said which arm was responsible for them.
    //
    // MEASURED 2026-08-12 on netty `AdaptiveByteBufAllocatorTest` (dev
    // `6d1bfd531`), which is the shape this predicate should hurt most: 826 M
    // calls, and its hot allocator methods call `ArrayList.add`, `Math.min` and
    // `AtomicIntegerArray.get`, all shadowed. Arm split
    // `direct=474 interface-blind=97 inherited=60` — the class-blind arm is 15%
    // of the population, not the bulk.
    //
    // And the seal is NOT a throughput lever here. Interleaved on one box:
    // default 594 s / 1117 sealed, `-native-shadow-interface-blind` 493 s /
    // 1056 sealed, `-native-shadow-caller-seal` (the whole seal off) **591 s**
    // / 675 sealed. Compiling 626 more methods moved the wall clock 0.5%. So
    // making this arm precise is a correctness/coverage argument, not a
    // performance one — the cost on call-dense code is the per-entry transfer
    // machinery, not the population this seals. See
    // `docs/known-issues/netty/adaptive-bytebuf-allocator-throughput-20260812.md`.
    if direct {
        cratonvm_jit::note_jit_native_shadow_cause("direct");
    } else if inherited {
        cratonvm_jit::note_jit_native_shadow_cause("inherited");
    } else if interface_blind_possible_shadow {
        cratonvm_jit::note_jit_native_shadow_cause("interface-blind");
    }
    if (direct || inherited || interface_blind_possible_shadow)
        && crate::runtime::env_cache::dbg_jitc()
    {
        eprintln!(
            "[cratonvm-jitc] native-shadow target={}.{}{} direct={} inherited={} interface_blind={}",
            target_class, method_name, descriptor, direct, inherited, interface_blind_possible_shadow
        );
    }
    direct || inherited || interface_blind_possible_shadow
}

pub(super) fn jit_method_calls_native_shadowed(
    shared: &SharedVm,
    caller_class_id: ClassId,
    code: &[u8],
    code_len: usize,
) -> bool {
    let scan_len = code_len.min(code.len());
    let scan_code = &code[..scan_len];
    let mut pc = 0;
    while pc < scan_len {
        let (instruction, next_pc) = match Instruction::decode(scan_code, pc) {
            Ok(decoded) => decoded,
            Err(_) => return true,
        };
        if let Some(cp_idx) = jit_native_shadow_dynamic_invoke_index(&instruction) {
            if jit_invoke_targets_native_shadow(shared, caller_class_id, cp_idx) {
                return true;
            }
        }
        if next_pc <= pc || next_pc > scan_len {
            return true;
        }
        pc = next_pc;
    }
    false
}

/// Detect forced `Class` generic-metadata bridges in a prospective compiled
/// caller. They must continue through interpreter dispatch, which chooses the
/// Signature-attribute native implementation over the real-JDK bytecode.
///
/// The caller must pass its existing class-manager guard. Do not make this
/// helper acquire the lock itself: `try_jit_compile_callee_slow` already holds
/// a read guard while extracting the method body. If a class-loading writer
/// queues between that guard and a nested `read()`, parking_lot's task-fair
/// `RwLock` parks the nested read behind the writer while the outer read keeps
/// the writer parked forever. Hibernate model/ByteBuddy generation and
/// WildFly parallel extension boot both exercise that ordering.
pub(super) fn jit_method_calls_forced_class_generic_metadata(
    cm: &crate::classloading::ClassManager,
    caller_class_id: ClassId,
    code: &[u8],
    code_len: usize,
) -> bool {
    let scan_len = code_len.min(code.len());
    let scan_code = &code[..scan_len];
    let mut pc = 0;
    while pc < scan_len {
        let (instruction, next_pc) = match Instruction::decode(scan_code, pc) {
            Ok(decoded) => decoded,
            Err(_) => return true,
        };
        if let Some(cp_idx) = jit_native_shadow_dynamic_invoke_index(&instruction) {
            let is_forced_generic_metadata = {
                let Some(caller) = cm.get_class(caller_class_id) else {
                    return true;
                };
                let cp = &caller.constant_pool;
                let (class_index, nat_index) = match cp.get(cp_idx) {
                    Some(ConstantPoolEntry::MethodReference {
                        class_index,
                        name_and_type_index,
                    })
                    | Some(ConstantPoolEntry::InterfaceMethodReference {
                        class_index,
                        name_and_type_index,
                    }) => (*class_index, *name_and_type_index),
                    _ => return true,
                };
                matches!(
                    (
                        cp.get_class_name(class_index),
                        cp.get_name_and_type(nat_index)
                    ),
                    (
                        Some("java/lang/Class"),
                        Some(("getTypeParameters", "()[Ljava/lang/reflect/TypeVariable;"))
                            | Some(("getGenericInterfaces", "()[Ljava/lang/reflect/Type;"))
                            | Some(("getGenericSuperclass", "()Ljava/lang/reflect/Type;"))
                    )
                )
            };
            if is_forced_generic_metadata {
                return true;
            }
        }
        if next_pc <= pc || next_pc > scan_len {
            return true;
        }
        pc = next_pc;
    }
    false
}

/// Resolve a constant-pool class NAME for a method that is being COMPILED,
/// through the compiling class's own defining loader.
///
/// # The defect this exists to stop
///
/// The compiler used to answer these with `SharedVm::load_class_concurrent`,
/// which is deliberately loader-BLIND: its own doc says it "can only ever
/// produce a Bootstrap/Extension/Application-loaded class". For a method whose
/// owner was defined by a USER-DEFINED loader that is not a lookup at all --
/// it is a DEFINITION. `resolve_fast_path_class_id` sees the user loader's
/// copy, correctly refuses to hand a built-in-chain caller someone else's
/// namespace, and defines a SECOND copy under `Application`; the compiler then
/// bakes that second `ClassId` into the site.
///
/// Measured on `TestContextAotGeneratorIntegrationTests.processAheadOfTimeWithWebTests`
/// (Spring's `@CompileWithForkedClassLoader`, whose
/// `CompileWithForkedClassLoaderClassLoader` re-defines the whole classpath on
/// purpose): the background compiler, OSR-compiling
/// `com.thoughtworks.qdox.parser.impl.Parser.yyparse` for the FORK loader's
/// copy, resolved the method's `new` sites this way and baked
/// `TypeDef`-under-`Application`. Compiled code then allocated
/// Application-namespace `TypeDef`s inside a fork-namespace parser, and the
/// site's own `checkcast` -- correctly resolved through the compiling class's
/// loader by `intern_typecheck_target` -- refused them:
///
/// ```text
/// [cv-checkcast-fail] typecheck REFUSED: obj_cid=5444 obj_loader=Application
///     obj_cls=com/thoughtworks/qdox/parser/structs/TypeDef
///     site_target=5400 site_target_loader=UserDefined(3)
/// ```
///
/// which Java sees as `ClassCastException: class ...TypeDef cannot be cast to
/// class ...TypeDef` -- the textbook two-copies-one-name message, produced by
/// the compiler rather than by the program.
///
/// # What this does instead
///
/// * Owner defined by a BUILT-IN loader (the overwhelming majority): unchanged
///   -- `load_class_concurrent`, which for such an owner produces exactly the
///   class the interpreter would have resolved.
/// * Owner defined by a USER-DEFINED loader: LOOK UP only, through
///   `get_loaded_class_id_for_requester`, which prefers that loader's own
///   definition and then the built-in parent chain -- and never defines. A
///   miss returns `None`, so the site takes the DEFERRED path it already has
///   for an unresolvable target and the interpreter resolves it correctly at
///   run time. Declining to optimise is always available; defining a second
///   identity is not.
///
/// `CRATONVM_JIT_LOADER_BLIND_CP_RESOLVE=1` restores the old call for a
/// one-binary A/B.
pub(super) fn resolve_cp_class_for_owner(
    shared: &SharedVm,
    owner: ClassId,
    name: &str,
) -> Option<ClassId> {
    if jit_loader_blind_cp_resolve() {
        return shared.load_class_concurrent(name).ok();
    }
    let owner_loader = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(owner).map(|c| c.loader_id)
    };
    match owner_loader {
        Some(loader @ cratonvm_types::ClassLoaderId::UserDefined(_)) => {
            let cm = shared.classes.class_manager.read();
            cm.get_loaded_class_id_for_requester(name, loader)
        }
        _ => shared.load_class_concurrent(name).ok(),
    }
}

/// Kill switch for [`resolve_cp_class_for_owner`].
fn jit_loader_blind_cp_resolve() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_LOADER_BLIND_CP_RESOLVE")
    })
}

/// Shared body for the JIT `cp_elidable_init_resolver` closures: given the
/// holder class `holder_cid` and an `invokespecial` constant-pool index, return
/// `true` iff it targets a no-arg `<init>()V` whose construction is elidable
/// (see [`is_elidable_construction`]) — for both scalar replacement (IR `new`
/// elision) and the per-allocation ctor-dispatch elision.
///
/// Resolves the target via `load_class_concurrent` so APPLICATION-loaded
/// classes are covered, not only bootstrap/JDK ones: `find_class_by_name` does
/// NOT see app classes in the JIT-compile context (the same gap the
/// `execute`/`try_osr` ctor-elision paths hit). It reads the target
/// class name under a brief `class_manager` lock, DROPS it, resolves via
/// `load_class_concurrent` (already-loaded classes return cached — the common
/// case, since a ctor site's class is loaded — so no `<clinit>`/GC), then
/// re-reads `cm` to check elidability. Lock discipline matches the
/// `field_resolver` precedent (resolve-with-load BEFORE taking the inner `cm`
/// read), so no VM read lock is alive across the load. MUST NOT be called with
/// a `class_manager` lock already held.
pub(super) fn resolve_jit_elidable_init_loading(
    shared: &SharedVm,
    holder_cid: ClassId,
    cp_idx: u16,
) -> bool {
    // 1. Extract the target class name + confirm a no-arg `<init>()V` ref
    //    (brief `cm` read, dropped before the load).
    let target_name = {
        let cm = shared.classes.class_manager.read();
        let Some(holder) = cm.get_class(holder_cid) else {
            return false;
        };
        let cp = &holder.constant_pool;
        let (class_index, nat_index) = match cp.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index),
            _ => return false,
        };
        if !matches!(cp.get_name_and_type(nat_index), Some(("<init>", "()V"))) {
            return false;
        }
        match cp.get_class_name(class_index) {
            Some(n) => n.to_string(),
            None => return false,
        }
    };
    // 2. Resolve the target (loading if necessary) with NO `cm` lock held.
    let Some(target_id) = resolve_cp_class_for_owner(shared, holder_cid, &target_name) else {
        return false;
    };
    // 3. Check elidability (brief `cm` read).
    let cm = shared.classes.class_manager.read();
    let elidable = is_elidable_construction(shared, &cm, target_id);
    // DBG (CRATONVM_DBG_CTOR_FIX): when this resolves an elidable ctor whose
    // target `find_class_by_name` could NOT see, it is the app-class gap being
    // closed (the old resolver would have returned false here).
    if elidable && crate::runtime::env_cache::ctor_fix_dbg() {
        let via_find = cm
            .find_class_by_name_for_class(&target_name, holder_cid)
            .is_some();
        eprintln!(
            "[ctor-fix] elidable-resolver: {} elidable=true find_class_by_name={}{}",
            target_name,
            via_find,
            if via_find {
                ""
            } else {
                "  <- APP-CLASS GAP CLOSED"
            },
        );
    }
    elidable
}

/// `CRATONVM_JIT=sync-methods` — admit `ACC_SYNCHRONIZED` methods to the
/// invocation-counter compile path, where the interpreter's call wrapper owns
/// the implicit monitor. Read once and cached; this sits on the hot
/// uncached-invocation path. Default-OFF → behaviour byte-for-byte unchanged.
fn jit_sync_methods_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_SYNC_METHODS"))
}

/// Try to JIT-compile a method and return the upgraded cache target.
/// Returns None if the method is not JIT-compatible.
/// Uses the shared JIT cache to avoid re-compiling across threads.
///
/// WP2.4-F1: prefer [`try_jit_upgrade_with_gate`] from invoke-cache call
/// sites that already hold a [`RedefineGate`] from a prior bytecode hit;
/// this entry point synthesizes a fresh gate from the manager.
pub(super) fn try_jit_upgrade(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
) -> Option<CachedInvokeTarget> {
    // WP2.4-F1: derive a gate from the manager so JIT-only callers
    // (e.g. tier promotions outside the cache hit path) still get
    // staleness invalidation.
    let gate = RedefineGate::snapshot(
        shared
            .classes
            .class_manager
            .read()
            .class_redefine_generation_handle(cached.declaring_class_id),
    );
    try_jit_upgrade_with_gate(shared, cached, gate)
}

/// How many compiled field sites were refused because the field the resolver
/// FOUND does not have the descriptor the constant pool NAMED.
pub static JIT_FIELD_TAG_DISAGREEMENTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Does the resolved field's own descriptor agree with the one the constant
/// pool's `NameAndType` spells for this site?
///
/// # Why this was asked at all
///
/// **The premise below is now historical.** When this guard was written, this
/// VM's field resolution — `MemberResolver::locate_field`,
/// `ClassFile::find_own_field` and `find_field_recursive` underneath it —
/// matched on the **name alone** and returned the first field of that name it
/// met walking the class, its superinterfaces and its superclass chain, while
/// JVMS §5.4.3.2 resolves by name **and** descriptor.
///
/// Resolution applies the full key since 2026-08-27, and raises
/// `NoSuchFieldError` when the pair is absent since 2026-08-28
/// (`CRATONVM_FIELD_RESOLUTION_NAME_ONLY=1` restores the old answer). So a
/// disagreement can no longer arise from the resolver, and this guard should
/// count zero forever. It is kept as the tripwire for that: a non-zero here
/// now means the descriptor key was NOT applied on some path — the lenient
/// lever is armed, or a caller reached `locate_field` with `descriptor: None`
/// where it should have passed one.
///
/// For the interpreter that is almost always harmless: it reads and writes a
/// dynamically-tagged 16-byte cell, so landing on a same-named field of another
/// type produces a wrong value, not a wrong SHAPE.
///
/// For the JIT it is not harmless, because the two halves of a compiled field
/// site come from two different places. The **slot index** comes from that
/// name-only search; the **type tag** comes from the constant-pool descriptor,
/// which the search never consulted. When they name different fields the
/// compiler emits the tag of one field at the slot of the other — and a `putfield`
/// whose CP descriptor says `I` at a slot whose class declares `[C` writes a
/// `Value::Int` into a reference cell. That is the punned-cell shape exactly:
/// `SQLChar.rawData` (`[C`, slot 1) found holding `Int(1)`, dereferenced by a
/// compiled `arraylength` as the pointer `1`
/// (`known-issues/tomcat/punned-sqlchar-rawdata-cell-writer-localized-…`).
///
/// Refusing the site is the conservative answer: the method falls back to a
/// tier that reads the cell's own tag, so a disagreement costs compilation
/// rather than correctness. That was worth keeping after the resolver was
/// fixed, because it is cheap and it fails in the safe direction.
///
/// A `desc_byte` of 0 means the resolver had no descriptor to report (an empty
/// descriptor string); that is a missing observation, not a disagreement, so it
/// is admitted unchanged.
/// Snapshot of [`JIT_FIELD_TAG_DISAGREEMENTS`], for the end-of-run report.
pub fn jit_field_tag_disagreements() -> u64 {
    JIT_FIELD_TAG_DISAGREEMENTS.load(std::sync::atomic::Ordering::Relaxed)
}

fn jit_field_tag_agrees(field: &ResolvedField, cp_tag: u8, cp_idx: u16) -> bool {
    if field.desc_byte == 0 || field.desc_byte == cp_tag {
        return true;
    }
    let n = JIT_FIELD_TAG_DISAGREEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 32 {
        tracing::warn!(
            target: "cratonvm::jit",
            cp_index = cp_idx,
            declaring_class_id = field.declaring_class_id.as_u32(),
            field_index = field.field_index,
            resolved_desc = %(field.desc_byte as char),
            cp_desc = %(cp_tag as char),
            "refusing a compiled field site: name-only field resolution found a field whose descriptor differs from the one the constant pool names, so the site's slot index and its type tag describe different fields",
        );
    }
    false
}

/// Resolve one `ldc` / `ldc_w` constant-pool entry for the JIT, in the pool of
/// **`holder`**.
///
/// # Why this is one function and not three copies
///
/// A `String` or `Class` constant is baked into compiled code as a SITE — the
/// pair `(holder class id, cp index)` — which `jit_ldc_string_cp` /
/// `jit_ldc_class_cp` re-read at run time. The two halves of that pair must come
/// from the same constant pool, and nothing in the types says so: the index is a
/// `u16` and the holder a `u32`, and either can be filled in from whichever
/// `ClassId` happens to be in scope.
///
/// Three copies of this resolver existed — one for the compiling method, one for
/// a callee compiled as its own artifact, one for OSR — each spelling the holder
/// out by hand a few lines below its own `cm.get_class(...)`. The callee copy
/// spelled the CALLER's `class_id` in its `String` arm while reading the
/// CALLEE's pool; its `ClassReference` arm three lines further down used the
/// callee's. Compiling `SqlClientPool.newConnection` (which inlines
/// `SqlClientConnection.<init>`) then baked `(SqlClientPool, cp#47)` for a
/// literal that lives at cp#47 of `SqlClientConnection`. cp#47 of
/// `SqlClientPool` is a `ClassReference`, so the helper raised `InternalError`
/// and `TechEmpowerTest` answered HTTP 500 — 3/3 runs under
/// `CRATONVM_BG_COMPILE=0`.
///
/// **That was the loud half.** Had the caller held a `String` at that index,
/// compiled code would have pushed the **wrong literal** and reported nothing.
///
/// One function takes one `holder` and uses it for the lookup AND for both
/// baked sites, so the halves cannot disagree again. Behaviour is otherwise
/// identical to the three copies it replaces.
///
/// `get_utf8_wide(..).is_none()` is the representability test, unchanged: a
/// lone-surrogate literal cannot be pooled on a Rust `String` and stays
/// uncompilable.
fn jit_ldc_constant_for(
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    cp_idx: u16,
) -> Option<cratonvm_jit::JitLdcConstant> {
    let class = cm.get_class(holder)?;
    match class.constant_pool.get(cp_idx)? {
        ConstantPoolEntry::Integer(v) => Some(cratonvm_jit::JitLdcConstant::Immediate {
            bits: *v as i64,
            is_float: false,
        }),
        ConstantPoolEntry::Float(v) => Some(cratonvm_jit::JitLdcConstant::Immediate {
            bits: v.to_bits() as i64,
            is_float: true,
        }),
        ConstantPoolEntry::StringReference { string_index }
            if class.constant_pool.get_utf8_wide(*string_index).is_none() =>
        {
            // The SITE, not the text: the record JVMS 5.4.3 keeps is keyed
            // `(class, cp index)`.
            class.constant_pool.get_utf8(*string_index).map(|_| {
                cratonvm_jit::JitLdcConstant::String {
                    holder_class_id: holder.as_u32(),
                    cp_idx,
                }
            })
        }
        // `ldc <Class>`: the mirror is a heap object and the target class may
        // not be loaded yet, so report the SITE and let `helpers.ldc_class_cp`
        // resolve it and fetch the mirror at run time, the way the
        // interpreter's own `ldc` handler does.
        ConstantPoolEntry::ClassReference { .. } => {
            Some(cratonvm_jit::JitLdcConstant::ClassMirror {
                holder_class_id: holder.as_u32(),
                cp_idx,
            })
        }
        _ => None,
    }
}


/// Build an OPTIMIZING-TIER artifact for `cached`, and nothing else.
///
/// The input assembly — every constant-pool resolver, the invoke plans, the
/// `new`-site resolutions, the inline sites, the PGO profile, the helper table
/// — with **no admission policy and no side effects on the method's tiering
/// state**. That separation is the whole point of this function existing.
///
/// It was extracted from [`try_jit_upgrade_with_gate`], which is the
/// METHOD-ENTRY door: it asks a policy question first, and on a refusal it
/// calls `mark_jit_bail_listed`. That combination makes it unusable as a
/// question. Another door that called it to ask "could the optimizing tier
/// take this?" got two wrong answers at once — a refusal for a reason that
/// belongs to the method-entry door (any native-shadowed callee, i.e. nearly
/// every method containing a `println`), and a bail-list entry that
/// `compile_gate::admit` then honours at EVERY door, switching off compiles
/// that were working. The OSR door tried exactly that on 2026-09-04.
///
/// So: policy stays with the door that owns it, assembly lives here, and a
/// caller takes the second without triggering the first. `try_jit_upgrade_with_gate`
/// keeps its gates and calls this; the OSR door calls this and applies its own.
///
/// `None` when the backend declines the method — a compile refusal, which is
/// never a statement about whether the method may be compiled again.
pub(super) fn compile_optimizing_artifact(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
) -> Option<cratonvm_jit::CompiledMethod> {
    let class_id = cached.declaring_class_id;
    let resolver = |cp_idx: u16| -> Option<String> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        class
            .constant_pool
            .get_class_name(cp_idx)
            .map(|s| s.to_string())
    };
    let field_resolver = |cp_idx: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        // Resolve the field using the standard resolution mechanism
        let field = resolve_field_ref(shared, class_id, cp_idx).ok()?;
        // Get the field descriptor from the constant pool
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let nat_idx = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::FieldReference {
                name_and_type_index,
                ..
            }) => *name_and_type_index,
            _ => return None,
        };
        let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        let type_tag = *descriptor.as_bytes().first()?;
        if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
            return None;
        }
        // `None` ⇒ no genuine compact slot for this field (class has no
        // registered `CompactLayout`, or the index falls outside it).
        // Fabricating a `(0, false)` placeholder here poisoned the JIT's
        // compact-offset inline getfield/putfield with a garbage offset: for
        // a reference field it emitted a 32-bit sign-extended load of half a
        // `Value` cell, producing a bogus non-null receiver that SIGSEGVed in
        // the invoke inline cache (WildFly Host Controller `host=foo:add()`,
        // docs/known-issues/wildfly-domain-hostcontroller-sigsegv-*).
        Some((
            field.field_index,
            type_tag,
            cratonvm_types::compact_field_slot(
                field.declaring_class_id.as_u32(),
                field.field_index,
            )
            .map(|(o, r)| (o as u32, r)),
        ))
    };
    let static_field_resolver = |cp_idx: u16| -> Option<(u32, usize, u8, bool)> {
        let field = resolve_field_ref(shared, class_id, cp_idx).ok()?;
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let nat_idx = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::FieldReference {
                name_and_type_index,
                ..
            }) => *name_and_type_index,
            _ => return None,
        };
        let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        let type_tag = *descriptor.as_bytes().first()?;
        if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
            return None;
        }
        Some((
            field.declaring_class_id.as_u32(),
            field.field_index,
            type_tag,
            field.is_volatile,
        ))
    };
    let invoke_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        // Read method_ref or interface_method_ref from constant pool
        let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let target_class = class.constant_pool.get_class_name(class_idx)?;
        let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        Some((
            target_class.to_string(),
            method_name.to_string(),
            descriptor.to_string(),
        ))
    };
    // JVMS §6.5 `invokespecial` super-call redirect — see `try_compile`'s
    // doc comment on `cp_invokespecial_owner_resolver`. `class_id` here is
    // the class whose bytecode is being compiled, i.e. the CALLING class for
    // every invoke site scanned below — exactly the identity the redirect
    // rule needs. Returns `None` (no override) whenever the redirect does
    // not apply, which is the overwhelming majority of `invokespecial` sites
    // (constructors, private methods, ordinary direct-superclass supers).
    let invokespecial_owner_resolver = |cp_idx: u16, opcode: u8| -> Option<String> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let (class_idx, nat_idx, is_iface) = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index, false),
            Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index, true),
            _ => return None,
        };
        let target_class = class.constant_pool.get_class_name(class_idx)?;
        let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        // JVMS 5.4.6 -- an `invokevirtual` (0xb6) that resolves to a PRIVATE
        // method selects exactly that method: no override lookup, no walk up
        // from the receiver. javac emits 0xb6 for a call to a private instance
        // method from Java 11 on (JEP 181 nestmates), where it used to emit
        // `invokespecial` -- so this is now the ordinary encoding of
        // `this.somePrivateHelper()`, including the
        // constructor-calls-its-own-`init()` shape that
        // `io/vertx/core/net/TCPSSLOptions`, `ClientOptionsBase` and
        // `HttpClientOptions` all use, one per level of the same chain.
        //
        // Answering here reclassifies the site as a DIRECT bind at the
        // declaring class, which is what stops the compiled dispatchers
        // resolving it from the receiver and landing on the most-derived
        // same-named private method.
        //
        // Access control makes the answer precise rather than a guess: a
        // private method is invocable only from the class that declares it, so
        // the constant pool's owner name is this compiling class itself and no
        // loader-blind name lookup is in play.
        if opcode == 0xb6 {
            let cp_class_id = cm.find_class_by_name_for_class(target_class, class_id)?;
            let store = cm.class_store();
            if let Some(declaring_id) = crate::classloading::invokevirtual_private_declaring_class(
                cp_class_id,
                method_name,
                descriptor,
                store,
            ) {
                return store.get(declaring_id).map(|c| c.name.to_string());
            }
            // Not private, but possibly unoverridable anyway — a `final`
            // method, or any method of a `final` class, has exactly one
            // possible target at this site for the same reason a private one
            // does. Same conclusion (statically bound; substitute the
            // DECLARING class, which for this rule is often NOT the class the
            // constant pool names), reached by a different argument. Both
            // rules live outside this file — the private one in
            // `classloading::invokevirtual_private_declaring_class`, this one
            // in `invoke::invokevirtual_site_final_owner` — so these three
            // per-door copies cannot drift apart on either.
            return super::invoke::invokevirtual_site_final_owner(
                shared,
                &cm,
                class_id,
                target_class,
                method_name,
                descriptor,
            );
        }
        if opcode != 0xb7 {
            return None;
        }
        let cp_class_id = cm.find_class_by_name_for_class(target_class, class_id)?;
        let store = cm.class_store();
        let start = crate::classloading::invokespecial_selection_start(
            class_id,
            cp_class_id,
            is_iface,
            method_name,
            store,
        );
        if start == cp_class_id {
            return None;
        }
        store.get(start).map(|c| c.name.to_string())
    };
    // invokedynamic-uncommon-trap fix: resolves an invokedynamic CP index to
    // just its target descriptor (no bootstrap/CallSite resolution needed —
    // the codegen only needs the call site's arg/return stack effect).
    //
    // It also returns the BRIDGE SITE for that index (0 when the bootstrap is
    // one the bridge cannot serve). Folded into this resolver rather than added
    // as a second callback because `indy_info`'s fifth element has to be filled
    // in the same loop, and a second `Option<&dyn Fn>` parameter would have had
    // to be threaded through every `try_compile` caller in the tree — including
    // twenty test rows that pass `None` for a method with no indy in it.
    let indy_descriptor_resolver = |cp_idx: u16| -> Option<(String, usize)> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        match class.constant_pool.get(cp_idx)? {
            ConstantPoolEntry::InvokeDynamic {
                name_and_type_index,
                ..
            } => class
                .constant_pool
                .get_name_and_type(*name_and_type_index)
                .map(|(_name, descriptor)| {
                    (
                        descriptor.to_string(),
                        crate::runtime::invokedynamic::make_jit_indy_bridge_site_from_parts(
                            &class.constant_pool,
                            &class.bootstrap_methods,
                            cp_idx,
                            class_id,
                        )
                        .unwrap_or(0),
                    )
                }),
            _ => None,
        }
    };
    // new/anewarray resolver: maps a CP index of `new`/`anewarray` to a
    // `JitNewSite` — `Resolved` (class_id, num_fields,
    // has_nonzero_tag_primitive_init, has_finalizer) when the target class is
    // already loaded, `Deferred` (holder class id + CP index) when it is not.
    let new_resolver = |cp_idx: u16| -> Option<cratonvm_jit::JitNewSite> {
        let cm = shared.classes.class_manager.read();
        resolve_jit_new_site(&cm, class_id, cp_idx)
    };
    // PGO-02: receiver class-id -> class-name resolver for a guarded
    // speculative virtual/interface inline plan's SpeculatedReceiver
    // invalidation dependency (plan_inline's fail-closed rule — see
    // docs/feature-designs/profile-guided-inlining.md). `None` (id not
    // loaded, or unloaded between profiling and compiling) refuses
    // that one speculation rather than recording an unmatchable
    // name-less dependency.
    let class_id_namer = |cid: u32| -> Option<String> {
        let cm = shared.classes.class_manager.read();
        cm.get_class(cratonvm_types::ClassId::new(cid))
            .map(|c| c.name.to_string())
    };
    // PGO-02 R0: the BODY a receiver of exactly `cid` dispatches to at a site
    // declared `(cp_class, name, desc)` — what a guard admitting that class may
    // splice. See `resolve_receiver_inline_site` for why the constant-pool
    // callee is the wrong body here.
    let receiver_inline_resolver = |cid: u32, cp_class: &str, name: &str, desc: &str| {
        resolve_receiver_inline_site(
            shared,
            cached.declaring_class_id,
            cid,
            cp_class,
            name,
            desc,
            // No direct-bind resolver on the guarded-virtual path: the
            // closure that owns it is declared further down this function, and
            // a spliced body reached through a receiver guard is planned before
            // it exists. The consequence is a REFUSAL, never a downgrade — a
            // call-carrying body with nothing to bind to is not admitted at all
            // (see the admission rule in `resolve_inline_site_from`).
            None,
        )
    };
    // activate-ir-optimizer: elidable-`<init>` resolver for `new` scalar
    // replacement. Now default-ON (soaked: bt10/14/16/18 == HotSpot, POJO probes
    // == HotSpot, 802 jit + 20 differential tests green). `CRATONVM_JIT_SCALAR_NEW=0`
    // is the opt-out safety net — when off, `None` is passed and the IR builder
    // bails on `new`, restoring the single-pass backend for allocation methods.
    let scalar_new_on = crate::runtime::env_cache::jit_scalar_new();
    let elidable_init_resolver =
        |cp_idx: u16| -> bool { resolve_jit_elidable_init_loading(shared, class_id, cp_idx) };
    // invoke class-id resolver: maps an invoke* CP index to the class id of
    // its declared (Methodref) class. Used by the CRC32/CRC32C `update`
    // call-site intrinsics for the receiver class-id guard.
    let invoke_class_id_resolver = |cp_idx: u16| -> Option<u32> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let class_idx = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference { class_index, .. }) => *class_index,
            Some(ConstantPoolEntry::InterfaceMethodReference { class_index, .. }) => *class_index,
            _ => return None,
        };
        let target_class = class.constant_pool.get_class_name(class_idx)?;
        Some(
            cm.find_class_by_name_for_class(target_class, class_id)?
                .as_u32(),
        )
    };
    // Review #80: the class that DECLARES each invoke's resolved method, for
    // the `Atomic*` intrinsics' subclass sites.
    let invoke_declaring_class_resolver = |cp_idx: u16| -> Option<String> {
        cp_method_ref_declaring_class_name(shared, class_id, cp_idx)
    };

    let ldc2w_resolver = |cp_idx: u16| -> Option<(i64, bool)> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        // inc 35: report `(bits, is_double)` so the IR builder lowers a `double`
        // constant to `dconst` and a `long` to `lconst`.
        let val = match class.constant_pool.get(cp_idx)? {
            ConstantPoolEntry::Long(v) => Some((*v, false)),
            ConstantPoolEntry::Double(v) => Some((v.to_bits() as i64, true)), // Cast: JIT ABI -- float bits to i64
            _ => None,
        };
        if crate::runtime::env_cache::dbg_jit_ldc() {
            eprintln!(
                "[cratonvm-ldc2w] upgrade idx={} -> {:?} (f64 {})",
                cp_idx,
                val,
                // Cast: integer word reinterpreted as float/double bit pattern
                val.map(|(v, _)| f64::from_bits(v as u64))
                    .unwrap_or(f64::NAN)
            );
        }
        val
    };

    // RBC.2 — `ldc`/`ldc_w` int/float constants. This resolver was never
    // wired on the invocation-counter upgrade path, so ANY method containing
    // an `ldc` opcode (BC's `Nat192/Nat256.gte` load Integer.MIN_VALUE via
    // `ldc`, `SecP*Field` / `Mod` load reduction constants, the ASN.1 parser
    // statics load limit masks) failed codegen at the 0x12/0x13 arm on every
    // retry and stayed interpreted forever — the dominant cause of the
    // BC-suite 34-64× interpreter gap. A `MethodHandle`/`MethodType`/condy
    // `ldc` still returns `None` → permanent compile bail.
    let ldc_resolver = |cp_idx: u16| -> Option<cratonvm_jit::JitLdcConstant> {
        let cm = shared.classes.class_manager.read();
        jit_ldc_constant_for(&cm, class_id, cp_idx)
    };

    // Callee compiler: given (class_name, method_name, descriptor), try to JIT-compile
    // the callee and return (entry_ptr, needs_context). Used for cross-method direct calls.
    let callee_compiler = |callee_class: &str,
                           callee_method: &str,
                           callee_desc: &str|
     -> Option<(usize, bool)> {
        // RFJP.1 (RETIRED, lever-only) — refuse a callee on a class
        // transitively extending `java/util/concurrent/ForkJoinTask`.
        // Inert unless `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1`; matches
        // `try_jit_compile_callee`.
        if is_fjp_subclass_blocklisted(shared, callee_class, Some(cached.declaring_class_id)) {
            cratonvm_jit::note_direct_callee_bind_refusal(
                cratonvm_jit::DirectBindRefusal::FjpBlocklist,
            );
            return None;
        }
        // S111r15 — refuse to compile a callee that has a Rust native
        // shadow. Mirrors the gate in `try_jit_compile_callee` /
        // `try_jit_upgrade_with_gate` / first-call JIT / OSR. Without
        // this check, the recursive callee-compile path direct-called
        // `Character.toLowerCase(C)C`'s JDK bytecode (which delegates
        // to `(I)I` → `CharacterData.of/toLowerCase` virtual chain),
        // and the resulting machine code returned 0 for most inputs
        // after warm-up. Result: Spring's
        // `BeanPropertyName.toDashedForm` produced
        // `r\0\0\0\0\0\0\0-\0\0\0\0\0\0` for `bannerMode`, tripping
        // `InvalidConfigurationPropertyNameException` in SportMe.
        if shared
            .natives
            .native_methods
            .find(callee_class, callee_method, callee_desc)
            .is_some()
        {
            cratonvm_jit::note_direct_callee_bind_refusal(
                cratonvm_jit::DirectBindRefusal::NativeShadow,
            );
            return None;
        }
        // A direct compiled entry has no interpreter boundary to route an
        // implicit exception through the callee's own handler. Keep only
        // those methods on the checked dispatch path.
        {
            let cm = shared.classes.class_manager.read();
            if let Some(callee_cid) =
                cm.find_class_by_name_for_class(callee_class, cached.declaring_class_id)
            {
                let store = cm.class_store();
                if let Some((method, _decl)) = crate::classloading::find_method_recursive(
                    callee_cid,
                    callee_method,
                    callee_desc,
                    store,
                ) {
                    // Sibling of the same gate in `direct_callee_lookup`.
                    if method
                        .code()
                        .map_or(false, |code| !code.exception_table.is_empty())
                        && !cratonvm_jit::direct_call_exc_table_publish_enabled()
                    {
                        cratonvm_jit::note_direct_callee_bind_refusal(
                            cratonvm_jit::DirectBindRefusal::CalleeExceptionTable,
                        );
                        return None;
                    }
                }
            }
        }
        // Check JIT cache first
        let callee_class_arc: Arc<str> = Arc::from(callee_class);
        let callee_method_arc: Arc<str> = Arc::from(callee_method);
        let callee_desc_arc: Arc<str> = Arc::from(callee_desc);
        {
            let callee_class_id = shared
                .classes
                .class_manager
                .read()
                .find_class_by_name_for_class(callee_class, cached.declaring_class_id)
                .unwrap_or(ClassId::new(0));
            let jit_cache = shared.jit.jit_cache.read();
            if let Some(compiled) = jit_cache.get(
                &callee_class_arc,
                &callee_method_arc,
                &callee_desc_arc,
                callee_class_id,
            ) {
                // jit-invokedynamic-groovy-regression fix: never bake a
                // direct machine-code CALL to an artifact containing an
                // unconditional invokedynamic trap — its sentinel +
                // stashed frame would bail through the compiled CALLER's
                // epilogue, past the only point (a dispatch helper) that
                // can resume the callee precisely. Returning None keeps
                // the site on `jit_invoke_dispatch`, whose
                // `try_resume_trapped_callee` resolves the trap in place.
                if compiled.has_indy_trap {
                    cratonvm_jit::note_direct_callee_bind_refusal(
                        cratonvm_jit::DirectBindRefusal::IndyTrap,
                    );
                    return None;
                }
                // Cast: object/code pointer to integer address
                return Some((compiled.entry_ptr() as usize, compiled.needs_context()));
                // Cast: JIT entry point to address
            }
        }

        // Look up the callee class and method
        let cm = shared.classes.class_manager.read();
        let callee_class_id =
            cm.find_class_by_name_for_class(callee_class, cached.declaring_class_id)?;
        let store = cm.class_store();
        let (method, declaring_id) = crate::classloading::find_method_recursive(
            callee_class_id,
            callee_method,
            callee_desc,
            store,
        )?;
        // Direct callee compilation must share the synchronized-method gate.
        if method.is_synchronized() {
            cratonvm_jit::note_direct_callee_bind_refusal(
                cratonvm_jit::DirectBindRefusal::Synchronized,
            );
            return None;
        }

        let code_attr = method.code()?;

        // jit-invokestatic-clinit-gap fix (2026-07-17): JVMS §5.5
        // requires a class be initialized before the first invocation
        // of any of its own (not inherited) static methods -- the same
        // trigger family as the `jit_getstatic`/`jit_putstatic_*`/
        // `jit_new_object` fixes above, but for `invokestatic`. This
        // closure builds a raw machine-code CALL straight to the
        // callee's compiled entry point (`direct_calls` in
        // `jit/src/lib.rs`), bypassing BOTH the interpreter's own
        // `execute_invokestatic` (which calls
        // `ensure_class_initialized_shared` unconditionally before
        // every dispatch) and the JIT's generic fallback dispatch
        // helper (`jit_invoke_dispatch` -> `invoke_or_native` ->
        // `invoke_shared`, which also checks). Once a JIT-compiled
        // caller takes this direct-call fast path for an invokestatic
        // site, that site never routes through either checked path
        // again -- if the callee's declaring class hadn't been
        // initialized yet the moment this closure ran, it may never
        // get initialized before the direct CALL first executes.
        //
        // A class's initialized state is monotonic per JVMS (once
        // Initialized, it never reverts), so checking ONCE here, at
        // compile time, is sound forever for this call site. Only take
        // the direct-call fast path when the callee is a `static`
        // method (the actual JVMS trigger -- `invokespecial`'s
        // `<init>`/private/super calls reach this same closure but
        // don't independently require class init, since their
        // receiver's class was already initialized via `new`) AND its
        // declaring class is ALREADY initialized. Otherwise return
        // `None`, which drops the call site to the generic dispatch
        // fallback (`jit_invoke_dispatch`) -- correctness-safe (that
        // path checks), just not the fast path for this one call site
        // until a future recompile (e.g. after the class initializes
        // and the caller tiers up again). `is_class_initialized_fast`
        // reads the embedded per-`Class` atomic directly with no extra
        // lock -- `cm`'s read guard above (borrowed by `store`) is
        // still live here -- mirroring
        // `ensure_class_initialized_shared`'s own fast path.
        if method.is_static() {
            let declaring_class_initialized = store
                .get(declaring_id)
                .map(crate::vm::is_class_initialized_fast)
                .unwrap_or(false);
            if !declaring_class_initialized {
                cratonvm_jit::note_direct_callee_bind_refusal(
                    cratonvm_jit::DirectBindRefusal::DeclaringClassNotInitialized,
                );
                return None;
            }
        }

        let declaring_class_name = store.get(declaring_id).map(|c| &*c.name)?;
        let source_file = store
            .get(declaring_id)
            .and_then(|c| c.source_file.as_deref())
            .map(Arc::from);
        let num_params = count_method_params(callee_desc);

        let callee_cached = CachedBytecodeMethod {
            declaring_class_id: declaring_id,
            class_name: Arc::from(declaring_class_name),
            method_name: Arc::from(callee_method),
            method_descriptor: Arc::from(callee_desc),
            source_file,
            code: crate::runtime::frame::padded_bytecode(&code_attr.code),
            exception_table: Arc::from(code_attr.exception_table.as_slice()),
            max_stack: code_attr.max_stack,
            max_locals: code_attr.max_locals,
            num_params: num_params as u16, // Widening: parameter count conversion
            is_synchronized: method.is_synchronized(),
            is_static: method.is_static(),
            force_native_cache: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            tiering_settled: std::sync::atomic::AtomicU32::new(0),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        };
        drop(cm);

        // S-HIB.1 twin — apply the static skip list to recursive callee
        // compilations from THIS closure too. Before this check, the
        // closure honored only the FJP blocklist + native-shadow gate, so
        // a threshold compile could silently callee-compile methods every
        // other path bans: complex `<init>`/`<clinit>` bodies (observed:
        // `java/util/regex/Pattern.<init>` attempted during the
        // `Pattern.compile` upgrade), `is_known_miscompile` entries, and
        // anything in `CRATONVM_JIT_BISECT_SKIP` — which also made
        // skip-based bisection silently unsound for any method reachable
        // as a direct callee. Mirrors `try_jit_compile_callee`.
        {
            // The static JIT ban list was deleted 2026-07-31 (see
            // docs/known-issues/jit-bans/jit-bans-all-disabled-20260731.md).
            // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
            // remaining force-interpret lever, applied in `jit::try_compile`.
            // GPU-offload JIT admission gate — see offload_jit_gate.
            #[cfg(feature = "gpu-offload")]
            if crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
                shared,
                callee_cached.declaring_class_id,
                callee_method,
                &callee_cached.method_descriptor,
            ) {
                return None;
            }
        }

        // Build resolvers for the callee's constant pool
        let callee_cid = declaring_id;
        let c_resolver = |cp_idx: u16| -> Option<String> {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(callee_cid)?;
            class
                .constant_pool
                .get_class_name(cp_idx)
                .map(|s| s.to_string())
        };
        let c_field_resolver = |cp_idx: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
            let field = resolve_field_ref(shared, callee_cid, cp_idx).ok()?;
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(callee_cid)?;
            let nat_idx = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::FieldReference {
                    name_and_type_index,
                    ..
                }) => *name_and_type_index,
                _ => return None,
            };
            let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
            let type_tag = *descriptor.as_bytes().first()?;
            if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
                return None;
            }
            // `None` ⇒ no genuine compact slot — do NOT fabricate
            // `(0, false)` (see the sibling resolver's comment).
            Some((
                field.field_index,
                type_tag,
                cratonvm_types::compact_field_slot(
                    field.declaring_class_id.as_u32(),
                    field.field_index,
                )
                .map(|(o, r)| (o as u32, r)),
            ))
        };
        let c_static_field_resolver = |cp_idx: u16| -> Option<(u32, usize, u8, bool)> {
            let field = resolve_field_ref(shared, callee_cid, cp_idx).ok()?;
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(callee_cid)?;
            let nat_idx = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::FieldReference {
                    name_and_type_index,
                    ..
                }) => *name_and_type_index,
                _ => return None,
            };
            let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
            let type_tag = *descriptor.as_bytes().first()?;
            if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
                return None;
            }
            Some((
                field.declaring_class_id.as_u32(),
                field.field_index,
                type_tag,
                field.is_volatile,
            ))
        };
        let c_invoke_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(callee_cid)?;
            let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                }) => (*class_index, *name_and_type_index),
                Some(ConstantPoolEntry::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                }) => (*class_index, *name_and_type_index),
                _ => return None,
            };
            let target_class = class.constant_pool.get_class_name(class_idx)?;
            let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
            Some((
                target_class.to_string(),
                method_name.to_string(),
                descriptor.to_string(),
            ))
        };
        // JVMS §6.5 `invokespecial` super-call redirect for the callee's
        // own constant pool — see `invokespecial_owner_resolver` above /
        // `try_compile`'s doc comment. `callee_cid` is the class whose
        // bytecode is being compiled here (the eagerly-compiled callee),
        // i.e. the calling class for every invoke site in ITS bytecode.
        let c_invokespecial_owner_resolver = |cp_idx: u16, opcode: u8| -> Option<String> {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(callee_cid)?;
            let (class_idx, nat_idx, is_iface) = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                }) => (*class_index, *name_and_type_index, false),
                Some(ConstantPoolEntry::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                }) => (*class_index, *name_and_type_index, true),
                _ => return None,
            };
            let target_class = class.constant_pool.get_class_name(class_idx)?;
            let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
            // JVMS 5.4.6 -- an `invokevirtual` (0xb6) that resolves to a PRIVATE
            // method selects exactly that method: no override lookup, no walk up
            // from the receiver. javac emits 0xb6 for a call to a private instance
            // method from Java 11 on (JEP 181 nestmates), where it used to emit
            // `invokespecial` -- so this is now the ordinary encoding of
            // `this.somePrivateHelper()`, including the
            // constructor-calls-its-own-`init()` shape that
            // `io/vertx/core/net/TCPSSLOptions`, `ClientOptionsBase` and
            // `HttpClientOptions` all use, one per level of the same chain.
            //
            // Answering here reclassifies the site as a DIRECT bind at the
            // declaring class, which is what stops the compiled dispatchers
            // resolving it from the receiver and landing on the most-derived
            // same-named private method.
            //
            // Access control makes the answer precise rather than a guess: a
            // private method is invocable only from the class that declares it, so
            // the constant pool's owner name is this compiling class itself and no
            // loader-blind name lookup is in play.
            if opcode == 0xb6 {
                let cp_class_id = cm.find_class_by_name_for_class(target_class, callee_cid)?;
                let store = cm.class_store();
                if let Some(declaring_id) =
                    crate::classloading::invokevirtual_private_declaring_class(
                        cp_class_id,
                        method_name,
                        descriptor,
                        store,
                    )
                {
                    return store.get(declaring_id).map(|c| c.name.to_string());
                }
                // Not private, but possibly unoverridable anyway — a `final`
                // method, or any method of a `final` class, has exactly one
                // possible target at this site for the same reason a private one
                // does. Same conclusion (statically bound; substitute the
                // DECLARING class, which for this rule is often NOT the class the
                // constant pool names), reached by a different argument. Both
                // rules live outside this file — the private one in
                // `classloading::invokevirtual_private_declaring_class`, this one
                // in `invoke::invokevirtual_site_final_owner` — so these three
                // per-door copies cannot drift apart on either.
                return super::invoke::invokevirtual_site_final_owner(
                    shared,
                    &cm,
                    callee_cid,
                    target_class,
                    method_name,
                    descriptor,
                );
            }
            if opcode != 0xb7 {
                return None;
            }
            let cp_class_id = cm.find_class_by_name_for_class(target_class, callee_cid)?;
            let store = cm.class_store();
            let start = crate::classloading::invokespecial_selection_start(
                callee_cid,
                cp_class_id,
                is_iface,
                method_name,
                store,
            );
            if start == cp_class_id {
                return None;
            }
            store.get(start).map(|c| c.name.to_string())
        };
        // invokedynamic-uncommon-trap fix: resolves an invokedynamic CP
        // index to its target descriptor for the callee's constant pool.
        let c_indy_descriptor_resolver = |cp_idx: u16| -> Option<(String, usize)> {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(callee_cid)?;
            match class.constant_pool.get(cp_idx)? {
                ConstantPoolEntry::InvokeDynamic {
                    name_and_type_index,
                    ..
                } => class
                    .constant_pool
                    .get_name_and_type(*name_and_type_index)
                    .map(|(_name, descriptor)| {
                        (
                            descriptor.to_string(),
                            crate::runtime::invokedynamic::make_jit_indy_bridge_site_from_parts(
                                &class.constant_pool,
                                &class.bootstrap_methods,
                                cp_idx,
                                callee_cid,
                            )
                            .unwrap_or(0),
                        )
                    }),
                _ => None,
            }
        };

        // new/anewarray resolver for callee's constant pool
        let c_new_resolver = |cp_idx: u16| -> Option<cratonvm_jit::JitNewSite> {
            let cm = shared.classes.class_manager.read();
            resolve_jit_new_site(&cm, callee_cid, cp_idx)
        };
        // PGO-02: receiver class-id -> class-name resolver for a guarded
        // speculative virtual/interface inline plan's SpeculatedReceiver
        // invalidation dependency (plan_inline's fail-closed rule — see
        // docs/feature-designs/profile-guided-inlining.md). `None` (id not
        // loaded, or unloaded between profiling and compiling) refuses
        // that one speculation rather than recording an unmatchable
        // name-less dependency.
        let c_class_id_namer = |cid: u32| -> Option<String> {
            let cm = shared.classes.class_manager.read();
            cm.get_class(cratonvm_types::ClassId::new(cid))
                .map(|c| c.name.to_string())
        };
        // PGO-02 R0: the BODY a receiver of exactly `cid` dispatches to at a
        // site declared `(cp_class, name, desc)`. The guard admits a runtime
        // class, so this — not the constant-pool callee — is what may be
        // spliced behind it. See `resolve_receiver_inline_site`.
        let c_receiver_inline_resolver = |cid: u32, cp_class: &str, name: &str, desc: &str| {
            resolve_receiver_inline_site(shared, callee_cid, cid, cp_class, name, desc, None)
        };
        // The optimizing tier's own inline resolver for this callee compile.
        // No direct-bind resolver, and here the reason is scope rather than
        // capability: this is the CALLEE-compile path, entered from planning
        // for another method, and the only binder in reach
        // (`callee_compiler`) compiles what it is asked about. Handing it to a
        // resolver that runs inside planning would let one compile drive
        // another through a door with no depth budget of its own. The calls a
        // body spliced here leaves behind keep the dispatch helper; the main
        // path below is where the rows are produced.
        let c_ir_inline_resolver = |callee_class: &str, callee_method: &str, callee_desc: &str| {
            resolve_ir_inline_site(
                shared,
                callee_cid,
                callee_class,
                callee_method,
                callee_desc,
                None,
            )
        };
        // Elidable-`<init>` resolver for `new` scalar replacement, default-ON
        // (opt-out: CRATONVM_JIT_SCALAR_NEW=0).
        let c_scalar_new_on = crate::runtime::env_cache::jit_scalar_new();
        let c_elidable_init_resolver =
            |cp_idx: u16| -> bool { resolve_jit_elidable_init_loading(shared, callee_cid, cp_idx) };
        // invoke class-id resolver for the callee's constant pool — maps
        // an invoke* CP index to its declared class id, for the CRC32/
        // CRC32C `update` receiver class-id guard.
        let c_invoke_class_id_resolver = |cp_idx: u16| -> Option<u32> {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(callee_cid)?;
            let class_idx = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::MethodReference { class_index, .. }) => *class_index,
                Some(ConstantPoolEntry::InterfaceMethodReference { class_index, .. }) => {
                    *class_index
                }
                _ => return None,
            };
            let target_class = class.constant_pool.get_class_name(class_idx)?;
            Some(
                cm.find_class_by_name_for_class(target_class, callee_cid)?
                    .as_u32(),
            )
        };
        // Review #80: the declaring class of each invoke's resolved method.
        let c_invoke_declaring_class_resolver = |cp_idx: u16| -> Option<String> {
            cp_method_ref_declaring_class_name(shared, callee_cid, cp_idx)
        };

        let c_ldc2w_resolver = |cp_idx: u16| -> Option<(i64, bool)> {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(callee_cid)?;
            // inc 35: `(bits, is_double)`.
            match class.constant_pool.get(cp_idx)? {
                ConstantPoolEntry::Long(v) => Some((*v, false)),
                ConstantPoolEntry::Double(v) => Some((v.to_bits() as i64, true)), // Cast: JIT ABI -- float bits to i64
                _ => None,
            }
        };

        // RBC.2 — `ldc`/`ldc_w` int/float constants. Without this resolver
        // every method containing an `ldc` (e.g. BC's `Nat*.gte` loading
        // Integer.MIN_VALUE) failed codegen at the 0x12 arm and stayed
        // interpreted forever. String/Class ldc returns None → compile
        // bails (matches the OSR path's behaviour).
        let c_ldc_resolver = |cp_idx: u16| -> Option<cratonvm_jit::JitLdcConstant> {
            let cm = shared.classes.class_manager.read();
            jit_ldc_constant_for(&cm, callee_cid, cp_idx)
        };

        // Compile callee without recursive inlining (None for callee_compiler)
        let c_pgo_profile = {
            let profile_key = crate::jit::profile::MethodKey {
                class_id: callee_cached.declaring_class_id.as_u32(),
                method_name: callee_cached.method_name.clone(),
                descriptor: callee_cached.method_descriptor.clone(),
            };
            shared.jit.profile_store.get_profile(&profile_key)
        };
        let c_helpers = crate::jit::helpers::build_helpers_for(shared);
        let c_string_layout_resolver = || resolve_string_field_layout(shared);
        crate::jit::set_self_call_identity_stable(self_call_identity_stable(
            shared,
            callee_cached.declaring_class_id,
        ));
        // JDK-ONLY-WAVE2 §4. Answers "is this triple a reviewed
        // `NativeKind::Intrinsic`?" — `false` for `Bridge`, for `SyntheticStub`
        // and for anything unregistered, which is the fail-closed direction.
        // Cheap: only the strict arm of `direct_native_helper` calls it, and only
        // for a triple whose helper cell is already non-zero.
        let intrinsic_resolver = |class: &str, method: &str, descriptor: &str| -> bool {
            let registry = &shared.natives.native_methods;
            registry
                .resolve_id(class, method, descriptor)
                .and_then(|id| registry.kind_of_id(id))
                .is_some_and(|kind| kind == cratonvm_native_api::NativeKind::Intrinsic)
        };

        let mut compiled = crate::jit::try_compile_with_invokespecial_resolver(
            &callee_cached,
            Some(&c_resolver),
            Some(&c_field_resolver),
            Some(&c_static_field_resolver),
            Some(&c_invoke_resolver),
            Some(&c_invokespecial_owner_resolver),
            None, // no recursive inlining
            Some(&c_new_resolver),
            Some(&c_ldc_resolver),
            Some(&c_ldc2w_resolver),
            c_pgo_profile.as_ref(),
            &c_helpers,
            None, // no inlining in early-compile path
            // String call-site intrinsics (length/charAt/hashCode/equals/…):
            // resolve java/lang/String's value/coder/hash field layout so the
            // JIT inlines these accessors instead of crossing the VM→native
            // boundary per call (bug-03). `resolve_string_field_layout`
            // returns None → intrinsics bail to dispatch when String isn't
            // loaded yet.
            Some(&c_string_layout_resolver),
            Some(&c_invoke_class_id_resolver),
            if c_scalar_new_on {
                Some(&c_elidable_init_resolver)
            } else {
                None
            },
            // Early-compile path is the optimized (C2-equivalent) tier — the
            // tiered C1 routing only flows through the background worker.
            true,
            // Gap B: int-only invokestatic → Op::Call. Now default-ON
            // (inc 23, soaked: bt10/14/16/18 == HotSpot + IrCall/IrCallGc
            // probes == HotSpot, ON==OFF). `CRATONVM_JIT_IR_CALL=0` is the
            // opt-out — restores single-pass dispatch for invokestatic.
            crate::runtime::env_cache::jit_ir_call(),
            // inc 24/29: invokespecial → Op::Call. Now default-ON;
            // `CRATONVM_JIT_IR_CALL_SPECIAL=0` opts out.
            crate::runtime::env_cache::jit_ir_call_special(),
            // inc 25/29: long methods → IR path. Now default-ON; `CRATONVM_JIT_IR_LONG=0` opts out.
            crate::runtime::env_cache::jit_ir_long(),
            // inc 26 + inline-cache lowering: invokevirtual/invokeinterface
            // → Op::Call with MIC/PIC fast paths. Default-ON now that the IR
            // backend has parity with single-pass dispatch;
            // `CRATONVM_JIT_IR_CALL_VIRTUAL=0` opts out.
            crate::runtime::env_cache::jit_ir_call_virtual(),
            // inc 30 + Slices A/B/C: double/float XMM value tier. Now
            // default-ON — the tier is opcode-complete (frem/drem, FP arrays,
            // FP-slot deopt resume all landed) and validated == HotSpot
            // (bt10/14/16/18 checksums + FP E2E probes). `CRATONVM_JIT_IR_FP=0`
            // is the opt-out (restores the int/long/ref-only IR path).
            crate::runtime::env_cache::jit_ir_fp(),
            // invokedynamic-uncommon-trap fix: resolves an invokedynamic
            // CP index to its target descriptor for the callee's pool.
            Some(&c_indy_descriptor_resolver),
            if crate::runtime::env_cache::jit_guarded_virtual_inline() {
                Some(&c_class_id_namer)
            } else {
                None
            },
            if crate::runtime::env_cache::jit_guarded_virtual_inline() {
                Some(&c_receiver_inline_resolver)
            } else {
                None
            },
            // IR-tier inlining. Behind its own gate; `None` splices nothing.
            if cratonvm_jit::ir_inline_enabled() {
                Some(&c_ir_inline_resolver)
            } else {
                None
            },
            // Per-VM JDK-only policy (JDK-ONLY-WAVE2 §2). Was a process-global
            // latch the JIT read for itself, so a `Compatible` VM sharing a
            // process with a `JdkOnly` one lost the thin direct-call helpers.
            crate::vm::dispatch_policy(shared).is_jdk_only(),
            // JDK-ONLY-WAVE2 §4: the registry's own `NativeKind`, in place of

            // the JIT's seven hard-coded triples, as the §1.4 verdict on

            // whether a thin direct-call helper may shadow real bytecode.
            Some(&intrinsic_resolver),
            // This VM's per-bci de-spec registry.
            Some(&shared.jit.despec_registry),
            Some(&c_invoke_declaring_class_resolver),
        )?;
        let entry = compiled.entry_ptr() as usize; // Cast: JIT entry point to address
        let needs_ctx = compiled.needs_context();
        // jit-invokedynamic-groovy-regression fix — see the matching gate
        // at the JIT-cache-hit return above. The artifact is still cached
        // (below) for helper/interpreter dispatch, but never handed back
        // for a baked direct machine-code CALL.
        let indy_trap = compiled.has_indy_trap;
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] callee-compile {}.{}{} entry={:p} len={}",
                callee_cached.class_name,
                callee_cached.method_name,
                callee_cached.method_descriptor,
                compiled.entry_ptr(),
                compiled.code_bytes().len()
            );
        }
        crate::jit::disasm::maybe_dump_annotated(
            if compiled.used_ir_backend { "callee/ir" } else { "callee/sp" },
            &callee_cached.class_name,
            &callee_cached.method_name,
            &callee_cached.method_descriptor,
            compiled.entry_ptr(),
            compiled.code_bytes(),
            compiled.osr_pc_to_native.as_deref(),
            compiled.osr_local_assignments.as_deref(),
        );

        // Store in JIT cache
        stamp_compilation_epoch(
            shared,
            &callee_cached.class_name,
            &callee_cached.method_name,
            &callee_cached.method_descriptor,
            &mut compiled,
        );
        // Stamp the wrapped-entry requirement before the body is shared.
        // See `CompiledMethod::requires_wrapped_entry`: publication is the
        // last point that still knows this is an `ACC_SYNCHRONIZED` method,
        // and every unwrapped consumer downstream holds only a raw entry
        // pointer.
        compiled.requires_wrapped_entry = callee_cached.is_synchronized;
        {
            let mut jit_cache = shared.jit.jit_cache.write();
            jit_cache.put(
                callee_cached.class_name.clone(),
                callee_cached.method_name.clone(),
                callee_cached.method_descriptor.clone(),
                callee_cached.declaring_class_id,
                compiled,
            );
        }

        if indy_trap {
            return None;
        }
        Some((entry, needs_ctx))
    };

    let pgo_profile = {
        let profile_key = crate::jit::profile::MethodKey {
            class_id: cached.declaring_class_id.as_u32(),
            method_name: cached.method_name.clone(),
            descriptor: cached.method_descriptor.clone(),
        };
        shared.jit.profile_store.get_profile(&profile_key)
    };
    let helpers = crate::jit::helpers::build_helpers_for(shared);
    // Small-method inlining for the main tier-up compile. Without it, even a
    // trivial leaf like `static int add(int,int){return a+b;}` compiled to a
    // CALL per use, so call-heavy JDK-internal code (xalan/xerces DTM walks:
    // SuballocatedIntVector.elementAt, DTMDefaultBase._exptype, …) paid full
    // call overhead per node — ~1000x HotSpot, which inlines these to a few
    // instructions. The resolver only admits tiny, exception-free, call-free
    // leaves (see `resolve_inline_site` / MAX_INLINE_BYTECODE_SIZE), and the
    // codegen (`try_emit_inline`) snapshots+rolls back on any unsupported
    // bytecode, so a bail falls through to the existing direct-call/dispatch
    // path. Previously wired only into `try_jit_compile_callee_slow`.
    let inline_resolver = |callee_class: &str,
                           callee_method: &str,
                           callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_inline_site(
            shared,
            cached.declaring_class_id,
            callee_class,
            callee_method,
            callee_desc,
            // The calls INSIDE the body about to be spliced get the same
            // direct-bind treatment this method's own call sites get. Without
            // it a spliced call falls to the blind dispatch helper, which is a
            // measured 3.5x loss on an already-direct-bound chain — see
            // `jit_inline_call_dispatch`.
            Some(&callee_compiler),
        )
    };

    // The optimizing tier's own inline resolver — see `resolve_ir_inline_site`
    // for how its admission set differs in both directions.
    //
    // `callee_compiler` as the direct-bind resolver, which is the SAME one this
    // compile hands `try_compile` for its own call sites and the same one the
    // single-pass `inline_resolver` above hands its spliced bodies. Until
    // 2026-09-09 this argument was absent, and the note where it should have
    // been said "`IrBuilder` has no direct-call lowering inside a relocated
    // body to bake an entry into". That was true; it no longer is
    // (`ir_direct_calls` is keyed by combined-buffer pc and
    // `append_ir_inline_site` fills it), and while it was true every
    // statically-bound call an optimizing splice left behind lowered to
    // `jit_invoke_dispatch` and resolved its callee BY NAME on every
    // execution — the measured 3.5x loss the sibling's own comment describes,
    // paid on the tier that is supposed to be the fast one.
    //
    // Using the same resolver as the sibling matters beyond symmetry: it bounds
    // its own recursion (depth, cycle, fan-out), which is why the sibling's
    // comment gives reuse as the reason not to write a lookup by hand here.
    let ir_inline_resolver = |callee_class: &str,
                              callee_method: &str,
                              callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_ir_inline_site(
            shared,
            cached.declaring_class_id,
            callee_class,
            callee_method,
            callee_desc,
            if cratonvm_jit::ir_splice_direct_call_enabled() {
                Some(&callee_compiler as InlineDirectBind<'_>)
            } else {
                None
            },
        )
    };
    // Main-path small-method inlining is GATED default-OFF behind
    // `CRATONVM_JIT_MAIN_INLINE=1`. Enabling it inlines tiny arith/getter/field
    // leaves correctly (verified: `static int add(int,int){return a+b;}` emits no
    // CALL), but broadly enabling it surfaced a `try_emit_inline_body` miscompile
    // on some Spring boot paths (`ConcurrentReferenceHashMap$TaskOption not an
    // enum` CCE — an inlined body clobbering a caller-live value), so it must not
    // be default-ON until that is root-caused. The infrastructure was previously
    // wired only into `try_jit_compile_callee_slow`. See the JIT-inlining notes.
    let main_inline_on = crate::runtime::env_cache::jit_main_inline();
    let string_layout_resolver = || resolve_string_field_layout(shared);
    crate::jit::set_self_call_identity_stable(self_call_identity_stable(
        shared,
        cached.declaring_class_id,
    ));
    // JDK-ONLY-WAVE2 §4. Answers "is this triple a reviewed
    // `NativeKind::Intrinsic`?" — `false` for `Bridge`, for `SyntheticStub`
    // and for anything unregistered, which is the fail-closed direction.
    // Cheap: only the strict arm of `direct_native_helper` calls it, and only
    // for a triple whose helper cell is already non-zero.
    let intrinsic_resolver = |class: &str, method: &str, descriptor: &str| -> bool {
        let registry = &shared.natives.native_methods;
        registry
            .resolve_id(class, method, descriptor)
            .and_then(|id| registry.kind_of_id(id))
            .is_some_and(|kind| kind == cratonvm_native_api::NativeKind::Intrinsic)
    };

    let compiled = crate::jit::try_compile_with_invokespecial_resolver(
        cached,
        Some(&resolver),
        Some(&field_resolver),
        Some(&static_field_resolver),
        Some(&invoke_resolver),
        Some(&invokespecial_owner_resolver),
        Some(&callee_compiler),
        Some(&new_resolver),
        Some(&ldc_resolver),
        Some(&ldc2w_resolver),
        pgo_profile.as_ref(),
        &helpers,
        if main_inline_on {
            Some(&inline_resolver)
        } else {
            None
        },
        // String call-site intrinsics (length/charAt/hashCode/equals/…): resolve
        // java/lang/String's value/coder/hash field layout so the JIT inlines
        // these accessors instead of crossing the VM→native boundary per call
        // (bug-03). Mirrors the already-wired `try_jit_compile_callee_slow` path.
        Some(&string_layout_resolver),
        Some(&invoke_class_id_resolver),
        if scalar_new_on {
            Some(&elidable_init_resolver)
        } else {
            None
        },
        // Inline mutator compile path is the optimized (C2-equivalent) tier.
        true,
        // Gap B: int-only invokestatic → Op::Call. Now default-ON (inc 23);
        // `CRATONVM_JIT_IR_CALL=0` is the opt-out (single-pass dispatch).
        crate::runtime::env_cache::jit_ir_call(),
        // inc 24/29: invokespecial → Op::Call. Now default-ON; `CRATONVM_JIT_IR_CALL_SPECIAL=0` opts out.
        crate::runtime::env_cache::jit_ir_call_special(),
        // inc 25/29: long methods → IR path. Now default-ON; `CRATONVM_JIT_IR_LONG=0` opts out.
        crate::runtime::env_cache::jit_ir_long(),
        // inc 26: invokevirtual/invokeinterface → Op::Call (dynamic dispatch via
        // the helper), gated default-OFF (its own soak). `=1` opts in.
        crate::runtime::env_cache::jit_ir_call_virtual(),
        // inc 30 + Slices A/B/C: double/float XMM value tier. Now default-ON
        // (opcode-complete + validated == HotSpot). `CRATONVM_JIT_IR_FP=0` opts out.
        crate::runtime::env_cache::jit_ir_fp(),
        // invokedynamic-uncommon-trap fix: resolves an invokedynamic CP index
        // to its target descriptor so the codegen can lower the instruction
        // to an unconditional uncommon-trap deopt instead of bailing the
        // whole method.
        Some(&indy_descriptor_resolver),
        if crate::runtime::env_cache::jit_guarded_virtual_inline() {
            Some(&class_id_namer)
        } else {
            None
        },
        if crate::runtime::env_cache::jit_guarded_virtual_inline() {
            Some(&receiver_inline_resolver)
        } else {
            None
        },
        // IR-tier inlining. Behind its own gate; `None` splices nothing.
        if cratonvm_jit::ir_inline_enabled() {
            Some(&ir_inline_resolver)
        } else {
            None
        },
        // Per-VM JDK-only policy (JDK-ONLY-WAVE2 §2). Was a process-global
        // latch the JIT read for itself, so a `Compatible` VM sharing a
        // process with a `JdkOnly` one lost the thin direct-call helpers.
        crate::vm::dispatch_policy(shared).is_jdk_only(),
        // JDK-ONLY-WAVE2 §4: the registry's own `NativeKind`, in place of

        // the JIT's seven hard-coded triples, as the §1.4 verdict on

        // whether a thin direct-call helper may shadow real bytecode.
        Some(&intrinsic_resolver),
        // This VM's per-bci de-spec registry.
        Some(&shared.jit.despec_registry),
        Some(&invoke_declaring_class_resolver),
    )?;
    Some(compiled)
}

/// WP2.4-F1: variant of [`try_jit_upgrade`] that takes an explicit
/// [`RedefineGate`] so the JIT entry inherits the same staleness binding
/// as the bytecode entry it's replacing.  Saves one
/// `class_manager.read()` round-trip on the hot promotion path.
pub(super) fn try_jit_upgrade_with_gate(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    gate: RedefineGate,
) -> Option<CachedInvokeTarget> {
    // Panic containment for the inline upgrade door; see `try_jit_compile_callee`.
    match cratonvm_jit::tiered::contain_compile_panic(|| {
        try_jit_upgrade_with_gate_uncontained(shared, cached, gate)
    }) {
        Ok(target) => target,
        Err(payload) => {
            note_contained_mutator_compile_panic(
                cached.declaring_class_id,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
                &*payload,
            );
            None
        }
    }
}

/// [`try_jit_upgrade_with_gate`] without panic containment.
fn try_jit_upgrade_with_gate_uncontained(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    gate: RedefineGate,
) -> Option<CachedInvokeTarget> {
    // Kill-switch: CRATONVM_DISABLE_JIT=1 forces interpreter-only execution.
    // Mirrors the gate in `try_jit_compile_callee` so the user-facing
    // CRATONVM_DISABLE_JIT flag actually disables BOTH JIT entry points
    // (the caller-method counter path here, and the dispatcher path there).
    // Useful for bisecting JIT-vs-interpreter bugs during bootstrap crashes.
    if crate::runtime::env_cache::disable_jit() {
        return None;
    }
    // The gate is bound to this exact declaring class. Keep the conservative
    // no-JIT policy for a retransformed body, but do not punish every other
    // class in the VM after an agent touches one class.
    if gate.generation > 0 {
        return None;
    }
    // The compiled body carries no ACC_SYNCHRONIZED monitor prologue/epilogue —
    // the *caller* supplies it. Both interpreter entry points into compiled code
    // (`execute_jit_call` and `execute_jit_call_decoded`) already wrap the call
    // in a `JitSynchronizedMonitorGuard`, which acquires the receiver's (or the
    // class mirror's) monitor for the whole native activation and releases it on
    // every Rust return path. A `CachedInvokeTarget::Jit` is only ever consumed
    // through those two, so admitting a synchronized method here is contained.
    //
    // Every OTHER entry runs the body with no monitor at all, and each must
    // refuse a synchronized callee independently:
    //   * compiled→compiled direct dispatch — `try_jit_compile_callee`, both
    //     on its compile path (`..._slow`'s `is_synchronized` gate) AND on its
    //     `jit_cache` fast path, which serves an already-published body and so
    //     never reaches that gate;
    //   * `jit_invoke_dispatch`'s own `jit_cache` arm (`vm/src/jit/helpers.rs`),
    //     which fills `DISPATCH_CACHE` and does not go through
    //     `try_jit_compile_callee` at all;
    //   * the specialized `get(I)D` scalar routes in the same file — `Vector.get`
    //     is `synchronized` in the JDK, so this one is not hypothetical;
    //   * inlining — `resolve_inline_site`'s `method.is_synchronized()` gate;
    //   * OSR — the `is_synchronized` gate near the top of this file.
    //
    // The first three ask `CompiledMethod::requires_wrapped_entry`, stamped at
    // publication, because they hold a raw entry pointer and no method handle.
    // Listing only the compile-time gates here is what let the fast paths drift:
    // the enumeration said "three" while `jit_cache` answered for two more.
    //
    // Why this matters: every layer Tomcat's BCEL annotation scan drives per
    // byte (`ByteArrayInputStream.read()`, `DataInputStream.readUnsignedByte`)
    // is an ACC_SYNCHRONIZED one-liner, so this gate kept the whole webapp
    // deploy interpreted — the method was rejected here *before* it was ever
    // counted, which is why `jit-method-stats` reported it neither compiled nor
    // `hot_but_stuck_in_interpreter`. See
    // docs/internal/performance/interpreted-invoke-cost-350ns-RETIRED-20260911.md.
    //
    // Default-OFF pending the A/B and the concurrency soak: `CRATONVM_JIT=sync-methods`.
    if cached.is_synchronized && !jit_sync_methods_enabled() {
        return None;
    }
    // RBC.4 — short-circuit permanently-uncompilable methods BEFORE the
    // expensive gates below (two superclass-chain walks under the
    // class_manager read lock). The retry stride re-enters this function
    // every 64 calls for a hot method; with a scan-rejected (e.g. athrow)
    // method that meant tens of thousands of full gate evaluations per
    // suite run while `try_compile` would bail instantly anyway.
    if crate::jit::is_jit_bail_listed(
        cached.declaring_class_id,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        return None;
    }
    // S111r15 — refuse to JIT a method that has a Rust native shadow.
    // Mirrors the equivalent gate in `try_jit_compile_callee` so the
    // caller-method-counter path doesn't bypass natives that the
    // dispatcher path correctly defers to. Concretely: without this
    // check, `Character.toLowerCase(C)C` got JIT-compiled (its JDK
    // bytecode delegates to `(I)I` → `CharacterData.of/toLowerCase`
    // virtual chain), and the resulting machine code returned 0 for
    // most inputs after warm-up, corrupting Spring's
    // `BeanPropertyName.toDashedForm` (`bannerMode` →
    // `r\0\0\0\0\0\0\0-\0\0\0\0\0\0`) and tripping
    // `InvalidConfigurationPropertyNameException` during SportMe boot.
    //
    // bytebuddy_probe / ANTLR cold-path follow-up: reject an override on this
    // upgrade path only when its body invokes a native-shadowed target such as
    // `Object.equals`. A bytecode override that merely has an identity native
    // somewhere up its ancestor chain is allowed to compile.
    {
        if registered_native_will_run(
            shared,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        ) {
            if crate::runtime::env_cache::dbg_bblp()
                && cached.class_name.contains("LazyProjection")
                && &*cached.method_name == "equals"
            {
                eprintln!(
                    "[BBLP-upgrade] direct-native skip {}.{}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                );
            }
            return None;
        }
        let tdigest_numeric_kernel = &*cached.class_name == "org/elasticsearch/tdigest/Dist"
            && matches!(
                (&*cached.method_name, &*cached.method_descriptor),
                ("quantile", "(DILjava/util/function/Function;)D")
                    | ("cdf", "(DILjava/util/function/Function;)D")
            );
        if !tdigest_numeric_kernel
            && jit_method_calls_native_shadowed(
                shared,
                cached.declaring_class_id,
                &cached.code,
                cached.code.len().saturating_sub(2),
            )
        {
            if crate::runtime::env_cache::dbg_bblp()
                && cached.class_name.contains("LazyProjection")
                && &*cached.method_name == "equals"
            {
                eprintln!(
                    "[BBLP-upgrade] inner-native skip {}.{}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                );
            }
            // PERF FIX (2026-07-15, companion to the invoke-cache fix above):
            // this verdict is a pure function of the method's bytecode (which
            // native-shadowed targets it calls never changes for a given
            // class+method+descriptor, mirroring the `any_class_redefined()`
            // coarse-invalidation already relied on by the `mark_jit_bail_listed`
            // call in `jit::try_compile` — see the "RBC.4" comment at the top
            // of this function). Without memoizing it here, a hot method that
            // calls ANY native-shadowed target (extremely common — e.g. one
            // that calls `Object.equals`/`String` methods internally) re-runs
            // this full O(method-bytecode-size) decode-and-scan
            // (`jit_method_calls_native_shadowed`) from scratch every
            // `JIT_RETRY_STRIDE` (64) invocations, forever, for the lifetime
            // of the process — the exact "tens of thousands of full gate
            // evaluations per suite run" cost pattern RBC.4 fixed for
            // scan-rejected methods, just via a different gate that wasn't
            // wired into the same short-circuit. Mark it bail-listed so the
            // early `is_jit_bail_listed` check at the top of this function
            // short-circuits every future retry.
            crate::jit::mark_jit_bail_listed(
                cached.declaring_class_id,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
            return None;
        }
        if crate::runtime::env_cache::dbg_bblp()
            && cached.class_name.contains("LazyProjection")
            && &*cached.method_name == "equals"
        {
            eprintln!(
                "[BBLP-upgrade] no native shadow, COMPILING {}.{}{}",
                cached.class_name, cached.method_name, cached.method_descriptor
            );
        }
    }
    // W2-CHM: honor the JIT skip list on this caller-method-counter
    // promotion path too. Previously only the first-call compile path
    // (interpreter.rs::~1112) and the callee-dispatcher path
    // (try_jit_compile_callee) consulted `should_skip_jit`; promotions
    // triggered by the caller's invocation count silently bypassed the
    // list and JIT'd skip-listed methods (notably `Integer.valueOf` /
    // `Integer.<init>`) anyway, defeating the W2-CHM box-method ban.
    // Reproducer: `apps/chm_basic/ChmScale` lost entries `k992..k999`
    // even after `is_known_miscompile` listed `Integer.valueOf` because
    // ChmScale's `main` outer-frame loops crossed the per-callee
    // invocation threshold (2000) and re-promoted `Integer.valueOf`
    // here.
    {
        // The static JIT ban list was deleted 2026-07-31 (see
        // docs/known-issues/jit-bans/jit-bans-all-disabled-20260731.md).
        // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
        // remaining force-interpret lever, applied in `jit::try_compile`.
        // GPU-offload JIT admission gate — see offload_jit_gate: a
        // promoted caller containing an offload-eligible invokestatic
        // would bypass the interpreter offload hook.
        #[cfg(feature = "gpu-offload")]
        if crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
            shared,
            cached.declaring_class_id,
            &cached.method_name,
            &cached.method_descriptor,
        ) {
            return None;
        }
    }
    // Check shared JIT cache first
    {
        let jit_cache = shared.jit.jit_cache.read();
        if let Some(compiled) = jit_cache.get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        ) {
            let ret = cached.return_tag();
            let heap = compiled.needs_heap();
            return Some(CachedInvokeTarget::Jit {
                compiled: compiled.into(),
                num_params: cached.num_params,
                return_type: ret,
                needs_heap: heap,
                cached: cached.clone(),
                gate,
                supersede_epoch: crate::classloading::jit_supersede_epoch(),
            });
        }
    }

    // Try to compile — build CP resolvers for multianewarray and field access
    // The assembly, which this door no longer owns. Its gates above are the
    // policy half; `compile_optimizing_artifact` is the half another door can
    // reuse without inheriting them. See that function for why the split is
    // not cosmetic.
    let mut compiled = compile_optimizing_artifact(shared, cached)?;
    let ret = cached.return_tag();
    let heap = compiled.needs_heap();

    // Store in shared JIT cache
    stamp_compilation_epoch(
        shared,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        &mut compiled,
    );
    // Stamp the wrapped-entry requirement before the body is shared.
    // See `CompiledMethod::requires_wrapped_entry`: publication is the
    // last point that still knows this is an `ACC_SYNCHRONIZED` method,
    // and every unwrapped consumer downstream holds only a raw entry
    // pointer.
    compiled.requires_wrapped_entry = cached.is_synchronized;
    let compiled_arc = {
        let mut jit_cache = shared.jit.jit_cache.write();
        jit_cache.put(
            cached.class_name.clone(),
            cached.method_name.clone(),
            cached.method_descriptor.clone(),
            cached.declaring_class_id,
            compiled,
        );
        jit_cache.get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        )?
    };
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] upgrade-OK {}.{}{} entry={:p} len={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            compiled_arc.entry_ptr(),
            compiled_arc.code_bytes().len()
        );
    }
    crate::jit::disasm::maybe_dump(
        if compiled_arc.used_ir_backend { "upgrade/ir" } else { "upgrade/sp" },
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        compiled_arc.entry_ptr(),
        compiled_arc.code_bytes(),
    );

    Some(CachedInvokeTarget::Jit {
        compiled: compiled_arc.into(),
        num_params: cached.num_params,
        return_type: ret,
        needs_heap: heap,
        cached: cached.clone(),
        gate,
        supersede_epoch: crate::classloading::jit_supersede_epoch(),
    })
}

/// RFJP.1 — the RETIRED JIT correctness workaround for
/// `RecursiveTask<Long>.compute()`, now OFF by default.
///
/// # What it was
///
/// Methods on a class transitively extending `java/util/concurrent/ForkJoinTask`
/// were force-interpreted, because a JIT'd `compute()` returned 0 once the
/// recursion depth reached ~10+. That was root-caused not to regalloc but to a
/// `Long.valueOf` boxing miscompile, and fixed in Session 108
/// (commit 6f605451d, "RFJP.1 closed as side-effect"). The blocklist was never
/// taken back out.
///
/// # Why leaving it in was expensive
///
/// Its own comment claimed it was "narrow enough to leave ... CompletableFuture
/// paths JIT-eligible because they don't extend `ForkJoinTask` directly in the
/// hot path". That claim was FALSE: `CompletableFuture$UniCompose` and
/// `$UniRelay` extend `Completion`, which extends `ForkJoinTask`, and they ARE
/// the composition hot path — 199 476 and 119 668 invocations on
/// `HibfixComposeProbe2`, every one of them refused here. The whole completion
/// machinery ran interpreted, at 1.57x on that probe.
///
/// # What retired it (2026-08-27)
///
/// `probes/FjpStress.java` sums a `long[200000]` to depth 17 through all three
/// FJP bases the blocklist names — `RecursiveTask<Long>` (the boxed-return
/// shape RFJP.1 was recorded against), `RecursiveAction` and
/// `CountedCompleter` — 20 rounds each, at C1 and at C2, with
/// `compute()` WITNESSED compiled (`CRATONVM_DBG=jit-method-stats` moves
/// `FjpDeepSum$SumTask.compute` from `3 145 652 invocations compile-failed` to
/// compiled with `hot_but_stuck_in_interpreter=0`). Plus `FjpProbe` at depth 10
/// and `FjpDeepSum` at depth 17. See
/// `completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`.
///
/// Returns `true` only when `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1` puts the
/// workaround back AND the named class transitively extends
/// `java/util/concurrent/ForkJoinTask`.
pub fn is_fjp_subclass_blocklisted(
    shared: &SharedVm,
    class_name: &str,
    requesting_class_id: Option<ClassId>,
) -> bool {
    // ROLLBACK LEVER — `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1`, default OFF.
    //
    // Everything below this line is dead on the shipped configuration. It is
    // kept, rather than deleted, so a regression that the FjpStress/FjpProbe/
    // FjpDeepSum evidence did not cover can be bisected against the old
    // behaviour in one environment variable instead of one revert.
    if !crate::runtime::env_cache::jit_fjp_subclass_blocklist() {
        return false;
    }
    // Cheap exact-name fast path — the JDK classes themselves are always
    // affected by the same regalloc shape if they ever get to JIT.
    if class_name == "java/util/concurrent/ForkJoinTask"
        || class_name == "java/util/concurrent/RecursiveTask"
        || class_name == "java/util/concurrent/RecursiveAction"
        || class_name == "java/util/concurrent/CountedCompleter"
    {
        return true;
    }
    let cm = shared.classes.class_manager.read();
    let start_cid = requesting_class_id
        .and_then(|requester| cm.find_class_by_name_for_class(class_name, requester))
        .or_else(|| cm.find_unique_class_by_name(class_name));
    let Some(start_cid) = start_cid else {
        return false;
    };
    let mut cid = start_cid;
    // Bound the walk so a corrupt/circular hierarchy can't loop forever.
    for _ in 0..64 {
        let Some(class) = cm.get_class(cid) else {
            return false;
        };
        if &*class.name == "java/util/concurrent/ForkJoinTask" {
            return true;
        }
        match class.superclass {
            Some(parent_id) => cid = parent_id,
            None => return false,
        }
    }
    false
}

/// Negative-result cache for [`try_jit_compile_callee`]: direct-mapped table
/// of 64-bit FNV-1a fingerprints of `(class, method, descriptor)` triples
/// whose compile attempt returned `None`.
///
/// WS1 (kafka JIT throughput): the JIT dispatch helpers
/// (`jit_invoke_virtual_mic` et al.) call `try_jit_compile_callee` on every
/// dispatch whose MIC has no compiled entry. For a callee that can never
/// compile — native-shadowed (`HashMap.get`, reflection), skip-listed,
/// FJP-blocklisted, or a backend bail — every such call re-paid the full
/// pipeline: two class-hierarchy walks with string-keyed native lookups per
/// level, `find_method_recursive`, a full `padded_bytecode` copy of the
/// method body, and a compile attempt. On call-heavy code this made JIT'd
/// dispatch dramatically slower than the interpreter. One fingerprint probe
/// replaces all of it.
///
/// Safety of staleness/collisions: a wrong "negative" answer only means the
/// callee is invoked through `invoke_or_native` (interpreted) instead of a
/// compiled entry — always correct, just slower. Recovery paths: the JIT
/// cache is probed BEFORE this table (so a callee compiled later through the
/// interpreter-upgrade or OSR path is picked up immediately), and a cached
/// negative is re-verified every [`CALLEE_NEG_REPROBE_MASK`]+1'th hit.
pub(super) const CALLEE_NEG_CACHE_SLOTS: usize = 1 << 15;
pub(super) static CALLEE_NEG_CACHE: [std::sync::atomic::AtomicU64; CALLEE_NEG_CACHE_SLOTS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; CALLEE_NEG_CACHE_SLOTS];
/// Counter of negative-cache hits, used to periodically re-run the full
/// pipeline so a stale negative (e.g. a transient compile failure that
/// would succeed now) cannot pin a hot callee to the interpreter forever.
pub(super) static CALLEE_NEG_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(super) const CALLEE_NEG_REPROBE_MASK: u64 = 0xFFF;

pub(super) fn callee_neg_fingerprint(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for part in [class_name, method_name, descriptor] {
        for &b in part.as_bytes() {
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            h = (h ^ b as u64).wrapping_mul(FNV_PRIME);
        }
        // Separator so ("AB","C") and ("A","BC") fingerprint differently.
        h = (h ^ 0xff).wrapping_mul(FNV_PRIME);
    }
    // 0 is the table's "empty slot" sentinel.
    if h == 0 {
        1
    } else {
        h
    }
}

/// Compile a callee method by name, storing it in the JIT cache.
/// Called from `jit_invoke_dispatch` when a callee becomes hot.
/// Returns (entry_ptr, needs_context) on success.
///
/// wire-tiered-manager Step 3 — `optimize` selects the backend: inline
/// JIT-dispatch callers pass `true` (the optimizing C2-equivalent backend, the
/// historical behaviour); the background tiered compile worker passes the
/// C1/C2 value derived from the task's target tier
/// ([`crate::jit::tiered::tier_uses_optimized_backend`]). `false` routes to the
/// fast single-pass C1 backend. NOTE: the JIT-cache probe below returns any
/// already-published body regardless of `optimize`, so a method is compiled at
/// whatever tier reaches it *first* — there is no C1→C2 re-compile/supersede yet
/// (that needs safe code-cache replacement; tracked as a follow-up).
///
/// # Why this returns the artifact and not just its entry address
///
/// [`JitCache::put`] REPLACES the body stored under a key. The superseded
/// artifact's last `Arc` therefore drops, and `ExecutableBuffer::drop` unmaps
/// its code. A caller holding only `entry_ptr()` is racing exactly that: every
/// caller here either CALLs the address, publishes it into an inline cache, or
/// bakes it into generated code, and all three keep using it long after this
/// function returns. Handing back the `Arc` makes the address valid for as long
/// as the caller keeps the binding alive — and, because
/// `resolve_jit_entry_owner` can only succeed while the artifact lives, it is
/// also what lets the inline-cache publications inside that window take their
/// own keep-alive.
pub fn try_jit_compile_callee(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    // The mutator-thread compile door, with its panics contained the way the
    // background workers' are (`cratonvm_jit::tiered::contain_compile_panic`).
    // Without this a codegen panic unwound through the interpreter frame that
    // asked for the callee.
    match cratonvm_jit::tiered::contain_compile_panic(|| {
        try_jit_compile_callee_uncontained(shared, class_name, method_name, descriptor, optimize)
    }) {
        Ok(compiled) => compiled,
        Err(payload) => {
            // The verdict store is keyed per loaded class; resolve the name the
            // way the probe in `try_jit_compile_callee_uncontained` does. Only
            // on this (rare) panic arm, with no class-manager guard held.
            let class_id = shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(class_name)
                .unwrap_or(ClassId::new(0));
            note_contained_mutator_compile_panic(
                class_id,
                class_name,
                method_name,
                descriptor,
                &*payload,
            );
            None
        }
    }
}

/// A compile panic caught on a mutator thread: bail-list the method so no door
/// asks for it again, count it under the workers' `worker_panic` scheduling
/// event, and warn once per process.
fn note_contained_mutator_compile_panic(
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    payload: &(dyn std::any::Any + Send),
) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    cratonvm_jit::mark_jit_bail_listed(class_id, class_name, method_name, descriptor);
    cratonvm_jit::metrics::record_scheduling_event(cratonvm_jit::metrics::SCHEDULING_EVENTS[6]);
    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            target: "cratonvm::jit",
            "compile of {class_name}.{method_name}{descriptor} panicked on a mutator thread and \
             was contained: {}. The method is bail-listed and keeps running interpreted; later \
             contained panics are counted under the `worker_panic` scheduling event.",
            cratonvm_jit::tiered::panic_payload_message(payload),
        );
    }
}

/// [`try_jit_compile_callee`] without panic containment.
fn try_jit_compile_callee_uncontained(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    use std::sync::atomic::Ordering;
    // Kill-switch: CRATONVM_DISABLE_JIT=1 forces interpreter-only execution.
    // Useful for bisecting JIT-vs-interpreter bugs during bootstrap crashes.
    if crate::runtime::env_cache::disable_jit() {
        return None;
    }
    // `try_jit_compile_callee` is `&str`-only (called from both the
    // interpreter, which has a precise `ClassId`, and raw JIT-ABI
    // dispatch helpers keyed only by `JitInvokeInfo`'s static strings —
    // see `JitKey::declaring_class_id`'s doc comment). Resolving the
    // class globally by name here preserves this function's existing
    // (pre-existing, not loader-aware) probe/publish behavior; it does
    // not newly introduce the multi-loader-same-name collision this
    // session fixed at the interpreter's own dispatch-side cache
    // consultation (`execute_invoke_kind` / `execute_invokestatic_cached`
    // / `try_jit_upgrade_with_gate`), which is what a same-named class
    // loaded by a user `ClassLoader` actually dispatches through.
    //
    // Resolved once, up front, because the bail-list below is kept per loaded
    // class too. `try_jit_compile_callee_slow` records its verdicts under the
    // class the name resolves to uniquely, and whenever such a class exists
    // this lookup names the same one.
    let probe_class_id = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(class_name)
        .unwrap_or(ClassId::new(0));
    // RBC.4 — short-circuit permanently-uncompilable methods before the
    // FJP/native-shadow hierarchy walks (see try_jit_upgrade_with_gate).
    if crate::jit::is_jit_bail_listed(probe_class_id, class_name, method_name, descriptor) {
        if callee_probe_dbg() {
            // Carry the recorded refusal site into the tally key: "bail-listed"
            // alone says only that some earlier compile said no, and the
            // whole point of the tally is to name what has to be fixed.
            let why = cratonvm_jit::jit_bail_reason_for(
                probe_class_id,
                class_name,
                method_name,
                descriptor,
            )
            .unwrap_or_else(|| "reason-not-recorded".to_string());
            callee_probe_note(
                &format!("BAIL-LISTED[{why}]"),
                class_name,
                method_name,
                descriptor,
            );
        }
        return None;
    }
    // This API hands a raw entry pointer to direct dispatchers. Even if the
    // method has already been compiled for the ordinary interpreter entry,
    // those call sites have no implicit ACC_SYNCHRONIZED monitor wrapper.
    // Keep them on the generic invocation path; background tiering uses the
    // separate wrapped-entry helper below.
    if named_method_is_synchronized(shared, class_name, method_name, descriptor) {
        if callee_probe_dbg() {
            callee_probe_note("SYNCHRONIZED", class_name, method_name, descriptor);
        }
        return None;
    }
    // JIT-cache probe. Deliberately BEFORE the negative cache so a method
    // compiled later through another path (interpreter invocation-count
    // upgrade, OSR) is returned even when an earlier attempt through this
    // function negative-cached. `JitCache::get` takes `&str` directly —
    // the previous `Arc::from` per name was three wasted heap allocations
    // on every dispatch-helper call.
    {
        // `probe_class_id` was resolved at the top of this function.
        // No `class_was_redefined` gate: `redefine_class` calls
        // `jit_cache.clear_all()`, so any entry still present in the cache was
        // necessarily compiled AFTER the most recent redefinition of any
        // class. Refusing the lookup because the class was once redefined
        // permanently withheld compiled code from that class -- it is what
        // kept a mocked class interpreted forever.
        let jit_cache = shared.jit.jit_cache.read();
        if let Some(compiled) = jit_cache.get(class_name, method_name, descriptor, probe_class_id) {
            // A published body is not automatically a body THIS caller may
            // enter. `try_jit_compile_callee` is the by-name entry point for
            // the UNWRAPPED direct-call doors, and its slow path refuses an
            // `ACC_SYNCHRONIZED` callee for a reason that does not stop being
            // true once the body already exists: the compiled code carries no
            // monitor prologue, so a raw CALL to it simply does not lock.
            //
            // The background tiering door publishes synchronized bodies on
            // purpose — `try_jit_compile_wrapped_entry` — because
            // `execute_jit_call` wraps them. Serving one from here handed the
            // wrapped-entry body to a caller that supplies no monitor, which is
            // how `RSyncMethodJit`'s `static synchronized bumpStatic` lost
            // ~35 of 240 000 increments per run.
            if compiled.requires_wrapped_entry {
                cratonvm_jit::note_direct_callee_bind_refusal(
                    cratonvm_jit::DirectBindRefusal::Synchronized,
                );
                return None;
            }
            // Cast: object/code pointer to integer address
            let entry = compiled.entry_ptr() as usize; // Cast: JIT entry point to address
            let needs_ctx = compiled.needs_context();
            return Some((compiled, entry, needs_ctx));
        }
        // DIAG (`CRATONVM_DBG_CALLEE_PROBE=1`): the probe above requires an
        // EXACT `declaring_class_id`. Report the miss UNCONDITIONALLY, with the
        // exact strings — an empty `ids` list is as informative as a populated
        // one (it separates "wrong id" from "the name never matches at all").
        if callee_probe_dbg() {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n < 25 {
                let ids = jit_cache.debug_ids_for(class_name, method_name, descriptor);
                eprintln!(
                    "[callee-probe] CACHE-MISS class={class_name:?} method={method_name:?} \
                     desc={descriptor:?} probe_id={} ids_in_cache={:?}",
                    probe_class_id.as_u32(),
                    ids
                );
            }
            callee_probe_tally("CACHE-MISS", class_name, method_name);
        }
    }
    let fp = callee_neg_fingerprint(class_name, method_name, descriptor);
    // Widening: small integer index -> usize (non-negative, fits in pointer width)
    let slot = &CALLEE_NEG_CACHE[(fp as usize) & (CALLEE_NEG_CACHE_SLOTS - 1)];
    if slot.load(Ordering::Relaxed) == fp {
        let n = CALLEE_NEG_HITS.fetch_add(1, Ordering::Relaxed);
        if n & CALLEE_NEG_REPROBE_MASK != 0 {
            callee_probe_tally("NEG-CACHE", class_name, method_name);
            return None;
        }
        // Periodic re-probe: fall through and re-run the full pipeline.
    }
    let mut cache_negative = true;
    let res = try_jit_compile_callee_slow(
        shared,
        class_name,
        method_name,
        descriptor,
        optimize,
        &mut cache_negative,
        false,
    );
    if res.is_none() && callee_probe_dbg() {
        callee_probe_note("SLOW-PATH-NONE", class_name, method_name, descriptor);
    }
    match res {
        None if cache_negative => slot.store(fp, Ordering::Relaxed),
        // A re-probe that succeeded — drop the stale negative entry.
        Some(_) if slot.load(Ordering::Relaxed) == fp => slot.store(0, Ordering::Relaxed),
        _ => {}
    }
    res
}

/// Is the callee-probe diagnostic on? (`CRATONVM_DBG_CALLEE_PROBE=1`)
///
/// `try_jit_compile_callee` has four independent ways to answer `None`, and
/// they were indistinguishable from the outside — which is what made
/// "`hit_entry=0` forever" unfalsifiable. Each now names itself.
fn callee_probe_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_CALLEE_PROBE"))
}

fn callee_probe_note(why: &str, class_name: &str, method_name: &str, descriptor: &str) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 25 {
        eprintln!("[callee-probe] {why} {class_name}.{method_name}{descriptor}");
    }
    callee_probe_tally(why, class_name, method_name);
}

/// Per-`(reason, callee)` tally behind the same flag as [`callee_probe_note`].
///
/// The first-25 sample above is spent entirely on class loading before the
/// workload's own code runs, so it cannot answer the question the counter
/// `pub_probe_none` raises — *which* callees are refused, and for which of the
/// four reasons. Bounded to [`CALLEE_PROBE_TALLY_CAP`] distinct keys so a
/// pathological run cannot grow it without limit.
fn callee_probe_tally(why: &str, class_name: &str, method_name: &str) {
    if !callee_probe_dbg() {
        return;
    }
    const CALLEE_PROBE_TALLY_CAP: usize = 4096;
    let tally = CALLEE_PROBE_TALLY
        .get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let mut guard = tally.lock();
    let key = format!("{why} {class_name}.{method_name}");
    if guard.len() >= CALLEE_PROBE_TALLY_CAP && !guard.contains_key(&key) {
        return;
    }
    *guard.entry(key).or_insert(0) += 1;
}

#[allow(clippy::type_complexity)]
static CALLEE_PROBE_TALLY: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::OnceLock::new();

/// Report a callee that only compiles because the generic-metadata scan reads
/// the DECLARING class's constant pool (see the call site in
/// `try_jit_compile_callee_slow`). Each distinct callee is printed ONCE, the
/// first time it is admitted.
///
/// Printed eagerly rather than accumulated into the tally above for two
/// reasons: the tally's dump is truncated to its 30 hottest rows, and the run
/// this exists to diagnose is one that fails -- possibly by aborting before any
/// end-of-run dump would happen. The line format is `class.method` first so the
/// output feeds `CRATONVM_JIT_DENY` (a substring match on `class.method`)
/// directly.
fn note_newly_admitted_callee(class_name: &str, method_name: &str, descriptor: &str) {
    const NEWLY_ADMITTED_CAP: usize = 8192;
    static SEEN: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()));
    let key = format!("{class_name}.{method_name}");
    let mut guard = seen.lock();
    if guard.len() >= NEWLY_ADMITTED_CAP || !guard.insert(key.clone()) {
        return;
    }
    eprintln!("[callee-probe] NEWLY-ADMITTED {key} {descriptor}");
}

/// Dump the [`callee_probe_tally`] histogram, hottest first. Called from the
/// `mic-prof` dump so one run answers both "how often does the inline cache
/// fail to hold an entry" and "for which callees, and why".
pub(crate) fn dump_callee_probe_tally() {
    if !callee_probe_dbg() {
        return;
    }
    let Some(tally) = CALLEE_PROBE_TALLY.get() else {
        return;
    };
    let mut rows: Vec<(String, u64)> = tally.lock().iter().map(|(k, v)| (k.clone(), *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!("[callee-probe] tally ({} distinct keys)", rows.len());
    for (key, count) in rows.iter().take(30) {
        eprintln!("[callee-probe]   {count:>10} {key}");
    }
}

pub(super) fn named_method_is_synchronized(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let cm = shared.classes.class_manager.read();
    let Some(class_id) = cm.get_loaded_class_id(class_name) else {
        return false;
    };
    let store = cm.class_store();
    crate::classloading::find_method_recursive(class_id, method_name, descriptor, store)
        .is_some_and(|(method, _)| method.is_synchronized())
}

/// Background tiering publishes a body for `execute_jit_call`, whose wrapper
/// owns the implicit synchronized-method monitor. It must not be used by raw
/// direct-call sites; [`try_jit_compile_callee`] deliberately rejects those.
pub(super) fn try_jit_compile_wrapped_entry(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    // `named_class_was_redefined` was OR'd in here and, being permanent,
    // stopped a redefined class from ever supplying a compiled callee again.
    // Compilation reads the current (agent-woven) bytecode out of the class
    // store, and the previous artifacts were evicted by the redefinition, so
    // compiling now yields code for the body that is actually installed.
    if crate::runtime::env_cache::disable_jit() {
        return None;
    }
    let mut cache_negative = false;
    try_jit_compile_callee_slow(
        shared,
        class_name,
        method_name,
        descriptor,
        optimize,
        &mut cache_negative,
        true,
    )
}

/// The full (slow) compile pipeline behind [`try_jit_compile_callee`].
/// Sets `*cache_negative = false` when a `None` return is for a reason that
/// may change soon (currently: receiver class not loaded yet), so the caller
/// does not negative-cache it.
///
/// ## GC-STW-safety / VM-lock discipline (wire-tiered-manager increment 3)
///
/// This function runs both on the mutator (inline JIT-dispatch helpers) AND,
/// when `CRATONVM_BG_COMPILE` is on, on the GC-neutral `cratonvm-jit-compiler`
/// worker via [`background_compile_task`]. The worker is an unregistered
/// `std::thread::Builder` daemon (the G1-MarkComplete precedent): the STW
/// barrier never waits for it, so concurrent relocation is harmless to it
/// PROVIDED it holds no VM lock across a blocking op. The fatal failure mode is
/// indirect: a mutator wanting `class_manager.write()` (class definition) that
/// blocks behind a read lock the worker is holding across a wait can no longer
/// reach its safepoint, so a STW initiated by a third thread (whose `expected`
/// count includes that blocked mutator) never completes.
///
/// Lock order and bounded scopes (each VM lock acquire -> read out what is
/// needed -> DROP -> proceed; never two held simultaneously, never one held
/// across a blocking call / nested VM-lock acquisition / managed allocation):
///   1. `class_manager.read()` (`cm`) — bytecode/method metadata extraction
///      only; explicitly `drop(cm)` BEFORE `jit::try_compile`. The constant-pool
///      resolver closures handed to `try_compile` re-acquire `class_manager`
///      read locks TRANSIENTLY, each scoped to a single CP lookup and dropped at
///      closure return. The one resolver that can BLOCK or take
///      `class_manager.write()` — `resolve_field_ref` -> `load_class_concurrent`
///      (per-class-name condvar wait, write-lock class load with <clinit>/GC) —
///      is invoked by `field_resolver`/`static_field_resolver` BEFORE those
///      closures take their own `cm` read, so no VM read lock is alive across
///      that blocking/allocating call.
///   2. `flight_recorder.lock()` — held only for the single
///      `emit_compilation_event_arc` call; description + timestamp built first.
///   3. `jit_cache.write()` — held only for the publishing `put`; the key Arc
///      is built first. This is the cross-thread publish: a mutator's
///      `jit_cache` fast-path flips the call site to `Jit` on its next call.
/// No two of {class_manager, flight_recorder, jit_cache} are ever held at once.
/// GC itself takes none of these during STW (it scans deposited root snapshots),
/// so the worker's transient holds only matter via the mutator-stall path above.
#[allow(clippy::type_complexity)]
/// Deepest nested-compile stack at which the callee resolver will still compile a
/// not-yet-compiled statically bound callee in order to bind it directly
/// (`eager-callee-chain`).
///
/// Six levels covers the chains that matter in practice — a JUnit `assertEquals`
/// reaches `AssertionUtils.objectsAreEqual` in three, a `java.nio` absolute
/// accessor reaches its `ScopedMemoryAccess` leaf in three — while bounding the
/// worst-case native stack use and the worst-case latency of one tier-up. Past the
/// bound a site keeps the checked dispatch helper, which is always correct.
const MAX_EAGER_CALLEE_CHAIN_DEPTH: usize = 6;

/// Transitive callee compiles allowed per TOP-LEVEL compile.
///
/// The depth bound alone does not bound fan-out: a method with forty statically
/// bound sites, each of whose callees has forty of its own, would turn one tier-up
/// into thousands of compiles and a visible pause. This caps the total. It is
/// deliberately generous — the chains this exists for are a handful of methods deep
/// and a handful wide — so an ordinary method never reaches it.
const MAX_EAGER_CALLEE_CHAIN_PER_COMPILE: u32 = 96;

thread_local! {
    /// Transitive callee compiles spent under the current top-level compile. Reset
    /// by [`eager_callee_chain_enter_top_level`], which every entry to
    /// [`try_jit_compile_callee_slow`] calls; only the entry that finds no compile
    /// open on this thread actually clears it.
    static EAGER_CALLEE_CHAIN_SPENT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Start a fresh `eager-callee-chain` budget if this is a top-level compile.
///
/// "Top level" is `jit_active_compile_depth() == 0`: the jit crate pushes its
/// compile-stack entry inside `try_compile`, which this function has not called
/// yet, so a nested (callee) entry always sees a non-zero depth here and keeps the
/// outer budget.
fn eager_callee_chain_enter_top_level() {
    if cratonvm_jit::jit_active_compile_depth() == 0 {
        EAGER_CALLEE_CHAIN_SPENT.with(|c| c.set(0));
    }
}

/// Charge one transitive callee compile against the current budget. `false` means
/// the budget is spent and the site must keep dispatch.
fn eager_callee_chain_try_spend() -> bool {
    EAGER_CALLEE_CHAIN_SPENT.with(|c| {
        let spent = c.get();
        if spent >= MAX_EAGER_CALLEE_CHAIN_PER_COMPILE {
            false
        } else {
            c.set(spent + 1);
            true
        }
    })
}

pub(super) fn try_jit_compile_callee_slow(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    // wire-tiered-manager Step 3: `false` compiles this method with the fast
    // single-pass C1 backend (no IR pipeline); `true` uses the optimizing C2
    // backend. Threaded into `jit::try_compile`'s trailing flag.
    optimize: bool,
    cache_negative: &mut bool,
    allow_synchronized_wrapped_entry: bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    // `eager-callee-chain`: a top-level compile starts with a fresh transitive
    // callee-compile budget; a nested one inherits the outer compile's.
    eager_callee_chain_enter_top_level();
    // Compile verdicts are kept per loaded class. The two refusals below fire
    // before this function resolves the receiver class, so they look its
    // identity up only when they fire, with no class-manager guard held. It
    // is the same unique resolution `callee_class_id` gets further down.
    let receiver_class_id_for_verdicts = || {
        shared
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name(class_name)
            .unwrap_or(ClassId::new(0))
    };
    // RFJP.1 (RETIRED, lever-only) — refuse a method whose declaring class
    // transitively extends `java/util/concurrent/ForkJoinTask`. Inert unless
    // `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1`. This is the exit that reported
    // `reason=vm-fjp-subclass-blocklisted` for both `CompletableFuture$
    // UniCompose.tryFire` and `$UniRelay.tryFire`.
    if is_fjp_subclass_blocklisted(shared, class_name, None) {
        cratonvm_jit::record_compile_refusal(
            receiver_class_id_for_verdicts(),
            class_name,
            method_name,
            descriptor,
            "vm-fjp-subclass-blocklisted",
        );
        return None;
    }
    // FJP fix (CORRECTED): refuse to compile a method only when the method that
    // would ACTUALLY RUN for this receiver is natively shadowed — compiling its
    // bytecode would bypass the native (e.g. `ForkJoinTask.fork()`'s Unsafe-CAS
    // body). Two such cases:
    //   (1) the receiver's OWN class has a native for (method, desc) — checked
    //       here, before resolution;
    //   (2) the method is INHERITED from a class that has the native (e.g. a
    //       `ForkJoinTask` subclass that does not override `fork()`) — checked at
    //       the resolved DECLARING class just after `find_method_recursive` below.
    //
    // The previous implementation walked the ENTIRE ancestor chain and refused
    // whenever ANY ancestor had a native of the same signature. That wrongly
    // refused every bytecode OVERRIDE of a method that is native on `Object`
    // (`hashCode`/`equals`/`toString`/`clone`): a real override shadows the
    // ancestor native, so `find_method_recursive` stops at the override and it
    // IS compilable. The bug made hashCode/equals-heavy code run interpreted —
    // e.g. ANTLR's `ParserATNSimulator` ATN simulation (PredictionContext/
    // ATNConfig/DFAState `hashCode` are ~58% of the Groovy-parse profile) never
    // compiled, ~100x slower than HotSpot (Spring Boot buildSrc
    // `SpringRepositoriesExtensionTests` hang).
    if registered_native_will_run(shared, class_name, method_name, descriptor) {
        cratonvm_jit::record_compile_refusal(
            receiver_class_id_for_verdicts(),
            class_name,
            method_name,
            descriptor,
            "vm-callee-is-registered-native",
        );
        return None;
    }
    // Look up the method bytecode
    let cm = shared.classes.class_manager.read();
    let callee_class_id = match cm.find_unique_class_by_name(class_name) {
        Some(id) => id,
        None => {
            // The receiver's class may simply not be loaded yet — a later
            // attempt can succeed, so this `None` must not be cached.
            *cache_negative = false;
            // No loaded class to key the reason by: `ClassId(0)`.
            cratonvm_jit::record_compile_refusal(
                ClassId::new(0),
                class_name,
                method_name,
                descriptor,
                "vm-declaring-class-not-loaded",
            );
            return None;
        }
    };
    let store = cm.class_store();
    let (method, declaring_id) = crate::classloading::find_method_recursive(
        callee_class_id,
        method_name,
        descriptor,
        store,
    )?;
    // Direct dispatcher compilation also bypasses interpreter frame creation.
    if method.is_synchronized() && !allow_synchronized_wrapped_entry {
        cratonvm_jit::record_compile_refusal(
            callee_class_id,
            class_name,
            method_name,
            descriptor,
            "vm-synchronized-no-wrapped-entry",
        );
        return None;
    }

    let code_attr = method.code()?;
    // `cm` is intentionally passed through: do not recursively read-lock the
    // class manager while this guard and its borrowed method are alive.
    //
    // The class id here MUST be `declaring_id`, not `callee_class_id`. The scan
    // resolves the constant-pool indices embedded in `code_attr.code`, and
    // those indices only mean anything in the constant pool of the class that
    // DECLARES the method. `callee_class_id` is the RECEIVER's class, which for
    // any inherited method is a different class with a completely unrelated
    // pool: the same index there is a Utf8, a Fieldref, or out of range, so the
    // scan's `_ => return true` arm fired and the method was permanently
    // `mark_jit_bail_listed`ed for a constant it never referenced.
    //
    // The cost of that landed on exactly the code least able to absorb it —
    // framework hierarchies whose hot methods are inherited accessors. On
    // `InPredicateTest`'s 100k-element criteria IN-list, every hot SQM
    // accessor (`SqmTextValuedSimplePath.getReferencedPathSource`,
    // `BasicSqmPathSource.getExpressible`, ...) was bail-listed this way, so
    // `jit_invoke_virtual_mic` reported `hit_entry=0 / pub_probe_none=2228315`
    // — the inline cache NEVER held an entry, and 2.2M dispatches from compiled
    // code each took the entryless helper path instead of a direct call. That
    // made JIT-on ~2x SLOWER than `--nojit` on that test, which is what
    // docs/known-issues/hibernate/hib-inpredicate-*.md has been tracking.
    let scan_refuses = jit_method_calls_forced_class_generic_metadata(
        &cm,
        declaring_id,
        &code_attr.code,
        code_attr.code.len(),
    );
    // DIAG (`CRATONVM_DBG=callee-probe`): name the set of methods the
    // `callee_class_id` -> `declaring_id` fix above newly ADMITS, i.e. every
    // method the buggy argument would have bail-listed and this one compiles.
    //
    // That set is the search space for a miscompile the fix exposes: the fix
    // itself is a trigger, not a cause, so the question "which method does the
    // backend get wrong" has to be answered over the methods whose status it
    // actually changed -- not over the whole program. Diffing the tally across
    // two builds cannot answer it (the dump is truncated to 30 rows, and the
    // two runs load different code), but evaluating BOTH arguments in the one
    // build that exhibits the failure names the delta exactly.
    if callee_probe_dbg() && !scan_refuses && declaring_id != callee_class_id {
        let old_arg_refuses = jit_method_calls_forced_class_generic_metadata(
            &cm,
            callee_class_id,
            &code_attr.code,
            code_attr.code.len(),
        );
        if old_arg_refuses {
            note_newly_admitted_callee(class_name, method_name, descriptor);
        }
    }
    if scan_refuses {
        crate::jit::mark_jit_bail_listed(callee_class_id, class_name, method_name, descriptor);
        cratonvm_jit::record_compile_refusal(
            callee_class_id,
            class_name,
            method_name,
            descriptor,
            "vm-bytecode-scan-refused",
        );
        return None;
    }
    // jit-invokestatic-clinit-gap fix (2026-07-17): third occurrence of the
    // same gap as the `callee_compiler` (compile-time direct_calls) and
    // `resolve_inline_site` (inlining) closures above -- this is the
    // RUNTIME-side counterpart, invoked from `jit_invoke_dispatch`
    // (`vm/src/jit/helpers.rs`) via `try_compile_callee` the first time a
    // generic-dispatch invokestatic call site actually executes. On success
    // this function's `(entry_ptr, needs_context)` gets cached into
    // `jit_cache` and, per `jit_invoke_dispatch`'s own comment two call
    // sites down, invoked via `invoke_or_native`/a direct call -- NOT
    // through `invoke_shared`'s `ensure_class_initialized_shared` call.
    // Same JVMS SS5.5 requirement, same fix: refuse to hand back a compiled
    // entry for a `static` method whose declaring class isn't initialized
    // yet, forcing this call (this one time) through the always-safe
    // `invoke_or_native` fallback that jit_invoke_dispatch uses when this
    // function returns `None`. Monotonic init state makes a compile-time
    // (well, first-dispatch-time) check here sound for the cached entry's
    // entire remaining lifetime, exactly like the other two sites.
    if method.is_static() {
        let declaring_class_initialized = store
            .get(declaring_id)
            .map(crate::vm::is_class_initialized_fast)
            .unwrap_or(false);
        if !declaring_class_initialized {
            // Not yet initialized: don't cache a negative result either --
            // the class may initialize very soon (e.g. the very next
            // dispatch through the safe fallback), at which point this
            // function should succeed and start caching the fast entry.
            *cache_negative = false;
            cratonvm_jit::record_compile_refusal(
                callee_class_id,
                class_name,
                method_name,
                descriptor,
                "vm-declaring-class-not-initialized",
            );
            return None;
        }
    }
    let declaring_class_name = store.get(declaring_id).map(|c| &*c.name)?;
    // FJP fix (CORRECTED) case (2): the resolved method is INHERITED from a
    // class that has a Rust native override (e.g. `ForkJoinTask.fork()` reached
    // through a subclass that does not override it). Compiling its bytecode
    // would bypass the native, so refuse. Checking the DECLARING class (the
    // resolution point) — rather than every ancestor — is precisely what lets
    // bytecode OVERRIDES on the receiver/intermediate classes still compile.
    // (When `declaring_class_name == class_name` the own-class check above
    // already returned None, so this only fires for genuinely inherited natives.)
    if declaring_class_name != class_name
        && shared
            .natives
            .native_methods
            .find(declaring_class_name, method_name, descriptor)
            .is_some()
    {
        cratonvm_jit::record_compile_refusal(
            callee_class_id,
            class_name,
            method_name,
            descriptor,
            "vm-native-override-present",
        );
        return None;
    }
    let source_file = store
        .get(declaring_id)
        .and_then(|c| c.source_file.as_deref())
        .map(Arc::from);
    let num_params = count_method_params(descriptor);
    let padded_code = crate::runtime::frame::padded_bytecode(&code_attr.code);

    let cached = CachedBytecodeMethod {
        declaring_class_id: declaring_id,
        class_name: Arc::from(declaring_class_name),
        method_name: Arc::from(method_name),
        method_descriptor: Arc::from(descriptor),
        source_file,
        code: padded_code,
        exception_table: Arc::from(code_attr.exception_table.as_slice()),
        max_stack: code_attr.max_stack,
        max_locals: code_attr.max_locals,
        num_params: num_params as u16, // Widening: parameter count conversion
        is_synchronized: method.is_synchronized(),
        is_static: method.is_static(),
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    drop(cm);

    // S-HIB.1 — apply the static skip list to callee compilations triggered
    // from JIT helper callbacks (jit_invoke_virtual_mic et al.).  Without this
    // check any method — including complex `<init>`/`<clinit>` bodies with
    // putfield/invokedynamic that the first-call and OSR paths correctly ban —
    // could be compiled via this path, leading to JIT codegen bugs like the
    // `JoinedList.<init>` checkcast-on-primitive crash.
    {
        // The static JIT ban list was deleted 2026-07-31 (see
        // docs/known-issues/jit-bans/jit-bans-all-disabled-20260731.md).
        // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
        // remaining force-interpret lever, applied in `jit::try_compile`.
        // GPU-offload JIT admission gate — see offload_jit_gate.
        #[cfg(feature = "gpu-offload")]
        if crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
            shared,
            cached.declaring_class_id,
            method_name,
            &cached.method_descriptor,
        ) {
            cratonvm_jit::record_compile_refusal(
                callee_class_id,
                class_name,
                method_name,
                descriptor,
                "vm-callee-compile-gate-refused",
            );
            return None;
        }
    }

    // Build resolvers for the callee's constant pool
    let cid = declaring_id;
    let resolver = |cp_idx: u16| -> Option<String> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        class
            .constant_pool
            .get_class_name(cp_idx)
            .map(|s| s.to_string())
    };
    let field_resolver = |cp_idx: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        let field = resolve_field_ref(shared, cid, cp_idx).ok()?;
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        let nat_idx = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::FieldReference {
                name_and_type_index,
                ..
            }) => *name_and_type_index,
            _ => {
                cratonvm_jit::record_compile_refusal(
                    callee_class_id,
                    class_name,
                    method_name,
                    descriptor,
                    "vm-cp-entry-not-a-methodref",
                );
                return None;
            }
        };
        let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        let type_tag = *descriptor.as_bytes().first()?;
        if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
            return None;
        }
        // `None` ⇒ no genuine compact slot for this field (class has no
        // registered `CompactLayout`, or the index falls outside it).
        // Fabricating a `(0, false)` placeholder here poisoned the JIT's
        // compact-offset inline getfield/putfield with a garbage offset: for
        // a reference field it emitted a 32-bit sign-extended load of half a
        // `Value` cell, producing a bogus non-null receiver that SIGSEGVed in
        // the invoke inline cache (WildFly Host Controller `host=foo:add()`,
        // docs/known-issues/wildfly-domain-hostcontroller-sigsegv-*).
        Some((
            field.field_index,
            type_tag,
            cratonvm_types::compact_field_slot(
                field.declaring_class_id.as_u32(),
                field.field_index,
            )
            .map(|(o, r)| (o as u32, r)),
        ))
    };
    let static_field_resolver = |cp_idx: u16| -> Option<(u32, usize, u8, bool)> {
        let field = resolve_field_ref(shared, cid, cp_idx).ok()?;
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        let nat_idx = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::FieldReference {
                name_and_type_index,
                ..
            }) => *name_and_type_index,
            _ => return None,
        };
        let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        let type_tag = *descriptor.as_bytes().first()?;
        if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
            return None;
        }
        Some((
            field.declaring_class_id.as_u32(),
            field.field_index,
            type_tag,
            field.is_volatile,
        ))
    };
    let invoke_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let target_class = class.constant_pool.get_class_name(class_idx)?;
        let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        Some((
            target_class.to_string(),
            method_name.to_string(),
            descriptor.to_string(),
        ))
    };
    // JVMS §6.5 `invokespecial` super-call redirect — see
    // `invokespecial_owner_resolver` above / `try_compile`'s doc comment.
    // `cid` is this callee's own declaring class, i.e. the calling class for
    // every invoke site scanned in its bytecode.
    let invokespecial_owner_resolver = |cp_idx: u16, opcode: u8| -> Option<String> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        let (class_idx, nat_idx, is_iface) = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index, false),
            Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index, true),
            _ => return None,
        };
        let target_class = class.constant_pool.get_class_name(class_idx)?;
        let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        // JVMS 5.4.6 -- an `invokevirtual` (0xb6) that resolves to a PRIVATE
        // method selects exactly that method: no override lookup, no walk up
        // from the receiver. javac emits 0xb6 for a call to a private instance
        // method from Java 11 on (JEP 181 nestmates), where it used to emit
        // `invokespecial` -- so this is now the ordinary encoding of
        // `this.somePrivateHelper()`, including the
        // constructor-calls-its-own-`init()` shape that
        // `io/vertx/core/net/TCPSSLOptions`, `ClientOptionsBase` and
        // `HttpClientOptions` all use, one per level of the same chain.
        //
        // Answering here reclassifies the site as a DIRECT bind at the
        // declaring class, which is what stops the compiled dispatchers
        // resolving it from the receiver and landing on the most-derived
        // same-named private method.
        //
        // Access control makes the answer precise rather than a guess: a
        // private method is invocable only from the class that declares it, so
        // the constant pool's owner name is this compiling class itself and no
        // loader-blind name lookup is in play.
        if opcode == 0xb6 {
            let cp_class_id = cm.find_class_by_name_for_class(target_class, cid)?;
            let store = cm.class_store();
            if let Some(declaring_id) = crate::classloading::invokevirtual_private_declaring_class(
                cp_class_id,
                method_name,
                descriptor,
                store,
            ) {
                return store.get(declaring_id).map(|c| c.name.to_string());
            }
            // Not private, but possibly unoverridable anyway — a `final`
            // method, or any method of a `final` class, has exactly one
            // possible target at this site for the same reason a private one
            // does. Same conclusion (statically bound; substitute the
            // DECLARING class, which for this rule is often NOT the class the
            // constant pool names), reached by a different argument. Both
            // rules live outside this file — the private one in
            // `classloading::invokevirtual_private_declaring_class`, this one
            // in `invoke::invokevirtual_site_final_owner` — so these three
            // per-door copies cannot drift apart on either.
            return super::invoke::invokevirtual_site_final_owner(
                shared,
                &cm,
                cid,
                target_class,
                method_name,
                descriptor,
            );
        }
        if opcode != 0xb7 {
            return None;
        }
        let cp_class_id = cm.find_class_by_name_for_class(target_class, cid)?;
        let store = cm.class_store();
        let start = crate::classloading::invokespecial_selection_start(
            cid,
            cp_class_id,
            is_iface,
            method_name,
            store,
        );
        if start == cp_class_id {
            return None;
        }
        store.get(start).map(|c| c.name.to_string())
    };
    // invokedynamic-uncommon-trap fix: resolves an invokedynamic CP index to
    // its target descriptor (no bootstrap/CallSite resolution needed).
    let indy_descriptor_resolver = |cp_idx: u16| -> Option<(String, usize)> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        match class.constant_pool.get(cp_idx)? {
            ConstantPoolEntry::InvokeDynamic {
                name_and_type_index,
                ..
            } => class
                .constant_pool
                .get_name_and_type(*name_and_type_index)
                .map(|(_name, descriptor)| {
                    (
                        descriptor.to_string(),
                        crate::runtime::invokedynamic::make_jit_indy_bridge_site_from_parts(
                            &class.constant_pool,
                            &class.bootstrap_methods,
                            cp_idx,
                            cid,
                        )
                        .unwrap_or(0),
                    )
                }),
            _ => None,
        }
    };
    let new_resolver = |cp_idx: u16| -> Option<cratonvm_jit::JitNewSite> {
        let cm = shared.classes.class_manager.read();
        resolve_jit_new_site(&cm, cid, cp_idx)
    };
    // PGO-02: receiver class-id -> class-name resolver for a guarded
    // speculative virtual/interface inline plan's SpeculatedReceiver
    // invalidation dependency (plan_inline's fail-closed rule — see
    // docs/feature-designs/profile-guided-inlining.md). `None` (id not
    // loaded, or unloaded between profiling and compiling) refuses
    // that one speculation rather than recording an unmatchable
    // name-less dependency.
    let class_id_namer = |namer_cid: u32| -> Option<String> {
        let cm = shared.classes.class_manager.read();
        cm.get_class(cratonvm_types::ClassId::new(namer_cid))
            .map(|c| c.name.to_string())
    };
    // PGO-02 R0: see the sibling resolver in `try_jit_compile` — the body a
    // receiver of exactly this class id dispatches to, which is the only body a
    // guard admitting that class may splice.
    let receiver_inline_resolver = |rcv_cid: u32, cp_class: &str, name: &str, desc: &str| {
        resolve_receiver_inline_site(
            shared,
            cached.declaring_class_id,
            rcv_cid,
            cp_class,
            name,
            desc,
            // No direct-bind resolver on the guarded-virtual path: the
            // closure that owns it is declared further down this function, and
            // a spliced body reached through a receiver guard is planned before
            // it exists. The consequence is a REFUSAL, never a downgrade — a
            // call-carrying body with nothing to bind to is not admitted at all
            // (see the admission rule in `resolve_inline_site_from`).
            None,
        )
    };
    // Elidable-`<init>` resolver for `new` scalar replacement, default-ON
    // (opt-out: CRATONVM_JIT_SCALAR_NEW=0).
    let scalar_new_on = crate::runtime::env_cache::jit_scalar_new();
    let elidable_init_resolver =
        |cp_idx: u16| -> bool { resolve_jit_elidable_init_loading(shared, cid, cp_idx) };
    // invoke class-id resolver — maps an invoke* CP index to its declared
    // class id, consumed by the CRC32/CRC32C `update` receiver class-id guard.
    let invoke_class_id_resolver = |cp_idx: u16| -> Option<u32> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        let class_idx = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference { class_index, .. }) => *class_index,
            Some(ConstantPoolEntry::InterfaceMethodReference { class_index, .. }) => *class_index,
            _ => return None,
        };
        let target_class = class.constant_pool.get_class_name(class_idx)?;
        Some(cm.find_class_by_name_for_class(target_class, cid)?.as_u32())
    };
    // Review #80: the declaring class of each invoke's resolved method, for the
    // `Atomic*` intrinsics' subclass sites.
    let invoke_declaring_class_resolver = |cp_idx: u16| -> Option<String> {
        cp_method_ref_declaring_class_name(shared, cid, cp_idx)
    };
    let ldc2w_resolver = |cp_idx: u16| -> Option<(i64, bool)> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        // inc 35: `(bits, is_double)`.
        let val = match class.constant_pool.get(cp_idx)? {
            ConstantPoolEntry::Long(v) => Some((*v, false)),
            ConstantPoolEntry::Double(v) => Some((v.to_bits() as i64, true)), // Cast: JIT ABI -- float bits to i64
            _ => None,
        };
        if crate::runtime::env_cache::dbg_jit_ldc() {
            eprintln!(
                "[cratonvm-ldc2w] full idx={} -> {:?} (f64 {})",
                cp_idx,
                val,
                // Cast: integer word reinterpreted as float/double bit pattern
                val.map(|(v, _)| f64::from_bits(v as u64))
                    .unwrap_or(f64::NAN)
            );
        }
        val
    };

    // RBC.2 — `ldc`/`ldc_w` int/float/String/Class constants; see the matching
    // resolver in `try_jit_upgrade_with_gate`.
    let ldc_resolver = |cp_idx: u16| -> Option<cratonvm_jit::JitLdcConstant> {
        let cm = shared.classes.class_manager.read();
        jit_ldc_constant_for(&cm, cid, cp_idx)
    };

    let pgo_profile = {
        let profile_key = crate::jit::profile::MethodKey {
            class_id: cached.declaring_class_id.as_u32(),
            method_name: cached.method_name.clone(),
            descriptor: cached.method_descriptor.clone(),
        };
        shared.jit.profile_store.get_profile(&profile_key)
    };
    let helpers = crate::jit::helpers::build_helpers_for(shared);

    // Resolve java/lang/String's field layout for the JIT String call-site
    // intrinsics (see `resolve_string_field_layout`).
    let string_layout_resolver = || resolve_string_field_layout(shared);

    // Direct JIT-to-JIT calls, LOOKUP-ONLY (tomcat doc 04, 2026-07-27).
    //
    // This argument used to be `None` ("no recursive callee compilation"),
    // which is what the tiered BACKGROUND worker compiles every hot method
    // with. `None` does not merely decline to compile a callee eagerly — it
    // means `jit/src/lib.rs` plans NO `direct_calls` at all, so every
    // `invokestatic`/`invokespecial` in a background-compiled body falls
    // through to the generic `jit_invoke_dispatch` helper round trip, on
    // every call, forever. Measured on `apps/tomcat-suite-runner/probes/
    // CallCostProbe.java` (marginal cost of one extra invoke inside a
    // compiled loop, same binary, same run):
    //
    //     invokestatic, background-compiled body  ~196 ns
    //     invokestatic, mutator-compiled body     ~5-9 ns   (CRATONVM_BG_COMPILE=0)
    //     invokestatic, HotSpot                   ~0.5 ns
    //
    // i.e. ~30x on the single most common call form in ordinary Java, on the
    // path that compiles almost everything. Deploy-heavy Tomcat classes are
    // exactly this shape (reflection, class loading, Digester), which is the
    // residual "interpreter throughput x method count" wall doc 04 describes.
    //
    // The stated reason for `None` was recursion: `try_jit_upgrade_with_gate`'s
    // `callee_compiler` will COMPILE an unseen callee inline, and doing that
    // from the compile worker would nest compiles. This resolver keeps that
    // property by never compiling anything — it answers only from
    // `jit_cache`, so a callee that is already compiled (the overwhelmingly
    // common case once a workload is warm; the tiered manager compiles leaves
    // before their callers because leaves reach the invocation threshold
    // first) gets a raw `CALL`, and anything else stays on dispatch exactly
    // as before.
    //
    // Every refusal gate of the mutator-side `callee_compiler` is mirrored
    // here, in the same order; each `None` is correctness-safe because it
    // just leaves the site on the checked dispatch helper:
    //   * FJP subclass blocklist (RFJP.1)
    //   * Rust native shadow (S111r15)
    //   * BUG-H: callee declaring its own exception table — a raw CALL would
    //     let an implicit AIOOBE/NPE escape the callee's own `catch`
    //   * `synchronized` callee (no monitor pairing across a raw CALL)
    //   * JVMS 5.5: a `static` callee whose declaring class is not yet
    //     initialized (the direct CALL bypasses every init check)
    //   * an artifact carrying an unconditional invokedynamic trap
    let direct_callee_lookup = |callee_class: &str,
                                callee_method: &str,
                                callee_desc: &str|
     -> Option<(usize, bool)> {
        // Each arm names its refusal TWICE on purpose: the string is for the
        // per-site `dbg_jitc` trace, the `DirectBindRefusal` variant is for the
        // process-wide tally the `intrinsic-stats` census prints. A bare
        // `bind_misses` total cannot distinguish a compile-ORDER accident
        // (re-bindable) from a standing policy refusal (not), which is the
        // choice the two-causes page leaves open.
        macro_rules! dc_no {
                ($why:expr, $reason:expr) => {{
                    cratonvm_jit::note_direct_callee_bind_refusal($reason);
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] bg-direct-call DECLINED {callee_class}.{callee_method}{callee_desc}: {}",
                            $why
                        );
                    }
                    return None;
                }};
            }
        if is_fjp_subclass_blocklisted(shared, callee_class, Some(cached.declaring_class_id)) {
            dc_no!(
                "fjp-blocklist",
                cratonvm_jit::DirectBindRefusal::FjpBlocklist
            );
        }
        if shared
            .natives
            .native_methods
            .find(callee_class, callee_method, callee_desc)
            .is_some()
        {
            dc_no!(
                "native-shadow",
                cratonvm_jit::DirectBindRefusal::NativeShadow
            );
        }
        let callee_class_id = {
            let cm = shared.classes.class_manager.read();
            let Some(callee_cid) =
                cm.find_class_by_name_for_class(callee_class, cached.declaring_class_id)
            else {
                dc_no!(
                    "callee-class-not-found",
                    cratonvm_jit::DirectBindRefusal::CalleeClassNotFound
                );
            };
            let store = cm.class_store();
            let Some((method, declaring_id)) = crate::classloading::find_method_recursive(
                callee_cid,
                callee_method,
                callee_desc,
                store,
            ) else {
                dc_no!(
                    "callee-method-not-found",
                    cratonvm_jit::DirectBindRefusal::CalleeMethodNotFound
                );
            };
            if method.is_synchronized() {
                dc_no!(
                    "synchronized",
                    cratonvm_jit::DirectBindRefusal::Synchronized
                );
            }
            // A callee with no `Code` attribute at all (`map_or(true, ..)`)
            // is always refused — there is no body to bake a CALL to. A callee
            // that merely DECLARES a table is refused only while
            // `direct_call_exc_table_publish_enabled` is off; see that function
            // for why the stated reason has expired and what it is interlocked
            // against.
            let callee_code_bars_direct_call = match method.code() {
                None => true,
                Some(c) => {
                    !c.exception_table.is_empty()
                        && !cratonvm_jit::direct_call_exc_table_publish_enabled()
                }
            };
            if callee_code_bars_direct_call {
                dc_no!(
                    "callee-exception-table",
                    cratonvm_jit::DirectBindRefusal::CalleeExceptionTable
                );
            }
            if method.is_static()
                && !store
                    .get(declaring_id)
                    .map(crate::vm::is_class_initialized_fast)
                    .unwrap_or(false)
            {
                dc_no!(
                    "declaring-class-not-initialized",
                    cratonvm_jit::DirectBindRefusal::DeclaringClassNotInitialized
                );
            }
            callee_cid
        };
        let callee_class_arc: Arc<str> = Arc::from(callee_class);
        let callee_method_arc: Arc<str> = Arc::from(callee_method);
        let callee_desc_arc: Arc<str> = Arc::from(callee_desc);
        // Probe in a scope so the read lock is RELEASED before the transitive
        // compile below, which takes the same cache for writing.
        let cached_body = {
            let jit_cache = shared.jit.jit_cache.read();
            jit_cache.get(
                &callee_class_arc,
                &callee_method_arc,
                &callee_desc_arc,
                callee_class_id,
            )
        };
        // TRANSITIVE EAGER CALLEE COMPILE (`eager-callee-chain`).
        //
        // This resolver used to stop here with `callee-not-yet-compiled`, which
        // made eager callee binding exactly ONE level deep: the mutator door's
        // `callee_compiler` compiled a direct callee, but that callee was itself
        // compiled through this function, whose resolver bound ITS statically bound
        // sites to the generic `jit_invoke_dispatch` helper whenever they were not
        // compiled yet — and a compiled body never re-binds. So the whole chain's
        // speed depended on the ORDER the tiered manager happened to reach the
        // methods in, permanently.
        //
        // Measured on `probes/org/junit/jupiter/api/CompileOrderProbe.java` (JUnit's
        // `assertEquals` chain under a hot loop, real-JDK mode, G1): 478 ns/iter
        // compiling top-down against 73 bottom-up, with `CRATONVM_DBG_MIC_PROF=1`
        // reporting `disp_calls` 2 003 538 against 3 926 — one helper round trip per
        // iteration, ~150 ns of it, at ONE site (`CRATONVM_DBG_MIC_TRACE=1` named it:
        // `AssertEquals.assertEquals(Object,Object,String)`, reached from the
        // two-argument overload that had been compiled first).
        //
        // Compiling the callee here makes the bind order-independent: whichever
        // method the tiered manager reaches first, its statically bound callees are
        // compiled before its body is emitted, so the site binds directly.
        //
        // Bounded three ways, because this recursion is unbounded in principle:
        //  * DEPTH — `jit_active_compile_depth`; past the bound the site simply
        //    keeps dispatch, which is always correct;
        //  * CYCLES — `jit_active_compile_contains` declines a callee already open
        //    on this thread's compile stack (the jit crate's own
        //    `note_jit_recursive_compile_cycle` answers the same question for its
        //    `callee_compiler`);
        //  * FAN-OUT — a per-top-level-compile budget, so a method with many call
        //    sites cannot turn one compile into a whole-program one.
        //
        // Every other refusal is unchanged: this arm is reached only when the callee
        // passed all of the gates above and the ONLY thing missing was a compiled
        // body.
        let compiled = match cached_body {
            Some(compiled) => compiled,
            None => {
                if !crate::runtime::env_cache::jit_eager_callee_chain() {
                    dc_no!(
                        "callee-not-yet-compiled",
                        cratonvm_jit::DirectBindRefusal::CalleeNotYetCompiled
                    );
                }
                if cratonvm_jit::jit_active_compile_depth() > MAX_EAGER_CALLEE_CHAIN_DEPTH {
                    dc_no!(
                        "eager-callee-chain-depth",
                        cratonvm_jit::DirectBindRefusal::EagerChainDepth
                    );
                }
                if cratonvm_jit::jit_active_compile_contains(
                    callee_class,
                    callee_method,
                    callee_desc,
                ) {
                    dc_no!(
                        "eager-callee-chain-cycle",
                        cratonvm_jit::DirectBindRefusal::EagerChainCycle
                    );
                }
                if !eager_callee_chain_try_spend() {
                    dc_no!(
                        "eager-callee-chain-budget",
                        cratonvm_jit::DirectBindRefusal::EagerChainBudget
                    );
                }
                // `optimize` is this compile's own backend selection, so a C1 body's
                // callees are compiled at C1 and a C2 body's at C2 — the callee never
                // arrives at a tier its caller was not compiled against.
                let Some((body, _entry, _needs_ctx)) = try_jit_compile_callee(
                    shared,
                    callee_class,
                    callee_method,
                    callee_desc,
                    optimize,
                ) else {
                    dc_no!(
                        "eager-callee-chain-compile-declined",
                        cratonvm_jit::DirectBindRefusal::EagerChainCompileDeclined
                    );
                };
                body
            }
        };
        if compiled.has_indy_trap {
            dc_no!("indy-trap", cratonvm_jit::DirectBindRefusal::IndyTrap);
        }
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] bg-direct-call BOUND {callee_class}.{callee_method}{callee_desc}"
            );
        }
        // Cast: object/code pointer to integer address
        Some((compiled.entry_ptr() as usize, compiled.needs_context()))
    };

    let compile_start = std::time::Instant::now();
    crate::jit::set_self_call_identity_stable(self_call_identity_stable(
        shared,
        cached.declaring_class_id,
    ));
    // JDK-ONLY-WAVE2 §4. Answers "is this triple a reviewed
    // `NativeKind::Intrinsic`?" — `false` for `Bridge`, for `SyntheticStub`
    // and for anything unregistered, which is the fail-closed direction.
    // Cheap: only the strict arm of `direct_native_helper` calls it, and only
    // for a triple whose helper cell is already non-zero.
    let intrinsic_resolver = |class: &str, method: &str, descriptor: &str| -> bool {
        let registry = &shared.natives.native_methods;
        registry
            .resolve_id(class, method, descriptor)
            .and_then(|id| registry.kind_of_id(id))
            .is_some_and(|kind| kind == cratonvm_native_api::NativeKind::Intrinsic)
    };

    // Build inline resolver for method inlining (Session 31).
    //
    // Declared HERE rather than beside the other resolvers above because it
    // borrows `direct_callee_lookup`: the calls inside a body about to be
    // spliced get the same lookup-only direct binding this method's own call
    // sites get, and a spliced call that falls to the blind dispatch helper
    // instead is a measured 3.5x loss (see `jit_inline_call_dispatch`).
    let inline_resolver = |callee_class: &str,
                           callee_method: &str,
                           callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_inline_site(
            shared,
            cached.declaring_class_id,
            callee_class,
            callee_method,
            callee_desc,
            Some(&direct_callee_lookup),
        )
    };

    // The optimizing tier's own inline resolver. Same three-name question, a
    // different admission set — see `resolve_ir_inline_site`.
    //
    // This passed NO direct-bind resolver until 2026-09-09, on the stated
    // grounds that "`IrBuilder` has no direct-call lowering inside a relocated
    // body to bake an entry into". That was true and is no longer: the
    // `ir_direct_calls` map is keyed by COMBINED-BUFFER pc, `ir_lower`'s
    // `Op::Call` arm looks a spliced pc up in it like any other, and
    // `append_ir_inline_site` now produces the rows. Without the binder here
    // those rows are all empty, so every statically-bound call a splice left
    // behind kept resolving its callee BY NAME on every execution — the
    // measured 3.5x loss the single-pass sibling's comment above describes,
    // paid on the tier that is supposed to be the fast one.
    //
    // The same lookup-only closure the single-pass sibling uses, and for the
    // same reason: it binds an ALREADY-compiled callee and never compiles one,
    // so planning cannot recurse into compilation here.
    let ir_inline_resolver = |callee_class: &str,
                              callee_method: &str,
                              callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_ir_inline_site(
            shared,
            cached.declaring_class_id,
            callee_class,
            callee_method,
            callee_desc,
            if cratonvm_jit::ir_splice_direct_call_enabled() {
                Some(&direct_callee_lookup as InlineDirectBind<'_>)
            } else {
                None
            },
        )
    };

    let mut compiled = crate::jit::try_compile_with_invokespecial_resolver(
        &cached,
        Some(&resolver),
        Some(&field_resolver),
        Some(&static_field_resolver),
        Some(&invoke_resolver),
        Some(&invokespecial_owner_resolver),
        // Lookup-only: binds an ALREADY-compiled callee to a raw CALL, never
        // compiles one (see `direct_callee_lookup` above).
        Some(&direct_callee_lookup),
        Some(&new_resolver),
        Some(&ldc_resolver),
        Some(&ldc2w_resolver),
        pgo_profile.as_ref(),
        &helpers,
        Some(&inline_resolver),
        Some(&string_layout_resolver),
        Some(&invoke_class_id_resolver),
        if scalar_new_on {
            Some(&elidable_init_resolver)
        } else {
            None
        },
        // wire-tiered-manager Step 3: `optimize` selects the backend per call.
        // Inline JIT-dispatch callers pass `true` (optimized C2); the background
        // tiered worker passes the C1/C2 value derived from the task's tier.
        optimize,
        // Gap B: int-only invokestatic → Op::Call. Now default-ON (inc 23);
        // `CRATONVM_JIT_IR_CALL=0` is the opt-out (single-pass dispatch).
        crate::runtime::env_cache::jit_ir_call(),
        // inc 24/29: invokespecial → Op::Call. Now default-ON; `CRATONVM_JIT_IR_CALL_SPECIAL=0` opts out.
        crate::runtime::env_cache::jit_ir_call_special(),
        // inc 25/29: long methods → IR path. Now default-ON; `CRATONVM_JIT_IR_LONG=0` opts out.
        crate::runtime::env_cache::jit_ir_long(),
        // inc 26: invokevirtual/invokeinterface → Op::Call (dynamic dispatch via
        // the helper), gated default-OFF (its own soak). `=1` opts in.
        crate::runtime::env_cache::jit_ir_call_virtual(),
        // inc 30 + Slices A/B/C: double/float XMM value tier. Now default-ON
        // (opcode-complete + validated == HotSpot). `CRATONVM_JIT_IR_FP=0` opts out.
        crate::runtime::env_cache::jit_ir_fp(),
        // invokedynamic-uncommon-trap fix: resolves an invokedynamic CP index
        // to its target descriptor so the codegen can lower the instruction
        // to an unconditional uncommon-trap deopt instead of bailing the
        // whole method.
        Some(&indy_descriptor_resolver),
        if crate::runtime::env_cache::jit_guarded_virtual_inline() {
            Some(&class_id_namer)
        } else {
            None
        },
        if crate::runtime::env_cache::jit_guarded_virtual_inline() {
            Some(&receiver_inline_resolver)
        } else {
            None
        },
        // IR-tier inlining. Behind its own gate; `None` splices nothing.
        if cratonvm_jit::ir_inline_enabled() {
            Some(&ir_inline_resolver)
        } else {
            None
        },
        // Per-VM JDK-only policy (JDK-ONLY-WAVE2 §2). Was a process-global
        // latch the JIT read for itself, so a `Compatible` VM sharing a
        // process with a `JdkOnly` one lost the thin direct-call helpers.
        crate::vm::dispatch_policy(shared).is_jdk_only(),
        // JDK-ONLY-WAVE2 §4: the registry's own `NativeKind`, in place of

        // the JIT's seven hard-coded triples, as the §1.4 verdict on

        // whether a thin direct-call helper may shadow real bytecode.
        Some(&intrinsic_resolver),
        // This VM's per-bci de-spec registry.
        Some(&shared.jit.despec_registry),
        Some(&invoke_declaring_class_resolver),
    )?;
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] full-compile {}.{}{} entry={:p} len={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            compiled.entry_ptr(),
            compiled.code_bytes().len()
        );
    }
    crate::jit::disasm::maybe_dump(
        if compiled.used_ir_backend { "full/ir" } else { "full/sp" },
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        compiled.entry_ptr(),
        compiled.code_bytes(),
    );
    // The eager first-call door's half of the deferred-`new` retry. This door
    // reaches the backend directly and produces no `CompileOutcome`, so the
    // worker-loop route in `compile_one_task` never sees a method compiled
    // here -- which is every method the interpreter hands over on its own,
    // `RJitGc.make` among them. The one-shot memo is consumed here exactly as
    // it is there, so the two doors together still produce at most one extra
    // compile per method.
    if crate::runtime::env_cache::c2_supersede()
        && cratonvm_jit::take_deferred_new_retry(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            &|holder, cp_idx| {
                let cm = shared.classes.class_manager.read();
                matches!(
                    resolve_jit_new_site(&cm, ClassId::new(holder), cp_idx),
                    Some(cratonvm_jit::JitNewSite::Resolved { .. })
                )
            },
        )
    {
        shared
            .jit
            .tiered_manager
            .request_deferred_new_retry(&crate::jit::tiered::MethodKey::new(
                &*cached.class_name,
                &*cached.method_name,
                &*cached.method_descriptor,
            ));
    }
    // …and the other half: every method whose retry is HELD because its class
    // was not loaded. Nothing brings such a method back on its own — it already
    // has a body, so it is never compiled again, and the door above is only ever
    // walked by the method being compiled right now.
    resweep_held_deferred_new_retries(shared, false);
    let compile_duration_ns = compile_start.elapsed().as_nanos() as u64; // Cast: duration to u64 nanoseconds

    // Record JFR compilation event.
    //
    // GC-STW-safety / lock-scope discipline (wire-tiered-manager increment 3):
    // when this runs on the GC-neutral `cratonvm-jit-compiler` worker, the
    // `shared.debug.flight_recorder.lock()` must be held for the MINIMAL scope and
    // NEVER across a blocking op or a nested VM-lock acquisition — a mutator
    // wanting the recorder must not stall behind the worker (which would keep
    // that mutator off its safepoint and stall a third-thread STW). The
    // timestamp and the `Arc<str>` description (a Rust-heap alloc, not a managed
    // GC allocation) are built BEFORE the lock so the guard's live region is
    // exactly the `emit_compilation_event_arc` call and nothing else. No other
    // VM lock (`class_manager` / `jit_cache`) is held here — `cm` was dropped at
    // the `drop(cm)` above, and `jit_cache.write()` is taken AFTER this block.
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64; // Cast: duration to u64 nanoseconds
                            // Round-9 HIGH-5: build the Arc<str> once and hand ownership to the
                            // `_arc` variant instead of letting `emit_compilation_event` reallocate
                            // a fresh Arc from `&str` internally. Built before the lock so the alloc
                            // is outside the `flight_recorder` critical section.
    let method_desc: Arc<str> = Arc::from(format!(
        "{}::{}{}",
        cached.class_name, cached.method_name, cached.method_descriptor
    ));
    {
        let mut jfr = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_compilation_event_arc(
            &mut jfr,
            method_desc,
            1,     // compile_id
            4,     // compile_level (C2 equivalent)
            true,  // succeeded
            false, // is_osr
            0,     // code_size
            0,     // inlined_bytes
            now_ns.saturating_sub(compile_duration_ns),
            compile_duration_ns,
        );
    }

    // Store in JIT cache.
    //
    // Recompile-storm fix: key by the RECEIVER `class_name` (the value the
    // lookup in `try_jit_compile_callee` uses), NOT `cached.class_name` (the
    // DECLARING class). For an INHERITED method the two differ — e.g.
    // `SingletonPredictionContext.hashCode` resolves to the final
    // `PredictionContext.hashCode`, whose declaring class is
    // `PredictionContext`. Storing under the declaring class while the lookup
    // probes the receiver class made the cache miss on every polymorphic call,
    // so the dispatch helper recompiled the same method endlessly (42k+
    // recompiles of `PredictionContext.hashCode` observed during a Groovy
    // parse). Keying by the receiver makes the next call on the same receiver
    // class hit; distinct subclasses recompile at most once each. The compiled
    // code is identical regardless of receiver (it is the resolved method's
    // body), so dispatching it for any receiver of that class is correct.
    //
    // GC-STW-safety / lock-scope discipline (wire-tiered-manager increment 3):
    // this is the "publish" step — on the GC-neutral worker it is the moment a
    // freshly compiled body becomes visible to mutators (their `jit_cache`
    // fast-path flips the call site to `Jit` on the next invocation, the
    // cross-thread analogue of flipping the invoke cache). The
    // `shared.jit.jit_cache.write()` is held for the MINIMAL scope: the receiver
    // key `Arc` is built BEFORE the lock, the codegen + every resolver read of
    // `class_manager` already completed above (no VM lock is live here), and the
    // guard covers exactly the `put`. Holding nothing else means a mutator
    // taking `jit_cache.read()` (or `class_manager.write()` to define a class)
    // never blocks behind the worker, so it always reaches its safepoint and a
    // concurrent STW completes promptly.
    let receiver_key: std::sync::Arc<str> = std::sync::Arc::from(class_name);
    let method_name_key = cached.method_name.clone();
    let method_desc_key = cached.method_descriptor.clone();
    // A supersede the acceptance gate refused publishes NOTHING.
    //
    // The gate discards the optimizing body and `try_compile_inner` falls
    // through to the single-pass backend, so what reaches this point is an
    // equivalent of the body already in the cache. Publishing it replaces a C1
    // body with an equal one AND bumps the process-wide supersede epoch, which
    // stales every cached invoke target in every thread -- 1,558 evictions on
    // one H2 run. That is the entire cost the gate exists to avoid, paid in
    // full by a gate that refused.
    //
    // Measured before this: with the gate refusing 571 bodies,
    // `CRATONVM_C2_SUPERSEDE=0` was still ~5% faster in mean CPU on H2 against
    // a control pair agreeing to 0.04%.
    //
    // Both halves of the condition matter. A REFUSED verdict alone is not
    // enough: at the eager first-call door there is no predecessor, and
    // skipping the publish there would leave the method interpreted. `None`
    // means the method never reached the optimizing pipeline, which is an
    // absence of opinion rather than a refusal.
    let refused_supersede = cratonvm_jit::ir_evidence::take_last_verdict() == Some(false)
        && {
            let jit_cache = shared.jit.jit_cache.read();
            jit_cache
                .get(
                    &receiver_key,
                    &method_name_key,
                    &method_desc_key,
                    callee_class_id,
                )
                .is_some()
        };
    if refused_supersede {
        cratonvm_jit::ir_evidence::note_supersede_abandoned();
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] supersede ABANDONED {}.{}{} -- the optimizing body carried no evidence and a baseline body is already published",
                cached.class_name, cached.method_name, cached.method_descriptor,
            );
        }
        return None;
    }

    stamp_compilation_epoch(
        shared,
        &receiver_key,
        &method_name_key,
        &method_desc_key,
        &mut compiled,
    );
    // Stamp the wrapped-entry requirement before the body is shared.
    // See `CompiledMethod::requires_wrapped_entry`: publication is the
    // last point that still knows this is an `ACC_SYNCHRONIZED` method,
    // and every unwrapped consumer downstream holds only a raw entry
    // pointer.
    compiled.requires_wrapped_entry = cached.is_synchronized;
    let published = {
        let jit_cache = shared.jit.jit_cache.write();
        jit_cache.put(
            receiver_key.clone(),
            method_name_key.clone(),
            method_desc_key.clone(),
            callee_class_id,
            compiled,
        );
        // Read back a STRONG reference to what is now published under this key
        // rather than returning the address we held before the `put`. If a
        // concurrent publish won the race, this is its artifact — the live one
        // — instead of one whose buffer is already being unmapped.
        jit_cache.get(
            &receiver_key,
            &method_name_key,
            &method_desc_key,
            callee_class_id,
        )
    }?;
    let entry = published.entry_ptr() as usize; // Cast: JIT entry point to address
    let needs_ctx = published.needs_context();
    Some((published, entry, needs_ctx))
}

/// wire-tiered-manager increment 2 — the REAL off-thread compile callback.
///
/// Invoked on the background compile thread (see
/// `jit::tiered::start_background_compiler`) for each `CompilationTask` the
/// tiered manager drained off the mutator. Gated by `CRATONVM_BG_COMPILE`
/// (the closure is only installed when the flag is on).
///
/// It upgrades the captured `Weak<SharedVm>` (the worker holds no `Arc` of its
/// own, so it cannot keep the VM alive past teardown) and runs the same codegen
/// entry point the inline mutator path uses: [`try_jit_compile_callee`] does the
/// by-name `(class, method, descriptor)` lookup, builds the constant-pool
/// resolvers, calls `jit::try_compile`, and PUBLISHES the result into
/// `shared.jit.jit_cache`. Publishing into the shared cache is the cross-thread
/// "flip the invoke cache" mechanism: the per-thread `invoke_cache` is
/// thread-local and cannot be mutated from here, but the `Bytecode` arm's
/// `jit_cache` fast-path (interpreter.rs ~14366) upgrades the call site to
/// `Jit` on the next mutator invocation once the entry is present.
///
/// Step 3 (C1/C2 routing) — NOW REAL: the target tier selects the backend via
/// [`crate::jit::tiered::tier_uses_optimized_backend`], and that boolean is
/// threaded through `try_jit_compile_callee` into `jit::try_compile`'s trailing
/// `optimize` flag. `C2`/`FullProfile` → the optimizing IR pipeline; `C1`/
/// `C1WithProfiling` → the fast single-pass `x64::compile` backend (no IR
/// lowering / escape analysis / scheduling). This replaces the former advisory-
/// only hint (which compiled both tiers identically).
///
/// Boundary (follow-ups): the JIT-cache probe in `try_jit_compile_callee`
/// returns any already-published body regardless of tier, so a method is
/// compiled at whatever tier reaches it first — there is no C1→C2 supersede /
/// re-compile yet (that needs safe code-cache replacement; cf. the bug-24 baked-
/// pointer UAF risk). The single-pass backend still runs its own internal escape
/// analysis; disabling x64-internal passes for an even-leaner C1 is a separate
/// step. Both are out of scope for this routing increment and gated default-off
/// behind `CRATONVM_BG_COMPILE` regardless.
///
/// Returns `(wall_clock_compile_time_ms, published)` for the tiered stats.
/// A compile miss / bail (native shadow, skip-listed, backend bail, or a dropped
/// VM) reports `published = false` — the queued flag is still cleared by the
/// worker's `complete_task`, but (unlike an earlier version of this code)
/// `current_tier` is NOT advanced on a failed attempt, so a later mutator
/// invocation genuinely re-attempts (bounded by
/// `jit::tiered::MAX_TIER_FAIL_RETRIES` — see `complete_task`) instead of the
/// method being silently marked "compiled" and stuck interpreting forever.
///
/// ## GC-neutral daemon (wire-tiered-manager increment 3)
///
/// This closure body is the entire VM-side surface of the
/// `cratonvm-jit-compiler` worker, which `jit::tiered::start_background_compiler`
/// spawns as an UNREGISTERED `std::thread::Builder` daemon — exactly the
/// G1-MarkComplete / JDWP class of VM-internal thread. It is deliberately NOT a
/// mutator: it is never `register_with_daemon`'d, never polls a safepoint, never
/// calls `arrive_and_wait`, and holds NO managed `ObjectRef` across any GC point.
/// Therefore the STW barrier's `expected` count (driven by
/// `thread_registry.alive_count()`) never includes it, and concurrent
/// relocation during a STW is harmless to it. Registering it instead would
/// WRONGLY add its native Rust stack to the GC root set and make STW wait on a
/// thread that has no safepoint — neither is wanted.
///
/// It captures a `Weak<SharedVm>` (never an `Arc`, so it cannot keep the VM
/// alive past teardown), upgrades it per task, and NO-OPS when the upgrade fails
/// (VM dropped) — the same `self_arc.upgrade()` pattern JDWP uses. All VM-lock
/// scopes it touches are bounded inside `try_jit_compile_callee[_slow]` (see that
/// function's lock-order contract): nothing is held across the worker's queue
/// wait (its own `CompilerCore::wake` condvar, no VM lock), across class loading,
/// or across the JFR / jit_cache publish.
/// wire-tiered-manager Step 5: start the background compile worker once
/// (idempotent), wiring the real off-thread compile closure. Captures a
/// `Weak<SharedVm>` (the worker owns no Arc of the VM) and, per drained task,
/// runs [`background_compile_task`] off the mutator. Called from BOTH the
/// invocation tier-up trigger AND the back-edge OSR path, so an all-hot-loop
/// program (a `main()` loop that never crosses the invocation threshold) still
/// starts the worker.
pub(super) fn ensure_bg_compiler_started(shared: &SharedVm) {
    // One atomic load once this VM's workers are running. The `Weak` below
    // takes the `self_arc` read lock, and every tier-up stride used to pay it.
    if shared.jit.tiered_manager.compiler_active() {
        return;
    }
    let weak_vm: std::sync::Weak<SharedVm> =
        shared.self_arc.read().as_ref().cloned().unwrap_or_default();
    crate::jit::tiered::ensure_background_compiler(&shared.jit.tiered_manager, || {
        Box::new(
            move |task: &crate::jit::tiered::CompilationTask| -> crate::jit::tiered::CompileOutcome {
                background_compile_task(&weak_vm, task)
            },
        )
    });
}

/// Offer one tier-up stride of `cached`'s method to the tiered manager, and
/// return the tier it queued a compile at, if any.
///
/// The door every interpreter tier-up hook goes through. It checks the call
/// site's `tiering_settled` stamp first: a method the manager has already said
/// it can do nothing more for -- declined by policy, out of compile retries,
/// already at C2 -- used to take the manager's global `methods` mutex and
/// allocate three `String`s for its key at every stride, for the life of the
/// process. Now it costs one relaxed load and one atomic compare until
/// something (a deopt, an unload, a redefinition, a policy change) moves the
/// manager's generation. The key it builds shares the cached method's
/// `Arc<str>`s and carries the declaring class's identity, so same-named
/// classes in different loaders keep separate tiering state.
///
/// Starting the worker stays the caller's job, because the `CRATONVM_BG_COMPILE=0`
/// paths consult the manager without one.
pub(super) fn offer_invocation_to_tiered_manager(
    shared: &SharedVm,
    cached: &cratonvm_jit_api::CachedBytecodeMethod,
    invocation_count: u64,
) -> Option<crate::jit::tiered::CompilationTier> {
    use std::sync::atomic::Ordering::Relaxed;
    let manager = &shared.jit.tiered_manager;
    if manager.tiering_settled(cached.tiering_settled.load(Relaxed)) {
        return None;
    }
    let key = crate::jit::tiered::MethodKey::with_class_id(
        cached.declaring_class_id,
        Arc::clone(&cached.class_name),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
    );
    let verdict = manager.on_method_invocation_settling(&key, invocation_count);
    cached
        .tiering_settled
        .store(verdict.settled_generation, Relaxed);
    verdict.recommended
}

/// wire-tiered-manager Step 5: resolve the inputs the off-thread OSR compile
/// needs for `(class, method, descriptor)` — `(class_id, padded bytecode,
/// max_locals)` — from already-loaded class metadata. The method is currently
/// executing in the interpreter (that is what tripped the back-edge), so its
/// class is loaded; we never load it here. Returns `None` if the
/// class/method/Code attribute is absent. The padded bytecode matches
/// `Frame::code`'s layout (`padded_bytecode`, +2 zero tail) so the compiled
/// artifact's PC mapping lines up with the interpreter frame at OSR entry.
/// What a C1→C2 supersede publish actually did to the cached body.
///
/// The distinction exists because only one of the three can invalidate an
/// invoke-cache entry, and the supersede epoch is a process-wide counter that
/// every `Jit` entry in every thread is measured against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SupersedeOutcome {
    /// No body was in the cache under this key before the publish, so nothing
    /// was superseded. Reached by `promote_scalar_selfrec_to_ir`, which sends
    /// the narrow scalar self-recursion shape straight to the optimizing tier
    /// without a C1 body ever existing.
    FirstPublish,
    /// A body was replaced by one with identical code bytes — what a C2 task
    /// that fell back to the single-pass backend produces.
    Unchanged,
    /// A body was replaced by different code.
    Changed,
}

impl SupersedeOutcome {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            SupersedeOutcome::FirstPublish => "first-publish",
            SupersedeOutcome::Unchanged => "unchanged",
            SupersedeOutcome::Changed => "changed",
        }
    }
}

/// Classify a supersede from the artifact that was in the cache before the
/// publish and the one in it afterwards.
///
/// A missing *replacement* is reported as `Changed`, not as one of the two
/// cheap outcomes: the lookup failing is not evidence that nothing changed, and
/// this decides whether to skip an invalidation, so the unknown case must fail
/// towards the old unconditional behaviour.
pub(super) fn classify_supersede(
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> SupersedeOutcome {
    let Some(before) = before else {
        return SupersedeOutcome::FirstPublish;
    };
    let Some(after) = after else {
        return SupersedeOutcome::Changed;
    };
    // Pointer equality first: `put` may have refused the publish (install-epoch
    // guard, code-cache cap), leaving the cache holding the very artifact that
    // was there before. That is an unchanged body by definition and skips the
    // byte compare entirely.
    if std::ptr::eq(before.as_ptr(), after.as_ptr()) && before.len() == after.len() {
        return SupersedeOutcome::Unchanged;
    }
    if before == after {
        SupersedeOutcome::Unchanged
    } else {
        SupersedeOutcome::Changed
    }
}

/// Engagement census for the three supersede outcomes.
///
/// A switch that suppresses work needs a count of what it suppressed, or a
/// "no regression" reading cannot be told apart from "never fired".
static SUPERSEDE_FIRST_PUBLISH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static SUPERSEDE_UNCHANGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SUPERSEDE_CHANGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Wall time spent in C2 tasks, split by whether the optimizing pipeline
/// produced the body or threw its work away and let single-pass do it.
///
/// This is the number that decides whether the fall-through is worth
/// preventing. The epoch bump it also pays was already measured at ~9
/// invoke-cache evictions per run, i.e. nothing; the COMPILE is the part that
/// could plausibly cost something, so it is timed rather than assumed.
static C2_FELL_THROUGH_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static C2_FELL_THROUGH_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static C2_LOWERED_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static C2_LOWERED_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn note_c2_compile(fell_through: bool, micros: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let (n, t) = if fell_through {
        (&C2_FELL_THROUGH_COUNT, &C2_FELL_THROUGH_US)
    } else {
        (&C2_LOWERED_COUNT, &C2_LOWERED_US)
    };
    n.fetch_add(1, Relaxed);
    t.fetch_add(micros, Relaxed);
}

/// `(fell_through_count, fell_through_us, lowered_count, lowered_us)`.
pub fn c2_compile_census() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        C2_FELL_THROUGH_COUNT.load(Relaxed),
        C2_FELL_THROUGH_US.load(Relaxed),
        C2_LOWERED_COUNT.load(Relaxed),
        C2_LOWERED_US.load(Relaxed),
    )
}

pub(super) fn note_supersede_outcome(outcome: SupersedeOutcome) {
    let counter = match outcome {
        SupersedeOutcome::FirstPublish => &SUPERSEDE_FIRST_PUBLISH,
        SupersedeOutcome::Unchanged => &SUPERSEDE_UNCHANGED,
        SupersedeOutcome::Changed => &SUPERSEDE_CHANGED,
    };
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(first_publish, unchanged, changed)` supersede counts for this process.
pub fn supersede_census() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        SUPERSEDE_FIRST_PUBLISH.load(Relaxed),
        SUPERSEDE_UNCHANGED.load(Relaxed),
        SUPERSEDE_CHANGED.load(Relaxed),
    )
}

pub(super) fn fetch_osr_compile_inputs(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(ClassId, std::sync::Arc<[u8]>, u16)> {
    let cm = shared.classes.class_manager.read();
    // The task's own class identity first: resolving by name alone returns
    // `None` when two loaders define the name, and would otherwise compile
    // whichever copy the name lookup picked. The name is the fallback for a
    // key built without an id.
    let class_id = if class_id.as_u32() != 0
        && cm
            .get_class(class_id)
            .is_some_and(|class| class.name.as_ref() == class_name)
    {
        class_id
    } else {
        cm.get_loaded_class_id(class_name)?
    };
    let class = cm.get_class(class_id)?;
    let method = class
        .methods
        .iter()
        .find(|m| &*m.name == method_name && &*m.descriptor == descriptor)?;
    let code_attr = method
        .attributes
        .iter()
        .find_map(|a| match a.as_decoded() {
            Some(cratonvm_reader::attribute::Attribute::Code(ca)) => Some(ca),
            _ => None,
        })?;
    // `padded_bytecode` only copies bytes (Rust-heap alloc, no VM lock / no
    // blocking / no managed allocation), so building it under the `cm` read
    // lock is GC-STW-safe; the lock drops at function return.
    let padded = crate::runtime::frame::padded_bytecode(&code_attr.code);
    let max_locals = code_attr.max_locals;
    Some((class_id, padded, max_locals))
}

/// Admit the narrow scalar self-recursion shape directly to the optimizing IR
/// backend on its first background compilation. Compiling it as C1 first is
/// counterproductive: once a recursive C1 frame is entered, its direct calls
/// remain in that slower body for the entire subtree even if C2 is published
/// concurrently. Every call target is resolved here and must be this exact
/// static `(I)I` method; all remaining structural checks live in the JIT crate.
pub(super) fn promote_scalar_selfrec_to_ir(
    shared: &SharedVm,
    key: &crate::jit::tiered::MethodKey,
) -> bool {
    let Some((class_id, padded, _)) =
        fetch_osr_compile_inputs(shared, key.class_id, &key.class_name, &key.method_name, &key.descriptor)
    else {
        return false;
    };
    let code_len = padded.len().saturating_sub(2);
    if !cratonvm_jit::scalar_selfrec_ir_would_engage(&padded, code_len, &key.descriptor) {
        return false;
    }
    let Some(scan) = crate::jit::x64::jit_scan(&padded, code_len, &key.descriptor) else {
        return false;
    };
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    let is_static = class
        .methods
        .iter()
        .find(|m| &*m.name == &*key.method_name && &*m.descriptor == &*key.descriptor)
        .map(|m| m.is_static())
        .unwrap_or(false);
    if !is_static {
        return false;
    }
    scan.invoke_ops.iter().all(|(_, cp_idx, opcode)| {
        if *opcode != 0xb8 {
            return false;
        }
        let Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
            ..
        }) = class.constant_pool.get(*cp_idx)
        else {
            return false;
        };
        let Some(target_class) = class.constant_pool.get_class_name(*class_index) else {
            return false;
        };
        let Some((target_name, target_desc)) =
            class.constant_pool.get_name_and_type(*name_and_type_index)
        else {
            return false;
        };
        target_class == &*key.class_name
            && target_name == &*key.method_name
            && target_desc == &*key.descriptor
    })
}

pub(super) fn background_compile_task(
    weak_vm: &std::sync::Weak<SharedVm>,
    task: &crate::jit::tiered::CompilationTask,
) -> crate::jit::tiered::CompileOutcome {
    use crate::jit::tiered::CompileOutcome;
    // A compile that RAN and failed — spends one of the method's retries.
    let fail = CompileOutcome::failed;
    // The method was refused on grounds that cannot change while this process
    // lives (the skip list and the OSR-denial set are pure functions of the
    // loaded method), so the tier manager records the verdict once instead of
    // burning the retry budget re-asking. Keeping these apart is what makes
    // `tier_fail_count` mean "codegen is broken" rather than "banned by
    // policy" — see `MethodState::ineligible`.
    let declined = CompileOutcome::declined;
    let shared = match weak_vm.upgrade() {
        Some(s) => s,
        // VM dropped (teardown) — nothing to compile, and nothing to learn
        // about this method. Charge it to neither counter.
        None => return fail(0),
    };
    if crate::runtime::redefine_state::class_id_or_name_was_redefined(
        &shared,
        task.method_key.class_id.as_u32(),
        &task.method_key.class_name,
    ) {
        // A retransformed target remains interpreted for now. This is a
        // per-class decline; unrelated background tasks continue compiling.
        return declined(0);
    }
    // Keep asynchronous tiering aligned with the foreground admission paths.
    // Without this gate, methods rejected by the conservative skip list are
    // repeatedly queued by the tier manager. The final compiler gate then
    // declines each task, but the hot interpreter path keeps paying for the
    // failed background attempts. Hibernate's package-level fail-closed
    // policy made that retry loop large enough to turn ordinary suite classes
    // into timeout candidates.
    // The static JIT ban list was deleted 2026-07-31 (see
    // docs/known-issues/jit-bans/jit-bans-all-disabled-20260731.md).
    // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
    // remaining force-interpret lever, applied in `jit::try_compile`.
    if task.osr_bci.is_some() && shared.jit.tiered_manager.is_osr_denied(&task.method_key) {
        return declined(0);
    }
    let optimized = crate::jit::tiered::tier_uses_optimized_backend(task.target_tier)
        || (task.osr_bci.is_none() && promote_scalar_selfrec_to_ir(&shared, &task.method_key));
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] bg-compile {}.{}{} tier={:?} optimized={}{}",
            task.method_key.class_name,
            task.method_key.method_name,
            task.method_key.descriptor,
            task.target_tier,
            optimized,
            task.osr_bci
                .map(|b| format!(" osr_bci={b}"))
                .unwrap_or_default(),
        );
    }
    // wire-tiered-manager Step 5 (precise background OSR): an OSR-motivated task
    // compiles an OSR-enterable artifact OFF the mutator (the worker has no live
    // frame). `compile_osr_artifact` resolves all metadata from the (loaded)
    // class and publishes a `compiled_via_osr` body into `jit_cache`; the
    // mutator's back-edge path then ENTERS that published artifact (the existing
    // `osr_reused` reuse path) with no inline compile stall. `osr_bci` is the
    // back-edge the mutator will enter at — the artifact supports entry at every
    // loop header it emits, so the compile itself is entry-pc-independent.
    if let Some(osr_bci) = task.osr_bci {
        let start = std::time::Instant::now();
        // The class the compile's verdicts are recorded under: the key's id,
        // or the one `fetch_osr_compile_inputs` resolves when the key has none.
        let mut verdict_class_id = task.method_key.class_id;
        let published = if let Some((class_id, padded, max_locals)) = fetch_osr_compile_inputs(
            &shared,
            task.method_key.class_id,
            &task.method_key.class_name,
            &task.method_key.method_name,
            &task.method_key.descriptor,
        ) {
            verdict_class_id = class_id;
            compile_osr_artifact(
                &shared,
                class_id,
                task.method_key.class_name.to_string(),
                task.method_key.method_name.to_string(),
                task.method_key.descriptor.to_string(),
                &padded,
                max_locals as usize,
                osr_bci as usize,
            )
            .is_some()
        } else {
            false
        };
        if !published {
            // Which failures deny OSR, and which are retried.
            //
            // This used to deny OSR for the method on ANY unpublished result,
            // for the rest of the process. Most ways a background OSR compile
            // comes back empty are transient: the class manager could not hand
            // over the inputs this time (`fetch_osr_compile_inputs` returned
            // `None`), the code cache was full, a redefinition landed
            // mid-compile, a callee had not loaded yet. Denying on those turned
            // one unlucky compile into a method whose loops stayed interpreted
            // forever (a once-invoked harness main with the hot loop inline ran
            // ~8x slow with zero other diagnostics, perf/halfgap-20260717).
            //
            // So a failure is a failed compile -- it spends one of the method's
            // `MAX_TIER_FAIL_RETRIES` and the next hot back-edge asks again,
            // which also bounds the re-enqueue loop the old denial existed to
            // stop -- unless the compiler recorded a verdict the loaded bytecode
            // cannot change by bail-listing the method. Only then is OSR denied,
            // and that denial still expires when the install epoch moves.
            let permanent = cratonvm_jit::is_jit_bail_listed(
                verdict_class_id,
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
            );
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] OSR-compile FAILED {}.{}{} osr_bci={} stage={} — {}",
                    task.method_key.class_name,
                    task.method_key.method_name,
                    task.method_key.descriptor,
                    osr_bci,
                    osr_stage_get(),
                    if permanent {
                        "bail-listed: OSR denied until the install epoch moves"
                    } else {
                        "transient: counted as a failed compile and retried"
                    },
                );
                eprintln!(
                    "[cratonvm-jitc]   …and the bail this method last recorded: {}",
                    cratonvm_jit::jit_bail_reason_for(
                        verdict_class_id,
                        &task.method_key.class_name,
                        &task.method_key.method_name,
                        &task.method_key.descriptor,
                    )
                    .unwrap_or_else(|| "none recorded".to_string()),
                );
            }
            if permanent {
                shared
                    .jit
                    .tiered_manager
                    .mark_osr_denied(task.method_key.clone());
                return declined(start.elapsed().as_millis() as u64);
            }
        }
        return CompileOutcome {
            // Widening: smaller integer -> 64-bit (zero/sign-extended).
            compile_time_ms: start.elapsed().as_millis() as u64,
            published,
            // OSR artifacts serve loop entry; the invocation path re-tiers
            // separately, so an OSR task never seeds a C2 upgrade.
            c2_upgrade_candidate: false,
            deferred_new_retry: false,
            // A codegen attempt actually ran here; a non-publish is a real
            // failure, not a policy verdict.
            declined_permanently: false,
        };
    }
    let start = std::time::Instant::now();
    // The body about to be REPLACED, captured before the publish overwrites it.
    //
    // Nothing compares a C2 body against the C1 body it supersedes before
    // keeping it, and the open policy question that follows from that
    // (`perf-01-sieve-ir-body-slower-than-c1`) is deliberately not answered
    // here: the metrics a static comparison could use are all proxies, and the
    // two that look obvious both misjudge the good cases — a bigger body is
    // usually inlining or unrolling, and MORE call sites can be a callee's
    // calls after its frame was inlined away. What is missing is not a rule
    // but DATA, so this records the replacement instead of guessing at it.
    //
    // The capture is the whole artifact, not its length, and it is NOT gated on
    // the diagnostic flag any more: the epoch bump below is now conditional on
    // what this finds, so a debug-only capture would make the diagnostic change
    // the behaviour it reports. `JitCache::get` returns an `Arc` clone, so
    // holding it across the publish costs a refcount and keeps the superseded
    // artifact alive against `defer_jit_owner`'s drop.
    let superseded_body: Option<std::sync::Arc<cratonvm_jit::CompiledMethod>> = optimized
        .then(|| {
            let (class_id, _, _) = fetch_osr_compile_inputs(
                &shared,
                task.method_key.class_id,
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
            )?;
            let jit_cache = shared.jit.jit_cache.read();
            jit_cache.get(
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
                class_id,
            )
        })
        .flatten();
    // Real codegen + publish into the shared JIT cache. `try_jit_compile_callee`
    // is the by-name entry point shared with the JIT dispatch helpers; it stores
    // the compiled body under `(class, method, descriptor)` so the mutator's
    // `jit_cache` fast-path flips the call site to `Jit` on its next call.
    // `is_some()` reports whether it actually published one — a `None` (skip-
    // listed, resolver miss, code-cache cap, concurrent redefine, ...) must
    // NOT be reported as success, or the tiered manager marks this tier
    // "done" despite nothing having been compiled (see
    // `jit::tiered::CompilerCore::complete_task`).
    let published = try_jit_compile_wrapped_entry(
        &shared,
        &task.method_key.class_name,
        &task.method_key.method_name,
        &task.method_key.descriptor,
        // wire-tiered-manager Step 3: C1 (no-opt single-pass) vs C2 (optimizing
        // pipeline), selected by the task's target tier. This is the real
        // backend routing that replaces the former advisory-only hint.
        optimized,
    )
    .is_some();
    // Read on the SAME thread, immediately after the compile: did this C2 task
    // run the whole optimizing pipeline and then produce a single-pass body?
    let fell_through = cratonvm_jit::last_compile_fell_through_to_single_pass();
    let compile_us = start.elapsed().as_micros() as u64;
    if published && optimized {
        note_c2_compile(fell_through, compile_us);
    }
    // C1→C2 supersede, publish side: a freshly-published C2 body REPLACED the
    // C1 entry in `jit_cache` (JitCache::put overwrites by key). Bump the
    // global supersede epoch so per-thread invoke-cache `Jit` entries (which
    // snapshot the epoch at IC-fill time) report stale on their next hit,
    // self-evict, and re-resolve to the C2 body. Without this, call sites that
    // already flipped to the C1 artifact would run it forever.
    //
    // That bump is GLOBAL: it invalidates every `Jit` invoke-cache entry, for
    // every call site, in every thread — `CachedInvokeTarget::is_stale`
    // compares one process-wide counter, and `InvokeCache::get` self-evicts on
    // it. So it must be paid only when there is something to invalidate. The
    // first reading off the `c1=`/`c2=` diagnostic said it usually is not:
    // in one CratonBench run, 7 of 9 supersedes republished a body of exactly
    // the same size, and a 8th (`fib`) had no predecessor at all.
    //
    // Two of the three outcomes below cannot invalidate anything:
    //
    //  * `FirstPublish` — no prior body, so no `Jit` entry can be holding a
    //    replaced one. `fib` reaches C2 without a C1 body at all, because
    //    `promote_scalar_selfrec_to_ir` sends the narrow scalar self-recursion
    //    shape straight to the optimizing pipeline. The interpreter's negative
    //    "no compiled body" memo is NOT this counter's job: `JitCache::put`
    //    bumps `jit_cache_generation` on every publication precisely so a
    //    first insertion is observed there.
    //  * `Unchanged` — the published body is byte-identical to the one it
    //    replaced, which is what a C2 task that fell back to the single-pass
    //    backend produces (the IR admission gate declines, single-pass
    //    recompiles the same bytecode deterministically). An IC entry still
    //    holding the old artifact executes identical machine code, and it owns
    //    an `Arc` to it, so the artifact stays alive.
    //
    // Identical code bytes also imply an identical ABI — `needs_heap` and
    // `needs_context` are visible in the prologue — so an entry kept on the old
    // artifact cannot be called the wrong way.
    //
    // Skipping those two is OFF by default: `CRATONVM_JIT_SUPERSEDE_EPOCH_SKIP_USELESS=1`
    // turns it on. The bump was measured, not assumed, to be nearly free — 9
    // invoke-cache evictions over a whole CratonBench run and 0 over the regex
    // workload — so the saving is real but worth nothing, and a default-on
    // behaviour change that buys nothing is not worth its risk.
    if published && optimized {
        let replacement = {
            let jit_cache = shared.jit.jit_cache.read();
            fetch_osr_compile_inputs(
                &shared,
                task.method_key.class_id,
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
            )
            .and_then(|(class_id, _, _)| {
                jit_cache.get(
                    &task.method_key.class_name,
                    &task.method_key.method_name,
                    &task.method_key.descriptor,
                    class_id,
                )
            })
        };
        let outcome = classify_supersede(
            superseded_body.as_ref().map(|cm| cm.code_bytes()),
            replacement.as_ref().map(|cm| cm.code_bytes()),
        );
        note_supersede_outcome(outcome);
        let bumped = outcome == SupersedeOutcome::Changed
            || !crate::runtime::env_cache::supersede_epoch_skip_useless();
        if bumped {
            crate::classloading::bump_jit_supersede_epoch();
        }
        if crate::runtime::env_cache::dbg_jitc() {
            // `c1=` is the body this one replaced, `c2=` the one that replaced
            // it. Both, always: the question this line exists for is whether
            // the optimizing tier is producing a BETTER body, and a size on
            // its own answers nothing without the size it displaced.
            //
            // `outcome=` is what separates the three cases a bare `c1=?` used
            // to conflate — no predecessor, an identical republish, and a real
            // replacement — and `epoch_bumped=` says whether this publish
            // actually paid the process-wide invalidation.
            eprintln!(
                "[cratonvm-jitc] c2-supersede published {}.{}{} (epoch={}) c1={} c2={} outcome={} epoch_bumped={}",
                task.method_key.class_name,
                task.method_key.method_name,
                task.method_key.descriptor,
                crate::classloading::jit_supersede_epoch(),
                superseded_body
                    .as_ref()
                    .map(|cm| cm.code_bytes().len().to_string())
                    .unwrap_or_else(|| "none".to_string()),
                replacement
                    .as_ref()
                    .map(|cm| cm.code_bytes().len().to_string())
                    .unwrap_or_else(|| "?".to_string()),
                outcome.as_str(),
                bumped,
            );
            if fell_through {
                eprintln!(
                    "[cratonvm-jitc]   …the optimizing pipeline ran and then handed this method to the single-pass backend ({compile_us} us total)",
                );
            }
            // When a replacement is the same LENGTH but not the same bytes,
            // say how far apart it actually is. A handful of scattered bytes
            // is a relocation (an embedded absolute address that moved),
            // which is a body that could still be treated as unchanged; a
            // large fraction is genuinely different code and cannot.
            if let (Some(b), Some(a)) = (superseded_body.as_ref(), replacement.as_ref()) {
                let (bb, ab) = (b.code_bytes(), a.code_bytes());
                if bb.len() == ab.len() && bb != ab {
                    let differing = bb.iter().zip(ab).filter(|(x, y)| x != y).count();
                    let first = bb.iter().zip(ab).position(|(x, y)| x != y).unwrap_or(0);
                    eprintln!(
                        "[cratonvm-jitc]   …same length, {} of {} bytes differ ({:.3}%), first at +0x{:x}",
                        differing,
                        bb.len(),
                        100.0 * differing as f64 / bb.len() as f64,
                        first,
                    );
                }
            }
        }
    }
    // C1→C2 supersede, trigger side: report whether this method would take
    // the optimizing IR pipeline (and is expected to benefit) so the worker
    // loop enqueues a Low-priority C2 recompile after it records this C1
    // publish. Evaluated only on a successful non-optimized publish — the
    // scan + predicate are cheap and run once per method.
    // A method whose IR build bailed on a `new` whose class was not loaded YET
    // is owed ONE more optimizing attempt. Note the absence of `!optimized`:
    // that bail happens INSIDE a C2 task, which then falls through to the
    // single-pass backend — so the C1->C2 promotion below, which is only for a
    // C1 publish, is not the door this can use. `take_deferred_new_retry`
    // consumes the memo only when the deferred `new` sites RESOLVE now: a class
    // still unloaded holds the retry rather than burning it on an attempt that
    // would bail identically.
    let deferred_new_retry = published
        && crate::runtime::env_cache::c2_supersede()
        && cratonvm_jit::take_deferred_new_retry(
            &task.method_key.class_name,
            &task.method_key.method_name,
            &task.method_key.descriptor,
            &|holder, cp_idx| {
                let cm = shared.classes.class_manager.read();
                matches!(
                    resolve_jit_new_site(&cm, ClassId::new(holder), cp_idx),
                    Some(cratonvm_jit::JitNewSite::Resolved { .. })
                )
            },
        );
    let c2_upgrade_candidate = published
        && !optimized
        && crate::runtime::env_cache::c2_supersede()
        && fetch_osr_compile_inputs(
            &shared,
            task.method_key.class_id,
            &task.method_key.class_name,
            &task.method_key.method_name,
            &task.method_key.descriptor,
        )
        .map(|(_, padded, _)| {
            let code_len = padded.len().saturating_sub(2);
            cratonvm_jit::c2_upgrade_would_engage(
                &padded,
                code_len,
                &task.method_key.descriptor,
                crate::runtime::env_cache::jit_ir_long(),
                crate::runtime::env_cache::jit_ir_fp(),
                crate::runtime::env_cache::jit_ir_call_virtual(),
            )
        })
        .unwrap_or(false);
    // A `None` above is not one thing. The comment on `published` already lists
    // the causes — "skip-listed, resolver miss, code-cache cap, concurrent
    // redefine" — and two of them are PERMANENT POLICY, not a codegen attempt
    // that failed. Reporting every `None` as a codegen failure made
    // `complete_task` spend `tier_fail_count` on methods no compile was ever
    // run for: three futile background tasks each, then the method is retired
    // and reported by `jit-method-stats` as `compile-failed reason=unrecorded`
    // — unrecorded precisely because nothing ran to record a bail site.
    //
    // That is how a Spring Boot context startup reported
    // `hot_but_stuck_in_interpreter=79 (ineligible-by-policy=0,
    // compile-failures=69)` while 2499 methods sat in the skip-seal census:
    // every one of those 69 was a policy verdict wearing a codegen failure's
    // label, which sent the reader looking for a compiler bug that is not there.
    //
    // `MethodState::ineligible` is the field that exists for exactly this, and
    // `complete_task` already honours it — it just was never told.
    let declined_permanently = !published
        && (shared
            .jit
            .jit_skip_set
            .read()
            // The skip-set and `MethodKey` both hold `Arc<str>`, so the probe
            // key is three reference-count bumps.
            .contains(&(
                task.method_key.class_name.clone(),
                task.method_key.method_name.clone(),
                task.method_key.descriptor.clone(),
            ))
            || cratonvm_jit::is_jit_bail_listed(
                task.method_key.class_id,
                &task.method_key.class_name,
                &task.method_key.method_name,
                &task.method_key.descriptor,
            ));
    CompileOutcome {
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        compile_time_ms: start.elapsed().as_millis() as u64,
        published,
        c2_upgrade_candidate,
        deferred_new_retry,
        declined_permanently,
    }
}

/// Convert a JIT panic payload into a `MethodCallFailed`.
///
/// Parses known panic message patterns (e.g. "ArrayIndexOutOfBoundsException")
/// and creates the corresponding Java exception object. Unknown panics become
/// `InternalError` so they don't crash the VM.
pub fn jit_panic_to_exception(
    shared: &SharedVm,
    thread: &mut JvmThread,
    payload: Box<dyn std::any::Any + Send>,
) -> MethodCallFailed {
    let msg = if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        *s
    } else {
        "unknown JIT panic"
    };

    // Parse ArrayIndexOutOfBoundsException
    if msg.starts_with("ArrayIndexOutOfBoundsException") {
        let error = RuntimeError::aioobe_index_only(parse_aioobe_index(msg));
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Parse NullPointerException
    if msg.contains("NullPointerException") {
        let error = RuntimeError::NullPointerException {
            message: Some(msg.to_string()),
        };
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Parse ClassCastException
    if msg.contains("ClassCastException") {
        let error = RuntimeError::ClassCastException {
            message: msg.to_string(),
        };
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Parse StackOverflowError
    if msg.contains("StackOverflow") {
        let error = RuntimeError::StackOverflowError;
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Parse ArithmeticException (e.g. division by zero)
    if msg.contains("ArithmeticException") {
        let error = RuntimeError::ArithmeticException {
            message: msg.to_string(),
        };
        return crate::runtime::exceptions::throw_runtime_error(shared, thread, error);
    }

    // Unknown panic — wrap as InternalError (non-catchable)
    MethodCallFailed::InternalError(VmError::Internal {
        message: format!("JIT panic: {msg}"),
    })
}

/// Resolve an inline site for a callee method.
///
/// Returns `Some(InlineSite)` if the callee is eligible for inlining:
/// - Bytecode length <= MAX_INLINE_BYTECODE_SIZE (35)
/// - No exception handlers, not synchronized
/// - No unsupported bytecodes (new, checkcast, instanceof, invoke*, etc.)
///
/// Resolution starts at the CONSTANT-POOL class, which is the right answer for
/// `invokestatic`/`invokespecial` and the wrong one for a guarded virtual or
/// interface site — see [`resolve_receiver_inline_site`].
/// The plan-time direct-bind resolver an inline site consults for the calls
/// inside the body it is about to splice.
///
/// Exactly the closure shape the top-level `direct_calls` planning already uses
/// — `callee_compiler` on the mutator door, `direct_callee_lookup` on the
/// background one — so a spliced call inherits every one of their refusal gates
/// unchanged: FJP blocklist, native shadow, callee exception table,
/// `synchronized`, JVMS §5.5 static-init, indy trap, and the eager-callee-chain
/// depth / cycle / fan-out bounds. Answers `(compiled entry, needs context)`.
pub(super) type InlineDirectBind<'a> = &'a dyn Fn(&str, &str, &str) -> Option<(usize, bool)>;

pub(super) fn resolve_inline_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    direct_bind: Option<InlineDirectBind<'_>>,
) -> Option<cratonvm_jit::InlineSite> {
    resolve_inline_site_from(
        shared,
        requesting_class_id,
        None,
        callee_class,
        callee_method,
        callee_desc,
        0,
        direct_bind,
        false,
    )
}

/// Resolve a body for the **optimizing (IR) tier** to splice — a different
/// admission set from the single-pass one [`resolve_inline_site`] serves, in
/// both directions.
///
/// WIDER, because the refusals the single-pass path carries are its EMITTER's
/// limits, not resolution's: `x64::try_emit_inline_body` has no arm for `new`,
/// for an array load or store, or for `arraylength`, so the resolver refuses
/// those shapes to keep planning and emission consistent. `IrBuilder` lowers
/// all of them natively, and `new` is the entire point — an accessor that
/// allocates is exactly the body escape analysis has to see inside its caller.
///
/// NARROWER, because the IR splice re-executes the whole `invoke` on a deopt
/// (see `ir::IrInlineSite`) and because `IrBuilder` walks a relocated body with
/// no merge bookkeeping of its own. So v1 additionally refuses:
///
///  * any branch — a relocated body's merges and loop headers are computed
///    over the CALLER's code alone, and there is no second walker;
///  * `idiv`/`irem`/`ldiv`/`lrem` — the one guard `IrBuilder` emits is the
///    div-zero guard, and a guard inside a spliced region would deopt to a
///    re-execution point rather than to itself;
///  * `ldc`/`ldc2_w` — `InlineSite` records a raw `i64` where the builder needs
///    the value plus its float/double discriminator, and inventing that bit is
///    how a `long` constant becomes a `double`;
///  * more than one return, or a return that is not the last instruction;
///  * any target that is not provably monomorphic (see the `ir_mode` check on
///    the selected method) — the IR tier has no class-id guard node, so a
///    speculative splice has nothing to fall back to.
pub(super) fn resolve_ir_inline_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    direct_bind: Option<InlineDirectBind<'_>>,
) -> Option<cratonvm_jit::InlineSite> {
    resolve_inline_site_from(
        shared,
        requesting_class_id,
        None,
        callee_class,
        callee_method,
        callee_desc,
        0,
        direct_bind,
        true,
    )
}

/// Resolve the body a receiver of EXACTLY `receiver_class_id` dispatches to at
/// a call site declared `(cp_class, callee_method, callee_desc)` — PGO-02's
/// guarded-inline resolver.
///
/// A guarded inline compares the receiver's class id against a class taken
/// from the receiver-type profile, then runs a spliced body. Those two only
/// agree if the body is the one that class actually dispatches to. Resolving
/// from `cp_class` instead — the receiver expression's STATIC type — splices
/// the superclass's method behind a guard that just certified the subclass,
/// which is silent wrong code at every overriding site. Starting the JVMS
/// selection walk at the runtime receiver is also what gives `invokeinterface`
/// any reach at all: an interface's own declaration has no `Code`.
///
/// Fail-closed on every shape where "the method found by walking up from the
/// receiver" might NOT be the method real dispatch selects:
///
/// * the receiver class must be a loaded, non-interface, non-array class;
/// * the selected method must not be `private` (a private method is never
///   inherited, so a walk that finds one from a subclass receiver found
///   something dispatch would not);
/// * and if the selected method is declared somewhere OTHER than where the
///   constant-pool reference resolves, it must be genuinely an override:
///   `public`/`protected`, or package-private within the same runtime package.
///   A package-private method in a different package does NOT override
///   (JVMS §5.4.5), and the walk cannot tell the difference on its own.
pub(super) fn resolve_receiver_inline_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    receiver_class_id: u32,
    cp_class: &str,
    callee_method: &str,
    callee_desc: &str,
    direct_bind: Option<InlineDirectBind<'_>>,
) -> Option<cratonvm_jit::InlineSite> {
    resolve_inline_site_from(
        shared,
        requesting_class_id,
        Some(ClassId::new(receiver_class_id)),
        cp_class,
        callee_method,
        callee_desc,
        0,
        direct_bind,
        false,
    )
}

/// Shared core of [`resolve_inline_site`] and [`resolve_receiver_inline_site`].
///
/// `receiver_class_id` selects which of the two contracts applies: `None`
/// resolves from `callee_class` (constant-pool resolution), `Some` starts the
/// selection walk at that runtime class and applies the override-legality
/// checks documented on [`resolve_receiver_inline_site`].
///
/// `nest_depth` is how many splices already enclose this one: `0` for a site
/// spliced directly into a compiled method, `1` for a body spliced into that
/// body, and so on. It bounds the RECURSION this function performs on its own
/// callee's calls (`cratonvm_jit::MAX_INLINE_NEST_DEPTH`) — see the nested-site
/// resolution near the end.
fn resolve_inline_site_from(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    receiver_class_id: Option<ClassId>,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    nest_depth: usize,
    direct_bind: Option<InlineDirectBind<'_>>,
    // Resolve for the optimizing (IR) tier rather than the single-pass emitter.
    // See [`resolve_ir_inline_site`] for what the two admission sets differ on
    // and why. Threaded into the nested resolution below so a nested body is
    // held to the same contract as the one enclosing it.
    ir_mode: bool,
) -> Option<cratonvm_jit::InlineSite> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;

    // Name every refusal. This function has two dozen `return None`s and a
    // caller that can only see "refused"; two build cycles were spent today
    // guessing which one fired.
    macro_rules! no {
        ($why:expr) => {{
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] inline-resolve REFUSED {}.{}{} depth={}: {}",
                    callee_class, callee_method, callee_desc, nest_depth, $why
                );
            }
            return None;
        }};
    }

    // A registered native shadows the classfile body. Inlining that bytecode
    // would bypass the native completely, just as compiling the method itself
    // would. This must precede even the "tiny constructor" path below:
    // ConcurrentHashMap.<init>()V is only five bytes, but CratonVM's native
    // constructor installs the segmented backing store. Background C1 used to
    // inline the empty-looking JDK body, so a JIT-created map silently dropped
    // every put after tier-up. The callee compiler already has this own-class
    // guard; keep the inline resolver aligned with it.
    //
    // CONSTANT-POOL RESOLUTION ONLY. `callee_class` is the DECLARED class, and
    // for a receiver-resolved site that is a supertype which may own no body at
    // all — `java/lang/Object` for an `equals` call site, say. Refusing there
    // refuses every override too, including the plain-bytecode one the receiver
    // actually dispatches to.
    //
    // Measured 2026-08-18: this is what stopped devirtualisation inside a
    // splice from ever firing. The MIC evidence named the receiver class,
    // resolution found its real `equals` override, and the site was still
    // refused — `inline-resolve REFUSED java/lang/Object.equals: native-shadow`
    // — because `java/lang/Object.equals` has a registered native and that is
    // the name the constant pool carries. `objectsAreEqual` then refused in
    // turn, because a call in it was "neither spliced nor direct-bound".
    //
    // The SELECTED method is checked below against its own declaring class,
    // unconditionally, which is the precise form of this rule.
    if receiver_class_id.is_none()
        && shared
            .natives
            .native_methods
            .find(callee_class, callee_method, callee_desc)
            .is_some()
    {
        no!("native-shadow");
    }

    // Taken BEFORE the class-manager guard. Holding two of this subsystem's
    // locks at once is a lock-order obligation, and every other reader of
    // `lambda_proxies` here takes it without `class_manager` held; a proxy's
    // dispatch is synthesised elsewhere entirely, so one is never inlineable.
    if let Some(receiver_id) = receiver_class_id {
        if shared
            .classes
            .lambda_proxies
            .read()
            .contains_key(&receiver_id)
        {
            no!("receiver-is-lambda-proxy");
        }
    }

    let cm = shared.classes.class_manager.read();
    let Some(cp_class_id) = cm.find_class_by_name_for_class(callee_class, requesting_class_id)
    else {
        no!("cp-class-not-loaded");
    };
    let store = cm.class_store();
    // Where the JVMS method-selection walk starts. For a guarded site that is
    // the RUNTIME receiver class; `find_method_recursive` performs the
    // maximally-specific default-method selection only when handed the
    // receiver, which is the same reason the interpreter's own dispatch
    // redirects to the receiver id for interface calls.
    let search_start = receiver_class_id.unwrap_or(cp_class_id);
    if let Some(receiver_id) = receiver_class_id {
        let Some(receiver) = store.get(receiver_id) else {
            no!("receiver-class-not-in-store");
        };
        // A guard admits an EXACT class, so an interface or an array class is
        // never a class a receiver can have here.
        if receiver.is_interface() || receiver.name.starts_with('[') {
            no!("receiver-is-interface-or-array");
        }
    }
    let Some((method, declaring_id)) =
        crate::classloading::find_method_recursive(search_start, callee_method, callee_desc, store)
    else {
        no!("method-not-found-from-search-start");
    };
    if let Some(receiver_id) = receiver_class_id {
        if !receiver_resolution_is_dispatch_faithful(
            store,
            receiver_id,
            cp_class_id,
            declaring_id,
            method,
            callee_method,
        ) {
            no!(format!(
                "receiver-resolution-not-dispatch-faithful (selected on class id {})",
                declaring_id.as_u32()
            ));
        }
    }
    // The same rule, applied to the method actually SELECTED rather than the
    // one the constant pool names. Resolution may start at a subclass while the
    // executable override is registered on the declaring class, and it may
    // equally start at a supertype whose own method is native while the
    // receiver's override is ordinary bytecode.
    //
    // UNCONDITIONAL, where it used to be guarded by
    // `declaring_class_name != callee_class`. That guard was load-bearing only
    // because the early check above covered the equal case; now that the early
    // check runs for constant-pool resolution only, this one has to cover both
    // — otherwise a receiver-resolved site whose selection lands back on the
    // declared class would splice bytecode a native shadows. Checking exactly
    // the declaring class still permits a real bytecode override on an
    // intermediate subclass, matching `try_jit_compile_callee_slow`.
    let Some(declaring_class_name) = store.get(declaring_id).map(|c| &*c.name) else {
        no!("declaring-class-not-in-store");
    };
    // IR-tier splices are UNGUARDED: `IrBuilder` has no class-id compare and no
    // miss edge to send a wrong receiver down, so the target must be the only
    // body a call at this site can ever reach. `invokespecial`/`invokestatic`
    // are that by definition; a virtual site qualifies only when the language
    // has already ruled out an override.
    //
    // Deliberately NOT a CHA-style "no loaded subclass overrides it" answer:
    // that is true until the next class loads, so it needs an invalidation
    // dependency the IR path does not record. `final` is monotone.
    if ir_mode {
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        let monomorphic = method.is_static()
            || callee_method == "<init>"
            || method
                .access_flags
                .intersects(MethodAccessFlags::PRIVATE | MethodAccessFlags::FINAL)
            || store
                .get(declaring_id)
                .map(|c| {
                    c.access_flags
                        .contains(cratonvm_reader::class_access_flags::ClassAccessFlags::FINAL)
                })
                .unwrap_or(false);
        if !monomorphic {
            no!("ir-splice-target-not-provably-monomorphic");
        }
    }
    if shared
        .natives
        .native_methods
        .find(declaring_class_name, callee_method, callee_desc)
        .is_some()
    {
        no!("native-shadow-on-selected-method");
    }
    // ...and the same rule again, over the whole receiver-to-declaring chain.
    //
    // # Why the declaring class alone is not enough
    //
    // A native is registered on the class the RECEIVER actually has, and the
    // method it shadows is very often DECLARED on a superclass. The two
    // screens above ask about the constant-pool class and the declaring class,
    // and a guarded virtual site has neither: it starts the selection walk at
    // the runtime receiver, `find_method_recursive` returns the first concrete
    // body it meets, and that body's declaring class is where the screen then
    // looks — one or more classes ABOVE the one carrying the native.
    //
    // Measured 2026-09-04, `probes/TreeTailIterProbe.java` with
    // `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=1`: a compiled
    // `for (e : treeMap.tailMap(k).entrySet())` iterates ZERO entries while
    // `entrySet().size()` on the same object answers 6. The single spliced
    // site is `java/util/Iterator.hasNext()Z`, guarded on
    // `java/util/TreeMap$EntryIterator` -- which has
    // `native_al_itr_has_next` registered on it by the `VALUES_ITR_CARRIERS`
    // loop. But `hasNext` is DECLARED on `java/util/TreeMap$PrivateEntryIterator`,
    // which carries no native, so `declaring_class_name` above cleared the
    // screen and the splice ran the real JDK body -- `return next != null` over
    // a `next` field a natively-managed iterator never populates. False, every
    // time, from the first compiled call.
    //
    // Walking the chain is the precise form of the rule the two screens above
    // state, because it asks the question DISPATCH asks: not "does the class
    // that wrote this method have a native" but "does any class this receiver
    // IS have one". Bounded by `declaring_id` -- past it the body is not the
    // one being spliced -- and short in practice.
    if let Some(receiver_id) = receiver_class_id {
        let mut walk = Some(receiver_id);
        while let Some(cid) = walk {
            let Some(class) = store.get(cid) else { break };
            if shared
                .natives
                .native_methods
                .find(&*class.name, callee_method, callee_desc)
                .is_some()
            {
                no!(format!(
                    "native-shadow-on-receiver-chain (registered on {}, declared on {})",
                    class.name, declaring_class_name
                ));
            }
            if cid == declaring_id {
                break;
            }
            walk = class.superclass;
        }
    }
    // The class the SPLICED BODY belongs to, which is what an invalidation
    // dependency must name. For a constant-pool resolution this stays the
    // declared name (unchanged behaviour); for a receiver resolution the
    // declared name is a supertype that may own no body at all, so naming it
    // would record a dependency on a class whose redefinition cannot affect
    // the code, and miss the one whose redefinition can.
    let inlined_body_class_name = if receiver_class_id.is_some() {
        declaring_class_name.to_string()
    } else {
        callee_class.to_string()
    };
    // ...and its id, chosen by the SAME branch so the two can never name
    // different classes. A stack walk that has to answer in `ClassId` -- the
    // JEP 403 deep-reflection gate, `Class.forName`'s caller loader -- can then
    // see a spliced frame without resolving a JIT label by name, which would be
    // a guess in a security-relevant path. `cp_class_id` is the id
    // `callee_class` was looked up under; `declaring_id` is the class that
    // declares the body a receiver resolution selected.
    let inlined_body_class_id = if receiver_class_id.is_some() {
        declaring_id.as_u32()
    } else {
        cp_class_id.as_u32()
    };

    if method.is_synchronized() {
        no!("synchronized");
    }
    let Some(code_attr) = method.code() else {
        no!("selected-method-has-no-code");
    };
    let code_len = code_attr.code.len();
    if code_len > cratonvm_jit::MAX_INLINE_BYTECODE_SIZE {
        no!("too-large");
    }
    if !code_attr.exception_table.is_empty() {
        no!("callee-exception-table");
    }
    let is_static = method.is_static();
    // jit-inline-clinit-gap fix (2026-07-17): inlining a static method's
    // bytecode splices it directly into the caller with NO call boundary at
    // all -- not even the `direct_calls` raw CALL that `callee_compiler`
    // (this file, the sibling closure guarding `direct_calls`) gates on
    // class-init state. A callee whose OWN body never touches its class's
    // statics (no getstatic/putstatic/new of its own -- the existing
    // "conservative default: do NOT inline getstatic/putstatic-bearing
    // callees" gate a few lines below only excludes the OPPOSITE shape)
    // gives the JIT no code-level opportunity whatsoever to run `<clinit>`
    // before the inlined body executes, silently violating the same JVMS
    // §5.5 "class must be initialized before first invocation of any of its
    // static methods" requirement `callee_compiler`'s gate enforces for the
    // direct-call path. A class's initialized state is monotonic (JVMS:
    // once Initialized, always Initialized), so -- exactly like
    // `callee_compiler`'s check -- checking ONCE, here, at inline-planning
    // time is sound forever for this call site: only admit a static
    // callee for inlining when its declaring class is ALREADY initialized;
    // otherwise return `None`, which drops the site to the ordinary
    // dispatch/direct-call resolution below (both of which independently
    // enforce initialization). `cm`'s read guard is still live here.
    if is_static {
        let declaring_class_initialized = store
            .get(declaring_id)
            .map(crate::vm::is_class_initialized_fast)
            .unwrap_or(false);
        if !declaring_class_initialized {
            no!("static-declaring-class-not-initialized");
        }
    }
    let callee_max_locals = code_attr.max_locals as usize; // Widening: u16 to usize
    let code_bytes = code_attr.code.clone();

    let code = &code_bytes;
    let mut scan_pc = 0;
    let mut has_field_ops = false;
    let mut has_static_field_ops = false;
    let mut has_ldc = false;
    let mut has_ldc2w = false;
    // Conservative default: do NOT inline getstatic/putstatic-bearing callees.
    // A decisive bisect showed that excluding them makes main-path inlining
    // correct (Spring `S01_Context` 15/15, EnumSet.noneOf clean), where leaving
    // them in miscompiled `EnumSet.getUniverse` (inlined
    // `SharedSecrets.getJavaLangAccess()` getstatic feeding `getEnumConstantsShared`
    // → null → `ClassCastException: … not an enum`). The isolated getstatic-inline
    // patterns (cross-class, non-zero field index, interface-typed, lazy-set) all
    // compile correctly, so the failing interaction is narrower than "all
    // getstatic" — but excluding them is the safe conservative fix until it is
    // isolated. `CRATONVM_INLINE_ALLOW_STATIC=1` re-enables them for debugging.
    let inline_no_static = !crate::runtime::env_cache::inline_allow_static();
    // invokespecial sites deferred for elidability validation once the
    // callee's constant pool is available below: (callee_pc, cp_idx). Only
    // no-op super-constructor calls (`java/lang/Object.<init>()V` directly,
    // or a target passing `is_elidable_construction`) survive — anything
    // else rejects the whole site. This is what admits CONSTRUCTOR bodies
    // (which always begin `aload_0; invokespecial super.<init>`) to
    // inlining; the blanket 0xb7 rejection made every ctor un-inlineable,
    // so each `new C(args)` paid a full dispatch round trip per allocation.
    let mut special_sites: Vec<(usize, u16)> = Vec::new();
    // Calls the callee body makes, deferred exactly like `special_sites`
    // because resolving them needs the CALLEE's constant pool, which is only
    // reachable once `callee_class_info` is fetched below:
    // `(callee_pc, cp_idx, opcode)`.
    //
    // Gated: with `CRATONVM_JIT_INLINE_CALLS` off every `invoke*` still
    // rejects the whole site outright, which is the behaviour every release
    // before this one had. The gate is read ONCE here rather than per-opcode so
    // a site cannot be admitted under one answer and emitted under another.
    let inline_calls_allowed = crate::runtime::env_cache::jit_inline_calls();
    let mut invoke_sites: Vec<(usize, u16, u8)> = Vec::new();
    // IR-tier only: `new` sites in the body, `(callee_pc, cp_idx)`, resolved to
    // `(class_id, num_fields)` below once the callee's constant pool is in hand.
    // This is the row whose absence bailed `VolumeShort2.loadFromArray` out of
    // the IR tier altogether, and with it out of escape analysis.
    let mut ir_new_sites: Vec<(usize, u16)> = Vec::new();
    // IR-tier only: `(pc, cp_idx, is_checkcast)` for the body's type checks.
    let mut ir_typecheck_sites: Vec<(usize, u16, bool)> = Vec::new();
    // IR-tier only: pcs of the body's return opcodes. A relocated body is walked
    // straight through with no merge bookkeeping, so exactly one return, at the
    // end, is the shape the splice can honour.
    let mut ir_return_pcs: Vec<usize> = Vec::new();
    while scan_pc < code_len {
        match code[scan_pc] {
            0xaa | 0xab => no!("tableswitch/lookupswitch"),
            // `new` — refused for the single-pass emitter, which has no arm for
            // it; admitted (and resolved) for the IR builder, which lowers it
            // to `Op::New` for escape analysis to delete.
            0xbb if ir_mode => {
                if scan_pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                ir_new_sites.push((scan_pc, cp_idx));
                scan_pc += 3;
                continue;
            }
            0xbb | 0xbd | 0xc5 => no!("new/anewarray/multianewarray"),
            0xbf => no!("athrow"),
            // `checkcast` / `instanceof`. Refused for both tiers until
            // 2026-09-09, and for the optimizing tier the refusal was the same
            // shape as `ir-splice-static-field`'s: `IrBuilder` has had both
            // arms since cov-05, keyed by pc off `checkcast_info` /
            // `instanceof_info`, and nothing rebased a spliced body's rows into
            // them. The survey that motivated those arms counted 306 events on
            // this pair -- the largest single whole-method refusal, more than
            // every opcode gap combined -- because every typed read out of an
            // untyped container is a `checkcast`.
            //
            // Unlike `getstatic`, the rows are RESOLVED here rather than
            // already carried: `InlineSite` had no typecheck field, and the
            // target class can only be named through the CALLEE's constant
            // pool. See the resolution below, which refuses the body when a
            // target is not loaded rather than admitting it with rows missing:
            // a missing row bails the whole METHOD, so the caller would lose
            // its optimizing compile over a callee it merely wanted inlined.
            //
            // The single-pass emitter still has no arm for either, so its
            // refusal is unchanged.
            0xc0 | 0xc1 if ir_mode => {
                if !cratonvm_jit::ir::ir_splice_typecheck_enabled() {
                    no!("ir-splice-typecheck");
                }
                if scan_pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                ir_typecheck_sites.push((scan_pc, cp_idx, code[scan_pc] == 0xc0));
                scan_pc += 3;
                continue;
            }
            0xc0 | 0xc1 => no!("checkcast/instanceof"),
            0xc2 | 0xc3 => no!("monitorenter/monitorexit"),
            // A relocated body's control flow would need the merge/loop-header
            // bookkeeping `IrBuilder` computes over the CALLER's code alone.
            // Straight-line only, v1.
            // `jsr` / `ret` / `jsr_w` — subroutines. Refused unconditionally
            // and separately from the ordinary branches below: `IrBuilder` has
            // no lowering for a return address, and the verifier's CFG for one
            // is not the shape `normally_reachable_pcs` walks. No JDK-9+
            // compiler emits them.
            0xa8 | 0xa9 | 0xc9 if ir_mode => no!("ir-splice-subroutine"),
            // Ordinary intra-body control flow — every `if`, `goto` and
            // `goto_w`. Refused until 2026-09-09 with the note "straight-line
            // only, v1", and the missing piece was one pre-scan: the builder
            // ran the verifier's CFG analysis over the CALLER's bytecode alone,
            // so a relocated body's merge targets and loop headers were invisible
            // to it. `IrBuilder::build` now runs the same analysis over each
            // spliced body and rebases the result — see its `ir_splice_branch_enabled`
            // block, which reads the SAME switch this arm does.
            //
            // The cost of the refusal was not marginal: a callee with an `if`
            // is most callees. On a four-callee probe it refused the one
            // remaining body after the `ldc` refusal was lifted.
            //
            // Still refused, by the check further down and independently of
            // this: a body with more than one `return`, or a `return` that is
            // not its last instruction. Branching bodies that funnel to a
            // single trailing return — a ternary, an accumulate-then-return, a
            // loop — are what this admits.
            0x99..=0xa7 | 0xc6 | 0xc7 | 0xc8
                if ir_mode && !cratonvm_jit::ir::ir_splice_branch_enabled() =>
            {
                no!("ir-splice-branch")
            }
            // The only guard `IrBuilder` emits is div-zero, and a guard inside a
            // spliced region deopts to "re-execute the invoke" rather than to
            // itself. Keeping division out means a spliced region carries no
            // guard of its own at all.
            0x6c | 0x6d | 0x70 | 0x71 if ir_mode => no!("ir-splice-division"),
            // `ldc` / `ldc_w` / `ldc2_w` were refused here until 2026-09-09,
            // and the reason was plumbing rather than modelling: `InlineSite`
            // recorded a raw `i64` and dropped the float/double tag the builder
            // needs to choose between `Op::Const` and `Op::ConstF`. The tag now
            // rides along in `InlineSite::ldc_fp_pcs` and
            // `append_ir_inline_site` rebases both tables into
            // `IrInlineTables`, so the builder's own `0x12 | 0x13` and `0x14`
            // arms resolve a spliced constant exactly as they resolve one in
            // the caller's own code.
            //
            // The refusal was expensive out of all proportion to its cause: a
            // constant wider than `sipush` is ordinary Java, and on a
            // four-callee probe this term alone refused two of the four. What
            // is still refused is what the RESOLVER refuses — an `ldc` naming a
            // String, a Class, a MethodHandle, a MethodType or a condy site,
            // each of which has resolution side effects (interning, class
            // loading, `<clinit>`) that a spliced immediate would skip.
            0x12 | 0x13 | 0x14 if ir_mode && !ir_splice_ldc_enabled() => no!("ir-splice-ldc"),
            0xac..=0xb1 if ir_mode => {
                ir_return_pcs.push(scan_pc);
                scan_pc += 1;
                continue;
            }
            // Array element access and `arraylength` are refused below because
            // `try_emit_inline_body` bails on them. `IrBuilder` lowers all three
            // with their own bounds checks, and `Short2` keeps its two shorts in
            // a `short[2]`, so a splice that could not read one would stop at
            // the accessor it exists to delete.
            0x2e..=0x35 | 0x4f..=0x56 | 0xbe if ir_mode => {
                scan_pc += 1;
                continue;
            }
            // `getstatic` was refused here until 2026-09-09, with the note
            // "no `static_field_info` is rebased into the builder's tables, so
            // a spliced `getstatic` would find no row and bail the whole
            // method AFTER the walk had committed to the body". That was an
            // accurate description of the plumbing and not of any modelling
            // problem: `static_field_info` below has resolved these rows for
            // the single-pass inliner since it existed, and
            // `append_ir_inline_site` now rebases them into
            // `IrInlineTables::static_field_info`, which the builder's own
            // `0xb2` arm reads exactly as it reads a caller's site.
            //
            // The refusal was expensive out of proportion to its cause, in the
            // same way the `ldc` one was: `getstatic` is the single largest
            // opcode in the ir-coverage survey (92 of 273 events), because a
            // static-table read behind an accessor is what framework code is
            // mostly made of.
            //
            // `putstatic` (0xb3) stays refused, and this one IS modelling. The
            // builder has no arm for it at all, and a static reference write
            // owes an SATB pre-barrier that lives on the single-pass
            // `jit_putstatic_*` path — statics are a Rust-side table, not the
            // heap, so no collector `set_field` barrier covers them.
            0xb3 if ir_mode => no!("ir-splice-putstatic"),
            //
            // Handled here rather than by falling through to the ordinary
            // `0xb2 | 0xb3` arm below, because that arm is guarded by
            // `inline_no_static` — `CRATONVM_INLINE_ALLOW_STATIC`, which is
            // OFF by default. That switch belongs to the single-pass inline
            // mini-emitter (`x64/inlining.rs`), which materialises a static
            // read as a baked address and is the reason the gate exists. The
            // optimizing tier does not go through that emitter at all: its
            // `0xb2` arm builds an `Op::LoadStatic` whose lowering picks
            // between the direct load and `helpers.getstatic` on the
            // resolver's own already-initialised answer. Falling through would
            // have made this feature a no-op under its own default and left
            // the refusal UNNAMED, which is the failure the `no!` macro at the
            // top of this function exists to prevent.
            0xb2 if ir_mode => {
                if !cratonvm_jit::ir::ir_splice_getstatic_enabled() {
                    no!("ir-splice-static-field");
                }
                has_static_field_ops = true;
                scan_pc += 3;
                continue;
            }
            // invokevirtual / invokestatic / invokeinterface inside the
            // spliced body. These used to reject the site outright — the
            // emitter had no arm for them and, more fundamentally, nothing
            // resolved them: every other piece of callee metadata is keyed by
            // callee pc against the CALLEE's constant pool, and invokes had no
            // such entry. Both halves exist now (`invoke_targets` here,
            // `try_emit_inline_body`'s invoke arm there), so record the site
            // and resolve it below.
            //
            // `invokeinterface` is FIVE bytes (cp_idx, count, 0); the other two
            // are three. Advancing by the wrong width would desync the scan and
            // read operands as opcodes, which is the same class of bug the
            // `wide`-aware length below exists to prevent.
            0xb6 | 0xb8 | 0xb9 => {
                if !inline_calls_allowed {
                    return None;
                }
                let width = if code[scan_pc] == 0xb9 { 5 } else { 3 };
                if scan_pc + width > code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                invoke_sites.push((scan_pc, cp_idx, code[scan_pc]));
                scan_pc += width;
                continue;
            }
            0xb7 => {
                // invokespecial — defer: elidable no-op super-ctor calls are
                // allowed (validated below), everything else rejects.
                if scan_pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                special_sites.push((scan_pc, cp_idx));
                scan_pc += 3;
                continue;
            }
            0xba => no!("invokedynamic"),
            // Array loads/stores + arraylength need a bounds check (and AIOOBE
            // path) that the inline codegen (`x64::try_emit_inline_body`) does
            // NOT emit — it bails on these. Rejecting them HERE keeps the
            // resolver and codegen consistent: a method that would only bail
            // mid-inline instead stays on the cheaper direct-call path rather
            // than being planned, rolled back, and downgraded to the
            // dispatch-helper fallback. (Array-load inlining is a follow-up.)
            0x2e..=0x35 => no!("array-load"),
            0x4f..=0x56 => no!("array-store"),
            0xbe => no!("arraylength"),
            0xb4 | 0xb5 => {
                has_field_ops = true;
                scan_pc += 3;
                continue;
            }
            0xb2 | 0xb3 => {
                if inline_no_static {
                    return None;
                }
                has_static_field_ops = true;
                scan_pc += 3;
                continue;
            }
            0x12 => {
                has_ldc = true;
                scan_pc += 2;
                continue;
            }
            0x13 => {
                has_ldc = true;
                scan_pc += 3;
                continue;
            }
            0x14 => {
                has_ldc2w = true;
                scan_pc += 3;
                continue;
            }
            _ => {}
        }
        // `wide`-aware length so the walk stays in sync over a `wide iinc`
        // (6 bytes) — the bare opcode-length table treats every `wide` form as
        // 4 bytes, which would desync the scan and mis-read the widened
        // operands as opcodes (finding 5).
        scan_pc += inline_instr_length(code, scan_pc);
    }

    // The relocated body is walked from its first byte through to `code_len`
    // with the caller's frame parked in `SpliceFrame`, so the LAST instruction
    // must be a `return` — otherwise the walk runs off the end of the body into
    // whatever `lib.rs` appended next.
    //
    // More than one return was refused outright until 2026-09-09, on the
    // grounds that the walk leaves the splice at the first one and would never
    // reach the code after it. It no longer leaves: `IrBuilder::splice_return`
    // turns each return into an edge into a continuation built at the body's
    // end (`finish_multi_return_splice`), which is the ordinary
    // several-predecessors join the builder already performs for a caller's own
    // branches. Default OFF — `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1` lifts the
    // refusal, and `ir_splice_multi_return_enabled` says why it is not lifted
    // by default (it is neutral on throughput and forfeits the optimizing OSR
    // door while `emit_osr_entry_stubs` is unfixed);
    // the builder reads the SAME switch, and neither half may be flipped alone.
    //
    // Every `return` opcode is one byte, so "last instruction" is exactly
    // `last + 1 == code_len` with no length table needed.
    let multi_return_ok = ir_mode && cratonvm_jit::ir::ir_splice_multi_return_enabled();
    if ir_mode
        && (ir_return_pcs.is_empty()
            || ir_return_pcs[ir_return_pcs.len() - 1] + 1 != code_len
            || (!multi_return_ok && ir_return_pcs.len() != 1))
    {
        no!("ir-splice-not-single-trailing-return");
    }

    // Resolve the body's `new` sites against the CALLEE's constant pool. A
    // `Deferred` site — the target class not loaded from this holder's loader
    // yet — refuses the whole splice rather than being dropped: the builder's
    // `0xbb` arm bails the METHOD on a missing row, so admitting the body
    // without the row would cost the caller its IR compile entirely.
    let mut ir_new_info: Vec<(usize, u32, usize)> = Vec::new();
    if ir_mode && !ir_new_sites.is_empty() {
        for &(npc, cp_idx) in &ir_new_sites {
            match resolve_jit_new_site(&cm, declaring_id, cp_idx) {
                Some(cratonvm_jit::JitNewSite::Resolved {
                    class_id,
                    num_fields,
                    ..
                }) => ir_new_info.push((npc, class_id, num_fields)),
                _ => no!("ir-splice-new-site-unresolved"),
            }
        }
    }

    let Some(callee_class_info) = cm.get_class(declaring_id) else {
        no!("declaring-class-info-unavailable");
    };

    // Resolve the body's `checkcast` / `instanceof` targets against the
    // CALLEE's constant pool -- the only pool that can name them.
    //
    // Admitted only when the target class is already RESOLVED AND LOADED, which
    // is the same bar the caller's own sites are held to and for the same
    // reason: the not-yet-loaded path runs `jit_typecheck_resolve`, which can
    // call a user classloader's `loadClass`, arbitrary Java this tier does not
    // host inside a helper call. `resolve_jit_new_site` answers exactly that
    // question for a `CONSTANT_Class` entry, which is the shape both opcodes
    // take, so this reuses it rather than adding a second resolver; the field
    // count and init flags it also carries are irrelevant here.
    //
    // An unresolved target refuses the whole CALLEE, the same trade as
    // `ir-splice-new-site-unresolved`: a missing row bails the METHOD, so
    // admitting the body without one costs the CALLER its optimizing compile
    // over a callee it merely wanted inlined. Refusing costs the site its
    // inline and nothing else.
    //
    // The builder's `0xc0`/`0xc1` arms appear to be gentler -- a missing row
    // reaches `plant_uncommon_trap`. It is gated off by default
    // (`ir_unresolved_class_trap_enabled`), so the plant refuses and the arm
    // bails; and with it ON the trap fires and, inside a splice, deopts to
    // re-execute the invoke on every call. Neither setting makes admitting an
    // unresolved body the right move.
    let mut ir_typecheck_info: Vec<(usize, u32, String, bool)> = Vec::new();
    for &(tpc, cp_idx, is_checkcast) in &ir_typecheck_sites {
        let Some(cratonvm_jit::JitNewSite::Resolved {
            class_id: target_id,
            ..
        }) = resolve_jit_new_site(&cm, declaring_id, cp_idx)
        else {
            no!("ir-splice-typecheck-target-not-loaded");
        };
        let Some(name) = callee_class_info.constant_pool.get_class_name(cp_idx) else {
            no!("ir-splice-typecheck-target-unnamed");
        };
        ir_typecheck_info.push((tpc, target_id, name.to_string(), is_checkcast));
    }

    // Validate the deferred invokespecial sites: every one must be a
    // resolver-PROVEN no-op super-constructor call, or the whole callee is
    // rejected. Proven means the target is `java/lang/Object.<init>()V`
    // directly, or a `<init>()V` whose body is exactly
    // `aload_0; invokespecial Object.<init>; return`
    // (`is_elidable_construction` — the same predicate the elidable-ctor
    // call-site rewrite uses). The surviving PCs are recorded so the inline
    // body emitter pops the receiver and emits nothing at those sites.
    let mut elided_invoke_pcs: Vec<usize> = Vec::new();
    for &(spc, cp_idx) in &special_sites {
        let (ref_class_idx, nat_idx) = match callee_class_info.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let target_class = callee_class_info
            .constant_pool
            .get_class_name(ref_class_idx)?;
        let (target_name, target_desc) =
            callee_class_info.constant_pool.get_name_and_type(nat_idx)?;
        // An invokespecial that is NOT a provable no-op super-constructor call
        // used to reject the whole site, because the emitter's only 0xb7 arm is
        // the elision. With call splicing on there is a second arm — the
        // dispatch helper — so route it there instead of losing the site.
        let is_noop_ctor_shape = target_name == "<init>" && target_desc == "()V";
        let elidable = is_noop_ctor_shape
            && (target_class == "java/lang/Object" || {
                match cm.find_class_by_name_for_class(target_class, declaring_id) {
                    Some(tid) => is_elidable_construction(shared, &cm, tid),
                    None => false,
                }
            });
        if !elidable {
            if !inline_calls_allowed {
                return None;
            }
            invoke_sites.push((spc, cp_idx, 0xb7));
            continue;
        }
        elided_invoke_pcs.push(spc);
    }

    // Resolve the callee's own calls against the CALLEE's constant pool.
    //
    // Everything needed is textual — the name triple, the argument count and
    // the return byte — so this stays inside the `cm` guard with the field and
    // ldc scans; unlike `resolve_field_ref` it re-locks nothing. What it does
    // NOT do is find a body: `jit_invoke_dispatch` resolves the target at run
    // time from exactly these four fields, which is what makes a spliced call
    // behave identically to the same call in an un-spliced body (same handler,
    // same `<clinit>` barrier, same loader identity via `declaring_class_id`).
    //
    // `declaring_class_id` is the CALLEE's declaring class, not the enclosing
    // method's: a class NAME is not a class identity
    // (BUG-JIT-INVOKESPECIAL-LOADER-20260726), and the pool that named this
    // target belongs to the callee. Handing the dispatcher the enclosing
    // method's id would resolve a two-loader duplicate through the wrong copy.
    //
    // A site with even ONE unresolvable invoke is refused whole — the same
    // contract `ldc` follows ("the callee still compiles and is still CALLED,
    // it just is not spliced"), and the only alternative would be to splice a
    // body with a hole in it.
    let mut invoke_targets: Vec<(usize, cratonvm_jit::InlineInvokeTarget)> = Vec::new();
    for &(ipc, cp_idx, opcode) in &invoke_sites {
        let (ref_class_idx, nat_idx) = match callee_class_info.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            })
            | Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let target_class = callee_class_info
            .constant_pool
            .get_class_name(ref_class_idx)?;
        let (target_name, target_desc) =
            callee_class_info.constant_pool.get_name_and_type(nat_idx)?;
        let invoke_kind: u8 = match opcode {
            0xb6 => 0,
            0xb7 => 1,
            0xb9 => 2,
            0xb8 => 3,
            _ => return None,
        };
        // REFUSE the whole splice when the callee makes an FFM element access.
        //
        // `try_emit_inline_body`'s invoke arm lowers every call in a spliced
        // body to an ordinary dispatch — the intrinsic ladder does NOT run
        // inside an inlined body. So splicing a method that contains a
        // `MemorySegment.getAtIndex`/`setAtIndex` silently converts that site
        // from the ~11 ns/element fast path back to the ~1158 ns/element native
        // dispatch, and those accessors live in exactly the one-line wrappers
        // (`TornadoMemorySegment.getShortAtIndex`, `ShortArray.get`) an inliner
        // takes every time.
        //
        // Measured: with these still spliced, kfusion showed `fast_hits=0` and
        // no `getAtIndex` emitted at all; refusing the splice took the same run
        // to 40.4M hits against 6.0M misses. The trade is ONE compiled-to-
        // compiled call per element — a few nanoseconds — to keep a ~100x fast
        // path, and it is only taken for a descriptor the fast path will
        // actually emit (`ffm_kind_for_descriptor`, the same gate the emitter
        // asks), so a float carrier keeps being inlined as before.
        if matches!(opcode, 0xb6 | 0xb9)
            && target_class == "java/lang/foreign/MemorySegment"
            && matches!(target_name, "getAtIndex" | "setAtIndex")
            && cratonvm_jit::ffm_kind_for_descriptor(target_desc).is_some()
        {
            no!("ffm-accessor-keeps-its-own-fast-path");
        }
        // THE SAME REFUSAL, THE SAME REASON, FOR EVERY OTHER CALL THAT HAS ITS
        // OWN THIN DIRECT BIND.
        //
        // `try_compile_inner`'s direct-native ladder does not run inside a
        // spliced body — `try_emit_inline_body`'s invoke arm lowers every call
        // in one to an ordinary dispatch. The calls below are exactly the
        // one-line wrappers an inliner takes every time
        // (`PooledDirectByteBuf._setByte` is `memory.put(idx(index), value)`;
        // `AbstractByteBuf.ensureAccessible` bottoms out in one `VarHandle`
        // read), so splicing them converted each site from a ~10 ns helper back
        // to the ~160 ns generic native funnel.
        //
        // MEASURED, on `JdkZlibIntegrationTest#testHugeDecompress`
        // (`--dump-native-registry`, 369 s wall): **1.07 BILLION** funnel calls,
        // every one of them a spliced site whose bind never got the chance to
        // fire —
        //
        //     536 870 912  java/security/MessageDigest.update(B)V
        //     269 768 030  java/lang/invoke/VarHandle.get(...)
        //     268 435 456  java/nio/DirectByteBuffer.put(IB)...
        //
        // — against a `perf` profile that puts the funnel and its receiver
        // validation at ~45 % of the whole run. The trade is ONE
        // compiled-to-compiled call per operation for a ~16x funnel, and it is
        // taken only for the exact triples the ladder actually binds, so every
        // other call to these classes is inlined as before.
        if matches!(opcode, 0xb6 | 0xb9)
            && ((target_class == "java/nio/ByteBuffer"
                && ((target_name == "put" && target_desc == "(IB)Ljava/nio/ByteBuffer;")
                    || (target_name == "get" && target_desc == "(I)B")))
                || (target_class == "java/security/MessageDigest"
                    && target_name == "update"
                    && target_desc == "(B)V")
                || (target_class == "java/lang/invoke/VarHandle"
                    && (cratonvm_jit::varhandle_read_helper_slot(target_name, target_desc)
                        .is_some()
                        || cratonvm_jit::varhandle_write_helper_slot(target_name, target_desc)
                            .is_some())))
        {
            no!("thin-bound-native-keeps-its-own-fast-path");
        }
        // Receiver-included, one slot per parameter regardless of category —
        // the count `JitInvokeInfo::num_jit_args` carries and the count the
        // emitter pops, since the JIT operand stack holds one i64 per value.
        let num_jit_args = count_method_params(target_desc) + if opcode == 0xb8 { 0 } else { 1 };
        invoke_targets.push((
            ipc,
            cratonvm_jit::InlineInvokeTarget {
                class_name: target_class.to_string(),
                method_name: target_name.to_string(),
                descriptor: target_desc.to_string(),
                num_jit_args,
                return_type: cratonvm_jit::return_type(target_desc),
                invoke_kind,
                declaring_class_id: declaring_id.as_u32(),
                // Resolved below, once `cm` is dropped: the direct-bind
                // resolver takes `class_manager` itself (and may COMPILE the
                // callee), so calling it under this read guard is the same
                // self-deadlock the field-resolution phase is split out for.
                direct_entry: None,
            },
        ));
    }

    // Lock-order discipline (audit follow-up to the H2 ABBA fix): collect
    // the constant-pool facts for field ops HERE (they borrow `cm`), but
    // DELAY every `resolve_field_ref` call until `cm` is dropped below —
    // resolve_field_ref re-locks class_manager with a plain read() (wedges
    // behind any queued writer while we hold this read), can take
    // class_manager.write() via load_class_concurrent on a cold field
    // class (same-thread self-deadlock), and writes resolution_cache (the
    // cm→resolution_cache inversion).
    let mut field_sites: Vec<(usize, u16, u8)> = Vec::new();
    if has_field_ops {
        let mut fpc = 0;
        while fpc < code_len {
            if matches!(code[fpc], 0xb4 | 0xb5) && fpc + 2 < code_len {
                let cp_idx = ((code[fpc + 1] as u16) << 8) | code[fpc + 2] as u16; // Cast: bytecode operand decoding
                let nat_idx = match callee_class_info.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::FieldReference {
                        name_and_type_index,
                        ..
                    }) => *name_and_type_index,
                    _ => return None,
                };
                if let Some((_, desc)) = callee_class_info.constant_pool.get_name_and_type(nat_idx)
                {
                    let type_tag = *desc.as_bytes().first().unwrap_or(&b'L');
                    field_sites.push((fpc, cp_idx, type_tag));
                }
                fpc += 3;
            } else {
                fpc += inline_instr_length(code, fpc);
            }
        }
    }

    let mut static_sites: Vec<(usize, u16, u8)> = Vec::new();
    if has_static_field_ops {
        let mut fpc = 0;
        while fpc < code_len {
            if matches!(code[fpc], 0xb2 | 0xb3) && fpc + 2 < code_len {
                let cp_idx = ((code[fpc + 1] as u16) << 8) | code[fpc + 2] as u16; // Cast: bytecode operand decoding
                let nat_idx = match callee_class_info.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::FieldReference {
                        name_and_type_index,
                        ..
                    }) => *name_and_type_index,
                    _ => return None,
                };
                if let Some((_, desc)) = callee_class_info.constant_pool.get_name_and_type(nat_idx)
                {
                    let type_tag = *desc.as_bytes().first().unwrap_or(&b'L');
                    static_sites.push((fpc, cp_idx, type_tag));
                }
                fpc += 3;
            } else {
                fpc += inline_instr_length(code, fpc);
            }
        }
    }

    // `ldc` / `ldc_w` in an INLINE CANDIDATE.
    //
    // The inline mini-emitter (`x64/inlining.rs`) materialises these as bare
    // x86 immediates and has no path to `helpers.ldc_string` /
    // `helpers.ldc_class_cp`. So only the two constant kinds that ARE an
    // immediate — `Integer` and `Float` — can be recorded. Every other kind
    // (String, Class, MethodHandle, MethodType, condy) names a *reference*
    // materialised at run time, and there is no i64 that stands for it.
    //
    // This used to end `_ => 0`, which recorded a perfectly well-formed entry
    // claiming the constant's value was zero. The emitter then trusted it and
    // pushed `null`. A one-line `Dialect.extractPattern(unit) { return
    // "extract(?1 from ?2)"; }` spliced into `H2Dialect.extractPattern`
    // compiled to `xor eax,eax; ret`, and every Hibernate HQL `extract()` /
    // `cast()` / `str()` query then died in `PatternRenderer.<init>` with
    // `NullPointerException: ... because "pattern" is null` (2026-08-11 Linux
    // full suite: 17 of 34 method failures across four HQL classes, all green
    // under `--nojit`).
    //
    // Refuse the whole inline site instead. The callee still compiles and is
    // still CALLED — it just is not spliced — which is what "cannot model it"
    // has to mean.
    let mut ldc_info = Vec::new();
    // Callee PCs whose constant is a float or a double. The optimizing tier
    // cannot splice a body without this — see `InlineSite::ldc_fp_pcs` — and
    // recording it here rather than re-deriving it downstream is what keeps the
    // tag and the value from disagreeing: both come out of the same
    // constant-pool match.
    let mut ldc_fp_pcs: Vec<usize> = Vec::new();
    if has_ldc {
        let mut fpc = 0;
        while fpc < code_len {
            if code[fpc] == 0x12 && fpc + 1 < code_len {
                let cp_idx = code[fpc + 1] as u16; // Cast: bytecode operand decoding
                let val = match callee_class_info.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::Integer(v)) => *v as i64, // JVM spec: bounded float-to-long conversion
                    Some(ConstantPoolEntry::Float(v)) => {
                        ldc_fp_pcs.push(fpc);
                        (*v as f32).to_bits() as i32 as i64 // Cast: JIT ABI -- float bits to i64
                    }
                    _ => return None,
                };
                ldc_info.push((fpc, val));
                fpc += 2;
            } else if code[fpc] == 0x13 && fpc + 2 < code_len {
                let cp_idx = ((code[fpc + 1] as u16) << 8) | code[fpc + 2] as u16; // Cast: bytecode operand decoding
                let val = match callee_class_info.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::Integer(v)) => *v as i64, // JVM spec: bounded float-to-long conversion
                    Some(ConstantPoolEntry::Float(v)) => {
                        ldc_fp_pcs.push(fpc);
                        (*v as f32).to_bits() as i32 as i64 // Cast: JIT ABI -- float bits to i64
                    }
                    _ => return None,
                };
                ldc_info.push((fpc, val));
                fpc += 3;
            } else {
                fpc += inline_instr_length(code, fpc);
            }
        }
    }

    // Same contract for `ldc2_w`: `Long` and `Double` are the only entries the
    // JVMS permits here, so a third kind means the constant pool disagrees with
    // the bytecode — refuse rather than splice a zero.
    let mut ldc2w_info = Vec::new();
    if has_ldc2w {
        let mut fpc = 0;
        while fpc < code_len {
            if code[fpc] == 0x14 && fpc + 2 < code_len {
                let cp_idx = ((code[fpc + 1] as u16) << 8) | code[fpc + 2] as u16; // Cast: bytecode operand decoding
                let val = match callee_class_info.constant_pool.get(cp_idx)? {
                    ConstantPoolEntry::Long(v) => *v,
                    ConstantPoolEntry::Double(v) => {
                        ldc_fp_pcs.push(fpc);
                        v.to_bits() as i64 // Cast: JIT ABI -- float bits to i64
                    }
                    _ => return None,
                };
                ldc2w_info.push((fpc, val));
                fpc += 3;
            } else {
                fpc += inline_instr_length(code, fpc);
            }
        }
    }

    let num_params = count_method_params(callee_desc);
    let callee_num_args = num_params + if is_static { 0 } else { 1 };
    let return_type = cratonvm_jit::return_type(callee_desc);
    // A spliced call goes through `jit_invoke_dispatch`, whose first argument
    // is the VM context the emitter loads from `heap_local_offset`. Without
    // this the enclosing compile may not establish that slot at all and the
    // dispatch reads garbage. `needs_heap` is ORed across every spliced site
    // by the planner, so one call-carrying callee is enough to turn it on for
    // the whole method — the same way one field-touching callee already does.
    let needs_heap = has_field_ops || has_static_field_ops || !invoke_targets.is_empty();

    let padded = crate::runtime::frame::padded_bytecode(&code_bytes);

    drop(cm);

    // Phase 2 — resolve field refs with NO class_manager guard held (see
    // the lock-order comment above).
    let mut field_info = Vec::new();
    let mut compact_field_info = Vec::new();
    for (fpc, cp_idx, type_tag) in field_sites {
        if let Ok(resolved) = resolve_field_ref(shared, declaring_id, cp_idx) {
            if cratonvm_types::compact_ref_fields_enabled() {
                if let Some(layout) =
                    cratonvm_types::class_layout(resolved.declaring_class_id.as_u32())
                {
                    if let (Some(off), Some(is_ref)) = (
                        layout.field_offset(resolved.field_index),
                        layout.field_is_ref(resolved.field_index),
                    ) {
                        compact_field_info.push((fpc, off, is_ref));
                    }
                }
            }
            field_info.push((fpc, resolved.field_index, type_tag));
        }
    }
    let mut static_field_info = Vec::new();
    for (fpc, cp_idx, type_tag) in static_sites {
        if let Ok(resolved) = resolve_field_ref(shared, declaring_id, cp_idx) {
            static_field_info.push((
                fpc,
                resolved.declaring_class_id.as_u32(),
                resolved.field_index,
                type_tag,
                resolved.is_volatile,
            ));
        } else if ir_mode {
            // An unresolvable site is DROPPED for the single-pass emitter,
            // which bails that one site and keeps the rest of the body. The
            // optimizing tier has no such fallback: a `getstatic` with no row
            // bails the whole METHOD, after the splice has been committed to.
            // Refuse the body instead — the callee is still compiled and still
            // called, it is just not spliced. Same trade, and the same
            // sentence, as `ir-splice-new-site-unresolved`.
            no!("ir-splice-static-field-unresolved");
        }
    }

    // Nesting: a call inside the spliced body that is itself worth splicing.
    //
    // Resolved AFTER `drop(cm)`, and it must be: this recurses into
    // `resolve_inline_site_from`, which takes its own `class_manager.read()`.
    // parking_lot's RwLock is not reentrant, and a writer queued between the
    // two reads deadlocks the thread against itself — the same lock-order
    // obligation the field-resolution phase above was split out for.
    //
    // Only STATICALLY BOUND calls are candidates. `invokestatic` and
    // `invokespecial` name their target through the constant pool, which is
    // what `resolve_inline_site` resolves; `invokevirtual` / `invokeinterface`
    // select on the runtime receiver, and this planning context has no receiver
    // profile for a callee-internal pc (the profile is keyed by the enclosing
    // method's bci). Those keep the dispatch helper — which is step 4's whole
    // point, and is why nesting does not need them.
    //
    // A nested site is ADDITIVE: the pc keeps its `invoke_targets` entry too,
    // so a nested splice that bails mid-body inside the emitter falls back to
    // the ordinary call rather than failing the outer splice.
    // IR-tier nesting is deeper and is gated on nothing but `ir_mode`. Deeper
    // because the accessor chains it exists for are four levels on their own
    // (`get` → `loadFromArray` → `Short2.<init>()V` → `Short2.<init>([S)V`),
    // and stopping one short leaves the allocation being passed to an opaque
    // call — which is an escape, so the whole splice buys nothing. Ungated
    // because `CRATONVM_JIT_INLINE_NEST` is the single-pass emitter's switch and
    // the IR path is already behind its own.
    let nest_budget = if ir_mode {
        cratonvm_jit::MAX_IR_INLINE_NEST_DEPTH
    } else {
        cratonvm_jit::MAX_INLINE_NEST_DEPTH
    };
    let mut nested_sites: Vec<cratonvm_jit::NestedInlineSite> = Vec::new();
    if nest_depth + 1 < nest_budget && (ir_mode || crate::runtime::env_cache::jit_inline_nest()) {
        // The CALLEE's own receiver profile, fetched once for the whole body.
        //
        // This is the piece that makes devirtualising inside a splice possible
        // at all, and it needed no new profiling: receiver types are recorded
        // against the bci of the method that is EXECUTING, so a virtual call
        // inside `objectsAreEqual` is already profiled under
        // `objectsAreEqual`'s own `MethodKey` at its own bci — exactly the
        // (method, pc) pair a nested site names. The enclosing method's profile
        // never had this and never could, which is why re-keying it by
        // (caller pc, callee pc) was the wrong shape to reach for.
        let callee_profile = if crate::runtime::env_cache::jit_inline_splice_devirt() {
            shared
                .jit
                .profile_store
                .get_profile(&crate::jit::profile::MethodKey {
                    class_id: declaring_id.as_u32(),
                    method_name: Arc::from(callee_method),
                    descriptor: Arc::from(callee_desc),
                })
        } else {
            None
        };
        for (ipc, target) in &invoke_targets {
            // In IR mode every kind takes the statically-bound path: the
            // monomorphism gate on the selected method is what decides whether
            // a virtual target is spliceable, and it refuses the ones a guard
            // would otherwise have to cover. There is no guarded arm to fall
            // into, so routing `0`/`2` to the profile-driven branch below would
            // only produce sites the IR consumer must throw away.
            let kind = if ir_mode { 3 } else { target.invoke_kind };
            match kind {
                // Statically bound: one body, no guard.
                1 | 3 => {
                    let nested = resolve_inline_site_from(
                        shared,
                        declaring_id,
                        None,
                        &target.class_name,
                        &target.method_name,
                        &target.descriptor,
                        nest_depth + 1,
                        direct_bind,
                        ir_mode,
                    );
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] nest-static {}.{}{} at callee_pc={} depth={} -> {}",
                            target.class_name,
                            target.method_name,
                            target.descriptor,
                            ipc,
                            nest_depth + 1,
                            if nested.is_some() {
                                "SPLICED"
                            } else {
                                "refused"
                            },
                        );
                    }
                    if let Some(nested) = nested {
                        nested_sites.push(cratonvm_jit::NestedInlineSite {
                            callee_pc: *ipc,
                            guard_class_id: 0,
                            site: nested,
                        });
                    }
                }
                // Virtual / interface: one body per receiver class, so a splice
                // needs a guard and the profile has to name the class.
                0 | 2 => {
                    // THE GATE, and it belongs here rather than only on the
                    // profile fetch below. Gating just the profile left the MIC
                    // fallback running with `CRATONVM_JIT_INLINE_SPLICE_DEVIRT`
                    // OFF, so devirtualisation happened whenever nesting did and
                    // the flag's two arms were byte-identical — measured
                    // 2026-08-18, `nested-splice-guarded=1` in both. A switch
                    // that does not switch anything is worse than no switch: it
                    // makes an A/B report "no difference" for a feature that was
                    // on in both arms.
                    if !crate::runtime::env_cache::jit_inline_splice_devirt() {
                        continue;
                    }
                    // TWO sources, in this order, and the second is the one
                    // that actually answers for this workload.
                    //
                    //  1. the CALLEE's own receiver profile, keyed by its own
                    //     bci. The right key — receiver types are recorded
                    //     against the executing method — but measured
                    //     2026-08-18 it is EMPTY at exactly these sites: the
                    //     eager-callee-chain compiles a method like
                    //     `objectsAreEqual` before it ever runs its
                    //     `invokevirtual equals` interpreted, so nothing is
                    //     recorded. It is also gated off entirely unless
                    //     `CRATONVM_TIER_PGO` is set.
                    //  2. the CALLEE's COMPILED ARTIFACT's inline cache at that
                    //     bci. A method that skipped the interpreter has been
                    //     caching its receiver on every compiled call since,
                    //     which is the same evidence one layer down — and it is
                    //     available precisely when (1) is not.
                    //
                    // Both are speculation; the class-id guard is what makes
                    // either safe, and a wrong guess costs the miss edge.
                    let profile_dom = callee_profile
                        .as_ref()
                        .and_then(|p| p.receivers.get(ipc))
                        // Same 80% bar as the top-level guarded-virtual planner.
                        .and_then(|counts| crate::jit::profile::dominant_receiver(counts, 80));
                    let (dom, evidence) = match profile_dom {
                        Some(d) => (d, "profile"),
                        None => {
                            let artifact = shared.jit.jit_cache.read().get(
                                &Arc::from(callee_class),
                                &Arc::from(callee_method),
                                &Arc::from(callee_desc),
                                declaring_id,
                            );
                            let from_cache = artifact
                                .as_ref()
                                .and_then(|cm| cm.dominant_receiver_at_bci(*ipc));
                            match from_cache {
                                Some(d) => (d, "mic"),
                                None => {
                                    if crate::runtime::env_cache::dbg_jitc() {
                                        // Distinguish the three ways this can
                                        // answer nothing — no artifact at all,
                                        // an artifact with no slot at this bci,
                                        // and a slot that failed the dominance
                                        // bar — because they call for three
                                        // different fixes.
                                        let detail = match artifact.as_ref() {
                                            None => "no-artifact".to_string(),
                                            Some(cm) => format!(
                                                "artifact slots=[{}]",
                                                cm.mic_slot_census()
                                                    .iter()
                                                    .map(|(b, c, h, m)| format!(
                                                        "bci{b}:cls{c}:h{h}:m{m}"
                                                    ))
                                                    .collect::<Vec<_>>()
                                                    .join(",")
                                            ),
                                        };
                                        eprintln!(
                                            "[cratonvm-jitc] nest-virtual {}.{}{} at callee_pc={} -> no evidence (profile={} mic: {})",
                                            target.class_name,
                                            target.method_name,
                                            target.descriptor,
                                            ipc,
                                            if callee_profile.is_some() { "empty" } else { "absent" },
                                            detail,
                                        );
                                    }
                                    continue;
                                }
                            }
                        }
                    };
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] nest-virtual {}.{}{} at callee_pc={} -> guard on class {} (from {})",
                            target.class_name, target.method_name, target.descriptor, ipc, dom,
                            evidence,
                        );
                    }
                    // Resolve the body that receiver ACTUALLY dispatches to,
                    // not the constant-pool one: the guard certifies the
                    // subclass, so splicing the superclass's method behind it
                    // is silent wrong code at every overriding site. This is
                    // the same contract `resolve_receiver_inline_site`
                    // documents, and it applies verbatim one level down.
                    if let Some(nested) = resolve_inline_site_from(
                        shared,
                        declaring_id,
                        Some(ClassId::new(dom)),
                        &target.class_name,
                        &target.method_name,
                        &target.descriptor,
                        nest_depth + 1,
                        direct_bind,
                        // Unreachable in IR mode (every kind is routed to the
                        // statically-bound arm above), and a guarded splice is
                        // not something the IR consumer can emit — so `false`
                        // here is a statement, not a default.
                        false,
                    ) {
                        nested_sites.push(cratonvm_jit::NestedInlineSite {
                            callee_pc: *ipc,
                            guard_class_id: dom,
                            site: nested,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    // A spliced call must not be WORSE than the call it replaced.
    //
    // Measured 2026-08-18 (see `jit_inline_call_dispatch`): the chain this
    // whole line of work targets is already direct-bound, so emitting an
    // admitted call through the blind dispatch helper traded a ~4 ns raw CALL
    // for a ~175 ns name resolution — `assertFull` 47 -> 163-266 ns/iter, with
    // `disp_calls` going from 3 870 to 2 003 361 over 2 000 000 iterations.
    // Splicing away one frame does not pay for downgrading the call inside it.
    //
    // So unless the fallback is explicitly re-enabled, refuse any site with a
    // call that is not itself spliced, and then CLEAR `invoke_targets` — which
    // removes the emitter's fallback as well, so a nested splice that bails
    // during emission bails the enclosing splice instead of quietly becoming a
    // dispatch. Refusing costs the site its inline; admitting it costs 3.5x.
    // (`invoke_targets` is already `let mut` where it is built, ~540 lines up.)

    // Direct-bind whatever the same resolver the TOP LEVEL uses will bind.
    //
    // Runs after `drop(cm)` and after the nested resolution, and it must:
    // `callee_compiler` / `direct_callee_lookup` take `class_manager` for
    // reading and may transitively COMPILE the callee, which takes it for
    // writing. Both bounds that recursion themselves (depth, cycle, fan-out),
    // which is why reusing them is better than writing a lookup here.
    //
    // Skipped for a pc that is already NESTED — a spliced body beats a call,
    // and asking would compile a callee whose code this site is not going to
    // emit. Skipped for virtual/interface kinds: a direct bind names one body,
    // and those select on the runtime receiver (that is what
    // `resolve_receiver_inline_site` and the guarded-virtual path are for).
    if let Some(bind) = direct_bind {
        let nested_pcs: Vec<usize> = nested_sites.iter().map(|n| n.callee_pc).collect();
        for (ipc, target) in invoke_targets.iter_mut() {
            if nested_pcs.contains(ipc) {
                continue;
            }
            if target.invoke_kind != 1 && target.invoke_kind != 3 {
                continue;
            }
            target.direct_entry = bind(&target.class_name, &target.method_name, &target.descriptor);
        }
    }

    // A spliced call must not be WORSE than the call it replaced.
    //
    // Measured 2026-08-18 (see `jit_inline_call_dispatch`): the chain this
    // whole line of work targets is already direct-bound, so emitting an
    // admitted call through the blind dispatch helper traded a ~4 ns raw CALL
    // for a ~175 ns name resolution — `assertFull` 47 -> 163-266 ns/iter, with
    // `disp_calls` going from 3 870 to 2 003 361 over 2 000 000 iterations.
    // Splicing away one frame does not pay for downgrading the call inside it.
    //
    // So unless the fallback is explicitly re-enabled, every call in the body
    // must be either SPLICED IN TURN or DIRECT-BOUND; a site with one that is
    // neither is refused whole. Refusing costs the site its inline; admitting
    // it costs 3.5x.
    //
    // NOT applied in IR mode, and the difference is in the emitter, not the
    // policy. That measurement is of `try_emit_inline_body`'s invoke arm, which
    // lowers a spliced call to the BLIND dispatch helper — no MIC, no PIC, a
    // name resolution per call. `IrBuilder` lowers a spliced call to the same
    // `Op::Call` it lowers every other invoke to, and `ir_lower` gives it the
    // ordinary dispatch. That is still one inline cache short of what the
    // caller's own sites get (the cache is keyed by bytecode pc, and every node
    // in a spliced region carries the CALLER's `invoke` pc — see
    // `ir::IrInlineSite`), so it is not free; it is a call, not a resolution.
    //
    // Keeping the rule here would refuse exactly the bodies this lane exists
    // for: `VolumeShort2.loadFromArray` calls `ShortArray.get`, which is an FFM
    // accessor the nested resolver deliberately refuses to splice (it keeps its
    // own fast path), and no direct bind is offered for a virtual kind. One
    // un-spliceable call would cost the site its allocation.
    if !ir_mode && !crate::runtime::env_cache::jit_inline_call_dispatch() {
        let nested_pcs: Vec<usize> = nested_sites.iter().map(|n| n.callee_pc).collect();
        if let Some((pc, t)) = invoke_targets
            .iter()
            .find(|(pc, t)| t.direct_entry.is_none() && !nested_pcs.contains(pc))
        {
            no!(format!(
                "call at callee_pc={} to {}.{}{} (kind {}) is neither spliced nor direct-bound",
                pc, t.class_name, t.method_name, t.descriptor, t.invoke_kind
            ));
        }
        // A GUARDED nested splice keeps its dispatch entry no matter what: the
        // guard's miss edge has to go somewhere, and for a virtual site there
        // is no direct bind to send it to. That is the same bargain PGO-02
        // makes one level up — the cold edge pays the helper, the hot edge pays
        // nothing — and it is only a bargain while the guard actually holds,
        // which is what the 80% dominance bar above is for.
        //
        // An UNGUARDED nested pc with no direct bind loses its entry, so a
        // nested splice that bails at emission time bails the enclosing splice
        // rather than degrading to the helper. One that IS direct-bound keeps
        // it: falling back to a raw CALL is not a downgrade.
        let guarded_pcs: Vec<usize> = nested_sites
            .iter()
            .filter(|n| n.guard_class_id != 0)
            .map(|n| n.callee_pc)
            .collect();
        invoke_targets.retain(|(pc, t)| t.direct_entry.is_some() || guarded_pcs.contains(pc));
    }

    Some(cratonvm_jit::InlineSite {
        callee_code: padded.to_vec(),
        callee_code_len: code_len,
        callee_max_locals: callee_max_locals,
        callee_num_args,
        callee_is_static: is_static,
        return_type,
        field_info,
        compact_field_info,
        static_field_info,
        ldc_info,
        ldc2w_info,
        ldc_fp_pcs,
        needs_heap,
        class_name: inlined_body_class_name,
        class_id: inlined_body_class_id,
        method_name: callee_method.to_string(),
        descriptor: callee_desc.to_string(),
        elided_invoke_pcs,
        invoke_targets,
        // Interned by `try_compile_inner` right before backend emission; a
        // resolver never fills this, and an `InlineSite` that never reaches a
        // compile keeps it empty, which makes the emitter's invoke arm bail.
        resolved_invoke_infos: Vec::new(),
        nested_sites,
        ir_new_info,
        ir_typecheck_info,
    })
}

/// Whether a method reached by walking up from the RUNTIME RECEIVER is the
/// method real dispatch would select at a site declared against `cp_class_id`.
///
/// `find_method_recursive` implements JVMS selection, but selection is only
/// defined relative to a resolved method: a candidate overrides the resolved
/// method only if it is accessible to it (JVMS §5.4.5). A package-private
/// method in a *different* runtime package has the same name and descriptor and
/// is NOT an override — dispatch runs the resolved method, the walk finds the
/// impostor. Everything below is a fail-closed check for that class of
/// disagreement; a `false` answer means "do not inline", never "inline
/// something else".
#[allow(clippy::too_many_arguments)]
fn receiver_resolution_is_dispatch_faithful(
    store: &crate::classloading::ClassStore,
    receiver_id: ClassId,
    cp_class_id: ClassId,
    declaring_id: ClassId,
    method: &cratonvm_reader::method::ClassFileMethod,
    callee_method: &str,
) -> bool {
    // A virtual/interface site never dispatches to a static method, and a
    // private method is never inherited — a walk that reached one from a
    // subclass receiver found something dispatch could not.
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    if method.is_static()
        || method.access_flags.contains(MethodAccessFlags::PRIVATE)
        || method.is_abstract()
    {
        return false;
    }
    // `<init>`/`<clinit>` are not virtually dispatched at all.
    if callee_method.starts_with('<') {
        return false;
    }
    // The receiver must actually be a subtype of the declared class, or the
    // profile handed us a class id from a different site entirely.
    if !class_is_assignable_to(store, receiver_id, cp_class_id) {
        return false;
    }
    // Is the selected method genuinely an OVERRIDE of what the constant-pool
    // reference resolves to? Two sufficient conditions, both answerable from
    // what is already in hand — deliberately NOT by resolving the CP reference
    // as well. `vm/src/runtime/resolve/guard.rs` ratchets the interpreter's
    // metadata-table bypass budget downward and nothing raises it; a second
    // `find_method_recursive` here would, and the cheaper rules below cost
    // only reach, never correctness.
    //
    //  1. `public`/`protected` overrides anything with the same name and
    //     descriptor it inherits, in any package (JVMS §5.4.5).
    //  2. Otherwise, the method must be declared on the CONSTANT-POOL CLASS
    //     itself — in which case CP resolution stops there and the selected
    //     method IS the resolved method, so there is no override question to
    //     answer.
    //
    // What this refuses is the remaining shape: a PACKAGE-PRIVATE method found
    // by walking up from the receiver, declared somewhere other than the CP
    // class. It may or may not override — a package-private method in a
    // different runtime package does NOT (dispatch runs the resolved method,
    // while the walk found the impostor) — and telling those apart needs the
    // CP-side resolution this deliberately does without. Refusing costs a
    // package-private virtual site its inline; guessing costs a wrong body.
    if method
        .access_flags
        .intersects(MethodAccessFlags::PUBLIC | MethodAccessFlags::PROTECTED)
    {
        return true;
    }
    declaring_id == cp_class_id
}

/// Whether `sub` is `sup` or inherits/implements it.
fn class_is_assignable_to(
    store: &crate::classloading::ClassStore,
    sub: ClassId,
    sup: ClassId,
) -> bool {
    if sub == sup {
        return true;
    }
    let mut stack = vec![sub];
    let mut seen: std::collections::HashSet<ClassId> = std::collections::HashSet::new();
    while let Some(id) = stack.pop() {
        if id == sup {
            return true;
        }
        if !seen.insert(id) {
            continue;
        }
        let Some(class) = store.get(id) else {
            continue;
        };
        stack.extend(class.interfaces.iter().copied());
        if let Some(sc) = class.superclass {
            stack.push(sc);
        }
    }
    false
}

/// Instruction length for the inline-eligibility byte walk: the JIT's shared
/// decoder, `wide` forms included. The eligibility scan refuses
/// `tableswitch`/`lookupswitch` before it ever asks for a length.
#[inline]
pub(super) fn inline_instr_length(code: &[u8], pc: usize) -> usize {
    cratonvm_jit::bytecode_insn_len(code, pc)
}

/// Extract the array index from an AIOOBE panic message.
pub fn parse_aioobe_index(msg: &str) -> i32 {
    // Format: "ArrayIndexOutOfBoundsException: index N out of bounds for length M"
    if let Some(rest) = msg.strip_prefix("ArrayIndexOutOfBoundsException: index ") {
        if let Some(idx_str) = rest.split_whitespace().next() {
            if let Ok(idx) = idx_str.parse::<i32>() {
                return idx;
            }
        }
    }
    -1
}

/// Reconstruct a JIT'd method's incoming locals (`this` + declared params) as
/// `Value`s from the bit-exact `(CompactValue, kind)` arg slots that
/// `execute_jit_call` saved before dispatch. Used only on the cold
/// exception-routing path to repopulate the handler frame's locals (see
/// `route_jit_exception_through_method`). Mirrors the descriptor-aware decode
/// of the dispatch pop loop: receiver slot is `L`, the rest decode by the
/// method descriptor's parameter tags.
pub(super) fn jit_saved_args_to_values(
    cached: &CachedBytecodeMethod,
    saved_args: &[(CompactValue, u8)],
    np: usize,
) -> Vec<Value> {
    let is_static = cached.is_static;
    let mut out = Vec::with_capacity(np);
    // ONE forward scan, hoisted out of this per-argument loop.
    let param_tags = ParamTags::for_method(&cached);
    for i in 0..np {
        let (cv, kind) = saved_args[i];
        let desc_byte = if is_static {
            param_tags.get(&cached.method_descriptor, i)
        } else {
            param_tags.get_with_receiver(&cached.method_descriptor, i)
        };
        out.push(decode_arg_kind_aware(cv, kind, desc_byte));
    }
    out
}

/// Roots the reference arguments `execute_jit_call` popped off the caller's
/// operand stack, for the whole native activation.
///
/// Once popped they are no longer interpreter roots, yet the saved copies are
/// used AFTER the compiled call returns: re-pushed for a whole-method re-run,
/// and decoded into handler locals after an exception object was allocated.
/// The call itself can collect, so a moving young collection handed those
/// paths pre-move addresses. Each reference is pushed into `native_pin_roots`
/// as it is popped (its index in `arg_pins`, `usize::MAX` for a
/// non-reference) and re-read through [`pinned_saved_arg`]; this guard
/// releases the window on every return path. It is declared before
/// `JitSynchronizedMonitorGuard`, whose pin sits above this window, so that
/// guard drops first.
pub(super) struct JitArgPinGuard {
    pub(super) thread: *mut JvmThread,
    pub(super) base: usize,
}

impl Drop for JitArgPinGuard {
    fn drop(&mut self) {
        // SAFETY: `thread` came from the exclusive `&mut JvmThread` of the
        // enclosing activation, which outlives this guard — the same aliasing
        // discipline `JitSynchronizedMonitorGuard` relies on.
        let thread = unsafe { &mut *self.thread };
        if thread.native_pin_roots.len() > self.base {
            thread.native_pin_roots.truncate(self.base);
        }
    }
}

/// Saved argument `i`, with a pinned reference re-read at its current
/// (possibly relocated) address. See [`JitArgPinGuard`].
pub(super) fn pinned_saved_arg(
    thread: &JvmThread,
    saved_args: &[(CompactValue, u8)],
    arg_pins: &[usize],
    i: usize,
) -> (CompactValue, u8) {
    let (cv, kind) = saved_args[i];
    match arg_pins.get(i).and_then(|&pin| thread.native_pin_roots.get(pin)) {
        // Cast: heap address to the compact reference encoding.
        Some(obj) => (CompactValue::object(obj.as_ptr() as usize as u64), kind),
        None => (cv, kind),
    }
}

/// [`jit_saved_args_to_values`] over the saved arguments with every pinned
/// reference re-read first.
pub(super) fn jit_saved_args_to_values_pinned(
    cached: &CachedBytecodeMethod,
    saved_args: &[(CompactValue, u8)],
    np: usize,
    thread: &JvmThread,
    arg_pins: &[usize],
) -> Vec<Value> {
    let mut current = saved_args.to_vec();
    for (i, slot) in current.iter_mut().enumerate().take(np) {
        *slot = pinned_saved_arg(thread, saved_args, arg_pins, i);
    }
    jit_saved_args_to_values(cached, &current, np)
}

/// Owns the implicit monitor of a JIT-entered `ACC_SYNCHRONIZED` method.
///
/// Compiled code has no interpreter frame on which to keep `monitor_on_exit`,
/// so the monitor must instead be rooted explicitly for the complete native
/// activation and released on every Rust return path. The native-pin slot is
/// retained after acquisition because a moving collection may update it before
/// `Drop` performs the matching implicit `monitorexit`.
pub(super) struct JitSynchronizedMonitorGuard {
    pub(super) shared: *const SharedVm,
    pub(super) thread: *mut JvmThread,
    pub(super) pin_index: usize,
    pub(super) armed: bool,
}

impl JitSynchronizedMonitorGuard {
    pub(super) fn acquire(
        shared: &SharedVm,
        thread: &mut JvmThread,
        cached: &CachedBytecodeMethod,
        args: &mut [Value],
    ) -> Result<Self, MethodCallFailed> {
        let monitor = if cached.is_static {
            get_or_create_class_mirror(shared, cached.declaring_class_id)
        } else {
            match args.first() {
                Some(Value::Object(Some(obj))) => *obj,
                _ => {
                    return Err(MethodCallFailed::InternalError(VmError::Internal {
                        message: "JIT entered synchronized instance method without this"
                            .to_string(),
                    }))
                }
            }
        };
        let fixed =
            crate::vm::vm_exec::monitor_enter_synchronized_method(shared, thread, monitor, args);
        let pin_index = thread.native_pin_roots.len();
        thread.native_pin_roots.push(fixed);
        Ok(Self {
            shared: shared as *const SharedVm,
            thread: thread as *mut JvmThread,
            pin_index,
            armed: true,
        })
    }

    pub(super) fn transfer_to_handler_frame(mut self, frame: &mut crate::runtime::frame::Frame) {
        // The native pin protects the monitor while the replacement frame is
        // built. Once the frame owns it, normal frame unwinding performs the
        // matching implicit monitorexit.
        // SAFETY: `self.thread` came from an exclusive `&mut JvmThread` and
        // this guard is scoped inside that borrow, so the pointer is live and
        // unaliased. `self.pin_index` indexes a `native_pin_roots` entry this
        // guard pushed and has not released, so the slot is in range.
        unsafe {
            let thread = &mut *self.thread;
            let monitor = thread.native_pin_roots[self.pin_index];
            frame.monitor_on_exit = Some(monitor);
            thread.native_pin_roots.truncate(self.pin_index);
        }
        self.armed = false;
    }
}

impl Drop for JitSynchronizedMonitorGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: the guard is scoped to the enclosing JIT-entry function, so
        // its drop runs before the exclusive `JvmThread` borrow ends. The pin
        // slot remains live and contains the collector-forwarded monitor ref.
        unsafe {
            let shared = &*self.shared;
            let thread = &mut *self.thread;
            let Some(monitor) = thread.native_pin_roots.get(self.pin_index).copied() else {
                return;
            };
            if let Err(error) =
                crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, monitor, thread.thread_id)
            {
                tracing::warn!(thread_id = ?thread.thread_id, ?error,
                    "implicit monitorexit after JIT synchronized method failed");
            }
            thread.native_pin_roots.truncate(self.pin_index);
        }
    }
}

/// Execute a JIT-compiled method call: pop args, call native code, push result.
///
/// When `needs_heap` is true, passes a heap pointer as hidden first C argument,
/// enabling the JIT code to allocate arrays and access heap data.
#[inline]
pub(super) fn execute_jit_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    compiled: &crate::jit::CompiledMethod,
    num_params: u16,
    return_type: u8,
    needs_heap: bool,
    cached: &Arc<CachedBytecodeMethod>,
) -> Result<CachedCallResult, MethodCallFailed> {
    // Pop raw u64 args directly — avoids decode_value/Value enum overhead.
    //
    // The JIT backend wires Java params via ARG_REGS only (no stack-arg
    // marshalling): on Windows x64 ARG_REGS has 4 slots, on System V x64
    // it has 6. When `needs_heap` is set, ARG_REGS[0] holds the SharedVm
    // pointer, which leaves one fewer slot for Java params. If the
    // method's parameter count exceeds the platform's available register
    // slots, the JIT codegen would silently truncate (see x64.rs prologue
    // — `ARG_REGS.iter()...take(self.num_params)`), producing a method
    // body that reads uninitialised locals for the missing tail params.
    // Bail to the bytecode interpreter in that case instead of dispatching
    // a miscompiled call.
    //
    // Reproducer (before this gate): Spring Boot 2.x's
    // `ExecutableArchiveLauncher.getMainClass` → `ZipFile`/`JarFile`
    // chain calls into a 5+-arg JIT'd method on Windows and panics with
    // "index out of bounds: the len is 4 but the index is 4" at the
    // pop-into-`jit_args` loop below.
    const JIT_ABI_MAX_JAVA_ARGS: usize = 8;
    // cceres2: never trust a separately-cached ABI flag over the compiled
    // method's own record — a (compiled, needs_heap) pair captured at two
    // different times can disagree after a recompile, and marshalling with
    // the wrong flag shifts every argument by one inside the callee. See
    // try_call_compiled_entry_reentrant for the matching guard + rationale.
    let needs_heap = {
        let own = compiled.needs_heap();
        if own != needs_heap {
            tracing::warn!(
                "JIT ABI flag mismatch in execute_jit_call: cached needs_heap={needs_heap} \
                 but compiled {}.{}{} says {own} — using the compiled method's own flag",
                cached.class_name,
                cached.method_name,
                cached.method_descriptor,
            );
        }
        own
    };
    let np = num_params as usize; // Widening: parameter count conversion
    let max_java_params = JIT_ABI_MAX_JAVA_ARGS - if needs_heap { 1 } else { 0 };
    if np > max_java_params {
        return Ok(CachedCallResult::CacheMiss);
    }
    let mut jit_args = [0i64; JIT_ABI_MAX_JAVA_ARGS];
    // The JIT calling convention expects raw primitive bits with no NaN-box
    // tag (Int → sign-extended i64, Long → raw i64, Float → zero-extended u32
    // bits, Double → raw f64 bits, Object → pointer). Decode each arg slot by
    // its *parameter descriptor* (receiver slot = 'L' for instance methods):
    //
    //   * NaN-box leak (the original bugfix here): an `Int(11)` slot is encoded
    //     0xFFFC_0000_0000_000B; `pop_raw().as_i64` would pass those tag bits
    //     as the value, so a JIT'd `int n` arrived as 0xFFFC_..._000B instead
    //     of 11 — forwarded to `alloc_array` it produced "young gen exhausted —
    //     tried to allocate 18445618173802709003 bytes" (fannkuch n=11,
    //     FullStackBench `new boolean[100000]`). `decode_by_descriptor(b'I')`
    //     strips the tag → Int(11).
    //   * Collision long: a `long` arg whose bit pattern collides with the
    //     NaN-tag int space (BC safegcd 0xFFFC_… accumulator) was decoded by
    //     the prior unconditional `to_value()` as `Value::Int`, truncating to
    //     the low 32 bits. `decode_by_descriptor(b'J')` reinterprets the raw
    //     i64 bit-exact. See bc-ec-mod-mododdinverse-investigation.md.
    let is_static = cached.is_static;
    // Save the raw popped slots (bit-exact + long mark) so the i64::MIN deopt
    // arm below can restore them before the slow path re-pops the args. See
    // that arm for the underflow this prevents.
    let mut saved_args: [(CompactValue, u8); JIT_ABI_MAX_JAVA_ARGS] =
        [(CompactValue::zero(), 0u8); JIT_ABI_MAX_JAVA_ARGS];
    // ONE forward scan, hoisted out of this per-argument loop.
    let param_tags = ParamTags::for_method(&cached);
    // Reference arguments are rooted as they leave the operand stack; see
    // `JitArgPinGuard`, armed immediately after this loop.
    let args_pin_base = thread.native_pin_roots.len();
    let mut arg_pins = [usize::MAX; JIT_ABI_MAX_JAVA_ARGS];
    for i in (0..np).rev() {
        let (cv, kind) = thread.frames[frame_idx].stack.pop_with_kind_unchecked();
        saved_args[i] = (cv, kind);
        let desc_byte = if is_static {
            param_tags.get(&cached.method_descriptor, i)
        } else {
            param_tags.get_with_receiver(&cached.method_descriptor, i)
        };
        let v = decode_arg_kind_aware(cv, kind, desc_byte);
        if let Value::Object(Some(obj)) = &v {
            arg_pins[i] = thread.native_pin_roots.len();
            thread.native_pin_roots.push(*obj);
        }
        jit_args[i] = match v {
            // Widening: i32 -> i64 (sign-extended, JVM i2l)
            Value::Int(x) => x as i64,
            Value::Long(x) => x,
            // Cast: float/double raw bit pattern stored in integer word (no value conversion)
            Value::Float(x) => x.to_bits() as i64,
            // Cast: float/double raw bit pattern stored in integer word (no value conversion)
            Value::Double(x) => x.to_bits() as i64,
            // Cast: object/code pointer to integer address
            Value::Object(Some(obj)) => obj.as_ptr() as i64,
            Value::Object(None) => 0,
            _ => 0,
        };
    }

    let _arg_pin_guard = JitArgPinGuard {
        thread: thread as *mut JvmThread,
        base: args_pin_base,
    };
    let mut synchronized_args = cached
        .is_synchronized
        .then(|| jit_saved_args_to_values(cached, &saved_args, np));
    let _synchronized_monitor = if let Some(args) = synchronized_args.as_mut() {
        Some(JitSynchronizedMonitorGuard::acquire(
            shared, thread, cached, args,
        )?)
    } else {
        None
    };
    if let Some(args) = synchronized_args.as_deref() {
        for (i, value) in args.iter().enumerate().take(np) {
            jit_args[i] = match value {
                Value::Int(x) => *x as i64,
                Value::Long(x) => *x,
                Value::Float(x) => x.to_bits() as i64,
                Value::Double(x) => x.to_bits() as i64,
                Value::Object(Some(obj)) => obj.as_ptr() as i64,
                Value::Object(None) => 0,
                _ => 0,
            };
        }
    }

    let args_slice = &jit_args[..np];
    let vm_ptr = shared as *const _ as i64; // Cast: JIT ABI -- pointer to i64 register

    if crate::runtime::env_cache::dbg_asserteq()
        && cached.class_name.as_ref() == "junit/framework/Assert"
        && cached.method_name.as_ref() == "assertEquals"
    {
        let mut buf = String::new();
        for (i, a) in args_slice.iter().enumerate() {
            buf.push_str(&format!(" a{}=0x{:x}", i, a));
        }
        eprintln!(
            "[ASSERTEQ-ENTER] {}{} np={} needs_heap={}{}",
            cached.method_name, cached.method_descriptor, np, needs_heap, buf
        );
    }

    // Fast path: dispatch-free methods skip catch_unwind + thread-local overhead
    // task #44: migrated from `call`/`call_with_context` (panicking shims) to
    // `try_call`/`try_call_with_context`. JIT runtime invocation failures
    // (invalid code pointer / too-many-args) surface as
    // `MethodCallFailed::InternalError` instead of silent 0-returns.
    // PERF (JIT-entry drain consolidation): every return path below used to
    // pay SIX separate thread-local accesses draining the out-of-band signal
    // flags. Both arms now snapshot-and-clear the whole signal block in ONE
    // TLS access (`take_all_jit_signals`) and the drains consume the local
    // snapshot — semantics identical (everything was drained on every path
    // anyway; that unconditional draining IS the Round-8..11 leak-fix
    // discipline), minus the repeated TLS walks per call.
    let (result, mut sig) = if !compiled.has_dispatch {
        // NEW-1.5 + T1.1.a: even on the fast path, a JIT call may
        // transitively trigger GC via a helper. Push the entry guard
        // so the root scanner can find spill slots in this frame;
        // uses precise oop maps when the compiled method has them.
        // SAFETY: compiled is a finalized JIT CompiledMethod whose entry point was validated; args match the method's JVM descriptor.
        let fast_result: Result<i64, cratonvm_jit::CompileError> = {
            let _jit_root_guard =
                crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
                    &*compiled,
                    Some(thread.frames.len()),
                );
            unsafe {
                if needs_heap {
                    compiled.try_call_with_context(vm_ptr, args_slice)
                } else {
                    compiled.try_call(args_slice)
                }
            }
        };
        let sig = crate::jit::helpers::take_all_jit_signals(thread);
        match fast_result {
            Ok(v) => (v, sig),
            Err(jit_err) => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("JIT call failed: {jit_err}"),
                }));
            }
        }
    } else {
        let saved_jit_thread = crate::jit::helpers::set_jit_thread(thread);
        let jit_result = {
            let _jit_root_guard =
                crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
                    &*compiled,
                    Some(thread.frames.len()),
                );
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if needs_heap {
                    // SAFETY: compiled is a finalized JIT CompiledMethod whose entry point was validated; args match the method's JVM descriptor.
                    unsafe { compiled.try_call_with_context(vm_ptr, args_slice) }
                } else {
                    // SAFETY: compiled is a finalized JIT CompiledMethod whose entry point was validated; args match the method's JVM descriptor.
                    unsafe { compiled.try_call(args_slice) }
                }
            }))
        };
        crate::jit::helpers::restore_jit_thread(saved_jit_thread);
        // Check for pending Java exception from JIT dispatch callbacks.
        // The JIT-executed method has its own exception table; we must try
        // to route the exception through it before propagating to the caller.
        // The JIT ran the entire method, so in general we do not know the
        // exact throw-site PC inside the JIT'd method (it has no live
        // bytecode frame). RBC.6 correctness fix: the ONE case where the pc
        // IS known is a local `athrow` — the x64 codegen bakes its own bci
        // into the call to `jit_throw_exception`, stashed as
        // `sig.athrow_bci` (see `JitSignals::athrow_bci`). When present, use
        // it; otherwise fall back to the `usize::MAX` "PC unknown" sentinel
        // as before (a callee-propagated exception genuinely has no known pc
        // from this method's perspective). Without this, a method with 2+
        // exception-table entries whose catch types are in a subtype
        // relationship (e.g. one entry catches `RuntimeException`, a later,
        // unrelated entry catches `IllegalStateException`) could route ANY
        // matching-by-type exception to the FIRST declared entry regardless
        // of which try-region actually threw — confirmed via a differential
        // repro (`AthrowCountBisect.twoThrowsSequential`,
        // `vm/tests/jit_local_exception_handler_tests.rs`) before this fix.
        // The routing function still skips catch-all (`finally`) entries
        // when the pc is unknown, so they cannot spuriously swallow
        // exceptions thrown outside their protected region.
        let mut sig = crate::jit::helpers::take_all_jit_signals(thread);
        if let Some(exc) = sig.exception.take() {
            // The exception consumes the deopt — the one-shot drain above
            // already cleared the out-of-band deopt signal (MEDIUM
            // `i64::MIN`-collision fix) so it cannot leak to the next JIT
            // call. The dispatch helper that stashed this exception also set
            // the deopt flag before returning `i64::MIN`.
            // Decoded through the argument pins: the compiled call, and the
            // exception allocation, may have moved a reference argument.
            let exc_locals =
                jit_saved_args_to_values_pinned(cached, &saved_args, np, thread, &arg_pins);
            let throw_pc = jit_local_athrow_pc_kind(cached, sig.athrow_bci);
            return route_jit_signal_exception(
                shared,
                thread,
                frame_idx,
                cached,
                throw_pc,
                exc,
                &exc_locals,
            );
        }
        match jit_result {
            Ok(Ok(v)) => (v, sig),
            Ok(Err(jit_err)) => {
                // task #44: try_call/try_call_with_context surface invalid
                // code pointer / too-many-args as Err; propagate instead
                // of silently returning 0.
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("JIT call failed: {jit_err}"),
                }));
            }
            Err(panic_payload) => {
                return Err(jit_panic_to_exception(shared, thread, panic_payload));
            }
        }
    };

    // MEDIUM fix (i64::MIN deopt-sentinel collision): capture and clear the
    // out-of-band deopt/exception signal exactly ONCE, immediately after the
    // JIT body returns and before any flag-consuming drain below. The JIT
    // signals exception/deopt by returning `i64::MIN`, but a method that
    // legitimately returns `Long.MIN_VALUE` (or a J/D/F/I value whose JIT-ABI
    // bits equal `i64::MIN`) returns the same value WITHOUT setting this flag.
    // Every JIT path that produces the genuine deopt sentinel also sets the
    // flag (`jit_throw_aioobe` / `jit_uncommon_trap` / the dispatch helpers and
    // their x64 stubs). Taking it here clears it for the remaining return paths
    // so it can never leak to the next JIT call; the `result == i64::MIN` arm
    // below consults `deopt_signaled` instead of overloading the value. (The
    // slow-path exception drain above already early-returned for the pending-
    // exception case; the one-shot drain cleared the flag.)
    let deopt_signaled = sig.deopt;

    // Round-8 CRIT fix (NPE leak): drain the pending-NPE flag on EVERY
    // JIT return path, not only the `i64::MIN` deopt sentinel arm. A
    // JIT-compiled method whose inner dispatch (a nested `jit_invoke_*`
    // helper or a `jit_iastore`/`jit_aastore`/`jit_bastore` on a null
    // array — the void-return store helpers cannot signal via the
    // i64::MIN sentinel) sets the flag and then returns normally would
    // otherwise leak the flag to the *next* unrelated JIT helper call,
    // surfacing the NPE at the wrong PC / wrong method. The drain must
    // happen before *any* normal-return early-return. If a void-return
    // store helper set the flag, we surface the NPE here.
    //
    // Round-9/10 HIGH fix: route the NPE through the JIT'd method's
    // exception table (mirroring the `take_jit_pending_exception` path
    // above) so an in-method `catch (NullPointerException ...)` actually
    // observes the throw. Without this, a try/catch wrapped around a
    // JIT'd null-array store would silently propagate the NPE past the
    // catch and surface it in the caller. `usize::MAX` for `throw_pc`
    // means the routing function skips catch-all (`finally`) entries
    // (which can't safely match without a known PC) but still matches
    // typed handlers by exception class.
    if sig.npe {
        // These three arms synthesize a FRESH exception and route it; none of
        // them consumes the stashed deopt frame. Draining it here bounds the
        // window in which that frame holds raw heap addresses nothing scans —
        // and stops a later drain for the same method claiming it, since the
        // match compares method names only.
        let _ = cratonvm_jit::deopt::take_last_deopt();
        // The compiled frames this NPE was raised in have already left the
        // stack: the null-check stub called `jit_npe_with_action`, loaded the
        // i64::MIN deopt sentinel and ran the epilogue, so construction here
        // sees only what the interpreter still holds. `sig` carries the
        // snapshot the helper took while they were live.
        let npe_snapshot = sig.trap_frames.take();
        match crate::runtime::exceptions::throw_runtime_error(
            shared,
            thread,
            RuntimeError::NullPointerException {
                // Rebuilt from the trapping method's OWN bytecode when the
                // snapshot names the site, so a compiled row carries the same
                // JEP 358 message its interpreted row does; falls back to the
                // action-only string this used to build. See
                // `super::jit_npe_message`.
                message: super::jit_npe_message::jit_npe_message(
                    shared,
                    npe_snapshot.as_deref(),
                    sig.npe_action,
                ),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    npe_snapshot,
                );
                // Through the argument pins; see the signal-exception arm.
                let exc_locals =
                    jit_saved_args_to_values_pinned(cached, &saved_args, np, thread, &arg_pins);
                return route_jit_signal_exception(
                    shared,
                    thread,
                    frame_idx,
                    cached,
                    JitThrowPc::Unknown,
                    exc,
                    &exc_locals,
                );
            }
            other => return Err(other),
        }
    }

    // Round-11 fix (AIOOBE leak): complete the round-8/9 NPE drain for the
    // pending-AIOOBE flag. A JIT void-return store helper
    // (`jit_iastore`/`jit_bastore`/`jit_aastore`/...) that hits an
    // out-of-bounds index sets `JIT_PENDING_AIOOBE` and returns normally —
    // it cannot signal via the i64::MIN deopt sentinel — so the AIOOBE drain
    // inside the `result == i64::MIN` arm below never observes it and the
    // exception would silently leak to the next unrelated JIT helper call.
    // Drain it here, on the same normal-return path as the NPE drain above
    // and AFTER it (a frame cannot have both pending at once, matching JVM
    // semantics), and route the ArrayIndexOutOfBoundsException through the
    // JIT'd method's own exception table exactly like the NPE block — so an
    // in-method `catch (ArrayIndexOutOfBoundsException ...)` actually
    // observes the throw. `usize::MAX` for `throw_pc` mirrors the NPE path
    // (skip catch-all `finally` entries, still match typed handlers).
    if let Some((index, length)) = sig.aioobe {
        // These three arms synthesize a FRESH exception and route it; none of
        // them consumes the stashed deopt frame. Draining it here bounds the
        // window in which that frame holds raw heap addresses nothing scans —
        // and stops a later drain for the same method claiming it, since the
        // match compares method names only.
        let _ = cratonvm_jit::deopt::take_last_deopt();
        // The compiled frames this bounds check fired in have already left the
        // stack, exactly as in the `sig.npe` and `sig.arithmetic` arms: the
        // helper flagged the signal and the compiled body ran its epilogue, so
        // `fillInStackTrace` walks a stack that no longer has them. `sig`
        // carries the snapshot the helper took while they were live; without
        // draining it here the throwable keeps an EMPTY trace, and the snapshot
        // is left in the cell for the next take, which belongs to a different
        // throwable.
        let trap_snapshot = sig.trap_frames.take();
        let msg = cratonvm_types::error::out_of_bounds_message::check_index(index, length);
        match crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some(&msg),
        ) {
            Ok(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    trap_snapshot,
                );
                // Through the argument pins; see the signal-exception arm.
                let exc_locals =
                    jit_saved_args_to_values_pinned(cached, &saved_args, np, thread, &arg_pins);
                return route_jit_signal_exception(
                    shared,
                    thread,
                    frame_idx,
                    cached,
                    JitThrowPc::Unknown,
                    exc,
                    &exc_locals,
                );
            }
            Err(other) => return Err(other),
        }
    }

    // Divide-by-zero direct-throw drain (sibling of the AIOOBE block above).
    // The JIT `idiv`/`irem`/`ldiv`/`lrem` zero-divisor stub calls
    // `jit_throw_arithmetic`, which sets this flag + the deopt signal and returns
    // `i64::MIN`. Throw a real `ArithmeticException` ("/ by zero") through the
    // method's own exception table here, BEFORE the `i64::MIN` re-run arm below —
    // re-running the method from entry would double-execute any side effect that
    // preceded the trap (the prior `uncommon_trap` behaviour, a HotSpot
    // divergence). `usize::MAX` throw_pc mirrors the NPE/AIOOBE blocks (match
    // typed handlers by class, skip catch-all `finally`).
    if sig.arithmetic {
        // These three arms synthesize a FRESH exception and route it; none of
        // them consumes the stashed deopt frame. Draining it here bounds the
        // window in which that frame holds raw heap addresses nothing scans —
        // and stops a later drain for the same method claiming it, since the
        // match compares method names only.
        let _ = cratonvm_jit::deopt::take_last_deopt();
        // The compiled frames this div-by-zero was raised in have already
        // left the stack, exactly as in the `sig.npe` arm above: the
        // zero-divisor stub called `jit_throw_arithmetic`, loaded the i64::MIN
        // deopt sentinel and ran the epilogue. `sig` carries the snapshot that
        // helper took while they were live; without draining it here the
        // throwable keeps the frameless trace `fillInStackTrace` just built,
        // AND the snapshot is left in the cell for the next take — which
        // belongs to a different throwable.
        let trap_snapshot = sig.trap_frames.take();
        match crate::runtime::exceptions::throw_runtime_error(
            shared,
            thread,
            RuntimeError::ArithmeticException {
                message: "/ by zero".to_string(),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    trap_snapshot,
                );
                // Through the argument pins; see the signal-exception arm.
                let exc_locals =
                    jit_saved_args_to_values_pinned(cached, &saved_args, np, thread, &arg_pins);
                return route_jit_signal_exception(
                    shared,
                    thread,
                    frame_idx,
                    cached,
                    JitThrowPc::Unknown,
                    exc,
                    &exc_locals,
                );
            }
            other => return Err(other),
        }
    }

    // real-frame-deopt: IR-path deopt detection. The IR lowerer's trampoline
    // (`ir_deopt_entry`) stashes a reconstructed frame in `LAST_DEOPT` and
    // returns `i64::MIN` WITHOUT setting `JIT_DEOPT_PENDING` (it lives in the
    // jit crate, with no access to the VM flag), so an IR-path deopt is
    // invisible to `deopt_signaled` and would otherwise be mistaken for a real
    // `i64::MIN` return. Consume the stashed frame here (clearing it so it can
    // never leak to the next JIT call): precise-resume at the trapping bci when
    // enabled + mappable, else re-run the method from entry (`CacheMiss`),
    // restoring the popped args first exactly like the `i64::MIN` arm below.
    // The detection MUST run whenever a deopt could have stashed a frame (the
    // IR div-by-zero guard, and any future guard); the resume *gate* only
    // chooses precise mid-bci resume vs re-run. `ir_deopt_entry` always returns
    // `i64::MIN` when it stashes, so gating on `result == i64::MIN` keeps the
    // common JIT-return path free of the thread-local access while never missing
    // a deopt (an undetected i64::MIN would be pushed as a real return == 0).
    if result == i64::MIN {
        if let Some(rframe) = cratonvm_jit::deopt::take_last_deopt() {
            dbg_deopt_sink("jit-callsite-a", &rframe, "");
            if ir_deopt_resume_enabled() {
                if let Some(r) = resume_from_ir_deopt(shared, thread, cached, &rframe) {
                    return Ok(r);
                }
            }
            // real-frame-deopt Step 4: under CRATONVM_DEOPT_REAL, RESUME the
            // Object-bearing deopt at the trapping bci — build the interpreter
            // frame (GC-rooting the oops across the pool refill AND the push
            // handoff), push it, and resume there instead of re-running the whole
            // method from entry. On an out-of-scope / unmappable frame this
            // returns None and falls through to the re-run below. Gate-OFF
            // (default): skipped → byte-identical; the int-only
            // CRATONVM_IR_DEOPT_RESUME path above is untouched.
            //
            // x64-backport Step 5: gate on the per-method coverage flag
            // `compiled.can_deopt_resume` (finalized in x64 codegen: deopt
            // snapshots present AND no scalar replacement). A method off the gate
            // — e.g. one that scalar-replaced an object, whose snapshot records
            // machine provenance the mapper can't re-materialize — falls straight
            // through to the safe re-run instead of relying on the mapper bail.
            // ADDITIVE second arm: `can_deopt_resume` is false on every
            // optimizing-tier artifact in a production build, so without it
            // this sink fell through to the whole-method re-run below and ran
            // any side effect the compiled body had ALREADY committed a second
            // time, silently. See `sink_precise_resume_allowed`.
            if (cratonvm_jit::deopt_real_enabled() && compiled.can_deopt_resume)
                || sink_precise_resume_allowed_for(cached, &rframe)
            {
                // deopt-osr Step 9: epoch staleness guard + de-speculation
                // wiring (record the deopt, evict, escalate to not-entrant /
                // not-compilable, advance the live epoch). Resumes the trapping
                // frame only when the artifact has not been superseded.
                if let Some(r) = real_frame_deopt_resume_and_despeculate(
                    shared, thread, compiled, cached, &rframe,
                ) {
                    return Ok(r);
                }
            }
            for i in 0..np {
                // Restore the mark as well as the bits: a `KIND_DOUBLE` slot
                // re-pushed as `KIND_UNKNOWN` would be re-read by its NaN-box
                // sub-tag on the slow path, which is how a double carrying a
                // NaN payload lost it across a deopt.
                // Through the argument pins: a reference may have moved.
                let (cv, kind) = pinned_saved_arg(thread, &saved_args, &arg_pins, i);
                thread.frames[frame_idx]
                    .stack
                    .push_with_kind_unchecked(cv, kind);
            }
            return Ok(CachedCallResult::CacheMiss);
        }
    }

    // Deopt sentinel: i64::MIN means the method was deoptimized — fall through
    // to the interpreter slow path to re-execute. MEDIUM fix: only when the
    // out-of-band `deopt_signaled` flag confirms the JIT actually took the
    // exception/deopt path. A bare `result == i64::MIN` with the flag CLEAR is a
    // method legitimately returning `Long.MIN_VALUE` (or a J/D/F/I value whose
    // JIT-ABI bits equal `i64::MIN`) and must be pushed as a real value below —
    // re-running it would double-execute its side effects.
    if result == i64::MIN && deopt_signaled {
        // Check for pending AIOOBE from JIT bounds check (now unreachable in
        // practice — the drain above takes the flag on every return path,
        // including the i64::MIN deopt case — kept as a defensive belt; the
        // routing above supersedes the old uncatchable InternalError below).
        if let Some((index, _length)) = crate::jit::helpers::take_jit_pending_aioobe() {
            if aioobe_dbg() {
                eprintln!(
                    "[AIOOBE-JIT] idx={index} len={_length} — JIT-compiled bounds check failed"
                );
                for (i, f) in thread.frames.iter().enumerate().rev().take(15) {
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(f.class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    eprintln!(
                        "[AIOOBE-JIT-STK {i}] {}.{}{} pc={}",
                        cn,
                        f.method_name(),
                        f.method_descriptor(),
                        f.pc
                    );
                }
            }
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::aioobe(
                    index as i32, // Cast: bounds-check index
                    _length as i32,
                ),
            )));
        }
        // Note: pending-NPE drain was hoisted above the i64::MIN branch
        // (round-8 CRIT fix) so a void-return store helper that set the
        // flag but did not produce the sentinel value still surfaces the
        // NPE. The previous in-arm drain is intentionally removed —
        // moving it above means the i64::MIN arm runs with NPE already
        // taken, so we just fall through to deopt re-execution.
        //
        // CRIT (BC SPHINCS-256 / SHA-512 underflow): the args were popped
        // off the caller's operand stack at the top of this function, but
        // CacheMiss makes the invoke handler re-run the call via the slow
        // path (execute_invokestatic{,virtual,...}), which pops the args
        // AGAIN. Restore them here so the operand stack is exactly as the
        // slow path expects. This matters even for a *correct* method: the
        // in-band i64::MIN deopt sentinel collides with a legitimate
        // i64::MIN `long`/`double` return (e.g. `Pack.bigEndianToLong` on a
        // SHA-512 word == 0x8000_0000_0000_0000), so a non-deopting method
        // can land here. Without the restore the next bytecode (`lastore`,
        // etc.) pops a now-missing slot and panics with a value_stack
        // underflow (len 0 → usize::MAX index).
        for i in 0..np {
            // See the sibling restore above: bits AND mark.
            // Through the argument pins: a reference may have moved.
            let (cv, kind) = pinned_saved_arg(thread, &saved_args, &arg_pins, i);
            thread.frames[frame_idx]
                .stack
                .push_with_kind_unchecked(cv, kind);
        }
        return Ok(CachedCallResult::CacheMiss);
    }

    // Push return value
    match return_type {
        b'I' => {
            thread.frames[frame_idx]
                .stack
                .push_unchecked(Value::Int(result as i32)); // Cast: JIT ABI -- i64 register convention
        }
        b'J' => {
            thread.frames[frame_idx]
                .stack
                .push_unchecked(Value::Long(result));
        }
        b'F' => {
            let f = f32::from_bits(result as u32); // Cast: JIT ABI -- i64 register convention
            thread.frames[frame_idx]
                .stack
                .push_unchecked(Value::Float(f));
        }
        b'D' => {
            let d = f64::from_bits(result as u64); // Cast: JIT ABI -- i64 register convention
            thread.frames[frame_idx]
                .stack
                .push_unchecked(Value::Double(d));
        }
        b'B' | b'C' | b'S' | b'Z' => {
            thread.frames[frame_idx]
                .stack
                .push_unchecked(Value::Int(narrow_int_return(return_type, result)));
        }
        b'[' | b'L' => {
            if result == 0 {
                thread.frames[frame_idx]
                    .stack
                    .push_unchecked(Value::Object(None));
            } else {
                // SAFETY: result is a non-zero JIT return value encoding a heap pointer to a valid object header.
                thread.frames[frame_idx]
                    .stack
                    .push_unchecked(Value::Object(Some(unsafe {
                        crate::types::ObjectRef::from_raw(result as *mut u8) // Cast: JIT ABI -- i64 register convention
                    })));
            }
        }
        _ => {} // void — no push
    }

    Ok(CachedCallResult::Handled)
}

/// Dispatch an already-resolved compiled method using **pre-decoded** args
/// (`args_slice`), instead of popping them off the operand stack like
/// [`execute_jit_call`]. Used by the instance-method invocation tier-up path
/// (bug-03 layer B, default-ON; off-switch `CRATONVM_JIT_VIRTUAL_TIERUP=0`), which
/// reaches the JIT *after* the interception checks have already popped+decoded the args
/// into `args_slice`.
///
/// Returns:
///   * `Ok(Some(ccr))` — handled; `ccr` is the call result to return (normally
///     `Handled` with the return value pushed, or whatever
///     `route_jit_exception_through_method` decided when the JIT body threw).
///   * `Ok(None)` — the call could not be JIT-dispatched (too many args for the
///     JIT register ABI, or the method deoptimized via the `i64::MIN` sentinel).
///     NOTHING was pushed and the operand stack is untouched, so the caller must
///     fall through to the interpreted frame push (its `args_slice` is still
///     valid — it was popped into a local buffer, not consumed here).
///   * `Err(_)` — a Java exception was raised (NPE / AIOOBE / in-method throw).
///
/// `num_params` includes the receiver for a non-static `cached` (so
/// `args_slice[0]` is the receiver, matching the compiled instance prologue).
/// `execute_jit_call` stays the single source of truth for the stack-popping
/// (static) path; this mirrors only its run/exception/return logic so that path
/// is left byte-identical.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_jit_call_decoded(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    compiled: &crate::jit::CompiledMethod,
    num_params: u16,
    return_type: u8,
    needs_heap: bool,
    cached: &Arc<CachedBytecodeMethod>,
    args_slice: &[Value],
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    const JIT_ABI_MAX_JAVA_ARGS: usize = 8;
    // cceres2: same ABI-flag guard as execute_jit_call — the compiled
    // method's own record wins over any separately-cached flag.
    let needs_heap = {
        let own = compiled.needs_heap();
        if own != needs_heap {
            tracing::warn!(
                "JIT ABI flag mismatch in execute_jit_call_decoded: cached \
                 needs_heap={needs_heap} but compiled {}.{}{} says {own} — using \
                 the compiled method's own flag",
                cached.class_name,
                cached.method_name,
                cached.method_descriptor,
            );
        }
        own
    };
    let np = num_params as usize; // Widening: parameter count conversion
    let max_java_params = JIT_ABI_MAX_JAVA_ARGS - if needs_heap { 1 } else { 0 };
    // Too many args for the register-only JIT ABI, or a mismatch between the
    // decoded args and the declared count → interpreter fallback (Ok(None)).
    //
    // This is the LAST place a site that passed every tier-up condition and
    // found a compiled body can still be interpreted, and until the second
    // census (`CRATONVM_DBG_PROMOTE_REFUSE`) nothing named it: the site falls
    // through to the interpreted frame push with the operand stack untouched,
    // indistinguishable from never having been admitted at all. See
    // `interp_census::promote_refuse_enabled`.
    if np > max_java_params || args_slice.len() != np {
        if crate::runtime::interp_census::promote_refuse_enabled() {
            crate::runtime::interp_census::record_decoded_call_refusal(
                if np > max_java_params {
                    "decoded_call_abi_too_many_args"
                } else {
                    "decoded_call_arg_count_mismatch"
                },
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
        }
        return Ok(None);
    }
    // Decode each Java arg to its raw JIT-ABI bit pattern (Int → sign-extended
    // i64, Long → raw i64, Float/Double → zero-/raw-bits, Object → pointer).
    // `args_slice` is already descriptor-decoded by the caller (receiver = arg 0).
    let mut jit_args = [0i64; JIT_ABI_MAX_JAVA_ARGS];
    for (i, v) in args_slice.iter().enumerate().take(np) {
        jit_args[i] = match v {
            Value::Int(x) => *x as i64, // Cast: JIT ABI -- i64 register convention
            Value::Long(x) => *x,
            Value::Float(x) => x.to_bits() as i64, // Cast: JIT ABI -- float bits to i64
            Value::Double(x) => x.to_bits() as i64, // Cast: JIT ABI -- double bits to i64
            Value::Object(Some(obj)) => obj.as_ptr() as i64, // Cast: JIT ABI -- pointer to i64
            Value::Object(None) => 0,
            _ => 0,
        };
    }
    let mut synchronized_args = cached.is_synchronized.then(|| args_slice.to_vec());
    let _synchronized_monitor = if let Some(args) = synchronized_args.as_mut() {
        Some(JitSynchronizedMonitorGuard::acquire(
            shared, thread, cached, args,
        )?)
    } else {
        None
    };
    if let Some(args) = synchronized_args.as_deref() {
        for (i, value) in args.iter().enumerate().take(np) {
            jit_args[i] = match value {
                Value::Int(x) => *x as i64,
                Value::Long(x) => *x,
                Value::Float(x) => x.to_bits() as i64,
                Value::Double(x) => x.to_bits() as i64,
                Value::Object(Some(obj)) => obj.as_ptr() as i64,
                Value::Object(None) => 0,
                _ => 0,
            };
        }
    }
    let args_slice = synchronized_args.as_deref().unwrap_or(args_slice);
    let args_jit = &jit_args[..np];
    let vm_ptr = shared as *const _ as i64; // Cast: JIT ABI -- pointer to i64 register

    // Run the compiled body. Mirrors execute_jit_call's run+exception logic
    // (including its one-shot signal drain — see the PERF note there).
    let (result, mut sig) = if !compiled.has_dispatch {
        // SAFETY: compiled is a finalized JIT CompiledMethod with a validated entry; args match its JVM descriptor (receiver-aware).
        let fast_result: Result<i64, cratonvm_jit::CompileError> = {
            let _jit_root_guard =
                crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
                    &*compiled,
                    Some(thread.frames.len()),
                );
            unsafe {
                if needs_heap {
                    compiled.try_call_with_context(vm_ptr, args_jit)
                } else {
                    compiled.try_call(args_jit)
                }
            }
        };
        let sig = crate::jit::helpers::take_all_jit_signals(thread);
        match fast_result {
            Ok(v) => (v, sig),
            Err(jit_err) => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("JIT call failed: {jit_err}"),
                }));
            }
        }
    } else {
        let saved_jit_thread = crate::jit::helpers::set_jit_thread(thread);
        let jit_result = {
            let _jit_root_guard =
                crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
                    &*compiled,
                    Some(thread.frames.len()),
                );
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: see the fast-path SAFETY note above.
                unsafe {
                    if needs_heap {
                        compiled.try_call_with_context(vm_ptr, args_jit)
                    } else {
                        compiled.try_call(args_jit)
                    }
                }
            }))
        };
        crate::jit::helpers::restore_jit_thread(saved_jit_thread);
        let mut sig = crate::jit::helpers::take_all_jit_signals(thread);
        if let Some(exc) = sig.exception.take() {
            // The one-shot drain already cleared the out-of-band deopt signal
            // (MEDIUM `i64::MIN`-collision fix) so it cannot leak to the next
            // JIT call — the exception consumes the deopt. Mirrors
            // `execute_jit_call`.
            // RBC.6 correctness fix — see the identical comment at
            // `execute_jit_call`'s sibling call site: use the athrow's own
            // known bci when available instead of always `usize::MAX`.
            let throw_pc = jit_local_athrow_pc_kind(cached, sig.athrow_bci);
            return route_jit_signal_exception(
                shared, thread, frame_idx, cached, throw_pc, exc, args_slice,
            )
            .map(Some);
        }
        match jit_result {
            Ok(Ok(v)) => (v, sig),
            Ok(Err(jit_err)) => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("JIT call failed: {jit_err}"),
                }));
            }
            Err(panic_payload) => {
                return Err(jit_panic_to_exception(shared, thread, panic_payload));
            }
        }
    };

    // MEDIUM fix (i64::MIN deopt-sentinel collision): the one-shot drain
    // above captured+cleared the out-of-band deopt signal. See the matching
    // block in `execute_jit_call` for the full rationale.
    let deopt_signaled = sig.deopt;

    // Drain pending NPE / AIOOBE set by void-return store helpers (same as
    // execute_jit_call) — route through the JIT'd method's exception table.
    if sig.npe {
        // The compiled frames this NPE was raised in have already left the
        // stack: the null-check stub called `jit_npe_with_action`, loaded the
        // i64::MIN deopt sentinel and ran the epilogue, so construction here
        // sees only what the interpreter still holds. `sig` carries the
        // snapshot the helper took while they were live.
        let npe_snapshot = sig.trap_frames.take();
        match crate::runtime::exceptions::throw_runtime_error(
            shared,
            thread,
            RuntimeError::NullPointerException {
                // Rebuilt from the trapping method's OWN bytecode when the
                // snapshot names the site, so a compiled row carries the same
                // JEP 358 message its interpreted row does; falls back to the
                // action-only string this used to build. See
                // `super::jit_npe_message`.
                message: super::jit_npe_message::jit_npe_message(
                    shared,
                    npe_snapshot.as_deref(),
                    sig.npe_action,
                ),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    npe_snapshot,
                );
                return route_jit_signal_exception(
                    shared,
                    thread,
                    frame_idx,
                    cached,
                    JitThrowPc::Unknown,
                    exc,
                    args_slice,
                )
                .map(Some);
            }
            other => return Err(other),
        }
    }
    if let Some((index, length)) = sig.aioobe {
        // The compiled frames this bounds check fired in have already left the
        // stack, exactly as in the `sig.npe` and `sig.arithmetic` arms: the
        // helper flagged the signal and the compiled body ran its epilogue, so
        // `fillInStackTrace` walks a stack that no longer has them. `sig`
        // carries the snapshot the helper took while they were live; without
        // draining it here the throwable keeps an EMPTY trace, and the snapshot
        // is left in the cell for the next take, which belongs to a different
        // throwable.
        let trap_snapshot = sig.trap_frames.take();
        let msg = cratonvm_types::error::out_of_bounds_message::check_index(index, length);
        match crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some(&msg),
        ) {
            Ok(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    trap_snapshot,
                );
                return route_jit_signal_exception(
                    shared,
                    thread,
                    frame_idx,
                    cached,
                    JitThrowPc::Unknown,
                    exc,
                    args_slice,
                )
                .map(Some);
            }
            Err(other) => return Err(other),
        }
    }
    // Divide-by-zero direct-throw drain (sibling of the AIOOBE block above; see
    // the matching block in `execute_jit_call`). Throw `ArithmeticException`
    // through the method's exception table instead of re-running from entry.
    if sig.arithmetic {
        let trap_snapshot = sig.trap_frames.take();
        match crate::runtime::exceptions::throw_runtime_error(
            shared,
            thread,
            RuntimeError::ArithmeticException {
                message: "/ by zero".to_string(),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    trap_snapshot,
                );
                return route_jit_signal_exception(
                    shared,
                    thread,
                    frame_idx,
                    cached,
                    JitThrowPc::Unknown,
                    exc,
                    args_slice,
                )
                .map(Some);
            }
            other => return Err(other),
        }
    }

    // real-frame-deopt: IR-path deopt detection (mirrors the block in
    // `execute_jit_call`). `ir_deopt_entry` stashes `LAST_DEOPT` and returns
    // `i64::MIN` without setting `JIT_DEOPT_PENDING`, so consume the stashed
    // frame here too (clearing it). When precise resume is enabled and the frame
    // is mappable, resume the interpreter at the trapping bci (`Ok(Some(..))`);
    // otherwise re-run the method from entry (`Ok(None)`), which the caller
    // does from `args_slice` (the operand-stack args were popped by
    // `execute_invokevirtual_cached` before this call). This is the path the
    // instance-method invocation tier-up takes, so wiring resume here is what
    // makes an instance method's ref receiver/locals precise-resume (the static
    // MIC path goes through `execute_jit_call`). `resume_from_ir_deopt` bails
    // (returns `None`) side-effect-free before any frame mutation, so falling
    // through to re-run after a `None` is safe. Gated on `result == i64::MIN`
    // (a deopt always returns it) so the common path skips the thread-local.
    if result == i64::MIN {
        if let Some(rframe) = cratonvm_jit::deopt::take_last_deopt() {
            dbg_deopt_sink("jit-callsite-b", &rframe, "");
            if ir_deopt_resume_enabled() {
                if let Some(r) = resume_from_ir_deopt(shared, thread, cached, &rframe) {
                    return Ok(Some(r));
                }
            }
            // jit-invokedynamic-groovy-regression fix — mirror
            // `execute_jit_call`'s precise-resume arm on THIS sink too (the
            // instance-method tier-up path, the dominant path for Groovy's
            // `IndyInterface`-dispatched `doCall` methods): a frame-stashing
            // deopt (e.g. the unconditional invokedynamic reason-8 trap) in
            // the compiled target resumes at the trapping bci instead of
            // re-running the whole method from entry (which double-executes
            // every side effect committed before the trap). Identity/epoch
            // checks and de-speculation live inside
            // `real_frame_deopt_resume_and_despeculate`; any refusal falls
            // through to the safe re-run below.
            // ADDITIVE second arm: `can_deopt_resume` is false on every
            // optimizing-tier artifact in a production build, so without it
            // this sink fell through to the whole-method re-run below and ran
            // any side effect the compiled body had ALREADY committed a second
            // time, silently. See `sink_precise_resume_allowed`.
            if (cratonvm_jit::deopt_real_enabled() && compiled.can_deopt_resume)
                || sink_precise_resume_allowed_for(cached, &rframe)
            {
                if let Some(r) = real_frame_deopt_resume_and_despeculate(
                    shared, thread, compiled, cached, &rframe,
                ) {
                    return Ok(Some(r));
                }
            } else if !rframe.method_key.is_empty()
                && !deopt_frame_matches_method(
                    &rframe,
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                )
            {
                // Resume unavailable and the stash belongs to a DIFFERENT
                // (nested-callee) method: de-speculate the frame's real owner
                // so it stops re-trapping; `cached` is innocent.
                despeculate_stashed_frame_method(shared, &rframe);
            } else {
                // Resume unavailable (`can_deopt_resume` false / gate off) —
                // still drive de-speculation so an `UnreachedCode` (reason 8)
                // trap reached through THIS call site gets blacklisted
                // (`MakeNotCompilable`) instead of re-triggering on every
                // subsequent call (the FOURTH-pass Groovy finding: this sink
                // consumed the stash but never de-speculated, leaving the
                // trapping method compiled forever). Recover the reason from
                // the deopt point matching this bci (falling back to
                // `UnreachedCode`, the only reason this snapshot machinery
                // unconditionally records).
                let despec_reason = compiled
                    .deopt_points
                    .iter()
                    .find(|dp| dp.bci == rframe.bci)
                    .map(|dp| dp.reason)
                    .unwrap_or(cratonvm_jit::deopt::DeoptReason::UnreachedCode);
                let _ = crate::jit::helpers::DeoptimizationController::deoptimize(
                    shared,
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                    despec_reason,
                    rframe.bci,
                );
            }
            return Ok(None);
        }
    }

    // Deopt sentinel → interpreter fallback. The operand stack was never
    // touched here, so the caller's `args_slice` is still valid for the
    // interpreted frame push. MEDIUM fix: gate on the out-of-band
    // `deopt_signaled` flag so a method legitimately returning `Long.MIN_VALUE`
    // (or a J/D/F/I value whose JIT-ABI bits equal `i64::MIN`) is NOT mistaken
    // for a deopt — re-running it interpreted would double-execute side effects
    // (the previous "same result" claim is false for any method with side
    // effects). With the flag clear we fall through and push the real value.
    if result == i64::MIN && deopt_signaled {
        if crate::runtime::interp_census::promote_refuse_enabled() {
            crate::runtime::interp_census::record_decoded_call_refusal(
                "decoded_call_deopt",
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
        }
        return Ok(None);
    }

    // Push the return value (mirrors execute_jit_call).
    match return_type {
        b'I' | b'B' | b'C' | b'S' | b'Z' => {
            thread.frames[frame_idx]
                .stack
                .push_unchecked(Value::Int(narrow_int_return(return_type, result)));
        }
        b'J' => {
            thread.frames[frame_idx]
                .stack
                .push_unchecked(Value::Long(result));
        }
        b'F' => {
            let f = f32::from_bits(result as u32); // Cast: JIT ABI -- i64 register convention
            thread.frames[frame_idx]
                .stack
                .push_unchecked(Value::Float(f));
        }
        b'D' => {
            let d = f64::from_bits(result as u64); // Cast: JIT ABI -- i64 register convention
            thread.frames[frame_idx]
                .stack
                .push_unchecked(Value::Double(d));
        }
        b'[' | b'L' => {
            if result == 0 {
                thread.frames[frame_idx]
                    .stack
                    .push_unchecked(Value::Object(None));
            } else {
                // SAFETY: non-zero JIT return encodes a heap pointer to a valid object header.
                thread.frames[frame_idx]
                    .stack
                    .push_unchecked(Value::Object(Some(unsafe {
                        crate::types::ObjectRef::from_raw(result as *mut u8) // Cast: JIT ABI -- i64 register convention
                    })));
            }
        }
        _ => {} // void — no push
    }

    Ok(Some(CachedCallResult::Handled))
}

/// A ONE-SHOT sibling of [`execute_jit_call_decoded`] — enter an already
/// compiled method, run it to completion, and hand its return value back as a
/// `Value` instead of pushing it onto a caller's operand stack.
///
/// # Why this exists rather than another caller of `execute_jit_call_decoded`
///
/// `execute_jit_call_decoded` is written for ONE shape of caller: the
/// interpreter's own per-instruction dispatch loop. Its contract is
/// loop-integrated in two ways that a plain Rust subroutine cannot honour:
///
///  * the normal return value is PUSHED onto `thread.frames[frame_idx].stack`,
///    where the loop expects to find it; and
///  * on an exception routed into the callee's own handler, or on a
///    precise-resume deopt, it PUSHES an interpreter frame and returns
///    `CachedCallResult::FramePushed`, meaning "I have set up a frame; the
///    stepping loop will run it as part of normal control flow".
///
/// A dispatch helper such as `try_invoke_cached_lambda_impl` is not that loop.
/// It must return a complete `Value` synchronously, and it has no way to run a
/// frame somebody else pushed. Handing it `FramePushed` and reading the caller
/// frame's stack anyway leaves an ORPHANED frame behind, whose later return
/// corrupts the frame/stack accounting of a thread that has long since moved
/// on — observed as a `usize::MAX` operand-stack index underflow inside a
/// LATER, interpreted execution of the very same lambda body, thousands of
/// calls after the deopt that actually caused it. See
/// known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md
/// section 5.3 for that crash and 5.4(b) for this function being the
/// prescribed fix.
///
/// So this function keeps `execute_jit_call_decoded`'s run/signal/deopt logic
/// verbatim — same guards, same one-shot signal drain, same routing sinks —
/// and differs in exactly the two places above:
///
///  * a normal return is CONVERTED to a `Value` and returned, never pushed;
///  * a sink that materialises a frame has that frame RUN TO COMPLETION here
///    (`run_pushed_frame_to_completion`), so the handler / resumed body
///    finishes as part of this call and its result — value or exception —
///    becomes this call's result. Nothing is left on `thread.frames`.
///
/// The caller's frames are therefore never read and never written. That is why
/// this needs no trusted `frame_idx` and is safe from every dispatch context,
/// including the ones that hold no interpreter frame at all.
///
/// Returns:
///   * `Ok(Some(value))` — ran to completion; `value` is `None` for a `void`
///     descriptor and `Some(v)` otherwise.
///   * `Ok(None)` — DECLINED, side-effect-free: the ABI could not carry the
///     args, or a deopt landed with no resumable frame. Nothing was executed
///     that the caller must not repeat, so the caller falls back to the
///     interpreted path with the same `args`.
///   * `Err(_)` — a Java exception escaped the callee (or an internal error).
///     Propagates synchronously, exactly like the interpreted path's own `?`.
pub(super) fn execute_jit_call_oneshot(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    args_slice: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    const JIT_ABI_MAX_JAVA_ARGS: usize = 8;
    let needs_heap = compiled.needs_heap();
    // Widening: parameter count conversion
    let np = cached.num_params as usize + usize::from(!cached.is_static);
    let max_java_params = JIT_ABI_MAX_JAVA_ARGS - if needs_heap { 1 } else { 0 };
    // Too many args for the register-only JIT ABI, or a mismatch between the
    // decoded args and the declared count → interpreter fallback (Ok(None)).
    if np > max_java_params || args_slice.len() != np {
        return Ok(None);
    }
    // `is_synchronized` needs the monitor enter/exit this path does not do.
    // Every producer of a lambda-impl `CachedBytecodeMethod` already refuses
    // one, so this is a guard against a future producer, not a live arm.
    if cached.is_synchronized {
        return Ok(None);
    }
    // Decode each Java arg to its raw JIT-ABI bit pattern, exactly as
    // `execute_jit_call_decoded` does (receiver = arg 0 for an instance impl).
    let mut jit_args = [0i64; JIT_ABI_MAX_JAVA_ARGS];
    for (i, v) in args_slice.iter().enumerate().take(np) {
        jit_args[i] = match v {
            Value::Int(x) => *x as i64, // Cast: JIT ABI -- i64 register convention
            Value::Long(x) => *x,
            Value::Float(x) => x.to_bits() as i64, // Cast: JIT ABI -- float bits to i64
            Value::Double(x) => x.to_bits() as i64, // Cast: JIT ABI -- double bits to i64
            Value::Object(Some(obj)) => obj.as_ptr() as i64, // Cast: JIT ABI -- pointer to i64
            Value::Object(None) => 0,
            _ => 0,
        };
    }
    let args_jit = &jit_args[..np];
    let vm_ptr = shared as *const _ as i64; // Cast: JIT ABI -- pointer to i64 register
    let return_type = cached.return_tag();
    // `run_pushed_frame_to_completion` needs the depth the thread had BEFORE
    // any sink below materialised a frame.
    let frames_depth_on_entry = thread.frames.len();

    // Run the compiled body. Mirrors `execute_jit_call_decoded`'s run+exception
    // logic (including its one-shot signal drain).
    let (result, mut sig) = if !compiled.has_dispatch {
        // SAFETY: compiled is a finalized JIT CompiledMethod with a validated entry; args match its JVM descriptor (receiver-aware).
        let fast_result: Result<i64, cratonvm_jit::CompileError> = {
            let _jit_root_guard =
                crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
                    compiled,
                    Some(thread.frames.len()),
                );
            unsafe {
                if needs_heap {
                    compiled.try_call_with_context(vm_ptr, args_jit)
                } else {
                    compiled.try_call(args_jit)
                }
            }
        };
        let sig = crate::jit::helpers::take_all_jit_signals(thread);
        match fast_result {
            Ok(v) => (v, sig),
            Err(jit_err) => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("JIT call failed: {jit_err}"),
                }));
            }
        }
    } else {
        let saved_jit_thread = crate::jit::helpers::set_jit_thread(thread);
        let jit_result = {
            let _jit_root_guard =
                crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
                    compiled,
                    Some(thread.frames.len()),
                );
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: see the fast-path SAFETY note above.
                unsafe {
                    if needs_heap {
                        compiled.try_call_with_context(vm_ptr, args_jit)
                    } else {
                        compiled.try_call(args_jit)
                    }
                }
            }))
        };
        crate::jit::helpers::restore_jit_thread(saved_jit_thread);
        let mut sig = crate::jit::helpers::take_all_jit_signals(thread);
        if let Some(exc) = sig.exception.take() {
            let throw_pc = jit_local_athrow_pc_kind(cached, sig.athrow_bci);
            return oneshot_route_exception(
                shared,
                thread,
                cached,
                throw_pc,
                exc,
                args_slice,
                frames_depth_on_entry,
            );
        }
        match jit_result {
            Ok(Ok(v)) => (v, sig),
            Ok(Err(jit_err)) => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("JIT call failed: {jit_err}"),
                }));
            }
            Err(panic_payload) => {
                return Err(jit_panic_to_exception(shared, thread, panic_payload));
            }
        }
    };

    let deopt_signaled = sig.deopt;

    // Drain pending NPE / AIOOBE / divide-by-zero set by void-return store
    // helpers, routing each through the JIT'd method's own exception table
    // (same three sinks, same order, as `execute_jit_call_decoded`).
    if sig.npe {
        // The compiled frames this NPE was raised in have already left the
        // stack: the null-check stub called `jit_npe_with_action`, loaded the
        // i64::MIN deopt sentinel and ran the epilogue, so construction here
        // sees only what the interpreter still holds. `sig` carries the
        // snapshot the helper took while they were live.
        let npe_snapshot = sig.trap_frames.take();
        match crate::runtime::exceptions::throw_runtime_error(
            shared,
            thread,
            RuntimeError::NullPointerException {
                // Rebuilt from the trapping method's OWN bytecode when the
                // snapshot names the site, so a compiled row carries the same
                // JEP 358 message its interpreted row does; falls back to the
                // action-only string this used to build. See
                // `super::jit_npe_message`.
                message: super::jit_npe_message::jit_npe_message(
                    shared,
                    npe_snapshot.as_deref(),
                    sig.npe_action,
                ),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    npe_snapshot,
                );
                return oneshot_route_exception(
                    shared,
                    thread,
                    cached,
                    JitThrowPc::Unknown,
                    exc,
                    args_slice,
                    frames_depth_on_entry,
                );
            }
            other => return Err(other),
        }
    }
    if let Some((index, length)) = sig.aioobe {
        // The compiled frames this bounds check fired in have already left the
        // stack, exactly as in the `sig.npe` and `sig.arithmetic` arms: the
        // helper flagged the signal and the compiled body ran its epilogue, so
        // `fillInStackTrace` walks a stack that no longer has them. `sig`
        // carries the snapshot the helper took while they were live; without
        // draining it here the throwable keeps an EMPTY trace, and the snapshot
        // is left in the cell for the next take, which belongs to a different
        // throwable.
        let trap_snapshot = sig.trap_frames.take();
        let msg = cratonvm_types::error::out_of_bounds_message::check_index(index, length);
        match crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some(&msg),
        ) {
            Ok(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    trap_snapshot,
                );
                return oneshot_route_exception(
                    shared,
                    thread,
                    cached,
                    JitThrowPc::Unknown,
                    exc,
                    args_slice,
                    frames_depth_on_entry,
                );
            }
            Err(other) => return Err(other),
        }
    }
    if sig.arithmetic {
        let trap_snapshot = sig.trap_frames.take();
        match crate::runtime::exceptions::throw_runtime_error(
            shared,
            thread,
            RuntimeError::ArithmeticException {
                message: "/ by zero".to_string(),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => {
                crate::runtime::exceptions::attach_snapshotted_trap_frames(
                    shared,
                    &thread.frames,
                    exc,
                    trap_snapshot,
                );
                return oneshot_route_exception(
                    shared,
                    thread,
                    cached,
                    JitThrowPc::Unknown,
                    exc,
                    args_slice,
                    frames_depth_on_entry,
                );
            }
            other => return Err(other),
        }
    }

    // Deopt. The stashed reconstructed frame is the only way this call can
    // still produce the right answer: an `Ok(None)` decline re-runs the whole
    // body from entry in the interpreter, which double-executes every side
    // effect the compiled body already committed before it trapped. So prefer
    // resuming — and, unlike the loop-integrated sink, RUN the resumed frame
    // here instead of leaving it for a stepping loop that does not exist.
    //
    // This is the arm that produced section 5.3's crash in the earlier
    // `execute_jit_call_decoded`-reusing attempt: a lambda body whose `throw`
    // sits on a cold branch compiles that branch to an uncommon trap, so the
    // FIRST throwing call deopts, `resume_from_ir_deopt` pushed a frame, and
    // the helper returned as if the value had been produced normally.
    if result == i64::MIN {
        if let Some(rframe) = cratonvm_jit::deopt::take_last_deopt() {
            return resume_deopted_body(
                shared,
                thread,
                compiled,
                cached,
                &rframe,
                frames_depth_on_entry,
                "lambda-oneshot",
            );
        }
    }
    if result == i64::MIN && deopt_signaled {
        return Ok(None);
    }

    // Normal return — convert, never push.
    Ok(Some(match return_type {
        b'I' | b'B' | b'C' | b'S' | b'Z' => Some(Value::Int(narrow_int_return(return_type, result))),
        b'J' => Some(Value::Long(result)),
        b'F' => Some(Value::Float(f32::from_bits(result as u32))), // Cast: JIT ABI -- i64 register convention
        b'D' => Some(Value::Double(f64::from_bits(result as u64))), // Cast: JIT ABI -- i64 register convention
        b'[' | b'L' => Some(Value::Object(if result == 0 {
            None
        } else {
            // SAFETY: non-zero JIT return encodes a heap pointer to a valid object header.
            Some(unsafe { crate::types::ObjectRef::from_raw(result as *mut u8) })
            // Cast: JIT ABI -- i64 register convention
        })),
        _ => None, // void
    }))
}

/// Resume a compiled body that trapped, and run the resumed frame to
/// completion here.
///
/// **This is the only correct answer for a body that has already committed a
/// side effect.** A deopt sentinel does NOT mean "nothing happened": it means
/// the compiled body ran up to `rframe.bci` and stopped. Declining — returning
/// `Ok(None)` so the caller re-enters the body from entry — therefore
/// re-executes everything before that bci a second time. `rframe` is the
/// reconstructed state that makes resuming from the trap point possible, which
/// is why it must be spent rather than dropped.
///
/// Shared by both one-shot doors into a compiled lambda impl: the interpreter's
/// [`execute_jit_call_oneshot`] and the compiled caller's
/// `jit::helpers::try_lambda_site_direct_call`. It was inlined in the first of
/// those and simply absent from the second, which dropped the frame and let its
/// caller re-run the body — see
/// `docs/known-issues/hibernate/hib-reactive-3gc-run-regressions-20260820.md`
/// section 8 for the hibernate-reactive `reactiveRemove`-fires-twice defect
/// that came out of exactly that asymmetry.
///
/// Returns:
///   * `Ok(Some(v))` — the frame was resumed and ran to completion; `v` is this
///     call's result.
///   * `Ok(None)` — no resume was possible; the owning method has been
///     de-speculated instead. The caller may re-run the body, and by doing so
///     accepts that any side effect committed before the trap happens twice.
///   * `Err(_)` — a Java exception escaped the resumed frame.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_deopted_body(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    frames_depth_on_entry: usize,
    sink_label: &str,
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    dbg_deopt_sink(sink_label, rframe, "");
    if ir_deopt_resume_enabled() && resume_from_ir_deopt(shared, thread, cached, rframe).is_some() {
        return run_pushed_frame_to_completion(shared, thread, frames_depth_on_entry).map(Some);
    }
    // ADDITIVE second arm — see `sink_precise_resume_allowed`. Without it this
    // function's own doc ("the only correct answer for a body that has already
    // committed a side effect") described something it could not do on an
    // optimizing-tier artifact, which is the tier the defect it cites needs.
    if (cratonvm_jit::deopt_real_enabled() && compiled.can_deopt_resume)
        || sink_precise_resume_allowed_for(cached, rframe)
    {
        if real_frame_deopt_resume_and_despeculate(shared, thread, compiled, cached, rframe)
            .is_some()
        {
            return run_pushed_frame_to_completion(shared, thread, frames_depth_on_entry).map(Some);
        }
    } else if !rframe.method_key.is_empty()
        && !deopt_frame_matches_method(
            rframe,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
    {
        // The stash belongs to a nested callee, not to `cached`:
        // de-speculate its real owner so it stops re-trapping.
        despeculate_stashed_frame_method(shared, rframe);
    } else {
        let despec_reason = compiled
            .deopt_points
            .iter()
            .find(|dp| dp.bci == rframe.bci)
            .map(|dp| dp.reason)
            .unwrap_or(cratonvm_jit::deopt::DeoptReason::UnreachedCode);
        let _ = crate::jit::helpers::DeoptimizationController::deoptimize(
            shared,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            despec_reason,
            rframe.bci,
        );
    }
    Ok(None)
}

/// The one-shot arm of `route_jit_signal_exception`: route an exception raised
/// inside a compiled body through that body's OWN exception table, and — when
/// a handler matched and a frame was materialised for it — run that frame to
/// completion here so the handler's result is this call's result.
///
/// `route_jit_exception_through_method` returns `FramePushed` after pushing the
/// handler frame; for the interpreter's stepping loop that IS the answer, but a
/// one-shot subroutine must finish the frame itself. `Err(ExceptionThrown)` (no
/// matching handler) needs nothing: it already propagates synchronously, which
/// is exactly what the interpreted lambda path does.
///
/// The `caller_frame_idx` argument is passed as the current top-of-stack index
/// purely to satisfy the signature — `route_jit_exception_through_method` does
/// not read it (`let _ = caller_frame_idx`), because a routed handler frame is
/// built from the CALLEE's own incoming args, never from the caller's stack.
/// The `debug_assert` below pins the only property this path depends on: that
/// the sink pushed exactly one frame, the one we are about to run.
fn oneshot_route_exception(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    throw_pc: JitThrowPc,
    exc: ObjectRef,
    fallback_locals: &[Value],
    frames_depth_on_entry: usize,
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    let idx_for_signature = thread.frames.len().saturating_sub(1);
    match route_jit_signal_exception(
        shared,
        thread,
        idx_for_signature,
        cached,
        throw_pc,
        exc,
        fallback_locals,
    )? {
        CachedCallResult::FramePushed => {
            debug_assert_eq!(thread.frames.len(), frames_depth_on_entry + 1);
            run_pushed_frame_to_completion(shared, thread, frames_depth_on_entry).map(Some)
        }
        // `Handled` / `CacheMiss` are unreachable from
        // `route_jit_exception_through_method`, which only ever returns
        // `FramePushed` or `Err`. Decline rather than invent a return value.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod elidable_ctor_policy_tests {
    use super::elidable_ctor_native_would_run;
    use crate::vm::SharedVm;
    use cratonvm_native_api::NativeKind;
    use cratonvm_types::compat::CompatibilityMode;

    /// Not a real native — never invoked by these tests, which only ask the
    /// POLICY question. A registration needs a callback, so this is one.
    fn stub(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[crate::types::Value],
    ) -> Result<Option<crate::types::Value>, cratonvm_types::error::MethodCallFailed> {
        Ok(None)
    }

    fn vm_with(mode: CompatibilityMode, kind: NativeKind) -> SharedVm {
        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = mode;
        // `JdkOnly` + the synthetic library is a rejected pair (the synthetic
        // library IS ~5,200 synthetic stubs), and `VmConfig::default()` selects
        // the synthetic library. Turn it off for BOTH arms rather than only the
        // strict one, so the two VMs differ in exactly the variable under test.
        config.use_synthetic_jdk = false;
        let mut vm = SharedVm::new(config);
        vm.natives.native_methods.register_with_kind(
            "cratonvm/test/ElidableCtorFixture",
            "<init>",
            "()V",
            stub,
            kind,
        );
        vm
    }

    /// The predicate the JIT's constructor elision consults must flip with
    /// policy for a `Bridge`, and must NOT flip for an `Intrinsic`.
    ///
    /// This is the §1.4 rule stated as the JIT sees it. Under `Compatible` a
    /// registered `<init>()V` native wins over the bytecode constructor, so
    /// eliding the call would skip it — the json-smart `HashMap` defect. Under
    /// `JdkOnly` a non-intrinsic bridge in front of concrete bytecode loses (§7
    /// step 3), so nothing is skipped and the elision is sound. An `Intrinsic`
    /// is §1.4's reviewed exception and runs in both modes, so refusing to
    /// elide must survive the mode change.
    ///
    /// Both directions are asserted because the one-sided version passes
    /// against a predicate that has been rewritten to a constant.
    #[test]
    fn ctor_elision_asks_policy_not_just_registration() {
        const FIXTURE: &str = "cratonvm/test/ElidableCtorFixture";

        let compat_bridge = vm_with(CompatibilityMode::Compatible, NativeKind::Bridge);
        assert!(
            elidable_ctor_native_would_run(&compat_bridge, FIXTURE),
            "Compatible must keep the pre-policy behaviour: a registered <init> \
             native wins, so the constructor call may not be elided"
        );

        let strict_bridge = vm_with(CompatibilityMode::JdkOnly, NativeKind::Bridge);
        assert!(
            !elidable_ctor_native_would_run(&strict_bridge, FIXTURE),
            "JdkOnly sends a Bridge standing in front of concrete bytecode to the \
             bytecode (contract §7 step 3), so nothing is skipped by eliding and \
             the old blanket refusal was pessimism"
        );

        let strict_intrinsic = vm_with(CompatibilityMode::JdkOnly, NativeKind::Intrinsic);
        assert!(
            elidable_ctor_native_would_run(&strict_intrinsic, FIXTURE),
            "an Intrinsic is §1.4's reviewed exception and still runs under strict \
             policy, so eliding its constructor would skip it"
        );

        let compat_intrinsic = vm_with(CompatibilityMode::Compatible, NativeKind::Intrinsic);
        assert!(elidable_ctor_native_would_run(&compat_intrinsic, FIXTURE));
    }

    /// A class with NO registered `<init>()V` is answered `false` in both
    /// modes — otherwise the predicate would refuse every elision and read as
    /// working while doing nothing.
    #[test]
    fn an_unregistered_ctor_never_blocks_elision() {
        for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
            let vm = vm_with(mode, NativeKind::Bridge);
            assert!(
                !elidable_ctor_native_would_run(&vm, "cratonvm/test/NoSuchFixture"),
                "{mode:?}: an unregistered triple must not block elision"
            );
        }
    }
}

#[cfg(test)]
mod supersede_classification_tests {
    use super::{classify_supersede, note_supersede_outcome, supersede_census, SupersedeOutcome};

    /// The three outcomes a C2 publish can have, and which of them the epoch
    /// bump is for.
    #[test]
    fn classify_supersede_separates_the_three_publish_outcomes() {
        // No predecessor: `promote_scalar_selfrec_to_ir` reaches C2 without a
        // C1 body ever existing, and the old diagnostic reported that as
        // `c1=?` — indistinguishable from a failed lookup.
        assert_eq!(
            classify_supersede(None, Some(&[0x90, 0xc3])),
            SupersedeOutcome::FirstPublish,
        );
        assert_eq!(
            classify_supersede(Some(&[0x90, 0xc3]), Some(&[0x90, 0xc3])),
            SupersedeOutcome::Unchanged,
        );
        assert_eq!(
            classify_supersede(Some(&[0x90, 0xc3]), Some(&[0x31, 0xc0, 0xc3])),
            SupersedeOutcome::Changed,
        );
    }

    /// A missing replacement must NOT read as one of the two cheap outcomes.
    /// This decides whether to skip an invalidation, so "I could not tell" has
    /// to fail towards the historical unconditional bump.
    #[test]
    fn classify_supersede_fails_towards_bumping_when_the_replacement_is_unknown() {
        assert_eq!(
            classify_supersede(Some(&[0x90]), None),
            SupersedeOutcome::Changed,
        );
    }

    /// Same length is NOT the same body, and this is the case that refuted the
    /// hypothesis this classifier was built for: a C2 task that falls back to
    /// the single-pass backend recompiles the same bytecode, but each compile
    /// embeds fresh `JitInvokeInfo` pointers as absolute immediates
    /// (`emit_mov_imm64(ARG_REGS[1], info as *const _ as i64)`), so the bodies
    /// differ in 0.1-1% of their bytes. Measured on CratonBench: 6 of 5193
    /// bytes for `sieve`, 379 of 35686 for `Pattern.clazz`.
    #[test]
    fn classify_supersede_does_not_treat_equal_length_as_equal_code() {
        let before = [0x48, 0xb8, 0x00, 0x10, 0x20, 0x30];
        let after = [0x48, 0xb8, 0x00, 0x10, 0x99, 0x30];
        assert_eq!(before.len(), after.len());
        assert_eq!(
            classify_supersede(Some(&before), Some(&after)),
            SupersedeOutcome::Changed,
            "a relocated absolute immediate is a different body as far as byte              equality is concerned; treating equal length as equal code would              skip an invalidation that IS needed when the code really changed",
        );
    }

    /// The census must move, or a "no regression" reading cannot be told apart
    /// from "the classifier never ran".
    #[test]
    fn supersede_census_counts_each_outcome() {
        let (f0, u0, c0) = supersede_census();
        note_supersede_outcome(SupersedeOutcome::FirstPublish);
        note_supersede_outcome(SupersedeOutcome::Unchanged);
        note_supersede_outcome(SupersedeOutcome::Changed);
        note_supersede_outcome(SupersedeOutcome::Changed);
        let (f1, u1, c1) = supersede_census();
        assert_eq!((f1 - f0, u1 - u0, c1 - c0), (1, 1, 2));
    }
}

/// Re-offer held deferred-`new` retries in every live VM, called immediately
/// after a class-manager write guard releases its lock.
///
/// That is the moment a `new` site can have become resolvable, and it is the
/// only moment: a method holding a retry already has a body, so nothing
/// compiles it again and no compile-door sweep will ever look at it. Placed
/// beside `drain_pending_class_hooks` for the same reason that call is there —
/// the write lock is gone, so taking a fresh read lock here is safe.
pub fn resweep_deferred_new_after_class_definition() {
    if cratonvm_jit::held_deferred_new_count() == 0 {
        return;
    }
    for shared in crate::vm::vm_init::live_hook_vms_for_jit() {
        resweep_held_deferred_new_retries(&shared, true);
    }
}
