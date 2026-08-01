# HQL ordinal parameter silently dropped — `ordinal parameters []` under JIT

**Status:** CLOSED 2026-07-31. Two VM defects were found and fixed; the witness
class is green. The reported one-in-fourteen event itself was **never
reproduced** — read [What is and is not proven](#what-is-and-is-not-proven)
before citing this as a root-cause writeup.

**Original witness:** `org.hibernate.orm.test.hql.ASTParserLoadingTest#testComponentNullnessChecks`,
JIT mode, 1 failure in 14 runs (2026-07-31, `cratonvm-antlrfix-20260731.exe`).

**Fixed on:** `fix/hib-hql-ordinal-param-20260731`.

## Acceptance

| | runs | `ASTParserLoadingTest` | `ordinal parameters []` |
| --- | --- | --- | --- |
| baseline (dev tip) | 24 (22 valid) | **103/106 on every one** | 0 |
| after the fix | 24 | **106/106 on 18**; 105/106 on the other 6 | 0 |
| after re-merging `dev` | 6 | **106/106 on every one** | 0 |

HotSpot runs the class 106/106 in 18.8s, so 106 is the right target.

`dev` advanced by twenty-odd commits during the work — including one that
disables every JIT ban, which changes what gets compiled — so the branch was
re-merged and everything re-run on the result: the six runs above, plus 120/120
PASS over a `passed.txt` slice, 100 000 clean iterations of
`FunctionalInterfaceHijackProbe`, and the `native-collections` unit tests. A
107-class slice on the pre-merge binary was also clean.

The six 105/106 runs were one round, all failing the *same* test
(`testJpaTypeOperator`) with the *same* cause — JUnit's 120s per-test timeout —
while a full `cargo build` saturated all 32 cores alongside six concurrent VMs.
Those runs took 761s against ~600s for every other round, and the rounds before
and after them were 6/6 clean on the same binary. That is host load, not a
regression; the same round is the only place a rare
`NoSuchMethodError: java/lang/Integer.getTypeName()` warning appeared (recovered,
no test failed), and it too is absent from every unloaded round.

## Symptom as reported

```
java.lang.IllegalArgumentException: No parameter labelled '?1' in query with ordinal parameters []
  org.hibernate.query.internal.ParameterMetadataImpl.getQueryParameter(ParameterMetadataImpl.java:306)
  org.hibernate.query.internal.QueryParameterBindingsImpl.getBinding(QueryParameterBindingsImpl.java:151)
  org.hibernate.query.internal.AbstractCommonQueryContract.setParameter(AbstractCommonQueryContract.java:1126)
Caused by: org.hibernate.query.UnknownParameterException
```

`from Human where ?1 is null` parsed — no `SyntaxException`, no error listener
fired — but the ordinal parameter never reached `ParameterMetadataImpl`.

The empty bracket list is the informative half. `getOrdinalParameterLabels()`
returns `emptySet()` only when `queryParametersByPosition` is `null`, and that
happens only when the interpretation was built from a statement whose
`getSqmParameters()` was empty. A parameter that survived the parse was lost
between the parse tree and the statement's parameter set — not mis-bound, not
mis-typed: **absent**.

## The hunt

`run-astparser-hunt.sh` (new, tracked) runs the witness class six-up and greps
every log for the signature. `run-astparser-witness.sh` runs it serially, which
is right for a gate and hopeless as a hunt: at ~1 hit per 14 runs of a 3-12
minute class, a serial search costs hours per expected hit.

**24 runs on the dev-tip baseline reproduced the signature zero times.** They did
something more useful. Every one of the 22 that completed failed *three other
tests*, deterministically, which the original report does not mention:

```
java.lang.NoSuchMethodError: org.hibernate.sql.results.graph.embeddable.internal.EmbeddableInitializerImpl.add(Ljava/lang/Object;)Z
  org.hibernate.sql.results.graph.internal.AbstractInitializer.startLoading(AbstractInitializer.java:26)
  Suppressed: java.lang.NoSuchMethodError: org.h2.jdbc.JdbcPreparedStatement.add(Ljava/lang/Object;)Z
    org.hibernate.resource.jdbc.internal.ResourceRegistryStandardImpl.releaseResources(...:304)
```

— in `testPaginationWithPolymorphicQuery`, `testRowValueConstructorSyntaxInInList`
and `testImplicitPolymorphism`. HotSpot runs the class 106/106 in 18.8s; a
`--nojit` run is also 106/106; each of the three passes on its own under
`MethodRunner`, JIT or not. That is defect 1 — and it is the same *family* as
the report: a lambda body that silently does not run.

## Defect 1 — a user lambda routed to the collector natives (FIXED)

`Collector.accumulator()` and its three siblings return a synthetic object whose
class name IS the SAM interface (`make_collector_fn` →
`alloc_synthetic(ctx, "java/util/function/BiConsumer", 1)`), carrying the source
`Collector` in field 0. The natives that serve those objects are therefore
registered on the **public interface names**:

```
java/util/function/Supplier.get()Ljava/lang/Object;
java/util/function/BiConsumer.accept(Ljava/lang/Object;Ljava/lang/Object;)V
java/util/function/Function.apply(Ljava/lang/Object;)Ljava/lang/Object;
java/util/function/BinaryOperator.apply(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;
```

A lambda proxy's class id is synthetic and absent from the class store. When the
JIT's generic dispatch arm (`jit_invoke_dispatch`, the MIC-miss / megamorphic
path) met one, `virtual_dispatch_target_for_receiver` fell back to the
constant-pool class — the functional interface — and `invoke_or_native` resolved
those natives. None of them can decline: with no collector tag in field 0 they
take their catch-all arm.

| SAM | the lambda body | what ran instead |
| --- | --- | --- |
| `Supplier.get()` | — | returns a fresh empty `ArrayList` |
| `Function.apply(x)` | — | returns `x` |
| `BiConsumer.accept(a, b)` | — | `a.add(b)` |
| `BinaryOperator.apply(a, b)` | — | `a.addAll(b)`, returns `a` |

Three of the four are **silent**. The fourth is what made this findable:
`a.add(b)` on a first argument with no `add` is the `NoSuchMethodError` above,
and the class it names is whatever object happened to be the SAM's first
argument — a sub-initializer in one case, an H2 `PreparedStatement` in the other.
Neither has anything to do with collections, which is why the message reads as
nonsense until you know what produced it.

Two ingredients are required, and the witness has both:

* **compiled** — the interpreter resolves a lambda-proxy receiver through the
  proxy registry (`try_lambda_dispatch`) *before* native resolution, so it never
  reaches these natives. Hence JIT-only.
* **megamorphic** — a site that only ever sees one proxy class stays on a cached
  path. Hibernate's `forEachSubInitializer(BiConsumer, InitializerData)` is
  called with `Initializer::startLoading`, `::resolveKey` and
  `::initializeInstance`, which is exactly that shape.

**Fix:** `try_lambda_proxy_sam_dispatch` in `vm/src/jit/helpers.rs` — a
lambda-proxy receiver is dispatched through the proxy registry, never by name.
It guards **both** by-name dispatch bails (the generic slow path and
`bail_to_interpreter`) as one shared helper rather than a per-site patch, so a
third by-name path added later inherits the guard instead of re-opening the
hole. This is what the interpreter and the sibling interface helper already did.

Three of those four natives also read field 0 of the receiver before checking
that the receiver *has* a field 0 — on a non-capturing lambda proxy that is an
out-of-bounds read which captures a full Rust backtrace per call.
`collector_fn_source` asks the layout first.

**Witness:** `apps/hib-suite-runner/FunctionalInterfaceHijackProbe.java`. One
shared, hot dispatch method per interface, fed a rotation of five distinct
lambdas. Before: first failure at iteration 524, all four interfaces. After:
100 000 iterations clean. `--nojit` and HotSpot were always clean.

## Defect 2 — a relocated map's integer overlay evicted on address reuse (FIXED)

CratonVM stores a fresh exact-class `HashMap` with boxed-Integer keys in a
Rust-side overlay (`hm_int_fast`) rather than in heap nodes. A thread-local
single-entry memo caches `(raw pointer, identity hash) -> overlay key`, and
`HashMap.<init>` evicts the overlay entry the memo names whenever the new map's
raw address equals the memo's — on the premise that the address's previous
tenant must be dead.

Under a copying young collection it need not be. A surviving map is evacuated
(or promoted) and its old address handed straight back to the allocator; the
next `new HashMap()` that lands there matches the memo and drops the entry. The
overlay is that map's *only* storage — `try_hm_int_fast_put` adopts a map only
while its heap `size` is 0, and never writes nodes — so the still-live map reads
empty from that moment on: `get` returns null, `size()` returns 0, `keySet()`
iterates nothing, and nothing throws.

That is the reported symptom's shape exactly.
`ParameterMetadataImpl.queryParametersByPosition` is a fresh exact-class
`HashMap<Integer, QueryParameterImplementor>` filled from position 1, and losing
it renders `No parameter labelled '?1' in query with ordinal parameters []`
verbatim — both halves, the failed `get` and the empty label list.

**Fix:** the reverse owner index already distinguishes the two cases and is
already maintained across a collection — `gc_update_collection_overlay_refs`
re-keys a relocated collection from its old address to its new one, and
`clear_overlay_entries_for_key` drops it when the collection dies. So a moved
tenant is no longer recorded at its pre-move address, while a dead-but-unpruned
one still is. `overlay_owner_still_at` asks that question and the purge runs
only on a yes. Three unit tests cover the discriminator.

**Reachability, measured:** a new `CRATONVM_DBG_HMINIT_PURGE` reports every
recycled-address eviction the constructor considers, and whether the previous
tenant was actually dead. It did not fire **once** — neither across a Hibernate
query-path probe nor across `IntMapOverlayWipeProbe`, which exists to provoke
precisely this sequence. So this is filed as latent-hazard hardening. It is not
attributed to any observed failure.

## What is and is not proven

**Proven.** Defect 1 is reproduced, root-caused, fixed and regression-gated. It
broke three tests of the witness class on every JIT run, and it silently skips
lambda bodies VM-wide, not just here. Defect 2 is a real correctness hole with
the reported symptom's exact shape, fixed and unit-tested.

**Not proven.** Neither is demonstrated to be *the* cause of the 2026-07-31
event. The signature did not reproduce in 24 baseline runs, so there was nothing
to bisect against. And the parameter-collection path
(`SemanticQueryBuilder.resolveParameter` → `AbstractSqmStatement.addParameter` →
`DomainParameterXref` → `ParameterMetadataImpl`) contains **no**
`Supplier`/`Function`/`BiConsumer`/`BinaryOperator` call that defect 1 could
have hijacked: its one `Function` use — `DomainParameterXref`'s
`computeIfAbsent(queryParameter, impl -> new ArrayList<>())` — would have failed
loudly, not silently, had it been hijacked. Defect 2 fits the symptom
mechanically but never fired under instrumentation.

Do not cite this document as "the ordinal drop was caused by X." If the
signature recurs, the probes below are the tools, and the honest prior is that a
third defect exists.

## Regression gates

| Probe | What it catches |
| --- | --- |
| `FunctionalInterfaceHijackProbe.java` | defect 1, all four interfaces, seconds |
| `IntMapOverlayWipeProbe.java` | defect 2's sequence end to end |
| `ParamMapInvariantStress.java` | a silent drop in any java.util collection the parameter passes through — millions of trials per minute |
| `HqlParamBindProbe.java` | the reported symptom end to end through a real `SessionFactory`, ~3/second |
| `run-param-probe.sh`, `run-astparser-hunt.sh` | matrix and parallel drivers for the last two |
| `overlay_owner_liveness_tests` (Rust) | defect 2's dead-vs-relocated discriminator |

`HqlParseStress` remains the parse-level gate. It cannot see either defect, and
the original report already established that the parameter survives the parse.

## Notes for whoever picks this up

* `CRATONVM_GC_STRESS` multiplies minor collections, but the moving-young
  verifier emits nothing in any configuration tried, and under the JIT every
  young collection in a Hibernate workload diverts to the non-moving sweep (see
  `moving-young-inert-under-jit-throughput-tax-20260730.md`). A green GC-stress
  arm has not exercised the moving collector. `[GC] moving_young: cycles=N`
  counts only moving collections taken while a JIT frame is live, so `cycles=0`
  under `--nojit` means nothing either way.
* `CRATONVM_DBG_CCE_BT` now dumps the frame stack for **every** dispatch
  `NoSuchMethodError`, not only those whose receiver resolved to bare
  `java/lang/Object`. The old class-name test made it blind to exactly the
  interesting case — a mis-resolved receiver that lands on a real object of an
  unrelated class — which is how defect 1 presented.
* The Rust backtrace printed above the out-of-bounds-field-read warning is what
  named `native_collfn_accumulator_accept`. When a Java-level failure makes no
  sense, read the VM's own diagnostic immediately preceding it.
