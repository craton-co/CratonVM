# CratonVM Spring suite — genuine bug list (dev `8719dca85`)

| | |
|---|---|
| **Status** | OPEN — 58 confirmed genuine bugs remaining |
| **Captured** | 2026-07-17 (initial full-suite triage, dev `213d93ea`), reconfirmed 2026-07-20 (dev `8719dca85`) |
| **Worktree** | `/data/wt-spring-full-suite-20260717` (branch `chore/spring-full-suite-20260717`), Azure host `20.83.144.174` |

## Summary

Started from a full 2912-class suite run (dev `213d93ea`) fully triaged
against HotSpot (see history below), which found **177 confirmed genuine
bugs**. Reconfirmed by rerunning exactly those 263 previously-non-passing
classes on a fresh `dev` merge (`8719dca85`, ~3 days / several hundred
commits later), 4 shards, same settings (`suite-run.sh`, `BATCH=10
BATCH_TO=120 ONE_TO=120`, `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`, real JDK 25).

**119 of the 177 are now fixed.** 58 remain open.

| Of the 177 | Count |
|---|--:|
| Now OK (fixed) | 119 |
| Still FAIL | 40 |
| Still/newly TIMEOUT | 16 |
| Now LOADERR (was TIMEOUT) | 2 |
| **Still open** | **58** |

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

**HTTP JSON/message-converter cluster — fixed 2026-07-20 (8/8 classes).**
`http.converter.json.*` (Gson, Jackson2, MappingJackson2, Jsonb,
Kotlin-serialization), `http.converter.StringHttpMessageConverterTests`,
`http.ContentDispositionTests`, `http.client.SimpleClientHttpRequestFactoryTests`
all now pass 100%. Two independent root causes, both in `native-io`/
`native-builtins`/`vm`:
1. **Shared charset/encoding gap (7/8 classes).** `ByteArrayOutputStream
   .toString(Charset)`/`toString(String)` (`native-io/src/lib.rs`) ignored
   the charset argument entirely and always did lossy UTF-8 decoding —
   fine for ASCII/UTF-8 content, silently mangling anything else (UTF-16BE
   JSON bodies in the `writeUTF16`/`writeObjectInUtf16` tests, ISO-8859-1
   in `StringHttpMessageConverterTests.writeDefaultCharset`, Shift_JIS in
   `ContentDispositionTests.parseQuotedPrintableShiftJISFilename`'s
   RFC 2047 decode, all of which route through this exact JDK method via
   `StreamUtils.copyToString(ByteArrayOutputStream, Charset)`). Fixed by
   routing through the real `cratonvm_native_api::charset` engine using the
   requested charset.
2. **`SimpleClientHttpRequestFactoryTests` (1/8 classes, 3 residual method
   failures after fix 1).**
   - `deleteWithoutBodyDoesNotRaiseException`/`httpMethods`: the synthetic
     `HttpURLConnection.<init>(URL)` native (`native-builtins/src/
     http_url_connection.rs::huc_init`) unconditionally clobbered field 0
     (the real inherited `URLConnection.url`) whenever real JDK code called
     `super(url)` directly on a subclass (not just via `URL.openConnection
     ()`), breaking `getURL()` and real-carrier detection; separately,
     `setRequestMethod` accepted `"PATCH"` (real JDK's whitelist doesn't,
     throwing `ProtocolException` — added as a new `RuntimeError` variant).
   - `interceptor`: a genuinely deep, cross-cutting bug — `Mockito.mock
     (HttpURLConnection.class)` (default "inline" mock maker) redefines the
     class's bytecode IN PLACE via JVMTI rather than subclassing it, so
     CratonVM's redefine-generation counter for `java/net/HttpURLConnection`
     trips permanently for the rest of the process, for EVERY instance —
     including totally unrelated, genuinely real connections created by
     *later* tests in the same JVM. The interpreter's redefine-guard then
     ceded to the (Mockito-woven) bytecode for those real connections too,
     so `getResponseCode()`/`getHeaderField()`/etc. silently no-op'd instead
     of touching the real request/response. Fixed with a receiver-aware
     exemption in `vm/src/runtime/interpreter.rs::intercept_force_registered
     _native`: force the native for `java/net/HttpURLConnection` whenever
     the receiver's field 0 is non-null (a real carrier's populated `url`
     field vs. a Mockito mock's always-null Objenesis-constructed field),
     re-validated per-call so genuine mocks (field 0 stays null) are
     unaffected and still correctly route through Mockito's advice.

Verified via an 8-class targeted run (all 100%) plus a 27-class regression
sweep across `http.client.*`/`web.client.*`/the sibling `http.converter`
cluster (`FormHttpMessageConverterTests`, `BufferedImageHttpMessageConverterTests`,
`Jaxb2CollectionHttpMessageConverterTests`) — no regressions;
`web.client.RestClientIntegrationTests`/`RestTemplateIntegrationTests`
(both pre-existing, out-of-scope failures) even improved (4->2 and 7->3
failing methods respectively), consistent with sharing the same
HttpURLConnection root causes.

**`scheduling.concurrent.*` cluster — fixed 2026-07-20 (4/4 classes).**
`ConcurrentTaskExecutorTests`, `DecoratedThreadPoolTaskExecutorTests`,
`ThreadPoolTaskExecutorTests`, `ThreadPoolTaskSchedulerTests` all now pass
100% (18/18, 14/14, 23/23, 40/40). Root cause: `native-collections` shadowed
`getCorePoolSize`/`getMaximumPoolSize`/`isShutdown`/`isTerminated`/
`shutdownNow` on the concrete class `java/util/concurrent/ThreadPoolExecutor`
unconditionally with CratonVM's synthetic 2-field executor layout, even for
REAL bytecode-constructed `ThreadPoolExecutor` instances (disambiguated only
by class name, which collides with the synthetic placeholder) — so
`setCorePoolSize()`/`setMaximumPoolSize()` mutations were silently ignored on
readback, and `shutdownNow()` interrupted workers but always returned an
empty list instead of draining `workQueue`, leaving queued `FutureTask`s
neither run nor cancelled (`future.get(timeout)` threw `TimeoutException`
instead of `CancellationException`). Fixed by routing real receivers through
the real JDK bytecode instead of the synthetic slots (see
`native-collections/src/lib.rs` `tp_is_real`), landed on `dev` at `2b41ba9b0`.
`scheduling.quartz.QuartzSupportTests` was investigated as a possible shared
residual but could not be verified either way: its module
(`spring-context-support`) doesn't compile against the shared
spring-framework checkout used for classpath generation (missing the
`org.springframework.aop.target` source package entirely, pre-existing and
unrelated to CratonVM) — left open, out of scope for the concurrent-cluster
fix.

**Groovy — 1/4 fixed.** `scripting.groovy.GroovyAspectTests` is now fixed;
`context.groovy.GroovyBeanDefinitionReaderTests` and
`scripting.groovy.GroovyScriptFactoryTests` are still hung (TIMEOUT), and
`web.servlet.view.groovy.GroovyMarkupViewTests` still FAILs (9/10).

**Resolved since 2026-07-17**: the `web.servlet.mvc.method.RequestMappingInfoHandlerMappingTests`
anomaly (previously FAIL despite 43/43 methods passing) is now a clean OK
(45/45) — whatever caused that status/method-count mismatch is gone.

## Full class list (66), by module

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

All 8 HTTP JSON/message-converter cluster classes fixed 2026-07-20 — see
"Notable clusters" above. Removed from this table.

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

`scheduling.concurrent.*` (4 classes: `ConcurrentTaskExecutorTests`,
`DecoratedThreadPoolTaskExecutorTests`, `ThreadPoolTaskExecutorTests`,
`ThreadPoolTaskSchedulerTests`) fixed 2026-07-20 — see "Notable clusters"
above. Removed from this table.

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scheduling.quartz.QuartzSupportTests` | FAIL | 8/17 | 9296ms |

(`QuartzSupportTests` not re-verified this session — see note above; kept as
FAIL/8/17 from the 2026-07-20 reconfirmation rerun.)

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
