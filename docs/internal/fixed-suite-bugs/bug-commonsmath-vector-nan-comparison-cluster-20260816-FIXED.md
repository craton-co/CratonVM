# ✅ FIXED — `expected NaN but was NaN`: `Double.compare` was reading RAW bits, and five more copies of that rule were still doing it

## Status

**FIXED 2026-08-17** on branch `fix/commonsmath-nan-residual-20260817`.

Two stages, and the second is the reason this write-up exists:

1. **The filing's own classes were already green on `dev` before this branch
   started.** Commit `25e8762dc` ("Nan patch") repaired `Double.compare`,
   `Double.equals`, `Double.hashCode` and their `Float` twins to canonicalize
   the NaN payload the way `doubleToLongBits` specifies. Measured on a
   current-`dev` binary, HotSpot / `--nojit` / JIT all agree:

   | class | HotSpot | CratonVM `--nojit` | CratonVM JIT |
   | --- | --- | --- | --- |
   | `RealVectorTest` | 82 / 0 | 82 / 0 | 82 / 0 |
   | `SparseRealVectorTest` | 106 / 0 | 106 / 0 | 106 / 0 |
   | `KendallsCorrelationTest` | 25 / 0 | 25 / 0 | 25 / 0 |
   | `StatUtilsTest` | 18 / 0 | 18 / 0 | 18 / 0 |

2. **That fix repaired the wrapper methods and left five more transcriptions of
   the same rule untouched, in two other crates.** A whole-surface NaN census
   found them. `Comparator.naturalOrder()` was still running `f64::total_cmp`,
   so `TreeSet<Double>` and `TreeMap<Double,_>` mis-ordered NaN keys; the native
   `HashMap`/`HashSet` key hash was still raw-bits, so two keys that `equals`
   landed in different buckets. Those are fixed here.

## The root cause, stated once

Rust's `f64` and Java's `double` agree on arithmetic and disagree on *identity*.
Java routes equality, hashing and ordering through `doubleToLongBits`, which
**collapses every NaN payload to one canonical pattern**. Rust's `f64::to_bits`
is `doubleToRawLongBits`, which preserves it, and `f64::total_cmp` is IEEE 754
totalOrder, a *third* order that sorts a negatively-signed NaN below `-inf`.

That matters because a NaN with a payload is not exotic. `Math.sqrt(-1.0)` is
`0xfff8_0000_0000_0000` on x86 — **on HotSpot too** — while the `Double.NaN`
constant is `0x7ff8_0000_0000_0000`. JUnit4's
`Assert.assertEquals(String, double, double, double)` short-circuits on
`Double.compare(expected, actual) == 0` before it ever looks at the delta, so
comparing those two failed while printing both sides as `NaN`. That is the
filing's headline symptom, exactly.

The original filing ruled out `Double.equals`/`compare`/`doubleToLongBits` with
a probe — correctly, for the inputs it used. `NanEqualsProbe` compared
`Double.NaN` against `Double.NaN`, two copies of the *same* payload, which raw
bits get right. The question the probe could not ask was "two NaNs with
*different* payloads", and that is the whole defect. Its other hypotheses
(a duplicated `Assert` class, the Vintage engine, message rendering) were all
sound guesses and all wrong.

## What this branch found and fixed

`NanSurface.java` censuses the whole NaN-observable surface against a HotSpot
oracle: 13 160 rows over 23 double and 16 float bit patterns — every distinct
NaN a real program produces, plus the zeros and infinities — covering the
wrappers, the arithmetic, the array helpers, the collections, the comparators,
the sorts and the streams.

**Before: 1441 of 13 160 rows disagreed with HotSpot. After: 534, and every one
of those belongs to a different, already-filed defect** (see "What is left"). The
count is identical under `--nojit` and with the JIT, so none of it is a dispatch
difference.

Five sites, in three crates, each carrying its own transcription of the rule:

| site | was | broke |
| --- | --- | --- |
| `native-collections` `natural_compare` (×4 arms) | `f64::total_cmp` | `Comparator.naturalOrder()` disagreed with `Double.compare` on **206 of 529** sampled pairs; `TreeSet<Double>` held 13 elements where HotSpot holds 9 |
| `native-collections` `map_hash_key` | raw `to_bits` | `HashMap` with two `equals` NaN keys held **2 entries where HotSpot holds 1** — the `equals`/`hashCode` contract, broken |
| `native-collections` `element_hash_code` | raw `to_bits` | same, for the element-hash paths (`distinct()` counted 13 vs 9) |
| `native-collections` `double_compare` | raw `to_bits` | the comparator paths that call it |
| `native-builtins` `Arrays.parallelSort([D)` | `f64::total_cmp` | a negatively-signed NaN sorted to the **front**, below `-inf` |
| `native-builtins` `Comparable.compareTo` boxed fallback | `f64::total_cmp` | boxed-number natural order |

Each of the two `total_cmp` sites carried a comment asserting it was correct —
"`f64::total_cmp` implements exactly that ordering". It does not, and the two
orders agree on `-0.0 < +0.0` and on NaN-versus-number, which is exactly why the
substitution survived review and every test that never put a negative NaN in a
sorted collection.

### The fix is one transcription, not six patches

`types/src/jfp.rs` now holds `double_to_long_bits`, `float_to_int_bits`,
`double_compare`, `float_compare`, `double_ordering`, `float_ordering`,
`double_hash_code`, `float_hash_code`, `double_equals`, `float_equals` — with
the reasoning for each in one place — and every one of the sites above calls it,
including the two that were already correct (`lang_math`'s wrapper natives and
`native-collections`' `DoubleStream` helper). Both crates already depended on
`cratonvm-types`, so this cost no new edge in the dependency graph.

This is the point. The `Nan patch` fixed three methods and could not have fixed
the other five, because there was nothing tying them together — five private
helpers named `double_compare`, `java_double_compare`, `java_compare_double`,
`natural_compare` and an inline `total_cmp`, in three files, two of them correct
and three not. `grep total_cmp` over the workspace now returns only prose.

## What is left, and why it is not this bug

534 rows, split cleanly by cause:

* **397 — the `CompactValue` NaN-box tag collision.** Every one involves a
  double whose bit pattern is `0xFFFC_…` or above (sign + exponent + quiet +
  marker all set), which is bit-for-bit the tag pattern, so the slot stores the
  canonical NaN instead. Filed separately and independently root-caused as
  `nan-payloads-lost-to-the-compactvalue-tag-collision-FIXED-20260828`; this census
  reached it from the opposite direction and agrees with it exactly. Payload
  only: every value still IS NaN and still compares, hashes and prints as one.
* **137 — `drem` NaN payload propagation.** `a % b` where both are NaN returns a
  different payload from HotSpot's (`fff8…` vs `7ff8…`). JLS-unspecified: the
  spec fixes the *value* as NaN and says nothing about which one. Recorded here
  because a future census will see it and should not re-investigate.

Neither is a wrong answer under the JLS; both are payload fidelity.

## Regression cover

* `types/src/jfp.rs` — five unit tests, including
  `sorting_puts_every_nan_at_the_top_whatever_its_payload`, which pins the sort
  order that `total_cmp` got wrong, and an explicit assertion that
  `total_cmp` disagrees, so the test names the thing it guards against.
* The census itself is deterministic and oracle-driven, so it can be re-run
  against any future binary: `NanSurface gen` on HotSpot, `NanSurface check` on
  CratonVM.

## Method note

The filing's probe asked "is `Double.equals(NaN, NaN)` true?" and got the right
answer to the wrong question. **When a comparison fails on two values that print
identically, the next probe is not "are they equal" but "are they the same
bits"** — `Double.doubleToRawLongBits` at the assertion site, which the filing's
own next-steps list correctly proposed and which nobody had run.

And once one transcription of a JDK contract is found wrong, the question is not
"is it fixed" but **"how many copies of it are there"**. Here: six, in three
crates, and the one that had already been fixed was not the one users hit.

---

## Appendix — the filing as it stood, verbatim

Kept so this record is self-contained: everything above adjudicates the text
below, and nothing below has been edited.
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
