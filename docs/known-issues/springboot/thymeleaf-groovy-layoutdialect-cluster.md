# Thymeleaf autoconfigure — Groovy layout-dialect constructor mismatch + missing CapturedOutput warning

**Status: OPEN — found 2026-07-17**

## Cluster A — Groovy `DecorateProcessor` constructor mismatch

| Class | Failures |
|---|---:|
| `ThymeleafReactiveAutoConfigurationTests` | 5/6 |
| `ThymeleafServletAutoConfigurationTests` | 7/8 |

```
groovy.lang.GroovyRuntimeException: Could not find matching constructor for: nz.net.ultraq.thymeleaf.layoutdialect.decorators.DecorateProcessor(org.thymeleaf.templatemode.TemplateMode, String, nz.net.ultraq.thymeleaf.layoutdialect.decorators.strategies.AppendingStrategy, Integer, Integer)
   at groovy.lang.MetaClassImpl.invokeConstructor(MetaClassImpl.java:1816/1591)
   at nz.net.ultraq.thymeleaf.layoutdialect.LayoutDialect.getProcessors(LayoutDialect.groovy:119)
   at org.thymeleaf.DialectSetConfiguration.build(DialectSetConfiguration.java:152)
```

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-thymeleaf.org.springframework.boot.thymeleaf.autoconfigure.ThymeleafReactiv-42b7b372db9e.out.log` (lines 18-31),
`...ThymeleafServlet-c4b8626805dc.out.log` (lines 18-41).

Already noted (but not previously filed as a dedicated doc) in `docs/known-issues/springboot/README.md`'s HANG-rerun section as "a couple of `GroovyRuntimeException`s from the Thymeleaf layout-dialect integration ... not yet clustered" — this doc is that clustering.

### Root cause (Cluster A)

**Hypothesis, not confirmed.** Groovy's MOP `invokeConstructor` does closest-match resolution over `Class.getConstructors()`/`getDeclaredConstructors()`. CratonVM's reflective constructor metadata for `DecorateProcessor` likely mismatches HotSpot's (wrong arity, wrong parameter types, or duplicate entries), causing Groovy to reject the real 5-arg constructor. Not pinned to a file:line in CratonVM's reflection/native-builtins code — needs a standalone repro (`DecorateProcessor.class.getConstructors()` dumped and diffed against real HotSpot) to confirm.

## Cluster B — missing `CapturedOutput` warning for a nonexistent template location

Same two classes, `templateLocationDoesNotExist()` test:

```
java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "Cannot find template location"
```

Logs: same files, Reactive lines 42-59, Servlet lines 66-83.

### Root cause (Cluster B)

**Hypothesis, not confirmed, and distinct from Cluster A** — this is a `CapturedOutput` assertion, not a Groovy dispatch error. The `CapturedOutput` extension expects a WARN log line that CratonVM either never emits on this code path, or emits through a stream `CapturedOutput` doesn't intercept (same general shape as other `CapturedOutput`-miss failures seen elsewhere in this rerun — see the Graylog GELF-validation cluster). Not traced further.

## Affected classes

- `module/spring-boot-thymeleaf` | `ThymeleafReactiveAutoConfigurationTests`
- `module/spring-boot-thymeleaf` | `ThymeleafServletAutoConfigurationTests`
