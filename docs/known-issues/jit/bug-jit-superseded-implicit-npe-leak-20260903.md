# An implicit NPE raised behind an in-flight exception is delivered later, at a site that never faulted

## Status

**FIXED 2026-09-03.** `take_jit_pending_exception` now drops the implicit-trap
signals when it hands out an exception. Regression vector:
`regression-suite/src/RJitLambdaNpeSupersede.java`.

**REGRESSED 2026-09-04, and the cause is named.** `069e67b43` turned the
box/unbox intrinsic default-ON, and this vector is a boxing lambda
(`Function<String, Integer>`, so every `apply` is an `Integer.intValue()`).
With the intrinsic on, the vector's NPE escapes to `main` uncaught — the exact
symptom below. One run settles it, no build required:

```text
$ cratonvm -cp build RJitLambdaNpeSupersede
Exception in thread "main" java/lang/NullPointerException: ... "<local0>" is null

$ CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1 cratonvm -cp build RJitLambdaNpeSupersede
PASS RJitLambdaNpeSupersede (3 checks)
```

So the intrinsic's lowering does not honour the `take_jit_pending_exception`
discipline the fix below established: it is a second producer of the implicit
trap signals, and it was not taught to drop them.

Bisected by build, five arms: PASS at `ec96716a8` (the full suite was 90/90),
FAIL at `ded395383`, `b111a3514`, `842e6d0f9` and `04a5d4d02`. `ded395383` is
dev's own line with no feature branch merged into it, which is what rules out
everything landed beside it. The window is the fourteen commits
`ec96716a8..ded395383`, and `069e67b43` is the one the kill switch names.

## The symptom

`org.h2`-scale workloads were not needed. The reduced shape is 25 lines:

```java
Function<String, Integer> lengthOf = s -> s.length();
for (int i = 0; i < 200_000; i++) {
    String s = (i % 500 == 499) ? null : "abc";
    try { sum += lengthOf.apply(s) + stepFn(lengthOf, s); }
    catch (NullPointerException e) { caught++; }
}
```

HotSpot: `caught=400`, checksum `1210000`. CratonVM: `caught=401`, checksum
**`1210025`** — `31 - 6`, i.e. exactly one iteration whose string is `"abc"`
threw a `NullPointerException` instead of returning 3. Deterministic: the same
value on every run, at `i` ≈ 2500.

It also fails `vm/tests/lambda_jit_tierup_tests.rs::test_npe_from_body` — but
**only when that test is run alone**. Run with its eleven siblings it passes,
because the sibling tests keep the compiler busy and the arm never engages. A
green suite was therefore not evidence: the test agreed with HotSpot about a
path it never took.

## What it takes to fire

Each arm is 3 runs of the reduced probe; every one of these is a clean 0.

| arm | spurious NPEs |
|---|---|
| default | **1** |
| `--nojit` | 0 |
| `CRATONVM_JIT_THRESHOLD=1000000` | 0 |
| `CRATONVM_BG_COMPILE=0` | 0 |
| `CRATONVM_JIT_OSR=0` | 0 |
| `CRATONVM_JIT_LAMBDA_SITE=0` | 0 |
| `CRATONVM_JIT_LAMBDA_TIERUP=0` | 0 |
| `CRATONVM_OSR_EXIT_TRANSFER=0` / `=1` | 1 (no effect) |
| the same workload with no nulls at all | 0 |

So it needs an **OSR-compiled caller**, the **JIT-side lambda direct arm**, and
a **genuine implicit NPE** to have happened first. Two of those switches are
what made this findable; a third, `CRATONVM_NO_OSR=1`, is **not a flag that
exists** and its clean result was vacuous. The real spelling is
`CRATONVM_JIT_OSR=0`.

## The mechanism

Traced with a temporary instrument on every set / drain / restash of the
pending-NPE signal, with the Java iteration index interleaved:

```
[java] i=2499 s=null
[npedbg] MINSENT npe=false exc=false deopt=false stashed=true -> deopt(drop)
[npedbg] LDC exit resumed-threw npe_now=false
[npedbg] SET action_at code=0 trap_key=0 depth=1 top=["NpeDrain.main"]
[npedbg] SETDEOPT
[npedbg] TAKEEXC npe_now=true
[java] i=2500 s=abc
[npedbg] DRAIN take_all (npe set) depth=1 top=["NpeDrain.main"]
[npedbg] ROUTE bridge-alt sig.npe -> throw NPE
```

Read it in order:

1. At `i=2499` the compiled lambda body deopts (**not** on the NPE — `npe=false`
   in that drain). `try_lambda_site_direct_call` takes its deopt arm,
   `disable_direct()`s the site and finishes the call through
   `resume_deopted_body`, which re-enters the interpreter.
2. The interpreter raises the real `NullPointerException` and the arm returns
   **`Some(0)`** with it parked in `jit_pending_exception` — exactly what its
   contract says to do, and `npe_now=false` at that moment.
3. Compiled code then evaluates the **second operand of the same expression**,
   `stepFn(lengthOf, s)`, before its post-invoke guard fires. It dereferences
   the same null and raises a **second** implicit trap
   (`SET action_at` + `SETDEOPT`, one `jit_npe_with_action` call), with the
   interpreted stack still `depth=1 ["NpeDrain.main"]` — i.e. inside compiled
   code, not in any pushed callee frame.
4. The post-invoke guard now delivers the FIRST exception. `TAKEEXC npe_now=true`
   is the defect in one line: an exception is handed out while an implicit-trap
   flag is still standing.
5. Nothing owns that flag. Two iterations later an unrelated call through
   `execute_jit_call_decoded` drains it and builds a fresh NPE.

It fires exactly **once per run** because step 1 retires the direct arm for that
site, so no later call takes this route.

## The fix

An implicit signal is a *request* for a throwable, not a throwable. Once another
exception is in flight the request can never be granted: the frame whose trap
raised it is unwinding, and no door downstream owns the flag.
`take_jit_pending_exception` therefore drops `npe` (with its JEP-358 action and
compiled-frame snapshot), `aioobe` and `arithmetic` whenever it hands out an
exception.

This is the rule `take_all_jit_signals` already enforces by draining everything
at once; stating it at the other consumption point is what stops the two
disagreeing about whether a signal outlives the exception that overtook it. The
**deopt** flag is deliberately not dropped: it describes the compiled frame's
fate, which an exception does not settle.

All four production callers of `take_jit_pending_exception` consume the
exception to deliver or discard it; none re-stashes it, so none of them wants a
trap flag to survive.

## Two candidate fixes that were wrong, and how they said so

Both were built, A/B'd inside one binary behind a temporary switch, and
discarded — recorded here so the next reader does not re-derive them.

1. **Drain at `resume_deopted_body`'s completion exits.** No effect, and the
   instrument said why: the drain hook never fired, so the flag was not yet set
   when the resume returned.
2. **Drain at every exit of the lambda direct arm's deopt arm.** No effect
   either — `LDC exit resumed-threw npe_now=false` shows the flag is still clear
   when the arm returns. The trap happens *after* it, in compiled code.

The pattern in both: the flag is set later than the place that looked
responsible. Only logging the flag's state at each candidate boundary
distinguished them.

## Family

This is the fourth of its kind on the same flag — `Round-8 CRIT fix (NPE leak)`,
`Round-9 CRIT`, `Round-11 fix (AIOOBE leak)` are all commented in
`jit_bridge.rs` — and the first where the leak is not a missing drain on a
return path but a signal raised *behind* an exception that was already going to
win.
