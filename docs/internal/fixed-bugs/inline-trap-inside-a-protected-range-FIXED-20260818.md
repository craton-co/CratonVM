# The inline trap inside a `try` range — fixed, and the open page had the wrong producer

**Status: FIXED 2026-08-18**, on `fix/jit-inline-trap-deopt-point-20260817`,
branched from `dev` at `031fe394b`. Retires
`known-issues/jit/inline-trap-inside-a-protected-range-has-no-deopt-point-20260817.md`.

`SparseRealVectorTest` no longer crashes: 318 test-method invocations (3 passes
over 106 methods), zero `InternalError`, and the failure count now matches
HotSpot's exactly.

## Read this first: the open page named the wrong compiler

The open page's central claim was that `advance()` is compiled by the **baseline
x64 bytecode walker**, and that `emit_bounds_check`'s stub in
`jit/src/x64/deopt_stubs.rs` "does not register a deopt point, so ...
**there are no deopt points at all**, not merely a missing one at bci 51".

Every part of that is wrong, and the trace says so in one line:

```
[ir] admission …OpenIntToDoubleHashMap$Iterator.advance()V: admitted to the optimizing pipeline
```

It is the **optimizing (IR) tier**. The baseline walker is not involved, and its
bounds-check stub is not the producer.

The page reached that conclusion from the error message, which cannot support
it. Two traps in the message itself:

* **`reason` is derived, not reported.** The sink looks it up:
  `compiled.deopt_points.iter().find(|dp| dp.bci == rframe.bci).map(|dp| dp.reason)`,
  `unwrap_or(UnreachedCode)`. So `reason TransferToInterpreter` is *proof that a
  deopt point at that bci was found* — the opposite of "no deopt points at all".
  An empty set would have printed `UnreachedCode`.
* **`why` is a disjunction.** `can_deopt_resume=false (no deopt points, or an
  elided monitor)` names two causes and commits to neither. The page quoted it
  as if it asserted the first.

This is exactly the failure the open page warned about, citing the H2
precedent's "Lessons worth keeping" — *misattributed deopt frames, `reason`
defaults, "denying the named method is not a diagnosis"* — and then repeated.
The lesson survives another round: **when a message is the only evidence, read
the code that formats it before believing what it appears to say.**

## What is actually wrong

The IR tier lowers array element access, `arraylength`, field access and
integer division to **deopt guards** — `emit_array_null_bounds_guards`,
`emit_deopt_if_zero` in `jit/src/ir_lower.rs` — on an explicit promise in its
own comment:

> On a null array or an out-of-bounds index, deopt at `bci`: the interpreter
> re-executes the array opcode and throws the exact NPE / AIOOBE with full
> semantics (including any in-method handler).

Re-executing one opcode requires a **precise resume**. But on the IR path
`cm.can_deopt_resume` is left at its `false` default and set true in exactly one
place (`ir_lower.rs`), inside a condition requiring `sr_map.is_some()` — i.e.
`CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`. **Production IR artifacts
therefore always have `can_deopt_resume == false`**, and the promise in that
comment cannot be kept by any of them.

So when the guard fires, the interpreter has only whole-method replay left,
correctly refuses it (`advance()` stores `current` and `next` on every iteration
*before* the trapping `baload`), and raises the hard `InternalError`.

The single-pass backend has no such problem and never did: its bounds check
calls `jit_throw_aioobe`, returns the `i64::MIN` sentinel through the epilogue,
and the interpreter routes the exception through the method's own exception
table — **no resume required**. That asymmetry is the whole bug, and it is why
the first reduction of this shape did not reproduce: a small probe never reaches
the optimizing tier, so it got the backend that already works.

## The fix

`ir_unresumable_protected_trap` (`jit/src/lib.rs`), added to the optimizing
tier's admission conjunction. A method is declined when a **deopt-guarded
opcode** sits inside a protected range that **also commits a side effect**. It
then falls through to the single-pass backend — so the method stays *compiled*,
at single-pass code quality, rather than dropping to the interpreter.

Both narrowing terms are load-bearing, because the H2 precedent warns in as many
words: *"Do not apply the publish-side rule blind... the naive form would refuse
every trap-carrying artifact, including the many whose re-run-from-entry
fallback works fine."*

* **only the deopt-guarded opcodes** — array loads/stores, `arraylength`,
  `getfield`/`putfield`, `idiv`/`irem`/`ldiv`/`lrem`. Invokes, `new`, `ldc`,
  `checkcast` and the monitor ops all leave through the sentinel + exception
  routing, which needs no resume, so they are not listed. Using the existing
  `first_unsupported_precise_frame_site` set wholesale would have refused nearly
  every `try`/`catch` method in the tree.
* **only when the range commits a side effect** — a read-only
  `try { return a[i]; } catch (...)` replays harmlessly, so declining it would
  cost a compile and buy nothing.

The side-effect scan covers the **whole range**, not "before the trap in pc
order". The witness is a loop: its stores sit at a *lower* pc than the trap and
execute on the iteration after the one that throws. Pc order is not execution
order, and the cheap conservative answer is the correct one here.

## Measured reach

The risk of a compile-time refusal is that it quietly de-optimizes half the
tree, so the reach is measured rather than asserted:

| workload | IR candidates | admitted | **declined by this gate** |
|---|---|---|---|
| `SparseRealVectorTest` | 144 | 76 | **1** |
| `SparseRealMatrixTest` | 33 | 14 | **0** |

One method, and it is the witness. It is still compiled:
`full-compile …Iterator.advance()V entry=… len=3408`, by the single-pass
backend.

## Reproducing it, which was most of the work

The open page's recipe does not run as written on this host, for a reason worth
recording: **the commons-math tests are JUnit 4** (`org.junit.Test`), and
`apps/commons-math/cp.txt` has no junit4 jar. `CratonRunner` therefore discovers
0 tests and reports a clean pass, and a reflective driver sees *no annotations at
all* — `getAnnotations()` silently drops an annotation whose class will not
resolve. Adding `junit-4.13.2` + `hamcrest-core-1.3` fixes both.

That silent-zero is the trap to avoid next time: a harness that finds nothing
looks identical to a harness that finds nothing wrong.

```bash
JH=<jdk-25>
CP="<dir with ReflectTestDriver>;<all module target/{classes,test-classes}>;$(cat apps/commons-math/cp.txt);\
<...>/junit-4.13.2.jar;<...>/hamcrest-core-1.3.jar"
<cratonvm> --java-home "$JH" --Xmx 1g -cp "$CP" \
  ReflectTestDriver org.apache.commons.math4.legacy.linear.SparseRealVectorTest 3
# before: @@INTERNALERROR testEquals
# after:  @@DONE ran=318 failed=99   (99 == 3 x HotSpot's 33)
```

`probes/InlineTrapInTryProbe.java` is the dependency-free reduction of
`advance()` (verified by `javap`: `baload` inside the range, two `putfield`s
ahead of it) and `probes/IrTrapVariantsProbe.java` separates the three candidate
preconditions. **Neither reproduces the crash**, with or without
`CRATONVM_JIT_FORCE_C2=1` — they are kept because they pin the shape and the
single-pass behaviour, but the honest note is that the only reproduction is the
real test class. A reduction that does not reproduce is not a reproduction, and
saying so is cheaper than the next reader re-deriving it.

## What is deliberately NOT fixed

The IR tier still emits a promise it cannot keep — those guard comments still
say the interpreter will re-execute the opcode, and on a production artifact it
cannot. This change stops methods from *reaching* that path; it does not make
the path work. The real repair is one of:

1. give the IR tier's inline traps the single-pass treatment (throw + route
   through the exception table, no resume needed); or
2. make `can_deopt_resume` true for production IR artifacts, which means
   emitting monitor state in `FrameState` (every one currently hard-codes
   `monitors: Vec::new()`) and typing every slot the mapper can reject.

Either is a real piece of work, and the guard comments in `ir_lower.rs` should
be corrected to say "deopt, which on a production artifact means a whole-method
replay" until one of them lands.

## Related

* `unresumable-unconditional-trap-mvmap-FIXED-20260802.md` — the same error
  shape, a third distinct producer (`Op::Div` scheduling). Its warning against
  a blind publish-side rule is what shaped this fix's two narrowing terms.
* `../fixed-suite-bugs/jit/osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md`
  — the invoke-site reason-9 machinery, fixed separately and merged into `dev`
  while this change was in flight. Still not the mechanism here: the open page
  was right that this shape falls outside it, and wrong about what catches it
  instead.
* retired/commons-math-suite-run-RETIRED-20260818.md — the suite run this came from, now closed.
