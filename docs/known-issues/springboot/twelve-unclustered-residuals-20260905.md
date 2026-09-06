# Twelve unclustered Spring Boot residuals from the fixed-harness 3-GC triage

## Status

**OPEN, shallow pass only.** Each row below is one FAIL from the
2026-09-04/05 full 3-GC Spring Boot suite run
(`full_{generational,g1,zgc}_mysession_20260904_041104`), recorded with
whatever signature the run's own log already carried. None of these have
been reproduced in isolation, cross-checked against HotSpot, or traced past
the first exception line. This page exists so the 12 items aren't lost, not
because any one of them is understood yet — treat every "likely" below as a
guess, not a verdict.

Excluded from this page: the `PublicSuffixList` classloader-identity cluster
(4 classes) and the `JarUrlConnectionTests`/`NestedUrlConnectionTests` NPE
pair -- both **FIXED 2026-09-05** and retired to the internal tree as the
`publicsuffixlist-forked-classpath-jit-checkcast-loader-duplication-FIXED-20260905`
and `loader-jar-nested-url-connection-npe-pair-FIXED-20260905` write-ups; and
the 3 `BOTH-FAIL` classes already confirmed not-CratonVM-bugs (fail identically
on HotSpot).

Two of the fixes those retired pages carry are process-wide, so a row below
that was recorded on the 2026-09-04/05 binary may no longer reproduce:
`java.util.zip.ZipFile`/`java.util.jar.JarFile` natives are now immune to the
redefinition any `spy()`/inline `mock()` of a `JarFile` performs, and the
compiled `checkcast` no longer refuses two loader copies of one class name.
**Re-run a row before investigating it.**

## Likely one shared cause: missing test infrastructure (4 classes)

All four fail via a Spring `ApplicationContext` that never started
(`IllegalStateException: Unstarted application context ...
[startupFailure=BeanCreationException] failed to start`), and the one where
the underlying cause is visible names a live dependency this sandboxed host
almost certainly doesn't have:

```
org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests
  testStreams()
  BeanCreationException: Error creating bean 'defaultKafkaStreamsBuilder' ...
  Missing required configuration "bootstrap.servers" which has no default value.

org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationTests
  connectionDetailsWithSslBundleAreAppliedToStreams()
  -- same "Unstarted application context" wrapper, root cause not captured in this pass

org.springframework.boot.kafka.autoconfigure.metrics.KafkaMetricsAutoConfigurationTests
  whenKafkaStreamsIsEnabledAndThereIsNoMeterRegistryThenListenerCustomizationBacksOff()
  -- same wrapper, root cause not captured

org.springframework.boot.hibernate.autoconfigure.HibernateJpaAutoConfigurationTests
  customPersistenceManagedTypes()
  -- same wrapper, root cause not captured
```

**Not confirmed**: only the first shows its actual root cause in this pass's
logs (a missing Kafka broker's `bootstrap.servers`). The other three show
only the generic "context failed to start" wrapper — grouping them here is a
guess based on the identical wrapper text, not a traced shared cause. If this
host genuinely has no Kafka/testcontainers infrastructure, all three Kafka
rows and possibly the Hibernate one are environment gaps, not CratonVM bugs
— needs the actual `BeanCreationException` cause chain to confirm.

## Mockito argument-mismatch pair (2 classes, same module)

```
org.springframework.boot.grpc.client.autoconfigure.GrpcChannelBuilderCustomizersTests
  applyWhenHasServiceConfig()
  org.mockito.exceptions.verification.opentest4j.ArgumentsAreDifferent: Argument(s) are different! Wanted: ...

org.springframework.boot.grpc.client.autoconfigure.GrpcClientAutoConfigurationTests
  clientPropertiesChannelCustomizerAutoConfiguredWithHealthAsExpected()
  org.mockito.exceptions.verification.opentest4j.ArgumentsAreDifferent: Argument(s) are different! Wanted: ...
```

Both in `spring-boot-grpc-client`, both a Mockito `verify()` seeing a
different argument than expected. Could be a real behavioral difference in
what CratonVM's gRPC channel-builder autoconfiguration actually constructs,
or an object-equality/toString gap that makes an equal argument look
different to Mockito's matcher. Full `Wanted: ... Actual: ...` bodies were
not captured in this pass.

## Six singletons, no attempted grouping

```
org.springframework.boot.actuate.logging.LoggersEndpointTests
  groupNameSpecifiedShouldReturnConfiguredLevelAndMembers()
  AssertionFailedError: expected: "[\"test.member\"] (SingletonList@109e4)"

org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorIntegrationTests
  runWhenHasLocalFileLoadsWithLocalFileTakingPrecedenceOverClasspath()
  -- a large parameterized test; many "expected: ..." AssertionFailedErrors
     logged together, property-resolution-precedence values (profiles,
     property files). Needs the actual vs. expected pairs, not just the
     expected side, to say anything.

org.springframework.boot.couchbase.autoconfigure.CouchbaseAutoConfigurationTests
  whenObjectMapperBeanIsDefinedThenClusterEnvironmentObjectMapperIsDerivedFromIt()
  AssertionFailedError: [Extracted: wrapped] expected: "[\"custom\", \"JsonValueModule\"] (Set12@332e4)"

org.springframework.boot.data.jpa.autoconfigure.DataJpaRepositoriesWithEnversRevisionAutoConfigurationTests
  autoConfigurationShouldSucceedWithRevisionRepository()
  AssertionError: Expecting: ... (body not captured in this pass)

org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests
  explicitIntegrationComponentScan()
  AssertionError: Expecting: ... (body not captured; this class also showed
  HANG on Generational and PASS on ZGC in the same 3-arm run -- may be
  flaky/order-dependent rather than a clean deterministic FAIL)

org.springframework.boot.testcontainers.service.connection.ServiceConnectionContextCustomizerTests
  equalsAndHashCode()
  AssertionFailedError: expected: "ServiceConnectionContextCustomizer@67773d6b (ServiceConnectionContextCustomizer@6eed5)"
  -- an equals()/hashCode() contract test; the expected value already looks
     like two different object identities being compared, which may itself
     be the bug (object identity/hashing divergence) rather than incidental
     to it.
```

## Next steps, in priority order

1. **Confirm or rule out the testcontainers/infra hypothesis** for the
   4-class cluster — check whether Docker/testcontainers is available on
   this host at all; if not, those 3-4 rows are environment gaps and should
   move off this page entirely.
2. **Re-run each singleton in isolation** with full stdout captured (not
   just the first exception line) — several of these truncated the
   "actual" side of the assertion in this pass's extraction, which is the
   half that would actually explain anything.
3. **Cross-check against HotSpot** on the same harness before spending
   further CratonVM-side investigation on any one of these — several of the
   3 already-excluded `BOTH-FAIL` classes from this same run looked exactly
   this uninformative before that check showed they weren't CratonVM issues
   at all.

## Repro

```bash
cd apps/spring-boot-suite-runner
CV_BIN=<binary> ./run-spring-boot-suite.sh -Category all -ClassList <(printf '%s\n' \
  org.springframework.boot.actuate.logging.LoggersEndpointTests \
  org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorIntegrationTests \
  org.springframework.boot.couchbase.autoconfigure.CouchbaseAutoConfigurationTests \
  org.springframework.boot.data.jpa.autoconfigure.DataJpaRepositoriesWithEnversRevisionAutoConfigurationTests \
  org.springframework.boot.grpc.client.autoconfigure.GrpcChannelBuilderCustomizersTests \
  org.springframework.boot.grpc.client.autoconfigure.GrpcClientAutoConfigurationTests \
  org.springframework.boot.hibernate.autoconfigure.HibernateJpaAutoConfigurationTests \
  org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests \
  org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests \
  org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationTests \
  org.springframework.boot.kafka.autoconfigure.metrics.KafkaMetricsAutoConfigurationTests \
  org.springframework.boot.testcontainers.service.connection.ServiceConnectionContextCustomizerTests)
```
