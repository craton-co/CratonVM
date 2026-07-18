# `SpringApplicationWebServerTests` — web-application-type environment resolution not honored

**Status: OPEN — found 2026-07-17**

## Symptom

Module `module/spring-boot-web-server`, class `SpringApplicationWebServerTests`, 2 failing tests, both pointing at the same underlying mechanism (`SpringApplication` failing to resolve/apply the correct web-application-type-specific `Environment` subclass):

**`environmentIsConvertedIfTypeDoesNotMatch()`**: uses `ExampleReactiveWebConfig` + `--spring.profiles.active=withwebapplicationtype`, expecting `WebApplicationType.REACTIVE` (via a `@WithResource(name="application-withwebapplicationtype.properties", content="spring.main.web-application-type=reactive")`-provided profile file), asserting `context.getEnvironment().getClass().getName()` ends with `ApplicationReactiveWebEnvironment`. Instead the context resolves to **Servlet**, so it hits `ServletWebServerApplicationContext.getWebServerFactory()` with no Tomcat/servlet factory bean registered:

```
org.springframework.boot.web.server.context.MissingWebServerFactoryBeanException: No qualifying bean of type
'org.springframework.boot.web.server.servlet.ServletWebServerFactory' available: Unable to start
AnnotationConfigServletWebServerApplicationContext due to missing ServletWebServerFactory bean
```

**`webApplicationSwitchedOffInListener()`**: an `ApplicationEnvironmentPreparedEvent` listener asserts the environment class name ends with `ApplicationServletEnvironment` but gets plain `StandardEnvironment`:

```
java.lang.AssertionError:
Expecting actual:
  "org.springframework.core.env.StandardEnvironment"
to end with:
  "ApplicationServletEnvironment"
```

Test source: `apps/spring-boot/module/spring-boot-web-server/src/test/java/org/springframework/boot/web/server/SpringApplicationWebServerTests.java:139-154`.
Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard*/logs/module_spring-boot-web-server.SpringApplicationWebServerTests.out.log` (lines 87-142).

## Relationship to an existing FIXED doc — important discrepancy

`SpringApplicationWebServerTests` is listed as originally-affected in
[`../../internal/springboot/tomcatservletwebserverfactory-cross-module-classnotfound-crash-FIXED.md`](../../internal/springboot/tomcatservletwebserverfactory-cross-module-classnotfound-crash-FIXED.md),
marked **FIXED 2026-07-17 (today)**. That fix converted a fatal native-shim
class-not-found process **CRASH** into a normal catchable Java exception path
— and that part clearly worked: this class no longer crashes.

**However**, the class still **FAILs** — the `MissingWebServerFactoryBeanException`
above is a downstream symptom of a *different, still-open* bug: the
dynamically-materialized `application-withwebapplicationtype.properties`
profile file (via `@WithResource`) doesn't appear to be taking effect, so
`SpringApplication` falls back to classpath-based `WebApplicationType`
deduction and picks `SERVLET` instead of `REACTIVE`. **Unconfirmed** —
`@WithResource`'s CratonVM-side classpath-injection mechanism was not traced
to verify this. The FIXED doc's own repro/verification should be re-run
specifically against `SpringApplicationWebServerTests` — its "FIXED" claim
holds for the crash-vs-catchable-exception part, not for this class's
current FAIL as a whole.

## Root cause

**Hypothesis, not confirmed**: `SpringApplication.prepareEnvironment`/environment-conversion
logic isn't upgrading `StandardEnvironment` to the web-application-type-specific
subclass (`ApplicationServletEnvironment`/`ApplicationReactiveWebEnvironment`)
in these two scenarios — possibly because the deduced `WebApplicationType`
itself is wrong (per the `@WithResource` hypothesis above), or because the
conversion step itself has a gap. Not traced into
`SpringApplication.convertEnvironmentIfNecessary`/`deduceFromClasspath`
CratonVM behavior.

## Affected classes

- `module/spring-boot-web-server` | `SpringApplicationWebServerTests`
