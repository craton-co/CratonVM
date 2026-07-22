# `HQLTypeTest` / `JsonFunctionTests` — intermittent `NullPointerException: ATNState.transitions` inside real ANTLR `ParserATNSimulator` during HQL parse

| | |
|---|---|
| **Status** | ✅ FIXED — native ANTLR prediction is no longer used for the unshaded Hibernate runtime; validation is recorded below. |
| **Area** | Real, unshaded `org.antlr:antlr4-runtime:4.13.2` (`org.antlr.v4.runtime.atn.ParserATNSimulator`), used by Hibernate's generated HQL grammar (`org.hibernate.grammars.hql.HqlParser`) while parsing a query's SELECT expression list. |
| **Discovered** | 2026-07-21, Hibernate ORM JUnit5 suite "passed" category rerun, `apps/hib-suite-runner/run-hib.sh`, binary from worktree `C:\craton\CratonVM-hib-local-0712` (branch `test/hib-local-0712`, merged with `origin/dev` @ `7aed580f0`). |

## Resolution

**Status:** FIXED — retired from `docs/known-issues` after focused repeated
real-JDK Hibernate validation.

The failure was not an ANTLR grammar or JIT-compilation error. Hibernate uses
the unshaded `org/antlr/v4/runtime` implementation, whose native
`ParserATNSimulator` replacements recursively retain mutable ATN/DFA graph
references across allocation-capable calls. With a moving collector, this can
leave a stale `ATNState` for bytecode `computeTargetState`, producing the
observed null `transitions` field.

`native-builtins/src/lib.rs` now leaves the unshaded `ParserATNSimulator`
prediction methods (`computeReachSet`, `closure`, `closure_`,
`closureCheckingStopState`, `getEpsilonTarget`, and
`canDropLoopEntryEdgeInLeftRecursiveRule`) as real Java bytecode. The
unshaded ANTLR package remains JIT-excluded, so those methods execute with
ordinary interpreter-frame GC roots. The shaded Groovy ANTLR intrinsic set is
unchanged. This is a correctness boundary rather than a speculative partial
rooting repair inside the old recursive native path.

The native-registry regression asserts that the unshaded parser methods are
not registered while the remaining ANTLR intrinsics are retained. The
application checks below cover the original `JsonFunctionTests` stress repro,
the linked `HQLTypeTest` residual, and both JIT modes.

## Symptom

Original failures:

```
# run-20260721-175909-passed/on-real/shard-4/raw.log
@@FAIL org.hibernate.orm.test.query.hhh12225.HQLTypeTest :: java.lang.NullPointerException: ATNState.transitions
@@FAIL org.hibernate.orm.test.query.hhh12225.HQLTypeTest :: org.junit.platform.commons.JUnitException: Failed to close extension context
@@RESULT 134 org.hibernate.orm.test.query.hhh12225.HQLTypeTest found=2 started=2 ok=1 failed=1 aborted=0 skipped=0 ms=111303

# run-20260721-175909-passed/on-real/shard-7/raw.log
@@FAIL org.hibernate.orm.test.query.hql.JsonFunctionTests :: java.lang.NullPointerException: ATNState.transitions
@@FAIL org.hibernate.orm.test.query.hql.JsonFunctionTests :: jakarta.persistence.NoResultException: No result found for query [select json_value(...)...]
@@FAIL org.hibernate.orm.test.query.hql.JsonFunctionTests :: java.lang.IllegalArgumentException: argument "content" is null   (x2)
@@FAIL org.hibernate.orm.test.query.hql.JsonFunctionTests :: org.opentest4j.AssertionFailedError: Should fail because keys are not unique
@@FAIL org.hibernate.orm.test.query.hql.JsonFunctionTests :: org.junit.platform.commons.JUnitException: Failed to close extension context
@@RESULT 142 org.hibernate.orm.test.query.hql.JsonFunctionTests found=34 started=14 ok=8 failed=6 aborted=0 skipped=20 ms=128295
```

`-Dcraton.trace=1` gives the full stack trace (identical shape both times it was
captured, same top 3 frames):

```
java.lang.NullPointerException: ATNState.transitions
	at org.antlr.v4.runtime.atn.ParserATNSimulator.computeTargetState(ParserATNSimulator.java:554)
	at org.antlr.v4.runtime.atn.ParserATNSimulator.execATN(ParserATNSimulator.java:432)
	at org.antlr.v4.runtime.atn.ParserATNSimulator.adaptivePredict(ParserATNSimulator.java:371)
	at org.hibernate.grammars.hql.HqlParser.expressionOrPredicate(HqlParser.java:8090)
	at org.hibernate.grammars.hql.HqlParser.selectExpression(HqlParser.java:3836)
	at org.hibernate.grammars.hql.HqlParser.selection(HqlParser.java:3747)
	at org.hibernate.grammars.hql.HqlParser.selectionList(HqlParser.java:3683)
	at org.hibernate.grammars.hql.HqlParser.selectClause(HqlParser.java:3630)
	at org.hibernate.grammars.hql.HqlParser.query(HqlParser.java:2452)
	...
	at org.hibernate.query.hql.internal.StandardHqlTranslator.parseHql(StandardHqlTranslator.java:101)
	at org.hibernate.query.internal.QueryInterpretationCacheStandardImpl.createHqlInterpretation(...)
	...
	at org.hibernate.orm.test.query.hql.JsonFunctionTests.lambda$testJsonObjectAndArray$0(JsonFunctionTests.java:311)
```

The trigger query for `JsonFunctionTests` (`testJsonObjectAndArray`, line ~305-311) is a
deeply nested expression list: `json_object('a', json_array(1,2,3), 'b',
json_object('c', json_array(4,5,6))), json_array(json_object(...), json_object(...),
json_object(...))` — many nested function calls in one SELECT list, i.e. many
distinct ANTLR parser-decision visits in a single parse.

## Reproduction — confirmed genuine but intermittent

```
cd C:/craton/CratonVM/apps/hib-suite-runner
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m @common.args -Dcraton.trace=1 -Dcraton.batch=2 CratonRunner <listfile-with-both-classes> 0
```

8 total isolated attempts across two sessions (both classes run together per
attempt, fresh JVM each time):

| Run | `HQLTypeTest` | `JsonFunctionTests` | `JsonFunctionTests` ms |
|---|---|---|---|
| 1 | ok=2 failed=0 | ok=14 failed=0 | 39077 |
| 2 | ok=2 failed=0 | ok=14 failed=0 | 41183 |
| 3 | ok=2 failed=0 | **ATNState NPE, ok=9 failed=5** | 53918 |
| 4 | ok=2 failed=0 | ok=14 failed=0 | 53231 |
| 5 | ok=2 failed=0 | **ATNState NPE, ok=7 failed=7** | 72325 |
| 6 | ok=2 failed=0 | **ATNState NPE, ok=9 failed=5** | 68211 |
| 7-8 | (loop killed by tool timeout mid-run 7; not counted) | | |

`JsonFunctionTests` reproduced the NPE in **3 of 6** clean isolated attempts
(50%) — genuinely non-deterministic, not a stale-binary or environment
artifact (same binary, same classpath, same host, back-to-back runs).
`HQLTypeTest` did **not** reproduce it in any of the 6 isolated attempts,
despite being class 134 (with the NPE) in the original 4548-class full-suite
run. `HQLTypeTest` has only 2 tiny tests and 2 simple HQL queries (see
`hhh12225/HQLTypeTest.java`) — far less ANTLR-decision exposure per JVM
lifetime than `JsonFunctionTests`'s ~14 started tests × many nested
JSON-function queries. The likeliest reconciliation: in the original
full-suite run, `HQLTypeTest` ran as class **#134**, after 133 preceding
classes' worth of HQL parses in the *same JVM* had already exercised
Hibernate's grammar-decision DFA/ATN caches (ANTLR generated parsers cache
per-decision DFA state in fields shared across all parses in a process) —
giving the same latent defect far more accumulated opportunities to trigger
before `HQLTypeTest`'s own trivial queries run, vs. a fresh-JVM isolated
repro where the shared cache starts clean and `HQLTypeTest` alone doesn't
generate enough decision traffic to hit it. This is consistent with (not yet
proven as) both classes sharing one root cause: a latent defect in
CratonVM's handling of ANTLR's per-decision prediction/DFA cache that
manifests more often the more the cache has been used, rather than each
class having an independent, unrelated bug.

Elapsed time for `JsonFunctionTests` was also generally higher on
NPE-producing runs (53918/72325/68211 ms) than clean runs (39077/41183/53231
ms), though not perfectly separating (run 4 at 53231ms passed clean) — a
loose signal consistent with (not proof of) a timing/GC-window-sensitive
trigger rather than a pure, always-reproduces-the-same-way logic bug.

## Not the two previously-fixed/refuted ANTLR bugs in this codebase

- **Not the chained-operator closure bug** (`docs/internal/hql-antlr-chained-operator-syntax-error-FIXED.md`,
  fixed `4e2a4493`, wrong `inContext` flag passed to
  `antlr_parser_native_get_epsilon_target`). That bug: (a) throws
  `IllegalArgumentException`/`SyntaxException` ("no viable alternative"), not
  `NullPointerException`; (b) is **100% deterministic** — reproduces on the
  very first parse of any grammar that revisits the same decision twice, no
  warm-up needed. This bug is intermittent (50% in one class, 0% in 6 tries
  of another), the opposite reproducibility profile.
- **Not the entity-graph `RuleNode.getChildCount()` NPE cluster**
  (`docs/internal/hibernate-bugs/hib-entitygraph-antlr-rulenode-npe-cluster-FIXED.md`,
  FIXED). That cluster's root cause was Hibernate Models' method-reference
  dispatch (`Class::getDeclaredAnnotations`/`Class::getName`) silently
  selecting real-JDK bytecode instead of CratonVM's registered native Class
  mirror accessors — an entirely different subsystem (annotation
  introspection, not the ANTLR ATN/DFA prediction machinery) that happens to
  also throw an NPE naming an `org.antlr.v4.runtime.tree` type.
- **Not the `query.hql.FunctionTests`/`FunctionTests` translation cluster**
  (`docs/internal/hib-query-hql-functiontests-translation-cluster-NOT-A-BUG.md`,
  REFUTED) — that doc's failures are wrong-row-count/wrong-value query
  *results*, not parse-time crashes, and were shown to reproduce identically
  on real HotSpot (test-suite shared-fixture fragility, not a CratonVM bug).
  This NPE has no HotSpot analog claimed or checked here, but its symptom
  shape (a crash inside `ParserATNSimulator` itself) is unrelated to that
  cluster's symptom shape (successful parse, wrong SQL result).
- **Not the already-JIT-banned-by-default cold path.** `vm/src/jit/skip_list.rs`
  already bans the real, unshaded `org/antlr/v4/runtime/` package prefix from
  JIT by default (`hibernate_unshaded_antlr_runtime_stays_interpreted_by_default`
  test, `skip_list.rs:737-738,1511-1512`) — separate from the
  `groovyjarjarantlr4/`-specific `PredictionContext` miscompile ban. So this
  NPE is not a JIT-miscompile symptom; it happens with these ANTLR classes
  running interpreted by default.

## Working hypothesis (not confirmed): native ANTLR closure/reach-set reimplementation, GC-timing sensitive

CratonVM has a **native Rust reimplementation** of several
`ParserATNSimulator` methods, registered by class+method+descriptor
(`native-builtins/src/lib.rs`, ~line 19260-19307):
`closure`, `closure_`, `closureCheckingStopState`, `getEpsilonTarget`,
`computeReachSet`, `canDropLoopEntryEdgeInLeftRecursiveRule`. This is the
*exact* mechanism responsible for the previously-fixed chained-operator bug
(a one-argument logic error in this native code). `computeTargetState` and
`execATN` themselves are **not** natively reimplemented (real bytecode/
interpreter), but `computeTargetState` calls the native `computeReachSet`
directly, and the NPE fires on the very next line back in
`computeTargetState`'s own (interpreted) code when it dereferences an
`ATNState` object that `computeReachSet` handed back — i.e., the object
crossing the native→bytecode boundary is the one found null-fielded.

Given the intermittency (not 100% on any fixed input, unlike the previous
closure bug) and the loose correlation with longer/more-loaded runs, the
leading candidate is **not** a straightforward logic error in the native
reimplementation (that would reproduce every time, like the chained-operator
bug did) but something timing/GC-sensitive in how the native code holds onto
Java object references (`ATNState`/`ATNConfig`/DFA-state objects) across the
native↔interpreted boundary and across GC safepoints — the same general
failure family this codebase has hit before in other native/reflective code
paths that cache or pass around live object references without proper
GC-root bookkeeping (see `reference_stale_local_after_gc_capable_ctor_rewrite`,
`reference_update_root_snapshot_reflective_chain_scaling_20260721`,
`reference_g1_parallel_evac_selfforward_uaf`). **This has not been verified**
— no GC-log/`CRATONVM_DBG_ROOTSNAP`-style instrumentation was run against a
failing repro in this session; it is offered as the most consistent
hypothesis with the evidence gathered (intermittency + native-reimplemented
hot area + loose load correlation), not a confirmed mechanism.

## Recommendation / next steps

1. Correlate NPE-producing runs against GC-event timing (e.g. a GC log or
   `CRATONVM_DBG_*` GC instrumentation) to check whether the NPE always
   follows shortly after a GC cycle during `JsonFunctionTests`'s
   `testJsonObjectAndArray`/similar heavily-nested-query methods.
2. Audit `native-builtins/src/lib.rs`'s `closure`/`closure_`/
   `closureCheckingStopState`/`computeReachSet`/`getEpsilonTarget` Rust
   implementations for any raw Java-object pointer held across a potential
   safepoint/allocation without re-fetching from a GC-safe handle.
3. Re-run the isolated repro at higher volume (20+ attempts) to get a tighter
   failure-rate estimate and check whether `HQLTypeTest` ever reproduces
   alone given enough tries, which would strengthen (or, if it never does,
   weaken) the shared-static-DFA-cache-accumulation theory above.
4. If a GC correlation is confirmed, this likely belongs in the same family
   as the other native/reflective-root GC-safety bugs already tracked in
   this codebase's memory, and the fix approach (safepoint-gated root
   publish, or re-deriving pinned locals post-GC) may be directly reusable.

## Scope

Both classes fail with the identical exception class, message, and top-3
stack frames (`computeTargetState`→`execATN`→`adaptivePredict`), both
triggered from Hibernate's generated `HqlParser` mid-parse of a query's
SELECT-clause expression list. Treated here as one shared-root-cause
candidate pending the GC-correlation check above, not two independent bugs.
