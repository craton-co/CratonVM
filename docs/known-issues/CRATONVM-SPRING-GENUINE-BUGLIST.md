# CratonVM Spring suite — genuine bug list (dev `8719dca85`)

| | |
|---|---|
| **Status** | OPEN — 70 confirmed genuine bugs remaining |
| **Captured** | 2026-07-17 (initial full-suite triage, dev `213d93ea`), reconfirmed 2026-07-20 (dev `8719dca85`) |
| **Worktree** | `/data/wt-spring-full-suite-20260717` (branch `chore/spring-full-suite-20260717`), Azure host `20.83.144.174` |

## Summary

Started from a full 2912-class suite run (dev `213d93ea`) fully triaged
against HotSpot (see history below), which found **177 confirmed genuine
bugs**. Reconfirmed by rerunning exactly those 263 previously-non-passing
classes on a fresh `dev` merge (`8719dca85`, ~3 days / several hundred
commits later), 4 shards, same settings (`suite-run.sh`, `BATCH=10
BATCH_TO=120 ONE_TO=120`, `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`, real JDK 25).

**107 of the 177 are now fixed.** 70 remain open.

| Of the 177 | Count |
|---|--:|
| Now OK (fixed) | 107 |
| Still FAIL | 52 |
| Still/newly TIMEOUT | 16 |
| Now LOADERR (was TIMEOUT) | 2 |
| **Still open** | **70** |

The 86 environmentally-non-OK classes (73 EMPTY + 13 FAIL matching HotSpot,
not CratonVM bugs) were not rerun individually here but the 263-class rerun
included them — EMPTY count held steady at 73, consistent with them still
being environmental.

## Notable clusters (current state, 2026-07-20)

**JMX — 24/26 fixed, 2 remain.** The systemic `RequiredModelMBean` breakage
flagged on 2026-07-17 is now resolved for all but two classes:
`jmx.access.MBeanClientInterceptorTests` (11/14 pass) and
`jmx.access.RemoteMBeanClientInterceptorTests` (2/14 pass) — both partial
failures now, not total breakage, suggesting the fix addressed the core
issue and these two hit a narrower residual gap.

**AOT/TIMEOUT cluster — 16 classes, still fully hung**, plus 2 that flipped
from TIMEOUT to LOADERR (worth checking — a status-type change, not just
timing): `beans.factory.aot.BeanDefinitionMethodGeneratorTests` and
`beans.factory.aot.InstanceSupplierCodeGeneratorTests`. The still-hanging 16
grew slightly from the original 12 (picked up `test.context.aot.AotIntegrationTests`,
`web.service.registry.HttpServiceProxyRegistrationAotProcessorTests`,
`core.io.buffer.DataBufferTests`, `scripting.groovy.GroovyScriptFactoryTests`
— the last was FAIL before, now TIMEOUT). Full list in the table below
(`beans`, `context`, `orm`, `test`, `web` sections, all `TIMEOUT`/`LOADERR`
rows).

**`test.context.jdbc.*` cluster — fully fixed (0 remain).** All 25 classes
that were uniformly failing behind Spring's `ApplicationContext` failure
threshold circuit-breaker now pass. Whatever landed in the last 3 days
resolved the whole cluster at once — worth checking dev history for the
specific fix if attribution matters.

**HTTP JSON/message-converter cluster — fully unfixed (8 classes)**, same
as 2026-07-17: `http.converter.json.*` (Gson, Jsonb, Jackson2,
Kotlin-serialization), `http.converter.StringHttpMessageConverterTests`,
`http.ContentDispositionTests`, `http.client.SimpleClientHttpRequestFactoryTests`
— all still failing 1-3 methods each, consistent with one shared
charset/encoding gap.

**`scheduling.concurrent.*` cluster — fully unfixed (4 classes)**:
`ConcurrentTaskExecutorTests`, `DecoratedThreadPoolTaskExecutorTests`,
`ThreadPoolTaskExecutorTests`, `ThreadPoolTaskSchedulerTests`, plus
`scheduling.quartz.QuartzSupportTests` — all partial failures (2-9 methods
each), likely a shared executor/scheduler gap.

**Groovy — 1/4 fixed.** `scripting.groovy.GroovyAspectTests` is now fixed;
`context.groovy.GroovyBeanDefinitionReaderTests` and
`scripting.groovy.GroovyScriptFactoryTests` are still hung (TIMEOUT), and
`web.servlet.view.groovy.GroovyMarkupViewTests` still FAILs (9/10).

**Resolved since 2026-07-17**: the `web.servlet.mvc.method.RequestMappingInfoHandlerMappingTests`
anomaly (previously FAIL despite 43/43 methods passing) is now a clean OK
(45/45) — whatever caused that status/method-count mismatch is gone.

## Full class list (70), by module

### Aop

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `aop.framework.autoproxy.BeanNameAutoProxyCreatorTests` | FAIL | 8/9 | 8259ms |
| `aop.support.MethodMatchersTests` | FAIL | 13/14 | 10851ms |

### Beans

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `beans.ConcurrentBeanWrapperTests` | FAIL | 100/101 | 16852ms |
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | TIMEOUT | 0/0 | 120000ms |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | LOADERR | 0/0 | 2351ms |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | TIMEOUT | 0/0 | 120000ms |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | TIMEOUT | 0/0 | 120000ms |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT | 0/0 | 120000ms |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | LOADERR | 0/0 | 345ms |
| `beans.factory.xml.XmlBeanFactoryTests` | FAIL | 85/95 | 85837ms |

### Context

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` | TIMEOUT | 0/0 | 120000ms |
| `context.annotation.ConfigurationClassPostConstructAndAutowiringTests` | FAIL | 1/2 | 412ms |
| `context.annotation.ConfigurationClassPostProcessorTests` | FAIL | 82/85 | 20126ms |
| `context.annotation.Spr15275Tests` | FAIL | 4/6 | 2038ms |
| `context.annotation.Spr6602Tests` | FAIL | 1/2 | 1229ms |
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT | 0/0 | 120000ms |
| `context.groovy.GroovyBeanDefinitionReaderTests` | TIMEOUT | 0/0 | 120000ms |

### Core

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `core.GenericTypeResolverTests` | FAIL | 24/25 | 2651ms |
| `core.annotation.NestedRepeatableAnnotationsTests` | FAIL | 2/12 | 759ms |
| `core.io.ResourceTests` | FAIL | 66/68 | 4689ms |
| `core.io.buffer.DataBufferTests` | TIMEOUT | 0/0 | 120000ms |
| `core.retry.RetryPolicyTests` | FAIL | 22/23 | 828ms |

### Expression

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `expression.spel.MethodInvocationTests` | FAIL | 22/23 | 2158ms |
| `expression.spel.SpelCompilationCoverageTests` | FAIL | 159/162 | 26105ms |

### Http

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `http.ContentDispositionTests` | FAIL | 33/34 | 1346ms |
| `http.client.SimpleClientHttpRequestFactoryTests` | FAIL | 7/10 | 11006ms |
| `http.converter.StringHttpMessageConverterTests` | FAIL | 12/13 | 889ms |
| `http.converter.json.GsonHttpMessageConverterTests` | FAIL | 13/14 | 1553ms |
| `http.converter.json.JacksonJsonHttpMessageConverterTests` | FAIL | 33/34 | 6805ms |
| `http.converter.json.JsonbHttpMessageConverterTests` | FAIL | 13/14 | 823ms |
| `http.converter.json.KotlinSerializationJsonHttpMessageConverterTests` | FAIL | 24/25 | 11650ms |
| `http.converter.json.MappingJackson2HttpMessageConverterTests` | FAIL | 31/32 | 4911ms |

### Jdbc

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jdbc.core.namedparam.BeanPropertySqlParameterSourceTests` | FAIL | 7/10 | 4631ms |
| `jdbc.core.namedparam.MapSqlParameterSourceTests` | FAIL | 3/6 | 1118ms |

### Jms

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jms.core.JmsTemplateTransactedTests` | FAIL | 51/52 | 13857ms |

### Jmx

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jmx.access.MBeanClientInterceptorTests` | FAIL | 11/14 | 5716ms |
| `jmx.access.RemoteMBeanClientInterceptorTests` | FAIL | 2/14 | 15167ms |

### Jndi

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jndi.JndiObjectFactoryBeanTests` | FAIL | 24/25 | 2165ms |

### Orm

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` | TIMEOUT | 0/0 | 120000ms |
| `orm.jpa.support.PersistenceInjectionTests` | FAIL | 26/27 | 11461ms |

### Scheduling

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scheduling.concurrent.ConcurrentTaskExecutorTests` | FAIL | 16/18 | 4896ms |
| `scheduling.concurrent.DecoratedThreadPoolTaskExecutorTests` | FAIL | 12/14 | 4853ms |
| `scheduling.concurrent.ThreadPoolTaskExecutorTests` | FAIL | 19/23 | 4964ms |
| `scheduling.concurrent.ThreadPoolTaskSchedulerTests` | FAIL | 38/40 | 6671ms |
| `scheduling.quartz.QuartzSupportTests` | FAIL | 8/17 | 9296ms |

### Scripting

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scripting.groovy.GroovyScriptFactoryTests` | TIMEOUT | 0/0 | 120000ms |

### Test

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `test.context.BootstrapUtilsTests` | FAIL | 22/23 | 9109ms |
| `test.context.aot.AotIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `test.context.aot.TestClassScannerTests` | TIMEOUT | 0/0 | 120000ms |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL | 0/4 | 83845ms |
| `test.context.bean.override.mockito.MockitoBeanByTypeLookupIntegrationTests` | FAIL | 3/5 | 27473ms |
| `test.context.bean.override.mockito.constructor.MockitoBeanByTypeLookupForConstructorParametersIntegrationTests` | FAIL | 4/6 | 16982ms |
| `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` | FAIL | 0/2 | 918ms |
| `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` | TIMEOUT | 0/0 | 120000ms |
| `test.context.testng.TestNGConcurrencyTests` | FAIL | 0/1 | 3669ms |
| `test.web.servlet.assertj.MockMvcTesterIntegrationTests` | FAIL | 72/74 | 58069ms |

### Util

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `util.CollectionUtilsTests` | FAIL | 30/32 | 1016ms |
| `util.StreamUtilsTests` | FAIL | 10/11 | 5019ms |

### Web

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `web.client.RestClientIntegrationTests` | FAIL | 226/230 | 96436ms |
| `web.client.RestTemplateIntegrationTests` | FAIL | 118/125 | 88728ms |
| `web.context.request.RequestScopeTests` | FAIL | 0/7 | 1300ms |
| `web.reactive.function.client.WebClientIntegrationTests` | FAIL | 168/170 | 47143ms |
| `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` | TIMEOUT | 0/0 | 120000ms |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL | 3/5 | 100617ms |
| `web.servlet.config.MvcNamespaceTests` | FAIL | 24/25 | 24938ms |
| `web.servlet.config.annotation.ViewResolutionIntegrationTests` | FAIL | 6/7 | 29415ms |
| `web.servlet.view.groovy.GroovyMarkupViewTests` | FAIL | 9/10 | 28950ms |
| `web.socket.messaging.StompWebSocketIntegrationTests` | FAIL | 14/16 | 105475ms |

## Raw data

- Original full-suite triage (177 bugs): 8 shards, dev `213d93ea`,
  binary `cratonvm-fullsuite-20260717.bin`, cross-referenced against HotSpot
  (516-class baseline + a fresh 129-class targeted HotSpot rerun).
- Reconfirmation rerun (this update): 4 shards, dev `8719dca85`, binary
  `cratonvm-fullsuite2-20260720.bin`, `LIST=` the exact 263 non-OK classes
  from the original run.
- Per-class FAILCAUSE and crash-log detail available in
  `/data/tmp/nonpassed263-s{0..3}/{failcauses,crashes}.log` on the Azure
  host at capture time.
