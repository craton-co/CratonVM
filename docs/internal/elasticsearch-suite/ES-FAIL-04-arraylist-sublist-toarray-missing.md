# ES-FAIL-04 — synthetic `cratonvm/internal/ArrayListSubList` missing `toArray(T[])`

**Status:** OPEN
**Severity:** LOW — 7 `:server` search/sort test classes; deterministic; a missing synthetic-method overload, not a hard crash.
**VM:** `cratonvm.exe` from `dev` @ 16b69363. **Baseline:** HotSpot JDK 25.0.1 runs them fine.
**Date:** 2026-06-18

## Symptom

```
NoSuchMethodError method="cratonvm/internal/ArrayListSubList.toArray([Ljava/lang/Object;)[Ljava/lang/Object;"
Thread Thread-1 terminated with error: InternalError(Linkage(NoSuchMethodError { ... ArrayListSubList ... toArray ... }))
linkage error: no such method: cratonvm/internal/ArrayListSubList.toArray([Ljava/lang/Object;)[Ljava/lang/Object;
```

CratonVM's **synthetic `ArrayList.subList()` view** class (`cratonvm/internal/ArrayListSubList`) implements `toArray()` but **not the generic `toArray(T[])` overload**. Code that does `list.subList(a, b).toArray(new T[n])` throws `NoSuchMethodError`.

## Affected classes (deterministic, isolated re-run, `--nojit`, rc=1)
- `org.elasticsearch.search.slice.TermsSliceQueryTests`
- `org.elasticsearch.search.sort.BucketedSortForDoublesTests`
- `org.elasticsearch.search.sort.BucketedSortForFloatsTests`
- `org.elasticsearch.search.sort.BucketedSortForIntsTests`
- `org.elasticsearch.search.sort.BucketedSortForLongsTests`
- `org.elasticsearch.search.sort.FieldSortBuilderTests`
- `org.elasticsearch.search.sort.GeoDistanceSortBuilderTests`

## Note on the suite numbers (load artifact)

In the parallel suite run these 7 were recorded as **CRASH (rc=127)** with only the `JUnit version` banner printed. On isolated re-run (×4, fixed binary) they are **deterministically rc=1 with the `ArrayListSubList.toArray(T[])` `NoSuchMethodError`** — i.e. the silent `rc=127` was an artifact of the ~28-way parallel load on the box (process died early under contention), not a genuine hard crash. There were **no reproducible hard crashes (SIGSEGV/panic) in the whole suite** — every CratonVM failure is a hang (ES-HANG-01/02) or a wrong-behavior linkage/exception issue (ES-FAIL-03/04). The load-sensitive `rc=127` early exit is itself worth a glance (robustness under many concurrent VMs) but is secondary.

## Reproduce
```bash
export CLASSPATH="$(cat server/build/craton-testcp.txt)"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
cratonvm.exe --nojit -Djava.awt.headless=true -Dtests.asserts=false --Xmx 2g \
  --add-opens=java.base/java.util=ALL-UNNAMED \
  org.junit.runner.JUnitCore org.elasticsearch.search.sort.BucketedSortForLongsTests
```

## Fix vs handoff
**Handoff** — add the `toArray([Ljava/lang/Object;)[Ljava/lang/Object;` overload to the synthetic `cratonvm/internal/ArrayListSubList` (the class is generated at runtime — grep the VM for where `cratonvm/internal/ArrayListSubList` / the `ArrayList.subList` synthetic view is built and register `toArray(T[])` alongside the existing `toArray()`/`size`/`get`). Small, self-contained. Independent of ES-HANG-01.
