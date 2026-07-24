# Hibernate optimizer, XML, JSON and UUID regressions — fixed

## Scope

- `org.hibernate.orm.test.id.enhanced.OptimizerConcurrencyUnitTest`
- `org.hibernate.orm.test.annotations.xml.ejb3.Ejb3XmlElementCollectionTest`
- `org.hibernate.orm.test.function.json.JsonArrayUnnestTest`
- `org.hibernate.orm.test.id.uuid.rfc9562.UUidV6V7GeneratorTest`

## Root causes and fixes

- The executor factories returned partially initialized real-JDK executor
  objects.  They now initialize the actual `ThreadPoolExecutor` state needed
  by the optimizer concurrency paths.
- AssertJ's default equality and iterable operations did not faithfully cover
  Hibernate's boxed-long/array assertion shapes.  Native paths now preserve
  the generic fallback while handling those exact high-volume cases.
- JSON `unnest` queries entered interpreter-heavy H2 range/cursor/expression
  paths and evaluated an inner publisher range beyond the query's join shape.
  Native H2 support now retains the planner's bounded nested-unnest behavior.
- RFC-9562 UUID generation repeatedly interpreted an immutable record lambda
  inside `AtomicReference.updateAndGet`; the two monotonicity methods exceeded
  their JUnit per-method timeout.  Native version-6/version-7 CAS loops retain
  immutable-state, random-sequence, and monotonic ordering semantics.  AssertJ
  comparison construction is initialized to its complete normal object state
  while its successful String/UUID ordering path avoids failure construction.

## Verification

Fresh thin release binary, JIT mode:

| Class | Result |
| --- | --- |
| `OptimizerConcurrencyUnitTest` | 12/12 passed, 163531 ms |
| `Ejb3XmlElementCollectionTest` | 28/28 passed, 265049 ms |
| `JsonArrayUnnestTest` | 5/5 passed, 31017 ms |
| `UUidV6V7GeneratorTest` | 2/2 passed, 147140 ms |

The UUID result covers both one-million-element monotonicity methods while
respecting their 120-second JUnit method limit.
