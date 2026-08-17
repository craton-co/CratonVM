# ✅ FIXED — `AccurateMathStrictComparisonTest`: `Math.floorDiv` panicked the process, and hid six more `lang_math` defects behind the crash

## Status
**RESOLVED 2026-08-17** on branch `fix/commonsmath-math-divide-overflow-20260816`.
`AccurateMathStrictComparisonTest` now runs **69 tests found / 69 successful /
0 failed** under CratonVM — the same `found` count HotSpot JDK 25 reports on the
identical classpath, so this is a real green and not a `started=0` one.

Filed originally as: *commons-math: `AccurateMathStrictComparisonTest` crashes
the whole CratonVM process with a Rust integer-overflow panic.*

## The failure, as filed

```
thread 'main-vm' panicked at native-builtins/src/lang_math.rs:2256:13:
attempt to divide with overflow
[cratonvm] main-vm run() returned Err: Error in thread "main" internal error:
  native method panic: attempt to divide with overflow
```

Reproduced exactly, at exactly that line, before touching anything.

## Root cause

Line 2256 was `let d = a / b;` inside `native_math_floor_div_int`.

**Rust's `/` and `%` on signed integers are checked for the `MIN_VALUE / -1`
pair in every build profile.** This is not the usual debug-only overflow
assertion that `release` turns off — LLVM treats that particular division as
undefined behaviour, so the check is unconditional. The four `floorDiv` /
`floorMod` natives (int and long) all used plain `/` and `%` on operands that
come straight from Java, and `AccurateMathStrictComparisonTest` — which
reflectively walks every `public static` method on `StrictMath` and calls it
over generated edge-case inputs — reached that pair.

Java specifies a value for all four cases. `Math.floorDiv`'s javadoc says so in
as many words: for `floorDiv(Integer.MIN_VALUE, -1)` "integer overflow occurs
and the result is equal to `Integer.MIN_VALUE`". `floorMod` of the same pair is
`0`. The fix is `wrapping_div` / `wrapping_rem`, which is what the interpreter's
own `idiv`/`irem` opcodes and the JIT's constant folder had been doing all
along — only the natives were out of step.

The doc's own next step ("audit `lang_math.rs` for other unchecked arithmetic")
came back clean: those four functions held the only unguarded signed `/` and `%`
in the file, and `Integer.divideUnsigned` / `Long.remainderUnsigned` in
`phases_late.rs` operate on `u32`/`u64`, where the pair cannot arise.

## What the crash was hiding

This is the interesting half. A native panic ends the run, so **every other test
in the class was lost along with the one comparison that tripped it**. With the
panic gone the class actually executed its 69 reflective comparisons and
reported three genuine divergences that had never been visible:

| row | CratonVM said | HotSpot says |
|---|---|---|
| `double ulp(-Double.MAX_VALUE)` | `Infinity` | `2^971` |
| `float ulp(Float.MAX_VALUE)` | `Infinity` | `2^104` |
| `int getExponent(-4.9E-324)` | `-1074` | `-1023` |

* **`ulp(±MAX_VALUE)`** — the body computed `nextUp(|d|) - |d|`, and stepping up
  from `MAX_VALUE` lands on infinity. At the top of the range the gap below
  equals the gap above, so step *down* instead; that subtraction is exact.
* **`getExponent(subnormal)`** — the body normalized the significand and
  returned the subnormal's *true* exponent. The javadoc asks for "the unbiased
  exponent used in the **representation**", and every subnormal stores exponent
  field 0, so the answer is `MIN_EXPONENT - 1` == `-1023`, same as zero. The
  whole `leading_zeros` branch was computing a real number that nobody asked
  for.

## Then the audit found six more

Rather than stop at the three rows one test happened to check, the whole
registered `Math`/`StrictMath` native surface was replayed against a HotSpot
JDK 25 oracle — `probes/MathSurfaceSweep.java`, ~41,000 rows printed as **raw
bit patterns** so `-0.0`, NaN payloads and subnormals compare exactly. It
started at **764 divergent rows** and finished at **13**.

| family | rows | what was wrong |
|---|---|---|
| `nextAfter` | 168 | the JDK writes `return start + direction;` for the NaN case, which propagates whichever operand is the NaN, sign and payload intact; this returned a canonical NaN |
| `pow(±1, ±∞)`, `pow(1.0, NaN)` | 8 | C99 defines `pow(±1, anything)` as `1.0`; **Java deliberately overrides that to NaN**, so the case has to be taken before libm sees it |
| `log10(negative)` | 17 | NaN, but HotSpot's stub yields the x86 *default* QNaN, which has its sign bit **set**. `Math.log` already agreed; only `log10` needed pinning |
| `pow(NaN, y)` | 4 | libm clears the sign bit off a NaN base; HotSpot propagates it |
| `nextUp`/`nextDown`/`rint` (NaN) | 6 | the JDK bodies pass the argument through — `rint` via `Math.abs`, which clears the sign and keeps the payload |

Note the shape of the `pow` one: the existing comment in `native_math_pow`
**already claimed** the fall-through "preserv[ed] Java/JLS special-value
semantics (e.g. `pow(1, ±inf) == NaN`)". It did not. A comment asserting the
property is not the property.

### And one shortcut that was never priced

`native_math_pow` also carried an integer-exponent fast path: `b.fract() == 0 &&
|b| < 64` went to `powi`, i.e. repeated multiplication, plus a reciprocal for
negative exponents. `probes/PowIntExpProbe.java` puts 400 bases against every
exponent in `[-70, 70]` and prices it:

| path | divergent rows |
|---|---|
| `powi` shortcut | **36,947 of 55,600** (66%) |
| the `powf` line it bypassed | 17 of ~800 |

Removing the shortcut takes that census from **36,964 → 188** divergences.

The measured census that chose libm for `Math.pow` in the first place (the table
above `let strict` in `lang_math.rs`, "pow: libm 1, fdlibm 89") drew
**continuous** exponents, so every row it scored went down the `powf` path — it
never saw the shortcut sitting in front of it. The shortcut itself arrived in a
bulk agent-wave perf commit with no accuracy check of its own.

It was not even buying speed. Interleaved A/B on `PowPerfProbe`, integer
exponents, same host, three rounds:

| round | with `powi` | without |
|---|---|---|
| 1 | 2654 ns/call | **1407** |
| 2 | 2386 ns/call | **2082** |
| 3 | 1524 ns/call | **1336** |

Faster without it in every round. (HotSpot: 38.8 ns/call — a separate matter,
see the interpreter-throughput doc.)

## What is deliberately NOT fixed

**13 rows of the 41,195 remain**, all one-ulp: `Math.log10(π)`, `Math.sin(±2.5)`,
and 10 `Math.pow` rows. These are the documented libm-vs-intrinsic residual —
`lang_math.rs` explains at length why the functions HotSpot intrinsifies
(`_dsin`, `_dlog10`, `_dpow`, …) keep libm as the closer of the two available
backings, since HotSpot's own answers come from Intel LIBM assembly that matches
neither candidate. Closing those needs a third backing, not a bug fix.

**Two-argument rows where BOTH arguments are NaN are excluded from the sweep on
purpose.** Which NaN comes back is decided by which operand the register
allocator put in the destination of the add; HotSpot's answer there is a JIT
artifact, not a contract, and pinning it would make the instrument fail for the
wrong reason.

## Also in the original report, and correctly diagnosed there

The dozens of `Cannot find AccurateMath method corresponding to: ...
StrictMath.clamp/fma/floorDivExact/...` lines are commons-math's own harness
reporting methods added to `StrictMath` after the `AccurateMath` port was
written. Present and harmless on HotSpot too. Not a CratonVM defect, and the
original doc said so.

## Verification

* `AccurateMathStrictComparisonTest`: **69/69 pass** under CratonVM, and HotSpot
  reports the same 69 found — checked side by side on the same classpath.
* `probes/MathSurfaceSweep`: 764 → **13** divergent rows, all documented above.
* `probes/PowIntExpProbe`: 36,964 → **188** divergent rows.
* 14 new unit tests in `lang_math.rs`, every expectation a raw bit pattern
  recorded from the oracle rather than reasoned from the javadoc. `cargo test -p
  cratonvm-native-builtins lang_math` → **84 passed, 0 failed**.
* The red was proven first: the pre-fix binary panics at
  `lang_math.rs:2256`, the fixed one prints a corner-case checksum
  (`-6385378963727343400`) identical to HotSpot's.

## The transferable part

**A native panic is not a failed test, it is a lost test class.** The three
`ulp`/`getExponent` defects sat in the same file as the crash, in methods the
same test class checks, and stayed invisible for as long as the process died
before reporting. When a crash is fixed, re-read what the surviving run says
before calling it closed.

**A reflective differential test only covers the intersection.**
`AccurateMathStrictComparisonTest` checks `StrictMath` methods that
`AccurateMath` also implements — it found 3 of the 9 defects here. The other 6
needed a sweep of the surface *as registered*, not as some third-party test
happens to exercise it.

**Re-measure a fast path against the census that justified its neighbour.** The
`pow` shortcut lived directly above a carefully measured table that did not
cover it, which is the easiest possible way for a 66% divergence to look
audited.
