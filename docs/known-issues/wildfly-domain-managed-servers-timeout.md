# WildFly domain managed servers do not reach started state

Status: OPEN (proximate cause updated 2026-07-06 — see below)
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
2. Fix the `ServiceNotFoundException` on `Services.JBOSS_AS` under
   `CRATONVM_MSC_REAL_START=1` (standalone mode) — a small, deterministic, ~3-second
   repro is already in hand (see this doc's history for the exact `javap` disassembly
   pointing at `BootstrapImpl.internalBootstrap`).
3. Diagnose the domain-mode-specific hang under `CRATONVM_MSC_REAL_START=1` (no exception,
   no progress) — likely a lock-ordering or re-entrancy gap in the `drive_starts` loop
   specific to the host-controller's own service graph, per
   `handoff-wildfly-msc-service-start.md`'s "Value injection ... NOT wired" and async-
   services follow-ups.
4. Once boot reaches real sustained concurrent execution again, help resolve whichever
   corrupt-cell hypothesis is live in
   `wildfly-domain-heap-corrupt-value-timeout.md`, and confirm this doc's timeout is gone.
