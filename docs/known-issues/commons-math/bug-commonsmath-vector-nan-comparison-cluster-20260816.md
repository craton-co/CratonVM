# commons-math: `expected NaN but was NaN` assertion failures in vector/statistics tests (root cause not yet confirmed)

## Status
**PARTLY FIXED 2026-08-16; the NaN-shaped core is still OPEN and not
root-caused.** Found 2026-08-16 running commons-math under CratonVM on Azure.
Differential-verified against real HotSpot JDK 25: all affected classes pass on
HotSpot with the identical classpath.

**Closed since filing:** `StatUtilsTest` (`testMax`, `testMin`) — the
`expected:<NaN> but was:<-Infinity>` sibling listed below. It was never a
comparison problem at all: `Math.max`/`Math.min` were backed by Rust's
`f64::max`/`f64::min`, whose IEEE-754-2019 `maxNum` semantics deliberately
**ignore** a NaN operand and return the other one, where Java's `Math.max`
propagates it. Folding an array that contains `Double.NaN` therefore returned
the fold's identity element, `-Infinity`, instead of `NaN`. Fixed in
`native-builtins/src/lang_math.rs` (`java_max_double` and friends now transcribe
`java.lang.Math`'s own bodies, signed-zero clause included), found by the
HotSpot-oracle census described in the retired
`bug-commonsmath-gaussnewton-testmaxevaluations-no-exception-20260816`
write-up. `StatUtilsTest` is now 18/18.

**Still OPEN, unchanged by that fix** (identical failure counts before and
after): `RealVectorTest` (2), `SparseRealVectorTest` (3),
`KendallsCorrelationTest` (4). Everything below this line concerns those.

Filed as OPEN-not-root-caused rather than with a confident diagnosis because
the obvious hypothesis was checked directly and **ruled out** — see "What was
ruled out" below. This is deliberately honest about what's confirmed vs. not,
per this project's own convention of not crediting an untested explanation.

## The failure pattern
Multiple, otherwise-unrelated test classes fail with an assertion whose
printed expected and actual values are **both `NaN`**:
```
RealVectorTest:testMapSubtractToSelf
  => java.lang.AssertionError: NaN, entry #0 expected: java.lang.Double<NaN> but was: java.lang.Double<NaN>
     org.apache.commons.math4.legacy.TestUtils.assertEquals(TestUtils.java:256)

KendallsCorrelationTest:testSingleElement
  => java.lang.AssertionError: expected: java.lang.Double<NaN> but was: java.lang.Double<NaN>
     org.junit.Assert.fail(Assert.java:89)
```
`TestUtils.java:256` is inside
`assertEquals(String message, double[] expected, RealVector actual, double delta)`,
which calls **plain `org.junit.Assert.assertEquals(String, double, double, double)`**
per-entry — *not* the NaN-aware wrapper `TestUtils.assertEquals(String, double,
double, double)` defined a few lines above it in the same file (which
special-cases `Double.isNaN(expected)` before comparing). JUnit4's own
`Assert.assertEquals(double, double, double)` is documented to short-circuit
via `Double.compare(expected, actual) == 0` before falling through to the
delta comparison, and `Double.compare(NaN, NaN)` is specified to return `0`
(NaN is defined as equal to itself under `Double.compare`/`.equals()`, unlike
primitive `==`) — so this assertion is expected to pass when both sides are
NaN, on a spec-compliant JVM.

Also seen, not NaN-shaped but likely part of the same general "vector entry
comparison" family and worth investigating together:
* `SparseRealVectorTest:testSubtractSameType` — `entry #45, left = 0.0, right
  = NaN` (a genuine 0.0-vs-NaN numeric divergence, not a same-value
  comparison quirk). **Still open.**
* `StatUtilsTest:testMax` — `expected:<NaN> but was:<-Infinity>` (also a
  genuine numeric divergence). **FIXED** — see the Status section: `Math.max`
  was dropping the NaN. This one being a real arithmetic defect while the
  NaN-shaped ones are not is the reason they were worth separating.

## What was ruled out
A minimal, H2/commons-math-independent probe (`NanEqualsProbe.java`) checked
whether CratonVM's `java.lang.Double` NaN handling itself is broken:
```java
Double.valueOf(Double.NaN).equals(Double.valueOf(Double.NaN))   // true, both VMs
Double.compare(Double.NaN, Double.NaN)                          // 0, both VMs
Double.doubleToLongBits(Double.NaN)                             // identical bit pattern, both VMs
Double.doubleToLongBits(0.0/0.0)                                // same canonical NaN bits, both VMs
new ArrayList<Double>(List.of(Double.NaN)).contains(Double.NaN) // true, both VMs
```
**All identical between CratonVM and HotSpot.** So the core JDK primitive —
`Double.equals`/`compare`/`doubleToLongBits`, and NaN canonicalization from
different arithmetic paths (`0.0/0.0` vs `NaN*1.0` vs the `NaN` literal) — is
not the defect. Whatever makes `Assert.assertEquals(String, double, double,
double)` fail on two values that print identically as NaN must be something
more specific to the actual call context in these tests (JUnit Vintage engine
dispatch, the specific `RealVector.getEntry(i)` computation path, or
something in how the failure MESSAGE is rendered that doesn't reflect the
actual compared values) — not yet identified.

## Next steps
* Reproduce with a smaller, targeted probe: call
  `org.junit.Assert.assertEquals("msg", Double.NaN, someRealVector.getEntry(0), delta)`
  directly (bypassing the full `RealVectorTest` suite) to confirm the JUnit4
  assertion itself is where the divergence lives, not something upstream
  that computes a not-quite-NaN value that merely *displays* as NaN.
  Print `Double.doubleToLongBits(actual.getEntry(i))` right at the assertion
  site (not just its `toString()`) to rule out a "different NaN payload
  formats identically" possibility explicitly.
* Check which `org.junit.Assert` class is actually being loaded at the
  failure site — the classpath carries both the project's own resolved
  `junit:junit:4.13.2` (used to compile the JUnit4-style tests) and the
  `junit-platform-console-standalone` uber-jar's bundled JUnit4 (used to run
  them via the Vintage engine) — if these somehow resolve to different
  `Assert` class instances/versions under CratonVM's classloading vs
  HotSpot's, that alone could explain a divergent `assertEquals` behavior
  unrelated to `Double` semantics at all. Worth eliminating before assuming
  anything CratonVM-specific about `Double`.
* Investigate the remaining non-NaN-shaped sibling (`SparseRealVectorTest`)
  separately. Its former partner `StatUtilsTest` turned out to be an ordinary
  arithmetic defect with nothing to do with comparison, which is weak evidence
  that this one is too — check `Math`/`JdkMath` primitives on the actual
  operands before assuming a comparison or vector-machinery cause.
* Run the same trick that found the `StatUtilsTest` cause: replay a HotSpot
  oracle over `java.lang.Math` (`probes/MathCensus.java`) rather than reasoning
  about which primitive "should" be fine. It takes minutes and it named two
  defects that inspection had missed.

## Repro
```bash
cd apps/commons-math/commons-math-legacy
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "<full commons-math classpath>" org.junit.platform.console.ConsoleLauncher \
  --select-class org.apache.commons.math4.legacy.linear.RealVectorTest
# or KendallsCorrelationTest / SparseRealVectorTest / StatUtilsTest
```
