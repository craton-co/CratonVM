> **j27 wave 2, second follow-up (lane NATMISC, 2026-10-28): host run showed no engagement (identical output
> with `CRATONVM_JIT_SPLICE_STATIC_MINMAX=0`).** (Superseded, orchestrator log 2026-10-28: that run used
> snapshot s3, which predates the resolver edit -- 0 hits of `sp_splice_emits_minmax_inline` in the s3
> binary; on snapshot s4 the `sp-splice-minmax ADMITTED` / `DECLINED` lines print, see the follow-up block
> below.) A re-read of the refusal site found the predicate is asked
> with the INNER call's triple (`target_class/name/desc` from the candidate body's constant pool), and the
> `REFUSED <candidate>` line always names the candidate, so the line itself does not show which triple was
> tested. No code reason for a `false` answer was found. Added a diagnostic: under `CRATONVM_DBG_JITC=1`, every
> single-pass `invokestatic` row in a candidate body that `try_resolve_intrinsic` answers prints
> `[cratonvm-jitc] sp-splice-minmax ADMITTED (kept past static-intrinsic-keeps-its-own-fast-path)
> java/lang/Math.max(II)I in J27SpliceMinMax.f(II)I depth=0 callee_pc=2 invoke_kind=3` when it engages, or
> `... DECLINED (...)` with the same tail when the predicate says no. No `sp-splice-minmax` line at all means
> the binary does not hold this code (check `strings <cratonvm> | grep sp-splice-minmax`, and run
> `cargo test -p cratonvm-vm j27_sp_splice_minmax_resolver_tests` on the host).
>
> **j27 wave 2 follow-up (lane NATMISC, 2026-10-28): REFLECT2-21-2 int/long min/max FIXED IN CODE (resolver
> half added), HOST-VERIFIED s4 (2026-10-28).** Host facts (orchestrator log, snapshot s4): engagement --
> `CRATONVM_DBG_JITC` prints the `sp-splice-minmax ADMITTED` / `DECLINED` lines; the s4 battery's J27
> probes are SAME in both modes, including the `CRATONVM_JIT_SPLICE_STATIC_MINMAX=0` arm. Not shown by the
> log: a timing A/B. `resolve_inline_site_from` (`jit_bridge.rs`) asks the new
> `sp_splice_emits_minmax_inline` (single-pass only, `invokestatic`, `cratonvm_jit::x64::spliced_static_minmax_entry`,
> now `pub`): such a row is exempt from `static-intrinsic-keeps-its-own-fast-path`, is counted bound by the
> "neither spliced nor direct-bound" rule, and keeps its `invoke_targets` entry through the retain. Same
> switch `CRATONVM_JIT_SPLICE_STATIC_MINMAX` (`=0` restores both refusals). The CMOV is now ONE emitter,
> `emit_int_long_minmax_cmov` (`inlining.rs`), used by the top-level intrinsic arm (`op_invoke.rs`, bytes
> unchanged) and the splice arm. Unit tests `cratonvm-vm`
> `runtime::interpreter::jit_bridge::j27_sp_splice_minmax_resolver_tests` (2). Verify: `CRATONVM_DBG_JITC=1`
> on `J27SpliceMinMax` with the default no longer prints `REFUSED ...f(II)I ...:
> static-intrinsic-keeps-its-own-fast-path` (nor the "neither spliced nor direct-bound" line for `g`/`h`), the
> facts equal HotSpot's, and `=0` brings the refusal back. Still open: `getClass` / `Integer.valueOf` /
> float-double min/max in single-pass splices, and residual 2.
>
> **j27 wave 2 (lane NATMISC, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** REFLECT2-21-2, emitter
> half only: `try_emit_inline_body`'s invoke arm (`jit/src/x64/inlining.rs`; the function is there, not in
> `lib.rs`) now emits a spliced `invokestatic` of `Math`/`StrictMath` `min`/`max` `(II)I` / `(JJ)J` as the
> top-level arm's CMOV (`try_emit_spliced_static_minmax`, `spliced_static_minmax_entry`) instead of a call,
> behind `CRATONVM_JIT_SPLICE_STATIC_MINMAX` (**default ON**, `=0` kill, read at compile time). Only these
> four rows: register-only, no CPU gate, no helper, no deopt edge; float/double min/max, `getClass` and
> `Integer.valueOf` are not served. NOT done (resolver, `vm/src/runtime/interpreter/jit_bridge.rs`, outside
> this lane's files), and without it the new arm is reached only where a splice is already admitted with a
> recorded call at such a pc (e.g. `CRATONVM_JIT_SPLICE_KEEPS_STATIC_INTRINSIC=0` with a direct-bound
> target): (1) in `resolve_inline_site_from`, exempt a single-pass (`!ir_mode`) `0xb8` row from
> `static-intrinsic-keeps-its-own-fast-path` when the emitter serves it (export `spliced_static_minmax_entry`
> as a `pub fn` from `cratonvm_jit` for the test); (2) keep its `invoke_targets` entry (the emitter needs the
> `ResolvedInlineInvoke` row for the triple) and count its pc as bound in the "neither spliced nor
> direct-bound" rule. Residual 2 (IR `Integer.valueOf` box lowering in splices) unchanged. Unit tests
> `cratonvm-jit` `x64::inlining::j27_spliced_static_minmax_tests` (2). Probe
> `tools/probes/jit/j27/J27SpliceMinMax.java`, expected HotSpot 25: `fact max-int-sum 2428571`,
> `fact min-long-sum -3600000`, `fact min-int-neg-sum -14000000` (timing row with `-Dj27.time=true`). Verify:
> the probe's facts with default and `CRATONVM_JIT_SPLICE_KEEPS_STATIC_INTRINSIC=0` (the arm that reaches
> the new code), each against `=0` on the new switch.

> **j27 wave 1 (lane JNINAT, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** REFLECT2-21-3 (diagnostic) FIXED
> IN CODE: the constant-pool screen's refusal in `resolve_inline_site_from` (`jit_bridge.rs`) now reads
> `native-shadow (kind=<NativeKind>[; the call is intrinsified at the site (sp|ir|sp, ir)])`, built by
> `native_shadow_refusal_reason` / `native_shadow_refusal_text` only under `CRATONVM_DBG_JITC` (asks
> `cratonvm_jit::try_resolve_intrinsic` and `cratonvm_jit::ir::try_ir_scalar_intrinsic`). The reason still
> starts with the token `native-shadow` + a space, so `tools/scripts/jit-native-shadow-rank.py`
> (`native-shadow\S*`) counts the same reason. No switch (debug text only). Unit tests
> `cratonvm-vm` `runtime::interpreter::jit_bridge::j27_native_shadow_refusal_text_tests` (2). Host check:
> `CRATONVM_DBG_JITC=1` on `R21ReflectInvoke` prints `REFUSED java/lang/Math.max(II)I ...: native-shadow
> (kind=Intrinsic; the call is intrinsified at the site (sp, ir))`; `Integer.valueOf` / `Object.getClass` print
> their kind (their call-site lowering is the box / getClass prefixes, which are not in either intrinsic table, so
> no "intrinsified" suffix -- a known limit of the text). REFLECT2-21-2 NOT FIXED: it is an emitter change in
> `jit/src/lib.rs` `try_emit_inline_body` (run the intrinsic ladder inside a single-pass splice) and the IR
> splice's box lowering -- outside this lane's files; unchanged proposal in `jit-r21-reflect2-proposals.md`.

# `inline-resolve REFUSED Object.getClass / Integer.valueOf / Math.max: native-shadow` -- what they cost

Status: PARTLY (j27, 2026-10-28): REFLECT2-21-3 (diagnostic) FIXED IN CODE, PENDING HOST VERIFICATION (wave 1, lane JNINAT); REFLECT2-21-2 int/long min/max FIXED IN CODE, HOST-VERIFIED s4 (wave 2, lane NATMISC); `getClass` / `Integer.valueOf` / float-double min/max in single-pass splices and residual 2 still OPEN. Round-21 status: OPEN (round-21 host check, 2026-10-15: REFLECT2-21-2 / REFLECT2-21-3 not built; no round-21 log bears on them). Previous status: OPEN -- no defect in the reflective path; one residual (single-pass splices) and one diagnostic gap filed as
proposals REFLECT2-21-2 / REFLECT2-21-3 in `jit-r21-reflect2-proposals.md`. No resolver change made.

Area: `vm/src/runtime/interpreter/jit_bridge.rs` (`resolve_inline_site_from`, `inline_screen_native_shadows`),
`jit/src/lib.rs` (`splice_intrinsic_jdk_body_admitted*`), `jit/src/ir.rs` (`try_ir_scalar_intrinsic`).
Found by: JIT round 21 lane reflect2 (`CRATONVM_DBG_JITC=1` over `R21ReflectInvoke`: 97x `REFUSED
java/lang/Object.getClass: native-shadow`, plus `Integer.valueOf`, `Math.max`).

## What the lines are

`resolve_inline_site_from` refuses to SPLICE the JDK body of a triple whose registered native runs instead
(`inline_screen_native_shadows`: any registration but a yielding synthetic stub), unless the triple is a reviewed
pure-leaf row (`cratonvm_jit::splice_intrinsic_jdk_body_admitted_at` + `native_is_intrinsic`). The three triples:

| triple | registration | what the JIT does at the call instead |
|---|---|---|
| `Object.getClass()` | `native` in the JDK itself (no bytecode to splice) | single-pass: `is_object_get_class_site` intrinsic; IR: the `getClass()` prefix of `ir_lower` (and `ir_getclass_compares` folds inside splices) |
| `Integer.valueOf(I)` | `Intrinsic` (boxing cache) | single-pass: the box intrinsic (`lib.rs`, `box_calls`); IR: box ops / EA (`ea_ir_bridge.rs` `IntValue`) |
| `Math.max(II)I` | `Intrinsic` (`lang_math.rs`, category set explicitly) | IR: `ScalarOp::MaxI` at the method's own site AND inside an IR splice (`ir_splice_lowers_static_scalar_row`); single-pass: `JitIntrinsic::MathMaxInt` |

So at a compiling method's own site the refusal costs nothing: the resolver is asked, declines, and the intrinsic
serves the call. The sanctioned mechanism for admitting a shadowed body (`splice_intrinsic_jdk_body_admitted`) is for
a JDK body that is a pure leaf with no VM state, which is the wrong answer for all three: `getClass` has no body,
`Integer.valueOf` reads `IntegerCache` and allocates, and `Math.max(II)I` is already lowered as arithmetic where a
splice would otherwise take it (the reason `Math.abs(J)J` had to be made nested-only in nio21 wave 14).

None of these is on the reflective path's per-call cost (see
`jr21-reflect2-method-invoke-per-call-link-work-FIXED-20261015.md`): `Object.getClass` there is
`DirectMethodHandleAccessor.checkReceiver` / `Method.invoke`'s `obj.getClass()` for instance methods, compiled.

## Residuals (not fixed here: shared resolver, other lanes' areas)

1. Single-pass tier: a body that CALLS one of the three is refused whole when spliced -- `Math.max` by
   `static-intrinsic-keeps-its-own-fast-path` (`jit_splice_keeps_static_intrinsic`), `getClass` / `Integer.valueOf`
   by "neither spliced nor direct-bound" (`jit_bridge.rs` after the nested resolution) -- because
   `try_emit_inline_body` lowers a spliced call to the blind dispatch helper and the intrinsic ladder does not run
   inside a spliced body. Only the IR tier is exempt. A fix is an emitter change (run the intrinsic ladder for the
   three inside `try_emit_inline_body`, then count them as bound for that rule) -- proposal REFLECT2-21-2.
2. IR tier: `Integer.valueOf` inside a SPLICED body becomes a plain `Op::Call` (no box lowering there) unless EA
   removes the box. Same proposal.
3. The refusal line does not say the call is intrinsified, which is how it was read as an inlining blocker
   (REFLECT2-21-3: name the intrinsic in the `no!` text when `try_resolve_intrinsic` / `try_ir_scalar_intrinsic`
   answers for the triple).

## How to see it

`CRATONVM_DBG_JITC=1` on any probe whose hot method calls `Math.max(II)I` / `Integer.valueOf(I)`: the `REFUSED ...
native-shadow` line for the site, and the method's own compile still lowers the call (IR: `MaxI` / box ops; a
`perf` profile shows no `native_math_max_int` / no native-funnel frame for it). Single-pass residual: a static helper
`int f(int a, int b) { return Math.max(a, b) + 1; }` called from a hot loop that the single-pass tier compiles prints
`inline-resolve REFUSED ...f(II)I ...: static-intrinsic-keeps-its-own-fast-path` and keeps `f` a call.
