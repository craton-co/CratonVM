#[allow(unused_imports)]
use super::*;

/// `(cause, planted)` for every trap cause, always all three rows.
///
/// All three even when zero, because a zero on one row cannot be told from
/// "this workload has no such site" unless the other rows are beside it -- the
/// distinction this file has been burned by often enough to make a rule of.
pub fn ir_trap_census() -> [(&'static str, u64); TrapCause::COUNT] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        (TrapCause::Indy.as_str(), TRAPS_PLANTED[0].load(Relaxed)),
        (
            TrapCause::UnresolvedTypeCheck.as_str(),
            TRAPS_PLANTED[1].load(Relaxed),
        ),
        (
            TrapCause::UnresolvedNew.as_str(),
            TRAPS_PLANTED[2].load(Relaxed),
        ),
        (
            TrapCause::UninitNew.as_str(),
            TRAPS_PLANTED[3].load(Relaxed),
        ),
        (
            TrapCause::UnresolvedField.as_str(),
            TRAPS_PLANTED[4].load(Relaxed),
        ),
    ]
}

/// May a site this tier cannot lower become an uncommon trap, instead of
/// refusing the whole method? **Default ON**; `CRATONVM_JIT_IR_SITE_TRAP=0` is
/// the kill switch and restores the method-level refusal at every caller.
///
/// Measured on the H2 JDBC workload before this existed: 53 methods refused for
/// an `invokedynamic`, 34 for a `checkcast`/`instanceof` on an unloaded class,
/// and 13 for a `new` of one -- 100 methods, none of which had anything wrong
/// with the code the optimizing tier would actually have run.
///
/// THIS WAS BRIEFLY FLIPPED OFF ON 2026-09-07 AND THE FLIP WAS WRONG. Thirty
/// hibernate-reactive classes then read `ok=182 failed=7` with traps on against
/// `ok=241 failed=0` with them off, and 36 `InternalError: ... refusing
/// side-effecting replay` -- so the trap looked like the defect. It was only the
/// TRIGGER. The cause was a deopt SINK that aborted on a trapped frame its
/// sibling sink resumed, fixed the same day by another session
/// (`CRATONVM_JIT_DEOPT_SINK_RESUME`, default ON). Re-measured on a binary
/// carrying that fix:
///
/// ```text
///   site traps ON  + sink fix    ok=239  failed=0  InternalError=0
///   site traps OFF + sink fix    ok=241  failed=0  InternalError=0
/// ```
///
/// Zero errors either way, so there is nothing here to switch off. The lesson
/// kept is about the measurement, not the flag: an A/B with ONE lever proves
/// that lever is on the path to the failure, and says nothing about whether it
/// is the defect. A trigger and a cause both answer to a kill switch.
///
/// Round 16 (lane indy): the `invokedynamic` half of this trade is now
/// optional per site -- see [`ir_indy_bridge_enabled`].
///
/// Read per call (JIT round 19 wave 11, lane tail11): it used to be cached in
/// a process `OnceLock`, so a test or an embedding that changed the switch kept
/// the first answer while its per-build neighbours followed the change -- and
/// the planner and the builder could then disagree about the unloaded-static
/// exit (`r19-unloaded-static-call-trap-planner-and-builder-gate-differently`).
/// Same words as before turn it off.
pub fn ir_site_trap_enabled() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// Round 16 (lane indy, SY4-1): may the optimizing tier CALL a bridged
/// `invokedynamic` ([`Op::IndyBridge`], through the single-pass tier's own
/// `DirectHelperTable::indy_bridge`) instead of planting an uncommon trap at
/// it? **Default OFF** until measured on the orchestrator's build;
/// `CRATONVM_JIT_IR_INDY_BRIDGE=1` opts in.
///
/// Only bootstraps the VM bridges get the call (`StringConcatFactory`,
/// `LambdaMetafactory`, `SwitchBootstraps`, `ObjectMethods` -- a non-zero
/// bridge site from the resolver), and only non-`void` sites; every other
/// indy keeps the trap exactly as before. Read per compile (no static), so a
/// test's thread override reaches it and the statics ratchet does not move.
pub fn ir_indy_bridge_enabled() -> bool {
    false
        || ir_indy_bridge_with_inline_enabled()
}

/// JIT round 19 proposal wave 7, lane str7 (`CRATONVM_JIT_IR_INDY_BRIDGE_WITH_INLINE`,
/// default on, default on): the optimizing tier's indy bridge ONLY together with
/// its inline concat (`CRATONVM_JIT_IR_CONCAT_INLINE`'s lowering), as one
/// switch: it turns on [`ir_indy_bridge_enabled`] and
/// `x64::concat_emit::ir_concat_inline_enabled` together.
///
/// Why (CONCAT5-3's "8x worse"): the round-19 close measured
/// `CRATONVM_JIT_IR_INDY_BRIDGE=1` alone at `R19Concat5Ir concat5-ir-int-str`
/// 269 ns against the default's 31. Nothing in the optimizing body's call is
/// worse than the single-pass one -- both tiers call the same
/// `jit_indy_bridge` over the same site, and the single-pass bridge call of
/// the same concat cost 267 ns on the same binary (`R19Str6Rows
/// concat-int-str`, i13, where the single-pass inline was not yet default).
/// The 8x is the arm, not the call: the default single-pass body INLINES
/// the concat (`CRATONVM_JIT_CONCAT_INLINE`, default ON at the close), and
/// the bridge switch alone makes the method an optimizing body whose concat
/// is the bare bridge call. With the IR inline on, the two tiers tie, which is
/// the expected best for a kernel that is all concat. A bridge-only
/// optimizing concat is therefore never the configuration to flip; this
/// switch makes the coherent pair one decision. Read per compile, no static.
pub fn ir_indy_bridge_with_inline_enabled() -> bool {
    true
}

/// JIT round 23 wave 2b, lane L1 (`CRATONVM_JIT_IR_OSR_DOOR_TRAPS_IN_PLACE`,
/// perf, default ON, `=0` kill): the switch of
/// [`IrBuilder::trap_resumable_here`]'s OSR-door arm -- in the optimizing OSR
/// door's build, a site trap off every loop is planted even when the code
/// before it committed a side effect, because the door resumes it in place
/// (churn page J1). Read per compile, no static.
pub fn ir_osr_door_traps_in_place_enabled() -> bool {
    true
}

/// JIT round 23 wave 4 (proxy-define page, proposal 4b;
/// `CRATONVM_JIT_IR_UNRESOLVED_TYPECHECK_IN_PLACE`, perf; opt-in until wave 8,
/// now default ON, see the end): a method-entry optimizing build plants an own-code `checkcast` /
/// `instanceof` of a class that is not loaded as a site trap even where the
/// code before it committed a side effect, so that one cold type check no
/// longer refuses the whole method (`SplitConstantPool.internalAdd`,
/// `StackMapGenerator.processInvokeInstructions`: `checkcast
/// InvokeDynamicEntry` behind `opcode == invokedynamic`, never taken on the
/// proxy row; both stayed single-pass forever).
///
/// Soundness rests on the trap being resumed IN PLACE from its frame state
/// (REEXECUTE at the type check, operand on the stack) rather than by replay
/// from entry. The interpreter-entry and door sinks do that for an own-stash
/// trap with a frame under `sink_precise_resume_allowed` (not synchronized, no
/// monitor, `CRATONVM_JIT_DEOPT_SINK_RESUME` on), and `lib.rs` admits only such
/// methods. Why it is still opt-in: a sink that cannot rebuild the frame (a
/// `FrameValue` it cannot map, e.g. an object scalar-replaced across the
/// trap; a frameless trap) falls back to the replay rule, and for exactly
/// these traps that rule refuses ("refusing side-effecting replay", a Java
/// `InternalError`). The default-on route never plants a trap the fallback
/// cannot serve. Flip only after a run with `CRATONVM_DBG_DEOPT=1` shows these
/// traps resumed and the door census (`DoorRerunCause`) shows no refused
/// re-run. Read per compile, no static.
///
/// Wave 5: on `n23-w4.exe` the switch broke the proxy row
/// (`ClassLoader.<init>` resumed with `Int(0)` for its `(Certificate[]) null`
/// operand; page `r23-w5-typecheck-in-place-trap-resumes-a-null-as-int`). The
/// builder now also refuses an in-place trap whose frame holds a null constant
/// or a `new` (`IrBuilder::frame_resumes_in_place_cleanly`), and a null operand
/// needs no trap at all ([`ir_null_typecheck_no_resolve_enabled`]).
///
/// Wave 8 (lane W8-L1; R23-W8-FLIP): **default ON**, `=0` restores the
/// refusal. The fallback above is now closed after lowering too:
/// `CRATONVM_JIT_IR_IN_PLACE_TRAP_FRAME_CHECK` (lib.rs
/// `in_place_typecheck_trap_frames_rebuild`, default ON) discards a body whose
/// published frame at such a trap holds an unresumable slot, a virtual
/// (scalar-replaced) object, a monitor or a caller scope, so the single-pass
/// body keeps the method exactly as the builder's refusal did. Second victim
/// besides the proxy row: `java/util/regex/Pattern.compile()V` (pc 478,
/// `instanceof Pattern$Loop`), which never got an optimizing body.
pub fn ir_unresolved_typecheck_in_place_enabled() -> bool {
    true
}

/// JIT round 23 wave 8 (lane W8-L1; `CRATONVM_JIT_IR_UNRESOLVED_FIELD_IN_PLACE`,
/// perf, default ON, `=0` restores the refusal): the
/// [`ir_unresolved_typecheck_in_place_enabled`] rule for an admitted
/// unresolved-field site trap (`getstatic` / `putstatic` / `getfield` /
/// `putfield` of a not-yet-loaded owner, `IrBuilder::plant_field_site_trap`):
/// planted even after a committed side effect, because the method's sinks
/// resume it in place from its frame (REEXECUTE at the field site, operands on
/// the stack). Same admission (`lib.rs`: not synchronized, no monitor, no
/// `lambda$` body, `CRATONVM_JIT_DEOPT_SINK_RESUME` on), the same build-time
/// frame rule (`IrBuilder::frame_resumes_in_place_cleanly`) and the same
/// post-lowering check (`CRATONVM_JIT_IR_IN_PLACE_TRAP_FRAME_CHECK`). The VM
/// serves the first firing as the one-shot field-site trap (evict, run the
/// site interpreted, late-bind recompile). Victim: `Pattern.compile()V` pc 500
/// (`site TRAP REFUSED ... (unresolved-field)` once its pc-478 type check was
/// planted in place). Read per compile.
pub fn ir_unresolved_field_in_place_enabled() -> bool {
    true
}

/// JIT round 23 wave 8b (lane W8-L1;
/// `CRATONVM_JIT_IR_IN_PLACE_TRAP_DEAD_PLACEHOLDER`, correctness of the
/// admission, default ON, `=0` keeps the interned null): the `Ref` value an
/// in-place trap arm pushes is a fresh node the in-place frame rule knows to
/// be dead (`IrBuilder::trap_placeholder_ref`), so a second in-place trap
/// downstream of the first is not refused for it. Read per site.
pub fn ir_in_place_trap_dead_placeholder_enabled() -> bool {
    true
}

/// JIT round 23 wave 5 (proxy-define page; `CRATONVM_JIT_IR_NULL_TYPECHECK_NO_RESOLVE`,
/// perf, default ON, `=0` kill): a `checkcast` / `instanceof` whose operand is
/// the `Ref` null constant and whose class is not loaded builds no trap. JVMS
/// 6.5 resolves the class only for a non-null operand, so `checkcast` leaves
/// the null and `instanceof` answers 0 (HotSpot's interpreter and C2 never
/// resolve for null either). Before this, the site refused the optimizing build
/// (`ClassLoader.<init>`'s `(Certificate[]) null`, `CodeSource.<init>` on the
/// proxy row). Read per compile, no static.
pub fn ir_null_typecheck_no_resolve_enabled() -> bool {
    true
}

/// Is `pc` outside every textual back-edge range `[target, source]` of
/// `code[..code_len]` (`bytecode_analysis::back_edges`), i.e. on no loop of
/// a reducible method? JIT round 23 wave 2b, lane L1: the
/// [`IrBuilder::trap_resumable_here`] condition. A cycle closed only by an
/// exception edge is not seen; the cost of missing one is performance (a
/// trap that fires per iteration), never a wrong answer, since the door's
/// transfer is exact either way.
pub(crate) fn osr_door_trap_pc_off_every_loop(code: &[u8], code_len: usize, pc: usize) -> bool {
    crate::bytecode_analysis::back_edges(code, code_len.min(code.len()))
        .iter()
        .all(|&(target, source)| pc < target || pc > source)
}

/// JIT round 22 (lane throws; `CRATONVM_JIT_IR_INDY_BRIDGE_ON_TRAP_REFUSAL`,
/// perf, **opt-in, default OFF**): bridge an `invokedynamic` ONLY where the
/// default arm would otherwise bail the whole optimizing build -- the replay
/// guard refuses its uncommon trap (`site TRAP REFUSED`: the code before the
/// site already committed a side effect), or the door gave the method no indy
/// shapes. Every site whose trap the default arm plants keeps it.
///
/// Why (page `docs/feature-designs/deopt-osr.md`,
/// item J1): a string concatenation AFTER a hot loop bails the whole method,
/// the OSR optimizing build compiles the full method so it bails too, the
/// decline is memoised method-wide, and the loop runs its whole life in the
/// single-pass OSR body. The single-pass body calls the same bridge at that
/// site, so the bridged optimizing body is never worse AT the site; what it
/// changes is the tier of everything else. Unlike
/// [`ir_indy_bridge_enabled`] it never turns a method that compiled in the
/// optimizing tier before into one with a bare bridge: such a method planted
/// traps and keeps them. Only the optimizing OSR door's compiles take it
/// (`CompileRequest::osr_mode_exit_polls`): a method-entry compile that bails
/// here gets a single-pass body with the INLINE concat, and trading that for
/// an optimizing body with the bare bridge call is CONCAT5-3's 8x; an OSR body
/// is entered for its loop, which the concat after it does not dominate.
/// Read per compile, no static.
pub fn ir_indy_bridge_on_trap_refusal_enabled() -> bool {
    false
}

/// JIT round 22 wave 2 (lane throws, THROWS22-2;
/// `CRATONVM_JIT_IR_INDY_BRIDGE_ON_TRAP_REFUSAL_LOOP_METHODS`, perf, **default ON (flipped; `=0` kills)**, independent of [`ir_indy_bridge_on_trap_refusal_enabled`]):
/// the same refusal-only bridge plan for a METHOD-ENTRY compile, but only of a
/// method that has a loop and whose every `invokedynamic` lies outside every
/// loop (`lib.rs` `indy_sites_all_outside_loops`). The bridge then runs once
/// per call, not per iteration, while the loop gets the optimizing body the
/// refused trap used to cost it. A loop-free method keeps the single-pass body
/// with its inline concat (CONCAT5-3's 8x is a loop-free kernel). Read per
/// compile, no static.
pub fn ir_indy_bridge_on_trap_refusal_loop_methods_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// nio21 wave 11, lane L3 (W10L3-1; `CRATONVM_JIT_IR_SPLICE_INDY`, perf,
/// **default ON**, `0` keeps the wave-10 refusal): may the optimizing tier
/// splice a callee whose `invokedynamic`s are all constant-word lambda sites
/// (no arguments, a reference result, the instruction already linked and its
/// singleton published, `crate::InlineSite::ir_indy_rows`)? Each such site
/// builds as its `Op::IndyBridge`, whose probe reads the singleton. Independent
/// of [`ir_indy_bridge_enabled`]: the bridge here is the cold path of a site
/// whose answer is already known. Read per compile (no static); the VM-side
/// resolver reads the same switch.
pub fn ir_splice_indy_enabled() -> bool {
    true
}

/// JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3;
/// `CRATONVM_JIT_IR_INDY_LAMBDA_WORD`, perf, opt-in since the w11d/w12a A/B (lambda-word 19 vs 16 ns, `=1` enables), unset keeps the
/// trap or the bridge): may the optimizing tier build a method's OWN
/// non-capturing lambda site whose singleton is published as
/// `word != null ? word : bridge` ([`IrBuilder::try_indy_lambda_word`]),
/// independently of [`ir_indy_bridge_enabled`]? Read per compile (no static).
pub fn ir_indy_lambda_word_enabled() -> bool {
    false
}

/// JIT round 21 (lane lambdatrap, TLOCAL21-2;
/// `CRATONVM_JIT_IR_INDY_PUBLISHED_WORD_BRIDGE`, perf, **default ON**, `=0`
/// keeps the trap): with [`ir_indy_lambda_word_enabled`] off, may the
/// optimizing tier build a method's OWN non-capturing lambda site whose
/// singleton is published as the single-pass tier's shape -- an
/// [`Op::IndyBridge`], whose lowering reads the singleton through the site's
/// constant word before any call (`CRATONVM_JIT_INDY_CONSTANT_LAMBDA`) --
/// instead of an uncommon trap at it? Such a trap fires on the first arrival of
/// every call (page `jr21-tlocal-lambda-word-optimizing-body-traps-20261014`).
/// Read per compile (no static).
pub fn ir_indy_published_word_bridge_enabled() -> bool {
    true
}

/// t1 wave 11, lane JLAM (LAMBDATRAP21-4;
/// `CRATONVM_JIT_IR_INDY_LATE_LAMBDA_BRIDGE`, perf,
/// **default ON since j27 2026-10-28**, `=0` kills): may the optimizing tier build a method's OWN non-capturing
/// `LambdaMetafactory` site that nobody evaluated yet (no published
/// singleton: a site first reached after the compile) as an
/// [`Op::IndyBridge`] -- the single-pass tier's shape, whose first evaluation
/// links the instruction and publishes the word its later probes read --
/// instead of the uncommon trap that fires on the first arrival and costs
/// the method its optimizing body (the site-trap ban)? The lambda-only
/// default the general [`ir_indy_bridge_enabled`] cannot give. Read per
/// compile (no static).
pub fn ir_indy_late_lambda_bridge_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// t1 wave 8, lane LAMBDATRAP
/// (`CRATONVM_JIT_IR_INDY_PUBLISHED_WORD_BRIDGE_OVER_REFUSAL_ONLY`, routing,
/// **default ON**, `=0` restores the demotion): does a published-word lambda
/// site ([`ir_indy_published_word_bridge_enabled`]) keep its `Op::IndyBridge`
/// under the refusal-only bridge plan
/// ([`ir_indy_bridge_on_trap_refusal_loop_methods_enabled`], default ON, and
/// the OSR door's [`ir_indy_bridge_on_trap_refusal_enabled`])? That plan keeps
/// every trap the default arm would plant, and so it also demoted the r21
/// published-word bridge back to a trap in
/// any method with a loop, an ordinary invoke (the default shapes) and a
/// non-capturing lambda outside the loop: the trap fires on the first call,
/// the site-trap policy bans the optimizing tier for the whole method, and
/// the loop the plan exists to keep optimizing runs single-pass. The bridge
/// here is a probe of a known singleton, not the bare bootstrap call the plan
/// guards against (CONCAT5-3). Evidence: the jr21 host A/B of this very
/// trade (word-create-only 24 vs 243 ns, word-only-guarded 29 vs 90). Read
/// per site (no static).
pub fn ir_indy_published_word_bridge_over_refusal_only_enabled() -> bool {
    true
}

/// JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2 second half / W9L2-1
/// part 2; `CRATONVM_JIT_IR_INDY_CAPTURE_NEW`, perf, **default ON**, `=0` keeps
/// the trap or the bridge): may the optimizing tier build a method's OWN linked
/// capturing lambda site as `Op::New` of its proxy class plus one store per
/// capture ([`IrBuilder::try_indy_capture_new`])? Read per compile (no static);
/// with it off the VM-side fact is never asked for.
pub fn ir_indy_capture_new_enabled() -> bool {
    true
}

/// JIT round 21 lane mcs (`CRATONVM_JIT_INDY_MUTABLE_GUARD_SPLICE`, perf,
/// **default ON**, `=0` keeps the guarded plain call; page
/// `jr21-mcs-guarded-indy-call-lost-its-direct-row-FIXED-20261014.md`): may the
/// optimizing tier splice a guarded `MutableCallSite` / `VolatileCallSite`
/// site's planned STATIC target on the guard's hit edge
/// ([`IrBuilder::try_indy_mutable_guarded_splice`])? Read per guarded site per
/// compile (no static).
pub fn ir_indy_mutable_guard_splice_enabled() -> bool {
    true
}

/// mcs21 (MCS21-2, `CRATONVM_JIT_INDY_MUTABLE_GUARD_ROW`, perf, **default on**,
/// `=1` enables): may a guarded `MutableCallSite` / `VolatileCallSite` site's
/// `CallSite.target` read carry the compact row the VM named
/// ([`IndyMutableGuard::field_row`]), so a compact call site is read inline
/// (a header test, one load) instead of through the checked `jit_getfield`
/// helper on every execution of the site? Read per guarded site per compile
/// (no static). default on until a host A/B has priced it
/// (`jit-r21-mcs21-residual-proposals-20261102.md`, gate MCS21R-1).
pub fn ir_indy_mutable_guard_row_enabled() -> bool {
    true
}

/// JIT round 20 wave 19 (lane ySplice, SPLICE20-1 of
/// `docs/feature-designs/deopt-osr.md`;
/// `CRATONVM_JIT_IR_SPLICE_INDY_CAPTURE`, perf, **default ON**, `=0` keeps the
/// resolver's `capturing ... no constant word` refusal): may a SPLICED body's
/// linked capturing lambda site be built as its proxy's `Op::New` plus one
/// store per capture ([`IrBuilder::try_indy_capture_new`]), from the row the
/// VM resolver hands over (`crate::InlineSite::ir_indy_capture_rows`)? Asked
/// with [`ir_splice_indy_enabled`] and [`ir_indy_capture_new_enabled`], by the
/// resolver, the rebase and the builder alike. Read per compile (no static).
pub fn ir_splice_indy_capture_enabled() -> bool {
    ir_splice_indy_enabled()
        && ir_indy_capture_new_enabled()
        && true
}

/// JIT round 20 wave 19 (lane ySplice, SPLICE20-2;
/// `CRATONVM_JIT_IR_SPLICE_UNRESOLVED_NEW_EXIT`, perf, **default ON**, `=0`
/// keeps the resolver's `ir-splice-new-site-unresolved` /
/// `ir-splice-new-site-class-init-pending` refusals): may a spliced body's
/// `new` whose class is not loaded (or loaded and owing its initialization)
/// become a PRECISE exit at that `new`
/// ([`IrBuilder::plant_unloaded_typecheck_exit`]'s mechanism: an always-failing
/// guard resumed through the chain snapshot at the callee's own bci, so the
/// interpreter resolves, loads and initializes the class there, exactly once
/// and in its own order, and nothing of the splice is replayed)? The resolver
/// admits such a site only behind a branch edge the callee's profile says
/// never ran ([`splice_typecheck_arm_unexecuted`]); the exit is stamped
/// `DeoptReason::ClassLoading`, so the first one asks a recompile. Read per
/// compile (no static).
pub fn ir_splice_unresolved_new_exit_enabled() -> bool {
    true
}

/// JIT round 23 wave 9, lane L6 (`CRATONVM_JIT_IR_SPLICE_UNRESOLVED_LDC_EXIT`,
/// perf, **default ON** (default ON since j27 2026-10-28), `=0` keeps the resolver's `ldc-non-numeric-constant`
/// refusal): may a spliced body's `ldc` / `ldc_w` of a `CONSTANT_String` whose
/// entry is not resolved yet become a PRECISE exit at that `ldc`, the
/// mechanism of [`ir_splice_unresolved_new_exit_enabled`]?
///
/// The shape is `StringLatin1.newString([BII)`, which every `substring` of a
/// LATIN1 String calls: `if (len == 0) return "";` at pc 4. A split that never
/// produced an empty part never ran that `ldc`, so its entry stays unresolved,
/// and the whole body was refused. Each `substring` in `String.split(CIZ)` and
/// `Pattern.split` was then a real call. The resolver admits the site only
/// behind a branch edge the callee's profile says never ran
/// ([`splice_typecheck_arm_unexecuted`]). The exit resumes through the chain
/// snapshot at the `ldc`, so the interpreter resolves (interns) the constant
/// there, in the callee's frame, and nothing of the splice is replayed. It is
/// stamped `DeoptReason::ClassLoading`, so its first firing asks for the
/// recompile that finds the slot filled. Read per compile (no static).
///
/// Was opt-in (`=1`) from the n23-w9a A/B (P-cores, 3 reps, default vs `=0`) to j27:
/// `RxLoop split-comma` 1259/1166/1294 vs 1262/1035/1100 ns/op,
/// `W8L6SplitParts comma` 1230/1096/1126 vs 1325/1185/1136 -- no win.
pub fn ir_splice_unresolved_ldc_exit_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// JIT round 20 wave 19 (lane ySplice, SPLICE20-7;
/// `CRATONVM_JIT_SP_SPLICE_INDY_CONSTANT`, perf, **default ON**, `=0` keeps
/// the single-pass resolver's `invokedynamic` refusal): may the SINGLE-PASS
/// tier splice a body whose `invokedynamic`s are all constant-word lambda
/// sites (`()L...`, linked, singleton published: the optimizing tier's
/// `crate::InlineSite::ir_indy_rows` admission), emitting the site's
/// constant-word probe with the bridge call as its cold miss? Also needs
/// `CRATONVM_JIT_INDY_CONSTANT_LAMBDA` (the word is published only with it).
/// Read per compile (no static).
pub fn sp_splice_indy_constant_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_SPLICE_TYPECHECK` (perf, **default ON**, `=0` keeps the
/// single-pass resolver's `checkcast/instanceof` refusal): may the
/// SINGLE-PASS tier splice a body with type checks whose targets resolve
/// through the callee's constant pool (the optimizing tier's
/// `crate::InlineSite::ir_typecheck_info` rows), emitting the top-level arm's
/// own sequence at each? `String.equals` (`instanceof String`) was the
/// commonest refusal on in-process `javac` over jar classpaths. Read per
/// compile (no static).
pub fn sp_splice_typecheck_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_SPLICE_NEWARRAY` (L1W1-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`, churn
/// page J2-residual; perf, **default ON**, `=0` keeps the single-pass splice's
/// `newarray` refusal): may the SINGLE-PASS tier splice a primitive `newarray`
/// (`0xbc`) of a non-negative constant length in a callee with no exception
/// table and a unique safepoint-bci inline-frame chain? Read per compile (no
/// static).
pub fn sp_splice_newarray_enabled() -> bool {
    true
}

/// Round 16 (lane matrix): may the optimizing tier take a method holding a
/// `multianewarray` ([`Op::MultiNewArray`], a call to the single-pass tier's
/// own `multianewarray_n` helper)? **Default ON** since 2026-10-06
/// (`R16Bench matrix` 54-72 -> 4-5 ms on the Linux host, with
/// `CRATONVM_JIT_IR_OSR_REMAT_ARRAY_LOAD` and the vector pre-loop);
/// `CRATONVM_JIT_IR_MULTIANEWARRAY=0` turns it off.
///
/// Off, [`ir_compatible`] refuses every such method outright, as it did
/// before, and the method runs single-pass -- for `R16Bench.matrix`, which
/// allocates its three `double[n][n]` with it, that is the whole kernel,
/// OSR'd into a single-pass body. Read per compile (no static), so a test's
/// thread override reaches it and the statics ratchet does not move.
pub fn ir_multianewarray_enabled() -> bool {
    true
}

/// nio21 wave 15 (lane L3; `CRATONVM_JIT_IR_DEFERRED_NEW`, perf, **default
/// ON**, `0` restores the whole-method refusal): may the optimizing tier build
/// a `new` whose class is not loaded at compile time as [`Op::NewCp`], a call
/// of the CP-indexed `new_object_cp` helper the single-pass tier already
/// compiles the same site to?
///
/// Sound by construction: the helper is the interpreter's own `new`
/// (resolution through the referencing class's loader, access check,
/// `<clinit>`, allocation, every failure a stashed throwable at this bci), so
/// the optimizing body answers exactly what the single-pass body it replaces
/// answers. It costs nothing on the cold path it is for, and on a hot one it is
/// the single-pass tier's cost (no inline TLAB bump, no scalar replacement).
/// Read per compile (no static), so a test's thread override reaches it.
pub fn ir_deferred_new_enabled() -> bool {
    true
}

/// May an UNRESOLVED-CLASS site (`checkcast`, `instanceof`, `new`) become a
/// trap? **Default OFF**; `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP=1` opts in.
///
/// The reason it is off CHANGED on 2026-09-07 and the old reason is retracted.
/// It read "200,000 deopts on `UnresolvedTrapProbe`, every call, permanently
/// not-compilable", and every one of those numbers was a broken deopt sink
/// measured through this switch (see `ir_site_trap_enabled`). On a binary with
/// `CRATONVM_JIT_DEOPT_SINK_RESUME` in it, 30 hibernate-reactive classes with
/// this switch ON read `ok=241 failed=0` with **112 traps taken and zero**
/// `refusing side-effecting replay` -- identical to the default arm.
///
/// It stays off because turning it on was always a THROUGHPUT argument (+13
/// accepted bodies, +11 lowered on H2) and that has never been measured. Not
/// because it is harmful.
///
/// # Why this is separate from the indy trap, and why it ships off
///
/// The coldness argument for these three was: "the named class has never been
/// loaded, and a class that has never been loaded cannot have been touched by
/// any path that has executed." The first clause is true and the conclusion
/// does not follow from it, because the observation is made at COMPILE time.
/// A class not loaded when the method compiles can load a moment later, and the
/// path can then run — at which point the trap fires on a LIVE path, the body
/// returns the deopt sentinel, and it does so on every execution, forever.
///
/// That is not a theory. `ir_vs_singlepass_checkcast_not_yet_loaded_refuses_ir`
/// executes exactly that path: with the trap planted, the compiled body
/// returned `i64::MIN` instead of the object. The fixture was written years
/// before this trap existed and it refuted the argument on the first run.
///
/// The transient case already has a mechanism, and it is the opposite of this
/// one: `note_deferred_new_bail` / `take_deferred_new_retry` REFUSE the method
/// and re-offer it once the class loads. Compiling around the site instead
/// bypasses that machinery — the builder no longer bails, so nothing arms the
/// memo and nothing re-offers the method — which trades a delayed optimizing
/// body for a permanently deopting one.
///
/// `invokedynamic` is different and stays on: there is no "later" for a
/// bootstrap this tier will never lower, and the single-pass backend has made
/// exactly that trade by default since it stopped bailing on indy.
///
/// What would make this safe is `DeoptAction::RecompileAndReinterpret` at these
/// sites instead of `Reinterpret`, so the first trap triggers the recompile
/// that resolves the class. `Op::Guard`'s lowering hard-codes the action today;
/// parameterising it is the work this switch is waiting on. Read per site at
/// build time, not latched (j27 wave 4, lane SMALLFIX; no `static`,
/// `process_global_statics_ratchet`). Unsafe arm: only the strict on-words
/// (`1`/`true`/`on`/`yes`) enable it, as the latch did -- `=2` stays off.
pub fn ir_unresolved_class_trap_enabled() -> bool {
    matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true" | "on" | "yes")
    )
}

/// Report the exact [`IrBuilder::build`] site that refused a method.
///
/// The builder answers `Option<Graph>`, so every refusal is indistinguishable
/// from "the optimizing tier is switched off" — both present as zero IR bodies,
/// and this investigation drew the second conclusion from the first twice
/// (`jit-ir-relocation-map-contract.md`). Fifteen distinct
/// `return None` sites share one observable outcome; only the site tells them
/// apart, and nothing reported it.
///
/// `site` is `line!()` at the refusal rather than a parallel reason enum: it
/// names the site exactly and cannot drift out of step with the code.
#[cold]
#[inline(never)]
pub fn ir_build_bail<T>(site: u32, pc: usize) -> Option<T> {
    if ir_bail_reporting() {
        eprintln!("[ir] IrBuilder::build refused at ir.rs:{site} (bytecode pc {pc})");
    }
    // `bailout.rs`'s module doc names this as one of the three incompatible
    // ways the compiler says "I cannot compile this", and the one that "carries
    // no reason at all, so the per-method compiler report cannot be produced".
    // It could not: over an H2 test class, 51 of 620 compilations fell through
    // to single-pass and only 6 carried an attributed reason — the other 45
    // refused here, invisibly.
    //
    // The site/pc pair stays in the debug line above; what the report needs is
    // the method and the category, which is what this adds.
    // The SITE, not just the fact. On a real workload (H2 `TestAlter`) 38 of 50
    // fall-throughs land here, and with one shared category they were 38
    // identical rows — "the builder refused", 38 times, naming nothing to fix.
    // `ir.rs:<line>` is what separates them into the distinct refusals they
    // actually are, and the line is already the argument this function takes.
    attribute_build_bail_at(
        crate::bailout::BailoutReason::UnsupportedShape("IrBuilder::build"),
        site,
        pc,
    );
    None
}

/// Attach a builder refusal to the compilation in flight.
///
/// Split out so both bail helpers share one policy: the process-wide category
/// counter AND the per-compilation report, the same pair `verify_or_bail` uses.
pub(in crate::ir) fn attribute_build_bail(reason: crate::bailout::BailoutReason) {
    let bailout = crate::bailout::Bailout::new(reason);
    crate::bailout::record_bailout(&bailout);
    crate::metrics::note_current_bailout(&bailout, "build");
}

/// [`attribute_build_bail`] carrying the refusing site.
///
/// The category stays one value so the process-wide counters keep their stable
/// row set; the site rides in the bailout's context, which is what the
/// per-compilation report prints. That is the difference between "the builder
/// refused 38 times" and a ranked list of which refusals to go and fix.
pub(in crate::ir) fn attribute_build_bail_at(reason: crate::bailout::BailoutReason, site: u32, pc: usize) {
    let bailout =
        crate::bailout::Bailout::with_context(reason, format!("ir.rs:{site} (bytecode pc {pc})"));
    crate::bailout::record_bailout(&bailout);
    crate::metrics::note_current_bailout(&bailout, "build");
}

/// [`ir_build_bail`] for the opcode catch-all, which knows something more
/// useful than its own line number: *which* bytecode has no IR lowering.
#[cold]
#[inline(never)]
pub fn ir_build_bail_opcode<T>(op: u8, pc: usize) -> Option<T> {
    if ir_bail_reporting() {
        eprintln!("[ir] IrBuilder::build has no lowering for opcode {op:#04x} at bytecode pc {pc}");
    }
    // The one refusal that already knows exactly what it lacks, so it reports
    // the opcode rather than the generic shape.
    attribute_build_bail(crate::bailout::BailoutReason::UnsupportedOpcode { opcode: op });
    None
}

/// Report a φ whose type could not be proven by the [`join_data_type`] lattice.
///
/// Observability for the one place the type system gives up. Silence here is
/// what let "every reference merge is an `Int`" survive: the wrong type is not
/// a crash, it is a missing GC root and a mistyped deopt frame value, both of
/// which surface much later and somewhere else. Uses the same
/// `CRATONVM_DBG_JITC` / `CRATONVM_DBG_IR_COMPILES` switch as the build bails
/// ([`ir_build_bail`]) so one flag shows the whole IR-refusal picture, and is
/// deliberately a report rather than a `debug_assert!`: an untypeable merge is
/// a *dead* slot in verified bytecode, so it must degrade, not panic.
///
/// (If/when `crate::bailout` lands, this is the natural place to raise a
/// `BailoutReason::IrVerification` instead of falling back — the fallback is
/// what the current infallible signature forces.)
#[cold]
#[inline(never)]
pub(in crate::ir) fn report_phi_type_fallback(why: &str) {
    if ir_bail_reporting() {
        eprintln!(
            "[ir] phi type join failed: {why}; using the conservative {:?} \
             (not a GC root)",
            PHI_TYPE_FALLBACK
        );
    }
}

pub(in crate::ir) fn ir_bail_reporting() -> bool {
    false
}

/// Report which conjunct of [`ir_compatible`] refused a method.
///
/// Same problem as [`ir_build_bail`], one stage earlier: the predicate answers
/// a bare `bool`, so "this method is not IR-eligible" and "the optimizing tier
/// never ran" are the same observation. The refused conjunct is the only thing
/// that distinguishes a deliberate exclusion from a bug.
#[cold]
#[inline(never)]
pub(in crate::ir) fn ir_reject(why: &str) -> bool {
    if ir_bail_reporting() {
        eprintln!("[ir] ir_compatible refused: {why}");
    }
    false
}

/// Check if a method (from its JitScanResult) is suitable for IR compilation.
///
/// STUB-S7 widening: previously this rejected every method with any heap op,
/// invoke, or allocation — which excluded virtually all real-world methods and
/// left the IR-resident optimization passes (escape analysis, GVN, fold,
/// schedule, lower) effectively unreachable. We now admit methods with a
/// bounded number of common heap-touching opcodes; the IR builder itself
/// returns `None` when it encounters an opcode it doesn't yet lower, and the
/// caller in `lib.rs` (`try_compile`) falls through to the x64 single-pass
/// backend on either `build()` or `lower()` returning `None`. That fallback is
/// the safety net that lets us widen gradually without crashing.
///
/// `invokedynamic` (0xba) is admitted only when the builder may replace it
/// with an uncommon trap (`ir_site_trap_enabled`, see the `indy_ops` check
/// below and the builder's `0xba` arm). There is still no exception-edge IR:
/// the builder SKIPS handler bodies (STUB-S8, `normally_reachable_pcs`)
/// because a compiled frame hands every exception back to the interpreter,
/// and which exception tables are admissible at all is decided by
/// `precise_exception_frames` below. (This paragraph used to say an indy
/// method always falls back and a try/catch method is rejected at parse
/// time; both stopped being true when those arms landed.)
///
/// TODO(IR widening): increase caps once differential tests validate.
pub fn ir_compatible(scan: &crate::x64::JitScanResult) -> bool {
    // Caps below are deliberately conservative. A method that exceeds a cap is
    // still compilable via the x64 single-pass backend; we just decline to
    // route it through the IR pipeline until we've gained more confidence.

    // cov-07: `athrow` (0xbf) is NOT refused here any more. `IrBuilder::build`
    // now lowers it (`Op::Throw`), reusing the exact `jit_throw_exception`
    // sentinel-drain protocol `Op::CheckCast`'s helper-thrown
    // `ClassCastException` already uses (cov-05) — see `Op::Throw`'s doc
    // comment. `scan.has_athrow` itself is unchanged and still recorded;
    // nothing in this function reads it any more, but the scan field stays
    // for the debug trace and for any future caller that needs "does this
    // method contain an athrow" without re-decoding.
    //
    // What is NOT relaxed by this: a method whose handler reads a
    // non-parameter local still needs a precise resume frame, and the IR
    // tier has no way to publish one (that machinery is x64-only, gated
    // behind `precise_exception_frame_sites_supported`). That case is
    // excluded by `precise_exception_frames` (RBC.6, checked by this
    // function's caller) independently of which opcode throws — it gates on
    // ANY non-empty `exception_table`, not on `has_athrow` — so an
    // athrow-bearing method that needs precise locals was already refused
    // there before it ever reached this conjunct. See
    // `cov-07-athrow-RETIRED-*.md`.

    // invokedynamic-uncommon-trap fix: the IR builder has no lowering for
    // 0xba (it bails cleanly via the main loop's `_ => return None` if one
    // slips through), but decline it explicitly here rather than relying
    // solely on that later bail — this keeps the scope boundary self-
    // documenting and avoids running the (now 0xba-aware, but still only
    // IR-builder-adjacent) `bytecode_analysis::branch_target_map`/`bytecode_analysis::back_edges`
    // pre-scans on a method the IR path was never going to lower anyway.
    // Methods containing invokedynamic always fall back to the x64
    // single-pass backend, which lowers it directly.
    //
    // 2026-09-06: no longer unconditional. `IrBuilder`'s `0xba` arm now
    // replaces such a site with an uncommon trap -- the SAME trade the
    // single-pass backend has made by default since it stopped bailing on indy
    // -- so the method is admitted and compiles around the site. Measured
    // before this: **53 methods refused on the H2 JDBC workload**, none of them
    // for anything wrong with the code the optimizing tier would have run.
    //
    // The refusal survives with the kill switch, and it also survives ANYWHERE
    // the descriptor resolver cannot supply the site's stack effect: the `0xba`
    // arm bails the method on a pc it has no shape for, because guessing how
    // many operand-stack entries an indy consumes corrupts every bytecode after
    // it. The gate is the *admission*; the shape is the *proof*.
    if !scan.indy_ops.is_empty() && !ir_site_trap_enabled() {
        return ir_reject("!scan.indy_ops.is_empty()");
    }

    // Cap simple invokes (invokestatic / invokevirtual / invokespecial /
    // invokeinterface). Invokedynamic is rejected upstream by `jit_scan`.
    //
    // Raised 5 -> 32 by the IR direct-call slice. The old cap of 5 was not a
    // codegen limit at all: the IR path lowered EVERY invoke through the generic
    // `jit_invoke_dispatch` helper (no direct calls, no inline caches), so a
    // call-heavy method's "optimizing" IR recompile paid a full helper round trip
    // per call and could come out SLOWER than the single-pass body, which binds a
    // statically-resolved callee with a raw `CALL`. The cap was a blunt way of
    // saying "don't route call-heavy methods here". `ir_lower`'s
    // `emit_direct_cross_call` now closes that gap for the statically-bound kinds
    // (`invokestatic` and non-`<init>` `invokespecial` — the only kinds admitted
    // in a default configuration, since `CRATONVM_JIT_IR_CALL_VIRTUAL` is
    // default-OFF), so the original reason no longer applies to them.
    //
    // 2026-07-26 (jit-inlining-and-ir-calls): raised again, 32 -> 64, and the
    // "only the statically-bound kinds are covered" caveat above is now
    // obsolete. `ir_lower::emit_inline_cache_call` gives `invokevirtual` /
    // `invokeinterface` the same MIC + 3-way-PIC cascade the single-pass
    // backend emits, so a virtual site no longer pays a helper round trip with
    // a dynamic lookup either, and `CRATONVM_JIT_IR_CALL_VIRTUAL` inverted from
    // opt-in to opt-out. EVERY invoke kind now lowers at least as well as
    // single-pass does.
    //
    // What the cap still buys, unchanged: (1) each `Op::Call` widens the
    // argument staging region and forces `needs_context`, and `lower()` bails
    // when `1 + num_params` exceeds the ABI register file, so a very call-dense
    // method risks a lowering bail after all the graph work; (2) an
    // unresolvable callee still falls back to helper dispatch, so a method
    // whose invokes are mostly unresolvable gains nothing and only pays compile
    // time; (3) code size — an inline-cache site emits ~250 bytes, so 64 sites
    // is ~16 KiB of dispatch code, the same order the single-pass backend
    // produces for the same method. It is a budget, not a lowering gap.
    //
    // Netty compression wall (2026-10-14): the budget is per compile now
    // ([`ir_max_invokes`]), raised to 192 by default. `FastLz.compress` --
    // 1617 bytes whose whole job is one loop over `ByteBuf` byte accessors --
    // has more than 64 call sites, so it never had an optimizing body and
    // every byte of `FastLzIntegrationTest#testHugeDecompress` went through
    // single-pass calls.
    if scan.invoke_ops.len() > ir_max_invokes() {
        return ir_reject("scan.invoke_ops.len() > IR_MAX_INVOKES");
    }
    // `putstatic` lowers to its `jit_putstatic_*` helper (`Op::StoreStatic`),
    // which carries the SATB pre-barrier, so `scan.has_putstatic` is no longer
    // read here (round 11 wave 10; it refused every method with a static
    // write -- counters, lazy singletons, memo tables).
    // getfield/putfield. Raised 5 -> 64. Instance field access lowers to
    // `Op::Load`/`Op::Store` (optionally via the checked `jit_getfield`
    // helper); nothing about it scales worse than the single-pass backend, so
    // 5 was never a lowering limit — it was part of the same blanket "keep real
    // methods off the IR path" posture as the invoke cap.
    if scan.field_ops.len() > IR_MAX_FIELD_OPS {
        return ir_reject("scan.field_ops.len() > IR_MAX_FIELD_OPS");
    }
    // getstatic/putstatic (same shape as instance field ops for IR).
    if scan.static_field_ops.len() > IR_MAX_STATIC_FIELD_OPS {
        return ir_reject("scan.static_field_ops.len() > IR_MAX_STATIC_FIELD_OPS");
    }
    // `new` sites. Raised only 3 -> 16, deliberately the most conservative of
    // the budgets.
    //
    // DO NOT treat this like the others. It governs how many allocations escape
    // analysis may scalar-replace, and each one that SURVIVES is lowered through
    // the shared `emit_new_object_stub` — the same runtime-lowering stub the
    // baseline tier uses, not the inline TLAB bump-pointer sequence. Losing that
    // inline bump is one of the two independent causes of the July 2026 Binary
    // Trees 4x regression documented in BENCHMARK.md, so raising this cap
    // increases the number of sites that can pay the stub, not just the number
    // that can be optimized away.
    if scan.new_ops.len() > IR_MAX_ALLOCATIONS {
        return ir_reject("scan.new_ops.len() > IR_MAX_ALLOCATIONS");
    }
    // cov-06: `anewarray` sites. Same budget shape and same reasoning as the
    // `new` cap above — it bounds how many reference-array allocations are
    // admitted, each one lowered through the shared `emit_new_array_stub`.
    // A REFERENCE array is still never scalar-replaced (`escape_analysis.rs`
    // refuses `anewarray` outright: a never-stored slot would have to be
    // answered with `null` and the applier's zero default is numeric), so this
    // cap bounds stub-call cost and nothing else. A small constant-length
    // PRIMITIVE array is a different story — see `MAX_SCALAR_ARRAY_LEN`.
    // `newarray` (0xbc) has no
    // analogous cap: it carries no constant-pool site to resolve, so there is
    // nothing here to bound admission on — an unbounded number of primitive
    // array allocations costs the builder nothing `IR_MAX_GRAPH_NODES`
    // doesn't already bound.
    if scan.anewarray_ops.len() > IR_MAX_ARRAY_ALLOCATIONS {
        return ir_reject("scan.anewarray_ops.len() > IR_MAX_ARRAY_ALLOCATIONS");
    }

    // ── Surviving exclusions — real missing lowerings, not budgets ───
    //
    //  * ONE-DIMENSIONAL array allocation is NOT excluded, and this bullet
    //    used to say the opposite. Until 2026-09-16 it read "`newarray`
    //    (0xbc) and `anewarray` (0xbd) have no arm in `IrBuilder::build`'s
    //    opcode match and `Op::NewArray` is constructed nowhere", and it went
    //    on to claim that the `anewarray_ops` BUDGET had been replaced by a
    //    refusal. All three halves were false: cov-06 added the builder's
    //    0xbc/0xbd arms (`IrBuilder::build`), `Op::NewArray` is constructed
    //    by both of them and lowered by `ir_lower`, and the budget is still
    //    the `scan.anewarray_ops.len() > IR_MAX_ARRAY_ALLOCATIONS` check
    //    twenty lines above this comment. The `multianewarray` bullet
    //    directly below contradicted it in the same block.
    //
    //    The lesson the false text was trying to teach is still the right
    //    one, so it is restated here as a rule rather than as a claim about
    //    two opcodes: **a method the builder will refuse must be refused
    //    HERE**, cheaply and with a reason, not after a full graph build.
    //    Admission and the builder disagreeing is what made the optimizing
    //    tier produce zero bodies on every probe written for the
    //    relocation-map contract while every gate upstream reported "open"
    //    (jit-ir-relocation-map-contract.md). `ir_admission_and_builder_agree`
    //    (below) now pins the surviving direction of that rule as a test, so
    //    the next edit to either side fails CI instead of rotting into a
    //    comment that says the reverse of the code beside it.
    //
    //  * `multianewarray` (0xc5) — no arm in `IrBuilder::build`'s opcode
    //    match and `Op::NewArray` cannot represent a multi-dimensional
    //    allocation. Unlike `newarray`/`anewarray` (cov-06's first two
    //    increments — see `Op::NewArray`, the builder's 0xbc/0xbd arms and
    //    `ir_lower`'s lowering arm), this one is a genuinely different
    //    lowering shape: the single-pass backend's 0xc5 arm is a helper call
    //    sequence (`jit_multianewarray_n`, which pins the dimension counts
    //    into frame scratch words and passes their ADDRESS) the IR pipeline has
    //    no equivalent op for, and the survey found only 2 events against `anewarray`'s
    //    138 — not worth a new `Op` variant and a second resolver-fed info
    //    map for. Left refused deliberately; a method containing one still
    //    compiles fine on the single-pass backend.
    //  * `checkcast` / `instanceof` / `athrow` — NONE of the three is refused
    //    here (as of cov-05 for the first two, cov-07 for the third).
    //    `instanceof` produces an `int`, cannot throw, and has no
    //    control-flow consequence. `checkcast` (0xc0) and `athrow` (0xbf) CAN
    //    throw, but both reach the SAME generic "helper stashed a pending
    //    exception, drain it through the shared epilogue" protocol every
    //    other fallible `Opaque` call in this tier already has
    //    (`Op::ConstClass`'s resolution failure is the existing precedent) —
    //    neither is a jump into a LOCAL exception-table handler inside this
    //    compiled frame, which is what `precise_exception_frames` (checked by
    //    this function's caller) guards against, independently of which
    //    opcode throws. See
    //    `cov-05-checkcast-and-instanceof-RETIRED-*.md` and
    //    `cov-07-athrow-RETIRED-*.md`.
    //
    //    `IrBuilder::build`'s 0xc0/0xc1 arms additionally require the target
    //    class to be resolved-and-loaded at compile time (via
    //    `checkcast_info`/`instanceof_info`, populated in `lib.rs`'s
    //    `try_compile_inner`); a site whose target is not yet loaded is
    //    simply absent from that map, so *that* method bails at the builder
    //    instead of here — the not-yet-loaded slow path in
    //    `jit_typecheck_resolve` can run a user classloader's
    //    `loadClass`/`findClass`, arbitrary Java this tier does not host
    //    inside a helper call.
    //
    //    Round 16 (lane matrix): `Op::MultiNewArray` is that op, behind
    //    `CRATONVM_JIT_IR_MULTIANEWARRAY` (default ON since 2026-10-06). On, a
    //    site the compile door resolved builds the node and the rest bail in
    //    the builder, which is the `anewarray` model.
    if !scan.multianewarray_ops.is_empty() && !ir_multianewarray_enabled() {
        return ir_reject("!scan.multianewarray_ops.is_empty()");
    }

    true
}

/// Maximum invoke sites (`invokestatic`/`virtual`/`special`/`interface`) in a
/// method routed through the IR pipeline. Was 5, then 32; see the rationale at
/// the check in [`ir_compatible`].
pub const IR_MAX_INVOKES: usize = 64;

/// The invoke budget [`ir_compatible`] applies to this compile:
/// `CRATONVM_JIT_IR_MAX_INVOKES` (netty compression wall, 2026-10-14),
/// clamped to `IR_MAX_INVOKES..=512`, else 192. Read per compile; no static
/// (`process_global_statics_ratchet`).
pub fn ir_max_invokes() -> usize {
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map_or(192, |n| n.clamp(IR_MAX_INVOKES, 512))
}

/// Maximum `getfield`/`putfield` sites. Was 5.
pub const IR_MAX_FIELD_OPS: usize = 192;

/// Maximum `getstatic`/`putstatic` sites. Was 5.
pub const IR_MAX_STATIC_FIELD_OPS: usize = 64;

/// Maximum `new` sites. Was 3, then 16 while a surviving allocation lowered
/// through the shared `emit_new_object_stub` (a CALL where the single-pass
/// backend bumps a TLAB inline). Since 2026-09-02 the optimizing tier has its
/// own inline bump (`emit_inline_tlab_new_ir`) and the cap sits with every
/// neighbouring cap at 64. It never applied to arrays, which are refused
/// outright.
pub const IR_MAX_ALLOCATIONS: usize = 64;

/// cov-06: maximum `anewarray` sites. Same budget posture as
/// [`IR_MAX_ALLOCATIONS`] — every admitted REFERENCE-array site is always
/// lowered through the shared `emit_new_array_stub` (a reference array is never
/// scalar-replaced), so this bounds the stub-call cost, not an optimization
/// headroom. It does not cover `newarray` (0xbc), which is where a
/// scalar-replaceable primitive array comes from.
pub const IR_MAX_ARRAY_ALLOCATIONS: usize = 16;

/// Maximum bytecode length for the IR pipeline.
///
/// Was 200 — small enough that essentially no real application method
/// qualified, which made every other cap moot. 8000 is HotSpot's
/// `HugeMethodLimit`, the point at which HotSpot itself declines to compile a
/// method at all (`DontCompileHugeMethods`), so this stops being an
/// IR-specific restriction and becomes the same pathological-method backstop
/// every JVM has. Tier-4 compile time is separately bounded by
/// [`IR_MAX_GRAPH_NODES`], which reflects actual complexity rather than a byte
/// count.
pub const IR_MAX_BYTECODE_SIZE: usize = 8000;

/// Maximum node count of a built IR graph before the optimizing pipeline is
/// abandoned in favour of the single-pass backend.
///
/// This is the real compile-time guard for tier 4. `ir_optimize` runs passes
/// that are super-linear in node count (GVN's value numbering, the
/// escape-analysis connection graph, the scheduler's block placement), so a
/// bytecode-length cap alone does not bound compile time: an 8000-byte method
/// of straight-line arithmetic builds a far larger graph than an 8000-byte
/// call-heavy one. Checked in `try_compile_inner` right after
/// `IrBuilder::build` and before any optimization runs, so an over-large graph
/// costs one linear build and nothing else. Its runtime companion is
/// `tiered::MAX_C2_COMPILE_TIME_MS`, which catches whatever slips past this.
pub const IR_MAX_GRAPH_NODES: usize = 20_000;

/// Upper bound on `max_locals × bytecode bytes` for one [`IrBuilder::build`]
/// (round 11 wave 3).
///
/// The builder records a [`SafepointSnapshot`] at every walked bci, each one a
/// copy of the whole local frame, so its memory is `instructions × max_locals`
/// words before any pass prunes a snapshot — and nothing bounded the width:
/// [`IR_MAX_BYTECODE_SIZE`] caps bytes, JVMS allows `max_locals` up to 65535,
/// and Janino/Groovy-style generated methods commonly declare thousands
/// (`r11-irback-builder-memory-scales-with-bytecodes-times-frame-width`). An
/// 8000-byte method with 65535 locals is ~1.3 GB of snapshot words.
///
/// 2^25 words (128 MiB of `NodeId`s, counting bytes rather than
/// instructions, so roughly twice the real figure) refuses only such methods:
/// an 8000-byte method up to ~4000 locals, a 1000-byte one up to ~33000, still
/// builds. The byte count is the COMBINED buffer's, spliced bodies included,
/// which is why the bound is generous. [`IR_MAX_GRAPH_NODES`] cannot stand in
/// for this — it is checked after `build` returns, by which point the
/// snapshots already exist.
pub(in crate::ir) const IR_MAX_FRAME_SNAPSHOT_WORDS: usize = 1 << 25;

/// Walk iterations [`IrBuilder::build`] is allowed per byte of the combined
/// bytecode buffer before it refuses the method.
///
/// The *structural* bound is one iteration per instruction: `pc` advances
/// monotonically inside a range, the block walk's ranges are disjoint, and each
/// spliced body is entered once. Four visits per BYTE is therefore four times
/// looser than the loosest possible legal walk (an all-one-byte-opcode method),
/// which is the point — this is a backstop against a bookkeeping bug, not a
/// complexity policy, and it must never fire on a real method.
///
/// It exists because the walk had no enforced bound at all until 2026-09-17 and
/// a single mis-selected profiled branch could make it run forever; see
/// [`IrBuilder::prune_always_taken_branch`]. [`IR_MAX_GRAPH_NODES`] is not a
/// substitute: it is checked only once `build` has returned.
pub const IR_BUILD_STEPS_PER_CODE_BYTE: usize = 4;

/// Variant of [`ir_compatible`] that also enforces the bytecode-length budget
/// ([`IR_MAX_BYTECODE_SIZE`]). Kept as a separate entry point so callers that
/// do not know the method size stay source-compatible.
pub fn ir_compatible_sized(scan: &crate::x64::JitScanResult, code_len: usize) -> bool {
    if code_len > IR_MAX_BYTECODE_SIZE {
        return false;
    }
    ir_compatible(scan)
}

