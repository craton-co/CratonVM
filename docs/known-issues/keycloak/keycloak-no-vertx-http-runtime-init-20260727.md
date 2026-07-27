# OPEN — Keycloak "starts" but no Vert.x/HTTP: `Application.start()` returns without running RUNTIME_INIT

**Status: OPEN.** Rewritten 2026-07-27 (second pass) — the first version of
this doc blamed Arc bean resolution. **That was wrong**, see "Corrected
diagnosis" below. Follow-on to the closed boot blockers in
`docs/internal/keycloak/keycloak-boot-blocked-version-null-20260726.md`; not a
regression, this surface was simply unreachable before.

## Symptom

`kc.sh start-dev` on the real `keycloak-quarkus-dist-26.6.1` under CratonVM:

- reaches `io.quarkus.runtime.Quarkus.waitForExit()` on `main` — i.e.
  `ApplicationLifecycleManager.run` already called `application.start(args)`
  AND is now inside `KeycloakMain.run(args)`, the normal *running* state
  (verified from the bytecode: `ALM.run`'s pc lands right after
  `QuarkusApplication.run`);
- but the process has **6 OS threads** (`java`, `main-vm`, `Attach-Listener`,
  `cratonvm-jit-co`, `Common-Cleaner`, `Timer-0`) — no Vert.x event loop, no
  "JPA Startup Thread", no Infinispan threads;
- `main-vm` sits in `futex_do_wait` with its CPU time flat: genuinely parked,
  not spinning;
- no HTTP listener ever opens, and the
  `Keycloak … started in Ns. Listening on: http://…` banner never prints;
- no exception, anywhere: `CRATONVM_DBG_ATHROW=1` shows the last Java throw is
  Hibernate's own (caught) `UnsatisfiedResolutionException` for
  `MapStringConverter`, and `CRATONVM_STRICT_SWALLOWS=1` escalates nothing.

So `Application.start()` returned *successfully* without RUNTIME_INIT having
created anything. Identical with `--nojit`, so it is not a JIT miscompile.

## Where it diverges from HotSpot, exactly

HotSpot control (`JAVA_HOME=/home/victor/jdk25 bash bin/kc.sh start-dev …
--log-level=debug`, 18.7s to the banner) continues past the last line CratonVM
produces:

```
DEBUG …BeanContainerImpl] No matching bean found for … __QuarkusInit    <- last common line
DEBUG …InternalLoggerFactory] Using SLF4J as the default logging framework
DEBUG …smallrye.config] SRCFG01006: Loaded ConfigSource … (a second, RUNTIME config load)
FINE  …jakarta.json.spi.JsonProvider] Checking ServiceLoader
DEBUG …hibernate.orm.factory] HHH90020001: Instantiating factory …   (JPA Startup Thread)
DEBUG …QuarkusInfinispanRegionFactory] Starting Infinispan region factory
INFO  …[org.infinispan.CONTAINER] ISPN000974: Virtual threads support: enabled
…
INFO  …[io.quarkus] Keycloak 26.6.1 … started in 18.745s. Listening on: http://localhost:8080
```

CratonVM's last INFO is Hibernate's `HHH10001005: Database info:` block, which
is STATIC_INIT-phase work inside `ApplicationImpl.<clinit>`. Nothing from the
RUNTIME_INIT phase ever appears.

## Corrected diagnosis — the Arc "No matching bean found" lines are NORMAL

The first version of this doc treated ~40 trailing
`BeanContainerImpl: No matching bean found for type class … The bean might have
been marked as unused and removed during build` DEBUG lines as the failure.
They are not. **HotSpot logs 170 of them on the same boot** (`grep -c` on the
DEBUG control run), and an isolated probe
(`docs/known-issues/repros/keycloak-runner-loader-20260727/ArcInitProbe.java`)
that builds the real `RunnerClassLoader`, runs `Arc.initialize()` and resolves
the same types reports `available=false` for `__QuarkusInit` and
`ServerStringMessageBodyHandler` **on HotSpot too**, with a healthy container
(`beans=108, observers=16`). They are Quarkus telling you a bean was removed as
unused at build time. Ignore them.

## Ruled out

- **Synthetic stubs.** `CRATONVM_TRACE_UNIMPLEMENTED=1` over the whole boot
  reports exactly one stub, `io/quarkus/arc/impl/package-info` (benign).
- **A truncated/mis-parsed `ApplicationImpl`.** `AppImplProbe.java` compares the
  reflective method table against HotSpot: identical
  (`ApplicationImpl` → `doStart`, `doStop`, `getName`; `Application` → 13).
- **JIT miscompilation.** `--nojit` stalls at the same point.
- **A swallowed exception.** `CRATONVM_STRICT_SWALLOWS=1` escalates nothing.
- **Async work being dropped.** `FjpProbe.java` (with Keycloak's own
  `-Djava.util.concurrent.ForkJoinPool.common.threadFactory=…QuarkusForkJoinWorkerThreadFactory`)
  shows `CompletableFuture.supplyAsync`, `commonPool.submit` and a plain
  `new Thread(...)` all run to completion — though note CratonVM runs pool
  tasks INLINE (`supplyAsync -> Thread`, `submit -> main`) where HotSpot uses
  `ForkJoinPool.commonPool-worker-1`, and reports `parallelism=1` vs 15.

## Next step for whoever picks this up

The question is narrow and answerable: **does `ApplicationImpl.doStart(String[])`
execute its RUNTIME_INIT step calls at all?** It is a single generated method
with a long straight-line body of `deploy_N` calls. Instrument entry/exit of
that one method (or of `io.quarkus.runtime.StartupContext`) and compare against
the HotSpot DEBUG log's RUNTIME_INIT prefix (`InternalLoggerFactory` →
second SmallRye config load → `JsonProvider` ServiceLoader → JPA factory). If
`doStart` runs but its steps no-op, the next suspect is the recorder-proxy
dispatch; if `doStart` never runs, the suspect is the virtual dispatch of the
abstract `Application.doStart` to the `ApplicationImpl` override.

Note the inline-execution finding above: if any RUNTIME_INIT step hands work to
the common pool and *depends* on it running on another thread (Vert.x
initialisation does), inline execution on `main` could complete the step while
leaving the service unstarted. Worth checking before the dispatch theory.

## Second, independent bug found while investigating: log levels are ignored

`native_jboss_logging_logger_do_log` / `..._do_logf`
(`native-builtins/src/logmanager.rs`) stand in for the concrete backend's
`doLog`/`doLogf`. The real ones start with an `isEnabled(level)` check —
`org.jboss.logging.Logger.debugf` and friends do NOT check the level
themselves, they delegate it. Ours checked nothing, so **every** `tracef` /
`debugf` in the process was formatted and written: 2992 lines on this boot
against HotSpot's 10 (INFO) / 1978 (DEBUG). The `%d` / `%b` placeholders also
survive into the output, because the formatter only substitutes `%s`, `%%`
and `%n`.

A level check is now implemented but **opt-in** via
`CRATONVM_JBOSS_LOGGER_LEVEL_FILTER=1`, because the root cause is one level
deeper: **CratonVM ignores `-Djava.util.logging.manager`**, so Quarkus's
`org.jboss.logmanager.LogManager` is never installed, jboss-logging falls back
to `JDKLoggerProvider` (HotSpot uses `JBossLogManagerLogger` on the same
command line), and everything the application configures — including
`kc.sh --log-level=debug` — is invisible to us. Verified: with the filter
forced on, `--log-level=debug` produced **zero** DEBUG lines. Honouring
`java.util.logging.manager` is the real fix; then the filter can default on.
