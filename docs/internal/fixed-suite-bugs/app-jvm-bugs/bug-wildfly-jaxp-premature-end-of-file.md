# WildFly health — schema validation `Premature end of file` (WF-5)

## Status
**OPEN** on `target/release/cratonvm.exe` (2026-06-05 apps suite).

## Severity
**MEDIUM** — `apps/wildfly/health` JUnit module fails; isolated JAXP probes pass.

## App / suite
- **Tree:** `apps/wildfly/health/`
- **Runner:** `apps/_test-harness/RunDirTests` on `target/test-classes`
- **Harness:** `test-infra/run-all-apps-suites.sh`
- **Log:** `test-infra/suite-results/apps-all-20260605-170945/wildfly-health-cratonvm.log`

## Symptom

```
RESULT tests=2 failures=2 ignored=0 ms=3037 ok=false
FAIL testSubsystem[0](org.wildfly.extension.health.HealthSubsystemTestCase) :: (no message)
FAIL testSchema[0](org.wildfly.extension.health.HealthSubsystemTestCase)
  :: fatal error: Premature end of file.
```

- **rc:** 1 · **wall:** 4.6 s
- HotSpot: **PASS** (2/2) — not run in apps suite (CratonVM failed first)

## HotSpot behavior

Both parameterized tests in `HealthSubsystemTestCase` pass — XSD/schema validation succeeds in full JUnit context.

## CratonVM behavior

Discovery finds 1 runnable class, runs 2 tests, both fail. The schema test surfaces SAX `fatal error: Premature end of file.` — the validator is reading an **empty or truncated** input stream in situ.

Isolated JAXP probes on CratonVM (documented in prior runs) match HotSpot for deliberate bad input and for loading the same XSD bytes via TCCL. Failure is **context-dependent** (full WildFly controller classpath + JUnit runner), not a bare `SchemaFactory` repro.

### Noise in log (likely benign)

Many `gen_heap::get_field: out-of-bounds field read dropped` warnings on WildFly `*Constraint$Factory` inner classes during test setup — speculative layout probes, not the direct crash signature.

## Root cause (suspected)

Unknown. Leading hypotheses:

1. Wrong or truncated stream when the test harness supplies schema XML to the validator
2. TCCL / resource path difference inside JUnit vs isolated probe
3. Earlier exception swallowed; validator sees zero-byte input

**Blocked on WF-6:** `Throwable.getStackTrace()` after throw is unreliable, so the failing stream hook cannot be localized in-VM yet.

## Reproduce

```bash
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1
bash test-infra/run-all-apps-suites.sh
# or manually:
WFCP="apps/_test-harness;apps/wildfly/health/target/classes;apps/wildfly/health/target/test-classes;$(cat apps/wildfly/health/cratonvm-health-cp.txt)"
cratonvm.exe --java-home "<jdk-25>" --Xmx 2g -cp "$WFCP" RunDirTests apps/wildfly/health/target/test-classes
```

## Fix direction

1. Fix [WF-6](bug-wildfly-throwable-stack-trace-capture.md) stack traces.
2. Instrument byte length / first-last bytes of schema stream inside `testSchema`.
3. Diff stream source against HotSpot at the same hook.

## Related

- [bug-wildfly-throwable-stack-trace-capture.md](bug-wildfly-throwable-stack-trace-capture.md) (WF-6)
- [apps/wildfly/CRATONVM_BUGS.md](../../apps/wildfly/CRATONVM_BUGS.md)
- [apps/CRATONVM_CRASHES.md](../../apps/CRATONVM_CRASHES.md)
