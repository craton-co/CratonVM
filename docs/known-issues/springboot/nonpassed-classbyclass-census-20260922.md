# Spring Boot suite — 61 new PASS→FAIL regressions from `--jdk-only` becoming default

| | |
|---|---|
| **Baseline** | 2026-09-13, run `20260913-153854`, compatible-mode-default binary — PASS=938 FAIL=967 CRASH=24 EMPTY=43 ENV-GATED=3 (1975 classes), wall=1337.7s |
| **New run** | 2026-09-21/22, run `20260921-232902`, `--jdk-only`-default binary, local Windows, `C:/craton/CVM/target/release/cratonvm.exe`, real JDK 25, all defaults, category=all — PASS=877 FAIL=1028 CRASH=24 EMPTY=43 ENV-GATED=2 HANG=1 (1975 classes), wall=2601.2s |
| **Diff** | exactly **61 classes flipped PASS→FAIL, 0 flipped FAIL→PASS** — a clean regression set, not sampling noise |
| **Sources** | `apps/spring-boot-suite-runner/.suite/results/{20260913-153854,20260921-232902}/all-jit/results.tsv` |

This page is scoped to the 61-class diff. The much larger FAIL=1028 total is overwhelmingly pre-existing (967 of it was already FAIL on 2026-09-13, before `--jdk-only` was the default) and out of scope here — see the suite's other pages for that history.

## Root cause: `Path.getFileSystem()` returns null under `--jdk-only`

At least **39 of the 61** carry, verbatim, in `results.tsv`'s note column:

```
java.lang.NullPointerException: Cannot invoke "java.nio.file.FileSystem.provider()"
because the return value of "java.nio.file.Path.getFileSystem()" is null
```

(the sibling form `"java.nio.file.FileSystem.getPath(String, String[])"` also appears). A further several of the 61 show the identical NPE **wrapped** one or two frames up — `BeanDefinitionStoreException`, `IllegalStateException: Failed to load ApplicationContext`, or a bare `NullPointerException: Cannot invoke "java.` truncated in the note column exactly where `nio.file.FileSystem...` would continue — consistent with the same defect surfacing through Spring's classpath/resource scanning (`PathMatchingResourcePatternResolver`) rather than directly. In short: the great majority of the 61, plausibly all but a handful, are one mechanism.

Direct repro classes: `org.springframework.boot.SimpleMainTests`, `org.springframework.boot.info.SslInfoTests`.

**This has already been root-caused and handed off as a background task** (`task_568916e9`, "Fix null FileSystem on Path under `--jdk-only`"), which the user has since started in a separate session working in its own worktree (`jdk-only-issues-f23-f25-f26-...`). This page documents the census this sweep found; it does not duplicate that fix work.

The pattern is the mirror image of a bug this same session already found and confirmed fixed on dev tip: `java/util/Enumeration$Impl` fabrication being refused under strict `--jdk-only` (see the jdk-only known-issues history for that story) — there, a synthetic-fabrication refusal with no fallback silently no-opped instead of erroring. Here, a real JDK object (`Path`) is missing a real backing value (`FileSystem`) that `--jdk-only`'s snapshot/native-JDK boundary is supposed to supply.

## A related but distinct sub-cluster: loader/zip NPEs — 3

| class | note |
|---|---|
| `org.springframework.boot.loader.launch.ArchiveTests` | `UnsupportedOperationException: Path not associated with default file system.` |
| `org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests` | `NullPointerException: Cannot invoke "java.util.zip.ZipFile$CleanableResource.clean()"` |
| `org.springframework.boot.loader.net.protocol.jar.UrlJarFilesTests` | same as above |

These three also print a `[cratonvm] JVMS 6.5 uninstantiable-receiver census` diagnostic naming `java/net/JarURLConnection` (abstract) among several other abstract/interface classes a native handed back as if instantiable. Plausibly downstream of the same `Path`/`FileSystem` gap manifesting inside the zip/jar-loader's own filesystem provider rather than in general `Path` usage — **not confirmed** to be the same root cause as the main 39+, just adjacent and worth checking once that fix lands.

## Unclustered — 2

| class | note |
|---|---:|
| `org.springframework.boot.loader.zip.VirtualZipDataBlockTests` | bare `AssertionError` (no message) |
| `org.springframework.boot.test.context.filter.ExcludeFilterApplicationContextInitializerTests` | bare `AssertionError` (no message) |

Not yet characterized — the note column carries no exception text past the bare `AssertionError`, so these need their own log read before attributing a cause.

## Full list of the 61

```
org.springframework.boot.SimpleMainTests
org.springframework.boot.autoconfigure.AutoConfigurationExcludeFilterTests
org.springframework.boot.autoconfigure.condition.ConditionalOnMissingBeanTests
org.springframework.boot.context.ConfigurationWarningsApplicationContextInitializerTests
org.springframework.boot.context.TypeExcludeFilterTests
org.springframework.boot.context.properties.ConfigurationPropertiesScanRegistrarTests
org.springframework.boot.context.properties.ConfigurationPropertiesScanTests
org.springframework.boot.data.couchbase.test.autoconfigure.DataCouchbaseTestPropertiesIntegrationTests
org.springframework.boot.data.ldap.test.autoconfigure.DataLdapTestPropertiesIntegrationTests
org.springframework.boot.data.mongodb.test.autoconfigure.DataMongoTestPropertiesIntegrationTests
org.springframework.boot.data.r2dbc.test.autoconfigure.DataR2dbcTestPropertiesIntegrationTests
org.springframework.boot.diagnostics.analyzer.NoUniqueBeanDefinitionFailureAnalyzerTests
org.springframework.boot.gson.autoconfigure.jsontest.JsonTestWithAutoConfigureJsonTestersTests
org.springframework.boot.gson.autoconfigure.jsontest.SpringBootTestWithAutoConfigureJsonTestersTests
org.springframework.boot.health.autoconfigure.application.SslHealthContributorAutoConfigurationTests
org.springframework.boot.info.SslInfoTests
org.springframework.boot.jackson.JacksonMixinModuleTests
org.springframework.boot.jackson.autoconfigure.jsontest.JsonTestWithAutoConfigureJsonTestersTests
org.springframework.boot.jackson.autoconfigure.jsontest.SpringBootTestWithAutoConfigureJsonTestersTests
org.springframework.boot.jackson2.JsonMixinModuleTests
org.springframework.boot.json.BasicJsonParserTests
org.springframework.boot.json.GsonJsonParserTests
org.springframework.boot.json.JacksonJsonParserTests
org.springframework.boot.jsonb.autoconfigure.jsontest.JsonTestWithAutoConfigureJsonTestersTests
org.springframework.boot.jsonb.autoconfigure.jsontest.SpringBootTestWithAutoConfigureJsonTestersTests
org.springframework.boot.loader.launch.ArchiveTests
org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests
org.springframework.boot.loader.net.protocol.jar.UrlJarFilesTests
org.springframework.boot.loader.nio.file.NestedFileSystemZipFileSystemIntegrationTests
org.springframework.boot.loader.zip.VirtualZipDataBlockTests
org.springframework.boot.micrometer.tracing.test.autoconfigure.AutoConfigureTracingMissingIntegrationTests
org.springframework.boot.persistence.autoconfigure.EntityScannerTests
org.springframework.boot.restclient.test.autoconfigure.AutoConfigureMockRestServiceServerEnabledFalseIntegrationTests
org.springframework.boot.restclient.test.autoconfigure.RestClientTestPropertiesIntegrationTests
org.springframework.boot.restclient.test.autoconfigure.RestClientTestWithConfigurationPropertiesIntegrationTests
org.springframework.boot.ssl.jks.JksSslStoreBundleTests
org.springframework.boot.ssl.pem.LoadedPemSslStoreTests
org.springframework.boot.ssl.pem.PemCertificateParserTests
org.springframework.boot.ssl.pem.PemContentTests
org.springframework.boot.ssl.pem.PemSslStoreBundleTests
org.springframework.boot.test.autoconfigure.json.JsonTestPropertiesIntegrationTests
org.springframework.boot.test.autoconfigure.json.JsonTestWithAutoConfigureJsonTestersTests
org.springframework.boot.test.autoconfigure.json.SpringBootTestWithAutoConfigureJsonTestersTests
org.springframework.boot.test.autoconfigure.override.OverrideAutoConfigurationEnabledFalseIntegrationTests
org.springframework.boot.test.autoconfigure.override.OverrideAutoConfigurationEnabledTrueIntegrationTests
org.springframework.boot.test.context.AnnotatedClassFinderTests
org.springframework.boot.test.context.bootstrap.SpringBootTestContextBootstrapperIntegrationTests
org.springframework.boot.test.context.bootstrap.SpringBootTestContextBootstrapperTests
org.springframework.boot.test.context.bootstrap.SpringBootTestContextBootstrapperWithContextConfigurationTests
org.springframework.boot.test.context.bootstrap.SpringBootTestContextBootstrapperWithInitializersTests
org.springframework.boot.test.context.filter.ExcludeFilterApplicationContextInitializerTests
org.springframework.boot.testsupport.classpath.resources.OnClassWithPackageResourcesTests
org.springframework.boot.testsupport.classpath.resources.OnSuperClassWithPackageResourcesTests
org.springframework.boot.testsupport.classpath.resources.ResourcesTests
org.springframework.boot.testsupport.classpath.resources.WithPackageResourcesTests
org.springframework.boot.web.server.WebServerSslBundleTests
org.springframework.boot.web.server.servlet.context.AnnotationConfigServletWebServerApplicationContextTests
org.springframework.boot.web.server.servlet.context.MockWebEnvironmentServletComponentScanIntegrationTests
org.springframework.boot.web.server.servlet.context.ServletComponentScanIntegrationTests
org.springframework.boot.webclient.test.autoconfigure.WebClientTestPropertiesIntegrationTests
org.springframework.boot.webclient.test.autoconfigure.WebClientTestWithConfigurationPropertiesIntegrationTests
```

## Open items

1. Fix owned by `task_568916e9`, in progress in a separate session as of this writing — do not duplicate.
2. Once that lands, re-run this same 61-class list and confirm all (or nearly all) flip back to PASS; anything that doesn't is either the loader/zip sub-cluster (genuinely distinct) or one of the 2 unclustered `AssertionError`s.
3. The loader/zip sub-cluster (`ArchiveTests`, `JarUrlConnectionTests`, `UrlJarFilesTests`) should be re-checked independently even after the main fix, since its exception shapes differ.
