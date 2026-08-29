# RESOLVED — Keycloak "starts" but no Vert.x/HTTP: `Application.start()` returned without running RUNTIME_INIT

**Status: RESOLVED 2026-07-28.** Retired from `docs/known-issues/keycloak/`.
The reported failure -- `Application.start()` returning *successfully* having
created no Vert.x event loop, no HTTP listener and no error anywhere -- is
fixed, and the RUNTIME_INIT phase now executes end to end. Six general VM bugs
were found and fixed along the way, each with an isolated reproducer.

The boot does not yet reach `Listening on http://localhost:8080`; the chain of
blockers *downstream* of RUNTIME_INIT is filed separately as
the retired `keycloak-boot-chain-after-runtime-init-20260728` write-up
(now in this same folder; the whole chain was closed 2026-07-29).
Those are new surfaces this fix made reachable, not residuals of this issue.

## Root cause

`native-builtins/src/quarkus_staticinit.rs` registered
`io.quarkus.runtime.Application.start([Ljava/lang/String;)V` (plus `stop` and
`awaitShutdown`) as **no-ops**. `start()` is what drives the generated
`ApplicationImpl.doStart([String])` -- the RUNTIME_INIT phase that runs the
STARTUP_TASKS. The STATIC_INIT `<clinit>` deploy steps (ArC, RESTEasy
metadata, Hibernate metadata) ran as real bytecode, which is why the boot
*looked* healthy right up to the end; RUNTIME_INIT was skipped wholesale, so
`start()` "succeeded", `main` parked in `waitForExit`, and nothing ever bound
a socket.

An `CRATONVM_REAL_QUARKUS_START` opt-in gate to suppress the no-ops had existed
since 2026-06-21 but was never validated or flipped, so the default build still
took the silent path. It is now **default-on**; `CRATONVM_SYNTHETIC_QUARKUS_START=1`
(or `CRATONVM_REAL=quarkus-start=0`) restores the no-ops for A/B diagnosis.

## Correction to the 2026-07-27 write-up

That version said the second bug was that
`org.jboss.logging.Logger.debugf` and friends "do NOT check the level
themselves, they delegate that to `doLog`/`doLogf`". **That is wrong** --
jboss-logging 3.6.2's bytecode for `debugf` is literally
`if (isEnabled(DEBUG)) doLogf(...)`.

What actually produced 2992 lines against HotSpot's 10 is that our
`doLog`/`doLogf` natives *are* the sink: they print every record handed to them
straight to stderr, bypassing the backend's HANDLER chain, which is where a
real jboss-logmanager setup filters. `isEnabled` answers `true` on **both** VMs
(Quarkus runs its root logger at ALL and filters at the console handler).
Verified with an isolated probe: under
`-Djava.util.logging.manager=org.jboss.logmanager.LogManager`, HotSpot prints
nothing at all for `tracef`/`debugf`/`infof` (no handler configured) while
CratonVM printed all three.

The same write-up's premise that CratonVM "ignores `-Djava.util.logging.manager`"
is also not the operative problem: the property IS honoured
(`logmanager.rs::try_allocate_property_log_manager`). What differs from HotSpot
is the jboss-logging *provider* (`JDKLoggerProvider` vs
`JBossLogManagerLogger`), because `LoggerProviders.findProvider` is natively
short-circuited for the WildFly boot path. That divergence is cosmetic for
level handling -- both providers end up at our own natives -- and is left as is.

## The six fixes

1. **`Application.start/stop/awaitShutdown` no-ops retired**
   (`native-builtins/src/quarkus_staticinit.rs`). The RUNTIME_INIT phase runs:
   Netty event loops, Vert.x, Infinispan (`ISPN000556: Starting user
   marshaller`), Narayana JTA recovery, the Agroal/H2 datasource, the Hibernate
   `SessionFactory`, and the full Liquibase schema migration all execute as
   real bytecode. Verified with default flags -- the Agroal / Vert.x /
   net-sockets companion gates are NOT required to get this far.

2. **Array-class resolution was loader-blind**
   (`vm/src/runtime/interpreter.rs::resolve_class_loader_aware`). JVMS 5.3.3
   synthesises an array class from its resolved *component*, but every
   loader-faithful branch keyed on the array name itself and
   `drive_defining_loader_load` declines `[` names outright. So `[LX;` fell
   through to the flat global path, which either **fabricated a synthetic stub**
   for a component only a custom loader can serve (and registered it globally
   under `Application`, permanently poisoning `X`) or failed outright. Two
   sites now offer the component to the referencing class's own loader: a
   pre-pass for the would-be-stub case and a retry on resolution failure.
   Found via `ldc [Lorg/jboss/threads/EnhancedQueueExecutor$TaskNode;` from
   `EnhancedQueueExecutor.<clinit>` (surfacing as
   `TaskNode.<init> ... has no Code attribute`) and
   `[Lorg/antlr/v4/runtime/atn/ATNConfig;` from
   `ATNConfigSet$AbstractConfigHashSet.createBuckets`.

3. **`Class.getModule()` reported the wrong ClassLoader**
   (`native-builtins/src/lang_class.rs::unnamed_module_for_loader`,
   `lib.rs`, `classloader_real.rs`). There was ONE process-wide unnamed module
   whose `loader` was always the application loader. HotSpot gives every
   `ClassLoader` its own unnamed module. Every caller-sensitive JDK API that
   derives a loader from the caller's module therefore looked in the wrong
   place. Now memoised on the loader's own `java.lang.ClassLoader.unnamedModule`
   field (exact identity, GC-rooted by its owner); built-in loaders keep the
   single canonical module so the existing identity contracts are untouched.

4. **`ResourceBundle.getBundle(String)` ignored the caller's ClassLoader**
   (`native-builtins/src/locale_resources.rs`). Our natives replace every
   `getBundle` overload, and the no-loader ones fell back to CratonVM's
   process-wide `-cp` scan instead of the JDK's caller-sensitive resolution. A
   class defined by a custom loader could not find a bundle that only ITS
   loader can serve. A loader miss still falls back to the `-cp` scan, so the
   change is strictly additive. Framework-independent reproducer:
   `docs/known-issues/repros/keycloak-runtime-init-20260728/CallerBundleProbe.java`
   + `BundleUser.java`.

5. **`Properties` silently discarded whole files**
   (`native-builtins/src/properties_sidetable.rs`). Two defects:
   * `MAX_TOTAL_OBJECTS` was 10 000 and `put_kv` **silently dropped** every
     write to a not-yet-tracked object past that. The Keycloak boot had 24 016
     tracked objects by the time Infinispan loaded
     `../../../apps/META-INF/infinispan-version.properties`, so all 14 parsed entries were
     discarded. Raised to 262 144 (it is a runaway-growth backstop, not a
     working-set limit) and the drop is now visible under
     `CRATONVM_DIAG_PROPERTIES`.
   * `getProperty` consulted only the side-table, never the object's real
     `map` `ConcurrentHashMap` backing -- so anything that reached the object
     through real bytecode was invisible. It now reads the real backing before
     the `defaults` chain, matching the JDK's own order.
   * `store_parsed_entries` also ran its exact-class loop with the receiver
     **unpinned** across allocating calls; both branches now pin.
   Symptom: `Version.getProperty("infinispan.version", "0.0.0-SNAPSHOT")`
   returned the default, and Infinispan then rejected its own
   `urn:infinispan:config:16.0` namespace with `ISPN000327`.

6. **jboss-logging message formatting**
   (`native-builtins/src/logmanager.rs`). `doLogf` understood only
   `%s`/`%%`/`%n`; every other conversion (`%d`, `%b`, `%x`, `%.2f`, `%c`, ...)
   both survived into the output verbatim AND desynchronised the parameter
   cursor, shifting later `%s` onto the wrong argument
   (`"d=%d s=%s", 42, "str"` printed `d=%d s=42`). It now renders through the
   real `String.format`, which is what the concrete backends do, with the
   in-Rust pass kept as a fallback and its conversion scanner fixed to consume
   one parameter per conversion. `doLog` ignored args[4] entirely, so every
   `logv`-family line printed its raw pattern (Agroal's
   `{0}: Validation test on connection {1}`); it now applies
   `java.text.MessageFormat`. Both verified byte-for-byte against HotSpot.

   The DEBUG/TRACE level filter (`CRATONVM_JBOSS_LOGGER_LEVEL_FILTER`) is now
   **default-on** (`=0` opts out), modelling the console-handler threshold the
   natives bypass. Keycloak boot output: 11 331 -> 307 lines, every
   INFO/WARN/ERROR retained, and the boot is markedly faster.

## New diagnostics

* `CRATONVM_DBG_CLASS_RESOURCE=<substring>` -- names the `ClassLoader` a
  `Class.getResourceAsStream` was routed through and whether it produced bytes.
  A resource lookup that silently answers `null` is otherwise invisible and
  surfaces only as a distant application default.
* `CRATONVM_DIAG_PROPERTIES` now reports a `put_kv` capacity drop.

## Reproduction

```bash
cd /data/tmp/kc-dist/keycloak-26.6.1
JAVA_OPTS_KC_HEAP='-Xms512m -Xmx4g' \
JAVA_HOME=<dir-with-cratonvm-as-bin/java> CRATONVM_JAVA_HOME=/home/victor/jdk25 \
  bash bin/kc.sh start-dev --http-enabled=true --hostname-strict=false
```

`-Xmx4g` is required: `kc.sh start-dev` defaults to `-Xmx512m`, under which the
boot corrupts the heap rather than reporting an OutOfMemoryError -- see the
follow-on doc.
