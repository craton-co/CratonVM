# Spring Boot non-passing classes confirmed NOT CratonVM bugs — consolidated reference

**Purpose: stop future sessions re-investigating these.** Every entry below
was directly cross-checked against stock HotSpot 25 on the same classpath,
same harness (`SbRunner`) — and HotSpot fails, or is subject to the exact
same host constraint, identically. None of these belong in a "CratonVM
regression" count. Compiled 2026-08-20; the Jetty mTLS row added 2026-09-01, and
it is the one row whose HotSpot arm does not merely match — see the section
below it.

Spring Boot's non-passing-class history is overwhelmingly a **Windows** suite
(the primary suite host for this project). This index says explicitly, per
row, which platform the HotSpot cross-check was actually run on — several of
the historical "not a CratonVM bug" findings for this project are Windows
host-privilege gaps that simply do not reproduce at all on the Azure Linux
host (`20.80.105.49`), so there is little to report for a pure-Azure column
where the underlying condition doesn't exist on Linux in the first place.

| class(es) | why it's not a CratonVM bug | evidence | doc |
|---|---|---|---|
| `org.springframework.boot.autoconfigure.ssl.FileWatcherTests` (5 of 15 methods: `shouldFollowSymlink`, `shouldFollowSymlinkRecursively`, `shouldFollowRelativePathSymlinks`, `shouldTriggerOnConfigMapUpdates`, `shouldTriggerOnConfigMapAtomicMoveUpdates`) | **Windows only.** The host lacks `SeCreateSymbolicLinkPrivilege`/Developer Mode, so `Files.createSymbolicLink` fails identically on both VMs with `FileSystemException`. Reconfirmed on Azure Linux (`20.80.105.49`, 2026-08-11): Linux needs no elevated privilege, and the class (plus siblings `ConfigTreePropertySourceTests`, `ApplicationTempTests`) passes cleanly on both VMs there — the gap is entirely absent on Azure, not just "not a CratonVM bug" but literally not reachable there. | Windows: HotSpot 25 `FileSystemException`, same 5 method names, `10 tests successful / 5 tests failed`, byte-identical to CratonVM, reconfirmed across Generational/G1/ZGC on 2026-08-08/10. Linux (`dev@9f9c03f20`): both VMs pass 15/15. | `filewatchertests-windows-symlink-privilege-gap.md` |
| `org.springframework.boot.micrometer.metrics.autoconfigure.export.datadog.DatadogPropertiesConfigAdapterTests` (`adapterOverridesAllConfigMethods`) | `DatadogPropertiesConfigAdapter`'s own source never overrides `DatadogConfig.compress()` — the interface has it, the adapter doesn't implement it, on this checkout regardless of VM. A genuine gap in Spring Boot's own source, not a CratonVM reflection issue. | Azure Linux, 2026-08-27: `AssertJ: could not find the following elements: ["compress"]`, byte-identical on stock HotSpot 25 and CratonVM, same classpath. | this row |
| `org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.OtlpMetricsExportAutoConfigurationTests` | `IllegalArgumentException: Cannot locate field metricsSender on class io.micrometer.registry.otlp.OtlpMeterRegistry` — AssertJ's field introspection can't find a field the test expects, consistent with a dependency-version mismatch between the test source and the actual `micrometer-registry-otlp` jar this ad-hoc classpath-dump harness resolves (bypasses Gradle's own managed dependency resolution). | Azure Linux, 2026-08-27: identical `IllegalArgumentException`, same field/class name, on both stock HotSpot 25 and CratonVM. | this row |
| `org.springframework.boot.micrometer.tracing.brave.autoconfigure.OtlpExemplarsAutoConfigurationTests` (`otlpOutputShouldContainExemplars`, `otlpOutputShouldContainExemplarsWhenIncludeIsAllAndSpanIsNotSampled`) | The exported OTLP payload contains a duplicate `name: "test.observation"` entry — a content/protocol-level issue unrelated to the other two rows above despite living in the same module family. | Azure Linux, 2026-08-27: byte-identical `AssertionError` ("to appear only once" / appears twice) on both stock HotSpot 25 and CratonVM. | this row |
| `org.springframework.boot.jetty.reactive.JettyReactiveWebServerFactoryTests` (`sslNeedsClientAuthenticationFailsWithoutClientCertificate`) | **Host load, and HotSpot fails it MORE.** The test asserts with `verify(Duration.ofSeconds(10))` — a WALL-CLOCK budget — on two JVM-internal event loops handshaking over loopback. On an oversubscribed host nothing meets that budget, and the failure is a clean `AssertionError`, so the harness never sees a timeout and the row reads as a VM defect. A packet capture of a failing connection shows the server's `FIN` carrying `seq 1, ack 1` after 14.3 s: **neither side ever wrote a byte** — no `ClientHello`, no `ServerHello`. The TCP connection was completed by the KERNEL's accept queue and then sat unserviced, so there was no handshake to reject and no close path to blame. A passing connection reads `seq 1932, ack 482` and FINs 766 ms after the SYN. | Azure Linux, 2026-09-01. Synchronised bursts of 12 JVMs, both VMs in every burst, load 36-124, 180 runs each: **CratonVM 175 pass / 5 fail (2.8 %), stock HotSpot 25 171 pass / 9 fail (5.0 %)** — all 14 the same `VerifySubscriber timed out` assertion. Steady load does NOT reproduce it: 1161 CratonVM runs at load 20-35 and 38 runs pinned to two cores (up to 36 s wall each) were all green. | `jettyreactive-mtls-verify-timeout-is-load-not-a-close-path-defect-RETIRED-20260901.md` (internal) |
| `org.springframework.boot.loader.zip.ZipContentTests` | Not actually a failure: 28/29 tests pass, the sole non-pass is `TestAbortedException: Assumption failed: Insufficient disk space` on `openWhenZip64ThatExceedsZipSizeLimitOpensZip` (needs several GB of scratch space to build a Zip64 archive past the standard size limit) — a host resource constraint, and the harness's status classifier counts any ABORTED-containing run as `FAIL` even with zero actual test failures. | Azure Linux, re-confirmed 2026-08-28 on the suite runner: `tests=29 failed=0 aborted=1`. | `../../internal/fixed-suite-bugs/springboot/uri-getrawpath-percent-encoding-inconsistency-FIXED-20260827.md` |

## One row where HotSpot does not fail *identically* — it fails MORE

Every other entry on this page rests on "stock HotSpot fails the same way on
the same classpath". The Jetty mTLS row does not, and the difference is worth
stating rather than smoothing over: HotSpot fails it **1.8x more often** than
CratonVM under the same load.

That makes the row stronger, not weaker, but it also means the usual
cross-check is not what settles it. Three things were needed, and a future
session looking at a load-gated suite failure should reach for the same three:

1. **Pair the arms in the SAME burst, not in separate tables.** The page this
   row replaces had 219 CratonVM runs and no HotSpot column at all, and said
   so. A VM-only loop cannot separate "this VM is wrong" from "this budget does
   not survive this host".
2. **Reproduce with SIMULTANEITY, not with steady load.** All the quiet arms
   were green, including 38 runs pinned to two cores where the whole test took
   36 seconds of wall clock and still passed — the 10-second budget is on the
   `StepVerifier`, not on the process. What tripped it was a burst of cold JVM
   starts, which is the shape of the original (found 1991 classes deep in a
   full suite).
3. **Read the packet, not the log.** A `FIN` carrying `seq 1, ack 1` says
   nobody serviced the socket; `seq 1932, ack 482` says the rejection worked.
   No amount of Reactor Netty stack trace distinguishes those two, and two
   successive triages spent four days on the wrong subsystem because of it.

**Recommendation for the Spring Boot runner, not yet landed:** record
`/proc/loadavg` beside every suite row, and re-run any class that produces a
`VerifySubscriber timed out` at load >> core count before attributing it. The
suite run that produced the original also printed `hotspot baseline: none --
every failure will be attributed to CratonVM`, which is exactly the condition
under which a load-gated failure becomes a VM bug on paper.

## Checked and found to have already resolved itself (not filed as not-a-bug — nothing left to explain)

`org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests`
was flagged 2026-08-12 as `EMPTY` under CratonVM (`3 tests found, 3 skipped, 0
started` — read at the time as an environment-gated Testcontainers/Docker
conditional disable, not a hang or defect). Rechecked 2026-08-20 on Azure
Linux (`20.80.105.49`) against both a fresh HotSpot run and a fresh `dev`-tip
CratonVM build: **both now run and pass all 3 tests via Kafka's embedded
KRaft test cluster** (`kafka-cluster-test-kit`, no Docker/Testcontainers
involved at all — `containersFailed=0`, `3 tests successful`, `0 skipped`,
identical on both VMs). Whatever produced the 2026-08-12 skip no longer
applies on either the current classpath or current `dev`; there is no
divergence left to document.

## Checked and found to be a genuine, still-open CratonVM-side gap — NOT included above

`EmbeddedLdapAutoConfigurationTests.whenSslBundleIsConfiguredLdapsListenerIsConfigured`
(`springboot-ldap-dsa-tls-windows-only-gap.md`) was considered for this list
and excluded: HotSpot **does** negotiate the required `TLS_DHE_DSS_*` cipher
suite for a legacy-DSA certificate on the same Windows host
(`javax.net.debug=ssl:handshake` confirms it); rustls implements no DHE key
exchange at all and cannot reach that suite by any configuration. HotSpot
passes where CratonVM fails — the opposite of "not a CratonVM bug." It stays
filed as an accepted, still-OPEN platform limitation in
`docs/known-issues/springboot/springboot-ldap-dsa-tls-windows-only-gap.md`.

## What this list is not

It is not exhaustive over Spring Boot's full non-passing history — only
entries actually reverified as of 2026-08-20 appear here. Spring Boot's own
3-GC Azure sweep from 2026-08-19
(`apps/spring-boot-suite-runner/RESULTS-20260819-3gc-azure-CORRECTION.md`,
source data no longer present on disk) found most of an apparent FAIL-count
increase was two harness confounds (a working-directory bug and a missing
`--add-opens`), not CratonVM — that accounting is cited here for context but
not independently reproduced. The two genuine CratonVM-specific defects that
sweep surfaced (Jetty JSP class-loading, and a Groovy-closure
`MethodHandle.asSpreader` adaptation failure) are tracked separately in
`jetty-jsp-classload-and-groovy-methodhandle-20260819.md` — the second of
those two is now believed fixed by `dev` commit `d766af065` (2026-08-19,
the same fix that resolved the identical `MethodHandle.asSpreader` cluster
found independently in Spring Framework's own 2026-08-19 sweep — see
`bug-spring-methodhandle-asspreader-groovy-invocation-cluster-20260819-FIXED-20260820.md`),
not yet reverified against Spring Boot's own classes specifically.

## Related

- `docs/known-issues/spring/` — the Spring Framework sibling project's own
  `not-cratonvm-bugs-consolidated.md`, compiled the same day from a fuller
  Azure Linux full-suite sweep.
