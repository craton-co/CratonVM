# Keycloak 26.6.3 (Quarkus) boot under CratonVM — progress & gap chain

> **ARCHIVED 2026-07-01:** This is a historical Keycloak boot gap chain. The
> concrete non-GC/JIT blockers captured here have been fixed or split into
> focused internal notes. The last named blocker, real-JDK
> `CompletableFuture.complete(...)` not running `postComplete()`, is fixed on
> `dev` and tracked in
> [gc-gen-promotion-completablefuture-completion-loss.md](gc-gen-promotion-completablefuture-completion-loss.md).
> The remaining G1/two-worker hang noted near the end is a separate deep runtime
> follow-up and should get its own `docs/known-issues` entry if reproduced.

**Status:** 🟡 PARTIAL (updated 2026-06-20, branch `fix/keycloak-gap5-shutdownctx`) — **Gaps 1–5 FIXED; Gap 4 re-fixed; Gap 6 is the new frontier.** Gap 1 `JarEntry` getSize/getMethod=0, Gap 2 NIO missing-file `NoSuchFileException`, Gap 3 stale `sanitizeDisabledMappers` stub removed. **Gap 4 (static-synchronized → `Class` mirror, JVMS §2.11.10) had REGRESSED on dev for the real boot** — `0e81bc70` missed THREE interpreter invoke fast-paths (cached / stackless-cached / vcached) that still locked the synthetic `get_class_lock_object`, so `ApplicationStateNotification.notifyStartupFailed` (static-sync `notifyAll`) threw `IllegalMonitorStateException` and MASKED every real `<clinit>` failure; now fixed at all three sites (commit `8a3c6c07`). **Gap 5 (Quarkus recorder `ShutdownContext` null NPE) FIXED** — the `StartupContext` native shim shadowed the real `<init>` that registers the `StartupContext$1` `ShutdownContext` proxy under `getValue("io.quarkus.runtime.ShutdownContext")`; shim removed, real bytecode runs (commit `5a489187`). **Gaps 6 & 7 (@ConfigMapping) FIXED** (commits `bec91280`, `4ce68fd9`): `getConfigMapping` now builds the real `<iface>$$CMImpl` from live config via SmallRye `configMappingObject` **with default values applied** (mirrors `mapConfiguration`: `withMapping` + merge `getDefaultValues`) — no fabrication. **This unblocked the whole config cluster, ArC bean creation, and Keycloak's real startup.** The boot now runs through `Arc.initialize()` + CDI bean creation, Keycloak provider init, and Hibernate ORM/JPA, reaching the server-running lifecycle (`ApplicationLifecycleManager.waitForExit`). **Seven gaps fixed; the real ArC CDI container runs.**

> **UPDATE (2026-06-20, branch `fix/keycloak-gap8-datasource`, synced with `dev`):** **Gap 8 (Agroal
> datasource) is RESOLVED** behind the `CRATONVM_REAL_AGROAL` gate — it was NOT an H2-engine gap
> (the real `org.h2.Driver` works) but a shim-vs-real-bytecode collision: the `agroal_pool.rs` shim's
> `AgroalDataSourceConfigurationSupplier.get()` returned a bare-interface config → real
> `DataSourceProvider`/`io.agroal.pool.DataSource` bytecode hit `AbstractMethodError` on
> `dataSourceImplementation()`. With the gate the real Agroal pool runs over the real H2 driver and
> (under JIT) the boot advances **past the entire DB layer** into RESTEasy Reactive deployment.
> **Gap 9 — SEE THE 2026-06-20 RE-CHARACTERIZATION BLOCK below (before "### Quarkus ArC").** The
> earlier framing here — "a nondeterministic wedge of the Quarkus *JPA Startup Thread* in Rust native
> code, root-caused to the multi-thread-in-JIT-under-STW GC root-scanning gap" — is **REFUTED by
> direct evidence** (full-symbol cdb + watchdog Java-frame dump): there is NO wedged JPA thread (the
> only sleeping daemon is the benign Cleaner doing `ReferenceQueue.remove`); `main` reaches
> `waitForExit` (Quarkus thinks startup finished) but the **Vert.x/Netty HTTP server never starts**
> (no event-loop threads) and HTTP never binds; the `cross_thread_jit_gap` warning fires only
> intermittently (an unreliable red herring). Gap 9 is really **(throughput) + (a silent,
> nondeterministic failure where the HTTP server doesn't come up)**, with the fatal cause INVISIBLE
> (the #1 blocker). Plus raw throughput (~15× HotSpot). See the re-characterization for the localized
> Vert.x-startup gap and the prioritized next steps.

Goal: reach and validate the **real Quarkus ArC** CDI path (`CRATONVM_REAL_ARC`,
the `quarkus_arc.rs` shim's replacement). ArC's `Arc.initialize()` runs **late**
in the Quarkus boot, so it is gated behind a chain of earlier boot gaps. This
doc tracks that chain. Each gap fixed advances the real boot one step closer to
ArC.

## Repro (this session)

Real, augmented Keycloak server (generated ArC beans present —
`lib/quarkus/generated-bytecode.jar` has 177 `*_Bean`/`*_ClientProxy` classes):

- Dist: `C:\craton\keycloak-26.6.3` (downloaded from the GitHub release; the
  `apps/keycloak` source tree is `999.0.0-SNAPSHOT` and is **not** augmented, so
  it cannot boot — the released dist is the only bootable artifact here).
- Boot (mirrors `apps/probe/test-infra/run-keycloak.sh`, paths fixed for this
  machine):
  ```
  cd C:/craton/keycloak-26.6.3
  CRATONVM_DISABLE_JIT=1 cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --Xmx 2g \
    -Dkc.config.built=true -Dkc.home.dir=C:/craton/keycloak-26.6.3 \
    -Djboss.server.config.dir=C:/craton/keycloak-26.6.3/conf \
    -Djava.util.concurrent.ForkJoinPool.common.threadFactory=io.quarkus.bootstrap.forkjoin.QuarkusForkJoinWorkerThreadFactory \
    --jar lib/quarkus-run.jar start-dev
  ```
- HotSpot baseline: the same dist boots fully — *"Keycloak 26.6.3 on JVM
  (powered by Quarkus 3.33.2) started in ~22s. Listening on http://localhost:8080.
  Profile dev activated."* (the comparison oracle).

## Gap chain (boot order)

### Gap 1 — Quarkus `RunnerClassLoader` reads 0-byte class data ✅ FIXED
**Symptom:** boot dies at the very first app class —
`ClassFormatError: class file too short (0 bytes)` for
`org/keycloak/quarkus/runtime/KeycloakMain`, then `QuarkusEntryPoint` NPEs on a
null `Class`. Reached *before* ArC.

**Root cause (general bug, not Keycloak-specific):** `JarEntry.getSize()` /
`getMethod()` returned **0** for entries whose bytes are actually readable
(probe: `getSize()=0, getMethod()=0`, but `readAllBytes()=9582` correct). Quarkus
`RunnerClassLoader` sizes its class-byte read buffer from `entry.getSize()` → 0 →
defines the class from an empty array. Cause: the `phases_late.rs` "p59"
synthetic-`JarFile` `getEntry`/`getJarEntry` path wrote size/csize/method into
**synthetic slot indices**, but real `java.util.zip.ZipEntry.getSize()/getMethod()`
bytecode reads the **real** fields by their actual offset (never set → 0). The
correct `native-io/zip_real_jar.rs` path sets fields *by name*; p59 didn't.

**Fix:** `native-builtins/src/phases_late.rs` — `p59_jar_lookup_entry` and the
inline `getEntry` now also write `size`/`csize`/`method`/`crc`/`name` **by name**
(`set_field_by_name`), mirroring `zip_real_jar::alloc_zip_entry`. Verified: probe
now reports `getSize()=9582, getMethod()=8`; boot advances from "first class" all
the way through the Quarkus bootstrap classloader + runtime init into SmallRye
Config.

### Gap 2 — NIO `Files.newInputStream` throws wrong exception for a missing file ✅ FIXED
**Symptom:** `ERROR SRCFG00035: Failed to load resource .../conf/keycloak-dev.conf
(os error 2)` aborts `start-dev`. HotSpot boots fine — `keycloak-dev.conf` does
**not** exist and is an *optional* profile config source.

**Root cause (general bug):** for a missing file, CratonVM's NIO
`Files.newInputStream` / `FileSystemProvider.newByteChannel` threw a generic
`java.io.IOException` ("os error 2"), but HotSpot throws
`java.nio.file.NoSuchFileException`. SmallRye Config treats a config source as
optional by catching `NoSuchFileException`; the generic `IOException` escapes the
catch → fatal. (Probe confirmed: HotSpot NIO → `NoSuchFileException`; CratonVM NIO
→ `IOException`. `FileInputStream` already threw `FileNotFoundException` correctly
on both.)

**Fix:** `native-builtins/src/phases_late.rs` — the three NIO handlers
(`FileSystemProvider.newByteChannel`, `FileSystemProvider.newInputStream`,
`Files.newInputStream`) now map `std::io::ErrorKind::NotFound` →
`p57_no_such_file(ctx, &p)` (the existing typed `java.nio.file.NoSuchFileException`
builder) instead of the generic `p57_io_error`. (Boot-verification pending an
isolated rebuild — the shared `target/` is under concurrent-session contention.)

### Gap 3 — `Profile.CURRENT` null at config validation ✅ FIXED (stale synthetic stub removed)
**Root cause (found):** CratonVM had a `native-builtins/src/lib.rs` "Round 82"
native that **stubbed `PropertyMappers$MappersConfig.sanitizeDisabledMappers` to a
no-op** — added to dodge a `PropertyException("Duplicated mapper for key
'kc.file'")`. But `sanitizeDisabledMappers` is *also* where Keycloak configures
the feature `Profile`: it runs `DisabledMappersInterceptor.runWithDisabled(Runnable)`
→ `lambda$sanitizeDisabledMappers$3` → `Environment.getCurrentOrCreateFeatureProfile()`
→ `Profile.configure(...)` → sets `Profile.CURRENT`. No-op'ing the method left
`CURRENT` null. (Confirmed: the boot's `CRATONVM_DBG_ATHROW` trace showed
`Profile.configure` was *never* called — no exception, just skipped.)
**Fix:** removed the no-op stub so the real `sanitizeDisabledMappers` bytecode
runs. The duplicate-mapper `PropertyException` it guarded against **did not recur**
(the underlying map bug was already fixed elsewhere — the stub was stale). After
the fix the boot configures Profile, passes CLI config validation, prints
*"Running the server in development mode"*, and proceeds into the Quarkus
application startup (`Quarkus.run` → `ApplicationImpl.<clinit>`) — i.e. into the
lifecycle where ArC runs. **Lesson:** this is exactly the forbidden-synthetic-stub
pattern — a no-op shim added for one symptom silently skipped an unrelated,
load-bearing side effect.

### Gap 4 — `static synchronized` used a synthetic lock, not the `Class` mirror ✅ FIXED (re-fixed 2026-06-20)
> **UPDATE (2026-06-20): Gap 4 had REGRESSED on dev for the real boot.** `0e81bc70`
> fixed some monitor sites but THREE interpreter invoke fast-paths still acquired
> the synthetic `get_class_lock_object` for static-synchronized methods: the cached
> direct-call path, the stackless-cached path, and the vcached path
> (`vm/src/runtime/interpreter.rs` ~14717 / ~15754 / ~20023). So the IMSE below
> reproduced on a clean dev boot and MASKED the real Gap-5 cause. Fixed at all
> three sites — they now use `get_or_create_class_mirror(shared, declaring_id)`
> (commit `8a3c6c07`). Lesson: the same fix must cover EVERY invoke fast-path;
> grep `get_class_lock_object` to be sure none remain. The original write-up:
**Symptom:** with gap 3 fixed, the boot reaches the real Quarkus application
startup (`Quarkus.run → ApplicationImpl.<clinit>`) and dies:
```
ExceptionInInitializerError in io/quarkus/runner/ApplicationImpl.<clinit>
  cause = java.lang.IllegalMonitorStateException:
          thread Thread-0 called notifyAll() without owning the monitor
  at io.quarkus.dev.appstate.ApplicationStateNotification.notifyStartupFailed
```
**Root cause (general VM bug):** `ApplicationStateNotification.notifyStartupFailed`
is `static synchronized` and does `ApplicationStateNotification.class.notifyAll()`.
Per JVMS §2.11.10 a `static synchronized` method's monitor is the class's `Class`
object — but CratonVM acquired a **synthetic per-class lock object**
(`get_class_lock_object`) instead. So the thread held a *different* monitor than the
`Class` mirror that `notifyAll()` operates on → `IllegalMonitorStateException`.
Reproduced minimally: a `static synchronized` method doing `X.class.notifyAll()`
throws on CratonVM but not HotSpot; instance `synchronized` was unaffected.
**Fix:** all four monitor-acquisition sites (`vm/src/runtime/interpreter.rs` ×3,
`vm/src/vm/vm_exec.rs` ×1) now use `get_or_create_class_mirror(shared, class_id)`
— the same `Class` object that `synchronized (X.class)` blocks and
`X.class.wait()/notify()` use. Verified: a 3-part probe (static-sync
`Class.notifyAll`; cross-thread static-sync wait/notify — the exact
`ApplicationStateNotification` pattern; static-sync mutual exclusion) passes on
CratonVM == HotSpot. After the fix the boot runs the **whole** Quarkus application
startup (recorders, ArC via its shim, Hibernate Validator) and surfaces gap 5.

### Gap 5 — Quarkus recorder `ShutdownContext` null NPE ✅ FIXED (2026-06-20)
> **FIXED (commit `5a489187`).** Root cause confirmed by disassembly:
> `HibernateValidatorProcessor$shutdownConfigValidator…​.deploy_0` obtains the
> `ShutdownContext` via `startupContext.getValue("io.quarkus.runtime.ShutdownContext")`.
> The **real** `io.quarkus.runtime.StartupContext.<init>` registers a
> `StartupContext$1` (a `ShutdownContext` proxy whose `addShutdownTask` pushes to
> the StartupContext's real `shutdownTasks` deque) into its `values` map under
> `ShutdownContext.class.getName()`. CratonVM's `quarkus_staticinit.rs` native shim
> SHADOWED `StartupContext.<init>` (+ getValue/putValue/addShutdownTask/close) with
> salt-keyed side-tables, so the proxy was never registered → `getValue(…)` → null →
> NPE (masked by Gap 4 until the monitor re-fix above). (NB: `StartupContext`
> implements `Closeable`, NOT `ShutdownContext`; the proxy is a separate object —
> the earlier note here was wrong.) Fix = remove the StartupContext shim entirely
> and run the real bytecode (trivial HashMap + 2 ConcurrentLinkedDeques + the two
> inner-class proxies); `synthetic_stub_fields` only padded field counts (real
> class is larger, so its real layout was already used). Verified on the real boot:
> execution advances one full deploy step (clinit bci 405 → 425) and the NPE is
> gone. **The 5b logging gap is moot for the immediate frontier** — with Gap 4
> re-fixed, `CLINIT-CAUSE` surfaces the real cause directly (it showed Gap 6 below).
> The original write-up (for reference):
**Symptom:** the monitor fix unmasked the real failure. The boot now runs
`ApplicationImpl.<clinit>` to bci=814 (deep into the recorder chain — past ArC) and
fails:
```
ExceptionInInitializerError in io/quarkus/runner/ApplicationImpl.<clinit>
  cause = java.lang.RuntimeException: Failed to start quarkus
    cause = java.lang.NullPointerException
            at io.quarkus.hibernate.validator.runtime.HibernateValidatorRecorder.shutdownConfigValidator(HibernateValidatorRecorder.java:74)
            at io.quarkus.runner.ApplicationImpl.<clinit> (bci=405)
```
**Refined root cause (5a — the primary failure).** `shutdownConfigValidator` does
NOT "close a validator factory" — decompiled, its whole body is
`shutdownContext.addShutdownTask(new HibernateValidatorRecorder$11(this))` (it just
*registers* a shutdown task). The NPE at bci=9 is on the `invokeinterface
ShutdownContext.addShutdownTask` — i.e. the **`ShutdownContext` argument is null**.
In Quarkus, `io.quarkus.runtime.StartupContext implements ShutdownContext`, and
`ShutdownContext` is **not referenced anywhere in CratonVM's natives**. The
`native-builtins/src/quarkus_staticinit.rs` shim (which models `StartupContext` /
`RuntimeValue` / `getValue` / `addShutdownTask` / `runAllInStartupContext`) is
handing the generated `ApplicationImpl` a **null** where the `StartupContext`/
`ShutdownContext` should be — likely `StartupContext.getValue(key)` (or the
StartupContext local in the recorder-replay path) returning null. Note: CratonVM's
own `CLINIT-CAUSE` diagnostic surfaces this NPE directly, so the logging gap (5b)
is **not** required to diagnose 5a.
**Next step (5a):** trace how the generated `ApplicationImpl` obtains the
`ShutdownContext` arg for `shutdownConfigValidator` (CratonVM tracing on
`StartupContext.getValue` / the recorder-call args) and make the
`quarkus_staticinit` shim provide the real StartupContext there. Deep Quarkus
recorder-framework / generated-bytecode work.

**5b — the logging gap (investigated; deeper than one fix).** Keycloak produces
**zero** logger output on stderr — not even the SmallRye `SRCFG` config warnings
that appear on HotSpot (only the direct `System.out.println` "Running the server in
development mode" from Picocli shows). Investigation:
- `native-builtins/src/logmanager.rs` already intercepts both
  `org.jboss.logmanager.Logger.logRaw(ExtLogRecord/LogRecord)` **and**
  `JBossLogManagerLogger.doLog/doLogf` (the JBoss-Logging-facade backends) and
  routes them to stderr.
- The `logRaw` native was a **placeholder** that printed `<jboss-logmanager logRaw>`
  with no real content; I **upgraded** it to read the record's inherited
  `java.util.logging.LogRecord` fields by name (`level`/`loggerName`/`message`/
  `thrown`) and dump the throwable via `dump_throwable_to_stderr` — a correct,
  general improvement (kept), but **`logRaw` is never hit** on the Keycloak boot.
- Even the `doLog` interceptor produces nothing → **Keycloak's loggers don't reach
  any intercepted path at all.** So the gap is upstream: the JBoss-Logging-facade
  *provider selection* / the synthetic `Logger` returned by the LogManager shim
  swallowing `log()` before it reaches `doLog`/`logRaw`, OR the boot failing before
  substantial logging. Pinning it is a multi-step logging-subsystem investigation.
- **Not blocking 5a:** CratonVM's own `CLINIT-CAUSE` diagnostic already surfaces the
  primary failure (the null-`ShutdownContext` NPE), so 5a can proceed without 5b.

This is **past ArC initialization** — the boot exercises the real Quarkus recorder
chain end to end up to Hibernate Validator.

### Gap 6 — `@ConfigMapping` impl (`getConfigMapping`) ✅ FIXED (2026-06-20, commit `bec91280`)
> **FIXED — and it unblocked the whole config cluster, reaching real ArC.** The
> `getConfigMapping` native now builds the real generated `<iface>$$CMImpl` from the
> live config via SmallRye's own per-mapping constructor:
> `new SmallRyeConfigBuilder().getMappingsBuilder()` →
> `new ConfigMappingContext(thisConfig, mappingBuilder)` →
> `ConfigMappingLoader.configMappingObject(iface, context)` — real config values, no
> fabrication. The `ConfigMappings.registerConfigMappings` registry path was tried
> first but its `buildMappings` validates the FULL property set against only the one
> passed mapping → `SRCFG00050 … does not map to any root` (per-mapping registration
> is structurally impossible there); `configMappingObject` skips that cross-mapping
> validation. **Result:** the boot advances from clinit bci 425 → PAST SharedConfig.
> `<clinit>`, HibernateValidator build, ConfigBuildStep, and
> `ArcProcessor.initializeContainer` (bci 634) into **real ArC bean creation** (bci
> 685). Follow-ups: (a) the *complete* fix registers ALL mappings at config-build so
> the registry is populated + validation passes (then delete this native); (b) the
> 1-arg `getConfigMapping(Class)` form still uses the old interface-alloc shim.
> The original write-up (for reference):

#### Original Gap 6 write-up
**Symptom:** with Gaps 4 & 5 fixed, `ApplicationImpl.<clinit>` advances to the next
deploy step (`VirtualThreadsProcessor$setup…​.deploy_0`, clinit bci=425) and fails:
```
java.lang.AbstractMethodError
  at io.quarkus.virtual.threads.VirtualThreadsRecorder.setupVirtualThreads(VirtualThreadsRecorder.java:45) bci=4
  at io.quarkus.runner.recorded.VirtualThreadsProcessor$setup1414761027.deploy_0
  at io.quarkus.runner.ApplicationImpl.<clinit> (bci=425)
```
**Root cause (FULLY traced 2026-06-20):** `VirtualThreadsProcessor$setup….deploy_0`
gets `runtimeConfig` from
`SmallRyeConfig.getConfigMapping(VirtualThreadsConfig.class, "quarkus.virtual-threads")`
(bci 10), then `new VirtualThreadsRecorder(that)`; `setupVirtualThreads` bci=4 calls
`runtimeConfig.enabled()`. CratonVM has a **native shim** for
`SmallRyeConfig.getConfigMapping` (`native-builtins/src/phases_late.rs` ~5051,
"Round 87") that does `alloc_concurrent_synthetic(ctx, <interfaceName>, 0)` — i.e. it
allocates an object **of the `@ConfigMapping` INTERFACE itself**, which has no method
bodies → `invokeinterface enabled()` → `AbstractMethodError`. The real generated impl
**`io/quarkus/virtual/threads/VirtualThreadsConfig$$CMImpl`** (SmallRye
`ConfigMappingGenerator` output) IS present in `generated-bytecode.jar`, with a no-arg
ctor (fields default-zeroed) and a real `(io.smallrye.config.ConfigMappingContext)`
ctor that populates fields from config.

Why the shim exists / why this is deep: real `SmallRyeConfig.getConfigMapping(Class,
String)` reads `this.mappings` (a `Map<Class,Map<String,Object>>`) and throws
`SRCFG00027 mappingNotFound` when empty. Under CratonVM `mappings` is **never
populated** (the Quarkus runtime config build that calls `builder.withMapping(type,
prefix)` → `ConfigMappingProvider` → `new $$CMImpl(ConfigMappingContext)` and stores
them isn't wiring the registry). And `ConfigMappingContext`'s only ctor is
`(SmallRyeConfig, SmallRyeConfigBuilder$MappingBuilder)` — so you can't cheaply build
one ad-hoc in a native. **Two real fix paths (both multi-session SmallRye-config
work):** (a) make the runtime config build register the `@ConfigMapping` mappings so
the real `getConfigMapping` returns populated `$$CMImpl`s and the shim can be deleted;
(b) have the native construct `<cls>$$CMImpl` via the real `(ConfigMappingContext)`
ctor against the live `SmallRyeConfig`. **Do NOT** swap the shim to a default-valued
no-arg `$$CMImpl` — that is the forbidden B5 fabricated-config stub (every
`@ConfigMapping` would silently report wrong/empty values app-wide).

### Gap 7 — ArC bean creation NPE ✅ FIXED (2026-06-20, commit `4ce68fd9`)
> **FIXED.** Root cause: a follow-on from Gap 6. `$12.apply` (the
> HibernateValidatorFactory creation lambda) does
> `localesBuildTimeConfig.locales().contains(Locale.ROOT)` at pc=122, and
> `LocalesBuildTimeConfig.locales()` (a `@WithDefault` `Set`) returned **null** →
> `Set.contains` NPE → `CreationException`. The Gap-6 `configMappingObject` path
> built the `$$CMImpl` but skipped SmallRye's default-value setup, so `@WithDefault`
> properties were null. Fix: `cm_construct_via_context` now mirrors
> `ConfigMappings.mapConfiguration`'s per-mapping setup —
> `builder.withMapping(configClass)` + `config.getDefaultValues().addDefaults(builder.getDefaultValues())`
> — before `configMappingObject`, so defaulted/collection properties resolve.
> **Result: ArC bean creation SUCCEEDS, `ApplicationImpl.<clinit>` completes, the
> Quarkus application STARTS, and the boot reaches the server-running lifecycle
> (`ApplicationLifecycleManager.waitForExit`) running REAL Keycloak startup**:
> truststore provider init + Hibernate ORM (7.2.14) JPA persistence-unit
> `[keycloak-default]` + Hibernate Validator 9.1.0. New frontier: **Gap 8** (DB
> connection) below. The original write-up:

#### Original Gap 7 write-up
With Gap 6 fixed, `ApplicationImpl.<clinit>` reaches **bci=685** (past
`ArcProcessor.initializeContainer` at 634) and fails in **real ArC**:
```
java.lang.RuntimeException: Failed to start quarkus
  cause = jakarta.enterprise.inject.CreationException
    cause = java.lang.NullPointerException
  at io.quarkus.arc.impl.InstanceImpl.getBeanInstance (InstanceImpl.java:325)
  at io.quarkus.arc.impl.InstanceImpl.get (InstanceImpl.java:190)
  at io.quarkus.hibernate.validator.runtime.HibernateValidatorRecorder.hibernateValidatorFactoryInit (HibernateValidatorRecorder.java:296)
  at io.quarkus.runner.ApplicationImpl.<clinit> (bci=685)
```
This is genuine ArC CDI: `InstanceImpl.getBeanInstance` is resolving/creating a
bean (the Hibernate Validator factory) and hits an NPE during creation. **We are
past `Arc.initialize()` — the real ArC container is up and resolving beans.** Next
step: trace `InstanceImpl.getBeanInstance` (InstanceImpl.java:325) / the bean's
`create()` to find the null (likely a generated `*_Bean.create()` reading a null
injection point, container state, or a still-shimmed dependency).

### Gap 8 — dev datasource yields no DB connection ⏳ OPEN (current frontier)
With Gap 7 fixed, Keycloak's real startup runs all the way into the **database
layer** and stalls there. Hibernate ORM resolves the dialect/version but the
connection is null:
```
INFO  [org.hibernate.orm.connections.pooling] HHH10001005: Database info:
        Database JDBC URL [undefined/unknown]
        Database driver: undefined/unknown
        Database dialect: H2Dialect
        Database version: 2.4.240
DEBUG [org.hibernate.orm.jdbc.lob] HHH10010002: Disabling contextual LOB creation as connection was null
```
The process then spins (CPU climbing, log frozen) in the interpreter — Keycloak's
`start-dev` embedded **H2 + Agroal** datasource isn't producing a real
`java.sql.Connection` (JDBC URL/driver "undefined/unknown").

**Investigation (2026-06-20), for the next session:**
- CratonVM HAS an Agroal shim (`native-builtins/src/agroal_pool.rs`): its
  `AgroalDataSource.getConnection()` native returns a synthetic `org/h2/jdbc/JdbcConnection`
  (mapping `jdbc:h2:mem:*` → an in-process SQLite backend), and defaults a missing URL
  to `jdbc:h2:mem:agroal-default`. So if Hibernate reached *that* native it would get a
  non-null connection. It does NOT — the connection Hibernate sees is null. The native is
  registered on the `io/agroal/api/AgroalDataSource` **interface**; Quarkus's runtime
  datasource bean is the real `io.agroal.pool.DataSource` impl (created by the Agroal
  recorder), so `getConnection()` virtual-dispatches to the **real** Agroal pool bytecode
  (or a Quarkus `ConnectionProvider` holding a null datasource), bypassing the shim.
- Behaviour is **nondeterministic**: one run tears down the Hibernate bootstrap registry
  (`Stop region factory`, `destroying bootstrap registry` → persistence-unit build failed),
  another spins with CPU climbing and the log frozen. Either way HTTP never binds.
- `RUST_LOG` does not enable the `agroal_pool`/`jdbc`/`apps_h2` native targets (CratonVM's
  tracing setup ignores those), so trace those another way (add `tracing` at register/native
  sites, or `CRATONVM_DBG_*`).

**Next-session plan:** pin the exact datasource object class + the `getConnection` call
path (real `io.agroal.pool.DataSource` vs the shim vs a null `QuarkusConnectionProvider`
datasource), then EITHER (a) route Keycloak's datasource through the working
`agroal_pool.rs` shim (force-override on the concrete impl / connection provider), OR
(b) make the real Agroal + H2 (or H2→SQLite) connection path work end-to-end. Then handle
Hibernate schema generation for Keycloak's ~100 entities (slow under `--nojit`; consider
enabling JIT). Also separately determine whether the "spin" is a tight loop (a real bug) or
just slow interpreted Hibernate metadata work. This is a large, multi-session DB subsystem
(`agroal_pool.rs` / `jdbc.rs` / `apps_h2.rs` + real `java.sql` + the H2 driver, backed by the
VM's real file layer).

**Deep-dive follow-up (2026-06-20, continued):**
- **Path confirmed REAL, shim bypassed.** `io.quarkus.agroal.runtime.DataSources.createDataSource`
  does `new io.agroal.pool.DataSource(config, listeners)` (bci 432/476) — the real Agroal pool
  impl. So `getConnection()` virtual-dispatches to real Agroal bytecode → its `ConnectionFactory`
  → the real JDBC driver. The `agroal_pool.rs` shim (registered on the `io/agroal/api/AgroalDataSource`
  **interface**) is never reached. To use the shim, force-override on the concrete
  `io/agroal/pool/DataSource` (or shim the JDBC `Driver.connect`/`DriverManager.getConnection` the
  real pool calls).
- **The boot is FURTHER than "DB layer."** When Hibernate proceeds past the null connection (it
  builds metadata on the explicit `H2Dialect`, so the null connection is non-fatal *for bootstrap*),
  the boot advances into **RESTEasy Reactive deployment** — `ApplicationImpl.<clinit>` pc≈750 →
  `ResteasyReactiveProcessor$setupDeployment.deploy_41` → `RuntimeDeploymentManager.deploy` →
  `buildResourceMethod` → loading endpoint-invoker classes via `RunnerClassLoader`/`JarFileReference`
  (nested-JAR). That's very close to the HTTP bind.
- **Nondeterministic.** Some runs proceed to RESTEasy as above; others, after the null connection,
  tear down the Hibernate bootstrap (`Stop region factory` / `Clear region references`) and then
  hang/spin. No exception is printed (logging gap 5b). The variance points at a timing-dependent
  null in the connection path.
- **JIT-neutral.** Booting with JIT ENABLED neither crashes nor unblocks — it hits the same
  Hibernate/connection wall. So the wall is the **DB connection itself**, not interpreter speed.
- **No invoke-NPE in the connection path.** `CRATONVM_DBG_NPE_STACK` shows no null-receiver invoke
  in the H2/Agroal/Connection path — the connection is a **silent null return** (the real
  `org.h2.Driver.connect(url)` / real Agroal pool returns null without an obvious single deref gap),
  not a one-line bug like Gaps 4–7.
- **Available backends (mapped).** CratonVM has (1) a real **`rusqlite`-backed JDBC surface**
  (`phases_late.rs::jdbc_registry` + `DriverManager.getConnection`/`java.sql.Connection`/`Statement`/
  `PreparedStatement`/`ResultSet` natives, with a passing round-trip test), mapping `jdbc:h2:mem` →
  `jdbc:sqlite::memory:`; and (2) partial **real-H2-engine** support (`apps_h2.rs` `TableFilter`
  overrides). The real Agroal pool calls `org.h2.Driver.connect` (real H2 engine) with a `jdbc:h2:`
  URL and is wired to NEITHER → null.

**Why this is a genuine multi-session subsystem (not a one-line fix), and why no shim was landed:**
- Routing `org.h2.Driver.connect` → the rusqlite surface gives a *non-null* connection cheaply, BUT
  it's a **dead end for Keycloak**: the connection would report SQLite, and Keycloak validates its DB
  type + runs **Liquibase H2-dialect DDL** (≈100 tables) that SQLite can't execute. So a SQLite-route
  "close" would fake a connection and then break on the real schema — the forbidden papering-over
  pattern. Not landed.
- The **principled close** is making the real `org.h2.Driver.connect(url)` (real H2 engine bytecode)
  produce a working `JdbcConnection` under CratonVM, then Keycloak's Liquibase schema + JPA run on
  real H2. That's the H2 database engine running under the VM — a large, multi-gap effort on its own
  (the `apps_h2.rs` H2-engine path is the seed).
- **Recommended next-session start:** trace the real `org.h2.Driver.connect` execution (add Rust
  tracing at H2 `Engine`/`Session`/`JdbcConnection.<init>`, or step the H2 bytecode) to find the first
  concrete gap where it returns null/fails; also confirm whether the Agroal datasource **bean** is
  non-null (rule out a CDI-injection null vs an H2-engine null). Decide strategy: real-H2-under-CratonVM
  vs. an alternative Keycloak-supported DB path. The boot is otherwise within a few steps of binding HTTP.

> **UPDATE (2026-06-20, branch `fix/keycloak-gap8-datasource`) — the "real H2 engine"
> framing above is REFUTED; the real blocker was the Agroal shim, now fixed behind a gate.**
> Isolated repros (real `com.h2database.h2-2.4.240.jar`, no full boot) prove the **real
> `org.h2.Driver` already works under CratonVM byte-identically to HotSpot**: connect, DDL,
> sequences, `MERGE`, `DatabaseMetaData` (URL / driver name / product / `2.4.240` version —
> NOT "undefined/unknown"), and multiple file-mode connections to the same DB. So Gap 8 is
> **not** an H2-engine gap and does **not** need "the H2 database engine running under the VM".
> The only H2-engine gaps found are in the **`AUTO_SERVER=TRUE`** path (which Keycloak's
> dev-file URL does NOT use): a now-fixed `Properties.store` bug (below) and a residual H2 TCP
> auto-server bind issue — both out of scope for Keycloak.
>
> **Real blocker (deterministic, isolated via `KcAgroal` over the real `io.agroal.agroal-pool-3.0.1.jar`):**
> the `native-builtins/src/agroal_pool.rs` **shim** intercepts
> `AgroalDataSourceConfigurationSupplier.get()` and returns a synthetic object whose runtime
> class is the bare **interface** `io/agroal/api/configuration/AgroalDataSourceConfiguration`
> (no method bodies). Quarkus/Keycloak run the **real** container bytecode
> (`DataSources.createDataSource` → `new io.agroal.pool.DataSource(supplier.get(), …)` and
> `AgroalDataSource.from` → `DataSourceProvider.getDataSource`), which does
> `invokeinterface config.dataSourceImplementation()` and hits the abstract method →
> `AbstractMethodError: … dataSourceImplementation() … has no Code attribute`. This is the exact
> shim-vs-real-bytecode collision the design doc's risk section predicted, NOT a silent H2 null.
> (The earlier "silent null / undefined-unknown" Hibernate `HHH10001005` line is the *normal*
> lazy-datasource bootstrap introspection — HotSpot logs the same — not the failure.)
>
> **Fix (real-cdi-bean-container, Step 2 gate):** `CRATONVM_REAL_AGROAL=1` suppresses the
> Agroal shim registration (`native-builtins/src/lib.rs` — gated at the `register_agroal_natives`
> call site via `real_agroal()`), so the real `io.agroal.pool.*` bytecode runs over the working
> real `org.h2.Driver`. **Validated:** `KcAgroal` reaches `== DONE OK ==` in isolation (real
> `io.agroal.pool.DataSource`, real `getConnection()`, DDL + pool reuse), and the **real Keycloak
> boot with `CRATONVM_REAL_AGROAL=1` no longer throws the `AbstractMethodError`** — it advances
> through truststore init, Hibernate ORM, the `keycloak-default` persistence unit, and Hibernate
> Validator, and the main thread reaches `ApplicationLifecycleManager.waitForExit` (real AQS
> `ConditionObject.awaitUninterruptibly` + ForkJoinPool — the concurrency the design doc feared
> WORKS). Gate is currently **opt-in**; flip to default-on (opt-out `CRATONVM_SYNTHETIC_AGROAL`)
> once the boot is fully green (see next frontier), per the "validate the suite before flipping" rule.
>
> **New frontier (Gap 9) — throughput, not a hard blocker.** With the gate ON, `--nojit` stalls
> at 150s right after `Hibernate Validator` (no JPA-Startup-Thread / Liquibase output); the
> single worker thread was undumpable (in Rust native). That looked like a hang, but it was
> **slowness**: with **JIT enabled** and a 300s deadline the same boot blows *past* the entire DB
> layer — through the JPA Startup Thread, the real Agroal datasource connect, Hibernate
> SessionFactory + Liquibase — and reaches **RESTEasy Reactive deployment**
> (`ApplicationImpl.<clinit>` pc≈790 → `ResteasyReactiveProcessor$setupDeployment.deploy_41` →
> `RuntimeDeploymentManager.deploy` → `createDeployment`, plus generated-serializer class loading
> via `RunnerClassLoader`/`JarFileReference.consumeSharedJarFile`). The main thread is **actively
> running** there (the watchdog dump shows main at varying stack depths 16→44, not a fixed
> wait-site), and the "**1425 threads dumped**" banner is the known watchdog re-dump artifact (every
> entry is `name="main"`), NOT a thread explosion. So the Agroal fix unblocks Gap 8 in substance —
> the DB layer completes — and the remaining wall is that the boot is **~15×+ slower than HotSpot's
> 21 s** (HotSpot reaches `Listening on http://localhost:8080`). RESTEasy deployment is the *last*
> phase before HTTP bind. Confirmed against the HotSpot oracle: HotSpot's boot logs the SAME benign
> `HHH10001005 … connection was null / undefined-unknown / Database version 2.4.240 / Stop region
> factory` block, then a dedicated **"JPA Startup Thread"** does `Started datasource <default>
> connected to jdbc:h2:file:…keycloakdb;NON_KEYWORDS=VALUE;DB_CLOSE_ON_EXIT=FALSE;DB_CLOSE_DELAY=0`
> → SessionFactory → Vert.x/Netty → `Listening` (21 s). Next-session start: (a) run the JIT boot
> with a longer deadline / at `--log-level=info` (the default boot emits 31 k TRACE/DEBUG lines —
> the logging itself is a large tax) to confirm it reaches `Listening`; (b) profile the slow phases
> (JIT tier-up coverage of the Hibernate/RESTEasy hot loops; the `NestedPropertyMappingInterceptor`
> config-resolution recursion seen on the hot stack; the per-class `RunnerClassLoader` jar reads).
> Once it reaches `Listening`, flip the Agroal gate to default-on (opt-out `CRATONVM_SYNTHETIC_AGROAL`).
>
> **CORRECTION / refinement — Gap 9 is BOTH throughput AND a nondeterministic native stall.** A
> third run (JIT, 580s deadline) did NOT match the lucky 300s run: it stalled again right after
> `Hibernate Validator` with only 3 threads (main + `Thread-1` daemon + `Timer`), main parked in
> `waitForExit`, `Thread-1` (the JPA Startup Thread) **stuck in Rust native** (undumpable). So the
> boot is **nondeterministic**: sometimes it progresses past the DB layer into RESTEasy deployment
> (proving the Agroal fix works end to end), other times the JPA Startup Thread wedges in native
> before `Started datasource` while main parks prematurely in `waitForExit` (the persistence unit
> never completes, no HTTP bind). That premature-park + native-stuck-worker pattern smells like a
> concurrency / GC-vs-parked-thread race (cf. the FJP root-reclaim and ES reactor-worker entries),
> NOT pure slowness. **Concrete evidence:** the stalling run logged
> `cratonvm_vm::jit::conservative_roots: scan_active_jit_frames: another thread holds live JIT
> frames while this thread's JIT chain is empty … the documented multi-thread-in-JIT-under-STW gap …
> cross_thread_jit_gap_hits=1/2`. I.e. a stop-the-world GC fired while `Thread-1` (the JPA Startup
> Thread) was executing JIT'd code with live roots the cross-thread root scanner could not see —
> covered only by a possibly-stale `root_snapshot`. A dropped live root → freed-then-used object →
> the worker wedges. This is the SAME tracked GC×JIT precise-roots gap as the
> precise-JIT-stack-maps / FJP-root-reclaim / ReflRepro-A2 work (register-resident / cross-thread
> JIT roots under STW), surfacing here because the boot is the first multi-threaded-JIT app to drive
> it under real GC pressure. NOTE: `--Xmx 6g` does NOT avoid it (tested) — a larger *max* heap does
> not reduce *young-gen* STW frequency, and the gap fires during young collections; so heap size is
> not the lever here (unlike the ES `6g→0` case, which was a different GC behaviour). The warning is
> conditional ("a stale snapshot *would* drop a live root"): it fires on essentially every run, but
> the boot only *wedges* on the runs where a root is actually dropped — matching the observed
> nondeterminism (same binary: one run reaches RESTEasy, others wedge at the JPA Startup Thread).
> The real fixes are the tracked precise-roots ones: `CRATONVM_SHADOW_STACK` precise JIT roots and
> the cross-thread STW JIT-root-scan follow-up (`CRATONVM_STRICT_JIT_ROOTS=1` makes the gap fatal,
> useful to force/locate a drop). This is squarely the precise-JIT-stack-maps program's work. (`--log-level=info` did not take effect — the boot still emitted TRACE/DEBUG —
> so the logging-tax hypothesis for the slowness is still untested; set the level via `keycloak.conf`
> / `quarkus.log.level` next time.) Net: the **deterministic** Gap-8 Agroal blocker is fixed and the
> DB layer is reachable; the remaining Gap-9 frontier is (1) a nondeterministic JPA-Startup-Thread
> native stall (get its wait-site — it is the single highest-value next step) and (2) raw throughput.
> Do NOT flip the Agroal gate to default-on until the boot reaches `Listening` reliably.
>
> **CONFIRMED after syncing to `dev` (53 commits) + testing the precise-roots opt-in.** The branch
> was merged up to `dev` — including `5085b137 fix(gc): forward monitor + native roots on
> safepoint-resume path` and the ES GC root-cause wave — and rebuilt. The Gap-9 wedge **still
> reproduces** (boot wedges at the JPA Startup Thread, `cross_thread_jit_gap` fires twice), so dev's
> current GC root-safety fixes do NOT close it. **`CRATONVM_SHADOW_STACK=1` also wedges** — and the
> code shows why: the cross-thread STW collector (`thread_registry::collect_all_root_snapshots`)
> reads ONLY each thread's *deposited* `root_snapshot`; it does no cross-thread stack scan, and
> `CRATONVM_SHADOW_STACK` only changes how a thread scans *its own* (thread-local) roots — it does
> not address the *staleness* of a peer's deposited snapshot. `wait_for_all` is cooperative (threads
> arrive via `arrive_and_wait`), and the interpreter safepoint (interpreter.rs:1750) + GC initiator
> (`maybe_gc_forced`, :618) both `update_root_snapshot` before parking — so the residual drop is a
> subtle path where a JIT worker's deposited snapshot is stale at the moment a peer's STW collector
> reads it. **The fix is a real cross-thread STW JIT-root scan**: at the safepoint, capture each
> parked worker's `(JIT spill range / SP, stack_high)` and have the collector walk it directly. A
> *conservative* peer-stack scan is provably sound here (GC is forced non-moving while any thread is
> in JIT — over-rooting can only retain, never corrupt), but it is still a GC-core change that must
> be validated against the precise-JIT GC oracle (bintrees18=68332206, pool 18/18). **Ownership:**
> this is the precise-JIT-stack-maps program's tracked follow-up (worktree `CratonVM-pjsm` /
> `feat/shadow-stack-followups`, whose §4 multi-thread work is partially done) — it should land there
> and reach `dev`, NOT as a drive-by in this datasource branch. Once it lands, re-run this boot; the
> Agroal gate can then flip to default-on.
>
> **MAJOR REFINEMENT (2026-06-20, deeper code read + `CRATONVM_STRICT_JIT_ROOTS` boot) — a
> cross-thread STACK scan is REDUNDANT; the real drop is REGISTER-RESIDENT.** Reading the park path:
> `safepoint_check` (interpreter.rs:1742-1750) already calls `invalidate_scan_cache_for_gc()` (which
> bumps the JIT-boundary gen → forces a *fresh* scan) and then `update_root_snapshot`, whose
> `scan_active_jit_frames` conservatively scans `[scanner_sp, entry_sp]` for **every** JIT entry —
> i.e. the parked peer's **entire JIT stack region**, fresh, into its deposited `root_snapshot`. Both
> park paths (`safepoint_check`; the blocking-native `deposit_root_snapshot`) go through this. So the
> collector, via `collect_all_root_snapshots`, **already has every parked peer's JIT *stack* roots.**
> A collector-side conservative `[park_sp, stack_high]` re-scan would cover the *same* stack range
> (plus interpreter frames already covered) → **provably redundant; it cannot fix the wedge.** A
> `CRATONVM_GC_VERIFY_STALE=1 CRATONVM_STRICT_JIT_ROOTS=1` boot confirmed the *condition* (panic on
> `Thread-1`, an `InnocuousThread` acting as GC collector, `GLOBAL_JIT_DEPTH=3` — genuinely
> concurrent multi-thread JIT) but the deposit covers those stack roots. Therefore the residual
> dropped root is **REGISTER-RESIDENT** — a live oop held in a register (not spilled to the stack) at
> the GC safepoint, which **no** stack scan (deposit-side or collector-side) can see. This is exactly
> the `reference_reflrepro_a2_register_root` class. **Correct fix = JIT codegen, not a scan:** spill
> every live-oop GPR (incl. caller-saved / `rax`) across safepoints, OR emit precise oop maps that
> record register locations (`CRATONVM_PRECISE_JIT_MAPS` path). This is the precise-JIT-stack-maps
> program's core remaining work. Diagnostic to pin the exact oop: `CRATONVM_GC_VERIFY_STALE=1` WITHOUT
> `STRICT` (so it doesn't abort on the benign conservative condition first), then inspect the
> zeroed-header report. **Do NOT implement the cross-thread stack scan — it is redundant.**
>
> **DECISIVE REFUTATION (2026-06-20) — Gap 9 is NOT a missed GC root at all (register-resident
> hypothesis REFUTED).** I implemented `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` (jit/src/x64.rs):
> blind-spill the FULL 14-GPR file (rax,rcx,rdx,rbx,rsi,rdi,r8–r15) to reserved frame slots at every
> GC-capable safepoint — the maximal A2-class register-spill (caller-saved + arg + rax + callee-saved).
> **Confirmed engaged** by `CRATONVM_DBG_JIT_DISASM`: `AllocProbe.run` emits all 14 `mov [rbp-…],reg`
> stores immediately before the `new` call. **GC-correct:** bintrees18 == 68332206 (golden) with the
> flag on, identical perf (38.6 s vs 38.8 s). **Yet the Keycloak boot STILL wedges** at the same
> point. Since the conservative scan covers that spill region and the spill captures *every* register,
> a missed register-resident root is impossible — and the four other levers also failed:
> `CRATONVM_JIT_SAFEPOINT_REG_SPILL=1` (callee-saved), `CRATONVM_SHADOW_STACK` (operand-stack register
> oops), `CRATONVM_DBG_FULLSTACK_SCAN` (collector's whole stack), `CRATONVM_GC_VERIFY_STALE` (0
> parked-interpreter-local zeroed-header hits). **Five independent root-coverage mechanisms fail ⇒ the
> wedge is NOT a dropped GC root** (not interpreter-local, not operand-stack/callee-saved/caller-saved/
> arg/rax-register, not collector-stack). The `cross_thread_jit_gap` warning is a CONSERVATIVE red
> herring (it fires on any multi-thread-in-JIT-under-STW, regardless of an actual drop). **New
> hypotheses (root-drop ruled out):** (a) a **JIT miscompile** → the JPA-startup worker spins in
> wrong control flow; (b) a **concurrency deadlock/livelock** (a VM lock/future/AQS that never
> releases); (c) a **native-call hang** (the worker stuck in a Rust native). Next decisive
> discriminator: a `--nojit` boot with a long deadline — if it progresses past the wedge (just slow),
> it's a JIT miscompile; if it also wedges, it's concurrency/native. Also: pin the worker's native
> wait-site (it's undumpable by the Java-frame watchdog). The `=all` spill is retained as a
> GC-validated, default-off diagnostic lever (not a fix for this bug). **Net: stop chasing GC roots
> for Gap 9.**
>
> **LOCALIZATION via `cdb` + sleep tracing (2026-06-20) — it is a Rust-internal `std::thread::sleep`
> poll-loop, NOT a Java-level wait, NOT JIT.** Confirmed `--nojit` wedges identically with the SAME
> signature (main parked in `waitForExit`; the wedged worker undumpable by the Java-frame watchdog),
> so JIT-miscompile is ruled out too. Attached `cdb` (Win10 debugger) to the live hung process
> (`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` so it doesn't self-abort): the wedged OS thread `"Thread-1"`
> is in `std::thread::sleep` ← `std::sys::thread::windows::sleep` ← `CreateWaitableTimerExW` (a
> *Rust* sleep, deep under interpreter/JIT frames); a separate `"main-vm"` OS thread is blocked in
> `ntdll!ZwReadFile` (a synchronous handle read); OS `"main"` is in `NtWaitForSingleObject` (the
> `waitForExit` park). Added a gated `CRATONVM_DBG_SLEEP_TRACE` (`native-builtins/src/lang_system.rs`)
> that dumps the Java caller chain at `Thread.sleep`/`sleepNanos`/`join(timeout)` — it fired **0**
> times during the wedge, proving the worker's sleep is NOT `java.lang.Thread.sleep`/`join` (and the
> object monitor uses a real `parking_lot::Condvar`, not sleep-poll). So the loop is a *VM-internal*
> Rust sleep-retry whose exact site is still unpinned because the release build (`strip=debuginfo`,
> `debug=false`) leaves `cdb` resolving every private frame to `socket_addr::impl$6::fmt+<offset>`
> (garbage). HotSpot oracle: after Hibernate Validator, the JPA Startup Thread does `Started
> datasource` → SessionFactory, then *main* runs a burst of `JtaTransactionWrapper` commits
> ("Non-HTTP task" — Keycloak DB bootstrap) → `Listening`. On CratonVM the log freezes right at
> Hibernate Validator (none of that happens) and main reaches `waitForExit` without binding HTTP.
> **Next-session unblock (highest leverage):** rebuild with full symbols (`[profile.release]
> debug=2, strip="none"`) and re-attach `cdb` → real function names for `Thread-1`'s sleep-loop AND
> `main-vm`'s `ZwReadFile` (use `!handle @rcx f` on `main-vm` to identify the file/socket it blocks
> on — likely the actual wedge, with `Thread-1` merely polling for its result). Then fix that
> primitive. This is a deep, nondeterministic concurrency/startup bug; the originally-assumed
> register-resident-root cause is DISPROVEN.
>
> **Tangential general VM bug fixed:** `java.util.Properties.store(OutputStream, comments)`
> (`native-collections/src/lib.rs::native_props_store`) wrote ONLY the comment and dropped EVERY
> entry — `props_collect_keys` read bucket-node field 0 as the key, correct only for legacy nodes
> (key=0) but not real-JDK nodes (hash=0, key=1, so field 0 is the int hash). Rewrote `store` to
> use the layout-aware `map_collect_entries` (the collector behind `entrySet()`/`toString()`) +
> JDK `saveConvert` escaping + the real `#`-prefixed comment format; fixed `props_collect_keys`
> likewise. Affects every `Properties.store` consumer; surfaced here because H2's `AUTO_SERVER`
> `FileLock` does a save→load→equals watchdog that an entry-less file fails ("Concurrent update").

> **RE-CHARACTERIZATION (2026-06-20, full-symbol cdb + watchdog Java-frame dump + ATHROW
> across ~7 boots) — Gap 9 is NOT a JPA-Startup-Thread Rust sleep-poll wedge; it is a
> SLOW, NONDETERMINISTIC boot that reaches RESTEasy deployment and then fails SILENTLY,
> never binding HTTP. The earlier "sleep-poll / register-resident-root / native-stall"
> framing is superseded by direct evidence.** Built a full-symbol binary
> (`scripts/build-kcboot-sym.bat`: `CARGO_PROFILE_RELEASE_DEBUG=1 STRIP=none` →
> `target-kcboot-sym/`; the default release PDB is `strip=debuginfo` and resolves every
> Rust frame to `core::net::socket_addr::impl$6::fmt+<huge>` — useless) and used `cdb`
> + the built-in `CRATONVM_DBG_HANGWALK` all-threads native dumper + the
> `--stack-dump-on-timeout` Java-frame watchdog + `CRATONVM_DBG_ATHROW`.
>
> **What the boot actually does (gate ON, `CRATONVM_REAL_AGROAL=1`):**
> - The Java-frame watchdog shows only THREE Java threads: `main` (the Java main thread),
>   `Thread-1` (a daemon doing `ReferenceQueue.remove(timeout)` — a Cleaner/Reference
>   consumer, BENIGN), `Thread-2` (`java.util.TimerThread` in `Object.wait()`, BENIGN).
>   There is **no wedged "JPA Startup Thread"** — the prior "Thread-1 stuck in a Rust
>   sleep-poll" reading was a transient/misread.
> - cdb (full symbols) confirms `main` parks in `LockSupport.park`
>   (`ParkState::park_interruptible`, jvm_thread.rs:176, via the AQS `ConditionObject`)
>   inside `ApplicationLifecycleManager.waitForExit` → `KeycloakMain.run` (the
>   `QuarkusApplication.run`) → i.e. **`application.start()` RETURNED — Quarkus considers
>   startup "complete"** — yet the log is frozen at Hibernate Validator and **port 8080 is
>   NOT bound** (verified live: process alive, `curl` → 000, no `Listening` log). The
>   launcher OS `main` is just `handler.join()`ing main-vm (NtWaitForSingleObject).
> - The "frozen at Hibernate Validator" log is MISLEADING: the boot continues SILENTLY
>   past "Loaded expression factory via original TCCL". `CRATONVM_DBG_ATHROW` shows it
>   actively building the Hibernate Validator `PredefinedScopeValidatorFactoryImpl`
>   (constraint metadata over Keycloak's many `@…` validation annotations — the only
>   exceptions are BENIGN, caught `NoSuchMethodException`/`ValidationException(javafx
>   ObservableValue)`/`UnsatisfiedResolutionException(MapStringConverter)`/`HibernateException(/hibernate.properties)`),
>   and the watchdog caught `main` in **real RESTEasy Reactive deployment**
>   (`ApplicationImpl.<clinit>` pc≈750 → `ResteasyReactiveProcessor$setupDeployment.deploy_41`
>   → `RuntimeDeploymentManager.deploy` → `RuntimeResourceDeployment.buildResourceMethod`
>   over Keycloak's hundreds of REST endpoints). So the boot reaches RESTEasy deploy — it
>   does NOT "stall at Hibernate Validator".
>
> **Thread dump of the parked process (cdb, 6 OS threads) — the Vert.x HTTP server NEVER
> STARTS.** The only threads are: `main` (launcher, NtWaitForSingleObject joining main-vm),
> `main-vm` (Java main, parked in `LockSupport.park`/`waitForExit`), `Thread-1` (in
> `ZwCreateTimer2` = a Rust `std::thread::sleep` — this is the BENIGN Cleaner's
> `ReferenceQueue.remove(timeout)` poll; **this is exactly the "Rust sleep-poll" the prior
> session saw and MISATTRIBUTED to a wedged "JPA Startup Thread"**), `Thread-2`
> (`TimerThread`), and two idle Windows thread-pool workers
> (`ZwWaitForWorkViaWorkerFactory`). **There are NO `vert.x-eventloop-thread-*` / Netty NIO
> threads** — a live Quarkus HTTP server always spawns them. So even though `main` reaches
> `waitForExit` (Quarkus thinks startup finished), the Vert.x/Netty HTTP server start task
> ran without actually starting the server (no event loops, no bind). **The functional gap
> is therefore localized to the Vert.x/Netty HTTP server startup (`VertxHttpRecorder` /
> `VertxCoreRecorder`), NOT the DB / Hibernate / RESTEasy-deploy path (which all run).**
>
> **The failure is SILENT and NONDETERMINISTIC (across ~7 boots):** the process ends as
> (a) all-threads-parked wedge, (b) `main` parked at `waitForExit` then a silent process
> exit after ~50 s, or (c) a silent `exit(1)` under JIT — with **no error, no exception,
> no panic, no `System.exit` log** (the doc's "logging gap 5b" / invisible-failure
> blocker, strongly re-confirmed as the #1 obstacle). HTTP never binds in ANY run, even
> with `CRATONVM_REAL_NET_SOCKETS=1` (the server-socket path is real-bytecode-capable via
> `ServerSocketAdaptor`, proven for Tomcat, so the bind SHOULD work once startup truly
> completes). The `cross_thread_jit_gap` (GC×JIT multi-thread root-scan) warning fires
> only INTERMITTENTLY (2/4 captured runs, incl. a `--nojit` run) → unreliable indicator,
> consistent with the doc's "conservative red herring".
>
> **The dominant, measured problem is THROUGHPUT.** Two large taxes: (1) **logging** —
> CratonVM's JBoss-LogManager bridge (`native-builtins/src/logmanager.rs`) emits EVERY
> record and defers level-filtering to the Rust `tracing` subscriber ("levels are
> inherited from the process-wide tracing subscriber"), so Keycloak's `--log-level`/
> `quarkus.log.level` does NOT suppress the per-certificate TRACE/DEBUG truststore flood
> (~31 k lines) — a real, general, fixable throughput bug; (2) CPU-bound metadata builds
> (ValidatorFactory over hundreds of constraints, RESTEasy deploy over hundreds of
> endpoints) under interpreter / JIT-warmup. HotSpot reaches `Listening` in ~21 s; under
> CratonVM the boot is many minutes and dies before binding.
>
> **Prioritized next steps (each a real sub-task):**
> 1. **Diagnosability FIRST (the unblocker):** make the silent failure VISIBLE. Pin the
>    exact silent-exit path (it is NOT Java `System.exit` — `native_system_exit` eprintln's
>    and that line never appears; NOT a printed `run()` Err or panic). Candidate: an
>    abnormal/daemon-only VM exit or a buffered-then-lost final log. Add an unconditional,
>    flushed eprintln at every `std::process::exit` site + on the JVM "last non-daemon
>    thread exited" path, and flush the JBoss-LogManager/console handler on exit. Until the
>    fatal cause is printable, every further step is guesswork (exactly the doc's takeaway).
> 2. **Throughput:** honor the Java-configured log level in the JBoss-LogManager bridge
>    (don't emit filtered-out records); profile/JIT-cover the ValidatorFactory + RESTEasy
>    `buildResourceMethod` hot loops.
> 3. **GC×JIT precise roots:** the tracked precise-JIT-stack-maps / cross-thread-STW-root
>    program (worktree `CratonVM-pjsm`) — re-test once (1) makes failures visible, using
>    `CRATONVM_STRICT_JIT_ROOTS=1` to force any real drop fatal.
> 4. **Vert.x HTTP server startup (now-localized functional gap).** The thread dump proves
>    the server never starts (no `vert.x-eventloop-*` threads) even though startup "completes".
>    **ROOT CAUSE FOUND (2026-06-21) — a Gap-8-style shim-vs-real mismatch.** CratonVM's
>    `native-builtins/src/vertx_eventloop.rs` registers a synthetic native
>    `io.vertx.core.impl.VertxImpl.init(I)V` (note: `int` arg) whose body spawns the
>    `vert.x-eventloop-N` threads as NON-daemon — its comment notes this is "what keeps
>    Quarkus/Keycloak alive past main()". But Keycloak 26.6.3 bundles **Vert.x 4.5.27**
>    (`lib/lib/main/io.vertx.vertx-core-4.5.27.jar`), whose `VertxImpl` has
>    `void init(java.util.List<VerticleFactory>)` and builds its Netty `eventLoopGroup`
>    (a `private final io.netty.channel.EventLoopGroup`) **in the constructor** — there is **NO
>    `init(int)` method**. So the synthetic native NEVER matches/fires → the event-loop threads
>    are never spawned via that hook → no HTTP bind, AND no non-daemon Vert.x threads to keep the
>    VM alive (so `main`'s `waitForExit` park nondeterministically either holds the process open
>    or lets it silently exit — the two failure modes observed). The synthetic-event-loop model in
>    `vertx_eventloop.rs` is the wrong shape for Vert.x 4.5 — exactly analogous to the Agroal shim
>    in Gap 8. **Fix direction (mirror Gap 8):** add a `CRATONVM_REAL_VERTX` gate that suppresses
>    the `vertx_eventloop.rs` synthetic natives so the real Vert.x 4.5 + Netty
>    `NioEventLoopGroup`/`SingleThreadEventExecutor` bytecode runs (real `NioEventLoop` threads
>    spawned via real `Thread.start`; real `ServerSocketChannel` bind under
>    `CRATONVM_REAL_NET_SOCKETS=1`), then make any remaining real-Netty natives work. Validate in
>    isolation first (a minimal Vert.x `HttpServer.listen()` repro) before the full boot, per the
>    "validate before flipping" rule. This is the gap between "Quarkus thinks it started" and a
>    real `Listening on http://localhost:8080`.
>
> Repro harness for this session: `scratch/gap9-*.sh` (boot variants),
> `scratch/gap9-wait-and-cdb.sh` (wait-for-wedge + cdb all-thread dump),
> `scripts/build-kcboot-sym.bat` (full-symbol build). Boot needs
> `CRATONVM_REAL_AGROAL=1` (+ `CRATONVM_REAL_NET_SOCKETS=1` for a real listen).

> **MILESTONE (2026-06-21) — the real Vert.x/Netty HTTP server SERVES under CratonVM.**
> Validated in isolation with a minimal `Vertx.vertx().createHttpServer().listen(8080)`
> repro (`scratch/vertx/`, real Vert.x 4.5.27 + Netty jars from the Keycloak dist):
> `curl localhost:8080` → **HTTP 200** + the request handler fires (`HANDLED request path=/`).
> This is Keycloak/Quarkus's HTTP foundation (Vert.x → Netty NIO). Required gates:
> `CRATONVM_REAL_VERTX=1` + `CRATONVM_REAL_NET_SOCKETS=1`. Five commits, each a real VM bug,
> found by fix→build→validate iteration with the new `run() Ok/Err` diagnostic +
> `CRATONVM_DBG_SELECTOR` + cdb:
> 1. **`e7fec84f`** — route `sun/nio/ch/WEPollSelectorProvider.{openSelector,openServerSocketChannel,
>    openSocketChannel}` to CratonVM's selector/`ssc_open`/`sc_open`. JDK 21+ Windows defaults to
>    the wepoll selector provider; Netty's `NioEventLoop`/`NioServerSocketChannel` call
>    `provider.openX()` DIRECTLY (bypassing the static `Selector.open()`/`ServerSocketChannel.open()`
>    CratonVM intercepts), so the real `WEPoll`/`ServerSocketChannelImpl` path ran →
>    `UnsatisfiedLinkError: sun/nio/ch/WEPoll.eventSize()I` + `ClosedChannelException`.
> 2. **`185fb300`** — `ServerSocketChannel.isBound()`/`localAddress()` natives (impl-methods on
>    `ServerSocketChannelImpl` that `ServerSocketAdaptor`/Netty call on our abstract-class channel
>    object). → port 8080 binds, `LISTENING` fires.
> 3. **`5f8f96b1`** — `CRATONVM_REAL_VERTX` gate (mirrors `real_agroal`): suppresses the synthetic
>    `vertx_eventloop` natives so the REAL Netty `NioEventLoop.run()` drives `Selector.select()`
>    (the synthetic loop ran only tasks/timers and never polled the selector). cdb confirmed the
>    real Netty event-loop threads then sit in `nio_selector::selector_select`→`WSAPoll`.
> 4. **`0670dc5f`** — populate the selector's LIVE `selectedKeys` field on select. `WSAPoll` DID
>    detect the accept (`CRATONVM_DBG_SELECTOR`: `EXIT n=1`/connection) but Netty served nothing
>    because it reflectively REPLACES `SelectorImpl.selectedKeys` with its own `SelectedSelectionKeySet`
>    and reads the FIELD; CratonVM only mirrored readyOps into `sk_table` + overrode `selectedKeys()`
>    on-demand (Tomcat/ES path) and never wrote the field. `populate_selected_keys_field()` adds each
>    ready key to the field (JDK-faithful; null-safe; on-demand path unchanged so Tomcat/ES unaffected).
>
> **The real Agroal H2/JDBC path also runs** (these gates are additive to `CRATONVM_REAL_AGROAL=1`).
> NB: the earlier `VertxImpl.init` "shim mismatch" hypothesis (b18c6e32 on `dev`) was a RED HERRING —
> the real blockers were the WEPoll selector routing + the Netty `selectedKeys`-field. The synthetic
> `vertx_eventloop` natives are now bypassed by the gate, not the bug.
>
> **OPEN — full Keycloak boot still blocked EARLIER (the original silent-exit, Gap 9 core).** With
> ALL gates, the full boot runs ~7 min then **self-terminates SILENTLY** at the post-Hibernate-Validator
> phase (the slow ValidatorFactory build + RESTEasy deploy over Keycloak's hundreds of constraints/
> endpoints) — BEFORE reaching the (now-working) Vert.x HTTP startup. No diagnostic fires: NOT
> main-vm `run()` returning (the new `run() Ok/Err` log is silent), NOT `System.exit`
> (`native_system_exit` eprintln absent), NOT a Rust panic (the panic hook eprintln's), NOT a GC
> OOM/abort signal, NOT the crash handler. So a background thread terminates the process via a path
> that bypasses every hook. Pinning it via `cdb` launched with a breakpoint on
> `ntdll!NtTerminateProcess` (catches exit/abort/terminate from any thread → calling stack).
> This silent exit + the silent post-Hibernate logging are the remaining full-boot walls (Gap 9's
> throughput + invisible-failure core). The HTTP layer is no longer a blocker.
>
> **REFINED (2026-06-21, memory monitor + parked-state cdb with ALL gates) — it is NOT throughput at
> the end; main PARKS, then exits.** A 15 s WS/CPU monitor shows the boot reaches a PARKED state at
> ~4 min (WS flat ~1083 MB, CPU flat ~108–115 s — i.e. IDLE, not computing) and stays idle ~5 min,
> then self-exits (~570 s; the bare run reported code 127). cdb of the PARKED process (with
> `CRATONVM_REAL_VERTX=1` + `CRATONVM_REAL_NET_SOCKETS=1`) shows only 5 threads — `main` (launcher
> join, NtWaitForSingleObject), `main-vm` (parked in `LockSupport.park` ← `ParkState::park_interruptible`
> ← `native_lock_support_park`, i.e. the AQS condition inside `ApplicationLifecycleManager.waitForExit`),
> `Thread-1` (Cleaner `ReferenceQueue.remove`), `Thread-2` (Timer) — and **ZERO `vert.x-eventloop`/Netty
> threads**. So **`application.start()` RETURNS (main reaches `waitForExit`) WITHOUT ever running the
> Vert.x HTTP server startup** — exactly the original "main parks prematurely in waitForExit" symptom.
> The Vert.x serving chain fixed above is correct (isolated repro serves) but is NEVER REACHED by the
> full boot. **So the true Gap-9 core blocker is UPSTREAM of HTTP: the Quarkus startup/recorder-deploy
> chain (`ApplicationImpl.<clinit>` deploy steps → `doStart` STARTUP_TASKS) completes/returns without
> running the HTTP-server (`VertxHttpRecorder`/`VertxCoreRecorder`) deploy+startup step** (no Vert.x
> instance is created → no event loops → no bind), yet `start()` reports success and main parks.
> NEXT (deep, multi-session): trace the deploy chain to find where the HTTP-server startup step is and
> why it doesn't run/take effect (a step is skipped, a recorder returns a null/no-op RuntimeValue, or
> the chain is truncated after RESTEasy-deploy) — plus the silent post-park exit (~570 s) and the
> logging-flush visibility. This is squarely the Quarkus recorder-framework work (cf. Gaps 5–7).

> **BREAKTHROUGH (2026-06-21) — the silent-exit was a NO-OP SHIM on `Application.start`; gating it
> off unblocks RUNTIME_INIT and makes the failure VISIBLE.** SMOKING GUN: `quarkus_staticinit.rs`
> registered `io.quarkus.runtime.Application.start([String])V` (+ `stop`/`awaitShutdown`) as a no-op
> (`native_app_lifecycle_no_op`). `Application.start()` → generated `ApplicationImpl.doStart()` is the
> **RUNTIME_INIT** phase (Vert.x HTTP listen, datasource connect, Infinispan, Narayana JTA, …). The
> STATIC_INIT `<clinit>` deploy steps (ArC / RESTEasy-metadata / Hibernate) run as real bytecode (Gaps
> 4–7), which is why the boot reached RESTEasy deploy — but the no-op SKIPPED all of RUNTIME_INIT, so no
> HTTP server, no Vert.x threads, and `start()` "succeeded" → main parked at `waitForExit`, then silently
> exited. **FIX: `CRATONVM_REAL_QUARKUS_START` gate** (opt-in, mirrors `real_agroal`/`real_vertx`)
> suppresses the no-op so the real `start()`→`doStart()` bytecode runs (commit on
> `fix/keycloak-gap8-datasource`). With it + `REAL_AGROAL`/`REAL_VERTX`/`REAL_NET_SOCKETS`, the full boot
> **now runs RUNTIME_INIT** — the log shows Infinispan (`Virtual threads support: enabled`), Narayana JTA,
> **Vert.x** (`io.vertx.core.logging…`) and **Netty** (`io.netty.util.ResourceLeakDetector`,
> `InternalThreadLocalMap`) all INITIALIZING (none of which happened before) — and the failure is now
> LOUD instead of silent (Keycloak's own `ExecutionExceptionHandler`: *"ERROR: Failed to start server in
> (development) mode"* + the cause + `[cratonvm] System.exit(1) called`). So the doc's #1 blocker
> (invisible failures) is resolved for the full boot.
>
> **NEXT GAP (now visible + being fixed): JBoss-LogManager `LoggerNode` NPE.** RUNTIME_INIT logging
> config fails with *"Cannot invoke org.jboss.logmanager.LoggerNode.setUseParentHandlers(boolean) because
> this.loggerNode is null"*. CratonVM uses synthetic `org/jboss/logmanager/Logger` objects with NO
> `loggerNode` and intercepts the loggerNode-deref methods (getLevel/setLevel/isLoggable/logRaw/
> getUseParentHandlers/…), but `setUseParentHandlers(Z)V` was MISSING from that list, so its real bytecode
> derefs the null node. FIX: add a null-safe no-op `org/jboss/logmanager/Logger.setUseParentHandlers(Z)V`
> (logmanager.rs, mirroring the JUL override + the constant `getUseParentHandlers`). Building + validating;
> expect the boot to advance to the next RUNTIME_INIT gap (then iterate toward `Listening`). The boot is
> now an iterative, VISIBLE gap-walk through RUNTIME_INIT, not a silent wall.
>
> **GAP-WALK PROGRESS (2026-06-21):**
> - ✅ `setUseParentHandlers` fixed → boot advanced; the log now shows **Netty configuring its event
>   loops** (`io.netty.channel.MultithreadEventLoopGroup -Dio.netty.eventLoopThreads: 64`,
>   `NioEventLoop` `-Dio.netty.noKeySetOptimization`/`selectorAutoRebuildThreshold`) + the Quarkus
>   thread-pool — i.e. RUNTIME_INIT is well underway.
> - ⏳ NEXT GAP (now the active frontier): **Hibernate SessionFactory build fails** —
>   *"[PersistenceUnit: keycloak-default] Unable to build Hibernate SessionFactory ... Cannot invoke
>   `com.github.benmanes.caffeine.cache.LocalCacheFactory.newInstance(Caffeine, AsyncCacheLoader, boolean)`
>   because `factory` is null"*. Caffeine 3.2.3's `LocalCacheFactory` is an INTERFACE that dynamically
>   loads a generated per-feature cache-impl class via `MethodHandles.Lookup` (`LOOKUP.findClass` /
>   `findConstructor`, cached in `FACTORIES` via `computeIfAbsent(name, ::newFactory)`); under CratonVM
>   `loadFactory(name)` returns null (a `MethodHandles.Lookup.findClass`/`findConstructor` or
>   `computeIfAbsent` gap on Caffeine's generated classes), so `factory.newInstance(...)` NPEs. NOT a
>   no-op-stub; a real MethodHandles/reflection sub-investigation (lang_invoke.rs). Fix it, then continue
>   the gap-walk (next likely: the H2 datasource connect, then the Vert.x HTTP listen — at which point the
>   already-fixed serving chain should bind 8080).
> - Benign/teardown noise seen (not the blocker): `SQLServerDriver`/`oracle.jdbc.OracleConnection`
>   missing-driver clinit (unused drivers); a `VarHandleReferences$FieldInstanceReadWrite.<init>`
>   NoSuchMethodError from `org.jboss.threads…clearThreadLocals` during teardown.
>
> **STATUS:** Gap 9 transformed from a silent wall into a visible, advancing RUNTIME_INIT gap-walk.
> 8 fixes merged to `dev` (merge `498fae5f`): the Vert.x/Netty HTTP serving chain (4) + the
> `CRATONVM_REAL_QUARKUS_START` gate + `setUseParentHandlers` + docs. Boot order now: STATIC_INIT
> `<clinit>` (ArC/RESTEasy-metadata/Hibernate-metadata) → RUNTIME_INIT (logging ✅ → Netty event loops ✅
> → Hibernate SessionFactory build ❌ Caffeine). Required gates: `CRATONVM_REAL_AGROAL` +
> `CRATONVM_REAL_VERTX` + `CRATONVM_REAL_NET_SOCKETS` + `CRATONVM_REAL_QUARKUS_START` (all opt-in).
>
> **UPDATE (2026-06-22) — Caffeine cleared; CURRENT BLOCKER is a gen-GC bug.** The Caffeine
> `LocalCacheFactory` failure was the **VarHandle static-field init** bug, FIXED + merged to `dev`
> (`b223fd21`; `vh_static_slot` now triggers the holder `<clinit>` — see
> [`reference_varhandle_static_init_on_access`] in memory). That unblocked the **entire** Hibernate
> SessionFactory build: the boot now reaches Agroal+H2, Infinispan region factory, Narayana JTA recovery,
> Vert.x router init, and the 64-entity metamodel, ending at `No schema management actions found`.
> It then **HANGS** there: main parks in `JPAConfig.startAll → CompletableFuture.get → Signaller.block →
> LockSupport.park` and is never unparked — a **gen-GC promotion bug that loses the CompletableFuture
> completion** (the future is promoted to old gen while the pushed young `Signaller` is lost by the
> copying young GC). Full root-cause, 3 s repro, and workarounds (`-XX:+UseG1GC` /
> `CRATONVM_NO_GC_PROMOTION=1`) in
> **[gc-gen-promotion-completablefuture-completion-loss.md](gc-gen-promotion-completablefuture-completion-loss.md)** (✅ now FIXED on dev — was a synthetic `CompletableFuture.complete` native missing `postComplete()`, not the GC; moved to docs/internal).
> NOTE: with G1 (immune to that GC bug) the boot reaches the same point but hits a SEPARATE hang — two
> persistence-unit worker threads stuck executing bytecode (a worker livelock, distinct from the GC bug).

### Quarkus ArC (`CRATONVM_REAL_ARC`) — REACHED and running
Real ArC bytecode RUNS during the boot — `Arc.initialize` → container →
`InstanceImpl` bean resolution/creation all execute as real bytecode, and ArC
successfully creates beans (HibernateValidatorFactory, etc.). A dedicated
`CRATONVM_REAL_ARC` gate (suppress the `quarkus_arc.rs` shim so the real container
is solely authoritative) is now meaningfully testable via the boot.

### Milestone (2026-06-20)
The real Keycloak 26.6.3 (Quarkus) server now boots under CratonVM **through ArC
into real Keycloak startup** — past class loading, config (incl. real
`@ConfigMapping`), the full recorder chain, `Arc.initialize()` + CDI bean creation,
Keycloak provider init, and Hibernate ORM/JPA — stopping at the DB-connection gap
(Gap 8). Seven boot gaps fixed (Gaps 1–7). Two general VM bugs landed on `dev`
(Gap 4 static-sync monitor, Gap 5 ShutdownContext); Gaps 6–7 (real `@ConfigMapping`)
are on `fix/keycloak-gap6-configmapping`.

## Key takeaway

Walking the **real** Keycloak (Quarkus) server boot under CratonVM, with HotSpot
as the oracle, has surfaced and fixed **four general VM bugs** so far — each a real
bug affecting more than Keycloak:
1. `JarEntry.getSize()/getMethod()` returned 0 (Quarkus `RunnerClassLoader`).
2. NIO `Files.newInputStream` threw `IOException` not `NoSuchFileException` for a
   missing file (optional-config sources).
3. a stale no-op stub of `sanitizeDisabledMappers` that also skipped Profile config.
4. `static synchronized` methods locked a synthetic object instead of the `Class`
   mirror (broke `Class.wait()/notify()` from static-sync methods).

The boot has advanced from *"dies at the first class load"* to *"runs the entire
Quarkus application startup — recorders, ArC (via its shim), Hibernate Validator —
and fails at gap 5"*, i.e. **past `Arc.initialize()`**. The single highest-leverage
next step is the **logging gap** (gap 5b): Keycloak's JBoss-LogManager output barely
reaches stdout, so primary failures are invisible. Fixing it makes gap 5 and every
subsequent gap directly readable instead of inferred. The boot is a productive,
HotSpot-comparable driver for the remaining gaps toward `Listening on …`.
