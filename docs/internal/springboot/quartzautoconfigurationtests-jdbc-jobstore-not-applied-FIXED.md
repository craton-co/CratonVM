# `QuartzAutoConfigurationTests`: `spring.quartz.job-store-type=jdbc` is set but the scheduler still uses `RAMJobStore`

**Status: FIXED 2026-07-20 (archived — see
`docs/internal/springboot/quartzautoconfigurationtests-jdbc-jobstore-not-applied-FIXED.md`)**

## Update 2026-07-20 — root-caused and fixed, plus one residual it was masking

Branch `fix/quartz-jdbc-jobstore-20260720`, worktree
`C:\craton\CratonVM-quartz-jdbc-jobstore-20260720`. Two independent, unrelated
native-shim gaps combined to produce the symptom; both are now fixed.

### Root cause 1 (the doc's original bug): `java.util.Properties.putIfAbsent` had no native override

`SchedulerFactoryBean.initSchedulerFactory(Properties)` (Spring Framework,
`spring-context-support`) does:

```java
if (this.dataSource != null) {
    props.putIfAbsent(PROP_JOB_STORE_CLASS, LocalDataSourceJobStore.class.getName());
}
```

confirmed by disassembling `SchedulerFactoryBean.class` — `invokevirtual
java/util/Properties.putIfAbsent`. CratonVM's `Properties` side-table
(`native-builtins/src/properties_sidetable.rs`) registers natives for `put`,
`setProperty`, `get`, `getProperty`, `remove`, `clear`, `putAll`, etc., all of
which read/write a Rust-side `HashMap` keyed by GC-stable object identity
(`getProperty`/`get` consult **only** that side-table, never the real JDK
`Hashtable`/`ConcurrentHashMap` backing fields, per the module's documented
"synthetic Properties has a broken inner Hashtable layout" rationale) — but
`putIfAbsent` was never registered. The call fell through to real (inherited)
`Hashtable.putIfAbsent` bytecode, which wrote into the real backing fields
directly, invisible to the side-table. The next read —
`PropertiesParser.getStringProperty("org.quartz.jobStore.class",
RAMJobStore.class.getName())` inside Quartz's own
`StdSchedulerFactory.instantiate(...)` (this is our side-table-only
`getProperty(String,String)` native) — missed the write and silently fell
back to Quartz's own default, wiring up `RAMJobStore` instead of
`LocalDataSourceJobStore` even though the JDBC customizer bean ran
successfully and the schema was correctly initialized.

Fixed by adding `native_properties_put_if_absent` (`Map.putIfAbsent`
semantics: existing-value check via the same side-table→CHM→system-property
read path `native_properties_get` already uses, otherwise delegate to
`native_properties_put`'s side-table+CHM insert) and registering it for
`java/util/Properties.putIfAbsent(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;`.

Verified: `QuartzAutoConfigurationTests` went from 5/24 FAIL to 1/24 FAIL
(the residual below) with this fix alone.

### Root cause 2 (residual, previously masked): `sun.nio.cs.StreamDecoder`'s native shim only handled the `InputStream`-backed factory, not the `ReadableByteChannel`-backed one

With root cause 1 fixed, `withFlyway` still failed — `Table
"QRTZ_JOB_DETAILS" not found` even though Flyway logged "Successfully
applied 1 migration". Isolated with a standalone repro (no Spring/Quartz):
`Flyway.configure().locations("filesystem:...").migrate()` against a
correctly-populated `V2__quartz.sql` reported success but created **zero**
tables under CratonVM (byte-identical on HotSpot: 12 tables). Root cause:
`FileSystemResource.read()` (Flyway) builds its `Reader` via
`Channels.newReader(FileChannel, CharsetDecoder, int)`, which constructs a
`sun.nio.cs.StreamDecoder` through the real (never natively intercepted)
`StreamDecoder.forDecoder(ReadableByteChannel, CharsetDecoder, int)` factory
— setting the real `ch` field, not `in` (no `InputStream` exists in this
variant at all).

CratonVM's `StreamDecoder` shim (`native-io/src/stream_decoder.rs`) natively
intercepts `read`/`read([CII)I`/`close`/`ready` on **every** `StreamDecoder`
instance regardless of which factory built it (native registration is
per-class, not per-construction-path), but its refill logic only knew how to
pull bytes from the `in` (`InputStream`) field. For a channel-built decoder
`in` is null, so the existing code treated that as immediate EOF —
`readLine()` returned `null` on the very first call, so
`BufferedReader`-driven consumers (Flyway's SQL-script parser) saw an empty
file and Flyway "successfully" ran a zero-statement migration.

Fixed by adding a second refill path in `decode_into`: when `in` is absent
but `ch` (a `ReadableByteChannel`) is present, allocate a heap `ByteBuffer`
(reusing `alloc_byte_buffer`), call `ch.read(ByteBuffer)`, and pull the bytes
back out of the buffer's backing array — mirroring the existing
`InputStream`-based branch. Also: read the real charset off the decoder's own
`cs` field when there's no side-table entry (previously such objects
defaulted to a hardcoded `"UTF-8"`, which only happened to be correct here by
coincidence), and close the channel (not just `in`) in `native_sd_close` so a
`FileChannel` opened this way isn't leaked.

Verified with two isolated repros (`Channels.newReader` char-for-char
byte-identical to HotSpot; a standalone Flyway+H2+HikariCP repro creating all
12 `QRTZ_*`/`flyway_schema_history` tables, matching HotSpot) and the full
`QuartzAutoConfigurationTests` class: 24/24 PASS. Regression-checked the
whole `spring-boot-quartz`/`spring-boot-flyway`/`spring-boot-liquibase`
modules (21 classes) against the pre-fix baseline binary — the 3
FAIL/1 HANG found in that batch (`Flyway110AutoConfigurationTests`,
`Liquibase423AutoConfigurationTests`, `QuartzEndpointWebIntegrationTests`,
`FlywayAutoConfigurationTests`) reproduce identically (or worse — one
HANGs on baseline but completes with a real result under the fix) on the
unfixed baseline binary, and their failure content (a `@ConfigurationProperties`
annotation-metadata error, Flyway OSS license-gate rejections, a missing
`dataSource` property) is unrelated to `Properties`/`StreamDecoder` — no
regressions.

`cargo test -p cratonvm-native-io stream_decoder` (16/16) and
`cargo test -p cratonvm-native-builtins properties` (30/30) both pass
unchanged.

## Original report (2026-07-17)

## Symptom

| Class | tests failed/total |
|---|---:|
| `QuartzAutoConfigurationTests` | 5/24 |

All 5 failures (`withLiquibase`, `withDataSource`,
`dataSourceWithQuartzDataSourceQualifierUsedWhenMultiplePresent`,
`withDataSourceNoTransactionManager`, `withFlyway`) go through the same
shared assertion helper and fail identically:

```
=> java.lang.AssertionError:
Expecting
  org.quartz.simpl.RAMJobStore
to be assignable from:
  [org.springframework.scheduling.quartz.LocalDataSourceJobStore]
but was not assignable from:
  [org.springframework.scheduling.quartz.LocalDataSourceJobStore]
       org.springframework.boot.quartz.autoconfigure.QuartzAutoConfigurationTests.lambda$assertDataSourceInitialized$0(QuartzAutoConfigurationTests.java:398)
       org.springframework.boot.quartz.autoconfigure.QuartzAutoConfigurationTests.withDataSource(QuartzAutoConfigurationTests.java:131)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-quartz.org.springframework.boot.quartz.autoconfigure.QuartzAutoConfigurationTests.out.log`

The asserting code (`QuartzAutoConfigurationTests.java:398`) is:
```java
assertThat(scheduler.getMetaData().getJobStoreClass()).isAssignableFrom(LocalDataSourceJobStore.class);
```
AssertJ's `isAssignableFrom` message prints the **actual** value first —
so the log is saying `scheduler.getMetaData().getJobStoreClass()` really
returned `org.quartz.simpl.RAMJobStore`, not
`LocalDataSourceJobStore`(or any of its ancestors). `RAMJobStore` and
`LocalDataSourceJobStore` are unrelated classes in Quartz's `JobStore`
hierarchy (`LocalDataSourceJobStore` extends `JobStoreCMT` extends
`JobStoreSupport`; `RAMJobStore` is a separate direct `JobStore`
implementation), so this is not a "same name, different `ClassId`"
identity bug — the scheduler genuinely, functionally ended up configured
with the in-memory job store instead of the JDBC-backed one, even though
every one of the 5 failing tests explicitly sets
`.withPropertyValues("spring.quartz.job-store-type=jdbc")` (confirmed by
reading `QuartzAutoConfigurationTests.java:130` in this worktree).

## Root cause (unconfirmed hypothesis, 2026-07-17)

`QuartzAutoConfiguration.JdbcStoreTypeConfiguration`
(`module/spring-boot-quartz/src/main/java/.../QuartzAutoConfiguration.java`)
is gated by:
```java
@ConditionalOnSingleCandidate(DataSource.class)
@ConditionalOnProperty(name = "spring.quartz.job-store-type", havingValue = "jdbc")
```
Its `dataSourceCustomizer` bean is the only thing that calls
`schedulerFactoryBean.setDataSource(...)`; without it, Quartz's
`SchedulerFactoryBean` falls back to its own default (`RAMJobStore`) —
exactly the observed symptom. **This hypothesis turned out to be wrong** —
see the 2026-07-20 update above for the confirmed root cause
(`Properties.putIfAbsent`, not a condition-evaluation gap).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-quartz` | `org.springframework.boot.quartz.autoconfigure.QuartzAutoConfigurationTests` |
