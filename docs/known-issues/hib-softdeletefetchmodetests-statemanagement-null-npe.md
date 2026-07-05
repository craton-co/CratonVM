# Hibernate `SoftDeleteFetchModeTests` — `EntityPersister.stateManagement` is `null` during mapping-model build

| | |
|---|---|
| **Status** | 🔴 OPEN — not yet root-caused. Confirmed CratonVM-specific (HotSpot passes 1/1). |
| **Area** | Metamodel bootstrap — `EntityPersister`/`StateManagement` wiring for soft-delete fetch-mode mapping |
| **Symptom** | `java.lang.NullPointerException: Cannot invoke "org.hibernate.persister.state.spi.StateManagement.createAuxiliaryMapping(...)" because "this.stateManagement" is null` |
| **Severity** | low — single class affected in this sweep. |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage (Azure host, dev `49aaf713`, real-JDK, JIT on, `TIMEOUT=1200`). |

## Symptom

```
@@FAIL org.hibernate.orm.test.softdelete.SoftDeleteFetchModeTests :: java.lang.NullPointerException: Cannot invoke "org.hibernate.persister.state.spi.StateManagement.createAuxiliaryMapping(org.hibernate.persister.entity.EntityPersister, org.hibernate.mapping.RootClass, org.hibernate.metamodel.mapping.internal.MappingModelCreationProcess)" because "this.stateManagement" is null
```

This fails during SessionFactory/mapping-model bootstrap, before any SQL is
logged (no `Hibernate:` DDL lines precede the failure, unlike most other
classes in this sweep). `EntityPersister.stateManagement` — a field that
should be set during persister construction/initialization — is still
`null` by the time `createAuxiliaryMapping` needs it for a soft-delete
fetch-mode mapping.

HotSpot passes this test cleanly (`found=1 ok=1 failed=0`, Azure HotSpot
baseline), confirming this is CratonVM-specific — it is **not** an upstream
Hibernate-8.0-SNAPSHOT API/version-drift gap (a real possibility to rule out
given the log shows `HHH000001: Hibernate ORM core version 8.0.0-SNAPSHOT`),
since HotSpot exercises the identical snapshot jar successfully.

## Note — supersedes a stale prior triage entry

[hib-linux-fail-bucket-triage-20260703.md](hib-linux-fail-bucket-triage-20260703.md)
flagged this same class as an open item on 2026-07-03, but with a
**different** symptom: `Expecting UnsupportedMappingException...` (an
expected-exception-not-thrown validation gap). The failure has since changed
to the `stateManagement` NPE above — either the test's behavior/expectations
shifted with a `dev` merge between 2026-07-03 and 2026-07-05, or an
intervening fix changed how far the bootstrap gets before failing (getting
past the point that used to swallow/miss the expected exception, and now
failing earlier/differently at persister construction). Either way this is
the current, up-to-date symptom as of dev `49aaf713`.

## Repro

Azure host, harness at `/home/victor/hibpkg/runner`:
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <(echo org.hibernate.orm.test.softdelete.SoftDeleteFetchModeTests) 0
```

## Next steps (not yet done)

- Trace `EntityPersister`'s constructor/init path to find where
  `stateManagement` should be assigned and why it's skipped for this
  specific soft-delete + fetch-mode combination (likely a conditional
  branch in persister setup that CratonVM takes differently, e.g. due to a
  field/annotation-processing order difference during metamodel build).
- Confirm with `--nojit` whether this reproduces identically (bootstrap-time
  NPEs in this codebase are usually interpreter-reachable, not JIT-specific,
  but worth ruling out given the immediate-failure/no-DDL timing).
