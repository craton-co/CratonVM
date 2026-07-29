# `OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb`: genuine `OutOfMemoryError` parsing a 3MB+ YAML file

**Status: OPEN — found 2026-07-28**, `craton-rerun-20260728` (1500s-timeout
rerun of the 2026-07-23 residual, see
`apps/spring-boot-suite-runner/RESULTS-20260728.md`). Was `FAIL` on
07-23, `CRASH` (fatal `OutOfMemoryError`, unhandled by the test itself) on
this rerun.

## Symptom

**Module:** `core/spring-boot`

```
Exception in thread "main" java/lang/OutOfMemoryError: Java heap space (alloc_array length 1026)
```

Stack (from the `.err.log`, real SnakeYAML bytecode, not a CratonVM
internal path):

```
at java/util/Arrays.copyOfRange(Arrays.java:3929)
at org/yaml/snakeyaml/reader/StreamReader.update(StreamReader.java:182)
at org/yaml/snakeyaml/scanner/ScannerImpl.scanPlain(ScannerImpl.java:2059)
at org/yaml/snakeyaml/scanner/ScannerImpl.fetchPlain(ScannerImpl.java:1089)
at org/yaml/snakeyaml/scanner/ScannerImpl.fetchMoreTokens(ScannerImpl.java:447)
at org/yaml/snakeyaml/parser/ParserImpl$ParseBlockSequenceEntryKey.produce(ParserImpl.java:543)
... (SnakeYAML composer/constructor chain)
at org/springframework/boot/env/OriginTrackedYamlLoader.load(OriginTrackedYamlLoader.java:85)
at org/springframework/boot/env/OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb(OriginTrackedYamlLoaderTests.java:247)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/core_spring-boot.org.springframework.boot.env.OriginTrackedYamlLoaderTests.err.log`

## Relationship to the original 2026-07-11 dismissal

This exact class was flagged in the very first suite run
(`RESULTS-20260711.md`, "HANG rerun" section) as a `NoClassDefFoundError:
junit-platform-commons/ExceptionUtils` **CRASH**, dismissed at the time as
"a runner classpath artifact (pathing-jar manifest truncation suspected),
not a CratonVM bug — not filed." **That symptom is gone.** This run's
crash has a completely different signature (a real `OutOfMemoryError` from
real SnakeYAML bytecode, no `NoClassDefFoundError` anywhere in this log) —
the original dismissal doesn't apply here and should not be assumed to
cover this new finding.

## Root cause — not confirmed

The allocation that fails (`alloc_array length 1026` — roughly a 1KB-2KB
array, depending on element type) is tiny relative to the default 2GB
(`-MaxHeap 2g`) runner heap and relative to what real HotSpot needs to
parse the same file (the test's own name promises the fixture is "bigger
than 3Mb" but real JDK handles it without incident — this is a
CratonVM-specific resource exhaustion, not an inherently oversized
workload). `StreamReader.update()` is SnakeYAML's rolling-buffer growth
path, called very frequently while scanning a large plain-scalar YAML
node — i.e. this OOM is reached after many small array allocations
accumulate, not from one giant allocation. Two candidate explanations,
neither traced to source this session:

1. A genuine CratonVM heap-accounting or GC-reclamation bug causes garbage
   from this alloc-heavy scanning loop to not be collected promptly enough
   (a leak or premature/failed collection), so the heap fills up on
   workloads that stress small-object churn even though the live-set stays
   small.
2. CratonVM's per-object/per-array overhead is high enough (or its
   GC's effective usable-heap fraction is low enough) that a workload
   real HotSpot handles in well under 2GB genuinely exceeds it under
   CratonVM — a capacity/overhead gap rather than a correctness bug per
   se, in which case the practical fix might be a bigger default heap for
   this class of workload rather than a VM defect.

Needs a heap profile / allocation-rate trace during a live repro to
distinguish a leak from genuine overhead before proposing a fix direction.

## Affected classes

- `core/spring-boot` | `org.springframework.boot.env.OriginTrackedYamlLoaderTests` (`canLoadFilesBiggerThan3Mb`)
