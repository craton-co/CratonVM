# commons-math: `AccurateMathStrictComparisonTest` crashes the whole CratonVM process with a Rust integer-overflow panic

## Status
**OPEN, confirmed CratonVM-specific, high severity (process crash, not a test assertion failure)**
— found 2026-08-16 running commons-math's `commons-math-legacy-core` suite
under CratonVM on Azure. Differential-verified against real HotSpot JDK 25:
passes cleanly (all reflective comparisons resolve or are skipped as
expected).

## The crash
`AccurateMathStrictComparisonTest` reflectively walks every `public static`
method on `java.lang.StrictMath` and, for each, looks up (and calls) the
corresponding method on commons-math's own `AccurateMath`, comparing
results across generated edge-case inputs (this is the class's whole
purpose — a reflection-driven differential test against the JDK's own
`StrictMath`).

Under CratonVM this doesn't fail a JUnit assertion — **it panics and aborts
the entire process**:
```
thread 'main-vm' panicked at native-builtins/src/lang_math.rs:2256:13:
attempt to divide with overflow
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
ERROR cratonvm_vm::vm::vm_exec: Native method panic caught in <cb@0x64c334b9e620>:
  attempt to divide with overflow
  (native invoked from org/apache/commons/math4/legacy/core/jdkmath/AccurateMathStrictComparisonTest.callMethods(...))
[cratonvm] main-vm run() returned Err: Error in thread "main" internal error: native method panic: attempt to divide with overflow
```
A Rust-level checked-arithmetic panic (`attempt to divide with overflow`)
inside a native builtin at `native-builtins/src/lang_math.rs:2256` is
reached from a reflective invocation of one of CratonVM's native
`Math`/`StrictMath`-family intrinsics, most likely one of the
`floorDiv`/`floorMod`/`ceilDiv`/`ceilMod` family being exercised at the
classic overflow edge case (`Integer.MIN_VALUE` divided by `-1`, or the
`long` equivalent) — Java's own `Math.floorDiv`/`floorMod` etc. are
specified to either return a defined result or throw `ArithmeticException`
for exactly this input; a raw Rust integer-division panic is neither, and
takes the whole VM process down with it (`rc` becomes non-zero, no JUnit
report at all — every other test in the class is lost along with it, not
just the one comparison that triggered it).

## Also observed (separate, lower-severity, likely NOT a CratonVM bug)
Before the panic, dozens of lines like:
```
Cannot find AccurateMath method corresponding to: public static long java.lang.StrictMath.floorDivExact(long,long)
Cannot find AccurateMath method corresponding to: public static double java.lang.StrictMath.fma(double,double,double)
Cannot find AccurateMath method corresponding to: public static double java.lang.StrictMath.clamp(double,double,double)
...
```
These are the test's own harness reporting that commons-math's `AccurateMath`
class (last meaningfully updated for an older JDK baseline) doesn't yet
implement several `StrictMath` methods added in more recent JDK releases
(`clamp`, `fma`, `*Exact` variants, `multiplyHigh`, etc.). This is a
commons-math-side gap, not a CratonVM defect — HotSpot's `StrictMath` simply
has more methods than the AccurateMath port is a copy of. Confirmed
present-and-harmless on HotSpot too (same messages, no failure). Included
here only so the panic below isn't confused with this separate, benign
category of "unmatched method" messages that precede it in the log.

## Next steps
* Open `native-builtins/src/lang_math.rs` at line 2256 to identify exactly
  which native builtin performs the unchecked division, and which
  StrictMath-family method it backs.
* Fix: match Java's actual specified behavior for that method at the
  overflow edge case (typically `ArithmeticException` for the `Exact`
  variants, or a defined wraparound/saturation result for plain
  `floorDiv`/`floorMod`/`ceilDiv`/`ceilMod` — check the specific method's
  javadoc) instead of an unchecked/panicking Rust division.
* This is a process-crash-on-panic pattern — worth a quick audit of
  `lang_math.rs` for other unchecked arithmetic (`/`, `%`, or unwrapped
  `checked_div`/`checked_rem`) that could panic the same way on other inputs
  the test class's edge-case generator happens not to hit.

## Repro
```bash
cd apps/commons-math/commons-math-legacy-core
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "<full commons-math classpath>" org.junit.platform.console.ConsoleLauncher \
  --disable-banner --disable-ansi-colors --select-class \
  org.apache.commons.math4.legacy.core.jdkmath.AccurateMathStrictComparisonTest
```
Confirmed absent on stock HotSpot JDK 25 with the identical classpath (class
passes cleanly).
