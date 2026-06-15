# CratonVM × Spring Framework — CV-unique bug index

Each file = one distinct CratonVM bug (root cause), with the Spring test classes it affects.
**Only bugs where HotSpot JDK 25 passes are listed** (HotSpot-also-fails cases are dropped as
"same behaviour"). Suite: `apps/spring-framework` (7.1.0-SNAPSHOT, 2930 test classes, JUnit 6.1.0).
Baseline run still in progress — this index grows as modules complete.

| # | Bug | Category | Affected classes | Owner | Status |
|---|-----|----------|------------------|-------|--------|
| [01](spring-bug-01-annotation-proxy.md) | Annotation: first-proxy `$Proxy0` fallback + char[] coercion **(FIXED)**; `@AliasFor`/synthesis (open) | CORRECTNESS | ~9 (`core.annotation.*`) | **me** | **PARTIAL ✓ on dev** `168ca1b6` (AnnotationUtilsTests 8→6 fail) |
| [02](spring-bug-02-kotlin-reflect-builtins-null.md) | kotlin-reflect `getBuiltInClassByFqName` returns null | CORRECTNESS | 8 (Kotlin tests) | **HANDOFF** | symptom FIXED on dev (cf58ea48); residual metadata bug = 02b |
| [03](spring-bug-03-stream-iface-no-code-attribute.md) | synthetic `IntStream` (range/filter/mapToInt) stamped as interface class → `findFirst` `AbstractMethodError` | CORRECTNESS/dispatch | 2+ | **me** | **FIXED ✓ on dev** `4c5a4d09` |
| [04](spring-bug-04-junit-timeout-interceptor-double-proceed.md) | JUnit `@Timeout` interceptor `proceed()` called twice | CORRECTNESS/threads | 3+ | **HANDOFF** (chip) | OPEN |
| [05](spring-bug-05-dynamic-proxy-module-system.md) | `Proxy` disabled: "module system not fully initialized" | CORRECTNESS/bootstrap | many | **me** | **FIXED ✓ verified** |
| [06](spring-bug-06-mergedannotations-hang.md) | `MergedAnnotationsTests` **HANGS** (JUnit discovery loop, annotation reflection) | **HANG** | 1 | **me** | OPEN (⊆ bug-01) |
| [08](spring-bug-08-serializable-proxy-roundtrip.md) | serializable JDK-proxy round-trip broken (`SerializableTypeWrapper`) | CORRECTNESS | 1 | handoff? | OPEN (surfaced by bug-05 fix) |
| [09](spring-bug-09-collection-layout-probe-oob.md) | OOB field read on `EmptyMap` (map-layout probe) → **SIGSEGV** | **CRASH** | 1+ | me | **CRASH FIXED ✓ on dev** `5941addd` (residual perf-hang separate) |
| [10](spring-bug-10-junit-platform-execution-loaderr.md) | JUnit-platform execution failures on AspectJ-woven AOP classes (`TestEngine.getId` no-Code, `Status` CCE, NPEs) | CORRECTNESS/dispatch | ~18 LOADERR | handoff | OPEN (mixed; `getId` subset ⊆ bug-03 pattern) |

| [11](spring-bug-11-groovy-and-scheduler-crashes.md) | Groovy crashes: getfield-OOB **(guard FIXED)** + `jrt:` Layer B **(FIXED on dev)**; dup2 JIT miscompile (open) | **CRASH** | 3 | me/handoff | **PARTIAL ✓ on dev** `486c93e1`+`560fa5a5`; dup2 root open |

**Fixed on dev this session: bug-05, bug-03, bug-09** (3 merges → `12c22ec5`). Handed off: bug-02, bug-04.
**Crashes found:** 5 (3 Groovy cluster, 1 scheduler, 1 = the now-fixed EmptyMap SIGSEGV).

## Recommended fix order (by blast radius)
1. **bug-05** (proxy/init-level) — tiny flag flip, unblocks dynamic proxies suite-wide.
2. **bug-01** (annotation proxy) — foundational, cascades across every module.
3. **bug-03** (stream dispatch) — core itable bug, isolate with the 1-line trigger.
4. **bug-04** (@Timeout threads) — broad test-infra band.
5. **bug-02** (kotlin-reflect) — large but Kotlin-only; good handoff.

## Not yet grouped (tail under investigation)
`core.ResolvableTypeTests, MethodParameterTests, GenericTypeResolverTests, BridgeMethodResolverTests,
ParameterizedTypeReferenceTests, StandardReflectionParameterNameDiscoverTests, NullnessTests,
SortedPropertiesTests, SimpleAliasRegistryTests, aot.hint.ReflectionHintsTests,
aot.hint.ReflectionTypeReferenceTests, aot.nativex.RuntimeHintsWriterTests,
aot.generate.{FileSystem,InMemory}GeneratedFilesTests` — likely split between bug-01 (annotation)
and a few small distinct generics/collection bugs. Reports added as confirmed.

## Dropped (HotSpot also fails — NOT CratonVM bugs)
`aot.nativex.FileNativeConfigurationWriterTests` (env: GraalVM nativex output dep)
