# Hibernate 121-class non-passed list — remote rerun (2026-07-08/09): 88/121 now pass; new classloader-poisoning bug found

| | |
|---|---|
| **What** | 4-shard rerun of the same 121-class non-passed list (`apps/hib-suite-runner/nonpassed.txt`) on a fresh Azure remote host (`20.83.144.174`), following up the 2026-07-07 local Windows rerun. |
| **Build** | `dev` merged to `a28a346e` (branch `test/hib-remote-rerun-20260708`, worktree `/data/data/hib-remote-rerun-20260708`, binary `cvhibremote0708`). |
| **Config** | real-JDK (`--java-home /home/victor/jdk25`), JIT on, `SHARDS=4`, `TIMEOUT=1200`. Wall time: 58m15s. |
| **Result** | `PASS=88 FAIL=14 CRASH=4 ABORTED=5 LOADERR=7 HANG=3` (of 121). |

## Massive progress since the last two checkpoints

| Run | Date | dev | PASS | FAIL | CRASH | HANG | Other |
|---|---|---|---|---|---|---|---|
| Azure baseline | 2026-07-05 | `49aaf713` | 0 (this was the non-passed *definition*) | 99 | 16 | 3 | 3 |
| Local Windows | 2026-07-07 | `d0a779f6` | 16 | 75 | 17 | 10 | 3 ABORTED |
| **Remote (this doc)** | 2026-07-08/09 | `a28a346e` | **88** | 14 | 4 | 3 | 5 ABORTED, 7 LOADERR |

The fleet of concurrent fix branches (dozens of `codex/*` worktrees observed on
this host, many targeting exactly the classes/clusters documented from the
2026-07-05 and 2026-07-07 runs) has resolved the large majority of the
original 121-class list. Full pass list (88 classes) is in this run's
`results.tsv`; not reproduced here for brevity — see
`apps/hib-suite-runner/out-hibremote0708-*/results.tsv` on the remote host,
or re-derive via `awk -F'\t' '$3=="PASS"{print $2}'`.

## New finding: `JpaLargeBlobTest` poisons classloading for the rest of its batch

The 7 `LOADERR` entries are **not independent bugs** — they're a single
cascading failure. `CratonRunner` runs multiple test classes per JVM process
(batch mode) until a crash/hang/`@@DONE`. In shard-3's raw log:

```
@@BEGIN 22 org.hibernate.orm.test.lob.JpaLargeBlobTest
@@RESULT 22 org.hibernate.orm.test.lob.JpaLargeBlobTest found=1 started=1 ok=0 failed=1 aborted=0 skipped=0 ms=1135763
@@BATCHEND 23
@@BEGIN 23 org.hibernate.orm.test.mapping.embeddable.JsonWithArrayEmbeddableTest
@@RESULT 23 ... found=0 started=0 ok=0 failed=0 aborted=0 skipped=0 ms=4 loaderror=java.lang.ClassNotFoundException
@@BATCHEND 24
@@BEGIN 24 org.hibernate.orm.test.mapping.generated.InVmGenerationsWithAnnotationsTests
@@RESULT 24 ... ms=3 loaderror=java.lang.ClassNotFoundException
... (continues for every remaining class in the batch, ms=3-4 each) ...
@@DONE
```

`JpaLargeBlobTest` itself took **1,135,763ms (~19 minutes)** — no longer a
clean HANG like the 2026-07-07 finding
([hib-jpalargeblobtest-object-read-nosuchmethod.md](hib-jpalargeblobtest-object-read-nosuchmethod.md)
if still present — see the note below about doc loss), but still extremely
slow, and it still fails (`found=1 ok=0 failed=1`). Immediately after it,
**every subsequent class in the same JVM process** fails to even load
(`ClassNotFoundException`, ~3-4ms each — far too fast to be a real
classpath-scan failure) until the batch ends. This is the SAME symptom
pattern across every occurrence in this run: `JsonWithArrayEmbeddableTest`,
`InVmGenerationsWithAnnotationsTests`, `BaseIdEntityByteCodeTest`,
`OneToOneJoinColumnsEmbeddedIdTest`, `ClassLoaderServiceImplTest`,
`StoredProcedureTest`, `OffsetDateTimeTest` — all 7 `LOADERR` entries are
exactly the classes that happened to be queued after `JpaLargeBlobTest` in
that shard's batch, not classes with their own independent load bug.
(Confirmed: `ClassLoaderServiceImplTest` passed cleanly in the 2026-07-07
local run, ruling out a class-specific defect.)

**Hypothesis:** whatever `JpaLargeBlobTest` does while reading the JDBC
Blob's binary stream (see the cross-referenced dispatch-bug doc) leaves
CratonVM's classloading subsystem in a corrupted/exhausted state — plausibly
a leaked native handle, a classloader-namespace id that never gets released
after the slow/failing Blob read, or some global class-lookup cache/table
left in a bad state — such that every subsequent `Class.forName`-style
lookup in that process immediately fails. This is a MORE SEVERE finding
than the original per-class dispatch bug: it means one broken test can
silently blank out results for everything queued after it in the same batch
run, which also means **some of this run's other apparent results for
batch-mates of a LOADERR class should be treated with suspicion** if this
pattern recurs elsewhere undetected (spot-check: in this run, only shard-3
hit it, and the 3 classes before `JpaLargeBlobTest` in that shard loaded and
ran fine, consistent with the corruption being triggered BY `JpaLargeBlobTest`
specifically, not a preexisting state issue).

**Note on doc continuity:** the 2026-07-07 docs for `JpaLargeBlobTest`,
`InPredicateTest`, and others were lost from the shared main worktree
between sessions (an unrelated concurrent `git` operation reset it — see the
2026-07-07 local-rerun doc's own note). Background tasks were spawned to
investigate `JpaLargeBlobTest`'s hang regression and 2 other findings; this
classloader-poisoning behavior is new information for whoever picks that up.

## Other residuals in this run (not deeply triaged)

- **HANG (3):** `type.temporal.ZonedDateTimeTest`,
  `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`,
  `hql.ASTParserLoadingTest`.
- **CRASH (4):** `sql.exec.SmokeTests` (rc=1; a native diagnostic in the log
  suggests an uncaught exception on a non-main thread whose stack trace
  wasn't captured — `CRATONVM_DBG_CHARSET=1`/`CRATONVM_DBG_ATHROW=1` would
  help reproduce with detail), `joinedsubclassbatch.IdentityJoinedSubclassBatchingTest`
  (rc=134/SIGABRT, "the monitored command dumped core"),
  `sql.storedproc.ResultMappingTest` and `type.temporal.LocalDateTimeTest`
  (both rc=134, but with **no captured `@@BEGIN` line in the raw log** —
  likely lost to stdio buffering on abrupt process death rather than a
  truly instant crash; re-run individually with unbuffered output to get a
  real repro).
- **ABORTED (5):** `manytomanyassociationclass.surrogateid.generated.ManyToManyAssociationClassGeneratedIdTest`,
  `type.temporal.OffsetTimeTest` (176/396 ok, 88 aborted),
  `bytecode.enhancement.basic.InheritedTest`, `type.temporal.InstantTests`
  (112/204 ok, 92 aborted), `bytecode.enhancement.basic.MappedSuperclassTest`
  — the temporal classes' high abort counts likely reflect Hibernate's own
  `@CustomEnhancementContext`/dialect-gated `assumeTrue` skips rather than
  new CratonVM bugs (matches a pattern already noted in
  [hib-bytecode-enhancement-loader-faithful-linking.md](hib-bytecode-enhancement-loader-faithful-linking.md)
  for `InheritedTest`/`MappedSuperclassTest`).
- **FAIL (14):** `batch.BatchTest`, `bytecode.enhancement.basic.FinalFieldEnhancementTest`,
  `bytecode.enhancement.graph.LoadAndFetchGraphAssociationNotExplicitlySpecifiedTest`,
  `id.enhanced.OptimizerConcurrencyUnitTest`, `query.hql.FunctionTests` (still
  blocked — see [hib-temporal-sql-parameter-placeholder-duplication.md](hib-temporal-sql-parameter-placeholder-duplication.md)),
  `batchfetch.DynamicBatchFetchTest`, `bytecode.enhancement.lazy.LazyOneToOneRemoveFlushAccessTest`,
  `bytecode.enhancement.merge.CompositeMergeTest`, `id.uuid.rfc9562.UUidV6V7GeneratorTest`,
  `function.json.JsonArrayUnnestTest`, `onetoone.embeddedid.OneToOneEmbeddedIdTest`,
  `annotations.xml.ejb3.Ejb3XmlElementCollectionTest`,
  `bytecode.enhancement.lazy.proxy.FetchGraphTest`, `lob.JpaLargeBlobTest`
  (see classloader-poisoning finding above).

## Not yet done

- Deep individual triage of the remaining 14 FAIL / 4 CRASH / 3 HANG / 5
  ABORTED classes against the 2026-07-05/07 findings (which of these are
  genuinely new vs. already-tracked) — only the classloader-poisoning
  pattern was traced to a root-cause-adjacent explanation in this pass.
- Reproducing the classloader-poisoning bug in isolation (2-class batch:
  `JpaLargeBlobTest` then any other class) to confirm the hypothesis and
  narrow which native subsystem leaks state.
