# WildFly domain managed servers do not reach started state

Status: FIXED (2026-07-09: stock WildFly 32.0.1 domain mode reaches Host Controller start and both managed servers start/connect/register under the clean `probe92` CratonVM build; no remaining `NoSuchMethodError`, CCE/NPE, constructor OOB, native watchdog `pending=1/taken=0`, or fatal signatures.)
Date found: 2026-07-05
Area: WildFly domain mode startup under CratonVM

## 2026-07-09 Resolution - managed servers reach started/registered state

Final probe branch/build: `codex/wildfly-close-socket-inventory-20260708-202526`, binaries:

```text
/data/data/bin/java-wildfly-close-socket-inventory-20260708-202526-probe92
/data/data/bin/cratonvm-wildfly-close-socket-inventory-20260708-202526-probe92
/data/data/cratonvm-javahome-wildfly-close-socket-inventory-20260708-202526-probe92
```

Final root causes closed in the last residual pass:

1. `sun.reflect.ReflectionFactory` / `jdk.internal.reflect.ReflectionFactory` serialization helpers were registered in the essential native set and force-routed over the real JDK bytecode so `readObjectForSerialization`, `writeObjectForSerialization`, `readResolveForSerialization`, `writeReplaceForSerialization`, and serialization constructors return HotSpot-shaped null/MethodHandle/Constructor results.
2. Real-layout `java.lang.reflect.Constructor` mirrors no longer read CratonVM extra metadata slots past `object_num_fields`; descriptor/parameter/accessibility reads now fall back to standard JDK fields or side-table metadata instead of producing the slot-15 constructor OOB seen immediately before the managed-server `Object.readObject` failure.
3. Synthetic MethodHandle dispatch now neutralizes missing serialization hooks across static, special, and virtual dispatch paths, so a stale hook handle cannot escape as `java/lang/Object.readObject(ObjectInputStream)` and abort JBoss Marshalling during `DomainServerMain`.

Verification:

```text
probe: /tmp/wildfly-close-socket-inventory-20260708-202526-probe92-clean-1783614658
root_alive: yes at +520s
server-one: root service started, WFLYHC0021 connected, WFLYHC0020 registered
server-two: root service started, WFLYHC0021 connected, WFLYHC0020 registered
Host Controller: WFLYSRV0025 started
NoSuchMethodError: 0
ClassCastException: 0
NullPointerException: 0
gen_heap::get_field OOB: 0
pending=1 / taken=0 watchdog signature: 0 / 0
FATAL: 0
```

A diagnostic run immediately before the clean run (`probe92-rfdbg-1783613991`) produced 725 `[rf-ser]` decisions, showed the ReflectionFactory route returning null for absent hooks and real handles for private hooks, and also reached both managed-server registration points without the previous failures.

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
[bug-15](bug-15-msc-real-start-servicenotfound-and-domain-hang.md).
Two root causes confirmed and fixed in `../../../../native-builtins/src/jboss_msc.rs`:

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
behind them (all in `../../../../native-builtins/src/jboss_msc.rs`; every behavioral
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
`../class-manager-rwlock-recursive-read-deadlock-FIXED.md`),
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

## 2026-07-07 update (seventh session) — the standalone-boot blocker is pinned to a specific STW accounting stall (BUG-03 family); precise census captured

The 2026-07-07 sixth-session update above noted a "GC/STW cooperative-mutator
stall ~75s into standalone boot". This session reproduced it under a clean
JIT-on standalone boot (WildFly 32.0.1.Final binary dist, `bin/standalone.sh
-b=127.0.0.2 -bmanagement=127.0.0.2`, real JDK 25, `CRATONVM_MSC_REAL_START`
NOT needed — this is the plain standalone path) and pinned the exact
accounting via `CRATONVM_DBG_STW_CENSUS=1` + two independent live `gdb`
captures.

**Precise signature (reproducible):**

```text
[stw-request] initiator=11 alive=20 blocked=14 expected=5
[stw-census]  rounds=64 pending=1 taken=0 blocked=14 alive=20
```

- `expected = alive - 1(initiator) - blocked = 20 - 1 - 14 = 5`.
- `arrived = expected - pending = 4`. So of the 5 counted (non-blocked,
  non-initiator) mutators, 4 reach the JIT-takeover safepoint and **one never
  does** — the STW `wait_for_all_timeout` loop in
  `stw_take_over_and_wait` (`../../../../vm/src/runtime/interpreter.rs`) then spins
  forever (rounds keep climbing past 64; the process sits at ~0-2% CPU for the
  whole timeout window — confirmed genuinely stalled, not slow).
- `taken=0`: the cross-thread takeover froze **zero** in-JIT peers, i.e.
  `conservative_roots::any_thread_in_jit()` reported no thread currently
  executing JIT machine code, so `take_over_pass` never excused anyone. The
  pending mutator is therefore NOT caught by the in-JIT takeover path.

**Every thread is parked at the stall.** Two full `gdb -p <pid> -batch -ex
'thread apply all bt'` captures (one 22-thread early-boot, one 96-thread
deep-boot) show **no thread spinning in JIT or interpreter code** — the top
frame of every thread is either `__futex_abstimed_wait_common64` (parking_lot
park, ~58/96 in the deep capture) or `epoll_wait` (XNIO NIO I/O threads,
~38/96). So the one `pending` mutator is *parked in a native* (a
`LockSupport.park`/`parkNanos`, a `Selector`/`epoll_wait`, or an executor idle
park) yet is still counted in the barrier's `expected` set (`blocked=false` in
`debug_thread_census`), i.e. it entered that native block WITHOUT going through
the `gc_barrier` blocked-region protocol (`enter_blocked` /
`mark_blocked_region_enter`). The census breadcrumbs for the non-blocked
mutators point at `org/jboss/threads/EnhancedQueueExecutor$ThreadBody.run@442`
(the JBoss Threads worker idle-park) and one `java/io/FileInputStream.read`
(these frame_traces are last-seen breadcrumbs and can be stale, so treat as a
lead, not proof).

**This is the BUG-03 family** ("cross-thread STW JIT root scan INSUFFICIENT",
see `..` / memory `bug03-cross-thread-jit-root-scan-insufficient`):
a mutator that neither cooperatively reaches an interpreter safepoint nor is
detected as in-JIT stalls the STW barrier. What is new and useful here is a
**clean, deterministic repro** (WildFly standalone boot under JIT reliably
wedges; far simpler than the app/ForkJoin workloads BUG-03 was originally
chased with) plus the exact census numbers isolating it to **one** non-blocked
mutator parked in a native, with `taken=0` proving the in-JIT takeover is not
the mechanism that would rescue it.

**Live `vm_state` narrows the mechanism (the harder half of BUG-03).** A second
run with `CRATONVM_DBG_STW_CENSUS=1 CRATONVM_DBG_VM_STATE=1` printed the pending
population (`expected=7 blocked=11 alive=19`, one pending) with live states: the
non-blocked mutators are the JBoss-Threads `EnhancedQueueExecutor` workers, all
showing `state="native:return"` (`vm/src/vm/vm_exec.rs:615` — the breadcrumb set
immediately after a native callback returns, before the next native call). Yet
in `gdb` these very threads are parked in `__futex_abstimed_wait_common64` /
`epoll_wait`. The reconciliation: they parked *after* their last real native
returned, via a path that updated neither the `gc_barrier` blocked accounting
(`blocked=false` — so still counted in `expected`) nor the interpreter
`vm_state` (stale `native:return`). That is a **park that skips
`enter_blocked`** — i.e. NOT `NativeContextImpl::park`/`monitor_wait` (both of
which set `blocked=true`), but a JIT-compiled executor idle-park (or an internal
`parking_lot` wait) whose `Rip` lands in libc/futex, not in a registered JIT
code range.

This is precisely the scenario `../../../../vm/src/jit/xt_root_scan.rs` (lines ~105-118)
calls out: *"A peer that is blocked (or parked) with its `Rip` in Rust/native
code can still have live JIT frames on its native stack."* The helper-window
scan there recovers such a thread's **roots** after the barrier, but nothing
lets the **barrier itself complete**: the thread is counted in `expected`,
`take_over_pass` cannot freeze it (its `Rip` is not inside a JIT range, so the
forcible-takeover pass skips it → `taken=0`), and it never cooperatively
arrives → infinite spin in `stw_take_over_and_wait`.

**Recommended next step (deliberately NOT attempted this session — this is the
harder, still-open half of BUG-03: deep GC-barrier/takeover work with high
regression risk, and the host was too SSH-unstable to iterate a GC-internals
change safely):** the fix must make a JIT-thread parked-in-native either (a)
register as `gc_barrier`-blocked at the JIT→park boundary (so it is excluded
from `expected`, the same way `NativeContextImpl::park` excludes an
interpreter park), or (b) be *excused* from the barrier by the takeover the way
a frozen in-JIT peer is (identity-matched against `counted_os_tids`), since it
holds live JIT frames but cannot be frozen at its libc `Rip`. Option (a) is
cleaner but requires the JIT's park/blocking-call lowering to go through the
blocked-region hook; option (b) is a `stw_take_over_and_wait` change to treat
"counted mutator, parked with JIT frames, un-freezable" as excused rather than
awaited. First confirm which park the executor worker actually uses
(`Unsafe.park` is intrinsified in the JIT vs. calling `native_unsafe_park` →
`ctx.park` → `enter_blocked`; the observed `blocked=false` proves the executor
is NOT reaching `ctx.park`, so the JIT is either intrinsifying the park or the
wait is on an internal `parking_lot` primitive).

This is the current gating blocker for a JIT-on WildFly standalone (and, by
extension, the domain servers/host-controller, which are standalone-shaped
JIT-on boots). It is distinct from — and now more clearly separated than — the
no-JIT `awaitServers` propagation gap and the JIT-on domain interface-
resolution miscompile documented above.

## 2026-07-07 update (eighth session) — a THIRD, distinct WFLYSRV0082 defect found in `bin/domain.sh`'s stock config; corrects scope of the earlier JIT-miscompile note; JIT bisect attempted, did not converge

Follow-up on the sixth-session finding ("JIT-on: HC interface resolution
fails with WFLYSRV0082 ... does NOT reproduce under no-JIT"). That
observation was made entirely through the **Arquillian testsuite harness**,
whose `DomainTestSupport`-generated `host.xml`/`domain.xml` use **literal**
`<inet-address value="127.0.0.2"/>` addresses (no `${...}` expression
syntax). This session tested the **stock** `bin/domain.sh`/`bin/standalone.sh`
config instead (`<inet-address value="${jboss.bind.address.management:127.0.0.1}"/>`)
and found a **third, separate** interface-resolution defect that reproduces
identically regardless of JIT state or bind address:

```text
[Host Controller] DEBUG [org.jboss.as.server.net] Starting NetworkInterfaceService
[Host Controller] ERROR [org.jboss.as.controller.management-operation] WFLYCTL0013: Operation ("add") failed - address: (["host"=>"primary","core-service"=>"management","management-interface"=>"http-interface"])
  - failure description: {"WFLYCTL0412: Required services that are not installed:" => [""], "WFLYCTL0180: ... => ["service  is missing []"]}
[Host Controller] ERROR ... address: (["host"=>"primary","interface"=>"management"])
  - failure description: {"WFLYCTL0080: Failed services" => {"" => "WFLYSRV0082: failed to resolve interface management"}}
```

Reproduced with `bin/domain.sh` (both default `127.0.0.1` binding and
explicit `-bmanagement=127.0.0.2`) under **both** JIT-on and
`CRATONVM_DISABLE_JIT=1` — same failure every time, ruling out JIT and the
specific bind address as factors for THIS variant.

**What was ruled out this session, with direct probes:**
- `ModelNode` expression resolution (`${jboss.bind.address.management:127.0.0.1}`)
  resolves correctly to `"127.0.0.1"` on both HotSpot and CratonVM — confirmed
  via a standalone `org.jboss.dmr.ModelNode.resolve()` probe.
- `ServiceName.toString()`/`.equals()`/`.hashCode()` all work correctly
  against CratonVM's synthetic `ServiceName` mirror (which unconditionally
  intercepts `of`/`append`/`getCanonicalName`/`getParent` — NOT gated behind
  `CRATONVM_MSC_REAL_START`, so this runs on every boot). A direct probe
  (`ServiceName.of("jboss","network","interface","management")`) round-trips
  `toString()`/`equals()` identically on both VMs.
- `NetworkInterfaceService.resolveInterface(OverallInterfaceCriteria)`
  invoked directly via reflection with the exact same JVM flags as the
  Host Controller resolves fine on both VMs (confirmed in the sixth-session
  update above).

**Not yet found:** the `"service  is missing []"` (two spaces = an empty
`ServiceName`, `[]` = an empty dependency list) means some REAL MSC service
registration during boot is keyed by a genuinely empty-segment `ServiceName`
— i.e. a real value that should have carried a service reference (most
likely the `http-interface` management-interface's dependency on the
"management" `NetworkInterfaceService`, wired via the model's
`<socket interface="management" .../>` attribute) collapsed to zero segments
somewhere in the real `org.jboss.as.controller`/model-processing bytecode
between reading that attribute and constructing the dependency's
`ServiceName`. This is NOT a `ServiceName`-machinery bug (ruled out above);
it must be upstream, in whatever code builds a composite `ServiceName` from
a model attribute value during capability/socket-binding resolution. Not
pinned to a specific class/method this session.

**JIT bisect on the testsuite's own WFLYSRV0082 (literal-address config,
the ORIGINAL sixth-session finding) — attempted, did not converge.** Ran
`CRATONVM_JIT_DENY=org/jboss/as/controller/interfaces/,org/jboss/dmr/,java/net/`
against `DefaultConfigSmokeTestCase#testStandardHost` under the full E2E
harness. The run silently died with zero output past the JUnit test-class
header — no `BUILD SUCCESS`/`FAILURE`, no crash dump, no nested JVM left
alive — most likely because denying JIT wholesale for `org/jboss/dmr/`
(an extremely hot-path package touched by nearly every model operation) is
not a safe bisection axis by itself, or coincided with host instability
(this session's Azure host had frequent SSH connection resets and at least
one instance of a detached background launch dying without `disown -a`).
Not re-attempted after two consecutive silent deaths — this needs a
narrower, single-class `CRATONVM_JIT_BISECT_SKIP=Class.method` bisect
(rather than whole-package `CRATONVM_JIT_DENY`) on a quieter host, or a
live `gdb`/deopt-trace capture of the actual HC process at the moment of
its `WFLYSRV0082` failure under the testsuite's literal-address config,
which was never attempted directly (only the domain.sh variant was
deep-probed this session).

**Status:** three distinct, real, reproducible `WFLYSRV0082`-shaped
defects are now known across this doc pair:
1. Testsuite literal-address config, JIT-on only (sixth session) — cause
   still unknown, JIT-implicated but not yet isolated to a method.
2. `bin/domain.sh`/`bin/standalone.sh` stock expression-config, BOTH JIT
   states, BOTH default and explicit bind addresses (this session) — cause
   narrowed to an empty-`ServiceName` dependency, not yet pinned to the
   exact model-attribute-read/ServiceName-construction site.
3. (Unconfirmed whether #1 and #2 share a root cause — the testsuite's
   no-JIT PASS on its own literal-address config is the strongest evidence
   they're different, since #2 fails under no-JIT too.)

Both docs remain OPEN. No fix landed this session; this is a documentation
and scoping pass only, to prevent a future session from re-treading the
same ground or conflating these three distinct failure modes.

## 2026-07-08 update — logging follow-up residual fixed; XNIO native blocking hardened; broader STW stall remains OPEN

Branch `codex/fix-wildfly-stw-park-20260708-165032` on the Azure probe host, using a separate worktree from `dev` and uniquely named binaries:

```text
/data/data/cratonvm-builtins/cratonvm-wildfly-residuals-20260708-171950
/data/data/cratonvm-builtins/java-wildfly-residuals-20260708-171950
/data/data/fakejdk-wildfly-residuals-20260708-171950/bin/java
```

Two concrete fixes landed in this pass:

1. **`String.getCanonicalName()` residual fixed.** The immediate post-LogManager WildFly boot failure was not a missing JDK API. `ContextNames$BindInfo.getBinderServiceName()` was returning a synthetic field populated with a `String`, while real WildFly expects `org.jboss.msc.service.ServiceName`. The naming bridge now allocates the real four-field `BindInfo` shape and stores `ServiceName` mirrors for both parent and binder service names. See the fixed note in `wildfly-bindinfo-binder-servicename.md`.
2. **XNIO registered native selector threads now publish GC-blocked state.** A new `NativeThreadBlocker` hook lets native-spawned carrier threads publish their OS tid and bracket host-native waits. `xnio_io_thread::run_io_loop_with_blocker` wraps `selector.select(timeout)` so XNIO `epoll`/selector waits are excluded from STW expected-count accounting and visible as blocked in the thread registry.

Validation performed:

```text
cargo test -p cratonvm-native-builtins t19_2_b_context_names_bind_info_parses_absolute_name -- --nocapture
cargo test -p cratonvm-native-builtins t19_2_b_service_based_naming_store_registers_msc_service -- --nocapture
cargo check -p cratonvm-native-api -p cratonvm-native-builtins -p cratonvm-vm
cargo build --release -p cratonvm-cli --features java-bin-alias --bin cratonvm --bin java
```

The direct WildFly `standalone.sh` probe with the rebuilt fake JDK confirms the `String.getCanonicalName` error is gone and boot progresses further. New visible failures before the remaining STW stall are:

```text
WFLYCTL0158: Operation handler failed: java.lang.NullPointerException: Cannot invoke "org.jboss.modules.ModuleLoader.loadModule(org.jboss.modules.ModuleIdentifier)"
WFLYCTL0158: Operation handler failed: java.lang.NullPointerException: Method parameter cannot be null
```

The STW residual is **not fixed**. The rebuilt JIT-on standalone probe still times out with:

```text
[stw-census] rounds=64 pending=1 taken=0 blocked=67 alive=76
```

The pending thread in that run was a non-blocked `ParallelBootOperationStepHandler$ParallelBootTransactionControl.operationPrepared` worker (`t53`), while a follow-up probe with `CRATONVM_JIT_BISECT_SKIP='org/jboss/threads/EnhancedQueueExecutor$ThreadBody.run'` made idle `ThreadBody.run@442` workers consistently blocked but still wedged later with a non-blocked `operationPrepared` worker (`t38`). That rules out a single `EnhancedQueueExecutor$ThreadBody.run` JIT skip as a complete fix. The next pass should focus on the Java/AQS/Future wait path used by `operationPrepared` and the two new functional boot NPEs above.

Status remains OPEN: the fixed `BindInfo` bug is closed under `..`; this doc continues to track the broader WildFly domain/standalone boot residuals.
## 2026-07-08 update - stock-config ServiceName, loopback, and Undertow parser blockers fixed

Branch `codex/wildfly-residuals-20260708-164820` targeted the concrete stock-config
`bin/domain.sh` residual from the eighth-session update where the management-interface
failure was keyed by an empty MSC service name (`service  is missing []`). Three separate
CratonVM gaps were found behind that symptom:

1. **JBoss MSC `ServiceName` mirrors were incomplete - FIXED.** The native surface covered
   `ServiceName.of(String...)` but missed the real overloads
   `ServiceName.of(ServiceName,String...)`, `append(String...)`, and `append(ServiceName)`
   while also registering a non-real `append(String)` descriptor. Native-created mirrors
   also left the real `parent` field null and cached a canonical-string hash instead of
   JBoss MSC's segment hash. Real WildFly bytecode reads `name`, `parent`, and `hashCode`
   directly when composing/looking up services, so mixed native/bytecode composition could
   collapse or compare incorrectly. The fix populates the parent chain recursively, uses the
   Java/JBoss segment hash, registers the real overloads, and makes `getCanonicalName` /
   `getParent` robust for real `ServiceName.JBOSS` objects with lazy canonical fields.

2. **`NetworkInterface.isLoopback0("lo", 1)` always returned false - FIXED.** A focused
   HotSpot/CratonVM probe using the stock expression model
   `${jboss.bind.address.management:127.0.0.1}` showed both VMs parse the criterion as
   `LoopbackAddressInterfaceCriteria(address=127.0.0.3)`, but CratonVM returned
   `acceptableSize=0` because its low-level `java.net.NetworkInterface.isLoopback0`
   native answered false for the synthetic loopback interface. HotSpot accepts `lo` and
   returns `/127.0.0.3`. The fix reports loopback for `name == "lo"` or index `1`.

3. **Undertow parser constants were native-nooped - FIXED.** `HttpRequestParser` reflects
   over `Headers`, `Methods`, and `Protocols` and calls `HttpString.toString()` on each
   static `HttpString`. CratonVM's Undertow shim no-oped `Headers.<clinit>`, leaving those
   reflected values null. The fix lets real `Headers.<clinit>` populate the constants and
   teaches `HttpString.toString()` to handle both CratonVM's old synthetic one-slot layout
   and Undertow's real `bytes`/`string` layout.

Validation before the loopback fix showed the ServiceName change removed the previous empty
name failure: the stock no-JIT `domain.sh` repro with
`java-wildfly-residuals-20260708-164820-svcname` no longer printed
`service  is missing []`; the remaining failure was keyed by the real service name
`jboss.network.management`. The interface-criteria probe then isolated and fixed the
next layer.

A rebuild with `java-wildfly-residuals-20260708-164820-svcname-netif` confirmed
`NetworkInterfaceService matched interface binding` and no longer reported
`WFLYSRV0082`. That exposed a later Undertow parser blocker:
`HttpRequestParser$$generated.<clinit>` failed because `Headers.<clinit>` had been
native-nooped, leaving reflected static `HttpString` constants null. The fix lets real
`Headers.<clinit>` run and makes `HttpString.toString()` understand both CratonVM's old
synthetic one-slot layout and Undertow's real `bytes`/`string` layout. A focused
`HttpRequestParser.httpStrings()` probe now matches HotSpot (`size=144`, including
`Host`, `GET`, and `HTTP/1.1`).

A final stock no-JIT `domain.sh` rerun with
`java-wildfly-residuals-20260708-164820-svcname-netif-undertow` advanced past both fixed
layers. Current stock-domain boundary is now:

```text
NetworkInterfaceService matched interface binding
WFLYSRV0083: Failed to start the http-interface service
Caused by: java.lang.NullPointerException: Cannot invoke "org.jboss.modules.ModuleLoader.loadModule(org.jboss.modules.ModuleIdentifier)" because "moduleLoader" is null
```

This document remains OPEN until the Arquillian no-JIT `awaitServers` propagation gap,
the JIT-on literal-address WFLYSRV0082, and this newly exposed stock management-HTTP
module-loader wiring failure are closed.

## 2026-07-08 update -- ModuleLoader/capability ServiceName null residuals fixed; STW stall remains OPEN

Branch `codex/fix-wildfly-null-residuals-20260708-175605` on the Azure probe host, using a separate worktree from `dev` and uniquely named binaries:

```text
/data/data/cratonvm-builtins/cratonvm-wildfly-20260708-175605-nullres
/data/data/cratonvm-builtins/java-wildfly-20260708-175605-nullres
/data/data/fakejdk-wildfly-20260708-175605-nullres/bin/java
```

Two functional residuals from the previous 2026-07-08 pass are now fixed:

1. **Datasource `ModuleLoader.loadModule(ModuleIdentifier)` null receiver -- FIXED.** `JdbcDriverAdd.performRuntime` calls `Module.getCallerModuleLoader().loadModule(identifier)`. CratonVM already handled the `loadModule(...)` side, but did not provide `Module.getCallerModuleLoader()` / `Module.getBootModuleLoader()`. The module bridge now returns the existing boot `LocalModuleLoader` for both methods.
2. **Infinispan `ServiceBuilderImpl.requires(null)` / "Method parameter cannot be null" -- FIXED.** `XAResourceRecoveryServiceConfigurator.configure()` obtains `org.wildfly.transactions.xa-resource-recovery-registry` via `OperationContext.getCapabilityServiceName(name, type)`. WildFly's real `OperationContextImpl` falls back to parsing the capability name as an MSC `ServiceName` when registry lookup is unavailable; under CratonVM the under-modeled registry path could return null instead. The WildFly core bridge now mirrors that fallback for the `getCapabilityServiceName(...)` overloads and appends dynamic parts where applicable.

Focused validation:

```text
cargo test -p cratonvm-native-builtins t19_h4_get_caller_module_loader_returns_boot_loader -- --nocapture
cargo test -p cratonvm-native-builtins t19_2_a_operation_context_capability_name -- --nocapture
cargo check -p cratonvm-native-api -p cratonvm-native-builtins -p cratonvm-vm
cargo build --release -p cratonvm-cli --features java-bin-alias --bin cratonvm --bin java
```

The follow-up direct WildFly standalone probe no longer reports either null residual. The process still times out with the pre-existing STW cooperation stall:

```text
[stw-census] rounds=64 pending=1 taken=0 blocked=48 alive=53
```

The pending/nonblocked threads remain in the `EnhancedQueueExecutor$ThreadBody.run` / `ParallelBootOperationStepHandler` cooperation family. Status remains OPEN for the broader WildFly boot timeout; the two null residuals from this section are closed under `wildfly-moduleloader-capability-servicename-nulls.md`.

## 2026-07-08 update - XNIO accept server and module aliases fixed; stock-domain boundary reduced to process-controller STW/native watchdog

Branch `codex/wildfly-moduleloader-20260708-150444` on the Azure probe host, using a separate worktree from `/data/data/cratonvm`:

```text
/data/data/codex-wildfly-moduleloader-20260708-150444
```

Unique rebuilt binaries:

```text
/data/data/bin/java-wildfly-moduleloader-20260708-150444-callerloader
/data/data/bin/cratonvm-wildfly-moduleloader-20260708-150444-callerloader
```

Two additional stock-domain blockers exposed after the caller-loader fix were closed in this pass, and the accessor coverage was extended to `Module.getContextModuleLoader()`:

1. **JBoss Modules caller/context loader accessors - FIXED/EXTENDED.** The rebased `dev` branch already fixes `Module.getBootModuleLoader()` / `getCallerModuleLoader()` for the datasource null receiver. This branch keeps that helper and registers the same boot `LocalModuleLoader` for `Module.getContextModuleLoader()` as well, matching the rest of the synthetic JBoss Modules bridge.
2. **XNIO `createTcpConnectionServer` - FIXED for the management HTTP boot path.** Once the caller-loader NPE was gone, stock `domain.sh` reached `XNIO000900: Method 'createTcpConnectionServer' is not supported on this implementation`. CratonVM now provides focused natives for `XnioWorker.createTcpConnectionServer` and `NioXnioWorker.createTcpConnectionServer`, binds a real Rust `TcpListener` for the requested `InetSocketAddress`, stores it in a registry, and returns a synthetic `AcceptingChannel` mirror with the channel methods WildFly/XNIO expects during boot (`getAcceptSetter`, `resumeAccepts`, `getLocalAddress`, `close`, option probes, worker/thread accessors, and related no-op await/wakeup paths).
3. **JBoss Modules `module-alias` resolution - FIXED.** After the XNIO bridge, host boot failed parsing the stock domain because `org.jboss.as.modcluster` has no direct service descriptor: it is a `module-alias` to `org.wildfly.extension.mod_cluster`. The `module.xml` parser now captures `target-name`, and module resolution follows aliases before collecting resources/service providers. A real WildFly distribution regression test now asserts that `org.jboss.as.modcluster` exposes `org.wildfly.extension.mod_cluster.ModClusterExtension` for `../../../../apps/META-INF/services/org.jboss.as.controller.Extension`.

Validation:

```text
cargo test -p cratonvm-native-builtins wf_domain_static_module_loader_accessors_return_boot_loader -- --nocapture
cargo test -p cratonvm-native-builtins t19_h4_register_jboss_module_loader_adds_surface -- --nocapture
cargo test -p cratonvm-native-builtins xnio_worker::tests -- --nocapture
cargo test -p cratonvm-native-builtins parses_module_alias_target_name -- --nocapture
cargo test -p cratonvm-native-builtins wf_domain_resolve_module_alias_follows_target_name -- --nocapture
cargo test -p cratonvm-native-builtins wildfly_jboss_modules_service_provider_leak_real_dist_scoping -- --nocapture
cargo check -p cratonvm-classloading -p cratonvm-native-builtins
cargo build --release -p cratonvm-cli --features java-bin-alias --bins
```

Stock WildFly 32.0.1.Final `bin/domain.sh` was rerun no-JIT against fresh domain bases using the rebuilt `java` shim. The old signatures are gone: no `moduleLoader` NPE, no `XNIO000900`, no `WFLYCTL0153`, and no missing `../../../../apps/META-INF/services` for modcluster. A pre-rebase run reached both the earlier network-interface success and the alias-dependent extension success:

```text
NetworkInterfaceService matched interface binding
Final response for step handler ... handling add in address [("extension" => "org.jboss.as.modcluster")] is {"outcome" => "success"}
```

After rebasing onto current `origin/dev` and rebuilding the same unique binaries, the final integrated probe (`/tmp/wildfly-domain-moduleloader-final-20260708-150444-nojit-ring/domain-run.log`) still avoids the old signatures and reaches host-controller extension initialization (`Initializing Connector Extension`, `Activating Weld Extension`, `Initializing ResourceAdapters Extension`) before the process-controller CratonVM process hangs silently and its watchdog aborts. With `CRATONVM_ENABLE_NATIVE_RING=1 CRATONVM_DEFAULT_WATCHDOG_SEC=90`:

```text
STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1 taken=0
=== T19.H1 watchdog: deadline of 90s elapsed; requesting thread stack dumps ===
=== T19.H1 watchdog: 0 thread(s) dumped; aborting process ===
=== T19.H1 watchdog: no Java threads responded -- main thread is in native (Rust) code. pid=3972727. ===
```

The native-call/dispatch ring for that watchdog run is dominated by the process controller's `org.jboss.as.process.ManagedProcess$ReadTask.run()` loop reading child output via `sun/nio/cs/StreamDecoder.read`, `java/io/FileInputStream.readBytes`, and `FileInputStream.available0`, then writing it through `OutputStreamWriter`/`PrintStream`. This run also prints the existing `ReentrantReadWriteLock$NonfairSync` CAS retry diagnostics, but the durable reduced boundary is the process-controller STW/native watchdog hang after the management-interface and extension-initialization layers.

Status remains OPEN: this pass closed the newly exposed stock management-HTTP XNIO/module-alias layers, but did not close the no-JIT Arquillian `awaitServers` propagation gap, the JIT-on literal-address WFLYSRV0082, or the broader STW/native watchdog residual now exposed by stock `domain.sh`.

## 2026-07-08 update - Process.waitFor STW/native gap fixed; new stock-domain boundary is start-servers socket inventory

Branch `codex/wildfly-stw-managedprocess-20260708-194254` on the Azure probe host, using a separate worktree from `/data/data/cratonvm`:

```text
/data/data/codex-wildfly-stw-managedprocess-20260708-194254
```

Unique rebuilt binaries:

```text
/data/data/bin/java-wildfly-stw-managedprocess-20260708-194254
/data/data/bin/cratonvm-wildfly-stw-managedprocess-20260708-194254
```

The previous reduced boundary was confirmed in a stock WildFly 32.0.1.Final no-JIT
`bin/domain.sh` run with STW census/native-ring diagnostics enabled. The Process
Controller hit:

```text
[stw-request] initiator=3 alive=8 blocked=6 expected=1
STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1 taken=0
[stw-census] rounds=64 pending=1 taken=0 blocked=6 alive=8
t5 ... state="native:java/lang/Process.waitFor()I" top=org/jboss/as/process/ManagedProcess$JoinTask.run@9
```

The waiting peer was in `java.lang.Process.waitFor()I` but was not published as
GC-blocked, while the native ring was still dominated by
`org.jboss.as.process.ManagedProcess$ReadTask.run()` reading child process output. The
watchdog then reported zero Java stack dumps because no Java threads responded.

Fix in `../../../../native-io/src/process.rs`: bracket all blocking process waits with
`NativeContext::begin_blocking_region()` / `end_blocking_region()` and resync moved
object references where needed. This covers `java.lang.Process.waitFor()I`,
`java.lang.Process.waitFor(long, TimeUnit)` (both the overflow fallback and poll sleep),
and `java.lang.ProcessHandleImpl.waitForProcessExit0(JZ)I`.

Focused validation:

```text
cargo test -p cratonvm-native-io process_wait_for_enters_gc_blocked_region -- --nocapture
cargo test -p cratonvm-native-io process_handle_wait_for_exit_enters_gc_blocked_region -- --nocapture
cargo test -p cratonvm-native-io process_wait_for_timeout_enters_gc_blocked_region_between_polls -- --nocapture
cargo check -p cratonvm-native-io -p cratonvm-vm -p cratonvm-native-builtins
cargo build --release -p cratonvm-cli --features java-bin-alias --bins
```

The follow-up stock-domain probe with the rebuilt `java` shim shows the original
Process.waitFor STW accounting bug is gone. The Process Controller now reports the
process wait as blocked and has no expected peer to wait for:

```text
[stw-request] initiator=3 alive=8 blocked=7 expected=0
```

The Host Controller also gets further than the previous boundary, including the earlier
network-interface/modcluster milestones and a real host-controller started message:

```text
NetworkInterfaceService matched interface binding
Final response for step handler ... [("extension" => "org.jboss.as.modcluster")] is {"outcome" => "success"}
WFLYSRV0025: WildFly Full 32.0.1.Final ... (Host Controller) started
```

The newly exposed stock-domain boundary is now the Host Controller's `start-servers`
request into the Process Controller inventory path:

```text
WFLYCTL0013: Operation ("start-servers") failed - address: ([("host" => "primary")])
Caused by: java.io.IOException: Socket.getOutputStream: not connected
    at org.jboss.as.process.ProcessControllerClient.requestProcessInventory(ProcessControllerClient.java:247)
    at org.jboss.as.process.protocol.ConnectionImpl.writeMessage(ConnectionImpl.java:85)
```

After that failure, the watchdog can dump Java stacks again rather than reporting that no
Java threads responded. Status remains OPEN: this pass fixes the process wait/native STW
cooperation defect, but does not close the no-JIT Arquillian `awaitServers` propagation
gap, the JIT-on literal-address `WFLYSRV0082`, or the newly exposed
`Socket.getOutputStream: not connected` process-controller inventory residual.

## 2026-07-09 update - process-controller bootstrap linkage gaps fixed; respawn/watchdog boundary remains

Branch `codex/fix-wildfly-stw-rollback-20260708-194425` on the Azure probe host, using
a separate worktree from `/data/data/cratonvm`:

```text
/data/data/cratonvm-worktrees/20260708-194425-wildfly-stw-rollback
```

Unique rebuilt probe binaries were installed under:

```text
/data/data/probes/wildfly-stw-rollback-20260708-194425/bin/
```

This pass continued from the reduced stock-domain boundary above and closed the
process-controller bootstrap/linkage residuals that were masking the broader STW/native
watchdog problem:

1. **ServerSocket local-address propagation fixed.** Plain `ServerSocket.bind` now records
   the actual bound host/port and `getInetAddress()` / `getLocalSocketAddress()` return
   resolved `InetSocketAddress` mirrors instead of null-address holders.
2. **Early process-controller JDK surface filled in.** `Base64.getEncoder()`,
   `Arrays.hashCode(byte[])`, `ProcessBuilder(List)`, `Process` stream accessors, and
   `FileDescriptor`-backed `FileInputStream` / `FileOutputStream` constructors are now
   visible before the later phase tables are installed.
3. **Process stream wrappers fixed.** `BufferedInputStream`, `FilterOutputStream`,
   `OutputStream.write(byte[])`, and `OutputStreamWriter(OutputStream, Charset)` now have
   the synthetic declarations and native delegates needed by
   `ManagedProcess$ReadTask` and WildFly's `Base64OutputStream`. The synthetic `java.io`
   hierarchy was corrected so real bytecode can inherit `FilterInputStream.in` and
   `FilterOutputStream.out` by name.
4. **Respawn policy sleep fixed.** `TimeUnit.sleep(long)` is declared and bridged through
   the existing `Thread.sleep` implementation, so `RespawnPolicy$2.respawn` no longer
   fails with `NoSuchMethodError`.

Focused validation highlights:

```text
cargo check -p cratonvm-native-builtins -p cratonvm-vm
cargo build -p cratonvm-cli --bin java --features java-bin-alias
domain-nojit-io-fields-20260709-075858.log: RC=0, previous
  OutputStreamWriter/Base64OutputStream/FilterOutputStream.out failures gone; exposed
  TimeUnit.sleep(J)V as the next missing method.
domain-nojit-timeunit-sleep-20260709-081840.log: RC=134 via the 90s watchdog, with
  grep -c NoSuch == 0.
domain-nojit-finaldev-20260709-083646.log: final binary rebased on current
  origin/dev, RC=134, grep -c NoSuch == 0, six Host Controller starts;
  native-ring tail ends at
  RespawnPolicy$2.respawn(...) -> TimeUnit.sleep(J)V.
```

The latest stock `domain.sh` probe now repeatedly starts the Host Controller, observes it
finish, sleeps through the respawn policy, and restarts it. After the configured
`CRATONVM_DEFAULT_WATCHDOG_SEC=90`, the process-controller VM still aborts in the known
native/STW watchdog shape:

```text
=== T19.H1 watchdog: deadline of 90s elapsed; requesting thread stack dumps ===
--- T19.H1 thread summary: 20 registered thread(s) ---
  tid=0 name="main" alive=true daemon=false roots=8
  ...
  tid=19 name="reaper for Host Controller" alive=true daemon=false roots=6
=== T19.H1 watchdog: 0 thread(s) dumped; aborting process ===
=== T19.H1 watchdog: no Java threads responded -- main thread is in native (Rust) code.
```

Status remains OPEN. This pass removed the process-controller stream/JDK linkage layers
that prevented clean reproduction of the residual, but did not resolve the broader
process-controller respawn/lifecycle and STW/native watchdog blocker.
