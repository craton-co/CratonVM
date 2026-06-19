# Keycloak 26.6.3 (Quarkus) boot under CratonVM — progress & gap chain

**Status:** 🟡 PARTIAL (audit 2026-06-19) — **4 of 5 boot gaps FIXED** in dev: Gap 1 `JarEntry` getSize/getMethod=0, Gap 2 NIO missing-file `NoSuchFileException`, Gap 3 stale `sanitizeDisabledMappers` stub removed, Gap 4 static-synchronized methods lock the `Class` mirror (`0e81bc70`, JVMS §2.11.10). Residual **OPEN** = the current boot frontier: **Gap 5** — Quarkus recorder `ShutdownContext` null NPE (`HibernateValidatorRecorder.shutdownConfigValidator`) + the JBoss-LogManager logging gap. The real ArC CDI boot is not yet reached.

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

### Gap 4 — `static synchronized` used a synthetic lock, not the `Class` mirror ✅ FIXED
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

### Gap 5 — Quarkus app startup fails: `HibernateValidatorRecorder` NPE ⏳ OPEN (next)
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
chain end to end up to Hibernate Validator. Gap 5 is the next frontier and is a
**multi-session, Quarkus-recorder-framework** effort (5a), with 5b as a visibility
enabler.

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
