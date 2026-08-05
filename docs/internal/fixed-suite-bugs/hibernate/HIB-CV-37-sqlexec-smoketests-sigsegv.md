# HIB-CV-37 - `sql.exec.SmokeTests` native-callback GC crash

**Status:** CLOSED as a native-collections GC-root bug on 2026-07-01.
**Original mode:** real-JDK, JIT on. HotSpot passed.
**Original symptom:** `org.hibernate.orm.test.sql.exec.SmokeTests` crashed with rc=139
inside `testQueryConcurrency`.

**2026-08-04 note (corrected later the same day):** this exact test crashed
again, same class/method, different mechanism. The first write-up called it a
register-invisible-root stale pointer (the "Layer 1" DoHead family); that was
refuted by its own log — the reclamation verdict was `young TO-space (the
inactive semispace)` and the unconditional young-sweep ring had no record, so
it is the MOVING collector's blocked-thread root path, not the non-moving
sweep. See `smoketests-stale-pointer-nosuchmethod-crash-20260804-RETIRED.md`
(same directory). Consistent with this doc's own "Verification" section below,
which already flagged that closure was never re-verified end to end against the
real test — that doesn't retroactively invalidate the fixes recorded here (they
address a real, different bug), it just means they were never sufficient to
guarantee this class crash-free.

## Closure note

This was left too broad in `docs/known-issues`: the original note bundled a
GC crash in `SmokeTests` with a separate `DynamicBatchFetchTest` JDBC binding
failure. Later triage in `HIB-misc16-correctness-sweep.md` says the
`DynamicBatchFetchTest` parameter-binding failure appears fixed, and its
remaining `testMultiLoad` symptom is a throughput timeout, not the HIB-CV-37
GC crash. HIB-CV-37 is therefore archived here, and any fresh Hibernate SQL
execution failure should get a new, narrow known-issue doc with a current repro.

## Why there were multiple HIB-CV-37 fixes

The crash signature was one suite symptom, but the root cause was a family of
native callback loops holding Java object refs in Rust locals across allocating
`ctx.invoke_virtual` calls. A moving young GC can relocate the callback, key,
value, or element and remap the native pin table, but it cannot rewrite a stale
Rust-local copy. The fix had to cover each native callback surface that could
drive the Hibernate path.

The follow-up commits were adjacent surfaces of the same native-local GC-root
family, not three independent fixes to one exact call site:

- stream / spliterator / base collection callbacks;
- additional collection callback loops (`LinkedHashMap`, `ArrayDeque`,
  `TreeMap`, `TreeSet`, `ConcurrentHashMap.forEach`, unmodifiable list iterator);
- map functional callbacks (`compute*`, `merge`, `replaceAll`);
- `ConcurrentHashMap` bulk callbacks (`forEachEntry`, `forEachKey`,
  `forEachValue`, `search`).

Each sweep added focused moving-GC regressions in
`native-collections/tests/gc_native_pins.rs`, and the full
`cratonvm-native-collections` package test suite passed after the final CHM
bulk sweep.

## Historical evidence

- The crashing test was `SmokeTests.testQueryConcurrency`: 5 worker threads,
  50 forks, 400 iterations, for 20,000 concurrent HQL queries.
- Crashes occurred on worker threads and varied by victim read site, consistent
  with a broad stale-ref corruptor rather than a single bad SQL operation.
- The truth table pointed at active moving GC: default heap crashed with and
  without JIT, while a large heap suppressed the SIGSEGV and changed the symptom.
- Native stream and collection intrinsics were confirmed to drive lambdas while
  holding callbacks and materialized element snapshots in unpinned locals.

## Verification

The current checkout used for the doc closure does not contain the historical
`apps/hib-suite-runner` harness, so this archival change did not rerun
`SmokeTests.testQueryConcurrency` end to end. Closure is based on the narrowed
root cause, the completed native callback pinning sweeps, and the focused
moving-GC regressions that exercise the stale-local failure mode directly.
