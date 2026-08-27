# Spring Framework, Spring Boot, and Hibernate Reactive 3-GC full-suite runs (2026-08-24/25): re-triaged 2026-08-27

## Status
**MOSTLY RESOLVED — 2 open.** The one mechanism this page root-caused —
`Class.getDeclaredMethods()` ordering — **has been falsified as the cause of
anything listed here**, by measurement rather than by argument.

Two independent re-runs happened on 2026-08-27 and agree. Both are kept, because
each covers what the other does not:

* a **broad sweep** of the full non-passed union (27 Spring Framework, 18 Spring
  Boot, 11 Hibernate Reactive), ZGC only, serial — recovering 24/27, 11/18 and
  10/11. It retracted the root cause but left *why* open, and took no HotSpot
  control;
* a **narrow re-triage** of the 38 classes this page names, with a HotSpot
  control for every CratonVM failure and all three GC arms for Hibernate
  Reactive. It settles the root cause by measurement, fixes 5 classes, and
  reclassifies 5 more as "fails identically on HotSpot" — which a sweep without
  a control necessarily counts as remaining CratonVM failures.

Combined: **2 functional defects remain** (a third, the Infinispan one, is fixed
here), plus 3 throughput items that are not
functional failures. 5 fixed here, 25 of the 38 already passed when re-run, and
5 fail identically on HotSpot.

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

The broad sweep reached the same retraction from the other direction: the
method-order divergence is still present against a fresh binary, and no
Jackson / property-order / reflection-ordering fix landed in the window, yet the
failures are gone. That left an either/or — "a coincidental correlation, or
Jackson does not read that order". The table above closes it: **Jackson does not
read that order.**

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

**Open, from the broad sweep rather than this page's own list (1).**
`test.context.aot.AotIntegrationTests` — FAIL, triaged in neither pass and not
among the 18 classes this page enumerates. Carried here so it is not lost along
with the sweep it came from.

**Neither of the other two is a functional failure.** Both are throughput, and
both were already known:

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

The broad sweep covered 11 Hibernate Reactive classes rather than these 5,
recovered 10, and left one: `MultithreadedInsertionWithLazyConnectionTest` — a
HANG, already tracked as a performance-family residual and not new here.

This also settles the reconciliation this page asked for.
`DatabaseHibernateReactiveTest` does NOT need a second cause distinct from the
Windows-locale one in
`hibernate/hibernate-and-hibernate-reactive-not-cratonvm-bugs.md`: on Linux, on
a swept host, it passes 2/2 on all three arms. And
`CollectionStatelessSessionListenerTest` matching a CLEARED batch-02 page is not
a reopening — it passes.

## What is actually left

### 1. `CacheAutoConfigurationTests` — FIXED (`456c09257`)

3 of 59, all Infinispan. `OffHeapMemoryAllocator` calls the restricted
`MemorySegment.reinterpret` from a **static initialiser**, CratonVM refused, and
JVMS 5.5 makes a `<clinit>` failure permanent for the class.

The refusal was the defect. Measured on Adoptium 25.0.4
(`probes/RestrictedFfmPolicyProbe.java`): a JDK 25 launcher WARNS and proceeds,
and throws only under `--illegal-native-access=deny`. `ofAddress` is not
restricted at all — the JDK permits it even under `deny`, because the segment it
returns is zero-length and widening it needs `reinterpret`. CratonVM denied both,
unconditionally. It now matches all three JDK modes, `--illegal-native-access`
is added so `deny` stays reachable, and `untrusted_code` is pinned to deny
regardless — that mode already forced the gate closed, so the new default would
otherwise have loosened it. **59/59.**

### 2. `JerseyWebEndpointManagementContextConfigurationTests` — NOT a defect in the test it fails in

1 of 4. **All four methods pass when run individually.** The failure needs a
specific pair, and only that pair:

| first | then `autoConfigurationIsConditionalOnClassResourceConfig` |
|---|---|
| `refreshSucceedsWithoutHealth` | **FAILS** |
| `jerseyWebEndpointsResourcesRegistrarForEndpointsIsAutoConfigured` | passes |
| `autoConfigurationIsConditionalOnServletWebApplication` | passes |

`refreshSucceedsWithoutHealth` is the one carrying `@ClassPathExclusions`, so it
runs under a `ModifiedClassPathClassLoader`. Something it leaves behind makes the
later test's `servletEndpointDiscoverer` fail to resolve `ObjectProvider<PathMapper>`
— i.e. Spring's `ObjectProvider.class == descriptor.getDependencyType()` identity
check stops matching. So this is **cross-test state leakage**, not anything wrong
with the failing test's own path, and it should not be triaged as an
`ObjectProvider` bug.

**Four hypotheses measured and killed**, recorded so they are not re-run:

* *Class-mirror identity* — every reflective surface (`getParameterTypes`,
  `Field.getType`, `getReturnType`, `Class.forName`, `getSuperclass`,
  `getInterfaces`, `getComponentType`, `getDeclaringClass`, `getClass`) returns
  the canonical mirror. Identical to HotSpot.
* *`FilteredClassLoader` delegation* — a class reached through one is the
  parent's copy, and the hidden class really is hidden.
* *Name-keyed absence memo* — a failed load through a hiding loader does not
  poison the class for the app loader, in either the class or package flavour.
* *Loader-faithful reflection after a duplicate definition* — defining a second
  copy of `ObjectProvider` in a parent-last loader does NOT make the original
  class's `getParameterTypes()` return the duplicate.

The minimal shapes all pass too: a `@Bean` method taking `ObjectProvider<T>` with
zero candidates, with and without `.withClassLoader(new FilteredClassLoader(...))`.
Whatever leaks is narrower than any of these.

### 3. `AotIntegrationTests` — cause chain identified, not fixed

1 of 4 (2 skipped). It is AOT **generation**, and the chain bottoms out in
ByteBuddy rather than in Spring:

```text
TestContextAotException: Failed to process test class
    [...mockito.integration.SpringExtensionAndMockitoExtensionIntegrationTests]
 -> MockitoException: Could not modify all classes [interface ...$UserService]
 -> IllegalStateException
 -> IllegalArgumentException: Unknown type:
        org/springframework/test/context/bean/override/mockito/hierarchies/FooService
```

"Unknown type" is ByteBuddy's `TypePool` failing to resolve a class file.
**Resource access is ruled out**: `getResourceAsStream`, `getResource` and
`Class.getResourceAsStream` all return the same bytes as HotSpot for
`FooService`, for the failing test class, and for `AotIntegrationTests` itself
(`probes/ClassBytesProbe.java`). So the bytes are reachable and something else in
the Mockito-inline / ByteBuddy path cannot see them — the same family as
`BeanRegistrationsAotContributionTests`, which is AOT + Mockito too.

And three throughput items that are NOT functional failures, all pre-existing —
listed so they are not re-triaged as defects:

4. `BeanRegistrationsAotContributionTests` — passes 14/14 in 21.6 min; scored a
   timeout by the harness cap alone.
5. `WebClientIntegrationTests` — 1/170, reactive-throughput residual.
6. `MultithreadedInsertionWithLazyConnectionTest` — HANG, tracked perf family.

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
* **Take the HotSpot control before calling something a remaining failure.**
  Five classes counted as still-failing fail identically on HotSpot, with
  matching counts. A sweep without a control cannot separate "our defect" from
  "this test passes nowhere", and here that is 5 of the residual list —
  including three of the four micrometer/loader classes the broad sweep left
  open.

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
