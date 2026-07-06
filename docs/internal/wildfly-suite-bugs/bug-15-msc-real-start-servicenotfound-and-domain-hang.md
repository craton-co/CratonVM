# Bug 15 - `CRATONVM_MSC_REAL_START` gate blocks any WildFly boot from reaching real service execution

Status: OPEN
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

## Why this matters beyond this one doc

Every open WildFly domain-mode timeout doc in `docs/known-issues/` (managed-servers-timeout,
heap-corrupt-value-timeout) ultimately depends on the boot reaching real, sustained,
multi-threaded application execution. Until this gate's remaining gaps are closed, nobody
can re-verify *any* fix in this area by hand-driving `standalone.sh`/`domain.sh` directly —
only the original Maven/Surefire/Arquillian harness (not available on the probe host used
here) reaches far enough, which is presumably how the 2026-07-05 evidence in those docs was
originally captured.
