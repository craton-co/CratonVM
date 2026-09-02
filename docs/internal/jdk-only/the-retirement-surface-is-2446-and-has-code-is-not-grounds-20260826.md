# The retirement surface is 2,446 rows — and `has_code: true` is not grounds for retiring any of them

**Status: MEASURED 2026-08-26.** No retirement performed. The top candidate was
adjudicated **KEEP**, and the reason generalises.

## 1. Sizing the surface from the registry itself

`--dump-native-registry` carries `real_declaring_method` per row — whether the
REAL JDK method is loaded, declared, `native`, and has a `Code` attribute.
That makes the retirement question answerable by query rather than by reading
registrars. Over the 10,080 **owning** registrations in `--jdk-only`:

```text
(loaded, declared, acc_native, has_code)     rows
(False, False, False, False)                 6047     class never loaded in this run
(True,  True,  False, True )                 2446  <- the JDK HAS a concrete body
(True,  False, False, False)                  994
(True,  True,  False, False)                  357
(True,  True,  True,  False)                  236     the real method is itself native
```

**2,446** owning registrations shadow a real JDK method that is loaded,
declared, non-native and has bytecode — 2,057 `bridge`, 389 `intrinsic`. Where
they sit:

```text
592  native-collections/src/lib.rs        87 rows  jdk/internal/misc/Unsafe
325  native-builtins/src/lib.rs           82 rows  sun/misc/Unsafe
291  native-builtins/src/lang_math.rs     69 rows  java/lang/Math
177  native-builtins/src/lang_string.rs   69 rows  java/lang/StrictMath
127  native-builtins/src/lang_misc.rs     63 rows  java/lang/Class
```

**This is a floor, not a total.** `loaded` means loaded during the measuring run
(`RStrings`), so the 6,047 `loaded=False` rows are unclassified rather than
absent. Quote 2,446 as "at least".

## 2. The number is a QUESTION, not a work list

The obvious next step is to retire the biggest clean family. `java/lang/StrictMath`
looked ideal: 69 rows, all `intrinsic`, all from one file, **0 invocations** in
the sizing run, and a class whose entire specification is *"use the JDK's exact
fdlibm algorithm"* — a VM substituting its own arguably violates the contract
even when the answers agree.

Every one of those signals pointed the wrong way.

### 2a. First: prove the natives even run

`probes/StrictMathBits.java` prints `Double.doubleToRawLongBits` for ~25
functions over 28 inputs plus six two-argument families — 1,709 lines, diffable
on stdout. `--jdk-only` against HotSpot 25.0.3+9:

```text
DIFFERING LINES vs HotSpot: 0   (of 1709)
```

A zero-diff is worthless if the natives never ran, so that was checked rather
than assumed — a registry dump taken **during the probe**:

```text
StrictMath owning rows: 69 · TOTAL invocations during the probe: 1708
  IEEEremainder/atan2/copySign/hypot/pow/nextAfter  inv=168 each
  acos/asin/atan/cbrt …                             inv=28 each
```

So the natives served 1,708 calls and were bit-identical to HotSpot on all of
them. (The `invocations: 0` in the sizing run meant only that `RStrings` does no
floating-point maths — a run-scoped fact, not a property of the family.)

### 2b. Then: read why they exist

`lang_math.rs` had already done this investigation, with a larger oracle than
mine — `probes/MathCensus.java`, **5,400 inputs per function**, six generators.
The natives exist BECAUSE the host libm disagreed with fdlibm on every one of
these functions:

```text
cbrt 30.98%  cosh 28.55%  sinh 28.08%  pow 9.73%  exp 9.62%  log1p 7.58%
log 7.37%  expm1 7.12%  tan 3.95%  asin 2.55%  cos 2.43%  sin 2.37% …
```

and the file ships a **three-way** split, measured not reasoned: rows HotSpot
does not intrinsify take fdlibm and are shared by both classes; HotSpot's
intrinsic set (`_dsin`, `_dcos`, `_dtan`, `_dexp`, `_dlog`, `_dlog10`, `_dpow`,
`_dcbrt`, `_dtanh`) comes from Intel LIBM stubs matching NEITHER candidate, so
there `Math` keeps libm and only `StrictMath` takes fdlibm.

The cost of getting it wrong is on record and is not a last-ULP curiosity: one
ULP in `Math.hypot` moved a commons-math optimizer onto a trajectory that hit an
exact fixed point in nine evaluations, so `TooManyEvaluationsException` was never
thrown and four `GaussNewtonOptimizer` tests failed.

**So my 0-diff was evidence the family WORKS, not evidence it is unnecessary.**
Retiring it would delete a deliberately-built, oracle-verified implementation to
remove 69 rows from a count.

## 3. The rule this yields

> **`has_code: true` says the JDK COULD do it. It says nothing about whether the
> JDK does it the same way.**

For most classes those coincide. For a class whose contract names an algorithm —
`StrictMath` is the extreme, but any `@IntrinsicCandidate` method, and anything
where HotSpot substitutes a stub, is the same shape — they do not. The registry
query finds candidates; only an oracle diff plus the registrar's own history
adjudicates one.

Adjudicated here, with evidence: **`java/lang/StrictMath`, 69 rows — KEEP.**

Three things to carry into the next 2,377:

* **Check invocations DURING the probe, not from a prior run.** A family can
  read `inv=0` because the measuring vector never touched it.
* **Read the registrar's own history before proposing a retirement.** This one
  answered the question in a comment block, with a bigger oracle, months ago.
  `git log -S` on the file is cheaper than a build.
* **A zero-diff is a KEEP argument as often as a retire argument.** Which one it
  is depends entirely on why the native was written, and that is not in the
  registry dump.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only --dump-native-registry reg.json -cp probes/out StrictMathBits
```
