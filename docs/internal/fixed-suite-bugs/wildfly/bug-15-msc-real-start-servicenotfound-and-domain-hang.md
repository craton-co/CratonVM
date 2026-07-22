# Bug 15 - `CRATONVM_MSC_REAL_START` gate blocks any WildFly boot from reaching real service execution

Status: PARTIALLY FIXED (2026-07-06) — Symptom 2's `ServiceNotFoundException` and a
masking worker-pool race are fixed; two new, deeper blockers found immediately behind
them (`addListener`/`removeListener` `AbstractMethodError`, and an as-yet-unidentified
real exception from `ApplicationServerService.start()`). See the Fix section below.
Severity: High
First confirmed: 2026-07-06, Azure host `20.83.144.174`, worktree `fix/wildfly-domain-corrupt-value-20260706`

## Context

While re-verifying the fix for
[`wildfly-domain-heap-corrupt-value-timeout.md`](../../known-issues/wildfly-domain-heap-corrupt-value-timeout.md),
no Maven/`wildfly-core` testsuite checkout was available, so a WildFly 32.0.1.Final
*binary distribution* (GitHub release, no build needed) was driven directly with
`bin/standalone.sh` and `bin/domain.sh` under a fresh `dev`-HEAD `cratonvm` build, using
the same real-JDK/`--nojit` configuration `apps/wildfly-suite-runner/run-suite.sh` uses
(that runner never sets `CRATONVM_MSC_REAL_START`, so this is the actual default the
suite runs under). Both entry points get stuck before any real subsystem/service work
happens, so no test in this area can currently reach sustained concurrent execution
(worker pools handling repeated requests) — the environment every domain-mode timeout doc
in this folder ultimately depends on.

## Symptom 1 — default config (`CRATONVM_MSC_REAL_START` unset): indefinite hang, no error

`standalone.sh` and `domain.sh` both reach:

```text
INFO [org.jboss.as] WFLYSRV0049: WildFly Full 32.0.1.Final (WildFly Core Unknown) starting
DEBUG [org.jboss.as.config] 
DEBUG [org.jboss.as.config] VM Arguments: 
TRACE [org.jboss.as.config] 
```

...and never produce another line. Confirmed as a genuine indefinite wait (not just a slow
interpreted boot) via `CRATONVM_DEFAULT_WATCHDOG_SEC=60` + the built-in stack-dump
watchdog:

```text
=== T19.H1 watchdog: deadline of 60s elapsed; requesting thread stack dumps ===
--- T19.H1 thread summary: 13 registered thread(s) ---
  tid=0 name="main" alive=true daemon=false roots=7
  ...
  tid=4 name="Thread-4" alive=true daemon=false roots=9
  ...
--- T19.H1 stack dump: tid=4 name="Thread-4" frames=1 ---
tid=4 depth=0 class=org/jboss/threads/EnhancedQueueExecutor$ThreadBody method=run desc=()V pc=445 last_pc=442 source=EnhancedQueueExecutor.java
--- T19.H1 end dump tid=4 ---
```

All non-daemon worker threads are correctly parked idle inside JBoss Threads'
`EnhancedQueueExecutor$ThreadBody.run` — near-0% CPU on the process the whole time. This
is consistent with `CRATONVM_MSC_REAL_START` being off by default (see
[`docs/internal/app-jvm-bugs/handoff-wildfly-msc-service-start.md`](../app-jvm-bugs/handoff-wildfly-msc-service-start.md)):
`ServiceBuilderImpl.install()` is only wired to actually drive
`Service.start(StartContext)` when that flag is set. With it off, whatever the boot
sequence is waiting on to signal "service started" never fires, so it waits forever.

## Symptom 2 — `CRATONVM_MSC_REAL_START=1`: standalone aborts, domain hangs even earlier

Setting the flag on standalone mode gets past the point above and into real
bytecode-driven service installs, then hard-aborts:

```text
org.jboss.msc.service.ServiceNotFoundException: service  not found
	at org.jboss.msc.service.ServiceRegistryException.<init>(ServiceRegistryException.java:48)
	at org.jboss.msc.service.ServiceNotFoundException.<init>(ServiceNotFoundException.java:48)
	at org.jboss.msc.service.ServiceContainerImpl.getRequiredService(ServiceContainerImpl.java:659)
	at org.jboss.msc.service.LeakDetectorServiceContainer.getRequiredService(LeakDetectorServiceContainer.java:112)
	at org.jboss.as.server.BootstrapImpl.internalBootstrap(BootstrapImpl.java:112)
	at org.jboss.as.server.BootstrapImpl.bootstrap(BootstrapImpl.java:63)
	at org.jboss.as.server.Main.main(Main.java:93)
FATAL [org.jboss.as.server] WFLYSRV0239: Aborting with exit code %d
```

Note the exception message itself is blank (`service  not found`, two spaces) — the
`ServiceName` involved renders as an empty string wherever
`ServiceRegistryException`/`ServiceNotFoundException` builds that message, which is a
second, smaller data point (something about how CratonVM represents/marshals this
particular `ServiceName` loses its segments before it reaches `toString()`).

`javap -p -c -l` on `org/jboss/as/server/BootstrapImpl.class` (extracted from
`modules/system/layers/base/org/jboss/as/server/main/wildfly-server-24.0.1.Final.jar` in
the WildFly 32.0.1.Final distribution) shows exactly what's being looked up:

```text
198: aload         7                    // serviceTarget (container.subTarget())
200: aload         8
202: invokestatic  Method org/jboss/as/controller/ControlledProcessStateService.addService:(...)
...
245: new           #60                  // class org/jboss/as/server/ApplicationServerService
...
260: aload         7
262: getstatic     #62                  // Field org/jboss/as/server/Services.JBOSS_AS:Lorg/jboss/msc/service/ServiceName;
265: aload         11                   // the new ApplicationServerService
267: invokeinterface  Method org/jboss/msc/service/ServiceTarget.addService:(Lorg/jboss/msc/service/ServiceName;Lorg/jboss/msc/service/Service;)Lorg/jboss/msc/service/ServiceBuilder;
272: invokeinterface  Method org/jboss/msc/service/ServiceBuilder.install:()Lorg/jboss/msc/service/ServiceController;
277: pop
278: aload_0
279: getfield      #6                   // Field container:Lorg/jboss/msc/service/ServiceContainer;
282: getstatic     #62                  // Field org/jboss/as/server/Services.JBOSS_AS:Lorg/jboss/msc/service/ServiceName; (SAME field)
285: invokeinterface  Method org/jboss/msc/service/ServiceContainer.getRequiredService:(Lorg/jboss/msc/service/ServiceName;)Lorg/jboss/msc/service/ServiceController;
```

The service is installed via the **legacy 2-arg convenience overload**
`ServiceTarget.addService(ServiceName, Service).install()` (not the modern
`addService(name).setInstance(svc).install()` builder-pattern call that
`handoff-wildfly-msc-service-start.md`'s P2 fix explicitly hooks), then looked up eight
bytecode instructions later with the exact same `Services.JBOSS_AS` static field. Two
leading hypotheses for a fixer to check first:

1. The 2-arg `addService(ServiceName, Service)` overload (likely a JBoss MSC
   `ServiceTarget` default/interface method) produces a builder/install call that our
   native hook's argument extraction doesn't recognize as the same shape as the 1-arg +
   `setInstance` pattern, so the install silently no-ops instead of registering.
2. The install DOES register, but the container's lookup key derived from a `ServiceName`
   at install time doesn't match the key derived from the (`getstatic`-cached, should be
   the same object) `ServiceName` at lookup time — e.g. hashing/equality on a synthesized
   wrapper rather than the real `ServiceName`'s segments.

Domain mode with the same flag does not reach even this exception — no error, no log
progress for the full 200-second window tried, worse than the default-flag hang above.
Not yet root-caused; likely a lock-ordering/re-entrancy issue specific to the
host-controller's own service graph in the `drive_starts` loop
(`native-builtins/src/jboss_msc.rs`), per `handoff-wildfly-msc-service-start.md`'s own
open P3 ("value injection ... NOT wired") and P4 (async services) follow-ups.

## Reproduce

```bash
# Get a binary distribution (no Maven needed):
curl -sL -o wildfly.zip https://github.com/wildfly/wildfly/releases/download/32.0.1.Final/wildfly-32.0.1.Final.zip
python3 -c "import zipfile; zipfile.ZipFile('wildfly.zip').extractall('.')"
chmod +x wildfly-32.0.1.Final/bin/*.sh

mkdir -p fakejdk/bin
ln -s <path-to>/cratonvm fakejdk/bin/java

cd wildfly-32.0.1.Final/bin
JAVA_HOME=<path-to>/fakejdk CRATONVM_JAVA_HOME=<real-jdk25> \
  CRATONVM_DISABLE_JIT=1 CRATONVM_MSC_REAL_START=1 \
  ./standalone.sh -b=127.0.0.1 -bmanagement=127.0.0.1
# -> ServiceNotFoundException within ~3 seconds.

# Default config (no CRATONVM_MSC_REAL_START) hangs indefinitely instead; add
# CRATONVM_DEFAULT_WATCHDOG_SEC=60 to get a thread-stack dump proving it's a genuine
# parked wait, not slow interpretation.
```

## 2026-07-06 Fix — ServiceNotFoundException + masking worker-pool race

Worktree `fix/wildfly-msc-getservice-registry-gap-20260706` off `dev`, same Azure host,
re-driving the identical repro below (WildFly 32.0.1.Final binary distribution,
`bin/standalone.sh`, real JDK 25 boot, `CRATONVM_MSC_REAL_START=1 CRATONVM_DISABLE_JIT=1`).

**Root cause 1 (this doc's headline `ServiceNotFoundException`):** `native_service_builder_install`
(the P2 hook on `ServiceBuilderImpl.install()`) registers every installed service *only* in
CratonVM's Rust-side shadow container (`global_container()` / `service_roots()`). It never
touches `ServiceContainerImpl`'s own real `registry` field — a genuine
`ConcurrentMap<ServiceName, ServiceRegistrationImpl>` in the real jboss-msc object, confirmed via
`javap -c` on the extracted `jboss-msc-1.5.4.Final.jar` class:

```
public ServiceController<?> getService(ServiceName);
  aload_0
  getfield #4      // Field registry:Ljava/util/concurrent/ConcurrentMap;
  aload_1
  invokeinterface  // ConcurrentMap.get(Object)
  checkcast        // ServiceRegistrationImpl
  ...
```

`getService`/`getRequiredService` were never intercepted (no native was registered for either),
so this real bytecode always read the real, always-empty `registry` map — even for a service
`install()` had just registered a moment earlier — and `getRequiredService` threw
`ServiceNotFoundException` (confirmed via `CRATONVM_DBG_MSC=1`: `install id=1 name=jboss.as ...`
immediately followed by the exception, with no further installs in between).
`LeakDetectorServiceContainer.getService`/`getRequiredService` (what `BootstrapImpl` actually
calls on `container`) just `invokeinterface` straight through to the real delegate's own method,
confirmed via `javap -c` — so hooking only the concrete `ServiceContainerImpl` class is
sufficient; no separate hook is needed on the wrapper.

**Fix:** added `native_service_container_get_service` / `native_service_container_get_required_service`
in `native-builtins/src/jboss_msc.rs`, registered on `org/jboss/msc/service/ServiceContainerImpl`
(gated behind `CRATONVM_MSC_REAL_START`, same as the rest of the P2 natives), answering from the
Rust container's `get_id(&name)` + `service_roots()[id].controller_mirror` instead of the
always-empty real map. On miss, throws a real `org.jboss.msc.service.ServiceNotFoundException`
(via `new_object_initialized`) with a legible message — real MSC's own message for this case
renders **blank** under CratonVM (`"service  not found"`, confirmed in the original repro output),
because it's built via an `invokedynamic` string-concat over the `ServiceName` argument, whose
`toString()` apparently resolves empty through that specific call shape. Not chased further here:
it's moot on the fixed success path, and only mattered for the exact wording of a genuine
not-found case.

**Verified:** repro no longer throws `ServiceNotFoundException`; `CRATONVM_DBG_MSC=1` shows
`[msc] getService jboss.as: found`.

**Root cause 2 (found investigating why `ApplicationServerService.start()` never appeared to run
any real logic even after fix 1):** `ServiceContainer::add_service` unconditionally pushes every
newly-installed `Active`/`Passive` service onto `task_queue` and notifies the background worker
pool — a queue/pool that predates P2 (T19.1-era) and whose `run_start_local` has, by its own doc
comment, "no real Java callback to run locally" — it just flips bookkeeping straight to `Up`. P2's
own driver, `drive_starts`, does **not** consume this queue — it independently scans
`state.services` via `take_ready_start`. Both mechanisms were live simultaneously, so a woken
worker thread could — and, confirmed via `CRATONVM_DBG_MSC=1`, reproducibly did — race
`drive_starts` for the exact same newly-installed service and fake-complete it first: a
`run_start_local (bookkeeping-only, NO real start() invoked) id=1` trace fired *before*
`install()`'s own `install id=1 ...` trace even printed. This meant `CRATONVM_MSC_REAL_START=1`
was not actually driving the real `start()` callback for any service that lost this race — the
opposite of what the flag promises — while still reporting `Up` and letting boot appear to
progress.

**Fix:** `add_service` now skips the `task_queue` push (and worker-pool notify) entirely when
`msc_real_start_enabled()` is true, so only `drive_starts`'s synchronous, real-callback-invoking
loop ever starts a service in that mode.

**Verified:** re-running the repro after this fix shows `[msc] -> start id=1` (real `drive_starts`
invocation) where the race previously suppressed it, and the real `ApplicationServerService.start()`
callback now actually runs — and throws a real exception, which the container correctly records as
`Failed` (visible directly in the log now: `MSC service start() threw — marked FAILED, boot
continues: ExceptionThrown(...) service=jboss.as`) rather than silently faking `Up`.

**Not fixed / flagged for follow-up:** `ServiceContainer::demand()` (reached via
`native_service_controller_set_mode`'s `Active`/`Passive` branch, registered unconditionally, not
gated behind the real-start flag) schedules onto the exact same background-worker queue and is
structurally the identical hazard. Not touched this session — `setMode` was never exercised by
this repro, so there was nothing to verify a fix against — but a future session hitting it should
apply the same `!msc_real_start_enabled()` guard.

## 2026-07-06 New blockers found (immediately behind the two fixes above)

1. **`ServiceController.addListener`/`removeListener` → `AbstractMethodError`.** Confirmed via
   `javap`: `ServiceController` is a real interface with both methods `abstract`, no default body.
   No native hook backs either, so `BootstrapImpl.internalBootstrap`'s
   `controller.addListener(new BootstrapImpl$1(...))` (registering a `LifecycleListener` that
   resolves a `FutureServiceContainer`) aborts immediately with `AbstractMethodError` at
   `BootstrapImpl.java:113`. **Not a trivial no-op fix**: real MSC's `addListener` fires past
   events immediately if the controller already reached a terminal state (`UP`/`FAILED`/`REMOVED`)
   — tractable, since our container already knows the current state — but `BootstrapImpl$1`'s own
   `handleEvent` (confirmed via `javap -c` on the anonymous class) chains into
   `controller.removeListener(this)` → `controller.getServiceContainer()` →
   `container.getRequiredService(Services.JBOSS_SERVER_CONTROLLER)` → registers yet another nested
   listener (`BootstrapImpl$1$1`) on *that* controller — i.e. a real, multi-hop async workflow, not
   a single synchronous callback. Worse, real MSC services commonly call
   `StartContext.asynchronous()` and finish on a background thread, so by the time `addListener` is
   called the state may genuinely still be `Starting` — which our shadow container has no
   mechanism to notify later (no stored listener list per controller). A shallow no-op
   implementation would trade this clean, immediate `AbstractMethodError` for a silent, harder-to
   -diagnose hang (the exact worse failure mode this doc's own Symptom 1 already illustrates) —
   deliberately left unimplemented rather than risk that regression under time pressure.
2. **`ApplicationServerService.start()` now genuinely runs (fix 2 above) and throws.** The
   exception's class/message aren't visible in current logging —
   `jboss_msc`'s error trace only prints `ExceptionThrown(ObjectRef { ptr: ... })`, not the
   exception's `getClass()`/`getMessage()`. This is now the actual gating defect on the path to
   real sustained WildFly execution (previously hidden entirely by the worker-pool race). Next
   session: decode the exception (extend the `tracing::error!` call in `drive_starts` to read
   `getClass().getName()` + `getMessage()` off the `ObjectRef`, or attach `gdb`) before further
   diagnosis.

## Why this matters beyond this one doc

Every open WildFly domain-mode timeout doc in `docs/known-issues/` (managed-servers-timeout,
heap-corrupt-value-timeout) ultimately depends on the boot reaching real, sustained,
multi-threaded application execution. Until this gate's remaining gaps are closed, nobody
can re-verify *any* fix in this area by hand-driving `standalone.sh`/`domain.sh` directly —
only the original Maven/Surefire/Arquillian harness (not available on the probe host used
here) reaches far enough, which is presumably how the 2026-07-05 evidence in those docs was
originally captured.

## 2026-07-07 Follow-up — the remaining next-steps from this doc are DONE

Branch `fix/wildfly-msc-realstart-boot-20260707`: the unidentified
`ApplicationServerService.start()` exception was identified (a
`StabilityMonitor.addController` ClassCastException on the synthetic mirror)
and fixed, `ServiceController.addListener`/`removeListener` are implemented
(with rest-state replay + transition events), the `demand()`/dependents/
`setMode` queue-race hazards are guarded, and eight further blockers behind
them were fixed (value plumbing, alias resolution, anonymous installs,
legacy getValue/Injector wiring, getState enum constants, getElapsedTime/
getStartException/getValue natives). Standalone boot now reaches subsystem
initialization with zero MSC service failures. Full list + the new proximate
blocker (a GC/STW cooperative-mutator stall, different bug class) in
`docs/known-issues/wildfly-domain-managed-servers-timeout.md` (2026-07-07
update).
