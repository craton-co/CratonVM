# WildFly domain managed servers do not reach started state

Status: OPEN
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
