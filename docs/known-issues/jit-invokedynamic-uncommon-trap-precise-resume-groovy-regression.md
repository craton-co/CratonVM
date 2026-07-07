# JIT invokedynamic uncommon-trap precise-resume regressed Groovy dynamic dispatch

| | |
|---|---|
| **Status** | ✅ **BOTH BUGS CLOSED 2026-07-07** (branch `fix/hib-temporal-placeholder-dup-20260707`) — the "real fix that closes both bugs at once" this doc called for landed. Root cause of THIS doc's Groovy regression finally pinned: `fb4a333d`'s stashed `ReconstructedFrame` carried **no method identity** (`method_key` was `String::new()` in the x64 producer), so when the reason-8 trap fired in a NESTED compiled callee (sentinel bubbling up through compiled callers' epilogue bails), the outermost interpreter sink resumed the OUTER method's frame with the INNER method's locals/stack/bci — arbitrary misexecution, exactly matching the "duplicate main method"/"Should never happen" Groovy-compiler-internal chaos (Groovy's deep dynamic-dispatch chains made nested traps the common case; `fb4a333d`'s own single-frame repros could never hit it). Fix: bake `"<class>.<method>:<descriptor>"` into every deopt snapshot, identity-check every resume consumer, resolve trapped callees precisely at their own dispatch-helper call site (`try_resume_trapped_callee`), gate direct-call/MIC/PIC publication for indy-trap artifacts, then RE-ENABLE reason-8 precise routing (under the standard `deopt_real_enabled()` kill-switch). The reopened silent-corruption risk is closed — confirmed by the Hibernate `type.temporal.*` placeholder-duplication bug it was causing (`docs/internal/hib-temporal-sql-parameter-placeholder-duplication-FIXED.md`, `IndyReplay` nested-shape 30000/30000 corrupt → 0). **Residuals keeping this doc open:** (a) Groovy `GroovyBeanDefinitionReaderTests` empirical re-verification on the Azure host (structurally the regression cannot recur — a mismatched frame takes the pre-`fb4a333d` re-run path that was Groovy-green — but the 18/30→? JIT-on number should be re-measured; no Spring checkout on the Windows box this fix was built on); (b) the pre-existing 12/30 JIT-on Groovy gap this doc already flagged as a separate uninvestigated residual; (c) `jit_uncommon_trap`'s `thread.frames.last()` wrong-method deopt attribution for OTHER reason codes' no-snapshot fallback (pre-existing, now documented). |
| **Area** | JIT x86-64 backend — `invokedynamic` (`0xba`) uncommon-trap deopt/resume (`jit/src/x64.rs`), consumed by `vm/src/runtime/interpreter.rs`'s deopt-resume sinks. |
| **Symptom** | With the JIT enabled (default), essentially every method of Spring's `org.springframework.context.groovy.GroovyBeanDefinitionReaderTests` fails with a Groovy-COMPILER-internal error — most commonly `startup failed: ... The method public static void main(String[] args) { ... } is a duplicate of the one declared for this script's body code` or `duplicates another method of the same signature`, occasionally a bare `Should never happen` (a Groovy AST/compiler internal assertion) — reproducing on a SINGLE test method run in total isolation. Disappears completely under `--nojit`. |
| **Severity** | High while open (silently broke all default-JIT Groovy dynamic-dispatch execution); the revert restores it, at the cost of reopening a separately-proven, real silent-corruption bug in an unrelated JIT deopt scenario (hot loop + late invokedynamic). |
| **Discovered** | 2026-07-07, verifying the `hib-proxyclassreuse-loader-blind-class-resolution.md` Residual B (`getEnclosingClass`) fix under default (JIT-on) settings — the Groovy suite that fix brought to 30/30 under `--nojit` still failed almost completely with JIT on, confirmed unrelated to loader-identity work. |
| **See also** | [hib-proxyclassreuse-loader-blind-class-resolution.md](hib-proxyclassreuse-loader-blind-class-resolution.md) — where this was first noticed as an aside; the loader-identity fixes there are unaffected by and unrelated to this doc. |

## Root cause

Commit `fb4a333d` ("Fix silent data corruption: precise resume for JIT
invokedynamic uncommon trap") made 5 changes to close a genuine, proven bug:
the OLD imprecise "safe reject" fallback for an `invokedynamic` uncommon trap
could only rewind execution to the method's OSR-entry bci, silently
discarding or duplicating every side effect JIT-compiled code had already
committed between OSR entry and the trap — confirmed nondeterministic
corruption on standalone repros (not committed to the repo; described only in
the commit message, referencing `HHH-15895`/`InPredicateTest`/`AccumRepro3`).

Bisected (2026-07-07) via per-item experimental revert flags, toggled one at
a time and tested against a single isolated Groovy test method
(`GroovyBeanDefinitionReaderTests.simpleBean`), confirming/refuting each of
`fb4a333d`'s 5 sub-changes independently:

1. `0xba` codegen arm unconditionally recording an OSR-exit snapshot
   (`emit_osr_exit_map_at`) — reverting this ALONE did **not** fix the
   Groovy regression.
2. `emit_deopt_stubs` routing reason 8 through the precise trampoline
   unconditionally — reverting this ALONE **did** fix the Groovy regression
   (verified both directions: flag off ⇒ regression persists; flag on ⇒
   `simpleBean` passes).
3. `can_osr_exit`'s elided-monitor exclusion — forcing `can_osr_exit` false
   entirely did **not** fix the Groovy regression (i.e. the bug does not
   depend on `can_osr_exit`'s VM-side gate at all — item 2's codegen-level
   routing is independent of it).
4. `try_osr()`'s dropped `osr_exit_transfer_enabled()` gate — not reached by
   this bug (confirmed via `CRATONVM_DBG_DEOPT`/custom traces: neither
   `try_osr`'s OSR-exit-transfer consumer nor its "TRANSFER"/"bail rejected"
   trace lines ever fire for this scenario at all — the actual corruption
   path is the `emit_deopt_stubs` reason-8 routing feeding a DIFFERENT,
   normal-call-deopt consumer, not the OSR-entry-reentry path `try_osr`
   guards).
5. `typed_local_frame_value`'s `LocalKind::Ref` trust relaxation — forcing it
   back to `Unsupported` (its pre-`fb4a333d` behavior) did **not** fix the
   Groovy regression.

So the regression is isolated precisely to item 2 (`emit_deopt_stubs`
unconditionally using `self.osr_exit_box_ptr_by_bci.get(&bci)` for reason 8),
independent of every other sub-change. **The exact reason the reconstructed
frame is unsound for THIS specific trap was not pinned down further** in the
time available — a repro built to match the commit's own description (a hot
loop, string-concat `invokedynamic` reached from inside it after real
committed work) did not reproduce corruption even on the PRE-`fb4a333d`
binary across 10 repeat runs, so the original repros could not be
reconstructed to compare directly against the reverted code. The most likely
remaining suspect (not verified): the reconstructed frame's OPERAND STACK at
an `invokedynamic` trap holds the live call-site arguments the interpreter
still needs to actually deliver (the call itself never runs in JIT-compiled
code — see the `0xba` codegen arm's own comment, "this instruction is never
actually JIT-executed") — a materially different shape than the loop-header
snapshots this same machinery (`osr_exit_box_ptr_by_bci` /
`build_and_record_deopt_point`) was originally designed for (see
`docs/feature-designs/deopt-osr.md`), which typically have an empty or
simple stack. If some stack slot's provenance/kind is subtly wrong for this
shape specifically, the resumed interpreter frame could hand a corrupted
receiver/argument to Groovy's `IndyInterface`/`CallSite` bootstrap machinery,
which is plausibly not idempotent against a corrupted re-invocation — a
real Groovy compiler-internal object (tracking, e.g., which methods a
generated class already declares) could get mutated twice from a single
logical call, producing the observed "duplicate main method" symptom without
requiring any bug in Groovy itself.

## Fix (2026-07-07)

`jit/src/x64.rs`, `emit_deopt_stubs`: reverted reason 8 (`UnreachedCode`) to
route through `None` (the imprecise "safe reject" — rewind to the method's
OSR entry bci) exactly as it did before `fb4a333d`, leaving every other part
of that commit (items 1, 3, 4, 5 above) intact. Reasons 2/6/7 (the BCE pilot
guard, string-intrinsic/call-site-type-check guards, and the loop-header
OSR-exit trigger) are completely unaffected — they keep `fb4a333d`'s
(and pre-existing) gated precise-resume behavior.

## Verification

- `GroovyBeanDefinitionReaderTests.simpleBean` in isolation, JIT default-on:
  FAIL (`fb4a333d` as committed) → PASS (this revert).
- `GroovyBeanDefinitionReaderTests`, 30 non-hanging methods together in one
  JVM (the 6 `component-scan`-DSL methods hang for a separate, pre-existing,
  unrelated reason — see `hib-proxyclassreuse-loader-blind-class-resolution.md`):
  - JIT default-on, `fb4a333d` as committed (no revert): **0/30**, every
    method fails with a Groovy-compiler-internal error.
  - JIT default-on, this revert applied: **18/30** — a large improvement,
    though not full parity with `--nojit` (see below).
  - `--nojit` (either JIT state, since this bug and its revert are both
    JIT-only): **30/30** — confirms this doc's fix (and the separate
    `getEnclosingClass` fix from `hib-proxyclassreuse-loader-blind-class-
    resolution.md`) fully resolve the Groovy suite once the JIT is out of
    the picture; JIT-on still has its OWN separate residual (12/30 failing,
    some with the SAME `beans$_run_closure1`/`Should never happen` symptom
    families) not investigated further here — likely a different JIT
    deopt/resume path (e.g. the JIT-compiled equivalent of the
    `execute_invokestatic` self-call fix from the loader-identity doc)
    hitting an analogous issue. Flagged as follow-up, not this doc's scope.
- `cargo test -p cratonvm-jit --lib`: 878 passed, 4 failed — the SAME 4
  pre-existing aarch64 branch-range failures `fb4a333d`'s own commit message
  already documented as pre-existing and unrelated. No new JIT unit-test
  regressions from this revert.

## Tradeoff — the original bug is reopened, unconfirmed by direct repro

This revert restores the PRE-`fb4a333d` "safe reject" (imprecise rewind-to-
OSR-entry) behavior for the invokedynamic uncommon trap specifically. That
is the EXACT mechanism `fb4a333d` proved causes nondeterministic silent data
corruption (lost/duplicated loop-committed side effects) when a hot loop's
OSR-compiled body reaches a live `invokedynamic` (e.g. ordinary string
concatenation) after already committing real work. This session could not
reconstruct `fb4a333d`'s own standalone repros (not present in the repository
— only described in its commit message) to directly re-confirm the
corruption returns; two independent attempts to build an equivalent repro
(a hot loop incrementing a counter and building a concat string, both
directly inside and after the loop) did not reproduce any corruption on
either the pre-`fb4a333d` or reverted-fix binary across repeated runs, so
the reopened risk is unconfirmed empirically in EITHER direction from this
session's own testing — it is asserted here on the strength of `fb4a333d`'s
own prior verification, not independently re-demonstrated.

**Recommended follow-up**: build a precise per-slot soundness check specific
to the invokedynamic trap's operand-stack shape (analogous to the existing
`uses_long_float_double` method-level gate for stack width-ambiguity) so
reason 8 can safely use the precise-resume path when the recorded snapshot
is provably sound and fall back to the pre-`fb4a333d` behavior only when it
is not — closing both bugs at once instead of trading one for the other.
Understanding the EXACT unsoundness in the reconstructed frame (rather than
this doc's structural bisection down to "which code path", not "which byte
is wrong") is the prerequisite for that.
