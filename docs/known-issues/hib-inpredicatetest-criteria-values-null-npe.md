# Hibernate `InPredicateTest` — Criteria `In` predicate's value list is `null` at execution

| | |
|---|---|
| **Status** | 🔴 OPEN — not yet root-caused. Confirmed CratonVM-specific (HotSpot passes 1/1). |
| **Area** | JPA Criteria API — `CriteriaBuilder.in(...)` / `Predicate.In` value-list plumbing |
| **Symptom** | `java.lang.NullPointerException: Cannot invoke "java.util.Collection.size()" because "values" is null` |
| **Severity** | low — single class affected in this sweep. |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage (Azure host, dev `49aaf713`, real-JDK, JIT on, `TIMEOUT=1200`). |

## Symptom

`org.hibernate.orm.test.jpa.criteria.InPredicateTest` builds a
`CriteriaBuilder.in(...)` predicate and executes the resulting query. Setup
(schema DDL) succeeds; the failure happens during translation/execution of
the criteria query itself:

```
Hibernate:
    create global temporary table HTE_EVENT_TABLE(rn_ integer not null, id bigint, name varchar(255), primary key (rn_)) transactional
Hibernate:
    drop table HTE_EVENT_TABLE
@@FAIL org.hibernate.orm.test.jpa.criteria.InPredicateTest :: java.lang.NullPointerException: Cannot invoke "java.util.Collection.size()" because "values" is null
```

A local field/variable named `values` (the `In` predicate's candidate-value
collection) is `null` where Hibernate's criteria-to-SQM/HQL translation
expects a real (possibly empty) `Collection`. HotSpot passes this test
cleanly (`found=1 ok=1 failed=0`, Azure HotSpot baseline), confirming this is
CratonVM-specific — the entity/table setup succeeds, so this isn't a
schema/dialect issue, just something in the criteria-predicate object graph
that ends up with a null collection field on CratonVM where HotSpot has a
real (or empty) one.

## Hypothesis (not yet verified)

Given the identical entity/table plumbing works, the likely culprit is one
of:
- A varargs-to-`List`/array-to-`Collection` conversion CratonVM handles
  differently for the specific overload `CriteriaBuilder.in(Expression, Object...)`
  vs. `in(Expression, Collection)` used by this test.
- A field on the internal `In`/`InPredicate` implementation object that's
  supposed to default to an empty collection (e.g. `new ArrayList<>()` in a
  constructor) but is left `null` — possibly a constructor-initializer
  ordering bug (a pattern this session has seen before with `<clinit>`/
  `<init>` field-assignment ordering under JIT/loader-aware paths, though no
  direct evidence ties this to those mechanisms yet).

## Repro

Azure host, harness at `/home/victor/hibpkg/runner`:
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <(echo org.hibernate.orm.test.jpa.criteria.InPredicateTest) 0
```

## Next steps (not yet done)

- Read the actual test method body (`InPredicateTest` in the Hibernate ORM
  8.0 source) to identify exactly which `CriteriaBuilder.in(...)` overload
  and value-supply pattern (array/varargs/`Collection`/sub-`Expression`) is
  used, then trace which internal object's `values` field is null.
- Check with `--nojit` to rule out JIT involvement.
