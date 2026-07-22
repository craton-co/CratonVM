# `SpringApplicationWebServerTests` — web-application-type environment resolution not honored

**Status: FIXED (2026-07-18)**

## Symptom (as originally found, 2026-07-17)

Module `module/spring-boot-web-server`, class `SpringApplicationWebServerTests`, 2 failing tests, both pointing at the same underlying mechanism (`SpringApplication` failing to resolve/apply the correct web-application-type-specific `Environment` subclass):

- **`environmentIsConvertedIfTypeDoesNotMatch()`**: used `ExampleReactiveWebConfig` + `--spring.profiles.active=withwebapplicationtype`, expecting `WebApplicationType.REACTIVE` via a `@WithResource`-provided profile file, asserting the environment ends with `ApplicationReactiveWebEnvironment`. Instead resolved to Servlet, hitting `MissingWebServerFactoryBeanException`.
- **`webApplicationSwitchedOffInListener()`**: an `ApplicationEnvironmentPreparedEvent` listener asserted the environment class name ends with `ApplicationServletEnvironment` but got plain `StandardEnvironment`.

## Resolution

Two independent things were true by the time this was re-investigated (2026-07-18):

1. **`environmentIsConvertedIfTypeDoesNotMatch()` was already passing** on current `dev` before any change in this investigation — it was fixed as a side effect of unrelated work landed between 2026-07-17 and 2026-07-18 (the doc's "unconfirmed `@WithResource` classpath-injection" hypothesis was never the live blocker by the time this was re-run; `@WithResource` file materialization works correctly, per the already-merged fixes documented in `messagesourceautoconfigurationtests-getmessage-default-fallback-FIXED.md` and `hibernatejpaautoconfigurationtests-stall-hang-FIXED.md`).
2. **`webApplicationSwitchedOffInListener()`** was a genuine, separate CratonVM defect, root-caused and fixed here.

### Root cause

`native-builtins/src/spring_startup_bootstrap.rs`'s native override for
`SpringApplication.getOrCreateEnvironment()` (`spring_app_get_or_create_environment`)
unconditionally built a generic `StandardEnvironment` via
`construct_real_standard_environment`, regardless of the application's
`WebApplicationType`. That shim was written to guarantee the environment's
`systemProperties`/`systemEnvironment` property sources are populated (a
separate, real partial-bootstrap gap) — but it bypassed the real bytecode's
type-aware construction entirely
(`this.applicationContextFactory.createEnvironment(webApplicationType)`,
resolved via the `ApplicationContextFactory` SPI in `spring.factories`,
which for `SERVLET`/`REACTIVE` returns a fresh
`ApplicationServletEnvironment`/`ApplicationReactiveWebEnvironment`).

Most call sites never observed the discrepancy: `SpringApplication.prepareEnvironment()`
later runs `EnvironmentConverter.convertEnvironmentIfNecessary(environment,
deduceEnvironmentClass())`, which reflectively constructs the correct
type-specific environment and copies property sources across — so any test
that only inspects `context.getEnvironment()` *after* `run()` returns sees
the correct, converted type. But `ApplicationEnvironmentPreparedEvent` fires
*before* that conversion pass (right after `getOrCreateEnvironment()` +
`configureEnvironment()`), so a listener observing the environment at that
point saw the generic `StandardEnvironment` instead of
`ApplicationServletEnvironment` — exactly `webApplicationSwitchedOffInListener`'s
failure.

### Fix

Added `create_web_application_environment()`, which mirrors the real
bytecode: it reads `this.properties.getWebApplicationType()` and
`this.applicationContextFactory`, then invokes the factory's
`createEnvironment(WebApplicationType)` (dispatched via `invoke_virtual`, so
it runs the real Java `ApplicationContextFactory` implementations resolved
through `spring.factories`, e.g. `ServletWebServerApplicationContextFactory`
/ `ReactiveWebServerApplicationContextFactory`). `spring_app_get_or_create_environment`
now tries this type-aware path first and only falls back to the generic
`construct_real_standard_environment` safety net (preserved for the
`WebApplicationType.NONE` case and any factory-resolution failure) when it
returns `None`.

## Verification

Built and ran on the Azure Linux build host (worktree
`/data/data/wt-springapp-webserver-envres-20260718`, branch
`fix/springapp-webserver-envres-20260718`):

- `SpringApplicationWebServerTests`: 8/8 pass (was 7/8 on the pre-fix build
  of the same commit; `webApplicationSwitchedOffInListener` now passes).
- Regression check, same module (`spring-boot-web-server`):
  `MissingWebServerFactoryBeanFailureAnalyzerTests` (2/2),
  `AnnotationConfigServletWebServerApplicationContextTests` (9/9),
  `ServletWebServerApplicationContextTests` (32/32),
  `XmlServletWebServerApplicationContextTests` (6/6) all clean.
  `ServletComponentScanIntegrationTests` has 1 pre-existing failure
  (`indexedComponentsAreRegistered`, an unrelated classpath-scanning
  `FileNotFoundException` on a test-resource `.class` file) — confirmed
  byte-for-byte identical on the pre-fix baseline build, not a regression.
- Regression check, `module/spring-boot-jetty`:
  `AutoConfigureWebServerJettyServletTests` (1/1),
  `JettyMetricsAutoConfigurationTests` (11/11),
  `JettyServletWebServerAutoConfigurationTests` (15/15),
  `JettyServletWebServerMvcIntegrationTests` (2/2) all clean — these are the
  Jetty-side classes from the related
  `tomcatservletwebserverfactory-cross-module-classnotfound-crash-FIXED.md`
  fix, sharing the same `getOrCreateEnvironment` codepath.
- Broadest regression check: `core/spring-boot`'s `SpringApplicationTests`
  (102 tests, the heaviest exerciser of `SpringApplication.getOrCreateEnvironment`)
  — fix build and pre-fix baseline build produce byte-for-byte identical
  results (102 tests, 13 failed, 2 skipped, same 13 failing test names in
  both — all pre-existing `CapturedOutput`-related failures, unrelated to
  this change). Zero regressions.

## Affected classes

- `module/spring-boot-web-server` | `SpringApplicationWebServerTests` — FIXED

## Related

- `docs/internal/springboot/tomcatservletwebserverfactory-cross-module-classnotfound-crash-FIXED.md` — the doc that first listed `SpringApplicationWebServerTests` as affected (for the unrelated Tomcat-shim crash, fixed 2026-07-17); this doc closes out its residual FAIL.
- `docs/internal/springboot/messagesourceautoconfigurationtests-getmessage-default-fallback-FIXED.md`, `docs/internal/springboot/hibernatejpaautoconfigurationtests-stall-hang-FIXED.md` — prior `@WithResource` fixes that (as a side effect) resolved `environmentIsConvertedIfTypeDoesNotMatch` before this investigation reached it.
