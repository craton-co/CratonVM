# ES vector-codec family: dot-product scores read back as 0.0, and a JIT-only SIGSEGV

Status: OPEN

Observed: 2026-07-11, while verifying the fix for
[`ES-FAIL-FAMILY-20260710-floatbuffer-abstract-receiver-nocode-FIXED.md`](../../internal/elasticsearch-suite/ES-FAIL-FAMILY-20260710-floatbuffer-abstract-receiver-nocode-FIXED.md).

Not a duplicate of that doc: these two symptoms are unmasked by, but
independent of, the abstract-receiver no-Code bug — confirmed present on the
**unmodified baseline binary** (before the FloatBuffer fix), and unaffected
(neither fixed nor worsened) by it. Two distinct symptoms, tracked together
because both surfaced in the same investigation and both point at the same
suspect: `native_es92_int7_vectors_scorer_int7_dot_product_bulk`, added the
same day in commit `1c60eb0ffa` ("Fix Elasticsearch sliced IVF no-JIT
hang").

## Symptom 1: vector similarity scores read back as 0.0 (interpreter, `--nojit`)

Every ES vector-codec class in the family (`ES813*`, `ES814*`, `ES816*`,
`ES818*`, `ES93*`, `ES940*`, `ESNext*`) runs to completion under `--nojit`
but with several failures of this shape:

```text
java.lang.AssertionError: expected:<1.0> but was:<0.0>
org.junit.internal.ArrayComparisonFailure: arrays first differed at element [0]; expected:<0.3339009> but was:<0.0>
```

Example counts (Linux/Azure host, `cratonvm-es-floatbuffer-fix2`,
`--nojit`, same for the pre-fix baseline binary):

| Class | Tests run | Failures |
|---|---:|---:|
| `ES940v1DiskBBQVectorsFormatTests` | 52 | 9 |
| `ES816BinaryQuantizedVectorsFormatTests` | 59 | 11 |
| `ES818HnswBinaryQuantizedVectorsFormatTests` | 58 | 14-15 |
| `ES93FlatVectorFormatTests` | 106 | 16 |
| `ES93BinaryQuantizedBFloat16VectorsFormatTests` | 58 | 2 |

Every observed value is exactly `0.0` (or an array of exact `0.0`s) where a
non-zero similarity/dot-product score was expected — consistent with a
scorer that always computes/returns zero rather than a rounding or
byte-order bug. Prime suspect: `int7DotProductBulk` on
`org/elasticsearch/simdvec/ES92Int7VectorsScorer`
(`native_es92_int7_vectors_scorer_int7_dot_product_bulk`,
`native-builtins/src/lib.rs`, added in `1c60eb0ffa` same-day as the
FloatBuffer fixes) — not yet confirmed by a focused repro; flagging for
follow-up rather than tracing further in this session.

## Symptom 2: JIT-only SIGSEGV (JIT on, default mode)

Under the CLI's default JIT-on mode, the SAME classes that complete cleanly
under `--nojit` instead crash:

```text
Thread N "SUITE-<Class>" received signal SIGSEGV, Segmentation fault.
0x00007ffff77b3144 in ?? ()
#0  0x00007ffff77b3144 in ?? ()
#1  0x0000020014bbc660 in ?? ()
...
```

- Reproduces on 20/22 of the family's classes (all but
  `PreconditionerTests`, which is unrelated single-fixture, and — before the
  FloatBuffer fix — `ES814HnswScalarQuantizedVectorsFormatTests`, which hit
  the `put(int,float)` AbstractMethodError first and never reached the
  crashing code path).
- `gdb` backtrace is entirely `?? ()` frames with no ELF symbols — the crash
  is in JIT-generated machine code (expected: JIT frames never
  symbolicate, see `reference_crash_debug_tooling` project notes), not in
  interpreter/native Rust code.
- Confirmed via `--nojit`: same test classes run to completion with no
  crash (only the Symptom-1 assertion failures) — this is JIT-specific.
- Confirmed on the **unmodified pre-FloatBuffer-fix baseline binary** too
  (`cratonvm-es-floatbuffer-baseline`, built from `origin/dev` before this
  session's changes) — not introduced by the FloatBuffer fix.
- Given the timing (same-day as `1c60eb0ffa`) and that the affected code
  path is exactly the vector-scoring hot loop the sliced-IVF fix targeted,
  this is the leading suspect for both symptoms, but not confirmed by
  bisection in this session.

## Severity

High: this blocks essentially the entire ES vector-codec test family from
passing under the CLI's default (JIT-on) mode, even though the interpreter
path only has the (unrelated, likely pre-existing) 0.0-score bug.

## Repro

```bash
# Azure host, ES checkout+testcp already staged at:
ESDIR=/data/data/es-jit-deopt-gc-bundle-20260708-214648/elasticsearch
CP=$(cat "$ESDIR/server/build/craton-testcp.txt" | tr -d '\n')
cd "$ESDIR"
# JIT on (default) -> SIGSEGV:
<cratonvm-bin> --java-home /home/victor/jdk25 --stack-dump-on-timeout 0 --Xmx 2g \
  -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home="$ESDIR" \
  <further -Dtests.*/--add-opens flags, see run-elasticsearch-suite.ps1 Get-EsJavaArgs> \
  -cp "$CP" org.junit.runner.JUnitCore \
  org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940v1DiskBBQVectorsFormatTests
# --nojit -> completes, 9/52 failures, all "expected X but was 0.0":
<cratonvm-bin> --nojit ... (same args)
```

## Evidence

- Verification artifacts (Azure host, may be cleaned up):
  `/data/data/es-nocode-verify-*/`, `/tmp/gdb-repro.out`.
- Binaries: `/data/data/wt-es-floatbuffer-nocode-20260710/target/release/cratonvm-es-floatbuffer-{baseline,fix,fix2}`.
