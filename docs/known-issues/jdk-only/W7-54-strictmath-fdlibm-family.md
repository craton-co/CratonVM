# W7-54 — `StrictMath` was the platform's libm, on every function, not just `log`

> # RETIRED 2026-08-12 (lane B8) — RUN, and bit-exact on 29 of 29 values.
>
> Every prior pass on this record, including the RE-VERIFIED one below, says in
> its own words that nothing was built or run on CratonVM. It has now been run.
> Binary `/c/craton/jdkonly-wave2-target/release/cratonvm.exe --jdk-only`,
> oracle Temurin `jdk-25.0.3.9-hotspot`, one class file on both arms, every
> value printed as `Double.doubleToRawLongBits` so a 1-ulp drift cannot hide in
> decimal:
>
> ```
> $ diff hsm.txt cvm.txt && echo IDENTICAL
> IDENTICAL
> ```
>
> 29 values: the eighteen fdlibm-backed transcendentals
> (`sin cos tan asin acos atan atan2 log log10 exp pow cbrt hypot log1p expm1
> sinh cosh tanh`), `sqrt`, five `IEEEremainder` inputs, and the six
> `Math`/`StrictMath` `min`/`max` NaN and `-0.0` cases. Sample:
>
> ```
> sin=4605754516372524270 (0.8414709848078965)
> rem_MAX_MINNORM=0 (0.0)
> Math.min_1_NaN=9221120237041090560 (NaN)
> Math.min_-0_0=-9223372036854775808 (-0.0)
> ```
>
> **§4's correction is confirmed by measurement, and it was the right call to
> make it.** The probe uses the discriminating half-integers §4 identifies, not
> the one it caught itself using:
>
> | input | measured | what it proves |
> |---|---|---|
> | `IEEEremainder(1.5, 1.0)` | `-0.5` | agrees under BOTH rules — **cannot discriminate**, exactly as §4 says |
> | `IEEEremainder(2.5, 1.0)` | `0.5` | ties-to-even (`2.5→2`); the ties-away body would give `-0.5` |
> | `IEEEremainder(0.5, 1.0)` | `0.5` | ties-to-even (`0.5→0`) |
> | `IEEEremainder(4.5, 1.0)` | `0.5` | ties-to-even (`4.5→4`) |
> | `IEEEremainder(MAX_VALUE, MIN_NORMAL)` | `0.0` | **not** `-Infinity` — the recorded broken value is gone |
>
> Three independent half-integers land on the ties-to-even answer, so the
> fdlibm body is live on the running path and the libm body is not reachable
> from it. §4's warning that a `!Double.isNaN(r)` guard would have **admitted**
> the broken `-Infinity` is also confirmed as sound: the repaired value is
> `0.0`, and only an exact-bits assertion separates the two.
>
> **Scheduling: the vector is real and it runs.** `--jdk-only` is the mode this
> record is filed under and the values above came from it. §12's alternative
> repair — de-register the eighteen strict triples in real-JDK mode and let the
> image's own `java.lang.FdLibm` bytecode run — is **not** taken and is not
> needed for correctness; it remains a throughput/architecture question for
> docs/architecture/natives-over-real-jdk-classes.md §1, not a defect. Retiring
> this record does not close that.

> ## RE-VERIFIED 2026-08-12 (lane A14). The port is in the tree and reaches the running path. It now has a SCHEDULED vector, and §4's two worked examples are WRONG.
>
> Nothing was built or run on CratonVM in this pass either. What was done is
> read against today's source, and measured against Microsoft OpenJDK 25.0.3.9
> (`javap -version` → 25.0.3), which is this host's `java.home`.
>
> **1. The split is in the tree and it is complete.**
> `lang_math.rs::register_math_natives` takes `class` and branches on
> `let strict = class == "java/lang/StrictMath";`. The strict arm registers
> eighteen fdlibm-backed bodies — `pow sin cos tan asin acos atan atan2 log
> log10 exp cbrt hypot log1p expm1 sinh cosh tanh` — through the
> `strict_math_unary!` / `strict_math_binary!` macros, whose expansion is
> `cratonvm_types::fdlibm::$fn`. The `else` arm keeps the `native_math_*`
> libm bodies. `sqrt` and `IEEEremainder` are registered once, outside the
> branch, and shared; `types/src/fdlibm.rs` carries all nineteen `pub fn`s.
> Registered from `register_essential_natives_with_shims` (the real-JDK arm,
> so both `--real-jdk` and `--jdk-only`) and again from
> `register_synthetic_overrides`.
>
> **2. New, and it is the question this record never asked: does the JIT
> bypass the fix?** It does not. `jit/src/lib.rs::try_resolve_intrinsic` opens
> with `if class == "java/lang/Math" || class == "java/lang/StrictMath"`, which
> is exactly the shape that would reinstate libm above the natives — a
> "JIT thin direct helper reimplements the native" defect. Its match arms were
> read in full: `sqrt floor ceil rint abs fma min max multiplyHigh
> unsignedMultiplyHigh`. **Not one transcendental is in the list**, so no
> compiled call site can route around the fdlibm bodies. `jit-cuda`'s
> `analyzer.rs` names `StrictMath` only for `sqrt`/`abs`/`fma`. Every function
> in that combined list is either correctly-rounded by IEEE 754 or exact, so
> the intrinsics are admissible for `StrictMath` as well as `Math`.
>
> **3. New: in JDK 25 `StrictMath`'s transcendentals are NOT `native`.**
> `javap -p java.lang.StrictMath` shows `public static double sin(double);`
> with no `native` modifier — JDK 21 moved fdlibm into Java, so the real image
> already carries bit-exact bytecode over `java.lang.FdLibm`. CratonVM's
> registrations are therefore **shadows over correct real JDK bytecode**, and
> they win: the ambient kind at that block is `NativeKind::Intrinsic`, and
> `synthetic_stub_kind_should_yield_to_real_bytecode` returns early unless the
> kind is `SyntheticStub`. That makes the Rust port necessary *given the
> registrations stay*, and it also means there is a second, cheaper repair this
> record never considered — de-register the eighteen strict triples in
> real-JDK mode and let the image's own `FdLibm` run, which is free
> correctness and the direction
> docs/architecture/natives-over-real-jdk-classes.md §1 exists to push. Not
> taken here: it is a registration change with a throughput cost on a hot
> surface and it needs the build this lane does not have. Filed in §12.
>
> **4. §4's two worked examples do not hold. Measured, not re-read.** The
> defect class is real and the fix is right; the two numbers cited to
> illustrate it are both wrong, and one of them cannot fail:
>
> | §4 claims | measured on JDK 25 | verdict |
> |---|---|---|
> | `IEEEremainder(1.5, 1.0)` returned `0.5`, answer is `-0.5` | fdlibm gives `-0.5` **and so does the ties-away body** | **cannot discriminate** — `1.5` rounds to `2` under ties-to-even and ties-away alike |
> | `IEEEremainder(MAX_VALUE, MIN_NORMAL)`: "the quotient is `+inf` and `a - inf*b` is NaN" | the broken body returns **`-Infinity`**, not NaN | wrong value; defect real |
>
> The half-integer quotients that actually separate the two rules are the ones
> whose ties-to-even target is the **lower** even integer — `2.5 → 2`,
> `0.5 → 0`, `4.5 → 4`. §4 picked the one half-integer that agrees. This was
> established by re-implementing the recorded pre-fix body
> (`a - (a/b).round() * b`) in Java and replaying it beside `StrictMath`; the
> transcript is in §11's mutation table. It matters beyond the prose: a lane
> writing a vector from §4 as given would have shipped a test that passes on
> the defect.
>
> The `-Infinity` correction also has a teeth consequence. A guard written as
> `!Double.isNaN(r)` — the obvious reading of §4 — **admits the broken
> answer**, because an infinity is not a NaN. The assertion has to be
> `Double.isFinite(r)`.
>
> **5. The coverage gap this record left open is now closed.** §10 said
> "nothing was re-measured end-to-end on a CratonVM binary" and §11 listed the
> steps. The reason it stayed open is structural rather than clerical: this
> record's evidence was 24 `cargo test` unit tests over the ported *routines*
> plus `probes/StrictMathOracleDumpProbe` and `probes/StrictMathVectorProbe` —
> and **`probes/` is never run by `regression-suite/run.sh` at any `SUITE=`
> value**, while the unit tests never go through the registry. So no scheduled
> artefact ever exercised the *registered natives*. A `grep -rl StrictMath
> regression-suite/` returned **nothing**: there was zero StrictMath coverage
> in the suite.
>
> `regression-suite/src/RJdkStrictMath.java` is new and is that artefact —
> 662 checks over 557 golden vectors on all twenty functions, asserted by
> `Double.doubleToRawLongBits`, measured on Microsoft OpenJDK 25.0.3.9. It is
> a **shared** vector (§3 above is why: one expectation for HotSpot,
> `--real-jdk` and `--jdk-only`). It is registered by nomination, not by this
> lane — see §11.
>
> **6. It was proven able to fail.** Green on the HotSpot oracle
> (`PASS RJdkStrictMath (662 checks)`, no `javac -Xlint:all` warnings), and
> RED under three separate mutations — see §11. A golden table never seen to
> fail is the trap this directory keeps paying for, and `atan`'s vacuous
> green in §6 is the same shape one level down.
>
> **Still not established, and this lane could not:** that the tree compiles,
> that the registered natives return these bits on either CratonVM arm, or
> that `Random.nextGaussian` now matches. Those need the build. What changed
> is that confirming them is now one scheduled suite run rather than a probe
> somebody has to remember to invoke.

**Status: SOURCE LANDED, NOT RE-MEASURED.** No CratonVM binary was built from this branch —
this lane does not run `cargo build`. Every number below is measured, but on the two arms
that can be measured without one: HotSpot (jdk-25.0.3.9-hotspot, Windows x86-64) and the
platform libm that Rust's `f64` methods reach, which is the same MSVC CRT the CratonVM
natives were calling. The claim that CratonVM now returns the fdlibm bits is a claim about
source plus a bit-exact test of the ported routines; the end-to-end re-measurement is the
last section.

This is a **HotSpot-parity fix**, so it applies to both modes and is in scope for the
frozen Compatible mode (`--real-jdk`). The carve-out fits unusually cleanly here:
`StrictMath` returning *platform-dependent* results violates its own specification, so
there was no behaviour anyone could have been depending on. Whatever a caller saw was an
accident of the host's C runtime.

---

## 1. What `StrictMath` is for

`java.lang.Math` and `java.lang.StrictMath` expose the same signatures and have **different
contracts**:

| | contract | may use |
|---|---|---|
| `Math.f` | result within **1 ULP** of exact, and semi-monotonic | host libm, CPU intrinsics, anything |
| `StrictMath.f` | **the fdlibm result, bit for bit, on every platform and every VM** | only the fdlibm algorithm |

That difference is the entire reason `StrictMath` exists as a separate class. It is what
lets `strictfp`-era numeric code, and anything that must reproduce a stream of values
exactly, get the same answer on every machine.

CratonVM registered **the same backing function pointer for both classes**. One line in
`native-builtins/src/lib.rs` calls `register_math_natives(registry, "java/lang/Math")` and
the next calls it with `"java/lang/StrictMath"`, and inside, every transcendental resolved
to a body like `Ok(Some(Value::Double(v.ln())))`. So `StrictMath` satisfied `Math`'s
contract and was presented as satisfying the stricter one.

Two standing `VULN / LIMITATION` comments at those registration sites said exactly this and
had said it for a long time. They were right. They are now retired.

---

## 2. The census, and why the obvious way to take it gives zeros

`log` was already fixed under W7-44-numberformat-enum-and-double-tostring.md, found because
it surfaced: `new Random(42).nextGaussian()` printed `1.141905315473055` against HotSpot's
`1.1419053154730547`, one ULP apart, because the polar method multiplies by
`StrictMath.sqrt(-2 * StrictMath.log(s) / s)` and every operation in that expression except
`log` is exactly-rounded IEEE. That fix left the other seventeen functions open, with no
measurement of how bad they were.

### The measurement that does not work

The natural probe compares `StrictMath.f(x)` against `Math.f(x)` inside one HotSpot, which
is what the `log` finding used (7.3% of uniform draws in `(0,1)`). Run across the whole
surface it gives this:

```text
asin  0.000%   acos  0.000%   atan  0.000%   atan2 0.000%   hypot 0.000%
log1p 0.000%   expm1 0.000%   sinh  0.000%   cosh  0.000%   IEEEremainder 0.000%
```

Ten clean rows — and every one of them is an artefact. In JDK 25, `java.lang.Math`'s
transcendentals are **source-level delegates**:

```java
public static double asin(double a) {
    return StrictMath.asin(a); // default impl. delegates to StrictMath
}
```

Twenty-three of them read like that. The two only diverge where C2 substitutes an x86
intrinsic. So a `0.000%` row means *"HotSpot has no intrinsic for this function"* and says
nothing whatsoever about the C runtime a native VM links. Taking that table at face value
would have closed ten functions that were, in fact, wrong — the shape catalogued in
`INDEX_vacuous_greens` as a probe that cannot fail.

`probes/StrictMathCensusProbe.java` still exists and still prints these numbers, because
the intrinsic map is worth knowing, but it prints the caveat underneath them.

### The measurement that works

The comparison that matters is fdlibm against **the libm CratonVM actually calls**, and no
Java program can make it. So `probes/StrictMathOracleDumpProbe.java` dumps the oracle out
of the JVM — 100k sampled inputs per function across each function's meaningful domain,
argument and result as raw bits — and a Rust program replays it against `f64::sin` and
friends, which reach the same MSVC CRT the natives did.

Measured on Temurin 25.0.3+9 / Windows x86-64, **bit-disagreement with fdlibm**:

| function | differ | rate | worst |
|---|---|---|---|
| `IEEEremainder` | 49826 / 100000 | **49.826%** | **unbounded** |
| `cbrt` | 30984 / 100000 | 30.984% | 2 ULP |
| `cosh` | 28551 / 100000 | 28.551% | 2 ULP |
| `sinh` | 28078 / 100000 | 28.078% | 2 ULP |
| `pow` | 9733 / 100000 | 9.733% | 1 ULP |
| `exp` | 9617 / 100000 | 9.617% | 1 ULP |
| `log1p` | 7583 / 100000 | 7.583% | 1 ULP |
| `log` (0,1) | 7367 / 100000 | 7.367% | 1 ULP |
| `expm1` | 7119 / 100000 | 7.119% | 2 ULP |
| `tan` | 3946 / 100000 | 3.946% | 1 ULP |
| `asin` | 2547 / 100000 | 2.547% | 1 ULP |
| `cos` | 2432 / 100000 | 2.432% | 1 ULP |
| `sin` | 2369 / 100000 | 2.369% | 1 ULP |
| `tanh` | 2323 / 100000 | 2.323% | 2 ULP |
| `acos` | 892 / 100000 | 0.892% | 1 ULP |
| `atan2` | 363 / 100000 | 0.363% | 1 ULP |
| `hypot` | 321 / 100000 | 0.321% | 1 ULP |
| `log10` | 261 / 100000 | 0.261% | 1 ULP |
| `log` (full range) | 118 / 100000 | 0.118% | 1 ULP |
| `atan` | 9 / 100000 | 0.009% | 1 ULP |
| `sqrt` | **0 / 100000** | **0.000%** | — |

Every function deviates. `log`, the one that got noticed, is ninth.

---

## 3. `sqrt` is the only real zero, and it is a theorem

`sqrt` needs no port and never will. IEEE 754 **requires** `sqrt` to be correctly rounded,
so `FdLibm.Sqrt.compute` and the hardware `sqrtsd` compute the same function by
construction. Its zero is structural, not empirical.

That distinction is worth keeping visible, because a missing test and a test that documents
why no test is needed look identical from a distance. `sqrt` therefore has a test —
`sqrt_needs_no_port_because_ieee754_requires_exact_rounding` — that asserts the property on
the values where a non-conforming implementation would show first, with a failure message
that says what a failure would mean.

Its fourteen vectors were derived by hand first and **two of them were wrong**:
`sqrt(MIN_NORMAL)` and the sign of `sqrt(-1)`'s NaN. They were re-measured on HotSpot like
everything else. Deriving an oracle is not measuring one.

---

## 4. `IEEEremainder` was not a last-ULP problem

Its 49.8% is a different kind of number from the rest of the table, and its worst error is
unbounded rather than 1–2 ULP. The registered body was:

```rust
let result = if b == 0.0 || a.is_infinite() || b.is_nan() {
    f64::NAN
} else {
    let q = (a / b).round();
    a - q * b
};
```

Wrong two independent ways:

1. **`f64::round` is ties-away-from-zero.** IEEE 754 requires the quotient rounded to the
   nearest integer with **ties to even**. Every half-integer quotient came out with the
   wrong remainder — `IEEEremainder(1.5, 1.0)` returned `0.5` where the answer is `-0.5`.
2. **`a / b` overflows.** For operands whose remainder is perfectly representable —
   `IEEEremainder(MAX_VALUE, MIN_NORMAL)` — the quotient is `+inf` and `a - inf*b` is NaN.
   fdlibm never forms the quotient at all: it reduces by `fmod` against `2p` and finishes
   with two conditional subtractions, so the result is exact by construction.

This one is a defect in **`Math.IEEEremainder` as well**. Both classes' specs read "as
prescribed by the IEEE 754 standard", which fixes the result exactly — there is no 1-ULP
latitude to hide in. So `IEEEremainder` stays a single shared body, and that body is now
fdlibm's.

That makes it the answer to "does any native back both `Math` and `StrictMath`?" — before
this change, **all eighteen did**. After it, two still do: `sqrt` because sharing is
correct, and `IEEEremainder` because both classes want the same exact answer.

---

## 5. The port

`types/src/fdlibm.rs` grows from the single `log` routine to the full family: `sin`, `cos`,
`tan` (with the shared `RemPio2` / `KernelRemPio2` argument reduction), `asin`, `acos`,
`atan`, `atan2`, `exp`, `cbrt`, `hypot`, `pow`, `log10`, `log1p`, `expm1`, `sinh`, `cosh`,
`tanh`, `ieee_remainder`, plus the `__HI`/`__LO` word helpers, `Math.powerOfTwoD` and
`Math.scalb`.

Ported from **`java.lang.FdLibm` as shipped in JDK 25**, not from the C original and not
from memory. JDK 21 moved these routines out of native code into Java, so the Java source
is the normative text now. It is in `lib/src.zip`.

Rules the module follows, all of them the existing `log` port's:

- **Bracketing is preserved expression for expression.** Floating-point addition is not
  associative. Re-associating any polynomial or reconstruction changes the last bit and
  defeats the entire point of porting.
- **Every constant is written by bit pattern**, produced by running
  `Double.doubleToRawLongBits` over the hex-float literals lifted straight out of
  `FdLibm.java`. Rust has no hex float literal, and a decimal transcription is one more
  place for a last-ULP mistake to enter.
- **Java integer semantics are reproduced, not approximated.** Java's `int` arithmetic wraps
  and its shifts mask the count to five bits; Rust's panic. And Java hex literals above
  `0x7fffffff` are *negative* `int`s compared *signed* — `hx <= 0xbfd2_bec3` in `Log1p` is a
  comparison against `-1076626237`, and reading it as unsigned takes the wrong branch for
  every `x` in `(-0.2929, 0)`.
- **Cross-function calls stay inside the module.** `Log10` calls this `log`, `Atan2` calls
  this `atan`, the hyperbolics call this `expm1`/`exp` — exactly as the Java calls
  `StrictMath.log`/`atan`/`expm1`/`exp`. Routing any of them to `f64::ln` would reintroduce
  the whole defect one level down, and that is precisely how `sinh` and `cosh` came to be
  the second- and third-worst rows in the census: they inherit `expm1`/`exp`'s deviation on
  top of their own.

Two oddities in the reference are **reproduced rather than tidied**, each with a note:

- `Sinh` guards with `Long.compareUnsigned` where the otherwise identical `Cosh` guard uses
  `Integer.compareUnsigned`. Both operands sign-extend to `long`, so these are **not the
  same predicate**. Only the exact `ix == 0x408633ce` boundary can tell.
- `pow` writes `z = -1.0 * z` where `-z` would differ in a produced NaN's sign.

A port's job is to match the reference. "One sick collector ⇒ diff the two impls of the same
primitive" cuts the other way too: the way to avoid two implementations that disagree is to
have one, and to make it the reference's.

### Verification

Bit-for-bit against **2.4 million samples** — the full 100k-per-function oracle above,
replayed against the port instead of against libm. **Zero mismatches**, including
`sin`/`cos`/`tan` at arguments up to `1e6` where the multi-precision `KernelRemPio2`
reduction is doing all the work.

---

## 6. Proving the tests can fail

24 unit tests in `types/src/fdlibm.rs`, 1337 golden vectors across 19 tables, all measured
on HotSpot by `probes/StrictMathVectorProbe.java`. They assert **bits, never a tolerance** —
a tolerance passes on platform libm and therefore proves nothing, which is how the
`nextGaussian` divergence survived a green suite for months.

They live in `types/` so `cargo test -p cratonvm-types` runs them. Not in
`vm/src/vm/tests.rs`, which is synthetic-jdk-only and would go dark on the default build.

Each table is **branch boundaries crossed with seeded random draws**. The boundaries are the
half that matters: every fdlibm routine is a decision tree over the high word — `|x| <
2^-27`, `|x| >= 0.6744`, `|x| > 22`, `hx <= 0xbfd2bec3` — and a port that takes one wrong
branch is correct almost everywhere and wrong on a set a uniform sample never visits. Each
boundary constant appears with both its neighbours by ULP.

Then the check that decides whether any of it is worth anything: **replay every committed
table against the platform libm and count the rows that FAIL.** A table libm already
satisfies is a test that cannot detect the defect it was written for.

| table | vectors | libm fails | | table | vectors | libm fails |
|---|---|---|---|---|---|---|
| `IEEEremainder` | 81 | 29 | | `log10` | 62 | 2 |
| `cbrt` | 65 | 19 | | `tanh` | 71 | 2 |
| `atan2` | 64 | 13 | | `acos` | 65 | 1 |
| `sinh` | 71 | 11 | | `cos` | 89 | 1 |
| `cosh` | 71 | 10 | | `hypot` | 70 | 1 |
| `expm1` | 77 | 9 | | `sin` | 89 | 1 |
| `exp` | 71 | 7 | | `atan` | 65 | **0** |
| `log1p` | 71 | 6 | | `sqrt` | 14 | **0** |
| `pow` | 87 | 6 | | | | |
| `asin` | 65 | 3 | | | | |
| `tan` | 89 | 3 | | | | |

`sqrt`'s zero is the theorem from §3. **`atan`'s was a real hole.** Its deviation rate is
0.009% — nine in a hundred thousand — so a 65-vector table had better than a 99% chance of
containing no row that libm gets wrong. It would have passed whether or not `atan` was
ported at all, and it looked exactly like the other seventeen tests.

Fixed by pinning the nine precise inputs where MSVC's CRT and fdlibm disagree, found by
replaying the oracle, with a comment saying not to drop them when regenerating. The lesson
generalises past this file: **a near-zero deviation rate is what makes a vacuous green easy
to miss.** The functions that are almost always right are the ones whose tests most need to
be checked against a known-bad implementation, because random sampling will not do it for
you.

Every table also carries a lower-bound assertion on its own row count, so a table that
silently empties out fails loudly instead of passing vacuously.

---

## 7. The fix that was on the registrar that loses

W7-44 put fdlibm's `log` into `Random.nextGaussian`'s polar method. The VM went
on emitting the wrong value anyway, because it fixed the wrong copy.

`java/util/Random.nextGaussian` is registered **twice**, and registration is
last-write-wins:

| | registrar | body | had the fix? |
|---|---|---|---|
| 1 | `native-collections` `register_random_natives` | its own polar helper | **yes** |
| 2 | `native-builtins::securerandom` | its own polar loop | **no — `f64::ln`** |

and `vm/src/vm/vm_init.rs` calls the second one *after* the first, deliberately,
inside a block labelled `LAST-WRITE-WINS BOUNDARY — do not reorder`. The
collections version reads the LCG seed from a synthetic two-field layout; in
real-JDK mode field 0 is an `AtomicLong` reference rather than a long, so that
version reads zero and a seeded `Random` produces all-zero output. The
`securerandom` handlers are layout-independent, so they *must* win.

They won, and they were still on `f64::ln`. A third copy in
`native-builtins/src/lib.rs` — superseded, unregistered, but compiled and tested
— was on `f64::ln` too.

### Why nothing caught it

There **was** a bit-exact test asserting the seeded stream, and it was green the
entire time. It lives in `native-collections` and calls that crate's helper
directly; it never goes through the registry, so it tests a body the VM does not
run.

That is the part worth carrying forward. A test on the wrong side of a
last-write-wins boundary is not weak evidence, it is *no* evidence — and it is
indistinguishable from a good test by inspection. It asserts the right values,
by bits, for the right reason. The only thing wrong with it is which function it
calls.

### What changed

- `securerandom`'s body uses `cratonvm_types::fdlibm::log`. This is the one that
  runs.
- Its polar arithmetic is extracted into `rnd_gaussian_pair`, taking `next(bits)`
  as a closure. The native itself cannot be unit-tested — its draws need a live
  `NativeContext` — and *that* is why the only test of this arithmetic had ended
  up written against another crate's duplicate of it. Untestable code grows a
  tested twin somewhere else, and the twin drifts.
- `native-builtins` now carries its own `next_gaussian_matches_jdk_seeded_sequence`
  over its own helper: the same six `new Random(42)` values from Temurin
  25.0.3+9, asserted as bits.
- The superseded `lib.rs` copy is fixed too. A superseded copy left on `f64::ln`
  is what a future re-registration reinstates silently.

**Fixing one registrar of two is indistinguishable from fixing none.**

---

## 8. The tolerance sweep

Eleven tolerances found and tightened to bit comparisons. Criterion: the thing
compared has an **exact** contract — IEEE-754 correctly-rounded, or specified bit
manipulation, or a named javadoc special case — but was asserted approximately.

| site | was | contract |
|---|---|---|
| `jit/src/x64/tests.rs` | `sqrt(2.0)` within `1e-14` | exactly rounded |
| `vm/src/vm/tests.rs` | `nextUp(1.0)`, `nextDown(1.0)` within `1e-10` | bit manipulation |
| `vm/src/vm/tests.rs` | `nextAfter` × 2, ordering only (`v > 1.0`) | bit manipulation |
| `vm/src/vm/tests.rs` | `ulp(1.0)` `< 1e-10`, `ulp(1.0f)` `< 1e-5` | exactly `2^-52` / `2^-23` |
| `vm/src/vm/tests.rs` | `hypot(3,4)`, `sinh(1.0)`, `toRadians(180)` | exact in practice |
| `native-builtins/src/lang_math.rs` | `toRadians(180)`, `toDegrees(PI)` | exact |
| `vm/tests/…/TckLang.java` | `sqrt(144.0)` in a ±0.01 band | exactly `12.0` |
| `vm/tests/…/TckLang.java` | `Math.abs(sin(0.0)) < 0.001` | signed zero |

`ulp(1.0)` is the illustrative one: the contract is a single double, `2^-52`, and
`v > 0.0 && v < 1e-10` passes for roughly a million wrong answers.

### A predicted defect that did not survive the oracle

A source-level reading flagged `toRadians`/`toDegrees` as a **live divergence**,
not merely a loose test: the JDK computes `angdeg / 180.0 * PI` while
`native_math_to_radians` delegates to Rust's `f64::to_radians()`, which is
`self * (PI / 180.0)` — a different expression that rounds differently, and
structurally the same shape as the `log` bug.

It is wrong. That is **JDK 8's** formula. JDK 25 reads:

```java
private static final double DEGREES_TO_RADIANS = 0.017453292519943295;
public static double toRadians(double angdeg) { return angdeg * DEGREES_TO_RADIANS; }
```

and that literal is `0x3f91df46a2529d39`, **bit-identical** to Rust's
`PI / 180.0`. Measured over 2M samples across the full exponent range: zero
disagreements, both conversions. There is no divergence; the tolerance was just
too wide.

Worth recording because the reasoning was good and the conclusion was still
wrong. A predicted defect is a hypothesis until an oracle answers it, and the
cost of checking here was one twelve-line program.

### Left alone, deliberately

- `Math.pow`/`log`/`exp` bands in `TckLang` — a genuine 1-ULP contract, and
  `exp(1.0) == Math.E` is not guaranteed.
- `SecureRandom.nextGaussian`'s distribution check — statistical, no seeded
  contract.
- The GPU float kernel's `1e-3` — PTX FMA contraction is real.
- **`types/src/fdlibm.rs`'s own `1e-15` whole-range guard.** This one looks
  exactly like the defect and is not: it is a deliberately coarse check of the
  port against *host libm*, paired with the bit-exact golden table beside it to
  catch a botched branch on an exponent the table misses. Tightening it would
  make it fail on correct code. The distinguishing question is not "is there a
  tolerance" but "what is on the other side of the comparison".
- A cluster of loose asserts on double round-trips through value stacks and FFI:
  exact contracts, but a tolerance there cannot hide a libm gap, and most already
  sit beside a correct bit-exact sibling.

---

## 9. Not a defect, checked because it looked like one

`native_math_pow` carries a fast path: integer exponent, `|b| < 64`, finite base ⇒
`a.powi(b)`. `powi` is repeated multiplication, whose error grows with the exponent, and
`Math.pow` promises 1 ULP — so at `b = 63` a naive analysis predicts several ULP and a
contract violation.

Measured against fdlibm `pow` over 2M samples (base in `(0.5, 100)`, integer exponents in
`[-63, 63]`): **0 samples exceed 1 ULP**, worst observed 1 ULP — the same bound `powf`
achieves on the same inputs. The fast path meets the contract and stays.

---

## 10. What is *not* covered

- **Nothing was re-measured end-to-end on a CratonVM binary.** This lane does not build.
  The differential probe row that would confirm it is `Random.nextGaussian` and the
  `StrictMath.*` rows of the shadow differential.
- **`sqrt` is not ported**, deliberately — §3.
- **The oracle is one platform.** All disagreement rates above are MSVC CRT on Windows
  x86-64. glibc and macOS libm deviate from fdlibm too, but at different rates and on
  different inputs; the *port* is platform-independent by construction, so the rates only
  ever mattered for prioritisation. Re-running `StrictMathOracleDumpProbe` on Linux would
  produce a different table and the same conclusion.
- **`java.lang.Math` is untouched** apart from `IEEEremainder`, and that is intentional.
  libm satisfies `Math`'s 1-ULP bound; pointing both classes at the fdlibm bodies would fix
  nothing and would only make the far more common caller slower.

---

## 11. Re-measurement

The steps that would close this record properly, in order:

1. `cargo test -p cratonvm-types` — the 24 bit-exact tests.
2. `cargo clippy --workspace --all-targets -- -D warnings` — the module carries a
   module-level `#![allow(clippy::eq_op)]` (fdlibm's `x - x` NaN idiom, 22 sites) and three
   per-function allows, each with a note; nothing else should fire.
3. Build, then run the differential probe and confirm the `Random.nextGaussian` row and the
   `StrictMath.*` rows against HotSpot.
4. Re-run `probes/StrictMathOracleDumpProbe.java` against a CratonVM binary rather than
   against Rust's `f64` methods, which measures the *registered natives* end to end rather
   than the ported routines in isolation. That is the arm this record cannot supply.
5. `cargo test -p cratonvm-native-builtins` — the new
   `next_gaussian_matches_jdk_seeded_sequence` in `securerandom`, and the two tightened
   `toRadians`/`toDegrees` unit tests.
6. `cargo test -p cratonvm-jit` and the `vm` suites, for the other nine tightened
   assertions. Each expected value was measured against the actual backing before being
   asserted, so these should pass unchanged — but they are assertions that were loosened
   once already, and the point of tightening them is that they now can fail.

Step 4 is the one this record could not supply, and it is the one that is now
scheduled rather than remembered — see §11 below.

---

## 11. The scheduled vector: `RJdkStrictMath` (added 2026-08-12)

`regression-suite/src/RJdkStrictMath.java`. **557 golden vectors, 662 checks,
20 functions**, every expectation measured on Microsoft OpenJDK 25.0.3.9 and
written as raw bit patterns on both sides — Java has no hex float literal, and a
decimal transcription is one more place for a last-ULP mistake to enter.

Why a suite vector and not another probe: §10's "not re-measured end to end" was
not an oversight, it was unreachable with the artefacts this record shipped. The
24 unit tests call the ported routines directly and never touch the registry —
the same wrong-side-of-the-boundary shape §7 catalogues, where a bit-exact test
in `native-collections` was green for months against a body the VM does not run.
The two probes do go through the VM, but `probes/` is **never** run by
`regression-suite/run.sh` at any `SUITE=` value. `RJdkStrictMath` is the first
artefact that is both on the running path and scheduled.

**It is a shared vector.** In JDK 25 `StrictMath.sin` and friends are not
`native` (see the header block, §3), so HotSpot's answer IS fdlibm's and is a
sound oracle; CratonVM's `Intrinsic`-kind natives do not yield to real bytecode,
so both CratonVM arms reach the Rust port. One expectation, three arms, no mode
divergence to encode — unlike `RJdkStrict`, this belongs in the shared list.

Design choices worth not re-litigating:

* **Bits, never a tolerance.** A tolerance passes on platform libm and therefore
  proves nothing — §6's whole argument, applied to the suite.
* **NaN results are asserted as `isNaN`, not as raw bits.** Which NaN a routine
  produces is unspecified; `0.0/0.0` on x86 yields a QNaN with the sign bit SET,
  which is what the oracle recorded. Freezing that would lock a divergence in
  rather than detect one — the failure mode
  docs/known-issues/jdk-only/W6-5-vacuous-tests.md warns about from the other
  side. Signed **zero** is specified (`sin(-0.0) == -0.0`) and stays under the
  raw-bit comparison.
* **A `switch` on an int code, not a method reference.** A lambda-linkage
  failure inside a numeric vector would read as a `StrictMath` failure, and
  lambda dispatch is this tree's most expensive shape.
* **Nothing computed is printed.** Every `CK` line carries a fixed count,
  because `run.sh` diffs the two runs' `CK` lines in one session.
* **Each table carries a lower-bound row-count assertion**, so a table that
  silently empties out fails loudly instead of passing vacuously — §6's rule.

### Proving it can fail

Green on the oracle, and RED under three mutations. Green-on-the-tree alone is
what §6 already showed to be worth nothing for `atan`.

| arm | result |
|---|---|
| HotSpot 25.0.3.9, `javac -Xlint:all` clean | `PASS RJdkStrictMath (662 checks)` |
| **MUT A** — the recorded pre-fix `IEEEremainder` body restored | **RED** at `IEEEremainder(2.5, 1.0)`: expected `0x3fe0…`, got `0xbfe0…` |
| **MUT B** — `cbrt` shifted by one ULP (`Math.nextUp`) | **RED** at the first row; a tolerance-based test passes this |
| **MUT C** — `atan2`'s two arguments transposed | **RED** at `atan2(+0.0, 1.0)`: expected `0x0`, got `0x3ff921fb54442d18` |

MUT A is the one that found the §4 error: with the table skipped it fails the
*named* discriminators too, and it fails at `2.5`, not at the `1.5` §4 cites.
MUT C is worth keeping because a transposed `atan2` is a defect no accuracy test
can see — `atan2(y,x)` and `atan2(x,y)` are both plausible angles and only the
quadrant is wrong — which is why the sign/zero/infinity matrix sits at the head
of that table.

### Honest limit on the sampled half

The boundary vectors are hand-chosen and are the half that matters; the sampled
half can only catch a function whose deviation rate is not tiny. Against the
MSVC CRT those rates ran from `cbrt` 30.98% to `atan` 0.009% (§2), so these
tables are near-certain to catch `cbrt`/`cosh`/`sinh` and **will not catch
`atan` by sampling at all** — `atan`'s coverage here is its four branch
boundaries (`7/16, 11/16, 19/16, 39/16`) and nothing else. That is the same hole
§6 found in the unit tables and fixed by pinning nine oracle-derived inputs; the
suite vector does not have those nine, because finding them needs the Rust-side
replay this lane cannot run. **Named residual**, §12.

### Registration

The `run.sh` line is a **nomination**, not an edit — this lane does not own
`run.sh`. `RJdkStrictMath` goes on the shared class list, not `JDKONLY_CLASSES`
(which is the mode-divergent list `RJdkStrict` belongs to).

---

## 12. Residuals opened by this pass

1. **The nine `atan` oracle-derived inputs are not in the suite vector.**
   §6 pins them in `types/src/fdlibm.rs`; reproducing them for `RJdkStrictMath`
   needs a replay of the oracle against the platform libm, which is a Rust
   program. Until then `atan` is boundary-covered only, and this is stated at
   the site rather than left for a reader to infer.
2. **The de-registration alternative (header §3) is unevaluated.** JDK 25 ships
   bit-exact `FdLibm` bytecode for all eighteen; CratonVM shadows it with
   `Intrinsic`-kind natives that do not yield. Dropping the eighteen strict
   triples in real-JDK mode would be free correctness and one fewer shadow, at
   an unmeasured throughput cost on a hot surface. Needs a build and an owner.
   Note this would NOT remove the need for the Rust port: synthetic mode has no
   `FdLibm` bytecode to fall back to.
3. **The oracle is still one platform.** All rates in §2 are MSVC CRT on Windows
   x86-64. The *port* is platform-independent by construction and the new golden
   tables are too — they are fdlibm's answers, not this host's — so a Linux run
   should produce identical `CK` lines. That is an assertion this vector now
   makes checkable rather than one it assumes.
