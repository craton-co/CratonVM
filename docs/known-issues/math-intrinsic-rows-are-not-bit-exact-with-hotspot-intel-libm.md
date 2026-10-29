# `Math.sin/cos/tan/exp/log/log10/pow/cbrt/tanh` are not bit-exact with HotSpot (both modes)

Status: OPEN, kept by decision (commons-math triage). Not closable under the current license.
Area: `native-builtins/src/lang_math.rs` (the "split rows": `Math` backed by the host libm, `StrictMath` by
`cratonvm_types::fdlibm`)
Severity: LOW (at most 1 ULP on the measured rows; Java-visible only through the last digits of a result)

## What differs

HotSpot x86_64 serves the `java.lang.Math` rows it intrinsifies (`_dsin`, `_dcos`, `_dtan`, `_dexp`, `_dlog`,
`_dlog10`, `_dpow`, `_dcbrt`, `_dtanh`) from Intel LIBM assembly stubs, the same answer in its interpreter, C1
and C2. Those answers match neither fdlibm nor any host libm. CratonVM backs these `Math` rows with the host
libm, the closer of the two backings it has (per-row census in
`docs/internal/fixed-suite-bugs/bug-commonsmath-iterative-numeric-fp-divergence-cluster-CLOSED.md`).
`StrictMath` and every non-intrinsified `Math` row run the fdlibm port, which is 0/5400 against HotSpot.

Measured on `jdk-25.0.3.9-hotspot`, Windows x86_64, `Math.sin(i * PI / N)` for `N = 2..2^14`:

| | HotSpot | CratonVM (default mode and `--compatible`) |
|---|---|---|
| `Math.sin(PI / 4)` | `0.7071067811865475` | `0.7071067811865476` |
| `Math.sin(11 * PI / 16)` | `0.8314696123025455` | `0.8314696123025453` |

In the first row CratonVM's host libm differs from `StrictMath` (`…475`), and HotSpot's stub agrees with
`StrictMath`. In the second, the host libm agrees with `StrictMath`, and HotSpot's stub does not. Neither
backing tracks the stub.

## Where it shows

commons-math's `FastCosineTransformerTest#testTransformReal[0]` under `CORPUS_RNG_SEED=1` (the seeded corpus
harness, `regression-suite/corpus/runners/rng-seed/`): the same test fails on both VMs with the same expected
value, but the actual result printed in the message differs in the last digits
(`-2.5977331794906154E-4` on HotSpot, `-2.597733179490893E-4` on CratonVM), through
`FastCosineTransform.fct`'s `Math.sin(arg)` / `Math.cos(arg)`. That was the only real (identity-hash-masked)
difference across 16 random-input classes and 6 seeds. The corpus gate compares which tests fail and the
counts, not failure messages, so it scores the row AGREE. A test whose pass/fail flipped on this ULP would
still score DIVERGE.

## What would close it

A bit-compatible implementation of the Intel LIBM algorithms (constant tables included) for the nine rows,
used for `Math` only, in the interpreter and in every JIT path that intrinsifies these calls. The only known
source is OpenJDK's `stubGenerator_x86_64_{sin,cos,tan,exp,log,pow,cbrt,tanh}.cpp`, which is GPLv2.
Porting it into this Apache-2.0 tree was declined. A clean-room implementation from published
descriptions is possible in principle, but bit-exactness would have to be proven row by row against HotSpot.
That is what would have to exist for this page to go.
