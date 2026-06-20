# Keycloak 26.6.3 (Quarkus) boot under CratonVM — progress & gap chain

**Status:** 🟡 PARTIAL (updated 2026-06-20, branch `fix/keycloak-gap5-shutdownctx`) — **Gaps 1–5 FIXED; Gap 4 re-fixed; Gap 6 is the new frontier.** Gap 1 `JarEntry` getSize/getMethod=0, Gap 2 NIO missing-file `NoSuchFileException`, Gap 3 stale `sanitizeDisabledMappers` stub removed. **Gap 4 (static-synchronized → `Class` mirror, JVMS §2.11.10) had REGRESSED on dev for the real boot** — `0e81bc70` missed THREE interpreter invoke fast-paths (cached / stackless-cached / vcached) that still locked the synthetic `get_class_lock_object`, so `ApplicationStateNotification.notifyStartupFailed` (static-sync `notifyAll`) threw `IllegalMonitorStateException` and MASKED every real `<clinit>` failure; now fixed at all three sites (commit `8a3c6c07`). **Gap 5 (Quarkus recorder `ShutdownContext` null NPE) FIXED** — the `StartupContext` native shim shadowed the real `<init>` that registers the `StartupContext$1` `ShutdownContext` proxy under `getValue("io.quarkus.runtime.ShutdownContext")`; shim removed, real bytecode runs (commit `5a489187`). **Gaps 6 & 7 (@ConfigMapping) FIXED** (commits `bec91280`, `4ce68fd9`): `getConfigMapping` now builds the real `<iface>$$CMImpl` from live config via SmallRye `configMappingObject` **with default values applied** (mirrors `mapConfiguration`: `withMapping` + merge `getDefaultValues`) — no fabrication. **This unblocked the whole config cluster, ArC bean creation, and Keycloak's real startup.** The boot now runs through `Arc.initialize()` + CDI bean creation, Keycloak provider init, and Hibernate ORM/JPA, reaching the server-running lifecycle (`ApplicationLifecycleManager.waitForExit`). Current **OPEN** frontier: **Gap 8** — Keycloak's `start-dev` embedded H2/Agroal datasource yields **no `java.sql.Connection`** (Hibernate logs `Database JDBC URL [undefined/unknown]`, `connection was null`) and the interpreter spins. **Seven gaps fixed; the real ArC CDI container runs; HTTP not yet bound (DB layer is the wall).**

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
