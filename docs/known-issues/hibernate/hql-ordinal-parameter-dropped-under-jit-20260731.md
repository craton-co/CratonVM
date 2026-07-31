# HQL ordinal parameter silently dropped — `ordinal parameters []` under JIT

**Status:** OPEN — observed once, cause not yet located. Filed so the signature
is searchable and the probe that can catch it exists.

**Witness:** `org.hibernate.orm.test.hql.ASTParserLoadingTest#testComponentNullnessChecks`,
JIT mode, 1 failure in 6 runs (2026-07-31, `cratonvm-antlrfix-20260731.exe`).

## Symptom

```
java.lang.IllegalArgumentException: No parameter labelled '?1' in query with ordinal parameters []
  org.hibernate.query.internal.ParameterMetadataImpl.getQueryParameter(ParameterMetadataImpl.java:306)
  org.hibernate.query.internal.QueryParameterBindingsImpl.getBinding(QueryParameterBindingsImpl.java:151)
  org.hibernate.query.internal.AbstractCommonQueryContract.setParameter(AbstractCommonQueryContract.java:1126)
Caused by: org.hibernate.query.UnknownParameterException
```

The query is `from Human where ?1 is null` (H2Dialect branch of the test). It
parsed — there is **no** `SyntaxException` and no error listener fired — but the
ordinal parameter never reached `ParameterMetadataImpl`, so the subsequent
`setParameter(1, …)` has nothing to bind to.

## Why this is filed separately from the ANTLR root defect

It was found while verifying
`docs/internal/fixed-suite-bugs/hibernate/antlr-native-roots-moving-young-hql-misparse-20260730-FIXED.md`,
and it is tempting to fold it in. It is a different shape:

* That defect's symptom is a **rejected** parse — `SyntaxException: … no viable
  alternative` on valid HQL. Across all twelve witness runs (six JIT, six
  `--nojit`) on the converted binary there were **zero** `SyntaxException`s.
* This one is a parse that **succeeds** with a production missing.

Nothing rules out a shared cause, but attributing it to the ANTLR root fix
without evidence would be exactly the mistake the 2026-07-29 investigation made
when it blamed (and deleted) the trivial-accessor fast path.

## What is known

* Only reproduced in JIT mode. Never seen in six `--nojit` runs of the same
  class, nor in the 141-class `others` corpus (`--nojit`), nor on the dev
  baseline in any arm run so far.
* The failing run had 6 `[moving-young] fallback` warnings versus 1-4 in the
  passing runs — i.e. *more* of its young collections ran the NON-MOVING sweep,
  which is the opposite of what a relocation defect needs. Weak signal, one
  sample.
* Reproduction rate is roughly 1 in 6 full-class runs, each ~7 minutes. That is
  too slow to bisect against.

## How to hunt it

`apps/hib-suite-runner/HqlParseStress.java` now carries four parameterized
queries (`?1`, `?2`, `:first`, `:n`) and asserts that **every parameter marker
in the source survives into `statement().getText()`** — a check that catches a
silently missing production, which a syntax-error count cannot. Run it with
`run-hql-stress.sh` across the JIT × GC-stress matrix; it does thousands of
parses per process in ~3 minutes.

**That probe does not reproduce it.** 250 iterations × 14 queries (3500 parses,
including four parameterized ones) across jit/`--nojit` × default/`GC_STRESS=4M`
reported `misparsed=0` on **both** the dev baseline and the converted binary,
with identical results per arm. So at the parse-tree level the parameter
survives. That points downstream of ANTLR — Hibernate's `SemanticQueryBuilder` /
`ParameterCollector`, or the query-interpretation cache handing back another
query's SQM — rather than at the parse itself.

The next step is a `MethodRunner`-based loop over `testComponentNullnessChecks`
alone (`MethodRunner <class> <method>` runs a single JUnit method, which is far
cheaper than the ~7-minute full class), and if that reproduces, dumping the
parse tree and the collected parameter set at the point of failure. Note the
one observed failure came after ~100 sibling tests had run, so a
single-method loop may not be enough to provoke it.

**Caution for whoever picks this up:** `CRATONVM_GC_STRESS` multiplies minor
collections (measured: 3 → 21 → 595 over the same workload) but the
moving-young **verifier emits nothing** in any configuration tried, so there is
no evidence those extra collections relocate anything. Under the JIT that is
expected — see
`moving-young-inert-under-jit-throughput-tax-20260730.md`, which shows every
young collection in a JIT-enabled Hibernate workload diverting to the
non-moving sweep. Do not treat a green GC-stress arm as having exercised the
moving collector, and note that `[GC] moving_young: cycles=N` only counts
moving collections taken *while a JIT frame is live* (`gen_heap.rs`,
`record_moving_young_cycle`), so `cycles=0` under `--nojit` is expected and
means nothing either way.
