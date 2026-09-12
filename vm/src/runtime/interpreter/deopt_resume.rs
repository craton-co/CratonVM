// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Rebuilding an interpreter frame from a compiled one.
//!
//! A deoptimisation hands back a `FrameState` — locals and operand stack as
//! `FrameValue`s — and this module turns that into a real `Frame` the
//! interpreter can resume in. `build_deopt_frame_inner` does the
//! construction; `verify_reconstructed_frame` and
//! `verify_reconstructed_oops` check it before anything runs on it,
//! because the failure mode of a wrong reconstruction is a reference-typed
//! slot holding an integer.
//!
//! `transfer_osr_exit_into_live_frame_checked` is the OSR-exit half: the
//! same reconstruction, but written into a frame that already exists and is
//! currently executing.
//!
//! `despeculate_stashed_frame_method` is the part that makes deopt
//! *progress* rather than loop — resuming without withdrawing the
//! assumption that failed just re-enters the same compiled code and traps
//! again.

use super::*;

// ---------------------------------------------------------------------------
// Exception routing and handler search
// ---------------------------------------------------------------------------
//
// Interpreted and compiled handler search, and the two routes across the
// boundary between them: `interpreter/exception_dispatch.rs`.

/// real-frame-deopt: `true` (default OFF) when an IR-path deopt should resume
/// the interpreter at the trapping bci from the reconstructed frame, instead of
/// re-running the method from entry. Gated by `CRATONVM_IR_DEOPT_RESUME` while
/// it soaks — the precise resume is unvalidated against the full VM suite.
///
/// # "default OFF is inert" was true once, and stopped being true
///
/// This doc used to finish "…and no production IR method emits a deopt guard
/// yet, so default OFF is inert." That clause is FALSE and was load-bearing for
/// a real defect: it is why three `jit_bridge` sinks could gate their precise
/// resume behind this flag and read as harmless, while in fact they fell
/// through to re-running the method from entry and repeating any side effect
/// the compiled body had already committed
/// (`jit-bridge-sinks-re-ran-a-side-effecting-body-FIXED-20260907.md`).
///
/// Production IR methods emit deopt guards routinely — array access, field
/// access and division all lower to one, and every `invokedynamic` the tier
/// cannot lower gets an unconditional planted trap. Measured 2026-09-07 on a
/// single `ASTParserLoadingTest` run: **27 547 traps taken at runtime.**
///
/// What makes default OFF tolerable now is NOT inertness. It is that the sinks
/// no longer depend on this flag to resume: they ask
/// [`sink_precise_resume_allowed`], which is default ON. This flag now governs
/// only the `resume_from_ir_deopt` path, whose distinguishing capability is
/// inlined-caller chains (`materialise_inlined_chain`) — a case
/// `build_deopt_frame_inner` declines and counts as
/// `DeoptFrameBail::InlinedChain`, and which has not been observed to occur.
pub(super) fn ir_deopt_resume_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG
        .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_IR_DEOPT_RESUME").is_some())
}

/// Map ONE reconstructed `FrameValue` (already resolved in-stub against the
/// machine state) to an interpreter `Value`. Handles the type-source kinds the
/// producer can emit: a cat-1 `Int`, a cat-2 `Long`, cat-1 `Float` / cat-2
/// `Double` (deopt-osr P2 — raw IEEE-754 bits → `f32`/`f64`), and an
/// object-reference (`StackSlotRef`, resolved in-stub to a raw heap-pointer
/// word). Returns `None` for any not-yet-reconstructable variant
/// (`Unsupported`/`VirtualObject`/`VirtualObjectRef`, or an unresolved
/// `Register`/`StackSlot*` which should never reach here), so the caller falls
/// back to the safe re-run path rather than materialise a mistyped slot.
///
/// `Object(w)` is the `real-frame-deopt` type source for ref-typed slots (e.g.
/// an instance method's `this`): `w` is the raw heap pointer captured **in-stub**
/// at the guard (0 == null). No Java allocation runs between that capture and the
/// frame push (`refill_pools_from_shared` only recycles Rust buffers — see
/// `resume_from_ir_deopt`), so the pointer stays valid and needs no temporary GC
/// root here; once the frame is pushed its slots are scanned as roots. (A
/// GC-backed `VirtualObject` materialisation — which *does* allocate — is Phase
/// B and is still rejected via the `None` arm.)
pub(super) fn fv_to_value(v: &cratonvm_jit::deopt::FrameValue) -> Option<Value> {
    use cratonvm_jit::deopt::FrameValue;
    match v {
        FrameValue::Int(i) => Some(Value::Int(*i as i32)),
        FrameValue::Long(l) => Some(Value::Long(*l)),
        // FP-slot resume: a `float`/`double` live at a deopt guard is carried as
        // raw bits (`Float` = low-32, `Double` = full-64) and rebuilt into the
        // typed `Value`. A `Double` is cat-2 (one compact operand-stack slot, two
        // JVM local slots — see `ir_deopt_locals`).
        FrameValue::Float(bits) => Some(Value::Float(f32::from_bits(*bits as u32))),
        FrameValue::Double(bits) => Some(Value::Double(f64::from_bits(*bits))),
        FrameValue::Object(w) => Some(match *w {
            0 => Value::Object(None),
            // SAFETY: `w` is a live, 8-byte-aligned heap pointer read
            // synchronously at the guard; no GC has run since (see above).
            p => Value::Object(Some(unsafe { ObjectRef::from_raw(p as *mut u8) })),
        }),
        FrameValue::Undefined => Some(Value::Int(0)),
        _ => None,
    }
}

/// Map the reconstructed **operand stack** `FrameValue`s to interpreter
/// `Value`s, 1:1. The interpreter's operand stack is a *compact* value stack —
/// one slot per value, including a cat-2 `long` (`push_long` advances by one
/// compact slot, KIND_LONG) — and the IR abstract stack likewise carries one
/// entry per `long`, so no two-slot expansion is needed here.
pub(super) fn ir_deopt_frame_values(
    vals: &[cratonvm_jit::deopt::FrameValue],
) -> Option<Vec<Value>> {
    vals.iter().map(fv_to_value).collect()
}

// (`ir_deopt_frame_values_with_objects` — the 1:1 cat-2-REJECTING mapper — was
// retired here: its sole caller, the OSR-exit in-place transfer, now uses the
// cat-2/FP-aware 1:1 `ir_deopt_frame_values` (full-workspace caller audit done,
// applying the deletion lesson from its earlier dangling-call breakage).)

/// Map the reconstructed **locals** `FrameValue`s to a *compact* interpreter
/// arg list for `Frame::new_pooled`. Unlike the operand stack, JVM local slots
/// are category-2 *two-slot*: a `long`/`double` at slot `i` reserves the upper
/// half at `i+1`, which the snapshot records as `Undefined`/`HighHalf`.
/// `copy_args_to_locals` (inside `new_pooled`) re-expands each cat-2 arg back into
/// its two slots, so we must hand it a COMPACT list (one entry per cat-2 value) —
/// passing the JVM-slot-indexed snapshot verbatim, with its placeholder, would
/// mis-align every subsequent local. We therefore skip the reserved upper-half
/// slot after each `Long`/`Double`.
pub(super) fn ir_deopt_locals(vals: &[cratonvm_jit::deopt::FrameValue]) -> Option<Vec<Value>> {
    use cratonvm_jit::deopt::FrameValue;
    let mut out = Vec::with_capacity(vals.len());
    let mut i = 0;
    while i < vals.len() {
        out.push(fv_to_value(&vals[i])?);
        // Both `long` and `double` are category-2 (two JVM local slots): the
        // snapshot reserves the upper-half slot (recorded as `Undefined`), which
        // `copy_args_to_locals` re-creates from the compact list, so skip it here.
        i += if matches!(vals[i], FrameValue::Long(_) | FrameValue::Double(_)) {
            2
        } else {
            1
        };
    }
    Some(out)
}

/// Resume interpretation from an IR-path deopt: build a frame for the deopting
/// method (`cached`) with the reconstructed locals/operand-stack and resume at
/// the reconstructed bci. Returns `Some(FramePushed)` on success, or `None` to
/// fall back to the re-run path (unmappable values, inlined caller chain, or
/// held monitors — none handled in this first cut). Mirrors the frame-push
/// ordering of `route_jit_exception_through_method` (refill pools, build via
/// `Frame::new_pooled`, push, set pc) so GC-root and pool handling match.
pub(super) fn resume_from_ir_deopt(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<CachedCallResult> {
    // CRATONVM_DBG_DEOPT: trace the resume decision (PRECISE mid-bci resume vs
    // FALLBACK re-run, with the reason) so the type source can be validated
    // live — e.g. an instance method's `this` resolving to `Value::Object(..)`
    // rather than a truncated `Value::Int`. Cheap (only when the var is set).
    let trace = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some();
    let bail = |why: &str| -> Option<CachedCallResult> {
        if trace {
            eprintln!(
                "[cratonvm-deopt] FALLBACK re-run {}.{}{} at bci={} ({why})",
                cached.class_name, cached.method_name, cached.method_descriptor, rframe.bci,
            );
        }
        None
    };
    // An inlined chain materialises every frame it names, outermost first, and
    // only if EVERY one of them checks out — see `materialise_inlined_chain`.
    // Before 2026-08-18 this sink refused the chain outright and took the
    // whole-method re-run, which is what made an inlined body that publishes
    // deopt metadata unrepresentable and therefore forbidden at the splice.
    //
    // A refusal still falls back to that re-run, so the worst case is exactly
    // the old behaviour.
    if !rframe.caller_frames.is_empty() {
        return match materialise_inlined_chain(shared, cached, rframe) {
            Ok(chain) => push_inlined_chain(shared, thread, chain, trace),
            Err(why) => bail(&format!("inlined caller chain: {why}")),
        };
    }
    if !rframe.monitors.is_empty() {
        return bail("held monitors");
    }
    // Locals use the cat-2-aware compactor (a `long` is two JVM slots, re-expanded
    // by copy_args_to_locals); the operand stack is compact (one slot per value).
    let locals = match ir_deopt_locals(&rframe.locals) {
        Some(l) => l,
        None => return bail("unmappable local slot"),
    };
    let stack_vals = match ir_deopt_frame_values(&rframe.stack) {
        Some(s) => s,
        None => return bail("unmappable stack slot"),
    };

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
        &locals,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    // Populate the operand stack and resume pc BEFORE pushing the frame, so that
    // when `push_frame_and_fire_entry` fires a JVMTI MethodEntry callback (which
    // may allocate Java heap and trigger a GC) EVERY reconstructed oop — locals
    // AND operand-stack refs — is already in a GC-scanned frame slot. (Review
    // finding: a reconstructed stack ref held only in the Rust `stack_vals` Vec
    // was momentarily unrooted across the fire; locals were already safe.)
    for v in stack_vals {
        frame.stack.push(v).ok()?;
    }
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    frame.pc = rframe.bci as usize;
    if trace {
        eprintln!(
            "[cratonvm-deopt] PRECISE resume {}.{}{} at bci={} locals={:?}",
            cached.class_name, cached.method_name, cached.method_descriptor, rframe.bci, locals,
        );
    }
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);
    // P1 shadow record (`docs/threading/thread-transition-states.md` §7.2):
    // the `Deoptimizing -> JavaRunning` edge. The reconstructed values now live
    // in a GC-scanned interpreter frame, which is precisely the property the
    // `Deoptimizing` state exists to say the thread did NOT have. Usually a
    // no-op self-edge — the JIT entry pop that returned us here already
    // resolved the window (see `conservative_roots::leaving_compiled_state`) —
    // but this is the site that closes it for any deopt path that materialises
    // frames without an intervening pop.
    crate::threading::thread_state::record_transition(
        crate::threading::thread_state::ThreadExecState::JavaRunning,
        "interpreter::resume_from_ir_deopt",
    );
    Some(CachedCallResult::FramePushed)
}

/// How deep an inlined caller chain this sink will materialise.
///
/// HotSpot's own inlining depth limit is 9 and the IR-side
/// `MAX_INLINE_SCOPE_DEPTH` is 64; this is the *resume* budget, which is a
/// different question — every frame in the chain is one interpreter frame this
/// sink must push atomically, and a deopt that half-pushes is unrecoverable.
/// Refusing a deeper chain costs a whole-method re-run, which is the same
/// outcome as refusing the chain outright and is what happened before.
///
/// Defined as the jit crate's constant rather than repeating the number:
/// `CompiledMethod::osr_exit_policy` refuses at ADMISSION any chain deeper than
/// this, so that an OSR entry is never spent reaching a reject this sink was
/// always going to give. Two copies of the number would let those two drift.
pub(super) const MAX_INLINE_RESUME_DEPTH: usize = cratonvm_jit::deopt::MAX_OSR_INLINE_RESUME_DEPTH;

/// The bci to park an inlined CALLER frame at.
///
/// A caller scope's `bci` names an `invoke` that is **already in progress** —
/// `ResumeSemantics::for_caller_scope()` is `RESUME`, not `REEXECUTE`. Parking
/// the interpreter at that bci would call the callee a second time, which is
/// the double-execution defect the whole deopt-resume contract exists to
/// prevent, one bytecode instead of one loop iteration. The frame must resume
/// at the invoke's SUCCESSOR, so that when the frame above it returns, the
/// interpreter's ordinary return protocol pushes the result onto this frame's
/// operand stack and carries on.
///
/// This is exactly the computation `jit/src/lib.rs` cannot do — its comment on
/// `ResumeSemantics::RESUME` says "computing the successor bci needs the
/// method's bytecode, which this crate does not have, so such a point is
/// refused at admission". The VM has the bytecode, so it is computed here.
///
/// Fail-closed on anything that is not an invoke: a caller scope whose bci does
/// not name a call is a malformed chain, not a resumable frame.
pub(super) fn caller_resume_pc(
    code: &[u8],
    code_len: usize,
    invoke_bci: usize,
) -> Result<usize, String> {
    if invoke_bci >= code_len {
        return Err(format!(
            "caller bci {invoke_bci} is past the end of a {code_len}-byte method"
        ));
    }
    // 0xb6 invokevirtual / 0xb7 invokespecial / 0xb8 invokestatic are 3 bytes;
    // 0xb9 invokeinterface and 0xba invokedynamic are 5. Nothing else may carry
    // a caller scope.
    let len = match code[invoke_bci] {
        0xb6 | 0xb7 | 0xb8 => 3usize,
        0xb9 | 0xba => 5,
        other => {
            return Err(format!(
                "caller bci {invoke_bci} holds opcode {other:#04x}, which is not an invoke"
            ))
        }
    };
    let next = invoke_bci + len;
    if next > code_len {
        return Err(format!(
            "the invoke at caller bci {invoke_bci} runs past the end of a {code_len}-byte method"
        ));
    }
    Ok(next)
}

/// Map one reconstructed scope to the `(locals, stack)` a FRESH interpreter
/// frame is built from.
///
/// The difference from the in-place OSR transfer is `Unsupported`, and it is
/// the whole reason this is a separate function. That transfer tolerates an
/// `Unsupported` LOCAL because the live frame already holds a value there and
/// the verified bytecode proves the slot is dead or re-stored before it is
/// read. **A materialised frame has no such value** — there is nothing to leave
/// in place, and every resume sink maps a missing slot to `Value::Int(0)`, so
/// tolerating it here would resume a caller with silently-zeroed locals. That
/// is the failure `FrameValue::MaterializationRequired` exists to make
/// impossible, and `docs/jit/deopt-inline-scopes.md` makes the same point about
/// an undescribed caller frame: it lowers to `[Unsupported]` precisely so this
/// consumer refuses it rather than inventing an empty frame.
pub(super) fn caller_frame_values(
    rf: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Result<(Vec<Value>, Vec<Value>), String> {
    use cratonvm_jit::deopt::FrameValue;
    if !rf.monitors.is_empty() {
        return Err(format!(
            "caller scope {} holds {} monitor(s)",
            rf.method_key,
            rf.monitors.len()
        ));
    }
    if let Some(i) = rf
        .locals
        .iter()
        .position(|v| matches!(v, FrameValue::Unsupported))
    {
        return Err(format!(
            "caller scope {} local {i} is Unsupported, and a materialised frame has no \
             existing value to leave in place",
            rf.method_key
        ));
    }
    let locals = ir_deopt_locals(&rf.locals)
        .ok_or_else(|| format!("caller scope {} has an unmappable local", rf.method_key))?;
    let stack = ir_deopt_frame_values(&rf.stack).ok_or_else(|| {
        format!(
            "caller scope {} has an unmappable stack slot",
            rf.method_key
        )
    })?;
    Ok((locals, stack))
}

/// One materialised frame of an inlined caller chain, ready to push.
pub(super) struct InlinedChainFrame {
    pub(super) cached: Arc<CachedBytecodeMethod>,
    pub(super) locals: Vec<Value>,
    pub(super) stack: Vec<Value>,
    /// Where to park: an invoke's successor for a caller scope, and the
    /// snapshot's own bci for the innermost (trapping) scope.
    pub(super) resume_pc: usize,
}

/// Resolve an inlined callee named by a caller scope's `method_key`, in the
/// loader context of the method that inlined it.
///
/// `FrameState::method_key` is a bare `"class.name:descriptor"` string with no
/// `ClassId`, and a name alone does not identify a class — two loaders can
/// define the same name. Resolving relative to the ENCLOSING method's declaring
/// class is not a guess: it is the same context `resolve_inline_site_from` used
/// when it chose the body to splice, so this walk reaches the same method the
/// compiler inlined or it reaches nothing.
pub(super) fn resolve_inlined_callee(
    shared: &SharedVm,
    enclosing_class_id: ClassId,
    method_key: &str,
) -> Result<Arc<CachedBytecodeMethod>, String> {
    let Some((rest, desc)) = method_key.rsplit_once(':') else {
        return Err(format!("unparseable method key {method_key:?}"));
    };
    let Some((class_name, method_name)) = rest.rsplit_once('.') else {
        return Err(format!("unparseable method key {method_key:?}"));
    };
    let cm = shared.classes.class_manager.read();
    let Some(cid) = cm.find_class_by_name_for_class(class_name, enclosing_class_id) else {
        return Err(format!(
            "{class_name} is not resolvable from the class that inlined it"
        ));
    };
    // Through `MemberResolver`, not `find_method_recursive` directly. That is
    // the one entry point for resolution (`runtime::resolve::guard` enforces
    // it): it requires a VM identity, so a `ClassId` minted by another VM
    // cannot be resolved against this one, and it distinguishes NoSuchMethod
    // from "not cached". Both matter here — `method_key` is a bare string
    // carrying neither a VM nor a loader, which is exactly the ambiguity this
    // door exists to close.
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let (declaring, index) = resolver
        .declared_method(&cm, resolver.scope(cid), method_name, desc)
        .and_then(|scoped| resolver.adopt(scoped))
        .map_err(|e| format!("{method_key} did not resolve: {e}"))?;
    let Some(decl_class) = cm.class_store().get(declaring) else {
        return Err(format!(
            "{method_key} resolved to a class that is not loaded"
        ));
    };
    let Some(method) = decl_class.methods.get(index as usize) else {
        return Err(format!(
            "{method_key} resolved to a method index out of range"
        ));
    };
    let Some(code_attr) = method.code() else {
        return Err(format!("{method_key} has no Code attribute"));
    };
    let source_file = cm.get_class(declaring).and_then(|c| c.source_file.clone());
    Ok(Arc::new(CachedBytecodeMethod {
        declaring_class_id: declaring,
        class_name: Arc::from(class_name),
        method_name: Arc::from(method_name),
        method_descriptor: Arc::from(desc),
        source_file: source_file.map(|s| Arc::from(s.as_str())),
        code: crate::runtime::frame::padded_bytecode(&code_attr.code),
        exception_table: Arc::from(code_attr.exception_table.as_slice()),
        max_stack: code_attr.max_stack,
        max_locals: code_attr.max_locals,
        // Widening: parameter count fits u16.
        num_params: crate::runtime::interpreter::count_method_params(desc) as u16,
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
    }))
}

/// Materialise a whole inlined deopt chain, OUTERMOST first, or refuse it.
///
/// **Step 1 of the five-step chain in
/// `docs/known-issues/netty/httpresponsestatustest-exhaustive-loop-timeout-20260816.md`.**
/// Every resume sink in this file refuses `caller_frames` outright today, which
/// is why `try_emit_inline_site` may not splice a body that publishes deopt
/// metadata, which is why the inliner cannot nest, which is why that class
/// cannot reach its budget. This is the function that has to exist before any
/// of that moves.
///
/// `rframe` is the innermost (trapping) scope and `rframe.caller_frames` runs
/// innermost-first, so the chain to push is `caller_frames` reversed, then
/// `rframe` itself. `outermost` is the compiled method the artifact belongs to,
/// and the LAST caller scope must name it — an artifact only ever inlines
/// *into* its own body, so a chain whose outermost scope is some other method
/// is mis-routed, not deep.
///
/// **Nothing is pushed here.** The caller pushes the returned frames, and every
/// check that can refuse runs first, because a deopt that half-materialises a
/// chain has no recovery: the safe fallback (a whole-method re-run) is only
/// available while no frame has been pushed.
pub(super) fn materialise_inlined_chain(
    shared: &SharedVm,
    outermost: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Result<Vec<InlinedChainFrame>, String> {
    let depth = rframe.caller_frames.len();
    if depth > MAX_INLINE_RESUME_DEPTH {
        return Err(format!(
            "inlined chain is {depth} deep, past the {MAX_INLINE_RESUME_DEPTH}-frame resume budget"
        ));
    }
    let Some(outer_scope) = rframe.caller_frames.last() else {
        return Err("no caller frames".to_string());
    };
    if !deopt_frame_matches_method(
        outer_scope,
        &outermost.class_name,
        &outermost.method_name,
        &outermost.method_descriptor,
    ) {
        return Err(format!(
            "outermost caller scope {} does not name the compiled method {}.{}{}",
            outer_scope.method_key,
            outermost.class_name,
            outermost.method_name,
            outermost.method_descriptor
        ));
    }

    let mut out: Vec<InlinedChainFrame> = Vec::with_capacity(depth + 1);
    // The outermost scope IS the compiled method, so it needs no resolution;
    // it is built here and every scope beneath it by the shared walk below.
    let (locals, stack) = caller_frame_values(outer_scope)?;
    // `cached.code` carries 2 bytes of speculative-read padding.
    let code_len = outermost.code.len().saturating_sub(2);
    let resume_pc = caller_resume_pc(&outermost.code, code_len, outer_scope.bci as usize)?;
    if locals.len() > outermost.max_locals as usize || stack.len() > outermost.max_stack as usize {
        return Err(format!(
            "outermost scope {} does not fit its own frame",
            outer_scope.method_key
        ));
    }
    out.push(InlinedChainFrame {
        cached: outermost.clone(),
        locals,
        stack,
        resume_pc,
    });
    out.extend(materialise_inner_scopes(
        shared,
        outermost.declaring_class_id,
        rframe,
    )?);
    Ok(out)
}

/// Materialise every scope BENEATH the outermost one, outermost-first.
///
/// Shared by the two sinks that differ only in what happens to the outermost
/// frame: the deopt-exit sink pushes it like any other
/// ([`materialise_inlined_chain`]), while the OSR-exit sink writes it into the
/// live interpreter frame in place ([`transfer_osr_exit_chain_into_live_frame`])
/// because that frame is the one OSR replaced and is still on the stack.
/// Everything below the outermost is identical in both, which is why it is one
/// function rather than two that must be kept in step.
///
/// `enclosing_class_id` is the outermost method's declaring class; each scope is
/// resolved in the loader context of the scope that encloses it, and the walk
/// carries that context inward.
fn materialise_inner_scopes(
    shared: &SharedVm,
    enclosing_class_id: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Result<Vec<InlinedChainFrame>, String> {
    let depth = rframe.caller_frames.len();
    let mut out: Vec<InlinedChainFrame> = Vec::with_capacity(depth);
    let mut enclosing = enclosing_class_id;
    // `caller_frames` is innermost-first and the outermost is handled by the
    // caller, so this is every scope except the last, walked outermost-inward.
    for scope in rframe.caller_frames.iter().rev().skip(1) {
        let cached = resolve_inlined_callee(shared, enclosing, &scope.method_key)?;
        let (locals, stack) = caller_frame_values(scope)?;
        let code_len = cached.code.len().saturating_sub(2);
        let resume_pc = caller_resume_pc(&cached.code, code_len, scope.bci as usize)?;
        if locals.len() > cached.max_locals as usize || stack.len() > cached.max_stack as usize {
            return Err(format!(
                "caller scope {} does not fit its own frame ({} locals / {} stack against {}/{})",
                scope.method_key,
                locals.len(),
                stack.len(),
                cached.max_locals,
                cached.max_stack
            ));
        }
        enclosing = cached.declaring_class_id;
        out.push(InlinedChainFrame {
            cached,
            locals,
            stack,
            resume_pc,
        });
    }

    // The innermost (trapping) scope. Its bci is a REEXECUTE point — the
    // bytecode there has not taken effect — so it is parked AT its own bci, not
    // at a successor.
    let innermost = resolve_inlined_callee(shared, enclosing, &rframe.method_key)?;
    let (locals, stack) = caller_frame_values(rframe)?;
    if locals.len() > innermost.max_locals as usize || stack.len() > innermost.max_stack as usize {
        return Err(format!(
            "trapping scope {} does not fit its own frame",
            rframe.method_key
        ));
    }
    out.push(InlinedChainFrame {
        cached: innermost,
        locals,
        stack,
        resume_pc: rframe.bci as usize,
    });
    Ok(out)
}

/// Push a materialised inlined chain, outermost first.
///
/// Separate from [`materialise_inlined_chain`] so the fail-closed split is
/// structural rather than a convention: everything that can refuse happens
/// before this function is called, and this function cannot fail in a way that
/// leaves a partial chain. The only fallible step left is the operand-stack
/// push, and the frame's own `max_stack` was checked against the snapshot
/// during materialisation, so it cannot overflow here.
fn push_inlined_chain(
    shared: &SharedVm,
    thread: &mut JvmThread,
    chain: Vec<InlinedChainFrame>,
    trace: bool,
) -> Option<CachedCallResult> {
    if trace {
        let path = chain
            .iter()
            .map(|f| {
                format!(
                    "{}.{}@{}",
                    f.cached.class_name, f.cached.method_name, f.resume_pc
                )
            })
            .collect::<Vec<_>>()
            .join(" -> ");
        eprintln!(
            "[cratonvm-deopt] PRECISE resume of a {}-frame inlined chain: {path}",
            chain.len()
        );
    }
    for f in chain {
        thread.refill_pools_from_shared(
            &shared.mem.operand_stack_pool,
            &shared.mem.tag_pool,
            // Widening: small unsigned -> usize (non-negative, fits)
            f.cached.max_locals as usize,
            // Widening: small unsigned -> usize (non-negative, fits)
            (f.cached.max_stack as usize).max(16) + 8,
        );
        let mut frame = crate::runtime::frame::Frame::new_pooled(
            f.cached.declaring_class_id,
            f.cached.class_name.clone(),
            f.cached.method_name.clone(),
            f.cached.method_descriptor.clone(),
            f.cached.source_file.clone(),
            f.cached.code.clone(),
            f.cached.exception_table.clone(),
            f.cached.max_stack,
            f.cached.max_locals,
            &f.locals,
            &mut thread.locals_pool,
            &mut thread.stacks_pool,
        );
        // Stack and pc BEFORE the push, for the same reason the single-frame
        // sink does it: `push_frame_and_fire_entry` can fire a JVMTI
        // MethodEntry callback that allocates, and every reconstructed oop must
        // already be in a GC-scanned frame slot when it does.
        for v in f.stack {
            frame.stack.push(v).ok()?;
        }
        frame.pc = f.resume_pc;
        push_frame_and_fire_entry(shared.vm_identity, thread, frame);
    }
    crate::threading::thread_state::record_transition(
        crate::threading::thread_state::ThreadExecState::JavaRunning,
        "interpreter::resume_from_ir_deopt(inlined chain)",
    );
    Some(CachedCallResult::FramePushed)
}

/// CRATONVM_DEOPT_VERIFY (x64-backport Step 5 / Workstream A4) — structural
/// invariant check on a reconstructed deopt frame, run BEFORE it is resumed.
/// This is the first, always-sound layer of the differential verifier: it catches
/// the corruption a snapshot/regalloc map drift most often produces — a slot count
/// past the method's declared maxima (e.g. a one-slot-shifted snapshot) or a
/// malformed virtual-object descriptor / dangling `VirtualObjectRef` — WITHOUT the
/// false positives a naive per-slot type check would raise on the legitimate,
/// re-running cat-2/FP `Unsupported` slots. A violation is reported loudly and
/// makes the sink fall back to the safe re-run (fail-safe). The heavier eager-
/// deopt differential (force a deopt at a non-failing guard, compare the
/// reconstructed-interpreter end-result against the JIT result) is the follow-up
/// layer — see `docs/feature-designs/deopt-osr-steps789-handoff.md`.
///
/// Pure (no heap deref, no env read) so it is unit-testable; the `CRATONVM_DEOPT_VERIFY`
/// gate is consulted by the caller (`build_deopt_frame_inner`).
pub(super) fn verify_reconstructed_frame(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    max_locals: u16,
    max_stack: u16,
) -> Result<(), String> {
    use cratonvm_jit::deopt::FrameValue;
    use std::collections::BTreeSet;

    if rframe.locals.len() > max_locals as usize {
        return Err(format!(
            "locals {} exceed max_locals {}",
            rframe.locals.len(),
            max_locals
        ));
    }
    if rframe.stack.len() > max_stack as usize {
        return Err(format!(
            "stack {} exceeds max_stack {}",
            rframe.stack.len(),
            max_stack
        ));
    }

    // Collect every virtual-object id DEFINED in the frame (recursing through
    // field graphs), checking each descriptor's declared-vs-actual field count.
    fn walk_defs(vals: &[FrameValue], defined: &mut BTreeSet<usize>) -> Result<(), String> {
        for v in vals {
            if let FrameValue::VirtualObject(state) = v {
                if state.field_values.len() != state.num_fields {
                    return Err(format!(
                        "virtual object id {} declares {} fields but carries {} values",
                        state.id,
                        state.num_fields,
                        state.field_values.len()
                    ));
                }
                defined.insert(state.id);
                walk_defs(&state.field_values, defined)?;
            }
        }
        Ok(())
    }
    let mut defined = BTreeSet::new();
    walk_defs(&rframe.locals, &mut defined)?;
    walk_defs(&rframe.stack, &mut defined)?;

    // Every VirtualObjectRef(id) must resolve to a VirtualObject defined in the
    // frame (else materialization would later fail with an unknown id).
    fn walk_refs(vals: &[FrameValue], defined: &BTreeSet<usize>) -> Result<(), String> {
        for v in vals {
            match v {
                FrameValue::VirtualObjectRef(id) if !defined.contains(id) => {
                    return Err(format!(
                        "virtual object ref {id} has no defining VirtualObject in the frame"
                    ));
                }
                FrameValue::VirtualObject(state) => walk_refs(&state.field_values, defined)?,
                _ => {}
            }
        }
        Ok(())
    }
    walk_refs(&rframe.locals, &defined)?;
    walk_refs(&rframe.stack, &defined)?;
    Ok(())
}

/// CRATONVM_DEOPT_VERIFY oop-plausibility layer (verifier increment 2). Every
/// `Object(addr)` slot in the reconstructed frame — locals, operand stack, and
/// recursively the already-real `Object` fields of scalar-replaced descriptors —
/// must be null or a real heap address (`heap.is_heap_addr`: alignment + region
/// containment, NO header deref, so it is safe on an arbitrary garbage word).
/// This catches the most dangerous map drift the structural layer cannot: a
/// non-oop value (a small int, a stale/wild pointer) landing in a slot the frame
/// resumes as an object reference — a use-after-free on first dereference. A
/// violation forces the safe re-run. Not pure (needs the heap); the env gate is
/// consulted by the caller.
pub(super) fn verify_reconstructed_oops(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    shared: &SharedVm,
) -> Result<(), String> {
    use cratonvm_jit::deopt::FrameValue;
    fn check(vals: &[FrameValue], shared: &SharedVm, region: &str) -> Result<(), String> {
        for (i, v) in vals.iter().enumerate() {
            match v {
                FrameValue::Object(addr) if *addr != 0 => {
                    if shared.mem.heap.is_heap_addr(*addr as usize).is_none() {
                        return Err(format!(
                            "{region}[{i}] = Object(0x{addr:x}) is not a valid heap address"
                        ));
                    }
                }
                // Recurse into a scalar-replaced descriptor's already-real fields
                // (nested VirtualObject / VirtualObjectRef carry no address yet).
                FrameValue::VirtualObject(state) => check(&state.field_values, shared, "vfield")?,
                _ => {}
            }
        }
        Ok(())
    }
    check(&rframe.locals, shared, "local")?;
    check(&rframe.stack, shared, "stack")?;
    Ok(())
}

/// real-frame-deopt Step 3 — build the interpreter `Frame` for an Object-bearing
/// deopt, GC-rooting the reconstructed oops across the pool refill. Returns the
/// built `Frame` (NOT pushed onto `thread.frames`) with the oops still pinned in
/// `thread.native_pin_roots` above the caller's watermark — the CALLER must
/// `native_pin_roots.truncate(pin_base)` after discarding the frame (the
/// wrapper [`build_validate_discard_ir_deopt`] does this unconditionally).
/// `None` if the frame is out-of-scope (inlined caller chain / held monitors) or
/// carries an unmappable slot (cat-2/Unsupported/virtual/FP/unresolved).
///
/// GC-rooting: the oops are pinned BEFORE `refill_pools_from_shared` (whose
/// `acquire()` may GC). While pinned they are scanned (`memory/roots.rs`) and
/// forwarded in place by a moving collector (`memory/gc.rs`); after refill each
/// oop is RE-READ from its (forwarded) pin slot into the frame, so the frame
/// never holds a stale pre-GC address. NO JVM allocation occurs between the
/// re-read and the return (`Frame::new_pooled` / `ValueStack::push` are
/// Rust-side only), so no GC can stale the built frame. `stress` forces a GC
/// immediately before refill — the ONLY sanctioned injection point — to
/// exercise the forward-in-place path (tests / `CRATONVM_GC_STRESS`).
/// Why [`build_deopt_frame_inner`] declined to rebuild a trapped frame.
///
/// # Why this is counted at all
///
/// Before 2026-09-07 the deopt sinks answered a trap they could not resume by
/// re-running the method from entry, which for a body that had already
/// committed a store runs it twice
/// (`jit-bridge-sinks-re-ran-a-side-effecting-body-FIXED-20260907.md`). The fix
/// resumes instead — but only when the frame can be rebuilt, and "when it
/// cannot" was a single phrase covering NINE distinct causes, none of them
/// counted. So the residual could be described and not sized, and nobody could
/// say whether a given workload hits it at all, or which cause to attack first.
///
/// `try_resume_trapped_callee` learned the same lesson in its own comments: *"a
/// refusal that cannot be named cannot be counted, which is why the orphan in
/// `jit-direct-call-mints-an-orphaned-deopt-frame` was attributed to inlining
/// on no evidence."* These are that naming, for the rebuild side.
///
/// Ungated: one relaxed increment on a path that is already doing frame
/// reconstruction, and a census that is off by default is a census nobody reads
/// when the number finally matters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeoptFrameBail {
    /// The stash names an inlined caller chain. `materialise_inlined_chain`
    /// handles those for `resume_from_ir_deopt`; this builder does not.
    InlinedChain,
    /// The frame belongs to a DIFFERENT method than the one being rebuilt — a
    /// nested callee's trap whose sentinel travelled outward.
    IdentityMismatch,
    /// `bci == u32::MAX`, the superseded-guard sentinel, which exists precisely
    /// so this check fails.
    SupersededSentinel,
    /// `CRATONVM_DEOPT_VERIFY` found a structural invariant violated.
    VerifyFailed,
    /// An `ACC_SYNCHRONIZED` method carrying scalar-replaced objects: the
    /// method monitor may have been elided under that replacement.
    SynchronizedWithVirtuals,
    /// Re-materialising the scalar-replaced object graph failed.
    VirtualMaterialise,
    /// A local slot the mapper has no representation for.
    UnmappableLocal,
    /// An operand-stack slot the mapper has no representation for.
    UnmappableStack,
    /// A held monitor that is not a resolved object reference.
    BadMonitor,
    /// The reconstructed operand stack did not fit the frame's padded stack.
    StackPush,
}

impl DeoptFrameBail {
    const ALL: [DeoptFrameBail; 10] = [
        DeoptFrameBail::InlinedChain,
        DeoptFrameBail::IdentityMismatch,
        DeoptFrameBail::SupersededSentinel,
        DeoptFrameBail::VerifyFailed,
        DeoptFrameBail::SynchronizedWithVirtuals,
        DeoptFrameBail::VirtualMaterialise,
        DeoptFrameBail::UnmappableLocal,
        DeoptFrameBail::UnmappableStack,
        DeoptFrameBail::BadMonitor,
        DeoptFrameBail::StackPush,
    ];

    /// The census name. Hyphenated and stable: these are grepped out of suite
    /// logs, so renaming one silently breaks whoever is tracking it.
    pub fn name(self) -> &'static str {
        match self {
            DeoptFrameBail::InlinedChain => "inlined-caller-chain",
            DeoptFrameBail::IdentityMismatch => "identity-mismatch",
            DeoptFrameBail::SupersededSentinel => "superseded-guard-sentinel",
            DeoptFrameBail::VerifyFailed => "deopt-verify-failed",
            DeoptFrameBail::SynchronizedWithVirtuals => "synchronized-with-virtual-objects",
            DeoptFrameBail::VirtualMaterialise => "virtual-object-materialise-failed",
            DeoptFrameBail::UnmappableLocal => "unmappable-local-slot",
            DeoptFrameBail::UnmappableStack => "unmappable-stack-slot",
            DeoptFrameBail::BadMonitor => "held-monitor-not-an-object",
            DeoptFrameBail::StackPush => "operand-stack-did-not-fit",
        }
    }
}

static DEOPT_FRAME_BAILS: [std::sync::atomic::AtomicU64; 10] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 10];

/// Count one decline, and trace it under `CRATONVM_DBG_DEOPT`.
fn note_deopt_frame_bail(why: DeoptFrameBail, rframe: &cratonvm_jit::deopt::ReconstructedFrame) {
    DEOPT_FRAME_BAILS[why as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
        eprintln!(
            "[cratonvm-deopt] frame rebuild refused ({}): stash={} bci={}",
            why.name(),
            rframe.method_key,
            rframe.bci,
        );
    }
}

/// `(reason, count)` for every way a trapped frame could not be rebuilt.
///
/// A non-zero total is the size of the residual the sink fix left behind: those
/// traps still fall back to re-running the method from entry, side effects and
/// all. Zero means every trap this run took was resumed precisely.
pub fn deopt_frame_bail_counts() -> Vec<(&'static str, u64)> {
    DeoptFrameBail::ALL
        .iter()
        .map(|w| {
            (
                w.name(),
                DEOPT_FRAME_BAILS[*w as usize].load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .collect()
}

/// Total declines, across every reason — the one number a regression test
/// asserts is zero.
pub fn deopt_frame_bail_total() -> u64 {
    DEOPT_FRAME_BAILS
        .iter()
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        .sum()
}

/// Reset the census. **Tests only** — a test that warms a VM and then measures
/// one trapping call needs the warm-up's declines out of the way.
///
/// `pub` because that test is an INTEGRATION test in `vm/tests/`, a separate
/// crate that sees neither `pub(crate)` nor `#[cfg(test)]`. Carried on the
/// test-only-public-API ratchet for that reason -- see the "third disposition"
/// note in `vm/tests/no_test_only_public_api.rs`.
pub fn reset_deopt_frame_bail_counts() {
    for c in DEOPT_FRAME_BAILS.iter() {
        c.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(crate) fn build_deopt_frame_inner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    stress: bool,
) -> Option<crate::runtime::frame::Frame> {
    // Single non-inlined frame (inlined-caller chains stay out of scope). Held
    // monitors are NO LONGER a blanket bail: Phase C records `synchronized(obj)`
    // blocks over scalar-replaced objects as `MonitorInfo` and re-acquires them on
    // resume (below). The x64 producer only emits monitors over scalar objects
    // (a non-scalar elision sets `has_elided_monitor` → `can_deopt_resume=false`),
    // so every monitor here is materializable + relockable.
    if !rframe.caller_frames.is_empty() {
        note_deopt_frame_bail(DeoptFrameBail::InlinedChain, rframe);
        return None;
    }

    // Identity gate (jit-invokedynamic-groovy-regression), belt-and-suspenders
    // for every caller: the frame's baked `method_key` must name the method
    // whose bytecode/frame-shape (`cached`) we are about to materialize it
    // into. A mismatched (nested-callee) or key-less (legacy producer /
    // superseded-guard sentinel) frame bails to the safe re-run. Also bounds-
    // check the resume bci against this method's code — the superseded-guard
    // sentinel stashes `bci == u32::MAX` precisely so this fails.
    if !deopt_frame_matches_method(
        rframe,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        note_deopt_frame_bail(DeoptFrameBail::IdentityMismatch, rframe);
        return None;
    }
    if rframe.bci == u32::MAX {
        note_deopt_frame_bail(DeoptFrameBail::SupersededSentinel, rframe);
        return None;
    }

    // CRATONVM_DEOPT_VERIFY: structural-invariant check before resuming. On a
    // violation (slot count past the method maxima — e.g. a shifted snapshot — or
    // a malformed/dangling virtual descriptor) report loudly and force the safe
    // re-run. Runs on the ORIGINAL frame (virtuals intact) so the descriptor
    // checks see them. Gate is read-once; default-off ⇒ skipped entirely.
    if cratonvm_jit::deopt_verify_enabled() {
        let verdict = verify_reconstructed_frame(rframe, cached.max_locals, cached.max_stack)
            .and_then(|()| verify_reconstructed_oops(rframe, shared));
        if let Err(why) = verdict {
            eprintln!(
                "[DEOPT-VERIFY] {} bci={}: reconstructed-frame invariant violated: {why} \
                 — forcing safe re-run",
                rframe.method_key, rframe.bci
            );
            note_deopt_frame_bail(DeoptFrameBail::VerifyFailed, rframe);
            return None;
        }
    }

    // Workstream A: re-materialize scalar-replaced (virtual) objects into real
    // heap shells before mapping. The reconstructed frame may carry
    // `VirtualObject`/`VirtualObjectRef` slots (escape analysis elided the
    // allocation on the fast path); the mapper below has no representation for
    // them and would bail to re-run. Materialize them into a heap object graph
    // and rewrite the slots to real `Object` refs first.
    //
    // GC-rooting: `keep_pins = true` leaves each shell pinned in
    // `native_pin_roots`; those pins ride the SAME `pin_base..truncate` window
    // `resume_real_ir_deopt` holds across `push_frame_and_fire_entry`, so the
    // shells are rooted continuously from allocation until the resumed frame
    // roots them. On any materialization failure (unsupported field / unknown
    // id) the pins are released and we fall back to re-run (`?` → `None`).
    use cratonvm_jit::deopt::FrameValue;
    let has_virtual = rframe.locals.iter().chain(rframe.stack.iter()).any(|v| {
        matches!(
            v,
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
        )
    });
    if has_virtual && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some() {
        eprintln!(
            "[DBG_SCALAR_DEOPT] build_deopt_frame_inner: materializing virtual object(s) at bci={}",
            rframe.bci
        );
    }
    let materialized_frame;
    let rframe: &cratonvm_jit::deopt::ReconstructedFrame = if has_virtual {
        // Elided-monitor gate. Escape analysis performs lock elision over
        // non-escaping objects (`jit::escape_analysis::find_lock_elisions`): a
        // scalar-replaced object may have had its `monitorenter`/`monitorexit`
        // elided, so a resumed frame that later runs `monitorexit` would hit an
        // un-entered monitor. Two layers protect against this:
        //   1. A frame *holding* a monitor already bailed above (`rframe.threads.monitors`).
        //   2. `ACC_SYNCHRONIZED` methods bail here (the method monitor is elided
        //      under scalar replacement of `this`/the receiver).
        // The residual case — a `synchronized(obj)` *block* over a scalar-replaced
        // object in a non-synchronized method — is NOT detectable from the
        // reconstructed frame alone (an elided monitor leaves no trace). The IR
        // guard-surviving-SR producer (`ir_lower::frame_value_for_object`, gated by
        // `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`) now DOES write
        // `VirtualObject` deopt slots, so `has_virtual` can be true under that gate.
        // The two layers above (a monitor-holding frame and an `ACC_SYNCHRONIZED`
        // method both bail) cover the elision cases the IR path can produce today:
        // the IR escape analysis does not emit `synchronized`-block lock elision on
        // this path, so the residual block-elision case does not arise. A future
        // emitter that elides a `synchronized(obj)` block over a scalar-replaced
        // object MUST carry an "elided monitor present" flag on the deopt point for
        // this sink to bail on. This whole path is `CRATONVM_DEOPT_REAL`-gated
        // (default-off).
        if cached.is_synchronized {
            note_deopt_frame_bail(DeoptFrameBail::SynchronizedWithVirtuals, rframe);
            return None;
        }
        let mut copy = rframe.clone();
        if crate::runtime::deopt_materialize::materialize_virtual_objects(
            shared, thread, &mut copy, /* stress_gc */ false, /* keep_pins */ true,
        )
        .is_err()
        {
            note_deopt_frame_bail(DeoptFrameBail::VirtualMaterialise, rframe);
            return None;
        }
        materialized_frame = copy;
        &materialized_frame
    } else {
        rframe
    };

    // deopt-osr P2 — map via the cat-2-aware mappers (both route through
    // `fv_to_value`, so Int/Long/Float/Double/Object/Undefined all map): LOCALS
    // collapse the JVM-two-slot snapshot (a `long`/`double` reserves its upper
    // half, skipped) into the compact arg list `Frame::new_pooled` re-expands; the
    // operand STACK is already compact (one entry per value). Any unresolvable
    // slot (`Unsupported`/virtual/unresolved machine form) returns `None` → safe
    // re-run.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPTSLOT").is_some()
        && (ir_deopt_locals(&rframe.locals).is_none()
            || ir_deopt_frame_values(&rframe.stack).is_none())
    {
        eprintln!(
            "[DBG_DEOPTSLOT] {} bci={} locals={:?} stack={:?}",
            rframe.method_key, rframe.bci, rframe.locals, rframe.stack
        );
    }
    let locals = match ir_deopt_locals(&rframe.locals) {
        Some(l) => l,
        None => {
            note_deopt_frame_bail(DeoptFrameBail::UnmappableLocal, rframe);
            return None;
        }
    };
    let stack_vals = match ir_deopt_frame_values(&rframe.stack) {
        Some(v) => v,
        None => {
            note_deopt_frame_bail(DeoptFrameBail::UnmappableStack, rframe);
            return None;
        }
    };

    // ROOT the reconstructed oops BEFORE the GC-capable refill (locals then
    // stack — the order the re-read below relies on).
    let pin_base = thread.native_pin_roots.len();
    for v in locals.iter().chain(stack_vals.iter()) {
        if let Value::Object(Some(obj)) = v {
            thread.native_pin_roots.push(*obj);
        }
    }
    // Phase C: pin the held-monitor objects too (materialized shells), so the
    // refill GC forwards them in place and the relock below uses live addresses.
    // `monitor_objs` keeps (pin-relative position implied by push order, depth);
    // a non-Object monitor (an unresolved virtual ref — shouldn't occur, the
    // materializer rewrote them) bails to safe re-run.
    let mut monitor_depths: Vec<u32> = Vec::with_capacity(rframe.monitors.len());
    for m in &rframe.monitors {
        match &m.object {
            FrameValue::Object(addr) => {
                // SAFETY: materialization produced this raw oop and the frame is
                // pinned before any allocation or GC can make the address stale.
                let obj = unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) };
                thread.native_pin_roots.push(obj);
                monitor_depths.push(m.lock_depth);
            }
            // Null monitor or an unresolved form — refuse rather than relock a
            // bogus object (would corrupt the monitor table). Release nothing
            // extra; the caller truncates `native_pin_roots` to its watermark.
            _ => {
                note_deopt_frame_bail(DeoptFrameBail::BadMonitor, rframe);
                return None;
            }
        }
    }

    // Stress hook: the ONLY sanctioned GC injection point — before refill, while
    // every reconstructed oop is pinned (a GC after the re-read would stale the
    // unrooted `*_fwd` vecs / the un-pushed frame; there is none, by construction).
    if stress {
        maybe_gc_forced_pub_at(shared, thread, "deopt-resume");
    }

    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        cached.max_locals as usize,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        (cached.max_stack as usize).max(16) + 8,
    );

    // Re-read each oop from its (possibly forwarded) pin slot, in push order, so
    // the frame carries the current address, never the stale `Object(u64)`.
    let mut k = pin_base;
    let mut locals_fwd = Vec::with_capacity(locals.len());
    for v in &locals {
        match v {
            Value::Object(Some(_)) => {
                let fwd = thread.native_pin_roots[k];
                k += 1;
                locals_fwd.push(Value::Object(Some(fwd)));
            }
            other => locals_fwd.push(*other),
        }
    }
    let mut stack_fwd = Vec::with_capacity(stack_vals.len());
    for v in &stack_vals {
        match v {
            Value::Object(Some(_)) => {
                let fwd = thread.native_pin_roots[k];
                k += 1;
                stack_fwd.push(Value::Object(Some(fwd)));
            }
            other => stack_fwd.push(*other),
        }
    }
    // Phase C: re-read the forwarded monitor objects (pushed after locals+stack),
    // pairing each with its lock depth for the relock below.
    let mut monitors_fwd: Vec<(ObjectRef, u32)> = Vec::with_capacity(monitor_depths.len());
    for &depth in &monitor_depths {
        let fwd = thread.native_pin_roots[k];
        k += 1;
        monitors_fwd.push((fwd, depth));
    }

    // Build the Frame as a LOCAL (never pushed onto thread.frames), so the
    // subsequent re-run sees identical interpreter state after it is discarded.
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
        &locals_fwd,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    for v in &stack_fwd {
        // A clean pilot never overflows the padded stack. If it somehow did,
        // recycle the frame's pooled buffers before bailing — `Frame` has no
        // `Drop` impl (recycling is explicit), so a plain `?`-return would leak
        // them. The caller then releases the pins and re-runs.
        if frame.stack.push(*v).is_err() {
            frame.recycle(&mut thread.locals_pool, &mut thread.stacks_pool);
            note_deopt_frame_bail(DeoptFrameBail::StackPush, rframe);
            return None;
        }
    }
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    frame.pc = rframe.bci as usize;
    // Phase C: re-acquire each elided monitor on the re-materialized (thread-local,
    // uncontended) object, `lock_depth` times, so the resumed frame's eventual
    // `monitorexit` (and any nested exits) balance via the MonitorTable. Done after
    // every fallible step so a bail never leaves a stray lock. `monitors.enter` is
    // the recursive uncontended enter (the object is fresh thread-local — no
    // contention, no GC).
    for &(obj, depth) in &monitors_fwd {
        for _ in 0..depth {
            shared.threads.monitors.enter(obj, thread.thread_id);
        }
    }
    Some(frame)
}

/// May a deopt sink resume this trapped body from its reconstructed frame
/// WITHOUT the backend's `can_deopt_resume` vouching for it?
///
/// # One predicate, asked at every sink
///
/// Four sinks consume a stashed IR deopt frame, and which one a trap reaches
/// depends only on how the callee was entered. Until 2026-09-07 they gave three
/// different answers to the same event:
///
/// * `try_resume_trapped_callee` (`jit/helpers.rs`) resumed it precisely;
/// * `execute`'s tier-up sink (`interpreter.rs`) raised a hard `InternalError`
///   — fixed 2026-09-07;
/// * `execute_jit_call` (`jit-callsite-a`), `execute_jit_call_decoded`
///   (`jit-callsite-b`) and [`resume_deopted_body`] re-ran the whole method from
///   entry, with no side-effect check — so a body that had already committed a
///   store committed it a second time, silently.
///
/// A deopt sentinel does NOT mean "nothing happened": the compiled body ran up
/// to `bci` and stopped. Re-entering at bci 0 re-executes everything before it.
/// [`resume_deopted_body`]'s own doc says exactly that, and names what it cost —
/// the hibernate-reactive `reactiveRemove`-fires-twice defect, one
/// `ArrayLoop.next()` dispatch and two deletes. The fix for THAT added the
/// resume call this predicate now makes reachable: it was gated on
/// `can_deopt_resume`, which an optimizing-tier artifact never has, so it could
/// not fire on the tier the defect actually needs.
///
/// # The three refusals
///
/// They are what the reconstructed frame genuinely cannot describe, and they
/// are the same three `try_resume_trapped_callee` makes or the emission side
/// names:
///
/// * an **`ACC_SYNCHRONIZED`** method — the method monitor is not in the frame;
/// * a body that **takes a monitor at all**. Every `FrameState` `ir_lower`
///   builds hard-codes `monitors: Vec::new()`, so a resumed frame for such a
///   body believes it holds no lock. That is also what covers the one thing
///   `can_deopt_resume` really protected: an elided monitor FORCES the flag
///   false, so such a body reaches this predicate — and eliding is a codegen
///   decision, not a bytecode rewrite, so the ops are still there to see;
/// * a **resume bci past the method's code**.
///
/// # Additive by construction
///
/// Every caller ORs this beside its existing `can_deopt_resume` arm rather than
/// replacing it. A backend that SET that flag has already vouched no monitor
/// was elided, so applying these guards there too would refuse a single-pass
/// body with an ordinary `synchronized` block that resumes correctly today.
pub(crate) fn sink_precise_resume_allowed(
    code: &[u8],
    code_len: usize,
    is_synchronized: bool,
    bci: u32,
) -> bool {
    cratonvm_jit::deopt_sink_resume_enabled()
        && !is_synchronized
        && !cratonvm_jit::bytecode_holds_monitor(code, code_len)
        && (bci as usize) < code_len
}

/// [`sink_precise_resume_allowed`] for a sink holding a `CachedBytecodeMethod`
/// rather than a raw `Code` attribute.
///
/// `cached.code` is `padded_bytecode`, i.e. the real body followed by zero
/// bytes. That is safe for both readers: `0x00` is `nop`, so the monitor walk
/// runs off the end finding nothing, and the bci bound is the one
/// `try_resume_trapped_callee` already applies against the same padded length.
pub(crate) fn sink_precise_resume_allowed_for(
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    sink_precise_resume_allowed(
        &cached.code,
        cached.code.len(),
        cached.is_synchronized,
        rframe.bci,
    )
}

/// real-frame-deopt Step 4 — RESUME a real (Object-bearing) deopt at the trapping
/// bci instead of re-running the whole method from entry (the payoff that kills
/// the side-effect double-execution).
///
/// Builds the interpreter `Frame` from the reconstructed locals/stack (oops
/// GC-rooted across the pool refill — see [`build_deopt_frame_inner`]), then
/// PUSHES it onto `thread.frames` so the interpreter resumes there.
///
/// The GC-rooting HANDOFF is the load-bearing invariant: the temporary
/// `native_pin_roots` pins are held ACROSS `push_frame_and_fire_entry` (which may
/// fire entry hooks that allocate / GC) and released only AFTER the push — once
/// pushed, the frame's locals/stack are themselves GC roots (scanned by the
/// normal interpreter-frame root walk), so the reconstructed oops are rooted by
/// the pins, then by BOTH pins and frame, then by the frame alone, with no
/// unrooted window. Returns `Some(FramePushed)` on a clean resume, or `None`
/// (re-run) for an out-of-scope / unmappable frame — releasing any partial pins
/// in both cases.
///
/// Gated by `CRATONVM_DEOPT_REAL` at the sink; the pre-existing
/// `CRATONVM_IR_DEOPT_RESUME` int-only path (`resume_from_ir_deopt`) runs first
/// and is left intact.
pub(super) fn resume_real_ir_deopt(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<CachedCallResult> {
    let pin_base = thread.native_pin_roots.len();
    match build_deopt_frame_inner(shared, thread, cached, rframe, false) {
        Some(frame) => {
            // Push FIRST, pins STILL installed: during the push the oops are
            // rooted by the pins, and once pushed also by the frame. Only THEN
            // release the pins — the frame roots them from here on.
            push_frame_and_fire_entry(shared.vm_identity, thread, frame);
            thread.native_pin_roots.truncate(pin_base);
            Some(CachedCallResult::FramePushed)
        }
        None => {
            // Out-of-scope / unmappable: release any partial pins, fall back to
            // the whole-method re-run.
            thread.native_pin_roots.truncate(pin_base);
            None
        }
    }
}

/// deopt-osr Step 8 follow-up (P4) — TRUE OSR-exit: transfer the JIT-advanced loop
/// state from a reconstructed frame into the LIVE interpreter frame at `frame_idx`
/// (overwrite locals + operand stack in place, set pc), so the interpreter resumes
/// the loop body exactly where the OSR-compiled code bailed.
///
/// OSR is *same-frame* replacement — the OSR'd code ran within `frame_idx`'s
/// logical frame — so unlike `resume_real_ir_deopt` (which pushes a NEW frame for a
/// normal-call deopt) this MUTATES the existing frame, preserving its identity and
/// bookkeeping (`backward_count`, `osr_attempt_counts`, `monitor_on_exit`, `seq`,
/// the cold metadata) that a wholesale rebuild-and-swap would drop. That matters:
/// the safe-reject alternative discards the JIT's advanced loop state and lets the
/// interpreter re-run the iterations the OSR'd code already executed, which
/// double-executes any side effect it committed (the gap this closes).
///
/// Returns `Some(())` on a clean transfer (the caller then returns `None` from
/// `try_osr`, so the interpreter resumes THIS mutated frame), or `None` for an
/// out-of-scope / unmappable frame.
///
/// **A `None` here is NOT a safe reject.** By the time this runs the OSR'd body
/// has committed iterations, so "continue interpreting THIS frame from where it
/// was" re-executes every one of them. `artifact` + `plan` are the validated
/// entry (`CompiledMethod::validate_osr_entry`, spent by `osr_enter_planned`);
/// its `osr_exit_policy` walked every deopt point of the artifact at ADMISSION
/// and refused the entry outright (`osr-entry-unresumable-exit`) when any of
/// them could reconstruct a frame this transfer would reject. That is what makes
/// the refusal path unreachable after a committed body rather than merely rare —
/// discovering it here would be useless, because the only remaining options are
/// to replay the committed iterations or to lose them. See
/// `docs/jit/on-stack-replacement.md` §4.
///
/// `plan.resume_after_exit` — not `rframe.bci` — names the pc the live frame is
/// parked at; see the call site below for what it re-checks.
///
/// GC-safety. The reconstructed oops were read in-stub (`x64_deopt_entry`) as raw
/// heap words. The OSR-exit snapshot's provenance is Register / StackSlot /
/// StackSlotRef only — never `VirtualObject` — so there is NO materialization, and
/// an in-place overwrite reuses the frame's existing pooled buffers, so there is NO
/// pool refill. Hence NO Java-heap allocation runs between the in-stub capture and
/// the writes below: the addresses stay current and become rooted by the frame's
/// own GC-scanned slots the instant they are written. A `VirtualObject`-bearing
/// frame (unreachable from the OSR-exit emitter today) bails to reject rather than
/// allocate without the pin dance. The per-slot oop typing comes from the same
/// `local_oop_masks` dataflow that the already-validated deopt-EXIT resume relies
/// on; under `CRATONVM_DEOPT_VERIFY` the structural invariants are checked first.
pub(super) fn transfer_osr_exit_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    plan: &cratonvm_jit::OsrEntryPlan,
) -> Option<()> {
    match transfer_osr_exit_into_live_frame_checked(
        shared, thread, frame_idx, rframe, artifact, plan,
    ) {
        Ok(()) => Some(()),
        Err(why) => {
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
                eprintln!(
                    "[cratonvm-deopt] OSR-exit transfer reject ({why}) at bci={}",
                    rframe.bci
                );
            }
            None
        }
    }
}

/// The body of [`transfer_osr_exit_into_live_frame`], returning the refusal
/// *reason* instead of a bare `None`.
///
/// Split out so every refusal has a name a test can assert on. The bare
/// `Option` the caller sees discards the reason (and traces it under
/// `CRATONVM_DBG_DEOPT`), which is exactly how a `MaterializationRequired`
/// slot used to be reported as the generic "unmappable local" — see the guard
/// below.
///
/// **Fail-closed contract.** Every check that can refuse runs BEFORE the first
/// write to the live frame, so a refusal can never half-write it. That includes
/// the resume-bci decision: [`cratonvm_jit::OsrEntryPlan::resume_after_exit`]
/// is consulted before the locals/stack are overwritten, not after.
pub(super) fn transfer_osr_exit_into_live_frame_checked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    plan: &cratonvm_jit::OsrEntryPlan,
) -> Result<(), String> {
    use cratonvm_jit::deopt::FrameValue;
    let trace = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some();

    // Phase-A scope (mirror `build_deopt_frame_inner`): a single non-inlined frame,
    // no held monitors, no virtual (scalar-replaced) slots. The OSR-exit snapshot
    // never emits virtuals; if a future emitter does, reject until the elided-
    // monitor handling that gates `build_deopt_frame_inner` is wired here too.
    //
    // A caller chain is no longer refused here — it is transferred by the
    // multi-frame sibling below. The OUTERMOST scope is the OSR'd method, whose
    // interpreter frame is the live one this function writes in place; every
    // scope beneath it is an inlined callee that has to be PUSHED. Step 2 of the
    // five-step chain in
    // `docs/known-issues/netty/httpresponsestatustest-exhaustive-loop-timeout-20260816.md`.
    //
    // A refusal there still returns `Err`, so the caller's behaviour for any
    // chain the transfer cannot honour is exactly what it was.
    if !rframe.caller_frames.is_empty() {
        return transfer_osr_exit_chain_into_live_frame(
            shared, thread, frame_idx, rframe, artifact,
        );
    }
    if !rframe.monitors.is_empty() {
        return Err("held monitors".to_string());
    }
    if rframe.locals.iter().chain(rframe.stack.iter()).any(|v| {
        matches!(
            v,
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
        )
    }) {
        return Err("virtual-object slot".to_string());
    }
    // `MaterializationRequired` is NOT `Unsupported`, and the difference is the
    // whole reason `jit/src/deopt.rs` split the variant out: `Unsupported` means
    // "the coarse whole-method classifier cannot name this slot's kind" (tolerated
    // below — the live frame's current value is provably safe to leave in place),
    // whereas `MaterializationRequired` means an optimization DELETED a value that
    // *was* live and left no rebuild recipe. The live frame's stale pre-entry word
    // is then genuinely WRONG, not merely unread, and reconstructing it as a silent
    // null/zero is precisely what the variant exists to make impossible.
    //
    // Without this arm the refusal still happened — the variant falls through
    // `fv_to_value`'s catch-all `None` — but it was reported as "unmappable local",
    // naming the wrong cause, and only for LOCALS (a stack slot went to
    // "unmappable stack slot"). Naming it here also makes it a *scope* verdict,
    // decided before any mapping, alongside the other Phase-A refusals.
    //
    // Belt and braces: `deopt::frame_state_is_resumable` makes the same call on the
    // jit side, so `validate_osr_entry` refuses such an artifact at ENTRY
    // (`osr-entry-unresumable-exit`) and this transfer should never see one.
    if let Some(what) = rframe
        .locals
        .iter()
        .enumerate()
        .map(|(i, v)| ("local", i, v))
        .chain(
            rframe
                .stack
                .iter()
                .enumerate()
                .map(|(i, v)| ("stack", i, v)),
        )
        .find_map(|(region, i, v)| match v {
            FrameValue::MaterializationRequired(ev) => {
                Some(format!("materialization required ({region} {i}: {ev})"))
            }
            _ => None,
        })
    {
        return Err(what);
    }

    // CRATONVM_DEOPT_VERIFY: structural + oop-plausibility checks before mutating
    // the frame — mirrors `build_deopt_frame_inner`. Structural catches a shifted /
    // malformed snapshot (slot counts past the method maxima, bad virtual
    // descriptors); oop-plausibility (`verify_reconstructed_oops`) catches a
    // garbage address in an `Object` slot before it is written into the live frame
    // (which would otherwise hand the GC a dangling root). Default-off ⇒ skipped.
    if cratonvm_jit::deopt_verify_enabled() {
        let (max_locals, max_stack) = {
            let frame = &thread.frames[frame_idx];
            (frame.max_locals, frame.max_stack)
        };
        let verdict = verify_reconstructed_frame(rframe, max_locals, max_stack)
            .and_then(|()| verify_reconstructed_oops(rframe, shared));
        if let Err(why) = verdict {
            eprintln!(
                "[DEOPT-VERIFY] OSR-exit bci={}: reconstructed-frame invariant violated: {why} \
                 — forcing safe reject",
                rframe.bci
            );
            return Err(format!("deopt-verify: {why}"));
        }
    }

    // The reconstructed slots must fit the live frame's storage. (`set_local_unchecked`
    // / `push_unchecked` panic out-of-bounds, so this guard is load-bearing.)
    {
        let frame = &thread.frames[frame_idx];
        if rframe.locals.len() > frame.locals_len() || rframe.stack.len() > frame.max_stack as usize
        {
            return Err("slot overflow".to_string());
        }
    }

    // Map reconstructed FrameValues → interpreter Values (Int / Long / Float /
    // Double / Object / Undefined via `fv_to_value`; virtual / unresolved →
    // None ⇒ reject). Pure Rust; no Java allocation. Done BEFORE any frame
    // mutation so a reject can never half-write the frame.
    //
    // `ir_deopt_frame_values` is the 1:1 (NON-collapsing) mapper, which is exactly
    // what the in-place transfer needs: the locals snapshot is JVM-slot-indexed
    // (one entry per slot), and we write it slot-for-slot below. A cat-2
    // `long`/`double` therefore arrives as two entries — `Long`/`Double` at slot N
    // plus the reserved upper-half `Undefined` (→ `Int(0)`) at N+1 — which is the
    // correct two-slot JVM layout (`lload N` reads the full value from slot N; the
    // dead N+1 is never read). The deopt-EXIT path, which builds a fresh frame via
    // `Frame::new_pooled`, uses the COLLAPSING `ir_deopt_locals` instead because
    // `copy_args_to_locals` re-expands a compact list; here there is no
    // re-expansion, so collapsing would mis-align the direct slot writes.
    //
    // FIX (jit-osr-loop-duplicate-execution, silent data corruption): a LOCAL
    // slot's `FrameValue::Unsupported` must NOT reject the whole transfer the
    // way an unmappable STACK slot does. `classify_local_kinds` (jit/src/x64.rs)
    // is a coarse WHOLE-METHOD scan: a slot accessed as more than one JVM kind
    // ANYWHERE in the method (e.g. an `int` loop counter whose slot is later
    // reused, after the loop's scope ends, for an unrelated `long`) is always
    // `Ambiguous` → `Unsupported`, at EVERY bci in that method, even ones where
    // the reused slot provably cannot be read yet. The bytecode we're resuming
    // already passed verification, which requires a fresh `store` before any
    // `load` of a given logical local — so at the resume bci, an `Unsupported`
    // slot is either genuinely dead (its old value is never read before being
    // overwritten) or belongs to a not-yet-live disjoint reuse of the slot;
    // either way its CURRENT value in the live frame is safe to leave in
    // place. Previously this fell through to `bail("unmappable local")` on
    // every method with any such slot, which discarded the whole transfer —
    // even after the OSR'd loop had already run to completion with real,
    // committed side effects (e.g. `ArrayList.add`) — and let the interpreter
    // resume from the STALE pre-OSR pc/locals, silently re-executing (and
    // re-committing) every iteration since OSR entry. See
    // jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md.
    let mut locals: Vec<Option<Value>> = Vec::with_capacity(rframe.locals.len());
    for (i, v) in rframe.locals.iter().enumerate() {
        if matches!(v, cratonvm_jit::deopt::FrameValue::Unsupported) {
            locals.push(None);
        } else {
            match fv_to_value(v) {
                Some(val) => locals.push(Some(val)),
                // Reachable only for a variant `fv_to_value` cannot type. The
                // `MaterializationRequired` case — historically the confusing
                // occupant of this arm — is named by its own guard above, so this
                // label no longer stands in for it.
                None => return Err(format!("unmappable local ({i}: {v:?})")),
            }
        }
    }
    let stack_vals = match ir_deopt_frame_values(&rframe.stack) {
        Some(s) => s,
        None => return Err("unmappable stack slot".to_string()),
    };

    // The exact bci to park the live frame at. This is the ONLY sanctioned resume
    // point once compiled code has run: `resume_after_exit` re-checks that
    // `rframe.bci` is a deopt point THIS artifact recorded and that its
    // `ResumeSemantics` is `REEXECUTE` (i.e. the bytecode there has not taken
    // effect), and returns the reconstructed frame's own bci — never the OSR entry
    // bci. The bare `frame.pc = rframe.bci` it replaces trusted a bci that could be
    // a mis-routed stash, or a `RESUME`/`RETHROW` point that must not be re-executed
    // (the same double-execution defect this whole path exists to prevent, one
    // bytecode instead of one loop iteration).
    //
    // Decided BEFORE the writes below so a refusal leaves the frame untouched. A
    // refusal here means the OSR'd body committed work the interpreter cannot be
    // resumed after — the caller must NOT safe-reject; see the exit sink in
    // `try_osr`. `validate_osr_entry`'s `osr_exit_policy` walks every deopt point
    // of the artifact at admission and refuses the entry outright
    // (`osr-entry-unresumable-exit`) when one of them could land here, which is
    // what makes this branch unreachable rather than merely rare.
    let resume_bci = plan
        .resume_after_exit(artifact, rframe)
        .map_err(|b| format!("unresumable exit: {b}"))?;

    // Overwrite the live frame IN PLACE. No Java allocation here, so the
    // reconstructed oops remain valid and are rooted by the frame's slots the moment
    // they are written. The locals snapshot is JVM-slot-indexed (one entry per slot,
    // cat-2 as its two-slot pair), so slot `i` ← `locals[i]` is 1:1. A `None` entry
    // (an `Unsupported` source slot, see above) leaves that slot's existing live
    // value untouched instead of writing anything.
    let frame = &mut thread.frames[frame_idx];
    for (i, v) in locals.iter().enumerate() {
        if let Some(val) = v {
            frame.set_local_unchecked(i, *val);
        }
    }
    frame.stack.clear();
    for v in &stack_vals {
        frame.stack.push_unchecked(*v);
    }
    // The plan-validated resume point (see above), not the raw `rframe.bci`.
    frame.pc = resume_bci;

    // osr-02 frame comparator: record the RESUMED frame, after the write.
    //
    // After, not before, and not from `rframe`: what the brief asks about is
    // the state the interpreter resumes on, and that is not always what the
    // reconstruction proposed. An `Unsupported` source slot is deliberately
    // left at the live frame's current value (see the mapping loop above), so a
    // record built from `rframe` would describe a frame that never exists.
    if super::osr_frame_trace::enabled() {
        super::osr_frame_trace::record_exit(frame, resume_bci);
    }

    if trace {
        eprintln!(
            "[cratonvm-deopt] OSR-exit TRANSFER into live frame: resume bci={resume_bci} \
             ({} locals, {} stack, entry_pc={})",
            locals.len(),
            stack_vals.len(),
            plan.entry_pc,
        );
    }
    Ok(())
}

/// The RBC.6b lift's exception exit: transfer a **reason-9**
/// (`DeoptReason::PendingException`) frame published by an OSR'd body into the
/// live interpreter frame and park it at `handler_pc`, with `exc` on the
/// operand stack.
///
/// The sibling of [`transfer_osr_exit_into_live_frame_checked`], and it exists
/// for the same reason that one does: once an OSR'd body has committed loop
/// iterations, "resume interpretation where the frame was parked" is not a safe
/// fallback but a silent re-execution of everything since OSR entry (RBC.7).
/// Until 2026-08-17 that could not arise, because `compile_osr_artifact`
/// refused every method with an exception table outright; now that it admits
/// the ones whose protected-range throwing sites all publish a precise frame,
/// this is the path those exceptions take.
///
/// Three things differ from the resume transfer, and each is deliberate:
///
///  * **The resume point is a handler, not a re-execute bci.** A reason-9
///    point's [`cratonvm_jit::deopt::ResumeSemantics`] is `RETHROW`, so
///    `OsrEntryPlan::resume_after_exit` correctly refuses it — its bci names a
///    THROWING instruction, not somewhere the interpreter may be parked. The
///    caller has already run that bci through this frame's own exception table
///    (`find_exception_handler_any_pc`) and passes the handler it found.
///
///  * **The operand stack is not restored.** JVMS §2.10: entering a handler
///    empties the operand stack and pushes the throwable. Whatever the snapshot
///    recorded there is discarded by definition, so an unmappable STACK slot —
///    fatal to the resume transfer, which has to rebuild the stack exactly —
///    cannot make this transfer wrong. Locals are the whole payload.
///
///  * **The bci is checked against a `PendingException` point.** The resume
///    transfer spends `resume_after_exit` on that check; here it is made
///    directly. A frame whose bci this artifact recorded no reason-9 point for
///    is a mis-routed stash, and routing a mis-routed stash into a handler
///    would enter it with another site's locals.
///
/// Fail-closed exactly as the sibling is: every check that can refuse runs
/// BEFORE the first write to the live frame, so a refusal can never half-write
/// it. `Err` means the caller must NOT resume this frame — see the exception
/// drain in `try_osr`, which propagates instead.
pub(super) fn transfer_osr_exception_exit_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    handler_pc: usize,
    exc: ObjectRef,
) -> Result<(), String> {
    use cratonvm_jit::deopt::FrameValue;

    // ── Phase-A scope, same three as the sibling ──────────────────────────
    if !rframe.caller_frames.is_empty() {
        return Err("inlined caller chain".to_string());
    }
    if !rframe.monitors.is_empty() {
        return Err("held monitors".to_string());
    }
    if rframe.locals.iter().any(|v| {
        matches!(
            v,
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
        )
    }) {
        return Err("virtual-object local".to_string());
    }
    // `MaterializationRequired` means an optimization DELETED a value that WAS
    // live and left no rebuild recipe, so the live frame's stale pre-OSR word is
    // genuinely wrong rather than merely unread. Named here, as its own refusal,
    // for the reason the sibling names it: falling through to the generic
    // "unmappable local" reports the wrong cause. Only LOCALS are inspected —
    // the stack is discarded at handler entry (see the doc comment).
    if let Some(what) = rframe.locals.iter().enumerate().find_map(|(i, v)| match v {
        FrameValue::MaterializationRequired(ev) => {
            Some(format!("materialization required (local {i}: {ev})"))
        }
        _ => None,
    }) {
        return Err(what);
    }

    // ── The stash really is a reason-9 point of THIS artifact ─────────────
    //
    // The caller has already checked the frame's baked `method_key` names this
    // method. That is identity; this is provenance: an ordinary guard deopt
    // (reason 0-8) reaching here would mean the bci names a re-execute point
    // and the exception came from somewhere else entirely.
    if !artifact.deopt_points.iter().any(|dp| {
        dp.bci == rframe.bci && dp.reason == cratonvm_jit::deopt::DeoptReason::PendingException
    }) {
        return Err(format!(
            "exit bci {} is not a PendingException point of this artifact",
            rframe.bci
        ));
    }

    // ── Oop plausibility, before a garbage address becomes a GC root ──────
    //
    // Default-off (`CRATONVM_DEOPT_VERIFY`). The structural sibling check is
    // deliberately not run: it validates the operand stack against `max_stack`,
    // and this transfer does not write the operand stack.
    if cratonvm_jit::deopt_verify_enabled() {
        if let Err(why) = verify_reconstructed_oops(rframe, shared) {
            eprintln!(
                "[DEOPT-VERIFY] OSR exception-exit bci={}: {why} — refusing the transfer",
                rframe.bci
            );
            return Err(format!("deopt-verify: {why}"));
        }
    }

    // ── The reconstructed slots must fit the live frame ───────────────────
    if rframe.locals.len() > thread.frames[frame_idx].locals_len() {
        return Err("local slot overflow".to_string());
    }

    // ── Map, then write ───────────────────────────────────────────────────
    //
    // `Unsupported` in a LOCAL leaves the live frame's current value alone, for
    // exactly the argument the sibling makes: `classify_local_kinds` is a coarse
    // whole-method scan that marks a slot ambiguous at EVERY bci if it is
    // accessed as two kinds anywhere, and the already-verified bytecode
    // guarantees such a slot is dead or re-stored before it is read.
    let mut locals: Vec<Option<Value>> = Vec::with_capacity(rframe.locals.len());
    for (i, v) in rframe.locals.iter().enumerate() {
        if matches!(v, FrameValue::Unsupported) {
            locals.push(None);
        } else {
            match fv_to_value(v) {
                Some(val) => locals.push(Some(val)),
                None => return Err(format!("unmappable local ({i}: {v:?})")),
            }
        }
    }

    let frame = &mut thread.frames[frame_idx];
    for (i, v) in locals.iter().enumerate() {
        if let Some(val) = v {
            frame.set_local_unchecked(i, *val);
        }
    }
    // JVMS §2.10 handler entry: empty the operand stack, push the throwable.
    frame.stack.clear();
    frame.stack.push_unchecked(Value::Object(Some(exc)));
    frame.pc = handler_pc;

    if super::osr_frame_trace::enabled() {
        super::osr_frame_trace::record_exit(frame, handler_pc);
    }
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
        eprintln!(
            "[cratonvm-deopt] OSR exception-exit TRANSFER: throw bci={} -> handler pc={} \
             ({} locals)",
            rframe.bci,
            handler_pc,
            locals.len(),
        );
    }
    Ok(())
}

/// The multi-frame half of the OSR-exit transfer.
///
/// **Step 2 of the inliner chain.** The single-frame transfer above writes the
/// live interpreter frame — the one OSR replaced — and returns. With an inlined
/// chain there is more than one frame to restore, and they are not alike:
///
/// * the **outermost** scope IS the OSR'd method, so its frame already exists
///   and is the live one. It is written IN PLACE, and parked at the SUCCESSOR of
///   its invoke, because that invoke is in progress (`RESUME` semantics) — the
///   same rule [`caller_resume_pc`] enforces for the deopt-exit sink.
/// * every scope **beneath** it is an inlined callee with no frame at all. Those
///   are pushed, outermost-first, so the innermost (trapping) one ends up on
///   top and the interpreter resumes in it.
///
/// The innermost scope is parked at its own bci: it is the trapping point, and
/// its bytecode has not taken effect.
///
/// **Fail-closed, and the ordering is the whole safety argument.** Everything
/// resolvable is resolved and checked BEFORE the live frame is touched, because
/// once it has been overwritten there is no way back to the caller's safe
/// reject — and unlike the single-frame case, a partial success here would leave
/// a live frame describing one method and a pushed frame describing another.
/// `materialise_inner_scopes` does all of the fallible work up front; the writes
/// below cannot fail.
fn transfer_osr_exit_chain_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
) -> Result<(), String> {
    use cratonvm_jit::deopt::FrameValue;

    let depth = rframe.caller_frames.len();
    if depth > MAX_INLINE_RESUME_DEPTH {
        return Err(format!(
            "inlined chain is {depth} deep, past the {MAX_INLINE_RESUME_DEPTH}-frame resume budget"
        ));
    }
    let Some(outer_scope) = rframe.caller_frames.last() else {
        return Err("no caller frames".to_string());
    };

    // ── the outermost scope must be the live frame's own method ──────────
    //
    // An artifact only ever inlines INTO its own body, so a chain whose
    // outermost scope names some other method is a mis-routed stash rather than
    // a deep one — the same identity rule the single-frame path applies to
    // `rframe` itself.
    let (
        live_class,
        live_method,
        live_desc,
        live_class_id,
        live_code,
        live_max_locals,
        live_max_stack,
    ) = {
        let f = &thread.frames[frame_idx];
        (
            f.class_name().to_string(),
            f.method_name().to_string(),
            f.method_descriptor().to_string(),
            f.class_id,
            f.code.clone(),
            f.locals_len(),
            f.max_stack as usize,
        )
    };
    if !deopt_frame_matches_method(outer_scope, &live_class, &live_method, &live_desc) {
        return Err(format!(
            "outermost caller scope {} does not name the live frame {live_class}.{live_method}{live_desc}",
            outer_scope.method_key
        ));
    }

    // ── the outermost scope's own values, and where to park it ───────────
    //
    // `caller_frame_values` rather than the tolerant mapping the single-frame
    // transfer uses: this frame is being rewritten to describe a DIFFERENT point
    // in its own execution — the invoke it is parked in — so leaving an
    // `Unsupported` local at whatever the OSR'd body happened to leave there is
    // not the "provably dead or re-stored" case that rule rests on.
    let (outer_locals, outer_stack) = caller_frame_values(outer_scope)?;
    // `frame.code` carries 2 bytes of speculative-read padding.
    let live_code_len = live_code.len().saturating_sub(2);
    let outer_resume_pc = caller_resume_pc(&live_code, live_code_len, outer_scope.bci as usize)?;
    if outer_locals.len() > live_max_locals || outer_stack.len() > live_max_stack {
        return Err(format!(
            "outermost scope {} does not fit the live frame ({} locals / {} stack against {}/{})",
            outer_scope.method_key,
            outer_locals.len(),
            outer_stack.len(),
            live_max_locals,
            live_max_stack
        ));
    }

    // ── every scope beneath it, fully resolved before anything is written ──
    let inner = materialise_inner_scopes(shared, live_class_id, rframe)?;

    // The artifact must actually have recorded the trapping bci, exactly as
    // `resume_after_exit` requires for the flat case. Without this a mis-routed
    // stash whose scopes happen to type-check would be resumed.
    if !artifact.deopt_points.iter().any(|p| p.bci == rframe.bci) {
        return Err(format!(
            "trapping bci {} is not a recorded deopt point of this artifact",
            rframe.bci
        ));
    }

    // `CRATONVM_DEOPT_VERIFY`, before a reconstructed oop becomes a GC root.
    if cratonvm_jit::deopt_verify_enabled() {
        if let Err(why) = verify_reconstructed_oops(rframe, shared) {
            eprintln!(
                "[DEOPT-VERIFY] OSR-exit chain at bci={}: {why} — refusing the transfer",
                rframe.bci
            );
            return Err(format!("deopt-verify: {why}"));
        }
    }

    // Virtual objects have no materializer on this path, in either half.
    if rframe
        .caller_frames
        .iter()
        .chain(std::iter::once(rframe))
        .any(|f| {
            f.locals.iter().chain(f.stack.iter()).any(|v| {
                matches!(
                    v,
                    FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
                )
            })
        })
    {
        return Err("virtual-object slot in an inlined chain".to_string());
    }

    // ── writes only, from here ───────────────────────────────────────────
    {
        let frame = &mut thread.frames[frame_idx];
        for (i, v) in outer_locals.iter().enumerate() {
            frame.set_local_unchecked(i, *v);
        }
        frame.stack.clear();
        for v in &outer_stack {
            frame.stack.push_unchecked(*v);
        }
        frame.pc = outer_resume_pc;
        if super::osr_frame_trace::enabled() {
            super::osr_frame_trace::record_exit(frame, outer_resume_pc);
        }
    }
    let trace = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some();
    if trace {
        eprintln!(
            "[cratonvm-deopt] OSR-exit CHAIN transfer: {live_class}.{live_method} in place @{outer_resume_pc}, \
             then {} pushed frame(s), trapping bci={}",
            inner.len(),
            rframe.bci
        );
    }
    push_inlined_chain(shared, thread, inner, trace)
        .map(|_| ())
        .ok_or_else(|| {
            "pushing the inlined chain failed after the live frame was written".to_string()
        })
}

/// deopt-osr Step 9 — stamp a freshly compiled artifact with the method's
/// current live compilation epoch (`SharedVm::method_epochs`) so the
/// real-frame-deopt resume sink can distinguish a current compilation from one
/// superseded by a later invalidation. No-op (the field stays `0` and is never
/// read) unless `CRATONVM_DEOPT_REAL` is on, so production artifacts are
/// byte-identical. Uses the same `"<class>.<method>:<descriptor>"` key as
/// `DeoptimizationController::deoptimize`, which advances the epoch.
///
/// Step 9 follow-up (a): ALSO stamp the artifact's `DeoptEpochGuard` (baked into
/// its frame-deopt stubs) with the same creation epoch and a stable pointer to
/// the live-epoch cell. (`x64_deopt_entry` used it to short-circuit before
/// dereferencing the deopt box under a `CRATONVM_JIT_FREE_CODE=1` mode that
/// freed boxes under running frames; that mode is gone and the entry now
/// ignores the guard.) No-op when no guard was emitted (`deopt_epoch_guard`
/// null) — i.e. on every production artifact.
#[inline]
pub(super) fn stamp_compilation_epoch(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    cm: &mut crate::jit::CompiledMethod,
) {
    if cratonvm_jit::deopt_real_enabled() {
        let key = format!("{class_name}.{method_name}:{descriptor}");
        // Take a stable pointer to the live-epoch cell first (creating it at
        // epoch 0 if absent), then read its value as the creation epoch, so the
        // field and the baked guard agree. A concurrent `bump` racing here only
        // makes the guard consider itself superseded → safe whole-method re-run.
        let cell = shared.live_epoch_cell_ptr(&key);
        // SAFETY: `cell` is a stable, retained `AtomicU64` from `method_epochs`.
        let epoch = unsafe { (*cell).load(std::sync::atomic::Ordering::Relaxed) };
        cm.compilation_epoch = epoch;
        cm.stamp_deopt_epoch_guard(epoch, cell);
    }
}


/// jit-invokedynamic-groovy-regression fix — does the stashed reconstructed
/// frame belong to `cached`? The producer bakes the compiling method's
/// `"<class>.<method>:<descriptor>"` key into every snapshot
/// (`build_and_record_deopt_point`); a frame stashed by a NESTED compiled
/// callee (whose sentinel bubbled up through its compiled callers' epilogue
/// bails) carries THAT callee's key and must never be resumed as if it were
/// this method's. An empty key (legacy/test producer, or the superseded-guard
/// `bci == u32::MAX` sentinel frame) never matches.

/// CRATONVM_DBG_DEOPT — record which `take_last_deopt()` sink consumed a
/// stashed frame, and what method that sink was running.
///
/// The stash is a single thread-local slot with four consumers. A frame taken
/// by the wrong one de-speculates an innocent method and cannot resume, so the
/// consuming site is the first thing any deopt investigation needs and was the
/// one thing not recorded.
pub(crate) fn dbg_deopt_sink(
    site: &str,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    running: &str,
) {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_none() {
        return;
    }
    eprintln!(
        "[cratonvm-deopt] sink={site} running={running} stash={} bci={}",
        rframe.method_key, rframe.bci,
    );
}

/// May the interpreter re-run this method from entry instead of resuming
/// precisely at `resume_bci`?
///
/// `true` means the abandoned compiled attempt cannot have committed anything
/// observable, so a re-run from bci 0 is the same execution — the locals are
/// rebuilt from the same arguments and nothing outside the frame was written.
///
/// # Why a PREFIX, and not the whole body
///
/// The rule used to be "the whole method commits no side effect", and that is
/// the right rule for a method the compiler ran end to end. It is the wrong
/// rule for an abandoned attempt: what a replay could DUPLICATE is only what
/// the attempt already committed, and the attempt stopped at `resume_bci`.
/// Every deopt point this VM emits carries `ResumeSemantics::REEXECUTE` (only
/// `PendingException` differs, and those frames go to a different stash), so
/// the bytecode AT `resume_bci` had not completed and the bytecodes after it
/// never ran. Only `code[..resume_bci]` can have committed anything.
///
/// The narrower rule cost a real answer: netty's
/// `UnpooledHeapByteBuf._getUnsignedMedium` is `getfield array; invokestatic
/// getUnsignedMedium`, and once `CRATONVM_JIT_IR_INLINE` splices that callee
/// in, an out-of-bounds read deopts at the invoke. The whole body "commits a
/// side effect" — it contains a call — so the replay was refused and a
/// three-byte bounds check raised `InternalError` instead of
/// `IndexOutOfBoundsException`. Nothing before the invoke commits anything, and
/// the call itself never completed, so the replay was always safe. See
/// `ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828.md`.
///
/// # The second half
///
/// A spliced artifact's attempt also ran part of a RELOCATED callee body, which
/// the caller's bytecode does not describe. `spliced_bodies_pure` is the
/// artifact's own answer for that half (`CompiledMethod::
/// spliced_bodies_side_effect_free`), computed at compile time with this same
/// predicate over each spliced region. It is vacuously `true` for an artifact
/// that spliced nothing.
///
/// Conservative in both directions it can be: an out-of-range `resume_bci`
/// (including the `u32::MAX` identity-less re-run sentinel) falls back to
/// asking the question of the whole body, which is the historical rule.
pub(crate) fn replay_from_entry_is_observably_equivalent(
    code: &[u8],
    spliced_bodies_pure: bool,
    resume_bci: u32,
) -> bool {
    // The historical rule first: a body that commits nothing anywhere needs no
    // reasoning about where the attempt stopped, and answers `true` even for
    // the `u32::MAX` sentinel.
    if !cratonvm_jit::bytecode_commits_side_effect(code, code.len()) {
        return true;
    }
    if !spliced_bodies_pure {
        return false;
    }
    let Ok(prefix) = usize::try_from(resume_bci) else {
        return false;
    };
    if prefix > code.len() {
        return false;
    }
    !cratonvm_jit::bytecode_commits_side_effect(code, prefix)
}

pub(crate) fn deopt_frame_matches_method(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if rframe.method_key.is_empty() {
        return false;
    }
    let Some((rest, key_desc)) = rframe.method_key.rsplit_once(':') else {
        return false;
    };
    let Some((key_class, key_method)) = rest.rsplit_once('.') else {
        return false;
    };
    key_class == class_name && key_method == method_name && key_desc == descriptor
}

/// jit-invokedynamic-groovy-regression fix — de-speculate the method a
/// MISMATCHED stashed frame actually belongs to (parsed from its baked
/// `method_key`), so the truly-trapping method gets evicted/blacklisted and
/// stops re-trapping, instead of the consumer's own (innocent) method eating
/// the deopt accounting. No-op for an unparseable/empty key.
pub(super) fn despeculate_stashed_frame_method(
    shared: &SharedVm,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) {
    let Some((rest, key_desc)) = rframe.method_key.rsplit_once(':') else {
        return;
    };
    let Some((key_class, key_method)) = rest.rsplit_once('.') else {
        return;
    };
    // `UnreachedCode` = give-up-immediately (`recommend_action`): the frame's
    // owner is made not-compilable on the first mismatch so it stops
    // re-trapping, instead of the count-based recompile churn `UncommonTrap`
    // would drive.
    crate::jit::helpers::DeoptimizationController::deoptimize(
        shared,
        key_class,
        key_method,
        key_desc,
        cratonvm_jit::deopt::DeoptReason::UnreachedCode,
        rframe.bci,
    );
}

/// deopt-osr Step 9 — resume a real-frame deopt under the epoch staleness
/// guard, then drive de-speculation. Only reached under `CRATONVM_DEOPT_REAL`
/// with `compiled.can_deopt_resume`, so it is inert in production.
///
/// 1. **Staleness guard.** The running artifact (`compiled`) carries the
///    compilation epoch live when it was installed; the method's *live* epoch
///    advances on every invalidation (`SharedVm::bump_compilation_epoch`). An
///    artifact whose epoch is behind is superseded, but its frame is still
///    resumed: the trapping frame owns its artifact, so the baked
///    `DeoptimizationPoint` is valid and self-consistent with the code that
///    trapped (the epochs version the speculation, not the frame layout).
///    Only a redefinition of the declaring class, which invalidates the
///    bytecode itself, falls back to the whole-method re-run (`None`).
/// 2. **Resume.** Build + push the interpreter frame and resume at the
///    trapping bci (`resume_real_ir_deopt`).
/// 3. **De-speculation.** Record the deopt and drive the escalation policy
///    (`DeoptimizationController::deoptimize`: log the event so the deopt rate
///    is observable, evict so the next call recompiles, blacklist on repeated
///    deopts), which also advances the live epoch. Run AFTER the resume
///    decision so it only affects FUTURE invocations — the current frame,
///    already resumed, is unaffected. The reason is recovered from the matching
///    deopt point so OSR-exit events stay countable separately from guards.
///
/// Returns `Some` when the frame was resumed (caller returns it), `None` to
/// fall through to the whole-method re-run.
pub(super) fn real_frame_deopt_resume_and_despeculate(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<CachedCallResult> {
    // Identity gate (jit-invokedynamic-groovy-regression): never resume a
    // frame that was stashed by a DIFFERENT method (a nested compiled callee's
    // trap whose sentinel propagated up to this outer sink). Resuming it here
    // would materialize THIS method's frame with the callee's locals/stack/bci
    // — arbitrary misexecution (the root cause of the Groovy "duplicate main
    // method" compiler-internal failures that forced the 5ceb880f revert).
    // De-speculate the frame's real owner so it stops re-trapping, then fall
    // back to the whole-method re-run (the pre-existing conservative path).
    if !deopt_frame_matches_method(
        rframe,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
            eprintln!(
                "[cratonvm-deopt] stashed frame identity mismatch: frame={} bci={} \
                 vs sink method {}.{}:{} — despeculating frame owner, safe re-run",
                rframe.method_key,
                rframe.bci,
                cached.class_name,
                cached.method_name,
                cached.method_descriptor
            );
        }
        despeculate_stashed_frame_method(shared, rframe);
        return None;
    }
    let method_key = format!(
        "{}.{}:{}",
        cached.class_name, cached.method_name, cached.method_descriptor
    );
    let live = shared.compilation_epoch_for(&method_key);
    // A superseded artifact still resumes. The trapping frame owns its
    // artifact until it returns, so its code and deopt boxes are live, and the
    // snapshot is SELF-CONSISTENT with the (stale, still executing) code that
    // trapped — the epochs version the speculation, not the frame layout — so
    // resuming is sound once the identity check above passed
    // (jit-invokedynamic-groovy-regression fix: skipping here forced the
    // imprecise whole-method re-run for every trap arriving through a stale
    // cached entry right after the first de-speculation, re-duplicating side
    // effects). Class redefinition invalidates the bytecode itself, so it keeps
    // the conservative skip.
    let fresh = compiled.compilation_epoch >= live
        || !class_was_redefined(shared, cached.declaring_class_id);
    let resumed = if fresh {
        resume_real_ir_deopt(shared, thread, cached, rframe)
    } else {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
            eprintln!(
                "[cratonvm-deopt] skip resume — artifact epoch {} < live {} for {} (superseded)",
                compiled.compilation_epoch, live, method_key
            );
        }
        None
    };
    // De-speculate (record + evict + escalate + bump live epoch). The reason is
    // recovered from the deopt point matching the trapping bci so OSR-exit
    // events are tallied separately from guard deopts.
    let reason = compiled
        .deopt_points
        .iter()
        .find(|dp| dp.bci == rframe.bci)
        .map(|dp| dp.reason)
        .unwrap_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap);
    crate::jit::helpers::DeoptimizationController::deoptimize(
        shared,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        reason,
        rframe.bci,
    );

    // deopt-osr Step 9 follow-up (c): per-bci de-spec. `deoptimize` above evicts
    // the whole artifact and (on enough *aggregate* deopts) escalates to a
    // whole-method blacklist. Before that escalation can fire, give the SINGLE
    // speculation site that keeps failing a chance to be dropped on its own: once
    // THIS bci has deopted `PER_BCI_DESPEC_LIMIT` times, record `(method, bci)` in
    // the de-spec registry the optimizing backend consults
    // (`despec_contains`), so the next compilation suppresses just that
    // speculative guard (falling back to per-access bounds checks) and the method
    // stays compiled. A method whose deopts are spread across many bcis still
    // hits the per-method backstop; a method with one pathological site gets
    // de-spec'd there and never reaches whole-method give-up. The limit mirrors
    // HotSpot's `PerBytecodeTrapLimit`. Skipped for the superseded-artifact
    // sentinel (`bci == u32::MAX`), whose failing site was already counted on the
    // pre-supersession deopts.
    //
    // NO LONGER inert in production. This sink was `deopt-real`-only when that
    // sentence was written; since 2026-09-07 the three `jit_bridge` sinks reach
    // it through `sink_precise_resume_allowed` too, so per-bci de-spec now fires
    // on ordinary runs. That is the intended direction — one pathological
    // speculation site gets suppressed on the next compile and the method stays
    // compiled, instead of the whole method being given up.
    const PER_BCI_DESPEC_LIMIT: usize = 4;
    if rframe.bci != u32::MAX {
        let bci_deopts = shared
            .jit
            .deopt_log
            .lock()
            .deopt_count_at_bci(&method_key, rframe.bci);
        if bci_deopts >= PER_BCI_DESPEC_LIMIT {
            cratonvm_jit::deopt::despec_insert(&method_key, rframe.bci);
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
                eprintln!(
                    "[cratonvm-deopt] per-bci de-spec: {} bci={} ({} deopts ≥ {}) — \
                     speculation suppressed on next compile (method stays compilable)",
                    method_key, rframe.bci, bci_deopts, PER_BCI_DESPEC_LIMIT
                );
            }
        }
    }
    // P1 shadow record (`docs/threading/thread-transition-states.md` §7.2):
    // close the `Deoptimizing` window on the RESUMED path only. A `None` here
    // means no frame was materialised and the caller falls back to the
    // whole-method re-run — that path leaves compiled code through the JIT
    // entry pop, which resolves the window itself
    // (`conservative_roots::leaving_compiled_state`).
    if resumed.is_some() {
        crate::threading::thread_state::record_transition(
            crate::threading::thread_state::ThreadExecState::JavaRunning,
            "interpreter::real_frame_deopt_resume_and_despeculate",
        );
    }
    resumed
}

#[cfg(test)]
mod deopt_step3_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_jit::deopt::{FrameValue, ReconstructedFrame, VirtualObjectState};
    use std::sync::Arc;

    fn minimal_cached() -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(0),
            class_name: Arc::from("T"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(&[0xb1u8][..]), // return
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 8,
            max_locals: 4,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            tiering_settled: std::sync::atomic::AtomicU32::new(0),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        })
    }

    /// As `minimal_cached` but `ACC_SYNCHRONIZED` — exercises the
    /// elided-monitor gate for virtual-object resume.
    fn synchronized_cached() -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(0),
            class_name: Arc::from("T"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(&[0xb1u8][..]), // return
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 8,
            max_locals: 4,
            num_params: 0,
            is_synchronized: true,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            tiering_settled: std::sync::atomic::AtomicU32::new(0),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        })
    }

    /// A scalar-replaced object placeholder: id `id`, class `class_id`, with the
    /// given field values (`num_fields` derived from the vec length).
    fn vobj(id: usize, class_id: u32, fields: Vec<FrameValue>) -> FrameValue {
        FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id,
            class_id,
            num_fields: fields.len(),
            field_values: fields,
        })
    }

    fn rframe(locals: Vec<FrameValue>, stack: Vec<FrameValue>, bci: u32) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: "T.m:()V".to_string(),
            bci,
            locals,
            stack,
            monitors: Vec::new(),
            caller_frames: Vec::new(),
        }
    }

    /// Pure stack mapper (`ir_deopt_frame_values` → `fv_to_value`):
    /// Int/Undefined/Object map; `Unsupported` + virtual bail.
    #[test]
    fn maps_int_and_object_refuses_cat2_and_virtual() {
        let got = ir_deopt_frame_values(&[
            FrameValue::Int(42),
            FrameValue::Undefined,
            FrameValue::Object(0x1000),
            FrameValue::Object(0),
        ]);
        assert_eq!(
            got,
            Some(vec![
                Value::Int(42),
                Value::Int(0),
                // SAFETY: test-only sentinel address (0x1000); never dereferenced, only compared for equality.
                Value::Object(Some(unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) })),
                Value::Object(None),
            ])
        );
        assert!(ir_deopt_frame_values(&[FrameValue::Unsupported]).is_none());
        assert!(ir_deopt_frame_values(&[FrameValue::VirtualObjectRef(0)]).is_none());
    }

    /// ldiv/lrem long deopt-resume: a `long` reconstructs as a full-64-bit
    /// `Value::Long`, and the LOCALS mapper COLLAPSES the JVM-two-slot snapshot
    /// (a `long` reserves its upper half as `Undefined`) into the compact arg
    /// list `copy_args_to_locals` re-expands — so a local after a `long` is not
    /// mis-aligned. The operand-stack mapper keeps one entry per `long` (already
    /// compact). A regression in either mapper silently corrupts a resumed
    /// long-bearing frame at an `ldiv`/`lrem` (or int-div) deopt.
    #[test]
    fn long_locals_collapse_and_compact_stack() {
        // locals: long a@0-1, int n@2  →  JVM-slot-indexed [Long, Undefined, Int].
        // Collapses to one entry per long (upper-half placeholder dropped) so the
        // int stays adjacent for the cat-2 re-expansion inside new_pooled.
        assert_eq!(
            ir_deopt_locals(&[
                FrameValue::Long(0x7_0000_0000),
                FrameValue::Undefined,
                FrameValue::Int(9),
            ]),
            Some(vec![Value::Long(0x7_0000_0000), Value::Int(9)]),
        );
        // Operand stack is already compact (one entry per long) — no collapse.
        assert_eq!(
            ir_deopt_frame_values(&[FrameValue::Long(123), FrameValue::Long(0)]),
            Some(vec![Value::Long(123), Value::Long(0)]),
        );
    }

    /// Build a frame with an Int local, a real Object local, and an Int on the
    /// operand stack; assert the built frame's locals/stack/pc.
    #[test]
    fn builds_int_and_object_frame() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();

        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object/code pointer to integer address
        let addr = obj.as_ptr() as usize as u64;
        let rf = rframe(
            vec![FrameValue::Int(42), FrameValue::Object(addr)],
            vec![FrameValue::Int(7)],
            5,
        );

        let pin_base = thread.native_pin_roots.len();
        let frame = build_deopt_frame_inner(&shared, &mut thread, &cached, &rf, false)
            .expect("clean pilot must build");
        assert_eq!(frame.pc, 5);
        assert_eq!(frame.get_local(0), Value::Int(42));
        assert!(matches!(frame.get_local(1), Value::Object(Some(_))));
        assert_eq!(frame.stack.len(), 1);
        assert_eq!(frame.stack.peek_at(0), Value::Int(7));
        drop(frame);
        thread.native_pin_roots.truncate(pin_base);
    }

    /// deopt-osr P2 — a frame with cat-2 `long`, cat-1 `float` locals and a
    /// cat-2 `double` on the operand stack reconstructs with the right widths.
    /// The locals cat-2 collapse must keep slots aligned: the `long`'s reserved
    /// upper half (an `Undefined` placeholder in the snapshot) is dropped and
    /// re-expanded by `copy_args_to_locals`, so the `float` at JVM slot 2 lands
    /// at slot 2 (NOT shifted into the long's high half), and the `int` at 3.
    #[test]
    fn builds_cat2_and_fp_frame() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached(); // max_locals = 4, max_stack = 8

        // JVM-slot locals for (long a, float f, int n): a@0 (upper half @1),
        // f@2, n@3.
        let rf = rframe(
            vec![
                FrameValue::Long(0x7_0000_0001),
                FrameValue::Undefined, // long's reserved upper half
                FrameValue::Float(2.5f32.to_bits() as u64),
                FrameValue::Int(9),
            ],
            vec![FrameValue::Double(std::f64::consts::PI.to_bits())],
            3,
        );

        let pin_base = thread.native_pin_roots.len();
        let frame = build_deopt_frame_inner(&shared, &mut thread, &cached, &rf, false)
            .expect("cat-2/FP frame must build");
        assert_eq!(frame.pc, 3);
        // The long is stored at slot 0 with all 64 bits intact (the resumed
        // `lload` reads the raw word as a long — `local_kinds` disambiguates
        // long-vs-double, which the generic NaN-boxed `get_local` cannot).
        assert_eq!(
            frame.get_local_raw(0),
            0x7_0000_0001,
            "long keeps all 64 bits (no truncation)"
        );
        // The cat-2 collapse kept the float at slot 2 and the int at slot 3 — had
        // the long's upper half NOT been dropped, these would shift by one.
        assert_eq!(
            frame.get_local(2),
            Value::Float(2.5),
            "float at its own slot"
        );
        assert_eq!(
            frame.get_local(3),
            Value::Int(9),
            "int not shifted by collapse"
        );
        assert_eq!(frame.stack.len(), 1);
        assert_eq!(
            frame.stack.peek_at(0),
            Value::Double(std::f64::consts::PI),
            "double on the operand stack (compact, one slot)"
        );
        drop(frame);
        thread.native_pin_roots.truncate(pin_base);
    }

    /// An unmappable (cat-2 `Unsupported`) slot bails to `None` (re-run) and
    /// leaks no pins / pushes no frame.
    #[test]
    fn refuses_unmappable_without_leaking_pins() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        let rf = rframe(vec![FrameValue::Unsupported], vec![], 0);
        assert!(resume_real_ir_deopt(&shared, &mut thread, &cached, &rf).is_none());
        assert_eq!(thread.native_pin_roots.len(), 0);
        assert_eq!(thread.frames.len(), 0);
    }

    /// Step-4 GC-rooting HANDOFF — the load-bearing invariant: after
    /// `resume_real_ir_deopt` pushes the frame and releases the temporary pins,
    /// a forced GC must STILL find the reconstructed oop — via the PUSHED FRAME
    /// (its locals are the root now), not the released pins. A handoff mistake
    /// here would be a production UAF under the gate.
    #[test]
    fn resumed_frame_roots_oops_after_pin_release() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object/code pointer to integer address
        let addr = obj.as_ptr() as usize as u64;
        let rf = rframe(vec![FrameValue::Object(addr)], vec![], 3);

        let pin_base = thread.native_pin_roots.len();
        let r = resume_real_ir_deopt(&shared, &mut thread, &cached, &rf)
            .expect("clean pilot must resume");
        assert!(matches!(r, CachedCallResult::FramePushed));
        // Pins released after the push; the pushed frame is the sole root now.
        assert_eq!(thread.native_pin_roots.len(), pin_base);
        assert_eq!(thread.frames.len(), 1);

        // Force a GC: the oop must survive via the pushed frame (and be forwarded
        // in place under a moving collector).
        maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-resume");

        let frame = thread.frames.last().expect("resumed frame is on the stack");
        assert_eq!(frame.pc, 3);
        match frame.get_local(0) {
            Value::Object(Some(o)) => {
                assert_eq!(
                    shared.mem.heap.class_id_of(o),
                    cratonvm_types::ClassId::new(7)
                );
            }
            other => panic!("local 0 must survive GC via the pushed frame, got {other:?}"),
        }
    }

    /// The reconstructed Object survives a forced GC during the build (rooted via
    /// `native_pin_roots`); the built frame holds the live (forwarded) ref.
    #[test]
    fn object_survives_forced_gc_during_build() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();

        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object/code pointer to integer address
        let addr = obj.as_ptr() as usize as u64;
        let rf = rframe(vec![FrameValue::Object(addr)], vec![], 0);

        let pin_base = thread.native_pin_roots.len();
        let frame =
            build_deopt_frame_inner(&shared, &mut thread, &cached, &rf, /* stress */ true)
                .expect("must build under a forced GC");
        match frame.get_local(0) {
            Value::Object(Some(o)) => {
                assert_eq!(
                    shared.mem.heap.class_id_of(o),
                    cratonvm_types::ClassId::new(7)
                );
            }
            other => panic!("local 0 must be a live object, got {other:?}"),
        }
        drop(frame);
        thread.native_pin_roots.truncate(pin_base);
    }

    // ---------------------------------------------------------------------
    // Workstream A — virtual-object (scalar-replaced) re-materialization wired
    // into the resume path.
    // ---------------------------------------------------------------------

    /// A1/A2: a reconstructed frame carrying a `VirtualObject` local resumes —
    /// the materializer turns it into a real heap object, the frame is pushed,
    /// the temporary shell pins are released, and the materialized object (with
    /// its fields) survives a forced GC via the pushed frame.
    #[test]
    fn resumes_frame_with_virtual_object() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();

        // local 0 = scalar-replaced object id 0, class 5, fields [Int(42), Undefined].
        let rf = rframe(
            vec![vobj(0, 5, vec![FrameValue::Int(42), FrameValue::Undefined])],
            vec![],
            4,
        );

        let pin_base = thread.native_pin_roots.len();
        let r = resume_real_ir_deopt(&shared, &mut thread, &cached, &rf)
            .expect("virtual-bearing frame must resume after materialization");
        assert!(matches!(r, CachedCallResult::FramePushed));
        // All temporary shell pins released after the push (the frame roots them).
        assert_eq!(thread.native_pin_roots.len(), pin_base);
        assert_eq!(thread.frames.len(), 1);

        // Force a GC: the materialized shell must survive via the pushed frame.
        maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-resume");
        let frame = thread.frames.last().expect("resumed frame is on the stack");
        assert_eq!(frame.pc, 4);
        match frame.get_local(0) {
            Value::Object(Some(o)) => {
                assert_eq!(
                    shared.mem.heap.class_id_of(o),
                    cratonvm_types::ClassId::new(5)
                );
                assert_eq!(shared.mem.heap.get_field(o, 0), Value::Int(42));
                assert_eq!(shared.mem.heap.get_field(o, 1), Value::Int(0)); // Undefined -> 0
            }
            other => panic!("local 0 must be the materialized object, got {other:?}"),
        }
    }

    /// A3: a frame with two mutually-referencing scalar-replaced objects resumes
    /// with both materialized as heap objects whose fields point at each other
    /// (the two-phase shells-first materializer resolves the back-edge).
    #[test]
    fn resumes_frame_with_object_cycle() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();

        // local 0 = A(id 0).f0 -> B ; local 1 = B(id 1).f0 -> A
        let a = vobj(0, 1, vec![FrameValue::VirtualObjectRef(1)]);
        let b = vobj(1, 1, vec![FrameValue::VirtualObjectRef(0)]);
        let rf = rframe(vec![a, b], vec![], 2);

        let r = resume_real_ir_deopt(&shared, &mut thread, &cached, &rf)
            .expect("cyclic virtual frame must resume");
        assert!(matches!(r, CachedCallResult::FramePushed));

        // No GC forced here (addresses stable): verify the heap cycle is wired.
        let frame = thread.frames.last().expect("resumed frame is on the stack");
        let (oa, ob) = match (frame.get_local(0), frame.get_local(1)) {
            (Value::Object(Some(oa)), Value::Object(Some(ob))) => (oa, ob),
            other => panic!("both locals must be materialized objects, got {other:?}"),
        };
        assert_eq!(shared.mem.heap.get_field(oa, 0), Value::Object(Some(ob)));
        assert_eq!(shared.mem.heap.get_field(ob, 0), Value::Object(Some(oa)));
    }

    /// Phase C (monitors): a frame holding a `synchronized(scalarObj)` monitor
    /// resumes — the scalar object is materialized and its elided lock is
    /// re-acquired on the resumed thread at the recorded depth, so the resumed
    /// frame's eventual `monitorexit`(es) balance.
    #[test]
    fn resumes_frame_with_held_monitor() {
        use cratonvm_jit::deopt::MonitorInfo;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached(); // NOT synchronized

        // local 0 = scalar object id 0 (class 5, one Int field); a held monitor on
        // it at recursion depth 2 (nested `synchronized` blocks).
        let rf = ReconstructedFrame {
            method_key: "T.m:()V".to_string(),
            bci: 4,
            locals: vec![vobj(0, 5, vec![FrameValue::Int(9)])],
            stack: vec![],
            monitors: vec![MonitorInfo {
                object: FrameValue::VirtualObjectRef(0),
                lock_depth: 2,
            }],
            caller_frames: Vec::new(),
        };

        let pin_base = thread.native_pin_roots.len();
        let r = resume_real_ir_deopt(&shared, &mut thread, &cached, &rf)
            .expect("monitor-bearing frame must resume");
        assert!(matches!(r, CachedCallResult::FramePushed));
        assert_eq!(thread.native_pin_roots.len(), pin_base);

        let obj = match thread.frames.last().unwrap().get_local(0) {
            Value::Object(Some(o)) => o,
            other => panic!("local 0 must be the materialized object, got {other:?}"),
        };
        // The lock is held at depth 2: two exits succeed, a third fails (not owned).
        assert!(
            shared.threads.monitors.exit(obj, thread.thread_id).is_ok(),
            "exit 1 (2->1)"
        );
        assert!(
            shared.threads.monitors.exit(obj, thread.thread_id).is_ok(),
            "exit 2 (1->0)"
        );
        assert!(
            shared.threads.monitors.exit(obj, thread.thread_id).is_err(),
            "exit 3 must fail — monitor no longer held"
        );
    }

    /// A2 elided-monitor gate: a `synchronized` method carrying a virtual frame
    /// must NOT resume (lock elision over the scalar-replaced object is
    /// undetectable here) — it falls back to re-run with no pins leaked and no
    /// frame pushed.
    #[test]
    fn virtual_resume_blocked_for_synchronized_method() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = synchronized_cached();
        let rf = rframe(vec![vobj(0, 5, vec![FrameValue::Int(1)])], vec![], 0);

        assert!(resume_real_ir_deopt(&shared, &mut thread, &cached, &rf).is_none());
        assert_eq!(thread.native_pin_roots.len(), 0);
        assert_eq!(thread.frames.len(), 0);
    }

    // ---------------------------------------------------------------------
    // Step 9 — de-speculation wiring + compilation-epoch staleness guard.
    // ---------------------------------------------------------------------

    /// A `CompiledMethod` with a single bounds-check deopt point at `bci`, stamped
    /// with `epoch`. The 1-byte `ret` body is never executed by these tests — only
    /// the metadata (`compilation_epoch`, `deopt_points`) is consulted.
    fn cm_with_deopt_point(epoch: u64, bci: u32) -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.compilation_epoch = epoch;
        cm.deopt_points
            .push(cratonvm_jit::deopt::DeoptimizationPoint {
                native_offset: 0,
                bci,
                reason: cratonvm_jit::deopt::DeoptReason::BoundsCheck,
                action: cratonvm_jit::deopt::DeoptAction::Reinterpret,
                semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
                speculation_id: 0,
                frame_state: cratonvm_jit::deopt::FrameState {
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

    /// The per-method live compilation-epoch registry: absent → 0, `bump`
    /// increments and returns, `compilation_epoch_for` reads back.
    #[test]
    fn step9_epoch_registry_bump_and_read() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        assert_eq!(shared.compilation_epoch_for("X.y:()V"), 0);
        assert_eq!(shared.bump_compilation_epoch("X.y:()V"), 1);
        assert_eq!(shared.compilation_epoch_for("X.y:()V"), 1);
        assert_eq!(shared.bump_compilation_epoch("X.y:()V"), 2);
        assert_eq!(shared.compilation_epoch_for("X.y:()V"), 2);
        // Independent methods have independent epochs.
        assert_eq!(shared.compilation_epoch_for("X.z:()V"), 0);
    }

    /// A *current* artifact (epoch == live) resumes the trapping frame AND the
    /// deopt is recorded in the log (so the deopt rate is observable).
    #[test]
    fn step9_fresh_artifact_resumes_and_records_deopt() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached(); // T.m:()V
        let cm = cm_with_deopt_point(0, 5);
        let rf = rframe(vec![FrameValue::Int(1)], vec![], 5);

        // live epoch for T.m:()V is 0 (never invalidated) == artifact epoch 0.
        let r = real_frame_deopt_resume_and_despeculate(&shared, &mut thread, &cm, &cached, &rf)
            .expect("current artifact must resume");
        assert!(matches!(r, CachedCallResult::FramePushed));
        assert_eq!(thread.frames.len(), 1);
        // De-speculation recorded the event (reason recovered from the deopt point).
        assert_eq!(shared.jit.deopt_log.lock().deopt_count("T.m:()V"), 1);
    }

    /// A *superseded* artifact (epoch < live, simulating an invalidation since
    /// it was installed) STILL resumes in the default retain-everything mode
    /// (jit-invokedynamic-groovy-regression fix): the stale artifact's code
    /// and deopt boxes are leaked, so its snapshot remains self-consistent
    /// with the code that trapped, and refusing forced the corrupting
    /// imprecise re-run for traps arriving through stale cached entries. The
    /// conservative skip is retained only after class redefinition.
    #[test]
    fn step9_stale_artifact_resumes_in_retain_mode() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached(); // T.m:()V
        let cm = cm_with_deopt_point(0, 5); // artifact epoch 0
        let rf = rframe(vec![FrameValue::Int(1)], vec![], 5);

        // Simulate a prior invalidation: live epoch advances past the artifact.
        assert_eq!(shared.bump_compilation_epoch("T.m:()V"), 1);

        let r = real_frame_deopt_resume_and_despeculate(&shared, &mut thread, &cm, &cached, &rf)
            .expect("superseded artifact must still resume in retain mode");
        assert!(matches!(r, CachedCallResult::FramePushed));
        assert_eq!(thread.frames.len(), 1);
        // Recorded for the deopt rate as before.
        assert_eq!(shared.jit.deopt_log.lock().deopt_count("T.m:()V"), 1);
    }

    /// deopt-osr Step 9 follow-up (c): per-bci de-spec. After
    /// `PER_BCI_DESPEC_LIMIT` deopts at the SAME bci, the sink records
    /// `(method, bci)` in the de-spec registry the optimizing backend consults
    /// (`despec_contains`) — so that ONE speculation is suppressed on the next
    /// compile instead of the whole method being blacklisted. Fewer deopts, or a
    /// different bci, do not de-spec.
    #[test]
    fn step9_fuc_per_bci_despec_after_limit() {
        // A test-unique method key so the process-global de-spec registry cannot
        // collide with other parallel tests.
        let cached = Arc::new(CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(0),
            class_name: Arc::from("DespecFuC"),
            method_name: Arc::from("loop"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(&[0xb1u8][..]), // return
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 8,
            max_locals: 4,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            tiering_settled: std::sync::atomic::AtomicU32::new(0),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        });
        let key = "DespecFuC.loop:()V";
        cratonvm_jit::deopt::despec_clear_for_test();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");

        // Drive 4 deopts at the SAME bci (5). Each call re-evicts and bumps the
        // live epoch, so only the first resumes; all four are recorded, and the
        // per-bci de-spec fires exactly when the count reaches the limit (4).
        for n in 1..=4u32 {
            let cm = cm_with_deopt_point(0, 5);
            // The frame's baked identity must name `cached` — the identity gate
            // (jit-invokedynamic-groovy-regression fix) refuses a mismatched
            // frame before any of the de-spec accounting this test measures.
            let rf = ReconstructedFrame {
                method_key: key.to_string(),
                bci: 5,
                locals: vec![FrameValue::Int(1)],
                stack: vec![],
                monitors: Vec::new(),
                caller_frames: Vec::new(),
            };
            let _ =
                real_frame_deopt_resume_and_despeculate(&shared, &mut thread, &cm, &cached, &rf);
            let despec_now = cratonvm_jit::deopt::despec_contains(key, 5);
            if n < 4 {
                assert!(
                    !despec_now,
                    "must NOT de-spec before the per-bci limit (n={n})"
                );
            } else {
                assert!(despec_now, "must de-spec once the per-bci limit is reached");
            }
        }
        // A different bci on the same method is unaffected — de-spec is per-site.
        assert!(!cratonvm_jit::deopt::despec_contains(key, 9));
        cratonvm_jit::deopt::despec_clear_for_test();
    }

    /// jit-invokedynamic-groovy-regression fix — the frame-identity parser:
    /// exact `"<class>.<method>:<descriptor>"` matches; empty / unparseable /
    /// differing keys never match (an empty key is the legacy-producer and
    /// superseded-guard-sentinel shape, which must always take the safe
    /// re-run).
    #[test]
    fn deopt_frame_identity_matching() {
        let rf = |key: &str| ReconstructedFrame {
            method_key: key.to_string(),
            bci: 0,
            locals: vec![],
            stack: vec![],
            monitors: Vec::new(),
            caller_frames: Vec::new(),
        };
        // Slash-form class names contain '.': only the LAST '.' before the
        // ':' separates class from method.
        assert!(deopt_frame_matches_method(
            &rf("org/x/Foo.bar:(I)V"),
            "org/x/Foo",
            "bar",
            "(I)V"
        ));
        assert!(!deopt_frame_matches_method(
            &rf("org/x/Foo.bar:(I)V"),
            "org/x/Foo",
            "baz",
            "(I)V"
        ));
        assert!(!deopt_frame_matches_method(
            &rf("org/x/Foo.bar:(I)V"),
            "org/x/Other",
            "bar",
            "(I)V"
        ));
        assert!(!deopt_frame_matches_method(
            &rf("org/x/Foo.bar:(I)V"),
            "org/x/Foo",
            "bar",
            "(J)V"
        ));
        assert!(!deopt_frame_matches_method(&rf(""), "T", "m", "()V"));
        assert!(!deopt_frame_matches_method(&rf("garbage"), "T", "m", "()V"));
    }

    /// jit-invokedynamic-groovy-regression fix — a MISMATCHED stashed frame
    /// must not resume into the sink's method: `real_frame_deopt_resume_and_
    /// despeculate` refuses (returns `None`, no frame pushed) and
    /// de-speculates the frame's REAL owner (parsed from its key), not the
    /// sink's method.
    #[test]
    fn mismatched_frame_refused_and_owner_despeculated() {
        let cached = minimal_cached(); // "T"/"m"/"()V"
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cm = cm_with_deopt_point(0, 3);
        let foreign = ReconstructedFrame {
            method_key: "Inner/Callee.trap:(I)I".to_string(),
            bci: 3,
            locals: vec![FrameValue::Int(7)],
            stack: vec![],
            monitors: Vec::new(),
            caller_frames: Vec::new(),
        };
        let frames_before = thread.frames.len();
        let r =
            real_frame_deopt_resume_and_despeculate(&shared, &mut thread, &cm, &cached, &foreign);
        assert!(r.is_none(), "mismatched frame must refuse to resume");
        assert_eq!(
            thread.frames.len(),
            frames_before,
            "no frame may be pushed for a mismatched stash"
        );
        // The deopt was attributed to the FRAME's method, not the sink's.
        let log = shared.jit.deopt_log.lock();
        assert!(
            log.deopt_count_at_bci("Inner/Callee.trap:(I)I", 3) >= 1,
            "frame owner must receive the deopt accounting"
        );
        assert_eq!(
            log.deopt_count_at_bci("T.m:()V", 3),
            0,
            "sink method must NOT be charged for a foreign frame"
        );
    }

    // ---------------------------------------------------------------------
    // CRATONVM_DEOPT_VERIFY — structural-invariant verifier (P1, increment 1).
    // ---------------------------------------------------------------------

    /// A well-formed frame (ints, objects, a valid virtual-object cycle) passes.
    #[test]
    fn verify_accepts_valid_frame() {
        let rf = rframe(
            vec![FrameValue::Int(1), FrameValue::Object(0x1000)],
            vec![FrameValue::Int(2)],
            0,
        );
        assert!(verify_reconstructed_frame(&rf, 4, 8).is_ok());

        // A valid two-object cycle (each ref resolves to a defined object).
        let a = vobj(0, 1, vec![FrameValue::VirtualObjectRef(1)]);
        let b = vobj(1, 1, vec![FrameValue::VirtualObjectRef(0)]);
        let cyc = rframe(vec![a, b], vec![], 0);
        assert!(verify_reconstructed_frame(&cyc, 4, 8).is_ok());
    }

    /// A slot count past the declared maxima — the signature of a shifted
    /// snapshot — is rejected.
    #[test]
    fn verify_rejects_overlong_locals_and_stack() {
        let too_many_locals = rframe(vec![FrameValue::Int(0); 5], vec![], 0);
        assert!(verify_reconstructed_frame(&too_many_locals, 4, 8).is_err());

        let too_deep_stack = rframe(vec![], vec![FrameValue::Int(0); 9], 0);
        assert!(verify_reconstructed_frame(&too_deep_stack, 4, 8).is_err());
    }

    /// A virtual-object descriptor whose declared `num_fields` disagrees with its
    /// actual `field_values` length is rejected (malformed snapshot).
    #[test]
    fn verify_rejects_malformed_virtual_field_count() {
        let bad = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 0,
            class_id: 1,
            num_fields: 2,                          // claims 2…
            field_values: vec![FrameValue::Int(1)], // …but carries 1
        });
        let rf = rframe(vec![bad], vec![], 0);
        assert!(verify_reconstructed_frame(&rf, 4, 8).is_err());
    }

    /// A `VirtualObjectRef` with no defining `VirtualObject` in the frame is
    /// rejected (it would later fail materialization with an unknown id).
    #[test]
    fn verify_rejects_dangling_virtual_ref() {
        let rf = rframe(vec![FrameValue::VirtualObjectRef(9)], vec![], 0);
        assert!(verify_reconstructed_frame(&rf, 4, 8).is_err());
    }

    /// Oop layer: a real heap address (and null) passes; a non-heap word in an
    /// Object slot — the UAF-causing drift — is rejected.
    #[test]
    fn verify_oops_accepts_real_rejects_bogus() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        let addr = obj.as_ptr() as usize as u64;

        // Real object + null pass.
        let good = rframe(
            vec![FrameValue::Object(addr), FrameValue::Object(0)],
            vec![],
            0,
        );
        assert!(verify_reconstructed_oops(&good, &shared).is_ok());

        // A small/wild address that is not in the heap is rejected.
        let bad = rframe(vec![FrameValue::Object(0x1234)], vec![], 0);
        assert!(verify_reconstructed_oops(&bad, &shared).is_err());

        // A bogus already-real field inside a scalar-replaced descriptor is also
        // caught (recursion into vfields).
        let bad_field = rframe(
            vec![vobj(0, 5, vec![FrameValue::Object(0x1234)])],
            vec![],
            0,
        );
        assert!(verify_reconstructed_oops(&bad_field, &shared).is_err());
    }

    /// A3 GC-stress: a virtual frame built with a forced GC at the refill point —
    /// the materialized shell is pinned (materialize keep-pins + the build re-pin)
    /// and forwarded in place, so the built frame holds the live object.
    #[test]
    fn virtual_shells_survive_forced_gc_during_build() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        let rf = rframe(vec![vobj(0, 5, vec![FrameValue::Int(7)])], vec![], 0);

        let pin_base = thread.native_pin_roots.len();
        let frame =
            build_deopt_frame_inner(&shared, &mut thread, &cached, &rf, /* stress */ true)
                .expect("virtual frame must build under a forced GC");
        match frame.get_local(0) {
            Value::Object(Some(o)) => {
                assert_eq!(
                    shared.mem.heap.class_id_of(o),
                    cratonvm_types::ClassId::new(5)
                );
                assert_eq!(shared.mem.heap.get_field(o, 0), Value::Int(7));
            }
            other => panic!("materialized shell must survive GC during build, got {other:?}"),
        }
        drop(frame);
        thread.native_pin_roots.truncate(pin_base);
    }

    // ---------------------------------------------------------------------
    // P4 — TRUE OSR-exit transfer into the live interpreter frame.
    // ---------------------------------------------------------------------

    /// Seed a single live frame for `cached` (the OSR'd method) holding the given
    /// reconstructed state, returning the now-live `thread.frames[0]`.
    fn seed_live_frame(
        shared: &SharedVm,
        thread: &mut JvmThread,
        cached: &Arc<CachedBytecodeMethod>,
        locals: Vec<FrameValue>,
        stack: Vec<FrameValue>,
        bci: u32,
    ) {
        let rf = rframe(locals, stack, bci);
        resume_real_ir_deopt(shared, thread, cached, &rf).expect("seed frame must push");
        assert_eq!(thread.frames.len(), 1);
    }

    /// A validated OSR-entry plan plus the artifact it was validated against —
    /// the two arguments the in-place transfer now needs.
    ///
    /// The artifact records ONE `REEXECUTE` deopt point at `exit_bci`, which is
    /// what `OsrEntryPlan::resume_after_exit` requires before it will name that
    /// bci as an exact resume point; `entry_pc` is the loop header the entry was
    /// taken at. The plan is built by hand rather than through
    /// `validate_osr_entry` so these tests stay about the transfer — the
    /// validator has its own coverage in `jit/src/lib.rs`.
    fn osr_plan_for(
        entry_pc: usize,
        exit_bci: u32,
    ) -> (cratonvm_jit::CompiledMethod, cratonvm_jit::OsrEntryPlan) {
        let cm = cm_with_deopt_point(0, exit_bci);
        let plan = cratonvm_jit::OsrEntryPlan {
            entry_pc,
            resume_bci: entry_pc,
            native_offset: 0,
            dead_mask: 0,
            contract: cratonvm_jit::OsrContractSource::RegisterHomes,
            exit_policy: cratonvm_jit::OsrExitPolicy::ExactTransfer,
            expected_locals: Vec::new(),
        };
        (cm, plan)
    }

    /// The core P4 invariant: the JIT-advanced loop state OVERWRITES the live
    /// frame's locals + operand stack IN PLACE and re-points pc, WITHOUT pushing a
    /// new frame — so the interpreter resumes the loop body from where the OSR'd
    /// code bailed (vs the reject, which would re-run those iterations).
    #[test]
    fn osr_exit_transfers_advanced_state_into_live_frame() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();

        // Pre-advance live state: i=100, acc=4950, a stale operand-stack entry, at
        // loop-header bci 7.
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(100), FrameValue::Int(4950)],
            vec![FrameValue::Int(777)],
            7,
        );

        // The OSR'd code advanced 5 iterations (i=105, acc=5460) and left the
        // operand stack empty at the loop header.
        let advanced = rframe(vec![FrameValue::Int(105), FrameValue::Int(5460)], vec![], 7);
        let (cm, plan) = osr_plan_for(7, 7);
        assert!(
            transfer_osr_exit_into_live_frame(&shared, &mut thread, 0, &advanced, &cm, &plan)
                .is_some(),
            "clean int frame must transfer"
        );

        // SAME frame (no push), with advanced locals, cleared stack, pc at the bci.
        assert_eq!(thread.frames.len(), 1, "transfer must not push a frame");
        let frame = &thread.frames[0];
        assert_eq!(frame.get_local(0), Value::Int(105));
        assert_eq!(frame.get_local(1), Value::Int(5460));
        assert_eq!(frame.pc, 7);
        assert_eq!(frame.stack.len(), 0, "stale operand stack must be replaced");
    }

    /// FU1 — the OSR-exit transfer reconstructs cat-2 (`long`/`double`) and FP
    /// (`float`) state into the live frame (was: rejected → re-run). The 1:1
    /// JVM-slot write places a `long`'s value at slot N and a dead `Int(0)` at the
    /// reserved upper half N+1, so the following `int` is NOT shifted; a `double`
    /// rides one compact operand-stack slot.
    #[test]
    fn osr_exit_transfer_handles_cat2_and_fp() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached(); // max_locals = 4, max_stack = 8
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(1), FrameValue::Int(2)],
            vec![],
            0,
        );

        // JVM-slot locals for (long a, float f, int n): a@0 (upper half @1), f@2, n@3.
        let advanced = rframe(
            vec![
                FrameValue::Long(0x7_0000_0001),
                FrameValue::Undefined,
                FrameValue::Float(2.5f32.to_bits() as u64),
                FrameValue::Int(9),
            ],
            vec![FrameValue::Double(std::f64::consts::PI.to_bits())],
            3,
        );
        let (cm, plan) = osr_plan_for(0, 3);
        assert!(
            transfer_osr_exit_into_live_frame(&shared, &mut thread, 0, &advanced, &cm, &plan)
                .is_some(),
            "cat-2/FP frame must transfer (no longer rejected)"
        );
        let frame = &thread.frames[0];
        assert_eq!(frame.pc, 3);
        // long: all 64 bits at slot 0 (raw word — local_kinds disambiguates
        // long-vs-double, which the NaN-boxed get_local cannot).
        assert_eq!(
            frame.get_local_raw(0),
            0x7_0000_0001,
            "long keeps all 64 bits"
        );
        // The cat-2 write did not shift the float (slot 2) or int (slot 3).
        assert_eq!(
            frame.get_local(2),
            Value::Float(2.5),
            "float at its own slot"
        );
        assert_eq!(
            frame.get_local(3),
            Value::Int(9),
            "int not shifted by the cat-2 write"
        );
        assert_eq!(frame.stack.len(), 1);
        assert_eq!(frame.stack.peek_at(0), Value::Double(std::f64::consts::PI));
    }

    /// A non-empty reconstructed operand stack is transferred verbatim (the stack
    /// is cleared then refilled from the snapshot).
    #[test]
    fn osr_exit_transfer_replaces_operand_stack() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(0)],
            vec![],
            0,
        );

        let advanced = rframe(
            vec![FrameValue::Int(1)],
            vec![FrameValue::Int(3), FrameValue::Int(4)],
            2,
        );
        let (cm, plan) = osr_plan_for(0, 2);
        assert!(
            transfer_osr_exit_into_live_frame(&shared, &mut thread, 0, &advanced, &cm, &plan)
                .is_some()
        );
        let frame = &thread.frames[0];
        assert_eq!(frame.pc, 2);
        assert_eq!(frame.stack.len(), 2);
        // Snapshot order is bottom→top ([3, 4]); `peek_at(0)` is the TOP.
        assert_eq!(frame.stack.peek_at(0), Value::Int(4));
        assert_eq!(frame.stack.peek_at(1), Value::Int(3));
    }

    /// GC-safety: an object reference carried in the advanced state must survive a
    /// forced GC AFTER the transfer — rooted via the mutated LIVE frame's slot (the
    /// transfer installs no temporary pin; the frame slot is the root, forwarded in
    /// place by a moving collector).
    #[test]
    fn osr_exit_transfer_roots_oop_via_live_frame() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(0)],
            vec![],
            0,
        );

        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object pointer to integer address.
        let addr = obj.as_ptr() as usize as u64;
        let advanced = rframe(
            vec![FrameValue::Object(addr), FrameValue::Int(9)],
            vec![],
            3,
        );
        let (cm, plan) = osr_plan_for(0, 3);
        assert!(
            transfer_osr_exit_into_live_frame(&shared, &mut thread, 0, &advanced, &cm, &plan)
                .is_some()
        );

        // No Java allocation between in-stub capture and the in-place write, so the
        // raw address was valid; the frame slot now roots it. Force a GC — it must
        // survive (and be forwarded in place under a moving collector).
        maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-resume");
        let frame = &thread.frames[0];
        assert_eq!(frame.pc, 3);
        match frame.get_local(0) {
            Value::Object(Some(o)) => {
                assert_eq!(
                    shared.mem.heap.class_id_of(o),
                    cratonvm_types::ClassId::new(7)
                );
            }
            other => panic!("local 0 must survive GC via the live frame, got {other:?}"),
        }
        assert_eq!(frame.get_local(1), Value::Int(9));
    }

    /// FIX (jit-osr-loop-duplicate-execution): an `Unsupported` LOCAL slot must
    /// NOT reject the whole transfer. `classify_local_kinds` marks a slot
    /// `Ambiguous` (→ `Unsupported`) whenever it is used as more than one JVM
    /// kind ANYWHERE in the method — including a slot legally reused, after its
    /// original local's scope ends, for an unrelated local (e.g. an `int` loop
    /// counter's slot later reused for a `long`). The bytecode being resumed
    /// already passed verification, which requires a fresh `store` before any
    /// `load` of a given logical local, so an `Unsupported` slot's CURRENT live
    /// value is always safe to leave untouched. Before this fix, ANY such slot
    /// rejected the entire transfer — discarding real, already-committed OSR
    /// side effects and forcing the interpreter to silently re-execute them
    /// from stale pre-OSR state (the root cause documented in
    /// jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md).
    #[test]
    fn osr_exit_transfer_tolerates_unmappable_local() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(11), FrameValue::Int(22)],
            vec![FrameValue::Int(33)],
            5,
        );

        let advanced = rframe(
            vec![FrameValue::Int(99), FrameValue::Unsupported],
            vec![],
            8,
        );
        let (cm, plan) = osr_plan_for(5, 8);
        assert!(
            transfer_osr_exit_into_live_frame(&shared, &mut thread, 0, &advanced, &cm, &plan)
                .is_some(),
            "an Unsupported LOCAL must not block the transfer"
        );

        let frame = &thread.frames[0];
        // Mappable slot 0 is overwritten from the reconstructed state...
        assert_eq!(frame.get_local(0), Value::Int(99));
        // ...but the Unsupported slot 1 keeps its PRE-transfer live value
        // (it is provably not yet readable in this scope — see doc comment).
        assert_eq!(frame.get_local(1), Value::Int(22));
        assert_eq!(frame.pc, 8);
        assert_eq!(
            frame.stack.len(),
            0,
            "empty snapshot stack replaces the stale one"
        );
    }

    /// An unmappable OPERAND STACK slot (unlike a local) still rejects the whole
    /// transfer and leaves the live frame COMPLETELY untouched: stack values are
    /// transient and about to be consumed, so there is no scope/verification
    /// guarantee protecting a stale or fabricated value the way there is for a
    /// local — the mapping happens before any mutation, so a reject can never
    /// half-write the frame (the caller then safely continues interpreting the
    /// pre-OSR state).
    #[test]
    fn osr_exit_transfer_rejects_unmappable_stack_without_mutating() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(11), FrameValue::Int(22)],
            vec![FrameValue::Int(33)],
            5,
        );

        let bad = rframe(
            vec![FrameValue::Int(99), FrameValue::Int(0)],
            vec![FrameValue::Unsupported],
            8,
        );
        let (cm, plan) = osr_plan_for(5, 8);
        assert!(
            transfer_osr_exit_into_live_frame(&shared, &mut thread, 0, &bad, &cm, &plan).is_none()
        );

        // Frame fully intact.
        let frame = &thread.frames[0];
        assert_eq!(frame.get_local(0), Value::Int(11));
        assert_eq!(frame.get_local(1), Value::Int(22));
        assert_eq!(frame.pc, 5);
        assert_eq!(frame.stack.len(), 1);
        assert_eq!(frame.stack.peek_at(0), Value::Int(33));
    }

    /// A virtual-object slot (never emitted by the OSR-exit snapshot today) rejects
    /// rather than allocate without the materialize pin-dance — the frame is left
    /// untouched.
    #[test]
    fn osr_exit_transfer_rejects_virtual_slot() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(1)],
            vec![],
            0,
        );

        let bad = rframe(vec![vobj(0, 5, vec![FrameValue::Int(1)])], vec![], 0);
        let (cm, plan) = osr_plan_for(0, 0);
        assert!(
            transfer_osr_exit_into_live_frame(&shared, &mut thread, 0, &bad, &cm, &plan).is_none()
        );
        assert_eq!(thread.frames[0].get_local(0), Value::Int(1));
    }

    /// An inlined-caller chain or a held monitor is out of Phase-A scope → reject.
    #[test]
    fn osr_exit_transfer_rejects_out_of_scope() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(1)],
            vec![],
            0,
        );

        let mut inlined = rframe(vec![FrameValue::Int(2)], vec![], 0);
        inlined.caller_frames.push(rframe(vec![], vec![], 0));
        let (cm, plan) = osr_plan_for(0, 0);
        assert!(
            transfer_osr_exit_into_live_frame(&shared, &mut thread, 0, &inlined, &cm, &plan)
                .is_none()
        );
        assert_eq!(thread.frames[0].get_local(0), Value::Int(1));
    }

    // ---------------------------------------------------------------------
    // C2-review — the `MaterializationRequired` refusal, and the validated
    // resume point that replaced the bare `frame.pc = rframe.bci`.
    // ---------------------------------------------------------------------

    /// The refusal reason for `rframe` against a plan whose artifact records a
    /// `REEXECUTE` deopt point at the frame's own bci — i.e. everything except
    /// the slot in question is in order.
    fn transfer_refusal(
        shared: &SharedVm,
        thread: &mut JvmThread,
        rframe: &ReconstructedFrame,
    ) -> String {
        // Cast: bci (u16-range in these fixtures) → usize entry pc.
        let (cm, plan) = osr_plan_for(rframe.bci as usize, rframe.bci);
        transfer_osr_exit_into_live_frame_checked(shared, thread, 0, rframe, &cm, &plan)
            .expect_err("this fixture must refuse")
    }

    /// A `MaterializationRequired` local names ITS OWN cause. Before the guard it
    /// fell through `fv_to_value`'s catch-all `None` and was reported as the
    /// generic "unmappable local" — which is what the variant was split out of
    /// `Unsupported` to stop happening. The refusal itself is not new; the name is.
    #[test]
    fn osr_exit_transfer_names_materialization_required_local() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(11), FrameValue::Int(22)],
            vec![],
            7,
        );

        let deleted = rframe(
            vec![
                FrameValue::Int(99),
                FrameValue::MaterializationRequired(cratonvm_jit::deopt::EliminatedValue::new(
                    7,
                    cratonvm_jit::deopt::EliminationCause::EliminatedStore,
                )),
            ],
            vec![],
            7,
        );
        let why = transfer_refusal(&shared, &mut thread, &deleted);
        assert!(
            why.starts_with("materialization required"),
            "must name the real cause, got {why:?}"
        );
        assert!(
            !why.contains("unmappable local"),
            "must NOT be reported as the generic unmappable-local bail, got {why:?}"
        );
        assert!(
            why.contains("local 1"),
            "must name the offending slot, got {why:?}"
        );

        // Fail closed: the live frame is untouched, exactly as for every other
        // pre-mutation refusal.
        let frame = &thread.frames[0];
        assert_eq!(frame.get_local(0), Value::Int(11));
        assert_eq!(frame.get_local(1), Value::Int(22));
        assert_eq!(frame.pc, 7);
    }

    /// The same marker on the operand STACK is named too — it used to reach the
    /// unrelated "unmappable stack slot" label.
    #[test]
    fn osr_exit_transfer_names_materialization_required_stack_slot() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(1)],
            vec![],
            3,
        );

        let deleted = rframe(
            vec![FrameValue::Int(2)],
            vec![FrameValue::MaterializationRequired(
                cratonvm_jit::deopt::EliminatedValue::unknown(
                    cratonvm_jit::deopt::EliminationCause::Unclassified,
                ),
            )],
            3,
        );
        let why = transfer_refusal(&shared, &mut thread, &deleted);
        assert!(
            why.starts_with("materialization required") && why.contains("stack 0"),
            "must name the stack slot, got {why:?}"
        );
    }

    /// `Unsupported` and `MaterializationRequired` are NOT the same verdict: the
    /// first is tolerated (coarse-classifier noise, the live value stays), the
    /// second refuses. That asymmetry is the entire reason for the split variant.
    #[test]
    fn unsupported_is_tolerated_where_materialization_required_refuses() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(1)],
            vec![],
            4,
        );

        let (cm, plan) = osr_plan_for(4, 4);
        let tolerated = rframe(vec![FrameValue::Int(2), FrameValue::Unsupported], vec![], 4);
        assert!(
            transfer_osr_exit_into_live_frame_checked(
                &shared,
                &mut thread,
                0,
                &tolerated,
                &cm,
                &plan
            )
            .is_ok(),
            "an Unsupported local is coarse-classifier noise, not a deleted value"
        );

        let refused = rframe(
            vec![
                FrameValue::Int(3),
                FrameValue::MaterializationRequired(cratonvm_jit::deopt::EliminatedValue::unknown(
                    cratonvm_jit::deopt::EliminationCause::EliminatedStore,
                )),
            ],
            vec![],
            4,
        );
        assert!(transfer_refusal(&shared, &mut thread, &refused)
            .starts_with("materialization required"));
    }

    /// The resume point comes from `OsrEntryPlan::resume_after_exit`, not from the
    /// raw `rframe.bci`: a bci the artifact never recorded a deopt point for is a
    /// mis-routed stash, and parking the interpreter there is a guess. It refuses,
    /// and the frame is left untouched — the entry gate is what makes this
    /// unreachable after a committed body.
    #[test]
    fn osr_exit_transfer_refuses_a_bci_the_artifact_never_recorded() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(1)],
            vec![],
            2,
        );

        // The artifact's only deopt point is at bci 2; the stash names bci 9.
        let (cm, plan) = osr_plan_for(2, 2);
        let stray = rframe(vec![FrameValue::Int(7)], vec![], 9);
        let why =
            transfer_osr_exit_into_live_frame_checked(&shared, &mut thread, 0, &stray, &cm, &plan)
                .expect_err("a bci from nowhere is not a resume point");
        assert!(why.contains("unresumable exit"), "got {why:?}");

        let frame = &thread.frames[0];
        assert_eq!(frame.pc, 2, "a refused transfer must not move the pc");
        assert_eq!(frame.get_local(0), Value::Int(1), "nor write a local");
    }

    /// A validated transfer parks the frame at the RECONSTRUCTED frame's own bci —
    /// where the OSR'd body actually stopped — and never at the entry pc. Resuming
    /// at the entry pc is the `jit-osr-bail-reruns-loop-iterations` defect: every
    /// iteration the compiled body committed would run a second time.
    #[test]
    fn validated_resume_lands_where_the_body_stopped_not_at_the_entry_pc() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        // Entered at the loop header (bci 2) with i=0; the body committed 50
        // iterations and bailed at bci 9.
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(0)],
            vec![],
            2,
        );

        let cm = cm_with_deopt_point(0, 9);
        let plan = cratonvm_jit::OsrEntryPlan {
            entry_pc: 2,
            resume_bci: 2,
            native_offset: 0,
            dead_mask: 0,
            contract: cratonvm_jit::OsrContractSource::RegisterHomes,
            exit_policy: cratonvm_jit::OsrExitPolicy::ExactTransfer,
            expected_locals: Vec::new(),
        };
        let advanced = rframe(vec![FrameValue::Int(50)], vec![], 9);
        assert!(
            transfer_osr_exit_into_live_frame(&shared, &mut thread, 0, &advanced, &cm, &plan)
                .is_some()
        );

        let frame = &thread.frames[0];
        assert_eq!(
            frame.pc, 9,
            "resume at the bail's own bci, so the committed iterations are not replayed"
        );
        assert_ne!(frame.pc, plan.entry_pc, "never fall back to the entry pc");
        assert_eq!(
            frame.get_local(0),
            Value::Int(50),
            "the JIT-advanced induction variable must survive"
        );
    }

    /// A caller chain no longer refuses for BEING a chain — it refuses for a
    /// named reason, or it is transferred.
    ///
    /// This test used to assert the blanket refusal (`"inlined caller chain"`),
    /// which was the whole rule until 2026-08-18: all three resume sinks
    /// declined a non-empty `caller_frames` outright. Step 2 of the inliner
    /// chain replaced that with `transfer_osr_exit_chain_into_live_frame`, which
    /// writes the outermost scope into the live frame and pushes the rest, so
    /// the refusals below are the specific ones that remain.
    ///
    /// Its original point survives and is asserted last: a
    /// `MaterializationRequired` slot is a per-SLOT verdict that says nothing
    /// about scope depth, so it still refuses on its own reason and is not
    /// masked by anything the chain work added.
    ///
    /// The ACCEPTED path is not exercised here: it needs an inlined callee that
    /// resolves through `MemberResolver`, i.e. a real loaded class, which is
    /// more than this fixture builds. It is covered on the jit side by
    /// `an_inlined_caller_scope_refuses_the_entry_contract_but_not_a_deopt_point`
    /// and per-rule by `a_caller_scope_resumes_after_its_invoke_not_at_it`.
    #[test]
    fn a_caller_chain_refuses_for_a_named_reason_not_for_being_a_chain() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        seed_live_frame(
            &shared,
            &mut thread,
            &cached,
            vec![FrameValue::Int(1)],
            vec![],
            0,
        );

        // The fixture's body is `return`, so bci 0 is not an invoke — which is
        // exactly one of the malformed shapes the transfer must name rather
        // than resume. A caller scope's bci ALWAYS names a call in progress.
        let mut inlined = rframe(vec![FrameValue::Int(2)], vec![], 0);
        inlined.caller_frames.push(rframe(vec![], vec![], 0));
        let why = transfer_refusal(&shared, &mut thread, &inlined);
        assert!(
            why.contains("not an invoke") || why.contains("past the end"),
            "the refusal must name what is wrong with the scope, not that it exists: {why}"
        );
        assert!(
            !why.contains("inlined caller chain"),
            "a chain is no longer refused for being a chain: {why}"
        );

        // Past the depth budget, and the budget is the jit crate's so admission
        // and transfer cannot disagree about it.
        let mut too_deep = rframe(vec![FrameValue::Int(2)], vec![], 0);
        for _ in 0..(MAX_INLINE_RESUME_DEPTH + 1) {
            too_deep.caller_frames.push(rframe(vec![], vec![], 0));
        }
        let why = transfer_refusal(&shared, &mut thread, &too_deep);
        assert!(why.contains("resume budget"), "{why}");

        // The original point: a per-SLOT verdict, unmasked by the chain rules.
        let mut unmaterialisable = rframe(vec![FrameValue::Int(2)], vec![], 0);
        let mut scope = rframe(vec![], vec![], 0);
        scope.locals = vec![FrameValue::MaterializationRequired(
            cratonvm_jit::deopt::EliminatedValue::new(
                7,
                cratonvm_jit::deopt::EliminationCause::EliminatedStore,
            ),
        )];
        unmaterialisable.caller_frames.push(scope);
        let why = transfer_refusal(&shared, &mut thread, &unmaterialisable);
        assert!(
            !why.contains("inlined caller chain"),
            "the chain is not what is wrong here: {why}"
        );
    }
    // ── replay_from_entry_is_observably_equivalent ──────────────────────

    /// netty's `UnpooledHeapByteBuf._getUnsignedMedium`, byte for byte:
    /// `aload_0; getfield #57; iload_1; invokestatic #262; ireturn`.
    ///
    /// Nine bytes, one of which is a call — so the OLD rule ("the body commits
    /// no side effect") refused every replay, and an out-of-bounds read through
    /// the spliced callee raised `InternalError` instead of
    /// `IndexOutOfBoundsException`. The deopt resumes at the invoke, bci 5, and
    /// nothing before bci 5 commits anything.
    const GET_UNSIGNED_MEDIUM: [u8; 9] = [
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x39, // 1: getfield
        0x1b, // 4: iload_1
        0xb8, 0x01, 0x06, // 5: invokestatic  <- the spliced site, the resume bci
        0xac, // 8: ireturn
    ];

    #[test]
    fn a_replay_is_allowed_when_nothing_before_the_resume_point_commits() {
        // The whole body contains a call, so the historical rule refuses.
        assert!(cratonvm_jit::bytecode_commits_side_effect(
            &GET_UNSIGNED_MEDIUM,
            GET_UNSIGNED_MEDIUM.len()
        ));
        // But the abandoned attempt stopped AT the call, and the prefix is
        // three pure loads.
        assert!(replay_from_entry_is_observably_equivalent(
            &GET_UNSIGNED_MEDIUM,
            true,
            5
        ));
    }

    /// The same body, with a spliced callee that could have written something.
    /// The prefix says nothing about the relocated bytecode, so the artifact's
    /// own answer has to veto.
    #[test]
    fn an_impure_spliced_body_vetoes_the_prefix_rule() {
        assert!(!replay_from_entry_is_observably_equivalent(
            &GET_UNSIGNED_MEDIUM,
            false,
            5
        ));
    }

    /// A resume point PAST a side effect is still refused: the attempt already
    /// committed it, and a re-run from entry would do it twice.
    #[test]
    fn a_resume_point_after_a_store_still_refuses() {
        // 0: aload_0  1: iload_1  2: putfield  5: aload_0  6: iload_1
        // 7: invokestatic  10: ireturn
        let code = [
            0x2a, 0x1b, 0xb5, 0x00, 0x01, 0x2a, 0x1b, 0xb8, 0x00, 0x02, 0xac,
        ];
        assert!(
            !replay_from_entry_is_observably_equivalent(&code, true, 7),
            "the putfield at bci 2 is before the resume point and would be \
             duplicated"
        );
        // Resuming at the putfield itself is fine — it had not run.
        assert!(replay_from_entry_is_observably_equivalent(&code, true, 2));
    }

    /// A body that commits nothing anywhere keeps the historical answer,
    /// including for the identity-less `u32::MAX` re-run sentinel that a null
    /// deopt point stashes.
    #[test]
    fn a_pure_body_replays_at_any_resume_point_including_the_sentinel() {
        let pure = [0x2a, 0x1b, 0xac]; // aload_0; iload_1; ireturn
        assert!(replay_from_entry_is_observably_equivalent(&pure, true, 0));
        assert!(replay_from_entry_is_observably_equivalent(
            &pure,
            true,
            u32::MAX
        ));
        assert!(replay_from_entry_is_observably_equivalent(
            &pure,
            false,
            u32::MAX
        ));
        // ...and an impure body with that sentinel is refused, because there is
        // no resume point to reason about.
        assert!(!replay_from_entry_is_observably_equivalent(
            &GET_UNSIGNED_MEDIUM,
            true,
            u32::MAX
        ));
    }

    /// A resume bci past the end of the body is nonsense; refuse rather than
    /// answer from a truncated walk.
    #[test]
    fn an_out_of_range_resume_bci_refuses() {
        assert!(!replay_from_entry_is_observably_equivalent(
            &GET_UNSIGNED_MEDIUM,
            true,
            36
        ));
    }
}
