# JSP compilation fails: Eclipse JDT parser `ArrayIndexOutOfBoundsException`

**Current status:** FIXED and retired to `..` (2026-07-08).

The original targeted repro was fixed on branch
`fix/jasper-jdt-parser-aioobe-20260706` (2026-07-06). Later Windows residuals
in the default full-class order were closed on 2026-07-08 by keeping the Eclipse
JDT parser package (`org/eclipse/jdt/internal/compiler/parser/`) interpreted
under the conservative JIT policy. The first residual was JIT-only and
order-dependent: `testBug55262` passed in a fresh JVM, `testBug51584` followed
by `testBug55262` failed with `ArrayIndexOutOfBoundsException: Index -1 out of
bounds for length 100`, and the same two-method sequence passed with `--nojit`.

JIT bisection narrowed the residual to the Eclipse JDT parser package:
`CRATONVM_JIT_BISECT_ONLY=org/eclipse/jdt/internal/compiler/parser/` still
failed, while adding
`CRATONVM_JIT_BISECT_SKIP=org/eclipse/jdt/internal/compiler/parser/Parser.consumeRule`
made the two-method repro pass. After rebasing onto newer `dev`, the full class
could still surface parser-adjacent heap corruption/OOM around the
`testBug53257*` sequence unless the parser package was interpreted. The current
fix is therefore a correctness-first parser-package skip until the backend issue
in that generated parser switch/stack-update shape is root-caused.

Closure evidence:
- Two-method release repro with the fixed binary
  `cratonvm-jasper-jdt-residual-20260708-003.exe`:
  `RunOne org.apache.jasper.compiler.TestCompiler testBug51584 testBug55262`
  -> `tests=2 failures=0 ignored=0`.
- Full standard Tomcat suite-runner class, no method exclusions:
  `jasper-residual-20260708-010-final-current-dev-parser-guard`, real JDK, JIT on, no
  `CRATONVM_JIT_DENY` override, `org.apache.jasper.compiler.TestCompiler` ->
  `PASS` in 500.4s.

## Historical Status Notes

**Status:** ✅ FIXED on branch `fix/jasper-jdt-parser-aioobe-20260706` (2026-07-06)
for the targeted repro. **Residual observed on Windows (2026-07-06/07):**
running the full `TestCompiler` class through the standard suite runner
(`org.junit.runner.JUnitCore`, no method exclusions — unlike this doc's own
verification, which explicitly excludes `testBug51584`) still shows problems,
in two different forms across two separate runs on a binary confirmed to
include this fix (`git merge-base --is-ancestor e60b7a5c` true):
- One run: `testBug53257g` still failed with
  `ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 100`
  (note: length 100, not the original 50 — a different call site/data size
  hitting a similarly-shaped defect).
- A second, independent run: the whole class never printed a single test-case
  line and hung for the full 300s timeout, with `WARN
  cratonvm_classloading::jar_signer: JDK cacerts=...cacerts PKCS#12 parse
  failed: PKCS#12: bag decode failed` / `jar signer: rejecting signer block:
  SignerInfo is missing authenticatedAttributes` in the log immediately
  before the hang — possibly an unrelated jar-signature-verification issue
  (Eclipse JDT's `ecj` jar is signed) rather than a recurrence of the AIOOBE,
  not yet distinguished from the original bug.

Not yet re-tasked pending closer isolation — this doc's own verification
methodology (excluding `testBug51584`, Linux-only for some of this session's
other work) may not fully cover the natural/default test-class execution
order that the standard runner and HotSpot both use. Whoever picks this up
next should first determine, with the class run start-to-finish with no
method exclusions on a fresh binary, whether: (a) the length-100 AIOOBE and
the hang are the same underlying defect surfacing differently run-to-run
(non-deterministic, like the "double execution" root cause described below),
(b) the hang is really the unrelated cacerts/jar-signer issue, and (c) if (a),
whether the fix's fourth sub-fix (arraycopy args-buffer aliasing) has a
similar unaddressed case for a different array shape/size than the one
originally isolated.
**Severity:** was medium (broke JSP compilation for specific source shapes);
possible residual severity TBD pending the above.

## Summary

`org.apache.jasper.compiler.TestCompiler` (`testBug53257f`/`testBug53257g`,
order-dependent) failed compiling a generated JSP servlet with
`ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 50` inside
**Eclipse JDT's own bundled Java parser**
(`org.eclipse.jdt.internal.compiler.parser.Parser`) — not Jasper's JSP-to-Java
translation. HotSpot compiles the identical generated Java source through the
identical bundled JDT parser without error, so the defect was in CratonVM,
not JDT.

Found via a full 651-class Apache Tomcat suite rerun (`osr600verify`).

## Root cause (three independent bugs, all in `../../../jit/src/x64.rs` / `../../../jit/src/lib.rs`)

The JDT `Parser` class repeatedly uses the idiom `this.intStack[this.intPtr--]`
(read the top of an internal parse stack and post-decrement the pointer) —
compiled by javac as `getfield; getfield; dup_x1; iconst_1; isub; putfield;
iaload`. `Parser.consumeTypeImportOnDemandDeclarationName` (and, once
JIT-warmed, `Parser.consumeBlock`/`consumeRule`) use this idiom immediately
before a `System.arraycopy(charArrayField, ..., ...)` call whose source is a
**reference-element array** (`char[][]`, `identifierStack`) — this call
*always* fails System.arraycopy's JIT-inlined "same primitive element kind"
guard, since JDT's identifier stacks are declared `char[][]`, not `char[]`.

That combination exposed three separate, independently-real defects:

1. **`getfield` never marked a reference-typed field's result as a GC oop on
   the JIT operand stack** (`../../../jit/src/x64.rs`, the `0xb4` handler's three
   codegen paths — compact-inline, standard-inline, and the runtime-helper
   fallback). Every OTHER stack-producing opcode in this file
   (`aload`/`aaload`/`dup`/`dup_x1`/…) calls `mark_top_as_oop()` when the
   pushed value is a reference; `getfield` never did, for any of its three
   codegen paths. A reference field's value therefore decayed to a plain
   `FrameValue::Int`/`Value::Int` at any GC-safepoint or deopt boundary that
   captured it — confirmed directly via `CRATONVM_DBG_DEOPT=1`, which showed
   `identifierStack`'s value captured as `Int(4332917944)` (its raw pointer
   bits, mistyped) instead of `Object(...)`.
   **Fix:** call `self.mark_top_as_oop()` in all three `getfield` paths when
   `type_tag`/`c_is_ref` indicates a reference field.

2. **The precise local-variable oop dataflow used for deopt-resume never saw
   `this`/reference parameters as oops**, because `param_oop_mask` (which
   seeds that dataflow) was computed only when `precise_jit_maps_enabled()`
   or `moving_young_enabled()` was on — both default-OFF feature flags
   unrelated to `deopt_real_enabled()` (default-ON). `local_kinds`
   (populated unconditionally under `deopt_real_enabled()`) correctly
   classified `this` as `LocalKind::Ref`, but with an all-zero oop mask
   `typed_local_frame_value`'s `LocalKind::Ref => FrameValue::Unsupported`
   arm always fired — so `this` was `Unsupported` at *every* deopt point in
   *every* method, unconditionally.
   **Fix:** also seed `param_oop_mask` under `deopt_real_enabled()`, at all
   three call sites (`../../../jit/src/lib.rs`'s hot-path `try_compile`,
   `../../../vm/src/runtime/interpreter.rs`'s early-compile and OSR-compile seed
   sites).

3. **The `System.arraycopy` fast-path intrinsic's only bail strategy was a
   deopt trap whose sole historical resume mechanism was a whole-method
   re-run** — safe only when the method performed no observable side effect
   before the call, an invariant the intrinsic's own design comment
   acknowledged relying on ("never trade correctness for inlining") but that
   this JDT method violates (the `intStack`/`identifierLengthStack` pointer
   decrement is a real heap write that happens *before* the arraycopy call
   in the same invocation). A `char[][]` array. like `identifierStack`,
   *always* fails the intrinsic's reference-vs-primitive guard, so *every*
   call deopted, and whichever call-dispatch path handled the return (there
   turned out to be two: one that attempts `real_frame_deopt_resume_and_
   despeculate`, and an older one at `interpreter.rs`'s JIT-fast-call site
   that *never* attempts precise resume and unconditionally re-runs the
   whole method) would, on the naive path, silently repeat the earlier
   pointer decrement — confirmed directly by instrumenting a synthetic
   repro with a call counter: `consumeLike()` was invoked **twice** for one
   logical call (`callsMade=2`), decrementing the stack pointer twice and
   eventually driving it to -1.
   **Fix:** stop relying on deopt-and-rerun for this intrinsic's bail path
   entirely. `../../../jit/src/lib.rs` now additionally registers a normal
   `JitInvokeInfo` dispatch fallback (`invoke_info`) for every
   `System.arraycopy` call site, identical to what a non-intrinsic
   `invokestatic` site gets. `../../../jit/src/x64.rs`'s arraycopy intrinsic routes
   every guard failure (null, non-array, reference-element array,
   mismatched kind, out-of-bounds — ALL of them, not just the
   reference-array case) through a normal `jit_invoke_dispatch` CALL using
   that registered info, instead of the deopt-stub. A guard failure now
   throws or succeeds via ordinary call/exception semantics — no re-run, so
   no double-executed side effect. Falls back to the historical deopt trap
   only if `invoke_info` wasn't registered for some reason (defensive; not
   expected to trigger for this intrinsic). Also fixed the arraycopy
   intrinsic's de-speculation: it never consulted the existing per-bci
   `despec_contains` registry before re-emitting its guard on recompile
   (unlike the loop-header speculative-BCE guards), so a call site proven to
   always fail kept re-triggering compile/deopt/evict cycling forever; it
   now skips the speculative fast path once de-specced.

   **A fourdth, self-inflicted bug found and fixed during (3)'s
   implementation:** the new args-buffer construction for the direct-dispatch
   redirect initially read-then-stored each of the 5 arraycopy arguments
   into a buffer that ALIASED the same 5 scratch-home frame slots the values
   were being read FROM (the reserved args-buffer offset coincided with the
   scratch homes' own base, since arraycopy's scratch homes are
   intentionally never counted against `next_spill_offset`) — and the
   target buffer order is the *reverse* of the scratch-home order, so a
   naive per-index load-then-store overwrote a not-yet-read source. Fixed by
   loading all five operands into distinct registers FIRST, then storing —
   the same aliasing-defeat pattern already used (and explicitly commented)
   for the fast-path's own guard setup a few lines above.

## Symptom (pre-fix)

```
HTTP Status 500 — Internal Server Error
Message: org.apache.jasper.JasperException: Unable to compile class for JSP
Root Cause: java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 50
	at org.eclipse.jdt.internal.compiler.parser.Parser.consumeRule(Parser.java:7045)
	at org.eclipse.jdt.internal.compiler.parser.Parser.parse(Parser.java:11701)
	...
```

A second failure signature in the same class run:
```
java.lang.ArrayIndexOutOfBoundsException (no message)
	at org.eclipse.jdt.internal.compiler.parser.Parser.consumeTypeImportOnDemandDeclarationName(Parser.java:9708)
	at org.eclipse.jdt.internal.compiler.parser.Parser.consumeRule(Parser.java:6721)
```

## Reproduction

Real-world (Tomcat, order-dependent — `testBug51584` earlier in the class
hangs on an unrelated bug, so it must be excluded to reach the target test in
the same JVM/JIT-warm state):
```powershell
# compile a tiny custom JUnit runner (RunExcept.java) that filters out
# testBug51584 via org.junit.runner.manipulation.Filter, then:
cratonvm.exe ... -cp <scratch>;<tomcat cp.txt> RunExcept org.apache.jasper.compiler.TestCompiler testBug51584
```

Minimal synthetic repro (no Tomcat needed, reproduces in ~13,000 iterations):
a class with an int-stack pointer decrement idiom (`arr[this.field--]`)
immediately followed by a `System.arraycopy` call whose source is a
reference-element array (e.g. `char[][]`), inside a tight loop that also
allocates (GC/JIT-recompile pressure required — a version without allocation,
or with a primitive-array arraycopy, does not reproduce). See the T-series
probes (`T3`/`T8`/`T13`) built during this investigation for the exact shape;
add a call-counter field to directly observe the double-execution
(`callsMade == 2` per logical call, pre-fix).

## Verification

- All 4 fixes together: real Tomcat repro now `11/11, 0 failures`.
- 12 synthetic T-series repros (T3–T14), up to 100,000 iterations each: 0
  mismatches, 0 double-execution anomalies (was reliably reproducing by
  ~13,000 iterations pre-fix).
- `cargo test -p cratonvm-jit --lib` (debug mode): 877/877 pass (4 unrelated
  AArch64 `debug_assert!`-based tests fail only under `--release`, where
  `debug_assertions` is compiled out — pre-existing gap in those tests'
  invocation convention, unrelated to this fix).
- 80-class Tomcat regression sample (real JDK, JIT on, classes that PASS on
  the `overnight0629c` baseline): 71 PASS, 9 non-PASS. All 9 verified
  byte-for-byte identical (same PASS/FAIL/TIMEOUT outcome) between this
  branch's binary and an unmodified `dev` baseline binary when re-run
  serially — i.e. all 9 are pre-existing issues or `-Parallel 4` contention
  noise, not regressions from this fix. Two of the 9
  (`jakarta.el.TestBeanSupport`, `jakarta.el.TestImportHandlerStandardPackages`)
  are the already-documented pre-existing findings from the OSR regression
  cluster investigation ([[jit-osr-backedge-value-corruption-cluster]]).

## Related

The arraycopy intrinsic's design comment ("reference arrays intentionally
bail to native — never trade correctness for inlining") is now honored via a
direct dispatch call instead of a deopt trap; the same "deopt trap whose only
resume path is an unsafe whole-method re-run" pattern likely affects other
speculative JIT intrinsics/guards that can be reached after an earlier
observable side effect in the same method — worth a broader audit if a
similar double-execution symptom recurs elsewhere.
