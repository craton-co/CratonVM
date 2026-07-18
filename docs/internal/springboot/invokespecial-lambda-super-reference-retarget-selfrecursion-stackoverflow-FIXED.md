# `Interface.super::method` lambda references get retargeted onto the receiver's runtime class — infinite self-recursion StackOverflowError

**Status: FIXED 2026-07-18**

## Symptom

Module `module/spring-boot-micrometer-metrics`:

| Class | Failures from this cause |
|---|---:|
| `export.influx.InfluxPropertiesConfigAdapterTests` | 2/3 (`adaptInfluxV1BasicConfig`, `adaptInfluxV2BasicConfig`) |
| `export.influx.InfluxMetricsExportAutoConfigurationTests` | 3/4 (`autoConfiguresItsConfigAndMeterRegistry`, `allowsCustomConfigToBeUsed`, `allowsCustomRegistryToBeUsed`) |

Representative trace (`InfluxPropertiesConfigAdapterTests`, full log
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-673c36637ba0.out.log`):

```
=> java.lang.InternalError: JIT dispatch into java/util/function/Supplier.get()Ljava/lang/Object; failed: runtime error: StackOverflowError
   io.micrometer.influx.InfluxConfig.apiVersion(InfluxConfig.java:147)
   org.springframework.boot.micrometer.metrics.autoconfigure.export.influx.InfluxPropertiesConfigAdapter.lambda$apiVersion$0(InfluxPropertiesConfigAdapter.java:101)
   io.micrometer.influx.InfluxConfig.apiVersion(InfluxConfig.java:147)
   org.springframework.boot.micrometer.metrics.autoconfigure.export.influx.InfluxPropertiesConfigAdapter.lambda$apiVersion$0(InfluxPropertiesConfigAdapter.java:101)
   ... (repeats until the stack is exhausted)
```

## Root cause (CONFIRMED)

`InfluxPropertiesConfigAdapter` (Spring Boot source) overrides `apiVersion()` as:

```java
@Override
public InfluxApiVersion apiVersion() {
    return obtain(InfluxProperties::getApiVersion, InfluxConfig.super::apiVersion);
}
```

`InfluxConfig.super::apiVersion` is a method reference to the **interface
default method**, compiled by javac into a private synthetic method (also
named `lambda$apiVersion$0` in the adapter class purely by coincidence of
being each class's first `apiVersion`-related lambda) whose body does
`invokespecial InfluxConfig.apiVersion()` — reaching the interface default
while bypassing the adapter's own override, exactly as `Interface.super::method`
is specified to behave.

`InfluxConfig.apiVersion()` (real Micrometer 1.17.0-RC1 bytecode, confirmed
via `javap -c -v`) independently builds **its own** fallback-value `Supplier`,
bound via a `LambdaMetafactory` invokedynamic call site whose target method
handle is `REF_invokeSpecial io/micrometer/influx/InfluxConfig.lambda$apiVersion$0:()Lio/micrometer/influx/InfluxApiVersion;`
— a **different**, unrelated private synthetic method declared directly on
`InfluxConfig` itself (returns the hardcoded default `InfluxApiVersion.V1`).
`REF_invokeSpecial` means this target must always invoke exactly
`InfluxConfig.lambda$apiVersion$0`, regardless of the receiver's actual
runtime class — that is the entire point of `invokespecial`.

CratonVM's lambda dispatch does not honor that. Confirmed in
`vm/src/runtime/interpreter.rs:21395-21440` — the `MethodHandleKind::InvokeSpecial`
arm of `try_lambda_dispatch`. Its own comment (lines 21396-21398) states
*"Special: dispatch on the declaring class (no virtual lookup)"*, but line
21429 calls the **retargeting** helper:

```rust
let result = invoke_on_class_shared(shared, thread, class_id, &call_site.impl_handle.member_name, &call_site.impl_handle.descriptor, &full_args)?;
```

instead of the non-retargeting sibling `invoke_on_class_shared_no_retarget`
(`vm/src/vm/vm_exec.rs:13268-13285`), whose doc comment states it exists
precisely for "`MethodHandles.Lookup.findSpecial` and the default-method
super-call pattern... this never retargets... that is the whole point of
invokespecial." Three other call sites in the same file
(`interpreter.rs:20437`, `20900`, `21140`) correctly use the non-retargeting
variant for analogous non-virtual dispatch — line 21429 is the outlier.

The retargeting logic itself lives in `invoke_on_class_shared`
(`vm_exec.rs:13243-13260`) → the swap at `vm_exec.rs:13347-13397`: when the
receiver's actual class (`InfluxPropertiesConfigAdapter`) differs from the
method handle's declaring class (`InfluxConfig`), `InfluxConfig` is an
interface, the receiver is concrete, the method isn't static, and
`cm.is_subclass_of(receiver_class, declaring_class)` holds (the adapter does
implement `InfluxConfig`), `class_id` is silently swapped to the receiver's
own class (line 13385). `find_method_recursive` then resolves
`lambda$apiVersion$0` by name/descriptor **on `InfluxPropertiesConfigAdapter`**
— landing on the adapter's own same-named synthetic wrapper for
`InfluxConfig.super::apiVersion` instead of the interface's real
`lambda$apiVersion$0` — which calls back into `InfluxConfig.apiVersion()`,
which builds the same wrong-retargeted lambda again, forever, until the stack
overflows.

This is a two-classes-independently-generating-a-same-named-synthetic-lambda-method
collision made observable by a single dispatch-pinning bug; the underlying
defect (retargeting an `invokespecial`-bound lambda target) would misbehave
for any pair of classes with a same-named/same-arity private lambda helper
where one references `Interface.super::method` on the other.

## What to fix

`try_lambda_dispatch` now calls `invoke_on_class_shared_no_retarget` for
`MethodHandleKind::InvokeSpecial`, matching the sibling non-virtual dispatch
paths and preserving the method handle's implementation owner.

## Regression and validation

- Added `vm/tests/lambda_invokespecial_no_retarget.rs`, an end-to-end Java
  probe that creates the same synthetic-helper-name collision and verifies an
  `Interface.super::method` supplier returns the interface default value.
- The focused probe passed with JIT enabled and with `--nojit`.
- The real Spring Boot Micrometer classes both passed under the isolated VM:
  `InfluxPropertiesConfigAdapterTests` (3 tests) and
  `InfluxMetricsExportAutoConfigurationTests` (4 tests), in both JIT and
  disabled-JIT modes. The prior `StackOverflowError` marker is absent.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.influx.InfluxPropertiesConfigAdapterTests` (2 of 3 tests) |
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.influx.InfluxMetricsExportAutoConfigurationTests` (3 of 4 tests; the former separate `getMethods()` cluster is fixed, see [`../../internal/springboot/class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md`](../../internal/springboot/class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md)) |
