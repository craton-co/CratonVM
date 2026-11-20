#[allow(unused_imports)]
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
pub(in crate::runtime::interpreter) fn ir_deopt_resume_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG
        .get_or_init(|| None::<std::ffi::OsString>.is_some())}

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
    *ON.get_or_init(|| false)}

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
pub(in crate::runtime::interpreter) fn fv_to_value(v: &cratonvm_jit::deopt::FrameValue) -> Option<Value> {
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
pub(in crate::runtime::interpreter) fn ir_deopt_frame_values(
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
pub(in crate::runtime::interpreter) fn ir_deopt_locals(vals: &[cratonvm_jit::deopt::FrameValue]) -> Option<Vec<Value>> {
    use cratonvm_jit::deopt::FrameValue;
    let mut out = Vec::with_capacity(vals.len());
    let mut i = 0;
    while i < vals.len() {
        // JIT round 21 wave 2 (lane keepset): a cat-2 value whose upper half
        // holds another value is the verifier's TOP; see
        // `cat2_pair_split_in_locals`.
        if cat2_pair_split_in_locals(vals, i) {
            out.push(Value::Int(0));
            i += 1;
            continue;
        }
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



/// Is `vals[i]` (a JVM-slot-indexed locals snapshot) a `long` / `double`
/// whose reserved upper half `vals[i + 1]` holds another DESCRIBED value
/// rather than the `Undefined` placeholder? That is a producer bug (the IR
/// builder's join, `IrBuilder::kill_cat2_values_split_at_a_join`, is the one
/// known; ECJ / Kotlin slot reuse reaches it): the slot after the cat-2 value
/// was written, so the JVM verifier calls the cat-2 slot TOP, nothing reads
/// it, and the value at `i + 1` is live. [`ir_deopt_locals`] used to skip
/// `i + 1` as an upper half -- dropping that live value and shifting every
/// later local of the rebuilt frame by one slot. Under the switch the cat-2
/// value is dropped instead (`Int(0)`, one slot) and `i + 1` is kept, logged
/// under `CRATONVM_DBG_DEOPT=1`. An upper half the producer could not describe
/// (`Unsupported`, ...) keeps the old skip: refusing the frame would trade a
/// dead slot for a re-run. False with the switch at `0`.
pub(in crate::runtime::interpreter::deopt_resume) fn cat2_pair_split_in_locals(vals: &[cratonvm_jit::deopt::FrameValue], i: usize) -> bool {
    use cratonvm_jit::deopt::FrameValue;
    let split = matches!(vals.get(i), Some(FrameValue::Long(_) | FrameValue::Double(_)))
        && vals
            .get(i + 1)
            .is_some_and(|hi| !matches!(hi, FrameValue::Undefined) && fv_to_value(hi).is_some());
    if !split {
        return false;
    }
    if false {
        eprintln!(
            "[cratonvm-deopt] locals slot {i}: {:?} with a described upper half {:?}; \
             treated as TOP (producer bug)",
            vals[i],
            vals[i + 1]
        );
    }
    true
}

/// Resume interpretation from an IR-path deopt: build a frame for the deopting
/// method (`cached`) with the reconstructed locals/operand-stack and resume at
/// the reconstructed bci. Returns `Some(FramePushed)` on success, or `None` to
/// fall back to the re-run path (unmappable values, inlined caller chain, or
/// held monitors — none handled in this first cut). Mirrors the frame-push
/// ordering of `route_jit_exception_through_method` (refill pools, build via
/// `Frame::new_pooled`, push, set pc) so GC-root and pool handling match.
pub(in crate::runtime::interpreter) fn resume_from_ir_deopt(
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
            Err(why) => {
                // Round 15 wave 3 (lane chain3, CH2W3-1).
                note_inlined_chain_census(shared, ChainCensusEvent::Refused);
                bail(&format!("inlined caller chain: {why}"))
            }
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
                held.push((
                    unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) },
                    m.lock_depth,
                ));
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
    frame.note_method_is_static(cached.is_static);
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
    let handed_monitor = handed_method_monitor_at(
        shared,
        cached.declaring_class_id,
        cached.is_synchronized,
        cached.is_static,
        &cached.code,
        rframe,
    );
    for (i, &(obj, depth)) in held.iter().enumerate() {
        if handed_monitor == Some(i) {
            frame.monitor_on_exit = Some(obj);
            continue;
        }
        for _ in 0..depth {
            frame.held_monitors.push(obj);
        }
    }
    // Interpreter round i6 wave 119, lane L6: a compiled frame that names no
    // lock leaves an understated record where the method can hold one (as
    // `build_deopt_frame_inner`).
    // Wave 121 (lane L6): the producer in the mark, trusted or not.
    if held.is_empty() {
        crate::runtime::interpreter::held_monitors::clear_untrusted_record(
            &mut frame,
            crate::runtime::interpreter::held_monitors::RecordProducer::IrDeoptResume,
        );
    } else {
        crate::runtime::interpreter::held_monitors::note_trusted_record(
            &mut frame,
            crate::runtime::interpreter::held_monitors::RecordProducer::IrDeoptResume,
        );
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
/// The legacy resume budget is 9 and the IR-side
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
pub(in crate::runtime::interpreter) const MAX_INLINE_RESUME_DEPTH: usize = cratonvm_jit::deopt::MAX_OSR_INLINE_RESUME_DEPTH;

/// The budget the sinks check, which is [`MAX_INLINE_RESUME_DEPTH`] unless
/// `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE` raised it (round rx27 wave 4, lane
/// chain; `cratonvm_jit::deopt::max_osr_inline_resume_depth`, the same
/// function the jit's admission reads). The chain is built in a `Vec`, and
/// every refusal still runs before the first push.
pub(in crate::runtime::interpreter::deopt_resume) fn inline_resume_budget() -> usize {
    MAX_INLINE_RESUME_DEPTH.max(cratonvm_jit::deopt::max_osr_inline_resume_depth() + 2)
}

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
pub(in crate::runtime::interpreter) fn caller_resume_pc(
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
/// `docs/feature-designs/deopt-osr.md` §5.2. The rule this encodes was
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
pub(in crate::runtime::interpreter::deopt_resume) fn scope_resume_pc(
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
pub(in crate::runtime::interpreter) fn caller_frame_values(
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
pub(in crate::runtime::interpreter) struct InlinedChainFrame {
    pub(in crate::runtime::interpreter) cached: Arc<CachedBytecodeMethod>,
    pub(in crate::runtime::interpreter) locals: Vec<Value>,
    pub(in crate::runtime::interpreter) stack: Vec<Value>,
    /// Where to park, decided by the scope's own
    /// [`cratonvm_jit::deopt::ResumeSemantics`] in [`scope_resume_pc`]: an
    /// invoke's successor for a `RESUME` scope (every caller scope), and the
    /// snapshot's own bci for a `REEXECUTE` one (the innermost, trapping
    /// scope).
    pub(in crate::runtime::interpreter) resume_pc: usize,
    /// The scope's own bci, the pushed frame's `last_instr_pc` (round 13 wave
    /// 6, lane chain2): for a caller scope the `invoke` it is parked in. The
    /// unwinder searches a CALLER frame's exception table at its
    /// `last_instr_pc` (`interpreter::unwind_to_handler`: "the invoke itself,
    /// since `pc` has already advanced"), and a frame built here had never
    /// executed an instruction, so it said 0: an exception the resumed callee
    /// threw found the handlers covering pc 0 instead of the ones covering the
    /// call -- a `catch` around the call missed, or one around pc 0 caught what
    /// it does not cover. Stack traces and JDWP read the same field.
    pub(in crate::runtime::interpreter) executing_pc: usize,
    /// The locks this scope's compiled code took and still holds (`relock ==
    /// false`), each with its `lock_depth`, outermost first
    /// ([`scope_taken_locks`]): the pushed frame's `held_monitors` record.
    /// Interpreter round i1 wave 25, lane L2. Raw addresses from the snapshot,
    /// exactly as exposed as `locals` until the push roots both.
    pub(in crate::runtime::interpreter) monitors: Vec<(ObjectRef, u32)>,
}

/// The locks a reconstructed scope's compiled code took (`relock == false`,
/// a non-null object, a non-zero depth), with their depths, in the frame
/// state's order (outermost first). The record a frame rebuilt for this scope
/// starts with — the seeding `build_deopt_frame_inner` does for a single
/// frame, for each frame of an inlined chain (interpreter round i1 wave 25,
/// lane L2). An elided lock (`relock`) is not listed: every chain path refuses
/// one (`caller_frame_values`).
pub(in crate::runtime::interpreter::deopt_resume) fn scope_taken_locks(rf: &cratonvm_jit::deopt::ReconstructedFrame) -> Vec<(ObjectRef, u32)> {
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
pub(in crate::runtime::interpreter) fn resolve_inlined_callee(
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
/// chain) keeps the name rule, widened by [`resolve_unhinted_scope_class`]
/// (`CRATONVM_DEOPT_RESUME_SCOPE_BY_LOADER`): the enclosing class by id, then
/// the name, then the constant-pool class's supertypes and defining loader.
pub(in crate::runtime::interpreter) fn resolve_inlined_callee_hinted(
    shared: &SharedVm,
    enclosing_class_id: ClassId,
    method_key: &str,
    hints: &[(String, u32)],
) -> Result<Arc<CachedBytecodeMethod>, String> {
    resolve_inlined_callee_scoped(shared, enclosing_class_id, method_key, 0, hints)
}

pub(in crate::runtime::interpreter) fn resolve_inlined_callee_scoped(
    shared: &SharedVm,
    enclosing_class_id: ClassId,
    method_key: &str,
    scope_class_id: u32,
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
    let hint = if scope_class_id != 0 {
        SpliceScopeClass::Exact(scope_class_id)
    } else {
        cratonvm_jit::deopt::splice_scope_class_hint(hints, method_key)
    };
    let cid = match hint {
        SpliceScopeClass::Exact(id) => {
            let hinted = ClassId::new(id);
            match cm.class_store().get(hinted) {
                // A hidden class's key may spell its class-FILE name
                // (`CRATONVM_HIDDEN_SELF_CP_BY_ID`, wave 21).
                Some(class)
                    if &*class.name == class_name
                        || cm.hidden_self_cp_answer(class_name, hinted).is_some() =>
                {
                    hinted
                }
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
        SpliceScopeClass::Unknown => resolve_unhinted_scope_class(
            &cm,
            enclosing_class_id,
            class_name,
            method_name,
            desc,
        )?,
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
                &code_attr.exception_table,
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

/// `CRATONVM_DEOPT_RESUME_SCOPE_BY_LOADER`, **default ON** (JIT round 21 wave
/// 16, lane deopt-resume; `r21-review-w15-jit-findings-FIXED-20261020` item 5).
///
/// A chain scope with no `splice_scope_class_ids` row used to be resolved by
/// NAME from the enclosing method's loader and nothing else. Under the switch
/// [`resolve_unhinted_scope_class`] first answers a key that names the
/// enclosing class itself by that class's id (a hidden class is never found
/// by name, and a same-named copy in a parent loader could shadow it). When
/// the name lookup then misses, it retries through the constant-pool classes
/// the enclosing pool names for that member: their supertypes by id, then
/// their defining loaders. Since wave 19 that constant-pool scan also runs
/// when the self or name rule answered, and a disagreement refuses the scope
/// (`r21-review-w18-findings-20261019` item 1). `=0` keeps the name rule
/// alone, under which those scopes fail closed and the resume falls back to a
/// slower whole-method re-run.
pub(in crate::runtime::interpreter::deopt_resume) fn deopt_resume_scope_by_loader_enabled() -> bool {
    true
}

/// The bound on the supertype walk of [`supertype_named`]. Real closures are
/// far smaller; the cap only stops a malformed hierarchy from spinning.
pub(in crate::runtime::interpreter::deopt_resume) const SCOPE_SUPERTYPE_WALK_LIMIT: usize = 512;

/// `start` or one of its supertypes (superclasses and interfaces, by id) whose
/// name is `class_name`. Ids, not names: a supertype is linked to the class
/// that the subtype's own loader resolved, which is exactly the class member
/// resolution walked when the compile chose the body.
///
/// The whole closure is walked (JIT round 21 wave 19,
/// `r21-review-w18-findings-20261019` item 1): a hierarchy can reach two
/// different classes of one name (a superclass from one loader, a
/// superinterface from another), and that is refused (`Err`) rather than
/// answered with whichever the walk met first. A walk past the bound is
/// `Ok(None)` (no evidence), as before.
pub(in crate::runtime::interpreter::deopt_resume) fn supertype_named(
    cm: &cratonvm_classloading::ClassManager,
    start: ClassId,
    class_name: &str,
) -> Result<Option<ClassId>, String> {
    let mut pending = vec![start];
    let mut seen: Vec<ClassId> = Vec::new();
    let mut found: Option<ClassId> = None;
    while let Some(id) = pending.pop() {
        if seen.contains(&id) {
            continue;
        }
        if seen.len() >= SCOPE_SUPERTYPE_WALK_LIMIT {
            return Ok(None);
        }
        seen.push(id);
        let Some(class) = cm.class_store().get(id) else {
            continue;
        };
        // Only the start can be hidden (no class names a hidden class as a
        // supertype); its own class-FILE name is itself, by id
        // (`CRATONVM_HIDDEN_SELF_CP_BY_ID`, JIT round 21 wave 21).
        let names_it = &*class.name == class_name
            || (id == start && cm.hidden_self_cp_answer(class_name, start).is_some());
        if names_it {
            match found {
                None => found = Some(id),
                Some(prev) if prev == id => {}
                Some(_) => {
                    let start_name = cm
                        .class_store()
                        .get(start)
                        .map_or_else(|| "the constant-pool class".to_string(), |c| c.name.to_string());
                    return Err(format!(
                        "{class_name} names two different supertypes of {start_name}; not guessed"
                    ));
                }
            }
        }
        if let Some(sup) = class.superclass {
            pending.push(sup);
        }
        pending.extend(class.interfaces.iter().copied());
    }
    Ok(found)
}

/// The class a chain scope with no class-id row names, or why it cannot be
/// found (fail closed). See [`deopt_resume_scope_by_loader_enabled`].
///
/// 1. The key names the enclosing class itself: that class, by id.
/// 2. The name, from the enclosing class's loader (the rule before the switch).
/// 3. Every `Methodref` / `InterfaceMethodref` in the enclosing pool whose
///    name and descriptor are the scope's: its class, resolved from the
///    enclosing loader (or the enclosing class itself for a self-reference),
///    then the key's class among that class's supertypes by id, else by name
///    from that class's DEFINING loader. One answer is used; two different
///    answers are refused rather than guessed.
///
/// Step 3 runs even when step 1 or 2 answered (JIT round 21 wave 19,
/// `r21-review-w18-findings-20261019` item 1): the enclosing loader may define
/// its own class of the key's name while the compile spliced another loader's
/// class reached through a constant-pool class's supertypes. When the scan
/// answers a different class than step 1-2, the scope is refused (fail
/// closed); when it has no answer, the step 1-2 answer stands.
pub(in crate::runtime::interpreter::deopt_resume) fn resolve_unhinted_scope_class(
    cm: &cratonvm_classloading::ClassManager,
    enclosing_class_id: ClassId,
    class_name: &str,
    method_name: &str,
    desc: &str,
) -> Result<ClassId, String> {
    // A hidden enclosing class's key and pool spell its class-FILE name, which
    // a by-name lookup answers with an ordinary class of that name once one is
    // loaded (`r21-review-w21-findings-20261019` item 1).
    // `find_cp_class_for_class` / `hidden_self_cp_answer` bind it to the
    // hidden class by id under `CRATONVM_HIDDEN_SELF_CP_BY_ID` (read once per
    // VM by the class manager) and are the plain name lookup otherwise.
    if !deopt_resume_scope_by_loader_enabled() {
        return cm
            .find_cp_class_for_class(class_name, enclosing_class_id)
            .ok_or_else(|| {
                format!("{class_name} is not resolvable from the class that inlined it")
            });
    }
    let names_enclosing = cm
        .class_store()
        .get(enclosing_class_id)
        .is_some_and(|class| &*class.name == class_name)
        || cm
            .hidden_self_cp_answer(class_name, enclosing_class_id)
            .is_some();
    let early = if names_enclosing {
        Some(enclosing_class_id)
    } else {
        cm.find_class_by_name_for_class(class_name, enclosing_class_id)
    };
    let scanned =
        scope_class_through_member_refs(cm, enclosing_class_id, class_name, method_name, desc)?;
    match (early, scanned) {
        (Some(early), Some(scanned)) if early != scanned => Err(format!(
            "{class_name} names one class from the class that inlined it and another through \
             its constant-pool classes; not guessed"
        )),
        (Some(cid), _) | (None, Some(cid)) => Ok(cid),
        (None, None) => Err(format!(
            "{class_name} is not resolvable from the class that inlined it"
        )),
    }
}

/// Step 3 of [`resolve_unhinted_scope_class`]: `Ok(None)` when no
/// constant-pool class of a matching member reference leads to `class_name`.
pub(in crate::runtime::interpreter::deopt_resume) fn scope_class_through_member_refs(
    cm: &cratonvm_classloading::ClassManager,
    enclosing_class_id: ClassId,
    class_name: &str,
    method_name: &str,
    desc: &str,
) -> Result<Option<ClassId>, String> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let Some(enclosing) = cm.class_store().get(enclosing_class_id) else {
        return Ok(None);
    };
    let pool = &enclosing.constant_pool;
    let mut found: Option<ClassId> = None;
    let mut cp_classes_seen: Vec<ClassId> = Vec::new();
    for index in 1..pool.len() {
        let Ok(index) = u16::try_from(index) else {
            break;
        };
        let (class_index, nat_index) = match pool.get(index) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            })
            | Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index),
            _ => continue,
        };
        if pool.get_name_and_type(nat_index) != Some((method_name, desc)) {
            continue;
        }
        let Some(cp_name) = pool.get_class_name(class_index) else {
            continue;
        };
        if cp_name.starts_with('[') {
            continue;
        }
        let cp_class = if cp_name == &*enclosing.name {
            enclosing_class_id
        } else {
            // The pool's own class-FILE name binds a hidden enclosing class
            // by id (`CRATONVM_HIDDEN_SELF_CP_BY_ID`, wave 21).
            match cm.find_cp_class_for_class(cp_name, enclosing_class_id) {
                Some(id) => id,
                None => continue,
            }
        };
        if cp_classes_seen.contains(&cp_class) {
            continue;
        }
        cp_classes_seen.push(cp_class);
        let candidate = match supertype_named(cm, cp_class, class_name)? {
            Some(id) => Some(id),
            None => cm.find_class_by_name_for_class(class_name, cp_class),
        };
        match (found, candidate) {
            (_, None) => {}
            (None, Some(id)) => found = Some(id),
            (Some(prev), Some(id)) if prev == id => {}
            (Some(_), Some(_)) => {
                return Err(format!(
                    "{class_name} resolves to two different classes through the constant-pool \
                     classes of the class that inlined it; not guessed"
                ))
            }
        }
    }
    Ok(found)
}

/// Materialise a whole inlined deopt chain, OUTERMOST first, or refuse it.
///
/// **Step 1 of the five-step chain in
/// `docs/feature-designs/real-jdk-mode.md`.**
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
pub(in crate::runtime::interpreter) fn materialise_inlined_chain(
    shared: &SharedVm,
    outermost: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    hints: &[(String, u32)],
    own_sources: &[(String, Arc<CachedBytecodeMethod>)],
) -> Result<Vec<InlinedChainFrame>, String> {
    let depth = rframe.caller_frames.len();
    let budget = inline_resume_budget();
    if depth > budget {
        return Err(format!(
            "inlined chain is {depth} deep, past the {budget}-frame resume budget"
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
pub(in crate::runtime::interpreter::deopt_resume) fn inner_scope_own_source(
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
pub(in crate::runtime::interpreter::deopt_resume) fn materialise_inner_scopes(
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
            None => resolve_inlined_callee_scoped(shared, enclosing, &scope.method_key, scope.class_id, hints)?,
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
        None => resolve_inlined_callee_scoped(shared, enclosing, &rframe.method_key, rframe.class_id, hints)?,
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
pub(in crate::runtime::interpreter::deopt_resume) fn chain_frames_report_their_bci() -> bool {
    true
}

/// Round 15 wave 3 (lane chain3, CH2W3-1 of `jit-r15w3-chain2-proposals-RETIRED-20261003.md`):
/// this VM's inlined-chain resume census (`JitBridgeState::inlined_chain_census`,
/// per VM, no process static).
///
/// The chain-arm flip's evidence is "`CRATONVM_DEOPT_EAGER_CHAINS=1` changes no
/// checked line", which cannot tell "every chain point was resumed precisely"
/// from "no chain was ever resumed" or "every chain was refused and the method
/// re-ran". Under `CRATONVM_DBG_DEOPT` each event below prints the running
/// totals as one `[cratonvm-deopt] eager-chains:` line, so the last such line
/// of a run is its census. A trap several sinks try in turn counts one refusal
/// per sink that refused it.
///
/// Round 15 wave 4 (lane chain4, `r15w3-chain3-forced-chain-exit-bit-patch`):
/// forced exits are told apart from natural ones. The forced pad is the
/// point's own (`ir_lower::emit_forced_chain_deopt`), so the stash is
/// identical; the forced path marks the point's cell and the sink that holds
/// the trapping artifact claims the mark ([`claim_forced_chain_trip`]).
/// `forced` counts the claims, `forced-resumed` those whose chain the same
/// sink then pushed (or transferred in place, for an OSR exit). The soak's
/// evidence is `forced == forced-resumed > 0` with `refused == 0`: every
/// forced exit a sink saw was resumed precisely. A claimed forced exit that
/// was refused, restashed or re-run shows as `forced > forced-resumed`.
///
/// Round 15 wave 4 (lane chain5): the soak is made honest and its coverage
/// stated. A claimed forced trip is left uncharged
/// ([`leave_forced_trip_uncharged`], `uncharged=`), so the soak no longer
/// evicts the bodies and withdraws the splices it soaks; each body's first
/// forced trip adds its chain points to `points=` and each point's first adds
/// one to `points-forced=`, so `never-forced=` (their difference) names the
/// chain points of the bodies the run exercised that were never forced (CH4W4-2;
/// with the first arrival forced too, CH4W4-3, a point there was never reached
/// or its forced trip was never claimed). The whole soak verdict is one line:
/// `forced == forced-resumed == uncharged > 0`, `refused == 0`,
/// `never-forced == 0` (or each never-forced point accounted for).
///
/// Round 16 lane chain (CH5W4-1 + CH5W4-3 of `jit-r15w4-chain5-proposals-RETIRED-20261003.md`):
/// the census is exact. The forced bit rides in the stash
/// (`cratonvm_jit::deopt::claim_forced_trip_mark`), so every sink tells a
/// forced trip without the trapping body (G1, G2 of
/// `r15w4-chain5-review-findings`) and each trip counts once in `forced=` and
/// `uncharged=` however many sinks see it; and `points=` is counted at
/// PUBLICATION (`JitCache::note_forced_chain_publication`), so it covers every
/// body the VM published, reached or not (G5): a never-forced point is a path
/// the run never took with that body installed, and the exit report names it.
#[derive(Default)]
pub(crate) struct InlinedChainCensus {
    /// Forced chain exits a sink claimed (`CRATONVM_DEOPT_EAGER_CHAINS`), each
    /// trip once.
    pub(in crate::runtime::interpreter::deopt_resume) forced: std::sync::atomic::AtomicU64,
    /// Of `forced`: the chain was pushed / transferred by that sink.
    pub(in crate::runtime::interpreter::deopt_resume) forced_resumed: std::sync::atomic::AtomicU64,
    /// Of `forced`: left uncharged by the sink's despeculation (round 15
    /// wave 4, lane chain5; `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED`), each
    /// trip once.
    pub(in crate::runtime::interpreter::deopt_resume) uncharged: std::sync::atomic::AtomicU64,
    /// Chain points (forced-block cells) of every body published while the
    /// soak was on (round 16 lane chain, CH5W4-3; before, of every body with a
    /// claimed forced trip).
    pub(in crate::runtime::interpreter::deopt_resume) points: std::sync::atomic::AtomicU64,
    /// Of `points`: points with at least one claimed forced trip (CH4W4-2).
    pub(in crate::runtime::interpreter::deopt_resume) points_forced: std::sync::atomic::AtomicU64,
    /// Of `forced`: trips at a point no publication row names (a body
    /// published past the coverage cap, or through a funnel that does not
    /// report). Round 16 lane chain; zero in a complete census.
    pub(in crate::runtime::interpreter::deopt_resume) forced_unlisted: std::sync::atomic::AtomicU64,
    /// Chains [`push_inlined_chain`] pushed whole (every sink's precise chain
    /// resume, the OSR-exit transfers' inner scopes included).
    pub(in crate::runtime::interpreter::deopt_resume) pushed: std::sync::atomic::AtomicU64,
    /// Frames those chains pushed.
    pub(in crate::runtime::interpreter::deopt_resume) frames: std::sync::atomic::AtomicU64,
    /// Of `pushed`: OSR-exit transfers, whose outermost scope was written into
    /// the live frame in place.
    pub(in crate::runtime::interpreter::deopt_resume) osr_in_place: std::sync::atomic::AtomicU64,
    /// A sink asked for a chain and did not get one pushed (materialisation
    /// refused, a redefinition it cannot vouch for, an OSR-exit transfer
    /// refused, or the push itself declined). Counted per SINK that refused,
    /// not per trap: a trap the call-site service refuses and the door then
    /// refuses again is two (`r15w4-chain4-review-findings` F3). The soak
    /// verdict only asks whether it is zero.
    pub(in crate::runtime::interpreter::deopt_resume) refused: std::sync::atomic::AtomicU64,
    /// Every event above, one per [`note_inlined_chain_census`] call (round 15
    /// wave 5, lane chain6; CH5W4-2 of `jit-r15w4-chain5-proposals-RETIRED-20261003.md`): the
    /// running line is printed only when this reaches a power of two, and the
    /// final one by [`report_inlined_chain_census_at_exit`].
    pub(in crate::runtime::interpreter::deopt_resume) events: std::sync::atomic::AtomicU64,
    /// Which chain points of each published body were forced
    /// ([`fold_chain_publications`], [`mark_chain_point_forced`]), so the exit
    /// report can NAME the `never-forced` ones (round 15 wave 5, lane chain6;
    /// CH5W4-5; fed at publication since round 16 lane chain, CH5W4-3). Stays
    /// empty without `CRATONVM_DEOPT_EAGER_CHAINS` (no body has a cell).
    pub(in crate::runtime::interpreter::deopt_resume) coverage: std::sync::Mutex<ChainCoverage>,
    /// JIT round 19 wave 12, lane tail12 (TAIL11-3 / DEOPT10-2's census): how
    /// [`build_deopt_frame_chain_for_point`] found the body owning a stashed
    /// chain point -- `[named by the stash, the cache's published bodies, the
    /// code-range walk, none]` -- counted per build with a point. Printed at
    /// exit under `CRATONVM_DBG_DEOPT` (`chain point owners:`); a run whose
    /// `range=` stays 0 never needed the walk.
    pub(in crate::runtime::interpreter::deopt_resume) owner_by: [std::sync::atomic::AtomicU64; 4],
}

/// [`InlinedChainCensus::owner_by`]'s columns.
#[derive(Clone, Copy)]
pub(in crate::runtime::interpreter::deopt_resume) enum ChainPointOwnerSource {
    Stash = 0,
    Cache = 1,
    Range = 2,
    None = 3,
}

/// Count one chain-point owner lookup ([`InlinedChainCensus::owner_by`]).
pub(in crate::runtime::interpreter::deopt_resume) fn note_chain_point_owner(shared: &SharedVm, source: ChainPointOwnerSource) {
    shared.jit.bridge.inlined_chain_census.owner_by[source as usize]
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The `chain point owners:` exit line, `None` when no lookup was counted.
pub(in crate::runtime::interpreter::deopt_resume) fn chain_point_owner_census_line(c: &InlinedChainCensus) -> Option<String> {
    use std::sync::atomic::Ordering::Relaxed;
    let [stash, cache, range, none] = [0, 1, 2, 3].map(|i| c.owner_by[i].load(Relaxed));
    (stash + cache + range + none > 0).then(|| {
        format!(
            "[cratonvm-deopt] chain point owners: stash={stash} cache={cache} range={range} \
             none={none} (range > 0: the code-range walk was still needed; \
             CRATONVM_DEOPT_CHAIN_POINT_OWNER_BY_RANGE)"
        )
    })
}

/// [`InlinedChainCensus::coverage`]: the published bodies' chain points, and
/// an index from point address to its row.
#[derive(Default)]
pub(in crate::runtime::interpreter::deopt_resume) struct ChainCoverage {
    pub(in crate::runtime::interpreter::deopt_resume) bodies: Vec<ChainCoverageBody>,
    // Point address -> (body, point) in `bodies`. A freed body's point
    // address can be reused by a later body; the later publication
    // overwrites the entry, so a trap finds the NEWEST row naming its point
    // (a trap can only come from a published, hence the newest, owner).
    // addr-keyed: not a heap address -- a JIT code address (chain point); overwritten by the newest publication, never moved by a GC.
    pub(in crate::runtime::interpreter::deopt_resume) by_point: std::collections::HashMap<usize, (usize, usize)>,
    /// Bodies past [`CHAIN_COVERAGE_BODIES_MAX`] (counted in `points=`, not
    /// described).
    pub(in crate::runtime::interpreter::deopt_resume) bodies_past_cap: u64,
}

/// One published body's chain points in [`ChainCoverage`].
pub(in crate::runtime::interpreter::deopt_resume) struct ChainCoverageBody {
    /// `entry`, `osr` or `osr-optimizing` (`JitCache::note_forced_chain_publication`).
    pub(in crate::runtime::interpreter::deopt_resume) kind: &'static str,
    pub(in crate::runtime::interpreter::deopt_resume) points: Vec<ChainCoveragePoint>,
}

/// One chain point (forced-block cell) of a [`ChainCoverageBody`].
pub(in crate::runtime::interpreter::deopt_resume) struct ChainCoveragePoint {
    /// A forced trip at this point was claimed.
    pub(in crate::runtime::interpreter::deopt_resume) forced: bool,
    /// Forced trips claimed at this point, each trip once (round 16 wave 14,
    /// lane framestate; CH16-2 of `jit-r16-chain-proposals-RETIRED-20261006.md`).
    pub(in crate::runtime::interpreter::deopt_resume) forced_trips: u32,
    /// Of `forced_trips`: trips whose chain a sink resumed, each trip once
    /// (`cratonvm_jit::deopt::mark_taken_forced_trip_resumed`). A point with
    /// fewer is listed `never-resumed` at exit and fails the soak verdict, so
    /// one refusing point can no longer hide behind another point's
    /// double-counted `forced-resumed=` total.
    pub(in crate::runtime::interpreter::deopt_resume) resumed_trips: u32,
    /// Its chain, innermost scope first (`method@bci <- ...`), and the reason.
    pub(in crate::runtime::interpreter::deopt_resume) what: String,
}

/// [`InlinedChainCensus::per_point_resumes`]: of the recorded points with a
/// claimed forced trip, how many had every trip resumed (`points-resumed=`)
/// and how many did not, and the latter by chain description:
/// `(description [kind], forced trips, resumed trips)`, summed over the
/// bodies that carry it.
#[derive(Default)]
pub(in crate::runtime::interpreter::deopt_resume) struct PerPointResumes {
    pub(in crate::runtime::interpreter::deopt_resume) resumed: u64,
    pub(in crate::runtime::interpreter::deopt_resume) unresumed: u64,
    pub(in crate::runtime::interpreter::deopt_resume) rows: Vec<(String, u64, u64)>,
}

/// Bodies [`InlinedChainCensus::coverage`] records; past it the listing is
/// partial and says so. A debug census, bounded like any other. Round 16 lane
/// chain: raised from 4096 with publication counting (every published body
/// is recorded, boot-compiled JDK bodies included).
pub(in crate::runtime::interpreter::deopt_resume) const CHAIN_COVERAGE_BODIES_MAX: usize = 65_536;

/// Distinct never-forced points the exit report lists by name (round 16 lane
/// chain: raised from 64, so a soak's whole never-forced list is printed; a
/// row is one chain description however many bodies carry it).
pub(in crate::runtime::interpreter::deopt_resume) const CHAIN_NEVER_FORCED_LISTED: usize = 4096;

impl InlinedChainCensus {
    /// The running totals as the one `[cratonvm-deopt] eager-chains:` line
    /// (its text unchanged since lane chain5, so a harness reading it still
    /// matches).
    pub(in crate::runtime::interpreter::deopt_resume) fn census_line(&self) -> String {
        use std::sync::atomic::Ordering::Relaxed;
        let on = |b: bool| if b { "on" } else { "off" };
        let (points, points_forced) = (self.points.load(Relaxed), self.points_forced.load(Relaxed));
        // Round 15 wave 4 (lane chain4): `forced=` / `forced-resumed=`, and
        // the chain arm the run had (CHN15-7: which of the three switches the
        // umbrella or an explicit setting turned on), so one line states the
        // soak's arm and its evidence. Lane chain5: `uncharged=` and the
        // coverage (`points=` / `points-forced=` / `never-forced=`, CH4W4-2).
        format!(
            "[cratonvm-deopt] eager-chains: forced={} forced-resumed={} uncharged={} \
             points={points} points-forced={points_forced} never-forced={} pushed={} frames={} \
             osr-in-place={} refused={} (this VM; CRATONVM_DEOPT_EAGER_CHAINS={}; chain arm: \
             frame-states={} chain-fences={} multi-return={})",
            self.forced.load(Relaxed),
            self.forced_resumed.load(Relaxed),
            self.uncharged.load(Relaxed),
            points.saturating_sub(points_forced),
            self.pushed.load(Relaxed),
            self.frames.load(Relaxed),
            self.osr_in_place.load(Relaxed),
            self.refused.load(Relaxed),
            on(cratonvm_jit::ir::ir_forced_chain_deopts_enabled()),
            on(cratonvm_jit::ir::ir_splice_frame_states_enabled()),
            on(cratonvm_jit::ir::ir_splice_chain_fences_enabled()),
            on(cratonvm_jit::ir::ir_splice_multi_return_enabled()),
        )
    }

    /// The eager-chains soak's CORRECTNESS verdict over the final totals
    /// (round 15 wave 5, lane chain6; CH5W4-4 of
    /// `jit-r15w4-chain5-proposals-RETIRED-20261003.md`): `Ok` passes, `Err` names the first
    /// clause that failed. The clauses, in order: something was forced
    /// (`forced > 0`, else the run soaked nothing); every claimed forced exit
    /// was resumed (`forced == forced-resumed`); every one was left uncharged
    /// when `uncharged` is on (`== uncharged`, else the soak evicted what it
    /// soaked); no sink refused a chain (`refused == 0`). Coverage
    /// (`never-forced`) is stated beside it, not folded in: a chain point on a
    /// path the probe never runs (a spliced `resize` branch of a body compiled
    /// during boot) is a probe gap, and the exit report names each one.
    /// Mid-run the totals are not comparable (a claim is counted before its
    /// push), which is why only the exit report states a verdict.
    pub(in crate::runtime::interpreter::deopt_resume) fn soak_verdict(&self, uncharged_on: bool) -> Result<(), String> {
        use std::sync::atomic::Ordering::Relaxed;
        let forced = self.forced.load(Relaxed);
        let resumed = self.forced_resumed.load(Relaxed);
        let uncharged = self.uncharged.load(Relaxed);
        let refused = self.refused.load(Relaxed);
        if forced == 0 {
            return Err("forced=0: no chain exit was forced".to_string());
        }
        if resumed != forced {
            return Err(format!("forced-resumed={resumed} != forced={forced}"));
        }
        if uncharged_on && uncharged != forced {
            return Err(format!("uncharged={uncharged} != forced={forced}"));
        }
        if refused != 0 {
            return Err(format!("refused={refused}"));
        }
        // Round 16 wave 14 (lane framestate, CH16-2): per point, every forced
        // trip resumed -- the totals above could net a refusing point out
        // against a double-counted one.
        let unresumed = self.per_point_resumes().unresumed;
        if unresumed != 0 {
            return Err(format!("points-unresumed={unresumed}"));
        }
        Ok(())
    }

    /// The per-point resume coverage of the recorded bodies (round 16 wave 14,
    /// lane framestate; CH16-2 of `jit-r16-chain-proposals-RETIRED-20261006.md`).
    pub(in crate::runtime::interpreter::deopt_resume) fn per_point_resumes(&self) -> PerPointResumes {
        let coverage = self
            .coverage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = PerPointResumes::default();
        let mut rows: std::collections::BTreeMap<String, (u64, u64)> =
            std::collections::BTreeMap::new();
        for b in &coverage.bodies {
            for p in b.points.iter().filter(|p| p.forced_trips > 0) {
                if p.resumed_trips >= p.forced_trips {
                    out.resumed += 1;
                    continue;
                }
                out.unresumed += 1;
                let row = rows
                    .entry(format!("{} [{}]", p.what, b.kind))
                    .or_insert((0, 0));
                row.0 += u64::from(p.forced_trips);
                row.1 += u64::from(p.resumed_trips);
            }
        }
        out.rows = rows
            .into_iter()
            .map(|(what, (forced, resumed))| (what, forced, resumed))
            .collect();
        out
    }

    /// The never-forced points of the recorded bodies, one row per distinct
    /// chain description (a method compiled N times has N bodies with the same
    /// points): `(description, bodies where never forced, bodies recorded)`,
    /// in description order. Also whether the recording hit its cap.
    pub(in crate::runtime::interpreter::deopt_resume) fn never_forced_points(&self) -> (Vec<(String, usize, usize)>, bool) {
        let coverage = self
            .coverage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows: std::collections::BTreeMap<String, (usize, usize)> =
            std::collections::BTreeMap::new();
        for b in &coverage.bodies {
            for p in &b.points {
                // The body kind is part of the row: an OSR body's never-forced
                // point is a different gap from its method-entry twin's.
                let row = rows
                    .entry(format!("{} [{}]", p.what, b.kind))
                    .or_insert((0, 0));
                row.1 += 1;
                if !p.forced {
                    row.0 += 1;
                }
            }
        }
        let never = rows
            .into_iter()
            .filter(|&(_, (n, _))| n > 0)
            .map(|(what, (n, of))| (what, n, of))
            .collect();
        (never, coverage.bodies_past_cap > 0)
    }
}

/// Round 16 lane chain (CH5W4-3 of `jit-r15w4-chain5-proposals-RETIRED-20261003.md`): fold the
/// chain points this VM's cache recorded at publication
/// (`JitCache::take_forced_chain_publications`) into the coverage table, and
/// count them in `points=`. Called before a forced trip is marked and at exit,
/// so a trap always finds its body's row (a body is published before anything
/// can enter it). Nothing to do, and no lock taken past the drain, without
/// `CRATONVM_DEOPT_EAGER_CHAINS`.
pub(in crate::runtime::interpreter::deopt_resume) fn fold_chain_publications(shared: &SharedVm) {
    let published = shared.jit.jit_cache.take_forced_chain_publications();
    if published.is_empty() {
        return;
    }
    fold_chain_publication_rows(&shared.jit.bridge.inlined_chain_census, published);
}

/// [`fold_chain_publications`]' table update, on rows already drained.
pub(in crate::runtime::interpreter::deopt_resume) fn fold_chain_publication_rows(
    census: &InlinedChainCensus,
    published: Vec<cratonvm_jit::deopt::PublishedChainPoints>,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut coverage = census
        .coverage
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for body in published {
        // Widening: usize -> u64 (a chain point count).
        census.points.fetch_add(body.points.len() as u64, Relaxed);
        if coverage.bodies.len() >= CHAIN_COVERAGE_BODIES_MAX {
            coverage.bodies_past_cap += 1;
            continue;
        }
        let at = coverage.bodies.len();
        let mut points = Vec::with_capacity(body.points.len());
        for (i, (addr, what)) in body.points.into_iter().enumerate() {
            coverage.by_point.insert(addr, (at, i));
            points.push(ChainCoveragePoint {
                forced: false,
                forced_trips: 0,
                resumed_trips: 0,
                what,
            });
        }
        coverage.bodies.push(ChainCoverageBody {
            kind: body.kind,
            points,
        });
    }
}

/// Round 16 lane chain (CH5W4-1 / CH5W4-3): mark the published chain point at
/// `point_addr` forced. `Some(true)` the first time for that point (count it
/// in `points-forced=`), `Some(false)` after, `None` when no publication row
/// names it (`forced-unlisted=`).
pub(in crate::runtime::interpreter::deopt_resume) fn mark_chain_point_forced(census: &InlinedChainCensus, point_addr: usize) -> Option<bool> {
    let mut coverage = census
        .coverage
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let &(body, point) = coverage.by_point.get(&point_addr)?;
    let p = coverage.bodies.get_mut(body)?.points.get_mut(point)?;
    p.forced_trips = p.forced_trips.saturating_add(1);
    Some(!std::mem::replace(&mut p.forced, true))
}

/// Round 16 wave 14 (lane framestate, CH16-2 of `jit-r16-chain-proposals-RETIRED-20261006.md`):
/// credit one resumed forced trip to the published chain point at
/// `point_addr`. Nothing when no publication row names it (the trip was
/// counted `forced-unlisted=` at its claim).
pub(in crate::runtime::interpreter::deopt_resume) fn mark_chain_point_resumed(census: &InlinedChainCensus, point_addr: usize) {
    let mut coverage = census
        .coverage
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(&(body, point)) = coverage.by_point.get(&point_addr) else {
        return;
    };
    if let Some(p) = coverage
        .bodies
        .get_mut(body)
        .and_then(|b| b.points.get_mut(point))
    {
        p.resumed_trips = p.resumed_trips.saturating_add(1);
    }
}

/// Round 15 wave 5 (lane chain6; CH5W4-2 + CH5W4-4 of
/// `jit-r15w4-chain5-proposals-RETIRED-20261003.md`): this VM's inlined-chain census at exit,
/// under `CRATONVM_DBG_DEOPT` -- the final `eager-chains:` line (the running
/// line is printed only at power-of-two event counts), and, under
/// `CRATONVM_DEOPT_EAGER_CHAINS`, one `[cratonvm-deopt] eager-chains
/// never-forced:` line per distinct chain point a counted body never had
/// forced (CH5W4-5: the chain, innermost scope first, the reason, and in how
/// many of the bodies that carry it), then ONE verdict line the orchestrator
/// gates the chain-arm flip on:
///
/// * `eager-chains verdict: FAIL (<first failed clause>)` -- a correctness
///   clause of [`InlinedChainCensus::soak_verdict`] failed;
/// * `eager-chains verdict: PARTIAL (never-forced=N of M points)` -- every
///   forced exit was resumed precisely, and N points were never forced (the
///   lines above name them: since round 16 lane chain, CH5W4-3, every point
///   of every body published while the soak was on is counted, so each
///   listed point is a path the run never took with that body installed);
/// * `eager-chains verdict: PASS` -- both.
///
/// Round 16 lane chain: a `forced-unlisted=N` line when forced trips came
/// from points no publication named (zero in a complete census).
///
/// Nothing when the debug switch is off, or when no chain event happened and
/// the soak switch is off. Called by the launcher's shutdown reports.
///
/// JIT round 19 proposal wave 6, lane sync6 (SELFLOCK19-3): also the
/// thread-exit monitor census ([`report_thread_exit_monitor_census`]).
pub fn report_inlined_chain_census_at_exit(shared: &SharedVm) {
    use std::sync::atomic::Ordering::Relaxed;
    // JIT round 19 wave 3, lane eager3: the eager-chain census
    // (`CRATONVM_DBG_EAGER_CHAIN_CENSUS`), printed from the same two shutdown
    // arms this report is.
    super::jit_bridge::report_eager_chain_census_at_exit(shared);
    report_thread_exit_monitor_census(shared);
    // Interpreter round i5 wave 101, lane L2 (page
    // `i101-L2-compiled-frame-without-a-safepoint-id-has-no-line`): the
    // compiled-frame line census (`cratonvm_jit::compiled_frame_line_counts`)
    // had no reader outside tests, so a line-less compiled frame in a trace
    // could not be attributed to its refusal population.
    if crate::runtime::env_cache::dbg_jitc() {
        let c = cratonvm_jit::compiled_frame_line_counts();
        let names = [
            "single-pass",
            "ir",
            "npe-trap",
            "no-sp-id",
            "id-unrecorded",
            "ir-untranslated",
            "out-of-range",
            "switched-off",
            "innermost-unrecorded",
        ];
        let row: Vec<String> = names
            .iter()
            .zip(c.iter())
            .map(|(name, n)| format!("{name}={n}"))
            .collect();
        eprintln!("[cratonvm-jitc] compiled-frame-lines {} [at exit]", row.join(" "));
    }
    if !dbg_deopt_enabled() {
        return;
    }
    let c = &shared.jit.bridge.inlined_chain_census;
    // JIT round 19 wave 12, lane tail12 (TAIL11-3): independent of the
    // eager-chain events below.
    if let Some(line) = chain_point_owner_census_line(c) {
        eprintln!("{line} [at exit]");
    }
    let soak = cratonvm_jit::ir::ir_forced_chain_deopts_enabled();
    // Round 16 lane chain (CH5W4-3): bodies published since the last forced
    // trip are counted too.
    fold_chain_publications(shared);
    if !soak && c.events.load(Relaxed) == 0 {
        return;
    }
    eprintln!("{} [at exit]", c.census_line());
    if !soak {
        return;
    }
    let unlisted = c.forced_unlisted.load(Relaxed);
    if unlisted > 0 {
        eprintln!(
            "[cratonvm-deopt] eager-chains forced-unlisted={unlisted}: forced trips at points \
             no publication named (not in points=)"
        );
    }
    let (never_rows, truncated) = c.never_forced_points();
    for (what, never, of) in never_rows.iter().take(CHAIN_NEVER_FORCED_LISTED) {
        eprintln!(
            "[cratonvm-deopt] eager-chains never-forced: {what} -- in {never} of {of} bodies"
        );
    }
    if never_rows.len() > CHAIN_NEVER_FORCED_LISTED || truncated {
        eprintln!(
            "[cratonvm-deopt] eager-chains never-forced: {} distinct point(s) not listed{}",
            never_rows.len().saturating_sub(CHAIN_NEVER_FORCED_LISTED),
            if truncated {
                "; bodies past the recording cap are not described"
            } else {
                ""
            },
        );
    }
    // Round 16 wave 14 (lane framestate, CH16-2): per-point resumes.
    let per_point = c.per_point_resumes();
    eprintln!(
        "[cratonvm-deopt] eager-chains per-point: points-forced={} points-resumed={} \
         points-unresumed={}",
        c.points_forced.load(Relaxed),
        per_point.resumed,
        per_point.unresumed,
    );
    for (what, forced, resumed) in per_point.rows.iter().take(CHAIN_NEVER_FORCED_LISTED) {
        eprintln!(
            "[cratonvm-deopt] eager-chains never-resumed: {what} -- {resumed} of {forced} \
             forced trip(s) resumed"
        );
    }
    let uncharged_on =
        true;
    let points = c.points.load(Relaxed);
    let never = points.saturating_sub(c.points_forced.load(Relaxed));
    match c.soak_verdict(uncharged_on) {
        Err(why) => eprintln!("[cratonvm-deopt] eager-chains verdict: FAIL ({why})"),
        Ok(()) if never > 0 => eprintln!(
            "[cratonvm-deopt] eager-chains verdict: PARTIAL (never-forced={never} of {points} points)"
        ),
        Ok(()) => eprintln!("[cratonvm-deopt] eager-chains verdict: PASS"),
    }
}

/// One [`InlinedChainCensus`] event.
#[derive(Clone, Copy)]
pub(in crate::runtime::interpreter::deopt_resume) enum ChainCensusEvent {
    Pushed {
        frames: usize,
    },
    OsrInPlace,
    Refused,
    /// A sink claimed a forced chain exit (round 15 wave 4, lane chain4),
    /// counted once per trip. Lane chain5 (CH4W4-2): the point's first such
    /// claim. Round 16 lane chain: `unlisted` when no publication named the
    /// point (`points=` is counted at publication, not here).
    Forced {
        first_at_point: bool,
        unlisted: bool,
    },
    /// ... and resumed its chain.
    ForcedResumed,
    /// ... and left it uncharged (round 15 wave 4, lane chain5).
    Uncharged,
}

/// Round 15 wave 4 (lane chain4): was the trap at `point_addr` a forced chain
/// exit (`CRATONVM_DEOPT_EAGER_CHAINS`)? Every sink that holds a chain stash
/// asks this ONCE per trap, before anything can refuse, and reports its
/// outcome with [`note_forced_chain_resumed`] (or by marking the chain it
/// builds, [`DeoptFrameChain::with_forced_trip`]). `false` -- and no count,
/// no other effect -- for a natural trap, or with the switch off. Since round
/// 15 wave 4 (lane chain5) the one thing decided on the answer is the sink's
/// charge ([`leave_forced_trip_uncharged`]).
///
/// Round 16 lane chain (CH5W4-1): the answer is the stash's own forced mark
/// (`cratonvm_jit::deopt::claim_forced_trip_mark`), so `artifact` is no longer
/// consulted (kept for the callers' shape); [`claim_forced_chain_trip_at`] is
/// the same question for a sink that holds no body. A trip a second sink
/// claims again (a restashed refusal) is still forced but counted once.
pub(crate) fn claim_forced_chain_trip(
    shared: &SharedVm,
    _artifact: &crate::jit::CompiledMethod,
    point_addr: usize,
) -> bool {
    claim_forced_chain_trip_at(shared, point_addr)
}

/// [`claim_forced_chain_trip`] by the trap's point address alone (round 16
/// lane chain, CH5W4-1): exact for a superseded body, an OSR body the cache
/// replaced, a foreign frame -- every sink that took the stash. On the trip's
/// first claim: counts `forced=`, folds this VM's pending publications and
/// marks the point in the coverage table (`points-forced=` on its first trip;
/// `forced-unlisted=` when no publication named it).
pub(crate) fn claim_forced_chain_trip_at(shared: &SharedVm, point_addr: usize) -> bool {
    let mark = cratonvm_jit::deopt::claim_forced_trip_mark(point_addr);
    if mark.forced && mark.first_claim {
        fold_chain_publications(shared);
        let census = &shared.jit.bridge.inlined_chain_census;
        let point = mark_chain_point_forced(census, point_addr);
        note_inlined_chain_census(
            shared,
            ChainCensusEvent::Forced {
                first_at_point: point == Some(true),
                unlisted: point.is_none(),
            },
        );
    }
    mark.forced
}

/// The outcome half of [`claim_forced_chain_trip`], for a sink that pushes
/// (or transfers) the chain itself: counts `forced-resumed` when the trap was
/// forced and the chain was resumed.
pub(in crate::runtime::interpreter::deopt_resume) fn note_forced_chain_resumed(shared: &SharedVm, forced: bool, resumed: bool) {
    if forced && resumed {
        // Round 16 wave 14 (lane framestate, CH16-2): and credit the trip's
        // point, once per trip (the stash mark the sink took).
        if let Some(point_addr) = cratonvm_jit::deopt::mark_taken_forced_trip_resumed() {
            mark_chain_point_resumed(&shared.jit.bridge.inlined_chain_census, point_addr);
        }
        note_inlined_chain_census(shared, ChainCensusEvent::ForcedResumed);
    }
}

/// [`claim_forced_chain_trip`] and its outcome in one call, for a sink whose
/// outcome is known where it asks (round 15 wave 4, lane chain4's patch page
/// `r15w4-chain4-forced-trip-claims-at-other-sinks-patch`, applied by lane
/// chain5). Returns the claim, for [`leave_forced_trip_uncharged`].
pub(crate) fn note_forced_chain_trip_outcome(
    shared: &SharedVm,
    artifact: &crate::jit::CompiledMethod,
    point_addr: usize,
    resumed: bool,
) -> bool {
    let forced = claim_forced_chain_trip(shared, artifact, point_addr);
    note_forced_chain_resumed(shared, forced, resumed);
    forced
}

/// `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED`, **default ON** (round 15 wave 4,
/// lane chain5; `r15w4-chain4-eager-chains-forced-trips-despeculate-their-splices`):
/// is a trap a sink claimed as forced ([`claim_forced_chain_trip`]) to be left
/// out of that sink's despeculation? Asked ONCE per trap, at the sink's charge
/// decision; a `true` answer is counted `uncharged=` in this VM's census.
///
/// A forced trip is a PASSING guard `CRATONVM_DEOPT_EAGER_CHAINS` sent to its
/// pad, not a failed speculation. Charged like one (record, evict, escalate
/// toward the blacklist, and after four the wildcard de-spec that withdraws
/// the splice at its call site), the soak removed the very chain points it
/// soaked after a few trips each and ran the default arm's code for the rest
/// of the run. Uncharged, a body keeps its chain points and is forced again,
/// so the soak exercises the steady state (late recompiles, OSR bodies entered
/// late, fully-tiered bodies). Natural traps are charged as before.
///
/// Inert unless `CRATONVM_DEOPT_EAGER_CHAINS` is on: `forced` is `true` only
/// for a trap a forced block marked, and no block exists otherwise, so
/// nothing (not even this flag read) changes with the eager switch off. `=0`
/// restores the round-13 soak, which also exercises eviction and de-spec.
///
/// Round 16 lane chain (CH5W4-1): counted once per trip
/// (`cratonvm_jit::deopt::mark_taken_forced_trip_uncharged`), so a forced trip
/// two sinks decide on -- possible now that a restashed one stays forced --
/// is one `uncharged=`.
pub(crate) fn leave_forced_trip_uncharged(shared: &SharedVm, forced: bool) -> bool {
    let uncharged = forced;
    if uncharged && cratonvm_jit::deopt::mark_taken_forced_trip_uncharged() {
        note_inlined_chain_census(shared, ChainCensusEvent::Uncharged);
    }
    uncharged
}

/// An OSR-exit chain transfer's outcome in this VM's census: the outermost
/// scope written in place (its inner scopes' push is counted by
/// [`push_inlined_chain`]), or a refusal.
pub(in crate::runtime::interpreter::deopt_resume) fn note_osr_chain_transfer<T>(shared: &SharedVm, outcome: &Result<T, String>) {
    note_inlined_chain_census(
        shared,
        if outcome.is_ok() {
            ChainCensusEvent::OsrInPlace
        } else {
            ChainCensusEvent::Refused
        },
    );
}

/// Count one chain-resume event in this VM's census and, under
/// `CRATONVM_DBG_DEOPT`, print the running totals (the census's reader).
pub(in crate::runtime::interpreter::deopt_resume) fn note_inlined_chain_census(shared: &SharedVm, event: ChainCensusEvent) {
    use std::sync::atomic::Ordering::Relaxed;
    let c = &shared.jit.bridge.inlined_chain_census;
    match event {
        ChainCensusEvent::Pushed { frames } => {
            c.pushed.fetch_add(1, Relaxed);
            // Widening: usize -> u64 (a frame count).
            c.frames.fetch_add(frames as u64, Relaxed);
        }
        ChainCensusEvent::OsrInPlace => {
            c.osr_in_place.fetch_add(1, Relaxed);
        }
        ChainCensusEvent::Refused => {
            c.refused.fetch_add(1, Relaxed);
        }
        ChainCensusEvent::Forced {
            first_at_point,
            unlisted,
        } => {
            c.forced.fetch_add(1, Relaxed);
            if first_at_point {
                c.points_forced.fetch_add(1, Relaxed);
            }
            if unlisted {
                c.forced_unlisted.fetch_add(1, Relaxed);
            }
        }
        ChainCensusEvent::ForcedResumed => {
            c.forced_resumed.fetch_add(1, Relaxed);
        }
        ChainCensusEvent::Uncharged => {
            c.uncharged.fetch_add(1, Relaxed);
        }
    }
    let events = c.events.fetch_add(1, Relaxed).wrapping_add(1);
    // Round 15 wave 5 (lane chain6, CH5W4-2): a soak with first-arrival
    // forcing makes tens of thousands of events; the running line is printed
    // at power-of-two counts only (about 20 lines for a million events), and
    // the final one at exit (`report_inlined_chain_census_at_exit`). A
    // refusal -- the soak's failure signal, zero in a clean run -- still
    // prints at once.
    let refused = matches!(event, ChainCensusEvent::Refused);
    if (events.is_power_of_two() || refused) && dbg_deopt_enabled() {
        eprintln!("{}", c.census_line());
    }
}

/// The operand-stack capacity `Frame::new_pooled` gives a frame whose method
/// declares `max_stack`: `max(max_stack, 16) + 8` slots (the pooled
/// `ValueStack`'s padded size). [`push_inlined_chain`] refills the pools with
/// it and checks every frame against it.
pub(in crate::runtime::interpreter::deopt_resume) fn pooled_frame_stack_capacity(max_stack: u16) -> usize {
    // Widening: small unsigned -> usize (non-negative, fits)
    (max_stack as usize).max(16) + 8
}

/// Does every frame of a chain fit the operand stack its pushed frame will
/// get ([`pooled_frame_stack_capacity`])? Asked before any frame of the chain
/// is pushed (round 16 wave 15, lane trapfix; FS14-5).
pub(in crate::runtime::interpreter::deopt_resume) fn chain_stacks_fit(frames: &[InlinedChainFrame]) -> bool {
    frames
        .iter()
        .all(|f| f.stack.len() <= pooled_frame_stack_capacity(f.cached.max_stack))
}

/// Push a materialised inlined chain, outermost first.
///
/// Separate from [`materialise_inlined_chain`] so the fail-closed split is
/// structural rather than a convention: everything that can refuse happens
/// before this function is called, and this function cannot fail in a way that
/// leaves a partial chain. The only fallible step left is the operand-stack
/// push, and the frame's own `max_stack` was checked against the snapshot
/// during materialisation, so it cannot overflow here.
///
/// Round 16 wave 15 (lane trapfix; FS14-5 of `jit-r16-framestate-proposals-RETIRED-20261006.md`,
/// CH2W3-7 of `jit-r15w3-chain2-proposals-RETIRED-20261003.md`): and that no longer rests on
/// every caller having built its frames through materialisation. Every frame's
/// stack is checked against the operand stack its pushed frame will get
/// ([`pooled_frame_stack_capacity`]) after materialisation and BEFORE the
/// first push ([`chain_stacks_fit`]); a chain that does not fit is refused
/// whole (census `refused=`), and the push loop below is infallible by
/// construction. A stack push that failed half-way used to return `None`
/// with the outer frames already on the thread's stack.
pub(in crate::runtime::interpreter::deopt_resume) fn push_inlined_chain(
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
        // Round 15 wave 3 (lane chain3, CH2W3-1).
        note_inlined_chain_census(shared, ChainCensusEvent::Refused);
        let _ = crate::runtime::interpreter::take_spliced_catch_report();
        return None;
    }
    let pinned_from = chain.virtuals.as_ref().and_then(|v| v.pinned_from);
    // Round 16 wave 15 (lane trapfix; FS14-5): every frame fits, or nothing
    // is pushed. Asked of the materialised stacks, which are the ones pushed;
    // a refusal releases what the materialisation pinned (its objects are
    // garbage then).
    if !chain_stacks_fit(&chain.frames) {
        if let Some(base) = pinned_from {
            if thread.native_pin_roots.len() > base {
                thread.native_pin_roots.truncate(base);
            }
        }
        note_inlined_chain_census(shared, ChainCensusEvent::Refused);
        let _ = crate::runtime::interpreter::take_spliced_catch_report();
        return None;
    }
    let outermost_cp_stamp = chain.outermost_cp_stamp;
    let inner_own_source_frames = std::mem::take(&mut chain.inner_own_source_frames);
    let forced = chain.forced;
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
            pooled_frame_stack_capacity(f.cached.max_stack),
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
        frame.note_method_is_static(f.cached.is_static);
        // Stack and pc BEFORE the push, so every reconstructed oop is in a
        // GC-scanned frame slot the moment the frame exists.
        // Checked before the first push (`chain_stacks_fit`), so no push can
        // fail here and leave the frames above on the thread's stack.
        debug_assert!(f.stack.len() <= pooled_frame_stack_capacity(f.cached.max_stack));
        for v in f.stack {
            frame.stack.push_unchecked(v);
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
        // Interpreter round i6 wave 119, lane L6: as the single-frame
        // rebuild, a scope that names no lock is marked understated where its
        // method can hold one.
        // Wave 121 (lane L6): the producer in the mark, trusted or not.
        if f.monitors.is_empty() {
            crate::runtime::interpreter::held_monitors::clear_untrusted_record(
                &mut frame,
                crate::runtime::interpreter::held_monitors::RecordProducer::InlinedChain,
            );
        } else {
            crate::runtime::interpreter::held_monitors::note_trusted_record(
                &mut frame,
                crate::runtime::interpreter::held_monitors::RecordProducer::InlinedChain,
            );
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
    // Round 15 wave 3 (lane chain3, CH2W3-1).
    note_inlined_chain_census(
        shared,
        ChainCensusEvent::Pushed {
            frames: thread.frames.len().saturating_sub(first_pushed),
        },
    );
    // Round 15 wave 4 (lane chain4): a claimed forced exit, resumed.
    note_forced_chain_resumed(shared, forced, true);
    // JIT r24 (CompletableFuture page, item 1): a spliced handler entered
    // while a catch client may be armed posts its catch here, on the callee
    // frame the handler trap resumed. Every frame is rooted by now, so the
    // callbacks may allocate.
    if let Some(top) = thread.frames.len().checked_sub(1).filter(|&t| t >= first_pushed) {
        crate::runtime::interpreter::report_spliced_catch_if_pending(shared, thread, top);
    } else {
        let _ = crate::runtime::interpreter::take_spliced_catch_report();
    }
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
/// layer — see `docs/feature-designs/real-frame-deopt.md`.
///
/// Pure (no heap deref, no env read) so it is unit-testable; the `CRATONVM_DEOPT_VERIFY`
/// gate is consulted by the caller (`build_deopt_frame_inner`).
pub(in crate::runtime::interpreter) fn verify_reconstructed_frame(
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
pub(in crate::runtime::interpreter) fn verify_reconstructed_oops(
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
pub(in crate::runtime::interpreter::deopt_resume) fn verify_chain_oops(
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
    /// `docs/feature-designs/deopt-osr.md` §5.2. A non-zero count means a
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
    pub(in crate::runtime::interpreter::deopt_resume) const ALL: [DeoptFrameBail; 13] = [
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

pub(in crate::runtime::interpreter::deopt_resume) static DEOPT_FRAME_BAILS: [std::sync::atomic::AtomicU64; 13] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 13];

/// Count one decline, and trace it under `CRATONVM_DBG_DEOPT`.
pub(in crate::runtime::interpreter::deopt_resume) fn note_deopt_frame_bail(why: DeoptFrameBail, rframe: &cratonvm_jit::deopt::ReconstructedFrame) {
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
    /// gcf f3/f: not a re-run either -- an owned-arguments call site handed
    /// its callee's trap to the caller's caller (`helpers::
    /// note_owned_args_handoff`), with the hand-off's replay verdict (`sound`
    /// = the callee replay is exact). Kept OUT of [`door_rerun_census`] (the
    /// re-run the hand-off causes is counted there by the sink that makes
    /// it, as `frameless`); read by [`owned_args_handoff_census`] for its
    /// own one-line exit report, so a row can judge engagement from the END
    /// of a long stderr.
    OwnedArgsHandoff,
}

impl DoorRerunCause {
    pub(in crate::runtime::interpreter::deopt_resume) fn name(self) -> &'static str {
        match self {
            DoorRerunCause::ForeignStash => "foreign-stash",
            DoorRerunCause::OwnStash => "own-stash",
            DoorRerunCause::Frameless => "frameless",
            DoorRerunCause::CalleeDeclined => "callee-declined",
            DoorRerunCause::CalleeFrameless => "callee-frameless",
            DoorRerunCause::CalleeException => "callee-exception",
            DoorRerunCause::Refused => "refused",
            DoorRerunCause::OwnedArgsHandoff => "owned-args-handoff",
        }
    }
}

/// `[cause * 2 + unsound]`, [`DoorRerunCause`] order.
pub(in crate::runtime::interpreter::deopt_resume) static DOOR_RERUNS: [std::sync::atomic::AtomicU64; 16] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 16];

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
        let _ = c.try_update(Relaxed, Relaxed, |n| n.checked_sub(1));
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

/// gcf f3/f: `(exact, unsound)` owned-arguments hand-offs
/// ([`DoorRerunCause::OwnedArgsHandoff`]), cumulative.
pub fn owned_args_handoff_census() -> (u64, u64) {
    let i = (DoorRerunCause::OwnedArgsHandoff as usize) * 2;
    let load = |j: usize| {
        DOOR_RERUNS
            .get(j)
            .map_or(0, |c| c.load(std::sync::atomic::Ordering::Relaxed))
    };
    (load(i), load(i + 1))
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
    let rerun_method = true
    .then_some(rerun_method);
    frameless_rerun_is_exact_of(code, None, site, rerun_method)
}

/// The one body of [`frameless_rerun_is_exact`] and
/// [`frameless_rerun_is_exact_for_callee`].
pub(in crate::runtime::interpreter::deopt_resume) fn frameless_rerun_is_exact_of(
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
pub(in crate::runtime::interpreter::deopt_resume) fn frameless_trap_identity_enabled() -> bool {
    true
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
            true
        }
        DoorRerunCause::CalleeDeclined | DoorRerunCause::CalleeFrameless => {
            true
        }
        DoorRerunCause::CalleeException => {
            true
        }
        // Counted only: the hand-off's re-run is judged by the sink that makes it.
        DoorRerunCause::Refused | DoorRerunCause::OwnedArgsHandoff => false,
    };
    switch_on && !redefined()
}

/// Has no class that `rframe`'s inlined chain names ever been redefined?
///
/// Walked as [`materialise_inner_scopes`] walks it: the compiled method's own
/// class, then each scope outermost-inward, every one resolved by name in the
/// loader context of the scope that encloses it -- so the classes checked are
/// the ones a rebuild would take the bytecode of. A scope that does not resolve
/// answers `false`, as the rebuild would refuse it. A class that was never
/// redefined has run its original bytecode since it was defined, which is the
/// bytecode any body compiled from it, whenever, describes at its bci.
pub(in crate::runtime::interpreter::deopt_resume) fn chain_scope_classes_never_redefined(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    if crate::runtime::redefine_state::class_was_redefined(shared, cached.declaring_class_id) {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] chain not vouched for: {} was redefined",
                cached.class_name
            );
        }
        return false;
    }
    let mut enclosing = cached.declaring_class_id;
    for scope in rframe
        .caller_frames
        .iter()
        .rev()
        .skip(1)
        .chain(std::iter::once(rframe))
    {
        let Ok(scope_method) =
            resolve_inlined_callee_scoped(shared, enclosing, &scope.method_key, scope.class_id, &[])
        else {
            return false;
        };
        enclosing = scope_method.declaring_class_id;
        if crate::runtime::redefine_state::class_was_redefined(shared, enclosing) {
            if dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] chain not vouched for: {} was redefined",
                    scope.method_key
                );
            }
            return false;
        }
    }
    true
}

/// Whether `body` is a compile of `cached`'s method. An inherited method's body
/// is published under its RECEIVER's key (`owner_class_id` names the subclass),
/// so the method's holder is the other candidate (`holder_class_id`); without
/// it a trap inside a splice into `Iterable.forEach` lost its owner and with it
/// the compile's scope-class hints, and the inlined caller chain was refused as
/// "not resolvable from the class that inlined it".
pub(in crate::runtime::interpreter::deopt_resume) fn chain_body_is_of_method(
    body: &cratonvm_jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
) -> bool {
    let declaring = cached.declaring_class_id.as_u32();
    body.owner_class_id == declaring || body.frame_class_id() == declaring
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
    {
        return build_deopt_frame_chain_hinted(shared, cached, rframe, &[]);
    }
    // j27 wave 7 (lane T1LAM3): a `RetainedCode`, whichever source found it
    // (the stash's and the range walk's are upgrades of a `Weak`, so this may
    // be the body's last owner): released through the retirement queue.
    let mut trapped: Option<cratonvm_jit::RetainedCode> = None;
    // Interpreter round i2 wave 60, lane L2
    // (`i59-L2-proposal-every-stash-names-its-trapping-body`): the body the
    // trampoline named at the trap, first. Then the cache, then the range walk.
    let mut named_by_stash = false;
    let mut owner_source = ChainPointOwnerSource::None;
    if point_addr != 0 && cratonvm_jit::deopt::stash_names_trapping_body_enabled() {
        trapped = crate::jit::helpers::stale_trap_not_entrant::taken_trapping_body_retained(
            point_addr,
        )
        .filter(|body| chain_body_is_of_method(body, cached));
        named_by_stash = trapped.is_some();
        if named_by_stash {
            owner_source = ChainPointOwnerSource::Stash;
        }
        if named_by_stash && dbg_deopt_enabled() {
            // The proposal's step 3: the walk must agree wherever both answer.
            let walked = cratonvm_jit::pin_jit_body_owning_deopt_point(point_addr)
                .map(cratonvm_jit::RetainedCode::new);
            let agrees = match (&trapped, &walked) {
                (Some(a), Some(b)) => Arc::ptr_eq(a.arc(), b.arc()),
                _ => false,
            };
            eprintln!(
                "[cratonvm-deopt] chain point owner named by the stash: stash={} bci={} \
                 range-walk-agrees={agrees}",
                rframe.method_key, rframe.bci,
            );
        }
    }
    if point_addr != 0 && !named_by_stash {
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
            .or_else(|| osr.filter(|body| body.owns_deopt_point(point_addr)))
            .map(cratonvm_jit::RetainedCode::new);
        if trapped.is_some() {
            owner_source = ChainPointOwnerSource::Cache;
        }
        // Interpreter round i2 wave 59, lane L2: a body the cache no longer
        // publishes -- withdrawn by a redefinition or an interpreter-only
        // request while a compiled caller still pins it -- whose forced
        // post-call CHAIN exit stashed this point. Without it the chain was
        // refused as "cannot be vouched for" after any redefinition, and the
        // call-site service raised `InternalError: precise deoptimization
        // unavailable` (`L2W59SinglePassPostCallExitInsideASplice`, base,
        // `CRATONVM_C2_SUPERSEDE=0`). Found through the code-range registry,
        // and only a body of this very method.
        if trapped.is_none() {
            trapped = cratonvm_jit::pin_jit_body_owning_deopt_point(point_addr)
                .map(cratonvm_jit::RetainedCode::new)
                .filter(|body| chain_body_is_of_method(body, cached));
            if trapped.is_some() {
                owner_source = ChainPointOwnerSource::Range;
            }
            if trapped.is_some() && dbg_deopt_enabled() {
                eprintln!(
                    "[cratonvm-deopt] chain point owner found by range (the cache no longer \
                     publishes it): stash={} bci={}",
                    rframe.method_key, rframe.bci,
                );
            }
        }
    }
    // TAIL11-3 (JIT round 19 wave 12, lane tail12): the owner census, per
    // build that had a point to look up.
    if point_addr != 0 {
        note_chain_point_owner(shared, owner_source);
    }
    // Round 15 wave 4 (lane chain4): a forced chain exit, claimed before
    // anything below can refuse; the chain carries the answer to its push
    // (`forced-resumed`). Census only. Round 16 lane chain (CH5W4-1): by the
    // stash's own mark, so a trap out of a body the cache no longer publishes
    // (G1 of `r15w4-chain5-review-findings`) is claimed too.
    let forced = claim_forced_chain_trip_at(shared, point_addr);
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
        // No published body owns the point (it was superseded): nothing says
        // when the chain was compiled, so the chain is vouched for only when
        // none of the classes it names was ever redefined. A redefinition of
        // some OTHER class says nothing about this chain's bytecode.
        None => {
            crate::classloading::any_class_redefined()
                && !chain_scope_classes_never_redefined(shared, cached, rframe)
        }
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
                    return Some(chain.with_forced_trip(forced));
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
        // Round 15 wave 3 (lane chain3, CH2W3-1).
        note_inlined_chain_census(shared, ChainCensusEvent::Refused);
        return None;
    }
    let built = match &trapped {
        Some(body) => {
            build_deopt_frame_chain_hinted(shared, cached, rframe, &body.splice_scope_class_ids)
        }
        None => build_deopt_frame_chain_hinted(shared, cached, rframe, &[]),
    };
    built.map(|chain| chain.with_forced_trip(forced))
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
pub(in crate::runtime::interpreter) fn build_deopt_frame_chain_sourced(
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
            // Round 15 wave 3 (lane chain3, CH2W3-1).
            note_inlined_chain_census(shared, ChainCensusEvent::Refused);
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
    pub(in crate::runtime::interpreter::deopt_resume) frames: Vec<InlinedChainFrame>,
    /// The chain's scalar-replaced objects, still to be re-allocated before the
    /// push ([`materialise_chain_virtuals`]); `None` for a chain that names
    /// none, whose `frames` are final. Round 13 wave 5, lane resume.
    pub(in crate::runtime::interpreter::deopt_resume) virtuals: Option<ChainVirtuals>,
    /// The trapping body's constant-pool generation
    /// (`CompiledMethod::compile_cp_stamp`), which the OUTERMOST frame is
    /// restamped with once pushed ([`push_inlined_chain`]); `None` when the
    /// sink did not say (interpreter round i1 wave 37, lane L3).
    pub(in crate::runtime::interpreter::deopt_resume) outermost_cp_stamp: Option<u64>,
    /// Indices within `frames` (outermost = 0) of the inner frames built from
    /// an own-source template ([`chain_inner_scope_own_sources`]), which
    /// [`push_inlined_chain`] restamps with `outermost_cp_stamp` too -- the
    /// sinks that cannot restamp positionally after the push (the tier-up
    /// sink and the call-site service run the chain to completion inside
    /// it). The doors leave `outermost_cp_stamp` `None`, so this is inert for
    /// them and they keep [`restamp_inner_own_source_frames`]. Round 14 wave 4,
    /// lane resume2 (CH3W-3).
    pub(in crate::runtime::interpreter::deopt_resume) inner_own_source_frames: Vec<usize>,
    /// The trap this chain resumes was a forced chain exit a sink claimed
    /// ([`claim_forced_chain_trip`]), so [`push_inlined_chain`] counts it
    /// `forced-resumed` once pushed. Census only. Round 15 wave 4, lane chain4.
    pub(in crate::runtime::interpreter::deopt_resume) forced: bool,
}

impl DeoptFrameChain {
    /// A chain whose frames are final: it names no scalar-replaced object.
    pub(in crate::runtime::interpreter::deopt_resume) fn ready(frames: Vec<InlinedChainFrame>) -> Self {
        Self {
            frames,
            virtuals: None,
            outermost_cp_stamp: None,
            inner_own_source_frames: Vec::new(),
            forced: false,
        }
    }

    /// This chain, resuming a trap [`claim_forced_chain_trip`] answered
    /// `forced` for: the push counts it in the census. For the sinks that run
    /// the chain elsewhere (the call-site service, the first-call tier-up
    /// sink). Round 15 wave 4, lane chain4.
    pub(crate) fn with_forced_trip(mut self, forced: bool) -> Self {
        self.forced = forced;
        self
    }

    /// Does this chain resume a trap [`claim_forced_chain_trip`] answered
    /// `forced` for? For a sink that built it elsewhere and decides the
    /// trap's charge itself (the call-site service,
    /// [`leave_forced_trip_uncharged`]). Round 15 wave 4, lane chain5.
    pub(crate) fn is_forced_trip(&self) -> bool {
        self.forced
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

    /// The opcode the TRAPPING (innermost) scope is parked at: its own bci in
    /// its own method's bytecode (`executing_pc`), `None` past the code.
    /// JIT round 19 wave 12, lane tail12 (TAIL11-1): the call-site service's
    /// division-guard re-tier asks this of a chain, whose outermost `bci` is
    /// an invoke.
    pub(crate) fn trapping_scope_opcode(&self) -> Option<u8> {
        let f = self.frames.last()?;
        let code_len = f.cached.code.len().saturating_sub(2);
        (f.executing_pc < code_len)
            .then(|| f.cached.code.get(f.executing_pc).copied())
            .flatten()
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
pub(in crate::runtime::interpreter::deopt_resume) struct ChainVirtuals {
    pub(in crate::runtime::interpreter::deopt_resume) merged: cratonvm_jit::deopt::ReconstructedFrame,
    /// `(locals, stack)` value counts of each scope in `merged`, in push order
    /// (the same order as [`DeoptFrameChain::frames`]).
    pub(in crate::runtime::interpreter::deopt_resume) shapes: Vec<(usize, usize)>,
    /// `native_pin_roots` length before the re-allocation, once it ran: the
    /// shells and re-read references stay pinned above it until the chain is
    /// pushed ([`push_inlined_chain`] releases them).
    pub(in crate::runtime::interpreter::deopt_resume) pinned_from: Option<usize>,
}

/// Does any scope of `rframe`'s chain (locals, stack or a monitor object)
/// name a scalar-replaced object?
pub(in crate::runtime::interpreter::deopt_resume) fn reconstructed_chain_names_virtual(rframe: &cratonvm_jit::deopt::ReconstructedFrame) -> bool {
    use cratonvm_jit::deopt::FrameValue;
    let names = |f: &cratonvm_jit::deopt::ReconstructedFrame| {
        f.locals
            .iter()
            .chain(f.stack.iter())
            .chain(f.monitors.iter().map(|m| &m.object))
            .any(|v| {
                matches!(
                    v,
                    FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
                )
            })
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
///   Under `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS` it did, until round 16 wave
///   14 (lane framestate) taught it this refusal
///   (`cratonvm_jit::deopt`'s `sync_chain_virtuals_refused`, kill switch
///   `CRATONVM_JIT_IR_SYNC_CHAIN_VIRTUALS_REPLAY_CHECK`).
pub(in crate::runtime::interpreter::deopt_resume) fn chain_virtuals_refusal(
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
pub(in crate::runtime::interpreter::deopt_resume) fn with_virtual_placeholders(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> cratonvm_jit::deopt::ReconstructedFrame {
    use cratonvm_jit::deopt::FrameValue;
    fn blank(values: &mut [FrameValue]) {
        for v in values.iter_mut() {
            if matches!(
                v,
                FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
            ) {
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
pub(in crate::runtime::interpreter::deopt_resume) fn merge_chain_scopes(rframe: &cratonvm_jit::deopt::ReconstructedFrame) -> ChainVirtuals {
    let mut merged = cratonvm_jit::deopt::ReconstructedFrame {
        class_id: 0,
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

/// The for-each residual round (2026-10-06): `rframe` with the scalar-replaced
/// objects of every scope re-allocated, for the planless in-place OSR exit
/// transfers ([`transfer_osr_guard_exit_into_live_frame`], flat and chain).
///
/// One materialiser call over the merged scopes ([`merge_chain_scopes`], ids
/// are chain-wide), as [`materialise_chain_virtuals`] makes for the deopt-exit
/// chain sinks: every ordinary reference of every scope is pinned before the
/// first allocation and re-read after the last, so every `Object` word of the
/// returned frame is current. The pins stay installed above the returned base:
/// the caller writes the live frame (and pushes any inner scope) with nothing
/// that can allocate in between, then truncates `native_pin_roots` to it. On
/// `Err` nothing is pinned; [`DeoptFrameBail::VirtualMaterialiseHeap`] means
/// the heap is exhausted and a collection ran, so `rframe`'s own `Object` words
/// may be stale.
pub(in crate::runtime::interpreter::deopt_resume) fn materialise_osr_exit_virtuals(
    shared: &SharedVm,
    thread: &mut JvmThread,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> Result<(cratonvm_jit::deopt::ReconstructedFrame, usize), DeoptFrameBail> {
    let mut virtuals = merge_chain_scopes(rframe);
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
    // Rust-side only from here: no collection can run before the caller's
    // writes.
    let mut out = rframe.clone();
    let callers = out.caller_frames.len();
    let (mut l, mut s) = (0usize, 0usize);
    for (k, &(nl, ns)) in virtuals.shapes.iter().enumerate() {
        let (Some(locals), Some(stack)) = (
            virtuals.merged.locals.get(l..l + nl),
            virtuals.merged.stack.get(s..s + ns),
        ) else {
            // Unreachable: the materialiser only rewrites slots in place.
            thread.native_pin_roots.truncate(base);
            note_deopt_frame_bail(DeoptFrameBail::VirtualMaterialise, &virtuals.merged);
            return Err(DeoptFrameBail::VirtualMaterialise);
        };
        // Push order: the outermost caller scope first, the trapping one last.
        let scope = if k < callers {
            &mut out.caller_frames[callers - 1 - k]
        } else {
            &mut out
        };
        scope.locals = locals.to_vec();
        scope.stack = stack.to_vec();
        l += nl;
        s += ns;
    }
    Ok((out, base))
}

/// The answer of [`materialise_osr_exit_virtuals`]'s refusal for the OSR
/// door: a heap failure sets `heap_exhausted`, and the door raises
/// `OutOfMemoryError` out of the activation
/// ([`rematerialisation_out_of_memory`]), as HotSpot pops the frames of a
/// failed reallocation; anything else refuses.
pub(in crate::runtime::interpreter::deopt_resume) fn osr_exit_materialise_refusal(why: DeoptFrameBail, heap_exhausted: &mut bool) -> String {
    if why == DeoptFrameBail::VirtualMaterialiseHeap {
        *heap_exhausted = true;
    }
    format!(
        "re-allocating its scalar-replaced objects failed ({})",
        why.name()
    )
}

/// [`materialise_inlined_chain`] for a chain that names scalar-replaced
/// objects: every check that can refuse runs now, over placeholders; the
/// objects are re-allocated by [`materialise_chain_virtuals`] right before
/// the push. Nothing is pinned or allocated here.
pub(in crate::runtime::interpreter::deopt_resume) fn chain_with_virtuals(
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
        forced: false,
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
                    class_id: 0,
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
///
/// gci i14/sx: read from the VM (`SharedVm::deopt_chain_run_from_outermost`,
/// taken once at construction), the same answer
/// `interpreter::run_pushed_frame_to_completion` reads, so the two sites
/// cannot disagree within one VM.
pub(in crate::runtime::interpreter::deopt_resume) fn chain_runs_from_its_outermost_frame(shared: &SharedVm) -> bool {
    shared.deopt_chain_run_from_outermost
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
pub(in crate::runtime::interpreter::deopt_resume) fn run_pushed_chain_to_completion(
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
/// `docs/feature-designs/deopt-osr.md`).
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
    let _vthread_pin = vthread_resume_pin(shared, thread);
    let trace = dbg_deopt_enabled();
    // Round 13 wave 11 (lane chain5): the full-heap answer of round 13 wave 8
    // (lane chain3) -- `OutOfMemoryError` at the caller, as for a single frame
    // -- lived in the test-only twin above since gcd d1/b moved both
    // production sinks here; `push_inlined_chain`'s own re-allocation answered
    // `None`, which both sinks turn into an `InternalError` ("could not push a
    // chain"). A no-op for a chain that names no scalar-replaced object
    // (`CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` off: every chain).
    if let Err(why) = materialise_chain_virtuals(shared, thread, &mut chain) {
        // Round 15 wave 4 (lane chain4): a chain the sink asked for and did
        // not push, as `push_inlined_chain`'s own re-allocation counts it
        // (this path returned before the push, uncounted).
        note_inlined_chain_census(shared, ChainCensusEvent::Refused);
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
    if !chain_runs_from_its_outermost_frame(shared) {
        return Some(crate::runtime::interpreter::run_pushed_frame_to_completion(
            shared,
            thread,
            depth_before,
        ));
    }
    Some(run_pushed_chain_to_completion(shared, thread, depth_before))
}

/// Pin a JDK virtual thread's continuation while a frame rebuilt from a
/// compiled activation runs to completion under a Rust sink (interpreter
/// round i5 wave 101, lane L2, `CRATONVM_VTHREAD_COMPILED_LEARNED`).
///
/// Those sinks hand the resumed method's result (or exception) back to the
/// compiled door or helper that called them, and some still have work after
/// it -- releasing a stash's locks on a propagation, a door re-run verdict --
/// so a `ContinuationYield` unwinding through one would freeze a stack whose
/// remount does not continue where the sink left off. With the switch on,
/// virtual threads run arbitrary compiled bodies, whose deopts land here, so
/// a park inside the resumed run must pin (`continuation_pinned_by_native`
/// reads the pin) and take the carrier compensation
/// (`jdk_continuation::vm_pinned_park_compensates`). `None` -- nothing held --
/// for every other thread, and with the switch off.
#[inline]
pub(in crate::runtime::interpreter::deopt_resume) fn vthread_resume_pin(
    shared: &SharedVm,
    thread: &JvmThread,
) -> Option<cratonvm_native_api::continuation_pin::PinGuard> {
    let pin = shared.config.vthread_compiled_learned
        && matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && thread.jdk_continuation.jdk();
    if pin {
        // Interpreter round i6 wave 117, lane L6: the `deopt_resume_pins`
        // census row (`CRATONVM_DBG_VTHREAD_CENSUS`; one config load off).
        crate::vm::jdk_continuation::note_deopt_resume_pin(shared);
    }
    pin.then(crate::runtime::native_callee_memo::pin_continuation)
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
    let _vthread_pin = vthread_resume_pin(shared, thread);
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
    let _vthread_pin = vthread_resume_pin(shared, thread);
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
    let _vthread_pin = vthread_resume_pin(shared, thread);
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
    let _vthread_pin = vthread_resume_pin(shared, thread);
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
pub(in crate::runtime::interpreter::deopt_resume) fn compiled_from_the_current_bytecode(
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
pub(in crate::runtime::interpreter) fn stamp_compiled_source_of(
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
pub(in crate::runtime::interpreter) fn obsolete_activation_source(
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
    super::obsolete_frames::rebuilt_body_translates(
        shared,
        source.declaring_class_id,
        &source.code,
        &source.exception_table,
        stamp,
    )
    .then_some((source, stamp))
}

/// Round 14 wave 3 (lane resume; proposal R14DP-1 of `jit-r14-deopt-proposals-RETIRED-20260930.md`):
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
    let _vthread_pin = vthread_resume_pin(shared, thread);
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
pub(in crate::runtime::interpreter::deopt_resume) fn resume_current_body_of_redefined_class() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Must a sink that built with `release_held_on_heap_failure` answer this
/// refusal with [`rematerialisation_out_of_memory`] rather than a re-run? True
/// exactly when the build released the compiled code's locks for it.
pub(crate) fn frame_build_heap_failure_is_answered(why: DeoptFrameBail) -> bool {
    why == DeoptFrameBail::VirtualMaterialiseHeap
}

/// Is `failed` the allocator's heap-exhaustion answer (the shape
/// `alloc_object_shared` / `gc_alloc_array` return once their collect-and-retry
/// ladder has failed)? After `validate_virtual_graph` it is the only way
/// `materialize_virtual_objects` can still fail.
pub(in crate::runtime::interpreter::deopt_resume) fn materialisation_failed_for_heap(failed: &MethodCallFailed) -> bool {
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
            message: "Java heap space: failed reallocation of scalar replaced objects".to_string(),
        },
    )
}

/// Pin every monitor the COMPILED code took (`relock == false`, a real object)
/// so a failed re-materialisation, whose allocator collected, can still name
/// them afterwards. Returns the pin base.
pub(in crate::runtime::interpreter::deopt_resume) fn pin_compiled_held_monitors(
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
pub(in crate::runtime::interpreter::deopt_resume) fn release_compiled_held_monitors(
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
    shared: &SharedVm,
    declaring_class_id: crate::classloading::ClassId,
    is_synchronized: bool,
    is_static: bool,
    code: &[u8],
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    handed_method_monitor_at(
        shared,
        declaring_class_id,
        is_synchronized,
        is_static,
        code,
        rframe,
    )
    .is_some()
}

/// Does THIS thread hold the method monitor of the synchronized method
/// `rframe` is a frame of (its receiver, local 0, or its class mirror),
/// while the frame names no hold of it ([`frame_hands_over_method_monitor`]
/// answered no)? Then a compiled caller holds it around a caller-held
/// synchronized `CALL` -- the one route into such a body that pushes no door
/// guard, and no self-locking body leaves without naming its hold -- and the
/// frame may be resumed releasing nothing of it: the caller's own edge
/// releases it once (shape 2 (c) of the i41 page;
/// `cratonvm_jit::caller_held_callee_resume_enabled`). `false` with the
/// switch off.
pub(crate) fn caller_holds_method_monitor(
    shared: &SharedVm,
    thread: &JvmThread,
    is_static: bool,
    declaring_class_id: crate::classloading::ClassId,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    if !cratonvm_jit::caller_held_callee_resume_enabled() {
        return false;
    }
    let lock = if is_static {
        shared
            .classes
            .class_mirrors
            .read()
            .get(&declaring_class_id)
            .copied()
    } else {
        match rframe.locals.first() {
            // SAFETY: a reconstructed frame's `Object` word is a live
            // reference the stash rooted.
            Some(cratonvm_jit::deopt::FrameValue::Object(addr)) if *addr != 0 => {
                Some(unsafe { ObjectRef::from_raw(*addr as usize as *mut u8) })
            }
            _ => None,
        }
    };
    let Some(lock) = lock else {
        return false;
    };
    // JIT round 19 proposal wave 6, lane sync6 (REVIEW6-1; unconditional,
    // fail-closed): when the frame itself names a taken lock on the same
    // object, this thread's ownership may be the frame's OWN block, which the
    // frame releases before it returns. Resuming it "releasing nothing" would
    // then drop the count to 0 halfway through the synchronized body. A
    // caller-held CALL's callee names no monitor on that object, so the shape
    // this function exists for keeps its answer; any other frame is refused
    // and re-run as before.
    if frame_names_a_taken_lock_on(rframe, lock) {
        return false;
    }
    shared.threads.monitors.current_owner(lock) == Some(thread.thread_id)
}

/// REVIEW6-1: does any scope of `rframe` name a lock its compiled code TOOK
/// (`relock == false`) on `lock`? Elided levels (`relock == true`) are not
/// held by the thread, so they cannot account for its ownership.
pub(in crate::runtime::interpreter::deopt_resume) fn frame_names_a_taken_lock_on(
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    lock: ObjectRef,
) -> bool {
    // Cast: object pointer to the raw word a stash carries.
    let word = lock.as_ptr() as usize as u64;
    let names = |monitors: &[cratonvm_jit::deopt::MonitorInfo]| {
        monitors.iter().any(|m| {
            !m.relock && matches!(m.object, cratonvm_jit::deopt::FrameValue::Object(a) if a == word)
        })
    };
    names(&rframe.monitors)
        || rframe
            .caller_frames
            .iter()
            .any(|scope| names(&scope.monitors))
}

