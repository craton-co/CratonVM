# `module/spring-boot-tomcat` 2026-07-17 rerun: 3 unrelated FAILs + embedded-server throughput-wall HANGs

**Status: OPEN — found 2026-07-17**

This module contributed 6 non-passing classes to this triage batch, splitting
into several unrelated root causes: 3 single-class FAILs (each a distinct
mechanism) and 3 HANGs that are a known, previously-characterized CratonVM
throughput limitation (a 4th HANG class from this module,
`TomcatServletWebServerServletContextListenerTests`, is a **different**
livelock signature already tracked in
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md) —
not repeated here). The `SslConnectorCustomizerTests` FAIL below is also
cross-filed as corroborating evidence in
[`ssl-pem-pkcs12-store-parse-failure-cluster.md`](ssl-pem-pkcs12-store-parse-failure-cluster.md).

## 1. `SslConnectorCustomizerTests` — JKS keystore parse failure blocks 2 protocol-configuration tests

2/8 tests fail:

```
JUnit Jupiter:SslConnectorCustomizerTests:sslEnabledProtocolsConfiguration()
    => java.lang.AssertionError:
Expecting actual not to be null
       org.springframework.boot.tomcat.SslConnectorCustomizerTests.sslEnabledProtocolsConfiguration(SslConnectorCustomizerTests.java:157)
JUnit Jupiter:SslConnectorCustomizerTests:sslEnabledMultipleProtocolsConfiguration()
    => java.lang.AssertionError:
Expecting actual not to be null
       org.springframework.boot.tomcat.SslConnectorCustomizerTests.sslEnabledMultipleProtocolsConfiguration(SslConnectorCustomizerTests.java:140)
```

The `.err.log` shows, for every one of the 5 SSL-connector sub-configurations
the class exercises, a repeating `WARN keystore: JKS key integrity check
failed (wrong password?)` pair followed (for the connectors under test) by
`ERROR [org.apache.catalina.util.LifecycleBase] Failed to initialize
component [Connector[...]] (org/apache/catalina/LifecycleException: Protocol
handler initialization failed)`.

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.SslConnectorCustomizerTests.err.log`
(and matching `.out.log`).

**Root cause: see [`ssl-pem-pkcs12-store-parse-failure-cluster.md`](ssl-pem-pkcs12-store-parse-failure-cluster.md)** —
this class is filed there as a third, independent module hitting the
identical `"JKS key integrity check failed"` text the same day (alongside a
PKCS12 MAC-verification failure in `core/spring-boot-autoconfigure` and a
native crash in `spring-boot-jetty`), strong evidence of one systemic
JKS/PKCS12 keystore-parsing gap in CratonVM's crypto layer. Not duplicated
here in full; see that doc for the details.

## 2. `TomcatEmbeddedWebappClassLoaderTests` — parent-delegated `getResource`/`getResources` never finds resources inside a WAR's `WEB-INF/classes`

2/2 tests fail:

```
JUnit Jupiter:TomcatEmbeddedWebappClassLoaderTests:getResourceFindsResourceFromParentClassLoader()
    => org.opentest4j.AssertionFailedError:
expected: jar:file:C:\Users\...\junit-.../test.war!/WEB-INF/classes/test.txt
 but was: null
       org.springframework.boot.tomcat.TomcatEmbeddedWebappClassLoaderTests.lambda$getResourceFindsResourceFromParentClassLoader$0(TomcatEmbeddedWebappClassLoaderTests.java:55)
       org.springframework.boot.tomcat.TomcatEmbeddedWebappClassLoaderTests.withWebappClassLoader(TomcatEmbeddedWebappClassLoaderTests.java:80)

JUnit Jupiter:TomcatEmbeddedWebappClassLoaderTests:getResourcesOnlyFindsResourcesFromParentClassLoader()
    => org.opentest4j.AssertionFailedError:
Expecting actual:
  []
to contain exactly (and in same order):
  [jar:file:C:\Users\...\junit-.../test.war!/WEB-INF/classes/test.txt]
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.TomcatEmbeddedWebappClassLoaderTests.err.log`
(and matching `.out.log`).

**Root cause (hypothesis, not confirmed against CratonVM native source
this session):** `withWebappClassLoader` (test helper, `.java:80`) builds a
real `.war` file on disk containing `WEB-INF/classes/test.txt`, constructs a
`TomcatEmbeddedWebappClassLoader` with a URL classloader pointed at that WAR
as its **parent**, and asserts `getResource("test.txt")`/`getResources(...)`
delegate up to the parent and resolve to a `jar:file:...!/WEB-INF/classes/test.txt`
URL. Getting `null`/`[]` back means either (a) the parent
`URLClassLoader`'s own `getResource` over a `.war`-suffixed jar-format file
fails to find an entry under `WEB-INF/classes/` on CratonVM (a `jar:` URL
handler / zip-central-directory gap specific to non-`.jar`-suffixed archive
files), or (b) `TomcatEmbeddedWebappClassLoader`'s parent-delegation call
itself never reaches the parent loader. Not traced to a specific CratonVM
source location this session — would need a standalone repro
(`URLClassLoader` over a hand-built `.war` with a `WEB-INF/classes/` entry,
calling `getResource` directly) to distinguish (a) from (b).

## 3. `TomcatMetricsAutoConfigurationTests` — `TomcatMetricsBinder` never binds live Tomcat metrics after `ApplicationStartedEvent`

2/5 tests fail (`autoConfiguresTomcatMetricsWithEmbeddedServletTomcat`,
`autoConfiguresTomcatMetricsWithEmbeddedReactiveTomcat`):

```
=> java.lang.AssertionError:
Expecting actual not to be null
       org.springframework.boot.tomcat.autoconfigure.metrics.TomcatMetricsAutoConfigurationTests.lambda$autoConfiguresTomcatMetricsWithEmbeddedServletTomcat$0(...)
```

Both failing tests share the same shape (`TomcatMetricsAutoConfigurationTests.java:65-71`,
`:82-87`, this worktree): start an embedded Tomcat with
`server.tomcat.mbeanregistry.enabled=true`, call
`context.publishEvent(createApplicationStartedEvent(...))` to trigger
`TomcatMetricsBinder`'s listener (which reads live values off Tomcat's MBean
registry), then assert `registry.find("tomcat.sessions.active.max").meter()`/
`"tomcat.threads.current"` are non-null. The third test in the class
(`autoConfiguresTomcatMetricsWithStandaloneTomcat`, which only asserts the
binder *bean* exists, not that it actually bound live metric values) passes.

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.autoconfigure.metrics.TomcatMetricsA-a32762ffd958.out.log`
(and matching `.err.log`).

**Root cause (hypothesis, not confirmed against CratonVM native source this
session):** `TomcatMetricsBinder`'s `ApplicationStartedEvent` listener binds
Micrometer meters by querying Tomcat's live `Manager`/`ThreadPool` MBeans
(`server.tomcat.mbeanregistry.enabled=true` is set specifically so these
MBeans exist). Given this project's own
`docs/internal/CRATONVM_BUGS/BUG-TC0622-jmx-mbean-registration-missing.md`
already documents CratonVM's synthetic JMX/`MBeanServer` (`native-builtins/src/jmx.rs`,
default-on `experimental-jmx` feature) as a **partial** subsystem, and a
sibling finding this same rerun
(`core-autoconfigure-singleton-fail-residuals-20260717.md` item 4) hits a
different `MBeanServer` gap (wrong exception type on a missing MBean), the
most likely explanation is that Tomcat's Manager/ThreadPool MBeans are never
actually registered with (or queryable from) CratonVM's `MBeanServer`, so
`TomcatMetricsBinder`'s meter-binding lookup finds nothing to bind and the
named meters never get created. Not traced to the specific
`Registry.getRegistry()`/`ObjectName` lookup this session — would need a
standalone repro querying `ManagementFactory.getPlatformMBeanServer()` for
the relevant `Tomcat:type=...` object names right after Tomcat starts, to
confirm the MBeans are (or aren't) actually there.

## 4. Embedded-Tomcat-per-test-method throughput wall (3 HANGs)

| Class | Shape |
|---|---:|
| `org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactoryCustomizerTests` (66 `@Test` methods) | 33 full `tomcat.start()`/deploy cycles completed in ~290s before the shard timeout, still progressing (last cycle cut off mid-startup) |
| `org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests` (18 `@Test` methods) | Similar repeated `Initializing/Starting ProtocolHandler` cycles, killed mid-progress |
| `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` (42 `@Test` methods) | Same shape; `.out.log` ends with a bare `Interrupted!` (harness-killed) |

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactory-2331692b613b.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests.err.log`

All 3 show clean, error-free, repeated `Initializing ProtocolHandler` →
`Starting service [Tomcat]` → `Starting Servlet engine` → `Starting
ProtocolHandler` → `Stopping ProtocolHandler` cycles with no exceptions, no
stall between cycles, and steadily increasing connector-instance counters
(e.g. `http-nio-auto-9` through `http-nio-auto-33`) right up to the point
the shard timeout kills the process — i.e. genuine forward progress, not a
deadlock.

**Root cause: this is the same, already-characterized "embedded-server
deployment throughput wall" documented for the standalone Tomcat test suite
in `docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`.**
That investigation quantified the dominant cost as `update_root_snapshot`
overhead (called on every object-returning native call during Tomcat's
reflection-heavy webapp deployment, `gc`/`vm` internals) multiplied by
per-class method count — each of these 3 classes runs its embedded-server
`tomcat.start()`/deploy/serve/stop cycle **once per `@Test` method** (18-66
methods each), and even with the partial mitigations already landed there
(`CRATONVM_ROOTSNAP_CACHE`, `CRATONVM_SKIP_REDUNDANT_NATIVE_SNAPSHOT`), a
single deploy still costs tens of seconds versus HotSpot's sub-second
deploy — comfortably explaining why HotSpot finishes all methods inside the
suite's per-class timeout while CratonVM does not, without any functional
defect. Confirmed applicable here (not just asserted by analogy) via the
observed per-instance cadence in these 3 logs (~8-9s/cycle for
`TomcatWebServerFactoryCustomizerTests`, consistent with that doc's
measured single-deploy cost) and the complete absence of any error/exception
anywhere in either log.

## Affected classes

| Module | Class | Issue |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.SslConnectorCustomizerTests` | 1 (JKS keystore parse) |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.TomcatEmbeddedWebappClassLoaderTests` | 2 (WAR resource resolution) |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.autoconfigure.metrics.TomcatMetricsAutoConfigurationTests` | 3 (MBean metrics not bound) |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactoryCustomizerTests` | 4 (throughput wall) |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests` | 4 (throughput wall) |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` | 4 (throughput wall) |

Not covered here: `org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests`
(HANG) — see
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md),
a different (livelock, not throughput) signature.
