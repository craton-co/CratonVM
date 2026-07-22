# `insertordering.InsertOrderingRCATest` HANG — genuine (not stuck) deep recursion in Hibernate's EAGER circular-fetch/graph resolution, a different phase than the previously-documented "generic architectural gap"

| | |
|---|---|
| **Status** | 🔴 **OPEN** — newly characterized. Not (solely) the previously-documented "method-diversity-bound JDBC batch-insert" architectural gap; a distinct, deep-recursion mechanism in Hibernate's `ToOneAttributeMapping`/`LoaderSelectBuilder` EAGER-fetch SQL-AST builder, most likely another trigger of the already-tracked, OPEN `update_root_snapshot` O(interpreter-depth) reflection/recursion-storm wall. |
| **Class** | `org.hibernate.orm.test.insertordering.InsertOrderingRCATest` (`testBatching`) |
| **Symptom this run** | `HANG`, `process-died rc=124` — the suite harness's flat 300s per-invocation timeout expired with **zero** `@@RESULT`/`@@FAIL` ever printed. |
| **"RCA"** | Literally **R**oot-**C**ause-**A**nalysis — the test's domain model (`DefaultTemplatesVault`) is a small RCA rule-engine schema (`rca_cause`, `rca_condition`, `rca_expression`, `RCATemplate`, …), not an abbreviation for anything CratonVM-related. |

## Source run

`apps/hib-suite-runner/runs/run-20260721-175909-passed/on-real/shard-5/raw.log`
(idx 93, `@@BEGIN 93 org.hibernate.orm.test.insertordering.InsertOrderingRCATest`
at line 46810; next class's process starts exactly 300s later at line 47038
— this invocation was killed by the wrapper's `timeout 300`). Binary from
worktree `CratonVM-hib-local-0712` (branch `test/hib-local-0712`, merged with
`origin/dev` @ `7aed580f0`).

The raw log's last visible content before the kill is schema DDL
(`create table rca_cond_and_expr`, `rca_cond_posssibility`, `rca_condition`,
`rca_expression`) — i.e. the class hadn't even finished `hbm2ddl`
auto-schema-creation, let alone reached `testBatching`'s persist/batch-insert
phase, before the 300s budget ran out.

This is a **different failure signature** than the one this class was
previously profiled for: `docs/internal/hib-120s-junit-timeout-cluster-20260716.md`
extensively re-measured this same class/method at ~59-70s (~8-9.7x HotSpot's
7350ms), root-caused to a "method-diversity-bound" JDBC batch-insert workload
(184 distinct `PreparedStatement` shapes for ~500 rows, wide C1-only compile
distribution, zero GC events, `--nojit` slower not faster — a genuine but
bounded interpreter/JIT throughput gap). That investigation never observed
anything resembling deep recursion, and its own worst-case numbers (59-70s)
are far short of this run's 300s+ hang. This run's hang happens **earlier**
in the test lifecycle (schema creation, not batch insert) and in a
completely different Hibernate subsystem.

## Solo reproduction

```
cd C:/craton/CratonVM/apps/hib-suite-runner
echo org.hibernate.orm.test.insertordering.InsertOrderingRCATest > /tmp/single-ior.txt
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=1 --stack-dump-on-timeout 90 CratonRunner /tmp/single-ior.txt 0
```

Reproduced on the first attempt. The `--stack-dump-on-timeout 90` watchdog
fired at 90s and, before aborting, took **2396 rapid successive stack
samples of the single `main` thread**. Frame count oscillates
sample-to-sample in a bounded range (163 → 164 → 161 → 160 → 158 → 159 →
158 → … → 151 → … → 146 → 147 → 148), and the leaf frames differ between
samples — definitive proof this is **not** a frozen/parked thread; it is
genuinely, continuously executing, just never completing within the
observation window (same diagnostic signature the `ASTParserLoadingTest`
and `joinedsubclassbatch` docs in this same directory use to rule out
deadlock).

The repeating pattern from roughly depth 90 through depth 140+ in every
sample is the identical 12-frame cycle:

```
ToOneAttributeMapping.generateFetch
  → ToOneAttributeMapping.generateFetch (overload)
  → ToOneAttributeMapping.withRegisteredAssociationKeys
  → ToOneAttributeMapping.lambda$generateFetch$0
  → ToOneAttributeMapping.buildEntityFetchJoined
  → EntityFetchJoinedImpl.<init>
  → AbstractEntityResultGraphNode.afterInitialize
  → AbstractFetchParent.afterInitialize
  → LoaderSqlAstCreationState.visitFetches
  → LoaderSelectBuilder.visitFetches
  → LoaderSelectBuilder.lambda$createFetchableConsumer$0
  → FetchParent.generateFetchableFetch
  → (repeat)
```

i.e. Hibernate's SQL-AST/loader building an **eagerly-joined fetch graph**,
recursing back into itself dozens of times. Near the *bottom* of the dump
(shallower frames, depth 141-146) the cycle-breaking machinery Hibernate
uses specifically to bound this — `ToOneAttributeMapping.resolveCircularFetch`
→ `determineCircularKeyResult` → `createTableGroupForDelayedFetch` →
`SimpleFromClauseAccessImpl.registerTableGroup` → `NavigablePath.equals`
(called twice, nested) — is visibly present and being exercised, but the
overall recursion still runs 140-160+ frames deep and does not terminate
within 90+ seconds.

## Why this is plausible for this specific entity model

Every association in this test's domain model is explicitly
`fetch = FetchType.EAGER` (unusual — normally discouraged, almost certainly
deliberate here to exercise Hibernate's own circular/eager-fetch handling),
and the `Condition`/`Expression` type hierarchy is genuinely
mutually-recursive at the **mapping metadata** level, independent of the
specific instance data persisted by `DefaultTemplatesVault`:

- `Condition` (abstract) ← `SimpleCondition` (→ `Expression` left/right,
  EAGER), `CompoundCondition` (→ `Condition` first/second, EAGER),
  `AlertCondition`.
- `Expression` (abstract) ← `MathExpression` (→ `Expression` left/right,
  EAGER), `ConditionalExpression` (→ `Set<ConditionAndExpression>`, EAGER),
  `ParameterExpression`, `ConstantExpression`, `FieldExpression`,
  `CalculationExpression`, `NumberedExpression` (→ `Expression`, EAGER).
- `ConditionAndExpression` → `Condition` **and** `Expression` (both EAGER) —
  the direct `Condition ↔ Expression` cycle.
- `Cause` → `Condition` ×2 (`condition`, `auxCondition`, both EAGER) plus
  `TimeManipulation` ×2, `Set<NumberedExpression>` ×2, `Set<Condition>`
  (`fetchConditions`) — a densely cross-referenced polymorphic graph.

When Hibernate's loader builds an eager-join `SELECT` for any of the
supertypes (`Condition`/`Expression`), it must reason about the
mapping-level possibility of every subtype's associations recursing back
into an already-visited type — exactly the class of problem
`resolveCircularFetch`/`NavigablePath`-keyed de-duplication exists to solve.
The stack shows that machinery running, but not converging quickly for
this particular densely-interconnected, all-EAGER hierarchy.

## Root-cause hypothesis

Most likely **not an isolated, independently-fixable defect** in this exact
code, but another manifestation of the same already-tracked, OPEN,
deferred `update_root_snapshot` per-native-call GC-root-snapshot cost being
`O(current interpreter stack depth)` (see
[`reference_update_root_snapshot_reflective_chain_scaling_20260721`],
[`docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`](../../internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md),
and this directory's own
[`astparserloadingtest-jaxb-reflection-storm-throughput-wall-20260721.md`](astparserloadingtest-jaxb-reflection-storm-throughput-wall-20260721.md)):
a 140-160-frame-deep, native-call-heavy (JDBC/reflection-backed property
access, entity-metadata lookups) recursive call chain that keeps growing
and shrinking by a few frames per step is exactly the "top-of-stack churns
every call, GC-root rescan cost tracks total depth" shape that mechanism
identifies as its worst case — just triggered here via Hibernate's SQL-AST
fetch-graph builder instead of JAXB reflection (the previous two triggers)
or ANTLR parsing. Whether the *underlying* recursion is itself
inefficient in Hibernate (an O(2^depth)-ish re-exploration of overlapping
subpaths of the `Condition`/`Expression` graph without effective
memoization — plausible independent of CratonVM, since the same code runs
on HotSpot) or purely CratonVM's per-call overhead making an already-large
but HotSpot-tractable recursion cross this harness's 300s wall is not
distinguished by this session; either way it is consistent with, not
contradictory to, the already-recorded HotSpot baseline of 7350ms for the
full test (HotSpot's raw throughput may simply finish the same recursion
in that budget).

## Host-load caveat

Concurrent, unrelated `cratonvm.exe`/`cratonvm-spring-boot-suite` processes
from other sessions were observed on this shared host both before and
during this investigation (up to 6 unrelated `cratonvm` processes plus one
`cratonvm-spring-boot-suite` process at one check). This does not change the
qualitative finding (continuous, varying-depth, Hibernate-fetch-graph-only
forward progress, never stuck at one PC) but the absolute "still running
past 90s" figure could be somewhat inflated versus a fully quiet host.

## Recommendation

Not independently fixable in this session — same posture as the sibling
`ASTParserLoadingTest`/`joinedsubclassbatch` findings in this directory.
Re-run this class once any `update_root_snapshot` fix effort (e.g. the
unmerged `codex/fix-oauth2-rootsnapshot-20260721-019f86d0` work) lands on
`dev`, with `--stack-dump-on-timeout 60` and a `--timeout` well above 300s,
to see whether the eager-fetch recursion now converges quickly. Until
then, treat this class's `HANG` under the suite's flat 300s default as
explained (deep, genuinely-progressing recursion in Hibernate's own
circular-EAGER-fetch resolution, most likely amplified by the tracked
`update_root_snapshot` mechanism), not as a fresh regression — but note it
is a **different** phase/mechanism than the previously-recorded ~59-70s
"generic architectural gap" for this same class, so that older
characterization should not be assumed to still be the whole story for
`InsertOrderingRCATest`.
