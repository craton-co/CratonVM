# HIB-ANTLR.1 / HIB-LONGTAIL.1's ANTLR half — CLOSED 2026-07-27, `org/antlr/v4/runtime/` is JIT-eligible again

**Status: RESOLVED.** Nothing in the JIT skip list bans `org/antlr/v4/runtime/`
as a package any more. HIB-ANTLR.1's own check went on 2026-07-26; the last
thing still holding the package — HIB-LONGTAIL.1's second prefix — was dropped
2026-07-27 after a full same-fixture A/B. The narrow, evidence-backed
`PredictionContext` equality/hash guard (ANTLR-COLDPATH.1) was *extended* to
cover this runtime explicitly, so the one cluster that ever had a reproduced
miscompile is no weaker than before.

## The two-step history

1. **2026-07-26 (original content of this doc).** HIB-ANTLR.1's own claim — a
   full HQL parse under JIT leaving `ATNState.transitions` null and corrupting
   the *next* parse in the same process — did not reproduce across 315 real HQL
   test methods (`ASTParserLoadingTest` 106/106, `HQLInsertAndUpdateTest` 5/5,
   `InstantTests` 112 ok / 0 failed / 92 aborted = baseline exactly), run with
   `CRATONVM_JIT_ALLOW_PACKAGES=org/antlr/v4/runtime/` and `org/hibernate/`
   still banned. Its check was deleted — but only as a **shadowed no-op**:
   HIB-LONGTAIL.1 (`vm/src/jit/skip_list.rs`) covered the identical prefix, so
   default behaviour did not change. That session deliberately left
   HIB-LONGTAIL.1 alone to avoid a same-file collision with a concurrent
   session, recording the narrowing as a recommendation.
2. **2026-07-27 (this closure).** Took that recommendation, with fresh evidence.

## Why HIB-LONGTAIL.1 never had a claim on the ANTLR half

Every justification HIB-LONGTAIL.1 still rests on comes from the H2 suite:

* the original `Schema  not found` metadata corruption (now fixed and extinct,
  0 of 218 classes),
* the ten-class 2026-07-26 A/B regression set, of which three classes
  (`TestStreamStore`, `TestFreeSpace`, `TestNestedJoins`) still hold the
  `org/h2/` half open — see
  [`h2-jitban-longtail1-CLOSED-20260805.md`](../fixed-suite-bugs/h2-suite-bugs/h2-jitban-longtail1-CLOSED-20260805.md).

The H2 suite never loads an `org/antlr/` class at all — H2's SQL parser is
hand-written, and no ANTLR artifact is on its classpath. HIB-LONGTAIL.1's own
doc said as much: *"The `org/antlr/v4/runtime/` half remains untested in
isolation — the H2 suite never exercises it."* The two prefixes were bundled by
a 2026-07-20 narrowing of a much broader (java.util-wide) rule, not by shared
evidence.

## The 2026-07-27 A/B

Fixture: the real Hibernate ORM 8.0 harness at
`/data/data/apps/hibernate-orm-harness/` on the Azure host (compiled
`hibernate-core` test classes + full `testRuntimeClasspath`, real H2 in-memory
DB, `hib-suite-runner/CratonRunner.java` JUnit5 Platform driver). This is the
one fixture on record that parses HQL through `org/antlr/v4/runtime/` — every
Hibernate query goes through it — and it ships the real
`antlr4-runtime-4.13.2.jar`.

Scope: **all 57 `org.hibernate.orm.test.hql.*Test` classes**, run one class per
VM, strictly one VM at a time, baseline immediately followed by the ban-removed
binary on the same class. `org/hibernate/` stayed banned (HIB-TEMPORAL.1) in
both arms, so the ANTLR ban was the only variable. Two binaries built from one
tree, differing only in this change.

| result | classes |
|---|---:|
| identical `found`/`started`/`ok`/`failed`/`aborted`/`skipped` | 55 |
| differed on the sweep, resolved to a tie by repeated runs (below) | 2 |

Representative volume: `HQLTest` 168/168 ok in both arms; `BooleanPredicateTest`,
`CoalesceTest`, `CollectionMapWithComponentValueTest`, `EnumTest`,
`FunctionNameAsColumnTest`, `HQLInsertAndUpdateTest`, `HqlOperatorTypesafetyTest`
and the rest byte-identical per class.

An earlier pass of the same A/B against an intermediate binary (ban removed,
`PredictionContext` guard not yet extended) gave the same picture: 15 SAME out
of 17 completed, both DIFFs being the same OOM flake landing on the *baseline*
side.

### Both DIFFs resolved to a tie — neither is ours

The two classes that differed on the single sweep were re-run repeatedly, one
VM at a time, and both came out symmetric.

**`BulkManipulationTest`** — 2 runs per arm:

| run | baseline | ban-removed |
|---|---|---|
| r1 | OOM flake | 51 found / 50 started / **50 ok / 0 failed** / 1 skipped |
| r2 | 51 / 50 / **50 ok / 0 failed** / 1 skipped | identical |

**`ASTParserLoadingTest`** — 3 runs per arm:

| run | baseline | ban-removed |
|---|---|---|
| r1 | 105 ok / 1 failed | 105 ok / 1 failed — **the same test**, same message |
| r2 | **106/106 ok** | OOM flake |
| r3 | OOM flake | **106/106 ok** |

Both arms produce a clean 106/106, and both arms produce each of the two
failure modes. Neither mode is specific to a configuration:

1. **`TimeoutException: testJpaTypeOperator … timed out after 120 seconds`** —
   the harness sets `junit.jupiter.execution.timeout.default=120s`, and this
   one long test exceeds it when the host is busy. It fired on the baseline and
   the ban-removed binary on the same test in the same round.
2. **`OutOfMemoryError: Java heap space (anewarray component 6 length
   1677721600)`** — a request for a 1.68-billion-element `float[]` that no
   Hibernate code asks for (`1677721600 == 0x64000000`). Pre-existing and
   load-dependent: it hit the baseline arm on `ASTParserLoadingTest` and
   `BulkManipulationTest` in one pass and the ban-removed arm on the same
   classes in others, and also appeared on
   `ScrollableCollectionFetchingTest` / `TreatKeywordTest` in both
   configurations. `ASTParserLoadingTest` ran 106/106 clean on the baseline
   binary when the host was quiet and OOM'd on that same binary once load
      average passed 100. **Identified 2026-07-27 (later the same day): this is
   the LICM/speculative pre-header bypass fixed in `613b10f4c`** — see
   `../fixed-suite-bugs/jit-licm-preheader-bypass-20260727.md`. Both arms of this
   A/B were built from a base that predates that fix, so it was never the
   ban. `component 6` is a ClassId (`java/lang/String`), not T_FLOAT, and
   `1677721600 == 25 * 2^26` is `AttributesImpl.ensureCapacity`'s doubling
   loop running against an un-written hoist slot. 0 OOM in 46 runs on a
   current-`dev` binary.

## What changed in the code

`vm/src/jit/skip_list.rs`:

* **HIB-LONGTAIL.1 narrowed to `org/h2/` only.** The
  `|| class_name.starts_with("org/antlr/v4/runtime/")` term is gone.
* **`is_antlr_prediction_context_miscompile` extended to the unshaded runtime.**
  It previously named only `groovyjarjarantlr4/v4/runtime/...` classes, so the
  7 guarded `PredictionContext` / `SingletonPredictionContext` /
  `ObjectEqualityComparator` methods in `org/antlr/v4/runtime/` were pinned to
  the interpreter *only* by HIB-LONGTAIL.1's prefix. Removing that prefix would
  have silently un-pinned them. It now matches on the suffix after either
  package prefix, mirroring what `is_antlr_prediction_context_native_override`
  (`vm/src/runtime/interpreter.rs`) already did on the native-dispatch side.
  This is the one place where the two ANTLR copies genuinely share a
  reproduced defect, and it stays guarded in both.
* The unit test formerly asserting "stays interpreted via HIB-LONGTAIL.1" is now
  `hibernate_unshaded_antlr_runtime_is_jit_eligible_after_hib_longtail_1_narrowing`,
  and additionally pins (a) the extended `PredictionContext` guard for the
  unshaded runtime, including under `CRATONVM_JIT_ALLOW_PACKAGES`, and (b) that
  the `org/h2/` half is untouched. 68/68 `jit::skip_list` tests pass.

## Net effect

* `org/antlr/v4/runtime/` classes are JIT-eligible under Conservative for the
  first time — a real behaviour change, not a shadowed no-op.
* The 7-method `PredictionContext` cluster stays interpreted in **both** the
  shaded and unshaded runtimes, and is still not liftable by
  `CRATONVM_JIT_ALLOW_PACKAGES`.
* `org/h2/` is unaffected; HIB-LONGTAIL.1 remains open on its three H2 classes.
* HIB-TEMPORAL.1 (`org/hibernate/`) was separate from this package-ban
  finding and remained active at the time. It was fixed and retired on
  2026-07-29; see
  [`hib-temporal-1-retired-20260729.md`](hib-temporal-1-retired-20260729.md).

## Reproduction

```bash
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
cd /data/data/apps/hibernate-orm-harness/hib-libs/test-classes
find org/hibernate/orm/test/hql -name '*Test.class' ! -name '*$*' \
  | sed 's/\.class$//' | tr '/' '.' | sort > /tmp/hql.txt
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
TMPDIR=/data/tmp <cratonvm-binary> --java-home /home/victor/jdk25 @common.args \
  CratonRunner /tmp/hql.txt 0
```

Run the same list against a binary built with the `org/antlr/v4/runtime/` term
restored in HIB-LONGTAIL.1 to reproduce the A/B. Keep the host quiet — under
heavy concurrent load the unrelated `float[1677721600]` OOM and the 120s
`testJpaTypeOperator` timeout both fire in either arm and mask the comparison.
Re-run any class that differs; on this fixture a single sweep is not enough to
call a DIFF real.
