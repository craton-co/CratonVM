# `Class.getGenericInterfaces()` on a lambda proxy fabricated a `ParameterizedType` where real HotSpot always returns the raw `Class`

**Status: FIXED — 2026-08-01** (found 2026-07-31)

Branch `fix/lambda-generic-interfaces-20260801`, commit
`fix(reflect): lambda getGenericInterfaces() returns the raw Class, as HotSpot does`.
Worktree `/data/data/wt-lambdagen-20260801` on the Azure host; probes and sweep
results under `/data/data/lambdagen-20260801/`.

## Symptom

Two `core/spring-boot` classes failed on CratonVM but passed on real HotSpot
(`jdk-25.0.3.9-hotspot`) with the identical classpath and JVM args.

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
  raise a throwable` — a conversion that should be rejected (wrong record type) instead
  silently succeeded.
- `addParserBeanWithTypeConvertsUsingTypeInformation` → wrong exception type
  (`ConversionFailedException` instead of the expected `ConverterNotFoundException`).

`org.springframework.boot.util.LambdaSafeTests` — 1/31 failure:
- `callbackWithLoggerShouldUseLogger` → `Wanted but not invoked: log.debug(contains("Non-matching
  CharSequence type..."), any(Throwable))` / `Actually, there were zero interactions with this mock.`

## Root cause

`native_class_get_generic_interfaces` (`native-builtins/src/lang_class.rs`) special-cased
lambda proxies and — since a previous session's "Residual 3" fix — reconstructed a concrete
`ParameterizedType` for the lambda's functional interface out of the call-site
**instantiated method descriptor**, via `lambda_functional_interface_generic_type`
(`native-builtins/src/generics.rs`).

**Real HotSpot never does this.** A `LambdaMetafactory`-spun implementation class carries no
`Signature` attribute at all, so `Class.getGenericInterfaces()` hands back the RAW
functional-interface `Class` for every lambda — regardless of how concretely the call site's
target type was parameterized. Confirmed by a standalone differential probe
(`LambdaGenProbe.java` / `LambdaGenProbe2.java`, run against both `cratonvm` and
`jdk-25.0.3.9-hotspot` with an identical classpath):

```
                                          HotSpot                        CratonVM (before)
Converter<CharSequence,ExampleRecord> l   interface ...Converter (Class)  ...Converter<CharSequence,ExampleRecord>
Supplier<String> l                        interface ...Supplier  (Class)  ...Supplier<java.lang.String>
Function<String,Integer> String::length   interface ...Function  (Class)  ...Function<java.lang.String,java.lang.Integer>
hasUnresolvableGenerics(converter)        true                            false
```

The premise the previous fix rested on was also wrong. It was added because
`GenericTypeResolver.resolveTypeArgument(lambdaClass, ApplicationContextInitializer.class)`
returned `null` under CratonVM, tripping `SpringApplication.applyInitializers`'
`Assert.state(requiredType != null, () -> "No generic type found for initializr of type " + ...)`
in `SpringBootContextLoaderAotTests`. On real HotSpot that resolver succeeds **with no
`ParameterizedType` in sight**, through `ResolvableType`'s *type-variable BOUND* fallback:
the raw interface `Class` still reports its own `getTypeParameters()`, so `hasGenerics()`
holds, and the unbound `C extends ConfigurableApplicationContext` resolves to its declared
bound. The real defect behind that test was the loader-blind interface resolution fixed
separately as **Residual 4** of the same cluster (still in place) — the test passes with
the reconstruction removed.

Fabricating a `ParameterizedType` is therefore observably wrong in itself, and it broke two
unrelated Spring idioms that specifically depend on a lambda's generics being reported as
**unresolvable**:

- `ApplicationConversionService.addBean()` (`ApplicationConversionService.java:389-397`)
  branches on `ResolvableType.forInstance(bean).as(type).hasUnresolvableGenerics()` to decide
  whether to register a type-aware `BeanAdapter` (using the `ResolvableType` supplied by the
  bean *definition*, which is correct) or the raw bean directly. On CratonVM this reported
  `false` for a lambda, so the raw (wrong) path was taken.
- `LambdaSafe.GenericTypeFilter.match()` (`LambdaSafe.java:364-380`) resolves the callback's
  type argument via `ResolvableType.forClass(callbackType, callbackInstance.getClass())` and
  uses it to **pre-filter** the call, skipping invocation (and the deliberate erasure-driven
  `ClassCastException` the class's own javadoc says it exists to catch) whenever the resolved
  generic doesn't match the argument's runtime type. On CratonVM the filter successfully
  resolved `T=StringBuilder` (real HotSpot cannot — lambdas are raw) and rejected the call up
  front, so the expected `ClassCastException` → `logger.debug(...)` path never ran.

## Fix

- `native-builtins/src/lang_class.rs` — the lambda branch of
  `native_class_get_generic_interfaces` now returns the raw functional-interface `Class`
  mirror unconditionally (the long-standing pre-"Residual 3" behavior), with the HotSpot
  parity rationale recorded in place. The loader-aware mirror lookup
  (`lambda_functional_interface_id_loader_aware`) and the non-empty-array guarantee that
  AspectJ's `execution(* Supplier+.get())` pointcut matching depends on are unchanged.
- `native-builtins/src/generics.rs` — deleted `lambda_functional_interface_generic_type`
  (164 lines); it had no other caller.
- `native-api/src/registry.rs` + `vm/src/vm/vm_exec.rs` — deleted the now-unused
  `NativeContext::lambda_call_site_descriptors` capability, which existed solely to feed that
  reconstruction and whose doc comment asserted the incorrect premise.

Net: 25 insertions, 274 deletions across 4 files.

## Verification

All runs on the Azure host, `SbRunnerMethod`/`SbRunner`-equivalent single-class runner,
`--java-home /data/data/jdk25-real`, identical classpath per module, CratonVM release build.

**Target classes** (`core/spring-boot`):

| class | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| `ApplicationConversionServiceTests` | 18/18 | 12/18 (6 failed) | **18/18** |
| `LambdaSafeTests` | 31/31 | 30/31 (1 failed) | **31/31** |

**Regression guard for the reverted "Residual 3"**:
`org.springframework.boot.test.context.SpringBootContextLoaderAotTests` — 1/1 PASS on
HotSpot, before, and after. The `IllegalStateException: No generic type found for initializr
of type ...` does **not** come back.

**Reflection-surface parity probe** (`LambdaGenProbe2`, 5 lambda/method-ref shapes ×
`getGenericSuperclass`/`getTypeParameters`/`getGenericInterfaces`/per-method generic
signatures + 5 `ResolvableType` views): after the fix, output is byte-identical to real
HotSpot except for one pre-existing, unrelated difference (see Notes).

**Module sweeps**, baseline binary vs fixed binary, same harness, same 600 s timeout:

`core/spring-boot`, 351 test classes:

| | PASS | FAIL | HANG | CRASH |
|---|---|---|---|---|
| before | 335 | 12 | 3 | 1 |
| after | **338** | **10** | 1 | 2 |

Per-class diff — only four classes moved, none of them a regression:
- `ApplicationConversionServiceTests` FAIL(6) → **PASS** (the fix)
- `LambdaSafeTests` FAIL(1) → **PASS** (the fix)
- `OriginTrackedMapPropertySourceTests` HANG → PASS (host-load artifact; the baseline arm ran
  while the host was at load average 40+)
- `OriginTrackedYamlLoaderTests` HANG → CRASH — **same failure on both binaries**, confirmed
  by an isolated back-to-back rerun: both arms die with
  `OutOfMemoryError: Java heap space (alloc_array length 1026)` in
  `canLoadFilesBiggerThan3Mb`. Pre-existing; only the timeout-vs-abort classification moved.

Every other non-PASS class has an identical failed-test count on both arms (`SpringApplicationTests`
10, `SpringApplicationBuilderTests` 16, `ConfigDataEnvironmentPostProcessorIntegrationTests` 67,
`ConfigurationPropertiesTests` 4, `NoSuchMethodFailureAnalyzerTests` 4, `ConfigTreePropertySourceTests` 3,
`PemSslStoreTests` 3, `ApplicationPidTests` 2, `SpringBootServletInitializerTests` 3,
`SpringBootVersionTests` 1, `ConfigurationPropertySourcesTests` HANG,
`LogbackLoggingSystemTests` CRASH).

`core/spring-boot-test`, 80 test classes: 78 PASS / 2 FAIL on **both** arms, **zero**
per-class differences.

**Rust unit tests**: `cargo test --no-fail-fast -p cratonvm-native-builtins
-p cratonvm-native-api -p cratonvm-vm` -- every suite green except two
pre-existing `cratonvm-native-builtins` failures
(`panama::tests::test_85_4_upcall_handle_and_invoke`,
`tls_deny::tests::every_plaintext_base_overload_is_accounted_for`), both
reproduced verbatim after reverting the four touched source files to the parent
commit, so they are unrelated to this change.

## Notes for future sessions

- The "Residual 3" section of
  `core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md` (same directory)
  has been annotated as SUPERSEDED — that fix is reverted, Residual 4's loader fix is what
  actually made the AOT test pass.
- **A raw `Class` from `getGenericInterfaces()` is not a gap to be filled.** Reflection-based
  generic resolvers are written to cope with erasure; when one appears to need a
  `ParameterizedType`, check whether the real JDK actually supplies one before synthesizing
  it — a differential probe against `jdk-25.0.3.9-hotspot` settles it in minutes and is far
  cheaper than the two-test regression this cost.
- A separate lambda-proxy parity gap surfaced while probing and was **fixed the same day** in
  `fix/lambda-writereplace-serializable-20260801` — see
  `../lambda-proxy-writereplace-serializable-parity-FIXED.md`. CratonVM's lambda proxies
  exposed a `writeReplace` method from `getDeclaredMethods()` (and answered `true` to
  `instanceof Serializable`) for *every* lambda, where real HotSpot does so only for
  serializable ones. It was the only remaining difference in the `LambdaGenProbe2` output;
  with it closed, that probe is now **byte-identical** to HotSpot.
