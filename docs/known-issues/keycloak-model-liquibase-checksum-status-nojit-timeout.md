# RealmModelTest no-JIT timeout in Liquibase checksum/status serialization

Status: open

Date observed: 2026-07-09, after fixing the Liquibase/Xerces XML parse timeout.

## Summary

`RealmModelTest` under CratonVM `--nojit` now gets past the earlier
Liquibase/Xerces XML schema parsing hotspot. The old stack in
`XMLEntityScanner.scanQName -> checkLimit -> XMLLimitAnalyzer.addValue` no
longer appears in the 900-second watchdog evidence.

The class still exceeds the 900-second suite-runner watchdog. The current stack
is later in Liquibase update/status work, while Liquibase is generating
checksums for change sets and serializing change objects.

HotSpot `-Xint` previously passed the same class/list in 142.649s, so this
remains a CratonVM no-JIT throughput residual rather than a Keycloak functional
failure.

## Evidence

The unique CratonVM binary used for the current validation was:

```text
C:\craton\cargo-targets\keycloak-liquibase-xerces-20260709-003\release\cratonvm-keycloak-liquibase-xerces-20260709-009.exe
```

Suite-runner command shape:

```powershell
& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -Vm craton -Category others -Jit off `
  -ClassList 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite\keycloak-model-realm-stw-20260708-001.tsv' `
  -RunName 'keycloak-liquibase-xerces-verify-20260709-009' `
  -Parallel 1 -TimeoutSec 900 `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' `
  -WorkDir 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite' `
  -Exe 'C:\craton\cargo-targets\keycloak-liquibase-xerces-20260709-003\release\cratonvm-keycloak-liquibase-xerces-20260709-009.exe'
```

Result:

```text
HANG 900.1s org.keycloak.testsuite.model.RealmModelTest
```

The representative 900-second stack-watchdog run is no longer in Xerces XML
schema parsing. The main thread is in:

```text
org.keycloak.connections.jpa.updater.liquibase.LiquibaseJpaUpdaterProvider.update
liquibase.command.core.AbstractUpdateCommandStep.getStatusVisitor
liquibase.changelog.visitor.StatusChangeLogIterator.run
liquibase.changelog.ChangeSetStatus.<init>
liquibase.changelog.ChangeSet.generateCheckSum
liquibase.change.AbstractChange.generateCheckSum
liquibase.serializer.core.string.StringChangeLogSerializer.serializeObject
liquibase.change.AbstractChange$1.include
```

## Current hypothesis

This is the next no-JIT Liquibase throughput layer after schema parsing. The
likely hot area is the checksum/status serializer path: change-object traversal,
reflection/property inspection, string serialization, and repeated visitor
filtering. It should be investigated as a Liquibase checksum/status residual,
not as another Xerces scanner or XML schema-loading issue.

## Next leads

- Instrument `StringChangeLogSerializer.serializeObject` and
  `AbstractChange.generateCheckSum` under `--nojit`.
- Compare method counts and allocation churn against HotSpot `-Xint`.
- Check whether the visitor/status path repeats checksum generation for the same
  change sets or repeatedly traverses reflective metadata.
- Keep the previously fixed Xerces XML parser intrinsics in place; the current
  evidence has already moved past that phase.
