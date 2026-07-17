# `MBeanServer.getDomains()` not registered on CratonVM's synthetic platform MBeanServer — `AbstractMethodError`

**Status: OPEN — found 2026-07-17**

## Symptom

Module `module/spring-boot-integration`, class `IntegrationAutoConfigurationTests`
— 2 of its 9 failures share this identical shape:

```
JUnit Jupiter:IntegrationAutoConfigurationTests:enableJmxIntegration()
    => java.lang.AbstractMethodError: method javax/management/MBeanServer.getDomains()[Ljava/lang/String; has no Code attribute
       org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests.lambda$enableJmxIntegration$0(IntegrationAutoConfigurationTests.java:161)

JUnit Jupiter:IntegrationAutoConfigurationTests:customizeJmxDomain()
    => java.lang.AbstractMethodError: method javax/management/MBeanServer.getDomains()[Ljava/lang/String; has no Code attribute
       org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests.lambda$customizeJmxDomain$0(IntegrationAutoConfigurationTests.java:178)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-integration.org.springframework.boot.integration.autoconfigure.IntegrationA-9d4bdd63bbf0.out.log`

## Root cause (confirmed against source)

`native-builtins/src/jmx.rs::alloc_mbean_server` (~line 3034) allocates the
in-process platform `MBeanServer` as a bare-interface synthetic object:

```rust
fn alloc_mbean_server(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "javax/management/MBeanServer", MBS_NUM_FIELDS);
    ...
}
```

i.e. the object's runtime class **is** the bare `javax/management/MBeanServer`
interface, not a concrete implementing class. This is safe only for methods
that the same file explicitly registers as natives on that class
(confirmed present: `getMBeanCount`, `registerMBean`, `getObjectInstance`,
`queryNames`, and others). `getDomains()` — the `MBeanServer` method that
returns the set of distinct domain names of all currently-registered
MBeans — has **no such registration anywhere in `jmx.rs`** (grepped the
whole file for `"getDomains"` and `getDomains`: zero hits besides the call
sites that fail). Any call to `MBeanServer.getDomains()` on this synthetic
object therefore falls straight through to the bare interface's abstract
method declaration, which carries no `Code` attribute, producing
`AbstractMethodError`.

This is the **same general defect shape** as two previously-fixed sibling
bugs (`docs/internal/fixed-suite-bugs/x509trustmanager-getacceptedissuers-abstractmethod-FIXED.md`,
`docs/internal/fixed-suite-bugs/SC-stream-collector-supplier-no-code.md`)
and this rerun's own
[`hateoas-stream-reduce-triarg-missing-native-abstractmethoderror.md`](hateoas-stream-reduce-triarg-missing-native-abstractmethoderror.md) —
a synthetic/native object stamped with (or falling back to) a bare interface
type, where one specific method of that interface was never given a native
registration. Fix shape, by precedent: register
`getDomains()Ljava/lang/String;` (actually `()[Ljava/lang/String;`) on the
synthetic `MBeanServer`, deriving the domain set from the existing
`MBS_NAMES`/`MBS_ONAMES` parallel-array registry (`native-builtins/src/jmx.rs`
~line 3016-3031) the same way `queryNames`/`getObjectInstance` already do.

## Other failures in this class (not clustered here)

`IntegrationAutoConfigurationTests` has 3 more distinct failures this
session did not root-cause and is not claiming as part of this bug:

- `defaultPoller()`: `AssertionFailedError: expected: -2147483648L but was: -1125897759358976L`
- `whenCustomPollerPropertiesAreSetThenTheyAreReflectedInPollerMetadata()`: `AssertionFailedError: expected: 1L but was: -1125899906842623L`
- `taskSchedulerIsNotOverridden()`: expects `scheduledExecutor.corePoolSize` == 3, got 1

The first two are suspicious — both "got" values are large negative longs
extremely close to `-(2^50)` (within `2^31` of it), while the expected
values are small (`Integer.MIN_VALUE` and `1`), which does not look like an
ordinary off-by-something arithmetic bug and might indicate a
field-slot/tagged-value decode issue on `PollerMetadata`'s `long` fields —
but this is speculation with zero source-level investigation behind it and
is flagged here only so it isn't lost, not asserted as a confirmed or even
credible root cause. The third looks like it could be a legitimate
scheduler-configuration behavioral difference rather than a VM bug. None of
the three are filed as their own doc.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-integration` | `org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests` (2 of 9 failing test methods: `enableJmxIntegration`, `customizeJmxDomain`) |
