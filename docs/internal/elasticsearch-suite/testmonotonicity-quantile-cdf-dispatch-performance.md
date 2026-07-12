# ES libs/tdigest `TDigestTests.testMonotonicity` dispatch performance — fixed

Status: FIXED (2026-07-11)

`SortingDigest` routes its private `Dist.quantile`/`Dist.cdf` numeric kernels
through an erased `Function<Integer, Double>` adapter. The VM repeatedly
resolved that dynamic interface call and built wrapper/frame state for each
array lookup. This made a single monotonicity sweep take ~948 seconds with
JIT enabled and fail to finish in a one-hour interpreter soak.

The dispatch path now caches receiver-specialized virtual/interface targets.
The TDigest private kernels scalarize their verified
`Integer.valueOf` → `Function.apply` → `Double.doubleValue` bytecode adapter,
and directly enter the cached compiled getter without creating a redundant
nested JIT root-chain entry. The interpreter recognizes the same adapter only
when the lambda target is a verified trivial `(int) -> double` array getter;
all other lambdas retain ordinary Java dispatch.

Validation on Azure (`20.83.144.174`) with the dedicated binary
`/data/data/cratonvm-es-monotonicity-quantile-dispatch-20260711` and a
faithful 100,000-value / 10,001-point monotonicity probe:

- JIT on: `TDIGEST_MONOTONICITY_OK seconds=229.675`.
- `--nojit`: `TDIGEST_MONOTONICITY_OK seconds=315.838`.

Both runs completed with correct monotonicity, replacing the former ~948 s
JIT run and >1 hour no-JIT non-completion. The checked-in Elasticsearch runner
was also invoked for `SortingDigestTests`, but its available fixture lacks
`libs/tdigest`'s `craton-testcp.txt`; it reported `NOCP` before a JVM could
start, so the exact runtime probe is the decisive validation artifact.
