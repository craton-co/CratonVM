# One deopt sink resumed a trapped frame and the other declared the same frame unusable — `refusing side-effecting replay`

## Status

**FIXED 2026-09-07** (`CRATONVM_JIT_DEOPT_SINK_RESUME`, default ON).

This page carries the mechanism and the fix. The two known-issue pages it
closes kept their own evidence and moved here beside it:

* `precise-deoptimization-unavailable-cross-suite-crash-20260907-FIXED.md` —
  the H2 + Spring population (8 H2 CRASH classes, the entire CRASH story for
  that run).
* `jit-precise-deopt-refused-transfer-to-interpreter-hibreactive-20260907-FIXED.md`
  — the hibernate-reactive population (7 classes, 17 occurrences, GC-independent
  across all three collectors).

A third page, `inline-trap-inside-a-protected-range-FIXED-20260818.md`, is the
earlier and narrower fix in the same family. This one does not replace it; see
*Relationship to the 2026-08-18 fix* below.

## The symptom

```
java.lang.InternalError: JIT dispatch into <callee> failed: internal error:
  precise deoptimization unavailable for <callee> at bci <N>
  (can_deopt_resume=false (no deopt points, or an elided monitor),
   stashed key "<callee-signature>", inline callers 0,
   reason TransferToInterpreter); refusing side-effecting replay
```

A hard process abort, not a catchable test failure: every class that hit it
lost all its remaining test methods.

## The defect

Two sinks consume a stashed IR deopt frame, and which one a trap reaches
depends only on how the callee was entered:

| sink | reached when | what it did |
|---|---|---|
| `try_resume_trapped_callee` (`vm/src/jit/helpers.rs`) | a compiled caller's dispatch helper invoked a callee that **already has** an artifact | rebuilt the frame with `build_deopt_frame_inner` and **resumed it precisely** |
| `execute`'s tier-up sink (`vm/src/runtime/interpreter.rs`, traced `execute-first-call-tierup`) | `execute` itself invoked the artifact | rebuilt the frame with `build_deopt_frame_inner` — but only under `compiled.can_deopt_resume`, and **raised the abort above** otherwise |

Same stash. Same builder. Opposite verdicts.

And `can_deopt_resume` is close to the wrong question for this sink to ask —
close, because it does carry weight on one path, which the fix below replaces
rather than discards. It is finalized two different ways:

* the **single-pass** backend sets it honestly —
  `!cm.deopt_points.is_empty() && !compiler.has_elided_monitor`
  (`jit/src/x64/driver.rs:2818`);
* the **optimizing IR** backend sets it only inside a condition requiring
  `sr_map.is_some()` (`jit/src/ir_lower.rs`), which is populated only under
  `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL` — two non-default debug
  flags.

So on a production artifact from the optimizing tier the flag is **false for
every method, with no exceptions**, and every trap taken in such a body that
reached the tier-up sink, in a method committing any side effect (any store,
any call), aborted. `replay_from_entry_is_observably_equivalent` — the sink's
only other escape hatch — cannot help there by definition: it fires exactly
when the body has no side effect to duplicate.

`can_deopt_resume` gates a **different** consumer:
`resume_real_ir_deopt`'s scalar-replacement materialisation. The tier-up sink
was asking a question nothing on its own path needed answered.

## Why the trap is not rare

The optimizing tier PLANTS an unconditional uncommon trap at every
`invokedynamic` it cannot lower (`ir::ir_site_trap_enabled`, **default ON** —
`IrBuilder`'s `0xba` arm, `plant_uncommon_trap`). That is not a
mis-speculation that might never fire: it fires the first time the compiled
body reaches the site. Deopt guards for array/field access and division
(`emit_array_null_bounds_guards`, `emit_deopt_if_zero`) are the other door.

Measured population on 2026-09-07, three unrelated suites, one day:

* **H2**: all 8 CRASH classes of the full 218-class 3-GC-arm run — the entire
  CRASH population, no other crash mechanism found.
* **hibernate-reactive**: 7 classes, 17 occurrences across 3 GC arms × 3
  shards, identical on Generational, G1 and ZGC.
* **Spring Framework**: `CrossOriginAnnotationIntegrationTests`
  (`CorsConfiguration.addAllowedOriginPattern` / `.addAllowedOrigin`).

## The second half: the trap never stopped firing

The tier-up sink also de-speculated with the **deopt point's own reason**. For
a trapping bci that carries a safepoint that reason is
`TransferToInterpreter`, and `recommend_action` maps it to `Reinterpret` — the
artifact stays live, the next call re-enters it, and it traps again. Forever.

`try_resume_trapped_callee` has known better since
`aadc860f2` (*take the site-trap decision ONCE*): for an IR **site trap** it
records `SpeculationFailed` instead, which recompiles rather than blacklists,
and arms `ir_evidence`'s memo so the recompile goes single-pass — which lowers
`invokedynamic` perfectly well and does not trap. `MakeNotCompilable` would be
worse than useless here: `compile_gate` consults it, so the method would lose
its body on **every** tier.

That policy now lives in one function, `despeculate_trapped_method`, which both
sinks call. Two sinks that disagreed about the same frame had also disagreed
about what to do with the method that produced it.

"Once" is `ir::claim_site_trap_decision` — a set of its own, and deliberately
not `ir_evidence`'s refusal memo, which has a second writer (the acceptance
gate marks a method refused whenever it discards an optimizing body). Reading
that memo as the flag would let a gate-refused method look already-decided on
its FIRST trap, so the eviction would never happen at all; see `81c9c9fd7`,
which found that hole in the helper the day this extraction was made.

## The fix

`vm/src/runtime/interpreter.rs` — the tier-up sink attempts the resume whenever
the stash is this method's and the frame materialises, `can_deopt_resume` or
not. `build_deopt_frame_inner` is self-guarding: it returns `None` on an
inlined caller chain, an identity mismatch, a `u32::MAX` bci, an unmappable
slot, or a malformed monitor, and the sink still refuses on a `None`.

The change is **strictly additive**: the old `can_deopt_resume` condition is
left exactly as it was, and the new behaviour is a second arm beside it. That
matters, because a backend that SET that flag has already vouched no monitor
was elided — hanging the new guards on that arm too would refuse a single-pass
body with an ordinary `synchronized` block that resumes correctly today,
turning a working path into the very abort this removes.

Three refusals govern the NEW arm, and they are the ones the reconstructed
frame genuinely cannot answer — the same three the sibling sink makes or the
emission side names:

* an **`ACC_SYNCHRONIZED`** method — the method monitor is not in the frame
  (`try_resume_trapped_callee` refuses this shape too, for this reason);
* a body that **takes a monitor at all**. Every `FrameState` `ir_lower` builds
  hard-codes `monitors: Vec::new()`, so a resumed frame for such a body
  believes it holds no lock. `ir_lower`'s own comment says the interpreter's
  sink "cannot fire on information that was never recorded" — the new
  `cratonvm_jit::bytecode_holds_monitor` is that information, read off the
  bytecode instead of off the frame. Whole-body and conservative, for the
  reason `ir_unresumable_protected_trap`'s side-effect scan is: pc order is not
  execution order;
* a **resume bci past the method's code**.

Those three are also what covers the one thing `can_deopt_resume` was really
protecting. On the SINGLE-PASS side the flag is set honestly —
`!deopt_points.is_empty() && !has_elided_monitor` — and the second conjunct is
real: escape analysis may elide a `monitorenter` over a non-escaping object,
and an elided monitor leaves no trace in the reconstructed frame. But eliding
is a codegen decision, not a bytecode rewrite, so such a body still CONTAINS
the monitor ops, `bytecode_holds_monitor` is true for it, and the resume is
refused on evidence the sink can actually see. `ACC_SYNCHRONIZED` covers the
method-level monitor the same way. What is left — `deopt_points.is_empty()` —
describes an artifact no trap can arrive at.

The two halves fit because an elided monitor *forces* `can_deopt_resume` false,
so such a body reaches the NEW arm, where the guard is — and a body whose
monitor was NOT elided keeps the flag, takes the old arm, and resumes exactly
as it always did.

The refusal message now names which of those declined, instead of blaming
`can_deopt_resume` — a flag that, on an optimizing artifact, is false whatever
anyone does, and so sends the next reader after something that is not the
cause.

## Evidence

`cratonvm/CompiledNpeMessage.storeToNull` — `NULL_ARRAY[0] = warm`, one
`iastore` through a null array reference. The IR tier lowers the store's null
check to a deopt guard; `iastore` is itself `opcode_commits_side_effect`, so
the replay hatch is closed. Reproduces the field signature exactly, in 0.1 s,
in-repo:

```
precise deoptimization unavailable for cratonvm/CompiledNpeMessage.storeToNull(I)I
  at bci 21 (can_deopt_resume=false (no deopt points, or an elided monitor),
  stashed key "cratonvm/CompiledNpeMessage.storeToNull:(I)I",
  inline callers 0, reason TransferToInterpreter); refusing side-effecting replay
```

Same `reason TransferToInterpreter`, same `inline callers 0`, same populated
stash, same `can_deopt_resume=false` as every field occurrence.

| arm | file | outcome |
|---|---|---|
| default (guard ON) | `vm/tests/jit_deopt_sink_resumes_a_side_effecting_trap.rs` | resumes; the interpreter re-executes the `iastore` and raises `NullPointerException` |
| `CRATONVM_JIT_DEOPT_SINK_RESUME=0` | `vm/tests/jit_deopt_sink_resume_off_arm.rs` | the original abort, verbatim |

One binary, one switch, two outcomes — so the ON arm's result is evidence that
the switch is what produces it, not just an outcome that happens to be green.

**The tier is load-bearing in both files.** Without the `CRATONVM_TIER_*`
overrides they install, `storeToNull` lands at C1, where the single-pass
backend sets `can_deopt_resume` itself and always resumed — a green run against
that would have asserted nothing. Both files check
`CompiledMethod::used_ir_backend` and panic rather than proceed.

That witness was already in the tree, recorded and excluded:
`jit_npe_message_from_compiled_code.rs` carried
`const UNASSERTED_STORE_SHAPE = "storeToNull"` with a comment naming this exact
abort as a known divergence it would not assert around. The shape is now the
third row of that file's `SHAPES` table.

## What this does NOT change, and the residual

The two sinks in `jit_bridge.rs` (`jit-callsite-a`, `jit-callsite-b`) take a
**third** answer to the same event: they fall back to a whole-method re-run
from entry, unconditionally, with no side-effect check and no refusal. For a
side-effecting body that is a silent double execution — worse than the abort,
not better — and it is untouched here.

It is not reachable through the shape this page fixes (those sinks run
`resume_from_ir_deopt` first, and reach the re-run only when it declines or is
gated off), and changing them would alter behaviour on paths that work today,
unmeasured. **Left open deliberately**, recorded in
`docs/known-issues/jit/jit-bridge-sinks-re-run-a-side-effecting-body-20260907.md`.

## Relationship to the 2026-08-18 fix

`inline-trap-inside-a-protected-range-FIXED-20260818.md` attacks the same
family from the compiler side: `ir_unresumable_protected_trap` declines
optimizing-tier admission for a method whose deopt-guarded opcode sits inside a
protected range that also commits a side effect, so the body goes single-pass
and never needs a precise resume.

That fix is narrow by design — its own page argues why, quoting
`unresumable-unconditional-trap-mvmap-FIXED-20260802.md`: *"Do not apply the
publish-side rule blind… the naive form would refuse every trap-carrying
artifact."* Today's population is outside it: none of the affected methods is
obviously a try/catch body, and an `invokedynamic` site trap has nothing to do
with a protected range.

Both remain. The 2026-08-18 fix avoids the resume; this one makes the resume
work when it is needed anyway. Widening the admission refusal to cover today's
population instead would have declined the optimizing tier for every method
containing a `String +` in a non-constant expression, which is ordinary Java.

## Files

* `jit/src/lib.rs` — `bytecode_holds_monitor`, `deopt_sink_resume_enabled`.
* `vm/src/jit/helpers.rs` — `despeculate_trapped_method`, extracted from
  `try_resume_trapped_callee` so both sinks ask one function.
* `vm/src/runtime/interpreter.rs` — the tier-up sink.
* `vm/tests/jit_deopt_sink_resumes_a_side_effecting_trap.rs`,
  `vm/tests/jit_deopt_sink_resume_off_arm.rs` — the A/B.
* `vm/tests/jit_npe_message_from_compiled_code.rs` — the retired exclusion.
