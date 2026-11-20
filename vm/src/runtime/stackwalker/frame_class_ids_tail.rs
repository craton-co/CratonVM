#[allow(unused_imports)]
use super::*;

/// The innermost `want` entries of [`frame_class_ids_with_compiled`]'s answer
/// (same INNERMOST-first order), and whether the result is the WHOLE answer.
///
/// SBR wave 6 (lane dispatch): reflection's caller resolution
/// (`lang_class::resolve_caller_class_id`) reads the first frame that is not
/// reflection plumbing -- one to three frames from the top -- off a walk of the
/// WHOLE stack, twice per non-public `newInstance`/`invoke`. The layout is the
/// same concatenation [`capture_tail_trace`] relies on (interpreter frame `i`,
/// then the compiled entries pushed at depth `i + 1`), so the suffix from frame
/// `start` needs only the compiled entries at depth `start + 1` or deeper, and
/// the window grows (ending at the whole stack) until it holds `want` ids.
pub fn frame_class_ids_tail(frames: &[Frame], want: usize) -> (Vec<ClassId>, bool) {
    let mut window = want.saturating_mul(2).max(8);
    loop {
        let start = frames.len().saturating_sub(window);
        let whole = start == 0;
        let min_depth = if whole {
            0
        } else {
            u32::try_from(start + 1).unwrap_or(u32::MAX)
        };
        let jit = crate::jit::stack_trace::active_compiled_frames_bounded(
            min_depth,
            if whole { usize::MAX } else { window },
        );
        let mut out: Vec<ClassId> = if jit.is_empty() {
            frames[start..].iter().rev().map(|f| f.class_id).collect()
        } else {
            interleave_class_ids(
                frames,
                start,
                jit,
                crate::runtime::interpreter::jit_bridge::live_osr_continuation_artifact,
            )
        };
        if whole || out.len() >= want {
            out.truncate(want);
            return (out, whole);        }
        window = window.saturating_mul(4);
    }
}

/// `frames[start..]` and the compiled entries `jit` pushed from them, as class
/// ids, INNERMOST first: interpreter frame `i`, the inlined levels of its
/// dropped OSR continuation, then the compiled entries pushed at depth `i + 1`.
/// The one layout [`frame_class_ids_with_compiled`] and [`frame_class_ids_tail`]
/// share; `osr_artifact` is [`drop_osr_continuations_with`]'s registry.
///
/// An OSR continuation [`drop_osr_continuations`] drops on its authoritative
/// arm is the frame's own compiled half, and the callees it INLINED at the
/// current program point are real frames between the interpreter frame and
/// whatever that half called. The display trace gets them through the frame's
/// override; these walks dropped them, so a `@CallerSensitive` method called
/// from a body inlined into an OSR'd loop (`MethodHandles.lookup()` from
/// `Friend.look()` inlined into `main`) was answered the loop's class
/// (probe `tools/probes/LookupClassInlined.java`). The levels follow [`push_inlined_class_ids`]'s rule (exact chain only).
pub(in crate::runtime::stackwalker) fn interleave_class_ids(
    frames: &[Frame],
    start: usize,
    jit: Vec<ActiveCompiledFrame>,
    osr_artifact: impl Fn(usize) -> Option<usize>,
) -> Vec<ClassId> {
    let mut osr_halves = OsrHolders::new();
    let (jit, _osr_bci) =
        drop_osr_continuations_with_holders(frames, jit, osr_artifact, Some(&mut osr_halves));
    let mut out: Vec<ClassId> =
        Vec::with_capacity(frames.len().saturating_sub(start) + jit.len());
    let mut next = 0usize;
    for (i, f) in frames.iter().enumerate().skip(start) {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled_class_ids(&mut out, &jit[next]);
            next += 1;
        }
        out.push(f.class_id);
        if let Some((_, half)) = osr_halves.iter().find(|(at, _)| *at == i) {
            push_inlined_class_ids(&mut out, half);
        }
    }
    for slot in &jit[next..] {
        push_compiled_class_ids(&mut out, slot);
    }
    out.reverse();
    out
}

/// Is the innermost entry of [`frame_class_ids_tail`]'s answer the class of
/// the code that is actually running there? An interpreter frame always is.
/// A compiled activation is when its artifact inlined nothing (its owner is
/// the running method), or when its inline chain at this program point is the
/// exact one and every level names its class; anything else -- a chain from
/// the coarse safepoint-id key, a hole, an inlining artifact that recorded no
/// chain here -- may report a JDK method that inlined a user method's code.
///
/// Interpreter round i3 (page
/// `i72-L4-a-bootstrap-callers-converting-invoke-resolves-its-site-through-the-system-loader`):
/// `mh_site_method_type` resolves a boot-loader caller's call-site type with
/// the boot loader only when this answers `true`; otherwise it keeps the
/// system loader, which also sees every class the boot loader sees.
pub fn innermost_frame_class_exact(frames: &[Frame]) -> bool {
    let depth = u32::try_from(frames.len()).unwrap_or(u32::MAX);
    let jit = crate::jit::stack_trace::active_compiled_frames_bounded(depth, 1);
    let Some(top) = jit.last() else {
        return true;
    };
    if top.cm_ptr == 0 {
        return false;
    }
    // SAFETY: the same window and contract as `apply_npe_trap_site`: this runs
    // on the owning thread inside a native the compiled activation called, so
    // its artifact is still registered and its chain entry still pushed, and
    // `cm_ptr` came from that walk moments ago.
    let cm = unsafe { &*(top.cm_ptr as *const cratonvm_jit::CompiledMethod) };
    if cm.inlined_methods.is_empty() && cm.inline_frame_map.is_empty() {
        return true;
    }
    let chain = top.chain();
    !chain.is_empty()
        && top.chain_exact
        && inline_caller_frames_enabled()
        && chain.iter().all(|level| level.class_id != 0)
}

/// Kill switch for the inlined levels in [`frame_class_ids_with_compiled`].
///
/// Default ON. `CRATONVM_JIT_NO_INLINE_CALLER_FRAMES=1` restores the flat
/// one-entry-per-artifact answer this walk gave before 2026-09-02, so a
/// caller-attribution verdict that changes after warm-up can be attributed to
/// this expansion or to something else inside ONE binary. Separate from
/// `CRATONVM_JIT_NO_INLINE_FRAME_MAP`, which kills the producer and so takes
/// the DISPLAY frames with it: this is the security-relevant half and is the
/// one worth being able to revert alone.
pub(in crate::runtime::stackwalker) fn inline_caller_frames_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        None::<std::ffi::OsString>.is_none()    })}

/// One compiled entry as one or more `ClassId`s: the artifact's own owner,
/// then the classes of the callees it INLINED at this program point, outermost
/// level first -- the same order and the same kept set
/// [`push_compiled_frames`] produces for the display path.
///
/// Refuses in three places, each of which leaves the flat answer this walk gave
/// before and never substitutes a guess:
///
///   * the switch is off;
///   * the chain came from the coarse safepoint-id key
///     (`!chain_exact`), which is shared with the inline cache's miss edge;
///   * a level carries `class_id == 0`, i.e. no id was recorded. That ends the
///     expansion INCLUDING the levels below it, because a hole re-parents
///     everything deeper.
pub(in crate::runtime::stackwalker) fn push_compiled_class_ids(out: &mut Vec<ClassId>, slot: &ActiveCompiledFrame) {
    out.push(ClassId::new(slot.owner_class_id));
    push_inlined_class_ids(out, slot);
}

/// [`push_compiled_class_ids`] without the owner: only the inlined levels.
pub(in crate::runtime::stackwalker) fn push_inlined_class_ids(out: &mut Vec<ClassId>, slot: &ActiveCompiledFrame) {
    if slot.chain().is_empty() || !slot.chain_exact || !inline_caller_frames_enabled() {
        return;
    }
    // The chain is innermost-first; `out` is outermost-first.
    for level in slot.chain().iter().rev() {
        if level.class_id == 0 {
            break;
        }
        out.push(ClassId::new(level.class_id));
    }
}

/// Kill switch for [`drop_osr_continuations`]. Default ON;
/// `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1` restores the duplicate, which is what
/// makes the frame-count difference an A/B inside one binary.
pub(in crate::runtime::stackwalker) fn osr_frame_dedupe_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        None::<std::ffi::OsString>.is_none()    })}

/// Drop compiled entries that are the SAME ACTIVATION as an interpreter frame.
///
/// An OSR transfer hands control to compiled code part-way through a method
/// that is already running, and the interpreter's `Frame` for it stays on
/// `thread.frames`. Both halves then describe one activation, and the trace
/// reported it twice — `SWCross` read 69 frames where HotSpot reads 68, its
/// first two entries both `main`.
///
/// The decider is **`can_osr_enter` on the frame this entry was pushed from**.
/// An OSR transfer leaves the interpreter frame parked at the BACK-EDGE it
/// jumped from, which is by construction one of that artifact's OSR entry
/// points. An interpreted caller of the same method is parked at an INVOKE,
/// which is not. All three conditions are required:
///
///   * `frames[interp_depth - 1]` exists (the frame the entry was pushed from),
///   * it names the same class, method and descriptor, and
///   * `cm.can_osr_enter(frame.pc)`.
///
/// **Two discriminators that look right and are not** — both measured wrong on
/// `probes/SWCross.java`, both cost a build:
///
///   * `compiled_via_osr`. The OSR-entered `main` carries `false`: the flag
///     records how the ARTIFACT was PRODUCED, not how this activation was
///     ENTERED, and `jit_bridge`'s own comment says a first-call/upgrade
///     artifact can OSR-enter too. Gating on it makes this function inert.
///   * `interp_depth` indexing a live frame. An OSR entry records
///     `interp_depth == frames.len()` exactly like an ordinary call, so the
///     depth alone separates nothing.
///
/// The third condition is what keeps self-recursion safe: an interpreted
/// `foo` calling a compiled `foo` satisfies the first two, and dropping that
/// frame would undo the nested-activation walk this file's sibling fix
/// restored.
///
/// # The registry makes rule 1 authoritative (2026-09-01)
///
/// `cm.can_osr_enter(frame.pc)` is a property of a **pc**, not of an
/// **activation**. An interpreted frame genuinely parked on a back-edge while
/// a RECURSIVE compiled activation of the same method is live satisfies it
/// too, and that activation's compiled entry was then dropped from the trace —
/// a real frame lost, the one direction this function is otherwise careful
/// never to fail in.
///
/// `jit_bridge::live_osr_continuation_artifact` closes that hole. The OSR
/// entry site publishes the `(interp_depth, artifact)` pair for exactly the
/// window the artifact is on the stack, from the same `thread.frames.len()`
/// that becomes the chain entry's `interp_depth` — so the frame index is
/// `depth - 1` and the artifact test is a plain `==` against the `cm_ptr` the
/// carrier already holds. When it answers, it is the authority on which
/// compiled entry is this frame's own body, and no pc is consulted.
///
/// `None` from it means **"no information"**, never "this frame is
/// interpreted": it is equally what `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` and a
/// contended borrow report. The pc heuristic therefore stays as the fallback,
/// and that is what keeps the whole change an A/B inside ONE binary — with
/// that switch set, this decision and the bci override below both revert to
/// exactly the pre-2026-09-01 answer.
///
/// # The ordinary compiled activation — rule 2 (2026-09-01)
///
/// An OSR continuation is not the only way one activation ends up with both
/// halves. With `CRATONVM_JIT_NO_INLINE=1` the witness in
/// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`
/// reports `leaf` twice — once as an interpreter frame with a line, once as a
/// compiled frame — and `can_osr_enter` says nothing about it, because that
/// frame is not parked at a back-edge.
///
/// **The proof that the two are one activation.** The interpreter transfers
/// control to another Java method in exactly one way: by executing an
/// `invoke*` opcode (`dispatch_static` / `dispatch_virtual` /
/// `dispatch_special` are the only callers of `execute_jit_call` and
/// `execute_jit_call_decoded`, and each is reached from its opcode arm). While
/// its callee runs, that frame's `last_instr_pc` names the instruction it is
/// suspended in — this is the field's whole purpose ("`pc` may have already
/// been advanced past the invoke instruction"), it is what `entry_from_frame`
/// reports as the frame's bci, and the witness confirms it: every interpreted
/// caller in the correct trace carries its CALL SITE's line. So an interpreter
/// frame whose `last_instr_pc` does NOT hold an invoke opcode cannot be the
/// caller of anything. If it names the same class, method and descriptor as a
/// compiled entry recorded at its own depth, the only remaining reading is
/// that the compiled entry IS that frame's body.
///
/// This is the same argument `can_osr_enter` makes, stated over the opcode
/// rather than over one artifact's OSR entry list, so it also covers a
/// continuation whose back-edge is not an entry point of *this* artifact.
///
/// **Direction of failure.** A frame we cannot read (a `last_instr_pc` outside
/// its own `code`) is treated as a caller, so the compiled entry survives —
/// the historical behaviour, an extra frame rather than a missing one.
///
/// `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1` turns this half off on its own, so it
/// can be separated from the OSR half inside ONE binary; the existing
/// `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1` still turns off both.
///
/// # How the two rules compose
///
/// They share ONE `(depth, label)` ledger, not one each. Both answer the same
/// question — "is this compiled entry the interpreter frame's own body?" — and
/// a frame has exactly one body, so between them they may remove at most one
/// entry per `(depth, label)`. Mutual recursion through compiled code
/// (`foo` -> `bar` -> `foo`) puts two `foo` activations at one depth; with a
/// ledger per rule the OSR rule could take the first and the call rule the
/// second, and the trace would lose a real frame — the failure this function
/// exists to avoid, reintroduced by the fix for it.
///
/// First-wins is the right tie-break because entries sharing a depth arrive
/// OUTERMOST-first (`active_compiled_frames` reverses its innermost-first walk
/// before pushing) and the activation the interpreter frame describes is the
/// outermost one. That ordering also means the continuation is reached before
/// any nested entry at its depth, so the ledger cannot cost it its override;
/// if it ever were reached second, the loss is an extra frame and a stale
/// line, i.e. exactly the historical answer.
///
/// When the registry answers with a DIFFERENT artifact, rule 2 is SKIPPED
/// rather than consulted. Its premise is that a frame not suspended at an
/// invoke cannot be the caller of anything, so a compiled entry at its depth
/// must be its body. An OSR'd frame is parked on a back-edge, so it is not at
/// an invoke either — the premise holds and the conclusion does not, because
/// the caller of that entry is the frame's own compiled body rather than the
/// frame. Letting rule 2 run there would drop the genuine nested activation
/// rule 1 had just declined to drop.
///
/// # The overrides
///
/// Dropping the duplicate is only half the job. That back-edge is also the
/// STALE position the surviving interpreter frame will be reported at: once
/// control transfers to compiled code its `Frame.pc` stops advancing, so every
/// later trace names the loop the method tiered up in rather than where it
/// actually is — `main:62`, the back-edge, where HotSpot says `main:66`, the
/// live call site. The continuation being dropped is the half that knows, so
/// its safepoint bci — and the inline chain recorded at that same program
/// point — are handed to the frame that survives.
///
/// It is deliberately an override applied during capture rather than a write
/// to `Frame::pc`. Two independent reasons, either one sufficient (both
/// established in the block comment above `jit_bridge::osr_pc_refresh_enabled`,
/// which is where the full argument lives):
///
///   * `Frame::live_locals_mask_here` and `Frame::scan_local_objects_inner`
///     compute the per-bci live-locals ROOT FILTER from
///     `[self.pc, self.last_instr_pc]`. Advancing `pc` to where compiled code
///     really is would make every slot that dies in between stop being a GC
///     root — on a frame whose locals are the pre-OSR copies the conservative
///     half of the JIT root scan is leaning on.
///   * The OSR safe-reject exit is correct only BECAUSE `frame.pc` is still
///     `entry_pc`. Moving it would resume the interpreter at a bci this
///     activation never reached.
///
/// So the override reaches the trace assembler and nothing else. No resume
/// path and no root scan can observe it, because it is never stored.
///
/// Only the AUTHORITATIVE arm produces one. The heuristic arm still drops the
/// entry, exactly as it did before, but it does not move the bci: its drop is
/// an inference about which activation the frame is, and a line carried across
/// on an inference is the confidently-wrong answer this file refuses
/// everywhere else. That is also what makes the kill switch a two-arm A/B
/// rather than a half-revert — `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` restores
/// both the old decision and the old line, in one binary.
///
/// The inline chain rides with the bci, and only with it: both are claims
/// about the SAME program point, so a chain without the bci that placed it
/// would be a claim this arm has not established.
///
/// Rule 2's drops deliberately produce NO override. That the surviving
/// interpreter frame's pc is stale was established and measured for the OSR
/// case; for an ordinary compiled activation it has not been, and giving a
/// frame a line on an unverified premise is the one outcome this file rules
/// out everywhere else. It is a one-line addition here if the evidence ever
/// arrives.
pub(crate) fn drop_osr_continuations(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
) -> (Vec<ActiveCompiledFrame>, OsrBciOverrides) {
    drop_osr_continuations_with(
        frames,
        jit,
        crate::runtime::interpreter::jit_bridge::live_osr_continuation_artifact,
    )
}

/// JIT round 19 wave 12, lane gc12 (GC1119-1, `CRATONVM_JMX_OSR_BODY_BLOCK_HOLDS`):
/// the OSR continuations rule 1 dropped on its AUTHORITATIVE arm, each with
/// the index of the interpreter frame it continues -- the activation whose
/// `synchronized` blocks taken after the transfer no frame records
/// (`held_monitors::osr_body_block_words`).
pub(crate) type OsrHolders = Vec<(usize, ActiveCompiledFrame)>;

/// [`drop_osr_continuations`], also answering the dropped OSR continuations
/// ([`OsrHolders`]) when `keep` (GC1119-1); an empty list otherwise.
pub(crate) fn drop_osr_continuations_keeping(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    keep: bool,
) -> (Vec<ActiveCompiledFrame>, OsrBciOverrides, OsrHolders) {
    let mut holders = OsrHolders::new();
    let (kept, overrides) = drop_osr_continuations_with_holders(
        frames,
        jit,
        crate::runtime::interpreter::jit_bridge::live_osr_continuation_artifact,
        keep.then_some(&mut holders),
    );
    (kept, overrides, holders)
}

/// [`drop_osr_continuations`] with the OSR-continuation registry of the
/// thread whose `frames` these are asked through `osr_artifact` -- the
/// calling thread's own, or a held thread's (interpreter round i2 wave 60,
/// lane L6; [`capture_trace_of_held_thread`]).
pub(in crate::runtime::stackwalker) fn drop_osr_continuations_with(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    osr_artifact: impl Fn(usize) -> Option<usize>,
) -> (Vec<ActiveCompiledFrame>, OsrBciOverrides) {
    drop_osr_continuations_with_holders(frames, jit, osr_artifact, None)
}

/// [`drop_osr_continuations_with`]; with `holders`, a copy of each OSR
/// continuation rule 1 drops on its authoritative arm goes there with its
/// frame's index (GC1119-1, round 19 wave 12, lane gc12).
pub(in crate::runtime::stackwalker) fn drop_osr_continuations_with_holders(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    osr_artifact: impl Fn(usize) -> Option<usize>,
    mut holders: Option<&mut OsrHolders>,
) -> (Vec<ActiveCompiledFrame>, OsrBciOverrides) {
    if !osr_frame_dedupe_enabled() {
        return (jit, OsrBciOverrides::default());
    }
    // JIT round 16 wave 7 (lane throwpath): both rules below start by asking
    // whether the entry names the frame it was pushed from, and an entry that
    // does not is kept with no other effect. When none does -- every capture
    // outside an OSR'd loop or a recursive call -- the answer is `jit` as it
    // came, so it is handed back instead of moved into a second `Vec`.
    if !jit
        .iter()
        .any(|f| compiled_frame_names_its_caller(frames, f))
    {
        return (jit, OsrBciOverrides::default());
    }
    let call_dedupe = call_frame_dedupe_enabled();
    // The `(depth, label)` pairs that have already had their one body entry
    // removed, by EITHER rule. See "How the two rules compose" above for why
    // this is shared rather than one ledger per rule.
    let mut deduped: Vec<(u32, Arc<str>)> = Vec::new();
    let mut overrides = OsrBciOverrides::default();
    // An owning loop rather than `filter(|f| ..)` over borrowed entries: an
    // entry this drops gives its `label` to the ledger and its `inline_chain`
    // to the overrides by MOVE. Cloning them was one allocation per inlined
    // level on every capture made from inside an OSR'd loop -- for
    // `new SomeException(msg)` that is the whole `<init>` chain, which the
    // fill-frame trim then discards anyway.
    let mut kept = Vec::with_capacity(jit.len());
    for f in jit {
        // `None`: keep. `Some(i)`: drop -- rule 1 when `i` names the frame the
        // continuation belongs to, rule 2 otherwise (see the two arms).
        let verdict: Option<Option<usize>> = 'decide: {
            // The frame this entry was pushed FROM. An OSR continuation was
            // pushed from the very frame it continues, so that frame is still
            // there and names the same method.
            let Some(i) = (f.interp_depth as usize).checked_sub(1) else {
                break 'decide None;
            };
            let Some(frame) = frames.get(i) else {
                break 'decide None;
            };
            if !label_names_frame(&f.label, frame) {
                break 'decide None;
            }
            if f.cm_ptr == 0 {
                break 'decide None;
            }
            // SAFETY: the pointer came from a chain entry whose frame is live
            // on this thread's stack, so the JIT cache still owns the `Arc`;
            // this read happens on the owning thread during that same capture.
            let cm = unsafe { &*(f.cm_ptr as *const cratonvm_jit::CompiledMethod) };
            // Rule 1 — the OSR continuation. The entry site knows the
            // `(frame, artifact)` pairing outright; the pc shape is only what
            // is left when it has nothing to say. An OSR transfer leaves the
            // interpreter frame parked at the BACK-EDGE it jumped from, which
            // is by construction one of this artifact's OSR entry points; an
            // interpreted caller of the same method is parked at an INVOKE,
            // which is not.
            //
            // Interpreter round i6 wave 127, lane L3
            // (`CRATONVM_JIT_OSR_DEDUPE_SPARES_INVOKE_CALLER`, parity, default
            // ON, `=0` off): that last claim held for the bci the trace
            // reports (`last_instr_pc`) but not for the `pc` the heuristic
            // reads. A call arm advances `frame.pc` PAST the invoke before it
            // dispatches, and the single-pass tier publishes an OSR entry at
            // every pc with an empty operand stack -- so a VOID self-call
            // followed by `return` (`HANDLE.invokeExact(n - 1, leaf); return;`,
            // `f(n - 1); return;`) leaves the interpreted caller at an
            // enterable pc, and its compiled recursive callee was dropped as
            // its "continuation": one real frame lost at the interpreted ->
            // compiled boundary of a deep recursion, in `getStackTrace` and
            // `StackWalker` alike
            // (`interpreter-i127-L3-jit-deep-handle-recursion-loses-a-frame-FIXED-20261224.md`).
            // An OSR transfer happens only on a back-edge branch, so its frame's
            // `last_instr_pc` is that branch, never an invoke; a frame
            // suspended at an invoke is a caller, and the heuristic now
            // spares it. The registry arm is untouched.
            let (is_continuation, authoritative) = match osr_artifact(i) {
                Some(live_cm) => (f.cm_ptr == live_cm, true),
                None => {
                    let enterable = cm.can_osr_enter(frame.pc);
                    if enterable
                        && frame_is_suspended_at_invoke(frame)
                    {
                        cratonvm_jit::note_stack_walk_dedupe(
                            cratonvm_jit::DEDUPE_OSR_HEURISTIC_SPARED_CALLER,
                        );
                        (false, false)
                    } else {
                        (enterable, false)
                    }
                }
            };
            if is_continuation {
                if already_deduped(&deduped, f.interp_depth, &f.label) {
                    break 'decide None;
                }
                cratonvm_jit::note_stack_walk_dedupe(if authoritative {
                    cratonvm_jit::DEDUPE_OSR_AUTHORITATIVE
                } else {
                    cratonvm_jit::DEDUPE_OSR_HEURISTIC
                });
                // The half that knows where control is, handed to the frame
                // that survives -- but ONLY on the authoritative arm. See "The
                // overrides" above.
                break 'decide Some((authoritative && f.bci >= 0).then_some(i));
            }
            if authoritative {
                // The registry named a DIFFERENT artifact as this frame's
                // body, so this entry is a distinct activation nested under
                // it, and rule 2's premise does not reach it. See the doc.
                break 'decide None;
            }
            // Rule 2 — the ordinary compiled activation. A frame suspended at
            // an invoke is a CALLER and both activations are real; anything
            // else cannot have called this method and is therefore its own
            // body running compiled.
            if !call_dedupe || frame_is_suspended_at_invoke(frame) {
                break 'decide None;
            }
            if already_deduped(&deduped, f.interp_depth, &f.label) {
                break 'decide None;
            }
            cratonvm_jit::note_stack_walk_dedupe(cratonvm_jit::DEDUPE_CALL_OPCODE);
            Some(None)
        };
        match verdict {
            None => kept.push(f),
            Some(override_for) => {
                // THROWS18-2 (round 19 wave 2): a shared chain is copied
                // into the override (an OSR'd frame's, at most one per frame).
                let (bci, interp_depth, label) = (f.bci, f.interp_depth, f.label.clone());
                if let Some(i) = override_for {
                    // GC1119-1: the activation itself, for its block holds.
                    if let Some(holders) = holders.as_deref_mut() {
                        holders.push((i, f.clone()));
                    }
                    overrides.insert(i, (bci, f.into_chain_vec()));
                }
                deduped.push((interp_depth, label));
            }
        }
    }
    (kept, overrides)
}

/// Apply the SAME two dedupe rules to a set of compiled frames snapshotted at
/// an implicit NPE that [`capture_full_trace`] applies to the live ones.
///
/// The snapshot is taken inside the JIT helper, so it holds every compiled
/// activation that was on the stack at the trap — INCLUDING an OSR
/// continuation, which is the same activation as an interpreter frame that is
/// still there. Splicing it in unfiltered printed `main` twice:
///
/// ```text
/// HotSpot            after_osr  [big:42 probe:56 main:71]
/// snapshot, undeduped           [big:42 probe:56 main:71 main:71]
/// ```
///
/// measured 3 of 3 on `probes/StackTraceCompiledCallee` once defect (5)'s other
/// doors were closed and the frames stopped disappearing — the duplicate was
/// invisible for as long as the whole set was being lost.
///
/// It reuses [`drop_osr_continuations`] rather than restating either rule.
/// There is exactly one place that decides whether a compiled entry and an
/// interpreter frame are one activation, so the live path and the snapshot path
/// cannot drift, and both kill switches
/// (`CRATONVM_JIT_NO_OSR_FRAME_DEDUPE`, `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE`)
/// cover both.
///
/// The bci overrides it computes are RETURNED, not discarded. They used to be
/// dropped here on the argument that they re-point an interpreter frame the
/// LIVE capture is about to emit and this path emits none. But the frame they
/// re-point was already emitted -- by the late capture this snapshot is being
/// spliced onto -- and at its stale `pc`, because the compiled half that knew
/// better had unwound by then. Dropping the override printed the back-edge the
/// method tiered up at instead of the call it was suspended in:
///
/// ```text
/// interpreter oracle  after_main_osr  [leaf:54 mid:55 outer:56 probe:71 main:95]
/// overrides dropped                   [leaf:54 mid:55 outer:56 probe:71 main:91]
/// ```
///
/// measured on `tools/probes/StackTraceAfterOsr.java`, 2026-09-23, in both
/// compatibility modes. [`apply_snapshot_osr_overrides`] hands them to the
/// captured trace.
///
/// `frames` is the thread's frame stack at CONSTRUCTION time, not at the trap.
/// The two agree for every shape this can be reached in — a frame popped
/// between the two would have taken its compiled entry with it — and the rule
/// fails in the safe direction anyway: a depth that no longer names a matching
/// frame KEEPS the compiled entry.
pub(crate) fn dedupe_compiled_snapshot(
    frames: &[Frame],
    snapshot: Vec<ActiveCompiledFrame>,
) -> (Vec<ActiveCompiledFrame>, OsrBciOverrides) {
    let snapshot = drop_still_live(frames, snapshot);
    drop_osr_continuations(frames, snapshot)
}

/// Remove the snapshotted compiled frames that cannot have unwound: those an
/// interpreter frame in `frames` still sits above.
///
/// A compiled activation recorded at `interp_depth == d` is placed below
/// interpreter frame `d` (see `interleave_compiled_frames`). If that frame
/// still exists when the throwable is constructed, the compiled activation
/// under it is still on the machine stack too, so the late capture already
/// walked it live -- with the same bci, since it has not moved since it made
/// the call it is suspended in. Splicing its snapshot copy on as well printed it
/// twice, measured on tools/probes/StackTraceAfterOsr.java under `--real-jdk`
/// with `CRATONVM_BG_COMPILE=0`, where `main` runs as a method-entry compiled
/// frame at depth 0 below three interpreted callers:
///
/// ```text
/// interpreter oracle  after_helper_warm  [leaf:54 mid:55 outer:56 probe:71 main:87]
/// splice unfiltered                      [leaf:54 mid:55 main:87 outer:56 probe:71 main:87]
/// ```
///
/// [`trailing_overlap`] cannot catch it: that finds the snapshot's frames at
/// the inner END of the capture, which is the shape only when every compiled
/// frame was live, and a still-live frame here sits at the OUTER end with the
/// interpreter frames above it in between.
///
/// Only frames at `d >= frames.len()` -- above the innermost interpreter frame
/// -- are the ones the snapshot exists to recover. An OSR continuation of the
/// innermost frame is recorded at exactly `frames.len()`, so it survives this
/// and still reaches [`drop_osr_continuations`] for its bci override.
pub(in crate::runtime::stackwalker) fn drop_still_live(
    frames: &[Frame],
    snapshot: Vec<ActiveCompiledFrame>,
) -> Vec<ActiveCompiledFrame> {
    let innermost_interp = frames.len();
    if snapshot
        .iter()
        .all(|f| f.interp_depth as usize >= innermost_interp)
    {
        return snapshot;
    }
    snapshot
        .into_iter()
        .filter(|f| f.interp_depth as usize >= innermost_interp)
        .collect()
}

/// Apply the OSR bci overrides [`dedupe_compiled_snapshot`] returned to a trace
/// that was captured AFTER the compiled frames had unwound.
///
/// Each override names an interpreter frame by its index into `frames`. The
/// trace does not record which entry came from which frame, so the entry is
/// found by walking both lists outermost-first together: frame `i`'s entry is
/// the first one at or after frame `i - 1`'s whose class, method, descriptor
/// and bci are the frame's own, `last_instr_pc` being the bci the capture
/// reports for an interpreter frame. The walk is in order, so recursion lines
/// each frame up with its own entry rather than with the first entry of the
/// same method.
///
/// The entry is re-pointed at the compiled half's bci, and the callees that
/// half had inlined at that point go immediately after it -- exactly what
/// `interleave_compiled_frames` does with the same override on the live path.
///
/// Fails in the safe direction. A frame whose entry cannot be found ends the
/// walk, and every override from there on is left unapplied, which is the
/// historical trace: a stale line, never a moved one.
pub(crate) fn apply_snapshot_osr_overrides(
    class_store: &ClassStore,
    frames: &[Frame],
    overrides: &OsrBciOverrides,
    trace: &mut Vec<StackTraceEntry>,
) {
    if overrides.is_empty() {
        return;
    }
    let last = overrides.keys().copied().max().unwrap_or(0);
    let mut cursor = 0usize;
    for (i, frame) in frames.iter().enumerate().take(last + 1) {
        let bci = frame.last_instr_pc.min(i32::MAX as usize) as i32;
        let Some(k) = trace[cursor..]
            .iter()
            .position(|e| {
                e.byte_code_index == bci
                    && *e.method_name == *frame.method_name()
                    && *e.class_name == *frame.class_name()
                    && e.method_descriptor.as_deref() == Some(&*frame.method_descriptor())
            })
            .map(|k| cursor + k)
        else {
            return;
        };
        cursor = k + 1;
        let Some((bci, chain)) = overrides.get(&i) else {
            continue;
        };
        let entry = &mut trace[k];
        entry.byte_code_index = *bci;
        entry.line_number = LINE_NUMBER_UNKNOWN;
        resolve_line_numbers_in_place(class_store, std::slice::from_mut(entry));
        let mut inlined = Vec::with_capacity(chain.len());
        push_inlined_chain(&mut inlined, class_store, chain, true);
        cursor += inlined.len();
        trace.splice(k + 1..k + 1, inlined);
    }
}

/// Display-only replacements for one interpreter frame's own `last_instr_pc`,
/// keyed by index into `frames`: the bytecode index of the COMPILED half of
/// that activation, and the callees that half had inlined at the very same
/// program point.
///
/// Produced by [`drop_osr_continuations`] — see "The overrides" there for why
/// this is passed to the trace assembler rather than written into
/// `Frame::pc`.
// addr-keyed: frame indices, not object addresses.
pub(crate) type OsrBciOverrides = rustc_hash::FxHashMap<usize, (i32, Vec<InlinedLevel>)>;

/// Does compiled entry `f` name the interpreter frame it was pushed from
/// (`frames[f.interp_depth - 1]`)? The first test of both rules of
/// [`drop_osr_continuations`]: an entry for which this is `false` is kept,
/// in place, and nothing else is consulted for it.
pub(in crate::runtime::stackwalker) fn compiled_frame_names_its_caller(frames: &[Frame], f: &ActiveCompiledFrame) -> bool {
    (f.interp_depth as usize)
        .checked_sub(1)
        .and_then(|i| frames.get(i))
        .is_some_and(|frame| label_names_frame(&f.label, frame))
}

/// Has this `(depth, label)` already had its one body entry removed by either
/// rule of [`drop_osr_continuations`]?
///
/// A linear scan of a `Vec` rather than a set: the ledger holds one element
/// per method with BOTH halves live at one depth, which is zero or one on
/// every trace measured, and hashing the label would cost more than the
/// compare it replaces.
pub(in crate::runtime::stackwalker) fn already_deduped(seen: &[(u32, Arc<str>)], depth: u32, label: &str) -> bool {
    seen.iter().any(|(d, l)| *d == depth && &**l == label)
}

/// Kill switch for the ordinary-compiled-activation half of
/// [`drop_osr_continuations`]. Default ON;
/// `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1` restores the duplicate frame, which
/// is what makes that half an A/B inside one binary independently of
/// `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE`.
pub(in crate::runtime::stackwalker) fn call_frame_dedupe_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        None::<std::ffi::OsString>.is_none()    })}



/// The five opcodes that hand control from an interpreter frame to another
/// Java method: `invokevirtual`, `invokespecial`, `invokestatic`,
/// `invokeinterface`, `invokedynamic` (JVMS 6.5). None of them can be `wide`,
/// so the byte at the bci is the whole test.
pub(in crate::runtime::stackwalker) const INVOKE_OPCODES: [u8; 5] = [0xb6, 0xb7, 0xb8, 0xb9, 0xba];

/// Is this interpreter frame suspended *inside a call it made*?
///
/// `last_instr_pc` is the instruction the frame is executing (see the field's
/// own doc and [`entry_from_frame`], which reports it as the frame's bci), so
/// for a frame with a callee above it that is the invoke. Answers `true` for a
/// pc outside the frame's own `code`, which is the fail-closed direction for
/// the only caller: a frame this cannot read keeps its compiled twin rather
/// than losing a real activation.
pub(in crate::runtime::stackwalker) fn frame_is_suspended_at_invoke(frame: &Frame) -> bool {
    match frame.code.get(frame.last_instr_pc) {
        Some(opcode) => INVOKE_OPCODES.contains(opcode),
        None => true,
    }
}

/// Does `label` (`class/Name.method:descriptor`) name the same method as
/// `frame`? Compares all three parts: an overload or a same-named method on
/// another class is a different activation.
///
/// JIT round 16 (lane throwpath, TP16-1): decided by length and prefix /
/// suffix compares first, so a label that does not name the frame -- the
/// common answer, asked for every compiled frame of every capture -- costs no
/// scan. Equal to the split-and-compare it replaced: `label` splits at its
/// last `:` and then that part's last `.` into exactly `(class, method,
/// descriptor)` iff it is `class.method:descriptor` with no `:` in the
/// descriptor and no `.` in the method name, which the last two tests check
/// (a JVM method name never holds a `.`; kept for exactness).
pub(in crate::runtime::stackwalker) fn label_names_frame(label: &str, frame: &Frame) -> bool {
    label_names_method(
        label,
        frame.class_name(),
        frame.method_name(),
        frame.method_descriptor(),
    )
}

/// [`label_names_frame`] on the frame's three names.
pub(in crate::runtime::stackwalker) fn label_names_method(label: &str, class_name: &str, method_name: &str, descriptor: &str) -> bool {
    let dot = class_name.len();
    let colon = dot + 1 + method_name.len();
    let bytes = label.as_bytes();
    bytes.len() == colon + 1 + descriptor.len()
        && bytes[dot] == b'.'
        && bytes[colon] == b':'
        && label.starts_with(class_name)
        && label.ends_with(descriptor)
        && &label[dot + 1..colon] == method_name
        && !descriptor.as_bytes().contains(&b':')
        && !method_name.as_bytes().contains(&b'.')
}

/// Splice the CURRENT thread's active compiled frames into its interpreter
/// frames, outermost first.
///
/// A JIT-compiled method runs without pushing a [`Frame`], so on its own
/// `frames` is the Java stack *minus everything the JIT has taken over* — which
/// grows as a workload warms up, until a stack that was 21 frames deep reports
/// 6. `jit` carries, per active compiled frame, the interpreter depth it was
/// entered at ([`crate::jit::stack_trace::active_compiled_frames`]), and
/// that is exactly the insertion point: an entry recorded at depth `d` was
/// pushed when `frames[..d]` already existed and `frames[d]` did not, so it
/// belongs immediately before `frames[d]`. Entries sharing a depth are nested
/// (compiled code dispatching to compiled code) and keep their push order,
/// which is already outermost-first.
///
/// A compiled entry carries a bytecode index whenever the artifact's own
/// metadata could name the program point it is standing at, and then resolves
/// a real line from it; otherwise it keeps [`LINE_NUMBER_UNKNOWN`], which
/// `StackTraceElement` renders as `(Unknown Source)`. A frame with an unknown
/// line is still strictly better than an absent frame:
/// `Thread.getStackTrace()` consumers ask *which methods are on the stack* far
/// more often than they ask which line.
///
/// # Inlined callees, and the ORDER they go in (2026-09-01)
///
/// A JIT-compiled artifact is not one Java frame. Every callee it inlined is a
/// method that is genuinely executing, pushes nothing, and — before this —
/// contributed nothing at all: the witness in
/// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`
/// reads `len=3 [leaf:25 probe:-1 main:62]` where HotSpot reads
/// `len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]`, and `mid`/`outer` are
/// missing for exactly this reason. [`push_compiled_frames`] expands one
/// compiled entry into that chain.
///
/// The direction is the one thing here that is easy to get backwards, so the
/// argument in full. `out` is built OUTERMOST FIRST (`frames` is outermost
/// first, and [`frame_class_ids_with_compiled`] reverses at the very end
/// precisely because of that; `active_compiled_frames` likewise reverses its
/// innermost-first `nested` vector before pushing, for this splice). An entry
/// pushed EARLIER is therefore FURTHER OUT. The enclosing compiled method is
/// outside every callee it inlined, so it is pushed first; the chain arrives
/// INNERMOST FIRST, so it is walked with `.rev()` — outermost inlined level
/// inward. Get it backwards and the trace reads as if the callee called its
/// own caller.
///
/// # The OSR interaction — option (a), COMPLETE
///
/// [`drop_osr_continuations`] deletes one compiled entry per capture as the
/// duplicate of an interpreter frame it shares an activation with, and hands
/// that entry's bci AND its chain to the survivor as a display-only override.
/// If that entry carried a chain, the choice is between carrying the chain
/// across too (complete) and dropping it with the entry (fail closed, losing
/// frames).
///
/// **This takes the complete option**, and the justification is not a new one:
/// it is exactly the argument already made for the bci, applied to the other
/// half of the same evidence. The override is produced ONLY on the
/// authoritative arm, where `jit_bridge::live_osr_continuation_artifact` has
/// named this artifact as this frame's own body — not inferred it from a pc
/// shape. The bci and the chain are then two readings of one program point in
/// one activation, taken together, from metadata the artifact vouches for. If
/// the bci may be shown, so may the frames standing inside it; refusing the
/// second while accepting the first would not be caution, it would be
/// inconsistency.
///
/// It is also fail-closed in the same shape as everything else here: no
/// override means no expansion, an empty chain means no expansion, and
/// `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` still reverts the decision, the line AND
/// the frames together, inside one binary.
///
/// The expansion goes immediately AFTER the interpreter frame is pushed and
/// before the next frame's compiled entries — the same `.rev()` direction, for
/// the same reason: those callees are deeper than the frame whose bci they
/// were recorded under, and shallower than anything at the next depth.
pub(in crate::runtime::stackwalker) fn interleave_compiled_frames(
    class_store: &ClassStore,
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    resolve_lines: bool,
) -> Vec<StackTraceEntry> {
    let mut out = Vec::with_capacity(frames.len() + jit.len());
    let mut next = 0usize;
    let mut repeat = CompiledEntryRepeat::new();
    for (i, f) in frames.iter().enumerate() {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled_frames_repeating(
                &mut out,
                class_store,
                &jit[next],
                resolve_lines,
                &mut repeat,
            );
            next += 1;
        }
        let mut entry = entry_from_frame_with_lines(class_store, f, resolve_lines);
        match osr_bci.get(&i) {
            Some((bci, chain)) => {
                reline_entry(class_store, &mut entry, *bci, resolve_lines);
                out.push(entry);
                // The callees inlined into the compiled half of THIS frame's
                // own activation, which left with the entry
                // `drop_osr_continuations` removed. Same direction as
                // `push_compiled_frames`, and for the same reason.
                push_inlined_chain(&mut out, class_store, chain, resolve_lines);
            }
            None => out.push(entry),
        }
    }
    for slot in &jit[next..] {
        push_compiled_frames_repeating(&mut out, class_store, slot, resolve_lines, &mut repeat);
    }
    out
}

/// [`interleave_compiled_frames`] without source/line resolution.
///
/// This is the Throwable construction path. It reads no global class metadata:
/// every identity value comes from either a live interpreter frame or the JIT
/// artifact label that produced the compiled activation.
///
/// `positions`, when given, receives each interpreter frame's index in the
/// result, in frame order (the published trace's per-frame monitor
/// attribution needs it, [`capture_published_trace`]).
pub(in crate::runtime::stackwalker) fn interleave_compiled_frames_without_store(
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    mut positions: Option<&mut Vec<u32>>,
) -> Vec<StackTraceEntry> {
    // Sized for the inlined levels too: they are most of a trace captured
    // under a spliced `<init>` chain, and growing past `frames + jit` cost a
    // reallocation and a full copy of the entries on every such capture.
    let inlined = jit.iter().map(|f| f.chain().len()).sum::<usize>()
        + osr_bci
            .values()
            .map(|(_, chain)| chain.len())
            .sum::<usize>();
    let mut out = Vec::with_capacity(frames.len() + jit.len() + inlined);
    let mut next = 0usize;
    for (i, frame) in frames.iter().enumerate() {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled_frames_without_store(&mut out, &jit[next]);
            next += 1;
        }
        let bci = frame.last_instr_pc.min(i32::MAX as usize) as i32;
        let mut entry = StackTraceEntry {
            class_name: frame.class_name_arc(),
            method_name: frame.method_name_arc(),
            method_descriptor: Some(frame.method_descriptor_arc()),
            source_file: frame.source_file_arc(),
            line_number: frame.own_line_number(bci).unwrap_or(LINE_NUMBER_UNKNOWN),
            byte_code_index: bci,
            class_id: frame.stack_trace_class_id(),
            method_index: None,
        };
        if let Some(positions) = positions.as_deref_mut() {
            // Cast: a trace index, bounded by the frame count plus the compiled
            // and inlined levels; far below u32::MAX.
            positions.push(out.len() as u32);
        }
        if let Some((bci, chain)) = osr_bci.get(&i) {
            entry.byte_code_index = *bci;
            out.push(entry);
            push_inlined_chain_without_store(&mut out, chain);
        } else {
            out.push(entry);
        }
    }
    for slot in &jit[next..] {
        push_compiled_frames_without_store(&mut out, slot);
    }
    out
}

pub(in crate::runtime::stackwalker) fn push_compiled_frames_without_store(out: &mut Vec<StackTraceEntry>, slot: &ActiveCompiledFrame) {
    let Some(entry) = compiled_frame_entry_without_store(slot) else {
        return;
    };
    out.push(entry);
    push_inlined_chain_without_store(out, slot.chain());
}

pub(in crate::runtime::stackwalker) fn push_inlined_chain_without_store(out: &mut Vec<StackTraceEntry>, chain: &[InlinedLevel]) {
    for level in chain.iter().rev() {
        match inlined_frame_entry_without_store(level) {
            Some(entry) => out.push(entry),
            None => break,
        }
    }
}

/// The three `Arc<str>` a compiled-frame label splits into.
pub(in crate::runtime::stackwalker) type ParsedFrameLabel = (Arc<str>, Arc<str>, Arc<str>);

/// Upper bound on [`PARSED_FRAME_LABELS`]; the memo is simply cleared when it
/// fills. Distinct labels are distinct compiled or inlined METHODS, so a thread
/// that captures traces through more than this many is not a hot loop.
pub(in crate::runtime::stackwalker) const PARSED_FRAME_LABEL_CAP: usize = 4096;

thread_local! {
    /// Label -> its split `(class, method, descriptor)`, shared `Arc`s.
    ///
    /// A throwable built in a compiled method reaches here once per compiled
    /// activation AND once per level the JIT inlined at that point — for
    /// `new SomeException(msg)` that is the whole `<init>` chain, five levels
    /// for `ArrayIndexOutOfBoundsException`, which the fill-frame trim then
    /// drops. Splitting and minting three fresh `Arc<str>` for each of them on
    /// every capture was three allocations per frame for text that never
    /// changes. A pure function of the label, so a hit is exactly the parse.
    ///
    /// The same row carries the label's hidden-frame decision
    /// ([`compiled_label_is_hidden`], interpreter round i1 wave 37), so a warm
    /// capture through a compiled JDK frame judges it without a method scan.
    ///
    /// WALK21-8 (JIT round 21 wave 1, lane walk): the same per-thread cell
    /// also holds the method-slot front cache ([`CaptureThreadMemo::slots`]),
    /// so the front cache adds no `static` (`per_vm_state_statics_ratchet`).
    pub(in crate::runtime::stackwalker) static PARSED_FRAME_LABELS: std::cell::RefCell<CaptureThreadMemo> =
        std::cell::RefCell::new(CaptureThreadMemo::new());
}

/// Entries of the WALK21-8 method-slot front cache (direct-mapped).
pub(in crate::runtime::stackwalker) const METHOD_SLOT_FRONT_ENTRIES: usize = 256;
/// An empty front-cache entry's slot index.
pub(in crate::runtime::stackwalker) const METHOD_SLOT_FRONT_EMPTY: u32 = u32::MAX;

/// What [`PARSED_FRAME_LABELS`] holds for this thread: the label memo (the
/// cell's original contents; `Deref`s to it, so its readers are unchanged)
/// and WALK21-8's method-slot front cache.
pub(in crate::runtime::stackwalker) struct CaptureThreadMemo {
    pub(in crate::runtime::stackwalker) labels: rustc_hash::FxHashMap<Box<str>, FrameLabelMemo>,
    /// `(class id, signature hash, slot index)`, direct-mapped by
    /// [`method_slot_front_index`]; `METHOD_SLOT_FRONT_EMPTY` is empty. Every
    /// hit is verified against the live class by the caller, exactly as a
    /// hit of the process-wide memo is.
    pub(in crate::runtime::stackwalker) slots: [(u32, u64, u32); METHOD_SLOT_FRONT_ENTRIES],
    /// The last [`find_method_index_memoized`] answer on this thread:
    /// `(class id, name address, name length, descriptor address, descriptor
    /// length, slot index)`. A deep walk captures frame after frame of the
    /// same method, its name and descriptor slices cut from the same label,
    /// so the repeat is one compare of six words instead of a hash of the
    /// whole signature and a shared lock (2026-10-26, the stack-walker
    /// deep-walk page). Every hit is verified against the live class, as a
    /// memo hit is, so an address reused for other text cannot answer.
    pub(in crate::runtime::stackwalker) last: (u32, usize, usize, usize, usize, u32),
}

impl CaptureThreadMemo {
    pub(in crate::runtime::stackwalker) fn new() -> Self {
        Self {
            labels: rustc_hash::FxHashMap::default(),
            slots: [(0, 0, METHOD_SLOT_FRONT_EMPTY); METHOD_SLOT_FRONT_ENTRIES],
            last: (0, 0, 0, 0, 0, METHOD_SLOT_FRONT_EMPTY),
        }
    }
}

/// The [`CaptureThreadMemo::last`] key of `(class_id, name, descriptor)`.
#[inline]
pub(in crate::runtime::stackwalker) fn method_slot_last_key(
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> (u32, usize, usize, usize, usize) {
    (
        class_id.as_u32(),
        name.as_ptr() as usize,
        name.len(),
        descriptor.as_ptr() as usize,
        descriptor.len(),
    )
}

/// This thread's last method-slot answer for the key, unverified, or `None`.
pub(in crate::runtime::stackwalker) fn method_slot_last_get(key: (u32, usize, usize, usize, usize)) -> Option<u32> {
    PARSED_FRAME_LABELS
        .try_with(|memo| {
            let memo = memo.try_borrow().ok()?;
            let (c, n, nl, d, dl, idx) = memo.last;
            ((c, n, nl, d, dl) == key && idx != METHOD_SLOT_FRONT_EMPTY).then_some(idx)
        })
        .ok()
        .flatten()
}

/// Remember `idx` as this thread's last method-slot answer for the key.
pub(in crate::runtime::stackwalker) fn method_slot_last_put(key: (u32, usize, usize, usize, usize), idx: u32) {
    let _ = PARSED_FRAME_LABELS.try_with(|memo| {
        if let Ok(mut memo) = memo.try_borrow_mut() {
            memo.last = (key.0, key.1, key.2, key.3, key.4, idx);
        }
    });
}

impl std::ops::Deref for CaptureThreadMemo {
    type Target = rustc_hash::FxHashMap<Box<str>, FrameLabelMemo>;
    fn deref(&self) -> &Self::Target {
        &self.labels
    }
}

impl std::ops::DerefMut for CaptureThreadMemo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.labels
    }
}

/// The front-cache entry a `(class id, signature hash)` key maps to.
#[inline]
pub(in crate::runtime::stackwalker) fn method_slot_front_index(key: (u32, u64)) -> usize {
    // Cast: the low bits of a mixed 64-bit word, masked to the table.
    ((key.1 ^ u64::from(key.0).wrapping_mul(0x9E37_79B9_7F4A_7C15)) as usize)
        & (METHOD_SLOT_FRONT_ENTRIES - 1)
}

/// WALK21-8: the front cache's slot index for `key`, unverified, or `None`
/// (empty, another key, or the cell busy). Lock-free: a thread-local read.
pub(in crate::runtime::stackwalker) fn method_slot_front_get(key: (u32, u64)) -> Option<u32> {
    PARSED_FRAME_LABELS
        .try_with(|memo| {
            let memo = memo.try_borrow().ok()?;
            let (cid, hash, idx) = memo.slots[method_slot_front_index(key)];
            (cid == key.0 && hash == key.1 && idx != METHOD_SLOT_FRONT_EMPTY).then_some(idx)
        })
        .ok()
        .flatten()
}

/// WALK21-8: remember `idx` for `key` in this thread's front cache (replacing
/// whatever shared its entry). Skipped when the cell is busy.
pub(in crate::runtime::stackwalker) fn method_slot_front_put(key: (u32, u64), idx: u32) {
    let _ = PARSED_FRAME_LABELS.try_with(|memo| {
        if let Ok(mut memo) = memo.try_borrow_mut() {
            memo.slots[method_slot_front_index(key)] = (key.0, key.1, idx);
        }
    });
}

/// WALK21-8: empty this thread's front cache ([`clear_method_slot_memo`]).
pub(in crate::runtime::stackwalker) fn method_slot_front_clear() {
    let _ = PARSED_FRAME_LABELS.try_with(|memo| {
        if let Ok(mut memo) = memo.try_borrow_mut() {
            memo.slots = [(0, 0, METHOD_SLOT_FRONT_EMPTY); METHOD_SLOT_FRONT_ENTRIES];
            memo.last = (0, 0, 0, 0, 0, METHOD_SLOT_FRONT_EMPTY);
        }
    });
}

/// One row of [`PARSED_FRAME_LABELS`].
pub(in crate::runtime::stackwalker) struct FrameLabelMemo {
    /// The label's split; a pure function of the label.
    pub(in crate::runtime::stackwalker) parsed: Option<ParsedFrameLabel>,
    /// The last [`compiled_label_is_hidden`] answer for this label:
    /// `(class store address, class id, hidden)`. Trusted only for the same
    /// store (one per VM, so a second VM on this thread recomputes) and the
    /// same class id; `None` until a capture asked.
    pub(in crate::runtime::stackwalker) hidden: Option<(usize, u32, bool)>,
}

pub(in crate::runtime::stackwalker) fn parse_frame_label_uncached(label: &str) -> Option<ParsedFrameLabel> {
    let (owner_and_method, descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() {
        return None;
    }
    Some((
        Arc::from(class_name),
        Arc::from(method_name),
        Arc::from(descriptor),
    ))
}

pub(in crate::runtime::stackwalker) fn parse_frame_label(label: &str) -> Option<ParsedFrameLabel> {
    PARSED_FRAME_LABELS
        .try_with(|memo| {
            let Ok(mut memo) = memo.try_borrow_mut() else {
                return parse_frame_label_uncached(label);
            };
            if let Some(hit) = memo.get(label) {
                return hit.parsed.clone();
            }
            if memo.len() >= PARSED_FRAME_LABEL_CAP {
                memo.clear();
            }
            let parsed = parse_frame_label_uncached(label);
            memo.insert(
                Box::from(label),
                FrameLabelMemo {
                    parsed: parsed.clone(),
                    hidden: None,
                },
            );
            parsed
        })
        .unwrap_or_else(|_| parse_frame_label_uncached(label))
}

/// Can the compiled or inlined frame `label` (`"class/Name.method:desc"`) be
/// hidden at all? A superset of [`entry_may_be_hidden`] of its class, read
/// from the label without splitting it (a JDK package prefix, or a `/0x`
/// anywhere), so a frame of an application class never reaches the class
/// store or the memo and costs no parse per capture.
pub(in crate::runtime::stackwalker) fn label_may_be_hidden(label: &str) -> bool {
    label.starts_with("java/")
        || label.starts_with("jdk/")
        || label.starts_with("sun/")
        || label.contains("/0x")
}

/// `Method::is_hidden` for a COMPILED activation or an INLINED level: the
/// method `label` names, in class `class_id` (the artifact's owner, or the id
/// the resolver recorded for the spliced callee; an inlined level's `0` means
/// none was recorded, and the caller keeps such a frame without asking).
/// Interpreter round i1 wave 37: waves 34-35
/// judged only interpreter frames, so a hot hidden method (a hidden class's
/// method, the `@Hidden` `ScopedValue$Carrier.runWith` inlined into `call`)
/// showed in the trace once it was compiled.
///
/// Memoized per label on this thread ([`FrameLabelMemo::hidden`]): a compiled
/// frame has no `CachedBytecodeMethod` to hold the answer, and a JDK class's
/// answer otherwise costs a method scan per frame per capture. The row is
/// keyed by the store's address and the class id as well; a store address
/// reused by a later VM with the same class id and label is the same JDK
/// method or the same kind of hidden class, whose answer does not change.
pub(in crate::runtime::stackwalker) fn compiled_label_is_hidden(store: &ClassStore, label: &str, class_id: u32) -> bool {
    if !label_may_be_hidden(label) {
        return false;
    }
    let store_key = store as *const ClassStore as usize;
    PARSED_FRAME_LABELS
        .try_with(|memo| {
            let Ok(mut memo) = memo.try_borrow_mut() else {
                return compiled_label_is_hidden_uncached(store, label, class_id);
            };
            if let Some(FrameLabelMemo {
                hidden: Some((memo_store, memo_class, hidden)),
                ..
            }) = memo.get(label)
            {
                if *memo_store == store_key && *memo_class == class_id {
                    return *hidden;
                }
            }
            let hidden = compiled_label_is_hidden_uncached(store, label, class_id);
            if let Some(row) = memo.get_mut(label) {
                row.hidden = Some((store_key, class_id, hidden));
            } else {
                if memo.len() >= PARSED_FRAME_LABEL_CAP {
                    memo.clear();
                }
                memo.insert(
                    Box::from(label),
                    FrameLabelMemo {
                        parsed: parse_frame_label_uncached(label),
                        hidden: Some((store_key, class_id, hidden)),
                    },
                );
            }
            hidden
        })
        .unwrap_or_else(|_| compiled_label_is_hidden_uncached(store, label, class_id))
}

/// [`compiled_label_is_hidden`] without the memo.
pub(in crate::runtime::stackwalker) fn compiled_label_is_hidden_uncached(store: &ClassStore, label: &str, class_id: u32) -> bool {
    let Some((owner_and_method, descriptor)) = label.rsplit_once(':') else {
        return false;
    };
    let Some((_, method_name)) = owner_and_method.rsplit_once('.') else {
        return false;
    };
    method_is_hidden(
        store,
        ClassId::new(class_id),
        method_name,
        Some(descriptor),
        None,
    )
}

pub(in crate::runtime::stackwalker) fn frame_entry_from_label_without_store(
    label: &str,
    class_id: Option<ClassId>,
    bci: i32,
) -> Option<StackTraceEntry> {
    let (class_name, method_name, descriptor) = parse_frame_label(label)?;
    Some(StackTraceEntry {
        class_name,
        method_name,
        method_descriptor: Some(descriptor),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id,
        method_index: None,
    })
}

pub(in crate::runtime::stackwalker) fn compiled_frame_entry_without_store(frame: &ActiveCompiledFrame) -> Option<StackTraceEntry> {
    frame_entry_from_label_without_store(
        &frame.label,
        Some(ClassId::new(frame.owner_class_id)),
        frame.bci,
    )
}

pub(in crate::runtime::stackwalker) fn inlined_frame_entry_without_store(level: &InlinedLevel) -> Option<StackTraceEntry> {
    const MAX_CODE_LENGTH: u32 = 65_536;
    if level.bci >= MAX_CODE_LENGTH {
        return None;
    }
    frame_entry_from_label_without_store(
        &level.label,
        (level.class_id != 0).then(|| ClassId::new(level.class_id)),
        level.bci as i32,
    )
}

/// One compiled entry as ONE OR MORE [`StackTraceEntry`] values: the
/// artifact's own method, then the callees it inlined at the program point it
/// is standing at, from the outermost inlined level inward.
///
/// An empty chain — every non-inlining method, every refusal on the producer
/// side, and the whole feature switched off with
/// `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` — reproduces the historical single
/// entry exactly, byte for byte.
///
/// A label this cannot parse yields NO frame and no inlined frames either:
/// they would have no caller to attach to.
pub(in crate::runtime::stackwalker) fn push_compiled_frames(
    out: &mut Vec<StackTraceEntry>,
    class_store: &ClassStore,
    slot: &ActiveCompiledFrame,
    resolve_lines: bool,
) {
    let Some(enclosing) = compiled_frame_entry(class_store, slot, resolve_lines) else {
        return;
    };
    out.push(enclosing);
    push_inlined_chain(out, class_store, slot.chain(), resolve_lines);
}

/// One capture's last compiled entry and what it was built from
/// ([`push_compiled_frames_repeating`]): the label's address (the label is
/// held by the walk for the whole capture, so its address names its text),
/// the owner class id and the bci. Never filled while the switch is off.
pub(in crate::runtime::stackwalker) struct CompiledEntryRepeat {
    pub(in crate::runtime::stackwalker) last: Option<((usize, u32, i32), StackTraceEntry)>,
    pub(in crate::runtime::stackwalker) on: bool,
}

impl CompiledEntryRepeat {
    pub(in crate::runtime::stackwalker) fn new() -> Self {
        Self {
            last: None,
            on: crate::jit::conservative_roots::stackwalk_repeat_entry_memo_enabled(),
        }
    }
}

/// [`push_compiled_frames`] for one capture: an activation of the same body
/// (the same label `Arc`), owner and bci as the previous compiled activation
/// gets a clone of that activation's entry instead of a second
/// [`compiled_frame_entry`] (the label split, the class-store read, the method
/// lookup, the line search and a source-file copy). `compiled_frame_entry` is
/// a pure function of those three and of `class_store` / `resolve_lines`,
/// which one capture holds fixed, so the clone is the entry it would build.
/// The stack-walker residual-costs page, item 3
/// (`CRATONVM_STACKWALK_REPEAT_ENTRY_MEMO`, default ON).
pub(in crate::runtime::stackwalker) fn push_compiled_frames_repeating(
    out: &mut Vec<StackTraceEntry>,
    class_store: &ClassStore,
    slot: &ActiveCompiledFrame,
    resolve_lines: bool,
    repeat: &mut CompiledEntryRepeat,
) {
    if !repeat.on {
        push_compiled_frames(out, class_store, slot, resolve_lines);
        return;
    }
    let key = (
        Arc::as_ptr(&slot.label) as *const u8 as usize,
        slot.owner_class_id,
        slot.bci,
    );
    let enclosing = match &repeat.last {
        Some((k, entry)) if *k == key => entry.clone(),
        _ => {
            let Some(entry) = compiled_frame_entry(class_store, slot, resolve_lines) else {
                return;
            };
            repeat.last = Some((key, entry.clone()));
            entry
        }
    };
    out.push(enclosing);
    push_inlined_chain(out, class_store, slot.chain(), resolve_lines);
}

/// Push one inline chain, OUTERMOST inlined level first, onto an `out` that is
/// already outermost-first and already holds the frame these are inlined INTO.
///
/// # `break`, not `continue`
///
/// A level that cannot be named leaves every DEEPER level with no caller.
/// Pushing them anyway would attach them to the wrong method — a trace that
/// reads as if a call happened that never did, and that a reader has no way to
/// tell from a real one. Losing a suffix of the chain is recoverable by a
/// reader (the trace is visibly short); a fabricated caller is not. So the
/// first refusal ends the chain.
pub(in crate::runtime::stackwalker) fn push_inlined_chain(
    out: &mut Vec<StackTraceEntry>,
    class_store: &ClassStore,
    chain: &[InlinedLevel],
    resolve_lines: bool,
) {
    // The chain is innermost-first; `out` is outermost-first.
    for level in chain.iter().rev() {
        match inlined_frame_entry(class_store, level, resolve_lines) {
            Some(e) => out.push(e),
            None => break,
        }
    }
}

/// One INLINED callee as a [`StackTraceEntry`], from the
/// `"class/Name.method:descriptor"` label the emitter recorded for it and that
/// callee's own bytecode index.
///
/// This is [`compiled_frame_entry`] with two differences, both forced by the
/// fact that an inlined callee is not an artifact:
///
///   * **there is no owner class id.** The emitter that spliced the body knew
///     the callee only by its internal name, so the class is resolved by name
///     through [`find_class_id_by_name_memoized`] — see that function for why
///     the memo is not optional here.
///   * **an absent class still yields a frame.** `compiled_frame_entry`
///     already takes exactly that position for its own `None` arm, and it is
///     the right one: the FRAME is vouched for by the artifact's own metadata
///     (this method was inlined at this point, which is a fact about the
///     machine code that ran) even when the store cannot be consulted to say
///     which line. Only the line is unknown, and it is reported as
///     [`LINE_NUMBER_UNKNOWN`].
///
/// # Nothing here guesses
///
/// The line is resolved only through `find_method_index_memoized`, which
/// matches on name AND descriptor: a tombstoned class yields no `Class`, an
/// overload set yields the one exact slot or nothing, and either way the entry
/// keeps [`LINE_NUMBER_UNKNOWN`]. An unparsable or empty-part label yields no
/// frame at all rather than a mangled name.
///
/// The bci bound is re-checked here even though the emitter already refuses a
/// row outside it (JVMS 4.9.1, `Code.code_length < 65536`). That is not
/// belt-and-braces: `line_number_for_bci_in_method` picks the largest
/// `start_pc <= bci`, so a bci PAST the end of the method resolves to its LAST
/// line — a confidently wrong line, which is the single worst outcome
/// available in this file. One comparison buys immunity from it at a crate
/// boundary this module cannot otherwise police.
pub(in crate::runtime::stackwalker) fn inlined_frame_entry(
    class_store: &ClassStore,
    level: &InlinedLevel,
    resolve_lines: bool,
) -> Option<StackTraceEntry> {
    let InlinedLevel {
        label,
        bci,
        class_id: recorded_class_id,
        ..
    } = level;
    let bci = *bci;
    // JVMS 4.9.1: `Code.code_length` must be less than 65536, so every genuine
    // bci is below it. Mirrors `conservative_roots::plausible_bci`.
    const MAX_CODE_LENGTH: u32 = 65_536;
    if bci >= MAX_CODE_LENGTH {
        return None;
    }
    let (owner_and_method, method_descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() {
        return None;
    }
    // The RESOLVER's own id first (2026-09-02). It is the id the splice was
    // planned against, so it needs no name scan and cannot pick a different
    // class of the same name under another loader -- the two failure modes of
    // the by-name route. `0` means the producer supplied none (every hand-built
    // fixture, and any artifact compiled before the field existed), and the
    // memoized by-name resolution stays as the fallback for exactly that.
    let by_id = (*recorded_class_id != 0)
        .then(|| ClassId::new(*recorded_class_id))
        .and_then(|id| class_store.get(id).map(|c| (id, c)));
    // `get` after the by-name resolution rather than trusting the id alone:
    // the memo's own verification already did this read, but the borrow cannot
    // escape it, and repeating it is one hash probe.
    let resolved = by_id.or_else(|| {
        find_class_id_by_name_memoized(class_store, class_name)
            .and_then(|id| class_store.get(id).map(|c| (id, c)))
    });
    // WALK20-2 (`CRATONVM_STACKWALK_SHARED_ENTRY_NAMES`): the class's and the
    // resolved method's own `Arc<str>`s instead of fresh copies of the same
    // text (the memo verifies the method's names against the label's).
    let shared = crate::jit::conservative_roots::r20_stackwalk_shared_entry_names_enabled();
    let mut shared_names: Option<(Arc<str>, Arc<str>)> = None;
    let (class_name, source_file, class_id, method_index, line_number) = match resolved {
        Some((id, c)) => {
            let method_index = find_method_index_memoized(c, id, method_name, method_descriptor);
            if shared {
                shared_names = method_index
                    .and_then(|i| c.methods.get(i as usize))
                    .map(|m| (Arc::clone(&m.name), Arc::clone(&m.descriptor)));
            }
            // Exactly the resolution `entry_from_frame` and
            // `compiled_frame_entry` perform, over the same memo and the same
            // `LineNumberTable` scan — one implementation of the JVMS 4.7.12
            // rule, reached from all three capture paths.
            let line_number = resolve_lines
                .then(|| {
                    method_index
                        .and_then(|i| c.methods.get(i as usize))
                        .and_then(|m| line_number_for_bci_in_method(m, bci as usize))
                })
                .flatten()
                .unwrap_or(LINE_NUMBER_UNKNOWN);
            (
                if shared {
                    Arc::clone(&c.name)
                } else {
                    Arc::from(&*c.name)
                },
                c.source_file.as_deref().map(Arc::from),
                Some(id),
                method_index,
                line_number,
            )
        }
        // Name-only entry. `class_id: None` is honest — no id was established,
        // and `StackFrame.getDeclaringClass()` must not be handed a guess.
        None => (Arc::from(class_name), None, None, None, LINE_NUMBER_UNKNOWN),
    };
    let (method_name, method_descriptor) = match shared_names {
        Some(names) => names,
        None => (Arc::from(method_name), Arc::from(method_descriptor)),
    };
    Some(StackTraceEntry {
        class_name,
        method_name,
        method_descriptor: Some(method_descriptor),
        source_file,
        line_number,
        // Cast: bounded above by the JVMS 4.9.1 check at the top.
        byte_code_index: bci as i32,
        class_id,
        method_index,
    })
}

/// Prepend the compiled frames snapshotted at a JIT-signalled implicit NPE to
/// a trace that was captured after they had already unwound.
///
/// An implicit NPE raised inside compiled code is CONSTRUCTED later, from the
/// interpreter, once the helper's `i64::MIN` sentinel has propagated out of the
/// compiled activation (see `jit::helpers::take_jit_pending_npe`). By then the
/// frames between the throw site and the first interpreter frame are gone, and
/// the trace names none of the code that actually raised the exception —
/// `[big:42 probe:56 main:67]` on HotSpot reads `[probe:56 main:67]` here, and
/// after an OSR of the caller it reads `[main:71]` alone.
///
/// The snapshot was taken inside the helper, while those frames were still on
/// the stack. They are innermost-first and belong in front of everything the
/// late capture found. Entries are built through [`push_compiled_frames`], the
/// same builder the live splice uses, so a frame recovered this way carries
/// the same bci, the same line and the same inlined callees as one caught
/// live.
///
/// `drop_hidden` (`--real-jdk`, interpreter round i1 wave 37) leaves the
/// snapshot's hidden frames out ([`stack_entry_is_hidden`]) BEFORE the
/// overlap merge, as the construction-time capture left them out of `trace`:
/// merging an unfiltered snapshot onto a filtered trace would both show the
/// hidden frame and defeat the overlap (a hidden frame between two live ones
/// makes the two lists disagree), printing the live ones twice.
pub fn append_snapshotted_compiled_frames(
    class_store: &ClassStore,
    snapshot: &[ActiveCompiledFrame],
    mut trace: Vec<StackTraceEntry>,
    drop_hidden: bool,
) -> Vec<StackTraceEntry> {
    if snapshot.is_empty() {
        return trace;
    }
    // ORDER. A STORED trace is outermost-first — the Java `StackTraceElement[]`
    // is built by reversing it, which is why `capture_stack_trace`'s consumers
    // iterate `.rev()`. The snapshotted frames are the INNERMOST ones (they ran
    // below everything the late capture could still see), so outermost-first
    // means they go on the END, in the order `active_compiled_frames` already
    // returns them. Getting this backwards is not a subtle failure: it prints
    // the throw site as the outermost frame, which reads as a plausible trace
    // of a completely different call.
    let mut fresh: Vec<StackTraceEntry> = Vec::with_capacity(snapshot.len());
    for f in snapshot {
        // An inlined callee is deeper than the artifact that inlined it, so it
        // is pushed after it — the same direction as the live splice, for the
        // same reason.
        push_compiled_frames(&mut fresh, class_store, f, false);
    }
    if drop_hidden {
        let dbg = crate::runtime::env_cache::dbg_sttrace();
        fresh.retain(|e| {
            let hidden = stack_entry_is_hidden(class_store, e);
            if hidden && dbg {
                eprintln!(
                    "STTRACE_DBG_HIDDEN kind=trap-splice {}.{}",
                    e.class_name, e.method_name
                );
            }
            !hidden
        });
    }
    let overlap = trailing_overlap(&trace, &fresh);
    trace.reserve(fresh.len() - overlap);
    trace.extend(fresh.into_iter().skip(overlap));
    trace
}

/// How many entries at the FRONT of `fresh` the tail of `trace` already holds.
///
/// # Why this is needed at all
///
/// [`append_snapshotted_compiled_frames`]'s premise is that the compiled frames
/// have already unwound by the time the throwable is constructed — the whole
/// reason the snapshot exists. One door breaks that premise:
/// `jit::helpers::materialize_implicit_signal` builds the throwable INSIDE the
/// JIT helper, with every compiled frame still on the stack, so
/// `fillInStackTrace` has already walked and spliced them. Appending the
/// snapshot unfiltered then prints them twice:
///
/// ```text
/// as PRINTED, i.e. innermost first -- this function's own lists are the
/// reverse of that:
/// HotSpot                       [leaf:25 mid:26 outer:27 probe:42 main:66]
/// unfiltered snapshot splice    [leaf:25 mid:26 outer:27 probe:42
///                                        mid:26 outer:27 probe:42 main:66]
/// ```
///
/// measured on a `synchronized` throw-site variant of
/// `probes/StackTraceAfterOsr.java`, where the `synchronized` is doing nothing
/// but routing the raise through that door.
///
/// Since round 12 wave 4 that door hands the snapshot to the capture itself
/// ([`arm_trap_capture`]) whenever the snapshot can stand in for the walk, and
/// then splices nothing; this merge serves the shapes and doors that cannot.
/// The overlap cannot repair a still-live frame the walk read at a different
/// bci than the snapshot (a same-frame compiled `catch`): that pair compares
/// unequal and the frame would print twice, which is one reason the door
/// prefers the one-walk route.
///
/// # Why the OVERLAP and not a membership test
///
/// Both lists describe ONE stack, outermost-first, and `fresh` is the inner
/// end of it. So the only correct merge is the longest prefix of `fresh` that
/// is a suffix of `trace` — which is also what makes genuine recursion safe: a
/// method that really does appear twice appears twice in BOTH lists, and the
/// maximal overlap lines them up instead of deleting a real frame. A membership
/// test ("is this method already in the trace?") would delete one.
///
/// Zero overlap — every shape where the frames HAD unwound — appends
/// everything, which is byte-for-byte what this function did before.
///
/// Entries are compared on `(class_name, method_name, byte_code_index)`: the
/// identity of a program point. The line number is derived from the bci and the
/// source file from the class, so neither adds information, and `class_id` /
/// `method_index` are `None` on a name-only entry and would make two readings of
/// one frame compare unequal.
pub(in crate::runtime::stackwalker) fn trailing_overlap(trace: &[StackTraceEntry], fresh: &[StackTraceEntry]) -> usize {
    let max = trace.len().min(fresh.len());
    for n in (1..=max).rev() {
        let tail = &trace[trace.len() - n..];
        if tail
            .iter()
            .zip(&fresh[..n])
            .all(|(a, b)| same_program_point(a, b))
        {
            return n;
        }
    }
    0
}

/// Do two entries name the same program point? See [`trailing_overlap`].
pub(in crate::runtime::stackwalker) fn same_program_point(a: &StackTraceEntry, b: &StackTraceEntry) -> bool {
    a.class_name == b.class_name
        && a.method_name == b.method_name
        && a.byte_code_index == b.byte_code_index
}

/// Re-point an already-built entry at `bci`, re-resolving its line.
///
/// Used for the OSR-entered interpreter frame whose own `pc` is stale (see
/// [`drop_osr_continuations`]). Leaves the line `LINE_NUMBER_UNKNOWN` when the
/// method has no `LineNumberTable` row covering `bci`, exactly as
/// [`entry_from_frame`] would.
pub(in crate::runtime::stackwalker) fn reline_entry(
    class_store: &ClassStore,
    entry: &mut StackTraceEntry,
    bci: i32,
    resolve_lines: bool,
) {
    entry.byte_code_index = bci;
    entry.line_number = if resolve_lines {
        entry
            .class_id
            .and_then(|cid| class_store.get(cid))
            .zip(entry.method_index)
            .and_then(|(class, idx)| class.methods.get(idx as usize))
            .and_then(|m| line_number_for_bci_in_method(m, bci.max(0) as usize))
            .unwrap_or(LINE_NUMBER_UNKNOWN)
    } else {
        LINE_NUMBER_UNKNOWN
    };
}

/// One compiled frame as a [`StackTraceEntry`], from the artifact's
/// `"class/Name.method:descriptor"` label and its owning class id.
///
/// Returns `None` for a label this cannot parse rather than emitting a frame
/// with a mangled name — the only labels in production come from
/// `x64::compile_with_param_slots`' `method_key` (and the matching stamp on the
/// optimizing tier), which are always of that shape.
pub(in crate::runtime::stackwalker) fn compiled_frame_entry(
    class_store: &ClassStore,
    frame: &ActiveCompiledFrame,
    resolve_lines: bool,
) -> Option<StackTraceEntry> {
    let ActiveCompiledFrame {
        label,
        owner_class_id,
        bci,
        ..
    } = frame;
    let (owner_and_method, method_descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() {
        return None;
    }
    let class_id = ClassId::new(*owner_class_id);
    let class = class_store.get(class_id);
    // WALK20-2 (`CRATONVM_STACKWALK_SHARED_ENTRY_NAMES`): the class's own
    // `Arc<str>`s instead of fresh copies of the same text.
    let shared = crate::jit::conservative_roots::r20_stackwalk_shared_entry_names_enabled();
    // Prefer the class's own recorded name: the label is built from the same
    // string, but a class the store knows is the authority, and it also gives
    // the source file the label cannot carry.
    let (class_name, source_file, method_index) = match class {
        Some(c) if shared => (
            Arc::clone(&c.name),
            c.source_file.as_deref().map(std::sync::Arc::from),
            find_method_index_memoized(c, class_id, method_name, method_descriptor),
        ),
        Some(c) => (
            std::sync::Arc::from(&*c.name),
            c.source_file.as_deref().map(std::sync::Arc::from),
            find_method_index_memoized(
                c,
                class_id,
                &std::sync::Arc::<str>::from(method_name),
                &std::sync::Arc::<str>::from(method_descriptor),
            ),
        ),
        None => (std::sync::Arc::from(class_name), None, None),
    };
    // WALK20-2: the resolved method's own names (the memo verifies both
    // against the label's slices, so the text is the same).
    let shared_names = class
        .filter(|_| shared)
        .zip(method_index)
        .and_then(|(c, idx)| c.methods.get(idx as usize))
        .map(|m| (Arc::clone(&m.name), Arc::clone(&m.descriptor)));
    // The bci comes from the safepoint id the live frame published, and
    // `activation_bci` has already required the artifact to name it as one of
    // its own recorded safepoints — so this is a bytecode index the method
    // really stopped at, not a guess. `-1` (no usable id) keeps the old
    // answer: `LINE_NUMBER_UNKNOWN`, rendered `(Unknown Source)`. A wrong line
    // would still be worse than none; what changed is that a right one is now
    // available for the overwhelming majority of frames.
    let line_number = if resolve_lines && *bci >= 0 {
        class
            .zip(method_index)
            .and_then(|(c, idx)| c.methods.get(idx as usize))
            .and_then(|m| line_number_for_bci_in_method(m, *bci as usize))
            .unwrap_or(LINE_NUMBER_UNKNOWN)
    } else {
        LINE_NUMBER_UNKNOWN
    };
    let (method_name, method_descriptor) = match shared_names {
        Some(names) => names,
        None => (
            std::sync::Arc::from(method_name),
            std::sync::Arc::from(method_descriptor),
        ),
    };
    Some(StackTraceEntry {
        class_name,
        method_name,
        method_descriptor: Some(method_descriptor),
        source_file,
        line_number,
        byte_code_index: *bci,
        class_id: Some(class_id),
        method_index,
    })
}

/// Like [`capture_full_trace`] but WITHOUT resolving source-line numbers — so it
/// needs no `ClassStore` and takes no lock. Used to publish a per-thread frame
/// snapshot at blocking deposit points (see `deposit_root_snapshot`) for
/// cross-thread `Thread.getStackTrace()` / `dumpThreads()`: the diagnostic only
/// needs `class.method` (+ BCI) to pinpoint where a parked thread is stuck, and
/// keeping it lock-free keeps it safe to call from every deposit site. The line
/// number is left [`LINE_NUMBER_UNKNOWN`], except for a frame running a body a
/// redefinition replaced, which carries its own lines (`Frame::own_line_number`).
///
/// Deferred resolution: pass the result to [`resolve_line_numbers_in_place`]
/// once a `ClassStore` borrow is available — `ThreadRegistry::frame_trace_of_resolved`
/// is the wired-up reader.
///
/// These entries carry no [`StackTraceEntry::method_index`]: deriving one needs
/// the `ClassStore` this function deliberately does not take. Deferred
/// resolution therefore falls back to the unambiguous-name rule for them and
/// leaves overloaded frames at `UNKNOWN`. It never reports a *wrong* line, only
/// an unknown one, which is strictly better than the all-`UNKNOWN` snapshot
/// this function returns.
pub fn capture_frames_no_lines(frames: &[Frame]) -> Vec<StackTraceEntry> {
    frames
        .iter()
        .map(|f| StackTraceEntry {
            class_name: f.class_name_arc(),
            method_name: f.method_name_arc(),
            method_descriptor: None,
            source_file: f.source_file_arc(),
            line_number: f
                .own_line_number(f.last_instr_pc.min(i32::MAX as usize) as i32)
                .unwrap_or(LINE_NUMBER_UNKNOWN),
            byte_code_index: f.last_instr_pc.min(i32::MAX as usize) as i32,
            class_id: f.stack_trace_class_id(),
            // Deliberately absent: resolving an index requires the ClassStore
            // borrow this deposit path must not take. See CR-CLO-2 in
            // `cross-owner-closeout.md` for the
            // `Frame`-side change that would make it free.
            method_index: None,
        })
        .collect()
}

/// The trace a thread PUBLISHES for other threads (`JvmThread::frame_trace`,
/// read by another thread's `Thread.getStackTrace()`, `getAllStackTraces()`
/// and thread dumps through `ThreadRegistry::frame_trace_of_resolved`), with
/// the thread's compiled activations interleaved -- the same frames an
/// own-thread capture shows. Runs on the publishing thread itself (its JIT
/// activation chain is thread-local), at the blocking deposit and at a
/// safepoint a cross-thread read asked for.
///
/// It used to be [`capture_frames_no_lines`] alone, so a thread parked under
/// compiled callers published its park frames and the interpreter frames
/// around them with every compiled method between them missing (interpreter
/// round i1 wave 38, lane L7;
/// `docs/feature-designs/real-jdk-mode.md`).
/// A thread that entered no compiled code has an empty chain and gets exactly
/// the entries it got before.
///
/// `CRATONVM_DBG_STTRACE=1` prints one `[sttrace] publish:` line per capture
/// that had compiled activations.
///
/// A compiled activation of a body compiled before its class's redefinition
/// (the obsolete activation JEP 109 keeps running) gets the line its OWN
/// body's table gives its bci, here, as a `Throwable`'s retained compiled
/// frame does ([`publish_compiled_lines_at_their_stamps`]; interpreter round
/// i1 wave 40, lane L3). Before, its entry carried no line and the reader
/// resolved it from the class as it is then: the edited body's table.
///
/// `reflective_calls` / `reflective_named` are the thread's
/// `JvmThread::reflective_calls` and `reflective_frames_named`: the JDK frames
/// of each reflective call the thread is inside go between the caller and the
/// target, as the thread's own capture lists them
/// ([`splice_published_reflective_frames`]; interpreter round i1 wave 45,
/// lane L3). A thread outside every reflective call pays one `is_empty`.
///
/// `served` is the thread's `JvmThread::served_thread_run`: the `Thread.run`
/// frames of the served calls it is inside go in from their rows, as its own
/// capture lists them (round 15 wave 6, lane trace6; TR4W3-2 of
/// `r15w5-trace5-review-findings-CLOSED-20260930.md`). From the memo only
/// ([`ServedThreadRunCalls::published_splices`]); a reader whose trace lists
/// none keeps its rules (`vm_exec::thread_stack_trace_served`).
pub(crate) fn capture_published_trace(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    reflective_calls: &[ReflectiveCallRow],
    reflective_named: &ReflectiveFramesNamed,
    served: &ServedThreadRunCalls,
) -> PublishedTrace {
    capture_published_trace_with_jit(
        shared,
        frames,
        reflective_calls,
        reflective_named,
        served,
        crate::jit::stack_trace::active_compiled_frames(),
        crate::runtime::interpreter::jit_bridge::live_osr_continuation_artifact,
    )
}

/// Assemble the owner's or a held peer's trace with the same reflective,
/// served Thread.run, obsolete-line, OSR and monitor-position rules.
#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_published_trace_with_jit(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    reflective_calls: &[ReflectiveCallRow],
    reflective_named: &ReflectiveFramesNamed,
    served: &ServedThreadRunCalls,
    jit: Vec<ActiveCompiledFrame>,
    osr_artifact: impl Fn(usize) -> Option<usize>,
) -> PublishedTrace {
    if jit.is_empty() {
        let mut trace = PublishedTrace {
            entries: capture_frames_no_lines(frames),
            frame_positions: None,
            compiled_holds: Vec::new(),
        };
        if !reflective_calls.is_empty() || !served.is_empty() {
            splice_published_reflective_frames(
                frames,
                &jit,
                &OsrBciOverrides::default(),
                reflective_calls,
                reflective_named,
                served,
                cratonvm_classloading::class_redefinition_count(),
                &mut trace,
                Some(&shared.threads.throwable_layout_cache.stack_walk_named),
            );
        }
        return trace;
    }
    let activations = jit.len();
    // GC1119-1 (round 19 wave 12, lane gc12): the dropped OSR continuations
    // too, under `CRATONVM_JMX_OSR_BODY_BLOCK_HOLDS`.
    // Preserve this capture's OSR registry: held-thread traces can be built
    // for a peer whose continuation table is not the caller's table.
    let mut osr_holders = OsrHolders::new();
    let (jit, osr_bci) = drop_osr_continuations_with_holders(
        frames,
        jit,
        osr_artifact,
        crate::runtime::interpreter::held_monitors::osr_body_block_holds_enabled()
            .then_some(&mut osr_holders),
    );
    let mut positions = Vec::with_capacity(frames.len());
    let mut entries =
        interleave_compiled_frames_without_store(frames, &jit, &osr_bci, Some(&mut positions));
    if cratonvm_classloading::class_redefinition_count() != 0 {
        publish_compiled_lines_at_their_stamps(shared, frames, &jit, &osr_bci, &mut entries);
    }
    if crate::runtime::env_cache::dbg_sttrace() {
        eprintln!(
            "[sttrace] publish: interpreted={} compiled-activations={} entries={}",
            frames.len(),
            activations,
            entries.len()
        );
    }
    // JIT round 19 proposal wave 7, lane gc7 (SYNC619-2): the compiled
    // activations' own holds at their positions (candidates; the publish
    // keeps those on the lock stack). Empty unless an activation can hold one.
    let compiled_holds = crate::runtime::interpreter::held_monitors::published_compiled_holds(
        shared,
        frames,
        &jit,
        &osr_bci,
        &osr_holders,
        &entries,
    );
    let mut trace = PublishedTrace {
        entries,
        frame_positions: Some(positions),
        compiled_holds,
    };
    if !reflective_calls.is_empty() || !served.is_empty() {
        splice_published_reflective_frames(
            frames,
            &jit,
            &osr_bci,
            reflective_calls,
            reflective_named,
            served,
            cratonvm_classloading::class_redefinition_count(),
            &mut trace,
            Some(&shared.threads.throwable_layout_cache.stack_walk_named),
        );
    }
    trace
}

/// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-1; a narrow hook for the
/// frame-trace pause, `gc_and_alloc::publish_lazy_claimed_traces`):
/// [`capture_published_trace`] of a thread with NO compiled activation, run
/// by ANOTHER thread. `capture_published_trace` reads the compiled chain of
/// the thread that calls it (thread-local); for a thread whose chain is empty
/// its answer is exactly this one -- its first branch, with the same empty
/// chain -- so a pause holding such a thread (claimed in C) publishes the
/// trace the thread would publish itself. Takes no lock (the reflective and
/// served rows are the memoized ones, as in the owner's capture).
pub(crate) fn capture_published_trace_interpreted(
    frames: &[Frame],
    reflective_calls: &[ReflectiveCallRow],
    reflective_named: &ReflectiveFramesNamed,
    served: &ServedThreadRunCalls,
) -> PublishedTrace {
    let jit: Vec<ActiveCompiledFrame> = Vec::new();
    let mut trace = PublishedTrace {
        entries: capture_frames_no_lines(frames),
        frame_positions: None,
        compiled_holds: Vec::new(),
    };
    if !reflective_calls.is_empty() || !served.is_empty() {
        splice_published_reflective_frames(
            frames,
            &jit,
            &OsrBciOverrides::default(),
            reflective_calls,
            reflective_named,
            served,
            cratonvm_classloading::class_redefinition_count(),
            &mut trace,
            None,
        );
    }
    trace
}

/// [`capture_published_trace`] of ANOTHER thread, held where it cannot
/// change `frames` or the compiled activations `jit` names (a thread a
/// handshake pause excused in a native it entered under compiled code;
/// interpreter round i2 wave 60, lane L6,
/// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED`). `jit` is the walk of a copy
/// of its chain (`JitChainSnapshot::of_held_thread`; empty for a thread with
/// interpreter frames only), and `osr_artifact` answers its OSR-continuation
/// registry.
///
/// `rows` (interpreter round i2, the i53 retirement; `jvm_thread::NX_REFLECTIVE`):
/// the reflective and served `Thread.run` calls the thread stood in when it
/// entered the native, as it recorded them (`jvm_thread::NativeRowsSnapshot`),
/// spliced as its own published trace splices them
/// ([`splice_published_reflective_frames`]). Before, a thread under a
/// reflective call was never listable, and one under a served call was
/// listed without its `Thread.run` frame.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn capture_trace_of_held_thread(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    osr_artifact: impl Fn(usize) -> Option<usize>,
    rows: Option<&crate::threading::jvm_thread::NativeRowsSnapshot>,
) -> Vec<StackTraceEntry> {
    capture_published_trace_of_held_thread(shared, frames, jit, osr_artifact, rows).entries
}

/// [`capture_trace_of_held_thread`], with each interpreter frame's index in
/// the entries (`PublishedTrace::frame_positions`, `None` = one entry per
/// frame), shifted by the spliced rows exactly as the thread's own publish
/// shifts them -- so the pause that publishes the held thread's trace can
/// publish its per-frame locked-monitor attribution with it (JIT round 19
/// wave 9, lane gc9, `CRATONVM_JMX_HELD_NATIVE_FRAME_MONITORS`). The compiled
/// activations' own holds (`PublishedTrace::compiled_holds`) only under
/// `CRATONVM_JMX_HELD_NATIVE_COMPILED_HOLDS` (JIT round 19 wave 11, lane
/// gc11, GC920-4; `held_monitors::held_thread_compiled_holds`): frame words
/// of the held thread's stack, each read within its activation's own band.
pub(crate) fn capture_published_trace_of_held_thread(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    osr_artifact: impl Fn(usize) -> Option<usize>,
    rows: Option<&crate::threading::jvm_thread::NativeRowsSnapshot>,
) -> PublishedTrace {
    if jit.is_empty() {
        let trace = PublishedTrace {
            entries: capture_frames_no_lines(frames),
            frame_positions: None,
            compiled_holds: Vec::new(),
        };
        return match rows {
            Some(rows) => splice_held_rows(
                frames,
                &jit,
                &OsrBciOverrides::default(),
                rows,
                trace,
                Some(&shared.threads.throwable_layout_cache.stack_walk_named),
            ),
            None => trace,
        };
    }
    // GC1119-1 (round 19 wave 12, lane gc12): the dropped OSR continuations
    // too, when the held thread's compiled holds are read at all.
    let mut osr_holders = OsrHolders::new();
    let keep = crate::runtime::interpreter::held_monitors::held_native_compiled_holds_enabled()
        && crate::runtime::interpreter::held_monitors::osr_body_block_holds_enabled();
    let (jit, osr_bci) = drop_osr_continuations_with_holders(
        frames,
        jit,
        osr_artifact,
        keep.then_some(&mut osr_holders),
    );
    let mut positions = Vec::with_capacity(frames.len());
    let mut entries =
        interleave_compiled_frames_without_store(frames, &jit, &osr_bci, Some(&mut positions));
    if cratonvm_classloading::class_redefinition_count() != 0 {
        publish_compiled_lines_at_their_stamps(shared, frames, &jit, &osr_bci, &mut entries);
    }
    // GC920-4 (JIT round 19 wave 11, lane gc11): the held thread's compiled
    // holds, opt-in. SAFETY: this function's contract -- the thread is held
    // where it changes neither `frames` nor the activations `jit` names.
    let compiled_holds = unsafe {
        crate::runtime::interpreter::held_monitors::held_thread_compiled_holds(
            shared,
            frames,
            &jit,
            &osr_bci,
            &osr_holders,
            &entries,
        )
    };
    let trace = PublishedTrace {
        entries,
        frame_positions: Some(positions),
        compiled_holds,
    };
    match rows {
        Some(rows) => splice_held_rows(
            frames,
            &jit,
            &osr_bci,
            rows,
            trace,
            Some(&shared.threads.throwable_layout_cache.stack_walk_named),
        ),
        None => trace,
    }
}

/// [`capture_trace_of_held_thread`]'s splice of the recorded `rows` into
/// `entries`, a capture of `frames` / `jit` / `osr_bci` laid out as
/// [`collect_trace_slots`] lays them out.
pub(in crate::runtime::stackwalker) fn splice_held_rows(
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    rows: &crate::threading::jvm_thread::NativeRowsSnapshot,
    mut trace: PublishedTrace,
    walk: Option<&StackWalkFramesNamed>,
) -> PublishedTrace {
    if rows.reflective_calls.is_empty() && rows.served.is_empty() {
        return trace;
    }
    // The positions ride along (round 19 wave 9, lane gc9): the splice shifts
    // them as it shifts the thread's own publish.
    splice_published_reflective_frames(
        frames,
        jit,
        osr_bci,
        &rows.reflective_calls,
        &rows.reflective_named,
        &rows.served,
        cratonvm_classloading::class_redefinition_count(),
        &mut trace,
        walk,
    );
    trace
}

/// The JDK frames of `calls` (the publishing thread's reflective calls) in
/// `trace`, a published capture of `frames` laid out as
/// [`collect_trace_slots`] lays out `frames` / `jit` / `osr_bci` (one entry per
/// slot), each at [`trace_anchor_position`] -- the placement the thread's own
/// capture uses -- and `trace.frame_positions` shifted to match, so the
/// per-frame monitor attribution (`publish_jmx_frame_monitors`) keeps naming
/// the right rows. `count` is the class-redefinition count now: frames named
/// at another count are stale and left out.
///
/// The frames come from `named` ONLY ([`reflective_splices_memoized`]): a
/// publish may run while the thread holds the class-manager lock (a blocking
/// deposit inside class loading), so it names nothing itself. The native
/// entering a call names them ([`name_reflective_frames_if_stale`]), so a call
/// is left out only when that found the lock busy, or they cannot be named.
/// A call that pushed nothing yet is left out (a thread parked in the native
/// itself). Interpreter round i1 wave 45, lane L3 (the proposal
/// `docs/feature-designs/real-jdk-mode.md`);
/// `CRATONVM_DBG_STTRACE=1` prints one `[sttrace] publish-reflect:` line per
/// publish that listed a call.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::stackwalker) fn splice_published_reflective_frames(
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    calls: &[ReflectiveCallRow],
    named: &ReflectiveFramesNamed,
    served: &ServedThreadRunCalls,
    count: u64,
    trace: &mut PublishedTrace,
    walk: Option<&StackWalkFramesNamed>,
) {
    let mut splices = reflective_splices_memoized(named, calls, count, walk);
    // Round 15 wave 6 (lane trace6; TR4W3-2): the served `Thread.run` rows,
    // merged by depth as the thread's own capture merges them. The switch is
    // read only when the thread is inside a served call.
    let mut served_rows = 0usize;
    if !served.is_empty()
        && true
    {
        let rows = served.published_splices(count);
        served_rows = rows.len();
        merge_served_thread_run_splices(&mut splices, rows);
    }
    if splices.is_empty() {
        return;
    }
    let slots = collect_trace_slots(frames, jit, osr_bci);
    if slots.len() != trace.entries.len() {
        return;
    }
    let positions = reflective_splice_positions(&slots, &splices, false);
    if positions.is_empty() {
        return;
    }
    // Each interpreter frame's entry index, before the splice.
    let before: Vec<u32> = match trace.frame_positions.take() {
        Some(p) => p,
        None => (0..frames.len())
            .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
            .collect(),
    };
    // After it: grown by the frames spliced at or before each entry (a call's
    // frames go right BEFORE the entry at its position).
    let mut after = Vec::with_capacity(before.len());
    let mut shift = 0usize;
    let mut next = positions.iter().peekable();
    for &p in &before {
        while let Some(&&(position, k)) = next.peek() {
            if position > p as usize {
                break;
            }
            shift += splices[k].entries.len();
            next.next();
        }
        after.push(u32::try_from(p as usize + shift).unwrap_or(u32::MAX));
    }
    // SYNC619-2 (round 19 wave 7, lane gc7): the compiled holds' entries move
    // by the same rule (a call's frames go right BEFORE the entry at its
    // position, so an entry at `p` grows by every splice at or before `p`).
    for (p, _) in trace.compiled_holds.iter_mut() {
        let grown: usize = positions
            .iter()
            .take_while(|&&(position, _)| position <= *p as usize)
            .map(|&(_, k)| splices[k].entries.len())
            .sum();
        *p = u32::try_from(*p as usize + grown).unwrap_or(u32::MAX);
    }
    let mut listed = 0usize;
    // From the innermost, so an earlier position still indexes the entry it
    // named; calls at one position keep their order, the outer one first.
    for &(position, k) in positions.iter().rev() {
        let inner = trace.entries.split_off(position);
        trace.entries.extend(splices[k].entries.iter().cloned());
        trace.entries.extend(inner);
        listed += 1;
    }
    trace.frame_positions = Some(after);
    if crate::runtime::env_cache::dbg_sttrace() {
        // Round 15 wave 7 (lane drainC; `r15w6-trace6-review-findings` item 3):
        // `listed` counts reflective calls AND served rows; `rows` says how
        // many of the merged splices were served `Thread.run` rows (last, so
        // the `calls=N listed=N` text older probes grep stays intact).
        eprintln!(
            "[sttrace] publish-reflect: calls={} listed={listed} entries={} rows={served_rows}",
            calls.len(),
            trace.entries.len()
        );
    }
}

/// What [`capture_published_trace`] returns.
pub struct PublishedTrace {
    /// The trace, outermost frame first.
    pub entries: Vec<StackTraceEntry>,
    /// Each interpreter frame's index in `entries`, in frame order; `None`
    /// when there is one entry per frame (no compiled activation), so frame
    /// `i` is entry `i`. The per-frame locked-monitor attribution published
    /// with the trace (`ThreadRegistry::publish_jmx_frame_monitors`) is
    /// translated through it for a reader of the whole trace.
    pub frame_positions: Option<Vec<u32>>,
    /// JIT round 19 proposal wave 7, lane gc7 (SYNC619-2): the monitors the
    /// compiled activations hold as their own, as `(index in entries,
    /// address)` pairs, innermost first
    /// (`held_monitors::published_compiled_holds`). Addresses are candidates,
    /// never dereferenced: `ThreadRegistry::publish_jmx_frame_monitors` keeps
    /// one only as the lock-stack slot of the object at that address. Empty
    /// for a trace with no compiled activation holding anything.
    pub compiled_holds: Vec<(u32, usize)>,
}

/// The lines of `entries` (a published trace, laid out as
/// [`collect_trace_slots`] lays out the same `frames` / `jit` / `osr_bci`)
/// whose compiled activation, or a callee it inlined, runs a body compiled
/// before its class's last redefinition: from the table that body's class
/// had at the compile stamp (`obsolete_frames::resolve_lines_as_captured`,
/// the `Throwable` path's resolver, with the stamps the stack walk's capture
/// also reads). Interpreter round i1 wave 40, lane L3
/// (`L3W40OtherThreadObsoleteLine.java`,
/// `docs/feature-designs/real-jdk-mode.md`).
///
/// HotSpot's thread dump / `Thread.getStackTrace()` of such an activation
/// names its old body's line, as a `Throwable` does. The reader
/// (`ThreadRegistry::frame_trace_of_resolved`) resolves an entry with no line
/// from the class as it is when read, which after the redefinition is the
/// edited body's table at the old body's bci; an entry resolved here keeps its
/// line.
///
/// Runs only once some class of the process was redefined and the thread has
/// compiled activations; takes the class-manager lock only when one of them
/// is older than the current redefinition count, and never waits for it (the
/// publishing thread may hold it): a busy lock leaves the entries as before.
pub(in crate::runtime::stackwalker) fn publish_compiled_lines_at_their_stamps(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    entries: &mut [StackTraceEntry],
) {
    let now = cratonvm_classloading::class_redefinition_count();
    let slots = collect_trace_slots(frames, jit, osr_bci);
    if slots.len() != entries.len() {
        return;
    }
    let stamps: Vec<Option<u64>> = slots
        .iter()
        .map(|slot| match slot {
            TraceSlot::Interp { .. } => None,
            TraceSlot::Compiled(compiled) => compiled.compile_cp_stamp,
            TraceSlot::Inlined(_, stamp) => *stamp,
        })
        .collect();
    if !stamps.iter().flatten().any(|&stamp| stamp < now) {
        return;
    }
    let Some(cm) = shared.classes.class_manager.try_read() else {
        return;
    };
    crate::runtime::interpreter::obsolete_frames::resolve_lines_as_captured(
        &cm,
        now,
        entries,
        Some(stamps.as_slice()),
    );
}

/// Lock-free, exact-descriptor variant for Throwable construction.
///
/// Unlike the diagnostic deposit snapshot, a live `Frame` already carries its
/// descriptor. Retaining it avoids both a `ClassStore` read lock and the
/// per-signature memo lookup at construction time while still letting a reader
/// resolve an overloaded method exactly later.
pub fn capture_frames_no_lines_exact(frames: &[Frame]) -> Vec<StackTraceEntry> {
    frames
        .iter()
        .map(|f| StackTraceEntry {
            class_name: f.class_name_arc(),
            method_name: f.method_name_arc(),
            method_descriptor: Some(f.method_descriptor_arc()),
            source_file: f.source_file_arc(),
            line_number: f
                .own_line_number(f.last_instr_pc.min(i32::MAX as usize) as i32)
                .unwrap_or(LINE_NUMBER_UNKNOWN),
            byte_code_index: f.last_instr_pc.min(i32::MAX as usize) as i32,
            class_id: f.stack_trace_class_id(),
            method_index: None,
        })
        .collect()
}

/// Resolve line numbers for entries captured *without* them (see
/// [`capture_frames_no_lines`]), in place, from the live `ClassStore`.
///
/// This is the deferred half of a lazy capture. It is deliberately
/// **fail-closed**: an entry is only filled in when the resolution is provably
/// the same one an eager capture would have produced.
///
/// An entry is resolved iff all of the following hold:
///
/// 1. its `line_number` is still [`LINE_NUMBER_UNKNOWN`] — an already-resolved
///    entry (and in particular a [`LINE_NUMBER_NATIVE`] one) is never touched;
/// 2. it carries a `class_id` — synthetic entries with no backing interpreter
///    frame have no class to resolve against;
/// 3. that `class_id` is still live in the store — an unloaded class leaves a
///    tombstone (`ClassStore::remove`) and `ClassId`s are monotonic and never
///    reused, so this can only ever fail *closed*, never resolve against the
///    wrong class;
/// 4. its `byte_code_index` is non-negative;
/// 5. the exact declaring method can be identified — see below.
///
/// Returns the number of entries that were filled in.
///
/// # How the method is identified (rule 5)
///
/// **Preferred, and exact: `method_index`.** An entry captured through
/// [`entry_from_frame`] carries [`StackTraceEntry::method_index`], the frame's
/// slot in `Class::methods`. The index is re-read from the *live* class and its
/// `name` re-checked against the entry's `method_name` before it is used, so a
/// redefinition that reordered or removed methods cannot resolve against the
/// wrong body — it simply fails the check and drops through to the fallback.
/// With the index present, an overload set is no obstacle: overloads occupy
/// distinct slots.
///
/// **Fallback, when `method_index` is absent** (synthetic entries, and the
/// deliberately `ClassStore`-free [`capture_frames_no_lines`] snapshot): resolve
/// only when **exactly one** method declared by that class carries the recorded
/// name. With more than one, the entry names an overload set and there is
/// nothing left to disambiguate with; two overloads have different
/// `LineNumberTable`s, so guessing would print a line from the wrong method
/// body. Those entries keep `UNKNOWN`.
///
/// Either way the function is **fail-closed**: it yields the line an eager
/// capture would have produced, or `UNKNOWN`. It never reports a wrong line.
///
/// # Relationship to the `Throwable` path
///
/// Throwable construction uses [`capture_full_trace_without_lines`]. Its
/// entries carry an exact `method_index`, and `SharedVm::throwable_stack_trace`
/// calls this resolver before exposing a Java-visible trace. Therefore an
/// overloaded method resolves to the same line as eager capture, while a
/// caught-and-discarded Throwable avoids the work altogether.
///
/// It is a strict *improvement* for [`capture_frames_no_lines`] consumers,
/// which have no line numbers at all otherwise.
pub fn resolve_line_numbers_in_place(
    class_store: &ClassStore,
    entries: &mut [StackTraceEntry],
) -> usize {
    let mut resolved = 0usize;
    for entry in entries.iter_mut() {
        if entry.byte_code_index < 0 {
            continue;
        }
        let Some(class_id) = entry.class_id else {
            continue;
        };
        let Some(class) = class_store.get(class_id) else {
            continue;
        };
        if entry.source_file.is_none() {
            entry.source_file = class.source_file.as_deref().map(Arc::from);
        }
        if entry.line_number != LINE_NUMBER_UNKNOWN {
            continue;
        }
        let name = &*entry.method_name;

        // Exact path: a captured descriptor avoids an eager class-store lookup
        // on the construction path. The legacy slot is the fallback for
        // entries captured before descriptors were retained.
        let mut method: Option<&ClassFileMethod> = entry
            .method_descriptor
            .as_deref()
            .and_then(|descriptor| class.find_method(name, descriptor));
        if method.is_none() {
            method = entry
                .method_index
                .and_then(|idx| class.methods.get(idx as usize))
                .filter(|m| &*m.name == name);
        }

        if method.is_none() {
            // No index, or the index no longer names this method (redefinition).
            // Fall back to the unambiguous-name rule, which is also fail-closed:
            // if the name is unique within the class it can only denote the one
            // method, and if it is not, we decline.
            let mut only: Option<&ClassFileMethod> = None;
            for m in &class.methods {
                if &*m.name == name {
                    if only.is_some() {
                        // Overload set — cannot disambiguate without an index.
                        only = None;
                        break;
                    }
                    only = Some(m);
                }
            }
            method = only;
        }

        let Some(method) = method else {
            continue;
        };
        if let Some(line) = line_number_for_bci_in_method(method, entry.byte_code_index as usize) {
            entry.line_number = line;
            resolved += 1;
        }
    }
    resolved
}

/// Synthesize a synthetic "no source info" entry. Used for native frames
/// injected from outside the interpreter (JNI up-calls, host-side
/// StackWalker probes during VM bootstrap).
pub fn synthetic_entry(class_name: Arc<str>, method_name: Arc<str>) -> StackTraceEntry {
    StackTraceEntry {
        class_name,
        method_name,
        method_descriptor: None,
        source_file: None,
        line_number: LINE_NUMBER_NATIVE,
        byte_code_index: -1,
        class_id: None,
        method_index: None,
    }
}

/// Consult a [`Class`]'s attributes for the `SourceFile` attribute.
/// Fallback for frames whose cached `source_file_arc()` is `None`.
pub fn source_file_of_class(class: &Class) -> Option<Arc<str>> {
    class.source_file.as_deref().map(Arc::from)
}

// ---------------------------------------------------------------------------
// Stand-in frames: the JDK frames a VM-implemented method runs without
// ---------------------------------------------------------------------------
//
// Round 13 wave 13 (lane trace3),
// `r13w12-orch-vm-implemented-jdk-methods-leave-no-stack-frames-FIXED-20260929.md`.
// A method the VM serves itself pushes no Java frame, so an exception raised
// inside it was captured with its CALLER on top. HotSpot, for an interrupted
// `Thread.sleep(10)` on JDK 25:
//
// ```text
// java.base/java.lang.Thread.sleepNanos0(Native Method)
// java.base/java.lang.Thread.sleepNanos(Thread.java:509)
// java.base/java.lang.Thread.sleep(Thread.java:540)
// Main.main(Main.java:3)
// ```
//
// `--compatible` (a registered `Thread.sleep`) recorded only `Main.main`;
// `--real-jdk` (real `sleep` and `sleepNanos`, native `sleepNanos0`) had no
// native leaf. The innermost captured frame names what was running: it stands
// at the invoke of the method the VM served. [`native_standin_frames`] rebuilds
// that method's frames from the real class bytes -- each Java level at its own
// call to the next (its `LineNumberTable` line), down to the `native` leaf
// (line -2) -- for the census below only, and only for the throwables HotSpot
// raises INSIDE that leaf. The throwable screen is what keeps an exception the
// invoke itself raised before entering (a null receiver's
// `NullPointerException` at `o.wait()`) on the caller, where HotSpot has it.

/// One JDK frame a native stands for: a row of [`STANDIN_TABLE`], the one
/// table every capture that lists such frames reads (interpreter round i1
/// wave 48, lane L3; `docs/feature-designs/real-jdk-mode.md`,
/// stage 1). Which rule places the frame is its [`StandinFamily`]'s.
pub(in crate::runtime::stackwalker) struct StandinMethod {
    pub(in crate::runtime::stackwalker) class: &'static str,
    pub(in crate::runtime::stackwalker) name: &'static str,
    /// `""`: derived from the call the frame stands for (a method-handle
    /// holder's erased shape, [`hidden_invoke_standin_frames_with`]).
    pub(in crate::runtime::stackwalker) descriptor: &'static str,
    /// The throwables HotSpot raises from inside this method's native leaf.
    pub(in crate::runtime::stackwalker) throws: &'static [&'static str],
    /// May the chain START here (the innermost captured frame calls it)?
    /// `false` for a row reached only as an inner hop, whose call site out
    /// is right only for the way that hop enters it (`Thread.join(long)`
    /// from `join()`: the `wait(0)` loop, not the timed one).
    pub(in crate::runtime::stackwalker) entry: bool,
    /// `(owner, name, descriptor)` of the call the frame stands at, for the
    /// families placed by a named call (the reflective steps: each at its
    /// first call of the next; `descriptor` `""`: picked by the call's
    /// arity). `None`: the family's own rule finds the site (the census's
    /// last census call, an argument check's constructor call).
    pub(in crate::runtime::stackwalker) calls: Option<(&'static str, &'static str, &'static str)>,
    /// Is the frame hidden (`@Hidden`, a method-handle holder's
    /// LambdaForm), listed only by a walk that shows hidden frames?
    pub(in crate::runtime::stackwalker) hidden: bool,
}

impl StandinMethod {
    /// The defaults of a census-shaped row, for `..StandinMethod::ROW`.
    pub(in crate::runtime::stackwalker) const ROW: StandinMethod = StandinMethod {
        class: "",
        name: "",
        descriptor: "",
        throws: &[],
        entry: true,
        calls: None,
        hidden: false,
    };

    /// `(owner, name, descriptor)` of the call a reflective step stands at.
    /// Every reflective row names one; `("", "", "")` for a row that does
    /// not (never matched by a class-file call).
    pub(in crate::runtime::stackwalker) fn call(&self) -> (&'static str, &'static str, &'static str) {
        self.calls.unwrap_or(("", "", ""))
    }
}

/// Which rule places a [`STANDIN_TABLE`] row's frame, and which captures read
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::stackwalker) enum StandinFamily {
    /// `Thread.sleep`, `Object.wait`, `Thread.join`, `Array.newInstance`: a
    /// chain down to the native leaf, each level at its last census call
    /// ([`native_standin_frames`], a parked thread's published trace, a
    /// debugger's listing).
    Census,
    /// `LockSupport.park*`: a debugger's listing only
    /// ([`debugger_standin_frames`]).
    Park,
    /// A method whose own bytecode rejects an argument at its one
    /// `IllegalArgumentException` construction ([`standin_arg_check_frames`]).
    ArgCheck,
    /// The `(long, int)` overloads with two such sites
    /// ([`served_arg_check_standin_frames`]).
    ArgCheckTwoSite,
    /// `Method.invoke`'s frames ([`reflective_entries`]).
    MethodInvoke,
    /// `Constructor.newInstance`'s frames ([`reflective_entries`]).
    ConstructorNewInstance,
    /// The hidden method-handle frames of a `MethodHandle.invokeExact` /
    /// `invoke` or `Method.invoke` call, listed by a `SHOW_HIDDEN_FRAMES`
    /// walk only ([`hidden_invoke_standin_frames_with`]).
    HiddenInvoke,
    /// `AccessController.doPrivileged`'s one frame, at its `run()` call, for
    /// each overload the VM serves with a bridge ([`reflective_entries`],
    /// placed as the reflective steps are, and listed in every view;
    /// interpreter round i1 wave 49, lane L3).
    Privileged,
    /// A served `Class.getField` / `getMethod` / `getConstructor` ... miss:
    /// the looked-up method at its one `throw new NoSuch...Exception`
    /// ([`member_lookup_standin_frames`]; interpreter round i1 wave 52, lane
    /// L3).
    MemberLookup,
}

/// The one table of the JDK frames a native stands for, by family (wave 48,
/// lane L3). Every stand-in capture reads its rows from here
/// ([`standin_rows`]): a throwable's, a published trace's, a walk's and a
/// debugger's listing (`debugger_standin_frames`).
pub(in crate::runtime::stackwalker) const STANDIN_TABLE: &[(StandinFamily, &[StandinMethod])] = &[
    (StandinFamily::Census, STANDIN_METHODS),
    (StandinFamily::Park, STANDIN_PARK_METHODS),
    (StandinFamily::ArgCheck, STANDIN_ARG_CHECK_METHODS),
    (
        StandinFamily::ArgCheckTwoSite,
        STANDIN_ARG_CHECK_TWO_SITE_METHODS,
    ),
    (StandinFamily::MethodInvoke, METHOD_INVOKE_STEPS),
    (
        StandinFamily::ConstructorNewInstance,
        CONSTRUCTOR_NEW_INSTANCE_STEPS,
    ),
    (StandinFamily::HiddenInvoke, HIDDEN_INVOKE_METHODS),
    (StandinFamily::Privileged, PRIVILEGED_STEPS),
    (StandinFamily::MemberLookup, MEMBER_LOOKUP_METHODS),
];

/// `family`'s rows of [`STANDIN_TABLE`] (empty for a family with none).
pub(in crate::runtime::stackwalker) fn standin_rows(family: StandinFamily) -> &'static [StandinMethod] {
    STANDIN_TABLE
        .iter()
        .find(|(f, _)| *f == family)
        .map_or(&[], |(_, rows)| rows)
}

pub(in crate::runtime::stackwalker) const STANDIN_SLEEP_THROWS: &[&str] = &["java/lang/InterruptedException"];
pub(in crate::runtime::stackwalker) const STANDIN_JOIN_THROWS: &[&str] = &["java/lang/InterruptedException"];
pub(in crate::runtime::stackwalker) const STANDIN_INTERRUPTED: &str = "java/lang/InterruptedException";

/// Round 14 wave 6 (lane trace5): the served timed joins, `Thread.join(J)V`
/// and `join(JI)V`: not entries of the census (the frames cannot tell their
/// `wait` loop), entries of [`timed_join_standin_frames`] (the native can).
pub(in crate::runtime::stackwalker) fn is_served_timed_join_row(row: &StandinMethod) -> bool {
    row.class == "java/lang/Thread"
        && row.name == "join"
        && matches!(row.descriptor, "(J)V" | "(JI)V")
}
const STANDIN_WAIT_THROWS: &[&str] = &[
    "java/lang/InterruptedException",
    "java/lang/IllegalMonitorStateException",
];

/// The census. A chain is found in the real bytecode, not written down here,
/// so one table serves JDK 17 (`sleep(long)` and `wait(long)` are themselves
/// `native`), 21 (`sleep0`) and 25 (`sleepNanos` -> `sleepNanos0`, `wait0`).
pub(in crate::runtime::stackwalker) const STANDIN_METHODS: &[StandinMethod] = &[
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(JI)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(Ljava/time/Duration;)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleepNanos",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleepNanos0",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep0",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/util/concurrent/TimeUnit",
        name: "sleep",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait",
        descriptor: "()V",
        throws: STANDIN_WAIT_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait",
        descriptor: "(J)V",
        throws: STANDIN_WAIT_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait",
        descriptor: "(JI)V",
        throws: STANDIN_WAIT_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait0",
        descriptor: "(J)V",
        throws: STANDIN_WAIT_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    // Round 14 wave 4 (lane trace3; `r13w13-trace3-vm-served-jdk-methods-residuals`
    // item 1): a registered `join()` (`--compatible`). JDK 17/21/25 `join()` is
    // `join(0L)`, whose LAST `wait` call is the `millis == 0` loop's `wait(0)`
    // (line 1887 on JDK 25), then `Object.wait(long)` -> `wait0`. That call
    // names `java/lang/Thread.wait(J)V` (javac qualifies by the enclosing
    // class), which the owner resolution of `last_standin_call` walks up to
    // `Object`. `join(long)` is NOT an entry: a direct timed join HotSpot
    // leaves from its OTHER `wait(delay)` site (line 1881), which the frames
    // alone cannot tell apart. `CRATONVM_THROWABLE_STANDIN_JOIN_FRAMES=0` drops
    // both rows. Round 14 wave 6 (lane trace5): the served native tells it
    // (`timed_join_standin_frames`), and the `join(long)` hop picks its
    // `wait` by argument, not position ([`StandinScreen::join_waits_zero`]).
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "()V",
        throws: STANDIN_JOIN_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "(J)V",
        throws: STANDIN_JOIN_THROWS,
        entry: false,
        ..StandinMethod::ROW
    },
    // Round 14 wave 6 (lane trace5): the registered `join(long, int)` is
    // `join(millis)` after its checks (JDK 17/21/25), and the served native
    // runs the timed join, whose hint picks the `join(long)` hop's `wait`.
    // Reached only through `timed_join_standin_frames`.
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "(JI)V",
        throws: STANDIN_JOIN_THROWS,
        entry: false,
        ..StandinMethod::ROW
    },
    // Round 14 wave 5 (lane trace4): `Array.newInstance(Class, int)` is Java
    // in JDK 17-25 (`return newArray(componentType, length);`, line 76 on 25),
    // but a registered `Bridge` serves it in both modes
    // (`reflect_annotations::register_reflect_array_natives`), so a
    // `NegativeArraySizeException` had the caller on top. HotSpot:
    // `Array.newArray(Native Method)`, `Array.newInstance(Array.java:76)`.
    // The null / `void.class` component (NPE / IAE) is raised by `newArray`
    // too; the call is static, so no receiver NPE is in play.
    // `CRATONVM_THROWABLE_STANDIN_ARRAY_FRAMES=0` drops both rows.
    StandinMethod {
        class: "java/lang/reflect/Array",
        name: "newInstance",
        descriptor: "(Ljava/lang/Class;I)Ljava/lang/Object;",
        throws: STANDIN_NEW_ARRAY_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/reflect/Array",
        name: "newArray",
        descriptor: "(Ljava/lang/Class;I)Ljava/lang/Object;",
        throws: STANDIN_NEW_ARRAY_THROWS,
        entry: true,
        ..StandinMethod::ROW
    },
    // Round 15 wave 2 (lane trace2; TR5-5 of `jit-r14-trace5-proposals-RETIRED-20260930.md`):
    // the `LockSupport` parks and the `Unsafe.park` native they end in, for
    // a PARKED thread's published stack only (`throws` is empty, so no
    // throwable screen admits them: a park raises nothing). HotSpot 25, an
    // idle `ThreadPoolExecutor` worker: `Unsafe.park(Native Method)`,
    // `LockSupport.park(LockSupport.java:371)`, `AbstractQueuedSynchronizer$
    // ConditionNode.block(...)`. `--compatible` serves every `LockSupport`
    // park (registered `Bridge`s), so the published stack stood at the
    // caller; `--real-jdk` runs them and stood at `U.park(..)`. Each
    // `LockSupport` body calls `U.park` once, on its platform branch (the
    // `VirtualThreads.park` call of its virtual branch names no row), so a
    // virtual thread's chain is refused by its caller
    // ([`parked_frames_fit_virtual_thread`]).
    // `CRATONVM_THREAD_STACK_PARKED_PARK_FRAMES=0` drops the rows.
    StandinMethod {
        class: STANDIN_UNSAFE_CLASS,
        name: "park",
        descriptor: "(ZJ)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: STANDIN_LOCK_SUPPORT_CLASS,
        name: "park",
        descriptor: "(Ljava/lang/Object;)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: STANDIN_LOCK_SUPPORT_CLASS,
        name: "park",
        descriptor: "()V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: STANDIN_LOCK_SUPPORT_CLASS,
        name: "parkNanos",
        descriptor: "(Ljava/lang/Object;J)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: STANDIN_LOCK_SUPPORT_CLASS,
        name: "parkNanos",
        descriptor: "(J)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: STANDIN_LOCK_SUPPORT_CLASS,
        name: "parkUntil",
        descriptor: "(Ljava/lang/Object;J)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: STANDIN_LOCK_SUPPORT_CLASS,
        name: "parkUntil",
        descriptor: "(J)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
];

/// Round 15 wave 2 (lane trace2; TR5-5): the classes of the parked-only rows.
pub(in crate::runtime::stackwalker) const STANDIN_UNSAFE_CLASS: &str = "jdk/internal/misc/Unsafe";
pub(in crate::runtime::stackwalker) const STANDIN_LOCK_SUPPORT_CLASS: &str = "java/util/concurrent/locks/LockSupport";

pub(in crate::runtime::stackwalker) const STANDIN_NEW_ARRAY_THROWS: &[&str] = &[
    "java/lang/NegativeArraySizeException",
    "java/lang/NullPointerException",
    "java/lang/IllegalArgumentException",
];

/// Interpreter round i1 wave 47, lane L1: the park family, rows only a
/// debugger's listing of a thread blocked in a stand-in uses
/// ([`StandinScreen::Debugger`], [`debugger_standin_frames`]); no throwable
/// screen admits them (`throws` is empty). `--compatible` serves
/// `LockSupport.park*` with registered natives, so a thread parked there ran
/// no JDK frame, where HotSpot 25.0.3 lists `Unsafe.park` (native) over
/// `LockSupport.parkNanos` over the caller (`L1W45JdiSuspendedInNative`'s
/// `parker`). JDK 17-25 `park*` call `U.park(..)` last on the platform
/// branch (the virtual one calls `VirtualThreads.park`, no row).
pub(in crate::runtime::stackwalker) const STANDIN_PARK_METHODS: &[StandinMethod] = &[
    StandinMethod {
        class: "java/util/concurrent/locks/LockSupport",
        name: "park",
        descriptor: "()V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/util/concurrent/locks/LockSupport",
        name: "park",
        descriptor: "(Ljava/lang/Object;)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/util/concurrent/locks/LockSupport",
        name: "parkNanos",
        descriptor: "(J)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/util/concurrent/locks/LockSupport",
        name: "parkNanos",
        descriptor: "(Ljava/lang/Object;J)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/util/concurrent/locks/LockSupport",
        name: "parkUntil",
        descriptor: "(J)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/util/concurrent/locks/LockSupport",
        name: "parkUntil",
        descriptor: "(Ljava/lang/Object;J)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "jdk/internal/misc/Unsafe",
        name: "park",
        descriptor: "(ZJ)V",
        throws: &[],
        entry: true,
        ..StandinMethod::ROW
    },
];

/// Is `row` switched on? The `join` rows (round 14 wave 4) and the `Array`
/// rows (wave 5) have a switch of their own; it is read only once a call
/// matched one.
pub(in crate::runtime::stackwalker) fn standin_row_enabled(row: &StandinMethod) -> bool {
    if row.name == "join" {
        return true;
    }
    if row.class == "java/lang/reflect/Array" {
        return true;
    }
    // Round 15 wave 2 (lane trace2; TR5-5).
    if matches!(row.class, STANDIN_UNSAFE_CLASS | STANDIN_LOCK_SUPPORT_CLASS) {
        return true;
    }
    true
}

/// Which rows a stand-in chain may use.
#[derive(Clone, Copy)]
pub(in crate::runtime::stackwalker) enum StandinScreen<'a> {
    /// A throwable raised inside the leaf: the rows that raise it.
    Throwable(&'a str),
    /// A thread PARKED inside the leaf (another thread's published stack,
    /// round 14 wave 4 T4-3): every row -- the thread is in the leaf, so
    /// there is no throwable to screen by.
    Parked,
    /// A throwable raised inside the leaf on a VIRTUAL thread (round 14 wave
    /// 5, lane trace4): the `Object.wait` rows only, each hop at its FIRST
    /// census call. See [`virtual_thread_standin_frames`].
    VirtualThrowable(&'a str),
    /// An `InterruptedException` raised by a served DIRECT `Thread.join(long)`
    /// (round 14 wave 6, lane trace5, TR5-3), which the native says ran with a
    /// positive timeout (`timed`) or none. See [`timed_join_standin_frames`].
    TimedJoin { timed: bool },
    /// A debugger listing a thread blocked in (or suspended at the return
    /// of) a stand-in (interpreter round i1 wave 47, lane L1;
    /// [`debugger_standin_frames`]): every row [`Self::Parked`] admits, and
    /// the park family ([`STANDIN_PARK_METHODS`]).
    #[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
    Debugger,
}

impl StandinScreen<'_> {
    /// The rows this screen may look up: the census, and for
    /// [`Self::Debugger`] the park family too.
    pub(in crate::runtime::stackwalker) fn rows(self) -> impl Iterator<Item = &'static StandinMethod> {
        let park: &'static [StandinMethod] = if matches!(self, Self::Debugger) {
            standin_rows(StandinFamily::Park)
        } else {
            &[]
        };
        standin_rows(StandinFamily::Census)
            .iter()
            .chain(park.iter())
    }

    pub(in crate::runtime::stackwalker) fn admits(self, row: &StandinMethod) -> bool {
        match self {
            Self::Throwable(throwable_class) => row.throws.contains(&throwable_class),
            Self::Parked | Self::Debugger => true,
            // The `Object.wait` rows, and the `Array` rows (no thread branch).
            Self::VirtualThrowable(throwable_class) => {
                matches!(row.class, "java/lang/Object" | "java/lang/reflect/Array")
                    && row.throws.contains(&throwable_class)
            }
            Self::TimedJoin { .. } => row.throws.contains(&STANDIN_INTERRUPTED),
        }
    }

    /// May a chain START at `row`? The entry rows; and, for
    /// [`Self::TimedJoin`], the `entry: false` rows `Thread.join(J)V` /
    /// `join(JI)V`, whose `wait` call the native's hint decides.
    pub(in crate::runtime::stackwalker) fn enters_at(self, row: &StandinMethod) -> bool {
        row.entry || (matches!(self, Self::TimedJoin { .. }) && is_served_timed_join_row(row))
    }

    /// Which `wait` call the `Thread.join(long)` hop leaves from, told apart by
    /// its ARGUMENT rather than its position (round 14 wave 6, lane trace5):
    /// `true` = the `wait(0)` (an `lconst_0` argument: `join()` is
    /// `join(0L)`, and a direct `join(0)`), `false` = the timed `wait(delay)`.
    /// JDK 21/25 put the timed loop first, JDK 17 the `wait(0)` loop, so the
    /// "last call" rule answered JDK 17's `join()` from the wrong loop.
    pub(in crate::runtime::stackwalker) fn join_waits_zero(self) -> bool {
        match self {
            Self::TimedJoin { timed } => !timed,
            _ => true,
        }
    }

    /// Does a hop leave its body from its FIRST census call rather than its
    /// last? Only on a virtual thread: JDK 25 `Object.wait(long)` calls `wait0`
    /// twice, the virtual-thread branch first.
    pub(in crate::runtime::stackwalker) fn first_call(self) -> bool {
        matches!(self, Self::VirtualThrowable(_))
    }
}

/// The longest chain rebuilt (JDK 25's longest is `TimeUnit.sleep` ->
/// `Thread.sleep` -> `sleepNanos` -> `sleepNanos0`).
pub(in crate::runtime::stackwalker) const STANDIN_CHAIN_MAX: usize = 8;

/// Could a throwable of this class get stand-in frames at all? The first,
/// cheapest screen of [`native_standin_frames`].
pub(crate) fn is_standin_throwable(throwable_class: &str) -> bool {
    standin_rows(StandinFamily::Census)
        .iter()
        .any(|m| m.throws.contains(&throwable_class))
        || throwable_class == STANDIN_ARG_CHECK_THROWABLE
}

/// Round 14 wave 2 (lane trace; `r13w13-trace3-vm-served-jdk-methods-residuals`
/// item 2): the census methods whose OWN bytecode rejects a bad argument before
/// reaching the leaf. HotSpot, `Thread.sleep(-1)` on JDK 25: ONE frame,
/// `java.base/java.lang.Thread.sleep(Thread.java:537)`, at the
/// `IllegalArgumentException` construction -- no leaf frames. A registered
/// `sleep` / `wait` threw it with the caller on top.
pub(in crate::runtime::stackwalker) const STANDIN_ARG_CHECK_THROWABLE: &str = "java/lang/IllegalArgumentException";
pub(in crate::runtime::stackwalker) const STANDIN_ARG_CHECK_METHODS: &[StandinMethod] = &[
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(J)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait",
        descriptor: "(J)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
        ..StandinMethod::ROW
    },
    // Round 14 wave 4 (lane trace3): `join(-1)` of a registered timed `join`
    // (`--compatible`); one construction site on JDK 17/21/25.
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "(J)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
        ..StandinMethod::ROW
    },
];

/// The one frame of an argument-check stand-in (see
/// [`STANDIN_ARG_CHECK_METHODS`]): the method at its ONLY
/// `invokespecial IllegalArgumentException.<init>` (a body with two such sites
/// is ambiguous and gets none), or at `(Native Method)` when the method is
/// itself `native` (JDK 17's `sleep(long)`). `CRATONVM_THROWABLE_STANDIN_ARG_CHECK_FRAMES=0`
/// turns it off.
pub(in crate::runtime::stackwalker) fn standin_arg_check_frames(
    store: &ClassStore,
    owner_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_reader::instruction::Instruction;
    let (class_id, class) = standin_rows(StandinFamily::ArgCheck)
        .iter()
        .filter(|m| m.name == name && m.descriptor == descriptor && standin_row_enabled(m))
        .find_map(|m| standin_declaring_class(store, owner_id, m))?;
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
    if method.is_native() {
        return Some(vec![BacktraceFrame::Entry(standin_entry(
            class, class_id, method, index, None,
        ))]);
    }
    let code = method.code()?;
    let bytes: &[u8] = &code.code;
    let mut site = None;
    let mut pc = 0usize;
    while pc < bytes.len() {
        let (instruction, next) = Instruction::decode(bytes, pc).ok()?;
        if let Instruction::Invokespecial(cp_index) = instruction {
            if let Some((owner, "<init>", _)) = cp_method_ref(&class.constant_pool, cp_index) {
                if owner == STANDIN_ARG_CHECK_THROWABLE {
                    if site.is_some() {
                        return None;
                    }
                    site = Some(pc);
                }
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    let site = site?;
    Some(vec![BacktraceFrame::Entry(standin_entry(
        class,
        class_id,
        method,
        index,
        Some(site),
    ))])
}

/// Lock-free pre-screen of the innermost frame: `false` when it certainly does
/// not stand at a census call. An interpreted frame must stand at an
/// `invokevirtual` / `invokestatic`, or at an `invokespecial` inside
/// `java.lang.Object` (javac's call of the private `wait0`); every other shape
/// is decided under the class store.
pub(crate) fn may_stand_at_standin_call(innermost: &BacktraceFrame) -> bool {
    match innermost {
        BacktraceFrame::Method { method, bci, .. } => {
            let Ok(bci) = usize::try_from(*bci) else {
                return false;
            };
            match method.code.get(bci).copied() {
                Some(0xb6 | 0xb8) => true,
                Some(0xb7) => &*method.class_name == "java/lang/Object",
                _ => false,
            }
        }
        _ => true,
    }
}

/// `(class, method, descriptor, bci)` of the frame whose call is decoded.
pub(in crate::runtime::stackwalker) fn standin_call_site(frame: &BacktraceFrame) -> Option<(ClassId, Arc<str>, Arc<str>, usize)> {
    let (class_id, name, descriptor, bci) = match frame {
        BacktraceFrame::Method {
            method,
            bci,
            class_id,
        } => (
            *class_id,
            Arc::clone(&method.method_name),
            Arc::clone(&method.method_descriptor),
            *bci,
        ),
        other => {
            let entry = other.to_entry();
            (
                entry.class_id?,
                entry.method_name,
                entry.method_descriptor?,
                entry.byte_code_index,
            )
        }
    };
    Some((class_id, name, descriptor, usize::try_from(bci).ok()?))
}

/// `(owner, name, descriptor)` of the method `CONSTANT_Methodref` (or
/// `InterfaceMethodref`) `index` names.
pub(crate) fn cp_method_ref(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    index: u16,
) -> Option<(&str, &str, &str)> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let (class_index, nat_index) = match cp.get(index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner = cp.get_class_name(class_index)?;
    let (name, descriptor) = cp.get_name_and_type(nat_index)?;
    Some((owner, name, descriptor))
}

/// The method the `invokevirtual` / `invokespecial` / `invokestatic` at `pc`
/// calls, or `None` for any other instruction.
pub(in crate::runtime::stackwalker) fn invoke_target_at<'a>(
    cp: &'a cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    pc: usize,
) -> Option<(&'a str, &'a str, &'a str)> {
    use cratonvm_reader::instruction::Instruction;
    match Instruction::decode(code, pc).ok()?.0 {
        Instruction::Invokevirtual(index)
        | Instruction::Invokespecial(index)
        | Instruction::Invokestatic(index) => cp_method_ref(cp, index),
        _ => None,
    }
}

/// The LAST call in `code` of a census method `screen` admits, with its pc,
/// the row, and the id of the row's class the call reaches. Last, because
/// where a JDK body has two (JDK 25 `wait(long)`: the virtual-thread branch,
/// then the platform one) the platform path is the later; a virtual thread's
/// screen ([`StandinScreen::first_call`], round 14 wave 5) takes the FIRST
/// instead. Any decode failure answers `None`: a
/// chain is rebuilt whole or not at all.
///
/// `reaches(owner, row)` answers the id of `row.class` when the call's
/// constant-pool `owner` resolves to it (round 14 wave 4, lane trace3, T4-2 of
/// `jit-r14-trace4-proposals-RETIRED-20260930.md`): javac qualifies an unqualified call by the
/// enclosing class (JLS 13.1), so `wait(delay)` inside `Thread.join(long)`
/// names `java/lang/Thread.wait(J)V`, which the literal compare it replaced
/// never matched. A call whose owner does not reach the row is not a census
/// call.
///
/// `zero_timeout` (round 14 wave 6, lane trace5): `Some(zero)` considers only
/// the calls whose argument is (`true`) or is not (`false`) an `lconst_0`
/// pushed right before the call -- the `Thread.join(long)` hop's pick between
/// its `wait(0)` and `wait(delay)` ([`StandinScreen::join_waits_zero`]).
pub(in crate::runtime::stackwalker) fn last_standin_call(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    screen: StandinScreen<'_>,
    zero_timeout: Option<bool>,
    reaches: &mut dyn FnMut(&str, &'static StandinMethod) -> Option<ClassId>,
) -> Option<(usize, &'static StandinMethod, ClassId)> {
    use cratonvm_reader::instruction::Instruction;
    let mut found = None;
    let mut pc = 0usize;
    let mut after_lconst_0 = false;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let argument_admitted = zero_timeout.is_none_or(|zero| zero == after_lconst_0);
        after_lconst_0 = matches!(instruction, Instruction::Lconst0);
        let index = match instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index)
                if argument_admitted =>
            {
                Some(index)
            }
            _ => None,
        };
        if let Some((owner, name, descriptor)) = index.and_then(|index| cp_method_ref(cp, index)) {
            let reached = screen
                .rows()
                .filter(|m| m.name == name && m.descriptor == descriptor && screen.admits(m))
                .find_map(|m| Some((m, reaches(owner, m)?)));
            if let Some((row, class_id)) = reached {
                found = Some((pc, row, class_id));
                // Round 14 wave 5 (lane trace4): a virtual thread's hop.
                if screen.first_call() {
                    return found;
                }
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    found}

/// A stand-in frame of `method` (slot `index` of `class`): at `call_pc` with
/// its line, or the native leaf (line -2) when `call_pc` is `None`.
pub(in crate::runtime::stackwalker) fn standin_entry(
    class: &Class,
    class_id: ClassId,
    method: &ClassFileMethod,
    index: usize,
    call_pc: Option<usize>,
) -> StackTraceEntry {
    let (line_number, byte_code_index) = match call_pc {
        Some(pc) => (
            line_number_for_bci_in_method(method, pc).unwrap_or(LINE_NUMBER_UNKNOWN),
            i32::try_from(pc).unwrap_or(-1),
        ),
        None => (LINE_NUMBER_NATIVE, -1),
    };
    StackTraceEntry {
        class_name: Arc::clone(&class.name),
        method_name: Arc::clone(&method.name),
        method_descriptor: Some(Arc::clone(&method.descriptor)),
        // HotSpot names the holder's source file on a native frame too
        // (`StackTraceElement.getFileName()` is "Thread.java").
        source_file: source_file_of_class(class),
        line_number,
        byte_code_index,
        class_id: Some(class_id),
        method_index: u32::try_from(index).ok(),
    }
}

/// From the class `owner_id` a call names, up the superclass chain to the
/// census class `target.class`; `None` when the chain does not reach it, or a
/// class on the way declares the method itself (a subclass's own
/// `static void sleep(long)` hides `Thread.sleep`).
pub(in crate::runtime::stackwalker) fn standin_declaring_class<'s>(
    store: &'s ClassStore,
    owner_id: ClassId,
    target: &StandinMethod,
) -> Option<(ClassId, &'s Class)> {
    let mut id = owner_id;
    for _ in 0..64 {
        let class = store.get(id)?;
        if &*class.name == target.class {
            return Some((id, class));
        }
        if class.find_method(target.name, target.descriptor).is_some() {
            return None;
        }
        id = class.superclass?;
    }
    None
}

/// The JDK frames the method the innermost captured frame was calling stands
/// in for, OUTERMOST-first (append them to an outermost-first trace), or
/// `None`. See the section comment above.
///
/// `resolve_class(requester, name)` resolves a class name as the requester's
/// loader would (the class manager's `find_class_by_name_for_class`). For a
/// platform thread, and (round 15 wave 1, [`virtual_standin_as_bound`]) for
/// this VM's virtual threads, which are bound threads running the platform
/// branches. A `java.lang.VirtualThread`'s `sleep` would run other JDK code
/// (`VirtualThread.sleepNanos`, `parkNanos`), and its `wait` leave from the
/// other branch ([`virtual_thread_standin_frames`]).
pub(crate) fn native_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    standin_frames_screened(
        store,
        innermost,
        throwable_class,
        StandinScreen::Throwable(throwable_class),
        resolve_class,
    )
}

/// Round 14 wave 5 (lane trace4; `r13w13-trace3-vm-served-jdk-methods-residuals`
/// item 5): [`native_standin_frames`] for a capture on a VIRTUAL thread. Its
/// `sleep` runs `VirtualThread.sleepNanos` (Java, which raises the
/// `InterruptedException` itself: no native leaf to rebuild), but its
/// `Object.wait` still ends in `wait0` -- JDK 25 `wait(long)` calls it from
/// the virtual-thread branch, the FIRST of its two calls; JDK 21 has one.
/// So only the `Object.wait` rows chain, each hop at its first census call;
/// HotSpot 25 for an interrupted registered `wait()` on a virtual thread:
/// `Object.wait0(Native Method)`, `Object.wait(Object.java:<first wait0
/// call's line>)`, `Object.wait(Object.java:...)`, caller. The argument-check
/// rows are unchanged (their check precedes the branch).
/// `CRATONVM_THROWABLE_STANDIN_VIRTUAL_WAIT_FRAMES=0` answers nothing for a
/// wait chain (read only once one was found). The `Array` rows chain here too
/// (their bodies do not branch on the thread).
///
/// Only a SERVED `wait` is rebuilt this way. A virtual thread on this VM is a
/// `ThreadBuilders$BoundVirtualThread` (`ContinuationSupport.isSupported0`
/// answers `false`), not a `java.lang.VirtualThread`, so where the real
/// `wait(long)` bytecode runs (`--real-jdk`) it takes the PLATFORM branch and
/// its own frame says so (`Object.java:389` on JDK 25, HotSpot's virtual
/// thread 382). That frame is what ran; it is not rewritten.
///
/// Round 15 wave 1 (lane trace; `r14w7-trace6-virtual-thread-frames-reference`):
/// the capture stopped asking this by default -- see
/// [`virtual_standin_as_bound`]. Interpreter round i1 wave 48, lane L1 made it
/// the default again for the throwable capture (HotSpot's DEFAULT virtual
/// threads are the reference: `vm_exec::virtual_standin_frames_as_bound`,
/// default off); `CRATONVM_THROWABLE_STANDIN_VIRTUAL_AS_BOUND=1` skips it.
pub(crate) fn virtual_thread_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    let frames = standin_frames_screened(
        store,
        innermost,
        throwable_class,
        StandinScreen::VirtualThrowable(throwable_class),
        resolve_class,
    )?;
    let is_wait_chain = throwable_class != STANDIN_ARG_CHECK_THROWABLE
        && frames
            .first()
            .is_some_and(|f| &*f.to_entry().class_name == "java/lang/Object");
    if is_wait_chain
        && false
    {
        return None;
    }
    Some(frames)
}

/// Round 15 wave 1 (lane trace; `r14w7-trace6-virtual-thread-frames-reference`,
/// owner decision: match HotSpot `-XX:-VMContinuations`): does a capture on a
/// VIRTUAL thread take the platform thread's stand-in rows
/// ([`native_standin_frames`], and the served timed join's
/// [`timed_join_standin_frames`]) rather than [`virtual_thread_standin_frames`]?
/// A virtual thread on this VM is a `ThreadBuilders$BoundVirtualThread`
/// (`ContinuationSupport.isSupported0` answers `false`), so the JDK bodies of
/// `sleep`, `wait` and `join` take their PLATFORM branch wherever their
/// bytecode runs (`--real-jdk`), exactly as on HotSpot without VM
/// continuations; the served methods (`--compatible`) now get the same rows,
/// and the two modes agree: a virtual `wait()` leaves `wait(long)` from its
/// platform `wait0` call (JDK 25 line 389, HotSpot default 382), a virtual
/// `sleep` gets `sleepNanos0` / `sleepNanos` / `sleep` (HotSpot default runs
/// `VirtualThread.sleepNanos` instead, which does not run here), and a
/// virtual thread's direct `join(long)` gets its `wait` loop.
/// `CRATONVM_THROWABLE_STANDIN_VIRTUAL_AS_BOUND=0` restores the round-14 rows
/// (`CRATONVM_THROWABLE_STANDIN_VIRTUAL_WAIT_FRAMES` applies only then).
///
/// Merge with interpreter round i1 wave 48 (lane L1), which measured against
/// HotSpot's DEFAULT virtual threads: the throwable capture's stand-in rows
/// read the same key default OFF (`vm_exec::virtual_standin_frames_as_bound`;
/// `=1` restores this function's platform rows there).
///
/// Round 17 wave 2 (lane smallfix): this is now the ONE reader of the key,
/// default OFF as its `flag_groups.rs` row says (`runtime_flag_on`), and
/// `vm_exec::virtual_standin_frames_as_bound` delegates here. Until then this
/// function read it default ON, so with the variable unset the throwable
/// rows answered "virtual" while the `Thread.run` middle frames (capture and
/// walks) and a parked virtual thread's published stack answered "bound".
/// Those three arms now answer "not bound" when the variable is unset; `=1`
/// restores them. What each arm prints and what HotSpot 25 prints:
/// `docs/feature-designs/real-jdk-mode.md`.
pub(crate) fn virtual_standin_as_bound() -> bool {
    false}

/// Interpreter round i1 wave 48, lane L1: the JDK frames of an
/// `InterruptedException` a virtual-thread sleep raised in the shape of
/// HotSpot's DEFAULT virtual threads (`VmConfig::virtual_sleep_continuation_shape`):
/// HotSpot 25 (continuations on) throws it in `VirtualThread.sleepNanos`,
/// `new InterruptedException()` before sleeping (its first such throw,
/// `during == false`) or after the park (its second), and lists
/// `VirtualThread.sleepNanos`, `Thread.sleepNanos` at its call of
/// `VirtualThread.sleepNanos`, `Thread.sleep` at its call of `sleepNanos`,
/// caller (`L1W48VirtualSleepPath`). This VM's virtual threads are
/// `BoundVirtualThread`s, whose `Thread.sleepNanos` takes the platform
/// branch to `sleepNanos0`; the frames are read from the real class bytes,
/// line numbers included.
///
/// Answers `(replacement for the innermost frame, frames to append
/// OUTERMOST-first)`:
/// * the innermost frame is a caller standing at a call of `Thread.sleep`
///   (the `sleep(long)` / `sleep(long, int)` stand-ins): no replacement;
///   append `sleep`, `sleepNanos`, `VirtualThread.sleepNanos`;
/// * the innermost frame is `Thread.sleepNanos` itself at its call of
///   `sleepNanos0` (its bytecode ran, `--real-jdk` or `sleep(Duration)`):
///   it is moved to its call of `VirtualThread.sleepNanos` (the branch
///   HotSpot's default takes) and `VirtualThread.sleepNanos` is appended.
///
/// `None` for any other shape, or when a class or call cannot be read
/// (`VirtualThread` not loaded): the caller falls back to the census.
pub(crate) fn virtual_sleep_continuation_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    during: bool,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<(Option<BacktraceFrame>, Vec<BacktraceFrame>)> {
    const THREAD: &str = "java/lang/Thread";
    const VIRTUAL_THREAD: &str = "java/lang/VirtualThread";
    const SLEEP_NANOS: (&str, &str) = ("sleepNanos", "(J)V");
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let caller_code = caller.code()?;
    let (owner, callee, callee_descriptor) =
        invoke_target_at(&caller_class.constant_pool, &caller_code.code, bci)?;
    // The first call in `method`'s code of `owner.name descriptor`.
    let first_call = |class: &Class, method: &ClassFileMethod, target: (&str, &str, &str)| {
        use cratonvm_reader::instruction::Instruction;
        let code = &method.code()?.code;
        let mut pc = 0usize;
        while pc < code.len() {
            let (instruction, next) = Instruction::decode(code, pc).ok()?;
            if let Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index) = instruction
            {
                if cp_method_ref(&class.constant_pool, index) == Some(target) {
                    return Some(pc);
                }
            }
            pc = next;
        }
        None
    };
    let entry = |class: &Class, id: ClassId, name: &str, descriptor: &str, pc: usize| {
        let (index, method) = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
        Some(BacktraceFrame::Entry(standin_entry(
            class,
            id,
            method,
            index,
            Some(pc),
        )))
    };
    let vt_call = (VIRTUAL_THREAD, SLEEP_NANOS.0, SLEEP_NANOS.1);
    let mut out = Vec::with_capacity(3);
    let (thread_id, replace) = if &*caller_class.name == THREAD
        && (&*caller_name, &*caller_descriptor) == SLEEP_NANOS
        && (owner, callee) == (THREAD, "sleepNanos0")
    {
        let pc = first_call(caller_class, caller, vt_call)?;
        let moved = entry(caller_class, caller_id, SLEEP_NANOS.0, SLEEP_NANOS.1, pc)?;
        (caller_id, Some(moved))
    } else if owner == THREAD && callee == "sleep" && matches!(callee_descriptor, "(J)V" | "(JI)V")
    {
        let thread_id = resolve_class(caller_id, THREAD)?;
        let thread = store.get(thread_id)?;
        let sleep = thread.find_method(callee, callee_descriptor)?;
        let pc = first_call(thread, sleep, (THREAD, SLEEP_NANOS.0, SLEEP_NANOS.1))?;
        out.push(entry(thread, thread_id, callee, callee_descriptor, pc)?);
        let sleep_nanos = thread.find_method(SLEEP_NANOS.0, SLEEP_NANOS.1)?;
        let pc = first_call(thread, sleep_nanos, vt_call)?;
        out.push(entry(thread, thread_id, SLEEP_NANOS.0, SLEEP_NANOS.1, pc)?);
        (thread_id, None)
    } else {
        return None;
    };
    let vt_id = resolve_class(thread_id, VIRTUAL_THREAD)?;
    let vt = store.get(vt_id)?;
    let vt_sleep = vt.find_method(SLEEP_NANOS.0, SLEEP_NANOS.1)?;
    // Its `new InterruptedException()` calls, in order: the check before the
    // sleep, then the one after the park.
    let init = ("java/lang/InterruptedException", "<init>", "()V");
    let throw_pc = {
        use cratonvm_reader::instruction::Instruction;
        let code = &vt_sleep.code()?.code;
        let mut seen = Vec::with_capacity(2);
        let mut pc = 0usize;
        while pc < code.len() {
            let (instruction, next) = Instruction::decode(code, pc).ok()?;
            if let Instruction::Invokespecial(index) = instruction {
                if cp_method_ref(&vt.constant_pool, index) == Some(init) {
                    seen.push(pc);
                }
            }
            pc = next;
        }
        *seen.get(usize::from(during))?
    };
    out.push(entry(vt, vt_id, SLEEP_NANOS.0, SLEEP_NANOS.1, throw_pc)?);
    Some((replace, out))
}



/// Round 14 wave 6 (lane trace5; TR5-3 of `jit-r14-trace5-proposals-RETIRED-20260930.md`): the
/// JDK frames of an `InterruptedException` a served DIRECT `Thread.join(long)`
/// raised (`--compatible`: `join(J)V` is registered), OUTERMOST-first, or
/// `None`. The frames alone cannot tell which `wait` loop HotSpot's
/// `join(long)` was in; the native can, and left `timed` (millis > 0) in
/// `JvmThread::served_timed_join_hint`. HotSpot 25, `t.join(500)`
/// interrupted: `Object.wait0(Native Method)`, `Object.wait(Object.java:389)`,
/// `Thread.join(Thread.java:1881)` (the timed loop's `wait(delay)`), caller;
/// `t.join(0)` leaves from the `wait(0)` loop (1887). The innermost captured
/// frame must stand at a call of `join(J)V` or `join(JI)V` itself (the VM
/// serves `join(long, int)` through the timed join with the rounded millis,
/// which is what HotSpot's `join(long, int)` hands `join(long)`).
pub(crate) fn timed_join_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    timed: bool,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let caller_code = caller.code()?;
    let (owner, name, descriptor) =
        invoke_target_at(&caller_class.constant_pool, &caller_code.code, bci)?;
    let served_join = name == "join" && matches!(descriptor, "(J)V" | "(JI)V");
    if !served_join {
        return None;
    }
    standin_chain(
        store,
        caller_id,
        (owner, name, descriptor),
        StandinScreen::TimedJoin { timed },
        resolve_class,
    )
}

/// `(owner, name, descriptor)` of the call the innermost captured frame stands
/// at, with the caller's class id (its constant pool names the owner).
pub(in crate::runtime::stackwalker) fn innermost_call_target<'s>(
    store: &'s ClassStore,
    innermost: &BacktraceFrame,
) -> Option<(ClassId, &'s Class, (&'s str, &'s str, &'s str))> {
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let code = caller.code()?;
    let target = invoke_target_at(&caller_class.constant_pool, &code.code, bci)?;
    Some((caller_id, caller_class, target))
}

/// Round 14 wave 7 (lane trace6; TW6-2 of `jit-r14-trace5w6-proposals-RETIRED-20260930.md`,
/// `r13w13-trace3-vm-served-jdk-methods-residuals` item 5): does the served
/// `Thread.join()` the innermost frame stands at decline the census's `wait`
/// chain for this `InterruptedException`? The native's hint (`declines`,
/// `JvmThread::served_timed_join_hint`) is `true` when its target is a
/// `java.lang.VirtualThread`, whose JDK `join(long)` runs
/// `VirtualThread.joinNanos` rather than the `wait` loops -- HotSpot's frames
/// there are `CountDownLatch` / AQS code the census does not describe, so the
/// capture keeps the caller on top rather than show loops that did not run.
/// The served timed joins decline by leaving no hint (`join(J)V` / `join(JI)V`
/// are no census entries). A `BoundVirtualThread` target (every "virtual"
/// thread on this VM) is not declined: its JDK body runs the platform loops.
pub(crate) fn served_join_declines_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    declines: bool,
) -> bool {
    declines
        && throwable_class == STANDIN_INTERRUPTED
        && innermost_call_target(store, innermost)
            .is_some_and(|(_, _, (_, name, descriptor))| name == "join" && descriptor == "()V")
}

/// Round 14 wave 7 (lane trace6; TW6-3 of `jit-r14-trace5w6-proposals-RETIRED-20260930.md`,
/// `r13w13-trace3-vm-served-jdk-methods-residuals` item 1): the `(long, int)`
/// overloads whose JDK body rejects a bad argument at TWO
/// `IllegalArgumentException` construction sites -- `millis < 0` first, the
/// `nanos` range second, in JDK 17, 21 and 25 alike -- which the one-site rule
/// ([`standin_arg_check_frames`]) refuses as ambiguous.
pub(in crate::runtime::stackwalker) const STANDIN_ARG_CHECK_TWO_SITE_METHODS: &[StandinMethod] = &[
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "(JI)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(JI)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
        ..StandinMethod::ROW
    },
    // Not `Object.wait(long, int)`: its native (`native_object_wait_timeout_nanos`)
    // is registered only by `register_synthetic_overrides`; on a real JDK its
    // bytecode runs and throws from its own frame.
];

/// The one frame of an `IllegalArgumentException` a served two-site overload
/// ([`STANDIN_ARG_CHECK_TWO_SITE_METHODS`]) raised, OUTERMOST-first, or `None`.
/// The native says which check failed (`second_check`: the hint it left,
/// `JvmThread::served_timed_join_hint`); the frame is the method at that
/// check's `invokespecial IllegalArgumentException.<init>`, with its line --
/// HotSpot, `t.join(0, -1)` on JDK 25: `Thread.join(Thread.java:1928)`, caller.
/// A body without exactly two sites answers nothing (fail-closed, as before);
/// a `native` overload is its own `(Native Method)` frame whichever check
/// failed. `CRATONVM_THROWABLE_STANDIN_ARG_CHECK_SITE_HINT=0` (or
/// `CRATONVM_THROWABLE_STANDIN_ARG_CHECK_FRAMES=0`) answers nothing.
pub(crate) fn served_arg_check_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    second_check: bool,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_reader::instruction::Instruction;
    if throwable_class != STANDIN_ARG_CHECK_THROWABLE {
        return None;
    }
    let (caller_id, caller_class, (owner, name, descriptor)) =
        innermost_call_target(store, innermost)?;
    if !standin_rows(StandinFamily::ArgCheckTwoSite)
        .iter()
        .any(|m| m.name == name && m.descriptor == descriptor)
    {
        return None;
    }
    let owner_id = if owner == &*caller_class.name {
        caller_id
    } else {
        resolve_class(caller_id, owner)?
    };
    let (class_id, class) = standin_rows(StandinFamily::ArgCheckTwoSite)
        .iter()
        .filter(|m| m.name == name && m.descriptor == descriptor && standin_row_enabled(m))
        .find_map(|m| standin_declaring_class(store, owner_id, m))?;
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
    if method.is_native() {
        return Some(vec![BacktraceFrame::Entry(standin_entry(
            class, class_id, method, index, None,
        ))]);
    }
    let code = method.code()?;
    let bytes: &[u8] = &code.code;
    let mut sites = [None; 2];
    let mut count = 0usize;
    let mut pc = 0usize;
    while pc < bytes.len() {
        let (instruction, next) = Instruction::decode(bytes, pc).ok()?;
        if let Instruction::Invokespecial(cp_index) = instruction {
            if let Some((STANDIN_ARG_CHECK_THROWABLE, "<init>", _)) =
                cp_method_ref(&class.constant_pool, cp_index)
            {
                if count >= sites.len() {
                    return None;
                }
                sites[count] = Some(pc);
                count += 1;
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    if count != sites.len() {
        return None;
    }
    let site = sites[usize::from(second_check)]?;
    Some(vec![BacktraceFrame::Entry(standin_entry(
        class,
        class_id,
        method,
        index,
        Some(site),
    ))])
}

/// Round 14 wave 7 (lane trace6; RV6-2 of `jit-r14-review6-proposals-RETIRED-20260930.md`):
/// `true` when the throwable is a `NullPointerException` and the innermost
/// frame, interpreted, stands at an `invokevirtual` / `invokespecial` /
/// `invokeinterface` -- the VM's own receiver NPE, or one a served instance
/// method raised. Neither stand-in rule can answer it: the census admits an
/// NPE only on its `Array` rows (static calls), and [`native_leaf_frame`]
/// refuses a non-static call's NPE. Decided from the opcode alone, before
/// either decodes the call or resolves its owner. Compiled innermost frames
/// are decided under the class store, by
/// [`compiled_innermost_gets_no_standin_frames`] (round 15 wave 5).
/// `CRATONVM_THROWABLE_RECEIVER_NPE_SCREEN=0` turns it off (read only once a
/// frame matched).
pub(crate) fn receiver_npe_gets_no_standin_frames(
    innermost: &BacktraceFrame,
    throwable_class: &str,
) -> bool {
    let BacktraceFrame::Method { method, bci, .. } = innermost else {
        return false;
    };
    throwable_class == "java/lang/NullPointerException"
        && usize::try_from(*bci).is_ok_and(|bci| code_is_instance_invoke_at(&method.code, bci))
}

/// Round 15 wave 5 (lane throw1; `r15w5-throw1-plain-throw-costs-3x-hotspot-CLOSED-20260930.md`
/// cost item C5): the screens above for a COMPILED or INLINED innermost frame,
/// from its opcode decoded ONCE. `true` when no stand-in rule can answer.
///
/// [`may_stand_at_standin_call`] and [`may_stand_at_native_call`] answer
/// `true` for every compiled frame (its code is not at hand without the store),
/// and that is the frame every throw out of compiled code has on top -- a
/// compiled `checkcast`'s CCE, a compiled null receiver's NPE. Each rule then
/// decoded the call site on its own: [`native_standin_frames`] (for the census
/// throwables, NPE among them), [`native_leaf_frame`] and
/// [`callback_native_leaf_frames`], each through [`standin_call_site`] (the
/// label parsed into an entry), the class, a by-name method scan and the code,
/// to find an opcode none of them takes.
///
/// Exact: every rule resolves the call site exactly as [`innermost_call_shape`]
/// does and answers `None` when a step fails; [`native_standin_frames`],
/// [`timed_join_standin_frames`], [`served_arg_check_standin_frames`] and
/// [`served_join_declines_standin_frames`] need an `invokevirtual` /
/// `invokespecial` / `invokestatic` there ([`invoke_target_at`]),
/// [`native_leaf_frame`] the same three, [`callback_native_leaf_frames`] an
/// `invokestatic`. A call of an `<init>` -- where every explicit
/// `throw new X(..)` out of compiled code stands -- names no census row, no
/// argument-check row, no `join` and no `forName`, and [`native_leaf_frame`]
/// refuses a `<`-name. And an NPE at an `invokevirtual` / `invokespecial` is
/// the receiver shape [`receiver_npe_gets_no_standin_frames`] refuses for an
/// interpreted frame, for the reason it gives there: the census admits an NPE
/// only on its static `Array` rows and [`native_leaf_frame`] refuses a
/// non-static call's NPE. An interpreted innermost frame answers `false`
/// (its own screens above decide it). `CRATONVM_THROWABLE_STANDIN_OPCODE_SCREEN=0`
/// answers `false` always.
pub(crate) fn compiled_innermost_gets_no_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
) -> bool {
    if matches!(innermost, BacktraceFrame::Method { .. })
        || !crate::runtime::exceptions::throw1_switches().standin_opcode_screen
    {
        return false;
    }
    match innermost_call_shape(store, innermost) {
        Some((_, true)) => true,
        Some((0xb8, false)) => false,
        Some((0xb6 | 0xb7, false)) => throwable_class == "java/lang/NullPointerException",
        _ => true,
    }
}

/// JIT round 19 wave 5, lane throws5 (`CRATONVM_THROWABLE_STANDIN_FILL_CALL_SCREEN=1`,
/// opt-in): does `innermost` stand at an `invokevirtual` / `invokespecial` of
/// a no-argument `fillInStackTrace` returning a reference -- an explicit
/// refill, `t.fillInStackTrace()` (the frame below the trimmed
/// `Throwable.fillInStackTrace`)?
///
/// Such a frame was refused by [`compiled_innermost_gets_no_standin_frames`]
/// for every throwable but an NPE (an instance call may be a census or a
/// native-leaf call), so every refill ran the stand-in rules under the
/// class-manager lock AND, since "nothing appended" from those rules is not
/// a memoisable answer, never made a capture memo: each refill paid a whole
/// capture where a repeated construction hits the memo
/// (`R19Throws3Shapes refill-only` 2.6x HotSpot, `new-only` at parity).
/// Of the rules, only [`native_leaf_frame`] can answer a call of this name and
/// shape: no census, argument-check, `join` or member-lookup row is named
/// `fillInStackTrace` (asserted by `r19w5_throws5_fill_call_screen_tests`), and
/// [`callback_native_leaf_frames`] needs an `invokestatic`. The caller runs
/// `native_leaf_frame` itself (a `native fillInStackTrace()` override) and
/// screens only on its `None`.
pub(crate) fn innermost_calls_fill_in_stack_trace(
    store: &ClassStore,
    innermost: &BacktraceFrame,
) -> bool {
    use cratonvm_reader::instruction::Instruction;
    let Some((caller_id, caller_name, caller_descriptor, bci)) = standin_call_site(innermost)
    else {
        return false;
    };
    let Some(caller_class) = store.get(caller_id) else {
        return false;
    };
    let Some(code) = caller_class
        .find_method(&caller_name, &caller_descriptor)
        .and_then(|m| m.code())
    else {
        return false;
    };
    let cp_index = match Instruction::decode(&code.code, bci).map(|d| d.0) {
        Ok(Instruction::Invokevirtual(index) | Instruction::Invokespecial(index)) => index,
        _ => return false,
    };
    cp_method_ref(&caller_class.constant_pool, cp_index).is_some_and(|(_, name, descriptor)| {
        name == "fillInStackTrace" && descriptor.starts_with("()L")
    })
}


/// `CRATONVM_THROWABLE_STANDIN_FILL_CALL_SCREEN=1` (JIT round 19 wave 5, lane
/// throws5; default ON since the round-19 proposal round: refill 1977 -> 637 ns; `=0` turns it off). Read per capture that reaches the
/// stand-in rules at a refill site -- a memo miss only, once the memo holds the
/// site -- so no process static is kept for it.
pub(crate) fn standin_fill_call_screen_enabled() -> bool {
    true
}

/// The opcode at the bci `frame` stands at, resolved as every stand-in rule
/// resolves its call site ([`standin_call_site`], the caller's class, the
/// method by name and descriptor, its `Code`), and whether it is an
/// `invokevirtual` / `invokespecial` / `invokestatic` of a `<`-named method
/// ([`invoke_target_at`], as the rules decode it); `None` where one of those
/// steps fails, i.e. where every rule fails too.
pub(in crate::runtime::stackwalker) fn innermost_call_shape(store: &ClassStore, frame: &BacktraceFrame) -> Option<(u8, bool)> {
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(frame)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let code = caller.code()?;
    let opcode = code.code.get(bci).copied()?;
    let calls_initializer = matches!(opcode, 0xb6..=0xb8)
        && invoke_target_at(&caller_class.constant_pool, &code.code, bci)
            .is_some_and(|(_, name, _)| name.starts_with('<'));
    Some((opcode, calls_initializer))
}

/// Ways in a [`StandinSiteMemo`] (a throw loop alternating a few sites keeps
/// all of them).
pub(in crate::runtime::stackwalker) const STANDIN_SITE_MEMO_WAYS: usize = 4;

/// JIT round 16 wave 11 (lane throw2, TP16-10 of `jit-r16-throwpath-proposals-RETIRED-20261006.md`):
/// one thread's recent "no stand-in frames" answers of
/// [`compiled_innermost_gets_no_standin_frames`], per throw site.
///
/// Every throw out of compiled code has a compiled or inlined innermost frame,
/// and every such capture took the class-manager read lock, looked the
/// throwable class up, parsed the frame's label into an entry (four `Arc<str>`
/// allocations), ran a by-name `Class::find_method` and decoded the opcode at
/// the bci -- to learn, for `throw new X(..)`, that the frame stands at an
/// `<init>` call. The decoded answer is a function of the frame (its label,
/// owner / class id, bci and `cp_stamp`), the throwable's class and the class
/// store's method tables, which change only by redefinition (class ids are
/// never reused). So a `true` answer is remembered under exactly that key plus
/// `class_redefinition_count()`, and a later capture with an equal key answers
/// `true` without the lock. A `false` answer is never remembered (the stand-in
/// rules then run, as before), nor is any frame that is not `Compiled` /
/// `Inlined`.
///
/// `CRATONVM_THROWABLE_STANDIN_SITE_MEMO=1`, default OFF until measured. Kept
/// per `JvmThread`; holds a label refcount per way and no heap reference.
#[derive(Default)]
pub(crate) struct StandinSiteMemo {
    pub(in crate::runtime::stackwalker) ways: [Option<StandinSiteKey>; STANDIN_SITE_MEMO_WAYS],
    /// The way the next [`Self::remember`] overwrites (round robin).
    pub(in crate::runtime::stackwalker) next: usize,
}

/// What a [`StandinSiteMemo`] way answers for.
pub(in crate::runtime::stackwalker) struct StandinSiteKey {
    pub(in crate::runtime::stackwalker) redefinitions: u64,
    pub(in crate::runtime::stackwalker) throwable_class: ClassId,
    /// A `Compiled` or `Inlined` frame (its label shared with the artifact).
    pub(in crate::runtime::stackwalker) frame: BacktraceFrame,
}

impl StandinSiteMemo {
    /// `true` when a way remembers that `innermost`, filling a `throwable_class`
    /// with `redefinitions` redefinitions seen, gets no stand-in frames.
    pub(crate) fn answers(
        &self,
        redefinitions: u64,
        throwable_class: ClassId,
        innermost: &BacktraceFrame,
    ) -> bool {
        self.ways.iter().flatten().any(|way| {
            way.redefinitions == redefinitions
                && way.throwable_class == throwable_class
                && same_standin_site(&way.frame, innermost)
        })
    }

    /// Remember that `innermost` gets no stand-in frames for `throwable_class`
    /// (the caller's [`compiled_innermost_gets_no_standin_frames`] answered
    /// `true`). Ignored for a frame that is not `Compiled` / `Inlined`.
    pub(crate) fn remember(
        &mut self,
        redefinitions: u64,
        throwable_class: ClassId,
        innermost: &BacktraceFrame,
    ) {
        if !matches!(
            innermost,
            BacktraceFrame::Compiled { .. } | BacktraceFrame::Inlined { .. }
        ) || self.answers(redefinitions, throwable_class, innermost)
        {
            return;
        }
        let way = self.next % STANDIN_SITE_MEMO_WAYS;
        self.ways[way] = Some(StandinSiteKey {
            redefinitions,
            throwable_class,
            frame: innermost.clone(),
        });
        self.next = way + 1;
    }
}

/// Do two compiled / inlined frames stand at the same site, i.e. would
/// [`standin_call_site`] read the same `(class id, name, descriptor, bci)`
/// from both? Every field takes part (the label by content: two artifacts of
/// one method share no `Arc`). Any other frame shape answers `false`.
pub(in crate::runtime::stackwalker) fn same_standin_site(a: &BacktraceFrame, b: &BacktraceFrame) -> bool {
    let same_label = |x: &Arc<str>, y: &Arc<str>| Arc::ptr_eq(x, y) || **x == **y;
    match (a, b) {
        (
            BacktraceFrame::Compiled {
                label: la,
                owner_class_id: oa,
                bci: ba,
                cp_stamp: ca,
            },
            BacktraceFrame::Compiled {
                label: lb,
                owner_class_id: ob,
                bci: bb,
                cp_stamp: cb,
            },
        ) => oa == ob && ba == bb && ca == cb && same_label(la, lb),
        (
            BacktraceFrame::Inlined {
                label: la,
                class_id: ia,
                bci: ba,
                cp_stamp: ca,
                // Wave 123 (L3): not part of the site. `standin_call_site`
                // reads `(class id, name, descriptor, bci)` only, and a
                // single-pass splice (stamped) and an optimizing-tier one
                // (unstamped) of the same callee stand at the same site --
                // comparing the stamp would split them and miss the memo.
                hidden: _,
            },
            BacktraceFrame::Inlined {
                label: lb,
                class_id: ib,
                bci: bb,
                cp_stamp: cb,
                hidden: _,
            },
        ) => ia == ib && ba == bb && ca == cb && same_label(la, lb),
        _ => false,
    }
}

/// Ways in a [`ThrowableIsAMemo`].
pub(in crate::runtime::stackwalker) const THROWABLE_IS_A_MEMO_WAYS: usize = 8;

/// JIT round 16 wave 11 (lane throw2, TP16-12): one thread's recent answers of
/// the fill-frame trim's `throwable->is_a(holder)` ([`throwable_holder_is_a`]),
/// for frames that carry their holder's class id.
///
/// The trim asks it for every `fillInStackTrace` / `<init>` frame on top of a
/// capture -- three or four per `new X(..)` -- and each ask took the
/// class-manager read lock (once per capture) and walked the throwable's
/// superclass chain through the class store. The answer for a `(throwable
/// class, holder class)` pair of ids cannot change: class ids are never
/// reused and a class's supertypes are fixed from definition (a redefinition
/// that changes them is refused). The key carries `class_redefinition_count()`
/// all the same, so a redefinition empties the memo in effect. Holder-by-name
/// asks (an entry without a class id) are never memoised, nor is an ask whose
/// throwable class the store does not hold.
///
/// With [`StandinSiteMemo`] also on, a steady-state throw out of compiled code
/// takes no class-manager lock in the capture at all.
/// `CRATONVM_THROWABLE_TRIM_IS_A_MEMO=1`, default OFF until measured. Kept per
/// `JvmThread`.
#[derive(Default)]
pub(crate) struct ThrowableIsAMemo {
    /// `(redefinitions, throwable class, holder class, answer)`.
    pub(in crate::runtime::stackwalker) ways: [Option<(u64, ClassId, ClassId, bool)>; THROWABLE_IS_A_MEMO_WAYS],
    pub(in crate::runtime::stackwalker) next: usize,
}

impl ThrowableIsAMemo {
    /// The remembered answer for `(throwable_class, holder)`, if any.
    pub(crate) fn lookup(
        &self,
        redefinitions: u64,
        throwable_class: ClassId,
        holder: ClassId,
    ) -> Option<bool> {
        self.ways
            .iter()
            .flatten()
            .find(|(r, t, h, _)| *r == redefinitions && *t == throwable_class && *h == holder)
            .map(|way| way.3)
    }

    /// Remember the answer the class store gave for `(throwable_class, holder)`.
    pub(crate) fn remember(
        &mut self,
        redefinitions: u64,
        throwable_class: ClassId,
        holder: ClassId,
        answer: bool,
    ) {
        if self
            .lookup(redefinitions, throwable_class, holder)
            .is_some()
        {
            return;
        }
        let way = self.next % THROWABLE_IS_A_MEMO_WAYS;
        self.ways[way] = Some((redefinitions, throwable_class, holder, answer));
        self.next = way + 1;
    }
}

/// `code[bci]` is an `invokevirtual` / `invokespecial` / `invokeinterface`.
pub(in crate::runtime::stackwalker) fn code_is_instance_invoke_at(code: &[u8], bci: usize) -> bool {
    matches!(code.get(bci).copied(), Some(0xb6 | 0xb7 | 0xb9))
}

/// The body of [`native_standin_frames`] / [`virtual_thread_standin_frames`];
/// `screen` picks the chain rows.
pub(in crate::runtime::stackwalker) fn standin_frames_screened(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    screen: StandinScreen<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    if !is_standin_throwable(throwable_class) {
        return None;
    }
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let caller_code = caller.code()?;
    let (owner, name, descriptor) =
        invoke_target_at(&caller_class.constant_pool, &caller_code.code, bci)?;
    // Screen by name first: every compiled `new IllegalArgumentException`
    // reaches here standing at its own `<init>` call.
    if throwable_class == STANDIN_ARG_CHECK_THROWABLE
        && standin_rows(StandinFamily::ArgCheck)
            .iter()
            .any(|m| m.name == name && m.descriptor == descriptor)
    {
        let owner_id = resolve_class(caller_id, owner)?;
        return standin_arg_check_frames(store, owner_id, name, descriptor);
    }
    // Round 14 wave 5 (lane trace4): no chain can start at a call that names
    // no entry row -- decided before any class is resolved (a receiver
    // `NullPointerException` at any invoke reaches here since the `Array`
    // rows admit NPE). An `IllegalArgumentException` at a call that is no
    // argument-check row may still start one (`Array.newInstance`).
    if !standin_rows(StandinFamily::Census)
        .iter()
        .any(|m| m.entry && m.name == name && m.descriptor == descriptor)
    {
        return None;
    }
    standin_chain(
        store,
        caller_id,
        (owner, name, descriptor),
        screen,
        resolve_class,
    )
}

/// The chain of [`native_standin_frames`] from the call `callee` (`(owner,
/// name, descriptor)` as the constant pool of class `caller_id` names it),
/// for the rows `screen` admits.
pub(in crate::runtime::stackwalker) fn standin_chain(
    store: &ClassStore,
    caller_id: ClassId,
    callee: (&str, &str, &str),
    screen: StandinScreen<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    let (owner, name, descriptor) = callee;
    let owner_id = resolve_class(caller_id, owner)?;
    let (class_id, class) = screen
        .rows()
        .filter(|m| {
            screen.enters_at(m)
                && m.name == name
                && m.descriptor == descriptor
                && screen.admits(m)
                && standin_row_enabled(m)
        })
        .find_map(|m| standin_declaring_class(store, owner_id, m))?;
    standin_chain_from(
        store,
        (class_id, class),
        (name, descriptor),
        screen,
        resolve_class,
    )
}

/// The keyed body of [`standin_chain`] (interpreter round i2 wave 61, lane
/// L3): the chain from the census method `(name, descriptor)` of `class`
/// (id `class_id`), already screened as an entry row the screen admits.
pub(in crate::runtime::stackwalker) fn standin_chain_from<'s>(
    store: &'s ClassStore,
    (mut class_id, mut class): (ClassId, &'s Class),
    (name, descriptor): (&str, &str),
    screen: StandinScreen<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    let (mut name, mut descriptor) = (name, descriptor);
    let mut out = Vec::new();
    for _ in 0..STANDIN_CHAIN_MAX {
        let (index, method) = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
        if method.is_native() {
            out.push(BacktraceFrame::Entry(standin_entry(
                class, class_id, method, index, None,
            )));
            return Some(out);
        }
        let code = method.code()?;
        let current_id = class_id;
        let current_name: &str = &class.name;
        let mut reaches = |owner: &str, row: &'static StandinMethod| -> Option<ClassId> {
            if owner == row.class {
                // The literal owner (every JDK sleep / wait hop): as before.
                if owner == current_name {
                    return Some(current_id);
                }
                let id = resolve_class(current_id, owner)?;
                return (store.get(id).map(|c| &*c.name) == Some(row.class)).then_some(id);
            }
            // T4-2: an owner naming a class below the row's
            // (`java/lang/Thread.wait(J)V`), read only once a call matched a
            // row by name and descriptor.
            let owner_id = if owner == current_name {
                current_id
            } else {
                resolve_class(current_id, owner)?
            };
            standin_declaring_class(store, owner_id, row).map(|(id, _)| id)
        };
        // Round 14 wave 6 (lane trace5): the `Thread.join(long)` hop leaves
        // from the `wait` its argument names, not the last one.
        // `CRATONVM_THROWABLE_STANDIN_TIMED_JOIN_FRAMES=0` restores the last.
        let zero_timeout = (current_name == "java/lang/Thread"
            && name == "join"
            && descriptor == "(J)V"
            && true)
        .then(|| screen.join_waits_zero());
        let (call_pc, next, next_id) = last_standin_call(
            &class.constant_pool,
            &code.code,
            screen,
            zero_timeout,
            &mut reaches,
        )?;
        out.push(BacktraceFrame::Entry(standin_entry(
            class,
            class_id,
            method,
            index,
            Some(call_pc),
        )));
        class_id = next_id;
        class = store.get(class_id)?;
        if &*class.name != next.class {
            return None;
        }
        name = next.name;
        descriptor = next.descriptor;
    }
    None
}

// ---------------------------------------------------------------------------
// Reflection frames: the JDK frames a reflective call runs without
// ---------------------------------------------------------------------------
//
// Interpreter round i1 wave 43, lane L3
// (`docs/feature-designs/real-jdk-mode.md`;
// what is still not listed:
// `docs/feature-designs/real-jdk-mode.md`).
// The VM serves
// `Method.invoke` and `Constructor.newInstance` with natives (both modes; the
// interpreter's cached-invoke door keeps them native,
// `dispatch_virtual::native_override_for_cached_reflect_invoke`), so the
// target's frame sat directly on its caller's. HotSpot, JDK 25, for a static
// `R.dump` reached by `Method.invoke` from `R.main` (a `Throwable`'s trace,
// `Thread.getStackTrace` and a `SHOW_REFLECT_FRAMES` walk alike):
//
// ```text
// R.dump(R.java:5)
// java.base/jdk.internal.reflect.DirectMethodHandleAccessor.invoke(DirectMethodHandleAccessor.java:104)
// java.base/java.lang.reflect.Method.invoke(Method.java:565)
// R.main(R.java:13)
// ```
//
// and for a constructor `DirectConstructorHandleAccessor.newInstance`,
// `Constructor.newInstanceWithCaller` and `Constructor.newInstance`. The
// method handle frames between the accessor and the target are hidden
// (`@Hidden` / hidden classes) and are not listed. The natives record the call
// on the thread (`JvmThread::reflective_calls`: the interpreter depth and the
// JIT entry-chain length at the call); a capture lists the JDK frames, each at
// its call to the next (its `LineNumberTable` line, found in the real class
// bytes as the stand-in frames above are), right below the first frame the
// call put on the stack: the first interpreter frame it pushed, or, since
// wave 44, a compiled activation its target entered without one (a compiled
// activation names the JIT entry-chain entry it came from,
// `ActiveCompiledFrame::chain_index`, so the ones entered after the call are
// told from the call's compiled callers: `trace_anchor_position`).
// Fail-closed, the capture lists nothing for a call when:
//
// * nothing was pushed above it and the capture is not a throwable's (a
//   throwable the native itself raised, `InvocationTargetException` or an
//   argument check's `IllegalArgumentException`, lists the call on top since
//   waves 45 and 46, at the lines HotSpot's frames stand at:
//   [`reflective_raise_entries`], [`reflective_check_entries`]);
// * a class or method the frames name is not loaded or not found (JDK 17's
//   reflection has other frames).

/// One reflective call a thread is inside (`JvmThread::reflective_calls`):
/// where its JDK frames go in a capture of that thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReflectiveCallRow {
    /// The thread's interpreter depth at the call: the first interpreter
    /// frame the call pushed is `frames[interp_depth]`.
    pub interp_depth: u32,
    /// The thread's JIT entry-chain length at the call
    /// (`conservative_roots::current_thread_jit_depth`): entries below it are
    /// the call's compiled callers.
    pub jit_depth: u32,
    /// Which call.
    pub kind: cratonvm_native_api::ReflectiveCallKind,
    /// The argument check of the call that failed, noted by its native right
    /// before it builds the check's throwable inside the call
    /// (`NativeExceptionAccess::note_reflective_check`; `None`: none failed).
    /// A capture of that throwable lists the check's frames
    /// ([`reflective_check_entries`]). Interpreter round i1 wave 46, lane L3.
    pub check: Option<cratonvm_native_api::ReflectiveCheck>,
    /// The class of the throwable the call's target threw, noted by the
    /// native right before it wraps it in an `InvocationTargetException`
    /// (`NativeExceptionAccess::note_reflective_raise_target`; `None`: not
    /// noted). The wrapper's capture picks the accessor's `catch` arm from it
    /// when the wrapper's constructor pushed no frame to read it from
    /// (`--compatible`'s stub constructor). Interpreter round i1 wave 48,
    /// lane L3.
    pub raised: Option<ClassId>,
    /// The door record this call's door replaced, while the door is in
    /// flight: the served `Method.invoke` notes the static JDK method its
    /// `invoke_by_class_id` door enters on the thread's door record
    /// (`JvmThread::served_door_in_flight`, [`DoorInFlight`]) right before the
    /// door, and restores the outer record from here right after it
    /// (`NativeExceptionAccess::note_reflective_target`). `None`: no door of
    /// this call in flight. A capture made while the record stands, with
    /// nothing but the throwable's construction pushed above the call, lists
    /// that method's JDK frames above the call's ([`keyed_served_raise_frames`]):
    /// a throwable a callee of the served method built inside its native (the
    /// served loader's `ClassNotFoundException` under `Class.forName(String,
    /// boolean, ClassLoader)`).
    ///
    /// Interpreter round i2 wave 64, lane L3 (proposal
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// replaces wave 62's `target` (the method itself, on the row: a second
    /// carrier the capture had to choose against the record). One carrier,
    /// one screen, one read in the capture.
    pub door_save: Option<Option<DoorInFlight>>,
    /// A served `MethodHandle.invokeWithArguments` row only: the slot of the
    /// thread's native pin stack (`JvmThread::native_pin_roots`) that holds
    /// the ADAPTER the served call runs, `asType(genericMethodType(n))` (the
    /// `asTypeCache` entry, or the handle itself), while the served lanes run
    /// it (`NativeInvokeAccess::note_invoke_with_arguments_adapter`; `None`:
    /// none noted). Read by the thread's own walk
    /// (`invoke_with_arguments_adapters`) for the adapter's form and
    /// customization; meaningless to any other thread. Interpreter round i7
    /// wave 136, lane L4 (page
    /// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, residuals 1-2).
    pub adapter_pin: Option<u32>,
}

/// The static JDK method a by-name door is entering (`JvmThread::served_door_in_flight`),
/// set by the door right before its call and restored to the outer door's
/// record right after (`vm_exec`'s static doors, `NativeInvokeAccess::invoke_static*`).
/// Interpreter round i2 wave 63, lane L3 (`VmConfig::served_raise_door_key`,
/// proposal
/// `docs/feature-designs/real-jdk-mode.md`):
/// for every static door -- a method handle's `REF_invokeStatic` door above
/// all, whose caller's `invokeExact` names no method -- and, since wave 64,
/// for the served `Method.invoke`'s door too ([`ReflectiveCallRow::door_save`]). A capture made while it stands,
/// with nothing but the throwable's construction above `interp_depth` and no
/// compiled activation entered since, lists the method's JDK frames
/// ([`keyed_served_raise_frames`]) above the door's caller.
///
/// Inline (20 bytes as an `Option`, no niche) at the END of `JvmThread`, in
/// the tail padding the layout ratchet's 64-byte step leaves: no hot field
/// moves and no door allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoorInFlight {
    /// The method the door selected, as `(class, method slot)`; `None` when
    /// the selection is nothing the keyed families answer (an unloaded
    /// owner, an abstract method): the innermost door then keys nothing,
    /// whatever an outer door noted.
    pub key: Option<(ClassId, u32)>,
    /// The thread's interpreter depth at the door: the first interpreter
    /// frame the door's call pushed is `frames[interp_depth]`.
    pub interp_depth: u32,
    /// The thread's JIT entry-chain length at the door
    /// (`conservative_roots::current_thread_jit_depth`).
    pub jit_depth: u32,
}

/// Does a door on `class_name` (internal form) note a [`DoorInFlight`]? The
/// screen the served `Method.invoke` applies to its row note and the
/// caller-sensitive adapter: a `java/` or `jdk/` class, the only classes a
/// native serves. One prefix test, so an application door pays a branch.
#[inline]
pub(crate) fn door_in_flight_screens(class_name: &str) -> bool {
    class_name.starts_with("java/") || class_name.starts_with("jdk/")
}

/// Is the reflective `call`'s own door in flight and keyed, the thread's door
/// record standing as `record`? Its door noted ([`ReflectiveCallRow::door_save`])
/// and the record the innermost: the call's own (at the call's depth, a
/// method it resolved) or a door entered inside the call (deeper). A
/// throwable built above such a call is its target's, not the accessor's
/// own raise (`vm_exec::attach_reflective_raise`). Interpreter round i2 wave
/// 64, lane L3 (wave 62's `target.is_some()`).
pub(crate) fn reflective_door_keyed(
    call: &ReflectiveCallRow,
    record: Option<DoorInFlight>,
) -> bool {
    call.door_save.is_some()
        && record.is_some_and(|door| {
            (door.interp_depth, door.jit_depth) > (call.interp_depth, call.jit_depth)
                || door.key.is_some()
        })
}

/// One entry of a [`DoorKeyMemo`]: a door's selection inputs and the method
/// it selected.
#[derive(Debug)]
pub(in crate::runtime::stackwalker) struct DoorKeyMemoEntry {
    /// The owner the selection started from, as the door named it.
    pub(in crate::runtime::stackwalker) owner: DoorKeyMemoOwner,
    pub(in crate::runtime::stackwalker) method: Box<str>,
    pub(in crate::runtime::stackwalker) descriptor: Box<str>,
    pub(in crate::runtime::stackwalker) key: Option<(ClassId, u32)>,
}

/// What a door names its owner by, as a [`DoorKeyMemo`] keeps it.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::runtime::stackwalker) enum DoorKeyMemoOwner {
    Id(ClassId),
    /// A class name and the class whose loader resolved it (the innermost
    /// interpreted frame's class at the door).
    Name(Box<str>, Option<ClassId>),
}

/// What a door names its owner by, borrowed, for [`DoorKeyMemo::lookup`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum DoorKeyOwnerRef<'a> {
    Id(ClassId),
    Name(&'a str, Option<ClassId>),
}

