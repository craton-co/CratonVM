# `ObjectProvider<X>` injection points fail to resolve under an isolated `ModifiedClassPathClassLoader`, even though a matching bean of type `X` exists

**Status: OPEN — found 2026-07-19, residual of [`modifiedclasspath-aether-network-hang-cluster-FIXED.md`](../../internal/springboot/modifiedclasspath-aether-network-hang-cluster-FIXED.md)**

## Symptom

Multiple classes across `module/spring-boot-jersey` and
`module/spring-boot-security` — all `@ManagementContextConfiguration`-style
auto-configuration tests run under an isolated `ModifiedClassPathClassLoader`
(triggered by class- or method-level `@ClassPathExclusions`, none of which
exclude anything related to `spring-beans` or the injected type itself) —
fail with:

```
org.springframework.beans.factory.NoSuchBeanDefinitionException: No qualifying bean of type
'org.springframework.beans.factory.ObjectProvider<org.springframework.boot.jersey.autoconfigure.actuate.web.ManagementContextResourceConfigCustomizer>'
available: expected at least 1 bean which qualifies as autowire candidate. Dependency annotations: {}
```

Reproduced with `cratonvm.exe` (`sb-runner` harness) in worktree
`CratonVM-aether-modifiedclasspath-20260718-019f753a`:

| Class | Result |
|---|---|
| `module/spring-boot-jersey` `JerseyChildManagementContextConfigurationTests` | `tests=6 failed=5` |
| `module/spring-boot-security` `SecurityFilterAutoConfigurationEarlyInitializationTests` | `tests=1 failed=1` |
| `module/spring-boot-security` `ManagementWebSecurityAutoConfigurationTests` | `tests=10 failed=1` |
| `module/spring-boot-security` `ReactiveManagementWebSecurityAutoConfigurationTests` | `tests=9 failed=1` |

`JerseyChildManagementContextConfigurationTests`'s
`@ClassPathExclusions("spring-webmvc-*")` doesn't touch `spring-beans` (where
`ObjectProvider` lives) or the specific `X` type in each failure
(`ManagementContextResourceConfigCustomizer`, `SecurityFilterAutoConfiguration`
health-endpoint types, etc.) — those types are genuinely present and, per
the surrounding `@Configuration` classes, genuinely registered as beans.

## Root-cause hypothesis (not confirmed by a debugger attach)

`ObjectProvider<T>` is Spring's always-resolvable lazy/optional injection
wrapper — `DefaultListableBeanFactory` special-cases it at the injection-point
descriptor level so that resolution succeeds regardless of whether a `T` bean
currently exists (the "no bean found" case is meant to surface later, from
calling `.getIfAvailable()`, not at injection-point resolution time). Getting
`NoSuchBeanDefinitionException` for the wrapper type itself, rather than
either a successful (possibly empty) `ObjectProvider` or a later
`.getIfAvailable()`-time failure, points at a **classloader-identity split**:
the injection point's declared generic type (`ObjectProvider<
ManagementContextResourceConfigCustomizer>`, read via reflection on the
constructor/method parameter, resolved through the isolated
`ModifiedClassPathClassLoader`) and the registered bean definition's type
(recorded during `@Configuration` class parsing/registration, potentially
through a different resolution path or timing) may not be the *same* `Class`
object for `ManagementContextResourceConfigCustomizer` even though both have
the same binary name — the same general failure family as the `@Nested`
outer-instance mismatch in
[`connectionfactoryunwrappertests-nested-outer-instance-identity.md`](connectionfactoryunwrappertests-nested-outer-instance-identity.md)
and possibly the `OnBeanCondition` deduction gap in
[`isolated-loader-onbeancondition-type-deduction-bypass.md`](isolated-loader-onbeancondition-type-deduction-bypass.md) —
all three surfaced only after the now-fixed recursion bug stopped masking
every isolated-loader test with an infinite hang, and all three are
consistent with resolution-path inconsistencies specific to isolated
`URLClassLoader`s that weren't exercised (or weren't distinguishable from a
total hang) before that fix.

## Suggested next step

Pick the smallest failing case (`SecurityFilterAutoConfigurationEarlyInitializationTests`,
1 test method) and trace where the `ManagementContextResourceConfigCustomizer`-equivalent
bean's registered type `Class` object comes from (ASM-based `@Configuration`
class parsing vs. later reflective resolution) versus where the injection
point's `ObjectProvider<X>` generic argument `Class` object comes from,
looking for two different resolution calls that could return non-identical
`Class` objects for the same binary name under the isolated loader — likely
in the same family of gaps as `preload_isolated_loader_supertypes`/
`resolve_class_loader_aware` (see the now-fixed parent doc and the
`OnBeanCondition` residual doc above) but not yet narrowed to a specific
call site.
