# `HikariDataSourceConfigurationTests` — 300s TIMEOUT, stalls mid-`HikariPool` startup — root cause not confirmed

**Status: OPEN.** Not the already-FIXED `ModifiedClassPathExtension`
`DiscoveryIssueException` bug that previously affected 4 of this class's 13
methods (confirmed fixed and re-verified 13/13 passing on 2026-07-28) — this
is a different, new hang in an un-annotated method.

## Symptom

2026-08-06 full-suite Windows run (`craton-fullsuite-windows-20260806-s3/all-jit`),
`-Xmx 2g`, 300s/class, default Generational GC:

`org.springframework.boot.jdbc.autoconfigure.HikariDataSourceConfigurationTests`
(`module/spring-boot-jdbc`) — TIMEOUT/HANG, 300.136s.

Log: `craton-fullsuite-windows-20260806-s3/all-jit/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.HikariD-475fa28c68dc.{out,err}.log`

`.out.log` is exactly 2 lines, then nothing for the rest of the run:

```
02:30:04.491 [main] WARN com.zaxxer.hikari.HikariConfig -- HikariPool-1 - using dataSourceClassName and ignoring jdbcUrl.
02:30:04.511 [main] INFO com.zaxxer.hikari.HikariDataSource -- HikariPool-1 - Starting...
```

The process never logs `HikariPool-1 - Added connection` or `Start
completed` (both of which appear routinely elsewhere in this same suite run
for other classes' Hikari-backed contexts, e.g.
`IntegrationAutoConfigurationTests`'s log shows the full sequence in
279ms: `Starting...` → `Added connection conn0: url=jdbc:h2:mem:...` →
`Start completed.`) — so whatever HikariCP is doing between "Starting..."
and its next log line never completes or times out on its own.

`.err.log` shows normal VM boot, then two `[moving-young]` GC fallback
warnings roughly 2 and 3.5 minutes into the run:

```
2026-08-07T02:29:25...  (boot banner, Mockito self-attach)
2026-08-07T02:32:01.715263Z  WARN cratonvm_gc::gc_quiescence: [moving-young] fallback #1: reason=unregistered-jit-frame-on-stack ...
2026-08-07T02:33:32.348513Z  WARN cratonvm_gc::gc_quiescence: [moving-young] fallback #2: reason=unregistered-jit-frame-on-stack ...
```

The presence of GC activity ~2-3.5 minutes into the stall (i.e. well after
the last `.out.log` line at 02:30:04) means the process is not fully wedged
— it is still allocating and running young collections — but whatever loop
or wait it's in never reaches the point of logging `Added connection`,
throwing, or otherwise making test-visible progress. That rules out a
complete deadlock (a genuinely deadlocked thread would stop allocating) in
favor of either a slow-but-live retry loop or a blocking wait that's
periodically interrupted/retried without ever succeeding or giving up.

## Which test this is, and what it does

The test corpus's own log ordering plus the exact WARN text
(`using dataSourceClassName and ignoring jdbcUrl`) identifies this as
`testDataSourceGenericPropertiesOverridden`:

```java
@Test
void testDataSourceGenericPropertiesOverridden() {
    this.contextRunner
        .withPropertyValues(PREFIX + "data-source-properties.dataSourceClassName=org.h2.JDBCDataSource")
        .run((context) -> {
            HikariDataSource ds = context.getBean(HikariDataSource.class);
            assertThat(ds.getDataSourceProperties().getProperty("dataSourceClassName"))
                .isEqualTo("org.h2.JDBCDataSource");
        });
}
```

(`apps/spring-boot/module/spring-boot-jdbc/src/test/java/org/springframework/boot/jdbc/autoconfigure/HikariDataSourceConfigurationTests.java:85-94`.)
This test does **not** itself carry `@ClassPathExclusions`/`@ClassPathOverrides`
— it is one of the class's 9 plain methods, not one of the 4 that route
through `ModifiedClassPathExtension`'s nested-`Launcher` mechanism. So this
is unrelated to that pathway's already-documented failure modes (see the
"Ruled out" section below).

`data-source-properties.dataSourceClassName` sets an arbitrary entry in
`HikariConfig`'s generic `dataSourceProperties` bag (normally meant for
driver-specific tuning knobs), not the actual `hikari.data-source-class-name`
config field — the base `contextRunner`'s only datasource hint is
`spring.datasource.type=HikariDataSource`, so Spring Boot's embedded-database
detection still selects H2 and Hikari still builds a JDBC-URL-based
`DriverDataSource`, meaning `"dataSourceClassName"` ends up as a **connection
property string handed to the JDBC driver alongside the real connection
properties**, not as an actual class to instantiate. Whether CratonVM's H2
driver/JDBC path does something unexpected (block, spin, retry) when handed
an unrecognized `dataSourceClassName` connection property was not confirmed
this session.

## Ruled out

**Not the `ModifiedClassPathExtension` `DiscoveryIssueException` bug.**
`HikariDataSourceConfigurationTests` is explicitly listed in
`modifiedclasspathextension-nested-launcher-uniqueid-discovery-failure-FIXED.md`
(4 of its 13 methods — `configureDataSourceClassNameWithNoEmbeddedDatabaseAvailable`,
and the three `whenCheckpointRestoreIsAvailable*` methods — originally hit
`DiscoveryIssueException: UniqueIdSelector ... could not be resolved`,
FIXED 2026-07-28, re-verified **13/13 passing in both JIT and `--nojit`**
against release binary `cratonvm-modcp-nested-20260728-019fa9e4.exe`). That
bug also fails fast (a `DiscoveryIssueException`, not a hang) and only
affects the 4 annotated methods — the method identified above as the hang
point is not one of them, and the symptom (a genuine multi-minute stall, not
an immediate discovery error) does not match regardless.

**Not the same signature as `DevToolsEmbeddedDataSourceAutoConfigurationTests`'s
hang in this same run** — that one is now root-caused and FIXED (2026-08-09),
see
[`devtoolsembeddeddatasourceautoconfigurationtests-load-time-transform-rescan-FIXED.md`](../../internal/fixed-suite-bugs/springboot/devtoolsembeddeddatasourceautoconfigurationtests-load-time-transform-rescan-FIXED.md).
It was the `java.lang.instrument` load-time transform hook re-offering every
class to Mockito's self-attached `ClassFileTransformer` on every constant-pool
resolution, which only reaches classes loaded through a **user loader** — that
class carries class-level `@ClassPathExclusions`, so all of its work runs under
`ModifiedClassPathClassLoader`. `HikariDataSourceConfigurationTests` carries no
such annotation and runs on the application loader, where the hook's
already-defined early-out fires normally, so the mechanism does not reach it.

Two claims this page inherited from that one's original triage were **wrong**
and should not be reused as discriminators: its "zero GC activity ⇒ a tight,
non-allocating spin" reading (the hung process in fact allocates ~2.4 MB/s and
does zero file I/O — no `[moving-young]` line means no *fallback*, not no
allocation), and its "Windows-specific" framing (the Aug-05 binary passes that
class on the same Windows box in 21s). The contrast that does still hold is
the one about *this* class: 2 lines of real progress and an actual HikariCP
connection-pool-start call, which no part of the fixed mechanism explains.

## Prior timings for this exact class

| Run | Result | Seconds |
|---|---|---:|
| `craton-fullsuite-azure-20260802` | FAIL (1 failure) | 30.690 |
| `craton-fullsuite-azure-20260805-s?` | PASS | 37.562 |
| `craton-fullsuite-windows-20260806-s3` (this doc) | **HANG** | 300.136 |

Like `DevToolsEmbeddedDataSourceAutoConfigurationTests`, this class was
previously fast (30-38s) on Azure Linux, so this reads as a genuine new stall
rather than "the margin ran out" (contrast with the Flyway/Integration
margin doc, where both classes show continuous progress to the very end).

## Not confirmed

- The exact call/thread the process is blocked in — no debugger attach or
  `--stack-sample-ms` run was taken this session (out of scope: no
  multi-minute reruns).
- Whether it is Windows-specific, host-load-specific (16-way parallel on this
  run), or a genuine correctness defect exposed by this particular
  `dataSourceClassName`-as-connection-property test shape.
- Whether the two GC fallback events during the stall are incidental
  background allocation or evidence of the stalled path itself allocating
  (e.g., a retry loop that allocates per attempt).

## Affected classes

- `module/spring-boot-jdbc` — `org.springframework.boot.jdbc.autoconfigure.HikariDataSourceConfigurationTests` (hangs in `testDataSourceGenericPropertiesOverridden`, based on log-order + WARN-text identification; not confirmed via a per-test isolated rerun)
