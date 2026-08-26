# Spring Framework, Spring Boot, and Hibernate Reactive 3-GC full-suite runs (2026-08-24/25): newly-visible FAILs, one root-caused

## Status
**OPEN, mixed confidence, one cluster root-caused.** Filed as a single
cross-project doc rather than three, since most of what's here is one
mechanism (`Class.getDeclaredMethods()` ordering) surfacing in three
different test suites, and splitting it three ways would just duplicate the
same investigation.

## Why these look like "regressions" but probably aren't
All three suites were rerun in full (3 GC variants each: generational/G1/ZGC)
after several days of major fixes (`ConcurrentHashMap.entrySet().removeIf()`
NPE, `MethodHandle.asSpreader`, a JIT operand-stack spill-slot bug, immutable-
collection serialization). Comparing against each project's own known-issues
history: almost none of the classes failing now were flagged before. That is
consistent with **newly-exposed, pre-existing bugs, not new breakage** — the
classes below likely always hit this reflection-ordering divergence, but
never got far enough to demonstrate it while the earlier, more fundamental
blockers (CHM/MethodHandle/JIT) were still in the way. Nothing here has been
shown to be an actual regression (a thing that used to pass and now doesn't
on the same binary); that would need a bisect this doc does not attempt.

## Root-caused: `Class.getDeclaredMethods()` order diverges from HotSpot

### Confirmed with a direct probe on a real fixture class
`org.springframework.test.web.Person` (the shared JSON/XML fixture behind
several Spring Framework MVC-sample tests):
```java
for (Method m : Person.class.getDeclaredMethods()) System.out.println(m.getName());
```
```
HotSpot:   getName, equals, toString, hashCode, setName, getSomeDouble, setSomeDouble, isSomeBoolean, setSomeBoolean
CratonVM:  getName, setName, getSomeDouble, setSomeDouble, isSomeBoolean, setSomeBoolean, equals, hashCode, toString
```
`Class.getDeclaredFields()` order matches between the two VMs for the same
class — this is specifically a **method** enumeration-order divergence, not
a field one. CratonVM's order matches the class's literal source/bytecode
declaration order; HotSpot's does not (HotSpot's own `getDeclaredMethods()`
order is JLS-unspecified and does not have to match source order — in
practice it interleaves `equals`/`toString`/`hashCode` differently from
javac's declaration order for reasons not investigated here).

### Why this matters: Jackson's default property order comes from this
Jackson's bean-property introspection walks `getDeclaredMethods()` (absent
an explicit `@JsonPropertyOrder` or alphabetic-sort config) to decide
serialization order. A different method order therefore produces a
different JSON property order — confirmed directly:
```
expected:<{"name":"Joe","someBoolean":false,"someDouble":0.0}>
     was:<{"name":"Joe","someDouble":0.0,"someBoolean":false}>
```
identical on all three Spring Framework classes below that hit it.

## Spring Framework — 20 classes fail on all 3 GC variants (generational/G1/ZGC), full 2,848-class suite, 2026-08-24

Only 2 of the 20 were already known:
* `FileNativeConfigurationWriterTests` — not a CratonVM bug (HotSpot fails
  identically; see `spring/not-cratonvm-bugs-consolidated.md`).
* `BeanRegistrationsAotContributionTests` — known AOT/Mockito
  dispatch-throughput wall (times out; predates this run).

**Confirmed hitting the `getDeclaredMethods()`-order bug** (identical
JSON-property-swap symptom, `someBoolean`/`someDouble` transposed):
* `org.springframework.test.web.servlet.samples.context.JavaConfigTests`
* `org.springframework.test.web.servlet.samples.context.XmlConfigTests`
* `org.springframework.test.web.servlet.samples.standalone.AsyncTests`

**Plausibly the same root cause, not individually confirmed** — same
Jackson-JSON-output theme:
* `org.springframework.test.web.servlet.samples.client.standalone.FilterTests`
  (`AssertionError: Response header 'ETag' expected:<[...]> but was:<[...]>`
  — an ETag is typically a hash of the response body; a reordered JSON body
  hashes differently)
* `org.springframework.web.reactive.result.method.annotation.ResponseBodyResultHandlerTests`
  (`problemDetailContentNegotiation`)
* `org.springframework.web.reactive.result.method.annotation.ResponseEntityResultHandlerTests`
  (`handleErrorResponse`, `handleProblemDetail`)
* `org.springframework.web.servlet.mvc.method.annotation.RequestResponseBodyMethodProcessorTests`
  (`problemDetailWhenProblemXmlRequested`)
* `org.springframework.jms.listener.adapter.MessagingMessageListenerAdapterTests`
  (`replyJackson` — the method name itself names Jackson)

**Not yet triaged** (no obvious shared theme, or too little evidence
gathered in this pass):
* `org.springframework.core.retry.RetryPolicyTests` (`predicatesCombined`, bare `AssertionError`)
* `org.springframework.http.codec.json.JacksonCsvEncoderTests` (`encode`, bare `AssertionFailedError` — also Jackson-named, worth checking against the same theory first)
* `org.springframework.http.server.reactive.ZeroCopyIntegrationTests` (`[3] Reactor Netty`, `RestClientException` extracting an `image/png` response)
* `org.springframework.web.reactive.DispatcherHandlerErrorTests` (`noStaticResource`)
* `org.springframework.web.reactive.function.client.WebClientIntegrationTests` — likely the already-known reactive-throughput residual (`VerifySubscriber timed out`), not new
* `org.springframework.web.reactive.result.method.annotation.RequestMappingExceptionHandlingIntegrationTests`
* `org.springframework.web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests`
* `org.springframework.web.servlet.resource.ResourceHttpRequestHandlerIntegrationTests` (`noResourceFoundException`)

## Spring Boot — 15 classes fail on all 3 GC variants, full 1,991-class suite, 2026-08-25

None matched existing known-issues docs. **Plausibly the same
`getDeclaredMethods()`-order root cause** (Jackson-named or JSON-serializing
endpoints), not individually confirmed:
* `org.springframework.boot.jackson.JacksonComponentModuleTests`
* `org.springframework.boot.jackson.JacksonMixinModuleTests`
* `org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests`
* `org.springframework.boot.health.actuate.endpoint.CompositeHealthDescriptorTests`
* `org.springframework.boot.health.actuate.endpoint.IndicatedHealthDescriptorTests`
* `org.springframework.boot.health.actuate.endpoint.SystemHealthDescriptorTests`
* `org.springframework.boot.health.contributor.HealthTests`
  (health descriptors serialize to JSON for the actuator endpoint, same
  Jackson-ordering exposure surface as Spring Framework's MVC samples)

**Not yet triaged, no obvious shared theme**:
* `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests`
* `org.springframework.boot.jersey.autoconfigure.actuate.web.JerseyWebEndpointManagementContextConfigurationTests`
* `org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests`
  (previously seen passing 3/3 on 2026-08-20 with an older binary — worth
  a direct rerun before assuming this is new, could be a Testcontainers
  flake)
* `org.springframework.boot.loader.nio.file.NestedPathTests`
* `org.springframework.boot.loader.zip.ZipContentTests`
* `org.springframework.boot.micrometer.metrics.autoconfigure.export.datadog.DatadogPropertiesConfigAdapterTests`
* `org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.OtlpMetricsExportAutoConfigurationTests`
* `org.springframework.boot.micrometer.tracing.brave.autoconfigure.OtlpExemplarsAutoConfigurationTests`

## Hibernate Reactive — 5 classes fail on all 3 GC variants, "passed"-category rerun (206 classes), 2026-08-25

**Partially conflicts with an existing doc, needs reconciliation, not
assumed identical:**
* `org.hibernate.reactive.it.quarkus.qe.database.DatabaseHibernateReactiveTest`
  (`nameIsNull`, `nameIsTooLong`) — `hibernate/hibernate-and-hibernate-reactive-not-cratonvm-bugs.md`
  attributes this exact class/method's failure to a **Windows-only** cause
  (host `ru_RU` display language makes hibernate-validator pick the Russian
  message instead of the English one the test asserts, "identical under
  HotSpot"). This run is on **Azure Linux**, not Windows, so that specific
  mechanism cannot be what's firing here — same symptom, likely a different
  or additional cause. Not re-diagnosed in this pass.

**Match older, possibly-stale investigation docs** (Aug 12) — status unclear,
not reconciled:
* `org.hibernate.reactive.BlockTableGeneratorTest` — `hibernate-reactive/investigate-batch-01.md`
* `org.hibernate.reactive.CollectionStatelessSessionListenerTest` — `hibernate-reactive/investigate-batch-02-CLEARED-20260812.md` (note: "CLEARED" in the filename — if that means resolved, this class failing again now would itself be worth a closer look)

**Undocumented:**
* `org.hibernate.reactive.TableGeneratorTest`
* `org.hibernate.reactive.dynamic.DynamicEntityTest` (`test(VertxTestContext)`,
  `CompletionException: AssertionError`, no further detail captured)

## Next steps
* Confirm the `getDeclaredMethods()` divergence's actual mechanism (not just
  its existence) — check whether CratonVM's reflection data is built from a
  simple bytecode method-table walk while HotSpot's involves some
  additional internal reordering (annotation processing order, method-table
  hashing, JVMTI-visible vs not). A fix should replicate the ORDER, not just
  the ordering algorithm's happenstance HotSpot output, since arbitrary code
  (like Jackson) depends on the DE FACTO behavior, not the JLS spec.
* Run the "plausibly the same root cause" classes (both Spring Framework and
  Spring Boot lists above) through the same JSON-property-order check to
  convert "plausible" into "confirmed" or rule them out individually.
* Reconcile `DatabaseHibernateReactiveTest`'s Linux failure against the
  existing Windows-locale doc — get the actual assertion message/locale on
  this Azure host before assuming either "same bug, doc incomplete" or
  "different bug, coincidentally same test."
* Rerun `KafkaAutoConfigurationIntegrationTests` in isolation before
  concluding it's newly broken — it was confirmed passing 3/3 days earlier
  on an older binary.

## Repro
```bash
# getDeclaredMethods() order divergence — no framework needed
source <toolchain env>
javac -cp "<spring-test classpath>" -d probeclasses RealPersonProbe.java
<cratonvm-bin> --java-home <jdk25-home> -c "probeclasses:<spring-test classpath>" RealPersonProbe
# compare against: <jdk25-home>/bin/java -cp "probeclasses:<spring-test classpath>" RealPersonProbe
```
```java
import java.lang.reflect.Method;
import org.springframework.test.web.Person;
public class RealPersonProbe {
    public static void main(String[] args) {
        for (Method m : Person.class.getDeclaredMethods()) System.out.println(m.getName());
    }
}
```

Full per-variant results:
* Spring Framework: `/data/spring-fullsuite3gc-20260824/{default,g1,zgc}-jit-real-all-*/results.tsv`
* Spring Boot: `/data/cratonvm/apps/spring-boot-suite-runner/.suite/results/{default,g1,zgc}-fullsuite-20260825/all-jit/results.tsv`
* Hibernate Reactive: `/data/cratonvm/apps/hibernate-reactive-suite-runner/runs/full-{generational,g1,zgc}-20260825_175236/run-20260825-175236-passed/on-real/results.tsv`
