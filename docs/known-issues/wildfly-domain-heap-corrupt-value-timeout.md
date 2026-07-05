# WildFly domain startup timeout with repeated corrupt `Value` cell guard

Status: OPEN
Severity: High
First confirmed: 2026-07-05 on Azure worktree `codex/wildfly-nonpassed-probes-20260705-035722`

## Symptom

After fixing the JBoss Modules multi-entry `-mp` bug, `EEConcurrencyExecutorShutdownTestCase` no longer exits immediately during process-controller launch. It now waits the full startup window and fails with:

```text
java.util.concurrent.TimeoutException: Managed servers were not started within [120] seconds
```

The log repeatedly emits the same heap guard diagnostic while the test polls management:

```text
gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) - returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). slot=0x2002600b1d0 raw0="0x0000000100000009" raw1="0x0000000000000000"
```

The management client retries `remote://127.0.0.1:9999` until timeout. The generated domain directory contains configuration and `data/kernel/process-uuid`, but no `process-controller.log` or `host-controller.log` beyond the empty audit log.

## Evidence

Primary run:

```text
/data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner/out/azure-eeconcurrency-mpmulti2-082-jit-real-failed-20260705-160124
```

Key files:

```text
logs/00001-org.jboss.as.test.integration.domain.EEConcurrencyExecutorShutdownTestCase.log
failcauses.log
summary.txt
```

Result summary:

```text
classes: FAIL=1
test-methods: found=1 passed=0 failed=0 errors=1 sum-class-ms=128157
wall-clock=128s
```

## Notes

This is distinct from the fixed process-controller module-path bug. The old immediate `ModuleNotFoundException` and `MDC.put` linkage failure are gone with `cratonvm-wildfly-nonpassed-20260705-035722-mpmulti2`; the remaining failure is a real 120-second domain startup timeout with a repeated guarded heap-corruption signature.
