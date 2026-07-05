# Elasticsearch no-JIT partial suite bug sweep

Status: open

Date observed: 2026-07-02

## Summary

A CratonVM no-JIT Elasticsearch suite run was started to collect more bug
information after the JIT-on sweep. The run was stopped by request after 1366
recorded classes, so this is a partial sweep, not a complete suite result.

Command shape:

```text
Vm=craton
Jit=off
Category=all
Start=1
Count=0
Parallel=16
TimeoutSec=300
RunName=es-nojit-full-20260702
```

Unique binary:

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\target\release\cratonvm-elasticsearch-nojit-suite-20260702.exe
```

## Partial result

At stop time:

```text
classes recorded: 1366
PASS: 29
FAIL: 1327
HANG: 10
CRASH: 0
```

The last recorded row was:

```text
index=1366
class=org.elasticsearch.index.codec.vectors.es93.ES93BinaryQuantizedVectorsFormatTests
status=FAIL
seconds=72.306
```

## Main signatures in the partial run

Compared with the HotSpot baseline:

```text
1140 XContentProvider ModuleDescriptor.uses null failures where HotSpot passed
99   UnmodifiableSet sorted/navigable contract failures where HotSpot passed
10   SegmentVarHandle MemorySegment access failures where HotSpot passed
6    codec/doc-values/postings hangs where HotSpot passed
4    Byte Buddy AnnotatedType proxy failures where HotSpot passed
1    RestClient RequestOptions header-list mismatch where HotSpot passed
1    RestClient retry host identity mismatch where HotSpot passed
1    RestClient wrong-endpoint NodeSelector NPE where HotSpot passed
1    RandomizedTesting suite timeout where HotSpot passed
1    MappingStatsTests Object-to-Writeable cast failure where HotSpot passed
1    TSDBStoredFieldsFormatTests no-JIT hang where HotSpot passed
```

Additional overlapping failures still carry useful VM information:

```text
2 LoggerFactory.provider() null log files in vectorization init (RESOLVED —
  not a CratonVM bug; HotSpot failed identically. Root cause and fix in
  ../internal/elasticsearch-suite/elasticsearch-loggerfactory-provider-null.md)
9 Buffer.isReadOnly() has no Code attribute log files
2 TSDB doc-values classes that crashed in JIT-on mode hung in no-JIT mode
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```

## 2026-07-05 follow-up: ProviderLocator / module descriptor path

Targeted the largest deterministic signature from the partial no-JIT sweep: `XContentProvider ModuleDescriptor.uses null` through Elasticsearch's provider/module loading path.

Findings on current `dev` before this patch:

- A focused module-layer probe failed before descriptor access because `ModuleLayer.boot().configuration()` returned a synthetic `java.lang.module.Configuration` whose private `nameToModule` cache was null. Real JDK `Configuration.findModule` dereferenced it during `Configuration.resolve`, producing `NullPointerException: Cannot invoke "java.util.Map.get(Object)" because "this.nameToModule" is null`.
- After initializing the synthetic configuration caches, the same path reached module-info parsing, where JDK `ModuleDescriptor.read` rejected even Java 9 `module-info.class` with `InvalidModuleDescriptorException: Unsupported major.minor version 53.0`. Root cause: `jdk.internal.misc.VM.isSupportedModuleDescriptorVersion(int,int)` and `isSupportedClassFileVersion(int,int)` real bytecode read an uninitialized `classFileMajorVersion` static, so every module descriptor version was rejected.

Patch applied:

- `native-builtins/src/jboss_jdkspecific.rs`: `ModuleLayer.configuration()` now seeds `parents`, `graph`, `modules`, and `nameToModule` with initialized empty JDK collections, so real `Configuration.findModule` returns `Optional.empty()` instead of NPEing on null caches.
- `native-builtins/src/lib.rs`: registered native overrides for `jdk/internal/misc/VM.isSupportedClassFileVersion(II)Z` and `isSupportedModuleDescriptorVersion(II)Z`, using the VM's advertised Java 25 classfile ceiling (`major 69`) and the JDK minor-version rules.

Validation with `/data/target-es-nojit-partial-next-20260705/release/cratonvm-es-nojit-partial-next-azure-20260705`:

```text
ModuleFixesProbe OK findModule=Optional.empty descriptor=x.foo.impl uses=[java.util.function.IntSupplier]
```

Residual, still open:

A closer Elasticsearch `ProviderLocator` embedded-module probe now gets past both prior failures but stops at the next JPMS modeling gap:

```text
java.lang.module.FindException: Module java.base not found, required by x.foo.impl
```

So this note remains in `docs/known-issues`: the deterministic module descriptor/cache failures are fixed, but the Elasticsearch provider module path still needs a synthetic boot configuration that can satisfy at least `java.base` resolution before the original no-JIT roll-up can be retired.
