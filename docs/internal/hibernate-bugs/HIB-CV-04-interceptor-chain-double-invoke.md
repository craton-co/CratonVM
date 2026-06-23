# HIB-CV-04 — `@Jpa`/`@SessionFactory` tests fail: "Chain of InvocationInterceptors called invocation multiple times"

**Severity:** High — the single largest CratonVM-only failure bucket (~27% of recorded classes in the early JIT-on run; every `@Jpa`/`@SessionFactory` test that opens a transaction)
**Status:** ✅ RESOLVED — this was a downstream cascade of HIB-CV-06 (the `Properties` system-property
pollution corrupted the bootstrap password; the EMF bootstrap then threw/re-entered during the
slow connect, tripping `ValidatingInvocation`'s once-only check). Fixed by the HIB-CV-06
system-Properties marker fix. `DeleteDecomposerTest` now 12/12 PASS, `MiniJpaT` ok=1.

**(original investigation below)**
**Status (orig):** OPEN (root-caused to a double test-method invocation; exact re-entry point not yet isolated)
**HotSpot:** not affected (same classes pass)

## Symptom

Most Hibernate ORM tests fail every method with:

```
org.junit.platform.commons.JUnitException: Chain of InvocationInterceptors called
  invocation multiple times instead of just once: org.junit.jupiter.engine.extension.TimeoutExtension
```

`found=N started=N ok=0 failed=N` — the methods start but each fails the chain validation.

## Root cause (mechanism)

`InvocationInterceptorChain$ValidatingInvocation` wraps the base (real) test-method
invocation. Its `markInvokedOrSkipped()` does:

```java
if (!invokedOrSkipped.compareAndSet(false, true)) {
    fail("Chain of InvocationInterceptors called invocation multiple times instead of just once");
}
```

The `fail()` message appends the interceptor list (just `[TimeoutExtension]` here — that
name is **context, not the culprit**). The CAS returns `false` because the base invocation's
`proceed()` reaches `ValidatingInvocation` **twice** for a single `started` test. So CratonVM
drives the test-method invocation chain to the base **two times** where HotSpot drives it once.

- Not JIT: reproduces with `CRATONVM_DISABLE_JIT=1`.
- `AtomicBoolean.compareAndSet` is **not** broken in general (plain `@ParameterizedTest`
  classes pass — they also go through `ValidatingInvocation`).

## Minimal reproduction

```java
@Jpa(annotatedClasses = { MiniJpaT.E.class })
public class MiniJpaT {
    @Test void empty(EntityManagerFactoryScope scope) { scope.inTransaction(em -> {}); }  // FAILS "multiple times"
    @Entity(name="ET") static class E { @Id Integer id; E(){} E(int i){id=i;} }
}
```

Bisection of the trigger:

| Probe | body | result |
|-------|------|--------|
| `MiniJpaC` | `@Jpa`, body just increments a counter (no scope use) | **PASS** (body runs once) |
| `MiniJpaT` | `@Jpa`, `scope.inTransaction(em -> {})` (empty txn) | **FAIL** "multiple times" |
| `MiniJpa1` | `@Jpa`, `scope.inTransaction(em -> em.persist(new E(1)))` | **FAIL** "multiple times" |
| `IdGeneratorOverridingTest` | `@Jpa`, `scope.inTransaction(em -> em.persist(new B()))` | **PASS** (3/3) — anomalous exception |

So the trigger is the **`EntityManagerFactoryScope` first real use → lazy EMF/transaction
bootstrap** inside the test body. Bootstrap does service loading + XSD work + an H2
connection (the `H2DatabaseCleaner` runs here). One of those steps re-enters the JUnit
method-invocation path, so the base invocation's `proceed()` fires a second time.
(`IdGeneratorOverridingTest` passing despite the same shape is unexplained — it is the
exception, not the rule.)

## Suspected area / follow-up

Something during the in-test EMF/transaction bootstrap re-enters the interceptor chain. Candidate
threads to pull:
- A caught error during bootstrap (e.g. the `ServiceLoader$Itr.hasNext()Z` `NoSuchMethodError`
  that fires on every bootstrap — see the suite logs) whose exception unwinding through the
  lambda/`MethodHandle` interceptor chain causes `proceed()` to be re-executed.
- The `EntityManagerFactoryExtension` lifecycle (`TestInstancePostProcessor` /
  parameter resolution for `EntityManagerFactoryScope`) running the invocation lambda twice.

Next step: instrument `InvocationInterceptorChain.chainAndInvoke` / the `ReflectiveInterceptorCall`
lambda to log each `proceed()` entry with a stack capture, run `MiniJpaT`, and identify the second
entry's origin.

## Impact

Because nearly all Hibernate ORM tests use `@Jpa`/`@SessionFactory` + a transaction, this single
defect accounts for the large majority of CratonVM-only test failures in this suite. Fixing it
should move a large fraction from FAIL → PASS.
