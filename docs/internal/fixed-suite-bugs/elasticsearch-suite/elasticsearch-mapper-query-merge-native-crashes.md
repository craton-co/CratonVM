# Elasticsearch mapper/query/merge native crashes

Status: FIXED (2026-07-04) — see Resolution below. Moved to `..`.

Date observed: 2026-07-02

## Resolution (2026-07-04)

Root cause: `cratonvm/internal/UnmodifiableSet` — the synthetic wrapper class
backing `Collections.unmodifiableSet`/`unmodifiableSortedSet`/
`unmodifiableNavigableSet` — had **no NavigableSet/SortedSet methods
registered at all** (`tailSet`, `headSet`, `subSet`, `first`, `last`,
`ceiling`/`floor`/`higher`/`lower`, `descendingSet`, `descendingIterator`,
`pollFirst`/`pollLast`), and did not declare implementing `SortedSet`/
`NavigableSet` in its synthetic class stamp. Elasticsearch's
`KnownIndexVersions.<clinit>` (in `test/framework`, touched by any test that
uses `IndexVersionUtils`) runs:

```java
Collections.unmodifiableNavigableSet(new TreeSet<>(...)).tailSet(MINIMUM_COMPATIBLE, true)
```

which raised `NoSuchMethodError`, aborting `<clinit>` with
`ExceptionInInitializerError` for every caller — cascading into
`NoClassDefFoundError`/`ClassCastException`/null-`serviceHolder` failures
across the whole mapper/query/merge-scheduler test family (this shared
`test/framework` utility, not the individual test classes, is why the
crashes clustered the way they did). On the 2026-07-02 dev tip this manifested
as the documented `0xC0000409` hard crash; by 2026-07-04 an unrelated
intervening dev fix had already made the crash itself unreproducible (the
class list here doesn't have any `0xC0000409` reproductions with a fresh
`release-with-debug` build even before the fix below) — but the missing
`tailSet`/NavigableSet surface itself was still broken and still blocking
correct test execution, so it needed fixing regardless.

Fix (branch `fix/es-mapper-query-merge-native-crashes-20260704`):
1. `../../../../native-collections/src/lib.rs` — register the full NavigableSet/SortedSet
   read surface (`first`/`last`/`comparator`/`ceiling`/`floor`/`higher`/
   `lower`/`descendingIterator`/`pollFirst`/`pollLast`/`descendingSet`/
   `headSet`/`tailSet`/`subSet`, both inclusive-flag and legacy overloads) on
   `UNMOD_SET_CLASS`, mirroring the pattern already used for
   `UnmodifiableMap`'s NavigableMap surface. Sub-view results (`tailSet`,
   `headSet`, `subSet`, `descendingSet`) are re-wrapped unmodifiable, matching
   the JDK's `UnmodifiableNavigableSet` contract.
2. `../../../../vm/src/vm/vm_init.rs` — declare `java/util/SortedSet` and
   `java/util/NavigableSet` as interfaces of the `UnmodifiableSet` synthetic
   class stamp (needed once callers actually navigate the returned wrapper —
   without it, a `(NavigableSet)`/`(SortedSet)` checkcast on the wrapper threw
   `ClassCastException`).
3. `../../../../vm/src/runtime/crash_handler.rs` — the Windows VEH's fatal-fault list was
   missing `STATUS_STACK_BUFFER_OVERRUN` (`0xC0000409`, the `/GS`-cookie
   fastfail this bug's crash signature used), so this whole crash family
   produced empty stderr with no diagnostic. Added it to the fatal list so any
   future recurrence of this exception code gets a symbolized backtrace
   instead of a bare Windows Application Error entry.

Verified: all originally-crashing classes (`TypeParsersTests`, `UidTests`,
`ConstantScoreQueryBuilderTests`, `MergeSchedulerSettingsTests`,
`MergePolicyConfigTests`, `BinaryDenseVectorScriptDocValuesTests`,
`BoolQueryBuilderTests`, etc.) now run to completion with real
pass/fail results instead of `NoSuchMethodError`/`ClassCastException`
cascades or hard crashes, under both `-Parallel 1` and `-Parallel 4`.

**Residual, unrelated crash found during verification**: 5 classes in this
same range (`UpdateMappingTests`, `TsidExtractingIdFieldMapperTests`,
`CombineIntervalsSourceProviderTests`, `DisjunctionIntervalsSourceProviderTests`,
`FilterIntervalsSourceProviderTests`) still crash, but with `0xC0000005`
(`EXCEPTION_ACCESS_VIOLATION`) inside JIT/deopt/GC bookkeeping code — a
different, previously-unknown bug, deterministic even under `-Parallel 1`.
Tracked separately: `../jit-deopt-gc-heap-corruption-server-tests.md` (stays in
`../../../known-issues`, NOT fixed by this change).

## Summary

The current full Elasticsearch suite has a cluster of native CratonVM process
crashes in mapper, query, and merge-configuration tests. These are runner
`CRASH` rows, not JUnit failures.

Windows Application Error entries for the same binary show:

```text
Exception code: 0xc0000409
Fault offset: 0x0000000000e1b458
Faulting application path:
C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 32 CratonVM crashes in this family.
- 29 are CratonVM-only: HotSpot passed the same classes.
- 3 overlap HotSpot baseline failures.

Representative row:

```text
index=1686
module=server
class=org.elasticsearch.index.mapper.TextFieldAnalyzerModeTests
CratonVM=CRASH, 34.420s
HotSpot=PASS, 15.912s
```

Examples:

```text
org.elasticsearch.index.mapper.TypeParsersTests
org.elasticsearch.index.mapper.UidTests
org.elasticsearch.index.mapper.UpdateMappingTests
org.elasticsearch.index.mapper.ValuesWithOffsetsDocValuesLoaderTests
org.elasticsearch.index.mapper.vectors.BinaryDenseVectorScriptDocValuesTests
org.elasticsearch.index.query.ConstantScoreQueryBuilderTests
org.elasticsearch.index.query.CombinedFieldsQueryParsingTests
org.elasticsearch.index.MergeSchedulerSettingsTests
org.elasticsearch.index.MergePolicyConfigTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1686 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-mapper-native-crash-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.query.ConstantScoreQueryBuilderTests.out.log
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.query.ConstantScoreQueryBuilderTests.err.log
Windows Application log, Application Error source, 2026-07-02 around 15:02 local time
```
