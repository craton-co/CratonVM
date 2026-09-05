# An implicit NPE raised behind an in-flight exception is delivered later, at a site that never faulted

## Status

**FIXED 2026-09-03. Regressed and re-fixed 2026-09-04. Retired 2026-09-04.**
`take_jit_pending_exception` drops the implicit-trap signals when it hands out
an exception. Regression vector:
`regression-suite/src/RJitLambdaNpeSupersede.java`, in `run.sh`'s
`CORE_CLASSES`.

Retired on `dev@9c66b0b7d`, against the check this page itself says is the only
one that counts -- `test_npe_from_body` run **ALONE**, because with its eleven
siblings the arm never engages and a green file proves nothing:

```
cargo test --release -p cratonvm-vm --test lambda_jit_tierup_tests \
    test_npe_from_body -- --exact
```

`ok. 1 passed; 11 filtered out`, 3 of 3, plus `regression-suite/run.sh` at
90/90 including `RJitLambdaNpeSupersede`, and the reduced probe at checksum
`1210000` against HotSpot's `1210000`.

**REGRESSED 2026-09-04 by a DIFFERENT defect, and FIXED the same day.** The
regression was real and this vector caught it, but the mechanism first written
here was wrong and is corrected below — the intrinsic is not a second producer
of implicit-trap signals, and its null path never fired at all.

### What actually happened

`try_lambda_site_direct_call`'s resumed-body arm parked a real exception in
`jit_pending_exception` and returned **`0`**, calling it "the null/zero
sentinel" and leaving it "for the compiled caller's own post-invoke check".

**There is no such sentinel.** `emit_post_invoke_exception_check` compares `RAX`
against `i64::MIN`, and consults `dispatch_threw` only on that comparison. `0`
is an ordinary null reference return, so the check kept it and compiled code
carried on with a null while the exception sat unclaimed.

It looked correct because of what usually FOLLOWS a SAM call. Unboxing the
result — `Integer.intValue()` — was a CALL, and that call crossed into Rust and
delivered the pending exception a moment later at a site that could route it.
`069e67b43` made the box/unbox intrinsic default-ON, the unbox became an inline
load, the crossing disappeared, and the exception escaped its own `catch`.

The fix is one line: park the exception and return `i64::MIN`, the sentinel the
check actually tests for. It is right for every return type — for a reference
`i64::MIN` cannot be a valid heap address, and for the `J`/`D`/`F` shapes where
it is a representable value the check disambiguates through `dispatch_threw`,
which finds exactly the signal parked on the line above.

### How it was found, and the two wrong turns

The arm table is the same as the original bug's — `CRATONVM_JIT_OSR=0`,
`CRATONVM_JIT_LAMBDA_SITE=0`, `CRATONVM_JIT_LAMBDA_TIERUP=0` and a high
threshold each make it disappear — which is why it looked like the same defect.
Two hypotheses were built on that and both were wrong:

1. **"The intrinsic is a second producer of implicit-trap signals."** It is not:
   its null-receiver path is a DEOPT, not a `jit_npe_with_action`. Refuted by
   reading the emitter.
2. **"The inlined `intValue` sees the null and traps."** An instrument on
   `jit_uncommon_trap` printed nothing — because with `deopt_real` on, reason 6
   goes to `x64_deopt_entry` instead. `CRATONVM_DEOPT_REAL=0` made the failure
   disappear, which named the frame-deopt path and led to the trace that
   settled it:

```text
[cratonvm-deopt] x64 frame-deopt entry reason=ReceiverTypeChanged at bci=118
                 stack=[Int(0), Object(0)]
[cratonvm-deopt] OSR exception with no precise frame in ...main — throw site is
                 outside every protected range; propagating
```

`Object(0)` on the stack at the unbox is the null the caller should never have
been handed.

3. **"The BOX_UNBOX intrinsic claimed a site the OSR admission had already
   promised."** Reached independently, from `test_npe_from_body` rather than
   from the suite vector, and it is a true statement that is not the cause.
   `compile_osr_artifact` admits a method with an exception table on the
   promise that every throwing site inside a protected range publishes a
   reason-9 precise frame, `first_unsupported_precise_frame_site` checks that
   over the BYTECODE where the site is an ordinary `invokevirtual`, and the
   intrinsic then substitutes a lowering whose edges are reason-6. Refusing the
   intrinsic for a `pc` inside the exception table DOES make the vector pass --
   and so does the one-line sentinel fix above, with the intrinsic left fully
   on. Measured both ways: with `2422b006d` in and that refusal reverted,
   `test_npe_from_body` alone is `ok` 3 of 3 and the suite is 90/90. So the
   refusal removes an INGREDIENT (the Rust crossing at the unbox) and the
   sentinel fix removes the DEFECT. It was dropped rather than landed beside
   it, because it costs the intrinsic every site inside a `try` and buys
   nothing once the caller returns the sentinel the check tests for.

   Two things it left behind. The first is a trap worth keeping: declining the
   intrinsic inside `bytecode_walk`'s BOX_UNBOX region rather than at the
   resolution leaves `callee_entry` holding the intrinsic sentinel and drops
   through to the plain direct-call path, which emits a `CALL` to that value --
   `SIGSEGV at pc=0xffffffffffffffc7`, `fault pc is in NO live registered code
   buffer`. A site the resolver has already claimed cannot be un-claimed
   downstream.

   The second was a claim, and it is **RETRACTED**. This page first said the
   admission carried "a real gap ... even though it is not this defect":
   `first_unsupported_precise_frame_site` clears opcode `0xb6` because
   `precise_frame_publishing_opcode` says its lowering publishes a reason-9
   frame, and the BOX_UNBOX region then substitutes a lowering whose edges are
   reason-6. The source reading is correct; the conclusion drawn from it was
   not. Worse, it was left as an OPEN gap inside a RETIRED page, which is the
   one place nothing tracks it.

   Measured 2026-09-05 on `dev@7acc0b27c`, with two probes built for exactly the
   shape it predicts:

   * `Integer.intValue()` / `Long.longValue()` as the ONLY throwing opcode in a
     protected range, in a method invoked once so OSR is the only compile door,
     with the handler reading locals set before the `try`;
   * the same with the handler reading locals set before the LOOP -- the stale
     pre-OSR locals `route_osr_exception_out_of_artifact` names as the hazard --
     plus a witness the loop advances, so a stale read and a correct read differ.

   Both engage, which is the half a passing probe has to prove first:
   `OSR-compile ...arm()V entry_pc=24` on a method with a non-empty exception
   table, and `[box-unbox-intrinsic] java/lang/Integer.intValue()I` for a site
   that exists nowhere but inside that `try`. Both match HotSpot exactly, 3 runs
   each, handler entered with correct locals: `caught=400 bad=0 drift=0
   escaped=0`.

   **A reason-6 deopt is not a weaker publication than a reason-9 frame; it is a
   stronger action.** It abandons the compiled frame and hands a reconstructed
   one back to the interpreter, which raises the NPE and searches the exception
   table itself. The reason-9 frame exists for an exception that arrives INSIDE
   the compiled body and has to be routed WITHOUT leaving it. A site that deopts
   instead of publishing reaches the promise's purpose by another road, so the
   admission's soundness does not depend on the substituted lowering publishing
   anything.

**Three arms each removed the symptom, and only one named the defect.**
`CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1`, `CRATONVM_JIT_OSR_EXC_TABLE=0` and
`CRATONVM_DEOPT_REAL=0` are all clean, and each supported a different story. A
kill switch identifies an ingredient; only the trace above identified the cause.

`probes/BoxUnboxNpeProbe.java` is the reduced repro: it needs only
`lengthOf.apply(s)` (the vector's `stepFn` hop is not required), takes `-Dprobe.n`,
and prints the iteration index.

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
