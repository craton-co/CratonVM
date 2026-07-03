# JIT inline-alloc header corruption under Hibernate batch workloads (OOM) — OPEN

**Status:** OPEN — root cause not isolated, only characterized.
**Discovered:** 2026-07-03, while chasing HIB-CV-38 (see
[`docs/internal/hibernate-bugs/HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md`](../internal/hibernate-bugs/HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md)
for the unrelated bug that doc was originally filed for — that one is fixed).

## Symptom

`org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest.testMultiLoad`
(2000-row batch insert, then a 2000-id multi-load) run with JIT on, real-JDK,
`-Xmx 1500m`, via `apps/hib-suite-runner`'s `CratonRunner` (dev `62f1f39a` +
the HIB-CV-38 Boolean fix):

```
GC: inconsistent header — kind=Object but array_length=1 (num_slots=1715269, class_id=0);
  inline-alloc forgot to set kind=Array. Treating as corrupt so the walker can re-sync.
GC: skipping suspected false root at 0x27e97018 (...)
non-moving sweep: no free-block anchor remains after offset 4010576 — abandoning rest of arena (389205424 bytes retained)
java.lang.OutOfMemoryError: Java heap space
```

5/6 soak runs (JIT on) hit this and OOM after 350-550 seconds. The 6th got
further and hit an unrelated `ConstraintViolationException` (likely stale H2
state from a previous run's incomplete `@AfterEach` truncate after an earlier
crash, not itself evidence of a distinct bug).

With `--nojit`, the corruption/OOM does not occur: `testDynamicBatchFetch`
passes (`ok=1`) and `testMultiLoad` instead hits a plain 120s JUnit
`@Timeout` (interpreter is simply slow for a 2000-iteration persist loop) —
no GC warnings, no corruption. This confirms the corruption is JIT-inline-alloc
specific, not present in the interpreter path.

## Why this is a new/open issue, not a reopening

The warning text (`kind=Object but array_length=N ...; inline-alloc forgot to
set kind=Array`) is the exact signature documented and marked **FIXED** in
`docs/internal/app-jvm-bugs/jit-bintrees18-inline-alloc-and-bc-ec-round2.md`
(BUG 1): a publish-before-initialize race in `emit_inline_tlab_new`
(`jit/src/x64.rs`), where the TLAB cursor commit was ordered before the
object header was fully written, letting a concurrent heap walk observe a
TLAB-zeroed header. That fix (write the full header before the cursor commit)
**is present on current dev** — confirmed by reading `emit_inline_tlab_new`
directly — and `bintrees18` itself is checksummed correct
(`docs/internal/gaps/gap-bintrees18-gc-throughput.md`).

So this is a **different trigger of the same corruption family**, not a
regression of the bintrees18 fix. Candidates not yet distinguished:

1. A separate inline-allocation fast path (e.g. for arrays specifically —
   `anewarray`/`newarray`, or the `compact_ref_fields` branch inside
   `emit_inline_tlab_new` itself) that has an analogous
   publish-before-initialize ordering bug not covered by the bintrees18 fix
   (which was analyzed against plain 2-field `Node` objects, never arrays).
2. bintrees18 allocates one uniform object shape at high frequency; Hibernate's
   batch path allocates a heavy *mix* of shapes/sizes (Strings, JDBC parameter
   arrays, HashMap/collection backing arrays, entity instances) — possibly
   exposing a race that needs that heterogeneity (e.g. TLAB-boundary or
   GC-timing interaction the uniform-shape bintrees18 workload doesn't hit).
3. `class_id=0` in most of the observed warnings is consistent with the
   walker genuinely reading a TLAB-zeroed header (the exact pre-fix
   bintrees18 signature) — suggesting the SAME race, just via a code path the
   prior fix didn't reach, rather than a new mechanism.

## Repro

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.trace=1 -Dcraton.batch=1 CratonRunner "lst_cv37_batch.txt" 0
```

(`lst_cv37_batch.txt` contains just `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest`.)
Needs a binary built with the HIB-CV-38 Boolean fix — without it, this run
fails earlier at JUnit-launcher bootstrap before ever reaching `testMultiLoad`.

## Next steps

- Check whether array allocation (`anewarray`/`newarray`) has its own inline
  TLAB fast path distinct from `emit_inline_tlab_new`, or whether it always
  falls through to a helper — if the latter, look at the `compact_ref_fields`
  branch inside `emit_inline_tlab_new` for the same header-vs-commit ordering
  bug bintrees18 had.
- A larger `-Xmx` (per the HIB-CV-37 truth table pattern, where a big heap
  suppressed a similar symptom) would help distinguish "genuine leak from
  abandoned arena regions" vs. "this workload's live set is just larger than
  1500m" — not yet tried.
- `CRATONVM_JIT_DISABLE_INLINE_NEW=1` (the exact toggle that isolated BUG 1 in
  the bintrees18 doc) on this repro would confirm/deny inline-`new` as the
  source without fully disabling JIT (which also disables inline arrays, if
  those are a separate path) — not yet tried.
