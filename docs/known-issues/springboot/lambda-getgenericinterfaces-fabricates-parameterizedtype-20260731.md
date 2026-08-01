# `Class.getGenericInterfaces()` on a lambda proxy fabricates a `ParameterizedType` where real HotSpot always returns the raw `Class`

**Status: OPEN — found 2026-07-31**

## Symptom

Two `core/spring-boot` classes fail on CratonVM but pass on real HotSpot
(`jdk-25.0.3.9-hotspot`) with the identical classpath and JVM args, verified
with a single-method rerun via `SbRunnerMethod`:

`org.springframework.boot.convert.ApplicationConversionServiceTests` — 6/18 failures:
- `addBeansWhenHasParserBeanMethodAddParser` / `addBeansWhenHasPrinterBeanMethodAddPrinter` /
  `addBeansWhenHasConverterBeanMethodAddConverter` → uncaught `java.lang.IllegalArgumentException`
  (a Mockito-stubbed `willThrow` that should never fire, because the adapter path should have
  been taken instead of the raw `registry.addConverter(bean)` path).
- `addConverterBeanWithTypeConvertsUsingTypeInformation` → `ConverterNotFoundException: No
  converter found capable of converting from type [java.lang.String] to type
  [ApplicationConversionServiceTests$ExampleRecord]` — the registered adapter converter is
  never found by `GenericConversionService`.
- `addPrinterBeanWithTypeConvertsUsingTypeInformation` → `AssertionError: Expecting code to
  raise a throwable` — a conversion that should be rejected (wrong record type) instead silently
  succeeds.
- `addParserBeanWithTypeConvertsUsingTypeInformation` → wrong exception type
  (`ConversionFailedException` instead of the expected `ConverterNotFoundException`).

`org.springframework.boot.util.LambdaSafeTests` — 1/31 failure:
- `callbackWithLoggerShouldUseLogger` → `Wanted but not invoked: log.debug(contains("Non-matching
  CharSequence type..."), any(Throwable))` / `Actually, there were zero interactions with this mock.`

## Root cause

Confirmed with a standalone probe (`ConvProbe.java` / `LambdaGenProbe2.java`, run against both
`cratonvm.exe` and real `jdk-25.0.3.9-hotspot` with an identical classpath):

```java
Converter<CharSequence, ExampleRecord> concrete = (source) -> new ExampleRecord(source.toString());
for (var t : concrete.getClass().getGenericInterfaces()) System.out.println(t);
```

- **Real HotSpot:** always prints `interface org.springframework.core.convert.converter.Converter`
  (the **raw** `Class`) — for *every* lambda, regardless of whether the target variable/field type
  is concretely parameterized (`Converter<CharSequence, ExampleRecord>`) or a wildcard
  (`Converter<?, ?>`). This is fundamental JDK behavior: `LambdaMetafactory`-spun implementation
  classes carry no `Signature` attribute at all, so `Class.getGenericInterfaces()` can never
  produce a `ParameterizedType` for them.
- **CratonVM:** prints `org.springframework.core.convert.converter.Converter<java.lang.Object,
  java.lang.Object>` — a real `ParameterizedTypeImpl` — even for the wildcard-typed
  `Converter<?, ?>` case, where the erased/instantiated call-site descriptor is `(Object)Object`.

The fabrication happens in `native_class_get_generic_interfaces`
(`native-builtins/src/lang_class.rs:13985`), which for a lambda proxy calls
`lambda_functional_interface_generic_type` (`native-builtins/src/generics.rs:930`). That function
resolves each of the SAM interface's root type parameters from the lambda's call-site
**instantiated method descriptor** (`native-builtins/src/generics.rs:1043-1052`) and always treats
the result as a concrete, resolved type argument — including when the instantiated descriptor's
`Object` is itself just the erasure fallback (a wildcard or raw-typed target has no concrete
binding to instantiate against; `Object` there is *not* a real resolved type, but the code cannot
tell the difference and reifies it into `ParameterizedType(..., Object, Object)` anyway).

This machinery was added deliberately in a previous session (see
`docs/internal/fixed-suite-bugs/springboot/core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md`,
"Residual 3" — fixed `SpringBootContextLoaderAotTests`' `AotApplicationContextInitializer` case,
where `GenericTypeResolver.resolveTypeArgument` threw `IllegalStateException` on a raw `Class`).
That fix is real and was not reverted; the probe above shows it is simply **based on an incorrect
premise about real-JDK lambda reflection** — HotSpot never exposes a `ParameterizedType` here, so
building one (even a fully-correct, concretely-resolved one) is itself observably wrong, and it
now breaks two different, unrelated Spring idioms that depend on lambda generics being reported as
**unresolvable**:

- `ApplicationConversionService.addBean()` (`ApplicationConversionService.java:389-397`) branches
  on `ResolvableType.forInstance(bean).as(type).hasUnresolvableGenerics()` to decide whether to
  register a type-aware `BeanAdapter` (using the `ResolvableType` supplied by the bean
  *definition*, which is correct) or the raw bean directly. On CratonVM this now reports
  `false` for a lambda, so the raw (wrong) path is taken.
- `LambdaSafe.GenericTypeFilter.match()` (`LambdaSafe.java:364-380`) resolves the callback's type
  argument via `ResolvableType.forClass(callbackType, callbackInstance.getClass())` and uses it to
  **pre-filter** the call, skipping invocation (and the deliberate erasure-driven
  `ClassCastException` the class's own javadoc says it exists to catch) whenever the resolved
  generic doesn't match the argument's runtime type. On CratonVM the filter now successfully
  resolves `T=StringBuilder` (real HotSpot cannot — lambdas are raw) and rejects the call up front,
  so the expected `ClassCastException` → `logger.debug(...)` path never runs.

## Affected classes
- `core/spring-boot` — `org.springframework.boot.convert.ApplicationConversionServiceTests`
- `core/spring-boot` — `org.springframework.boot.util.LambdaSafeTests`
