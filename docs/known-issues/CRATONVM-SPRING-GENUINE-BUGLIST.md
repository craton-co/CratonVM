# CratonVM Spring suite — genuine bug list (full suite, dev `213d93ea`)

| | |
|---|---|
| **Status** | OPEN — 177 confirmed genuine bugs, fully triaged |
| **Captured** | 2026-07-17, worktree `/data/wt-spring-full-suite-20260717` (branch `chore/spring-full-suite-20260717`, off `origin/dev` @ `213d93ea`), Azure host `20.83.144.174` |
| **Supersedes** | The prior `CRATONVM-SPRING-GENUINE-BUGLIST-125.md` / `-159.md` lineage and `CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`, both retired to `docs/internal/spring/` (they covered only a 516-class non-passed subset from July 8-11, ~986 commits behind this run). |

## Summary

Ran the **complete** Spring Framework test suite — all 2912 test classes across
22 modules, no filtering — in 8 shards (`suite-run.sh`, `BATCH=10 BATCH_TO=120
ONE_TO=120`, `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`, real JDK 25).

| Status | Count | |
|---|--:|---|
| OK | 2649 | 91.0% |
| FAIL | 176 | |
| EMPTY | 73 | |
| TIMEOUT | 13 | |
| LOADERR | 1 | |
| **Non-OK total** | **263** | |

Every one of the 263 non-OK classes was cross-referenced against HotSpot
(Temurin JDK 25, same classpath, same worktree) to separate genuine CratonVM
bugs from environmental noise (missing test infrastructure, classes HotSpot
itself can't run, etc.):

- **134 classes** already had HotSpot data from an existing 516-class baseline
  (`hs516_final.tsv`, captured 2026-07-09, still valid — corrected classpath
  jars, so still an apples-to-apples comparison).
- **129 classes** had no prior baseline. Ran a fresh, targeted HotSpot pass
  over exactly those 129 (`suite-run-hotspot.sh`, same settings) —
  **all 129 passed cleanly on HotSpot.**

| Outcome | Count |
|---|--:|
| Environmental (fails/empty identically on HotSpot) | 86 |
| **Genuine CratonVM bug** | **177** |

The environmental 86 = 73 EMPTY (HotSpot also reports EMPTY for every one of
these — not CratonVM's fault, no runnable tests found) + 13 FAIL (fails the
same way on HotSpot too). Full breakdown: `86 + 177 = 263`. Nothing left
unclassified.

## Notable clusters (shared root cause candidates, not yet individually root-caused)

**JMX — systemic, entire module broken (26 classes, all `jmx.*` sub-packages:
`access`, `export`, `export.annotation`, `export.assembler`,
`export.naming`, `support`)**. All pass 100% on HotSpot. Not a partial
regression — every JMX test class fails on CratonVM, with failure ratios
ranging from total (`export.naming.IdentityNamingStrategyTests` 0/1,
`jmx.export.LazyInitMBeanTests` 0/1) to partial (`export.MBeanExporterTests`
33/38). Sample cause: `javax.management.RuntimeErrorException: Error
occurred in RequiredModelMBean while trying to invoke operation setName`
(`NotificationListenerTests`). A prior fix
([[jmx-rmi-connector-and-platform-mbean-registration-fixed]]) addressed
`getThreadInfo(long)` signature mismatch on 2026-07-05, but that was clearly
not the whole story — the module-wide breadth here points to a deeper,
still-unfixed MBean/RequiredModelMBean gap.

**AOT bean-registration TIMEOUT cluster (10 classes)** — unchanged from the
prior doc's finding, still hard-hanging at the 120s ceiling with `found=0`:
`beans.factory.aot.BeanDefinitionMethodGeneratorTests`,
`beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests`,
`beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests`,
`beans.factory.aot.BeanRegistrationsAotContributionTests`,
`beans.factory.aot.InstanceSupplierCodeGeneratorTests`,
`context.annotation.ComponentScanParserBeanDefinitionDefaultsTests`,
`context.aot.ApplicationContextAotGeneratorTests`,
`orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests`,
`test.context.aot.TestClassScannerTests`,
`test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests`,
`web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests`,
`web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests`.
The retired `CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md` (in
`docs/internal/spring/`) has a 1500s-timeout diagnostic confirming most of
these are genuinely hung, not just slow — worth reading before investigating.

**`test.context.jdbc.*` — total, uniform failure (30 classes, ~all `0/N`
methods passing)**: every class in this package fails 100% of its methods.
Sample cause: `IllegalStateException: Failed to load ApplicationContext` /
`ApplicationContext failure threshold (1) exceeded: skipping repeated
attempt to load context` — the *first* context load fails and Spring's own
failure-threshold circuit-breaker then short-circuits every subsequent
attempt, so the true root cause is masked behind this generic message.
Points at something in embedded-datasource/SQL-script bootstrap
(`EmptyDatabaseConfig`-style contexts) that CratonVM can't satisfy. Worth
isolating one class outside the failure-threshold noise to get the real
underlying exception.

**HTTP JSON/message-converter cluster (7 classes)** — `http.converter.json.*`
(Gson, Jsonb, Jackson2, Kotlin-serialization) and
`http.codec.cbor.JacksonCborDecoderTests` all show small (1-3 method)
failure counts, mostly around `writeUTF16`/encoding-specific tests — likely
one shared charset/encoding gap rather than 7 separate bugs.

**Groovy scripting (4 classes)**: `context.groovy.GroovyBeanDefinitionReaderTests`,
`scripting.groovy.GroovyAspectTests`, `scripting.groovy.GroovyScriptFactoryTests`,
`web.servlet.view.groovy.GroovyMarkupViewTests` — consistent with prior
findings, still broken.

**Anomaly worth a second look**: `web.servlet.mvc.method.RequestMappingInfoHandlerMappingTests`
is recorded as **FAIL with 43/43 methods passing** — every individual test
method succeeded, but the class-level status is still FAIL. Points at a
container-level exception (e.g. during teardown) that KRun's status logic
counts against the class despite 100% method pass rate — check this one
first since it might reveal a status-reporting bug in the harness itself
rather than a real Spring failure.

## Full class list (177), by module

### Aop

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `aop.aspectj.ThisAndTargetSelectionOnlyPointcutsTests` | FAIL | 0/7 | 1219ms |
| `aop.framework.adapter.ThrowsAdviceInterceptorTests` | FAIL | 5/6 | 595ms |
| `aop.framework.autoproxy.BeanNameAutoProxyCreatorTests` | FAIL | 8/9 | 4216ms |
| `aop.support.MethodMatchersTests` | LOADERR | 0/0 | 4538ms |
| `aop.target.LazyCreationTargetSourceTests` | FAIL | 0/1 | 98ms |

### Beans

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `beans.ConcurrentBeanWrapperTests` | FAIL | 100/101 | 5296ms |
| `beans.PropertyDescriptorUtilsPropertyResolutionTests` | FAIL | 33/42 | 27870ms |
| `beans.factory.DefaultListableBeanFactoryTests` | FAIL | 182/186 | 10697ms |
| `beans.factory.FactoryBeanLookupTests` | FAIL | 3/5 | 875ms |
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | FAIL | 13/14 | 93871ms |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | TIMEOUT | 0/0 | 120000ms |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | TIMEOUT | 0/0 | 120000ms |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | TIMEOUT | 0/0 | 120000ms |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT | 0/0 | 120000ms |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | TIMEOUT | 0/0 | 120000ms |
| `beans.factory.xml.XmlBeanCollectionTests` | FAIL | 31/34 | 17750ms |
| `beans.factory.xml.XmlBeanFactoryTests` | FAIL | 85/95 | 19957ms |

### Context

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `context.annotation.AnnotationConfigApplicationContextTests` | FAIL | 35/36 | 3550ms |
| `context.annotation.CommonAnnotationBeanPostProcessorTests` | FAIL | 20/21 | 962ms |
| `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` | TIMEOUT | 0/0 | 120000ms |
| `context.annotation.ConfigurationClassPostConstructAndAutowiringTests` | FAIL | 1/2 | 485ms |
| `context.annotation.ConfigurationClassPostProcessorTests` | FAIL | 82/85 | 8141ms |
| `context.annotation.SimpleScanTests` | FAIL | 0/1 | 101ms |
| `context.annotation.Spr15275Tests` | FAIL | 4/6 | 892ms |
| `context.annotation.Spr6602Tests` | FAIL | 1/2 | 1113ms |
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT | 0/0 | 120000ms |
| `context.groovy.GroovyBeanDefinitionReaderTests` | FAIL | 35/36 | 86233ms |

### Core

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `core.BridgeMethodResolverTests` | FAIL | 26/29 | 552ms |
| `core.GenericTypeResolverTests` | FAIL | 24/25 | 369ms |
| `core.ResolvableTypeTests` | FAIL | 161/162 | 5853ms |
| `core.annotation.AnnotatedElementUtilsTests` | FAIL | 81/82 | 1610ms |
| `core.annotation.MergedAnnotationsTests` | FAIL | 177/178 | 4168ms |
| `core.annotation.MultipleComposedAnnotationsOnSingleAnnotatedElementTests` | FAIL | 17/19 | 313ms |
| `core.annotation.NestedRepeatableAnnotationsTests` | FAIL | 2/12 | 508ms |
| `core.io.ResourceTests` | FAIL | 66/68 | 2707ms |
| `core.io.buffer.DataBufferTests` | TIMEOUT | 0/0 | 120000ms |
| `core.retry.RetryPolicyTests` | FAIL | 22/23 | 900ms |

### Expression

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `expression.spel.MethodInvocationTests` | FAIL | 21/23 | 1243ms |
| `expression.spel.SpelCompilationCoverageTests` | FAIL | 161/162 | 16253ms |

### Format

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `format.datetime.standard.DateTimeFormattingTests` | FAIL | 58/61 | 4275ms |
| `format.datetime.standard.InstantFormatterTests` | FAIL | 30/40 | 1893ms |

### Http

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `http.ContentDispositionTests` | FAIL | 33/34 | 755ms |
| `http.client.SimpleClientHttpRequestFactoryTests` | FAIL | 6/10 | 3658ms |
| `http.codec.cbor.JacksonCborDecoderTests` | FAIL | 0/3 | 127ms |
| `http.converter.StringHttpMessageConverterTests` | FAIL | 12/13 | 245ms |
| `http.converter.json.GsonHttpMessageConverterTests` | FAIL | 13/14 | 308ms |
| `http.converter.json.JacksonJsonHttpMessageConverterTests` | FAIL | 33/34 | 1638ms |
| `http.converter.json.JsonbHttpMessageConverterTests` | FAIL | 13/14 | 525ms |
| `http.converter.json.KotlinSerializationJsonHttpMessageConverterTests` | FAIL | 24/25 | 1916ms |
| `http.converter.json.MappingJackson2HttpMessageConverterTests` | FAIL | 31/32 | 2608ms |

### Jdbc

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jdbc.core.namedparam.BeanPropertySqlParameterSourceTests` | FAIL | 7/10 | 284ms |
| `jdbc.core.namedparam.MapSqlParameterSourceTests` | FAIL | 3/6 | 139ms |

### Jms

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jms.core.JmsTemplateTransactedTests` | FAIL | 51/52 | 3616ms |

### Jmx

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jmx.access.MBeanClientInterceptorTests` | FAIL | 4/14 | 3583ms |
| `jmx.access.RemoteMBeanClientInterceptorTests` | FAIL | 2/14 | 8062ms |
| `jmx.export.CustomEditorConfigurerTests` | FAIL | 1/2 | 502ms |
| `jmx.export.LazyInitMBeanTests` | FAIL | 0/1 | 491ms |
| `jmx.export.MBeanExporterOperationsTests` | FAIL | 3/6 | 315ms |
| `jmx.export.MBeanExporterTests` | FAIL | 33/38 | 3291ms |
| `jmx.export.NotificationListenerTests` | FAIL | 1/13 | 800ms |
| `jmx.export.NotificationPublisherTests` | FAIL | 3/4 | 1237ms |
| `jmx.export.PropertyPlaceholderConfigurerTests` | FAIL | 1/2 | 334ms |
| `jmx.export.annotation.AnnotationLazyInitMBeanTests` | FAIL | 0/3 | 3425ms |
| `jmx.export.annotation.AnnotationMetadataAssemblerTests` | FAIL | 21/31 | 16776ms |
| `jmx.export.annotation.EnableMBeanExportConfigurationTests` | FAIL | 2/8 | 4030ms |
| `jmx.export.assembler.InterfaceBasedMBeanInfoAssemblerCustomTests` | FAIL | 7/13 | 3652ms |
| `jmx.export.assembler.InterfaceBasedMBeanInfoAssemblerMappedTests` | FAIL | 11/17 | 2927ms |
| `jmx.export.assembler.InterfaceBasedMBeanInfoAssemblerTests` | FAIL | 6/12 | 4354ms |
| `jmx.export.assembler.MethodExclusionMBeanInfoAssemblerComboTests` | FAIL | 8/14 | 2108ms |
| `jmx.export.assembler.MethodExclusionMBeanInfoAssemblerMappedTests` | FAIL | 8/14 | 3456ms |
| `jmx.export.assembler.MethodExclusionMBeanInfoAssemblerNotMappedTests` | FAIL | 8/14 | 3519ms |
| `jmx.export.assembler.MethodExclusionMBeanInfoAssemblerTests` | FAIL | 8/14 | 4301ms |
| `jmx.export.assembler.MethodNameBasedMBeanInfoAssemblerMappedTests` | FAIL | 9/15 | 4472ms |
| `jmx.export.assembler.MethodNameBasedMBeanInfoAssemblerTests` | FAIL | 8/14 | 3119ms |
| `jmx.export.assembler.ReflectiveAssemblerTests` | FAIL | 6/12 | 1796ms |
| `jmx.export.naming.IdentityNamingStrategyTests` | FAIL | 0/1 | 40ms |
| `jmx.export.naming.MetadataNamingStrategyTests` | FAIL | 0/6 | 131ms |
| `jmx.support.JmxUtilsTests` | FAIL | 11/12 | 192ms |
| `jmx.support.MBeanServerFactoryBeanTests` | FAIL | 5/7 | 146ms |

### Jndi

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jndi.JndiObjectFactoryBeanTests` | FAIL | 24/25 | 1366ms |

### Messaging

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `messaging.simp.stomp.ReactorNettyStompBrokerRelayIntegrationTests` | FAIL | 0/7 | 622ms |
| `messaging.simp.stomp.ReactorNettyTcpStompClientTests` | FAIL | 0/1 | 216ms |

### Orm

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` | TIMEOUT | 0/0 | 120000ms |
| `orm.jpa.support.PersistenceInjectionTests` | FAIL | 26/27 | 2706ms |

### R2dbc

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `r2dbc.core.H2DatabaseClientContextIntegrationTests` | FAIL | 0/10 | 1074ms |

### Resilience

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `resilience.RetryInterceptorTests` | FAIL | 20/21 | 3748ms |

### Scheduling

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scheduling.annotation.AsyncExecutionTests` | FAIL | 16/18 | 11229ms |
| `scheduling.annotation.EnableAsyncTests` | FAIL | 17/18 | 14467ms |
| `scheduling.concurrent.ConcurrentTaskExecutorTests` | FAIL | 16/18 | 4648ms |
| `scheduling.concurrent.DecoratedThreadPoolTaskExecutorTests` | FAIL | 12/14 | 4667ms |
| `scheduling.concurrent.ThreadPoolTaskExecutorTests` | FAIL | 19/23 | 4861ms |
| `scheduling.concurrent.ThreadPoolTaskSchedulerTests` | FAIL | 38/40 | 6048ms |
| `scheduling.quartz.QuartzSupportTests` | FAIL | 8/17 | 5071ms |

### Scripting

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scripting.groovy.GroovyAspectTests` | FAIL | 3/4 | 5085ms |
| `scripting.groovy.GroovyScriptFactoryTests` | FAIL | 28/38 | 53953ms |

### Test

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `test.context.BootstrapUtilsTests` | FAIL | 22/23 | 766ms |
| `test.context.aot.AotIntegrationTests` | FAIL | 0/4 | 59847ms |
| `test.context.aot.TestClassScannerTests` | TIMEOUT | 0/0 | 120000ms |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL | 0/4 | 16952ms |
| `test.context.aot.samples.jdbc.SqlScriptsSpringJupiterTests` | FAIL | 0/1 | 1497ms |
| `test.context.bean.override.convention.TestBeanByTypeLookupIntegrationTests` | FAIL | 1/4 | 523ms |
| `test.context.bean.override.mockito.MockitoBeanByTypeLookupIntegrationTests` | FAIL | 3/5 | 6188ms |
| `test.context.bean.override.mockito.constructor.MockitoBeanByTypeLookupForConstructorParametersIntegrationTests` | FAIL | 4/6 | 5994ms |
| `test.context.config.interfaces.SqlConfigInterfaceTests` | FAIL | 0/1 | 1285ms |
| `test.context.jdbc.AfterTestClassSqlScriptsTests` | FAIL | 0/4 | 1434ms |
| `test.context.jdbc.BeforeTestClassSqlScriptsTests` | FAIL | 0/6 | 1840ms |
| `test.context.jdbc.ComposedAnnotationSqlScriptsTests` | FAIL | 0/1 | 2544ms |
| `test.context.jdbc.CustomScriptSyntaxSqlScriptsTests` | FAIL | 0/1 | 2997ms |
| `test.context.jdbc.DataSourceOnlySqlScriptsTests` | FAIL | 0/2 | 2022ms |
| `test.context.jdbc.DefaultScriptDetectionSqlScriptsTests` | FAIL | 0/2 | 2118ms |
| `test.context.jdbc.GlobalCustomScriptSyntaxSqlScriptsTests` | FAIL | 0/1 | 2692ms |
| `test.context.jdbc.InferredDataSourceSqlScriptsTests` | FAIL | 0/2 | 2147ms |
| `test.context.jdbc.InferredDataSourceTransactionalSqlScriptsTests` | FAIL | 0/2 | 3555ms |
| `test.context.jdbc.InfrastructureProxyTransactionalSqlScriptsTests` | FAIL | 0/1 | 911ms |
| `test.context.jdbc.IsolatedTransactionModeSqlScriptsTests` | FAIL | 0/1 | 1443ms |
| `test.context.jdbc.MetaAnnotationSqlScriptsTests` | FAIL | 0/2 | 152ms |
| `test.context.jdbc.MultipleDataSourcesAndTransactionManagersSqlScriptsTests` | FAIL | 0/2 | 1301ms |
| `test.context.jdbc.MultipleDataSourcesAndTransactionManagersTransactionalSqlScriptsTests` | FAIL | 0/2 | 1154ms |
| `test.context.jdbc.NonTransactionalSqlScriptsTests` | FAIL | 0/2 | 941ms |
| `test.context.jdbc.PopulatedSchemaTransactionalSqlScriptsTests` | FAIL | 0/1 | 1022ms |
| `test.context.jdbc.PrimaryDataSourceTests` | FAIL | 0/1 | 919ms |
| `test.context.jdbc.PropertyPlaceholderSqlScriptsTests` | FAIL | 0/2 | 2715ms |
| `test.context.jdbc.RepeatableSqlAnnotationSqlScriptsChildTests` | FAIL | 0/2 | 1070ms |
| `test.context.jdbc.RepeatableSqlAnnotationSqlScriptsParentTests` | FAIL | 0/2 | 163ms |
| `test.context.jdbc.TransactionalAfterTestMethodSqlScriptsTests` | FAIL | 0/2 | 201ms |
| `test.context.jdbc.TransactionalInlinedStatementsSqlScriptsTests` | FAIL | 0/2 | 1218ms |
| `test.context.jdbc.TransactionalSqlScriptsTests` | FAIL | 0/4 | 355ms |
| `test.context.jdbc.merging.ClassLevelMergeSqlMergeModeTests` | FAIL | 0/3 | 196ms |
| `test.context.jdbc.merging.ClassLevelOverrideSqlMergeModeTests` | FAIL | 0/3 | 188ms |
| `test.context.junit.jupiter.FailingBeforeAndAfterMethodsSpringExtensionTests` | FAIL | 7/9 | 2066ms |
| `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` | FAIL | 0/2 | 395ms |
| `test.context.junit.jupiter.nested.SqlScriptNestedTests` | FAIL | 0/8 | 1266ms |
| `test.context.junit.jupiter.nested.TransactionalNestedTests` | FAIL | 0/8 | 2219ms |
| `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` | TIMEOUT | 0/0 | 120000ms |
| `test.context.junit.jupiter.transaction.TimedTransactionalSpringExtensionTests` | FAIL | 0/1 | 1107ms |
| `test.context.junit.jupiter.transaction.TransactionLifecycleMethodParameterInjectionTests` | FAIL | 0/1 | 2852ms |
| `test.context.junit4.rules.ProgrammaticTxMgmtSpringRuleTests` | FAIL | 0/12 | 1610ms |
| `test.context.junit4.rules.TransactionalSqlScriptsSpringRuleTests` | FAIL | 0/2 | 793ms |
| `test.context.litemode.TransactionalAnnotatedConfigClassWithAtConfigurationTests` | FAIL | 0/2 | 1302ms |
| `test.context.litemode.TransactionalAnnotatedConfigClassesWithoutAtConfigurationTests` | FAIL | 0/2 | 1186ms |
| `test.context.orm.jpa.JpaEntityListenerTests` | FAIL | 0/4 | 2148ms |
| `test.context.orm.jpa.JpaPersonRepositoryTests` | FAIL | 0/2 | 2528ms |
| `test.context.testng.AnnotationConfigTransactionalTestNGSpringContextTests` | FAIL | 0/2 | 2634ms |
| `test.context.testng.TestNGConcurrencyTests` | FAIL | 0/1 | 1613ms |
| `test.context.testng.transaction.programmatic.ProgrammaticTxMgmtTestNGTests` | FAIL | 0/12 | 2889ms |
| `test.context.transaction.DefaultRollbackFalseRollbackAnnotationTransactionalTests` | FAIL | 0/1 | 1400ms |
| `test.context.transaction.DefaultRollbackTrueRollbackAnnotationTransactionalTests` | FAIL | 0/1 | 1464ms |
| `test.context.transaction.RollbackOverrideDefaultRollbackFalseTransactionalTests` | FAIL | 0/1 | 1857ms |
| `test.context.transaction.RollbackOverrideDefaultRollbackTrueTransactionalTests` | FAIL | 0/1 | 1159ms |
| `test.context.transaction.manager.PrimaryTransactionManagerTests` | FAIL | 0/1 | 1338ms |
| `test.context.transaction.programmatic.ProgrammaticTxMgmtTests` | FAIL | 0/12 | 1768ms |
| `test.web.servlet.assertj.MockMvcTesterIntegrationTests` | FAIL | 72/74 | 12538ms |

### Transaction

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `transaction.annotation.AnnotationTransactionNamespaceHandlerTests` | FAIL | 4/5 | 2719ms |
| `transaction.annotation.EnableTransactionManagementIntegrationTests` | FAIL | 4/9 | 7210ms |
| `transaction.interceptor.BeanFactoryTransactionTests` | FAIL | 2/9 | 3323ms |

### Util

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `util.ClassUtilsTests` | FAIL | 104/106 | 1693ms |
| `util.CollectionUtilsTests` | FAIL | 30/32 | 478ms |
| `util.StreamUtilsTests` | FAIL | 10/11 | 2006ms |

### Web

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `web.client.RestClientIntegrationTests` | FAIL | 226/230 | 36198ms |
| `web.client.RestTemplateIntegrationTests` | FAIL | 119/125 | 22291ms |
| `web.client.support.KotlinRestTemplateHttpServiceProxyTests` | FAIL | 8/11 | 5231ms |
| `web.context.request.RequestScopeTests` | FAIL | 5/7 | 2075ms |
| `web.method.HandlerMethodTests` | FAIL | 7/9 | 286ms |
| `web.reactive.function.client.WebClientIntegrationTests` | FAIL | 168/170 | 25637ms |
| `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` | FAIL | 3/5 | 704ms |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL | 3/5 | 695ms |
| `web.servlet.config.MvcNamespaceTests` | FAIL | 24/25 | 10177ms |
| `web.servlet.config.annotation.ViewResolutionIntegrationTests` | FAIL | 5/7 | 8451ms |
| `web.servlet.mvc.method.RequestMappingInfoHandlerMappingTests` | FAIL | 43/43 | 6240ms |
| `web.servlet.mvc.method.annotation.UriTemplateServletAnnotationControllerHandlerMethodTests` | FAIL | 36/38 | 12009ms |
| `web.servlet.view.groovy.GroovyMarkupConfigurerTests` | FAIL | 8/9 | 740ms |
| `web.servlet.view.groovy.GroovyMarkupViewTests` | FAIL | 4/10 | 1434ms |
| `web.socket.config.MessageBrokerBeanDefinitionParserTests` | FAIL | 7/8 | 5653ms |
| `web.socket.config.annotation.WebSocketMessageBrokerConfigurationSupportTests` | FAIL | 11/12 | 8778ms |
| `web.socket.messaging.StompWebSocketIntegrationTests` | FAIL | 13/16 | 79141ms |

## Raw data

- Full-suite merged results: 8 shards, `suite-run.sh`, dev `213d93ea`,
  binary `cratonvm-fullsuite-20260717.bin`.
- 516-class HotSpot baseline: `hs516_final.tsv` (repo root, tracked in git),
  captured 2026-07-09.
- 129-class targeted HotSpot rerun: `suite-run-hotspot.sh`, real JDK 25
  (`/data/data/jdk25-real`), single shard, all 129 passed.
- Per-class FAILCAUSE and crash-log detail available in the worktree's
  `/data/tmp/fullsuite-s{0..7}/{failcauses,crashes}.log` and
  `/data/tmp/hs129/failcauses.log` on the Azure host at capture time.
