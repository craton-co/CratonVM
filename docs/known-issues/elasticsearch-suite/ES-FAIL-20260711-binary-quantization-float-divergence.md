# ES failure - binary quantization float divergence

Status: OPEN

Date observed: 2026-07-11

## Current-dev evidence

Focused probe against current `dev` commit
`d274d898c43a4ca07ac877ba85543d153d2ea83c`, built as
`cratonvm-es-focused-currentdev-20260711-172542`:

| VM mode | Class result |
| --- | --- |
| HotSpot | PASS, 8 tests, 0 failures |
| CratonVM JIT on | FAIL, 8 tests, 1 failure |
| CratonVM JIT off | FAIL, 8 tests, 1 failure |

Class: `org.elasticsearch.index.codec.vectors.es816.BinaryQuantizationTests`
(`others` index 1344). The sole divergent test is
`testQuantizeForQueryCosine`:

```text
java.lang.AssertionError: expected:<-0.086002514> but was:<-0.0860025>
```

## Scope

This is a deterministic floating-point result mismatch, not a JIT-only
miscompile: the exact assertion reproduces with the interpreter. It is also
separate from vector-file footer corruption because this focused test has no
Lucene persistence failure.

## Repro

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 \
  -Category others -Start 1344 -Count 1 -Vm craton -Jit on -TimeoutSec 120 \
  -ElasticsearchRoot <compiled-elasticsearch> -RefCsv <compiled-elasticsearch>/cratonvm-suite/results.jit.all.tsv \
  -Exe <cratonvm-es-focused-currentdev-20260711-172542> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64
```

The HotSpot control passes. Repeating with `-Jit off` retains the same one
failure and the same values.

## Next step

Reduce `testQuantizeForQueryCosine` to the first intermediate value that
differs from HotSpot. Check float conversion, narrowing, division, and math
helper semantics before changing vector-codec code; the current evidence is
not enough to attribute the error to any one operation.
