# JIT inline-alloc header corruption under Hibernate batch workloads (OOM) — FIXED

**Status:** FIXED — this was a shared GenerationalHeap allocation
publish-before-initialize race, not x64 inline allocation codegen.
**Discovered:** 2026-07-03, while chasing HIB-CV-38 (see
[`docs/internal/hibernate-bugs/HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md`](../hibernate-bugs/HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md)
for the unrelated bug that doc was originally filed for — that one is fixed).
**Also the likely explanation for HIB-CV-39** (a `DynamicBatchFetchTest` SIGSEGV
filed before the HIB-CV-38 fix landed): see
[`docs/internal/hibernate-bugs/HIB-CV-39-dynamicbatchfetch-sigsegv-regression.md`](../hibernate-bugs/HIB-CV-39-dynamicbatchfetch-sigsegv-regression.md)
for the closure reasoning — with HIB-CV-38 fixed, the identical repro no longer
SIGSEGVs and instead deterministically reproduces this bug.

## Resolution

Root cause: `gc/src/gen_heap.rs` reserved bytes from `young_from` under the
arena lock, released that lock, then zeroed the span and let callers write the
object/array header later. A concurrent GC/walker could acquire the same arena
lock in that window and observe a reserved but headerless allocation. That
matches the stale/zero header warning stream and also explains why
`CRATONVM_JIT_DISABLE_INLINE_NEW=1` did not suppress the bug: JIT helper calls
for `new`, `newarray`, and `anewarray` still use the shared heap allocation
path.

Fix: young generation allocation now uses
`try_alloc_young_initialized` / `alloc_young_initialized`, which reserve,
zero, and run the caller's header initializer while the `young_from` lock is
still held. The direct old-generation spill paths now also write headers while
holding the `old_gen` lock, closing the same publish-before-initialize shape
for major-GC walks.

Validation:

- `cargo test -p cratonvm-gc young_alloc_initializes_header_before_unlocking_arena`
  passed. The new regression test intentionally blocks inside the initializer
  and proves the young arena lock is still held until the header write can
  complete.
- `cargo test -p cratonvm-gc --lib` passed: 762 tests.
- `cargo build -p cratonvm-cli --bin cratonvm` passed, and the worktree binary
  was copied to `cratonvm-hib-inlinealloc-gc-20260703-001.exe`.
- The original `apps/hib-suite-runner` fixture and `common.args` file named in
  this note are not present in this checkout, so the Hibernate batch repro
  itself was not rerun here.

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

## Why this was a new issue, not a reopening

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
regression of the bintrees18 fix. Candidates (see "Additional evidence" for
which of these are now ruled out):

1. ~~A separate inline-allocation fast path for arrays~~ — **RULED OUT
   2026-07-03**: `newarray`/`anewarray` never had an inline TLAB path at all;
   they unconditionally call the `jit_newarray`/`jit_anewarray_object` helpers
   (confirmed by direct code read, `jit/src/x64.rs` ~23211-23385, opcodes
   0xbc/0xbd). There is no array-specific inline fast path to be buggy.
2. bintrees18 allocates one uniform object shape at high frequency; Hibernate's
   batch path allocates a heavy *mix* of shapes/sizes (Strings, JDBC parameter
   arrays, HashMap/collection backing arrays, entity instances) — possibly
   exposing a race that needs that heterogeneity (e.g. TLAB-boundary or
   GC-timing interaction the uniform-shape bintrees18 workload doesn't hit).
   **Resolved** — the heavy mixed allocation workload exposed the shared heap
   helper's publish-before-initialize window, not per-shape JIT allocation
   codegen.
3. `class_id=0` in most of the observed warnings is consistent with the
   walker genuinely reading a TLAB-zeroed header (the exact pre-fix
   bintrees18 signature) — suggesting the SAME race, just via a code path the
   prior fix didn't reach, rather than a new mechanism. **Confirmed**:
   the corrupted `class_id`/`num_slots`/`array_length` values vary run-to-run
   (0, 4, 2000, 786439952, ...) rather than being one fixed garbage pattern —
   consistent with reading genuinely uninitialized/stale memory. The source was
   the shared young/old allocation helpers publishing reserved spans before the
   header writes were complete.

## Repro

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.trace=1 -Dcraton.batch=1 CratonRunner "lst_cv37_batch.txt" 0
```

(`lst_cv37_batch.txt` contains just `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest`.)
Needs a binary built with the HIB-CV-38 Boolean fix — without it, this run
fails earlier at JUnit-launcher bootstrap before ever reaching `testMultiLoad`.

## Additional evidence (2026-07-03, `investigate/hib-cv-39-dynamicbatchfetch-sigsegv` branch)

Rebuilt `cratonvm-cli` from dev (post HIB-CV-38 merge, includes the `-XX:+UseG1GC`
concurrent-marking fix too — not relevant here since G1 is opt-in and this repro
doesn't pass `-XX:+UseG1GC`, so the default Generational/moving-young collector
is what's under test, same as bintrees18). Three isolated single-class reruns of
`DynamicBatchFetchTest` via `apps/hib-suite-runner`, `--java-home jdk-25`,
`@common.args -Dcraton.batch=1`:

1. **Baseline (JIT on, `--Xmx 1500m`)**: reproduced exactly as documented above —
   `inconsistent header` warning storm (`class_id=786439952`, `num_slots=2`,
   `array_length=2` this run) then `OutOfMemoryError` at `ms=553385`. **No
   SIGSEGV.**
2. **`CRATONVM_JIT_DISABLE_INLINE_NEW=1` (JIT still on, `--Xmx 1500m`)**: this
   disables `emit_inline_tlab_new` entirely, forcing every plain `new` through
   the `jit_new_object` helper — the exact toggle the old follow-up list asked
   for, and the same toggle that isolated the bintrees18 fix. **Did NOT
   suppress the corruption**: identical warning storm (`class_id=2000`,
   `num_slots=1180939`), `OutOfMemoryError` at `ms=499794`. This rules out
   `emit_inline_tlab_new` (both the base bump-pointer path and the
   `compact_ref_fields` branch inside it) as the source — the race survives
   with that inline path fully disabled.
3. **Large heap (`--Xmx 6000m`, JIT on, inline-`new` enabled)**: corruption
   warnings still occur (`class_id=0/4/4`, various `num_slots`/`array_length`),
   so a big heap does **not** cleanly suppress this the way it suppressed
   HIB-CV-37's SIGSEGV and the `type.temporal.*` cluster's crash. Instead the
   run limps through both test methods for ~20 minutes (`ms=1194268`) before
   failing differently: transient H2 DDL/transaction errors ("An old
   transaction with the same id is still open", "Index PRIMARY_KEY_ not
   found" — H2 here is in-process/in-memory per run, so this isn't
   cross-process stale disk state; more likely downstream fallout of the same
   header corruption confusing JDBC/schema-management bookkeeping rather than
   a distinct bug) and finally `testMultiLoad` hitting JUnit's plain 120s
   `@Timeout` despite JIT being on — i.e. a big heap changes the *terminal*
   symptom but does not stop the underlying corruption from happening.

**Net effect on the "Candidates" list above**: candidate 1 (array-specific
inline path) is definitively false — no such path exists. Candidate 2's most
obvious culprit (`emit_inline_tlab_new`'s per-object-shape codegen) is also
ruled out by evidence 2. The corruption was real and reproducible 3/3, but its
locus was narrower than originally scoped: **not** array codegen, **not** the
plain-`new` inline-TLAB codegen, but the shared allocation helpers used beneath
the JIT helper path.

## Closed follow-ups

- ~~Check whether array allocation has its own inline TLAB fast path~~ — done,
  it doesn't (see above).
- ~~Try `CRATONVM_JIT_DISABLE_INLINE_NEW=1`~~ — done; does not suppress (see
  above). `emit_inline_tlab_new` was not the culprit for this bug.
- ~~Try a larger `-Xmx`~~ — done; does not cleanly suppress, just delays and
  changes the terminal symptom (see above).
- ~~Inspect shared allocation helper ordering in `gc/src/gen_heap.rs`~~ — done.
  Young and old allocation spans are now initialized under the corresponding
  arena/generation lock before GC walkers can observe them.
- Rerun the original Hibernate batch fixture when that external runner is
  available in a checkout; it was not present in this worktree.
