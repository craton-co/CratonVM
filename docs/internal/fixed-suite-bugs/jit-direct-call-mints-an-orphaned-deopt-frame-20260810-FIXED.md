# A statically-bound JIT-to-JIT direct call minted an orphaned deopt frame — FIXED 2026-08-10

## Status
**FIXED 2026-08-10.** Was
`docs/known-issues/h2/jit-inlined-callee-deopt-frame-has-no-caller-chain-20260810.md`
(OPEN for one day). **Its title names the wrong mechanism** — inlining has
nothing to do with it — so this page is filed under the mechanism that was
actually measured. Search for the old name if you arrive from a citation.

## What the page said, and what is true

The page observed a real thing: a frame stashed in `jit/src/deopt.rs`'s
`LAST_DEOPT`, keyed to `org/h2/util/StringUtils.cache`, reaching a consumer
that could not attribute it, while `org.h2.value.ValueVarchar.get` was on the
dispatch path. Its diagnosis was that the JIT had INLINED `cache` into `get`,
that the trap therefore came out keyed to the inlinee with an empty
`caller_frames`, and that the fix was to make the compiler emit a frame CHAIN
so `try_resume_trapped_callee` could match on the outermost key.

Two things rule inlining out without running anything:

* `x64::Compiler::try_emit_inline_site` refuses any spliced body that published
  deopt metadata — it checks the postcondition (`deopt_stubs` /
  `deopt_points` grew) and rolls the whole splice back. An inlined body cannot
  publish a deopt point and survive.
* Even if it could, `build_and_record_deopt_point` bakes `self.method_key` —
  the CALLER's. An inlined trap would come out keyed `ValueVarchar.get`, not
  `StringUtils.cache`. The observed key is the callee's, which is what a
  SEPARATELY COMPILED callee stashes.

## The actual mechanism

`jit/src/lib.rs`'s call-site scan has a ladder that binds a statically-bound
callee's compiled entry as a raw `CALL` (the `callee_compiler` bind, guarded by
`direct_jit_callee_calls_enabled` and `matches!(invoke_kind, 1 | 3)`). It pushed
a `direct_calls` entry and then `continue`d — skipping the generic
`invoke_info.push((pc, info_ptr))` at the end of the loop.

The x64 emitter only emits `emit_inline_callee_deopt_check` after a direct CALL
when it HAS a `JitInvokeInfo` for that pc. With none, there is nothing between
the callee's `RET` and the caller's own code that can notice the callee trapped:
the callee's `i64::MIN` sentinel falls into the caller's shared
exception-check stub, which reloads the sentinel and runs the epilogue. The
caller returns the sentinel as if IT had deopted, and the frame the callee
stashed under its own key keeps travelling outward looking for "the outer
consumer that CAN attribute it" — which, once the sentinel has passed through
the callee's own caller, no longer exists.

The sibling arm of the same bind (the INLINE-BAIL FALLBACK, added for tomcat
doc 04) already documents in its own comment that it deliberately does NOT
`continue` "so the RBC.3 `JitInvokeInfo` fallback is still registered below".
Only this copy skipped it.

## Measured, not argued

A one-line diagnostic under `CRATONVM_DBG_DEOPT` names every direct call site
that gets no service check, and one line names the moment a sentinel leaves a
serviced site with a stash still present. On
`org.h2.test.scripts.TestScript` (real-JDK 25, `--Xmx 2g`, Azure host):

| | unserviced direct call sites | orphan events |
| --- | --- | --- |
| before | 582 (419 invokestatic, 90 invokestatic-tail, 73 invokespecial/virtual) | 2 |
| after the `invoke_info` fix | 220 → 400 by end of run, all intrinsic/native-helper binds + 89 tail | 0 |
| after the tail-call fix as well | 131, all intrinsic/native-helper binds | 0 |

The two orphan events before the fix are the page's own case, verbatim:

```
[cratonvm-deopt] callee-resume refused (stash is not this call site's callee):
    stash=org/h2/util/StringUtils.cache:(Ljava/lang/String;)Ljava/lang/String;
    site=get(Ljava/lang/String;Lorg/h2/engine/CastDataProvider;)Lorg/h2/value/Value;
[cratonvm-deopt] ORPHANED at the serviced call site get(…CastDataProvider;)…:
    stash=org/h2/util/StringUtils.cache:… bci=54
```

and the site that minted it is named directly:

```
[cratonvm-deopt] direct invokestatic call at
    org/h2/value/ValueVarchar.get:(Ljava/lang/String;Lorg/h2/engine/CastDataProvider;)Lorg/h2/value/Value;#38
    has NO callee-deopt service (info=false args_base=false)
```

`javap` confirms pc 38 in that method is exactly
`invokestatic org/h2/util/StringUtils.cache`, and bci 54 in `cache` is
`invokevirtual java/lang/String.equals` — the guard bail whose reconstructed
locals (`[Object, Object, Undefined, Int(253), Object]`) match `cache`'s frame,
not any inlined shape.

`org.h2.test.synth.TestCrashAPI`, the other class the page named: 0 orphans.

## The four changes

1. **`jit/src/lib.rs`** — the statically-bound `callee_compiler` bind now builds
   and registers a `JitInvokeInfo` for its pc before `continue`ing, the same way
   the `ArraycopyPrimitive` arm below it already does and for the same reason.
   This is the fix; everything else follows from it.
2. **`jit/src/x64/bytecode_walk.rs`** — a sibling tail call is refused when the
   site has a `JitInvokeInfo`. A tail call REPLACES the frame, so a trapping
   callee's sentinel and stash arrive at a site that invoked US, whose identity
   gate correctly refuses them; a real CALL keeps the frame alive long enough
   for the service check to run. After (1), "has an info" is exactly "the callee
   is a compiled artifact and can stash" — inline-machine-code intrinsics and
   the thin native helpers keep the tail form. Cost: the 90 tail sites on
   `TestScript` become ordinary calls.
3. **`vm/src/jit/helpers.rs`** — the five resolution declines in
   `try_resume_trapped_callee` were bare `?` / `return None` against a
   bound-but-unused `_resolve_trace`, so "the callee could not be resolved" was
   indistinguishable from "no stash". Each of them leaves the frame stashed,
   i.e. each MINTS an orphan; they are all traced now, plus one line at the
   point a sentinel leaves a serviced call site with a stash still present.
4. **`vm/src/runtime/interpreter.rs`** — the first-call tier-up sink ran
   `DeoptimizationController::deoptimize` on ITS OWN method unconditionally and
   then de-speculated the frame's real owner in the mismatch branch: two methods
   made not-compilable per foreign frame, one of them innocent
   (`StringFunction1.getValue`, a per-row expression evaluator). Now conditional
   on the frame being this method's.

## What it costs

Registering a `JitInvokeInfo` for these sites means the codegen now emits, per
direct call: the service-arg copy (`n` stores) and a `MOV imm64` + `CMP` +
not-taken `JNE`. Plus the 90 tail sites on `TestScript` become ordinary calls.

ABBA-interleaved on `org.h2.test.db.TestIndex`, per-PROCESS CPU, pre-fix binary
against post-fix binary:

| | rep 1 | rep 2 |
| --- | --- | --- |
| before | 174.5 s | 181.3 s |
| after | 178.1 s | 199.3 s |

The rep-1 pair — the two runs closest in time — is **+2.1%**. Read rep 2 as host
drift rather than signal: the *before* arm alone rose 174.5 → 181.3 over the
same window, and its wall time rose 150 s → 175 s as another agent's suite run
started on the same box. Every run PASSes on both arms.

Do not A/B this on `org.h2.test.synth.TestCrashAPI`: it is a random fuzzer, and
the same class took 64 s in one run and blew a 600 s cap on BOTH arms in
another.

## What is left, and why it is not a hole

131 direct call sites on `TestScript` still emit no service check. Every one is
an arm that binds either a `direct_native_helper` (a Rust function) or a
`try_resolve_intrinsic` / `try_resolve_string_intrinsic` entry (inline machine
code). Neither runs `x64_deopt_entry`, so neither can stash a frame — nothing to
service. The one intrinsic that CAN deopt, `ArraycopyPrimitive`, has registered
its own `JitInvokeInfo` since the JDT-parser fix.

The census is reproducible: `CRATONVM_DBG_DEOPT=1` and
`grep -c 'has NO callee-deopt service'` / `grep -c 'ORPHANED at the serviced'`.
The second number is the one that matters; it is 0.

## Pinned
`jit/src/lib.rs`'s
`statically_bound_direct_callee_call_still_registers_invoke_info` asserts both
that the fixture takes the DIRECT bind (`_direct_callee_entries == 1`, so a
future routing change cannot keep it green vacuously) and that a
`JitInvokeInfo` is registered. Verified RED (0 vs 1) with the registration
patched out.

## Related
- `fixed-suite-bugs/h2-suite-bugs/gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md`
  — the sweep the symptom came out of, and the tier-up sink fix that made the
  orphan survivable rather than fatal.
- `fixed-suite-bugs/jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md`
  — where the identity gate and `despeculate_stashed_frame_method` came from.
  Its "outer consumer that CAN attribute it" assumption is sound; what broke it
  was a call site that never asked.
