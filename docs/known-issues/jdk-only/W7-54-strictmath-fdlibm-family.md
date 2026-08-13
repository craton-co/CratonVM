# W7-54 — `StrictMath` was the platform's libm, on every function, not just `log`

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
