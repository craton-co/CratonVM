# Elasticsearch vector tests missing VectorSupport.registerNatives

Status: fixed

Date observed: 2026-07-02

Date fixed: 2026-07-02

## Summary

A pre-fix Elasticsearch vector slice failed under CratonVM because the JDK
incubator vector API reached missing real-JDK native registrations:

```text
jdk/internal/vm/vector/VectorSupport.registerNatives()I
jdk/internal/vm/vector/VectorSupport.getCPUFeatures()Ljava/lang/String;
jdk/internal/vm/vector/VectorSupport.getMaxLaneCount(Ljava/lang/Class;)I
```

The fix added the JDK 25 vector native surface to the real-JDK essential native
path. This specific `VectorSupport.registerNatives()` signature no longer
appears in the current full Elasticsearch suite run.

## Verification

Focused native registration tests:

```powershell
cargo test -p cratonvm-native-builtins register_essential_includes_jdk25_vector_support_natives
cargo test -p cratonvm-native-builtins vector_api_tests::test_vector_support_jdk25_natives_registered
```

Current Elasticsearch verification:

```text
Run: es-current-full-jiton-20260702
CratonVM binary: C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
Result: no failures containing VectorSupport.registerNatives()
```

## Historical evidence

The stale pre-fix run remains useful only as history:

```text
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.ES813Int8FlatVectorFormatTests.err.log
```
