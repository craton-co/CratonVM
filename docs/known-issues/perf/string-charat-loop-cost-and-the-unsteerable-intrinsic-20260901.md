# `String.charAt` costs ~190-340 ns in a counted loop, and the 3 ns path is reachable but not steerable

**Status:** OPEN. Measured 2026-09-01 on `dev` @ `56d6c3722`, x86-64 Linux,
8 cores, `/proc/loadavg` 5-11 recorded per run — the effect is 40-370x, two
orders outside that band. Real-JDK mode, JDK 25, default flags.

## The measurement

`probes/CharAtCostCurve.java`: three loop bodies at growing inner rep counts,
over a 100,000-char `String` and the `char[]` from the same text.

| body | reps | CratonVM | HotSpot 25 |
|---|---:|---:|---:|
| `s.charAt(i)` | 2 | 190.0 ns/char | 41.6 |
| | 10 | 189.7 | 0.75 |
| | 50 | 227.5 | 0.50 |
| | 200 | 186.1 | 0.52 |
| | 200 | 194.8 | 0.56 |
| | 200 | 191.3 | 0.53 |
| | 1000 | 195.6 | 0.49 |
| `charAt`, `length()` hoisted | 200 | 155.8 | 0.47 |
| | 1000 | 177.1 | 0.46 |
| `a[i]` on a `char[]` | 200 | **5.38** | 0.12 |
| | 1000 | **5.22** | 0.12 |

Two things to read off it:

* **It is flat.** From 200,000 characters to 100,000,000 the cost never moves.
  There is no warm-up ramp — whatever body runs at 200,000 characters is still
  running at 100 million.
* **It is `charAt`, not the loop.** The identical loop over a `char[]` is
  **36x cheaper in the same VM**. Not bounds checks, not the induction
  variable, not the compare. The call.

Against HotSpot: `charAt` **370x**, `char[]` **44x**.

## A 3 ns path exists in the same binary

`probes/CharAtWarmShape.java` runs a byte-identical body and prints:

```
CratonVM   warm3x50    3.44, 2.96 ns/char
           warm3x200   3.31, 3.46 ns/char
HotSpot    warm3x50    0.25       warm3x200  1.24
```

**~90x faster than the table above, same binary, same class file, same method
body, same host.** So the inline `charAt` lowering does engage —
`jit/src/x64/bytecode_walk.rs` emits a coder-branch decode with no `CALL`, and
this is it running. The two probes differ only in how the method is reached.

## Five hypotheses, five refutations

Every plausible discriminator was tested and each is ruled out by its own
measurement. This section is the value of the page: it is the list nobody
should pay for twice.

| hypothesis | test | result |
|---|---|---|
| OSR entry is slower than method entry | one long call vs 2,000 short ones (`OsrVsEntry`) | 239 vs 205 ns/char — **no** |
| the callee was cold when the caller compiled | make `charAt` hot in a third method first, then run | 327 vs 291 ns/char — **no** |
| the caller's shape decides it | six callers: direct, static helper, `LongSupplier`, user-declared interface | 312-343 ns/char, all six — **no** |
| the first compile is final, and its context decides | two identical methods, one first entered from `main`, one from a helper, then interleaved reruns (`probes/CharAtFirstCompile.java`) | 278-324 ns/char for both, all eight rows — **no** |
| it is a scale threshold | 200k → 100M characters in one call | flat 186-196 — **no** |

So: a body 90x faster exists, is selected by something, and that something is
none of tiering door, warm order, caller kind, first-compile context or scale.

## Neither documented lever moves it

Two flags exist for exactly this trade. Both are inert on the slow shape:

```
default                                  329.5, 333.7 ns/char
CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1   326.3, 328.6
CRATONVM_JIT_IR_OVER_INTRINSIC=1         333.4, 366.3
```

`string_intrinsic_pin_enabled` is default-ON and its own comment carries the
measurement that motivated it:

> ```
> C2/IR body, intrinsic dropped   504 ns/call
> C1 body, intrinsic emitted      135 ns/call   (3.7x)
> ```
>
> …the delta is that large because the intrinsic does not replace one call
> with one instruction — it replaces the whole
> `charAt → isLatin1 → StringLatin1.charAt → String.checkIndex →
> Preconditions.checkIndex` chain, whose tail is a registered NATIVE.

Right diagnosis, right defect. What the numbers say is that **the pin is not
reaching this population**: switching it off costs nothing, so it was not on.
`CRATONVM_DBG_JITC` reports
`[ir] admission Tiny.scan(...): admitted to the optimizing pipeline` for a
method whose body is nothing but `charAt`.

## Two structural facts that fit

**1. The optimizing tier has no String intrinsics at all.**

```sh
grep -c 'StringCharAt\|StringLength\|string_layout' jit/src/ir_lower.rs jit/src/ir.rs
# 0  0
```

Every String access intrinsic lives in `jit/src/x64/bytecode_walk.rs`, the
single-pass backend. A method admitted to the optimizing pipeline therefore
*cannot* keep them — which is precisely what the pin exists to prevent.

**2. The pin fails open, and the printed verdict cannot report it.**

`has_string_intrinsic_site` (`jit/src/lib.rs`) returns `false` — "do not pin" —
when either `layout` or `cp_invoke_resolver` is `None`. Both are plausible at
an OSR or eager-first-call door, and neither absence is counted.

Worse for diagnosis: the pin is a term of the real eligibility conjunction
(`jit/src/lib.rs` ~20319) and is **absent from the verdict chain that prints
the reason** (~20150-20215). That chain tests `optimize`,
`moving_young_disables_optimizing_tier`, `ir_compatible`, the exception table,
`precise_exception_frames`, `ir_unresumable_protected_trap`,
`single_pass_only_lowering_for` and the value shape — then says "admitted". A
method the pin declined and a method the pin never saw print the same line.

That is very likely why the five hypotheses above could all be refuted without
converging on an answer: the one instrument that could name the decision does
not report it.

## What would close it, in order

1. **Add the string-intrinsic term to the printed verdict chain.** One line,
   and it is the prerequisite for everything else — until it lands, no run can
   say whether the pin fired. This is the fix that turns the question above
   into a single-run observation.
2. **Make `has_string_intrinsic_site` fail closed** on a missing layout or
   resolver: pin the method to the backend that has the intrinsic rather than
   silently promoting it to the one that does not, and count the two `None`
   cases so the population becomes visible.
3. **Give the IR tier a `StringCharAt` / `StringLength` emitter.** Then the pin
   and `CRATONVM_JIT_IR_OVER_INTRINSIC` can both be retired — the flag's own
   doc comment already stages that: "Exists so the trade can be re-measured in
   one binary if the IR tier ever grows an intrinsic emitter, at which point
   this whole refusal should go away."

Separately, note the `char[]` row: **5.2 ns/char** for a bounds-checked element
read in a compiled counted loop is 44x HotSpot, has nothing to do with String,
and is its own question about baseline codegen quality.

## Reproducers

`probes/CharAtCostCurve.java`, `probes/CharAtWarmShape.java`,
`probes/CharAtFirstCompile.java`.

```sh
java -cp probes CharAtCostCurve;    cratonvm -cp probes CharAtCostCurve
java -cp probes CharAtWarmShape;    cratonvm -cp probes CharAtWarmShape
java -cp probes CharAtFirstCompile; cratonvm -cp probes CharAtFirstCompile
CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 cratonvm -cp probes CharAtCostCurve
CRATONVM_DBG_JITC=1 cratonvm -cp probes CharAtCostCurve 2>&1 | grep admission
```
