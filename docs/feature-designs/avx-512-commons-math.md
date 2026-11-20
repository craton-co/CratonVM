# AVX-512 numerical kernels and testing defaults

Status: testing implementation; AVX-512 paths default on for testing, subject to CPU and runtime vetoes. The testing profile enables all
AVX-512 paths by default on supported CPU/OS configurations; it is not a
throughput promotion claim.

## Shared JIT paths

The implementation operates on the real Java bytecode's IR, with no application
class-name substitutions or native registrations. `ir_vector_loop/avx512.rs`
adds 512-bit independent expressions over `double`, `float`, `int`, and selected
`long` operations. Two 64-byte steps replace four 32-byte steps per main trip,
keeping the main-loop work and poll interval bounded. EVEX memory operands use
unscaled disp32, including VM array headers. Existing null, length, induction,
alias, scalar-tail, and safepoint guards remain in the lowering.

Forward FP add/sub reductions compute independent expressions in vector lanes,
spill one bounded 64-byte batch, then apply scalar additions/subtractions in
source order to the original seed. There is no horizontal sum, reassociation,
or FMA. The 64-byte scratch span contains no call or poll and restores RSP
before every loop boundary. Reversed-operand accumulators and unsupported
widening, scoped, scan, iota, or expression forms retain their scalar/AVX2 path.

The restricted field-read hoist additionally admits loops that write only
primitive array elements. Such writes cannot alter an instance field. Calls,
allocations, field writes, reference-array stores, volatile/ordered accesses,
and the existing null/deopt/guard restrictions still refuse the hoist. This
makes an unchanged mantissa field an invariant array for Dfp's complement loop.

## Family audit

| Requested family | Applicable pattern and sequential contract |
| --- | --- |
| Dfp decimal floating point | Independent `complement` mantissa transform; radix carry propagation keeps Java order. |
| DfpMath high-precision math | Composed Dfp arithmetic inherits eligible mantissa transforms; high-precision series and convergence remain the library's decimal algorithms. |
| DfpDec decimal precision | Inherits eligible Dfp transforms; decimal rounding, precision and traps retain their existing order. |
| MathArrays | Elementwise scale/add/subtract/multiply/divide and eligible forward FP reductions; compensated multi-accumulator algorithms retain their original structure. |
| IntegerSequence | Scalar stepping, termination, overflow and callbacks have no independent SIMD batch; tested without inventing a duplicate-lane optimization. |
| LU | Contiguous row/RHS updates; indirect column reads in factorization require the original memory access sequence. |
| QR | Forward norms/dot products and Householder row updates. |
| Cholesky | Contiguous trailing-row and RHS updates. |
| EigenDecomposition | Eligible contiguous expressions and ordered reductions; indirect column transforms and convergence control retain their structure. |
| Kohonen SOM | Contiguous feature updates and eligible Euclidean-distance reductions; neuron selection and CAS retry remain ordered. |

These are eligibility statements, not a claim that every method in a family
vectorizes. The maintained gate records actual 512-bit emission labels per
family so missing admission remains visible.

## Default-on controls for testing

The following controls default ON (individual `=0` disables each):

* `CRATONVM_JIT=avx512-vector-loop=0`: disable new 512-bit IR steps.
* `CRATONVM_JIT=avx512-array-field-hoist=0`: disable the alias-safe field preparation.
* `CRATONVM_JIT=ir-vector-ordered-fp=0`: disable new ordered FP reductions.
* `CRATONVM_JIT=avx512-int-sum=0`: disable the earlier int-array sum path.
* `CRATONVM_JIT=avx512-rsa=0,avx512-ec=0,avx512-aes=0`: disable earlier crypto paths.
* `CRATONVM_JIT=avx512=0`: master veto, including existing string/sieve paths.

Feature checks remain mandatory. AVX-512F suffices for admitted numerical
steps; crypto retains its IFMA/VAES checks and native UTF-16 scans require BW.

## Maintained gate

`tools/probes/avx512-numerical/run.py <vm> --work-dir <owned-directory> --coverage`
uses the host's built Commons Math checkout and Maven/Gradle test dependencies
(`--cm-root` selects another checkout). It compiles the retained probes freshly,
runs the ten upstream JUnit classes and deterministic API probes under HotSpot
25, then both CratonVM modes with defaults, master veto, and only numerical
paths vetoed. It compares full stdout, not class names or successful exits alone.
The API probes hash all matrix/feature outputs and decimal strings, and include
cancellation-sensitive ordered sums and vector-boundary lengths. Diagnostic runs
record emitted 512-bit labels separately from timing. Timings are observations
of these processes; a performance claim requires separate steady-state pricing.

The gate retains its classpath, per-case stdout/stderr, results and binary/source
manifest for review. Its owned artifacts should be removed after delivery.
