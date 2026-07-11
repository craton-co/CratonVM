# ES failure family - foreign SegmentAllocator allocate dispatch

Status: OPEN

Date observed: 2026-07-11

## Current-dev evidence

Focused probe against current `dev` commit
`d274d898c43a4ca07ac877ba85543d153d2ea83c`, built as
`cratonvm-es-focused-currentdev-20260711-172542`:

| VM mode | Class result |
| --- | --- |
| HotSpot | PASS, 17 tests, 0 failures |
| CratonVM JIT on | FAIL, 0 tests, 2 bootstrap failures |
| CratonVM JIT off | FAIL, 0 tests, 2 bootstrap failures |

Representative class: `org.elasticsearch.core.FastMathTests` (`others`
index 13 in the compiled Elasticsearch fixture).

The first divergent exception is:

```text
java.lang.AbstractMethodError: method
java/lang/foreign/SegmentAllocator.allocate(JJ)Ljava/lang/foreign/MemorySegment;
has no Code attribute
```

Its stack begins at `SegmentAllocator.java:318`, called while Elasticsearch
initializes `JdkPosixCLibrary`, then `NativeAccessHolder` and
`BootstrapForTesting`. The bootstrap continues with native access disabled,
and the class later fails with downstream `NoClassDefFoundError`s. Those
downstream errors are consequences, not independent missing-class bugs.

## Diagnosis

`SegmentAllocator.allocate(long, long)` is an interface dispatch point in
the real JDK foreign-memory API. CratonVM is selecting the no-Code interface
declaration instead of the concrete allocator implementation, then attempts
to execute it. The failure is independent of the JIT and is distinct from
the already-fixed `MemoryLayout.varHandle` / `SegmentVarHandle` work.

## Repro

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 \
  -Category others -Start 13 -Count 1 -Vm craton -Jit on -TimeoutSec 120 \
  -ElasticsearchRoot <compiled-elasticsearch> -RefCsv <compiled-elasticsearch>/cratonvm-suite/results.jit.all.tsv \
  -Exe <cratonvm-es-focused-currentdev-20260711-172542> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64
```

Run again with `-Jit off`; the same abstract-method error must occur. The
HotSpot control passes with the same compiled fixture.

## Next step

Audit real-JDK interface method resolution for `SegmentAllocator` and add a
minimal `Arena` / `SegmentAllocator.allocate(long, long)` regression probe.
The fix must dispatch to the concrete allocator implementation rather than
providing a synthetic result for the abstract interface declaration.
