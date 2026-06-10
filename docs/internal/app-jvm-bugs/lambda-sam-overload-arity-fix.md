# Lambda SAM dispatch matched by name only — overloaded default methods mis-routed

## Symptom

Running JUnit5 tests via the JUnit Platform launcher, every @Test body fails to
run and is recorded as `java.lang.NullPointerException: Cannot invoke execute on
null`. With `CRATONVM_DBG_NPE_STACK=1` the null receiver is a `ThrowableCollector`
threaded through `ClassBasedTestDescriptor.instantiateAndPostProcessTestInstance`.

## Root cause

`org.junit.jupiter.engine.execution.TestInstancesProvider` declares **two
same-named methods**:

```java
// abstract SAM (what the lambda implements) — 3 args
TestInstances getTestInstances(ExtensionRegistry, ExtensionRegistrar, ThrowableCollector);
// default convenience overload — 2 args, delegates to the SAM
default TestInstances getTestInstances(MutableExtensionRegistry r, ThrowableCollector tc) {
    return getTestInstances(r, r, tc);
}
```

JUnit invokes the **2-arg default**. Its body re-invokes the **3-arg SAM**.

CratonVM's lambda-proxy dispatch matched the SAM by **method name only**
(`method_name == lcs.sam_method_name`). So the 2-arg default call was intercepted
as if it were the SAM and routed straight to the lambda body
(`lambda$testInstancesProvider$5`, which implements the 3-arg SAM) — **one
argument short**. The lambda body then captured its (uninitialised) trailing
`ThrowableCollector` parameter into a downstream `Supplier`; when that supplier
ran, the collector was `Uninitialized`/null and `throwableCollector.execute(...)`
NPE'd.

Confirmed via instrumentation: the 6-capture supplier proxy had
`cap[5] = Uninitialized` (the `ThrowableCollector` slot), because the outer SAM
body ran with a missing trailing arg.

## Fix

Intercept a lambda-proxy call as the SAM only when the supplied **argument count
matches** the SAM descriptor's parameter count — not just the name. Applied to
both dispatch paths:

- `vm/src/vm/vm_exec.rs` — `invoke_virtual` lambda-proxy branch.
- `vm/src/runtime/interpreter.rs` — `try_lambda_dispatch`.

When the arity doesn't match, dispatch falls through; a lambda-proxy receiver is
then routed to its `functional_interface` class, so the **real default method**
runs and re-invokes the SAM with the correct arity (which then matches and
dispatches to the lambda body correctly).

## Verification

- The minimal JUnit5 repro now reports `tests started=2 succeeded=2 failed=0`
  with `[alpha ran]`/`[beta ran]` printed (matching HotSpot); previously
  `succeeded=0 failed=2` with the `ThrowableCollector` NPE.
- Regression smoke: `Function`/`BiFunction`/`Supplier`, `Stream.map(String::length)`,
  `Collectors.toList/toMap`, and ServiceLoader method-reference streams all
  unaffected.
- A custom functional interface with an overloaded `default go(String)`
  delegating to a 2-arg SAM `go(String,String)` now returns the correct result
  (`Z|Z`) — directly exercising the fixed pattern.

## Note

Independent of, and complementary to, the DefaultClassDescriptor
`num_total_fields` allocation fix (branch fix/junit-classdescriptor-layout) and
the ServiceLoader.stream() Provider fix (branch fix/iface-method-dispatch). All
three were needed for the JUnit5 launcher path; this one makes the sample tests
actually pass.
