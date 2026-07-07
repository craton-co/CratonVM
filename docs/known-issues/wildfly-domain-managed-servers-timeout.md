# WildFly domain managed servers do not reach started state

Status: OPEN (2026-07-07: the real Arquillian test now RUNS end-to-end under CratonVM nested processes — see the 2026-07-07 update at the bottom. Two distinct residuals isolated: no-JIT boots both servers to WFLYSRV0025 started but awaitServers never sees them (management-model propagation gap); JIT-on fails HC interface resolution WFLYSRV0082 (a JIT miscompile). Both new, both OPEN.)
Date found: 2026-07-05
Area: WildFly domain mode startup under CratonVM

## Symptom

After the Surefire protocol fixes, `org.jboss.as.test.integration.domain.DefaultConfigSmokeTestCase` now runs and reports two JUnit methods, but both fail while waiting for managed domain servers:

```text
java.lang.RuntimeException: Could not start container
Caused by: java.util.concurrent.TimeoutException: Managed servers were not started within [120] seconds
```

Failing methods:

```text
DefaultConfigSmokeTestCase.testStandardHost:49
DefaultConfigSmokeTestCase.testPrimaryAndSecondary:70
```

## Evidence

Focused no-JIT run:

```text
/data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner/out/azure-defaultconfig-surefireps2-nojit-078-nojit-real-failed-20260705-152923
```

Surefire report:

```text
surefire-reports/00001-org.jboss.as.test.integration.domain.DefaultConfigSmokeTestCase/org.jboss.as.test.integration.domain.DefaultConfigSmokeTestCase.txt
```

Summary:

```text
classes: FAIL=1
test-methods: found=2 passed=0 failed=0 errors=2
```

## Notes

This is the first real WildFly domain-mode failure exposed after fixing the MSC null-controller issue and Surefire channel corruption. The visible behavior is repeated management connection attempts to `remote+http://127.0.0.1:9990`, followed by `DomainLifecycleUtil.awaitServers` timing out.

Next investigation should collect host-controller/server process state and determine whether the managed server process fails to launch, launches but cannot bind/connect, or reaches a rolled-back service state that the management client cannot observe as started.

## 2026-07-05 Update

The first launch blocker behind this timeout was fixed in `docs/internal/wildfly-suite-bugs/bug-14-jboss-modules-multi-entry-modulepath.md`: CratonVM now searches all WildFly `-mp` entries. With `cratonvm-wildfly-nonpassed-20260705-035722-mpmulti2`, `EEConcurrencyExecutorShutdownTestCase` advances to a full 120-second timeout. The current visible blocker is the repeated `gen_heap::read_slot` corrupt `Value` guard tracked separately in `wildfly-domain-heap-corrupt-value-timeout.md`.

## 2026-07-06 Update — boot-infrastructure blocker found; corrupt-cell root cause still open

The corrupt-`Value`-cell mechanism tracked in
[wildfly-domain-heap-corrupt-value-timeout.md](wildfly-domain-heap-corrupt-value-timeout.md)
is under active investigation there — not yet root-caused. This session initially
suspected the plain-field 16-byte `Value`-slot tearing bug closed by commits `2dfdfddc`
and `5198fccd` (2026-07-06), but a parallel investigation the same day empirically and
theoretically **refuted** that hypothesis (a single Java field's discriminant word is
invariant across writes to that field, so same-field tearing can only corrupt the
*payload*, never produce the out-of-range *discriminant* the guard actually flags — see
the other doc's Hypothesis 1/2 discussion; the current best unconfirmed candidate is
`e60b7a5c`'s JIT `getfield` reference-oop mistagging fix).

This doc's own contribution is orthogonal to which hypothesis is right: while trying to
get a live process far enough to re-observe the corrupt-cell guard under sustained load,
this session found a **separate, deeper, boot-infrastructure blocker** that stops WildFly
from doing any real sustained concurrent work under CratonVM at all right now — see below.
No Maven/`wildfly-core` testsuite
checkout was available on the Azure probe host to rerun the actual Arquillian
`DefaultConfigSmokeTestCase`. As a substitute, a WildFly 32.0.1.Final binary distribution
(GitHub release, no Maven build needed) was driven directly via `bin/domain.sh` under a
fresh `dev`-HEAD build, with the same real-JDK/`--nojit` configuration
`apps/wildfly-suite-runner/run-suite.sh` uses. This surfaced a **new, deeper, proximate
blocker that now stops domain-mode boot before it ever reaches the point this doc's
`awaitServers` timeout is about**:

CratonVM only drives the real `org.jboss.msc.service.Service.start(StartContext)`
callback when `CRATONVM_MSC_REAL_START=1` is set (default OFF — see
`docs/internal/app-jvm-bugs/handoff-wildfly-msc-service-start.md`, an existing, explicitly
scoped, in-progress effort). `run-suite.sh` does not set this flag. Without it:

- Process Controller successfully spawns Host Controller as a real child process (through
  the same CratonVM binary, confirmed via `ps`/log correlation), which itself starts
  booting (module loading, `WFLYSRV0049 ... starting`, extension parsing) — but the very
  first application-level MSC service install after that point never signals
  "started" back to whatever is waiting on it, so the process just sits idle forever.
  Confirmed with `CRATONVM_DEFAULT_WATCHDOG_SEC` + the built-in stack-dump watchdog: all
  non-daemon threads are correctly parked in `EnhancedQueueExecutor$ThreadBody.run` at
  near-0% CPU — a genuine indefinite wait, not merely slow interpreted execution.
- With `CRATONVM_MSC_REAL_START=1`, standalone boot (the simpler, single-process mode)
  gets further — reaching real bytecode-driven service installs — but then hard-aborts on
  `org.jboss.msc.service.ServiceNotFoundException: service  not found` inside
  `BootstrapImpl.internalBootstrap` (disassembly confirms this is
  `ServiceContainer.getRequiredService(Services.JBOSS_AS)`, immediately after
  `ServiceTarget.addService(Services.JBOSS_AS, service).install()` two bytecode
  instructions earlier — either that legacy 2-arg `addService(...).install()` shape isn't
  reaching the same native install hook as the modern builder-pattern install, or the
  lookup side isn't keying on the same `ServiceName` identity). Domain mode with the same
  flag hangs even earlier than the default configuration, for the full window tried (no
  exception, no log progress).

Neither configuration reaches the sustained, real multi-threaded application execution
that would let a management client actually observe (or time out on) real managed-server
state, so this session could not directly re-trigger this doc's exact symptom.

**Updated diagnosis:** whichever mechanism turns out to explain the corrupt-`Value`-cell
diagnostic, the **actual gating blocker this session found** for "managed servers reach
started state" via a hand-driven boot is the MSC real-service-start completeness gap
above (tracked in `handoff-wildfly-msc-service-start.md`'s own P3/P4/P5 follow-ups), plus
whatever caused the original 2026-07-05 run to get further than this session's hand-driven
repro did (most likely the real Arquillian/Surefire harness configures the flag, or a
cut-down test `domain.xml`/`host.xml`, differently from a vanilla `domain.sh` launch).

**Recommended next steps, in order:**
1. Restore Maven + a `wildfly-core` testsuite checkout on a probe host so
   `DefaultConfigSmokeTestCase`/`EEConcurrencyExecutorShutdownTestCase` can be run directly
   again, rather than approximating with a hand-driven binary distribution.
2. ~~Fix the `ServiceNotFoundException` on `Services.JBOSS_AS` under
   `CRATONVM_MSC_REAL_START=1` (standalone mode)~~ — **FIXED 2026-07-06**, see update below.
3. Diagnose the domain-mode-specific hang under `CRATONVM_MSC_REAL_START=1` (no exception,
   no progress) — likely a lock-ordering or re-entrancy gap in the `drive_starts` loop
   specific to the host-controller's own service graph, per
   `handoff-wildfly-msc-service-start.md`'s "Value injection ... NOT wired" and async-
   services follow-ups.
4. Once boot reaches real sustained concurrent execution again, help resolve whichever
   corrupt-cell hypothesis is live in
   `wildfly-domain-heap-corrupt-value-timeout.md`, and confirm this doc's timeout is gone.

## 2026-07-06 Update — ServiceNotFoundException + a masking race FIXED; two new blockers found

Worktree `fix/wildfly-msc-getservice-registry-gap-20260706` off `dev` (Azure probe host),
re-driving the same hand-built WildFly 32.0.1.Final binary distribution + `bin/standalone.sh`
repro from the update above (`CRATONVM_MSC_REAL_START=1 CRATONVM_DISABLE_JIT=1`, real JDK 25
boot). Full detail in
[bug-15](../internal/wildfly-suite-bugs/bug-15-msc-real-start-servicenotfound-and-domain-hang.md).
Two root causes confirmed and fixed in `native-builtins/src/jboss_msc.rs`:

1. **`ServiceNotFoundException` (bug-15's own headline symptom) — FIXED.** The P2
   `ServiceBuilderImpl.install()` hook (`native_service_builder_install`) registers every
   installed service only in CratonVM's own Rust-side shadow container
   (`global_container()`/`service_roots()`) — it never touches `ServiceContainerImpl`'s real
   `registry` field (a real `ConcurrentMap<ServiceName, ServiceRegistrationImpl>`). Real,
   unintercepted `ServiceContainerImpl.getService`/`getRequiredService` bytecode reads
   *that* map, which install() never populated, so even a service installed a moment earlier
   was reported not-found. Fix: intercept `getService`/`getRequiredService` on the concrete
   `ServiceContainerImpl` class (not the `ServiceContainer` interface) and answer from the
   same shadow container `install()` populates.
   `LeakDetectorServiceContainer.getService`/`getRequiredService` (what `BootstrapImpl`
   actually calls) just forward to the real delegate via `invokeinterface`, so hooking only
   the concrete class is sufficient.
2. **A worker-pool race silently faked service starts — FIXED.** `ServiceContainer::add_service`
   unconditionally scheduled every newly-installed `Active`/`Passive` service onto a
   background-worker task queue *in addition to* the real, synchronous `drive_starts` loop
   that P2 relies on to invoke the actual `Service.start(StartContext)` callback. A background
   worker thread could win that race and "complete" the service via bookkeeping-only
   `run_start_local` (which has no real Java callback to invoke — a T19.1-era leftover),
   marking it `Up` **without ever running its real `start()` method**. Confirmed via
   `CRATONVM_DBG_MSC=1`: a `run_start_local` bookkeeping-only trace fired for the
   top-level `jboss.as` service *before* `install()`'s own trace even printed. Fixed by
   skipping that queue push when `CRATONVM_MSC_REAL_START` is set, so only `drive_starts`
   ever starts a service in that mode.

With both fixed, boot now correctly reaches `drive_starts`'s real `invoke_virtual(svc, "start", ...)`
call for `ApplicationServerService` — which throws a real (currently unidentified — our
`MethodCallFailed::ExceptionThrown` logging only prints the raw `ObjectRef`, not the exception's
class/message) exception. The container correctly marks the service `Failed` rather than faking
`Up` (a direct, visible improvement from fix #2 above: the race previously would have hidden this
failure entirely). Immediately behind that, `BootstrapImpl.internalBootstrap` still hits a **new**
blocker: `controller.addListener(...)` throws `AbstractMethodError` — `ServiceController` is a
real interface (`addListener`/`removeListener` are abstract, no default body) and no native hook
backs either method, so any code calling them on our synthetic mirror aborts. A correct
implementation isn't a trivial no-op: `BootstrapImpl`'s real listener chain
(`BootstrapImpl$1` → `getServiceContainer()` → `getRequiredService(JBOSS_SERVER_CONTROLLER)` →
another nested listener) expects real MSC's "fire past events immediately on `addListener`"
semantics, and by the time it's called the service may still be `Starting` (real MSC services
commonly call `StartContext.asynchronous()` and finish on a background thread) rather than
already `Up`/`Failed` — which our shadow container has no mechanism to notify later.

**Recommended next steps, in order:**
1. Identify the real exception `ApplicationServerService.start()` now throws (add exception
   class-name/`getMessage()` decoding to the `jboss_msc` error logging, or attach a debugger) —
   this is now the actual gating defect for reaching real sustained execution, previously
   masked by the fixed race.
2. Implement `ServiceController.addListener`/`removeListener` natives against the shadow
   container, including deferred notification for services that are still `Starting`
   (not just the already-terminal fast path) — see bug-15 for the exact real bytecode shape
   this needs to satisfy.
3. `ServiceContainer::demand()` (driven by `native_service_controller_set_mode`'s
   `Active`/`Passive` branch) schedules onto the same background-worker queue as the now-fixed
   `add_service` path and is structurally the same hazard — **not fixed this session** (not
   reproduced; `setMode` was never hit in this repro), but worth the same guard if/when a
   real-start path is found to exercise it.
4. Once boot reaches real sustained concurrent execution, resolve whichever corrupt-cell
   hypothesis is live in `wildfly-domain-heap-corrupt-value-timeout.md`, and confirm this
   doc's original `awaitServers` timeout is gone.

## 2026-07-07 Update — MSC real-start boot infrastructure cleared; standalone boot reaches subsystem initialization; new proximate blocker is a GC/STW cooperation stall

Branch `fix/wildfly-msc-realstart-boot-20260707` (Azure host, same WildFly
32.0.1.Final binary-distribution repro as above, `CRATONVM_MSC_REAL_START=1
CRATONVM_DISABLE_JIT=1`, real JDK 25). All three "recommended next steps"
from the update above are DONE, plus eight more blockers found and fixed
behind them (all in `native-builtins/src/jboss_msc.rs`; every behavioral
change is gated behind `CRATONVM_MSC_REAL_START` except the diagnostics):

1. **start()-failure logging decoded** (exception class + `detailMessage`,
   plus full `printStackTrace` under `CRATONVM_DBG_MSC`). The previously
   "unidentified exception" from `ApplicationServerService.start()` was
   `ClassCastException: ServiceController cannot be cast to
   ServiceControllerImpl` — real `StabilityMonitor.addController`
   (StabilityMonitor.java:115) downcasting our synthetic controller mirror.
2. **`StabilityMonitor.addController`/`removeController` → no-op natives.**
   Per-monitor membership is redundant: the `awaitStability` natives already
   answer from the global shadow container.
3. **`ServiceController.addListener`/`removeListener` implemented** with real
   MSC semantics: listeners stored + GC-rooted per service
   (`service_roots`), fired on Up/Failed transitions from the drive loop,
   and the rest-state event REPLAYED on `addListener`
   (`BootstrapImpl.internalBootstrap`'s completion chain requires the
   replay). Was `AbstractMethodError` on any call.
4. **`ServiceController.getStartException`/`getValue`,
   `LifecycleContext.getElapsedTime` natives** — same
   code-less-interface-method `AbstractMethodError` family, hit by
   `BootstrapImpl$1`'s FAILED branch and `BootstrapListener` boot timing.
5. **`getState` now returns the real `ServiceController$State` enum
   constant** (via `valueOf`; ordinal-Int fallback for mock contexts). The
   old Int return silently failed `WritableValueImpl.accept`'s
   `state == State.STARTING` reference compare — killing ALL modern value
   injection.
6. **P3 value plumbing wired** (`wire_provides_injectors`): `install()` now
   sets each provides-`WritableValueImpl.controller` to the mirror AND the
   per-name real `ServiceRegistrationImpl.injector` (via the target's real
   `getOrCreateRegistration`), exactly what real `install()`'s
   `registration.set(...)` does. Fixes both
   `IllegalStateException("Outside of Service lifecycle method")` (all
   `jboss.server.path.*` services) and `"Service is not installed"` on the
   requires side.
7. **Alias index for dependency resolution**: `requires(X)` is now satisfied
   by a service providing `X` under a different primary name (WildFly
   capability names — `org.wildfly.management.executor` etc.), matching real
   MSC's per-registration model. `can_start`/`get_id`/`getService` resolve
   through it.
8. **Anonymous installs no longer dropped**: `ServiceTarget.addService()`
   with no name (addressable only via `provides()`) previously bailed with
   "unreadable serviceId", silently losing whole services —
   `ControlledProcessStateService` (provides
   `org.wildfly.management.process-state-notifier`) was lost this way,
   permanently dep-blocking `console-availability` → `server-controller`.
   The first provided name is promoted to primary; the rest become aliases.
9. **`ServiceRegistrationImpl.getValue` intercepted** with full semantics:
   injector-wired (modern) providers delegate to the real
   `WritableValueImpl`; LEGACY 2-arg `addService(name, Service)` providers
   (the management executor, `ServerService$ServerExecutorService`) resolve
   through the shadow container to the legacy `Service.getValue()`. Real
   bytecode previously required `registration.instance` (never populated
   under interception) and threw `"Service is not installed"`.
10. **Legacy `addDependency(name, type, Injector)` injection wired**:
    `(dependency registration, Injector)` pairs captured at install
    (GC-rooted), values resolved and `Injector.inject(value)`d BEFORE
    `start()` runs (real MSC StartTask order) — `ServerService`'s
    `InjectedValue` fields (ServerService.java:272) read them during start.
11. **Task-queue fake-start race guards extended** to `demand()`,
    `schedule_dependents_of()` and `setMode`'s drain (same hazard class as
    the already-fixed `add_service` race: background `run_start_local` marks
    services Up without running the real `start()`). Demanded
    OnDemand/Lazy services start via the drive loop's scan (`demanded`
    flag) instead.

**Result:** `standalone.sh` boot goes from "`jboss.as` start() aborts within
~3s" to **31 services installed / 29 Up with zero service failures**, the
`ServerService Thread Pool` spinning up real worker threads, standalone.xml
parsed, and subsystem extensions initializing (Datasources, Transactions,
Weld, JSF, JAX-RS, Deployment Scanner, ...). This is exactly the "real
sustained concurrent execution" environment the 2026-07-06 update identified
as the prerequisite this whole doc family depends on.

**New proximate blocker (OPEN, different bug class):** ~75s into boot a
stop-the-world phase wedges — repeated
`STW cross-thread JIT takeover is still waiting for cooperative mutators
rounds=64 pending=1 taken=0` (vm/src/runtime/interpreter.rs) — one mutator
of the ~18 now-live threads never reaches a safepoint / GC-blocked state, so
the STW never completes and boot hangs until timeout (log:
`/tmp/wf-check9.log`, Azure host). Next session: catch it live, dump the
pending thread's stack (gdb + the lock-state-word technique from
`docs/internal/fixed-suite-bugs/class-manager-rwlock-recursive-read-deadlock-FIXED.md`),
and audit GC-blocked marking on JBoss Threads' `EnhancedQueueExecutor` park
paths.

Also still open: the corrupt-`Value`-cell diagnostic
([wildfly-domain-heap-corrupt-value-timeout.md](wildfly-domain-heap-corrupt-value-timeout.md)),
domain-mode-specific behavior under the flag, and the original
`DefaultConfigSmokeTestCase` verification (still needs Maven + a
`wildfly-core` testsuite checkout on a probe host).

## 2026-07-07 update — working end-to-end Arquillian harness established; DefaultConfigSmokeTestCase now RUNS under CratonVM nested processes; two new distinct residual blockers isolated

This is the first session to actually run the real Arquillian domain test with the
domain's own nested process-controller / host-controller / servers executing on
CratonVM (every prior session was blocked on infrastructure or on the MSC real-start
gate). Built on the 11 MSC real-start boot fixes landed earlier the same day
(`4b2508cf`, see `wildfly-domain-managed-servers-timeout`'s sibling work) plus the
three merged fixes from the corrupt-Value doc (`48b3c2d2` / `952f0093` /
`jit-instanceof-uaf`).

### Harness recipe (reusable — prior sessions could not get this far)

The critical detail is that the domain's nested processes do NOT inherit the outer
Surefire `-Djvm`; they are launched from `-Djboss.test.host.primary.jvmhome` /
`-Djboss.test.host.primary.controller.jvmhome` (read by
`DomainTestSupport`'s static init). Both must point at a directory whose `bin/java`
runs CratonVM, or the nested domain silently runs on the outer HotSpot and the result
is meaningless.

```bash
# Fork shim: a bin/java that re-execs a real JDK17 but sets CratonVM env, so the
# testsuite's own `-Djvm`/jvmhome selection lands on CratonVM for nested processes.
#   /data/data/forkjdk-msvt/bin/java :
#     #!/bin/bash
#     export JAVA_HOME=/data/data/fakejdk-msvt          # bin/java -> cratonvm binary
#     export CRATONVM_JAVA_HOME=/data/data/jdk25-real
#     # (add `export CRATONVM_DISABLE_JIT=1` for the no-JIT variant)
#     exec /usr/lib/jvm/java-17-openjdk-amd64/bin/java "$@"

# Invoke the already-extracted Maven distribution directly (the wrapper's bootstrap
# JVM hangs on the shared ~/.m2 cache under host load — see fifth-session note):
cd apps/wildfly/testsuite/domain
JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64 \
~/.m2/wrapper/dists/apache-maven-3.6.3-bin/*/apache-maven-3.6.3/bin/mvn -B -ntp \
  -Dsurefire.default-test.phase=test \
  -Dtest=DefaultConfigSmokeTestCase \
  -DfailIfNoTests=false -Dsurefire.failIfNoSpecifiedTests=false \
  "-Djvm=/data/data/forkjdk-msvt/bin/java" \
  "-Djboss.test.host.primary.jvmhome=/data/data/fakejdk-msvt" \
  "-Djboss.test.host.primary.controller.jvmhome=/data/data/fakejdk-msvt" \
  "-Djboss.test.host.primary.address=127.0.0.2" \
  "-Dtimeout.factor=800" \
  test
```

Notes: `-Djvm` must end in an actual `java` executable (Surefire validates the path);
bind the test to `127.0.0.2` to dodge the shared host's occupied `127.0.0.1:9990`;
`timeout.factor=800` scales the Arquillian `awaitServers` window (default 120s) so an
interpreted boot is not falsely timed out. A stray earlier run can leave orphaned
`[Host Controller]`/`[Server:...]` HotSpot processes squatting `:9990`/`:9999` — kill
any `etimes > 3600` ones first (`ps -eo pid,etimes,args | grep '\[Host Controller\]'`).

### Result: the test executes end-to-end. Both boot modes now reach real domain
### execution — and each surfaces a different, newly-isolated residual.

**No-JIT (`CRATONVM_DISABLE_JIT=1` nested):** the host-controller boots fully and BOTH
managed servers reach `WFLYSRV0025 ... started in ~3.9s - Started 307 of 584 services`
(confirmed in `target/domains/.../servers/server-{one,two}/log/server.log`). The test
nonetheless fails with the original `TimeoutException: Managed servers were not started
within [N] seconds` — i.e. the servers ARE started, but the piece the Arquillian
`DomainLifecycleUtil.awaitServers` polls (the host-controller's view of managed-server
"started" state, via its management model / server-registration handshake) never
reports them as started to the domain client. This is a NEW, distinct residual: it is
NOT interface resolution (that passes here), NOT the `HIB-CV-32` corrupt-`Value` guard
(which did not fire at all — see the sibling doc's 2026-07-07 update), and NOT the MSC
real-start gate (fixed). Next step: instrument/trace the HC-side
`ServerRegistrationService` / domain-controller server-status propagation, or capture
the domain client's `read-attribute(server-state)` responses, to see why `started`
never propagates even though the server process logged it.

**JIT-on (default):** boot fails EARLIER, at ~311s, with `WFLYSRV0082: failed to
resolve interface management` (management + public), rolling the HC back to
`WFLYHC0034` abort. This does NOT reproduce under no-JIT (which resolves the same
`127.0.0.x` interface config fine), so it is a JIT miscompile on the interface-
resolution path. Ruled out this session: it is NOT `NetworkInterfaceService.
resolveInterface` itself — a standalone reflective probe calling that exact method in
an 8000-iteration hot loop (enough to trigger JIT) resolved 8000/8000 on the JIT
binary; and the `getAll()` NetworkInterface native correctly returns one loopback
interface (confirmed via a `CRATONVM_DBG_NETIF` trace: `[netif] getAll: ctor ok=true ->
returning 1 interface`). The miscompile is therefore somewhere in the XML-model →
`InterfaceCriteria` construction / expression-resolution path
(`${jboss.bind.address.management:127.0.0.1}` → `ModelNode` → criteria), not the
resolution loop the probe exercised — not yet pinned to a method. A
`CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_SKIP` bisect over the interface-model
handlers is the next step.

### Status

Both blockers are real and newly isolated, but neither is fixed, so this doc stays
OPEN. The concrete, durable gain this session is the harness above plus the narrowing:
the domain now boots far enough under CratonVM that the failure is a specific
management-model/JIT issue rather than the "never reaches sustained execution" wall
every prior session hit.
