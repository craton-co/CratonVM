# `InternalError: refusing side-effecting replay` (reason `TransferToInterpreter`) breaks 7 hibernate-reactive classes — GC-independent

## Status

**OPEN, new finding.** Confirmed reproducible and GC-independent: identical
signature on all three collectors (Generational, G1, ZGC), same host, same
binary, same run. Root mechanism identified from source (`can_deopt_resume`
is unconditionally `false` in a production build), but the exact final
raise site and a fix are not yet pinned — filed as a defect report with the
evidence needed to pick it up, not as a closed investigation.

## The symptom

Full hibernate-reactive suite, 3-GC-shard MySQL run, 2026-09-07
(`dev@e09041fb2`), local Windows box. 7 classes fail with the *same*
message shape wrapped in different outer exceptions
(`CompletionException` or bare `AssertionError`, depending on how the
async chain surfaces it):

```
java.lang.InternalError: JIT dispatch into <callee> failed: internal error:
  precise deoptimization unavailable for <callee> at bci <N>
  (can_deopt_resume=false (no deopt points, or an elided monitor),
   stashed key "<callee-signature>", inline callers 0, reason TransferToInterpreter);
  refusing side-effecting replay
```

Four distinct call sites, all `reason TransferToInterpreter`, all
`can_deopt_resume=false`:

| callee | bci | affected classes |
|---|---:|---|
| `ReactiveEntityInitializerImpl.reactiveInitializeEntityInstance` | 83 | `CascadeComplicatedToOnesEagerTest`, `EagerElementCollectionForEmbeddedEmbeddableTest`, `EagerElementCollectionForEmbeddableTypeListTest`, `EagerOrderedElementCollectionForEmbeddableTypeListTest` |
| `SqlClientConnection.selectIdentifier` (via `ReactiveConnection.selectIdentifier`) | 13 | `MultithreadedIdentityGenerationTest` |
| `ResultSetAdaptor.getString` (via `java.sql.ResultSet.getString`) | 11 | `schema.SchemaUpdateTest` |
| `UniOnTerminationCall$UniOnTerminationCallProcessor.onItem` (via `io.smallrye.mutiny.subscription.UniSubscriber.onItem`) | 29 | `OneToManyTest` |

17 occurrences total across the 3 GC arms × 3 shards (a class can hit this
more than once per run — one per parameterized/retried invocation).
Reproduces identically on Generational, G1, and ZGC — this is not a
GC-timing-dependent defect, it is a JIT deopt-machinery gap that any
collector's compiled code can hit.

## Root mechanism, confirmed from source

The message names its own gate. `can_deopt_resume` is a per-method flag
computed once, at compile time, in `jit/src/ir_lower.rs`:

```rust
// jit/src/ir_lower.rs ~15902
if sr_map.is_some()
    && cm._deopt_point_boxes.iter().any(|p| count_virtual_objects(&p.frame_state) > 0)
    && !graph.nodes.iter().any(|n| matches!(n.op, Op::MonitorEnter | Op::MonitorExit))
{
    cm.can_deopt_resume = true;
}
```

Every other path through `FrameState`/`CompiledMethod` construction leaves
`can_deopt_resume: false` (`jit/src/lib.rs:3332`, `:3418`). `sr_map` is only
populated under `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL` — two
non-default debug flags, per that function's own comment ("production is
unaffected"). **In a production build, with default flags — exactly this
run's configuration — `can_deopt_resume` is unconditionally `false` for
every compiled method, with no exceptions.**

When a compiled call needs to bail to the interpreter mid-flight (a trap:
here, `reason TransferToInterpreter`) and `can_deopt_resume` is `false`,
the VM's fallback is `try_resume_trapped_callee`
(`vm/src/jit/helpers.rs:4147`) — a *second*, independent recovery path that
does not itself require `can_deopt_resume`, but has its own chain of
refusal conditions (stash identity present and parseable, callee not
redefined, method signature matches the call site, callee class resolves,
method is not `ACC_SYNCHRONIZED`, resolved declaring class matches the
stash key). Every refusal in that chain is traced under
`CRATONVM_DBG_DEOPT` (`[cratonvm-deopt] callee-resume refused (<why>): ...`)
— **not captured in this run**, since the flag wasn't armed. When both the
compile-time `can_deopt_resume` gate and the runtime
`try_resume_trapped_callee` recovery decline, the VM has no safe way to
replay the trapped call and raises the `InternalError` above rather than
risk a double side effect — which is the right failure mode for a *library
internals* problem, but it is surfacing here in ordinary hibernate-reactive
application code, not a deliberately hostile or malformed program.

## What is NOT yet established

- **The exact reason `try_resume_trapped_callee` declined for these four
  call sites** — none of `CRATONVM_DBG_DEOPT`'s trace lines were captured
  this run. Re-running any one of the four with that flag armed is the
  fastest way to get past "some refusal in a five-way chain" to a named
  one.
- **Whether this is load-sensitive.** This run had unusually heavy
  concurrent load — 9 hibernate-reactive JVMs (3 arms × 3 shards) plus a
  concurrent, independent 12-JVM Spring Boot 3-GC suite on the same host —
  which affects JIT tiering timing (when a method gets compiled, and under
  what inlining/allocation state) and could change whether a given call
  site is even *reachable* while compiled. The `can_deopt_resume=false`
  mechanism itself is a permanent code property, not a load artifact — if
  a trap is hit while compiled, this fires regardless of load — but load
  may change *how often* the JIT has these specific methods compiled at
  the moment a trap-triggering input arrives. Not yet re-verified under a
  quiet, single-arm, single-JVM run.
- **Whether the four call sites share one fixable cause or are four
  independent instances of the same generic gate.** All four are
  first-invocation-shaped entity/connection/adapter code reached from
  hibernate-reactive's own reactive pipeline (`CompletionStage`/Mutiny
  `Uni` continuations) — plausibly the same class of trap (a
  monitor-adjacent or scalar-replacement-shaped bailout inside code the
  JIT chose to compile), but this has not been traced past the shared
  message shape.

## Distinguishing this from the already-fixed 2026-08-02 case

`docs/internal/fixed-bugs/unresumable-unconditional-trap-mvmap-FIXED-20260802.md`
(plain-text path — that directory is stripped from this repo's public git
history) fixed a different-but-related defect that produced the *same*
outer message text (`InternalError: ... refusing side-effecting replay`)
for `org.h2.mvstore.MVMap.evaluateMemoryForKey`. That case had **`reason
UnreachedCode`** and an **empty stashed key** (`stashed key ""`) — the
signature of a deopt frame that lost its method identity entirely. Today's
four call sites all show **`reason TransferToInterpreter`** with a
**populated, correctly-resolving stashed key** (the callee's own
signature) — a different `reason` value and a working stash, so this is
not that bug recurring. It is either a sibling gap in the same general
deopt-resume machinery, or a case the fixed bug's own `try_resume_trapped_callee`
refusal chain legitimately declines for a different, still-unnamed reason.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
JDK=/path/to/jdk-25
CRATONVM_DBG_DEOPT=1 <cratonvm> --java-home "$JDK" --Xmx 1500m \
  @common.args -Dcraton.batch=1 CratonRunner org.hibernate.reactive.OneToManyTest
# names which of try_resume_trapped_callee's refusal branches fires
```

The other three: `org.hibernate.reactive.CascadeComplicatedToOnesEagerTest`,
`org.hibernate.reactive.MultithreadedIdentityGenerationTest`,
`org.hibernate.reactive.schema.SchemaUpdateTest`.

Raw evidence (this run, untracked):
`apps/hibernate-reactive-suite-runner/runs/mysql-{default,g1,generational}-20260907-3gc-mysql-local/run-20260907-093320-passed/on-real/shard-{0,1,2}/raw.log`
— search for `@@TESTFAIL <class>` then the next `InternalError` line.

## Not in scope of this page

The bulk of this run's other 82 FAIL rows are the already-documented,
already-explained `vertx-junit5 @BeforeEach` 30-second checkpoint-budget
family (container start + `SessionFactory` build racing a fixed budget
under sharded concurrency) — see
`docs/internal/fixed-suite-bugs/hibernate-reactive/mysql-beforeeach-checkpoint-timeout-was-a-30s-budget-20260905-CLOSED.md`
(plain-text path; that doc explicitly predicts this exact recurrence under
a 3-arm × 3-shard MySQL run and says not to re-open it from one). Also
excluded: `techempower.TechEmpowerTest` (500 status — previously FIXED
2026-08-24, recurred today; not re-isolated this session, could be the
same load-sensitivity the fix's own repro notes already, needs a solo
rerun before treating as a regression) and the `HR000039: Flush during
cascade is dangerous` family on `MultithreadedInsertionTest`/
`MultithreadedInsertionWithLazyConnectionTest` (already tracked,
`hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`) and on
`FilterWithPaginationTest`/`QuerySpecificationTest` (not previously seen
with this exact exception on these two classes — worth a follow-up but not
chased down here). The 5 HANG classes
(`EagerElementCollectionForBasicTypeListTest`, `NoVertxContextTest`,
`MutinySessionTest`, `EagerElementCollectionForBasicTypeSetTest`,
`ImplicitSoftDeleteTests`) are identical across all three GC arms — GC-independent
and consistent with the same container-start/budget contention as the
checkpoint-timeout family, but not independently confirmed here.
