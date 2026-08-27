# Spring Framework, Spring Boot, and Hibernate Reactive 3-GC full-suite runs (2026-08-24/25): re-triaged 2026-08-27

## Status
**MOSTLY RESOLVED.** Of the 38 classes this page listed, **2 functional
defects remain**, plus 2 throughput items that are not failures. 5 were fixed
here, 25 already passed by the time they were re-run, and 5 fail identically on
HotSpot. The one mechanism this page root-caused —
`Class.getDeclaredMethods()` ordering — **has been falsified as the cause of
anything listed here**, by measurement rather than by argument.

The re-run is on `origin/dev` at `cdab6441d` and later, one class per process,
with a HotSpot control taken for every CratonVM failure.

## The root cause this page named is not the cause. Measured.

> The section this replaces read: *"Root-caused: `Class.getDeclaredMethods()`
> order diverges from HotSpot … Jackson's bean-property introspection walks
> `getDeclaredMethods()` … A different method order therefore produces a
> different JSON property order — confirmed directly."*
>
> It was not confirmed directly. Two orderings were observed to differ, a
> consumer known to read one of them was named, and the connection between them
> was assumed. It does not hold.

### The arithmetic never worked

The page's own two observations contradict each other, and this is visible
without running anything. On BOTH VMs the declared-method order lists
`getSomeDouble` **before** `isSomeBoolean`, so a serializer walking that order
emits `someDouble` first on both. Yet the failing assertion is:

```text
expected:<{"name":"Joe","someBoolean":false,"someDouble":0.0}>   (HotSpot)
     was:<{"name":"Joe","someDouble":0.0,"someBoolean":false}>   (CratonVM)
```

HotSpot emits `someBoolean` FIRST — which is neither its own getter order nor
its field order (`name, someDouble, someBoolean`, identical on both VMs). No
enumeration order on either VM produces HotSpot's JSON.

### What actually decides it

`probes/PersonOrderProbe.java` prints all four observables in one process on
each VM, against the real `org.springframework.test.web.Person` fixture:

| observable | HotSpot | CratonVM |
|---|---|---|
| `getDeclaredMethods` (getters) | name, someDouble, someBoolean | name, someDouble, someBoolean |
| `getDeclaredFields` | name, someDouble, someBoolean | **identical** |
| **Jackson 3** (`tools.jackson`) | `{"name","someBoolean","someDouble"}` | **identical** |
| **Jackson 2** (`com.fasterxml`) | `{"name","someDouble","someBoolean"}` | **identical** |
| `java.beans.Introspector` | class, name, someBoolean, someDouble | **identical** |

`name < someBoolean < someDouble` is **alphabetical**. Jackson 3 — which Spring
7 uses — sorts properties alphabetically by default; Jackson 2 uses declaration
order. Neither reads `getDeclaredMethods()` order, and **both emit byte-identical
JSON on the two VMs with the divergent method order in place.** That is the
decisive fact: the divergence is present and the output does not move.

The failing assertion is therefore CratonVM producing *Jackson 2's* answer where
HotSpot produces *Jackson 3's*. Converter selection was checked too and is also
identical — `ClassUtils.isPresent` for both mapper classes, and the whole
`RestTemplate` default converter list including
`JacksonJsonHttpMessageConverter`, agree on both VMs
(`probes/JacksonPickProbe.java`). So the JSON-ordering symptom was never
reproduced outside the suite at all, and every class it was attributed to now
passes for unrelated reasons.

### The divergence is real, and is filed separately

`Class.getDeclaredMethods()` order genuinely differs:

```text
HotSpot:   getName, equals, toString, hashCode, setName, getSomeDouble, …
CratonVM:  getName, setName, getSomeDouble, …, equals, hashCode, toString
```

CratonVM returns declaration order; HotSpot's is its own (`InstanceKlass`
method-table order, which is not source order and is unspecified by the JLS).
**No failure in this page is caused by it**, and nothing here should be planned
against it. It is left recorded as an observation, not a defect with a victim:
before spending anything on replicating HotSpot's order, find a consumer whose
output actually moves — the one this page nominated does not.

## Spring Framework — 18 listed, 0 functional defects

Re-run one class per process; every CratonVM failure re-run on HotSpot.

**Now pass, no change needed (12).** Every class the page attributed to the
`getDeclaredMethods` theory is in here:

`context.JavaConfigTests` · `context.XmlConfigTests` ·
`standalone.AsyncTests` · `client.standalone.FilterTests` ·
`ResponseBodyResultHandlerTests` · `ResponseEntityResultHandlerTests` ·
`RequestResponseBodyMethodProcessorTests` ·
`MessagingMessageListenerAdapterTests` · `JacksonCsvEncoderTests` ·
`DispatcherHandlerErrorTests` · `RequestMappingExceptionHandlingIntegrationTests` ·
`ResourceHttpRequestHandlerIntegrationTests`

**Fixed (3).**

| class | was | now | fix |
|---|---|---|---|
| `http.server.reactive.ZeroCopyIntegrationTests` | 1/2 | **2/2** | `e401a173d` |
| `RequestMappingMessageConversionIntegrationTests` | 159/160 | **160/160** | `e401a173d` |
| `core.retry.RetryPolicyTests` | 22/23 | **23/23** | `3b4d1e9cd` |

The first two are one defect, not two: both fail only their `[3] Reactor Netty`
arm with `Premature end of Content-Length delimited message body (expected: 951;
received: 0)` — a zero-copy `sendFile` announcing a length and sending nothing.
`FileChannelImpl.transferTo` takes a DIRECT `sendfile(2)` arm that first calls
`SocketChannelImpl.beforeTransferTo()`, ordinary JDK bytecode reading the
channel's own `private final` slots; CratonVM builds socket channels without
running the JDK constructor, so `writeLock` was null and it threw. The direct arm
is now declined (`canTransferToDirectly` → false), which is the JDK's own way of
saying "not this target"; file→file `sendfile` is untouched.

`RetryPolicyTests` needed two lambda-naming fixes, and the first one alone did
not fix it — the asserted string has no `@hash`, so it never came from
`toString`. See `probes/LambdaSimpleNameProbe.java`.

**Not a CratonVM bug (1).** `aot.nativex.FileNativeConfigurationWriterTests` —
HotSpot fails identically, as this page already recorded.

**Neither is a functional failure (2).** Both are throughput, and both were
already known:

* `beans.factory.aot.BeanRegistrationsAotContributionTests` — **PASSES, 14/14**,
  in **1 297 s (21.6 min)**. It is scored a timeout by the suite because it
  exceeds the harness cap, not because anything is wrong with the result. This
  page inherited "times out" from the sweep and it is worth stating precisely:
  the AOT/Mockito dispatch-throughput wall makes it slow, not broken.
* `web.reactive.function.client.WebClientIntegrationTests` — **1 of 170**,
  `AssertionError: VerifySubscriber timed out`. The pre-existing
  reactive-throughput residual this page already suspected, not a new defect.

## Spring Boot — 15 listed, 2 open

**Now pass, no change needed (8).** Again this is every Jackson- and
health-named class, i.e. the whole "plausibly the same root cause" group:

`jackson.JacksonComponentModuleTests` (9/9) ·
`jackson.JacksonMixinModuleTests` (5/5) ·
`jackson.autoconfigure.JacksonAutoConfigurationTests` (162/162) ·
`health.actuate.endpoint.CompositeHealthDescriptorTests` (6/6) ·
`health.actuate.endpoint.IndicatedHealthDescriptorTests` (2/2) ·
`health.actuate.endpoint.SystemHealthDescriptorTests` (3/3) ·
`health.contributor.HealthTests` (19/19) ·
`kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests` (3/3)

The last one is the Testcontainers flake this page flagged as worth re-running
before assuming it was new. It was worth re-running: it passes.

**Fixed (1).** `loader.nio.file.NestedPathTests` 30/31 → **31/31**
(`cdab6441d`). `toUriWhenHasSpecialCharsReturnsEncodedUri` died with
`IOError: URISyntaxException: Illegal character in path at index 40`, index 40
being a space. `URI.getRawPath()` answered the DECODED path on a
`Path.toUri()` result while `toString()` and `new URI(s).getRawPath()` were both
already correct. Two layers: `uri_publish_named` wrote one string to both `path`
(raw) and `decodedPath`, and both `Path.toUri()` registrations handed it the
decoded spelling. See `probes/UriRawPathProbe.java`.

**Not a CratonVM bug (4)** — HotSpot fails identically, same counts:

| class | both VMs |
|---|---|
| `loader.zip.ZipContentTests` | `aborted=1` |
| `…export.datadog.DatadogPropertiesConfigAdapterTests` | `failed=1` |
| `…export.otlp.OtlpMetricsExportAutoConfigurationTests` | `failed=2` |
| `…brave.autoconfigure.OtlpExemplarsAutoConfigurationTests` | `failed=2` |

**Still open (2).**

* `cache.autoconfigure.CacheAutoConfigurationTests` — 3 of 59:
  `infinispanAsJCacheWithCaches`, `infinispanAsJCacheWithConfig`,
  `infinispanCacheWithConfig`, all
  `IllegalStateException: Unstarted application context … startupFailure=
  UnsatisfiedDependencyException`. Infinispan-specific; HotSpot 59/59.
* `jersey.autoconfigure.actuate.web.JerseyWebEndpointManagementContextConfigurationTests`
  — 1 of 4, `NoSuchBeanDefinitionException: No qualifying bean of type
  'ObjectProvider<PathMapper>'`. A generic-type-resolution shape; HotSpot 4/4.

## Hibernate Reactive — 5 listed, 0 open

**All five pass, on all three GC arms** (`-XX:+UseGenerationalGC`,
`-XX:+UseG1GC`, `-XX:+UseZGC`) — 15/15 green, with real counts
(`found=1..4 ok=1..4`), not a zero-discovery green:

`it.quarkus.qe.database.DatabaseHibernateReactiveTest` ·
`BlockTableGeneratorTest` · `CollectionStatelessSessionListenerTest` ·
`TableGeneratorTest` · `dynamic.DynamicEntityTest`

**The likely cause of the original signal is the fixture, not the VM.** Before
this re-run the host had **four `postgres:18.4` containers `Up` for 30 hours**,
left by the 08-25 run that produced this page's data.
`TESTCONTAINERS_RYUK_DISABLED=true` leaks one container per run, and once enough
are up, live tests fail with `ClosedConnectionException: Failed to read any
response from the server` and `FATAL: terminating connection due to unexpected
postmaster exit` — which reads exactly like a VM/GC defect and is not one. The
re-run swept between classes and logged the container count beside every result,
so "the host was clean" is a recorded measurement rather than an assumption.

This also settles the reconciliation this page asked for.
`DatabaseHibernateReactiveTest` does NOT need a second cause distinct from the
Windows-locale one in
`hibernate/hibernate-and-hibernate-reactive-not-cratonvm-bugs.md`: on Linux, on
a swept host, it passes 2/2 on all three arms. And
`CollectionStatelessSessionListenerTest` matching a CLEARED batch-02 page is not
a reopening — it passes.

## What is actually left

Two functional defects, neither of them this page's original mechanism:

1. `CacheAutoConfigurationTests` — 3 Infinispan contexts fail to start.
2. `JerseyWebEndpointManagementContextConfigurationTests` — `ObjectProvider<PathMapper>` unsatisfied.

And two throughput items that are NOT failures, both pre-existing and both
already tracked elsewhere — listed so they are not re-triaged as defects:

3. `BeanRegistrationsAotContributionTests` — passes 14/14 in 21.6 min; scored a
   timeout by the harness cap alone.
4. `WebClientIntegrationTests` — 1/170, reactive-throughput residual.

## Method note — why a two-day-old suite list needs re-running before it is triaged

25 of the 38 classes here already passed by the time anyone looked at them, on a
tree only two days newer. This page was right to say its entries were probably
not regressions; what it did not do is re-run them before building a root cause
on top of the list. Two cheap habits would have caught both errors on this page:

* **Re-run the list before theorising.** Every class in the Jackson cluster —
  the evidence base for the whole `getDeclaredMethods` story — already passed.
* **Check that the consumer reads the thing you measured.** Two orderings
  differing is not a mechanism. One probe printing the serializer's actual
  output next to the ordering would have refuted it in a single run, which is
  what eventually happened.

## Reproduction

```bash
# the ordering question, settled in one process per VM (no framework needed
# beyond the spring-test classpath)
javac -cp "<spring-test cp>" -d probeclasses probes/PersonOrderProbe.java
<cratonvm> --java-home <jdk25> -c "probeclasses:<spring-test cp>" PersonOrderProbe
<jdk25>/bin/java              -cp "probeclasses:<spring-test cp>" PersonOrderProbe

# Hibernate Reactive — SWEEP FIRST or the result is about the containers
docker rm -f $(docker ps -a --filter ancestor=postgres:18.4 -q)
```

Full per-variant results from the original 3-GC runs:
* Spring Framework: `/data/spring-fullsuite3gc-20260824/{default,g1,zgc}-jit-real-all-*/results.tsv`
* Spring Boot: `/data/cratonvm/apps/spring-boot-suite-runner/.suite/results/{default,g1,zgc}-fullsuite-20260825/all-jit/results.tsv`
* Hibernate Reactive: `/data/cratonvm/apps/hibernate-reactive-suite-runner/runs/full-{generational,g1,zgc}-20260825_175236/run-20260825-175236-passed/on-real/results.tsv`
