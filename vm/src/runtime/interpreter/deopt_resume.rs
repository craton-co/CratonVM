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

/// `CRATONVM_DBG_DEOPT` — trace the deopt sinks. Read ONCE.
///
/// The ~20 sites in this file and `jit_bridge.rs` used to each re-read the
/// variable on every deopt (`runtime_var_os`/`runtime_flag_on` are uncached: a
/// declared-name set probe and, for an undeclared name, `std::env::var_os`
/// under the process environment lock), and disagreed about its value rule —
/// this file's `is_some()` read `=0` as ON, `jit_bridge`'s `runtime_flag_on`
/// read it as OFF. One cached predicate, `runtime_flag_on`'s rule.
pub(crate) fn dbg_deopt_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT"))
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
    let trace = dbg_deopt_enabled();
    let bail = |why: &str| -> Option<CachedCallResult> {
        if trace {
            eprintln!(
                "[cratonvm-deopt] FALLBACK re-run {}.{}{} at bci={} ({why})",
                cached.class_name, cached.method_name, cached.method_descriptor, rframe.bci,
            );
        }
        None
    };
    // The identity-less re-run sentinel (`bci == u32::MAX`, no key, no locals)
    // passed the key-only identity gate below and was pushed as a frame with
    // zeroed parameters at pc 4 294 967 295 (r11-tier). Every other sink
    // refuses it; so does this one now. (A bci past `cached`'s code is refused
    // after the chain branch, where `rframe` is known to be `cached`'s own
    // scope rather than an inlined callee's.)
    if rframe.bci == u32::MAX {
        return bail("re-run sentinel");
    }
    // An inlined chain materialises every frame it names, outermost first, and
    // only if EVERY one of them checks out — see `materialise_inlined_chain`.
    // Before 2026-08-18 this sink refused the chain outright and took the
    // whole-method re-run, which is what made an inlined body that publishes
    // deopt metadata unrepresentable and therefore forbidden at the splice.
    //
    // A refusal still falls back to that re-run, so the worst case is exactly
    // the old behaviour.
    if !rframe.caller_frames.is_empty() {
        // Round 13 wave 6 (lane chain2): this sink holds no artifact, so it
        // can neither resolve the scopes by the compile's class ids nor ask
        // whether a spliced callee's class was redefined since the compile
        // (`chain_inner_scope_redefined_since_compile`). Once any class was
        // redefined, leave the chain to the artifact-holding sink the doors
        // try next.
        if crate::classloading::any_class_redefined() {
            return bail("inlined caller chain after a class redefinition");
        }
        // No artifact here to read scope class ids from: the name rule.
        return match materialise_inlined_chain(shared, cached, rframe, &[], &[]) {
            Ok(chain) => push_inlined_chain(shared, thread, DeoptFrameChain::ready(chain), trace),
            Err(why) => bail(&format!("inlined caller chain: {why}")),
        };
    }
    // This sink has no relock path. A monitor the compiled code took itself
    // (`relock == false`) is still held by this thread and the resumed frame's
    // own `monitorexit` releases it, so only an elided one refuses.
    if rframe.monitors.iter().any(|m| m.relock) {
        return bail("held monitors that must be re-acquired");
    }
    // The held ones are recorded in the frame (below), so each must name a real
    // object — `build_deopt_frame_inner`'s `BadMonitor` refusal.
    let mut held: Vec<(ObjectRef, u32)> = Vec::new();
    for m in &rframe.monitors {
        match &m.object {
            cratonvm_jit::deopt::FrameValue::Object(addr) if *addr != 0 => {
                // SAFETY: a non-null reference the stash resolved from the
                // trapped frame; nothing that can collect runs between the take
                // and the push below (the pool refill is Rust memory).
                held.push((unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) }, m.lock_depth));
            }
            _ => return bail("held monitor that is not a resolved object"),
        }
    }
    // The same bound `sink_precise_resume_allowed_for` applies (the padded
    // length; see its doc).
    if rframe.bci as usize >= cached.code.len() {
        return bail("bci past the method's code");
    }
    // Identity gate (r9-vmside) — the one `real_frame_deopt_resume_and_
    // despeculate` has had since the Groovy "duplicate main method" revert. A
    // frame stashed by a nested compiled CALLEE whose sentinel propagated up to
    // this sink carries the callee's locals, stack and bci; building `cached`'s
    // frame out of them is arbitrary misexecution. Only a NON-EMPTY key is
    // judged, the same rule the mismatched-owner arm in
    // `execute_jit_call_decoded` applies, so a key-less synthetic stash keeps
    // the old behaviour. (The chain branch above has its own, stronger check on
    // the outermost scope; there `rframe` is the inlined callee by design.)
    if !rframe.method_key.is_empty()
        && !deopt_frame_matches_method(
            rframe,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
    {
        return bail("stashed frame belongs to a different method");
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
    // Refuse a snapshot that does not fit the frame it is about to become
    // BEFORE a pooled frame is built — the test `materialise_inlined_chain`
    // applies to every chain scope (r9-vmside). The stack half used to surface
    // only as the `frame.stack.push(v).ok()?` below, after the frame had been
    // taken from the pool, and then dropped it. A compacted local list is never
    // longer than the JVM-slot count, so the locals half is conservative.
    if locals.len() > cached.max_locals as usize || stack_vals.len() > cached.max_stack as usize {
        return bail("snapshot does not fit the method's own frame");
    }

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
    // Populate the operand stack and resume pc BEFORE pushing the frame, so
    // EVERY reconstructed oop — locals AND operand-stack refs — is in a
    // GC-scanned frame slot the moment the frame exists. (Review finding: when
    // this push still fired a JVMTI MethodEntry callback, which may allocate, a
    // stack ref held only in the Rust `stack_vals` Vec was unrooted across the
    // fire. A resumed frame no longer fires it — see `push_resumed_frame` — but
    // the ordering stays the safe one.)
    for v in stack_vals {
        frame.stack.push(v).ok()?;
    }
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    frame.pc = rframe.bci as usize;
    // The frame's record of the locks its compiled code took, as
    // `build_deopt_frame_inner` seeds it (interpreter round i1 wave 24, lane L2),
    // and the method monitor a self-locking body handed over as the frame's own
    // `monitor_on_exit`, as there (round 13 wave 9, lane chain4).
    let handed_monitor =
        handed_method_monitor_at(cached.is_synchronized, cached.is_static, &cached.code, rframe);
    for (i, &(obj, depth)) in held.iter().enumerate() {
        if handed_monitor == Some(i) {
            frame.monitor_on_exit = Some(obj);
            continue;
        }
        for _ in 0..depth {
            frame.held_monitors.push(obj);
        }
    }
    if trace {
        eprintln!(
            "[cratonvm-deopt] PRECISE resume {}.{}{} at bci={} locals={:?}",
            cached.class_name, cached.method_name, cached.method_descriptor, rframe.bci, locals,
        );
    }
    // No MethodEntry: the method was entered in compiled code.
    push_resumed_frame(thread, frame);
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

/// Where to park one scope of a reconstructed chain, decided by its
/// [`cratonvm_jit::deopt::ResumeSemantics`] rather than by its position.
///
/// Round 10 wave 9, closing the consumer half of
/// `docs/jit/deopt-frame-state-interning.md` §5.2. The rule this encodes was
/// already the rule — the innermost scope parks at its own bci, a caller scope
/// at its invoke's successor — but it was derived from *where the frame sat in
/// the chain*, in two places, with a comment each. Reading
/// `ReconstructedFrame::semantics` makes the field load-bearing, which is what
/// "the consumer reads the flag" has to mean if a producer that knows better is
/// ever to be able to say so: a genuine post-call resume point in the TRAPPING
/// scope would be parked correctly by this function and was silently
/// re-executed by the position rule.
///
/// Fail-closed on `RETHROW`: such a scope's bci names a throwing instruction
/// that belongs to the exception route (`transfer_osr_exception_exit_into_live_frame`),
/// and there is no resume pc to compute for it at all.
fn scope_resume_pc(
    code: &[u8],
    code_len: usize,
    scope: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Result<usize, String> {
    let semantics = scope.semantics;
    if semantics.rethrow_exception {
        return Err(format!(
            "scope {} at bci {} has RETHROW semantics: its bci names a throwing \
             instruction, not a resume point",
            scope.method_key, scope.bci
        ));
    }
    if semantics.reexecute {
        // Parked AT the bci: the bytecode there has not taken effect.
        let bci = scope.bci as usize;
        if bci >= code_len {
            return Err(format!(
                "scope {} bci {bci} is past the end of a {code_len}-byte method",
                scope.method_key
            ));
        }
        return Ok(bci);
    }
    // `RESUME`: the bytecode at `bci` is already in progress or complete, so the
    // frame parks after it. Today that is always an `invoke`, which is what
    // `caller_resume_pc` refuses to assume of anything else.
    caller_resume_pc(code, code_len, scope.bci as usize)
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
    // A materialised caller frame has no relock path; only a monitor the resume
    // would have to acquire (`relock`) refuses. One the compiled code took is
    // still held by this thread.
    if rf.monitors.iter().any(|m| m.relock) {
        return Err(format!(
            "caller scope {} holds {} monitor(s) that must be re-acquired",
            rf.method_key,
            rf.monitors.iter().filter(|m| m.relock).count()
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
    /// Where to park, decided by the scope's own
    /// [`cratonvm_jit::deopt::ResumeSemantics`] in [`scope_resume_pc`]: an
    /// invoke's successor for a `RESUME` scope (every caller scope), and the
    /// snapshot's own bci for a `REEXECUTE` one (the innermost, trapping
    /// scope).
    pub(super) resume_pc: usize,
    /// The scope's own bci, the pushed frame's `last_instr_pc` (round 13 wave
    /// 6, lane chain2): for a caller scope the `invoke` it is parked in. The
    /// unwinder searches a CALLER frame's exception table at its
    /// `last_instr_pc` (`interpreter::unwind_to_handler`: "the invoke itself,
    /// since `pc` has already advanced"), and a frame built here had never
    /// executed an instruction, so it said 0: an exception the resumed callee
    /// threw found the handlers covering pc 0 instead of the ones covering the
    /// call -- a `catch` around the call missed, or one around pc 0 caught what
    /// it does not cover. Stack traces and JDWP read the same field.
    pub(super) executing_pc: usize,
    /// The locks this scope's compiled code took and still holds (`relock ==
    /// false`), each with its `lock_depth`, outermost first
    /// ([`scope_taken_locks`]): the pushed frame's `held_monitors` record.
    /// Interpreter round i1 wave 25, lane L2. Raw addresses from the snapshot,
    /// exactly as exposed as `locals` until the push roots both.
    pub(super) monitors: Vec<(ObjectRef, u32)>,
}

/// The locks a reconstructed scope's compiled code took (`relock == false`,
/// a non-null object, a non-zero depth), with their depths, in the frame
/// state's order (outermost first). The record a frame rebuilt for this scope
/// starts with — the seeding `build_deopt_frame_inner` does for a single
/// frame, for each frame of an inlined chain (interpreter round i1 wave 25,
/// lane L2). An elided lock (`relock`) is not listed: every chain path refuses
/// one (`caller_frame_values`).
fn scope_taken_locks(rf: &cratonvm_jit::deopt::ReconstructedFrame) -> Vec<(ObjectRef, u32)> {
    use cratonvm_jit::deopt::FrameValue;
    rf.monitors
        .iter()
        .filter(|m| !m.relock && m.lock_depth > 0)
        .filter_map(|m| match &m.object {
            FrameValue::Object(addr) if *addr != 0 => {
                // SAFETY: a non-null reference the stash resolved from the
                // trapped frame, not yet dereferenced; it is only recorded.
                let obj = unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) };
                Some((obj, m.lock_depth))
            }
            _ => None,
        })
        .collect()
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
    resolve_inlined_callee_hinted(shared, enclosing_class_id, method_key, &[])
}

/// [`resolve_inlined_callee`] with the compile's own class ids for the scopes
/// it spliced (`CompiledMethod::splice_scope_class_ids`, round 13 wave 6, lane
/// chain2; `r13w5-resume-chain-scope-class-ids-patch`).
///
/// A scope the compile resolved to one class is looked up by THAT id, not by
/// name from the enclosing class's loader: a body the planner spliced through
/// a substituted owner (an interface site spliced to the profiled concrete
/// class) need not be visible by name there, and a different class of the same
/// name may be, which would rebuild the frame from another class's bytecode.
/// The id must still name a class of the key's name (fail closed otherwise). A
/// key the compile resolved to two different classes is refused rather than
/// guessed. A scope with no row (an artifact without the table, a single-pass
/// chain) keeps the name rule.
pub(super) fn resolve_inlined_callee_hinted(
    shared: &SharedVm,
    enclosing_class_id: ClassId,
    method_key: &str,
    hints: &[(String, u32)],
) -> Result<Arc<CachedBytecodeMethod>, String> {
    use cratonvm_jit::deopt::SpliceScopeClass;
    let Some((rest, desc)) = method_key.rsplit_once(':') else {
        return Err(format!("unparseable method key {method_key:?}"));
    };
    let Some((class_name, method_name)) = rest.rsplit_once('.') else {
        return Err(format!("unparseable method key {method_key:?}"));
    };
    let cm = shared.classes.class_manager.read();
    let cid = match cratonvm_jit::deopt::splice_scope_class_hint(hints, method_key) {
        SpliceScopeClass::Exact(id) => {
            let hinted = ClassId::new(id);
            match cm.class_store().get(hinted) {
                Some(class) if &*class.name == class_name => hinted,
                Some(class) => {
                    return Err(format!(
                        "{method_key}: the compile's class id {id} now names {}",
                        class.name
                    ))
                }
                None => {
                    return Err(format!(
                        "{method_key}: the compile's class id {id} is not loaded"
                    ))
                }
            }
        }
        SpliceScopeClass::Ambiguous => {
            return Err(format!(
                "{method_key} names two different classes in this compile; not resolved by name"
            ))
        }
        SpliceScopeClass::Unknown => {
            let Some(cid) = cm.find_class_by_name_for_class(class_name, enclosing_class_id) else {
                return Err(format!(
                    "{class_name} is not resolvable from the class that inlined it"
                ));
            };
            cid
        }
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
    // The DECLARING class's name, the class of the id and bytecode below
    // (round 13 wave 6, lane chain2). A key may name a subclass the method is
    // inherited through (`invokestatic Sub.helper`, a `final` class's
    // inherited method), and the frame then paired that name with the
    // superclass's id and code: a stack trace, JVMTI and every by-name frame
    // question saw a method the class does not declare.
    let declaring_name: Arc<str> = decl_class.name.clone();
    Ok(Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: declaring_name,
            method_name: Arc::from(method_name),
            method_descriptor: Arc::from(desc),
            source_file: source_file.map(|s| Arc::from(s.as_str())),
            // The per-method memo: one padded body per method, so this frame
            // shares the code pointer the quickening and liveness caches key on.
            code: crate::runtime::frame::padded_bytecode_for_method(
                declaring,
                method_name,
                desc,
                &code_attr.code,
            ),
            exception_table: Arc::from(code_attr.exception_table.as_slice()),
            max_stack: code_attr.max_stack,
            max_locals: code_attr.max_locals,
            // Widening: parameter count fits u16.
            num_params: crate::runtime::interpreter::count_method_params(desc) as u16,
            is_synchronized: method.is_synchronized(),
            is_static: method.is_static(),
        },
    )))
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
///
/// `hints` are the trapping artifact's scope class ids
/// ([`resolve_inlined_callee_hinted`]); `&[]` keeps the name rule for every
/// scope. `own_sources` are the inner scopes' own-source templates
/// ([`chain_inner_scope_own_sources`]); `&[]` resolves every inner scope now.
pub(super) fn materialise_inlined_chain(
    shared: &SharedVm,
    outermost: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
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
    let resume_pc = scope_resume_pc(&outermost.code, code_len, outer_scope)?;
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
        executing_pc: outer_scope.bci as usize,
        monitors: scope_taken_locks(outer_scope),
    });
    out.extend(materialise_inner_scopes(
        shared,
        outermost.declaring_class_id,
        rframe,
        hints,
        own_sources,
    )?);
    Ok(out)
}

/// The own-source template [`chain_inner_scope_own_sources`] chose for the
/// scope named `method_key`, if any (round 14 wave 3, lane chain, R14DP-5).
fn inner_scope_own_source(
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
    method_key: &str,
) -> Option<Arc<CachedBytecodeMethod>> {
    own_sources
        .iter()
        .find(|(key, _)| key == method_key)
        .map(|(_, template)| Arc::clone(template))
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
/// carries that context inward. A scope named in `own_sources` is built from
/// that template instead (round 14 wave 3, lane chain, R14DP-5).
fn materialise_inner_scopes(
    shared: &SharedVm,
    enclosing_class_id: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
) -> Result<Vec<InlinedChainFrame>, String> {
    let depth = rframe.caller_frames.len();
    let mut out: Vec<InlinedChainFrame> = Vec::with_capacity(depth);
    let mut enclosing = enclosing_class_id;
    // `caller_frames` is innermost-first and the outermost is handled by the
    // caller, so this is every scope except the last, walked outermost-inward.
    for scope in rframe.caller_frames.iter().rev().skip(1) {
        let cached = match inner_scope_own_source(own_sources, &scope.method_key) {
            Some(template) => template,
            None => resolve_inlined_callee_hinted(shared, enclosing, &scope.method_key, hints)?,
        };
        let (locals, stack) = caller_frame_values(scope)?;
        let code_len = cached.code.len().saturating_sub(2);
        let resume_pc = scope_resume_pc(&cached.code, code_len, scope)?;
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
            executing_pc: scope.bci as usize,
            monitors: scope_taken_locks(scope),
        });
    }

    // The innermost (trapping) scope. Its bci is a REEXECUTE point in every
    // chain either backend can build — the bytecode there has not taken effect —
    // so it parks AT its own bci rather than at a successor. Asked of the frame
    // rather than assumed: `scope_resume_pc` is the one place that rule lives.
    let innermost = match inner_scope_own_source(own_sources, &rframe.method_key) {
        Some(template) => template,
        None => resolve_inlined_callee_hinted(shared, enclosing, &rframe.method_key, hints)?,
    };
    let (locals, stack) = caller_frame_values(rframe)?;
    let innermost_code_len = innermost.code.len().saturating_sub(2);
    let resume_pc = scope_resume_pc(&innermost.code, innermost_code_len, rframe)?;
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
        resume_pc,
        executing_pc: rframe.bci as usize,
        monitors: scope_taken_locks(rframe),
    });
    Ok(out)
}

/// `CRATONVM_DEOPT_CHAIN_LAST_INSTR_PC`, **default ON** (round 13 wave 6,
/// lane chain2): every frame an inlined chain pushes, and the live frame the
/// OSR-exit chain transfer rewrites, reports its scope's bci as
/// `last_instr_pc` ([`InlinedChainFrame::executing_pc`]). `=0` restores 0 for
/// a pushed frame (and the live frame's stale value), under which an
/// exception the resumed callee throws is matched against the caller's
/// handlers at the wrong pc.
fn chain_frames_report_their_bci() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CHAIN_LAST_INSTR_PC")
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
    mut chain: DeoptFrameChain,
    trace: bool,
) -> Option<CachedCallResult> {
    // Round 13 wave 5 (lane resume): a chain naming scalar-replaced objects
    // gets them now, immediately before the push. A refusal here pushes
    // nothing (a sink that must tell a full heap apart calls
    // `materialise_chain_virtuals` itself first, as
    // `resume_real_ir_deopt_or_throw` does; for the others `None` is their
    // existing no-replay refusal).
    if materialise_chain_virtuals(shared, thread, &mut chain).is_err() {
        return None;
    }
    let pinned_from = chain.virtuals.as_ref().and_then(|v| v.pinned_from);
    let outermost_cp_stamp = chain.outermost_cp_stamp;
    let inner_own_source_frames = std::mem::take(&mut chain.inner_own_source_frames);
    let chain = chain.frames;
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
    // Nothing between these pushes may allocate: while frame 1 is live, frames
    // 2..n exist only as `Value`s in `chain` — Rust memory no root walk scans.
    // (This loop used to fire a MethodEntry per push, whose callback may
    // allocate and so GC, handing the later frames stale references after a
    // moving collection. A resumed frame fires none now — `push_resumed_frame`.)
    let first_pushed = thread.frames.len();
    let report_scope_bci = chain_frames_report_their_bci();
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
        // Stack and pc BEFORE the push, so every reconstructed oop is in a
        // GC-scanned frame slot the moment the frame exists.
        for v in f.stack {
            frame.stack.push(v).ok()?;
        }
        frame.pc = f.resume_pc;
        // Round 13 wave 6 (lane chain2): a caller frame reports the invoke it
        // is parked in, where the unwinder looks up its handlers; see
        // `InlinedChainFrame::executing_pc`.
        if report_scope_bci {
            frame.last_instr_pc = f.executing_pc;
        }
        // The scope's record of the locks its compiled code holds, as the
        // single-frame rebuild seeds it (interpreter round i1 wave 25, lane
        // L2): without it a chain frame's return or unwind while holding one
        // (hand-written bytecode) kept it silently, and the JMX per-frame
        // report attributed it to no frame.
        for &(obj, depth) in &f.monitors {
            for _ in 0..depth {
                frame.held_monitors.push(obj);
            }
        }
        // `push_resumed_frame` minus the diagnostics (run once, below, for the
        // top frame): harvest the retired slot this push overwrites, then push.
        thread.harvest_retired_slot();
        thread.frames.push(frame);
    }
    // No MethodEntry for any of them: every method in the chain was entered in
    // compiled code (the inlined ones inside the outermost's body) — see
    // `push_resumed_frame`. The top frame gets the push-time diagnostics.
    if thread.frames.len() > first_pushed {
        push_diagnostics_after_push(thread);
    }
    // The re-allocated objects and the re-read references are rooted by the
    // pushed frames from here on (`materialise_chain_virtuals`).
    if let Some(base) = pinned_from {
        if thread.native_pin_roots.len() > base {
            thread.native_pin_roots.truncate(base);
        }
    }
    // The outermost frame runs the code of the body's own pool generation
    // (`DeoptFrameChain::with_outermost_cp_stamp`; interpreter round i1 wave
    // 37, lane L3). Nothing while no class was ever redefined.
    if crate::classloading::any_class_redefined() && thread.frames.len() > first_pushed {
        super::obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code(
            shared,
            thread,
            first_pushed,
            outermost_cp_stamp,
        );
        // Round 14 wave 4 (lane resume2, CH3W-3): and every inner frame built
        // from the bytecode the compile spliced, for the sinks that run the
        // chain inside this call (`DeoptFrameChain::inner_own_source_frames`).
        // Outermost first, as the doors' positional restamp goes.
        for k in inner_own_source_frames {
            super::obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code(
                shared,
                thread,
                first_pushed + k,
                outermost_cp_stamp,
            );
        }
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

/// [`verify_reconstructed_oops`] over EVERY scope of an inlined chain, the
/// trapping one and each caller (round 13 wave 5, lane resume). The chain
/// sinks used to check no scope (`build_deopt_frame_chain`) or only the
/// trapping one (the OSR-exit chain transfer), so under
/// `CRATONVM_DEOPT_VERIFY` a caller scope's wild word became a root of a
/// pushed or rewritten frame unchecked.
fn verify_chain_oops(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    shared: &SharedVm,
) -> Result<(), String> {
    verify_reconstructed_oops(rframe, shared)?;
    for (depth, scope) in rframe.caller_frames.iter().enumerate() {
        verify_reconstructed_oops(scope, shared)
            .map_err(|why| format!("caller scope {} ({}): {why}", depth + 1, scope.method_key))?;
    }
    Ok(())
}

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
    /// The stash names an inlined caller chain and the SINK that asked cannot
    /// take one.
    ///
    /// `build_deopt_frame_inner` returns a single `Frame` by value, so a caller
    /// that wants a chain has to ask for one explicitly — `resume_real_ir_deopt`
    /// and `try_resume_trapped_callee` both do, as of N2. This counts the sinks
    /// that do not: today only `interpreter.rs`'s first-call tier-up arm, which
    /// is in a file this lane does not own (see `NOTES-deopt2.md`).
    InlinedChain,
    /// The stash names an inlined caller chain and the chain itself could not
    /// be materialised — the sink asked for one and
    /// [`materialise_inlined_chain`] refused.
    ///
    /// Separate from [`DeoptFrameBail::InlinedChain`] because they say opposite
    /// things about where the work is owed. `InlinedChain` is a sink that has
    /// not been taught; this is a chain the VM cannot rebuild, which is a
    /// producer-side or resolution-side fact (an unmappable slot, a scope whose
    /// callee no longer resolves, a chain past the resume-depth budget). Rolled
    /// into one counter, a reader could not tell "nobody asked" from "we asked
    /// and could not" — the same confusion `string_pin_not_asked` exists for on
    /// the compile side.
    InlinedChainUnmaterialisable,
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
    /// The frame's [`cratonvm_jit::deopt::ResumeSemantics`] say its `bci` is
    /// not a place the interpreter may be parked at.
    ///
    /// Every sink here resumes AT `rframe.bci`, which is only correct for a
    /// `REEXECUTE` point — a guard that fired before the bytecode it protects.
    /// A `RESUME` point's bytecode has already taken effect (parking there
    /// would run it twice) and a `RETHROW` point's `bci` names a throwing
    /// instruction that belongs to the exception route, not to a resume.
    ///
    /// Expected to stay at ZERO, and by two independent arguments rather than
    /// by hope: both trampoline entries route `rethrow_exception` frames to
    /// `LAST_EXCEPTIONAL` before the ordinary stash is reached, and
    /// `ordinary_stash_frame` replaces any remaining non-`REEXECUTE` point with
    /// the `bci == u32::MAX` re-run sentinel (counted separately, in
    /// `cratonvm_jit::deopt::deopt_stash_non_reexecute_refused`). This is the
    /// check the SINK makes for itself instead of trusting a gate two crates
    /// away — round 10 wave 9, closing the consumer half of
    /// `docs/jit/deopt-frame-state-interning.md` §5.2. A non-zero count means a
    /// producer started publishing non-re-execute points and something upstream
    /// stopped filtering them.
    NotAResumePoint,
    /// Re-materialising the scalar-replaced object graph failed because the
    /// heap is exhausted (an allocation that already collected and retried).
    /// Split from [`DeoptFrameBail::VirtualMaterialise`] (round 12 wave 5, lane
    /// replay3) because it is the one refusal no compile-time check can
    /// predict, and it has a spec answer instead of a re-run: HotSpot pops the
    /// deoptimized frame and throws `OutOfMemoryError` to its caller
    /// (`Deoptimization::realloc_objects` failing). A sink that asked for that
    /// answer ([`build_deopt_frame_or_refusal`] with
    /// `release_held_on_heap_failure`) gives it.
    VirtualMaterialiseHeap,
}

impl DeoptFrameBail {
    const ALL: [DeoptFrameBail; 13] = [
        DeoptFrameBail::InlinedChain,
        DeoptFrameBail::InlinedChainUnmaterialisable,
        DeoptFrameBail::IdentityMismatch,
        DeoptFrameBail::SupersededSentinel,
        DeoptFrameBail::VerifyFailed,
        DeoptFrameBail::SynchronizedWithVirtuals,
        DeoptFrameBail::VirtualMaterialise,
        DeoptFrameBail::UnmappableLocal,
        DeoptFrameBail::UnmappableStack,
        DeoptFrameBail::BadMonitor,
        DeoptFrameBail::StackPush,
        DeoptFrameBail::NotAResumePoint,
        DeoptFrameBail::VirtualMaterialiseHeap,
    ];

    /// The census name. Hyphenated and stable: these are grepped out of suite
    /// logs, so renaming one silently breaks whoever is tracking it.
    pub fn name(self) -> &'static str {
        match self {
            DeoptFrameBail::InlinedChain => "inlined-caller-chain",
            DeoptFrameBail::InlinedChainUnmaterialisable => "inlined-caller-chain-unmaterialisable",
            DeoptFrameBail::IdentityMismatch => "identity-mismatch",
            DeoptFrameBail::SupersededSentinel => "superseded-guard-sentinel",
            DeoptFrameBail::VerifyFailed => "deopt-verify-failed",
            DeoptFrameBail::SynchronizedWithVirtuals => "synchronized-with-virtual-objects",
            DeoptFrameBail::VirtualMaterialise => "virtual-object-materialise-failed",
            DeoptFrameBail::UnmappableLocal => "unmappable-local-slot",
            DeoptFrameBail::UnmappableStack => "unmappable-stack-slot",
            DeoptFrameBail::BadMonitor => "held-monitor-not-an-object",
            DeoptFrameBail::StackPush => "operand-stack-did-not-fit",
            DeoptFrameBail::NotAResumePoint => "not-a-resume-point",
            DeoptFrameBail::VirtualMaterialiseHeap => "virtual-object-materialise-heap-exhausted",
        }
    }
}

static DEOPT_FRAME_BAILS: [std::sync::atomic::AtomicU64; 13] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 13];

/// Count one decline, and trace it under `CRATONVM_DBG_DEOPT`.
fn note_deopt_frame_bail(why: DeoptFrameBail, rframe: &cratonvm_jit::deopt::ReconstructedFrame) {
    DEOPT_FRAME_BAILS[why as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if dbg_deopt_enabled() {
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

/// Why a method-entry door (`jit_bridge`'s `jit-callsite-a` / `-b`, the
/// one-shot lambda door, the synchronized template door) re-ran a compiled
/// method FROM ENTRY after its body stopped short of a return (round 12 wave
/// 4, lane replay2; `r12w3-replay-jit-bridge-doors-rerun-without-the-replay-rule`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DoorRerunCause {
    /// The stashed frame was fired by ANOTHER body (a compiled callee whose
    /// trap no call-site service claimed): the attempt stopped inside a call.
    ForeignStash,
    /// The body's own frame, which no resume arm could rebuild.
    OwnStash,
    /// The deopt sentinel with no stashed frame (a frameless trap stub, or a
    /// callee's bare trap): the door does not know where the attempt stopped.
    Frameless,
    /// Not a method-entry door: a compiled CALLER's call-site service re-ran
    /// the callee whose stashed frame `try_resume_trapped_callee` declined
    /// (`helpers::rerun_declined_callee_from_entry`; round 12 wave 5, lane
    /// replay3, `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns`
    /// cause 4). Counted, never retired: re-running only the callee duplicates
    /// strictly less than letting the stash escape to the caller's door. Since
    /// round 13 wave 8 (lane replay4) refused when the verdict is unsound
    /// ([`unsound_rerun_refused`]).
    CalleeDeclined,
    /// The same service for a callee's FRAMELESS trap
    /// (`helpers::service_frameless_callee_trap`), answered by the trap site
    /// the stub recorded ([`frameless_rerun_is_exact`]) over the method the
    /// re-run dispatches to.
    CalleeFrameless,
    /// The dispatch helper's KCFULL-13 arm (`helpers::
    /// route_implicit_exc_through_callee`): an exception escaped a compiled
    /// callee that declares a handler, and `run_jit_callee_handler` could not
    /// resume that handler (no precise locals, an unknown throw pc, or a
    /// callee the handler template does not resolve). The callee re-runs from
    /// entry, which calls the throwing sub-call again. Uncounted before round
    /// 13 wave 8 (lane replay4).
    CalleeException,
    /// Not a re-run: a sink that REFUSED one the replay rule called unsound
    /// and raised `InternalError` instead ([`note_door_rerun_refused`]; round
    /// 13 wave 8, lane replay4). Counted in the FIRST column, since nothing
    /// was committed twice, so the unsound column counts only re-runs that
    /// ran.
    Refused,
}

impl DoorRerunCause {
    fn name(self) -> &'static str {
        match self {
            DoorRerunCause::ForeignStash => "foreign-stash",
            DoorRerunCause::OwnStash => "own-stash",
            DoorRerunCause::Frameless => "frameless",
            DoorRerunCause::CalleeDeclined => "callee-declined",
            DoorRerunCause::CalleeFrameless => "callee-frameless",
            DoorRerunCause::CalleeException => "callee-exception",
            DoorRerunCause::Refused => "refused",
        }
    }
}

/// `[cause * 2 + unsound]`, [`DoorRerunCause`] order.
static DOOR_RERUNS: [std::sync::atomic::AtomicU64; 14] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 14];

/// Count one door re-run. `sound` is the replay rule's answer
/// ([`replay_from_entry_is_observably_equivalent`] and its stash form): `false`
/// means the re-run committed again what the abandoned attempt had committed.
pub(crate) fn note_door_rerun(cause: DoorRerunCause, sound: bool) {
    let i = (cause as usize) * 2 + usize::from(!sound);
    if let Some(c) = DOOR_RERUNS.get(i) {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A sink counted a re-run with [`note_door_rerun`] (as unsound) and then
/// refused it with an `InternalError` ([`unsound_rerun_refused`]): move that
/// count from `cause`'s unsound column to the `refused` row (round 13 wave 8,
/// lane replay4). Nothing ran twice, so the unsound column keeps counting
/// only the double commits that happened.
pub(crate) fn note_door_rerun_refused(cause: DoorRerunCause) {
    use std::sync::atomic::Ordering::Relaxed;
    if let Some(c) = DOOR_RERUNS.get((cause as usize) * 2 + 1) {
        let _ = c.fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1));
    }
    if let Some(c) = DOOR_RERUNS.get((DoorRerunCause::Refused as usize) * 2) {
        c.fetch_add(1, Relaxed);
    }
}

/// `execute`'s tier-up sink's refusal of a FOREIGN-stash or bare-sentinel
/// re-run the replay rule calls unsound ([`unsound_rerun_refused`]; round 13
/// wave 8, lane replay4): counted as refused, and a Java `InternalError` for
/// the call the caller made, as the `jit_bridge` doors raise it.
#[cold]
#[inline(never)]
pub(crate) fn refuse_tierup_sink_rerun(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cause: DoorRerunCause,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    what: &str,
) -> MethodCallFailed {
    note_door_rerun_refused(cause);
    let message = format!(
        "precise deoptimization unavailable for {class_name}.{method_name}{descriptor} {what} \
         (door execute-first-call, {cause:?}); refusing side-effecting replay"
    );
    crate::runtime::exceptions::throw_runtime_error(
        shared,
        thread,
        RuntimeError::InternalError { message },
    )
}

/// `(cause, sound, unsound)` for every [`DoorRerunCause`]. The unsound column
/// is expected to read zero: the compile-time checks
/// (`deopt::first_point_needing_unsound_replay` for the optimizing tier,
/// `deopt::single_pass_first_trap_needing_unsound_replay` for the single-pass
/// one) exist to keep every such re-run out of an installed body, and since
/// round 13 wave 8 every sink refuses what they miss
/// ([`unsound_rerun_refused`], the `refused` row). What is left there by
/// design: a REDEFINED class's re-run (the owner decision recorded on
/// `r12w5-replay3-bytecode-identity-on-the-artifact-patch`), and a kill switch
/// turned off.
pub fn door_rerun_census() -> Vec<(&'static str, u64, u64)> {
    [
        DoorRerunCause::ForeignStash,
        DoorRerunCause::OwnStash,
        DoorRerunCause::Frameless,
        DoorRerunCause::CalleeDeclined,
        DoorRerunCause::CalleeFrameless,
        DoorRerunCause::CalleeException,
        DoorRerunCause::Refused,
    ]
    .iter()
    .map(|&cause| {
        let i = (cause as usize) * 2;
        let load = |j: usize| {
            DOOR_RERUNS
                .get(j)
                .map_or(0, |c| c.load(std::sync::atomic::Ordering::Relaxed))
        };
        (cause.name(), load(i), load(i + 1))
    })
    .collect()
}

/// The replay rule's answer for a door about to re-run `ran`'s method from
/// entry, and why it re-runs (round 12 wave 4, lane replay2).
///
/// `code` is the padded bytecode of the method the door will re-run, and
/// `stash` the frame the body stashed with its point address, `None` for the
/// frameless sentinel. A frame this body did not fire, or one naming another
/// method, says nothing about where THIS attempt stopped, so it is answered
/// with the whole-body rule, as is the frameless sentinel.
///
/// An inlined chain is this body's own when its OUTERMOST scope names the
/// method ([`stash_identity_scope`], the rule of every other stash identity
/// gate since wave 26); it was counted `foreign-stash` before interpreter
/// round i1 wave 28 (lane L2). Its bci is a spliced callee's, so it is still
/// answered with the whole-body rule
/// ([`replay_from_entry_is_observably_equivalent_for_stash`]).
pub(crate) fn door_rerun_verdict(
    code: &[u8],
    ran: &crate::jit::CompiledMethod,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    stash: Option<(&cratonvm_jit::deopt::ReconstructedFrame, usize)>,
) -> (DoorRerunCause, bool) {
    let pure = ran.spliced_bodies_side_effect_free;
    match stash {
        Some((rframe, point))
            if deopt_frame_matches_artifact(
                stash_identity_scope(rframe),
                ran,
                point,
                class_name,
                method_name,
                descriptor,
            ) =>
        {
            (
                DoorRerunCause::OwnStash,
                replay_from_entry_is_observably_equivalent_for_stash(code, ran, rframe, point),
            )
        }
        Some(_) => (
            DoorRerunCause::ForeignStash,
            replay_from_entry_is_observably_equivalent(code, pure, u32::MAX),
        ),
        // Round 12 wave 7 (lane replay4): a body whose own frameless traps
        // all replay exactly (stamped by the single-pass finalizer) re-runs
        // exactly whichever of them fired. Round 13 wave 8 (lane replay4):
        // only when the sentinel came from one of those stubs, which the
        // trap helper now records ([`frameless_rerun_is_exact`]).
        None => (
            DoorRerunCause::Frameless,
            frameless_rerun_is_exact(
                code,
                Some(ran),
                crate::jit::helpers::last_frameless_trap_site(),
            ),
        ),
    }
}

/// The replay rule for a re-run from entry after a BARE sentinel (no stashed
/// frame), from the trap site the thread recorded when the deopt flag was
/// raised (round 13 wave 8, lane replay4).
///
/// `ran` is the body the sink entered, `None` for a call-site service that
/// did not enter it itself. `code` is the bytecode the re-run runs.
///
/// Before this, every bare sentinel was answered with the body's
/// `frameless_traps_replay_exact` stamp, which the single-pass finalizer
/// computes over its own frameless trap STUBS. A bare sentinel has other
/// producers: a helper that could not build its throwable, found no JIT
/// thread, or panicked under `OnPanic::Deopt`, anywhere in the body and after
/// any store; and a callee's stub whose sentinel no service claimed. The
/// stamp said nothing about those, so they were counted exact (and, with the
/// refusal, would have re-run). Now:
///
/// * [`FramelessTrapSite::Stub`] of `ran` itself (same compile id), or of an
///   unnamed body for a service: the trapping body's stamp, read when it
///   trapped (a service that names its method goes through
///   [`frameless_rerun_is_exact_for_callee`], round 13 wave 13, and takes the
///   stamp of its own method's stub only);
/// * `FramelessTrapSite::Dropped`, a callee frame the call-site service's
///   resume dropped after judging it at its own bci: that verdict, for the
///   service only;
/// * a stub of ANOTHER body, or [`FramelessTrapSite::Helper`]: the whole-body
///   rule, which knows nothing of where the attempt stopped;
/// * [`FramelessTrapSite::Unknown`] (a stub with no compile id, or nothing
///   recorded on this thread): the old rule.
///
/// `CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY=0` restores the old rule everywhere.
///
/// [`FramelessTrapSite::Stub`]: crate::jit::helpers::FramelessTrapSite::Stub
/// [`FramelessTrapSite::Helper`]: crate::jit::helpers::FramelessTrapSite::Helper
/// [`FramelessTrapSite::Unknown`]: crate::jit::helpers::FramelessTrapSite::Unknown
pub(crate) fn frameless_rerun_is_exact(
    code: &[u8],
    ran: Option<&crate::jit::CompiledMethod>,
    site: crate::jit::helpers::FramelessTrapSite,
) -> bool {
    frameless_rerun_is_exact_of(code, ran, site, None)
}

/// [`frameless_rerun_is_exact`] for a call-site service that knows WHICH
/// method it re-runs (round 13 wave 13, lane jitfix;
/// `r13w12-replay6-callee-service-trusts-any-stubs-exact-stamp`): `rerun_method`
/// is `helpers::frameless_trap_method_hash` of the callee it resolved. A stub
/// stamped for another method answers the whole-body rule instead of its own
/// `exact`, which is a statement about re-running the method that trapped. A
/// stub whose method did not parse (`0`) is trusted as before, and so is every
/// stub under `CRATONVM_JIT_FRAMELESS_TRAP_METHOD_IDENTITY=0` (default on).
pub(crate) fn frameless_rerun_is_exact_for_callee(
    code: &[u8],
    site: crate::jit::helpers::FramelessTrapSite,
    rerun_method: u64,
) -> bool {
    let rerun_method = cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_FRAMELESS_TRAP_METHOD_IDENTITY",
    )
    .then_some(rerun_method);
    frameless_rerun_is_exact_of(code, None, site, rerun_method)
}

/// The one body of [`frameless_rerun_is_exact`] and
/// [`frameless_rerun_is_exact_for_callee`].
fn frameless_rerun_is_exact_of(
    code: &[u8],
    ran: Option<&crate::jit::CompiledMethod>,
    site: crate::jit::helpers::FramelessTrapSite,
    rerun_method: Option<u64>,
) -> bool {
    use crate::jit::helpers::FramelessTrapSite;
    let pure = ran.is_some_and(|r| r.spliced_bodies_side_effect_free);
    let stamped = ran.is_some_and(|r| r.frameless_traps_replay_exact);
    let whole_body = || replay_from_entry_is_observably_equivalent(code, pure, u32::MAX);
    if !frameless_trap_identity_enabled() {
        return stamped || whole_body();
    }
    match site {
        FramelessTrapSite::Stub {
            compile_id,
            exact,
            method,
            ..
        } if ran.map_or(true, |r| r.compile_id == compile_id)
            && rerun_method.map_or(true, |m| method == 0 || method == m) =>
        {
            exact || whole_body()
        }
        // A callee frame the call-site service's resume dropped, judged at
        // its own bci: only that service (which entered no body) reads it.
        FramelessTrapSite::Dropped { exact } if ran.is_none() => exact || whole_body(),
        // gce e1/f: an owned-arguments hand-off, whose caller re-run replays
        // the callee's prefix alone; any sink re-running the caller reads it.
        FramelessTrapSite::Handoff { exact } => exact || whole_body(),
        FramelessTrapSite::Stub { .. }
        | FramelessTrapSite::Dropped { .. }
        | FramelessTrapSite::Helper => whole_body(),
        FramelessTrapSite::Unknown => stamped || whole_body(),
    }
}

/// `CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY`, **default ON**: whether
/// [`frameless_rerun_is_exact`] reads the recorded trap site (round 13 wave
/// 8, lane replay4). Read only on a re-run from entry.
fn frameless_trap_identity_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY")
}

/// ONE answer to "own stash, no resume, replay unsound" (round 13 wave 3,
/// lane replay2; proposal R13-2): must a sink that could not resume a trapped
/// body refuse the re-run from entry [`door_rerun_verdict`] judged, with an
/// `InternalError`? Only an [`DoorRerunCause::OwnStash`] re-run the replay rule
/// calls unsound, with `CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES` on (default),
/// of a class never redefined (`redefined` is asked last: it may take the
/// class-manager lock). A foreign stash and the frameless sentinel re-run:
/// their verdict is the whole-body rule, which knows nothing of where this
/// attempt stopped. A redefined class re-runs because the resume it could not
/// get is the bytecode-identity one
/// (`r12w5-replay3-bytecode-identity-on-the-artifact-patch`).
///
/// Since round 13 wave 8 every sink asks [`unsound_rerun_refused`], whose
/// own-stash arm this is.
pub(crate) fn unsound_own_stash_rerun_refused(
    cause: DoorRerunCause,
    sound: bool,
    redefined: impl FnOnce() -> bool,
) -> bool {
    !sound
        && cause == DoorRerunCause::OwnStash
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES")
        && !redefined()
}

/// [`unsound_own_stash_rerun_refused`] for EVERY cause (round 13 wave 8, lane
/// replay4): must a sink refuse, with an `InternalError`, a re-run from entry
/// the replay rule calls unsound? Asked by every sink that re-runs: the
/// `jit_bridge` doors, `execute`'s tier-up sink, the lambda direct arm and
/// the call-site services in `helpers.rs`.
///
/// A re-run the rule calls unsound commits again what the abandoned attempt
/// committed, and none of these sinks can resume the attempt precisely any
/// more, so the loud answer is the only one that is not a silent wrong one.
/// Before this wave only an own stash was refused; a foreign stash, a bare
/// sentinel and every callee service re-ran, counted in the census's unsound
/// column. The compile-time checks keep an installed body out of all of them,
/// so each refusal names a hole (`CRATONVM_DBG_DEOPT=1` prints the sink and
/// the method).
///
/// Still re-run: a sound verdict; a REDEFINED class (`redefined`, asked last:
/// the owner decision that a hot swap keeps its re-run); and each family
/// whose switch is off:
///
/// * own stash: `CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES` (round 12 wave 7);
/// * foreign stash and bare sentinel at a method-entry sink:
///   `CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES`;
/// * the call-site services' declined stash and bare sentinel:
///   `CRATONVM_JIT_CALLEE_UNSOUND_RERUN_RAISES`;
/// * the KCFULL-13 re-run of a callee whose handler could not be resumed:
///   `CRATONVM_JIT_CALLEE_EXCEPTION_RERUN_RAISES`.
///
/// All default ON.
pub(crate) fn unsound_rerun_refused(
    cause: DoorRerunCause,
    sound: bool,
    redefined: impl FnOnce() -> bool,
) -> bool {
    use cratonvm_types::flags::runtime_flag_default_on;
    if sound {
        return false;
    }
    let switch_on = match cause {
        DoorRerunCause::OwnStash => {
            return unsound_own_stash_rerun_refused(cause, sound, redefined);
        }
        DoorRerunCause::ForeignStash | DoorRerunCause::Frameless => {
            runtime_flag_default_on("CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES")
        }
        DoorRerunCause::CalleeDeclined | DoorRerunCause::CalleeFrameless => {
            runtime_flag_default_on("CRATONVM_JIT_CALLEE_UNSOUND_RERUN_RAISES")
        }
        DoorRerunCause::CalleeException => {
            runtime_flag_default_on("CRATONVM_JIT_CALLEE_EXCEPTION_RERUN_RAISES")
        }
        DoorRerunCause::Refused => false,
    };
    switch_on && !redefined()
}

/// Materialise an inlined caller chain for a sink that can take one — N2.
///
/// # Why this exists, and why it is separate from `build_deopt_frame_inner`
///
/// `jit/src/x64/deopt_stubs.rs` sets `frame_state.caller = self
/// .inline_caller_chain()`, and until this landed EVERY VM-side rebuild sink
/// refused a non-empty chain outright. So the one producer of owned caller
/// chains fed only consumers that could not read them: a trap inside a splice
/// took the `None` path into the side-effect check and, for any method with
/// stores, raised `InternalError: precise deoptimization unavailable … refusing
/// side-effecting replay`.
///
/// That is not live today and this lane's predecessor made that a guarantee
/// rather than an accident: `jit/src/x64/inlining.rs`'s splice postcondition
/// rolls back any splice that published a point, so no artifact carries a
/// chain. The refusal becomes acute the moment that postcondition is relaxed,
/// which is why this half lands FIRST — see N5 in `NOTES-deopt.md` and the
/// `NOTES-deopt2.md` entry for the `jit/src/x64.rs` half that must precede it.
///
/// It is a separate function rather than a branch inside
/// `build_deopt_frame_inner` because that function returns a single `Frame` by
/// value, and a chain is N frames that must be pushed outermost-first. A sink
/// therefore has to *opt in*, and the sinks that have not are counted as
/// [`DeoptFrameBail::InlinedChain`] rather than silently reading as "no chains
/// occur".
///
/// Returns the chain outermost-first, ready for [`push_inlined_chain`], or
/// `None` after counting the refusal. **Nothing is pushed and nothing is
/// pinned** — every check that can refuse has already run, so a caller holding
/// a `Some` still has the whole-method re-run available and a caller holding a
/// `None` has lost nothing.
///
/// Test-only since round 13 wave 8: every production sink holds its stashed
/// point and calls [`build_deopt_frame_chain_for_point`].
#[cfg(test)]
pub(crate) fn build_deopt_frame_chain(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<DeoptFrameChain> {
    build_deopt_frame_chain_for_point(shared, cached, rframe, 0)
}

/// [`build_deopt_frame_chain`] for a sink that holds the stashed point's
/// address but not the artifact that fired it -- the call-site service
/// (`helpers::try_resume_trapped_callee`, whose `stashed_point` it is).
/// Round 13 wave 8, lane chain3.
///
/// The body that owns `point_addr` is looked up among `cached`'s published
/// ones (method-entry, then OSR). Found, the chain is built from ITS scope
/// class ids ([`build_deopt_frame_chain_hinted`]) and refused when a scope's
/// class was redefined since its compile
/// ([`chain_inner_scope_redefined_since_compile`],
/// [`chain_outer_scope_redefined_since_compile`]) -- the two things the
/// artifact-holding sinks ask. Without it (point 0, or a body the cache no
/// longer publishes) the scopes resolve by name as before, and once any class
/// of this VM was redefined the chain is refused: nothing then says whether a
/// scope's bytecode is the one its bci describes (the rule
/// `resume_from_ir_deopt` applies for the same reason). Before, this sink
/// asked neither, so a spliced body reached through an owner the enclosing
/// class's loader cannot name (`HashMap` splicing an application key's
/// `hashCode`) was refused, and after a redefinition a scope could be rebuilt
/// from the new bytecode at the old bci. Kill switch
/// `CRATONVM_DEOPT_CHAIN_CALLSITE_BY_POINT=0` restores the name rule with no
/// redefinition check. Since round 14 wave 4 (lane resume2, CH3W-3) a stale
/// chain whose body is found is rebuilt from the bytecode that body ran
/// ([`build_stale_chain_from_own_sources`]) before it is refused.
pub(crate) fn build_deopt_frame_chain_for_point(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    point_addr: usize,
) -> Option<DeoptFrameChain> {
    if rframe.caller_frames.is_empty()
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CHAIN_CALLSITE_BY_POINT")
    {
        return build_deopt_frame_chain_hinted(shared, cached, rframe, &[]);
    }
    let mut trapped = None;
    if point_addr != 0 {
        let cache = shared.jit.jit_cache.read();
        let entry = cache.get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        );
        let osr = cache.get_osr(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        );
        trapped = entry
            .filter(|body| body.owns_deopt_point(point_addr))
            .or_else(|| osr.filter(|body| body.owns_deopt_point(point_addr)));
    }
    let refused = match &trapped {
        Some(body) => {
            chain_inner_scope_redefined_since_compile(
                shared,
                body,
                cached.declaring_class_id,
                rframe,
            ) || chain_outer_scope_redefined_since_compile(
                shared,
                body,
                cached.declaring_class_id,
                rframe,
            )
        }
        None => crate::classloading::any_class_redefined(),
    };
    // Round 14 wave 4 (lane resume2, CH3W-3): a stale scope of a chain whose
    // body is known is rebuilt from the bytecode that body ran, as the doors
    // and the tier-up sink rebuild it ([`build_stale_chain_from_own_sources`]);
    // only a scope with no usable template keeps the refusal. Kill switch
    // `CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS=0` restores the refusal.
    if refused {
        if let Some(body) = &trapped {
            if chain_own_source_sinks_enabled() {
                if let Some(chain) = build_stale_chain_from_own_sources(
                    shared,
                    body,
                    cached,
                    rframe,
                    &body.splice_scope_class_ids,
                ) {
                    return Some(chain);
                }
            }
        }
    }
    if refused {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] inlined chain not materialisable: stash={} bci={} (a class \
                 was redefined and the chain's bytecode cannot be vouched for)",
                rframe.method_key, rframe.bci,
            );
        }
        note_deopt_frame_bail(DeoptFrameBail::InlinedChainUnmaterialisable, rframe);
        return None;
    }
    match &trapped {
        Some(body) => {
            build_deopt_frame_chain_hinted(shared, cached, rframe, &body.splice_scope_class_ids)
        }
        None => build_deopt_frame_chain_hinted(shared, cached, rframe, &[]),
    }
}

/// [`build_deopt_frame_chain`] for a sink that holds the trapping artifact:
/// `hints` is its `CompiledMethod::splice_scope_class_ids`, so every inner
/// scope is resolved by the class id the compile spliced it from
/// ([`resolve_inlined_callee_hinted`]; round 13 wave 6, lane chain2).
pub(crate) fn build_deopt_frame_chain_hinted(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
) -> Option<DeoptFrameChain> {
    build_deopt_frame_chain_sourced(shared, cached, rframe, hints, &[])
}

/// [`build_deopt_frame_chain_hinted`] with the inner scopes' own-source
/// templates ([`chain_inner_scope_own_sources`]; round 14 wave 3, lane chain,
/// R14DP-5). The doors, the first-call tier-up sink and the call-site service
/// by point pass them (round 14 wave 4, lane resume2, CH3W-3); the chain
/// records which of its frames came from one
/// (`DeoptFrameChain::inner_own_source_frames`).
pub(super) fn build_deopt_frame_chain_sourced(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
) -> Option<DeoptFrameChain> {
    // The superseded-guard sentinel, checked HERE rather than left to the
    // materialiser. `scope_resume_pc` does now bound the trapping scope's bci
    // against its own method's code (round 10 wave 9 — it used to take
    // `rframe.bci` verbatim while the caller scopes went through
    // `caller_resume_pc`, which always bounded them), so `u32::MAX` would be
    // refused there too. This stays because it names the shape: a superseded
    // guard is not an out-of-range bci, it is a frame that was never meant to
    // resume, and the census must say so. The single-frame builder has the same
    // check for the same reason.
    if rframe.bci == u32::MAX {
        note_deopt_frame_bail(DeoptFrameBail::SupersededSentinel, rframe);
        return None;
    }
    // The TRAPPING scope is parked at its own bci by `materialise_inner_scopes`,
    // so it has to be a re-execute point for the same reason the single-frame
    // builder's is. The caller scopes are not: each is parked at its invoke's
    // SUCCESSOR by `caller_resume_pc`, which is what their
    // `ResumeSemantics::for_caller_scope()` (`RESUME`) means, and asserting
    // `reexecute` of them would refuse every chain there is.
    if !rframe.semantics.reexecute {
        note_deopt_frame_bail(DeoptFrameBail::NotAResumePoint, rframe);
        return None;
    }
    // `CRATONVM_DEOPT_VERIFY`, as the single-frame builder asks it: every
    // scope's reference words (round 13 wave 5, lane resume; this sink
    // checked none).
    if cratonvm_jit::deopt_verify_enabled() {
        if let Err(why) = verify_chain_oops(rframe, shared) {
            eprintln!(
                "[DEOPT-VERIFY] {} bci={}: inlined chain invariant violated: {why} \
                 — forcing safe re-run",
                rframe.method_key, rframe.bci
            );
            note_deopt_frame_bail(DeoptFrameBail::VerifyFailed, rframe);
            return None;
        }
    }
    // Round 13 wave 5 (lane resume): a chain naming a scalar-replaced object
    // is validated now and re-allocated at the push ([`ChainVirtuals`]), under
    // the switch the producer reads too. Without it such a chain is refused
    // below as before (`caller_frame_values` maps no virtual slot).
    let built = if reconstructed_chain_names_virtual(rframe)
        && cratonvm_jit::deopt::chain_virtuals_enabled()
    {
        chain_with_virtuals(shared, cached, rframe, hints, own_sources)
    } else {
        materialise_inlined_chain(shared, cached, rframe, hints, own_sources)
            .map(DeoptFrameChain::ready)
    };
    match built {
        Ok(mut chain) => {
            // Frame `k + 1` of the chain is the `k`-th inner scope,
            // outermost-inward (the order `materialise_inner_scopes` builds).
            if !own_sources.is_empty() {
                chain.inner_own_source_frames = rframe
                    .caller_frames
                    .iter()
                    .rev()
                    .skip(1)
                    .chain(std::iter::once(rframe))
                    .enumerate()
                    .filter(|(_, scope)| {
                        own_sources.iter().any(|(key, _)| *key == scope.method_key)
                    })
                    .map(|(k, _)| k + 1)
                    .collect();
            }
            Some(chain)
        }
        Err(why) => {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] inlined chain not materialisable: stash={} bci={} ({why})",
                    rframe.method_key, rframe.bci,
                );
            }
            note_deopt_frame_bail(DeoptFrameBail::InlinedChainUnmaterialisable, rframe);
            None
        }
    }
}

/// A materialised inlined caller chain, outermost first, ready to push.
///
/// Opaque on purpose. The frames inside carry pooled buffers and a push order
/// that is load-bearing, and the one sink outside this module that handles a
/// chain (`vm/src/jit/helpers.rs`) has no business reaching into either — it
/// gets a value it can only hand back to [`run_deopt_frame_chain_to_completion`].
/// That also keeps [`InlinedChainFrame`] module-private, so the GC-rooting
/// argument in [`push_inlined_chain`] stays the only place frames are pushed.
pub(crate) struct DeoptFrameChain {
    frames: Vec<InlinedChainFrame>,
    /// The chain's scalar-replaced objects, still to be re-allocated before the
    /// push ([`materialise_chain_virtuals`]); `None` for a chain that names
    /// none, whose `frames` are final. Round 13 wave 5, lane resume.
    virtuals: Option<ChainVirtuals>,
    /// The trapping body's constant-pool generation
    /// (`CompiledMethod::compile_cp_stamp`), which the OUTERMOST frame is
    /// restamped with once pushed ([`push_inlined_chain`]); `None` when the
    /// sink did not say (interpreter round i1 wave 37, lane L3).
    outermost_cp_stamp: Option<u64>,
    /// Indices within `frames` (outermost = 0) of the inner frames built from
    /// an own-source template ([`chain_inner_scope_own_sources`]), which
    /// [`push_inlined_chain`] restamps with `outermost_cp_stamp` too -- the
    /// sinks that cannot restamp positionally after the push (the tier-up
    /// sink and the call-site service run the chain to completion inside
    /// it). The doors leave `outermost_cp_stamp` `None`, so this is inert for
    /// them and they keep [`restamp_inner_own_source_frames`]. Round 14 wave 4,
    /// lane resume2 (CH3W-3).
    inner_own_source_frames: Vec<usize>,
}

impl DeoptFrameChain {
    /// A chain whose frames are final: it names no scalar-replaced object.
    fn ready(frames: Vec<InlinedChainFrame>) -> Self {
        Self {
            frames,
            virtuals: None,
            outermost_cp_stamp: None,
            inner_own_source_frames: Vec::new(),
        }
    }

    /// This chain, from a body compiled at constant-pool generation `stamp`
    /// (`CompiledMethod::compile_cp_stamp`): its outermost frame runs the
    /// bytecode the entering door held, which a redefinition of its class may
    /// have replaced while the body ran, so [`push_inlined_chain`] restamps
    /// it with `stamp` and moves it onto its translated body before it runs
    /// (`obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code`; the
    /// single-frame sinks have done this since wave 28). The inner frames
    /// resolved from their classes now are current; an inner frame built from
    /// an own-source template (a spliced callee's class redefined since the
    /// compile, [`chain_inner_scope_own_sources`]) is restamped with `stamp`
    /// as well (round 14 wave 4, lane resume2). Interpreter round i1 wave 37,
    /// lane L3.
    pub(crate) fn with_outermost_cp_stamp(mut self, stamp: Option<u64>) -> Self {
        self.outermost_cp_stamp = stamp;
        self
    }

    /// How many frames the chain will push. Diagnostics only.
    ///
    /// Not named `len`: a chain is never empty (it always carries at least the
    /// outermost scope and the trapping one), so `clippy::len_without_is_empty`
    /// would ask for an `is_empty` that could only ever answer `false`.
    pub(crate) fn frame_count(&self) -> usize {
        self.frames.len()
    }
}

/// A chain's scalar-replaced objects, as one frame the materialiser can take
/// (round 13 wave 5, lane resume; `r13w3-framestate-vm-chain-resume-gaps`
/// item 1).
///
/// Every scope's locals and operand stack, outermost scope first, are
/// concatenated into ONE `ReconstructedFrame`, because the producer numbers
/// virtual objects CHAIN-wide: `ir_lower::splice_chain_frame_state` resolves
/// the whole chain with one `emitted` set, so an object two scopes name (the
/// caller's local and the spliced callee's argument) is DEFINED in one scope
/// and REFERENCED from the other. Materialising scope by scope would find a
/// dangling reference, or, worse, give the two frames two different objects.
/// One call also keeps the GC argument of the single frame: the materialiser
/// pins every ordinary reference of the merged frame before its first
/// allocation and re-reads them after its last, so after it returns every
/// `Object` word of every scope is current, and nothing allocates again
/// before the push.
struct ChainVirtuals {
    merged: cratonvm_jit::deopt::ReconstructedFrame,
    /// `(locals, stack)` value counts of each scope in `merged`, in push order
    /// (the same order as [`DeoptFrameChain::frames`]).
    shapes: Vec<(usize, usize)>,
    /// `native_pin_roots` length before the re-allocation, once it ran: the
    /// shells and re-read references stay pinned above it until the chain is
    /// pushed ([`push_inlined_chain`] releases them).
    pinned_from: Option<usize>,
}

/// Does any scope of `rframe`'s chain (locals, stack or a monitor object)
/// name a scalar-replaced object?
fn reconstructed_chain_names_virtual(rframe: &cratonvm_jit::deopt::ReconstructedFrame) -> bool {
    use cratonvm_jit::deopt::FrameValue;
    let names = |f: &cratonvm_jit::deopt::ReconstructedFrame| {
        f.locals
            .iter()
            .chain(f.stack.iter())
            .chain(f.monitors.iter().map(|m| &m.object))
            .any(|v| matches!(v, FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)))
    };
    names(rframe) || rframe.caller_frames.iter().any(names)
}

/// Why the VM will not re-allocate `rframe`'s chain-held scalar-replaced
/// objects for `outermost`, before anything is resolved or allocated:
///
/// * a held monitor in any scope. A re-allocation that fails after a
///   collection abandons the activation, and the chain sinks have no path
///   that releases the compiled code's locks for it (the single-frame builder
///   has one, `release_held_on_heap_failure`); refusing costs the chain, not
///   a lock. The producer refuses the same chain
///   (`cratonvm_jit::deopt::inlined_chain_refusal`).
/// * an `ACC_SYNCHRONIZED` outermost method: its monitor is the door's, and
///   the compile-time replay check does not take such a chain as resumed
///   (`point_needs_unsound_replay_of_method`), so neither end relies on it.
fn chain_virtuals_refusal(
    outermost: &CachedBytecodeMethod,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<&'static str> {
    if outermost.is_synchronized {
        return Some("a synchronized method's chain names a scalar-replaced object");
    }
    if !rframe.monitors.is_empty() || rframe.caller_frames.iter().any(|c| !c.monitors.is_empty()) {
        return Some("a chain naming a scalar-replaced object holds a monitor");
    }
    None
}

/// `rframe` with every top-level scalar-replaced slot of every scope replaced
/// by `null`: the chain [`materialise_inlined_chain`] validates (resolution,
/// resume pcs, frame fit) before anything allocates. The placeholders never
/// reach a frame: [`materialise_chain_virtuals`] rebuilds every scope's values.
fn with_virtual_placeholders(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> cratonvm_jit::deopt::ReconstructedFrame {
    use cratonvm_jit::deopt::FrameValue;
    fn blank(values: &mut [FrameValue]) {
        for v in values.iter_mut() {
            if matches!(v, FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)) {
                *v = FrameValue::Object(0);
            }
        }
    }
    let mut out = rframe.clone();
    blank(&mut out.locals);
    blank(&mut out.stack);
    for c in out.caller_frames.iter_mut() {
        blank(&mut c.locals);
        blank(&mut c.stack);
    }
    out
}

/// [`ChainVirtuals`] of `rframe`: its scopes, outermost first (push order),
/// concatenated.
fn merge_chain_scopes(rframe: &cratonvm_jit::deopt::ReconstructedFrame) -> ChainVirtuals {
    let mut merged = cratonvm_jit::deopt::ReconstructedFrame {
        method_key: rframe.method_key.clone(),
        bci: rframe.bci,
        semantics: rframe.semantics,
        ..Default::default()
    };
    let mut shapes = Vec::with_capacity(rframe.caller_frames.len() + 1);
    for scope in rframe
        .caller_frames
        .iter()
        .rev()
        .chain(std::iter::once(rframe))
    {
        merged.locals.extend(scope.locals.iter().cloned());
        merged.stack.extend(scope.stack.iter().cloned());
        shapes.push((scope.locals.len(), scope.stack.len()));
    }
    ChainVirtuals {
        merged,
        shapes,
        pinned_from: None,
    }
}

/// [`materialise_inlined_chain`] for a chain that names scalar-replaced
/// objects: every check that can refuse runs now, over placeholders; the
/// objects are re-allocated by [`materialise_chain_virtuals`] right before
/// the push. Nothing is pinned or allocated here.
fn chain_with_virtuals(
    shared: &SharedVm,
    outermost: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
) -> Result<DeoptFrameChain, String> {
    if let Some(why) = chain_virtuals_refusal(outermost, rframe) {
        return Err(why.to_string());
    }
    let frames = materialise_inlined_chain(
        shared,
        outermost,
        &with_virtual_placeholders(rframe),
        hints,
        own_sources,
    )?;
    let virtuals = merge_chain_scopes(rframe);
    if virtuals.shapes.len() != frames.len() {
        return Err(format!(
            "{} chain frames for {} scopes",
            frames.len(),
            virtuals.shapes.len()
        ));
    }
    Ok(DeoptFrameChain {
        frames,
        virtuals: Some(virtuals),
        outermost_cp_stamp: None,
        inner_own_source_frames: Vec::new(),
    })
}

/// Re-allocate `chain`'s scalar-replaced objects and rebuild every frame's
/// values from the result (round 13 wave 5, lane resume). A no-op for a chain
/// that names none, or whose objects were already re-allocated.
///
/// One materialiser call over the merged frame ([`ChainVirtuals`]): it pins
/// every ordinary reference of every scope before its first allocation,
/// allocates one shell per object id, re-reads the references, fills the
/// fields and rewrites every virtual slot. Its pins stay installed
/// (`keep_pins`) and are recorded in `pinned_from`; the caller must push the
/// chain with NOTHING that can allocate in between ([`push_inlined_chain`]
/// releases them after its last push). On `Err` nothing is left pinned and
/// nothing was pushed; `DeoptFrameBail::VirtualMaterialiseHeap` means the
/// heap is exhausted and a collection ran, so the stash's own `Object` words
/// may be stale and must not be restashed.
pub(crate) fn materialise_chain_virtuals(
    shared: &SharedVm,
    thread: &mut JvmThread,
    chain: &mut DeoptFrameChain,
) -> Result<(), DeoptFrameBail> {
    let Some(virtuals) = chain.virtuals.as_mut() else {
        return Ok(());
    };
    if virtuals.pinned_from.is_some() {
        return Ok(());
    }
    let base = thread.native_pin_roots.len();
    if let Err(failed) = crate::runtime::deopt_materialize::materialize_virtual_objects(
        shared,
        thread,
        &mut virtuals.merged,
        /* stress_gc */ false,
        /* keep_pins */ true,
    ) {
        thread.native_pin_roots.truncate(base);
        let why = if materialisation_failed_for_heap(&failed) {
            DeoptFrameBail::VirtualMaterialiseHeap
        } else {
            DeoptFrameBail::VirtualMaterialise
        };
        note_deopt_frame_bail(why, &virtuals.merged);
        return Err(why);
    }
    // No collection can run from here to the push: the slicing and mapping
    // below are Rust-side only.
    let (mut l, mut s) = (0usize, 0usize);
    for (frame, &(nl, ns)) in chain.frames.iter_mut().zip(virtuals.shapes.iter()) {
        let mapped = match (
            virtuals.merged.locals.get(l..l + nl),
            virtuals.merged.stack.get(s..s + ns),
        ) {
            (Some(locals), Some(stack)) => {
                caller_frame_values(&cratonvm_jit::deopt::ReconstructedFrame {
                    method_key: frame.cached.method_name.to_string(),
                    locals: locals.to_vec(),
                    stack: stack.to_vec(),
                    ..Default::default()
                })
                .ok()
            }
            _ => None,
        };
        let Some((locals, stack)) = mapped else {
            // Unreachable: the placeholders mapped, and the materialiser only
            // turns virtual slots into `Object` words.
            thread.native_pin_roots.truncate(base);
            note_deopt_frame_bail(DeoptFrameBail::VirtualMaterialise, &virtuals.merged);
            return Err(DeoptFrameBail::VirtualMaterialise);
        };
        frame.locals = locals;
        frame.stack = stack;
        l += nl;
        s += ns;
    }
    virtuals.pinned_from = Some(base);
    Ok(())
}

/// Push a materialised chain and run it to completion, for a sink that needs
/// the method's RETURN VALUE rather than a pushed frame — N2.
///
/// `try_resume_trapped_callee` (`vm/src/jit/helpers.rs`) sits between a
/// compiled caller and a compiled callee that trapped, and its whole contract
/// is to hand the caller the value the callee would have returned. For a single
/// frame that is `execute_prebuilt_frame`. For an inlined chain it is the same
/// thing one level up: push every frame outermost-first and run until the stack
/// comes back to the depth it started at, which is exactly when the OUTERMOST
/// frame — the callee the compiled caller actually invoked — returns.
///
/// **The depth is taken BEFORE the first push, and that is the whole point.**
/// `execute_prebuilt_frame` takes it inside itself, i.e. after the outer frames
/// would already be on the stack, and would therefore return the moment the
/// INNERMOST frame returned — leaving the rest of the chain stranded mid-method
/// and handing the compiled caller the wrong method's return value.
///
/// Test-only since gcd d1/b moved both production sinks to
/// [`run_deopt_frame_chain_to_completion_releasing_pins`], which releases the
/// compiled frame's pins before the chain runs (`no_test_only_public_api`).
#[cfg(test)]
pub(crate) fn run_deopt_frame_chain_to_completion(
    shared: &SharedVm,
    thread: &mut JvmThread,
    mut chain: DeoptFrameChain,
) -> Option<crate::error::MethodCallResult> {
    let trace = dbg_deopt_enabled();
    // Round 13 wave 8 (lane chain3): the chain's scalar-replaced objects are
    // re-allocated here, before anything is pushed, so a full heap gets the
    // single frame's answer -- `OutOfMemoryError` at the caller, as HotSpot
    // pops the frames (`resume_real_ir_deopt_or_throw` does the same) --
    // instead of the callers' `InternalError` for a chain that could not be
    // pushed. Such a chain holds no monitor (`chain_virtuals_refusal`), so no
    // lock of the abandoned activation needs releasing. A no-op for a chain
    // that names none; `push_inlined_chain` finds the work done.
    if let Err(why) = materialise_chain_virtuals(shared, thread, &mut chain) {
        if frame_build_heap_failure_is_answered(why) {
            return Some(Err(rematerialisation_out_of_memory(shared, thread)));
        }
        return None;
    }
    let depth_before = thread.frames.len();
    // `push_inlined_chain` is the one push path, shared with the frame-pushing
    // sink so the two cannot drift in their GC-rooting or pool handling. It
    // answers `None` only if an operand-stack push fails, which materialisation
    // already bounded against each frame's own `max_stack`; on that path some
    // frames are already pushed, so the caller must NOT treat the `None` as
    // "nothing happened" — see its own comment there. (Since round 13 wave 5
    // it also answers `None`, with nothing pushed, when the chain's
    // scalar-replaced objects could not be re-allocated; the callers' answer,
    // an error at the call and no replay, is sound for both.)
    push_inlined_chain(shared, thread, chain, trace)?;
    if !chain_runs_from_its_outermost_frame() {
        return Some(crate::runtime::interpreter::run_pushed_frame_to_completion(
            shared,
            thread,
            depth_before,
        ));
    }
    Some(run_pushed_chain_to_completion(shared, thread, depth_before))
}

/// `CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST`, **default ON** (round 13 wave 6,
/// lane chain2): [`run_deopt_frame_chain_to_completion`] runs the pushed chain
/// with its OUTERMOST frame as the interpreter entry's floor. `=0` restores
/// `run_pushed_frame_to_completion`, whose `execute_frame` takes the TOP
/// (innermost) frame as the floor: an exception the resumed callee threw left
/// the interpreter entry after searching that one frame -- the caller frames
/// the chain pushed beneath it were popped unsearched, so a `catch` around
/// the spliced call never saw it -- and a normal return ended the entry with
/// the CALLEE's value, the caller frames popped without running the rest of
/// their bodies.
fn chain_runs_from_its_outermost_frame() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST")
}

/// `interpreter::run_pushed_frame_to_completion` for SEVERAL frames pushed
/// above `frames_depth_before_push` (an inlined chain, outermost first): the
/// interpreter entry runs from the top frame with `frames_depth_before_push`
/// -- the outermost pushed frame -- as its floor, so a return from an inner
/// frame continues its caller at the invoke's successor and an exception is
/// offered to every pushed frame's handlers, exactly as for frames the
/// interpreter pushed itself. The entry ends when the OUTERMOST frame
/// returns or throws out of it, which is the value (or exception) the sink's
/// caller is owed. Everything else -- the stop-the-world check, the panic and
/// native-OOM handling, a continuation yield keeping its frames, the final
/// pops -- is the single-frame runner's (round 13 wave 6, lane chain2).
fn run_pushed_chain_to_completion(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frames_depth_before_push: usize,
) -> crate::error::MethodCallResult {
    debug_assert!(thread.frames.len() > frames_depth_before_push);
    if shared
        .mem
        .gc_barrier
        .stw_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        super::safepoint_check(shared, thread);
    }
    let pin_floor = thread.native_pin_roots.len();
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::execute_frame_from_index(shared, thread, frames_depth_before_push)
    })) {
        Ok(r) => r,
        Err(panic_info)
            if crate::runtime::native_oom::is_native_alloc_oom_payload(&*panic_info) =>
        {
            thread.native_pin_roots.truncate(pin_floor);
            Err(crate::runtime::native_oom::caught_oom_as_call_failure(
                panic_info,
            ))
        }
        Err(panic_info) => {
            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic in bytecode execution".to_string()
            };
            if let Some(f) = thread.frames.last() {
                eprintln!(
                    "[PANIC_IN/prebuilt-chain] {}.{}{} pc={} max_stack={} :: {}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc,
                    f.max_stack,
                    msg
                );
            }
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NotImplemented { feature: msg },
            )))
        }
    };
    // A continuation yield keeps every frame, as the single-frame runner's
    // yield arm does: they are the virtual thread's continuation.
    if matches!(
        result,
        Err(MethodCallFailed::InternalError(
            VmError::ContinuationYield { .. }
        ))
    ) {
        return result;
    }
    let mut result = result;
    super::pop_root_frames_keeping_result(shared, thread, frames_depth_before_push, &mut result);
    result
}

/// [`run_deopt_frame_chain_to_completion`] for a chain whose reconstructed oops
/// the caller still pins above `pin_base`: push, release those pins, run --
/// the chain twin of [`execute_resumed_frame_releasing_pins`] (gcd d1/b; item
/// "the chain arm still keeps them to the end" of
/// `docs/internal/gc/gengc-r5w4-jit8-interpreter-deopt-resume-pins-live-for-the-whole-resumed-run-FIXED-20260928.md`).
///
/// Same liveness argument as the single frame's: once [`push_inlined_chain`]
/// has returned `Some`, every scope of the chain is on `thread.frames`, which
/// every collection scans and remaps, so each reference a resumed scope can
/// still read is rooted by its own frame; the pins above `pin_base` only
/// duplicate them or keep what a scope has dropped, until the OUTERMOST frame
/// returns. `push_inlined_chain` allocates nothing on the Java heap, so no
/// collection falls between the last push and the truncate. On `None` (a
/// partial push, unreachable by construction) nothing is released: the
/// caller's refusal path truncates as before.
pub(crate) fn run_deopt_frame_chain_to_completion_releasing_pins(
    shared: &SharedVm,
    thread: &mut JvmThread,
    mut chain: DeoptFrameChain,
    pin_base: usize,
) -> Option<crate::error::MethodCallResult> {
    let trace = dbg_deopt_enabled();
    // Round 13 wave 11 (lane chain5): the full-heap answer of round 13 wave 8
    // (lane chain3) -- `OutOfMemoryError` at the caller, as for a single frame
    // -- lived in the test-only twin above since gcd d1/b moved both
    // production sinks here; `push_inlined_chain`'s own re-allocation answered
    // `None`, which both sinks turn into an `InternalError` ("could not push a
    // chain"). A no-op for a chain that names no scalar-replaced object
    // (`CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` off: every chain).
    if let Err(why) = materialise_chain_virtuals(shared, thread, &mut chain) {
        if frame_build_heap_failure_is_answered(why) {
            return Some(Err(rematerialisation_out_of_memory(shared, thread)));
        }
        return None;
    }
    let depth_before = thread.frames.len();
    push_inlined_chain(shared, thread, chain, trace)?;
    if thread.native_pin_roots.len() > pin_base {
        thread.native_pin_roots.truncate(pin_base);
    }
    // The run itself is [`run_deopt_frame_chain_to_completion`]'s, including
    // round 13 wave 6's outermost-frame floor
    // (`CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST`): this twin was written before
    // that fix reached the round branch, and both production sinks call it.
    if !chain_runs_from_its_outermost_frame() {
        return Some(crate::runtime::interpreter::run_pushed_frame_to_completion(
            shared,
            thread,
            depth_before,
        ));
    }
    Some(run_pushed_chain_to_completion(shared, thread, depth_before))
}

/// `execute_prebuilt_frame` for ONE frame rebuilt from a compiled activation
/// — a deopt resume or a compiled callee's handler frame: pushed without
/// `MethodEntry` (`push_resumed_frame`, the method was entered in compiled
/// code), then run until it returns. The single-frame twin of
/// [`run_deopt_frame_chain_to_completion`].
pub(crate) fn execute_resumed_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame: Frame,
) -> crate::error::MethodCallResult {
    let depth_before = thread.frames.len();
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/execute_resumed_frame] depth={} {}.{}{} pc={}",
            depth_before,
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor(),
            frame.pc
        );
    }
    push_resumed_frame(thread, frame);
    crate::runtime::interpreter::run_pushed_frame_to_completion(shared, thread, depth_before)
}

/// [`execute_resumed_frame`] for a frame [`build_deopt_frame_inner`] built,
/// whose reconstructed oops are still pinned above `pin_base`: push, release
/// the pins, run (gen r5w4/jit8).
///
/// This is [`resume_real_ir_deopt`]'s handoff -- pins across the push, the
/// frame alone afterwards -- for a caller that runs the frame to completion
/// instead of returning to the interpreter loop. `try_resume_trapped_callee`
/// (`vm/src/jit/helpers.rs`) used to keep the pins for the whole run
/// ("over-rooting is harmless"). It is not harmless for retention: every
/// reference the trapped frame held at its deopt point stayed a root until the
/// resumed method RETURNED, however long it ran and whatever it dropped in
/// between -- a method that nulls the local holding a large structure and
/// then waits on a `WeakReference` to it (or allocates until the next OOME)
/// saw it survive every collection it ran.
///
/// Liveness argument: once pushed, the frame is on `thread.frames`, which
/// every collection path scans (`collect_roots` step 1, the safepoint and
/// blocked deposits) and remaps, so every reference the resumed method can
/// still read is rooted by its own frame -- locals by the per-bci liveness
/// rule the interpreter applies to every frame, the operand stack whole, the
/// relocked monitors through `held_monitors` (seeded by the build). The pins
/// above `pin_base` were pushed by the build for exactly this frame (the
/// caller's own pins, e.g. `CompiledLocksOfAStash`, sit below `pin_base`), so
/// after the push they only duplicate the frame or keep what the frame no
/// longer needs. `push_resumed_frame` allocates nothing on the Java heap, so no
/// collection runs between the push and the truncate.
pub(crate) fn execute_resumed_frame_releasing_pins(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame: Frame,
    pin_base: usize,
) -> crate::error::MethodCallResult {
    let depth_before = thread.frames.len();
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/execute_resumed_frame_releasing_pins] depth={} {}.{}{} pc={}",
            depth_before,
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor(),
            frame.pc
        );
    }
    push_resumed_frame(thread, frame);
    if thread.native_pin_roots.len() > pin_base {
        thread.native_pin_roots.truncate(pin_base);
    }
    crate::runtime::interpreter::run_pushed_frame_to_completion(shared, thread, depth_before)
}

/// [`execute_resumed_frame_releasing_pins`] for a frame rebuilt from the stash
/// of a compiled body whose constant-pool generation is `compile_cp_stamp`
/// (`CompiledMethod::compile_cp_stamp`): after the push and before the first
/// bytecode, the frame is restamped with that generation and moved onto its
/// translated body when a redefinition replaced the class's pool since the
/// compile (`obsolete_frames::stamp_frame_rebuilt_from_compiled_code`), as
/// [`real_frame_deopt_resume_or_throw_and_despeculate`] does for the
/// `jit_bridge` doors. A frame built now otherwise carries the CURRENT stamp
/// and reads the new pool with the old body's indices. `execute`'s first-call
/// tier-up sink resumes through here (round 13 wave 1, lane replay); it used
/// to push the frame unstamped. Nothing to do, and no switch read, while no
/// class was ever redefined. Kill switch
/// `CRATONVM_DEOPT_TIERUP_SINK_CP_STAMP=0`.
pub(crate) fn execute_rebuilt_frame_releasing_pins(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame: Frame,
    pin_base: usize,
    compile_cp_stamp: Option<u64>,
) -> crate::error::MethodCallResult {
    let depth_before = thread.frames.len();
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/execute_rebuilt_frame_releasing_pins] depth={} {}.{}{} pc={}",
            depth_before,
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor(),
            frame.pc
        );
    }
    push_resumed_frame(thread, frame);
    if thread.native_pin_roots.len() > pin_base {
        thread.native_pin_roots.truncate(pin_base);
    }
    if crate::classloading::any_class_redefined()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_TIERUP_SINK_CP_STAMP")
    {
        super::obsolete_frames::stamp_frame_rebuilt_from_compiled_code(
            shared,
            thread,
            compile_cp_stamp,
        );
    }
    crate::runtime::interpreter::run_pushed_frame_to_completion(shared, thread, depth_before)
}

/// [`execute_resumed_frame`] for a compiled callee's HANDLER frame
/// (`run_jit_callee_handler`): the frame sits at `handler_pc` with the
/// throwable on its operand stack, and once it is pushed — so it roots the
/// throwable and names the method — the catch is reported as the unwinder
/// reports one (`Exception` at `throw_pc`, then `ExceptionCatch`;
/// `report_exception_caught_by_compiled_door`, interpreter round i1 wave 21,
/// lane L2). Then the frame runs to completion.
pub(crate) fn execute_resumed_handler_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame: Frame,
    throw_pc: usize,
    handler_pc: usize,
) -> crate::error::MethodCallResult {
    let depth_before = thread.frames.len();
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/execute_resumed_handler_frame] depth={} {}.{}{} pc={}",
            depth_before,
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor(),
            frame.pc
        );
    }
    push_resumed_frame(thread, frame);
    crate::runtime::interpreter::report_exception_caught_by_compiled_door(
        shared,
        thread,
        depth_before,
        throw_pc,
        handler_pc,
    );
    crate::runtime::interpreter::run_pushed_frame_to_completion(shared, thread, depth_before)
}

/// real-frame-deopt Step 3 — build the interpreter `Frame` for an Object-bearing
/// deopt, GC-rooting the reconstructed oops across the pool refill. Returns the
/// built `Frame` (NOT pushed onto `thread.frames`) with the oops still pinned in
/// `thread.native_pin_roots` above the caller's watermark — the CALLER must
/// `native_pin_roots.truncate(pin_base)` once the frame is pushed (or
/// discarded), as [`resume_real_ir_deopt`] does.
/// `None` if the frame is out-of-scope (an inlined caller chain, a monitor that
/// is not a resolved object) or carries an unmappable slot
/// (cat-2/Unsupported/virtual/FP/unresolved).
///
/// The built frame's `held_monitors` record names every lock the compiled
/// frame held (interpreter round i1 wave 24, lane L2): see the relock loop at
/// the end.
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
pub(crate) fn build_deopt_frame_inner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    stress: bool,
) -> Option<crate::runtime::frame::Frame> {
    build_deopt_frame_or_refusal(shared, thread, cached, rframe, stress, false).ok()
}

/// `CRATONVM_DEOPT_REMATERIALISE_OOM`, **default ON** (`=0` restores the
/// pre-round-12-wave-5 answer, a declined resume and a re-run from entry):
/// whether a sink that asked for it answers a scalar-replaced object graph the
/// full heap could not re-materialise with `OutOfMemoryError` thrown out of the
/// trapped method, as HotSpot does. Read once.
fn rematerialise_oom_throws() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_REMATERIALISE_OOM")
    })
}

/// Was `compiled` compiled from `class_id`'s CURRENT bytecode, so a trap out
/// of it resumes precisely even though the class was redefined at some point?
/// True when its compilation began (`install_epoch`, stamped at the compile
/// gate before the first bytecode or constant-pool read) after this cache's
/// last flush and after the class's last scoped redefinition
/// (`JitCache::compiled_since_redefinition_of`, the rule the call-site service
/// `stashed_callee_predates_redefinition` and `granted_exit_of_a_current_body`
/// already apply). Such a body's bytecode and constant pool are the ones
/// `cached` holds, so the rebuilt frame continues the very code the artifact's
/// deopt points describe; without this every superseded body of a class an
/// agent ever retransformed re-ran from entry and committed its prefix twice
/// (`r12w5-replay3-bytecode-identity-on-the-artifact-patch-CLOSED-20260929.md`, whose
/// bytecode-hash stamp this replaces: equal bytes under a replaced constant
/// pool are not the same code). Kill switch
/// `CRATONVM_DEOPT_RESUME_CURRENT_BODY_OF_REDEFINED=0` (round 12 wave 6, lane
/// jni).
fn compiled_from_the_current_bytecode(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    class_id: ClassId,
) -> bool {
    resume_current_body_of_redefined_class()
        && shared
            .jit
            .jit_cache
            .compiled_since_redefinition_of(class_id, compiled.install_epoch)
}

/// Stamp `cm` with the template its compilation read
/// (`CompiledMethod::compiled_source`), at a publish site that compiled from
/// `source` (round 13 wave 3, lane replay2; proposal R13-1). One `Arc` per
/// publication; read only by [`obsolete_activation_source`]. A body stamped
/// twice keeps its first source.
#[inline]
pub(super) fn stamp_compiled_source_of(
    cm: &crate::jit::CompiledMethod,
    source: Arc<CachedBytecodeMethod>,
) {
    let _ = cm.stamp_compiled_source(source);
}

/// The bytecode a trap out of `compiled`, a body of a REDEFINED class, must
/// resume in: the template its compilation read
/// (`CompiledMethod::compiled_source`), when its publish site stamped one for
/// `cached`'s method, it has a constant-pool stamp, and the class's history
/// can move a frame of it onto the current pool
/// (`obsolete_frames::rebuilt_body_translates`). The rebuilt frame is then
/// restamped with the body's pool generation and translated, so the obsolete
/// activation continues in its OWN code (JVMTI `RedefineClasses`: "existing
/// activations continue in the obsolete method"; HotSpot deoptimizes into the
/// obsolete `Method*`). Without it a superseded body of such a class re-ran
/// its method from entry in whatever `cached` held (a double commit, then
/// code the activation never had), and a current one resumed into `cached`
/// even when the door had paired it with the NEW template.
///
/// `None` (the caller keeps `cached`, and its pre-existing rules) when the
/// switch is off, nothing was stamped, the stamp names another method, it is
/// `cached` itself, or the translation would be refused. For a single frame,
/// or for a chain's OUTERMOST frame only ([`chain_outermost_own_source`]):
/// an inlined chain's other frames come from their own templates. Kill switch
/// `CRATONVM_DEOPT_RESUME_OBSOLETE_ACTIVATION=0` (round 13 wave 3, lane
/// replay2).
pub(super) fn obsolete_activation_source(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
) -> Option<Arc<CachedBytecodeMethod>> {
    let source = compiled.compiled_source()?;
    if Arc::ptr_eq(source, cached)
        || source.declaring_class_id != cached.declaring_class_id
        || *source.method_name != *cached.method_name
        || *source.method_descriptor != *cached.method_descriptor
    {
        return None;
    }
    let stamp = compiled.compile_cp_stamp()?;
    // Read here, past the cheap refusals, on a trap of a redefined class's
    // body only (no new static for a once-cell).
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_RESUME_OBSOLETE_ACTIVATION")
    {
        return None;
    }
    super::obsolete_frames::rebuilt_body_translates(
        shared,
        source.declaring_class_id,
        &source.code,
        &source.exception_table,
        stamp,
    )
    .then(|| Arc::clone(source))
}

/// Round 14 wave 2 (lane deopt; proposal R13RP6-1): the bytecode a trap the
/// call-site service (`helpers::try_resume_trapped_callee`) is about to refuse
/// must resume in, when the trampoline named its body.
///
/// The service holds no artifact, so a trap out of a SUPERSEDED body of a
/// redefined class (one compiled before the redefinition: a stale call-site
/// binding kept calling it) was refused, and the callee re-ran from entry in
/// the NEW bytecode -- the activation's committed prefix replayed, then code
/// it never had. The x64 framed trap now stashes its body's own template
/// (`cratonvm_jit::deopt::peek_last_deopt_trap_source`), and this answers it
/// exactly as [`obsolete_activation_source`] answers the doors: the source
/// names `class_id` (the class the call site's loader resolves the stash key
/// to) and the stash's method, the body has a constant-pool stamp, and a frame
/// of it translates onto the current pool (`rebuilt_body_translates`). A
/// single frame only (a chain's inner scopes have no template), and never an
/// `ACC_SYNCHRONIZED` method (who owns its monitor is the service's
/// hand-over question, asked of the current method; refused as before).
///
/// `Some((source, stamp))`: build the frame from `source` and run it through
/// [`execute_own_source_frame_releasing_pins`] with `stamp`. `None` keeps the
/// refusal. Kill switch `CRATONVM_DEOPT_CALLSITE_OWN_SOURCE=0` (default ON),
/// read only on a trap of a redefined class's superseded body.
pub(crate) fn callsite_trap_own_source(
    shared: &SharedVm,
    class_id: ClassId,
    key_method: &str,
    key_desc: &str,
) -> Option<(Arc<CachedBytecodeMethod>, u64)> {
    let (source, stamp) = cratonvm_jit::deopt::peek_last_deopt_trap_source()?;
    let stamp = stamp?;
    if source.declaring_class_id != class_id
        || *source.method_name != *key_method
        || *source.method_descriptor != *key_desc
        || source.is_synchronized
    {
        return None;
    }
    if !cratonvm_jit::deopt::peek_last_deopt_frame(|f| f.caller_frames.is_empty()).unwrap_or(false)
    {
        return None;
    }
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CALLSITE_OWN_SOURCE") {
        return None;
    }
    super::obsolete_frames::rebuilt_body_translates(
        shared,
        source.declaring_class_id,
        &source.code,
        &source.exception_table,
        stamp,
    )
    .then_some((source, stamp))
}

/// Round 14 wave 3 (lane resume; proposal R14DP-1 of `jit-r14-deopt-proposals.md`):
/// the template the call-site service (`helpers::try_resume_trapped_callee`)
/// rebuilds a trapped callee's frame from when the trampoline named the
/// trapping body (`cratonvm_jit::deopt::peek_last_deopt_trap_source`) and that
/// body's class was NEVER redefined, so its own template is exactly the
/// method's current bytecode.
///
/// The service otherwise resolves the stash key through the call site's
/// loader, walks `find_method_recursive` and allocates a fresh
/// `CachedBytecodeMethod` on every trap; five of its refusals exist only
/// because a NAME is all it has (the class not visible from the call site's
/// loader, a declaring-class mismatch), and each one left the frame to an
/// outer sink as an orphan. The stashed template is the body's own: exact
/// identity, no class-manager lock, no allocation. A frame built from it needs
/// no restamp (the class has one pool generation).
///
/// `None` (the service keeps its name resolution) when nothing names the
/// body, the template is another method's than the stash key's, the method is
/// `ACC_SYNCHRONIZED` (the monitor hand-over question stays on the named path),
/// the class was redefined (the superseded-body case is
/// [`callsite_trap_own_source`]; a current body of a redefined class keeps the
/// name path), or with `CRATONVM_DEOPT_CALLSITE_STASH_TEMPLATE=0` (default ON).
pub(crate) fn callsite_trap_current_template(
    shared: &SharedVm,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
) -> Option<Arc<CachedBytecodeMethod>> {
    let (source, _) = cratonvm_jit::deopt::peek_last_deopt_trap_source()?;
    if *source.class_name != *key_class
        || *source.method_name != *key_method
        || *source.method_descriptor != *key_desc
        || source.is_synchronized
    {
        return None;
    }
    if crate::classloading::any_class_redefined()
        && class_was_redefined(shared, source.declaring_class_id)
    {
        return None;
    }
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CALLSITE_STASH_TEMPLATE") {
        return None;
    }
    Some(source)
}

/// [`execute_resumed_frame_releasing_pins`] for a frame built from a trapped
/// body's OWN bytecode ([`callsite_trap_own_source`]): after the push and
/// before the first bytecode the frame is restamped with that body's
/// constant-pool generation and moved onto its translated body, as the doors
/// do for [`obsolete_activation_source`]. Unconditional, unlike
/// [`execute_rebuilt_frame_releasing_pins`]'s switch: a frame running obsolete
/// code under the current pool's stamp would read the wrong constants.
pub(crate) fn execute_own_source_frame_releasing_pins(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame: Frame,
    pin_base: usize,
    compile_cp_stamp: u64,
) -> crate::error::MethodCallResult {
    let depth_before = thread.frames.len();
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/execute_own_source_frame_releasing_pins] depth={} {}.{}{} pc={}",
            depth_before,
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor(),
            frame.pc
        );
    }
    push_resumed_frame(thread, frame);
    if thread.native_pin_roots.len() > pin_base {
        thread.native_pin_roots.truncate(pin_base);
    }
    super::obsolete_frames::stamp_frame_rebuilt_from_compiled_code(
        shared,
        thread,
        Some(compile_cp_stamp),
    );
    crate::runtime::interpreter::run_pushed_frame_to_completion(shared, thread, depth_before)
}

/// `CRATONVM_DEOPT_RESUME_CURRENT_BODY_OF_REDEFINED`, **default ON**; `=0`
/// restores the pre-round-12-wave-6 skip (a trap out of a superseded body of a
/// redefined class re-runs the method from entry). Read once.
fn resume_current_body_of_redefined_class() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_DEOPT_RESUME_CURRENT_BODY_OF_REDEFINED",
        )
    })
}

/// Must a sink that built with `release_held_on_heap_failure` answer this
/// refusal with [`rematerialisation_out_of_memory`] rather than a re-run? True
/// exactly when the build released the compiled code's locks for it.
pub(crate) fn frame_build_heap_failure_is_answered(why: DeoptFrameBail) -> bool {
    why == DeoptFrameBail::VirtualMaterialiseHeap && rematerialise_oom_throws()
}

/// Is `failed` the allocator's heap-exhaustion answer (the shape
/// `alloc_object_shared` / `gc_alloc_array` return once their collect-and-retry
/// ladder has failed)? After `validate_virtual_graph` it is the only way
/// `materialize_virtual_objects` can still fail.
fn materialisation_failed_for_heap(failed: &MethodCallFailed) -> bool {
    matches!(
        failed,
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError { .. }))
    )
}

/// The throwable a sink raises for [`DeoptFrameBail::VirtualMaterialiseHeap`]:
/// HotSpot's `out_of_memory_error_realloc_objects` message, built without
/// collecting again and falling back to the preallocated singleton
/// (`throw_runtime_error`'s OOM rule). The trapped activation is abandoned, as
/// HotSpot pops it (`Deoptimization::pop_frames_failed_reallocs`): no handler of
/// the trapped method runs, and the caller receives the error at its call.
pub(crate) fn rematerialisation_out_of_memory(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> MethodCallFailed {
    crate::runtime::exceptions::throw_runtime_error(
        shared,
        thread,
        RuntimeError::OutOfMemoryError {
            message: "Java heap space: failed reallocation of scalar replaced objects"
                .to_string(),
        },
    )
}

/// Pin every monitor the COMPILED code took (`relock == false`, a real object)
/// so a failed re-materialisation, whose allocator collected, can still name
/// them afterwards. Returns the pin base.
fn pin_compiled_held_monitors(
    thread: &mut JvmThread,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> usize {
    use cratonvm_jit::deopt::FrameValue;
    let base = thread.native_pin_roots.len();
    for m in &rframe.monitors {
        if let (false, FrameValue::Object(addr)) = (m.relock, &m.object) {
            if *addr != 0 {
                // SAFETY: a non-null reference the deopt resolved from the
                // trapped frame, pinned before anything can collect.
                let obj = unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) };
                thread.native_pin_roots.push(obj);
            }
        }
    }
    base
}

/// The other half of [`pin_compiled_held_monitors`], for an activation that is
/// being abandoned: release each lock the compiled code took, `lock_depth`
/// levels, re-read through its (possibly forwarded) pin, then drop the pins.
/// HotSpot unlocks the popped frames' monitors the same way
/// (`pop_frames_failed_reallocs`); left held, they would outlive the frame
/// that owned them. An elided lock (`relock`) was never taken and is skipped.
fn release_compiled_held_monitors(
    shared: &SharedVm,
    thread: &mut JvmThread,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    base: usize,
) {
    use cratonvm_jit::deopt::FrameValue;
    let mut k = base;
    for m in &rframe.monitors {
        let (false, FrameValue::Object(addr)) = (m.relock, &m.object) else {
            continue;
        };
        if *addr == 0 {
            continue;
        }
        let Some(obj) = thread.native_pin_roots.get(k).copied() else {
            break;
        };
        k += 1;
        for _ in 0..m.lock_depth {
            let _ = crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, obj, thread.thread_id);
        }
    }
    thread.native_pin_roots.truncate(base);
}

/// Round 13 wave 9 (lane chain4;
/// `r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md`): does
/// `rframe`, a stashed frame of the method with these facts, carry the METHOD
/// monitor a self-locking body kept at its guard exit
/// (`cratonvm_jit::deopt::handed_method_monitor_index`)? Such a frame resumes
/// OWNING that hold (the frame builders make it the frame's
/// `monitor_on_exit`), so a door that holds no monitor of its own for the
/// method may resume it, and a sink that abandons it releases it like any lock
/// the compiled code took (`CompiledLocksOfAStash`). `code` is the method's
/// bytecode, padded or not (the monitor walk reads `nop`s past the end).
pub(crate) fn frame_hands_over_method_monitor(
    is_synchronized: bool,
    is_static: bool,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    handed_method_monitor_at(is_synchronized, is_static, code, rframe).is_some()
}

/// The index in `rframe.monitors` of the handed-over method monitor
/// ([`frame_hands_over_method_monitor`]); cheap `None` for every frame that
/// is not a synchronized method's naming exactly one monitor.
///
/// Round 13 wave 11 (lane sync7,
/// `r13w10-sync6-static-self-lock-deopt-hand-over-patch-FIXED-20260929.md`): a
/// `static synchronized` method's frame too, whose handed hold is its class
/// mirror (a static self-locking body's guard,
/// `CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER`). Every consumer reads it
/// the same way as an instance one: the builders make it `monitor_on_exit`,
/// the doors and `try_resume_trapped_callee` resume the frame although they
/// hold no monitor of their own, and every abandoning sink releases it as a
/// compiled lock (`CompiledLocksOfAStash`).
fn handed_method_monitor_at(
    is_synchronized: bool,
    is_static: bool,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<usize> {
    if !is_synchronized || rframe.monitors.len() != 1 {
        return None;
    }
    cratonvm_jit::deopt::handed_method_monitor_index(
        cratonvm_jit::deopt::SinkMethodMonitor::of(is_synchronized, is_static),
        cratonvm_jit::bytecode_holds_monitor(code, code.len()),
        rframe,
    )
}

/// Round 13 wave 9 (lane chain4): a door that does NOT resume a stashed frame
/// and re-runs its method from entry instead abandons the activation the frame
/// describes, so what that activation's compiled code locked is released first,
/// as `resume_or_despeculate_stash` does on its own re-run arm. For the arm of
/// a synchronized method's door that holds no monitor of its own
/// (`CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP`), which re-ran without asking and so
/// left a nested self-locking callee's handed monitor (or any lock a foreign
/// stash's compiled code took) held for good. Kill switch
/// `CRATONVM_DEOPT_UNRESUMED_STASH_RELEASES=0`. Returns how many single releases
/// succeeded.
pub(crate) fn release_locks_of_an_unresumed_stash(
    shared: &SharedVm,
    thread: &mut JvmThread,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> u32 {
    if rframe.monitors.is_empty()
        && rframe.caller_frames.iter().all(|scope| scope.monitors.is_empty())
    {
        return 0;
    }
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_UNRESUMED_STASH_RELEASES") {
        return 0;
    }
    CompiledLocksOfAStash::pin_compiled_locks(thread, rframe).release_for_a_rerun(shared, thread)
}

/// [`build_deopt_frame_inner`], answering WHY it declined (round 12 wave 5,
/// lane replay3; `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns`
/// cause 1).
///
/// `release_held_on_heap_failure` is the sink's promise that it will THROW on
/// [`DeoptFrameBail::VirtualMaterialiseHeap`]
/// ([`rematerialisation_out_of_memory`]) rather than re-run or restash: with
/// it (and `CRATONVM_DEOPT_REMATERIALISE_OOM` on), a heap failure first
/// releases the monitors the compiled code holds for the abandoned activation.
/// Without it the answer is the same `None` as before, only counted under its
/// own name.
pub(crate) fn build_deopt_frame_or_refusal(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    stress: bool,
    release_held_on_heap_failure: bool,
) -> Result<crate::runtime::frame::Frame, DeoptFrameBail> {
    // Single non-inlined frame (inlined-caller chains stay out of scope). Held
    // monitors are NO LONGER a blanket bail. Each `MonitorInfo` says whether the
    // resume must acquire it:
    //   * `relock == true` — an ELIDED lock. The single-pass backend's Phase C
    //     records `synchronized(scalarObj)` blocks this way, and the optimizing
    //     tier marks every monitor whose ops lock elision deleted. The object is
    //     materialized (if virtual) and locked `lock_depth` times below.
    //   * `relock == false` — a lock the compiled code took through the monitor
    //     helper (the optimizing tier; the single-pass tier since wave 26,
    //     where its analysis can name the lock). The thread still holds it; the resumed
    //     frame's `monitorexit` releases it. It is pinned and checked like any
    //     reference and otherwise left alone.
    if !rframe.caller_frames.is_empty() {
        note_deopt_frame_bail(DeoptFrameBail::InlinedChain, rframe);
        return Err(DeoptFrameBail::InlinedChain);
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
        return Err(DeoptFrameBail::IdentityMismatch);
    }
    if rframe.bci == u32::MAX {
        note_deopt_frame_bail(DeoptFrameBail::SupersededSentinel, rframe);
        return Err(DeoptFrameBail::SupersededSentinel);
    }
    // The frame says the interpreter may be parked at its bci, and this asks it
    // rather than assuming it. Everything below — `frame.pc = rframe.bci` at the
    // end of this function above all — is only correct for a `REEXECUTE` point.
    // See [`DeoptFrameBail::NotAResumePoint`] for why this is expected to stay
    // at zero and what a non-zero count would mean.
    if !rframe.semantics.reexecute {
        note_deopt_frame_bail(DeoptFrameBail::NotAResumePoint, rframe);
        return Err(DeoptFrameBail::NotAResumePoint);
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
            return Err(DeoptFrameBail::VerifyFailed);
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
    // `resume_real_ir_deopt` holds across `push_resumed_frame`, so the
    // shells are rooted continuously from allocation until the resumed frame
    // roots them. On any materialization failure (unsupported field / unknown
    // id) the pins are released and we fall back to re-run (`?` → `None`).
    use cratonvm_jit::deopt::FrameValue;
    // Monitors included: an elided lock on an object no local or stack slot
    // still names carries the object's only definition in its monitor entry.
    let has_virtual = rframe
        .locals
        .iter()
        .chain(rframe.stack.iter())
        .chain(rframe.monitors.iter().map(|m| &m.object))
        .any(|v| {
            matches!(
                v,
                FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
            )
        });
    if has_virtual && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_SCALAR_DEOPT") {
        eprintln!(
            "[DBG_SCALAR_DEOPT] build_deopt_frame_inner: materializing virtual object(s) at bci={}",
            rframe.bci
        );
    }
    // The operand-stack fit, asked before anything that can allocate or GC
    // (the materialiser, the pool refill) rather than at the push, for the
    // reason `refusal_before_materialisation` gives. The interpreter stack is
    // compact (one entry per `long`/`double`), so a verifiable frame never
    // holds more entries than the JVM-slot `max_stack`.
    // Widening: u16 -> usize.
    if rframe.stack.len() > cached.max_stack as usize {
        note_deopt_frame_bail(DeoptFrameBail::StackPush, rframe);
        return Err(DeoptFrameBail::StackPush);
    }
    let materialized_frame;
    let rframe: &cratonvm_jit::deopt::ReconstructedFrame = if has_virtual {
        // Elided-monitor gate. Escape analysis performs lock elision over
        // non-escaping objects (`jit::escape_analysis::find_lock_elisions`): a
        // scalar-replaced object may have had its `monitorenter`/`monitorexit`
        // elided, so a resumed frame that later runs `monitorexit` would hit an
        // un-entered monitor.
        //
        // A `synchronized(obj)` BLOCK over such an object is described: both
        // producers record the elided lock as a `relock` `MonitorInfo` (the
        // optimizing tier from the builder's monitor stack since 2026-09-12), and
        // the relock below re-acquires it on the materialized object.
        //
        // What stays refused is an `ACC_SYNCHRONIZED` method whose receiver
        // slot is itself virtual: its method monitor is not a frame-state entry
        // at all (the door takes it, not a `monitorenter`), so an elision of it
        // under scalar replacement of the receiver would leave no trace here.
        // Round 12 wave 7 (lane replay4): ONLY that shape. No tier compiles the
        // method monitor (a synchronized body is entered through a door that
        // holds it and hands it to the resumed frame), and the monitor is the
        // class mirror or an argument this compile did not allocate, so every
        // other synchronized frame materialises and resumes. It used to refuse
        // them all and the doors re-ran the method from entry, committing its
        // prefix twice, while the compile-time replay check called the frame
        // resumable. Both ends now read one predicate
        // (`cratonvm_jit::deopt::synchronized_frame_refuses_virtuals`; kill
        // switch `CRATONVM_DEOPT_SYNC_VIRTUALS_RESUME=0` refuses every one again,
        // at both ends).
        if cratonvm_jit::deopt::synchronized_frame_refuses_virtuals(
            cratonvm_jit::deopt::SinkMethodMonitor::of(cached.is_synchronized, cached.is_static),
            &rframe.locals,
            has_virtual,
        ) {
            note_deopt_frame_bail(DeoptFrameBail::SynchronizedWithVirtuals, rframe);
            return Err(DeoptFrameBail::SynchronizedWithVirtuals);
        }
        // Every refusal that can follow the materialisation is asked FIRST.
        // Materialising allocates, and an allocation can run a moving GC; the
        // caller took `rframe` out of the stash, so its `Object(addr)` words are
        // rooted by nothing during that window, and a sink that restashes it on
        // a refusal (`try_resume_trapped_callee`) would hand the next consumer
        // from-space addresses. See r11-tier-deopt-resume-gc-windows-FIXED-20260924.md.
        if let Some(why) = refusal_before_materialisation(rframe) {
            note_deopt_frame_bail(why, rframe);
            return Err(why);
        }
        let mut copy = rframe.clone();
        // A sink that will throw on a heap failure needs the compiled code's
        // held monitors at their post-collection addresses to release them, so
        // they are pinned across the materialiser. On success these pins sit
        // below the shells' and go with the caller's watermark truncation.
        let answer_heap_failure = release_held_on_heap_failure && rematerialise_oom_throws();
        let held_base = if answer_heap_failure {
            pin_compiled_held_monitors(thread, rframe)
        } else {
            thread.native_pin_roots.len()
        };
        if let Err(failed) = crate::runtime::deopt_materialize::materialize_virtual_objects(
            shared, thread, &mut copy, /* stress_gc */ false, /* keep_pins */ true,
        ) {
            // Round 12 wave 5 (lane replay3): a full heap is named apart from a
            // malformed graph; see `DeoptFrameBail::VirtualMaterialiseHeap`.
            let why = if materialisation_failed_for_heap(&failed) {
                DeoptFrameBail::VirtualMaterialiseHeap
            } else {
                DeoptFrameBail::VirtualMaterialise
            };
            if answer_heap_failure && why == DeoptFrameBail::VirtualMaterialiseHeap {
                release_compiled_held_monitors(shared, thread, rframe, held_base);
            }
            thread.native_pin_roots.truncate(held_base);
            note_deopt_frame_bail(why, rframe);
            return Err(why);
        }
        materialized_frame = copy;
        &materialized_frame
    } else {
        rframe
    };
    // Round 13 wave 9 (lane chain4): the method monitor a self-locking body
    // handed over with this frame, which the frame owns as its
    // `monitor_on_exit` (seeded below), never as a block monitor.
    let handed_monitor = handed_method_monitor_at(
        cached.is_synchronized,
        cached.is_static,
        &cached.code,
        rframe,
    );

    // deopt-osr P2 — map via the cat-2-aware mappers (both route through
    // `fv_to_value`, so Int/Long/Float/Double/Object/Undefined all map): LOCALS
    // collapse the JVM-two-slot snapshot (a `long`/`double` reserves its upper
    // half, skipped) into the compact arg list `Frame::new_pooled` re-expands; the
    // operand STACK is already compact (one entry per value). Any unresolvable
    // slot (`Unsupported`/virtual/unresolved machine form) returns `None` → safe
    // re-run.
    //
    // `CRATONVM_DBG_DEOPTSLOT` is read once: this runs on every frame rebuild.
    static DBG_DEOPTSLOT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let dbg_deoptslot = *DBG_DEOPTSLOT
        .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPTSLOT").is_some());
    if dbg_deoptslot
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
            return Err(DeoptFrameBail::UnmappableLocal);
        }
    };
    let stack_vals = match ir_deopt_frame_values(&rframe.stack) {
        Some(v) => v,
        None => {
            note_deopt_frame_bail(DeoptFrameBail::UnmappableStack, rframe);
            return Err(DeoptFrameBail::UnmappableStack);
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
    // Phase C: pin the held-monitor objects too (materialized shells, or the
    // real object a compiled lock holds), so the refill GC forwards them in
    // place and the relock below uses live addresses. `monitor_plan` keeps
    // (depth, relock) in push order; a non-Object monitor (an unresolved
    // virtual ref — shouldn't occur, the materializer rewrote them) or a null
    // one bails to safe re-run.
    let mut monitor_plan: Vec<(u32, bool)> = Vec::with_capacity(rframe.monitors.len());
    for m in &rframe.monitors {
        match &m.object {
            FrameValue::Object(addr) if *addr != 0 => {
                // SAFETY: `addr` is a non-null reference the deopt resolved from
                // the trapped frame (or materialization produced), and it is
                // pinned here before any allocation or GC can make it stale.
                let obj = unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) };
                thread.native_pin_roots.push(obj);
                monitor_plan.push((m.lock_depth, m.relock));
            }
            // Null monitor or an unresolved form — refuse rather than relock a
            // bogus object (would corrupt the monitor table). Release nothing
            // extra; the caller truncates `native_pin_roots` to its watermark.
            _ => {
                note_deopt_frame_bail(DeoptFrameBail::BadMonitor, rframe);
                return Err(DeoptFrameBail::BadMonitor);
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
    // pairing each with its lock depth and relock marker for the relock below.
    let mut monitors_fwd: Vec<(ObjectRef, u32, bool)> = Vec::with_capacity(monitor_plan.len());
    for &(depth, relock) in &monitor_plan {
        let fwd = thread.native_pin_roots[k];
        k += 1;
        monitors_fwd.push((fwd, depth, relock));
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
            return Err(DeoptFrameBail::StackPush);
        }
    }
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    frame.pc = rframe.bci as usize;
    // Phase C: re-acquire each ELIDED monitor (`relock`) on its object,
    // `lock_depth` times, so the resumed frame's eventual `monitorexit` (and any
    // nested exits) balance via the MonitorTable. Done after every fallible step
    // so a bail never leaves a stray lock. `monitors.enter` is the recursive
    // uncontended enter: an elided lock's object never escaped this thread, so
    // there is no contention and no GC.
    //
    // A monitor with `relock == false` is one the compiled code acquired through
    // the monitor helper. This thread still holds it in the monitor table, so
    // the resumed frame already holds it and entering again would leave it one
    // level too deep after the frame's own `monitorexit`.
    for &(obj, depth, relock) in &monitors_fwd {
        if !relock {
            continue;
        }
        for _ in 0..depth {
            shared.threads.monitors.enter(obj, thread.thread_id);
        }
        // Publish the relocked monitor as every other acquisition does, so
        // `getLockedMonitors()` sees it while the resumed frame holds it. One
        // publish per object: the book dedupes, and the frame's outermost
        // `monitorexit` retracts it (r11w5-sync-deopt-relock-publish-patch).
        if depth > 0 {
            shared
                .threads
                .thread_registry
                .complete_jmx_monitor_enter(thread.thread_id, obj);
        }
    }
    // Seed the frame's record of its block monitors (`Frame::held_monitors`,
    // JVMS §2.11.10; interpreter round i1 wave 24, lane L2): every lock the
    // compiled frame held, the ones its code took and the elided levels just
    // re-taken, `lock_depth` copies each, outermost first. HotSpot's unpacked
    // interpreter frame likewise gets a `BasicObjectLock` for every monitor the
    // compiled frame's debug info names (`vframeArrayElement::fill_in`).
    //
    // Without it the frame's own `monitorexit`s still balance — they fall to
    // `held_monitors::monitorexit_unrecorded`, which releases an acquisition
    // no record accounts for — but the rules that read the record would not
    // see these locks: a return or an unwind out of the frame while one is
    // still held (hand-written bytecode; javac releases first) would keep it
    // silently instead of throwing `IllegalMonitorStateException`, and an
    // interpreted callee's `monitorexit` of this frame's lock would release it
    // instead of throwing. An entry the monitor table does not back is dropped
    // by `held_monitors::prune_stale` before any report, so a description that
    // over-states a hold degrades to the old behaviour.
    //
    // GC: the entries are the forwarded addresses read back from the pins
    // above, and nothing allocates between here and the caller's push (the
    // same window the frame's locals rely on); once pushed, the record is a
    // frame root like `monitor_on_exit`.
    //
    // Round 13 wave 9 (lane chain4): except the METHOD monitor a self-locking
    // body handed over (`handed_monitor`): the frame owns it as its
    // `monitor_on_exit`, the implicit release of an `ACC_SYNCHRONIZED` return
    // or unwind (`pop_and_recycle_frame_with_reason`). Recorded as a block
    // monitor, the frame's return would throw `IllegalMonitorStateException`.
    // A door that holds a hold of its own for the method releases that one
    // (`transfer_to_resumed_frame`), so the thread ends the call holding what
    // it held before it.
    for (i, &(obj, depth, _)) in monitors_fwd.iter().enumerate() {
        if handed_monitor == Some(i) {
            frame.monitor_on_exit = Some(obj);
            continue;
        }
        for _ in 0..depth {
            frame.held_monitors.push(obj);
        }
    }
    Ok(frame)
}

/// The refusals [`build_deopt_frame_inner`] makes AFTER materialising virtual
/// objects, asked of the un-materialised frame: a `VirtualObject` /
/// `VirtualObjectRef` counts as a mappable, non-null reference (the
/// materialiser turns each into an `Object` shell). `None` means none of them
/// can fire once materialisation succeeds, so no refusal follows an allocation.
/// (The operand-stack fit is asked separately, before either path.)
fn refusal_before_materialisation(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<DeoptFrameBail> {
    use cratonvm_jit::deopt::FrameValue;
    fn mappable(v: &FrameValue) -> bool {
        matches!(
            v,
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
        ) || fv_to_value(v).is_some()
    }
    // The same walk as `ir_deopt_locals`: a cat-2 value's upper half is skipped.
    let mut i = 0;
    while i < rframe.locals.len() {
        let v = &rframe.locals[i];
        if !mappable(v) {
            return Some(DeoptFrameBail::UnmappableLocal);
        }
        i += if matches!(v, FrameValue::Long(_) | FrameValue::Double(_)) {
            2
        } else {
            1
        };
    }
    if !rframe.stack.iter().all(mappable) {
        return Some(DeoptFrameBail::UnmappableStack);
    }
    let monitors_ok = rframe.monitors.iter().all(|m| match &m.object {
        FrameValue::Object(addr) => *addr != 0,
        FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_) => true,
        _ => false,
    });
    if !monitors_ok {
        return Some(DeoptFrameBail::BadMonitor);
    }
    None
}

/// The locks a stashed compiled frame's code TOOK (`MonitorInfo::relock ==
/// false`), pinned from the moment a sink takes the stash until the sink knows
/// whether the frame resumes (interpreter round i1 wave 23, lane L2;
/// `docs/internal/fixed-bugs/interpreter-L2-a-refused-resume-of-a-frame-holding-a-compiled-lock-leaks-the-lock-FIXED-20260926.md`).
///
/// An optimizing body inside a `synchronized` block holds the block's monitor
/// through `jit_monitor_enter`, and a frame it stashes there (a guard trap, a
/// back-edge mode exit) describes that hold with `relock == false`. When the
/// frame RESUMES, the rebuilt interpreter frame inherits the hold and its own
/// `monitorexit` releases it (`build_deopt_frame_inner`). When a sink REFUSES
/// the frame and re-runs the method from entry, nothing else ever releases it:
/// the re-run enters the monitor again and leaves it once, so the thread owns
/// the lock for good. HotSpot never refuses a deoptimization; its interpreter
/// frame owns exactly what the compiled frame held, so the hold is released
/// once whichever way the activation ends. A re-run from entry must start from
/// the state it assumes — no lock held by the abandoned activation — so every
/// refusal that re-runs releases what the frame took, innermost first.
///
/// The objects are pinned (`native_pin_roots`) because the sinks' refusal
/// arms can run after a collection: the stash rooted the frame's `Object`
/// words, and once taken out of it nothing does. A moving collector forwards
/// the pins in place, so [`Self::release_for_a_rerun`] releases the live
/// address. Elided locks (`relock == true`) were never taken and are left
/// alone; a frame with no compiled lock pins nothing and costs nothing.
///
/// Every scope of an inlined caller chain is walked (outermost first, so the
/// release, which runs backwards, is innermost first): the re-run abandons
/// all of them. A frame the sink RESUMES keeps its locks
/// ([`Self::unpin_keeping_the_locks`]); a frame put back into the stash for an
/// outer sink keeps them too, since that sink decides.
///
/// # The exceptional channel (wave 24, lane L2)
///
/// A reason-9 frame (`LAST_EXCEPTIONAL`) names the same holds, and its sinks
/// make the same decision one way or the other: a sink that builds the
/// handler frame hands the trapping scope's locks to it
/// ([`Self::seed_frame_and_unpin`] / [`Self::seed_pushed_frame_and_unpin`] —
/// javac's catch-all handler then releases them through the frame's own
/// record), and a sink that propagates the exception past the activation
/// without its handler releases them ([`Self::release_for_a_propagation`]),
/// which is what that handler would have done before rethrowing the same
/// exception.
#[must_use = "a pinned hold must be released or unpinned, or its pins leak"]
pub(crate) struct CompiledLocksOfAStash {
    /// `native_pin_roots` watermark below the first pin.
    pin_base: usize,
    /// How many times each pinned object was entered, in pin order.
    depths: Vec<u32>,
    /// Index in `depths` (and the pins) of the TRAPPING scope's first lock;
    /// the ones before it belong to inlined caller scopes, whose locks a
    /// frame rebuilt for the trapping scope alone must not record.
    own_from: usize,
    /// Index in `depths` (and the pins) where the TAKEN locks end: the pins
    /// from here on are the trapping scope's elided levels (`relock`), which
    /// the compiled code never took. No release or seeding reads them; only
    /// [`Self::replace_live_record_with_retaken_levels_and_unpin`] does, for
    /// the one transfer that re-takes them (interpreter round i1 wave 27,
    /// lane L2).
    retake_from: usize,
    /// Index in `depths` (and the pins) where the OUTERMOST scope's taken
    /// locks end (they start at 0: that scope is walked first). The live
    /// frame an OSR-exit transfer rewrites is that scope's frame, so its
    /// record is these ([`Self::live_frame_locks`]). Equal to `retake_from`
    /// for a single-scope frame, where the outermost scope IS the trapping
    /// one. Round 13 wave 8 (lane sync5; part (c) of
    /// `r13w6-chain2-osr-guard-exit-chain-lock-record-patch-FIXED-20260928.md`).
    outer_to: usize,
}

impl CompiledLocksOfAStash {
    /// Pin the locks `rframe`'s compiled code took. Call right after the stash
    /// is taken, before anything that can allocate.
    pub(crate) fn pin_compiled_locks(
        thread: &mut JvmThread,
        rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    ) -> Self {
        Self::pin_scopes(thread, rframe, false)
    }

    /// [`Self::pin_compiled_locks`], plus the trapping scope's elided levels
    /// (the OUTERMOST scope's for an inlined chain: the scope the live frame
    /// becomes; round 13 wave 8, lane sync5)
    /// (`relock`) after every taken lock, for the OSR door, whose planless
    /// transfer re-takes them and records them
    /// ([`Self::replace_live_record_with_retaken_levels_and_unpin`]). Nothing
    /// releases or seeds an elided level: every other method reads the taken
    /// pins only. Interpreter round i1 wave 27, lane L2.
    pub(crate) fn pin_compiled_locks_and_elided_levels(
        thread: &mut JvmThread,
        rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    ) -> Self {
        Self::pin_scopes(thread, rframe, true)
    }

    fn pin_scopes(
        thread: &mut JvmThread,
        rframe: &cratonvm_jit::deopt::ReconstructedFrame,
        elided_levels: bool,
    ) -> Self {
        use cratonvm_jit::deopt::FrameValue;
        let pin_base = thread.native_pin_roots.len();
        let mut depths = Vec::new();
        let mut own_from = 0;
        let mut outer_to = 0;
        // The scope whose frame an OSR-exit transfer writes into the live
        // frame (the first one walked below).
        let outermost = stash_identity_scope(rframe);
        let scopes = rframe
            .caller_frames
            .iter()
            .rev()
            .chain(std::iter::once(rframe));
        for scope in scopes {
            // The trapping scope is the last one walked.
            if std::ptr::eq(scope, rframe) {
                own_from = depths.len();
            }
            for m in &scope.monitors {
                if m.relock || m.lock_depth == 0 {
                    continue;
                }
                if let FrameValue::Object(addr) = &m.object {
                    let addr = *addr;
                    if addr != 0 {
                        // SAFETY: a non-null reference the stash resolved from
                        // the trapped frame; the stash rooted it until the
                        // take, and nothing has run since.
                        let obj = unsafe { ObjectRef::from_raw(addr as usize as *mut u8) };
                        thread.native_pin_roots.push(obj);
                        depths.push(m.lock_depth);
                    }
                }
            }
            if std::ptr::eq(scope, outermost) {
                outer_to = depths.len();
            }
        }
        // The elided levels of the scope the live frame becomes, after every
        // taken lock, when asked: kept current for the record of a transfer
        // that re-takes them, and read by nothing else (wave 27, lane L2).
        // That is the trapping scope for a single-scope frame, and the
        // OUTERMOST one for an inlined chain, whose inner scopes become pushed
        // frames (round 13 wave 8, lane sync5).
        let retake_from = depths.len();
        let elided: &[cratonvm_jit::deopt::MonitorInfo] = if elided_levels {
            &outermost.monitors
        } else {
            &[]
        };
        for m in elided {
            if !m.relock || m.lock_depth == 0 {
                continue;
            }
            if let FrameValue::Object(addr) = &m.object {
                let addr = *addr;
                if addr != 0 {
                    // SAFETY: a non-null reference the stash resolved from
                    // the trapped frame, rooted by the stash until the take;
                    // nothing has run since.
                    let obj = unsafe { ObjectRef::from_raw(addr as usize as *mut u8) };
                    thread.native_pin_roots.push(obj);
                    depths.push(m.lock_depth);
                }
            }
        }
        Self {
            pin_base,
            depths,
            own_from,
            retake_from,
            outer_to,
        }
    }

    /// The frame resumed, or went back into the stash: its locks stay held
    /// (by the resumed frame, or for the next sink). Drop the pins.
    pub(crate) fn unpin_keeping_the_locks(self, thread: &mut JvmThread) {
        if !self.depths.is_empty() {
            thread.native_pin_roots.truncate(self.pin_base);
        }
    }

    /// The trapping scope's pinned locks, `(object at its current address,
    /// lock_depth)`, outermost first; empty when the pins were truncated
    /// underneath (every sink below this one truncates to its own, higher
    /// watermark, so that is a caller defect, reported under
    /// `CRATONVM_DBG_DEOPT`).
    fn own_locks(&self, thread: &JvmThread) -> Vec<(ObjectRef, u32)> {
        let n = self.depths.len();
        if thread.native_pin_roots.len() < self.pin_base + n {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] a resumed frame's lock pins were truncated underneath; \
                     its record names none of its {n} compiled lock(s)"
                );
            }
            return Vec::new();
        }
        (self.own_from..self.retake_from)
            .map(|i| (thread.native_pin_roots[self.pin_base + i], self.depths[i]))
            .collect()
    }

    /// The OUTERMOST scope's pinned locks, `(object at its current address,
    /// lock_depth)`, outermost first: the record of the live frame an OSR-exit
    /// transfer rewrites, which is that scope's frame. For a single-scope
    /// frame exactly [`Self::own_locks`]; for an inlined chain the trapping
    /// scope's locks belong to the frames the chain transfer PUSHES, not to
    /// the live one (round 13 wave 8, lane sync5; part (c) of
    /// `r13w6-chain2-osr-guard-exit-chain-lock-record-patch-FIXED-20260928.md`).
    /// Empty when the pins were truncated underneath, as [`Self::own_locks`].
    fn live_frame_locks(&self, thread: &JvmThread) -> Vec<(ObjectRef, u32)> {
        if self.outer_to == self.retake_from && self.own_from == 0 {
            return self.own_locks(thread);
        }
        let n = self.depths.len();
        if thread.native_pin_roots.len() < self.pin_base + n {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] a transferred chain's lock pins were truncated underneath; \
                     the live record names none of its {n} compiled lock(s)"
                );
            }
            return Vec::new();
        }
        (0..self.outer_to.min(self.retake_from))
            .map(|i| (thread.native_pin_roots[self.pin_base + i], self.depths[i]))
            .collect()
    }

    /// A frame not yet pushed resumes the trapping scope (a compiled callee's
    /// handler frame): record the locks its compiled code took in the frame's
    /// `held_monitors`, `lock_depth` copies each, outermost first — the seeding
    /// [`build_deopt_frame_inner`] does for a guard trap's frame — then drop
    /// the pins. Nothing is entered or released. Interpreter round i1 wave 24,
    /// lane L2.
    ///
    /// The frame must be pushed before anything that can collect: from here the
    /// record is the frame's own reference to each object, like its locals.
    pub(crate) fn seed_frame_and_unpin(self, thread: &mut JvmThread, frame: &mut Frame) {
        if self.depths.is_empty() {
            return;
        }
        for (obj, depth) in self.own_locks(thread) {
            for _ in 0..depth {
                frame.held_monitors.push(obj);
            }
        }
        thread.native_pin_roots.truncate(self.pin_base);
    }

    /// [`Self::seed_frame_and_unpin`] for a frame the sink has already pushed,
    /// at `thread.frames[frame_idx]` (the handler frame
    /// `route_jit_exception_through_method` pushes). The pins kept the objects
    /// current across whatever ran since the push.
    pub(crate) fn seed_pushed_frame_and_unpin(self, thread: &mut JvmThread, frame_idx: usize) {
        if self.depths.is_empty() {
            return;
        }
        let locks = self.own_locks(thread);
        if let Some(frame) = thread.frames.get_mut(frame_idx) {
            for (obj, depth) in locks {
                for _ in 0..depth {
                    frame.held_monitors.push(obj);
                }
            }
        }
        thread.native_pin_roots.truncate(self.pin_base);
    }

    /// The exception leaves the activation without its handler having run (a
    /// reason-9 sink that propagates: no handler covers the throw, a frame it
    /// could not map or materialise, a monitor or frame it could not build).
    /// The activation never resumes, so what its compiled code locked is
    /// released exactly as for a re-run ([`Self::release_for_a_rerun`]) —
    /// javac's catch-all would have released it and rethrown the same
    /// exception. Returns how many single releases succeeded.
    pub(crate) fn release_for_a_propagation(self, shared: &SharedVm, thread: &mut JvmThread) -> u32 {
        self.release_pinned(shared, thread, None)
    }

    /// [`Self::release_for_a_propagation`] for an OSR'd activation, whose
    /// frame is the LIVE interpreter frame at `frame_idx` and not a stash-only
    /// description: that frame's `held_monitors` record can still name a lock
    /// the interpreter took before the OSR entry, which the compiled frame
    /// describes too (`relock == false`). Each release retires one matching
    /// record entry, so the unwind that follows cannot release the same hold
    /// again (`held_monitors::replace_unwound_exception_if_locked`).
    pub(crate) fn release_for_a_propagation_of_live_frame(
        self,
        shared: &SharedVm,
        thread: &mut JvmThread,
        frame_idx: usize,
    ) -> u32 {
        self.release_pinned(shared, thread, Some(frame_idx))
    }

    /// The frame is abandoned and its method re-runs from entry: release every
    /// pinned lock `lock_depth` times, innermost first, through the release a
    /// frame's `monitorexit` makes (`monitor_exit_and_retract_jmx`), then drop
    /// the pins. Returns how many single releases succeeded.
    ///
    /// A release the monitor table refuses (the thread does not own the
    /// object) stops that object's releases and is reported under
    /// `CRATONVM_DBG_DEOPT`; it can only mean the frame described a hold the
    /// thread no longer has, and releasing further could not be right.
    pub(crate) fn release_for_a_rerun(self, shared: &SharedVm, thread: &mut JvmThread) -> u32 {
        self.release_pinned(shared, thread, None)
    }

    /// The release both abandon paths share; `live_frame` names a live frame
    /// whose record entries each release retires (see
    /// [`Self::release_for_a_propagation_of_live_frame`]).
    fn release_pinned(
        self,
        shared: &SharedVm,
        thread: &mut JvmThread,
        live_frame: Option<usize>,
    ) -> u32 {
        if self.depths.is_empty() {
            return 0;
        }
        let n = self.depths.len();
        let mut released = 0u32;
        // The pins must still be where they were put: every sink below this
        // one truncates to its own, higher watermark.
        if thread.native_pin_roots.len() >= self.pin_base + n {
            for i in (0..self.retake_from).rev() {
                let obj = thread.native_pin_roots[self.pin_base + i];
                for _ in 0..self.depths[i] {
                    if let Some(f) = live_frame.and_then(|idx| thread.frames.get_mut(idx)) {
                        let _ = f.held_monitors.remove_newest(obj);
                    }
                    match crate::vm::vm_exec::monitor_exit_and_retract_jmx(
                        shared,
                        obj,
                        thread.thread_id,
                    ) {
                        Ok(()) => released += 1,
                        Err(_) => {
                            if dbg_deopt_enabled() {
                                eprintln!(
                                    "[cratonvm-deopt] refused frame named a compiled lock \
                                     this thread does not hold; not released further"
                                );
                            }
                            break;
                        }
                    }
                }
            }
        } else if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] refused frame's lock pins were truncated underneath; \
                 {n} compiled lock(s) not released"
            );
        }
        thread.native_pin_roots.truncate(self.pin_base);
        if released > 0 && dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] released {released} compiled lock hold(s) of a refused frame \
                 before its re-run"
            );
        }
        released
    }

    /// An OSR'd activation's frame is abandoned but its LIVE interpreter frame
    /// at `frame_idx` carries on (the no-exception sentinel arm of `try_osr`,
    /// whose continuing arms resume the frame at its entry pc): release the
    /// holds the frame names BEYOND what that frame's `held_monitors` record
    /// already accounts for, innermost first, then drop the pins. Returns how
    /// many single releases succeeded. Interpreter round i1 wave 25, lane L2.
    ///
    /// The record is the interpreter's own account of the locks it took
    /// before the OSR entry (`try_osr_with_backoff` empties it only after a
    /// body that returned, threw out, or committed a transfer), and the entry
    /// contract lists each of them in the compiled frame too, `relock ==
    /// false`. Those stay with the frame, whose pc is still inside their
    /// blocks; the rest were taken by the compiled body after the entry and
    /// nothing else will release them. Each object's record count is spent on
    /// its outermost entries first, as the interpreter took those first.
    ///
    /// # The record may under-state (interpreter round i1 wave 28, lane L2)
    ///
    /// The record is allowed to UNDER-state (`held_monitors`' module doc): a
    /// committed single-pass OSR exit whose frame names no lock empties it
    /// while the frame, parked at the exit, may still be inside a block whose
    /// lock it holds. Trusted alone, the record then called that hold "taken
    /// by the body", and a later OSR body of the same activation that left
    /// through a reason-9 pad released the interpreter's own lock
    /// (`docs/internal/fixed-bugs/interpreter-L2-release-beyond-live-record-trusts-an-under-stated-record-FIXED-20260929.md`).
    /// So an object `entry` names — the thread owned it at the OSR entry,
    /// through one of the frame's reference locals ([`OsrEntryHolds`]) — is
    /// released at most as often as the thread's hold count of it GREW since
    /// that entry: the frame resumes at the entry pc, and the lock state it
    /// assumes there is the one the entry saw. That count comes from the
    /// monitor table, which cannot under-state. An object the snapshot does
    /// not name keeps the record rule, as before.
    pub(crate) fn release_beyond_live_record(
        self,
        shared: &SharedVm,
        thread: &mut JvmThread,
        frame_idx: usize,
        entry: &OsrEntryHolds,
    ) -> u32 {
        if self.depths.is_empty() {
            return 0;
        }
        let n = self.depths.len();
        let mut released = 0u32;
        // Holds of each snapshot object the body gained since the entry;
        // read before the first release below.
        let mut gained = entry.gains_since_entry(shared, thread, frame_idx);
        let mut kept_by_entry = 0u32;
        if thread.native_pin_roots.len() >= self.pin_base + n {
            // How many holds of each pinned entry the record accounts for.
            let mut kept: Vec<u32> = Vec::with_capacity(n);
            {
                let record: &[ObjectRef] = match thread.frames.get(frame_idx) {
                    Some(f) => f.held_monitors.as_slice(),
                    None => &[],
                };
                let mut spent: Vec<(ObjectRef, usize)> = Vec::new();
                for i in 0..self.retake_from {
                    let obj = thread.native_pin_roots[self.pin_base + i];
                    let in_record = record.iter().filter(|&&o| o == obj).count();
                    let already = spent.iter().find(|(o, _)| *o == obj).map_or(0, |(_, c)| *c);
                    let keep = in_record
                        .saturating_sub(already)
                        .min(usize::try_from(self.depths[i]).unwrap_or(usize::MAX));
                    match spent.iter_mut().find(|(o, _)| *o == obj) {
                        Some(entry) => entry.1 += keep,
                        None => spent.push((obj, keep)),
                    }
                    kept.push(u32::try_from(keep).unwrap_or(u32::MAX));
                }
            }
            for i in (0..self.retake_from).rev() {
                let obj = thread.native_pin_roots[self.pin_base + i];
                let mut left = self.depths[i].saturating_sub(kept[i]);
                while left > 0 {
                    // Wave 28: never below the entry's hold count.
                    if let Some(budget) = gained.iter_mut().find(|(o, _)| *o == obj) {
                        if budget.1 == 0 {
                            kept_by_entry += left;
                            break;
                        }
                        budget.1 -= 1;
                    }
                    left -= 1;
                    match crate::vm::vm_exec::monitor_exit_and_retract_jmx(
                        shared,
                        obj,
                        thread.thread_id,
                    ) {
                        Ok(()) => released += 1,
                        Err(_) => {
                            if dbg_deopt_enabled() {
                                eprintln!(
                                    "[cratonvm-deopt] an OSR'd body's abandoned frame named a \
                                     compiled lock this thread does not hold; not released further"
                                );
                            }
                            break;
                        }
                    }
                }
            }
        } else if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] an OSR'd body's lock pins were truncated underneath; \
                 {n} compiled lock(s) not released"
            );
        }
        thread.native_pin_roots.truncate(self.pin_base);
        if kept_by_entry > 0 && dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] kept {kept_by_entry} hold(s) an OSR'd body's abandoned frame \
                 named beyond the live record: the frame held them at the OSR entry"
            );
        }
        released
    }

    /// A frame resumed IN PLACE from an OSR'd body — the live interpreter
    /// frame at `frame_idx`, rewritten by an OSR-exit transfer — holds exactly
    /// the locks the exit frame names: REPLACE its `held_monitors` record with
    /// the trapping scope's pinned locks (`lock_depth` copies each, outermost
    /// first), then drop the pins. Nothing is entered or released.
    /// Interpreter round i1 wave 25, lane L2 (stage 2 of proposal
    /// `i23-L7-proposal-per-frame-locked-monitors`, the OSR half).
    ///
    /// Replaced, not added to: the record held the locks the interpreter took
    /// before the entry, and the compiled body may have released some of them
    /// since; the exit frame's description includes every one still held (the
    /// entry contract lists them `relock == false`). A frame that describes
    /// no taken lock (a single-pass body's where its wave-26 monitor analysis
    /// has no answer) leaves the record
    /// empty, which is what the transfers did before (`try_osr_with_backoff`
    /// emptied it): an under-stated record is the safe direction
    /// (`held_monitors`' module doc).
    pub(crate) fn replace_live_record_and_unpin(self, thread: &mut JvmThread, frame_idx: usize) {
        let locks = if self.depths.is_empty() {
            Vec::new()
        } else {
            self.live_frame_locks(thread)
        };
        if let Some(frame) = thread.frames.get_mut(frame_idx) {
            frame.held_monitors.clear();
            for (obj, depth) in locks {
                for _ in 0..depth {
                    frame.held_monitors.push(obj);
                }
            }
        }
        if !self.depths.is_empty() {
            thread.native_pin_roots.truncate(self.pin_base);
        }
    }

    /// [`Self::replace_live_record_and_unpin`] for a transfer that RE-TOOK
    /// the exit frame's elided levels (`transfer_osr_guard_exit_into_live_frame`,
    /// wave 26): the record names the taken locks and then each re-taken
    /// level, `lock_depth` copies each. Interpreter round i1 wave 27, lane L2.
    ///
    /// Until wave 27 the re-taken levels were left out, which is not the
    /// safe direction it was taken for once a later sink counts the record:
    /// after `synchronized (o) { synchronized (o) { loop1 } loop2 }` left at
    /// `loop1`'s header (record `[o]`, table 2), the interpreter's inner
    /// `monitorexit` spent the record's one entry, so at `loop2` the frame
    /// held `o` once with an EMPTY record; a later OSR body entered there
    /// that left through a reason-9 pad with no exception pending
    /// ([`release_locks_of_own_pad_exits`] with the live frame, or the chain
    /// refusal of `try_osr`) released every hold beyond the record
    /// ([`Self::release_beyond_live_record`]) — the interpreter's own hold of
    /// `o` — and the frame's outer `monitorexit` then threw
    /// `IllegalMonitorStateException` with the lock already free.
    ///
    /// Round 13 wave 8 (lane sync5): the record is the OUTERMOST scope's
    /// ([`Self::live_frame_locks`]), and so are the re-taken levels
    /// ([`Self::pin_compiled_locks_and_elided_levels`]); identical for a
    /// single-scope frame. An inlined chain's inner scopes become pushed
    /// frames, which the chain transfer seeds with their own locks.
    pub(crate) fn replace_live_record_with_retaken_levels_and_unpin(
        self,
        thread: &mut JvmThread,
        frame_idx: usize,
    ) {
        let n = self.depths.len();
        let mut locks = if n == 0 {
            Vec::new()
        } else {
            self.live_frame_locks(thread)
        };
        if n > self.retake_from && thread.native_pin_roots.len() >= self.pin_base + n {
            for i in self.retake_from..n {
                locks.push((thread.native_pin_roots[self.pin_base + i], self.depths[i]));
            }
        }
        if let Some(frame) = thread.frames.get_mut(frame_idx) {
            frame.held_monitors.clear();
            for (obj, depth) in locks {
                for _ in 0..depth {
                    frame.held_monitors.push(obj);
                }
            }
        }
        if n != 0 {
            thread.native_pin_roots.truncate(self.pin_base);
        }
    }
}

/// Release the compiled locks of the reason-9 frames the activation a door
/// just ran published on its way out, when no exception is pending: the
/// no-exception sentinel arm of `jit_bridge::run_jit_body_raw` (every
/// method-entry door) and of `try_osr`. Returns how many single releases
/// succeeded. Interpreter round i1 wave 25, lane L2
/// (`docs/internal/fixed-bugs/interpreter-L2-a-dropped-own-reason-9-frame-leaves-its-compiled-locks-held-for-the-rerun-FIXED-20260927.md`).
///
/// A protected call whose callee answered the DEOPT sentinel (a declined
/// callee-deopt service, a dispatch helper that raised only the deopt flag)
/// still reaches the caller's reason-9 pad, which publishes the caller's
/// frame and returns the sentinel without running a `monitorexit`. With no
/// exception pending, no handler runs and nothing resumes that frame, so it
/// is the only description of the locks the activation's compiled code took
/// (`relock == false`) and still holds; dropping it unreleased left the thread
/// owning them after the door re-ran the method from entry.
///
/// Claimed from the top of the stash while a frame sits above `floor` (the
/// depth the door recorded before entering the body), names this method and
/// was published by the body the door ran (`owns_point`); the first frame that
/// is not one stops the walk. Every claimed frame belongs to an activation
/// that left through its pad — the door's own, or a nested activation of the
/// same body whose caller passed the sentinel on — so each one's locks are
/// released, innermost first. With `live_frame` (the OSR door), the live
/// interpreter frame outlives the body: only the holds its record does not
/// already name are released ([`CompiledLocksOfAStash::release_beyond_live_record`]).
///
/// A frame below the floor is an earlier activation's, whose own sink decided
/// its locks' fate; it is never claimed here.
#[allow(clippy::too_many_arguments)]
pub(super) fn release_locks_of_own_pad_exits(
    shared: &SharedVm,
    thread: &mut JvmThread,
    owns_point: &dyn Fn(usize) -> bool,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    floor: usize,
    live_frame: Option<(usize, &OsrEntryHolds)>,
) -> u32 {
    let mut released = 0u32;
    while let Some((rframe, _cause, _point)) =
        cratonvm_jit::deopt::take_exceptional_frame_above_if(floor, |frame, point| {
            owns_point(point) && deopt_frame_matches_method(frame, class_name, method_name, descriptor)
        })
    {
        // Pinned right after the take, before anything that can allocate.
        let locks = CompiledLocksOfAStash::pin_compiled_locks(thread, &rframe);
        released += match live_frame {
            Some((frame_idx, entry)) => {
                locks.release_beyond_live_record(shared, thread, frame_idx, entry)
            }
            None => locks.release_for_a_rerun(shared, thread),
        };
    }
    if released > 0 && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] released {released} compiled lock hold(s) of {class_name}.\
             {method_name}{descriptor}'s reason-9 frame(s) published for a deopt sentinel"
        );
    }
    released
}

/// The locks the LIVE interpreter frame's thread owned at an OSR entry,
/// through the frame's reference locals: `(local slot, the thread's hold
/// count of the object in it)`, one per distinct owned object. Taken by
/// `jit_bridge::try_osr` before the body runs, and read only by
/// [`CompiledLocksOfAStash::release_beyond_live_record`], which releases a
/// named object at most as often as the thread's hold count of it grew since.
/// Interpreter round i1 wave 28, lane L2
/// (`docs/internal/fixed-bugs/interpreter-L2-release-beyond-live-record-trusts-an-under-stated-record-FIXED-20260929.md`).
///
/// # Why the locals and the monitor table
///
/// The frame's `held_monitors` record may under-state (a committed
/// single-pass exit that names no lock empties it), and the artifact's entry
/// contract names its pre-entry locks by machine location, which the
/// interpreter cannot read before the body runs. javac keeps every block
/// lock in a local for the whole block (`aload o; dup; astore t;
/// monitorenter`), and the monitor table's count cannot under-state. The
/// count includes holds of the same object by CALLER frames; the release rule
/// compares it with the count at the abandon point, so those cancel out.
///
/// # GC
///
/// Slots, not addresses: the live frame's locals are roots every collector
/// remaps, and nothing writes them between the entry and the two readers (the
/// no-exception sentinel arm continues the frame without a transfer, and the
/// chain refusal comes after a transfer that refuses before its first write).
/// A slot that no longer holds a reference when read back (the opt-in
/// `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS` nulls them) is skipped, and its object
/// falls back to the record rule.
///
/// # Cost
///
/// One pass over the frame's locals per OSR entry: a tag read per slot and a
/// mark-word read (`MonitorTable::holds`) per non-null reference; nothing is
/// allocated unless the thread owns one of them. An entry already builds the
/// seeded locals and tags from the same slots and walks every deopt point of
/// the artifact in `validate_osr_entry`.
#[derive(Debug, Default)]
pub(crate) struct OsrEntryHolds {
    slots: Vec<(usize, u32)>,
}

impl OsrEntryHolds {
    /// Snapshot `thread.frames[frame_idx]` at an OSR entry (see the type).
    pub(crate) fn snapshot(shared: &SharedVm, thread: &JvmThread, frame_idx: usize) -> Self {
        let mut slots: Vec<(usize, u32)> = Vec::new();
        let Some(frame) = thread.frames.get(frame_idx) else {
            return Self { slots };
        };
        let monitors = &shared.threads.monitors;
        for i in 0..frame.locals_len() {
            // The kind-honouring tag first, as `park_osr_entry_reference_locals`
            // reads them: a category-2 value is not a reference.
            if frame.get_local_tag(i) != cratonvm_types::VTAG_OBJECT {
                continue;
            }
            let Value::Object(Some(obj)) = frame.get_local_unchecked(i) else {
                continue;
            };
            if !monitors.holds(obj, thread.thread_id) {
                continue;
            }
            let seen = slots
                .iter()
                .any(|&(j, _)| matches!(frame.get_local_unchecked(j), Value::Object(Some(o)) if o == obj));
            if !seen {
                slots.push((i, monitors.entry_count(obj)));
            }
        }
        Self { slots }
    }

    /// Each snapshot object at its current address (read back through the
    /// live frame's slot), with how many holds of it the thread gained since
    /// the entry (0 when it holds it no more often, or no longer at all).
    fn gains_since_entry(
        &self,
        shared: &SharedVm,
        thread: &JvmThread,
        frame_idx: usize,
    ) -> Vec<(ObjectRef, u32)> {
        if self.slots.is_empty() {
            return Vec::new();
        }
        let Some(frame) = thread.frames.get(frame_idx) else {
            return Vec::new();
        };
        let monitors = &shared.threads.monitors;
        let mut gained: Vec<(ObjectRef, u32)> = Vec::with_capacity(self.slots.len());
        for &(i, at_entry) in &self.slots {
            if i >= frame.locals_len() || frame.get_local_tag(i) != cratonvm_types::VTAG_OBJECT {
                continue;
            }
            let Value::Object(Some(obj)) = frame.get_local_unchecked(i) else {
                continue;
            };
            let now = if monitors.holds(obj, thread.thread_id) {
                monitors.entry_count(obj)
            } else {
                0
            };
            gained.push((obj, now.saturating_sub(at_entry)));
        }
        gained
    }
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
///   (`jit-callsite-b`) and [`resume_deopted_body_at_point`] re-ran the whole method from
///   entry, with no side-effect check — so a body that had already committed a
///   store committed it a second time, silently.
///
/// A deopt sentinel does NOT mean "nothing happened": the compiled body ran up
/// to `bci` and stopped. Re-entering at bci 0 re-executes everything before it.
/// [`resume_deopted_body_at_point`]'s own doc says exactly that, and names what it cost —
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
/// * a body that **takes a monitor at all**. The optimizing tier's frame states
///   have recorded the monitor stack since 2026-09-12 (with a `relock` marker
///   for elided locks), but the SINGLE-PASS backend describes only the monitors
///   it scalar-replaced, and an elision that left no `MonitorInfo` would leave
///   no trace in the frame at all. Note the mood: that backend performs no such
///   elision today — `has_elided_monitor` is assigned `false` once and `true`
///   never, so the conjunct that would report one constant-folds (see the note
///   on `can_deopt_resume` in `jit/src/x64/driver.rs`, and
///   `docs/internal/fixed-bugs/r10-earelock-has-elided-monitor-is-never-set-FIXED-20260922.md`).
///   The refusal is kept because it does not depend on that: this arm cannot
///   tell the two backends' artifacts apart, and eliding is a codegen decision,
///   not a bytecode rewrite, so the ops are still there to see — which is what
///   keeps it holding on the day an untraceable elision is added. An
///   optimizing-tier body with monitors resumes precisely through the
///   `can_deopt_resume` arm instead;
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
    // The bound is on the bci in `cached`'s own code: an inlined chain's
    // `rframe.bci` is in a spliced callee's (wave 26, lane L2).
    sink_precise_resume_allowed(
        &cached.code,
        cached.code.len(),
        cached.is_synchronized,
        stash_identity_scope(rframe).bci,
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
/// `native_pin_roots` pins are held ACROSS `push_resumed_frame` and released
/// only AFTER the push — once
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
///
/// `Err` (round 12 wave 5, lane replay3) is the one refusal with an answer
/// other than a re-run: the scalar-replaced objects could not be re-allocated
/// because the heap is exhausted ([`DeoptFrameBail::VirtualMaterialiseHeap`]).
/// The monitors the compiled code held are released and the returned
/// `OutOfMemoryError` is the trapped method's outcome, thrown at its caller, as
/// HotSpot pops the frame. Every caller propagates it; none re-runs.
///
/// `hints` are the trapping artifact's scope class ids for an inlined chain
/// ([`build_deopt_frame_chain_hinted`]); `&[]` resolves every scope by name.
/// `own_sources` are its inner scopes' own-source templates
/// ([`chain_inner_scope_own_sources`]); `&[]` for none.
pub(super) fn resume_real_ir_deopt_or_throw(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    // No nested-stash refusal here, and deliberately. `rframe` is a re-execute
    // point (the only kind the ordinary stash admits), so if its bci holds an
    // invoke, that invoke has NOT run and re-executing it is the correct
    // resume. Anything the take discarded beneath it was a stale leftover, not
    // a callee — see `cratonvm_jit::deopt::deopt_stash_nesting_counts`.
    // N2 — an inlined caller chain is materialised and pushed, not refused.
    // `build_deopt_frame_chain` validates without pushing and without
    // pinning, and `push_inlined_chain` pushes each frame's oops directly into
    // the frame that roots them; only a chain naming scalar-replaced objects
    // pins, between their re-allocation and the push (its own watermark
    // below). A refusal still falls through to the whole-method re-run, so
    // the worst case is exactly the old behaviour.
    if !rframe.caller_frames.is_empty() {
        let trace = dbg_deopt_enabled();
        let Some(mut chain) =
            build_deopt_frame_chain_sourced(shared, cached, rframe, hints, own_sources)
        else {
            return Ok(None);
        };
        // Round 13 wave 5 (lane resume): a chain naming scalar-replaced
        // objects re-allocates them here, so a full heap gets the single
        // frame's answer (`OutOfMemoryError` at the caller). The chain holds
        // no monitor (`chain_virtuals_refusal`), so there is no lock to
        // release for the abandoned activation. Its pins end at the push.
        let pin_base = thread.native_pin_roots.len();
        if let Err(why) = materialise_chain_virtuals(shared, thread, &mut chain) {
            thread.native_pin_roots.truncate(pin_base);
            if frame_build_heap_failure_is_answered(why) {
                return Err(rematerialisation_out_of_memory(shared, thread));
            }
            return Ok(None);
        }
        let pushed = push_inlined_chain(shared, thread, chain, trace);
        if thread.native_pin_roots.len() > pin_base {
            thread.native_pin_roots.truncate(pin_base);
        }
        return Ok(pushed);
    }
    let pin_base = thread.native_pin_roots.len();
    match build_deopt_frame_or_refusal(shared, thread, cached, rframe, false, true) {
        Ok(frame) => {
            // Push FIRST, pins STILL installed: during the push the oops are
            // rooted by the pins, and once pushed also by the frame. Only THEN
            // release the pins — the frame roots them from here on. No
            // MethodEntry: the method was entered in compiled code.
            push_resumed_frame(thread, frame);
            thread.native_pin_roots.truncate(pin_base);
            Ok(Some(CachedCallResult::FramePushed))
        }
        Err(why) => {
            // Out-of-scope / unmappable: release any partial pins, fall back to
            // the whole-method re-run. A full heap is answered instead.
            thread.native_pin_roots.truncate(pin_base);
            if frame_build_heap_failure_is_answered(why) {
                return Err(rematerialisation_out_of_memory(shared, thread));
            }
            Ok(None)
        }
    }
}

/// The pre-round-12-wave-5 shape of [`resume_real_ir_deopt_or_throw`], which
/// the unit tests below were written against (none of them exhausts the heap).
#[cfg(test)]
pub(super) fn resume_real_ir_deopt(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<CachedCallResult> {
    resume_real_ir_deopt_or_throw(shared, thread, cached, rframe, &[], &[])
        .ok()
        .flatten()
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
            if dbg_deopt_enabled() {
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
    let trace = dbg_deopt_enabled();

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
            shared,
            thread,
            frame_idx,
            rframe,
            artifact,
            ChainExitProvenance::RecordedBci,
        )
        .map(|_| ());
    }
    // The in-place transfer has no relock path; a lock the compiled code took
    // (`relock == false`) is still held and stays with the live frame.
    if rframe.monitors.iter().any(|m| m.relock) {
        return Err("held monitors that must be re-acquired".to_string());
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
    // way an unmappable STACK slot does, and it never refuses here: refusing
    // after the OSR'd body ran is the double execution described below.
    //
    // What makes leaving the live value in place sound is decided at COMPILE
    // time, where the deopt point is published
    // (`jit/src/x64/deopt_stubs.rs::build_frame_state_at`). A local that is not
    // live-in at the point's bci (handler-aware bytecode liveness) is published
    // `Undefined`, however the whole-method classifier typed it. A live local
    // of a slot reused as two kinds is described through its per-bci kind.
    // Only a live local that cannot be described is published `Unsupported`,
    // and one of those anywhere in the artifact makes `validate_osr_entry`
    // refuse the ENTRY (`osr_exit_policy`) before any iteration runs. An
    // `Unsupported` local that still reaches an admitted transfer comes from a
    // resolve-time metadata defect (`deopt::resolve_value`), not from a slot
    // the classifier could not type. See
    // `jit-resume-tolerates-unsupported-locals-without-liveness-FIXED-20260912.md`.
    // Previously this fell through to `bail("unmappable local")` on
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
///  * **The bci is checked against a RETHROW point.** The resume transfer
///    spends `resume_after_exit` on that check; here it is made through
///    `osr_exit::exceptional_reason_at_bci`, the rethrow half of that module's
///    two-way partition of the point list. A frame whose bci this artifact
///    recorded no rethrow point for is a mis-routed stash, and routing a
///    mis-routed stash into a handler would enter it with another site's
///    locals.
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
    // As in the sibling: only a monitor the transfer would have to acquire.
    if rframe.monitors.iter().any(|m| m.relock) {
        return Err("held monitors that must be re-acquired".to_string());
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
    //
    // Asked of `semantics.rethrow_exception`, not of `reason ==
    // PendingException` (round 10 wave 9, closing
    // `docs/jit/deopt-frame-state-interning.md` §5.2's consumer half). The flag
    // is the field that means "this bci is not a resume point", which is
    // exactly the property this check needs; `reason` is the charging key. The
    // two are equivalent on any installed artifact — `DeoptVerifier`'s
    // exception-state-agreement lane refuses a point where they disagree, on
    // both backends' install paths in release builds — so this is the same set
    // of points, asked for the right reason.
    //
    // Asked through `osr_exit::exceptional_reason_at_bci` rather than by an
    // inline `any(..)`, and that is not cosmetic. The two bci lookups in that
    // module PARTITION the point list — `deopt_reason_at_bci` skips the rethrow
    // points, this one skips everything else — and this sink is the half of the
    // partition that had no production caller, which left the split asserted
    // only by `jit`'s own unit tests. A lookup with no caller reads as a guard
    // that is in place and holding when it has never run; routing the sink
    // through it makes the two halves symmetric at the call sites as well as in
    // the module. See
    // `docs/internal/retired/r10-offsetkey-exceptional-reason-at-bci-is-unwired-and-cannot-refuse-20260921-RETIRED-20260922.md`.
    //
    // `Ambiguous` is ACCEPTED here, unlike at the `deopt_reason_at_bci` sinks
    // that fall back to `UncommonTrap`. Those ask "which reason do I charge?",
    // a question two disagreeing points genuinely cannot answer. This one asks
    // "did this artifact record a rethrow point at this bci?", and two rethrow
    // points at one bci both answer yes. It is unreachable today —
    // `ResumeSemantics::for_reason` maps exactly one reason to `RETHROW`, pinned
    // by `osr_exit.rs::tests::the_exceptional_lookup_has_no_reachable_ambiguity`
    // — but the arm is written for what it would mean rather than left to be
    // decided under the pressure of a second `RETHROW` reason arriving.
    match cratonvm_jit::osr_exit::exceptional_reason_at_bci(&artifact.deopt_points, rframe.bci) {
        cratonvm_jit::osr_exit::ReasonAtBci::Unique(_)
        | cratonvm_jit::osr_exit::ReasonAtBci::Ambiguous => {}
        cratonvm_jit::osr_exit::ReasonAtBci::None => {
            return Err(format!(
                "exit bci {} is not a RETHROW (PendingException) point of this artifact",
                rframe.bci
            ));
        }
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
    // exactly the argument the sibling makes: a slot that is dead at the
    // throwing bci is published `Undefined` at compile time, and a live slot
    // that cannot be described refuses the OSR entry at admission, so an
    // admitted artifact never carries one here.
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
    if dbg_deopt_enabled() {
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

/// Tier proposal 27, phase 3 (round 11 wave 12, lane irexc): transfer a GUARD
/// exit of an optimizing OSR body into the live interpreter frame and park it
/// at the guard's bci, which re-executes the guarded bytecode. Returns that bci.
///
/// The third sibling of [`transfer_osr_exit_into_live_frame_checked`], for the
/// entry that has no `OsrEntryPlan`. The resume authority is
/// `osr_exit::guard_exit_resume_bci`: the stashed point, found by the
/// address `ir_deopt_entry` recorded, must be one of THIS artifact's guard
/// boxes at a bci the lowerer admitted (`ir_osr_guard_exit_bcis`), whose frame
/// it proved resumable in place at compile time. Everything else is the
/// sibling's in-place write — locals slot for slot, the operand stack rebuilt,
/// `Unsupported` locals left alone for the reason that function gives.
///
/// Why resuming here and re-entering later is sound is the lowerer's
/// classification, not this function's: see the phase-3 note in
/// `jit/src/osr_exit.rs`. Fail-closed like the siblings: every refusal happens
/// before the first write to the live frame.
pub(super) fn transfer_osr_guard_exit_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    point_addr: usize,
) -> Result<usize, String> {
    use cratonvm_jit::deopt::FrameValue;

    // Provenance and the resume bci first: nothing below is worth checking of
    // a frame that is not this artifact's admitted guard exit.
    let resume_bci = cratonvm_jit::osr_exit::guard_exit_resume_bci(
        &artifact._deopt_point_boxes,
        artifact.ir_osr_guard_exit_bcis.as_deref(),
        point_addr,
        rframe.bci,
    )
    .map_err(|why| format!("unresumable guard exit: {why}"))?;

    // The admitted frame's shape, re-asserted on what was reconstructed.
    //
    // Round 13 wave 8 (lane chain3; `r13w6-chain2-osr-guard-exit-chain-lock-record-patch`
    // part (b)): an inlined chain -- a guard inside a spliced body -- is
    // transferred as the planned OSR exit transfers one: the OUTERMOST scope
    // written into this live frame, parked after its `invoke`, and the
    // spliced callees pushed above it. `guard_exit_resume_bci` above proved
    // the point is this artifact's and was admitted
    // (`osr_exit::chain_resumable_in_place`: no monitor and no scalar-replaced
    // object in any scope). The returned pc is the live frame's.
    if !rframe.caller_frames.is_empty() {
        return transfer_osr_guard_exit_chain_into_live_frame(
            shared, thread, frame_idx, rframe, artifact,
        );
    }
    // Interpreter round i1 wave 25, lane L2: a back-edge POLL exit may name
    // the locks the frame holds (`osr_exit::poll_exit_point_resumable`, which
    // `guard_exit_resume_bci` asked above; a guard exit's point still names
    // none). Each is `relock == false`, a hold the thread already has for
    // this activation, so it stays with the live frame (the door rewrites the
    // frame's record from them).
    //
    // Interpreter round i1 wave 26, lane L2: a poll exit may also name an
    // ELIDED level (`relock`: a nested-lock elision's re-take, or a lock
    // escape analysis removed; `osr_exit::OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED`),
    // which the interpreter frame will release and so must hold. It is
    // re-taken below, after every fallible step, as `build_deopt_frame_inner`
    // re-takes one for a method-entry exit; here each such object must
    // already be a resolved, non-null reference. (`guard_exit_resume_bci`
    // above admitted the point through `poll_exit_point_resumable`, which
    // names no elided level with that switch off, and a guard exit's none.)
    let mut retake: Vec<(ObjectRef, u32)> = Vec::new();
    for m in rframe.monitors.iter().filter(|m| m.relock) {
        match &m.object {
            FrameValue::Object(addr) if *addr != 0 => {
                // SAFETY: a non-null reference the stash resolved from the
                // exit frame's own word; nothing that can collect runs between
                // the take and the re-take below (the same window the locals
                // written below rely on).
                let obj = unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) };
                retake.push((obj, m.lock_depth));
            }
            _ => return Err("an elided lock level on an unresolved object".to_string()),
        }
    }
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
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_) => {
                Some(format!("virtual-object slot ({region} {i})"))
            }
            FrameValue::MaterializationRequired(ev) => {
                Some(format!("materialization required ({region} {i}: {ev})"))
            }
            _ => None,
        })
    {
        return Err(what);
    }

    if cratonvm_jit::deopt_verify_enabled() {
        let (max_locals, max_stack) = {
            let frame = &thread.frames[frame_idx];
            (frame.max_locals, frame.max_stack)
        };
        let verdict = verify_reconstructed_frame(rframe, max_locals, max_stack)
            .and_then(|()| verify_reconstructed_oops(rframe, shared));
        if let Err(why) = verdict {
            eprintln!(
                "[DEOPT-VERIFY] OSR guard-exit bci={}: {why} — refusing the transfer",
                rframe.bci
            );
            return Err(format!("deopt-verify: {why}"));
        }
    }

    {
        let frame = &thread.frames[frame_idx];
        if rframe.locals.len() > frame.locals_len() || rframe.stack.len() > frame.max_stack as usize
        {
            return Err("slot overflow".to_string());
        }
    }

    // Map, then write — the sibling's 1:1 (non-collapsing) mapping.
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
    let stack_vals = match ir_deopt_frame_values(&rframe.stack) {
        Some(s) => s,
        None => return Err("unmappable stack slot".to_string()),
    };

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
    frame.pc = resume_bci;

    if super::osr_frame_trace::enabled() {
        super::osr_frame_trace::record_exit(frame, resume_bci);
    }
    // The elided levels (wave 26), after every fallible step: `lock_depth`
    // re-entrant enters each, published for `getLockedMonitors()` as every
    // acquisition is. Uncontended by construction — the thread holds the
    // object already (a nested elision's re-take) or the object never escaped
    // it (an escape-analysis elision) — so nothing blocks or collects. The
    // door's record rewrite names these levels too since wave 27
    // (`CompiledLocksOfAStash::replace_live_record_with_retaken_levels_and_unpin`):
    // a record short of them let `release_beyond_live_record` release the
    // interpreter's own hold later (see that function).
    for &(obj, depth) in &retake {
        for _ in 0..depth {
            shared.threads.monitors.enter(obj, thread.thread_id);
        }
        if depth > 0 {
            shared
                .threads
                .thread_registry
                .complete_jmx_monitor_enter(thread.thread_id, obj);
        }
    }
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR guard-exit TRANSFER into live frame: resume bci={resume_bci} \
             ({} locals, {} stack, {} elided level(s) re-taken)",
            locals.len(),
            stack_vals.len(),
            retake.len(),
        );
    }
    Ok(resume_bci)
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
///
/// Returns the pc the live frame is parked at (the outermost `invoke`'s
/// successor). The frames it pushes sit ABOVE `frame_idx`, so the caller's
/// interpreter loop must continue in the top frame, not in the live one
/// (`interpreter::try_osr_offer`, round 13 wave 8, lane chain3).
fn transfer_osr_exit_chain_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
    provenance: ChainExitProvenance,
) -> Result<usize, String> {
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
    // `outer_locals` is COMPACT (one entry per value, a `long`/`double` once);
    // written into the live frame it spans its JVM slots
    // (`write_compact_locals_in_place`), so the fit is asked in slots.
    let outer_slots = compact_locals_slot_count(&outer_locals);
    if outer_slots > live_max_locals || outer_stack.len() > live_max_stack {
        return Err(format!(
            "outermost scope {} does not fit the live frame ({} local slots / {} stack against {}/{})",
            outer_scope.method_key,
            outer_slots,
            outer_stack.len(),
            live_max_locals,
            live_max_stack
        ));
    }

    // ── every scope beneath it, fully resolved before anything is written ──
    // By the artifact's own scope class ids (round 13 wave 6, lane chain2),
    // and never from a spliced callee's class redefined since the compile:
    // its frame would be rebuilt from the NEW bytecode at the old bci
    // (`r13w5-resume-chain-inner-scope-of-a-redefined-class`), which the
    // deopt-exit doors already refuse. Round 14 wave 4 (lane resume2, CH3W-3):
    // unless every such scope has the bytecode the compile spliced
    // ([`chain_inner_scope_own_sources`]), which it is rebuilt from and
    // restamped with the body's pool generation after the push, as the doors
    // do (R14DP-5). This refusal comes after committed iterations, so it is
    // not a safe reject: resuming the right bytecode is strictly better.
    let inner_own_sources =
        if chain_inner_scope_redefined_since_compile(shared, artifact, live_class_id, rframe) {
            match chain_own_source_sinks_enabled()
                .then(|| chain_inner_scope_own_sources(shared, artifact, live_class_id, rframe))
                .flatten()
            {
                Some(rows) => rows,
                None => {
                    return Err(
                        "a spliced callee's class was redefined since the compile".to_string()
                    );
                }
            }
        } else {
            Vec::new()
        };
    let inner = materialise_inner_scopes(
        shared,
        live_class_id,
        rframe,
        &artifact.splice_scope_class_ids,
        &inner_own_sources,
    )?;

    // The artifact must actually have recorded the trapping bci, exactly as
    // `resume_after_exit` requires for the flat case. Without this a mis-routed
    // stash whose scopes happen to type-check would be resumed. The planless
    // guard exit proved provenance by the point's ADDRESS instead
    // (`osr_exit::guard_exit_resume_bci`); an optimizing tier's chain points
    // live only in its `_deopt_point_boxes`, so this check would refuse every
    // one of them (round 13 wave 8, lane chain3).
    if provenance == ChainExitProvenance::RecordedBci
        && !artifact.deopt_points.iter().any(|p| p.bci == rframe.bci)
    {
        return Err(format!(
            "trapping bci {} is not a recorded deopt point of this artifact",
            rframe.bci
        ));
    }

    // `CRATONVM_DEOPT_VERIFY`, before a reconstructed oop becomes a GC root.
    if cratonvm_jit::deopt_verify_enabled() {
        // Every scope (round 13 wave 5, lane resume): the outermost is written
        // into the live frame and the others pushed, so each one's words
        // become roots.
        if let Err(why) = verify_chain_oops(rframe, shared) {
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
        // By JVM slot, not by list index (interpreter round i1 wave 24, lane
        // L2): `outer_locals` is the compact list `caller_frame_values` makes
        // for `Frame::new_pooled`, which re-expands it. Written index for index,
        // a `long`/`double` local shifted every later local down one slot.
        // Cannot fail: the fit was checked in slots above.
        let _ = write_compact_locals_in_place(frame, &outer_locals);
        frame.stack.clear();
        for v in &outer_stack {
            frame.stack.push_unchecked(*v);
        }
        frame.pc = outer_resume_pc;
        // Round 13 wave 6 (lane chain2): parked in its invoke, which is where
        // the unwinder searches its handlers once the pushed callee throws.
        if chain_frames_report_their_bci() {
            frame.last_instr_pc = outer_scope.bci as usize;
        }
        if super::osr_frame_trace::enabled() {
            super::osr_frame_trace::record_exit(frame, outer_resume_pc);
        }
    }
    let trace = dbg_deopt_enabled();
    if trace {
        eprintln!(
            "[cratonvm-deopt] OSR-exit CHAIN transfer: {live_class}.{live_method} in place @{outer_resume_pc}, \
             then {} pushed frame(s), trapping bci={}",
            inner.len(),
            rframe.bci
        );
    }
    let first_pushed = thread.frames.len();
    let pushed = push_inlined_chain(shared, thread, DeoptFrameChain::ready(inner), trace)
        .map(|_| outer_resume_pc)
        .ok_or_else(|| {
            "pushing the inlined chain failed after the live frame was written".to_string()
        })?;
    // The pushed frames are the inner scopes only (the outermost is the live
    // frame), outermost-inward from `first_pushed`: restamp each one built
    // from an own-source template before the interpreter runs it.
    if !inner_own_sources.is_empty() {
        let stamp = artifact.compile_cp_stamp();
        let inner_scopes = rframe
            .caller_frames
            .iter()
            .rev()
            .skip(1)
            .chain(std::iter::once(rframe));
        for (k, scope) in inner_scopes.enumerate() {
            if inner_own_sources.iter().any(|(key, _)| *key == scope.method_key) {
                super::obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code(
                    shared,
                    thread,
                    first_pushed + k,
                    stamp,
                );
            }
        }
    }
    Ok(pushed)
}

/// How [`transfer_osr_exit_chain_into_live_frame`] establishes that the chain
/// came out of the artifact it is given (round 13 wave 8, lane chain3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChainExitProvenance {
    /// The planned OSR exit: the trapping bci must be one of the artifact's
    /// recorded `deopt_points`.
    RecordedBci,
    /// The planless guard exit: `osr_exit::guard_exit_resume_bci` found the
    /// stashed point among the artifact's boxes by address, admitted.
    PointAddress,
}

/// [`transfer_osr_guard_exit_into_live_frame`] for an inlined chain (round 13
/// wave 8, lane chain3; `r13w6-chain2-osr-guard-exit-chain-lock-record-patch`
/// part (b)). Returns the live frame's new pc.
///
/// Admission (`osr_exit::chain_resumable_in_place`) refused any chain naming a
/// monitor, in any scope: this transfer re-takes no elided level, and a lock
/// here can only be the looping method's own, whose javac catch-all covers the
/// `invoke` the door requires uncovered (the OSR door's record rewrite reads
/// the outermost scope since lane sync5's part (c), so it would be right).
/// Asked again here, before anything is written, so a chain the admission
/// rule never saw cannot reach the rewrite either. Every other check -- identity
/// of the outermost scope, redefinition of a spliced callee's class, fit,
/// virtual objects, `CRATONVM_DEOPT_VERIFY` -- is the planned exit's, in
/// [`transfer_osr_exit_chain_into_live_frame`], which writes nothing before
/// all of them passed.
fn transfer_osr_guard_exit_chain_into_live_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    artifact: &cratonvm_jit::CompiledMethod,
) -> Result<usize, String> {
    if !rframe.monitors.is_empty() || rframe.caller_frames.iter().any(|c| !c.monitors.is_empty()) {
        return Err("an inlined chain holding a monitor".to_string());
    }
    let resume_pc = transfer_osr_exit_chain_into_live_frame(
        shared,
        thread,
        frame_idx,
        rframe,
        artifact,
        ChainExitProvenance::PointAddress,
    )?;
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR guard-exit CHAIN transfer: live frame @{resume_pc}, {} frame(s) \
             pushed, trapping bci={}",
            rframe.caller_frames.len(),
            rframe.bci
        );
    }
    Ok(resume_pc)
}

/// How many JVM local slots a COMPACT local list (`ir_deopt_locals` /
/// `caller_frame_values`: one entry per value) spans: two per `long` /
/// `double`, one per anything else — the layout `copy_args_to_locals` builds.
fn compact_locals_slot_count(locals: &[Value]) -> usize {
    locals
        .iter()
        .map(|v| if v.is_category2() { 2 } else { 1 })
        .sum()
}

/// Write a COMPACT local list into an existing frame's JVM slots, a cat-2
/// value at its slot with its upper half invalidated by the store
/// (`Frame::set_local_unchecked`) — `copy_args_to_locals`'s layout, for a frame
/// that is rewritten in place rather than built. Slots past the list keep
/// their values. `Err` (nothing written) when the list spans more slots than
/// the frame has. Interpreter round i1 wave 24, lane L2.
fn write_compact_locals_in_place(frame: &mut Frame, locals: &[Value]) -> Result<(), String> {
    let slots = compact_locals_slot_count(locals);
    if slots > frame.locals_len() {
        return Err(format!(
            "{slots} local slots do not fit a frame of {}",
            frame.locals_len()
        ));
    }
    let mut slot = 0;
    for v in locals {
        frame.set_local_unchecked(slot, *v);
        slot += if v.is_category2() { 2 } else { 1 };
    }
    Ok(())
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
    if !dbg_deopt_enabled() {
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
/// That is true of straight-line code only. Inside a loop the earlier
/// iterations also ran the code after `resume_bci`, so since round 12 wave 3
/// the scanned range is the loop-closed extent
/// (`cratonvm_jit::ir::replay_committed_extent`,
/// `r12w2-irbuild-replay-prefix-ignores-loops`), and a point the compiler
/// marked `first_arrival` (an IR site trap, which fires the first time control
/// reaches it) scans only what is reachable before that first arrival. The rule
/// itself is `cratonvm_jit::deopt::replay_from_entry_commits_nothing_with_handlers`,
/// the one the compiler also asks before installing a body whose points would
/// need it; [`replay_from_entry_is_observably_equivalent_for_frame`] is the
/// form that can read the frame's `first_arrival`, and this one assumes the
/// point was conditional (never less strict).
///
/// This form follows no exception edge: it is for a caller that holds no body
/// (the `u32::MAX` whole-body question, or an impure `spliced_bodies_pure`,
/// where no edge can change the answer). A sink holding the body that ran asks
/// [`replay_from_entry_is_observably_equivalent_after`] or the stash form,
/// which add the edges that body's local handlers can take (round 13 wave 12,
/// lane replay6).
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
    // The historical rule first (inside the shared predicate): a body that
    // commits nothing anywhere needs no reasoning about where the attempt
    // stopped, and answers `true` even for the `u32::MAX` sentinel.
    //
    // `code` is the frame's padded bytecode; the two trailing zero bytes are
    // `nop`s to every walk the predicate makes.
    replay_is_exact_after(code, None, spliced_bodies_pure, resume_bci, false)
}

/// [`replay_from_entry_is_observably_equivalent`] for an attempt of the
/// compiled body `ran` that stopped at `resume_bci` (a conditional point), for
/// a sink that holds that body but no stash: the KCFULL-13 re-run after an
/// escaped exception and the lambda direct arm's implicit exception, both in
/// `helpers.rs` (round 13 wave 12, lane replay6). The spliced-body half is
/// `ran`'s own (`false` without a body), and the extent follows the exception
/// edges `ran`'s local handlers can take
/// (`cratonvm_jit::deopt::local_handler_replay_edges`).
pub(crate) fn replay_from_entry_is_observably_equivalent_after(
    code: &[u8],
    ran: Option<&crate::jit::CompiledMethod>,
    resume_bci: u32,
) -> bool {
    let pure = ran.is_some_and(|r| r.spliced_bodies_side_effect_free);
    replay_is_exact_after(code, ran, pure, resume_bci, false)
}

/// The one VM call into the shared replay rule
/// (`cratonvm_jit::deopt::replay_from_entry_commits_nothing_with_handlers`),
/// with the exception edges of the body that ran (round 13 wave 12, lane
/// replay6, `r13w12-osrdoor-vm-sinks-replay-extent-ignores-handler-edges`).
///
/// A single-pass body with local handlers enters its own `catch` block without
/// leaving compiled code, so a handler placed at or before its throw is a way
/// back to the trap the branch-only extent does not see. The edges are the
/// body's own local-handler sites, not the method's whole table: those are
/// the only exception edges its frame can take, and the single-pass finalizer
/// judged the body with the whole table (a superset), so the sink is never
/// stricter than the check that installed the body. `ran` is `None`, or a body
/// without sites (every optimizing-tier body), exactly as before.
/// `CRATONVM_JIT_REPLAY_EXTENT_HANDLER_EDGES=0` ignores the edges here as at
/// compile time.
fn replay_is_exact_after(
    code: &[u8],
    ran: Option<&crate::jit::CompiledMethod>,
    spliced_bodies_pure: bool,
    resume_bci: u32,
    first_arrival: bool,
) -> bool {
    let edges = ran.map_or_else(Vec::new, |r| {
        cratonvm_jit::deopt::local_handler_replay_edges(
            &r._jit_local_handler_sites,
            code,
            code.len(),
        )
    });
    cratonvm_jit::deopt::replay_from_entry_commits_nothing_with_handlers(
        code,
        code.len(),
        spliced_bodies_pure,
        resume_bci,
        first_arrival,
        &edges,
    )
}

/// [`replay_from_entry_is_observably_equivalent`] for the stashed frame
/// itself: the same rule, reading the frame's own `bci` and its
/// `ResumeSemantics::first_arrival` (round 12 wave 3, lane replay).
///
/// A sink holding the reconstructed frame asks this one, so an IR site trap
/// inside a loop (`for (..) { <indy>; counter++; }`, where the store is only
/// reachable through the trap) replays exactly instead of being refused by the
/// loop-closed extent, which must assume a conditional deopt.
///
/// `ran` is the body that fired the frame, whose local-handler edges the
/// extent follows (round 13 wave 12, lane replay6); `None` follows none.
pub(crate) fn replay_from_entry_is_observably_equivalent_for_frame(
    code: &[u8],
    spliced_bodies_pure: bool,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    ran: Option<&crate::jit::CompiledMethod>,
) -> bool {
    replay_is_exact_after(
        code,
        ran,
        spliced_bodies_pure,
        rframe.bci,
        rframe.semantics.first_arrival,
    )
}

/// The replay question as a method-entry sink holding the stash has to ask
/// it: `code` is the bytecode of the method the sink is about to re-run,
/// `ran` the artifact that ran, `rframe` / `point_addr` the stash (round 12
/// wave 3, lane replay).
///
/// * A frame some OTHER body fired (`deopt_stash_is_from_artifact` false) says
///   nothing about where THIS attempt stopped: its bci is another method's.
///   Only the whole-body rule answers (the `u32::MAX` form). The lambda sink
///   used to pass that foreign bci as if it were the impl's own.
/// * A frame with no point address (a plain restash) keeps the pre-wave-3
///   question at its bci, without the `first_arrival` fact, which only a point
///   `ran` provably owns may assert.
/// * An inlined chain (`caller_frames` non-empty) names its INNERMOST scope's
///   bci, a pc of a spliced callee's bytecode, not of `code`: only the
///   whole-body rule answers for it too (interpreter round i1 wave 28, lane
///   L2). `helpers.rs`'s callee service already filtered chains out before
///   asking; the lambda sink and [`door_rerun_verdict`] did not.
/// * Otherwise [`replay_from_entry_is_observably_equivalent_for_frame`].
pub(crate) fn replay_from_entry_is_observably_equivalent_for_stash(
    code: &[u8],
    ran: &crate::jit::CompiledMethod,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    point_addr: usize,
) -> bool {
    let pure = ran.spliced_bodies_side_effect_free;
    if !deopt_stash_is_from_artifact(ran, point_addr) || !rframe.caller_frames.is_empty() {
        return replay_from_entry_is_observably_equivalent(code, pure, u32::MAX);
    }
    // Round 13 wave 12 (lane replay6): `ran` fired this frame, so the extent
    // follows the exception edges its local handlers can take.
    if point_addr == 0 {
        return replay_is_exact_after(code, Some(ran), pure, rframe.bci, false);
    }
    replay_from_entry_is_observably_equivalent_for_frame(code, pure, rframe, Some(ran))
}

/// The guard that fired for an ordinary stashed frame, when it can be named —
/// the one attribution the sinks holding the method's bytecode share (round 11
/// wave 2, r11-tier-deopt-stash-drops-the-trap-reason, VM half).
///
/// The stash carries a bci and no reason, so the reason is re-derived:
///
/// 1. The non-`RETHROW` points at the bci, with the loop-header `OsrExit` maps
///    skipped outside the OSR-exit test triggers (an `OsrExit` map is an exit
///    source only under those; a loop-header BCE guard trap used to be charged
///    as `OsrExit`, which neither evicts nor matches the guard's de-spec id).
///    Before round 11 wave 2 only the non-resume sinks applied this rule, so one
///    trap was charged under two reasons depending on which arm caught it.
/// 2. When those disagree and are exactly the optimizing tier's `NullCheck` +
///    `BoundsCheck` pair at an array access (`ir_lower`'s unelided
///    `xaload`/`xastore`), the frame itself says which fired: it is a
///    re-execute point, so the array reference is on its operand stack, and
///    the null check precedes the bounds check. A null reference is
///    `NullCheck`; any other is `BoundsCheck`. Before this, every such trap was
///    charged as `UncommonTrap` and four of them withdrew BOTH speculations
///    through the wildcard id.
/// 3. A bci whose only point is an `OsrExit` map answers that.
///
/// `None` means the guard cannot be named from here.
pub(crate) fn trap_reason_at_frame(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<cratonvm_jit::deopt::DeoptReason> {
    use cratonvm_jit::deopt::{DeoptReason, FrameValue};
    let skip_osr_exit =
        !cratonvm_jit::osr_exit_test_enabled() && cratonvm_jit::osr_exit_after().is_none();
    let mut found: Option<DeoptReason> = None;
    let mut ambiguous = false;
    let mut only_array_guards = true;
    for p in compiled.deopt_points.iter() {
        if p.bci != rframe.bci
            || p.semantics.rethrow_exception
            || (skip_osr_exit && p.reason == DeoptReason::OsrExit)
        {
            continue;
        }
        if !matches!(p.reason, DeoptReason::NullCheck | DeoptReason::BoundsCheck) {
            only_array_guards = false;
        }
        match found {
            None => found = Some(p.reason),
            Some(r) if r == p.reason => {}
            Some(_) => ambiguous = true,
        }
    }
    if ambiguous {
        if !only_array_guards {
            return None;
        }
        let bci = usize::try_from(rframe.bci).ok()?;
        // Operand-stack depth of the array reference below the top: the
        // interpreter stack is compact, so a cat-2 stored value is one entry.
        let below_top = match *code.get(bci)? {
            0x2e..=0x35 => 2, // xaload: arrayref, index
            0x4f..=0x56 => 3, // xastore: arrayref, index, value
            _ => return None,
        };
        let at = rframe.stack.len().checked_sub(below_top)?;
        return match rframe.stack.get(at)? {
            FrameValue::Object(0) => Some(DeoptReason::NullCheck),
            FrameValue::Object(_)
            | FrameValue::VirtualObject(_)
            | FrameValue::VirtualObjectRef(_) => Some(DeoptReason::BoundsCheck),
            _ => None,
        };
    }
    if found.is_some() {
        return found;
    }
    match cratonvm_jit::osr_exit::deopt_reason_at_bci(&compiled.deopt_points, rframe.bci) {
        cratonvm_jit::osr_exit::ReasonAtBci::Unique(r) => Some(r),
        _ => None,
    }
}

/// [`trap_reason_at_frame`], with `UncommonTrap` for a guard that cannot be
/// named. `UncommonTrap` is CHARGED, so an unattributable trap still shows in
/// the trap rate and backs the method off, but it escalates on the ordinary
/// count-based schedule: an attribution nobody could make cannot blacklist a
/// method by itself (`UnreachedCode`, the old fallback, is `MakeNotCompilable`
/// on the first occurrence). Picking the first point in list order instead is
/// how a `PendingException` point used to be reported for a guard trap —
/// never charged, the artifact evicted and recompiled with the same wrong
/// speculation, forever.
///
/// Test-only since round 11 wave 5: every production sink asks
/// [`trap_reason_for_frame_with_cause`], which answers exactly this when the
/// stash carries no cause.
#[cfg(test)]
pub(crate) fn trap_reason_for_frame(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> cratonvm_jit::deopt::DeoptReason {
    trap_reason_at_frame(compiled, code, rframe)
        .unwrap_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap)
}

/// The guard that fired, named from the stash's own [`DeoptCause`] when it has
/// one, else re-derived by [`trap_reason_at_frame`] (round 11 wave 5,
/// r11w4-tier-vm-sinks-should-read-the-stashed-deopt-cause).
///
/// The cause is the reason and speculation id of the point the trampoline was
/// handed, so it is exact where the bci-based derivation answers `None` (two
/// guards at one non-array bci). It is believed only when THIS artifact has a
/// non-`RETHROW` point with that reason (and, for a non-zero id, that id) at
/// the frame's bci. A cause that describes some other artifact's point — a
/// foreign or leftover frame, an inlined scope's point — is ignored and the
/// old derivation runs, so a sink can never do worse than before.
///
/// The answered `speculation_id` is the point's own when it has one: that is
/// the id every compile-side consumer asks the de-spec registry about
/// (`p.speculation_id`), which the `(bci, reason)` derivation only equals by
/// convention. Otherwise it is [`cratonvm_jit::deopt::speculation_id`].
pub(crate) fn trap_cause_at_frame(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> Option<cratonvm_jit::deopt::DeoptCause> {
    use cratonvm_jit::deopt::{speculation_id, DeoptCause, SPECULATION_ID_ANY};
    if rframe.bci == u32::MAX {
        return None;
    }
    if let Some(c) = cause {
        let names_it = |p: &cratonvm_jit::deopt::DeoptimizationPoint| {
            p.bci == rframe.bci
                && p.reason == c.reason
                && !p.semantics.rethrow_exception
                && (c.speculation_id == SPECULATION_ID_ANY || p.speculation_id == c.speculation_id)
        };
        // An optimizing body's back-edge poll exit (interpreter round i1
        // waves 15/18, `ir_lower::Lowerer::emit_poll_mode_exit`) is published
        // in its boxes only, never in `deopt_points`: recognised there, so the
        // sinks that charge by this attribution (the first-call door's
        // `despeculate_trapped_method`) see the `OsrExit` an agent's exit is
        // and decline it (`exit_left_for_the_interpreter`).
        let recognised = compiled.deopt_points.iter().any(&names_it)
            || (c.reason == cratonvm_jit::deopt::DeoptReason::OsrExit
                && compiled._deopt_point_boxes.iter().any(|p| names_it(p)));
        if recognised {
            let id = if c.speculation_id == SPECULATION_ID_ANY {
                speculation_id(rframe.bci, c.reason)
            } else {
                c.speculation_id
            };
            return Some(DeoptCause {
                reason: c.reason,
                speculation_id: id,
            });
        }
    }
    trap_reason_at_frame(compiled, code, rframe).map(|reason| DeoptCause {
        reason,
        speculation_id: speculation_id(rframe.bci, reason),
    })
}

/// [`trap_cause_at_frame`]'s reason, with the `UncommonTrap` fallback
/// (`trap_reason_for_frame`'s rule) for a guard that cannot be named.
pub(crate) fn trap_reason_for_frame_with_cause(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> cratonvm_jit::deopt::DeoptReason {
    trap_cause_at_frame(compiled, code, rframe, cause)
        .map(|c| c.reason)
        .unwrap_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap)
}

/// Did this trap come out of an artifact that is no longer the published body
/// of its method — i.e. the JIT cache currently holds a DIFFERENT body for it?
///
/// `DeoptimizationController::deoptimize` evicts by name, not by artifact. A
/// thread still running (or re-entering through a stale call-site cache) the
/// body a first trap already retired would otherwise evict the FRESH body that
/// trap's recompile published, and charge the ledger again for the same
/// speculation failure — N threads in the old body charge N times, and 20
/// inside one decay window reach `MakeNotCompilable`
/// (r11-tier-deopt-storm-from-superseded-artifacts-and-no-recompile-cutoff).
///
/// An EMPTY cache slot answers `false`: the old body was evicted and nothing
/// replaced it yet, and charging then is the pre-existing behaviour (the eviction
/// is a no-op). Only the positive identity mismatch is treated as stale, so a
/// body the cache never held (a lookup under another class id, a hash
/// collision) keeps being charged exactly as before.
pub(crate) fn trapped_artifact_is_superseded(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    compiled: &crate::jit::CompiledMethod,
) -> bool {
    let live = shared.jit.jit_cache.read().get(
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        cached.declaring_class_id,
    );
    match live {
        Some(live) => !std::ptr::eq(Arc::as_ptr(&live), compiled),
        None => false,
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

/// The scope a stashed frame's IDENTITY is judged by at a sink that owns a
/// chain path: the OUTERMOST scope of an inlined chain — the method the
/// artifact belongs to, i.e. the one the door ran (`caller_frames` runs
/// innermost-first, so it is the `last()`) — and the frame itself otherwise.
/// Interpreter round i1 wave 26, lane L2
/// (`interpreter-L2-stash-identity-gates-name-the-innermost-scope-so-no-chain-resumes-FIXED-20260928`).
///
/// A chain's own `method_key` names the spliced callee that trapped, so a
/// gate that compares it with the sink's method read every chain as a nested
/// callee's stash, charged the innocent callee and re-ran the method from
/// entry: the chain paths behind those gates (`resume_real_ir_deopt`'s,
/// `transfer_osr_exit_into_live_frame`'s) were unreachable from a door. Only
/// a sink whose continuation materialises the chain may use this; one that
/// reads the frame's locals and bci as the sink method's own must keep
/// comparing the innermost name, which refuses a chain.
pub(crate) fn stash_identity_scope(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> &cratonvm_jit::deopt::ReconstructedFrame {
    rframe.caller_frames.last().unwrap_or(rframe)
}

/// Where a stashed frame's trap is charged against the sink's (outermost)
/// method: `(reason, bci)` in THAT method's bytecode space (interpreter round
/// i1 wave 26, lane L2). For a single-scope frame this is exactly the
/// attribution every sink shares — [`trap_reason_for_frame_with_cause`] at
/// `rframe.bci`. For an inlined chain the trap happened inside a spliced
/// callee, at a bci of the CALLEE's code: the charge goes to the call site in
/// the outer method (the outermost scope's bci), which is where the artifact
/// that speculated — and a recompile that stops speculating — lives, with the
/// stash's own reason (the bci-derived attribution would decode the callee's
/// bci in the outer method's bytecode) or `UncommonTrap`. HotSpot likewise
/// invalidates the outer nmethod; its per-bci trap history is kept in the
/// trapping scope's own profile, which this VM does not have per inline scope.
pub(crate) fn stash_charge_site(
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> (cratonvm_jit::deopt::DeoptReason, u32) {
    match rframe.caller_frames.last() {
        Some(outer) => (
            cause.map_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap, |c| c.reason),
            outer.bci,
        ),
        None => (
            trap_reason_for_frame_with_cause(compiled, code, rframe, cause),
            rframe.bci,
        ),
    }
}

/// Round 14 wave 3 (lane resume; proposal CH3-4 of `jit-r13-chain3-proposals-RETIRED-20260929.md`):
/// charge an optimizing OSR body's CHAIN guard exit -- a guard inside a
/// spliced callee, left through the planless guard exit's in-place chain
/// transfer (admitted since round 13 wave 8) -- as the doors charge a chain
/// trap ([`stash_charge_site`] and the chain arm of
/// [`real_frame_deopt_resume_or_throw_and_despeculate`]): against the OSR'd
/// method at its OUTERMOST scope's bci (the call site the artifact spliced),
/// under the stash's own reason, and after `PER_BCI_DESPEC_LIMIT` such
/// charges at that site the wildcard per-bci de-spec there. Until now
/// `jit_bridge::charge_osr_guard_exit` declined every chain, so such an exit
/// was never recorded at all, however often it recurred.
///
/// Held to the two reasons the flat OSR charge answers (`BoundsCheck`,
/// `ReceiverTypeChanged`) and to a stash cause (a chain's bci is a callee's,
/// so nothing can be read from the OSR'd method's own points). Returns whether
/// the exit was charged. The caller must NOT treat a charged chain exit as a
/// committed OSR entry: the de-spec at the call site does not withdraw the
/// spliced guard itself, so the per-pc rejection budget stays its brake.
/// Kill switch `CRATONVM_JIT_OSR_CHAIN_GUARD_EXIT_CHARGE` (default ON; read
/// only on such an exit).
pub(crate) fn charge_osr_chain_guard_exit(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> bool {
    use cratonvm_jit::deopt::DeoptReason;
    let Some(outer) = rframe.caller_frames.last() else {
        return false;
    };
    let Some(reason) = cause
        .map(|c| c.reason)
        .filter(|r| matches!(r, DeoptReason::BoundsCheck | DeoptReason::ReceiverTypeChanged))
    else {
        return false;
    };
    if rframe.bci == u32::MAX
        || outer.bci == u32::MAX
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_OSR_CHAIN_GUARD_EXIT_CHARGE",
        )
    {
        return false;
    }
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR chain guard exit charged: {class_name}.{method_name}{descriptor} \
             reason={reason:?} call-site bci={} ({}-frame chain, innermost {} bci={})",
            outer.bci,
            rframe.caller_frames.len() + 1,
            rframe.method_key,
            rframe.bci
        );
    }
    let _ = crate::jit::helpers::DeoptimizationController::deoptimize_in(
        shared,
        class_id,
        class_name,
        method_name,
        descriptor,
        reason,
        outer.bci,
    );
    // The doors' limit and grain: counted at `(reason, call site)`, withdrawn
    // with the wildcard (the call site speculated whatever it spliced).
    let method_key = format!("{class_name}.{method_name}:{descriptor}");
    let _ = despec_site_after_trap_limit(
        shared,
        &method_key,
        outer.bci,
        Some(reason),
        cratonvm_jit::deopt::SPECULATION_ID_ANY,
    );
    true
}

/// Charged traps at one `(method, bci)` -- at `(reason, bci)` for a named
/// speculation -- before that speculation is withdrawn from the method's next
/// compile. HotSpot's `PerBytecodeTrapLimit`.
pub(crate) const PER_BCI_DESPEC_LIMIT: usize = 4;

/// Round 14 wave 4 (lane resume2; proposal RS-3 of `jit-r14-resume-proposals.md`):
/// the per-bci de-spec rule, in one copy. Once `method_key` has been charged
/// [`PER_BCI_DESPEC_LIMIT`] times at `bci` -- counted at `(reason, bci)` when
/// `counted` names the reason, else every reason at the bci -- `speculation`
/// is recorded in this VM's de-spec registry at `bci`, so the next compile of
/// the method drops that speculative guard (or, for
/// `SPECULATION_ID_ANY`, everything it speculated there) and the method stays
/// compiled. Returns the count when it withdrew. Nothing for the
/// superseded-guard sentinel (`bci == u32::MAX`). Call it AFTER the charge
/// (the log's lock is taken here; the charge must have released it).
///
/// The callers keep the two choices that are deliberately theirs: which
/// reason is counted (a named speculation's own, a chain's stash reason at
/// the call site, or the whole bci for a wildcard) and which id is withdrawn.
/// The rule itself -- the limit, the grain, the sentinel, the insert -- was
/// written out three times (the doors, the call-site service's
/// `helpers::despeculate_trapped_method`, the OSR chain charge) and had
/// drifted once (round 11 wave 4: one copy summed every reason at the bci).
pub(crate) fn despec_site_after_trap_limit(
    shared: &SharedVm,
    method_key: &str,
    bci: u32,
    counted: Option<cratonvm_jit::deopt::DeoptReason>,
    speculation: u32,
) -> Option<usize> {
    if bci == u32::MAX {
        return None;
    }
    let charged = {
        let log = shared.jit.deopt_log.lock();
        match counted {
            Some(reason) => log.deopt_count_at_site(method_key, reason, bci),
            None => log.deopt_count_at_bci(method_key, bci),
        }
    };
    if charged < PER_BCI_DESPEC_LIMIT {
        return None;
    }
    shared.jit.despec_registry.insert(method_key, bci, speculation);
    Some(charged)
}

/// Did the stash entry whose point address is `point_addr`
/// (`take_last_deopt_with_point`) come out of `ran`, the body this sink just
/// executed? `true` for an unknown point (0: the re-run sentinel of a null
/// point, a plain restash), which leaves the decision to the name check.
///
/// Interpreter round i1 wave 10, lane L4
/// (`docs/internal/fixed-bugs/interpreter-L7-deopt-sinks-match-stashed-frames-by-name-FIXED-20260925.md`):
/// the name check alone accepts a frame fired by ANOTHER body with the same
/// `"<class>.<method>:<descriptor>"` key — another loader's copy of the class,
/// or a nested activation of this method under a different artifact — and a
/// sink would then rebuild this activation from that body's locals and bci.
/// The point address is the stash's identity token: every point a body's
/// stubs can fire, inlined scopes included, is one of that body's own
/// (`CompiledMethod::owns_deopt_point`), and `ran` is alive for the whole
/// sink, so its storage cannot have been reused for another body's points.
#[inline]
pub(crate) fn deopt_stash_is_from_artifact(
    ran: &crate::jit::CompiledMethod,
    point_addr: usize,
) -> bool {
    point_addr == 0 || ran.owns_deopt_point(point_addr)
}

/// [`deopt_frame_matches_method`] for a sink that holds the body it ran:
/// the frame must name the method AND have been fired by that very body
/// ([`deopt_stash_is_from_artifact`]).
pub(crate) fn deopt_frame_matches_artifact(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    ran: &crate::jit::CompiledMethod,
    point_addr: usize,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    deopt_frame_matches_method(rframe, class_name, method_name, descriptor)
        && deopt_stash_is_from_artifact(ran, point_addr)
}

/// jit-invokedynamic-groovy-regression fix — de-speculate the method a
/// MISMATCHED stashed frame actually belongs to (parsed from its baked
/// `method_key`), so the truly-trapping method gets evicted/blacklisted and
/// stops re-trapping, instead of the consumer's own (innocent) method eating
/// the deopt accounting. No-op for an unparseable/empty key.
///
/// `point_addr` is the address of the deopt point that stashed `rframe`
/// (`take_last_deopt_with_point`; 0 when the taker did not keep it). A trap
/// out of a body the owner's published one superseded is not charged: see
/// [`foreign_trap_came_from_a_superseded_body`].
///
/// `pub(crate)` since round 11 wave 12 (lane tier): the dispatch helpers'
/// declined-callee re-run
/// (`r11w12-tier-helpers-declined-callee-rerun-patch-FIXED-20260924.md`) charges
/// the stash it takes through this function, as the OSR sink's
/// identity-mismatch arm did when the stash reached it.
///
/// `cause` is the stash's own (`take_last_deopt_with_point`); an `OsrExit`
/// the interpreter-only mode asked for is not charged (wave 15,
/// [`super::stashed_exit_left_for_the_interpreter`]).
pub(crate) fn despeculate_stashed_frame_method_with_cause(
    shared: &SharedVm,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
) {
    // Interpreter round i1 wave 15, lane L3: the frame's method left at a
    // back-edge poll because it must now run interpreted, which says nothing
    // about its code. Charged as `UncommonTrap` below, the `OsrExit` rule of
    // `DeoptimizationController::deoptimize_in` would not see it.
    if super::stashed_exit_left_for_the_interpreter(shared, rframe, cause, point_addr) {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] foreign frame {} bci={}: left for the interpreter-only \
                 mode, not charged",
                rframe.method_key, rframe.bci
            );
        }
        return;
    }
    despeculate_stashed_frame_method(shared, rframe, point_addr);
}

/// [`despeculate_stashed_frame_method_with_cause`] for a sink that did not keep
/// the stash's cause; see there.
pub(crate) fn despeculate_stashed_frame_method(
    shared: &SharedVm,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    point_addr: usize,
) {
    // The frame's OWNER is the method its artifact belongs to: an inlined
    // chain's outermost scope, charged at that scope's own bci (the call site
    // of the splice), not the spliced callee that trapped, whose own body is
    // innocent (wave 26, lane L2; `stash_identity_scope`). The same frame for
    // a single scope.
    let owner = stash_identity_scope(rframe);
    let Some((rest, key_desc)) = owner.method_key.rsplit_once(':') else {
        return;
    };
    let Some((key_class, key_method)) = rest.rsplit_once('.') else {
        return;
    };
    if foreign_trap_came_from_a_superseded_body(shared, key_class, key_method, key_desc, point_addr)
    {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] foreign frame {} bci={}: trap from a superseded body, \
                 not charged, fresh body kept",
                rframe.method_key, rframe.bci
            );
        }
        return;
    }
    // N3(b). This used to charge `UnreachedCode`, the one reason
    // `recommend_action` answers `MakeNotCompilable` on the FIRST occurrence —
    // so method `B` was permanently retired because method `A`'s sink saw
    // `B`'s frame. Read what that says: "a frame arrived at the wrong sink" is
    // evidence about the STASH ROUTING, not about `B`'s code, and `B` may
    // never have mis-speculated at all. All this site actually knows is that
    // `B` trapped; charging it as if `B` were unreachable is a verdict nobody
    // took.
    //
    // `UncommonTrap` is the honest reason. It is CHARGED, so `B`'s trap rate
    // is still visible and `B` still backs off if it really is re-trapping —
    // which is the whole point of de-speculating the frame's real owner rather
    // than the innocent consumer — but it escalates on the ordinary
    // count-based schedule, so ONE mis-routed frame cannot blacklist a method.
    // Reserve `UnreachedCode` for genuinely unreachable code.
    //
    // The precise reason is NOT recoverable here: this sink holds the frame and
    // its bci but not `B`'s artifact, and looking `B`'s artifact up would mean
    // trusting a `method_key` we have just established does not name the method
    // we are in. `trap_reason_for_frame` is the disambiguating form and it needs
    // the artifact the frame actually came from.
    crate::jit::helpers::DeoptimizationController::deoptimize(
        shared,
        key_class,
        key_method,
        key_desc,
        cratonvm_jit::deopt::DeoptReason::UncommonTrap,
        owner.bci,
    );
}

/// Did the point at `point_addr` fire in a body that is no longer published
/// for `class.method desc`? The foreign-frame form of
/// [`trapped_artifact_is_superseded`], for a sink that holds the frame's
/// owner only by name (r11-tier-deopt-storm-from-superseded-artifacts-and-no-recompile-cutoff,
/// the last by-name sink).
///
/// `DeoptimizationController::deoptimize` evicts by name, so charging a trap
/// out of a retired body would evict the FRESH body its first trap's
/// recompile published, and count the one speculation failure again.
///
/// The class is resolved exactly as `deoptimize` resolves it (by name, id 0
/// when no single loader answers), so this asks about the very slot the charge
/// would evict. `true` only on a positive mismatch: a known point (non-zero),
/// a published method-entry body that does not own it, and no published OSR
/// body that owns it. An empty slot or an unknown point keeps the charge, as
/// `helpers::try_resume_trapped_callee`'s identical rule does, so the address
/// only ever suppresses a charge.
///
/// Limit, shared with that rule: an address cannot tell a retired body from
/// one that was never published under this name
/// (`helpers::trapping_artifact_is_superseded`, which holds the artifact,
/// charges such a body). Both answer "superseded" here. The charge lost for the second kind
/// would have evicted the published body, which is not the one trapping; what
/// it did buy is the ledger count toward `MakeNotCompilable`.
fn foreign_trap_came_from_a_superseded_body(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    point_addr: usize,
) -> bool {
    if point_addr == 0 {
        return false;
    }
    let class_id = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(class_name)
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let cache = shared.jit.jit_cache.read();
    let Some(live) = cache.get(class_name, method_name, descriptor, class_id) else {
        return false;
    };
    if live.owns_deopt_point(point_addr) {
        return false;
    }
    !cache
        .get_osr(class_name, method_name, descriptor, class_id)
        .is_some_and(|osr| osr.owns_deopt_point(point_addr))
}

/// Was the class of any INNER scope of `rframe`'s inlined chain (a spliced
/// callee, resolved as [`materialise_inner_scopes`] resolves it) redefined
/// after `compiled`'s compilation began? Such a scope would be rebuilt from its
/// method's current bytecode, which is not the bytecode its bci and locals
/// describe (round 13 wave 5, lane resume). `false` at once while no class was
/// ever redefined, for a scope that does not resolve (the chain builder
/// refuses it anyway), and with `CRATONVM_DEOPT_CHAIN_INNER_REDEFINED_REFUSES=0`
/// (default ON; read only once a spliced callee's class was redefined).
/// `false` for a single frame. The doors' re-run answer asks it too
/// (`jit_bridge::door_rerun_or_refuse`): such a chain re-runs as a redefined
/// class's frame does, instead of raising.
pub(super) fn chain_inner_scope_redefined_since_compile(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    outermost_class: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    if rframe.caller_frames.is_empty() || !crate::classloading::any_class_redefined() {
        return false;
    }
    let mut enclosing = outermost_class;
    // Outermost-inward, skipping the outermost scope (the compiled method).
    for scope in rframe
        .caller_frames
        .iter()
        .rev()
        .skip(1)
        .chain(std::iter::once(rframe))
    {
        // By the class the compile spliced (round 13 wave 6, lane chain2): the
        // name rule could answer another class of the same name, whose
        // redefinition says nothing about this scope.
        let Ok(callee) = resolve_inlined_callee_hinted(
            shared,
            enclosing,
            &scope.method_key,
            &compiled.splice_scope_class_ids,
        ) else {
            return false;
        };
        let class_id = callee.declaring_class_id;
        if class_was_redefined(shared, class_id)
            && !shared
                .jit
                .jit_cache
                .compiled_since_redefinition_of(class_id, compiled.install_epoch)
        {
            return cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEOPT_CHAIN_INNER_REDEFINED_REFUSES",
            );
        }
        enclosing = class_id;
    }
    false
}

/// Round 14 wave 3 (lane chain; proposal R14DP-5, page
/// `r13w5-resume-chain-inner-scope-of-a-redefined-class`): the templates the
/// inner scopes of `rframe`'s inlined chain whose class was redefined since
/// `compiled`'s compilation began ([`chain_inner_scope_redefined_since_compile`])
/// must be rebuilt from -- the bytecode the compile spliced
/// (`CompiledMethod::splice_scope_sources`), the obsolete method a JVMTI
/// redefinition leaves such an activation running.
///
/// `Some(rows)`, one `(method key, template)` per such scope, when EVERY such
/// scope has a retained body that names the same static-ness as the current
/// method and whose frame the class's history can move onto the current pool
/// (`obsolete_frames::rebuilt_body_translates`, from the body's
/// `compile_cp_stamp`). The template keeps the current method's identity,
/// modifiers and source file (a redefinition changes none of the first two),
/// has no exception table (the resolver never splices a body that has one),
/// and bounds its operand stack by two slots per code byte (no instruction
/// pushes more than two). The caller restamps each frame built from one
/// ([`restamp_inner_own_source_frames`]).
///
/// `None` (keep the refusal) for a single frame, when no inner scope was
/// redefined, when a scope does not resolve or has no row, and with
/// `CRATONVM_DEOPT_CHAIN_INNER_OWN_SOURCE=0`. Asked by the doors and, since
/// round 14 wave 4 (lane resume2, CH3W-3), by every other chain sink
/// ([`build_stale_chain_from_own_sources`], the OSR-exit chain transfer).
pub(super) fn chain_inner_scope_own_sources(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    outermost_class: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<Vec<(String, Arc<CachedBytecodeMethod>)>> {
    if rframe.caller_frames.is_empty()
        || compiled.splice_scope_sources.is_empty()
        || !crate::classloading::any_class_redefined()
        || !cratonvm_jit::chain_inner_own_source_enabled()
    {
        return None;
    }
    let stamp = compiled.compile_cp_stamp()?;
    let mut rows: Vec<(String, Arc<CachedBytecodeMethod>)> = Vec::new();
    let mut enclosing = outermost_class;
    for scope in rframe
        .caller_frames
        .iter()
        .rev()
        .skip(1)
        .chain(std::iter::once(rframe))
    {
        let current = resolve_inlined_callee_hinted(
            shared,
            enclosing,
            &scope.method_key,
            &compiled.splice_scope_class_ids,
        )
        .ok()?;
        let class_id = current.declaring_class_id;
        enclosing = class_id;
        if !class_was_redefined(shared, class_id)
            || shared
                .jit
                .jit_cache
                .compiled_since_redefinition_of(class_id, compiled.install_epoch)
            || rows.iter().any(|(key, _)| *key == scope.method_key)
        {
            continue;
        }
        let source = compiled
            .splice_scope_sources
            .iter()
            .find(|row| row.method_key == scope.method_key)?;
        if source.is_static != current.is_static {
            return None;
        }
        let code_len = source.code.len().saturating_sub(2);
        let template = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: class_id,
            class_name: current.class_name.clone(),
            method_name: current.method_name.clone(),
            method_descriptor: current.method_descriptor.clone(),
            source_file: current.source_file.clone(),
            code: Arc::clone(&source.code),
            exception_table: Arc::from(Vec::new()),
            max_stack: u16::try_from(code_len.saturating_mul(2)).unwrap_or(u16::MAX),
            max_locals: source.max_locals,
            num_params: current.num_params,
            is_synchronized: current.is_synchronized,
            is_static: current.is_static,
        });
        if !super::obsolete_frames::rebuilt_body_translates(
            shared,
            class_id,
            &template.code,
            &template.exception_table,
            stamp,
        ) {
            return None;
        }
        rows.push((scope.method_key.clone(), Arc::new(template)));
    }
    (!rows.is_empty()).then_some(rows)
}

/// Restamp, with the body's constant-pool generation, every frame of the
/// inlined chain just pushed for `rframe` that was built from an own-source
/// template ([`chain_inner_scope_own_sources`]), so it is moved onto its
/// class's current pool before it runs, exactly as the outermost frame is
/// (`obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code`). The chain
/// occupies the top `1 + caller_frames.len()` frames, outermost first.
fn restamp_inner_own_source_frames(
    shared: &SharedVm,
    thread: &mut JvmThread,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
    compile_cp_stamp: Option<u64>,
) {
    if own_sources.is_empty() {
        return;
    }
    let Some(outermost) = thread
        .frames
        .len()
        .checked_sub(1 + rframe.caller_frames.len())
    else {
        return;
    };
    let inner = rframe
        .caller_frames
        .iter()
        .rev()
        .skip(1)
        .chain(std::iter::once(rframe));
    for (k, scope) in inner.enumerate() {
        if own_sources.iter().any(|(key, _)| *key == scope.method_key) {
            super::obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code(
                shared,
                thread,
                outermost + 1 + k,
                compile_cp_stamp,
            );
        }
    }
}

/// Was the OUTERMOST scope's class of `rframe`'s inlined chain -- the
/// compiled method's own, `outermost_class` -- redefined after `compiled`'s
/// compilation began? Round 13 wave 8, lane chain3.
///
/// [`chain_inner_scope_redefined_since_compile`] asks the spliced callees;
/// this asks the method the body belongs to, for the sinks that rebuild its
/// frame from the CURRENT bytecode (the door's `cached`, the tier-up sink's
/// current `Code` attribute). A single frame has the own-source resume and
/// the constant-pool restamp for that case; a chain has neither (both are
/// single-frame, and `obsolete_frames::stamp_frame_rebuilt_from_compiled_code`
/// stamps only the TOP frame, which for a chain is the innermost), so the
/// frame would be parked at the old bci in the new code. `false` for a single
/// frame, while no class was ever redefined, and for a body compiled after
/// the class's last redefinition (its bytecode IS the current one). Kill
/// switch `CRATONVM_DEOPT_CHAIN_OUTER_REDEFINED_REFUSES` (default ON; read
/// only once such a chain traps). Not asked by the OSR-exit chain transfer:
/// that one writes the live frame, whose own bytecode it reads.
pub(super) fn chain_outer_scope_redefined_since_compile(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    outermost_class: ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    if rframe.caller_frames.is_empty() || !crate::classloading::any_class_redefined() {
        return false;
    }
    class_was_redefined(shared, outermost_class)
        && !shared
            .jit
            .jit_cache
            .compiled_since_redefinition_of(outermost_class, compiled.install_epoch)
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_DEOPT_CHAIN_OUTER_REDEFINED_REFUSES",
        )
}

/// Round 14 wave 2 (lane deopt; `r13w5-resume-chain-inner-scope-of-a-redefined-class`,
/// the outermost-scope half of the per-scope own-source resume): the bytecode
/// the OUTERMOST frame of `rframe`'s inlined chain must be rebuilt from when
/// the compiled method's own class was redefined after `compiled`'s
/// compilation began ([`chain_outer_scope_redefined_since_compile`]) and no
/// spliced callee's class was ([`chain_inner_scope_redefined_since_compile`]).
///
/// The outermost scope IS the compiled method, and its template is on the
/// artifact (`CompiledMethod::compiled_source`, [`obsolete_activation_source`]
/// with its translation check); the inner scopes are then current, so the
/// chain is exact once the outermost frame is built from that template and
/// restamped with the body's pool generation -- which every chain sink
/// already does to its outermost frame (`push_inlined_chain`'s
/// `outermost_cp_stamp`, the doors' `stamp_frame_at_rebuilt_from_compiled_code`).
/// Before, such a chain was refused, and the refusal re-ran the method from
/// entry (the doors), raised `InternalError` (the tier-up sink) or restashed
/// (the call-site service). A chain whose spliced callee's class was
/// redefined still needs the per-scope templates and stays refused.
///
/// `None` for a single frame, when the outermost class was not redefined since
/// the compile, when an inner scope's was, when the body kept no source or its
/// frame would not translate, or with `CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE=0`
/// (default ON; read only on such a trap).
pub(crate) fn chain_outermost_own_source(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Option<Arc<CachedBytecodeMethod>> {
    let outermost = cached.declaring_class_id;
    if !chain_outer_scope_redefined_since_compile(shared, compiled, outermost, rframe)
        || chain_inner_scope_redefined_since_compile(shared, compiled, outermost, rframe)
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE")
    {
        return None;
    }
    obsolete_activation_source(shared, compiled, cached)
}

/// Round 14 wave 4 (lane resume2; proposal CH3W-3 of `jit-r14-chain3-proposals.md`,
/// patch `r14w3-chain-tierup-sink-inner-own-source-patch`): the chain of
/// `rframe`, trapped out of `compiled`, for a sink that runs it to completion
/// itself (the first-call tier-up sink, the call-site service by point) when a
/// scope's class was redefined since the compile
/// ([`chain_inner_scope_redefined_since_compile`],
/// [`chain_outer_scope_redefined_since_compile`]). Each such scope is built from
/// the bytecode the body ran: the redefined spliced callees from the compile's
/// retained bodies ([`chain_inner_scope_own_sources`]), the outermost from the
/// body's own template ([`obsolete_activation_source`], under
/// `CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE`); every frame so built, and the
/// outermost, is restamped with the body's pool generation at the push
/// (`DeoptFrameChain::inner_own_source_frames`, `outermost_cp_stamp`). `None`
/// (the sink's refusal) when any stale scope has no usable template, and for a
/// redefined spliced callee with `CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS=0`
/// ([`chain_own_source_sinks_enabled`]; the outermost-only case is round 14
/// wave 2's and keeps its own switch). The doors ask the same two helpers
/// inline and restamp positionally.
pub(super) fn build_stale_chain_from_own_sources(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
) -> Option<DeoptFrameChain> {
    let outermost = cached.declaring_class_id;
    let inner_sources =
        if chain_inner_scope_redefined_since_compile(shared, compiled, outermost, rframe) {
            if !chain_own_source_sinks_enabled() {
                return None;
            }
            chain_inner_scope_own_sources(shared, compiled, outermost, rframe)?
        } else {
            Vec::new()
        };
    let resume_from = if chain_outer_scope_redefined_since_compile(shared, compiled, outermost, rframe)
    {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE") {
            return None;
        }
        obsolete_activation_source(shared, compiled, cached)?
    } else {
        Arc::clone(cached)
    };
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] {}.{}{} bci={}: a stale chain resumes in the bytecode the body ran \
             ({} inner scope template(s), outermost from {})",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            rframe.bci,
            inner_sources.len(),
            if Arc::ptr_eq(&resume_from, cached) { "the current code" } else { "the body's own" },
        );
    }
    build_deopt_frame_chain_sourced(shared, &resume_from, rframe, hints, &inner_sources)
        .map(|chain| chain.with_outermost_cp_stamp(compiled.compile_cp_stamp()))
}

/// `CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS`, **default ON** (round 14 wave 4,
/// lane resume2, CH3W-3): the chain sinks other than the doors -- the
/// first-call tier-up sink, the call-site service by point and both OSR-exit
/// chain transfers -- rebuild a redefined spliced callee's scope from the
/// bytecode the compile spliced, as the doors have since R14DP-5. `=0`
/// restores their refusal (and the call-site service's refusal of a chain
/// whose outermost class alone was redefined). Read only on such a trap.
fn chain_own_source_sinks_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS")
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
/// `cause` is the [`cratonvm_jit::deopt::DeoptCause`] the stash kept beside
/// `rframe` (`take_last_deopt_with_point`), `None` when the taker had none;
/// see [`trap_cause_at_frame`] for how it is used. `point_addr` is the
/// address of the point that fired (0 when unknown); only a FOREIGN frame's
/// charge reads it (`despeculate_stashed_frame_method`), because this
/// method's own is judged by `compiled`'s identity.
///
/// Returns `Ok(Some)` when the frame was resumed (caller returns it), `Ok(None)`
/// to fall through to the whole-method re-run, and `Err` with the
/// `OutOfMemoryError` [`resume_real_ir_deopt_or_throw`] raises when the heap
/// could not re-materialise the frame's scalar-replaced objects (round 12 wave
/// 5): the trapped method's outcome, never a re-run. The charge below is made
/// on every path.
pub(super) fn real_frame_deopt_resume_or_throw_and_despeculate(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    // Identity gate (jit-invokedynamic-groovy-regression): never resume a
    // frame that was stashed by a DIFFERENT method (a nested compiled callee's
    // trap whose sentinel propagated up to this outer sink). Resuming it here
    // would materialize THIS method's frame with the callee's locals/stack/bci
    // — arbitrary misexecution (the root cause of the Groovy "duplicate main
    // method" compiler-internal failures that forced the 5ceb880f revert).
    // De-speculate the frame's real owner so it stops re-trapping, then fall
    // back to the whole-method re-run (the pre-existing conservative path).
    // The frame must also come out of `compiled` itself, not out of another
    // body with the same name (`deopt_stash_is_from_artifact`).
    //
    // An inlined chain is named by its OUTERMOST scope (wave 26, lane L2;
    // `stash_identity_scope`): `resume_real_ir_deopt` materialises the whole
    // chain and checks that scope against `cached` again itself.
    let own_body = deopt_stash_is_from_artifact(compiled, point_addr);
    if !own_body
        || !deopt_frame_matches_method(
            stash_identity_scope(rframe),
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
    {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] stashed frame identity mismatch: frame={} bci={} \
                 own_body={own_body} vs sink method {}.{}:{} — despeculating frame \
                 owner, safe re-run",
                rframe.method_key,
                rframe.bci,
                cached.class_name,
                cached.method_name,
                cached.method_descriptor
            );
        }
        // A key-less sentinel is this body's only when its point is (or is
        // unknown); another body's sentinel names no owner to charge.
        if rframe.method_key.is_empty() && own_body {
            // r11-tier: the identity-less re-run sentinel names no owner, so
            // `despeculate_stashed_frame_method` returns without charging
            // anything and the body that stashed it — the one this sink just
            // ran, since the stash is popped innermost-first — kept trapping
            // uncounted. Charge it here, the way the sinks' non-resume arm
            // charges an ordinary frame; `UncommonTrap` escalates on the
            // count-based schedule, so one sentinel blacklists nothing.
            crate::jit::helpers::DeoptimizationController::deoptimize_in(
                shared,
                cached.declaring_class_id,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
                cratonvm_jit::deopt::DeoptReason::UncommonTrap,
                rframe.bci,
            );
            return Ok(None);
        }
        despeculate_stashed_frame_method_with_cause(shared, rframe, cause, point_addr);
        return Ok(None);
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
    // the conservative skip — except for the mode exit the safepoint slow path
    // granted a body compiled after every redefinition of this VM, whose
    // bytecode is the class's current one (interpreter round i1 wave 18, lane
    // L3; `granted_exit_of_a_current_body`), and for ANY trap out of a body
    // whose compilation began after the class's last redefinition
    // ([`compiled_from_the_current_bytecode`]; round 12 wave 6, lane jni).
    let redefined = class_was_redefined(shared, cached.declaring_class_id);
    // Round 13 wave 5 (lane resume): an inlined chain whose SPLICED callee's
    // class was redefined after this body's compilation began would be rebuilt
    // from that callee's NEW bytecode (inner scopes are resolved now) at the
    // old bci with the old locals. Not resumed; see
    // `r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md`.
    let chain_inner_stale = chain_inner_scope_redefined_since_compile(
        shared,
        compiled,
        cached.declaring_class_id,
        rframe,
    );
    if chain_inner_stale && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] {method_key} bci={}: a spliced callee's class was redefined \
             since the compile; the inlined chain is not resumed",
            rframe.bci
        );
    }
    // Round 13 wave 8 (lane chain3): the same for the OUTERMOST scope of a
    // chain. A single frame of a redefined class resumes in the bytecode it
    // was compiled from (`own_source` below, restamped); a chain has no
    // own-source path and would push the outermost frame from `cached`, which
    // is not that bytecode once the class changed after the compile.
    let chain_outer_stale = chain_outer_scope_redefined_since_compile(
        shared,
        compiled,
        cached.declaring_class_id,
        rframe,
    );
    if chain_outer_stale && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] {method_key} bci={}: the class was redefined since the compile; \
             the inlined chain is not resumed",
            rframe.bci
        );
    }
    // Round 14 wave 3 (lane chain, R14DP-5): such an inner scope is rebuilt
    // from the bytecode the compile spliced when the artifact kept it for
    // every such scope (`chain_inner_scope_own_sources`); only a scope with no
    // usable template still refuses the chain.
    let inner_own_sources = if chain_inner_stale {
        chain_inner_scope_own_sources(shared, compiled, cached.declaring_class_id, rframe)
    } else {
        None
    };
    let chain_inner_refused = chain_inner_stale && inner_own_sources.is_none();
    if inner_own_sources.is_some() && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] {method_key} bci={}: the redefined spliced callees resume in \
             the bytecode the compile spliced",
            rframe.bci
        );
    }
    let fresh = !chain_inner_refused
        && !chain_outer_stale
        && (compiled.compilation_epoch >= live
            || !redefined
            || super::granted_exit_of_a_current_body(shared, cause, point_addr)
            || compiled_from_the_current_bytecode(shared, compiled, cached.declaring_class_id));
    // Round 13 wave 3 (lane replay2; proposal R13-1): a body of a redefined
    // class resumes in the bytecode it was compiled from when its publish
    // site kept it, whatever `cached` holds, and a superseded one resumes
    // too instead of re-running from entry. Nothing changes for a class
    // never redefined, nor (until round 14 wave 2, below) for an inlined
    // chain.
    //
    // Interpreter round i1 wave 46, lane L2: and the exit the safepoint
    // verdict granted a method-entry body forced to leave as its class's
    // obsolete activation after a renumbering redefinition
    // (`jit_bridge::granted_obsolete_activation_source`), also when the door
    // holds that very bytecode (`obsolete_activation_source` answers `None`
    // for `cached` itself, and such a body is neither fresh nor current).
    let own_source = if redefined && rframe.caller_frames.is_empty() {
        obsolete_activation_source(shared, compiled, cached).or_else(|| {
            super::granted_obsolete_activation_source(
                shared,
                cause,
                point_addr,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            )
            .map(|(source, _)| source)
            .filter(|source| {
                compiled
                    .compiled_source()
                    .is_some_and(|own| Arc::ptr_eq(own, source))
            })
        })
    } else if chain_outer_stale && !chain_inner_stale {
        // Round 14 wave 2 (lane deopt): a chain whose only redefined scope is
        // the outermost resumes with that frame rebuilt from the body's own
        // template (restamped below, as every resumed outermost frame is).
        chain_outermost_own_source(shared, compiled, cached, rframe)
    } else if chain_outer_stale && !chain_inner_refused {
        // Round 14 wave 3 (lane chain, R14DP-5): both halves -- the outermost
        // frame from the body's own template (as above; that helper asks the
        // inner scopes itself and would refuse), the inner ones from theirs.
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE")
            .then(|| obsolete_activation_source(shared, compiled, cached))
            .flatten()
    } else {
        None
    };
    let resume_from = own_source.as_ref().unwrap_or(cached);
    let inner_own_sources: &[(String, Arc<CachedBytecodeMethod>)] =
        inner_own_sources.as_deref().unwrap_or(&[]);
    let resumed = if fresh || own_source.is_some() {
        if own_source.is_some() && dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] {method_key} bci={}: resuming in the bytecode the body \
                 was compiled from (class redefined since; fresh={fresh})",
                rframe.bci
            );
        }
        let resumed = resume_real_ir_deopt_or_throw(
            shared,
            thread,
            resume_from,
            rframe,
            &compiled.splice_scope_class_ids,
            inner_own_sources,
        );
        // The rebuilt frame runs the code the body was compiled from: stamped
        // with the body's constant-pool generation, so a frame of a body its
        // class's redefinition replaced reads its own constants (interpreter
        // round i1 wave 28, lane L3). For an inlined chain, its OUTERMOST
        // frame, which the chain pushed `caller_frames.len()` frames below the
        // top (wave 37; the inner frames are current, see
        // `DeoptFrameChain::with_outermost_cp_stamp`).
        if matches!(resumed, Ok(Some(CachedCallResult::FramePushed))) {
            if let Some(outermost) = thread
                .frames
                .len()
                .checked_sub(1 + rframe.caller_frames.len())
            {
                super::obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code(
                    shared,
                    thread,
                    outermost,
                    compiled.compile_cp_stamp(),
                );
            }
            // Round 14 wave 3 (lane chain, R14DP-5): and every inner frame
            // built from the bytecode the compile spliced.
            restamp_inner_own_source_frames(
                shared,
                thread,
                rframe,
                inner_own_sources,
                compiled.compile_cp_stamp(),
            );
        }
        resumed
    } else {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] skip resume — artifact epoch {} < live {} for {} (superseded)",
                compiled.compilation_epoch, live, method_key
            );
        }
        Ok(None)
    };
    // De-speculate (record + evict + escalate + bump live epoch). The reason is
    // recovered from the deopt point matching the trapping bci so OSR-exit
    // events are tallied separately from guard deopts.
    //
    // Not for a trap out of a SUPERSEDED artifact (a newer body is already
    // published): `deoptimize` evicts by name, so it would remove the fresh
    // body, and the speculation failure was already charged when the old body
    // was retired. See `trapped_artifact_is_superseded`.
    let superseded = trapped_artifact_is_superseded(shared, cached, compiled);
    // One attribution for the charge and the per-bci de-spec below: the
    // stash's cause when this artifact recognises it, else the bci derivation.
    //
    // An inlined chain (wave 26, lane L2) trapped at a bci of a spliced
    // CALLEE's code, which neither derivation may read against `cached`'s
    // bytecode: it is charged at the call site in `cached`
    // (`stash_charge_site`), under the stash's own reason with the wildcard
    // speculation id, so the per-bci de-spec below withdraws what the
    // artifact speculated at that call site.
    let chain = !rframe.caller_frames.is_empty();
    let named = if chain {
        cause.map(|c| cratonvm_jit::deopt::DeoptCause {
            reason: c.reason,
            speculation_id: cratonvm_jit::deopt::SPECULATION_ID_ANY,
        })
    } else {
        trap_cause_at_frame(compiled, &cached.code, rframe, cause)
    };
    let charge_bci = stash_identity_scope(rframe).bci;
    // Interpreter round i1 wave 15, lane L3: a method-entry body that left at
    // a back-edge poll because its method must now run interpreted
    // (`jvmti_events::exit_left_for_the_interpreter`) is resumed above like
    // any trap, and neither charged nor de-spec'd below. The stash's own cause
    // first, as `named` prefers it.
    let left_for_the_interpreter = cause.or(named).is_some_and(|c| {
        super::exit_left_for_the_interpreter(
            shared,
            c.reason,
            cached.declaring_class_id,
            &cached.method_name,
            &cached.method_descriptor,
        )
    });
    if superseded {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] trap from a superseded artifact of {method_key} bci={} \
                 — not charged, fresh body kept",
                rframe.bci
            );
        }
    } else if left_for_the_interpreter {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] {method_key} bci={}: left for the interpreter-only mode, \
                 not charged",
                rframe.bci
            );
        }
    } else {
        let reason = named
            .map(|c| c.reason)
            .unwrap_or(cratonvm_jit::deopt::DeoptReason::UncommonTrap);
        crate::jit::helpers::DeoptimizationController::deoptimize_in(
            shared,
            cached.declaring_class_id,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            reason,
            charge_bci,
        );
    }

    // deopt-osr Step 9 follow-up (c): per-bci de-spec. `deoptimize` above evicts
    // the whole artifact and (on enough *aggregate* deopts) escalates to a
    // whole-method blacklist. Before that escalation can fire, give the SINGLE
    // speculation site that keeps failing a chance to be dropped on its own: once
    // THIS bci has deopted `PER_BCI_DESPEC_LIMIT` times, record `(method, bci)` in
    // THIS VM's de-spec registry (`shared.jit.despec_registry`), which every
    // compile this VM requests consults, so the next compilation suppresses just that
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
    if !superseded && !left_for_the_interpreter && rframe.bci != u32::MAX {
        // M4 — record WHICH speculation at this bci failed, not just that
        // one did. A protected invoke publishes two points at one bytecode
        // index (a `ReceiverTypeChanged` guard and a `PendingException`
        // frame — `jit/src/osr_exit.rs` calls that the ordinary shape), and
        // under the old `(method, bci)` key four traps on either withdrew
        // BOTH. Asked through `trap_reason_at_frame` (round 11 wave 2: it
        // names the array-access NullCheck / BoundsCheck pair from the frame
        // and skips the loop-header `OsrExit` map), NOT `trap_reason_for_frame`,
        // which folds "cannot name it" into `UncommonTrap` — a real reason,
        // and one whose derived id would withdraw a speculation nothing had
        // said anything about. An undecidable bci records
        // `SPECULATION_ID_ANY`, the wildcard, the coarse behaviour this site
        // had before. Round 11 wave 5: `named` (above) prefers the stash's
        // own cause, so two guards at one non-array bci no longer fall to the
        // wildcard either.
        //
        // The LIMIT is asked at the same grain as the verdict (round 11 wave 4).
        // A named speculation is withdrawn after four traps of ITS OWN, at
        // `(reason, bci)`, which is what the charge above recorded and what
        // HotSpot's `PerBytecodeTrapLimit` counts. Summing every reason at the
        // bci let three `NullCheck` traps and one `BoundsCheck` trap withdraw
        // the bounds-check speculation, which had failed once. Only the
        // wildcard, which withdraws everything at the bci, keeps the
        // whole-bci count. The rule is `despec_site_after_trap_limit`'s (RS-3).
        let withdrawn = despec_site_after_trap_limit(
            shared,
            &method_key,
            charge_bci,
            named.map(|c| c.reason),
            named.map_or(cratonvm_jit::deopt::SPECULATION_ID_ANY, |c| c.speculation_id),
        );
        if let Some(bci_deopts) = withdrawn {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] per-bci de-spec: {} bci={} ({} deopts ≥ {}) — \
                     speculation suppressed on next compile (method stays compilable)",
                    method_key, charge_bci, bci_deopts, PER_BCI_DESPEC_LIMIT
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
    if matches!(resumed, Ok(Some(_))) {
        crate::threading::thread_state::record_transition(
            crate::threading::thread_state::ThreadExecState::JavaRunning,
            "interpreter::real_frame_deopt_resume_and_despeculate",
        );
    }
    resumed
}

/// The `Option` shape of [`real_frame_deopt_resume_or_throw_and_despeculate`]
/// the unit tests below were written against (none of them exhausts the heap).
#[cfg(test)]
pub(super) fn real_frame_deopt_resume_and_despeculate(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
) -> Option<CachedCallResult> {
    real_frame_deopt_resume_or_throw_and_despeculate(
        shared, thread, compiled, cached, rframe, cause, point_addr,
    )
    .ok()
    .flatten()
}

#[cfg(test)]
mod deopt_step3_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_jit::deopt::{FrameValue, ReconstructedFrame, VirtualObjectState};
    use std::sync::Arc;

    fn minimal_cached() -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
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
            },
        ))
    }

    /// As `minimal_cached` but `ACC_SYNCHRONIZED` — exercises the
    /// elided-monitor gate for virtual-object resume.
    fn synchronized_cached() -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
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
            },
        ))
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
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
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

    // -----------------------------------------------------------------------
    // N2 — a sink that asks for a chain gets one, or a named refusal
    // -----------------------------------------------------------------------

    /// An `rframe` naming one inlined caller scope. `caller_frames` runs
    /// innermost-first, so a single-element vec IS the outermost scope.
    fn rframe_with_outer_scope(outer_key: &str, outer_bci: u32) -> ReconstructedFrame {
        let mut rf = rframe(vec![FrameValue::Int(1)], Vec::new(), 3);
        rf.method_key = "T.inlinedCallee:()V".to_string();
        rf.caller_frames = vec![ReconstructedFrame {
            method_key: outer_key.to_string(),
            bci: outer_bci,
            locals: vec![FrameValue::Int(2)],
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics: cratonvm_jit::deopt::ResumeSemantics::for_caller_scope(),
            caller_frames: Vec::new(),
        }];
        rf
    }

    /// The superseded-guard sentinel must be refused BEFORE materialisation,
    /// not carried into it.
    ///
    /// `materialise_inner_scopes` parks the innermost (trapping) scope at
    /// `rframe.bci` verbatim — the CALLER scopes go through `caller_resume_pc`,
    /// which bounds them against their own code, but the trapping scope does
    /// not — so `u32::MAX` would become a resume pc past the end of the
    /// callee's bytecode. `build_deopt_frame_inner` has the same check for the
    /// same reason, which is exactly why the chain path needs its own.
    #[test]
    fn a_superseded_sentinel_is_refused_before_the_chain_is_materialised() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let cached = minimal_cached();
        let mut rf = rframe_with_outer_scope("T.m:()V", 0);
        rf.bci = u32::MAX;
        assert!(
            build_deopt_frame_chain(&shared, &cached, &rf).is_none(),
            "the sentinel exists precisely so this check fails"
        );
    }

    /// An artifact only ever inlines *into* its own body, so a chain whose
    /// outermost scope names some other method is MIS-ROUTED, not deep — the
    /// same fact `deopt_frame_matches_method` establishes for a single frame.
    /// Refusing it is what keeps a nested callee's trap from being materialised
    /// into this method's frame shape.
    #[test]
    fn a_chain_whose_outermost_scope_names_another_method_is_refused() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let cached = minimal_cached(); // T.m:()V
        let rf = rframe_with_outer_scope("Other.n:()I", 0);
        assert!(build_deopt_frame_chain(&shared, &cached, &rf).is_none());
    }

    /// A refusal is NAMED, not folded into the one `inlined-caller-chain`
    /// count.
    ///
    /// The two say opposite things about where the work is owed: the old
    /// counter means *a sink was not taught to ask*, the new one means *we
    /// asked and the VM could not rebuild it*. A reader who cannot tell those
    /// apart cannot tell a missing feature from a broken one — the same
    /// confusion `string_pin_not_asked` exists for on the compile side.
    #[test]
    fn an_unmaterialisable_chain_is_counted_under_its_own_name() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let cached = minimal_cached();
        let before: Vec<(&str, u64)> = deopt_frame_bail_counts();
        let read = |census: &[(&str, u64)], name: &str| -> u64 {
            census
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, c)| *c)
                .expect("the census must carry every reason")
        };

        assert!(build_deopt_frame_chain(
            &shared,
            &cached,
            &rframe_with_outer_scope("Other.n:()I", 0)
        )
        .is_none());

        let after = deopt_frame_bail_counts();
        assert_eq!(
            read(&after, "inlined-caller-chain-unmaterialisable"),
            read(&before, "inlined-caller-chain-unmaterialisable") + 1
        );
        assert_eq!(
            read(&after, "inlined-caller-chain"),
            read(&before, "inlined-caller-chain"),
            "a chain we ASKED for and could not build is not a sink that never asked"
        );
    }

    /// The sink that has NOT been taught still refuses, and is still counted
    /// under the original name.
    ///
    /// `build_deopt_frame_inner` returns one `Frame` by value and cannot
    /// express a chain, so its refusal is correct and must stay. What changed
    /// is that it is no longer the only answer available — the two sinks this
    /// lane owns call `build_deopt_frame_chain` instead.
    #[test]
    fn the_single_frame_builder_still_refuses_a_chain_and_says_so() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        let rf = rframe_with_outer_scope("T.m:()V", 0);
        assert!(build_deopt_frame_inner(&shared, &mut thread, &cached, &rf, false).is_none());
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

    /// Round 11 wave 2 (r11-tier-deopt-resume-gc-windows): a frame that will be
    /// refused anyway is refused BEFORE its virtual objects are materialised.
    /// Materialisation with `keep_pins` leaves each shell pinned, so a pin left
    /// above the watermark is the evidence an allocation (and a possible moving
    /// GC, while the caller's frame was unrooted) happened first.
    #[test]
    fn refusal_precedes_materialisation() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();
        let pin_base = thread.native_pin_roots.len();
        let unmappable_local = rframe(
            vec![
                vobj(0, 5, vec![FrameValue::Int(1)]),
                FrameValue::Unsupported,
            ],
            vec![],
            0,
        );
        assert!(
            build_deopt_frame_inner(&shared, &mut thread, &cached, &unmappable_local, true)
                .is_none()
        );
        assert_eq!(
            thread.native_pin_roots.len(),
            pin_base,
            "no shell may be allocated"
        );
        let unmappable_stack = rframe(
            vec![vobj(0, 5, vec![FrameValue::Int(1)])],
            vec![FrameValue::Unsupported],
            0,
        );
        assert!(
            build_deopt_frame_inner(&shared, &mut thread, &cached, &unmappable_stack, true)
                .is_none()
        );
        assert_eq!(
            thread.native_pin_roots.len(),
            pin_base,
            "no shell may be allocated"
        );
        // `minimal_cached` has max_stack 8: nine entries cannot fit.
        let overfull = rframe(
            vec![vobj(0, 5, vec![FrameValue::Int(1)])],
            vec![FrameValue::Int(0); 9],
            0,
        );
        assert!(build_deopt_frame_inner(&shared, &mut thread, &cached, &overfull, true).is_none());
        assert_eq!(
            thread.native_pin_roots.len(),
            pin_base,
            "no shell may be allocated"
        );
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
                relock: true,
            }],
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
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

    /// A lock the COMPILED code took (`relock == false`, the optimizing tier's
    /// description of a live `synchronized` block) is still held by the thread
    /// when the frame deoptimizes. The rebuilt interpreter frame must go on
    /// holding it exactly once: not released, and not taken a second time,
    /// which would leave it held after the frame's own `monitorexit`.
    #[test]
    fn resumed_frame_keeps_a_monitor_the_compiled_code_holds_without_relocking() {
        use cratonvm_jit::deopt::MonitorInfo;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();

        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object/code pointer to integer address
        let addr = obj.as_ptr() as usize as u64;
        // What the compiled body's `jit_monitor_enter` did before it trapped.
        shared.threads.monitors.enter(obj, thread.thread_id);

        let rf = ReconstructedFrame {
            method_key: "T.m:()V".to_string(),
            bci: 4,
            locals: vec![FrameValue::Object(addr)],
            stack: vec![],
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(addr),
                lock_depth: 1,
                relock: false,
            }],
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        };

        let pin_base = thread.native_pin_roots.len();
        let r = resume_real_ir_deopt(&shared, &mut thread, &cached, &rf)
            .expect("a frame holding a compiled lock must resume");
        assert!(matches!(r, CachedCallResult::FramePushed));
        assert_eq!(thread.native_pin_roots.len(), pin_base);

        let held = match thread.frames.last().unwrap().get_local(0) {
            Value::Object(Some(o)) => o,
            other => panic!("local 0 must be the locked object, got {other:?}"),
        };
        assert!(
            shared.threads.monitors.holds(held, thread.thread_id),
            "the resumed frame must still hold the compiled code's lock"
        );
        assert!(
            shared.threads.monitors.exit(held, thread.thread_id).is_ok(),
            "the frame's own monitorexit releases it"
        );
        assert!(
            shared
                .threads
                .monitors
                .exit(held, thread.thread_id)
                .is_err(),
            "held exactly once: the resume must not have entered it again"
        );
    }

    /// Interpreter round i1 wave 23, lane L2: a refused frame's compiled locks
    /// are released through their pins, so a collection between the take and
    /// the refusal (the materialiser, the pool refill) releases the object at
    /// its current address. Chain scopes count too, and elided locks and
    /// null monitors pin nothing.
    #[test]
    fn a_refused_frames_compiled_locks_are_released_through_their_pins() {
        use cratonvm_jit::deopt::MonitorInfo;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let tid = thread.thread_id;
        let alloc = || {
            shared
                .mem
                .heap
                .alloc_object(cratonvm_types::ClassId::new(7), 0)
        };
        let (inner, outer) = (alloc(), alloc());
        // This test's own roots, so it can find the objects after a move.
        thread.native_pin_roots.push(inner);
        thread.native_pin_roots.push(outer);
        shared.threads.monitors.enter(inner, tid);
        shared.threads.monitors.enter(outer, tid);
        shared.threads.monitors.enter(outer, tid);
        let taken = |obj: ObjectRef, depth: u32| MonitorInfo {
            // Cast: object pointer to the raw word a stash carries.
            object: FrameValue::Object(obj.as_ptr() as usize as u64),
            lock_depth: depth,
            relock: false,
        };
        let mut rf = rframe(vec![], vec![], 2);
        rf.monitors = vec![
            taken(inner, 1),
            MonitorInfo {
                object: FrameValue::Object(0),
                lock_depth: 1,
                relock: false,
            },
            MonitorInfo {
                object: FrameValue::Object(inner.as_ptr() as usize as u64),
                lock_depth: 1,
                relock: true,
            },
        ];
        let mut caller = rframe(vec![], vec![], 1);
        caller.monitors = vec![taken(outer, 2)];
        rf.caller_frames = vec![caller];

        let base = thread.native_pin_roots.len();
        let held = CompiledLocksOfAStash::pin_compiled_locks(&mut thread, &rf);
        assert_eq!(
            thread.native_pin_roots.len(),
            base + 2,
            "one pin per compiled lock; none for a null or an elided one"
        );
        maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-resume");
        assert_eq!(held.release_for_a_rerun(&shared, &mut thread), 3);
        assert_eq!(thread.native_pin_roots.len(), base, "the pins are dropped");
        let (inner_now, outer_now) = (thread.native_pin_roots[0], thread.native_pin_roots[1]);
        assert!(!shared.threads.monitors.holds(inner_now, tid));
        assert!(!shared.threads.monitors.holds(outer_now, tid));

        // A frame that resumes keeps them: pins dropped, nothing released.
        shared.threads.monitors.enter(inner_now, tid);
        let mut again = rframe(vec![], vec![], 2);
        again.monitors = vec![taken(inner_now, 1)];
        let kept = CompiledLocksOfAStash::pin_compiled_locks(&mut thread, &again);
        kept.unpin_keeping_the_locks(&mut thread);
        assert_eq!(thread.native_pin_roots.len(), base);
        assert!(shared.threads.monitors.holds(inner_now, tid));
        assert!(shared.threads.monitors.exit(inner_now, tid).is_ok());
        thread.native_pin_roots.clear();
    }

    /// A monitor the compiled code took (`relock == false`), `depth` deep.
    fn taken_lock(obj: ObjectRef, depth: u32) -> cratonvm_jit::deopt::MonitorInfo {
        cratonvm_jit::deopt::MonitorInfo {
            // Cast: object pointer to the raw word a stash carries.
            object: FrameValue::Object(obj.as_ptr() as usize as u64),
            lock_depth: depth,
            relock: false,
        }
    }

    /// An empty interpreter frame of `minimal_cached`, pins dropped.
    fn empty_live_frame(shared: &SharedVm, thread: &mut JvmThread) -> Frame {
        let cached = minimal_cached();
        let pin_base = thread.native_pin_roots.len();
        let frame = build_deopt_frame_inner(shared, thread, &cached, &rframe(vec![], vec![], 0), false)
            .expect("an empty frame rebuilds");
        thread.native_pin_roots.truncate(pin_base);
        frame
    }

    /// Interpreter round i1 wave 25, lane L2
    /// (`interpreter-L2-a-dropped-own-reason-9-frame-leaves-its-compiled-locks-held-for-the-rerun`):
    /// `static void m(Object lock, Runnable r) { synchronized (lock) { r.run(); } }`
    /// compiled, `r.run()` answered the DEOPT sentinel, and `m`'s reason-9 pad
    /// published its frame naming `lock` and returned the sentinel with no
    /// exception pending. The door's sentinel arm releases `lock` (the door
    /// re-runs `m` from entry, which enters it again). A frame another body
    /// published stops the claim; a frame below the door's entry floor (an
    /// earlier activation's) is never claimed.
    #[test]
    fn a_pad_exit_frame_above_the_floor_releases_its_compiled_locks() {
        std::thread::spawn(|| {
            use cratonvm_jit::deopt::{
                exceptional_stash_depth, restash_exceptional_frame_with_point,
                take_exceptional_frame_above_if, truncate_exceptional_stash_to,
            };
            const OWN: usize = 0x20;
            const OTHER: usize = 0x30;
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let mut thread = JvmThread::new(ThreadId(0), "test");
            let tid = thread.thread_id;
            let alloc = || {
                shared
                    .mem
                    .heap
                    .alloc_object(cratonvm_types::ClassId::new(7), 0)
            };
            let (lock, old) = (alloc(), alloc());
            thread.native_pin_roots.push(lock);
            thread.native_pin_roots.push(old);
            // An earlier activation's leftover, below the floor the door records.
            shared.threads.monitors.enter(old, tid);
            let mut leftover = rframe(vec![], vec![], 3);
            leftover.monitors = vec![taken_lock(old, 1)];
            restash_exceptional_frame_with_point(leftover, None, OWN);
            let floor = exceptional_stash_depth();
            // This activation: its compiled `monitorenter`, then the pad.
            shared.threads.monitors.enter(lock, tid);
            let mut pad = rframe(vec![], vec![], 5);
            pad.monitors = vec![taken_lock(lock, 1)];
            restash_exceptional_frame_with_point(pad, None, OWN);
            // A same-named frame another body published, on top.
            let mut foreign = rframe(vec![], vec![], 5);
            foreign.monitors = vec![taken_lock(lock, 1)];
            restash_exceptional_frame_with_point(foreign, None, OTHER);
            let owns = |p: usize| p == OWN;

            let vm: &SharedVm = &shared;
            let claim = |thread: &mut JvmThread| {
                release_locks_of_own_pad_exits(vm, thread, &owns, "T", "m", "()V", floor, None)
            };
            assert_eq!(claim(&mut thread), 0, "another body's frame stops the claim");
            assert!(shared.threads.monitors.holds(lock, tid));
            assert!(take_exceptional_frame_above_if(floor, |_, p| p == OTHER).is_some());

            let pins = thread.native_pin_roots.len();
            assert_eq!(claim(&mut thread), 1);
            assert_eq!(thread.native_pin_roots.len(), pins, "the pins are dropped");
            let (lock_now, old_now) = (thread.native_pin_roots[0], thread.native_pin_roots[1]);
            assert!(!shared.threads.monitors.holds(lock_now, tid), "the pad's hold is released");
            assert_eq!(exceptional_stash_depth(), floor, "the claimed frame is gone");
            assert!(
                shared.threads.monitors.holds(old_now, tid),
                "a frame below the floor is not claimed"
            );
            assert_eq!(claim(&mut thread), 0);
            assert!(shared.threads.monitors.exit(old_now, tid).is_ok());
            assert_eq!(truncate_exceptional_stash_to(0), 1);
            thread.native_pin_roots.clear();
        })
        .join()
        .expect("test thread");
    }

    /// Interpreter round i1 wave 25, lane L2: the OSR door's sentinel arm
    /// continues the LIVE frame, so only the holds its record does not name
    /// are released: the frame entered `a` before the OSR entry (recorded),
    /// the body entered `a` again and `b`; the pad frame names `a` twice and
    /// `b` once. `a` stays held once, by the record's entry; `b` is released.
    #[test]
    fn an_osr_pad_exit_releases_only_what_the_live_record_does_not_name() {
        std::thread::spawn(|| {
            use cratonvm_jit::deopt::{exceptional_stash_depth, restash_exceptional_frame_with_point};
            const OWN: usize = 0x40;
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let mut thread = JvmThread::new(ThreadId(0), "test");
            let tid = thread.thread_id;
            let alloc = || {
                shared
                    .mem
                    .heap
                    .alloc_object(cratonvm_types::ClassId::new(7), 0)
            };
            let (a, b) = (alloc(), alloc());
            let live = empty_live_frame(&shared, &mut thread);
            thread.frames.push(live);
            let frame_idx = thread.frames.len() - 1;
            // Before the entry: the interpreter's `monitorenter a`.
            shared.threads.monitors.enter(a, tid);
            thread.frames[frame_idx].held_monitors.push(a);
            // The body's own.
            shared.threads.monitors.enter(a, tid);
            shared.threads.monitors.enter(b, tid);
            let floor = exceptional_stash_depth();
            let mut pad = rframe(vec![], vec![], 5);
            pad.monitors = vec![taken_lock(a, 2), taken_lock(b, 1)];
            restash_exceptional_frame_with_point(pad, None, OWN);

            let owns = |p: usize| p == OWN;
            let released = release_locks_of_own_pad_exits(
                &shared,
                &mut thread,
                &owns,
                "T",
                "m",
                "()V",
                floor,
                Some((frame_idx, &OsrEntryHolds::default())),
            );
            assert_eq!(released, 2, "one level of `a` and `b`");
            assert!(!shared.threads.monitors.holds(b, tid));
            assert_eq!(
                thread.frames[frame_idx].held_monitors.as_slice(),
                &[a],
                "the record is the interpreter's and is left as it was"
            );
            assert!(shared.threads.monitors.exit(a, tid).is_ok(), "held once more");
            assert!(!shared.threads.monitors.holds(a, tid), "and only once");
            assert_eq!(exceptional_stash_depth(), floor);
        })
        .join()
        .expect("test thread");
    }

    /// Interpreter round i1 wave 28, lane L2
    /// (`docs/internal/fixed-bugs/interpreter-L2-release-beyond-live-record-trusts-an-under-stated-record-FIXED-20260929.md`):
    /// the live frame holds `o` through its own `monitorenter` (javac keeps
    /// it in local 1) while its record is EMPTY — a committed single-pass
    /// exit that named no lock emptied it. The OSR entry's snapshot
    /// (`OsrEntryHolds::snapshot`, what `try_osr` takes before the body runs)
    /// names `o` once; a CALLER frame's hold of `c` counts too, and cancels
    /// out. A pad frame that names `o` (pre-entry, `relock == false`) is
    /// released no further than the entry's count, even across a collection;
    /// holds the body gained are released. With no snapshot (the record rule
    /// alone), the same pad released the interpreter's own hold.
    #[test]
    fn an_osr_pad_exit_keeps_the_holds_the_frame_had_at_the_entry() {
        std::thread::spawn(|| {
            use cratonvm_jit::deopt::{exceptional_stash_depth, restash_exceptional_frame_with_point};
            const OWN: usize = 0x50;
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let mut thread = JvmThread::new(ThreadId(0), "test");
            let tid = thread.thread_id;
            let alloc = || {
                shared
                    .mem
                    .heap
                    .alloc_object(cratonvm_types::ClassId::new(7), 0)
            };
            let (o, c) = (alloc(), alloc());
            let live = empty_live_frame(&shared, &mut thread);
            thread.frames.push(live);
            let frame_idx = thread.frames.len() - 1;
            assert!(thread.frames[frame_idx].locals_len() >= 4, "the frame has local slots");
            // Local 1 and 3 name `o` (the source local and javac's temp), local
            // 2 names `c`, which a caller holds once and this frame once.
            thread.frames[frame_idx].set_local_unchecked(1, Value::Object(Some(o)));
            thread.frames[frame_idx].set_local_unchecked(2, Value::Object(Some(c)));
            thread.frames[frame_idx].set_local_unchecked(3, Value::Object(Some(o)));
            shared.threads.monitors.enter(o, tid);
            shared.threads.monitors.enter(c, tid);
            shared.threads.monitors.enter(c, tid);
            assert!(thread.frames[frame_idx].held_monitors.is_empty(), "the record under-states");
            let entry = OsrEntryHolds::snapshot(&shared, &thread, frame_idx);
            assert_eq!(entry.slots, vec![(1, 1), (2, 2)], "one row per owned object");

            let owns = |p: usize| p == OWN;
            let floor = exceptional_stash_depth();
            let stash_pad = |monitors: Vec<cratonvm_jit::deopt::MonitorInfo>| {
                let mut pad = rframe(vec![], vec![], 5);
                pad.monitors = monitors;
                restash_exceptional_frame_with_point(pad, None, OWN);
            };
            let release = |thread: &mut JvmThread, entry: &OsrEntryHolds| {
                release_locks_of_own_pad_exits(
                    &shared,
                    thread,
                    &owns,
                    "T",
                    "m",
                    "()V",
                    floor,
                    Some((frame_idx, entry)),
                )
            };

            // 1. The body took nothing: the pad names the pre-entry holds only.
            stash_pad(vec![taken_lock(o, 1), taken_lock(c, 1)]);
            maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-resume");
            assert_eq!(release(&mut thread, &entry), 0, "nothing the entry held is released");
            // The objects at their current addresses, through the frame's locals.
            let now = |thread: &JvmThread, slot: usize| match thread.frames[frame_idx]
                .get_local_unchecked(slot)
            {
                Value::Object(Some(obj)) => Some(obj),
                _ => None,
            };
            let o_now = now(&thread, 1).expect("local 1 keeps its reference");
            let c_now = now(&thread, 2).expect("local 2 keeps its reference");
            assert_eq!(shared.threads.monitors.entry_count(o_now), 1);
            assert_eq!(shared.threads.monitors.entry_count(c_now), 2);

            // 2. The body entered `o` again and `p`: exactly those are released.
            let p = alloc();
            shared.threads.monitors.enter(o_now, tid);
            shared.threads.monitors.enter(p, tid);
            stash_pad(vec![taken_lock(o_now, 2), taken_lock(c_now, 1), taken_lock(p, 1)]);
            assert_eq!(release(&mut thread, &entry), 2, "the body's `o` and `p`");
            assert!(!shared.threads.monitors.holds(p, tid));
            assert_eq!(shared.threads.monitors.entry_count(o_now), 1, "the frame's own `o`");
            assert_eq!(shared.threads.monitors.entry_count(c_now), 2, "the frame's and the caller's `c`");
            assert_eq!(exceptional_stash_depth(), floor);

            // 3. Positive control: the record rule alone releases the
            // interpreter's own hold of `o` (the page's defect).
            stash_pad(vec![taken_lock(o_now, 1)]);
            assert_eq!(release(&mut thread, &OsrEntryHolds::default()), 1);
            assert!(!shared.threads.monitors.holds(o_now, tid));
            for _ in 0..2 {
                assert!(shared.threads.monitors.exit(c_now, tid).is_ok());
            }
            assert!(!shared.threads.monitors.holds(c_now, tid));
        })
        .join()
        .expect("test thread");
    }

    /// Interpreter round i1 wave 25, lane L2 (stage 2 of proposal
    /// `i23-L7-proposal-per-frame-locked-monitors`, the OSR half): an in-place
    /// resume REPLACES the live frame's record with what the exit frame names
    /// — a pre-entry lock the body released since leaves it, a lock the body
    /// took joins it, `lock_depth` copies each — and a frame that names no
    /// lock (a single-pass body's, a monitor-free guard exit) empties it.
    /// Nothing is entered or released, and the pins are dropped.
    #[test]
    fn an_osr_in_place_resume_replaces_the_live_frames_record() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let tid = thread.thread_id;
        let alloc = || {
            shared
                .mem
                .heap
                .alloc_object(cratonvm_types::ClassId::new(7), 0)
        };
        let (kept, released_before, taken) = (alloc(), alloc(), alloc());
        let live = empty_live_frame(&shared, &mut thread);
        thread.frames.push(live);
        let frame_idx = thread.frames.len() - 1;
        thread.frames[frame_idx].held_monitors.push(kept);
        thread.frames[frame_idx].held_monitors.push(released_before);
        shared.threads.monitors.enter(kept, tid);
        shared.threads.monitors.enter(taken, tid);
        shared.threads.monitors.enter(taken, tid);
        let mut exit = rframe(vec![], vec![], 2);
        exit.monitors = vec![taken_lock(kept, 1), taken_lock(taken, 2)];
        let base = thread.native_pin_roots.len();
        let locks = CompiledLocksOfAStash::pin_compiled_locks(&mut thread, &exit);
        maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-resume");
        locks.replace_live_record_and_unpin(&mut thread, frame_idx);
        assert_eq!(thread.native_pin_roots.len(), base, "the pins are dropped");
        let record = thread.frames[frame_idx].held_monitors.as_slice().to_vec();
        assert_eq!(record.len(), 3);
        assert_eq!(record[1], record[2], "the taken lock, twice");
        assert!(shared.threads.monitors.holds(record[0], tid), "nothing is released");
        assert!(shared.threads.monitors.holds(record[1], tid));

        // A frame that names no lock empties the record.
        let none = CompiledLocksOfAStash::pin_compiled_locks(&mut thread, &rframe(vec![], vec![], 2));
        none.replace_live_record_and_unpin(&mut thread, frame_idx);
        assert!(thread.frames[frame_idx].held_monitors.is_empty());
        assert_eq!(thread.native_pin_roots.len(), base);
    }

    /// Interpreter round i1 wave 27, lane L2: the OSR door's planless
    /// transfer re-takes an exit frame's elided level, and the record then
    /// names it too — `synchronized (o) { synchronized (o) { loop } }` left
    /// at the loop records `o` twice. With the record short of it (wave
    /// 26), the frame's inner `monitorexit` spent the outer level's entry,
    /// and a later `release_beyond_live_record` released the interpreter's
    /// own hold. The elided pin is never released: a refusal still releases
    /// only the taken level.
    #[test]
    fn an_osr_transfer_that_re_takes_an_elided_level_records_it() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let tid = thread.thread_id;
        let o = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        let live = empty_live_frame(&shared, &mut thread);
        thread.frames.push(live);
        let frame_idx = thread.frames.len() - 1;
        let elided = |obj: ObjectRef| cratonvm_jit::deopt::MonitorInfo {
            // Cast: object pointer to the raw word a stash carries.
            object: FrameValue::Object(obj.as_ptr() as usize as u64),
            lock_depth: 1,
            relock: true,
        };
        let mut exit = rframe(vec![], vec![], 2);
        exit.monitors = vec![taken_lock(o, 1), elided(o)];
        let base = thread.native_pin_roots.len();

        // The plain pin leaves the elided level out, as before.
        let plain = CompiledLocksOfAStash::pin_compiled_locks(&mut thread, &exit);
        assert_eq!(thread.native_pin_roots.len(), base + 1);
        plain.unpin_keeping_the_locks(&mut thread);

        // The outer level held, the elided one re-taken by the transfer.
        shared.threads.monitors.enter(o, tid);
        let locks = CompiledLocksOfAStash::pin_compiled_locks_and_elided_levels(&mut thread, &exit);
        assert_eq!(thread.native_pin_roots.len(), base + 2, "the elided level is pinned too");
        shared.threads.monitors.enter(o, tid);
        maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-resume");
        locks.replace_live_record_with_retaken_levels_and_unpin(&mut thread, frame_idx);
        assert_eq!(thread.native_pin_roots.len(), base, "the pins are dropped");
        let record = thread.frames[frame_idx].held_monitors.as_slice().to_vec();
        assert_eq!(record.len(), 2, "the taken level and the re-taken one");
        assert_eq!(record[0], record[1]);
        let now = record[0];
        assert_eq!(shared.threads.monitors.entry_count(now), 2, "nothing entered or released");

        // A refusal with the elided level pinned releases the taken level
        // beyond the (now empty) record, and never the elided one.
        thread.frames[frame_idx].held_monitors.clear();
        let mut refused = rframe(vec![], vec![], 2);
        refused.monitors = vec![taken_lock(now, 1), elided(now)];
        let pinned =
            CompiledLocksOfAStash::pin_compiled_locks_and_elided_levels(&mut thread, &refused);
        assert_eq!(
            pinned.release_beyond_live_record(
                &shared,
                &mut thread,
                frame_idx,
                &OsrEntryHolds::default()
            ),
            1
        );
        assert_eq!(thread.native_pin_roots.len(), base);
        assert_eq!(shared.threads.monitors.entry_count(now), 1);
        assert!(shared.threads.monitors.exit(now, tid).is_ok());
        assert!(!shared.threads.monitors.holds(now, tid));
    }

    /// Round 13 wave 8 (lane sync5; part (c) of
    /// `r13w6-chain2-osr-guard-exit-chain-lock-record-patch-FIXED-20260928.md`): an
    /// in-place transfer of an inlined CHAIN writes the OUTERMOST scope into
    /// the live frame and pushes the inner scopes, each seeded with its own
    /// locks by `push_inlined_chain`. So the live frame's record is the
    /// outermost scope's taken locks and ITS elided levels, never the trapping
    /// scope's (which the pushed callee frame records). A single-scope frame
    /// is unchanged (the test above).
    #[test]
    fn a_chain_transfer_records_the_outermost_scopes_locks_in_the_live_frame() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let alloc = || {
            shared
                .mem
                .heap
                .alloc_object(cratonvm_types::ClassId::new(7), 0)
        };
        let (outer_obj, inner_obj) = (alloc(), alloc());
        let live = empty_live_frame(&shared, &mut thread);
        thread.frames.push(live);
        let frame_idx = thread.frames.len() - 1;
        let elided = |obj: ObjectRef| cratonvm_jit::deopt::MonitorInfo {
            // Cast: object pointer to the raw word a stash carries.
            object: FrameValue::Object(obj.as_ptr() as usize as u64),
            lock_depth: 1,
            relock: true,
        };
        let mut chain = rframe_with_outer_scope("T.m:()V", 4);
        chain.monitors = vec![taken_lock(inner_obj, 2), elided(inner_obj)];
        chain.caller_frames[0].monitors = vec![taken_lock(outer_obj, 1), elided(outer_obj)];
        let base = thread.native_pin_roots.len();

        // Both scopes' taken locks, then the OUTERMOST scope's elided level.
        let locks =
            CompiledLocksOfAStash::pin_compiled_locks_and_elided_levels(&mut thread, &chain);
        assert_eq!(thread.native_pin_roots.len(), base + 3);
        assert_eq!(
            thread.native_pin_roots[base + 2],
            outer_obj,
            "the outermost elided level"
        );
        locks.replace_live_record_with_retaken_levels_and_unpin(&mut thread, frame_idx);
        assert_eq!(thread.native_pin_roots.len(), base, "the pins are dropped");
        assert_eq!(
            thread.frames[frame_idx].held_monitors.as_slice().to_vec(),
            vec![outer_obj, outer_obj],
            "the outermost scope's taken lock and its re-taken level, not the callee's"
        );

        // The plain replace: the outermost scope's taken lock alone.
        let plain = CompiledLocksOfAStash::pin_compiled_locks(&mut thread, &chain);
        plain.replace_live_record_and_unpin(&mut thread, frame_idx);
        assert_eq!(
            thread.frames[frame_idx].held_monitors.as_slice().to_vec(),
            vec![outer_obj]
        );
        assert_eq!(thread.native_pin_roots.len(), base);

        // An outermost scope that holds nothing leaves the record empty, however
        // many locks the callee holds.
        chain.caller_frames[0].monitors.clear();
        let callee_only =
            CompiledLocksOfAStash::pin_compiled_locks_and_elided_levels(&mut thread, &chain);
        callee_only.replace_live_record_with_retaken_levels_and_unpin(&mut thread, frame_idx);
        assert!(thread.frames[frame_idx].held_monitors.is_empty());
        assert_eq!(thread.native_pin_roots.len(), base);
    }

    /// Interpreter round i1 wave 25, lane L2: every frame of a pushed inlined
    /// chain records the locks its own scope's compiled code holds
    /// (`scope_taken_locks`, seeded by `push_inlined_chain`) — a chain resume
    /// and the frames an OSR-exit chain transfer pushes used to start with an
    /// empty record. An elided, null or zero-depth monitor is not listed.
    #[test]
    fn an_inlined_chains_frames_record_their_scopes_locks() {
        use cratonvm_jit::deopt::MonitorInfo;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let alloc = || {
            shared
                .mem
                .heap
                .alloc_object(cratonvm_types::ClassId::new(7), 0)
        };
        let (a, b) = (alloc(), alloc());
        let mut scope = rframe(vec![], vec![], 0);
        scope.monitors = vec![
            taken_lock(b, 2),
            MonitorInfo {
                object: FrameValue::Object(0),
                lock_depth: 1,
                relock: false,
            },
            MonitorInfo {
                // Cast: object pointer to the raw word a stash carries.
                object: FrameValue::Object(a.as_ptr() as usize as u64),
                lock_depth: 1,
                relock: true,
            },
            taken_lock(a, 0),
        ];
        assert_eq!(scope_taken_locks(&scope), vec![(b, 2)]);

        let chain = DeoptFrameChain::ready(vec![
            InlinedChainFrame {
                cached: minimal_cached(),
                locals: Vec::new(),
                stack: Vec::new(),
                resume_pc: 0,
                executing_pc: 0,
                monitors: vec![(a, 1)],
            },
            InlinedChainFrame {
                cached: minimal_cached(),
                locals: Vec::new(),
                stack: Vec::new(),
                resume_pc: 0,
                executing_pc: 0,
                monitors: scope_taken_locks(&scope),
            },
        ]);
        let base = thread.frames.len();
        assert!(matches!(
            push_inlined_chain(&shared, &mut thread, chain, false),
            Some(CachedCallResult::FramePushed)
        ));
        assert_eq!(thread.frames[base].held_monitors.as_slice(), &[a]);
        assert_eq!(thread.frames[base + 1].held_monitors.as_slice(), &[b, b]);
    }

    /// Interpreter round i1 wave 24, lane L2: the OSR-exit chain transfer
    /// rewrites the live (outermost) frame from a COMPACT local list, so a
    /// `long` before an `int` must land in slots 0-1 and 2, not 0 and 1 (it was
    /// written index for index, shifting every local after a cat-2 one).
    #[test]
    fn compact_locals_are_written_in_place_by_jvm_slot() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached(); // max_locals = 4
        let mut frame = build_deopt_frame_inner(
            &shared,
            &mut thread,
            &cached,
            &rframe(vec![], vec![], 0),
            false,
        )
        .expect("an empty frame rebuilds");
        let compact = [Value::Long(7), Value::Int(3)];
        assert_eq!(compact_locals_slot_count(&compact), 3);
        assert!(write_compact_locals_in_place(&mut frame, &compact).is_ok());
        // A long is stored as its raw bits (`CompactValue::long`), which the
        // context-free `get_local` decodes as a double: read the slot raw.
        assert_eq!(frame.get_local_raw(0) as i64, 7);
        assert_eq!(frame.get_local(2), Value::Int(3), "the int after a long is local 2");
        // Five slots do not fit four: refused, and nothing is written.
        let too_wide = [Value::Long(1), Value::Long(2), Value::Int(9)];
        assert!(write_compact_locals_in_place(&mut frame, &too_wide).is_err());
        assert_eq!(frame.get_local_raw(0) as i64, 7);
    }

    /// Interpreter round i1 wave 24, lane L2: a rebuilt frame's
    /// `held_monitors` record names every lock the compiled frame held — a
    /// lock the compiled code took and an elided level the rebuild re-took,
    /// `lock_depth` copies each, outermost first — so the structured-locking
    /// rules (`interpreter::held_monitors`) see them as HotSpot's unpacked
    /// frame's monitor block does. The seeding a handler-frame sink does
    /// (`CompiledLocksOfAStash::seed_frame_and_unpin`) records the trapping
    /// scope's locks only, never an inlined caller scope's.
    #[test]
    fn a_rebuilt_frame_records_the_locks_its_compiled_frame_held() {
        use cratonvm_jit::deopt::MonitorInfo;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let tid = thread.thread_id;
        let cached = minimal_cached();
        let alloc = || {
            shared
                .mem
                .heap
                .alloc_object(cratonvm_types::ClassId::new(7), 0)
        };
        let (taken, elided, outer) = (alloc(), alloc(), alloc());
        // Cast: object pointer to the raw word a stash carries.
        let word = |o: ObjectRef| o.as_ptr() as usize as u64;
        let monitor = |o: ObjectRef, depth: u32, relock: bool| MonitorInfo {
            object: FrameValue::Object(word(o)),
            lock_depth: depth,
            relock,
        };
        // What the compiled body's `jit_monitor_enter`s did before it trapped.
        shared.threads.monitors.enter(taken, tid);
        shared.threads.monitors.enter(taken, tid);
        let mut rf = rframe(
            vec![FrameValue::Object(word(taken)), FrameValue::Object(word(elided))],
            vec![],
            0,
        );
        rf.monitors = vec![monitor(taken, 2, false), monitor(elided, 1, true)];
        let pin_base = thread.native_pin_roots.len();
        let frame = build_deopt_frame_inner(&shared, &mut thread, &cached, &rf, false)
            .expect("a frame holding a taken and an elided lock rebuilds");
        thread.native_pin_roots.truncate(pin_base);
        assert_eq!(
            frame.held_monitors.as_slice(),
            &[taken, taken, elided],
            "every hold of the compiled frame, lock_depth copies each, outermost first"
        );
        assert!(shared.threads.monitors.holds(elided, tid), "the elided level is re-taken");

        // A handler frame seeded from a chain's pins records only the trapping
        // scope's lock; the caller scope's lock belongs to another frame.
        shared.threads.monitors.enter(outer, tid);
        let mut inner = rframe(vec![], vec![], 0);
        inner.monitors = vec![monitor(taken, 1, false)];
        let mut caller = rframe(vec![], vec![], 0);
        caller.monitors = vec![monitor(outer, 1, false)];
        inner.caller_frames = vec![caller];
        let mut handler = build_deopt_frame_inner(&shared, &mut thread, &cached, &rframe(vec![], vec![], 0), false)
            .expect("an empty frame rebuilds");
        thread.native_pin_roots.truncate(pin_base);
        assert!(handler.held_monitors.is_empty());
        let locks = CompiledLocksOfAStash::pin_compiled_locks(&mut thread, &inner);
        assert_eq!(thread.native_pin_roots.len(), pin_base + 2);
        locks.seed_frame_and_unpin(&mut thread, &mut handler);
        assert_eq!(handler.held_monitors.as_slice(), &[taken]);
        assert_eq!(thread.native_pin_roots.len(), pin_base, "the pins are dropped");
        assert!(
            shared.threads.monitors.holds(outer, tid) && shared.threads.monitors.holds(taken, tid),
            "seeding releases nothing"
        );

        // The `CRATONVM_IR_DEOPT_RESUME` sink records a taken lock the same way.
        let mut int_only = rframe(vec![FrameValue::Int(1)], vec![], 0);
        int_only.monitors = vec![monitor(outer, 1, false)];
        let depth = thread.frames.len();
        assert!(matches!(
            resume_from_ir_deopt(&shared, &mut thread, &cached, &int_only),
            Some(CachedCallResult::FramePushed)
        ));
        assert_eq!(thread.frames[depth].held_monitors.as_slice(), &[outer]);
    }

    /// An ELIDED lock on an allocation that escape analysis kept (the object is
    /// a real reference, not a virtual one) must be acquired by the resume:
    /// the compiled code never locked it, and the interpreter frame will run
    /// the matching `monitorexit`.
    #[test]
    fn resumed_frame_relocks_an_elided_monitor_on_a_real_object() {
        use cratonvm_jit::deopt::MonitorInfo;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = minimal_cached();

        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object/code pointer to integer address
        let addr = obj.as_ptr() as usize as u64;
        assert!(!shared.threads.monitors.holds(obj, thread.thread_id));

        let rf = ReconstructedFrame {
            method_key: "T.m:()V".to_string(),
            bci: 4,
            locals: vec![FrameValue::Object(addr)],
            stack: vec![],
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(addr),
                lock_depth: 1,
                relock: true,
            }],
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        };

        let r = resume_real_ir_deopt(&shared, &mut thread, &cached, &rf)
            .expect("an elided-lock frame must resume");
        assert!(matches!(r, CachedCallResult::FramePushed));
        let held = match thread.frames.last().unwrap().get_local(0) {
            Value::Object(Some(o)) => o,
            other => panic!("local 0 must be the object, got {other:?}"),
        };
        assert!(
            shared.threads.monitors.exit(held, thread.thread_id).is_ok(),
            "the resume acquired the elided lock"
        );
        assert!(
            shared
                .threads
                .monitors
                .exit(held, thread.thread_id)
                .is_err(),
            "exactly once"
        );
    }

    /// A2 elided-monitor gate, narrowed in round 12 wave 7 (lane replay4): an
    /// INSTANCE `synchronized` method whose receiver slot (local 0) is itself
    /// virtual must NOT resume (the one shape in which the method monitor
    /// could have been a scalar-replaced object) -- it falls back with no pins
    /// leaked and no frame pushed.
    #[test]
    fn virtual_resume_blocked_for_synchronized_method() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cached = synchronized_instance_cached();
        let rf = rframe(vec![vobj(0, 5, vec![FrameValue::Int(1)])], vec![], 0);

        assert!(resume_real_ir_deopt(&shared, &mut thread, &cached, &rf).is_none());
        assert_eq!(thread.native_pin_roots.len(), 0);
        assert_eq!(thread.frames.len(), 0);
    }

    /// As [`synchronized_cached`] but an instance method (`this` in local 0).
    fn synchronized_instance_cached() -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
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
                is_static: false,
            },
        ))
    }

    /// Round 12 wave 7 (lane replay4,
    /// `r12w6-hunter4-synchronized-method-with-virtuals-reruns-from-entry`): a
    /// `static synchronized` method's frame naming a scalar-replaced object
    /// resumes (its monitor is the class mirror, held by the door), and so
    /// does an instance one whose receiver slot is a real object. Before
    /// wave 7 both were refused and every door re-ran the method from entry.
    #[test]
    fn a_synchronized_frame_with_a_virtual_non_receiver_resumes() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let pin_base = thread.native_pin_roots.len();
        let rf = rframe(vec![vobj(0, 5, vec![FrameValue::Int(1)])], vec![], 0);
        let r = resume_real_ir_deopt(&shared, &mut thread, &synchronized_cached(), &rf)
            .expect("a static synchronized frame with a virtual object must resume");
        assert!(matches!(r, CachedCallResult::FramePushed));
        assert_eq!(thread.native_pin_roots.len(), pin_base);
        assert_eq!(thread.frames.len(), 1);

        let receiver = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object pointer to integer address
        let addr = receiver.as_ptr() as usize as u64;
        let rf = rframe(
            vec![FrameValue::Object(addr), vobj(0, 5, vec![FrameValue::Int(2)])],
            vec![],
            0,
        );
        let r = resume_real_ir_deopt(&shared, &mut thread, &synchronized_instance_cached(), &rf)
            .expect("an instance synchronized frame with a real receiver must resume");
        assert!(matches!(r, CachedCallResult::FramePushed));
        assert_eq!(thread.frames.len(), 2);
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

    /// Round 11 wave 2 (r11-tier-deopt-stash-drops-the-trap-reason, VM half):
    /// the optimizing tier's NullCheck + BoundsCheck pair at one array access
    /// is told apart by the frame's own array reference, so neither trap is
    /// charged as `UncommonTrap` nor de-spec'd through the wildcard id.
    #[test]
    fn array_access_trap_reason_is_read_off_the_frame() {
        use cratonvm_jit::deopt::DeoptReason;
        let mut cm = cm_with_deopt_point(0, 3);
        let mut null_point = cm.deopt_points[0].clone();
        null_point.reason = DeoptReason::NullCheck;
        cm.deopt_points.push(null_point);
        // bci 3 is an `iaload`.
        let code = [0x00u8, 0x00, 0x00, 0x2e, 0xac];
        let null_array = rframe(vec![], vec![FrameValue::Object(0), FrameValue::Int(1)], 3);
        let bad_index = rframe(
            vec![],
            vec![FrameValue::Object(0x1000), FrameValue::Int(99)],
            3,
        );
        assert_eq!(
            trap_reason_at_frame(&cm, &code, &null_array),
            Some(DeoptReason::NullCheck)
        );
        assert_eq!(
            trap_reason_at_frame(&cm, &code, &bad_index),
            Some(DeoptReason::BoundsCheck)
        );
        // Not an array access: the pair stays unattributable.
        let not_array = [0x00u8, 0x00, 0x00, 0x60, 0xac];
        assert_eq!(trap_reason_at_frame(&cm, &not_array, &bad_index), None);
        assert_eq!(
            trap_reason_for_frame(&cm, &not_array, &bad_index),
            DeoptReason::UncommonTrap
        );
        // A single point is answered as itself, frame or not.
        let single = cm_with_deopt_point(0, 3);
        assert_eq!(
            trap_reason_at_frame(&single, &not_array, &null_array),
            Some(DeoptReason::BoundsCheck)
        );
    }

    /// Round 11 wave 4: the per-bci de-spec LIMIT is counted at the grain of
    /// the speculation it withdraws, `(reason, bci)`. At one array access with
    /// the optimizing tier's NullCheck + BoundsCheck pair, three null traps and
    /// one bounds trap withdraw nothing (the whole-bci sum used to reach the
    /// limit and withdraw the bounds check, which had failed once); a fourth
    /// null trap withdraws the null check alone.
    #[test]
    fn per_bci_despec_limit_counts_the_named_speculation_only() {
        use cratonvm_jit::deopt::{speculation_id, DeoptReason};
        let key = "T.m:()V";
        let cached = Arc::new(CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(0),
            class_name: Arc::from("T"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            // bci 3 is an `iaload`.
            code: Arc::from(&[0x00u8, 0x00, 0x00, 0x2e, 0xac][..]),
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 8,
            max_locals: 4,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            hidden_frame: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            tiering_settled: std::sync::atomic::AtomicU32::new(0),
            branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
            pool_generation: u64::MAX,
        });
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut cm = cm_with_deopt_point(0, 3);
        let mut null_point = cm.deopt_points[0].clone();
        null_point.reason = DeoptReason::NullCheck;
        cm.deopt_points.push(null_point);
        // `RESUME` frames: the resume refuses at `NotAResumePoint` before any
        // slot is mapped, so no frame holding the fake array address below is
        // ever pushed, and the charge + de-spec accounting still runs.
        let trap = |array: u64| ReconstructedFrame {
            method_key: key.to_string(),
            bci: 3,
            locals: vec![],
            stack: vec![FrameValue::Object(array), FrameValue::Int(1)],
            monitors: Vec::new(),
            semantics: cratonvm_jit::deopt::ResumeSemantics::RESUME,
            caller_frames: Vec::new(),
        };
        let null_spec = speculation_id(3, DeoptReason::NullCheck);
        let bounds_spec = speculation_id(3, DeoptReason::BoundsCheck);
        for _ in 0..3 {
            let _ = real_frame_deopt_resume_and_despeculate(
                &shared,
                &mut thread,
                &cm,
                &cached,
                &trap(0),
                None,
                0,
            );
        }
        let _ = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &trap(0x1000),
            None,
            0,
        );
        assert_eq!(thread.frames.len(), 0, "no trap in this test may resume");
        assert!(
            !shared.jit.despec_registry.contains(key, 3),
            "3 null + 1 bounds trap must withdraw neither speculation"
        );
        let _ = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &trap(0),
            None,
            0,
        );
        assert!(
            shared
                .jit
                .despec_registry
                .contains_speculation(key, 3, null_spec),
            "the fourth null trap withdraws the null check"
        );
        assert!(
            !shared
                .jit
                .despec_registry
                .contains_speculation(key, 3, bounds_spec),
            "the bounds check failed once and must stay speculated"
        );
    }

    /// Round 11 wave 5 (r11w4-tier-vm-sinks-should-read-the-stashed-deopt-cause):
    /// two guards at one NON-array bci cannot be told apart from the frame, so
    /// before the sinks read the stash's cause every trap there was charged as
    /// `UncommonTrap` and four withdrew both speculations through the wildcard.
    /// With the cause, the charge and the de-spec name the guard that fired.
    #[test]
    fn the_stashed_cause_names_the_guard_the_bci_cannot() {
        use cratonvm_jit::deopt::{speculation_id, DeoptCause, DeoptReason, SPECULATION_ID_ANY};
        let key = "T.m:()V";
        let cached = Arc::new(CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(0),
            class_name: Arc::from("T"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            // bci 3 is an `iadd`: not an array access.
            code: Arc::from(&[0x00u8, 0x00, 0x00, 0x60, 0xac][..]),
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 8,
            max_locals: 4,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            hidden_frame: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            tiering_settled: std::sync::atomic::AtomicU32::new(0),
            branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
            pool_generation: u64::MAX,
        });
        let bounds_spec = speculation_id(3, DeoptReason::BoundsCheck);
        let receiver_spec = speculation_id(3, DeoptReason::ReceiverTypeChanged);
        let mut cm = cm_with_deopt_point(0, 3);
        cm.deopt_points[0].speculation_id = bounds_spec;
        let mut receiver_point = cm.deopt_points[0].clone();
        receiver_point.reason = DeoptReason::ReceiverTypeChanged;
        receiver_point.speculation_id = receiver_spec;
        cm.deopt_points.push(receiver_point);
        // `RESUME` frames refuse at `NotAResumePoint`, so nothing is pushed and
        // only the charge + de-spec accounting runs.
        let trap = ReconstructedFrame {
            method_key: key.to_string(),
            bci: 3,
            locals: vec![],
            stack: vec![FrameValue::Int(1), FrameValue::Int(2)],
            monitors: Vec::new(),
            semantics: cratonvm_jit::deopt::ResumeSemantics::RESUME,
            caller_frames: Vec::new(),
        };
        let bounds = DeoptCause {
            reason: DeoptReason::BoundsCheck,
            speculation_id: bounds_spec,
        };
        // Without a cause the bci is undecidable; with one it is named. A
        // cause this artifact has no such point for is ignored.
        assert_eq!(trap_cause_at_frame(&cm, &cached.code, &trap, None), None);
        assert_eq!(
            trap_cause_at_frame(&cm, &cached.code, &trap, Some(bounds)),
            Some(bounds)
        );
        let foreign = DeoptCause {
            reason: DeoptReason::NullCheck,
            speculation_id: speculation_id(3, DeoptReason::NullCheck),
        };
        assert_eq!(
            trap_cause_at_frame(&cm, &cached.code, &trap, Some(foreign)),
            None
        );
        assert_eq!(
            trap_reason_for_frame_with_cause(&cm, &cached.code, &trap, Some(foreign)),
            DeoptReason::UncommonTrap
        );
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        for _ in 0..4 {
            let _ = real_frame_deopt_resume_and_despeculate(
                &shared,
                &mut thread,
                &cm,
                &cached,
                &trap,
                Some(bounds),
                0,
            );
        }
        assert_eq!(thread.frames.len(), 0, "no trap in this test may resume");
        assert_eq!(
            shared
                .jit
                .deopt_log
                .lock()
                .deopt_count_at_site(key, DeoptReason::BoundsCheck, 3),
            4
        );
        let registry = &shared.jit.despec_registry;
        assert!(registry.contains_speculation(key, 3, bounds_spec));
        assert!(
            !registry.contains_speculation(key, 3, SPECULATION_ID_ANY)
                && !registry.contains_speculation(key, 3, receiver_spec),
            "the receiver guard never failed and must stay speculated"
        );
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
        let r = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &rf,
            None,
            0,
        )
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

        let r = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &rf,
            None,
            0,
        )
        .expect("superseded artifact must still resume in retain mode");
        assert!(matches!(r, CachedCallResult::FramePushed));
        assert_eq!(thread.frames.len(), 1);
        // Recorded for the deopt rate as before.
        assert_eq!(shared.jit.deopt_log.lock().deopt_count("T.m:()V"), 1);
    }

    /// deopt-osr Step 9 follow-up (c): per-bci de-spec. After
    /// `PER_BCI_DESPEC_LIMIT` deopts at the SAME bci, the sink records
    /// `(method, bci)` in the de-spec registry the optimizing backend consults
    /// (`DespecRegistry::contains`) — so that ONE speculation is suppressed on
    /// the next compile instead of the whole method being blacklisted. Fewer
    /// deopts, or a different bci, do not de-spec. The registry is this test's
    /// own `SharedVm`'s, so no other test can see or perturb it.
    #[test]
    fn step9_fuc_per_bci_despec_after_limit() {
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
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
            },
        ));
        let key = "DespecFuC.loop:()V";
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
                semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
                caller_frames: Vec::new(),
            };
            let _ = real_frame_deopt_resume_and_despeculate(
                &shared,
                &mut thread,
                &cm,
                &cached,
                &rf,
                None,
                0,
            );
            let despec_now = shared.jit.despec_registry.contains(key, 5);
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
        assert!(!shared.jit.despec_registry.contains(key, 9));
        // And a second VM in the same process inherits none of it.
        let other = SharedVm::new(VmConfig::default());
        assert!(
            !other.jit.despec_registry.contains(key, 5),
            "a second VM must not inherit the first VM's despeculation"
        );
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
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
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
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        };
        let frames_before = thread.frames.len();
        let r = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &foreign,
            None,
            0,
        );
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

    /// r11-tier: the identity-less re-run sentinel names no owner, so it is
    /// charged to the sink's own method instead of to nobody.
    #[test]
    fn a_rerun_sentinel_is_charged_to_the_sink_method() {
        let cached = minimal_cached(); // "T"/"m"/"()V"
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cm = cm_with_deopt_point(0, 3);
        let sentinel = ReconstructedFrame {
            method_key: String::new(),
            bci: u32::MAX,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        };
        let frames_before = thread.frames.len();
        let r = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &sentinel,
            None,
            0,
        );
        assert!(r.is_none(), "a sentinel must never resume");
        assert_eq!(thread.frames.len(), frames_before);
        let log = shared.jit.deopt_log.lock();
        assert!(
            log.deopt_count_at_bci("T.m:()V", u32::MAX) >= 1,
            "the sink's own method must be charged for its sentinel"
        );
    }

    /// r11-tier-deopt-storm-from-superseded-artifacts-and-no-recompile-cutoff,
    /// the last by-name sink: a FOREIGN frame stashed by a body its owner no
    /// longer publishes is not charged, and the owner's fresh body stays
    /// published. A frame from the published body is charged and evicts it,
    /// exactly as before.
    #[test]
    fn a_foreign_frame_from_a_superseded_body_keeps_the_fresh_body() {
        use cratonvm_jit::deopt::DeoptimizationPoint;
        let cached = minimal_cached(); // "T"/"m"/"()V"
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cm = cm_with_deopt_point(0, 3);
        let (class, method, desc) = ("Inner/Fresh", "trap", "(I)I");
        // No class of that name is loaded, so the sink resolves id 0 by name,
        // as `DeoptimizationController::deoptimize` does.
        let id0 = cratonvm_types::ClassId::new(0);
        shared.jit.jit_cache.write().put(
            Arc::from(class),
            Arc::from(method),
            Arc::from(desc),
            id0,
            cm_with_deopt_point(0, 3),
        );
        let fresh = shared
            .jit
            .jit_cache
            .read()
            .get(class, method, desc, id0)
            .expect("the fresh body is published");
        let fresh_point = &fresh.deopt_points[0] as *const DeoptimizationPoint as usize;
        let retired = cm_with_deopt_point(0, 3);
        let retired_point = &retired.deopt_points[0] as *const DeoptimizationPoint as usize;
        let foreign = ReconstructedFrame {
            method_key: format!("{class}.{method}:{desc}"),
            bci: 3,
            locals: vec![FrameValue::Int(7)],
            stack: vec![],
            monitors: Vec::new(),
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        };
        let key = "Inner/Fresh.trap:(I)I";

        let r = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &foreign,
            None,
            retired_point,
        );
        assert!(r.is_none(), "a foreign frame never resumes here");
        assert_eq!(
            shared.jit.deopt_log.lock().deopt_count_at_bci(key, 3),
            0,
            "a trap out of a superseded body must not be charged"
        );
        let live = shared.jit.jit_cache.read().get(class, method, desc, id0);
        assert!(
            live.is_some_and(|live| Arc::ptr_eq(&live, &fresh)),
            "the fresh body must stay published"
        );

        let r = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &foreign,
            None,
            fresh_point,
        );
        assert!(r.is_none());
        assert!(
            shared.jit.deopt_log.lock().deopt_count_at_bci(key, 3) >= 1,
            "a trap out of the published body is charged"
        );
        assert!(
            shared
                .jit
                .jit_cache
                .read()
                .get(class, method, desc, id0)
                .is_none(),
            "and evicts it, as before"
        );
    }

    /// Interpreter round i1 wave 10, lane L4 (i7-L7 deopt sinks matched by
    /// name): the stash's point address identifies the body that fired it, so
    /// a frame whose key names the sink's own method but whose point belongs
    /// to ANOTHER body (another loader's copy, another activation's artifact)
    /// is refused; an unknown point (0) falls back to the name.
    #[test]
    fn a_same_named_frame_from_another_body_is_not_resumed() {
        use cratonvm_jit::deopt::DeoptimizationPoint;
        let cm = cm_with_deopt_point(0, 3);
        let other = cm_with_deopt_point(0, 3);
        let own_point = &cm.deopt_points[0] as *const DeoptimizationPoint as usize;
        let other_point = &other.deopt_points[0] as *const DeoptimizationPoint as usize;
        assert!(deopt_stash_is_from_artifact(&cm, own_point));
        assert!(
            deopt_stash_is_from_artifact(&cm, 0),
            "unknown: the name decides"
        );
        assert!(!deopt_stash_is_from_artifact(&cm, other_point));

        let same_named = ReconstructedFrame {
            method_key: "T.m:()V".to_string(),
            bci: 3,
            locals: vec![FrameValue::Int(7)],
            stack: vec![],
            monitors: Vec::new(),
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        };
        assert!(deopt_frame_matches_artifact(
            &same_named,
            &cm,
            own_point,
            "T",
            "m",
            "()V"
        ));
        assert!(deopt_frame_matches_artifact(
            &same_named,
            &cm,
            0,
            "T",
            "m",
            "()V"
        ));
        assert!(!deopt_frame_matches_artifact(
            &same_named,
            &cm,
            other_point,
            "T",
            "m",
            "()V"
        ));
        assert!(
            !deopt_frame_matches_artifact(&same_named, &cm, own_point, "T", "n", "()V"),
            "the name must still match"
        );

        let cached = minimal_cached(); // "T"/"m"/"()V"
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let frames_before = thread.frames.len();
        let r = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &same_named,
            None,
            other_point,
        );
        assert!(r.is_none(), "another body's frame must not resume here");
        assert_eq!(thread.frames.len(), frames_before);

        // Another body's key-less sentinel is not charged to the sink method.
        let sentinel = ReconstructedFrame {
            method_key: String::new(),
            bci: u32::MAX,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        };
        let r = real_frame_deopt_resume_and_despeculate(
            &shared,
            &mut thread,
            &cm,
            &cached,
            &sentinel,
            None,
            other_point,
        );
        assert!(r.is_none());
        assert_eq!(
            shared
                .jit
                .deopt_log
                .lock()
                .deopt_count_at_bci("T.m:()V", u32::MAX),
            0,
            "another body's sentinel is not the sink method's trap"
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
    /// NOT reject the whole transfer. Before this fix, ANY such slot rejected
    /// the entire transfer — discarding real, already-committed OSR side effects
    /// and forcing the interpreter to silently re-execute them from stale
    /// pre-OSR state (the root cause documented in
    /// jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md).
    ///
    /// The fixture hands the transfer a reconstructed frame directly, with a
    /// hand-built plan whose deopt point describes no locals, so slot 1 is
    /// neither live nor dead in any bytecode sense; the test pins the transfer's
    /// no-refusal rule, not a liveness verdict. In compiled code a slot dead at
    /// the exit bci is published `Undefined` and a live undescribable one refuses
    /// the OSR entry at admission — see
    /// jit-resume-tolerates-unsupported-locals-without-liveness-FIXED-20260912.md
    /// and the jit tests it names.
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
    /// first is tolerated (the live value stays; compile-time liveness and the
    /// admission veto keep a live one out of an admitted artifact), the second
    /// refuses. That asymmetry is the entire reason for the split variant.
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

    // ── round 12 wave 3 (lane replay): the loop-closed extent and the
    //    first-arrival fact (`r12w2-irbuild-replay-prefix-ignores-loops`) ──

    /// `for (i = 0; i < 10; i++) { <bci 8>; STATIC = 1; }` — the fixture
    /// `jit::ir::r12w2_irbuild_tests` uses. The store is reachable only
    /// THROUGH bci 8.
    ///
    /// ```text
    ///  0: iconst_0   1: istore_0   2: iload_0   3: bipush 10   5: if_icmpge 19
    ///  8: nop        9: iconst_1  10: putstatic #1  13: iinc 0 1  16: goto 2
    /// 19: return
    /// ```
    const R12W3_LOOP_STORE_AFTER_TRAP: [u8; 20] = [
        0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0e, 0x00, 0x04, 0xb3, 0x00, 0x01, 0x84, 0x00,
        0x01, 0xa7, 0xff, 0xf2, 0xb1,
    ];

    /// `for (i = 0; i < 10; i++) { if (i > 0) <bci 12>; STATIC = 1; }` —
    /// iteration 0 commits the putstatic before bci 12 is ever reached.
    ///
    /// ```text
    ///  0: iconst_0   1: istore_0   2: iload_0   3: bipush 10   5: if_icmpge 23
    ///  8: iload_0    9: ifle 13   12: nop      13: iconst_1   14: putstatic #1
    /// 17: iinc 0 1  20: goto 2    23: return
    /// ```
    const R12W3_LOOP_TRAP_SKIPPED_FIRST: [u8; 24] = [
        0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x12, 0x1a, 0x9e, 0x00, 0x04, 0x00, 0x04, 0xb3,
        0x00, 0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xee, 0xb1,
    ];

    fn r12w3_frame_at(
        bci: u32,
        semantics: cratonvm_jit::deopt::ResumeSemantics,
    ) -> cratonvm_jit::deopt::ReconstructedFrame {
        cratonvm_jit::deopt::ReconstructedFrame {
            method_key: String::new(),
            bci,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics,
            caller_frames: Vec::new(),
        }
    }

    /// A conditional deopt inside a loop: earlier iterations ran the store
    /// textually AFTER the resume point, so the prefix rule's `true` was a
    /// silent double commit. Before the loop the answer is unchanged.
    #[test]
    fn r12w3_a_resume_point_inside_a_loop_sees_the_store_after_it() {
        assert!(!replay_from_entry_is_observably_equivalent(
            &R12W3_LOOP_TRAP_SKIPPED_FIRST,
            true,
            12
        ));
        assert!(!replay_from_entry_is_observably_equivalent(
            &R12W3_LOOP_STORE_AFTER_TRAP,
            true,
            8
        ));
        assert!(replay_from_entry_is_observably_equivalent(
            &R12W3_LOOP_TRAP_SKIPPED_FIRST,
            true,
            1
        ));
    }

    /// The same bci with the first-arrival fact: a trap that fires the first
    /// time control reaches it cannot have let the store run, so the replay is
    /// exact — and it is still refused where iteration 0 skipped the trap, and
    /// still vetoed by an impure spliced body.
    #[test]
    fn r12w3_a_first_arrival_trap_replays_exactly_and_nothing_else_does() {
        use cratonvm_jit::deopt::ResumeSemantics;
        let conditional = ResumeSemantics::REEXECUTE;
        let first = conditional.with_first_arrival(true);
        assert!(replay_from_entry_is_observably_equivalent_for_frame(
            &R12W3_LOOP_STORE_AFTER_TRAP,
            true,
            &r12w3_frame_at(8, first),
            None
        ));
        assert!(!replay_from_entry_is_observably_equivalent_for_frame(
            &R12W3_LOOP_STORE_AFTER_TRAP,
            true,
            &r12w3_frame_at(8, conditional),
            None
        ));
        assert!(!replay_from_entry_is_observably_equivalent_for_frame(
            &R12W3_LOOP_TRAP_SKIPPED_FIRST,
            true,
            &r12w3_frame_at(12, first),
            None
        ));
        assert!(!replay_from_entry_is_observably_equivalent_for_frame(
            &R12W3_LOOP_STORE_AFTER_TRAP,
            false,
            &r12w3_frame_at(8, first),
            None
        ));
        // The fact changes nothing about how the frame resumes.
        assert_eq!(first.resume_kind(), ResumeSemantics::REEXECUTE);
    }

    // -----------------------------------------------------------------------
    // r9-vmside
    // -----------------------------------------------------------------------

    /// `resume_from_ir_deopt` (the `CRATONVM_IR_DEOPT_RESUME` sink, consulted
    /// before the real-frame sink at every call site) must refuse a frame that
    /// a DIFFERENT method stashed — the identity gate
    /// `real_frame_deopt_resume_and_despeculate` has — and still resume one
    /// that names `cached`. Both directions, so a gate rewritten to a constant
    /// fails.
    #[test]
    fn ir_deopt_resume_refuses_a_foreign_frame_and_accepts_its_own() {
        std::thread::spawn(|| {
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let mut thread = JvmThread::new(ThreadId(0), "test");
            let cached = minimal_cached(); // "T.m:()V"

            let mut foreign = rframe(vec![FrameValue::Int(5)], Vec::new(), 0);
            foreign.method_key = "Other.callee:(I)V".to_string();
            assert!(
                resume_from_ir_deopt(&shared, &mut thread, &cached, &foreign).is_none(),
                "a nested callee's frame must never be resumed as this method's"
            );
            assert!(thread.frames.is_empty(), "a refusal pushes nothing");

            let own = rframe(vec![FrameValue::Int(5)], Vec::new(), 0);
            let r = resume_from_ir_deopt(&shared, &mut thread, &cached, &own)
                .expect("a frame naming this method resumes");
            assert!(matches!(r, CachedCallResult::FramePushed));
            assert_eq!(thread.frames.len(), 1);
            assert_eq!(thread.frames[0].get_local(0), Value::Int(5));
        })
        .join()
        .expect("test thread");
    }

    /// A snapshot whose operand stack is deeper than the method's `max_stack`
    /// is refused before a pooled frame is built.
    #[test]
    fn ir_deopt_resume_refuses_a_snapshot_that_does_not_fit() {
        std::thread::spawn(|| {
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let mut thread = JvmThread::new(ThreadId(0), "test");
            let cached = minimal_cached(); // max_stack = 8
            let rf = rframe(Vec::new(), vec![FrameValue::Int(1); 9], 0);
            assert!(resume_from_ir_deopt(&shared, &mut thread, &cached, &rf).is_none());
            assert!(thread.frames.is_empty());
        })
        .join()
        .expect("test thread");
    }

    /// r11-tier: the identity-less re-run sentinel passed the key-only identity
    /// gate and was pushed as a frame at pc `u32::MAX`; it is refused now, and
    /// so is any bci past the method's code.
    #[test]
    fn ir_deopt_resume_refuses_the_rerun_sentinel_and_an_out_of_range_bci() {
        std::thread::spawn(|| {
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let mut thread = JvmThread::new(ThreadId(0), "test");
            let cached = minimal_cached(); // one-byte body
            let mut sentinel = rframe(Vec::new(), Vec::new(), u32::MAX);
            sentinel.method_key = String::new();
            assert!(resume_from_ir_deopt(&shared, &mut thread, &cached, &sentinel).is_none());
            let past = rframe(Vec::new(), Vec::new(), 7);
            assert!(resume_from_ir_deopt(&shared, &mut thread, &cached, &past).is_none());
            assert!(thread.frames.is_empty(), "a refusal pushes nothing");
        })
        .join()
        .expect("test thread");
    }

    /// `minimal_cached` under another method name, so two chain frames carry
    /// distinguishable JVMTI method ids.
    fn cached_named(method_name: &str) -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: cratonvm_types::ClassId::new(0),
                class_name: Arc::from("T"),
                method_name: Arc::from(method_name),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: Arc::from(&[0xb1u8][..]), // return
                exception_table: Arc::from(Vec::new().into_boxed_slice()),
                max_stack: 8,
                max_locals: 4,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ))
    }

    /// A chain is pushed WHOLE, and announces no MethodEntry (interpreter
    /// round i1 wave 9): every method in it was entered in compiled code, and
    /// HotSpot posts no entry for a deoptimized frame. It used to post one per
    /// frame, mid-body, at the resume pc.
    #[test]
    fn an_inlined_chain_is_pushed_whole_and_announces_no_method_entry() {
        use crate::runtime::jvmti::{self, EventCallbacks, EventMode, JvmtiEventKind};
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let vm = shared.vm_identity;
            jvmti::install_manager_for_vm(vm, Arc::new(jvmti::JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            assert_eq!(mgr.vm_identity(), vm);
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&seen);
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            mgr.set_event_callbacks(EventCallbacks {
                method_entry: Some(Box::new(move |_t, m| sink.lock().unwrap().push(m))),
                ..Default::default()
            })
            .expect("install callbacks");

            let mut thread = JvmThread::new(ThreadId(0), "test");
            let chain = DeoptFrameChain::ready(vec![
                InlinedChainFrame {
                    cached: cached_named("outer"),
                    locals: vec![Value::Int(1)],
                    stack: Vec::new(),
                    resume_pc: 0,
                    executing_pc: 0,
                    monitors: Vec::new(),
                },
                InlinedChainFrame {
                    cached: cached_named("inner"),
                    locals: vec![Value::Int(2)],
                    stack: vec![Value::Int(3)],
                    resume_pc: 0,
                    executing_pc: 0,
                    monitors: Vec::new(),
                },
            ]);
            let r = push_inlined_chain(&shared, &mut thread, chain, false);
            assert!(matches!(r, Some(CachedCallResult::FramePushed)));
            assert_eq!(thread.frames.len(), 2);
            assert_eq!(thread.frames[0].method_name(), "outer");
            assert_eq!(thread.frames[1].method_name(), "inner");
            assert_eq!(thread.frames[1].get_local(0), Value::Int(2));
            assert_eq!(thread.frames[1].stack.len(), 1);
            assert!(
                seen.lock().unwrap().is_empty(),
                "a resumed frame was entered in compiled code: no MethodEntry"
            );
            // The listener is live: an ordinary push still announces itself.
            let fresh = crate::runtime::frame::Frame::new_pooled(
                cratonvm_types::ClassId::new(0),
                Arc::from("T"),
                Arc::from("called"),
                Arc::from("()V"),
                None,
                Arc::from(&[0xb1u8][..]),
                Arc::from(Vec::new().into_boxed_slice()),
                8,
                4,
                &[],
                &mut thread.locals_pool,
                &mut thread.stacks_pool,
            );
            push_frame_and_fire_entry(vm, &mut thread, fresh);
            assert_eq!(
                *seen.lock().unwrap(),
                vec![synth_method_id(&thread.frames[2])]
            );
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// The single-frame sink announces no MethodEntry either (interpreter
    /// round i1 wave 9) — the same rule as the chain above.
    #[test]
    fn a_resumed_single_frame_announces_no_method_entry() {
        use crate::runtime::jvmti::{self, EventCallbacks, EventMode, JvmtiEventKind};
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let vm = shared.vm_identity;
            jvmti::install_manager_for_vm(vm, Arc::new(jvmti::JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&seen);
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            mgr.set_event_callbacks(EventCallbacks {
                method_entry: Some(Box::new(move |_t, m| sink.lock().unwrap().push(m))),
                ..Default::default()
            })
            .expect("install callbacks");

            let mut thread = JvmThread::new(ThreadId(0), "test");
            let cached = minimal_cached(); // "T.m:()V"
            let own = rframe(vec![FrameValue::Int(5)], Vec::new(), 0);
            let r = resume_from_ir_deopt(&shared, &mut thread, &cached, &own)
                .expect("a frame naming this method resumes");
            assert!(matches!(r, CachedCallResult::FramePushed));
            assert_eq!(thread.frames.len(), 1);
            assert!(
                seen.lock().unwrap().is_empty(),
                "a resumed frame was entered in compiled code: no MethodEntry"
            );
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// Round 13 wave 9 (lane chain4): a synchronized instance method's frame
    /// whose only monitor is the hold its self-locking body kept on `this` at
    /// a guard resumes OWNING it as the method monitor (`monitor_on_exit`),
    /// in both single-frame builders; recorded as a block monitor its return
    /// would throw `IllegalMonitorStateException`. A lock on another object,
    /// or in a method with monitor ops, stays a block monitor.
    #[test]
    fn a_handed_method_monitor_becomes_the_frames_monitor_on_exit() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let alloc = || {
            shared
                .mem
                .heap
                .alloc_object(cratonvm_types::ClassId::new(7), 0)
        };
        let (this, other) = (alloc(), alloc());
        // Cast: object pointer to the raw word a stash carries.
        let word = |o: ObjectRef| FrameValue::Object(o.as_ptr() as usize as u64);
        let cached = synchronized_instance_cached();
        let mut rf = rframe(vec![word(this)], vec![], 0);
        rf.monitors = vec![taken_lock(this, 1)];
        assert!(frame_hands_over_method_monitor(true, false, &cached.code, &rf));
        let frame = build_deopt_frame_inner(&shared, &mut thread, &cached, &rf, false)
            .expect("the frame rebuilds");
        assert_eq!(frame.monitor_on_exit, Some(this));
        assert!(frame.held_monitors.is_empty());

        let base = thread.frames.len();
        let r = resume_from_ir_deopt(&shared, &mut thread, &cached, &rf)
            .expect("the int-only sink resumes it too");
        assert!(matches!(r, CachedCallResult::FramePushed));
        assert_eq!(thread.frames[base].monitor_on_exit, Some(this));
        assert!(thread.frames[base].held_monitors.is_empty());

        let mut foreign = rframe(vec![word(this)], vec![], 0);
        foreign.monitors = vec![taken_lock(other, 1)];
        assert!(!frame_hands_over_method_monitor(true, false, &cached.code, &foreign));
        let frame = build_deopt_frame_inner(&shared, &mut thread, &cached, &foreign, false)
            .expect("the frame rebuilds");
        assert_eq!(frame.monitor_on_exit, None);
        assert_eq!(frame.held_monitors.as_slice(), &[other]);

        assert!(!frame_hands_over_method_monitor(false, false, &cached.code, &rf));
        // aload_0; monitorenter; return
        assert!(!frame_hands_over_method_monitor(true, false, &[0x2a, 0xc2, 0xb1], &rf));
    }

    /// Round 13 wave 11 (lane sync7): a `static synchronized` method's frame
    /// whose only monitor is one taken level -- the class mirror a static
    /// self-locking body handed over at a guard -- resumes owning it as its
    /// `monitor_on_exit`, in both single-frame builders, whatever its locals
    /// hold. Two monitors, or a method with monitor ops, keep block monitors.
    #[test]
    fn a_handed_class_mirror_becomes_the_static_frames_monitor_on_exit() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let alloc = || {
            shared
                .mem
                .heap
                .alloc_object(cratonvm_types::ClassId::new(7), 0)
        };
        let (mirror, other) = (alloc(), alloc());
        let cached = synchronized_cached();
        assert!(cached.is_static && cached.is_synchronized);
        let mut rf = rframe(vec![FrameValue::Int(3)], vec![], 0);
        rf.monitors = vec![taken_lock(mirror, 1)];
        assert!(frame_hands_over_method_monitor(true, true, &cached.code, &rf));
        let frame = build_deopt_frame_inner(&shared, &mut thread, &cached, &rf, false)
            .expect("the frame rebuilds");
        assert_eq!(frame.monitor_on_exit, Some(mirror));
        assert!(frame.held_monitors.is_empty());

        let base = thread.frames.len();
        let r = resume_from_ir_deopt(&shared, &mut thread, &cached, &rf)
            .expect("the int-only sink resumes it too");
        assert!(matches!(r, CachedCallResult::FramePushed));
        assert_eq!(thread.frames[base].monitor_on_exit, Some(mirror));
        assert!(thread.frames[base].held_monitors.is_empty());

        let mut two = rframe(vec![FrameValue::Int(3)], vec![], 0);
        two.monitors = vec![taken_lock(mirror, 1), taken_lock(other, 1)];
        assert!(!frame_hands_over_method_monitor(true, true, &cached.code, &two));
        let mut deep = rframe(vec![FrameValue::Int(3)], vec![], 0);
        deep.monitors = vec![taken_lock(mirror, 2)];
        assert!(!frame_hands_over_method_monitor(true, true, &cached.code, &deep));
        // ldc; monitorenter; return
        assert!(!frame_hands_over_method_monitor(true, true, &[0x12, 0x01, 0xc2, 0xb1], &rf));
    }
}

/// Round 12 wave 6 (lane jni): a trap out of a body compiled after its class's
/// last redefinition resumes; one compiled before it does not.
#[cfg(test)]
mod r12w6_jni_current_body_tests {
    use super::{compiled_from_the_current_bytecode, resume_current_body_of_redefined_class};
    use crate::classloading::ClassId;

    fn body_stamped_now() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.install_epoch = cratonvm_jit::jit_install_epoch();
        cm
    }

    #[test]
    fn only_a_body_compiled_after_the_redefinition_is_current() {
        if !resume_current_body_of_redefined_class() {
            return;
        }
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let redefined = ClassId::new(7);
        let other = ClassId::new(8);
        let before = body_stamped_now();
        assert!(compiled_from_the_current_bytecode(&shared, &before, redefined));
        let _ = shared
            .jit
            .jit_cache
            .invalidate_for_redefinition(redefined, "p/Redefined");
        assert!(!compiled_from_the_current_bytecode(&shared, &before, redefined));
        // Another class's redefinition barrier says nothing about this one.
        assert!(compiled_from_the_current_bytecode(&shared, &before, other));
        let after = body_stamped_now();
        assert!(compiled_from_the_current_bytecode(&shared, &after, redefined));
    }
}

/// Round 12 wave 4 (lane replay2): the door re-run verdict reads a stashed
/// frame's bci only when the frame is this body's own.
#[cfg(test)]
mod r12w4_replay2_door_tests {
    use super::{door_rerun_verdict, DoorRerunCause};
    use cratonvm_jit::deopt::{
        DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ReconstructedFrame,
        ResumeSemantics,
    };

    /// `putstatic` at bci 1, then a loop storing at bci 13 (the fixture of
    /// `cratonvm_jit::deopt`'s `r12w4_replay2_tests`).
    const STORE_THEN_LOOP: [u8; 23] = [
        0x04, 0xb3, 0x00, 0x01, 0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0x05, 0xb3, 0x00,
        0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];

    fn body_with_a_point() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.deopt_points.push(DeoptimizationPoint {
            native_offset: 0,
            bci: 4,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            semantics: ResumeSemantics::REEXECUTE,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: "C.m:()V".to_string(),
                bci: 4,
                locals: Vec::new(),
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
        });
        cm
    }

    fn frame(key: &str, bci: u32) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: key.to_string(),
            bci,
            semantics: ResumeSemantics::REEXECUTE,
            ..Default::default()
        }
    }

    #[test]
    fn the_verdict_reads_the_bci_only_of_this_bodys_own_frame() {
        let code = STORE_THEN_LOOP;
        let ran = body_with_a_point();
        let own = ran.deopt_points.as_ptr() as usize;
        let verdict = |stash: Option<(&ReconstructedFrame, usize)>| {
            door_rerun_verdict(&code, &ran, "C", "m", "()V", stash)
        };
        // Before the first store: the re-run is the same execution.
        assert_eq!(
            verdict(Some((&frame("C.m:()V", 0), own))),
            (DoorRerunCause::OwnStash, true)
        );
        // After it: the re-run commits the putstatic again.
        assert_eq!(
            verdict(Some((&frame("C.m:()V", 4), own))),
            (DoorRerunCause::OwnStash, false)
        );
        // Another method's frame, or a point this body does not own: its bci
        // says nothing about this attempt, so the whole body answers.
        assert_eq!(
            verdict(Some((&frame("D.n:()V", 0), own))),
            (DoorRerunCause::ForeignStash, false)
        );
        assert_eq!(
            verdict(Some((&frame("C.m:()V", 0), 1))),
            (DoorRerunCause::ForeignStash, false)
        );
        assert_eq!(verdict(None), (DoorRerunCause::Frameless, false));
        // Interpreter round i1 wave 28, lane L2: an inlined chain whose
        // OUTERMOST scope is this method is this body's own, and its bci (the
        // spliced callee's 0, which would read as "before the first store" in
        // `code`) is not read: the whole body answers.
        let mut chain = frame("D.n:()V", 0);
        chain.caller_frames = vec![frame("C.m:()V", 4)];
        assert_eq!(verdict(Some((&chain, own))), (DoorRerunCause::OwnStash, false));
        let mut other_chain = frame("C.m:()V", 0);
        other_chain.caller_frames = vec![frame("D.n:()V", 4)];
        assert_eq!(
            verdict(Some((&other_chain, own))),
            (DoorRerunCause::ForeignStash, false),
            "a chain of another method stays foreign"
        );
        assert!(!super::replay_from_entry_is_observably_equivalent_for_stash(
            &code, &ran, &chain, own
        ));
        // A body that commits nothing re-runs exactly whatever stopped it.
        let pure = [0x1a, 0x1b, 0x60, 0xac]; // iload_0 iload_1 iadd ireturn
        assert_eq!(
            door_rerun_verdict(&pure, &ran, "C", "m", "()V", None),
            (DoorRerunCause::Frameless, true)
        );
    }
}

/// Round 12 wave 5 (lane replay3): the full-heap re-materialisation answer and
/// the callee services' census rows.
#[cfg(test)]
mod r12w5_replay3_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame};
    use std::sync::Arc;

    fn frame_holding(addr: u64, lock_depth: u32, relock: bool) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: "T.m:()V".to_string(),
            bci: 4,
            locals: vec![FrameValue::Object(addr)],
            stack: vec![],
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(addr),
                lock_depth,
                relock,
            }],
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        }
    }

    /// HotSpot unlocks the monitors of the frames it pops after a failed
    /// reallocation; the abandoned activation's compiled locks must not
    /// outlive it, at every level the compiled code took.
    #[test]
    fn an_abandoned_activation_releases_the_locks_its_compiled_code_took() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object pointer to integer address
        let addr = obj.as_ptr() as usize as u64;
        shared.threads.monitors.enter(obj, thread.thread_id);
        shared.threads.monitors.enter(obj, thread.thread_id);
        let rf = frame_holding(addr, 2, false);
        let base = pin_compiled_held_monitors(&mut thread, &rf);
        assert_eq!(thread.native_pin_roots.len(), base + 1, "the held lock is pinned");
        release_compiled_held_monitors(&shared, &mut thread, &rf, base);
        assert_eq!(thread.native_pin_roots.len(), base, "the pins are released");
        assert!(
            !shared.threads.monitors.holds(obj, thread.thread_id),
            "both levels the compiled code took are released"
        );
    }

    /// An elided lock (`relock`) was never taken by the compiled code, so an
    /// abandoned activation has nothing of it to release.
    #[test]
    fn an_elided_lock_is_not_released_for_an_abandoned_activation() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object pointer to integer address
        let addr = obj.as_ptr() as usize as u64;
        // A lock some OTHER activation of this thread holds.
        shared.threads.monitors.enter(obj, thread.thread_id);
        let rf = frame_holding(addr, 1, true);
        let base = pin_compiled_held_monitors(&mut thread, &rf);
        assert_eq!(thread.native_pin_roots.len(), base, "nothing to pin");
        release_compiled_held_monitors(&shared, &mut thread, &rf, base);
        assert!(shared.threads.monitors.holds(obj, thread.thread_id));
        assert!(shared.threads.monitors.exit(obj, thread.thread_id).is_ok());
    }

    /// Only the allocator's heap-exhaustion shape is answered with
    /// `OutOfMemoryError`; a malformed graph stays an ordinary refusal.
    #[test]
    fn only_heap_exhaustion_is_the_rematerialisation_oom() {
        assert!(materialisation_failed_for_heap(&MethodCallFailed::InternalError(
            VmError::Runtime(RuntimeError::OutOfMemoryError {
                message: "Java heap space (alloc_object with 2 fields)".to_string(),
            })
        )));
        assert!(!materialisation_failed_for_heap(&MethodCallFailed::InternalError(
            VmError::Internal {
                message: "deopt materialize: frame references unknown virtual object id 3"
                    .to_string(),
            }
        )));
        assert!(frame_build_heap_failure_is_answered(
            DeoptFrameBail::VirtualMaterialiseHeap
        ));
        assert!(!frame_build_heap_failure_is_answered(
            DeoptFrameBail::VirtualMaterialise
        ));
    }

    /// The new refusal and the two callee services are census rows a reader
    /// can find by name, and a callee re-run is counted under its own cause.
    #[test]
    fn the_census_names_the_heap_refusal_and_the_callee_services() {
        assert!(deopt_frame_bail_counts()
            .iter()
            .any(|(n, _)| *n == "virtual-object-materialise-heap-exhausted"));
        let unsound = |name: &str| {
            door_rerun_census()
                .into_iter()
                .find(|(n, _, _)| *n == name)
                .map(|(_, _, u)| u)
        };
        let before = unsound("callee-declined").expect("the callee-declined row");
        assert!(unsound("callee-frameless").is_some(), "the callee-frameless row");
        note_door_rerun(DoorRerunCause::CalleeDeclined, false);
        let after = unsound("callee-declined").expect("the callee-declined row");
        // Other tests of this binary may only add to the row.
        assert!(after > before);
    }
}

/// Round 12 wave 7 (lane replay4): the door census reads the single-pass
/// finalizer's frameless-trap stamp.
#[cfg(test)]
mod r12w7_replay4_frameless_verdict_tests {
    use super::{door_rerun_verdict, DoorRerunCause};

    /// `putstatic` at bci 1, then a storing loop (the wave-4 fixture).
    const STORE_THEN_LOOP: [u8; 23] = [
        0x04, 0xb3, 0x00, 0x01, 0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0x05, 0xb3, 0x00,
        0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];

    #[test]
    fn a_body_whose_frameless_traps_replay_exactly_reruns_exactly() {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        let mut ran = cratonvm_jit::CompiledMethod::new(buf);
        let code = STORE_THEN_LOOP;
        assert_eq!(
            door_rerun_verdict(&code, &ran, "C", "m", "()V", None),
            (DoorRerunCause::Frameless, false),
            "unstamped: the whole-body rule"
        );
        ran.frameless_traps_replay_exact = true;
        assert_eq!(
            door_rerun_verdict(&code, &ran, "C", "m", "()V", None),
            (DoorRerunCause::Frameless, true)
        );
    }
}

/// Round 13 wave 3 (lane replay2; R13-2): the one "own stash, no resume,
/// replay unsound" predicate, with the doors' truth table
/// (`jit_bridge`'s `only_an_unsound_own_stash_of_a_class_never_redefined_is_refused`).
/// The kill switch row is the environment's, so it is not pinned here.
#[cfg(test)]
mod r13w3_replay2_rerun_answer_tests {
    use super::{unsound_own_stash_rerun_refused, DoorRerunCause};

    #[test]
    fn only_an_unsound_own_stash_of_a_class_never_redefined_is_refused() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES")
        {
            return;
        }
        let never = || false;
        assert!(unsound_own_stash_rerun_refused(DoorRerunCause::OwnStash, false, never));
        assert!(!unsound_own_stash_rerun_refused(DoorRerunCause::OwnStash, true, never));
        assert!(!unsound_own_stash_rerun_refused(DoorRerunCause::ForeignStash, false, never));
        assert!(!unsound_own_stash_rerun_refused(DoorRerunCause::Frameless, false, never));
        assert!(!unsound_own_stash_rerun_refused(DoorRerunCause::CalleeDeclined, false, never));
        assert!(!unsound_own_stash_rerun_refused(DoorRerunCause::OwnStash, false, || true));
        let asked = std::cell::Cell::new(false);
        assert!(!unsound_own_stash_rerun_refused(DoorRerunCause::OwnStash, true, || {
            asked.set(true);
            true
        }));
        assert!(!asked.get(), "the redefinition lock is not taken for a sound re-run");
    }
}

/// Round 13 wave 8 (lane replay4): every re-run the replay rule calls unsound
/// is refused, a refusal leaves the unsound column, and a bare sentinel is
/// judged by the trap site that raised it.
#[cfg(test)]
mod r13w8_replay4_rerun_refusal_tests {
    use super::{
        door_rerun_census, frameless_rerun_is_exact, frameless_rerun_is_exact_for_callee,
        note_door_rerun, note_door_rerun_refused, unsound_rerun_refused, DoorRerunCause,
    };
    use crate::jit::helpers::FramelessTrapSite;

    /// `putstatic` at bci 1, then a storing loop (the wave-4 fixture).
    const STORE_THEN_LOOP: [u8; 23] = [
        0x04, 0xb3, 0x00, 0x01, 0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0x05, 0xb3, 0x00,
        0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];

    fn switch_on(name: &str) -> bool {
        cratonvm_types::flags::runtime_flag_default_on(name)
    }

    #[test]
    fn every_unsound_cause_is_refused_and_a_redefined_class_is_not() {
        let never = || false;
        let causes = [
            (
                DoorRerunCause::OwnStash,
                "CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES",
            ),
            (
                DoorRerunCause::ForeignStash,
                "CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES",
            ),
            (
                DoorRerunCause::Frameless,
                "CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES",
            ),
            (
                DoorRerunCause::CalleeDeclined,
                "CRATONVM_JIT_CALLEE_UNSOUND_RERUN_RAISES",
            ),
            (
                DoorRerunCause::CalleeFrameless,
                "CRATONVM_JIT_CALLEE_UNSOUND_RERUN_RAISES",
            ),
            (
                DoorRerunCause::CalleeException,
                "CRATONVM_JIT_CALLEE_EXCEPTION_RERUN_RAISES",
            ),
        ];
        for (cause, switch) in causes {
            assert!(
                !unsound_rerun_refused(cause, true, never),
                "{cause:?}: a sound re-run"
            );
            assert!(
                !unsound_rerun_refused(cause, false, || true),
                "{cause:?}: a redefined class keeps its re-run"
            );
            // The switch row is the environment's.
            if switch_on(switch) {
                assert!(unsound_rerun_refused(cause, false, never), "{cause:?}");
            }
        }
        assert!(!unsound_rerun_refused(
            DoorRerunCause::Refused,
            false,
            never
        ));
        let asked = std::cell::Cell::new(false);
        assert!(!unsound_rerun_refused(
            DoorRerunCause::ForeignStash,
            true,
            || {
                asked.set(true);
                true
            }
        ));
        assert!(
            !asked.get(),
            "the redefinition lock is not taken for a sound re-run"
        );
    }

    #[test]
    fn a_refusal_moves_the_count_out_of_the_unsound_column() {
        let row = |name: &str| {
            door_rerun_census()
                .into_iter()
                .find(|(n, _, _)| *n == name)
                .map(|(_, s, u)| (s, u))
        };
        assert!(
            row("callee-exception").is_some(),
            "the callee-exception row"
        );
        let (refused_before, refused_unsound) = row("refused").unwrap_or((0, 0));
        assert_eq!(refused_unsound, 0, "a refusal is never a double commit");
        note_door_rerun(DoorRerunCause::CalleeException, false);
        note_door_rerun_refused(DoorRerunCause::CalleeException);
        let (refused_after, _) = row("refused").unwrap_or((0, 0));
        // Other tests of this binary may only add to the row.
        assert!(refused_after > refused_before);
    }

    #[test]
    fn a_bare_sentinel_is_judged_by_the_site_that_raised_it() {
        let Some(mut buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
            return;
        };
        buf.emit(&[0xC3]); // ret
                           // Ids no allocator hands out: the artifact's drop releases its id,
                           // which for a live one would unbind another test's body.
        const OWN_ID: u32 = 0xFFFF_FF07;
        const OTHER_ID: u32 = 0xFFFF_FF08;
        let mut ran = cratonvm_jit::CompiledMethod::new(buf);
        ran.compile_id = OWN_ID;
        ran.frameless_traps_replay_exact = true;
        let code = STORE_THEN_LOOP;
        let pure = [0x1a, 0x1b, 0x60, 0xac]; // iload_0 iload_1 iadd ireturn
        let own = FramelessTrapSite::Stub {
            compile_id: OWN_ID,
            bci: 4,
            exact: true,
            method: 0,
        };
        let other = FramelessTrapSite::Stub {
            compile_id: OTHER_ID,
            bci: 4,
            exact: true,
            method: 0,
        };
        if !switch_on("CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY") {
            return;
        }
        // This body's own stub: its stamp answers.
        assert!(frameless_rerun_is_exact(&code, Some(&ran), own));
        // Another body's stub, or a helper: only the whole-body rule.
        assert!(!frameless_rerun_is_exact(&code, Some(&ran), other));
        assert!(!frameless_rerun_is_exact(
            &code,
            Some(&ran),
            FramelessTrapSite::Helper
        ));
        assert!(frameless_rerun_is_exact(
            &pure,
            Some(&ran),
            FramelessTrapSite::Helper
        ));
        // Nothing recorded: the stamp, as before.
        assert!(frameless_rerun_is_exact(
            &code,
            Some(&ran),
            FramelessTrapSite::Unknown
        ));
        // A service that entered no body trusts the stub's own stamp.
        assert!(frameless_rerun_is_exact(&code, None, other));
        let unstamped = FramelessTrapSite::Stub {
            compile_id: OTHER_ID,
            bci: 4,
            exact: false,
            method: 0,
        };
        assert!(!frameless_rerun_is_exact(&code, None, unstamped));
        // A dropped callee frame's own verdict: for the service only.
        let dropped = FramelessTrapSite::Dropped { exact: true };
        assert!(frameless_rerun_is_exact(&code, None, dropped));
        assert!(!frameless_rerun_is_exact(&code, Some(&ran), dropped));
        assert!(!frameless_rerun_is_exact(
            &code,
            None,
            FramelessTrapSite::Dropped { exact: false }
        ));
    }

    /// Round 13 wave 13 (lane jitfix;
    /// `r13w12-replay6-callee-service-trusts-any-stubs-exact-stamp`): a
    /// call-site service re-running method Y does not take the `exact` stamp
    /// of a stub method X raised -- Y's storing body gets the whole-body rule
    /// -- while Y's own stub, and a stub whose method is unknown, still answer.
    #[test]
    fn a_service_trusts_only_its_own_methods_stub_stamp() {
        use crate::jit::helpers::frameless_trap_method_hash;
        if !switch_on("CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY")
            || !switch_on("CRATONVM_JIT_FRAMELESS_TRAP_METHOD_IDENTITY")
        {
            return;
        }
        let x = frameless_trap_method_hash("p/G", "g", "()V");
        let y = frameless_trap_method_hash("p/C", "c", "()V");
        assert_ne!(x, y);
        assert_ne!(x, 0, "0 is reserved for an unknown method");
        let stub = |method| FramelessTrapSite::Stub {
            compile_id: 0xFFFF_FF09,
            bci: 4,
            exact: true,
            method,
        };
        let code = STORE_THEN_LOOP;
        assert!(
            !frameless_rerun_is_exact_for_callee(&code, stub(x), y),
            "X's stamp says nothing about re-running Y, which stores before its loop"
        );
        assert!(frameless_rerun_is_exact_for_callee(&code, stub(y), y));
        assert!(
            frameless_rerun_is_exact_for_callee(&code, stub(0), y),
            "an unparsed label keeps the old trust"
        );
        let pure = [0x1a, 0x1b, 0x60, 0xac]; // iload_0 iload_1 iadd ireturn
        assert!(
            frameless_rerun_is_exact_for_callee(&pure, stub(x), y),
            "the whole-body rule still answers for a pure body"
        );
    }
}

/// Round 13 wave 5 (lane resume): an inlined chain whose scopes name
/// scalar-replaced objects (`r13w3-framestate-vm-chain-resume-gaps` item 1).
/// The producer numbers virtual objects chain-wide, so one object two scopes
/// name must become ONE heap object.
#[cfg(test)]
mod r13w5_resume_chain_virtual_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame, VirtualObjectState};

    fn cached(method_name: &str, is_synchronized: bool) -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: cratonvm_types::ClassId::new(0),
                class_name: Arc::from("T"),
                method_name: Arc::from(method_name),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: Arc::from(&[0xb1u8][..]), // return
                exception_table: Arc::from(Vec::new().into_boxed_slice()),
                max_stack: 8,
                max_locals: 4,
                num_params: 0,
                is_synchronized,
                is_static: true,
            },
        ))
    }

    fn vobj(id: usize, class_id: u32, fields: Vec<FrameValue>) -> FrameValue {
        FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id,
            class_id,
            num_fields: fields.len(),
            field_values: fields,
        })
    }

    fn scope(key: &str, locals: Vec<FrameValue>, stack: Vec<FrameValue>) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: key.to_string(),
            locals,
            stack,
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            ..Default::default()
        }
    }

    /// `T.outer` defines object 4 in local 0; the spliced `T.inner` names it
    /// by reference in its local 0 and holds an int on its stack.
    fn shared_object_chain(inner_ref: usize) -> ReconstructedFrame {
        let outer = scope(
            "T.outer:()V",
            vec![vobj(4, 5, vec![FrameValue::Int(9)]), FrameValue::Int(1)],
            Vec::new(),
        );
        let mut rf = scope(
            "T.inner:()V",
            vec![FrameValue::VirtualObjectRef(inner_ref)],
            vec![FrameValue::Int(3)],
        );
        rf.caller_frames = vec![outer];
        rf
    }

    /// The frames `materialise_inlined_chain` would have built over the
    /// placeholders of [`shared_object_chain`].
    fn placeholder_frames() -> Vec<InlinedChainFrame> {
        vec![
            InlinedChainFrame {
                cached: cached("outer", false),
                locals: vec![Value::Object(None), Value::Int(1)],
                stack: Vec::new(),
                resume_pc: 0,
                executing_pc: 0,
                monitors: Vec::new(),
            },
            InlinedChainFrame {
                cached: cached("inner", false),
                locals: vec![Value::Object(None)],
                stack: vec![Value::Int(3)],
                resume_pc: 0,
                executing_pc: 0,
                monitors: Vec::new(),
            },
        ]
    }

    #[test]
    fn one_object_named_by_two_scopes_becomes_one_heap_object() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let rf = shared_object_chain(4);
        assert!(reconstructed_chain_names_virtual(&rf));
        let virtuals = merge_chain_scopes(&rf);
        assert_eq!(virtuals.shapes, vec![(2, 0), (1, 1)], "outermost scope first");
        let chain = DeoptFrameChain {
            frames: placeholder_frames(),
            virtuals: Some(virtuals),
            outermost_cp_stamp: None,
            inner_own_source_frames: Vec::new(),
        };
        let pins = thread.native_pin_roots.len();
        let base = thread.frames.len();
        assert!(matches!(
            push_inlined_chain(&shared, &mut thread, chain, false),
            Some(CachedCallResult::FramePushed)
        ));
        assert_eq!(thread.native_pin_roots.len(), pins, "the pins end at the push");
        // The pushed frames alone keep the object alive across a collection.
        maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-resume");
        let (outer_obj, inner_obj) = match (
            thread.frames[base].get_local(0),
            thread.frames[base + 1].get_local(0),
        ) {
            (Value::Object(Some(o)), Value::Object(Some(i))) => (o, i),
            other => panic!("both scopes must hold the materialised object, got {other:?}"),
        };
        assert_eq!(outer_obj, inner_obj, "one object, not one per scope");
        assert_eq!(
            shared.mem.heap.class_id_of(outer_obj),
            cratonvm_types::ClassId::new(5)
        );
        assert_eq!(shared.mem.heap.get_field(outer_obj, 0), Value::Int(9));
        assert_eq!(thread.frames[base].get_local(1), Value::Int(1));
        assert_eq!(thread.frames[base + 1].stack.len(), 1);
    }

    /// A graph the materialiser refuses (a reference to an object no scope
    /// defines) pushes NOTHING and leaves no pin behind.
    #[test]
    fn an_unmaterialisable_chain_graph_pushes_nothing() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let rf = shared_object_chain(6);
        let mut chain = DeoptFrameChain {
            frames: placeholder_frames(),
            virtuals: Some(merge_chain_scopes(&rf)),
            outermost_cp_stamp: None,
            inner_own_source_frames: Vec::new(),
        };
        let pins = thread.native_pin_roots.len();
        let base = thread.frames.len();
        assert!(matches!(
            materialise_chain_virtuals(&shared, &mut thread, &mut chain),
            Err(DeoptFrameBail::VirtualMaterialise)
        ));
        assert_eq!(thread.native_pin_roots.len(), pins);
        assert!(push_inlined_chain(&shared, &mut thread, chain, false).is_none());
        assert_eq!(thread.frames.len(), base, "nothing pushed");
        assert_eq!(thread.native_pin_roots.len(), pins);
    }

    #[test]
    fn placeholders_blank_every_scope_and_only_virtual_slots() {
        let rf = shared_object_chain(4);
        let blank = with_virtual_placeholders(&rf);
        assert!(!reconstructed_chain_names_virtual(&blank));
        assert!(matches!(blank.locals[0], FrameValue::Object(0)));
        assert!(matches!(blank.stack[0], FrameValue::Int(3)));
        assert!(matches!(blank.caller_frames[0].locals[0], FrameValue::Object(0)));
        assert!(matches!(blank.caller_frames[0].locals[1], FrameValue::Int(1)));
    }

    /// `CRATONVM_DEOPT_VERIFY`'s reference check reaches every CALLER scope
    /// of a chain, not only the trapping one.
    #[test]
    fn the_chain_oop_check_reads_every_caller_scope() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared
            .mem
            .heap
            .alloc_object(cratonvm_types::ClassId::new(7), 0);
        // Cast: object pointer to the raw word a stash carries.
        let addr = obj.as_ptr() as usize as u64;
        let mut rf = scope("T.inner:()V", vec![FrameValue::Object(addr)], Vec::new());
        rf.caller_frames = vec![scope(
            "T.outer:()V",
            vec![FrameValue::Object(addr), FrameValue::Object(0)],
            Vec::new(),
        )];
        assert!(verify_chain_oops(&rf, &shared).is_ok());
        rf.caller_frames[0].locals[1] = FrameValue::Object(0x1234);
        assert!(verify_reconstructed_oops(&rf, &shared).is_ok(), "the old check missed it");
        let why = verify_chain_oops(&rf, &shared).expect_err("a wild caller word is caught");
        assert!(why.contains("caller scope 1"), "{why}");
    }

    #[test]
    fn a_monitor_or_a_synchronized_outer_method_refuses_the_chain_virtuals() {
        let rf = shared_object_chain(4);
        assert_eq!(chain_virtuals_refusal(&cached("outer", false), &rf), None);
        assert!(chain_virtuals_refusal(&cached("outer", true), &rf).is_some());
        let mut locked = rf.clone();
        locked.caller_frames[0].monitors.push(MonitorInfo {
            object: FrameValue::Object(0x1000),
            lock_depth: 1,
            relock: false,
        });
        assert!(chain_virtuals_refusal(&cached("outer", false), &locked).is_some());
    }
}

/// Round 13 wave 6 (lane chain2): an inlined chain's inner scope is resolved
/// by the class id its artifact recorded (`r13w5-resume-chain-scope-class-ids-patch`),
/// never by name when that id is known, ambiguous or stale.
#[cfg(test)]
mod r13w6_chain2_scope_hint_tests {
    use super::*;
    use crate::config::VmConfig;

    const KEY: &str = "p/Spliced.m:()I";

    /// No row: the name rule, as before the table existed.
    #[test]
    fn a_scope_without_a_row_keeps_the_name_rule() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let why = resolve_inlined_callee_hinted(&shared, ClassId::new(0), KEY, &[])
            .err()
            .expect("an unloaded class does not resolve");
        assert!(why.contains("not resolvable from the class that inlined it"), "{why}");
    }

    /// A row is authoritative: an id this VM does not hold refuses the scope
    /// instead of falling back to a lookup by name, which could find another
    /// class of the same name.
    #[test]
    fn a_recorded_class_id_is_used_instead_of_the_name() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let hints = vec![(KEY.to_string(), 987_654u32)];
        let why = resolve_inlined_callee_hinted(&shared, ClassId::new(0), KEY, &hints)
            .err()
            .expect("an id this VM never loaded does not resolve");
        assert!(why.contains("class id 987654 is not loaded"), "{why}");
    }

    /// Two classes of one name in one compile: refused, not guessed.
    #[test]
    fn an_ambiguous_key_is_refused() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let hints = vec![(KEY.to_string(), cratonvm_jit::deopt::SPLICE_SCOPE_CLASS_AMBIGUOUS)];
        let why = resolve_inlined_callee_hinted(&shared, ClassId::new(0), KEY, &hints)
            .err()
            .expect("an ambiguous scope is refused");
        assert!(why.contains("two different classes"), "{why}");
    }

    fn cached_named(method_name: &str) -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(0),
                class_name: Arc::from("T"),
                method_name: Arc::from(method_name),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                // invokestatic #1; return
                code: Arc::from(&[0xb8u8, 0x00, 0x01, 0xb1, 0x00, 0x00][..]),
                exception_table: Arc::from(Vec::new().into_boxed_slice()),
                max_stack: 8,
                max_locals: 4,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ))
    }

    /// `last_instr_pc` of each frame a three-scope chain pushes, the scopes
    /// reporting `executing` (outermost first).
    fn pushed_last_instr_pcs(executing: [usize; 3], switch: &str) -> Vec<usize> {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DEOPT_CHAIN_LAST_INSTR_PC", Some(switch))],
            || {
                let shared = Arc::new(SharedVm::new(VmConfig::default()));
                let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
                let frames: Vec<InlinedChainFrame> = ["outer", "mid", "inner"]
                    .iter()
                    .zip(executing.iter())
                    .map(|(name, &executing_pc)| InlinedChainFrame {
                        cached: cached_named(name),
                        locals: Vec::new(),
                        stack: Vec::new(),
                        resume_pc: 3,
                        executing_pc,
                        monitors: Vec::new(),
                    })
                    .collect();
                let base = thread.frames.len();
                let pushed =
                    push_inlined_chain(&shared, &mut thread, DeoptFrameChain::ready(frames), false);
                assert!(pushed.is_some());
                (0..3).map(|i| thread.frames[base + i].last_instr_pc).collect()
            },
        )
    }

    /// Every pushed frame reports its scope's bci as `last_instr_pc` -- a
    /// caller the invoke it is parked in, where the unwinder searches its
    /// handlers once the resumed callee throws -- not the 0 of a frame that
    /// never executed. `CRATONVM_DEOPT_CHAIN_LAST_INSTR_PC=0` restores the 0.
    #[test]
    fn a_pushed_caller_frame_reports_the_invoke_it_is_parked_in() {
        assert_eq!(pushed_last_instr_pcs([2, 1, 3], "1"), vec![2, 1, 3]);
        assert_eq!(pushed_last_instr_pcs([2, 1, 3], "0"), vec![0, 0, 0]);
    }

    fn int_method(
        name: &str,
        mut code: Vec<u8>,
        table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
    ) -> Arc<CachedBytecodeMethod> {
        code.extend_from_slice(&[0x00, 0x00]);
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(0),
                class_name: Arc::from("T"),
                method_name: Arc::from(name),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: Arc::from(code.as_slice()),
                exception_table: Arc::from(table.into_boxed_slice()),
                max_stack: 8,
                max_locals: 4,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ))
    }

    /// Run a two-frame chain -- `outer` parked after its `invokestatic` at
    /// bci 0 (resume pc 3), `inner` at its bci 0 -- to completion, with
    /// `CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST=switch`.
    fn run_chain(
        outer: Arc<CachedBytecodeMethod>,
        inner: Arc<CachedBytecodeMethod>,
        switch: &str,
    ) -> Option<crate::error::MethodCallResult> {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST", Some(switch))],
            || {
                let shared = Arc::new(SharedVm::new(VmConfig::default()));
                let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
                let frame = |cached: Arc<CachedBytecodeMethod>, resume_pc: usize| InlinedChainFrame {
                    cached,
                    locals: Vec::new(),
                    stack: Vec::new(),
                    resume_pc,
                    executing_pc: 0,
                    monitors: Vec::new(),
                };
                let chain = DeoptFrameChain::ready(vec![frame(outer, 3), frame(inner, 0)]);
                let r = run_deopt_frame_chain_to_completion(&shared, &mut thread, chain);
                assert_eq!(thread.frames.len(), 0, "every pushed frame is popped");
                r
            },
        )
    }

    /// The inner frame's return continues the OUTER frame (`+ 1`), whose
    /// return is the chain's value. Run from the innermost frame, the entry
    /// ended at the callee's return and handed back the callee's value.
    #[test]
    fn a_chain_returns_through_its_outer_frame() {
        // invokestatic #1; iconst_1; iadd; ireturn
        let outer = || int_method("outer", vec![0xb8, 0x00, 0x01, 0x04, 0x60, 0xac], Vec::new());
        // iconst_5; ireturn
        let inner = || int_method("inner", vec![0x08, 0xac], Vec::new());
        let on = run_chain(outer(), inner(), "1").expect("pushed");
        assert!(matches!(on, Ok(Some(Value::Int(6)))), "{on:?}");
        let off = run_chain(outer(), inner(), "0").expect("pushed");
        assert!(matches!(off, Ok(Some(Value::Int(5)))), "the pre-fix answer: {off:?}");
    }

    /// An exception the inner frame throws is offered to the outer frame's
    /// handler around the call (`R13CrashNullReceiverFields.bufGet`).
    #[test]
    fn an_exception_from_the_inner_frame_reaches_the_outer_handler() {
        let probe = Arc::new(SharedVm::new(VmConfig::default()));
        let mut probe_thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "probe");
        if crate::runtime::exceptions::create_exception_object(
            &probe,
            &mut probe_thread,
            "java/lang/NullPointerException",
            None,
        )
        .is_err()
        {
            return; // stripped VM: the throwable cannot be built
        }
        // 0: invokestatic #1  3: iconst_1  4: ireturn  5: pop  6: bipush 42  8: ireturn
        let outer = || {
            int_method(
                "outer",
                vec![0xb8, 0x00, 0x01, 0x04, 0xac, 0x57, 0x10, 0x2a, 0xac],
                vec![cratonvm_reader::attribute::ExceptionTableEntry {
                    start_pc: 0,
                    end_pc: 3,
                    handler_pc: 5,
                    catch_type: 0,
                }],
            )
        };
        // aconst_null; athrow
        let inner = || int_method("inner", vec![0x01, 0xbf], Vec::new());
        let on = run_chain(outer(), inner(), "1").expect("pushed");
        assert!(matches!(on, Ok(Some(Value::Int(42)))), "{on:?}");
        let off = run_chain(outer(), inner(), "0").expect("pushed");
        assert!(
            matches!(off, Err(MethodCallFailed::ExceptionThrown(_))),
            "the pre-fix answer: the outer handler was never searched: {off:?}"
        );
    }
}

/// Round 13 wave 8 (lane chain3): the planless OSR guard exit's chain arm,
/// the chain run-to-completion sink's refusals, and the outermost-scope
/// redefinition rule.
#[cfg(test)]
mod r13w8_chain3_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame, VirtualObjectState};

    fn cached(method_name: &str) -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(0),
                class_name: Arc::from("T"),
                method_name: Arc::from(method_name),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                // invokestatic #1; return
                code: Arc::from(&[0xb8u8, 0x00, 0x01, 0xb1, 0x00, 0x00][..]),
                exception_table: Arc::from(Vec::new().into_boxed_slice()),
                max_stack: 8,
                max_locals: 4,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ))
    }

    fn scope(key: &str, bci: u32, locals: Vec<FrameValue>) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: key.to_string(),
            bci,
            locals,
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            ..Default::default()
        }
    }

    /// `T.outer` parked in its `invokestatic` at bci 0, over `p/Q.inner`
    /// trapping at its bci 2.
    fn chain() -> ReconstructedFrame {
        let mut rf = scope("p/Q.inner:()V", 2, vec![FrameValue::Int(5)]);
        rf.caller_frames = vec![scope("T.outer:()V", 0, vec![FrameValue::Int(1)])];
        rf
    }

    fn artifact() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        cratonvm_jit::CompiledMethod::new(buf)
    }

    /// A live `T.outer` frame at pc 0, pushed; its index.
    fn live_frame(shared: &SharedVm, thread: &mut JvmThread) -> usize {
        let pin_base = thread.native_pin_roots.len();
        let frame = build_deopt_frame_inner(
            shared,
            thread,
            &cached("outer"),
            &scope("T.outer:()V", 0, Vec::new()),
            false,
        )
        .expect("an empty frame rebuilds");
        thread.native_pin_roots.truncate(pin_base);
        thread.frames.push(frame);
        thread.frames.len() - 1
    }

    /// A chain naming a monitor in ANY scope is refused before the live frame
    /// is written or anything is pushed: the OSR door's lock-record rewrite
    /// after a guard-exit transfer is not the chain's (the outermost scope's
    /// locks are part (c) of the patch page), and admission
    /// (`osr_exit::chain_resumable_in_place`) refused the same chain.
    #[test]
    fn a_chain_guard_exit_holding_a_monitor_writes_nothing() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let idx = live_frame(&shared, &mut thread);
        let before = (thread.frames.len(), thread.frames[idx].pc);
        let lock = MonitorInfo {
            object: FrameValue::Object(0x1000),
            lock_depth: 1,
            relock: false,
        };
        let cm = artifact();
        for in_outer in [false, true] {
            let mut rf = chain();
            if in_outer {
                rf.caller_frames[0].monitors.push(lock.clone());
            } else {
                rf.monitors.push(lock.clone());
            }
            let why =
                transfer_osr_guard_exit_chain_into_live_frame(&shared, &mut thread, idx, &rf, &cm)
                    .expect_err("a chain holding a monitor is not transferred");
            assert!(why.contains("monitor"), "{why}");
            assert_eq!((thread.frames.len(), thread.frames[idx].pc), before);
        }
    }

    /// Both provenance modes of the in-place chain transfer resolve every
    /// inner scope before anything is written: a callee that does not resolve
    /// (`p/Q`, not loaded) refuses with its own reason and leaves the live
    /// frame and the stack as they were.
    #[test]
    fn a_chain_transfer_that_cannot_resolve_its_callee_writes_nothing() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let idx = live_frame(&shared, &mut thread);
        let before = (thread.frames.len(), thread.frames[idx].pc);
        let cm = artifact();
        let planned = transfer_osr_exit_chain_into_live_frame(
            &shared,
            &mut thread,
            idx,
            &chain(),
            &cm,
            ChainExitProvenance::RecordedBci,
        )
        .expect_err("an unloaded callee does not resolve");
        let planless = transfer_osr_exit_chain_into_live_frame(
            &shared,
            &mut thread,
            idx,
            &chain(),
            &cm,
            ChainExitProvenance::PointAddress,
        )
        .expect_err("an unloaded callee does not resolve");
        for why in [&planned, &planless] {
            assert!(!why.contains("not a recorded deopt point"), "{why}");
            assert!(why.contains("p/Q"), "{why}");
        }
        assert_eq!((thread.frames.len(), thread.frames[idx].pc), before);
    }

    /// A chain whose scalar-replaced object graph cannot be materialised is
    /// refused by the run-to-completion sink BEFORE anything is pushed, and
    /// leaves no pin: its callers answer `None` with an error and no replay.
    #[test]
    fn an_unmaterialisable_chain_is_refused_before_the_run() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut rf = scope("T.inner:()V", 0, vec![FrameValue::VirtualObjectRef(6)]);
        rf.caller_frames = vec![scope(
            "T.outer:()V",
            0,
            vec![FrameValue::VirtualObject(VirtualObjectState {
                array_element_type: None,
                id: 4,
                class_id: 5,
                num_fields: 1,
                field_values: vec![FrameValue::Int(9)],
            })],
        )];
        let frame = |name: &str| InlinedChainFrame {
            cached: cached(name),
            locals: vec![Value::Object(None)],
            stack: Vec::new(),
            resume_pc: 3,
            executing_pc: 0,
            monitors: Vec::new(),
        };
        let chain = DeoptFrameChain {
            frames: vec![frame("outer"), frame("inner")],
            virtuals: Some(merge_chain_scopes(&rf)),
            outermost_cp_stamp: None,
            inner_own_source_frames: Vec::new(),
        };
        let (pins, base) = (thread.native_pin_roots.len(), thread.frames.len());
        assert!(run_deopt_frame_chain_to_completion(&shared, &mut thread, chain).is_none());
        assert_eq!(thread.frames.len(), base, "nothing pushed");
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin left behind");
        // Round 13 wave 11 (lane chain5): the production twin re-allocates
        // before anything is pushed too (a non-heap refusal is `None`, a heap
        // one `OutOfMemoryError`), and leaves no pin of its own.
        let chain = DeoptFrameChain {
            frames: vec![frame("outer"), frame("inner")],
            virtuals: Some(merge_chain_scopes(&rf)),
            outermost_cp_stamp: None,
            inner_own_source_frames: Vec::new(),
        };
        assert!(
            run_deopt_frame_chain_to_completion_releasing_pins(&shared, &mut thread, chain, pins)
                .is_none()
        );
        assert_eq!(thread.frames.len(), base, "nothing pushed");
        assert_eq!(thread.native_pin_roots.len(), pins, "no pin left behind");
    }

    /// The outermost-scope redefinition rule answers only for a chain, and
    /// not at all while no class of the process was ever redefined.
    #[test]
    fn the_outermost_redefinition_rule_is_a_chain_rule() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let cm = artifact();
        let flat = scope("T.outer:()V", 0, Vec::new());
        assert!(!chain_outer_scope_redefined_since_compile(
            &shared,
            &cm,
            ClassId::new(0),
            &flat
        ));
        if !crate::classloading::any_class_redefined() {
            assert!(!chain_outer_scope_redefined_since_compile(
                &shared,
                &cm,
                ClassId::new(0),
                &chain()
            ));
        }
    }
}

/// Round 13 wave 12 (lane replay6): the VM sinks judge a re-run with the
/// exception edges the body that ran can take
/// (`r13w12-osrdoor-vm-sinks-replay-extent-ignores-handler-edges`).
#[cfg(test)]
mod r13w12_replay6_sink_handler_edge_tests {
    use super::{
        door_rerun_verdict, replay_from_entry_is_observably_equivalent,
        replay_from_entry_is_observably_equivalent_after,
        replay_from_entry_is_observably_equivalent_for_stash, DoorRerunCause,
    };
    use cratonvm_jit::deopt::{
        DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ReconstructedFrame,
        ResumeSemantics,
    };
    use cratonvm_jit::JitLocalHandlerSite;

    /// A handler (pc 3) placed BEFORE the range it protects (the call at 15),
    /// falling back into the guard at 10 with a forward `goto`: the exc4
    /// shape, not javac output.
    ///
    /// ```text
    ///  0: goto 10          3: astore_1        4: goto 10       7-9: nop
    /// 10: iconst_1        11: nop            12: putstatic #1
    /// 15: invokestatic #2 18: return
    /// ```
    const HANDLER_BEFORE_RANGE: [u8; 19] = [
        0xa7, 0x00, 0x0a, 0x4c, 0xa7, 0x00, 0x06, 0x00, 0x00, 0x00, 0x04, 0x00, 0xb3, 0x00, 0x01,
        0xb8, 0x00, 0x02, 0xb1,
    ];

    /// A body with a point at the guard (bci 10), and, when `catches`, the
    /// local-handler site the single-pass emitter would give the call at 15.
    fn body(catches: bool) -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.deopt_points.push(DeoptimizationPoint {
            native_offset: 0,
            bci: 10,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            semantics: ResumeSemantics::REEXECUTE,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: "C.m:()V".to_string(),
                bci: 10,
                locals: Vec::new(),
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
        });
        if catches {
            cm._jit_local_handler_sites.push(Box::new(JitLocalHandlerSite {
                candidates: vec![("", 3)],
                declaring_class_id: 0,
                throw_bci: 15,
                cache: std::sync::atomic::AtomicU64::new(JitLocalHandlerSite::CACHE_EMPTY),
                implicit_cache: JitLocalHandlerSite::empty_implicit_cache(),
            }));
        }
        cm
    }

    fn frame_at_the_guard() -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: "C.m:()V".to_string(),
            bci: 10,
            semantics: ResumeSemantics::REEXECUTE,
            ..Default::default()
        }
    }

    /// Pass 1 stores at 12, the call throws at 15, the local handler comes
    /// back to the guard, and the guard traps: a re-run would store again.
    #[test]
    fn a_body_whose_handler_comes_back_to_the_trap_refuses_the_rerun() {
        let code = HANDLER_BEFORE_RANGE;
        let ran = body(true);
        let own = ran.deopt_points.as_ptr() as usize;
        let rframe = frame_at_the_guard();
        // The table-less form (no body in hand) sees a pure prefix.
        assert!(replay_from_entry_is_observably_equivalent(&code, true, 10));
        assert!(!replay_from_entry_is_observably_equivalent_after(&code, Some(&ran), 10));
        assert!(!replay_from_entry_is_observably_equivalent_for_stash(
            &code, &ran, &rframe, own
        ));
        // A plain restash (no point address) is judged with the same edges.
        assert!(!replay_from_entry_is_observably_equivalent_for_stash(
            &code, &ran, &rframe, 0
        ));
        assert_eq!(
            door_rerun_verdict(&code, &ran, "C", "m", "()V", Some((&rframe, own))),
            (DoorRerunCause::OwnStash, false)
        );
    }

    /// A body without local handlers (every optimizing-tier body) never took
    /// the edge: the answer is the one before this wave.
    #[test]
    fn a_body_without_local_handlers_keeps_the_branch_only_extent() {
        let code = HANDLER_BEFORE_RANGE;
        let ran = body(false);
        let own = ran.deopt_points.as_ptr() as usize;
        let rframe = frame_at_the_guard();
        assert!(replay_from_entry_is_observably_equivalent_after(&code, Some(&ran), 10));
        assert!(replay_from_entry_is_observably_equivalent_for_stash(
            &code, &ran, &rframe, own
        ));
        assert_eq!(
            door_rerun_verdict(&code, &ran, "C", "m", "()V", Some((&rframe, own))),
            (DoorRerunCause::OwnStash, true)
        );
        // No body at all: the spliced half is unknown, so only a pure body
        // re-runs exactly.
        assert!(!replay_from_entry_is_observably_equivalent_after(&code, None, 10));
    }

    /// `CRATONVM_JIT_REPLAY_EXTENT_HANDLER_EDGES=0` ignores the edges at the
    /// sinks as at compile time.
    #[test]
    fn the_kill_switch_restores_the_old_sink_answer() {
        let code = HANDLER_BEFORE_RANGE;
        let ran = body(true);
        let own = ran.deopt_points.as_ptr() as usize;
        let rframe = frame_at_the_guard();
        let edits = [("CRATONVM_JIT_REPLAY_EXTENT_HANDLER_EDGES", Some("0"))];
        let off = cratonvm_types::flags::with_thread_overrides(&edits, || {
            replay_from_entry_is_observably_equivalent_for_stash(&code, &ran, &rframe, own)
        });
        assert!(off);
    }
}

#[cfg(test)]
mod r14w2_deopt_callsite_own_source_tests {
    //! Round 14 wave 2 (lane deopt; proposal R13RP6-1): a trap out of a
    //! SUPERSEDED body of a redefined class, stashed by the x64 framed
    //! trampoline, carries the bytecode that body was compiled from, and the
    //! call-site service resumes it there (restamped), not in the new body.
    use super::*;
    use crate::config::VmConfig;
    use crate::types::Value;
    use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};
    use cratonvm_jit::deopt::{
        DeoptAction, DeoptEpochGuard, DeoptReason, DeoptimizationPoint, FrameState,
        ResumeSemantics, SavedRegisters,
    };

    /// `obsolete/VmProbe` with `static value()I` = `ldc #8; ireturn`, `#8`
    /// the Integer `constant` (the fixture of `obsolete_frames`' tests).
    fn ldc_class(constant: i32) -> Vec<u8> {
        probe_class("obsolete/VmProbe", constant)
    }

    /// [`ldc_class`] named `name`.
    fn probe_class(name: &str, constant: i32) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let code: &[u8] = &[0x12, 8, 0xac];
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&9u16.to_be_bytes());
        utf8(&mut b, name); // #1
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

    /// `value()`'s current body as a publish site's template.
    fn value_template(shared: &SharedVm, cid: ClassId) -> Option<Arc<CachedBytecodeMethod>> {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        let method = class.methods.iter().find(|m| &*m.name == "value")?;
        let code = method.code()?;
        Some(Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: cid,
                class_name: Arc::clone(&class.name),
                method_name: Arc::from("value"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(&code.code[..]),
                exception_table: Arc::from(Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new()),
                max_stack: code.max_stack,
                max_locals: code.max_locals,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        )))
    }

    /// A re-execute guard at `value()`'s first instruction.
    fn guard_at_entry() -> DeoptimizationPoint {
        DeoptimizationPoint {
            native_offset: 0,
            bci: 0,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: "obsolete/VmProbe.value:()I".to_string(),
                bci: 0,
                locals: Vec::new(),
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        }
    }

    #[test]
    fn a_superseded_body_resumes_in_its_own_bytecode_at_the_call_site() {
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(4_142), "r14-own");
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
        let Some(old) = value_template(&shared, cid) else {
            return;
        };
        let id = cratonvm_jit::reserve_compile_id();
        if id == 0 {
            // Every id live or awaiting grace: nothing to bind.
            return;
        }
        let guard: &'static DeoptEpochGuard =
            Box::leak(Box::new(DeoptEpochGuard::for_compile(id)));
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("a code buffer");
        buf.emit(&[0xC3]); // ret
        let mut body = Box::new(cratonvm_jit::CompiledMethod::new(buf));
        // Dropping `body` releases the id and its binding.
        body.compile_id = id;
        body.deopt_epoch_guard = guard as *const DeoptEpochGuard;
        let Some(stamp) = body.compile_cp_stamp() else {
            return;
        };
        stamp_compiled_source_of(&body, Arc::clone(&old));
        cratonvm_jit::bind_compile_id(id, &*body as *const cratonvm_jit::CompiledMethod as usize);

        let point = guard_at_entry();
        let regs = SavedRegisters::default();
        let _ = cratonvm_jit::deopt::take_last_deopt();
        // Before any redefinition there is nothing to translate from.
        assert_eq!(
            cratonvm_jit::deopt::x64_deopt_entry(&point, 0, &regs, guard),
            i64::MIN
        );
        assert!(callsite_trap_own_source(&shared, cid, "value", "()I").is_none());
        let _ = cratonvm_jit::deopt::take_last_deopt();

        shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        assert_eq!(
            cratonvm_jit::deopt::x64_deopt_entry(&point, 0, &regs, guard),
            i64::MIN
        );
        let (src, got_stamp) = callsite_trap_own_source(&shared, cid, "value", "()I")
            .expect("the superseded body's own bytecode");
        assert!(Arc::ptr_eq(&src, &old), "the body's own template");
        assert_eq!(got_stamp, stamp);
        assert!(
            callsite_trap_own_source(&shared, cid, "other", "()I").is_none(),
            "another method of the class is not this body"
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DEOPT_CALLSITE_OWN_SOURCE", Some("0"))],
            || callsite_trap_own_source(&shared, cid, "value", "()I"),
        );
        assert!(off.is_none(), "the kill switch keeps the refusal");

        // The resume runs the OLD constant (the new body returns 80000).
        let (rframe, _, _) =
            cratonvm_jit::deopt::take_last_deopt_with_point().expect("still stashed");
        let pin_base = thread.native_pin_roots.len();
        let Ok(frame) =
            build_deopt_frame_or_refusal(&shared, &mut thread, &src, &rframe, false, true)
        else {
            panic!("the frame builds from the body's own template");
        };
        let res =
            execute_own_source_frame_releasing_pins(&shared, &mut thread, frame, pin_base, got_stamp);
        assert!(
            matches!(res, Ok(Some(Value::Int(70_000)))),
            "the superseded activation keeps its own constant, got {res:?}"
        );
        drop(body);
    }

    fn define(shared: &SharedVm, name: &str, constant: i32) -> Option<ClassId> {
        shared
            .classes
            .class_manager
            .write()
            .define_class_with_options(
                name,
                &probe_class(name, constant),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            )
            .ok()
    }

    fn scope(key: &str, semantics: ResumeSemantics) -> cratonvm_jit::deopt::ReconstructedFrame {
        cratonvm_jit::deopt::ReconstructedFrame {
            method_key: key.to_string(),
            bci: 0,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics,
            caller_frames: Vec::new(),
        }
    }

    /// The outermost-scope half of the per-scope own-source resume: a chain
    /// whose ONLY redefined scope is the compiled method's own gets that
    /// frame's template from the body; a single frame, an unredefined class,
    /// a redefined inner scope and the kill switch get none.
    #[test]
    fn a_chain_whose_outer_class_alone_was_redefined_takes_the_body_template() {
        const OUTER: &str = "obsolete/VmProbe.value:()I";
        const INNER: &str = "obsolete/InnerProbe.value:()I";
        let shared = SharedVm::new(VmConfig::default());
        let (Some(outer), Some(inner)) = (
            define(&shared, "obsolete/VmProbe", 70_000),
            define(&shared, "obsolete/InnerProbe", 5),
        ) else {
            return;
        };
        if resolve_inlined_callee(&shared, outer, INNER).is_err() {
            // The stripped test VM does not resolve across the two classes.
            return;
        }
        let Some(old) = value_template(&shared, outer) else {
            return;
        };
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("a code buffer");
        buf.emit(&[0xC3]); // ret
        let mut body = cratonvm_jit::CompiledMethod::new(buf);
        body.install_epoch = cratonvm_jit::jit_install_epoch();
        if body.compile_cp_stamp().is_none() {
            return;
        }
        stamp_compiled_source_of(&body, Arc::clone(&old));
        let mut chain = scope(INNER, ResumeSemantics::REEXECUTE);
        chain.caller_frames = vec![scope(OUTER, ResumeSemantics::for_caller_scope())];
        let single = scope(OUTER, ResumeSemantics::REEXECUTE);
        assert!(
            chain_outermost_own_source(&shared, &body, &old, &chain).is_none(),
            "nothing redefined"
        );

        shared
            .classes
            .class_manager
            .write()
            .redefine_class(outer, ldc_class(80_000), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        let _ = shared
            .jit
            .jit_cache
            .invalidate_for_redefinition(outer, "obsolete/VmProbe");
        let Some(new) = value_template(&shared, outer) else {
            return;
        };
        let got = chain_outermost_own_source(&shared, &body, &new, &chain);
        assert!(
            got.is_some_and(|src| Arc::ptr_eq(&src, &old)),
            "the outermost frame is rebuilt from the body's own template"
        );
        assert!(
            chain_outermost_own_source(&shared, &body, &new, &single).is_none(),
            "a single frame is the doors' own-source path, not this one"
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE", Some("0"))],
            || chain_outermost_own_source(&shared, &body, &new, &chain),
        );
        assert!(off.is_none(), "the kill switch keeps the refusal");

        shared
            .classes
            .class_manager
            .write()
            .redefine_class(
                inner,
                probe_class("obsolete/InnerProbe", 6),
                RedefineOptions::default(),
            )
            .expect("a same-shape redefinition succeeds");
        let _ = shared
            .jit
            .jit_cache
            .invalidate_for_redefinition(inner, "obsolete/InnerProbe");
        assert!(
            chain_outermost_own_source(&shared, &body, &new, &chain).is_none(),
            "a spliced callee's class redefined too: still refused"
        );
    }

    /// Round 14 wave 3 (lane chain, R14DP-5): the inner-scope half. A chain
    /// whose SPLICED callee's class was redefined since the compile takes that
    /// scope's template from the bytecode the compile spliced
    /// (`CompiledMethod::splice_scope_sources`), and the materialised frame
    /// runs it; no row, or the kill switch, keeps the refusal.
    #[test]
    fn a_chain_whose_inner_class_was_redefined_takes_the_spliced_body() {
        const OUTER: &str = "obsolete/VmProbe.value:()I";
        const INNER: &str = "obsolete/InnerProbe.value:()I";
        let shared = SharedVm::new(VmConfig::default());
        let (Some(outer), Some(inner)) = (
            define(&shared, "obsolete/VmProbe", 70_000),
            define(&shared, "obsolete/InnerProbe", 5),
        ) else {
            return;
        };
        if resolve_inlined_callee(&shared, outer, INNER).is_err() {
            // The stripped test VM does not resolve across the two classes.
            return;
        }
        let spliced: Arc<[u8]> = crate::runtime::frame::padded_bytecode(&[0x12, 8, 0xac]);
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("a code buffer");
        buf.emit(&[0xC3]); // ret
        let mut body = cratonvm_jit::CompiledMethod::new(buf);
        body.install_epoch = cratonvm_jit::jit_install_epoch();
        if body.compile_cp_stamp().is_none() {
            return;
        }
        body.splice_scope_sources = vec![cratonvm_jit::SpliceScopeSource {
            method_key: INNER.to_string(),
            class_id: inner.as_u32(),
            code: Arc::clone(&spliced),
            max_locals: 0,
            is_static: true,
        }];
        let mut chain = scope(INNER, ResumeSemantics::REEXECUTE);
        chain.caller_frames = vec![scope(OUTER, ResumeSemantics::for_caller_scope())];
        assert!(
            chain_inner_scope_own_sources(&shared, &body, outer, &chain).is_none(),
            "nothing redefined"
        );

        shared
            .classes
            .class_manager
            .write()
            .redefine_class(
                inner,
                probe_class("obsolete/InnerProbe", 6),
                RedefineOptions::default(),
            )
            .expect("a same-shape redefinition succeeds");
        let _ = shared
            .jit
            .jit_cache
            .invalidate_for_redefinition(inner, "obsolete/InnerProbe");
        assert!(chain_inner_scope_redefined_since_compile(&shared, &body, outer, &chain));
        let rows = chain_inner_scope_own_sources(&shared, &body, outer, &chain)
            .expect("the redefined callee's spliced body");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, INNER);
        assert!(Arc::ptr_eq(&rows[0].1.code, &spliced), "the bytecode the compile spliced");
        assert_eq!(rows[0].1.declaring_class_id, inner);
        assert!(rows[0].1.exception_table.is_empty());
        let frames = materialise_inner_scopes(&shared, outer, &chain, &[], &rows)
            .expect("the inner scope materialises from its template");
        assert_eq!(frames.len(), 1);
        assert!(Arc::ptr_eq(&frames[0].cached.code, &spliced));
        // Without the template the same scope is resolved now, in the new code.
        let current = materialise_inner_scopes(&shared, outer, &chain, &[], &[])
            .expect("the current body still resolves");
        assert!(!Arc::ptr_eq(&current[0].cached.code, &spliced));

        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DEOPT_CHAIN_INNER_OWN_SOURCE", Some("0"))],
            || chain_inner_scope_own_sources(&shared, &body, outer, &chain),
        );
        assert!(off.is_none(), "the kill switch keeps the refusal");
        body.splice_scope_sources.clear();
        assert!(
            chain_inner_scope_own_sources(&shared, &body, outer, &chain).is_none(),
            "no retained body: still refused"
        );
    }

    /// Round 14 wave 4 (lane resume2, CH3W-3; patch
    /// `r14w3-chain-tierup-sink-inner-own-source-patch`): a chain built with an
    /// own-source template records that frame, and the push restamps it with
    /// the chain's stamp -- the sinks that run the chain inside the push (the
    /// tier-up sink, the call-site service) cannot restamp positionally after
    /// it as the doors do. The frame then runs the spliced bytecode against
    /// the pool it was compiled at (the OLD constant 5); unrestamped it read
    /// the redefined class's pool (6).
    #[test]
    fn a_pushed_chain_restamps_its_own_source_inner_frames() {
        const INNER: &str = "obsolete/InnerProbe.value:()I";
        let shared = SharedVm::new(VmConfig::default());
        let (Some(outer), Some(inner)) = (
            define(&shared, "obsolete/VmProbe", 70_000),
            define(&shared, "obsolete/InnerProbe", 5),
        ) else {
            return;
        };
        if resolve_inlined_callee(&shared, outer, INNER).is_err() {
            // The stripped test VM does not resolve across the two classes.
            return;
        }
        // The compiled method: `invokestatic #1; ireturn`, parked after the
        // invoke, which hands back whatever the inner frame returns.
        let outermost = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: outer,
                class_name: Arc::from("obsolete/VmProbe"),
                method_name: Arc::from("value"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(&[0xb8, 0x00, 0x01, 0xac]),
                exception_table: Arc::from(
                    Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new(),
                ),
                max_stack: 2,
                max_locals: 0,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ));
        let spliced: Arc<[u8]> = crate::runtime::frame::padded_bytecode(&[0x12, 8, 0xac]);
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("a code buffer");
        buf.emit(&[0xC3]); // ret
        let mut body = cratonvm_jit::CompiledMethod::new(buf);
        body.install_epoch = cratonvm_jit::jit_install_epoch();
        let Some(stamp) = body.compile_cp_stamp() else {
            return;
        };
        body.splice_scope_sources = vec![cratonvm_jit::SpliceScopeSource {
            method_key: INNER.to_string(),
            class_id: inner.as_u32(),
            code: Arc::clone(&spliced),
            max_locals: 0,
            is_static: true,
        }];
        let mut chain = scope(INNER, ResumeSemantics::REEXECUTE);
        chain.caller_frames = vec![scope(
            "obsolete/VmProbe.value:()I",
            ResumeSemantics::for_caller_scope(),
        )];
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(
                inner,
                probe_class("obsolete/InnerProbe", 6),
                RedefineOptions::default(),
            )
            .expect("a same-shape redefinition succeeds");
        let _ = shared
            .jit
            .jit_cache
            .invalidate_for_redefinition(inner, "obsolete/InnerProbe");
        let rows = chain_inner_scope_own_sources(&shared, &body, outer, &chain)
            .expect("the redefined callee's spliced body");

        let built = build_deopt_frame_chain_sourced(&shared, &outermost, &chain, &[], &rows)
            .expect("the chain builds from the template");
        assert_eq!(built.inner_own_source_frames, vec![1], "frame 1 is the template's");
        let plain = build_deopt_frame_chain_sourced(&shared, &outermost, &chain, &[], &[])
            .expect("the chain builds from the current body");
        assert!(plain.inner_own_source_frames.is_empty());
        drop(plain);

        let run = |mut c: DeoptFrameChain, restamp_inner: bool| {
            if !restamp_inner {
                c.inner_own_source_frames.clear();
            }
            let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(4_143), "r14w4");
            let r = run_deopt_frame_chain_to_completion(
                &shared,
                &mut thread,
                c.with_outermost_cp_stamp(Some(stamp)),
            );
            assert_eq!(thread.frames.len(), 0, "every pushed frame is popped");
            r
        };
        let restamped = run(built, true).expect("pushed");
        assert!(
            matches!(restamped, Ok(Some(Value::Int(5)))),
            "the spliced body reads the pool it was compiled at, got {restamped:?}"
        );
        let again = build_deopt_frame_chain_sourced(&shared, &outermost, &chain, &[], &rows)
            .expect("the chain builds from the template");
        let unstamped = run(again, false).expect("pushed");
        assert!(
            matches!(unstamped, Ok(Some(Value::Int(6)))),
            "the pre-fix answer reads the redefined pool, got {unstamped:?}"
        );
    }
}

#[cfg(test)]
mod r14w3_resume_tests {
    //! Round 14 wave 3 (lane resume): CH3-4 (an OSR chain guard exit is
    //! charged at its call site) and R14DP-1 (the call-site service takes a
    //! never-redefined trapping body's own template).
    use super::*;
    use crate::config::VmConfig;
    use cratonvm_jit::deopt::{
        speculation_id, DeoptAction, DeoptCause, DeoptEpochGuard, DeoptReason,
        DeoptimizationPoint, FrameState, FrameValue, ReconstructedFrame, ResumeSemantics,
        SavedRegisters, SPECULATION_ID_ANY,
    };

    const OUTER: &str = "r14w3/Loop.run:()I";

    fn scope(key: &str, bci: u32, semantics: ResumeSemantics) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: key.to_string(),
            bci,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics,
            caller_frames: Vec::new(),
        }
    }

    fn cause(bci: u32, reason: DeoptReason) -> Option<DeoptCause> {
        Some(DeoptCause {
            reason,
            speculation_id: speculation_id(bci, reason),
        })
    }

    /// CH3-4: a spliced `BoundsCheck` left through the OSR door's chain
    /// transfer is charged against the OSR'd method at the outermost scope's
    /// bci (17), never at the callee's (3), and the fourth charge withdraws
    /// the call site with the wildcard, as the doors' chain arm does. No
    /// cause, another reason, a single frame and the kill switch charge
    /// nothing.
    #[test]
    fn an_osr_chain_exit_is_charged_at_its_call_site() {
        let shared = SharedVm::new(VmConfig::default());
        let mut chain = scope("r14w3/Callee.get:(I)I", 3, ResumeSemantics::REEXECUTE);
        chain.caller_frames = vec![scope(OUTER, 17, ResumeSemantics::for_caller_scope())];
        let single = scope(OUTER, 17, ResumeSemantics::REEXECUTE);
        let charge = |frame: &ReconstructedFrame, c: Option<DeoptCause>| {
            charge_osr_chain_guard_exit(
                &shared,
                ClassId::new(0),
                "r14w3/Loop",
                "run",
                "()I",
                frame,
                c,
            )
        };
        let before = shared.jit.deopt_log.lock().total_deopts();
        assert!(!charge(&chain, None), "no cause names nothing");
        assert!(!charge(&chain, cause(3, DeoptReason::NullCheck)));
        assert!(!charge(&single, cause(17, DeoptReason::BoundsCheck)));
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_OSR_CHAIN_GUARD_EXIT_CHARGE", Some("0"))],
            || charge(&chain, cause(3, DeoptReason::BoundsCheck)),
        );
        assert!(!off, "the kill switch charges nothing");
        assert_eq!(shared.jit.deopt_log.lock().total_deopts(), before);
        for n in 1..=4 {
            assert!(charge(&chain, cause(3, DeoptReason::BoundsCheck)), "n={n}");
            assert_eq!(
                shared
                    .jit
                    .despec_registry
                    .contains_speculation(OUTER, 17, SPECULATION_ID_ANY),
                n >= 4,
                "the call site is withdrawn after four charges (n={n})"
            );
        }
        assert!(
            !shared.jit.despec_registry.contains(OUTER, 3),
            "never at the callee's bci"
        );
    }

    /// Round 14 wave 4 (lane resume2, RS-3): the one copy of the per-bci
    /// de-spec rule counts at the grain it is asked -- a named reason's own
    /// charges, or every reason at the bci -- withdraws at four, and ignores
    /// the superseded-guard sentinel.
    #[test]
    fn the_per_bci_despec_rule_counts_at_the_grain_it_is_asked() {
        const KEY: &str = "r14w4/Grain.m:()V";
        let shared = SharedVm::new(VmConfig::default());
        let charge = |reason: DeoptReason| {
            let _ = crate::jit::helpers::DeoptimizationController::deoptimize_in(
                &shared,
                ClassId::new(0),
                "r14w4/Grain",
                "m",
                "()V",
                reason,
                9,
            );
        };
        for _ in 0..3 {
            charge(DeoptReason::ReceiverTypeChanged);
        }
        charge(DeoptReason::BoundsCheck);
        let bounds = speculation_id(9, DeoptReason::BoundsCheck);
        assert_eq!(
            despec_site_after_trap_limit(&shared, KEY, 9, Some(DeoptReason::BoundsCheck), bounds),
            None,
            "one bounds-check charge withdraws nothing"
        );
        assert!(!shared.jit.despec_registry.contains(KEY, 9));
        assert_eq!(
            despec_site_after_trap_limit(
                &shared,
                KEY,
                u32::MAX,
                None,
                SPECULATION_ID_ANY
            ),
            None,
            "the sentinel is never withdrawn"
        );
        let whole = shared.jit.deopt_log.lock().deopt_count_at_bci(KEY, 9);
        let got = despec_site_after_trap_limit(&shared, KEY, 9, None, SPECULATION_ID_ANY);
        assert_eq!(got, (whole >= PER_BCI_DESPEC_LIMIT).then_some(whole));
        assert_eq!(
            shared
                .jit
                .despec_registry
                .contains_speculation(KEY, 9, SPECULATION_ID_ANY),
            whole >= PER_BCI_DESPEC_LIMIT,
            "the wildcard counts every reason at the bci ({whole} charged)"
        );
    }

    /// R14DP-1: the trampoline named the body; its class was never
    /// redefined, so the service takes that template for the stash key's
    /// method, and nothing for another method, another class, or with the
    /// kill switch off.
    #[test]
    fn the_call_site_service_takes_a_never_redefined_body_s_own_template() {
        let shared = SharedVm::new(VmConfig::default());
        let id = cratonvm_jit::reserve_compile_id();
        if id == 0 {
            // Every id live or awaiting grace: nothing to bind.
            return;
        }
        let source = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(9),
                class_name: Arc::from("r14w3/Own"),
                method_name: Arc::from("m"),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                // `return`, padded the way the VM pads bytecode.
                code: Arc::from(&[0xb1u8, 0, 0][..]),
                exception_table: Arc::from(
                    Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new(),
                ),
                max_stack: 0,
                max_locals: 1,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ));
        let guard: &'static DeoptEpochGuard =
            Box::leak(Box::new(DeoptEpochGuard::for_compile(id)));
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("a code buffer");
        buf.emit(&[0xC3]); // ret
        let mut body = Box::new(cratonvm_jit::CompiledMethod::new(buf));
        // Dropping `body` releases the id and its binding.
        body.compile_id = id;
        body.deopt_epoch_guard = guard as *const DeoptEpochGuard;
        stamp_compiled_source_of(&body, Arc::clone(&source));
        cratonvm_jit::bind_compile_id(id, &*body as *const cratonvm_jit::CompiledMethod as usize);
        let point = DeoptimizationPoint {
            native_offset: 0,
            bci: 0,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: "r14w3/Own.m:()V".to_string(),
                bci: 0,
                locals: vec![FrameValue::Int(1)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        let regs = SavedRegisters::default();
        let _ = cratonvm_jit::deopt::take_last_deopt();
        assert!(
            callsite_trap_current_template(&shared, "r14w3/Own", "m", "()V").is_none(),
            "nothing stashed"
        );
        assert_eq!(
            cratonvm_jit::deopt::x64_deopt_entry(&point, 0, &regs, guard),
            i64::MIN
        );
        if cratonvm_jit::deopt::peek_last_deopt_trap_source().is_none() {
            // This build's trampoline names no body: nothing to take.
            let _ = cratonvm_jit::deopt::take_last_deopt();
            drop(body);
            return;
        }
        let got = callsite_trap_current_template(&shared, "r14w3/Own", "m", "()V");
        assert!(
            got.is_some_and(|t| Arc::ptr_eq(&t, &source)),
            "the trapping body's own template"
        );
        assert!(callsite_trap_current_template(&shared, "r14w3/Other", "m", "()V").is_none());
        assert!(callsite_trap_current_template(&shared, "r14w3/Own", "m", "()I").is_none());
        assert!(callsite_trap_current_template(&shared, "r14w3/Own", "n", "()V").is_none());
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DEOPT_CALLSITE_STASH_TEMPLATE", Some("0"))],
            || callsite_trap_current_template(&shared, "r14w3/Own", "m", "()V"),
        );
        assert!(off.is_none(), "the kill switch keeps the name path");
        let _ = cratonvm_jit::deopt::take_last_deopt();
        drop(body);
    }
}
